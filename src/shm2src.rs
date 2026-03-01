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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shm_path: DEFAULT_PATH.to_string(),
            is_live: false,
            live_only: true,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct TimelineState {
    need_reset: bool,
    in_base_pts_ns: Option<i64>,
    out_base_running_ns: Option<u64>,
    last_sync_seq: u64,
    expected_sync_gen: u64,
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

fn rebase_pts_ns(desc_pts_ns: i64, in_base_pts_ns: i64, out_base_running_ns: u64) -> i64 {
    if desc_pts_ns < 0 {
        return -1;
    }
    let delta = (desc_pts_ns as i128) - (in_base_pts_ns as i128);
    let clamped_delta = delta.max(0) as u64;
    out_base_running_ns
        .saturating_add(clamped_delta)
        .min(i64::MAX as u64) as i64
}

fn apply_timeline_snapshot(ts: &mut TimelineState, snap: TimelineSnapshot, out_now_ns: u64) {
    if !snap.valid || snap.producer_pts_ns < 0 {
        return;
    }

    let in_base = ts.in_base_pts_ns.unwrap_or(snap.producer_pts_ns.max(0));
    let desired_out_base = out_now_ns.saturating_sub(snap.producer_pts_ns.max(0) as u64);

    match ts.out_base_running_ns {
        None => {
            ts.in_base_pts_ns = Some(in_base);
            ts.out_base_running_ns = Some(desired_out_base);
        }
        Some(cur_out_base) => {
            // Slew-limit correction to avoid visible jumps (2ms per beacon step).
            let err = (desired_out_base as i128) - (cur_out_base as i128);
            let step = err.clamp(-2_000_000, 2_000_000);
            let new_base = ((cur_out_base as i128) + step).max(0) as u64;
            ts.in_base_pts_ns = Some(in_base);
            ts.out_base_running_ns = Some(new_base);
        }
    }

    ts.last_sync_seq = snap.seq;
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
                _ => unreachable!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let state = self.state.lock().expect("state poisoned");
            match pspec.name() {
                "shm-path" => state.settings.shm_path.to_value(),
                "is-live" => state.settings.is_live.to_value(),
                "live-only" => state.settings.live_only.to_value(),
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
            state.timeline.need_reset = true;
            state.timeline.in_base_pts_ns = None;
            state.timeline.out_base_running_ns = None;
            state.timeline.last_sync_seq = 0;
            state.timeline.expected_sync_gen = snap.generation;

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
            state.timeline.need_reset = true;
            state.timeline.in_base_pts_ns = None;
            state.timeline.out_base_running_ns = None;
            state.timeline.last_sync_seq = 0;
            state.timeline.expected_sync_gen = 0;
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
            let (desc, ptr, snap) = loop {
                {
                    let state = self.state.lock().expect("state poisoned");
                    if state.unlocked {
                        return Err(gst::FlowError::Flushing);
                    }
                }
                let mut r = reader.lock().map_err(|_| gst::FlowError::Error)?;
                match r.try_recv_desc().map_err(|_| gst::FlowError::Error)? {
                    Some(desc) => {
                        let ptr = r.payload_ptr(&desc).map_err(|_| gst::FlowError::Error)?;
                        let snap = r.timeline_snapshot();
                        break (desc, ptr, snap);
                    }
                    None => {
                        drop(r);
                        poll_yield_sleep(&mut idle_cycles, Duration::from_millis(1));
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

                if snap.generation != ts.expected_sync_gen {
                    ts.expected_sync_gen = snap.generation;
                    ts.need_reset = true;
                    ts.in_base_pts_ns = None;
                    ts.out_base_running_ns = None;
                    ts.last_sync_seq = 0;
                }

                if live_only {
                    if let Some(now) = now_rt {
                        if snap.valid
                            && snap.snapshot_gen == ts.expected_sync_gen
                            && snap.seq != ts.last_sync_seq
                        {
                            apply_timeline_snapshot(&mut ts, snap, now);
                        }
                    }

                    let mut out_pts_ns = desc.pts_ns;
                    if ts.need_reset {
                        if let Some(now) = now_rt {
                            ts.in_base_pts_ns = Some(desc.pts_ns.max(0));
                            ts.out_base_running_ns = Some(now);
                            ts.need_reset = false;
                            buf.set_flags(gst::BufferFlags::DISCONT);
                        }
                    }
                    if let (Some(in_base), Some(out_base)) =
                        (ts.in_base_pts_ns, ts.out_base_running_ns)
                    {
                        out_pts_ns = rebase_pts_ns(desc.pts_ns, in_base, out_base);
                    }

                    if out_pts_ns >= 0 {
                        buf.set_pts(gst::ClockTime::from_nseconds(out_pts_ns as u64));
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
