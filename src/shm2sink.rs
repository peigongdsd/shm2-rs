use std::collections::HashMap;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;
use once_cell::sync::Lazy;

use crate::platform::resolve_backend;
use crate::transport::{
    AllocLease, TransportConfig, Writer, STARTUP_RUNNING, STARTUP_SRC_READY,
};

type WriterType = Writer;

#[cfg(unix)]
const DEFAULT_PATH: &str = "/dev/shm/gst-shm2-default";
#[cfg(windows)]
const DEFAULT_PATH: &str = "winshm://Local/gst-shm2-default";

#[derive(Debug)]
struct Settings {
    shm_path: String,
    shm_size: u64,
    perms: u32,
    wait_for_connection: bool,
    consumer_timeout_ms: u32,
    timeline_beacon_ms: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shm_path: DEFAULT_PATH.to_string(),
            shm_size: 64 * 1024 * 1024,
            perms: 0o660,
            wait_for_connection: true,
            consumer_timeout_ms: 1000,
            timeline_beacon_ms: 250,
        }
    }
}

#[derive(Default)]
struct State {
    settings: Settings,
    writer: Option<Arc<Mutex<WriterType>>>,
    allocator: Option<ShmArenaAllocator>,
    unlocked: bool,
    hb_stop: Option<Arc<AtomicBool>>,
    hb_thread: Option<std::thread::JoinHandle<()>>,
    gc_stop: Option<Arc<AtomicBool>>,
    gc_thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy)]
struct PendingLease {
    buffer_id: u32,
    offset: u64,
    len: u32,
}

#[derive(Default)]
struct AllocTracker {
    writer: Option<Arc<Mutex<WriterType>>>,
    pending_by_ptr: HashMap<usize, PendingLease>,
}

impl AllocTracker {
    fn alloc_lease_for_upstream(
        &mut self,
        size: usize,
        align: u64,
    ) -> Result<AllocLease, glib::BoolError> {
        let writer = self
            .writer
            .as_ref()
            .cloned()
            .ok_or_else(|| glib::bool_error!("shm writer not available"))?;
        let lease = {
            let mut w = writer
                .lock()
                .map_err(|_| glib::bool_error!("writer mutex poisoned"))?;
            w.alloc_lease(size as u32, align)
                .map_err(|_| glib::bool_error!("shm allocation failed"))?
        };
        self.pending_by_ptr.insert(
            lease.ptr as usize,
            PendingLease {
                buffer_id: lease.buffer_id,
                offset: lease.offset,
                len: lease.len,
            },
        );
        Ok(lease)
    }

    fn release_unpublished(&mut self, ptr: *mut u8) {
        if let Some(pending) = self.pending_by_ptr.remove(&(ptr as usize)) {
            if let Some(writer) = &self.writer {
                if let Ok(mut w) = writer.lock() {
                    let _ = w.free_lease(pending.buffer_id);
                }
            }
        }
    }

    fn mark_published(&mut self, ptr: *const u8, len: usize) -> Option<AllocLease> {
        let key = ptr as usize;
        let pending = self.pending_by_ptr.remove(&key)?;
        if pending.len as usize != len {
            // length mismatch means this is not a direct whole-block fast path
            self.pending_by_ptr.insert(key, pending);
            return None;
        }
        Some(AllocLease {
            buffer_id: pending.buffer_id,
            offset: pending.offset,
            len: pending.len,
            ptr: key as *mut u8,
        })
    }

}

struct ShmWritableMemory {
    tracker: Arc<Mutex<AllocTracker>>,
    ptr: *mut u8,
    len: usize,
}

impl AsMut<[u8]> for ShmWritableMemory {
    fn as_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

unsafe impl Send for ShmWritableMemory {}

impl Drop for ShmWritableMemory {
    fn drop(&mut self) {
        if let Ok(mut t) = self.tracker.lock() {
            t.release_unpublished(self.ptr);
        }
    }
}

mod alloc_imp {
    use super::*;

    #[derive(Default)]
    pub struct ShmArenaAllocator {
        pub tracker: Mutex<Option<Arc<Mutex<AllocTracker>>>>,
        pub align: Mutex<u64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ShmArenaAllocator {
        const NAME: &'static str = "GstShm2ArenaAllocator";
        type Type = super::ShmArenaAllocator;
        type ParentType = gst::Allocator;
    }

    impl ObjectImpl for ShmArenaAllocator {}
    impl GstObjectImpl for ShmArenaAllocator {}

    impl AllocatorImpl for ShmArenaAllocator {
        fn alloc(
            &self,
            size: usize,
            _params: Option<&gst::AllocationParams>,
        ) -> Result<gst::Memory, glib::BoolError> {
            let tracker = self
                .tracker
                .lock()
                .map_err(|_| glib::bool_error!("tracker mutex poisoned"))?
                .as_ref()
                .cloned()
                .ok_or_else(|| glib::bool_error!("tracker unavailable"))?;
            let align = *self
                .align
                .lock()
                .map_err(|_| glib::bool_error!("align mutex poisoned"))?;
            let lease = tracker
                .lock()
                .map_err(|_| glib::bool_error!("tracker mutex poisoned"))?
                .alloc_lease_for_upstream(size, align)?;
            let wrapped = ShmWritableMemory {
                tracker: tracker.clone(),
                ptr: lease.ptr,
                len: lease.len as usize,
            };
            Ok(gst::Memory::from_mut_slice(wrapped))
        }
    }
}

glib::wrapper! {
    pub struct ShmArenaAllocator(ObjectSubclass<alloc_imp::ShmArenaAllocator>) @extends gst::Allocator, gst::Object;
}

impl ShmArenaAllocator {
    fn with_tracker(tracker: Arc<Mutex<AllocTracker>>, align: u64) -> Self {
        let obj: ShmArenaAllocator = glib::Object::new();
        let imp = obj.imp();
        *imp.tracker.lock().expect("tracker mutex poisoned") = Some(tracker);
        *imp.align.lock().expect("align mutex poisoned") = align.max(1);
        obj
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

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Shm2Sink {
        state: Mutex<State>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Shm2Sink {
        const NAME: &'static str = "GstShm2Sink";
        type Type = super::Shm2Sink;
        type ParentType = gst_base::BaseSink;
    }

    impl ObjectImpl for Shm2Sink {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
                vec![
                    glib::ParamSpecString::builder("shm-path")
                        .nick("SHM path")
                        .blurb("Path of shared memory file")
                        .default_value(Some(DEFAULT_PATH))
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecUInt64::builder("shm-size")
                        .nick("SHM size")
                        .blurb("Size of shared memory region")
                        .default_value(64 * 1024 * 1024)
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecUInt::builder("perms")
                        .nick("Permissions")
                        .blurb("Permissions of shared memory file")
                        .default_value(0o660)
                        .maximum(0o7777)
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoolean::builder("wait-for-connection")
                        .nick("Wait for connection")
                        .blurb("Block rendering until a shm2src consumer is connected")
                        .default_value(true)
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecUInt::builder("consumer-timeout-ms")
                        .nick("Consumer timeout")
                        .blurb("Consumer heartbeat timeout in milliseconds")
                        .default_value(1000)
                        .minimum(1)
                        .maximum(60_000)
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecUInt::builder("timeline-beacon-ms")
                        .nick("Timeline beacon")
                        .blurb("Timeline synchronization beacon period in milliseconds")
                        .default_value(250)
                        .minimum(1)
                        .maximum(60_000)
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
                "shm-size" => {
                    if let Ok(v) = value.get::<u64>() {
                        state.settings.shm_size = v;
                    }
                }
                "perms" => {
                    if let Ok(v) = value.get::<u32>() {
                        state.settings.perms = v;
                    }
                }
                "wait-for-connection" => {
                    if let Ok(v) = value.get::<bool>() {
                        state.settings.wait_for_connection = v;
                    }
                }
                "consumer-timeout-ms" => {
                    if let Ok(v) = value.get::<u32>() {
                        state.settings.consumer_timeout_ms = v;
                    }
                }
                "timeline-beacon-ms" => {
                    if let Ok(v) = value.get::<u32>() {
                        state.settings.timeline_beacon_ms = v.max(1);
                    }
                }
                _ => unreachable!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let state = self.state.lock().expect("state poisoned");
            match pspec.name() {
                "shm-path" => state.settings.shm_path.to_value(),
                "shm-size" => state.settings.shm_size.to_value(),
                "perms" => state.settings.perms.to_value(),
                "wait-for-connection" => state.settings.wait_for_connection.to_value(),
                "consumer-timeout-ms" => state.settings.consumer_timeout_ms.to_value(),
                "timeline-beacon-ms" => state.settings.timeline_beacon_ms.to_value(),
                _ => unreachable!(),
            }
        }
    }

    impl GstObjectImpl for Shm2Sink {}
    impl ElementImpl for Shm2Sink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
                gst::subclass::ElementMetadata::new(
                    "SHM2 Sink",
                    "Sink",
                    "Send data over SHM-only transport",
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
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .expect("failed to create sink pad template"),
                ]
            });

            PAD_TEMPLATES.as_ref()
        }
    }

    impl BaseSinkImpl for Shm2Sink {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().expect("state poisoned");
            let cfg = TransportConfig {
                total_size: state.settings.shm_size as usize,
                perms: state.settings.perms,
                timeline_beacon_ms: state.settings.timeline_beacon_ms,
                ..Default::default()
            };

            let selected = resolve_backend(&state.settings.shm_path).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["Invalid shm-path '{}': {}", state.settings.shm_path, err]
                )
            })?;
            let writer =
                Writer::create(selected.backend.as_ref(), &selected.name, cfg).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenReadWrite,
                        [
                            "Failed to create shm writer at {} (resolved '{}'): {}",
                            state.settings.shm_path,
                            selected.name,
                            err
                        ]
                    )
                })?;

            writer.set_running();
            let writer = Arc::new(Mutex::new(writer));
            let tracker = Arc::new(Mutex::new(AllocTracker {
                writer: Some(writer.clone()),
                pending_by_ptr: HashMap::new(),
            }));
            let allocator = ShmArenaAllocator::with_tracker(tracker, 64);
            state.writer = Some(writer);
            state.allocator = Some(allocator);
            state.unlocked = false;

            if let Some(t) = state.hb_thread.take() {
                let _ = t.join();
            }
            if let Some(writer_arc) = state.writer.as_ref().cloned() {
                let stop = Arc::new(AtomicBool::new(false));
                state.hb_stop = Some(stop.clone());
                state.hb_thread = Some(std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok(w) = writer_arc.lock() {
                            w.producer_heartbeat_tick();
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }));
            }
            if let Some(t) = state.gc_thread.take() {
                let _ = t.join();
            }
            if let Some(writer_arc) = state.writer.as_ref().cloned() {
                let stop = Arc::new(AtomicBool::new(false));
                state.gc_stop = Some(stop.clone());
                state.gc_thread = Some(std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok(mut w) = writer_arc.lock() {
                            w.drain_recycles();
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }));
            }
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().expect("state poisoned");
            if let Some(stop) = state.hb_stop.take() {
                stop.store(true, Ordering::Relaxed);
            }
            if let Some(t) = state.hb_thread.take() {
                let _ = t.join();
            }
            if let Some(stop) = state.gc_stop.take() {
                stop.store(true, Ordering::Relaxed);
            }
            if let Some(t) = state.gc_thread.take() {
                let _ = t.join();
            }
            if let Some(writer) = &state.writer {
                if let Ok(w) = writer.lock() {
                    w.set_stopped();
                }
            }
            state.allocator = None;
            state.writer = None;
            state.unlocked = false;
            Ok(())
        }

        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let pts_ns = buffer.pts().map(|v| v.nseconds() as i64).unwrap_or(-1);
            let mut idle_no_consumer = 0u32;
            let mut idle_no_space = 0u32;

            loop {
                let state = self.state.lock().expect("state poisoned");
                if state.unlocked {
                    return Err(gst::FlowError::Flushing);
                }
                let wait_for_connection = state.settings.wait_for_connection;
                let timeout_ns = (state.settings.consumer_timeout_ms as u64) * 1_000_000;
                let writer = state
                    .writer
                    .as_ref()
                    .cloned()
                    .ok_or(gst::FlowError::Flushing)?;
                let allocator = state.allocator.as_ref().cloned();

                let online = {
                    let w = writer.lock().map_err(|_| gst::FlowError::Error)?;
                    w.is_consumer_online(timeout_ns)
                };

                if wait_for_connection && !online {
                    drop(state);
                    poll_yield_sleep(&mut idle_no_consumer, Duration::from_millis(5));
                    continue;
                }

                {
                    let mut w = writer.lock().map_err(|_| gst::FlowError::Error)?;
                    let startup = w.startup_snapshot();
                    if startup.state != STARTUP_RUNNING {
                        if startup.state == STARTUP_SRC_READY {
                            eprintln!(
                                "[shm2] startup: src ready (gen {}), switching to RUNNING",
                                startup.generation
                            );
                            let _ = w.set_startup_state(startup.generation, STARTUP_RUNNING);
                        } else {
                            eprintln!(
                                "[shm2] startup: waiting (gen {} state {})",
                                startup.generation, startup.state
                            );
                        }
                        w.emit_timeline_snapshot(pts_ns);
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }

                // Sink fast path: upstream memory came from our SHM allocator.
                let mut fast_published = false;
                if let Some(alloc) = allocator.as_ref() {
                    if buffer.n_memory() == 1 {
                        let mem0 = buffer.peek_memory(0);
                        if let Some(mem_alloc) = mem0.allocator() {
                            if mem_alloc == alloc.upcast_ref::<gst::Allocator>() {
                                if let Some(tracker) = alloc
                                    .imp()
                                    .tracker
                                    .lock()
                                    .expect("tracker mutex poisoned")
                                    .as_ref()
                                    .cloned()
                                {
                                    if let Some(lease) = tracker
                                        .lock()
                                        .expect("tracker mutex poisoned")
                                        .mark_published(
                                            map.as_slice().as_ptr(),
                                            map.as_slice().len(),
                                        )
                                    {
                                        let mut w =
                                            writer.lock().map_err(|_| gst::FlowError::Error)?;
                                        match w.publish_lease(lease, pts_ns) {
                                            Ok(_) => fast_published = true,
                                            Err(crate::platform::ShmError::NoConsumer)
                                                if wait_for_connection =>
                                            {
                                                drop(state);
                                                poll_yield_sleep(
                                                    &mut idle_no_consumer,
                                                    Duration::from_millis(5),
                                                );
                                                continue;
                                            }
                                            Err(crate::platform::ShmError::Exhausted) => {
                                                drop(state);
                                                poll_yield_sleep(
                                                    &mut idle_no_space,
                                                    Duration::from_millis(1),
                                                );
                                                continue;
                                            }
                                            Err(_) => return Err(gst::FlowError::Error),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if fast_published {
                    return Ok(gst::FlowSuccess::Ok);
                }

                let mut w = writer.lock().map_err(|_| gst::FlowError::Error)?;
                match w.publish(map.as_slice(), pts_ns) {
                    Ok(_) => return Ok(gst::FlowSuccess::Ok),
                    Err(crate::platform::ShmError::Exhausted) => {
                        drop(state);
                        poll_yield_sleep(&mut idle_no_space, Duration::from_millis(1));
                    }
                    Err(crate::platform::ShmError::NoConsumer) if wait_for_connection => {
                        drop(state);
                        poll_yield_sleep(&mut idle_no_consumer, Duration::from_millis(5));
                    }
                    Err(_) => return Err(gst::FlowError::Error),
                }
            }
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

        fn propose_allocation(
            &self,
            query: &mut gst::query::Allocation,
        ) -> Result<(), gst::LoggableError> {
            let state = self.state.lock().expect("state poisoned");
            if let Some(allocator) = &state.allocator {
                query.add_allocation_param(Some(allocator), gst::AllocationParams::default());
            }
            Ok(())
        }
    }
}

glib::wrapper! {
    pub struct Shm2Sink(ObjectSubclass<imp::Shm2Sink>) @extends gst_base::BaseSink, gst::Element, gst::Object;
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    gst::Element::register(plugin, "shm2sink", gst::Rank::NONE, Shm2Sink::static_type())
}
