pub mod allocator;
pub mod platform;
pub mod transport;

mod shm2sink;
mod shm2src;

use gstreamer as gst;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    shm2sink::register(Some(plugin))?;
    shm2src::register(Some(plugin))?;
    Ok(())
}

gst::plugin_define!(
    shm2,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "MIT",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY")
);
