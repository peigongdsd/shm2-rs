use std::mem::size_of;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::allocator::FreeListAllocator;
use crate::platform::{SharedRegion, ShmBackend, ShmError};

pub const MAGIC: [u8; 8] = *b"GSTSHM2\0";
pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 0;

const STATE_INIT: u32 = 0;
const STATE_RUNNING: u32 = 1;
const STATE_STOPPING: u32 = 2;
const STATE_STOPPED: u32 = 3;

const HEADER_PAD: usize = 4096;
const DESC_ALIGN: usize = 64;

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

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadyDesc {
    pub seq: u64,
    pub offset: u64,
    pub length: u32,
    pub buffer_id: u32,
    pub pts_ns: i64,
    pub dts_ns: i64,
    pub duration_ns: i64,
    pub flags: u32,
    pub checksum: u32,
    pub reserved: [u8; 8],
}

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug)]
pub struct RecycleDesc {
    pub seq: u64,
    pub buffer_id: u32,
    pub status: u32,
    pub offset: u64,
    pub length: u32,
    pub reserved: [u8; 36],
}

impl Default for RecycleDesc {
    fn default() -> Self {
        Self {
            seq: 0,
            buffer_id: 0,
            status: 0,
            offset: 0,
            length: 0,
            reserved: [0u8; 36],
        }
    }
}

#[repr(C)]
pub struct SharedHeader {
    pub magic: [u8; 8],
    pub version_major: u16,
    pub version_minor: u16,
    pub header_size: u32,
    pub total_size: u64,
    pub features: u64,
    pub state: AtomicU32,
    pub owner_pid: u32,
    pub consumer_owner_pid: AtomicU32,
    pub epoch: AtomicU64,
    pub producer_heartbeat_ns: AtomicU64,
    pub consumer_heartbeat_ns: AtomicU64,

    pub ready_capacity: u32,
    pub ready_entry_size: u32,
    pub ready_head: AtomicU64,
    pub ready_tail: AtomicU64,

    pub rec_capacity: u32,
    pub rec_entry_size: u32,
    pub rec_head: AtomicU64,
    pub rec_tail: AtomicU64,

    pub ready_offset: u64,
    pub recycle_offset: u64,
    pub arena_offset: u64,
    pub arena_size: u64,

    pub arena_used_bytes: AtomicU64,
    pub arena_high_watermark: AtomicU64,
    pub alloc_failures: AtomicU64,

    pub wait_for_connection: AtomicU32,
    pub drop_when_no_consumer: AtomicU32,
    pub notify_mode: AtomicU32,
}

pub struct TransportConfig {
    pub total_size: usize,
    pub ready_capacity: u32,
    pub recycle_capacity: u32,
    pub perms: u32,
    pub wait_for_connection: bool,
    pub drop_when_no_consumer: bool,
    pub allocator_align: u64,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            total_size: 64 * 1024 * 1024,
            ready_capacity: 4096,
            recycle_capacity: 4096,
            perms: 0o660,
            wait_for_connection: true,
            drop_when_no_consumer: false,
            allocator_align: 64,
        }
    }
}

pub struct Writer {
    region: Box<dyn SharedRegion>,
    hdr: &'static SharedHeader,
    ready: &'static mut [ReadyDesc],
    recycle: &'static mut [RecycleDesc],
    arena: *mut u8,
    allocator: FreeListAllocator,
    allocator_align: u64,
}

pub struct Reader {
    _region: Box<dyn SharedRegion>,
    hdr: &'static SharedHeader,
    ready: &'static mut [ReadyDesc],
    recycle: &'static mut [RecycleDesc],
    arena: *mut u8,
}

// Writer/Reader are guarded externally by element mutexes.
unsafe impl Send for Writer {}
unsafe impl Send for Reader {}

#[derive(Debug)]
pub struct ReceivedBuffer {
    pub seq: u64,
    pub buffer_id: u32,
    pub pts_ns: i64,
    pub dts_ns: i64,
    pub duration_ns: i64,
    pub flags: u32,
    pub payload: Vec<u8>,
    pub offset: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ReceivedDesc {
    pub seq: u64,
    pub buffer_id: u32,
    pub pts_ns: i64,
    pub dts_ns: i64,
    pub duration_ns: i64,
    pub flags: u32,
    pub offset: u64,
    pub len: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct AllocLease {
    pub buffer_id: u32,
    pub offset: u64,
    pub len: u32,
    pub ptr: *mut u8,
}

impl Writer {
    pub fn create(
        backend: &dyn ShmBackend,
        path: &str,
        cfg: TransportConfig,
    ) -> Result<Self, ShmError> {
        if cfg.ready_capacity == 0 || cfg.recycle_capacity == 0 {
            return Err(ShmError::InvalidConfig("ring capacities must be non-zero"));
        }

        let region = backend.create(path, cfg.total_size, cfg.perms)?;
        let (hdr, ready, recycle, arena_ptr, arena_size) = unsafe {
            map_layout(
                region.as_ptr().as_ptr(),
                region.len(),
                cfg.ready_capacity,
                cfg.recycle_capacity,
            )?
        };

        unsafe {
            std::ptr::write_bytes(region.as_ptr().as_ptr(), 0, region.len());
        }

        init_header(
            hdr,
            cfg.total_size as u64,
            cfg.ready_capacity,
            cfg.recycle_capacity,
            ready.as_ptr() as usize - region.as_ptr().as_ptr() as usize,
            recycle.as_ptr() as usize - region.as_ptr().as_ptr() as usize,
            arena_ptr as usize - region.as_ptr().as_ptr() as usize,
            arena_size,
            cfg.wait_for_connection,
            cfg.drop_when_no_consumer,
        );

        Ok(Self {
            region,
            hdr,
            ready,
            recycle,
            arena: arena_ptr,
            allocator: FreeListAllocator::new(arena_size),
            allocator_align: cfg.allocator_align,
        })
    }

    pub fn publish(&mut self, payload: &[u8], pts_ns: i64) -> Result<u32, ShmError> {
        let lease = self.alloc_lease(payload.len() as u32, self.allocator_align)?;
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), lease.ptr, payload.len());
        }
        if let Err(err) = self.publish_lease(lease, pts_ns) {
            let _ = self.free_lease(lease.buffer_id);
            return Err(err);
        }
        Ok(lease.buffer_id)
    }

    pub fn alloc_lease(&mut self, size: u32, align: u64) -> Result<AllocLease, ShmError> {
        self.drain_recycles();
        let alloc = self.allocator.alloc(size, align).ok_or_else(|| {
            self.hdr.alloc_failures.fetch_add(1, Ordering::Relaxed);
            ShmError::Exhausted
        })?;
        let ptr = unsafe { self.arena.add(alloc.offset as usize) };
        Ok(AllocLease {
            buffer_id: alloc.buffer_id,
            offset: alloc.offset,
            len: alloc.len,
            ptr,
        })
    }

    pub fn free_lease(&mut self, buffer_id: u32) -> bool {
        let freed = self.allocator.free_by_id(buffer_id);
        self.hdr
            .arena_used_bytes
            .store(self.allocator.used_bytes(), Ordering::Relaxed);
        freed
    }

    pub fn publish_lease(&mut self, lease: AllocLease, pts_ns: i64) -> Result<(), ShmError> {
        if !self.is_consumer_online(1_000_000_000) {
            return Err(ShmError::NoConsumer);
        }
        let mut idle_cycles = 0u32;
        loop {
            let head = self.hdr.ready_head.load(Ordering::Relaxed);
            let tail = self.hdr.ready_tail.load(Ordering::Acquire);
            let cap = u64::from(self.hdr.ready_capacity);
            if head.wrapping_sub(tail) < cap {
                let idx = (head % cap) as usize;
                self.ready[idx] = ReadyDesc {
                    seq: head + 1,
                    offset: lease.offset,
                    length: lease.len,
                    buffer_id: lease.buffer_id,
                    pts_ns,
                    dts_ns: pts_ns,
                    duration_ns: 0,
                    flags: 0,
                    checksum: 0,
                    reserved: [0u8; 8],
                };
                self.hdr.ready_head.store(head + 1, Ordering::Release);
                self.touch_producer_heartbeat();
                self.update_usage_metrics();
                return Ok(());
            }
            self.drain_recycles();
            poll_yield_sleep(&mut idle_cycles, Duration::from_millis(1));
        }
    }

    pub fn drain_recycles(&mut self) {
        let cap = u64::from(self.hdr.rec_capacity);
        loop {
            let head = self.hdr.rec_head.load(Ordering::Acquire);
            let tail = self.hdr.rec_tail.load(Ordering::Relaxed);
            if head == tail {
                break;
            }
            let idx = (tail % cap) as usize;
            let rec = self.recycle[idx];
            let _ = self.allocator.free_by_id(rec.buffer_id);
            self.hdr.rec_tail.store(tail + 1, Ordering::Release);
        }
        self.hdr
            .arena_used_bytes
            .store(self.allocator.used_bytes(), Ordering::Relaxed);
        self.touch_producer_heartbeat();
    }

    pub fn set_running(&self) {
        self.hdr.state.store(STATE_RUNNING, Ordering::Release);
    }

    pub fn set_stopped(&self) {
        self.hdr.state.store(STATE_STOPPING, Ordering::Release);
        self.hdr.state.store(STATE_STOPPED, Ordering::Release);
    }

    pub fn region_size(&self) -> usize {
        self.region.len()
    }

    pub fn is_consumer_online(&self, timeout_ns: u64) -> bool {
        let owner = self.hdr.consumer_owner_pid.load(Ordering::Acquire);
        if owner == 0 {
            return false;
        }
        let hb = self.hdr.consumer_heartbeat_ns.load(Ordering::Acquire);
        if hb == 0 {
            return false;
        }
        now_nanos().saturating_sub(hb) <= timeout_ns
    }

    fn touch_producer_heartbeat(&self) {
        self.hdr
            .producer_heartbeat_ns
            .store(now_nanos(), Ordering::Relaxed);
    }

    fn update_usage_metrics(&self) {
        let used = self.allocator.used_bytes();
        self.hdr.arena_used_bytes.store(used, Ordering::Relaxed);
        let mut prev_hw = self.hdr.arena_high_watermark.load(Ordering::Relaxed);
        while used > prev_hw {
            match self.hdr.arena_high_watermark.compare_exchange_weak(
                prev_hw,
                used,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(v) => prev_hw = v,
            }
        }
    }
}

impl Reader {
    pub fn open(backend: &dyn ShmBackend, path: &str) -> Result<Self, ShmError> {
        let region = backend.open(path)?;

        let hdr = unsafe { &*(region.as_ptr().as_ptr() as *const SharedHeader) };
        validate_header(hdr, region.len() as u64)?;

        let ready = unsafe {
            std::slice::from_raw_parts_mut(
                region.as_ptr().as_ptr().add(hdr.ready_offset as usize) as *mut ReadyDesc,
                hdr.ready_capacity as usize,
            )
        };

        let recycle = unsafe {
            std::slice::from_raw_parts_mut(
                region.as_ptr().as_ptr().add(hdr.recycle_offset as usize) as *mut RecycleDesc,
                hdr.rec_capacity as usize,
            )
        };

        let arena = unsafe { region.as_ptr().as_ptr().add(hdr.arena_offset as usize) };

        Ok(Self {
            _region: region,
            hdr,
            ready,
            recycle,
            arena,
        })
    }

    pub fn recv_blocking(&mut self) -> Result<ReceivedBuffer, ShmError> {
        let mut idle_cycles = 0u32;
        loop {
            if let Some(desc) = self.try_recv_desc()? {
                let ptr = self.payload_ptr(&desc)?;
                let mut payload = vec![0u8; desc.len as usize];
                unsafe {
                    std::ptr::copy_nonoverlapping(ptr, payload.as_mut_ptr(), payload.len());
                }
                return Ok(ReceivedBuffer {
                    seq: desc.seq,
                    buffer_id: desc.buffer_id,
                    pts_ns: desc.pts_ns,
                    dts_ns: desc.dts_ns,
                    duration_ns: desc.duration_ns,
                    flags: desc.flags,
                    payload,
                    offset: desc.offset,
                });
            }
            self.touch_consumer_heartbeat();
            poll_yield_sleep(&mut idle_cycles, Duration::from_millis(1));
        }
    }

    pub fn recv_desc_blocking(&mut self) -> Result<ReceivedDesc, ShmError> {
        let mut idle_cycles = 0u32;
        loop {
            if let Some(desc) = self.try_recv_desc()? {
                return Ok(desc);
            }
            self.touch_consumer_heartbeat();
            poll_yield_sleep(&mut idle_cycles, Duration::from_millis(1));
        }
    }

    pub fn try_recv_desc(&mut self) -> Result<Option<ReceivedDesc>, ShmError> {
        let cap = u64::from(self.hdr.ready_capacity);
        let head = self.hdr.ready_head.load(Ordering::Acquire);
        let tail = self.hdr.ready_tail.load(Ordering::Relaxed);
        if head == tail {
            self.touch_consumer_heartbeat();
            return Ok(None);
        }

        let idx = (tail % cap) as usize;
        let desc = self.ready[idx];
        let out = ReceivedDesc {
            seq: desc.seq,
            buffer_id: desc.buffer_id,
            pts_ns: desc.pts_ns,
            dts_ns: desc.dts_ns,
            duration_ns: desc.duration_ns,
            flags: desc.flags,
            offset: desc.offset,
            len: desc.length,
        };

        self.validate_bounds(out.offset, out.len as usize)?;
        self.hdr.ready_tail.store(tail + 1, Ordering::Release);
        self.touch_consumer_heartbeat();
        Ok(Some(out))
    }

    pub fn payload_ptr(&self, desc: &ReceivedDesc) -> Result<*const u8, ShmError> {
        self.validate_bounds(desc.offset, desc.len as usize)?;
        let ptr = unsafe { self.arena.add(desc.offset as usize) };
        Ok(ptr as *const u8)
    }

    pub fn recycle(&mut self, buf: &ReceivedBuffer) -> Result<(), ShmError> {
        self.recycle_desc(
            buf.buffer_id,
            buf.offset,
            buf.payload.len() as u32,
            0, /* status=OK */
        )
    }

    pub fn recycle_desc(
        &mut self,
        buffer_id: u32,
        offset: u64,
        length: u32,
        status: u32,
    ) -> Result<(), ShmError> {
        let cap = u64::from(self.hdr.rec_capacity);
        let mut idle_cycles = 0u32;
        loop {
            let head = self.hdr.rec_head.load(Ordering::Relaxed);
            let tail = self.hdr.rec_tail.load(Ordering::Acquire);
            if head.wrapping_sub(tail) < cap {
                let idx = (head % cap) as usize;
                self.recycle[idx] = RecycleDesc {
                    seq: head + 1,
                    buffer_id,
                    status,
                    offset,
                    length,
                    reserved: [0u8; 36],
                };
                self.hdr.rec_head.store(head + 1, Ordering::Release);
                self.touch_consumer_heartbeat();
                return Ok(());
            }
            poll_yield_sleep(&mut idle_cycles, Duration::from_millis(1));
        }
    }

    fn touch_consumer_heartbeat(&self) {
        self.hdr
            .consumer_heartbeat_ns
            .store(now_nanos(), Ordering::Relaxed);
    }

    pub fn claim_consumer(&mut self, pid: u32) -> Result<(), ShmError> {
        let resync_to_latest = || {
            // On (re)attach, skip stale queued frames and start from the latest point.
            let head = self.hdr.ready_head.load(Ordering::Acquire);
            self.hdr.ready_tail.store(head, Ordering::Release);
        };

        match self.hdr.consumer_owner_pid.compare_exchange(
            0,
            pid,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                resync_to_latest();
                self.touch_consumer_heartbeat();
                Ok(())
            }
            Err(current) if current == pid => {
                resync_to_latest();
                self.touch_consumer_heartbeat();
                Ok(())
            }
            Err(_) => Err(ShmError::Protocol("another consumer already connected")),
        }
    }

    pub fn release_consumer(&mut self, pid: u32) {
        let _ = self.hdr.consumer_owner_pid.compare_exchange(
            pid,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn validate_bounds(&self, offset: u64, len: usize) -> Result<(), ShmError> {
        let start = offset as usize;
        let end = start
            .checked_add(len)
            .ok_or(ShmError::Protocol("overflow in payload bounds"))?;
        if end > self.hdr.arena_size as usize {
            return Err(ShmError::Protocol("payload out of arena bounds"));
        }
        Ok(())
    }
}

unsafe fn map_layout(
    base: *mut u8,
    total_size: usize,
    ready_capacity: u32,
    rec_capacity: u32,
) -> Result<
    (
        &'static mut SharedHeader,
        &'static mut [ReadyDesc],
        &'static mut [RecycleDesc],
        *mut u8,
        u64,
    ),
    ShmError,
> {
    let header_size = HEADER_PAD.max(align_up(size_of::<SharedHeader>(), 64));
    let mut cursor = header_size;

    cursor = align_up(cursor, DESC_ALIGN);
    let ready_offset = cursor;
    let ready_size = ready_capacity as usize * size_of::<ReadyDesc>();
    cursor = cursor
        .checked_add(ready_size)
        .ok_or(ShmError::InvalidConfig("ready ring overflow"))?;

    cursor = align_up(cursor, DESC_ALIGN);
    let recycle_offset = cursor;
    let recycle_size = rec_capacity as usize * size_of::<RecycleDesc>();
    cursor = cursor
        .checked_add(recycle_size)
        .ok_or(ShmError::InvalidConfig("recycle ring overflow"))?;

    cursor = align_up(cursor, 4096);
    let arena_offset = cursor;
    if arena_offset >= total_size {
        return Err(ShmError::InvalidConfig("insufficient total size for arena"));
    }
    let arena_size = (total_size - arena_offset) as u64;

    let hdr = unsafe { &mut *(base.cast::<SharedHeader>()) };

    let ready_ptr = unsafe { base.add(ready_offset).cast::<ReadyDesc>() };
    let recycle_ptr = unsafe { base.add(recycle_offset).cast::<RecycleDesc>() };
    let arena_ptr = unsafe { base.add(arena_offset) };

    let ready = unsafe { std::slice::from_raw_parts_mut(ready_ptr, ready_capacity as usize) };
    let recycle = unsafe { std::slice::from_raw_parts_mut(recycle_ptr, rec_capacity as usize) };

    Ok((hdr, ready, recycle, arena_ptr, arena_size))
}

fn init_header(
    hdr: &mut SharedHeader,
    total_size: u64,
    ready_capacity: u32,
    rec_capacity: u32,
    ready_offset: usize,
    recycle_offset: usize,
    arena_offset: usize,
    arena_size: u64,
    wait_for_connection: bool,
    drop_when_no_consumer: bool,
) {
    hdr.magic = MAGIC;
    hdr.version_major = VERSION_MAJOR;
    hdr.version_minor = VERSION_MINOR;
    hdr.header_size = HEADER_PAD as u32;
    hdr.total_size = total_size;
    hdr.features = 0;
    hdr.state.store(STATE_INIT, Ordering::Relaxed);
    hdr.owner_pid = std::process::id();
    hdr.consumer_owner_pid.store(0, Ordering::Relaxed);
    hdr.epoch.store(1, Ordering::Relaxed);
    hdr.producer_heartbeat_ns
        .store(now_nanos(), Ordering::Relaxed);
    hdr.consumer_heartbeat_ns.store(0, Ordering::Relaxed);

    hdr.ready_capacity = ready_capacity;
    hdr.ready_entry_size = size_of::<ReadyDesc>() as u32;
    hdr.ready_head.store(0, Ordering::Relaxed);
    hdr.ready_tail.store(0, Ordering::Relaxed);

    hdr.rec_capacity = rec_capacity;
    hdr.rec_entry_size = size_of::<RecycleDesc>() as u32;
    hdr.rec_head.store(0, Ordering::Relaxed);
    hdr.rec_tail.store(0, Ordering::Relaxed);

    hdr.ready_offset = ready_offset as u64;
    hdr.recycle_offset = recycle_offset as u64;
    hdr.arena_offset = arena_offset as u64;
    hdr.arena_size = arena_size;

    hdr.arena_used_bytes.store(0, Ordering::Relaxed);
    hdr.arena_high_watermark.store(0, Ordering::Relaxed);
    hdr.alloc_failures.store(0, Ordering::Relaxed);
    hdr.wait_for_connection
        .store(wait_for_connection as u32, Ordering::Relaxed);
    hdr.drop_when_no_consumer
        .store(drop_when_no_consumer as u32, Ordering::Relaxed);
    hdr.notify_mode.store(0, Ordering::Relaxed);
    hdr.state.store(STATE_RUNNING, Ordering::Release);
}

fn validate_header(hdr: &SharedHeader, total_size: u64) -> Result<(), ShmError> {
    if hdr.magic != MAGIC {
        return Err(ShmError::Protocol("magic mismatch"));
    }
    if hdr.version_major != VERSION_MAJOR {
        return Err(ShmError::Protocol("major version mismatch"));
    }
    if hdr.total_size != total_size {
        return Err(ShmError::Protocol("mapped size mismatch"));
    }
    if hdr.ready_entry_size as usize != size_of::<ReadyDesc>() {
        return Err(ShmError::Protocol("ready entry size mismatch"));
    }
    if hdr.rec_entry_size as usize != size_of::<RecycleDesc>() {
        return Err(ShmError::Protocol("recycle entry size mismatch"));
    }
    Ok(())
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::from_secs(0))
        .as_nanos() as u64
}

fn align_up(v: usize, align: usize) -> usize {
    if align <= 1 {
        return v;
    }
    let rem = v % align;
    if rem == 0 { v } else { v + (align - rem) }
}

fn checksum32(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |acc, b| {
        acc.wrapping_mul(16777619).wrapping_add(*b as u32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_sizes_are_cacheline_aligned() {
        assert_eq!(size_of::<ReadyDesc>(), 64);
        assert_eq!(size_of::<RecycleDesc>(), 64);
    }
}
