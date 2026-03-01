use std::sync::Mutex;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;
use gstreamer as gst;
use gstreamer_base as gst_base;
use once_cell::sync::Lazy;

use crate::platform::posix_file::PosixFileBackend;
use crate::transport::{TransportConfig, Writer};

type WriterType = Writer<PosixFileBackend>;

const DEFAULT_PATH: &str = "/dev/shm/gst-shm2-default";

#[derive(Debug)]
struct Settings {
    shm_path: String,
    shm_size: u64,
    perms: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shm_path: DEFAULT_PATH.to_string(),
            shm_size: 64 * 1024 * 1024,
            perms: 0o660,
        }
    }
}

#[derive(Default)]
struct State {
    settings: Settings,
    writer: Option<WriterType>,
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
                _ => unreachable!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            let state = self.state.lock().expect("state poisoned");
            match pspec.name() {
                "shm-path" => state.settings.shm_path.to_value(),
                "shm-size" => state.settings.shm_size.to_value(),
                "perms" => state.settings.perms.to_value(),
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
                ..Default::default()
            };

            let backend = PosixFileBackend;
            let writer =
                Writer::create(&backend, &state.settings.shm_path, cfg).map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenReadWrite,
                        [
                            "Failed to create shm writer at {}: {}",
                            state.settings.shm_path,
                            err
                        ]
                    )
                })?;

            writer.set_running();
            state.writer = Some(writer);
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            let mut state = self.state.lock().expect("state poisoned");
            if let Some(writer) = &state.writer {
                writer.set_stopped();
            }
            state.writer = None;
            Ok(())
        }

        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let pts_ns = buffer.pts().map(|v| v.nseconds() as i64).unwrap_or(-1);

            let mut state = self.state.lock().expect("state poisoned");
            let writer = state.writer.as_mut().ok_or(gst::FlowError::Flushing)?;
            writer
                .publish(map.as_slice(), pts_ns)
                .map_err(|_| gst::FlowError::Error)?;
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

glib::wrapper! {
    pub struct Shm2Sink(ObjectSubclass<imp::Shm2Sink>) @extends gst_base::BaseSink, gst::Element, gst::Object;
}

pub fn register(plugin: Option<&gst::Plugin>) -> Result<(), glib::BoolError> {
    gst::Element::register(plugin, "shm2sink", gst::Rank::NONE, Shm2Sink::static_type())
}
