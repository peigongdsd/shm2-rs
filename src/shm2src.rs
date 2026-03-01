use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;
use once_cell::sync::Lazy;

use crate::platform::posix_file::PosixFileBackend;
use crate::transport::Reader;

type ReaderType = Reader<PosixFileBackend>;

const DEFAULT_PATH: &str = "/dev/shm/gst-shm2-default";

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
    reader: Option<ReaderType>,
    unlocked: bool,
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
            let backend = PosixFileBackend;
            let reader = Reader::open(&backend, &state.settings.shm_path).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    [
                        "Failed to open shm reader at {}: {}",
                        state.settings.shm_path,
                        err
                    ]
                )
            })?;
            self.obj().set_live(state.settings.is_live);
            state.reader = Some(reader);
            state.unlocked = false;
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().expect("state poisoned");
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
            let mut state = self.state.lock().expect("state poisoned");
            if state.unlocked {
                return Err(gst::FlowError::Flushing);
            }
            let reader = state.reader.as_mut().ok_or(gst::FlowError::Flushing)?;
            let received = reader.recv_blocking().map_err(|_| gst::FlowError::Error)?;
            reader
                .recycle(&received)
                .map_err(|_| gst::FlowError::Error)?;

            let mut out = gst::Buffer::from_mut_slice(received.payload);
            if let Some(buf) = out.get_mut() {
                if received.pts_ns >= 0 {
                    buf.set_pts(gst::ClockTime::from_nseconds(received.pts_ns as u64));
                }
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
