use std::thread;
use std::time::Duration;

use gstshm2::platform::ShmError;
use gstshm2::platform::posix_file::PosixFileBackend;
use gstshm2::transport::{TransportConfig, Writer};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/shm/gst-shm2-demo".to_string());
    let count = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(5000);

    let mut cfg = TransportConfig::default();
    cfg.total_size = 64 * 1024 * 1024;

    let backend = PosixFileBackend;
    let mut writer = Writer::create(&backend, &path, cfg)?;
    writer.set_running();

    println!(
        "producer: writing to {path} (region={} bytes)",
        writer.region_size()
    );

    for i in 0..count {
        let payload = format!("frame={i};hello=shm-only;transport=gst-shm2").into_bytes();
        loop {
            match writer.publish(&payload, i as i64 * 1_000_000) {
                Ok(buffer_id) => {
                    if i % 500 == 0 {
                        println!("producer: published seq={i} id={buffer_id}");
                    }
                    break;
                }
                Err(ShmError::Exhausted) => {
                    writer.drain_recycles();
                    thread::sleep(Duration::from_millis(1));
                }
                Err(err) => return Err(err.into()),
            }
        }
        if i % 16 == 0 {
            writer.drain_recycles();
        }
    }

    // Give consumer time to return outstanding buffers.
    for _ in 0..1000 {
        writer.drain_recycles();
        thread::sleep(Duration::from_millis(1));
    }

    writer.set_stopped();
    println!("producer: done");
    Ok(())
}
