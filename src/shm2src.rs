use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;
use once_cell::sync::Lazy;

use crate::platform::resolve_backend;
use crate::transport::{Reader, ReceivedDesc, TimelineSnapshot};

type ReaderType = Reader;

#[cfg(unix)]
const DEFAULT_PATH: &str = "/dev/shm/gst-shm2-default";
#[cfg(windows)]
const DEFAULT_PATH: &str = "winshm://Local/gst-shm2-default";

#[derive(Debug)]
struct Settings {
    shm_path: String,
    is_live: bool,
    live_only: bool,
    latest_only: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shm_path: DEFAULT_PATH.to_string(),
            is_live: false,
            live_only: true,
            latest_only: true,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct TimelineState {
    expected_gen: u64,
    offset_valid: bool,
    offset_ns: i64,
    last_snapshot_seq: u64,
    sink_mono_at_snap: u64,
    producer_pts_at_snap: i64,
    need_discont: bool,
    last_late_log_ns: u64,
}

#[derive(Default)]
struct State {
    settings: Settings,
    reader: Option<Arc<Mutex<ReaderType>>>,
    unlocked: bool,
    timeline: TimelineState,
    hb_stop: Option<Arc<AtomicBool>>,
    hb_thread: Option<std::thread::JoinHandle<()>>,
}

struct ShmReadWrap {
    reader: Arc<Mutex<ReaderType>>,
    desc: ReceivedDesc,
    ptr: *const u8,
    len: usize,
}

impl AsRef<[u8]> for ShmReadWrap {
    fn as_ref(&self) -> &[u8] {
        // Pointer and length are validated before wrapper creation.
        unsafe { slice::from_raw_parts(self.ptr, self.len) }
    }
}

unsafe impl Send for ShmReadWrap {}

impl Drop for ShmReadWrap {
    fn drop(&mut self) {
        if let Ok(mut reader) = self.reader.lock() {
            let _ = reader.recycle_desc(self.desc.buffer_id, self.desc.offset, self.desc.len, 0);
        }
    }
}

fn poll_yield_sleep(idle_cycles: &mut u32, steady_sleep: Duration) {
    thread::yield_now();
    let sleep_for = match *idle_cycles {
        0..=7 => Duration::from_micros(50),
        8..=31 => Duration::from_micros(200),
        32..=127 => Duration::from_millis(1),
        _ => steady_sleep,
    };
    thread::sleep(sleep_for);
    *idle_cycles = idle_cycles.saturating_add(1);
}

fn current_running_time_ns(elem: &crate::shm2src::Shm2Src) -> Option<u64> {
    let clock = elem.clock()?;
    let now = clock.time()?.nseconds();
    let base = elem.base_time()?.nseconds();
    Some(now.saturating_sub(base))
}

fn update_clock_sync(ts: &mut TimelineState, snap: TimelineSnapshot, local_now_ns: u64) {
    if !snap.valid || snap.producer_pts_ns < 0 {
        return;
    }
    if snap.seq == ts.last_snapshot_seq {
        return;
    }

    let desired_offset = (local_now_ns as i128) - (snap.sink_mono_ns as i128);
    let desired_offset = desired_offset
        .clamp(i64::MIN as i128, i64::MAX as i128) as i64;

    if !ts.offset_valid {
        ts.offset_ns = desired_offset;
        ts.offset_valid = true;
    } else {
        // Slew-limit correction to avoid visible jumps (2ms per beacon step).
        let err = desired_offset as i128 - ts.offset_ns as i128;
        let step = err.clamp(-2_000_000, 2_000_000);
        ts.offset_ns = (ts.offset_ns as i128 + step) as i64;
    }

    ts.last_snapshot_seq = snap.seq;
    ts.sink_mono_at_snap = snap.sink_mono_ns;
    ts.producer_pts_at_snap = snap.producer_pts_ns;
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Shm2Src {
        state: Mutex<State>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Shm2Src {
        const NAME: &'static str = "GstShm2Src";
        type Type = super::Shm2Src;
        type ParentType = gst_base::PushSrc;
    }

    impl ObjectImpl for Shm2Src {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
                vec![
                    glib::ParamSpecString::builder("shm-path")
                        .nick("SHM path")
                        .blurb("Path of shared memory file")
                        .default_value(Some(DEFAULT_PATH))
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoolean::builder("is-live")
                        .nick("Live")
                        .blurb("Act like a live source")
                        .default_value(false)
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoolean::builder("live-only")
                        .nick("Live only")
                        .blurb("On attach/re-attach, restart output timeline from current running time")
                        .default_value(true)
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoolean::builder("latest-only")
                        .nick("Latest only")
                        .blurb("Always take newest available frame and drop older queued frames")
                        .default_value(true)
                        .mutable_ready()
                        .build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            let mut state = self.state.lock().expect("state poisoned");
            match pspec.name() {
                "shm-path" => {
                    if let Ok(v) = value.get::<Option<String>>() {
                        state.settings.shm_path = v.unwrap_or_else(|| DEFAULT_PATH.to_string());
                    }
                }
                "is-live" => {
                    if let Ok(v) = value.get::<bool>() {
                        state.settings.is_live = v;
                        self.obj().set_live(v);
                    }
                }
                "live-only" => {
                    if let Ok(v) = value.get::<bool>() {
                        state.settings.live_only = v;
                    }
                }
                "latest-only" => {
                    if let Ok(v) = value.get::<bool>() {
                        state.settings.latest_only = v;
                    }
                }
                _ => unreachable!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let state = self.state.lock().expect("state poisoned");
            match pspec.name() {
                "shm-path" => state.settings.shm_path.to_value(),
                "is-live" => state.settings.is_live.to_value(),
                "live-only" => state.settings.live_only.to_value(),
                "latest-only" => state.settings.latest_only.to_value(),
                _ => unreachable!(),
            }
        }
    }

    impl GstObjectImpl for Shm2Src {}

    impl ElementImpl for Shm2Src {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
                gst::subclass::ElementMetadata::new(
                    "SHM2 Source",
                    "Source",
                    "Receive data from SHM-only transport",
                    "shm2-rs",
                )
            });
            Some(&*METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
                let caps = gst::Caps::new_any();
                vec![
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .expect("failed to create src pad template"),
                ]
            });
            PAD_TEMPLATES.as_ref()
        }
    }

    impl BaseSrcImpl for Shm2Src {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().expect("state poisoned");
            let selected = resolve_backend(&state.settings.shm_path).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["Invalid shm-path '{}': {}", state.settings.shm_path, err]
                )
            })?;
            let mut reader =
                Reader::open(selected.backend.as_ref(), &selected.name).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        [
                            "Failed to open shm reader at {} (resolved '{}'): {}",
                            state.settings.shm_path,
                            selected.name,
                            err
                        ]
                    )
                })?;
            reader.claim_consumer(std::process::id()).map_err(|_| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    [
                        "Another shm2src is already connected to {}",
                        state.settings.shm_path
                    ]
                )
            })?;
            self.obj().set_format(gst::Format::Time);
            let snap = reader.timeline_snapshot();
            self.obj().set_live(state.settings.is_live);
            state.reader = Some(Arc::new(Mutex::new(reader)));
            state.unlocked = false;
            state.timeline.expected_gen = snap.generation;
            state.timeline.offset_valid = false;
            state.timeline.offset_ns = 0;
            state.timeline.last_snapshot_seq = 0;
            state.timeline.sink_mono_at_snap = 0;
            state.timeline.producer_pts_at_snap = 0;
            state.timeline.need_discont = true;
            state.timeline.last_late_log_ns = 0;

            if let Some(t) = state.hb_thread.take() {
                let _ = t.join();
            }
            if let Some(reader_arc) = state.reader.as_ref().cloned() {
                let stop = Arc::new(AtomicBool::new(false));
                state.hb_stop = Some(stop.clone());
                state.hb_thread = Some(std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok(r) = reader_arc.lock() {
                            r.consumer_heartbeat_tick();
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }));
            }
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().expect("state poisoned");
            if let Some(reader) = state.reader.as_ref() {
                if let Ok(mut r) = reader.lock() {
                    r.release_consumer(std::process::id());
                }
            }
            state.reader = None;
            state.unlocked = false;
            state.timeline.expected_gen = 0;
            state.timeline.offset_valid = false;
            state.timeline.offset_ns = 0;
            state.timeline.last_snapshot_seq = 0;
            state.timeline.sink_mono_at_snap = 0;
            state.timeline.producer_pts_at_snap = 0;
            state.timeline.need_discont = true;
            state.timeline.last_late_log_ns = 0;
            if let Some(stop) = state.hb_stop.take() {
                stop.store(true, Ordering::Relaxed);
            }
            if let Some(t) = state.hb_thread.take() {
                let _ = t.join();
            }
            Ok(())
        }

        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().expect("state poisoned");
            state.unlocked = true;
            Ok(())
        }

        fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().expect("state poisoned");
            state.unlocked = false;
            Ok(())
        }

        fn is_seekable(&self) -> bool {
            false
        }
    }

    impl PushSrcImpl for Shm2Src {
        fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
            let reader = {
                let state = self.state.lock().expect("state poisoned");
                if state.unlocked {
                    return Err(gst::FlowError::Flushing);
                }
                state
                    .reader
                    .as_ref()
                    .cloned()
                    .ok_or(gst::FlowError::Flushing)?
            };

            let mut idle_cycles = 0u32;
            let (desc, ptr, snap, dropped) = loop {
                {
                    let state = self.state.lock().expect("state poisoned");
                    if state.unlocked {
                        return Err(gst::FlowError::Flushing);
                    }
                }
                let (latest_only, mut r) = {
                    let state = self.state.lock().expect("state poisoned");
                    (state.settings.latest_only, reader.lock().map_err(|_| gst::FlowError::Error)?)
                };
                if latest_only {
                    match r
                        .try_recv_latest_desc()
                        .map_err(|_| gst::FlowError::Error)?
                    {
                        Some((desc, dropped)) => {
                            let ptr = r.payload_ptr(&desc).map_err(|_| gst::FlowError::Error)?;
                            let snap = r.timeline_snapshot();
                        break (desc, ptr, snap, dropped);
                        }
                        None => {
                            drop(r);
                            poll_yield_sleep(&mut idle_cycles, Duration::from_millis(1));
                        }
                    }
                } else {
                    match r.try_recv_desc().map_err(|_| gst::FlowError::Error)? {
                        Some(desc) => {
                            let ptr = r.payload_ptr(&desc).map_err(|_| gst::FlowError::Error)?;
                            let snap = r.timeline_snapshot();
                            break (desc, ptr, snap, 0);
                        }
                        None => {
                            drop(r);
                            poll_yield_sleep(&mut idle_cycles, Duration::from_millis(1));
                        }
                    }
                }
            };

            let wrap = ShmReadWrap {
                reader,
                desc,
                ptr,
                len: desc.len as usize,
            };
            let mem = gst::Memory::from_slice(wrap);
            let mut out = gst::Buffer::new();

            if let Some(buf) = out.get_mut() {
                let now_rt = current_running_time_ns(&self.obj());
                let (live_only, mut ts) = {
                    let state = self.state.lock().expect("state poisoned");
                    (state.settings.live_only, TimelineState { ..state.timeline })
                };

                if snap.generation != ts.expected_gen {
                    ts.expected_gen = snap.generation;
                    ts.offset_valid = false;
                    ts.last_snapshot_seq = 0;
                    ts.need_discont = true;
                }

                if live_only {
                    if let Some(now) = now_rt {
                        update_clock_sync(&mut ts, snap, now);
                    }

                    let mut out_pts_ns: Option<u64> = None;
                    if ts.offset_valid && desc.pts_ns >= 0 {
                        let sink_time = desc.pts_ns
                            + (ts.sink_mono_at_snap as i64 - ts.producer_pts_at_snap);
                        let out = sink_time.saturating_add(ts.offset_ns);
                        if out >= 0 {
                            out_pts_ns = Some(out as u64);
                        }
                    }

                    if ts.need_discont || dropped > 0 {
                        buf.set_flags(gst::BufferFlags::DISCONT);
                        ts.need_discont = false;
                    }

                    if let Some(out_pts) = out_pts_ns {
                        if let Some(now) = now_rt {
                            if now > out_pts.saturating_add(50_000_000)
                                && now.saturating_sub(ts.last_late_log_ns) > 1_000_000_000
                            {
                                eprintln!(
                                    "[shm2] late pts: now_ns={} pts_ns={} lag_ns={} dropped={}",
                                    now,
                                    out_pts,
                                    now.saturating_sub(out_pts),
                                    dropped
                                );
                                ts.last_late_log_ns = now;
                            }
                        }
                        buf.set_pts(gst::ClockTime::from_nseconds(out_pts));
                    }

                    let mut state = self.state.lock().expect("state poisoned");
                    state.timeline = ts;
                } else if desc.pts_ns >= 0 {
                    buf.set_pts(gst::ClockTime::from_nseconds(desc.pts_ns as u64));
                }

                buf.append_memory(mem);
            }

            Ok(gst_base::subclass::base_src::CreateSuccess::NewBuffer(out))
        }
    }
}

glib::wrapper! {
    pub struct Shm2Src(ObjectSubclass<imp::Shm2Src>) @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    gst::Element::register(plugin, "shm2src", gst::Rank::NONE, Shm2Src::static_type())
}
