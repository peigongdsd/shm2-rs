# shm2-rs

SHM-only transport prototype for GStreamer, designed to communicate exclusively through a single shared-memory region (for example `shm:///dev/shm/gst-shm2-demo` on Linux or `winshm://Local/gst-shm2-demo` on Windows) with no control socket.

## Current Status

Implemented:
- Shared-memory transport core with:
  - fixed header
  - producer->consumer ready ring
  - consumer->producer recycle ring
  - producer-side free-list allocator
- OS backend abstraction (`platform`) with runtime backend selection from `shm-path`.
- Linux POSIX shared-file backend.
- Windows named shared-memory backend (`winshm://...`).
- Two standalone test binaries:
  - `shm2_producer`
  - `shm2_consumer`
- Initial GStreamer plugin wiring:
  - `shm2sink` (BaseSink)
  - `shm2src` (PushSrc)

Current plugin status:
- `shm2src`: **zero-copy output path implemented** (SHM-backed `GstMemory` + recycle on memory drop).
- `shm2sink`: **upstream zero-copy fast path implemented** via `propose_allocation` + custom allocator, with copy fallback for non-cooperating upstream memory.
- No consumer ownership/heartbeat gating: sink always publishes into SHM and drops/overwrites when full.

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

## shm-path URI Schemes

`shm-path` is backend-aware and supports URI-like schemes:

- Linux POSIX file backend:
  - `shm:///dev/shm/gst-shm2-demo`
  - backward-compatible plain path also works: `/dev/shm/gst-shm2-demo`
- Windows named shared memory backend:
  - `winshm://Local/gst-shm2-demo` (recommended default)
  - `winshm://Global/gst-shm2-demo` (may require elevated privilege/service context)
- Windows ivshmem backend (guest attach to existing shared BAR2 region):
  - `ivshmem://PCI\VEN_1AF4&DEV_1110&SUBSYS_11001AF4&REV_01\3&11583659&0&88`
  - `ivshmem://\\?\PCI#VEN_1AF4&DEV_1110&SUBSYS_11001AF4&REV_01#3&11583659&0&88#{df576976-569d-4672-95a0-f57e4ea0b210}`

Both producer and consumer must use the exact same `shm-path` value.
For `ivshmem://`, the backend is attach-only (reader/open side): use when a host-side daemon already owns and feeds the shared region.

## Run Transport Smoke Test

Linux (terminal 1):
```bash
cargo run --bin shm2_consumer -- shm:///dev/shm/gst-shm2-demo 2000
```

Linux (terminal 2):
```bash
cargo run --bin shm2_producer -- shm:///dev/shm/gst-shm2-demo 2000
```

Windows (PowerShell window 1):
```powershell
cargo run --bin shm2_consumer -- winshm://Local/gst-shm2-demo 2000
```

Windows (PowerShell window 2):
```powershell
cargo run --bin shm2_producer -- winshm://Local/gst-shm2-demo 2000
```

## shm2_relayd (Socket-Driven Wrapper)

`shm2_relayd` mirrors `v4l2-relayd` behavior using a TCP/vsock listener instead of V4L2 client usage events.
The output pipeline is fixed (appsrc → shm2sink). When no clients are connected, the input pipeline is set to `NULL` for power efficiency.

Usage:

```bash
cargo run --bin shm2_relayd -- \
  --listen tcp://0.0.0.0:5555 \
  --shm-path shm:///dev/shm/gst-shm2-pipe \
  --shm-size 67108864 \
  --input "videotestsrc is-live=true pattern=ball ! videoconvert"
```

Optional splash (runs only when no clients):

```bash
cargo run --bin shm2_relayd -- \
  --listen tcp://0.0.0.0:5555 \
  --shm-path shm:///dev/shm/gst-shm2-pipe \
  --shm-size 67108864 \
  --input "v4l2src ! videoconvert" \
  --splash "videotestsrc is-live=true pattern=black ! videoconvert"
```

Notes:
- Output pipeline is always PLAYING; input pipeline toggles on client connect/disconnect.
- `--shm-path` is the only output-side knob.
- For Linux vsock: `--listen vsock://CID:PORT`.

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
  shm2sink shm-path=shm:///dev/shm/gst-shm2-pipe
```

Terminal 2 (consumer pipeline):

```bash
gst-launch-1.0 -v \
  shm2src shm-path=shm:///dev/shm/gst-shm2-pipe is-live=true ! \
  queue ! videoconvert ! autovideosink
```

### Properties (current)

`shm2sink`:
- `shm-path` (string)
- `shm-size` (u64)
- `perms` (u32)
- `timeline-beacon-ms` (u32)

`shm2src`:
- `shm-path` (string)
- `is-live` (bool)
- `live-only` (bool): restart output timeline on attach/re-attach
- `latest-only` (bool): low-latency read policy (second-newest when available)

Audio example:

Terminal 1:
```bash
gst-launch-1.0 -v \
  audiotestsrc is-live=true wave=sine ! \
  audio/x-raw,format=S16LE,channels=2,rate=48000 ! \
  shm2sink shm-path=shm:///dev/shm/gst-shm2-audio
```

Terminal 2:
```bash
gst-launch-1.0 -v \
  shm2src shm-path=shm:///dev/shm/gst-shm2-audio is-live=true ! \
  queue ! audioconvert ! audioresample ! autoaudiosink
```

Notes:
- Start producer before consumer (`shm2src` expects SHM region to exist at start).
- `shm2src` is zero-copy on output.
- `shm2sink` uses zero-copy fast path when upstream adopts the proposed allocator; otherwise it falls back to copy.
- `shm-path` must resolve to the same shared-memory region on both sides.
- Sink never blocks for a consumer; when the arena fills it drops the oldest ready frame, and if the ready ring is empty it resets the arena.
- Multiple readers are undefined behavior (no consumer ownership enforcement).

## Limitations (Known)

- `shm2src` currently requires producer/SHM region to exist when source starts.
- No full stress/fault-recovery automated CI matrix yet.
- ivshmem backend currently targets Windows guest attach/open path; sink/create path is intentionally unsupported.
