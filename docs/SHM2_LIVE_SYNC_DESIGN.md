# SHM2 Live-Only Low-Latency Design (Sink-Clock Authoritative)

Status: Proposed redesign (live-only, frame-drop allowed, sink-clock authoritative).  
Target: Rust GStreamer plugin (shm2sink/shm2src) over one shared-memory region.

## Goals
1. Live-only, lowest latency. Frame dropping is allowed to keep latency low.
2. Sink clock is the only trusted clock. Consumer must sync to sink clock.
3. Sink GC of recycle ring runs in a separate worker thread.
4. If arena is full: drop oldest published frames first; if empty, reset arena.
5. On consumer attach: purge backlog, reset sink clock, initial clock sync.

## High-Level Transport Flow
- **Single shared-memory region** contains header, ready ring, recycle ring, arena.
- **Sink** publishes buffers into ready ring and handles allocator.
- **Src** consumes descriptors, wraps SHM memory, and recycles via recycle ring.
- **Sink clock** is pushed to consumer via header snapshots.

## Protocol Fields (Header)
Existing fields (already in codebase):
- `timeline_sink_mono_ns` (sink monotonic timestamp)
- `timeline_producer_pts_ns` (producer PTS for the published buffer)
- `timeline_gen` (generation number)
- `timeline_valid` (snapshot valid)
- `timeline_seq` (snapshot sequence)
- `ready_head`, `ready_tail` (ready ring)
- `rec_head`, `rec_tail` (recycle ring)

## Sink (Producer) Behavior
### 1) Attach Handling
- On consumer attach:
  - `ready_tail = ready_head` (purge backlog)
  - `timeline_gen++`
  - `timeline_valid = 0`

### 2) Timeline Beacon
- On each publish, or periodically (e.g. every 200ms):
  - write `timeline_sink_mono_ns = now()`
  - write `timeline_producer_pts_ns = buffer_pts` (from upstream)
  - set `timeline_valid = 1`
  - increment `timeline_seq`

### 3) GC Worker Thread (Dedicated)
- Runs continuously; drains recycle ring:
  - Read recycle entries
  - Free arena allocations (ignore invalid/duplicate frees)
  - Updates arena usage metrics
- Not in render hot path.

### 4) Arena Full Policy (Producer)
When allocation fails:
1. Drop **oldest ready ring entries** and free their buffers (advance `ready_tail`).
2. Retry allocation.
3. If ready ring is empty and still full:
   - Reset arena (drop all outstanding allocations)
   - `timeline_gen++` and `timeline_valid=0`

## Src (Consumer) Behavior
### 1) Live-only, Latest Frame
- Vsync-friendly live policy:
  - If `ready_len == 1`: consume the only entry.
  - If `ready_len == 2`: consume the oldest entry.
  - If `ready_len >= 3`: drop the oldest entry, then consume the next (2nd).
- Emit recycle entries for dropped frames (best-effort).

### 2) Sink-Clock Anchored Timestamps (Detailed)
**Key rule:** The pipeline clock is never changed. Only buffer PTS are adjusted to the sink timeline.

State held by src:
- `offset_ns` (sink time offset)
- `offset_valid` (bool)
- `expected_gen` (timeline generation)
- `last_snapshot_seq` (avoid duplicate updates)

**Offset derivation (from sink snapshot):**
- `offset_ns = timeline_sink_mono_ns - timeline_producer_pts_ns`

**PTS generation for each frame:**
- `out_pts = desc.pts_ns + offset_ns`
- `duration = frame_duration`
- If any frames were dropped since the last output, set `DISCONT`.

### 3) Attach / Reattach (Clock Sync)
On `timeline_gen` change:
1. Purge backlog (`ready_tail = ready_head` on sink, src treats as empty).
2. Set `offset_valid = false`.
3. Wait for the next **valid** sink snapshot.
4. Set `offset_ns` once, mark `offset_valid = true`.
5. Mark first frame as `DISCONT`.

### 4) Periodic Drift Correction (Slew Only)
When a new snapshot arrives (`timeline_seq` changes):
1. Compute `new_offset`.
2. Slew `offset_ns` toward `new_offset` by a small step (e.g. max ±2ms).
3. Ignore tiny errors to avoid jitter.

This avoids PTS jumps and keeps `sync=true` sinks stable.

## Why This Design Works
- **Low latency:** always newest frame, older frames dropped aggressively.
- **Cross-system:** only sink clock is trusted; consumer aligns via offset.
- **Stable scheduling:** sink provides authoritative time; consumer outputs PTS in sink time.
- **Allocator health:** recycle handled by GC thread + drop-oldest policy.
- **Reattach safety:** backlog flushed and timeline reset to prevent burst drain.

## Implementation Notes
- Use **poll-only** to remain ivshmem-compatible.
- Optional futex/doorbell notifications can be added later.
- Clock sync must be deterministic and not depend on local monotonic time.

## Future Work
- Add explicit "timestamp-mode" property (sink-anchored vs local).
- Add metrics for drop counts and recycle latency.
- Add automated integration test for attach/detach with sink-clock sync.
