# shm2-rs

SHM-only transport prototype for GStreamer, designed to communicate exclusively through a single shared-memory region (for example `/dev/shm/gst-shm2-demo`) with no control socket.

## Current Status

Implemented:
- Shared-memory transport core with:
  - fixed header
  - producer->consumer ready ring
  - consumer->producer recycle ring
  - producer-side free-list allocator
- OS backend abstraction (`platform`), with Linux POSIX shared-file backend implemented.
- Two standalone test binaries:
  - `shm2_producer`
  - `shm2_consumer`
- Initial GStreamer plugin wiring:
  - `shm2sink` (BaseSink)
  - `shm2src` (PushSrc)

Current plugin status:
- `shm2src`: **zero-copy output path implemented** (SHM-backed `GstMemory` + recycle on memory drop).
- `shm2sink`: still uses copy-path from incoming upstream buffers into SHM (sink-side upstream allocator fast path not yet implemented).

## Repository Layout

- `src/platform/`: OS abstraction + Linux backend
- `src/transport.rs`: shared protocol, rings, writer/reader
- `src/allocator.rs`: producer allocator
- `src/shm2sink.rs`: sink element
- `src/shm2src.rs`: source element
- `src/bin/shm2_producer.rs`: transport smoke-test producer
- `src/bin/shm2_consumer.rs`: transport smoke-test consumer

## Development Environment (Nix)

```bash
cd /home/krusl/code/gst-shm/shm2-rs
nix develop
```

## Build and Test

```bash
cargo fmt
cargo build --bins
cargo build --lib
cargo test
```

## Run Transport Smoke Test

Terminal 1:
```bash
cargo run --bin shm2_consumer -- /dev/shm/gst-shm2-demo 2000
```

Terminal 2:
```bash
cargo run --bin shm2_producer -- /dev/shm/gst-shm2-demo 2000
```

## Plugin Discovery

After build, plugin is produced as `target/debug/libgstshm2.so`.

```bash
GST_PLUGIN_PATH=$PWD/target/debug gst-inspect-1.0 shm2sink
GST_PLUGIN_PATH=$PWD/target/debug gst-inspect-1.0 shm2src
```

## Pipeline Usage (Current `shm2sink` / `shm2src`)

Use `GST_PLUGIN_PATH` so GStreamer can find `libgstshm2.so`:

```bash
export GST_PLUGIN_PATH=$PWD/target/debug
```

Terminal 1 (producer pipeline):

```bash
gst-launch-1.0 -v \
  videotestsrc is-live=true pattern=ball ! \
  video/x-raw,format=I420,width=320,height=240,framerate=30/1 ! \
  shm2sink shm-path=/dev/shm/gst-shm2-pipe
```

Terminal 2 (consumer pipeline):

```bash
gst-launch-1.0 -v \
  shm2src shm-path=/dev/shm/gst-shm2-pipe is-live=true ! \
  queue ! videoconvert ! autovideosink
```

Audio example:

Terminal 1:
```bash
gst-launch-1.0 -v \
  audiotestsrc is-live=true wave=sine ! \
  audio/x-raw,format=S16LE,channels=2,rate=48000 ! \
  shm2sink shm-path=/dev/shm/gst-shm2-audio
```

Terminal 2:
```bash
gst-launch-1.0 -v \
  shm2src shm-path=/dev/shm/gst-shm2-audio is-live=true ! \
  queue ! audioconvert ! audioresample ! autoaudiosink
```

Notes:
- Start producer before consumer with current startup behavior (`shm2src` expects SHM region to exist at start).
- `shm2src` is zero-copy on output; `shm2sink` is still copy-path on input.
- `shm-path` must point to the same shared-memory file on both sides.

## Limitations (Known)

- No custom GstAllocator/propose-allocation sink fast path yet (upstream->sink still copied into SHM).
- `shm2src` currently requires producer/SHM file to exist when source starts.
- No full stress/fault-recovery automated test matrix yet.

## Next Steps

1. Implement zero-copy descriptor path in `Reader`.
2. Add custom GstMemory/finalizer recycling in `shm2src`.
3. Add custom allocator + `propose_allocation` in `shm2sink`.
4. Add integration tests for real pipelines and crash/restart behavior.
