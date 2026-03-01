use std::thread;
use std::time::Duration;

use gstshm2::platform::posix_file::PosixFileBackend;
use gstshm2::transport::Reader;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/shm/gst-shm2-demo".to_string());
    let max = std::env::args()
        .nth(2)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(5000);

    let backend = PosixFileBackend;

    let mut reader = loop {
        match Reader::open(&backend, &path) {
            Ok(r) => break r,
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    };
    reader.claim_consumer(std::process::id())?;

    println!("consumer: attached to {path}");

    for i in 0..max {
        let buf = reader.recv_blocking()?;
        if i % 500 == 0 {
            let preview = String::from_utf8_lossy(&buf.payload);
            println!(
                "consumer: seq={} id={} bytes={} preview='{}'",
                buf.seq,
                buf.buffer_id,
                buf.payload.len(),
                preview
            );
        }
        reader.recycle(&buf)?;
    }

    println!("consumer: done");
    reader.release_consumer(std::process::id());
    Ok(())
}
