use std::slice;
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
use crate::transport::{Reader, ReceivedDesc};

type ReaderType = Reader;

#[cfg(unix)]
const DEFAULT_PATH: &str = "/dev/shm/gst-shm2-default";
#[cfg(windows)]
const DEFAULT_PATH: &str = "winshm://Local/gst-shm2-default";

#[derive(Debug)]
struct Settings {
    shm_path: String,
    is_live: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shm_path: DEFAULT_PATH.to_string(),
            is_live: false,
        }
    }
}

#[derive(Default)]
struct State {
    settings: Settings,
    reader: Option<Arc<Mutex<ReaderType>>>,
    unlocked: bool,
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
                _ => unreachable!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let state = self.state.lock().expect("state poisoned");
            match pspec.name() {
                "shm-path" => state.settings.shm_path.to_value(),
                "is-live" => state.settings.is_live.to_value(),
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
            self.obj().set_live(state.settings.is_live);
            state.reader = Some(Arc::new(Mutex::new(reader)));
            state.unlocked = false;
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
            let (desc, ptr) = loop {
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
                        break (desc, ptr);
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
                if desc.pts_ns >= 0 {
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
