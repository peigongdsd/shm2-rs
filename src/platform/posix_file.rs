use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::ptr::NonNull;

use crate::platform::{SharedRegion, ShmBackend, ShmError};

#[derive(Debug, Default, Clone, Copy)]
pub struct PosixFileBackend;

pub struct PosixFileRegion {
    ptr: NonNull<u8>,
    len: usize,
    fd: RawFd,
}

// The mapping is process-shared memory; send/sync are controlled by higher layers.
unsafe impl Send for PosixFileRegion {}
unsafe impl Sync for PosixFileRegion {}

impl SharedRegion for PosixFileRegion {
    fn as_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for PosixFileRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
            libc::close(self.fd);
        }
    }
}

impl PosixFileBackend {
    fn open_internal(
        &self,
        path: &str,
        flags: i32,
        perms: u32,
        truncate_to: Option<usize>,
    ) -> Result<PosixFileRegion, ShmError> {
        let cpath = CString::new(path)
            .map_err(|_| ShmError::InvalidConfig("path contains embedded NUL byte"))?;

        let fd = unsafe { libc::open(cpath.as_ptr(), flags, perms as libc::mode_t) };
        if fd < 0 {
            return Err(ShmError::Io(std::io::Error::last_os_error()));
        }

        if let Some(size) = truncate_to {
            if unsafe { libc::ftruncate(fd, size as libc::off_t) } < 0 {
                unsafe {
                    libc::close(fd);
                }
                return Err(ShmError::Io(std::io::Error::last_os_error()));
            }
        }

        let mut st = MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, st.as_mut_ptr()) } < 0 {
            unsafe {
                libc::close(fd);
            }
            return Err(ShmError::Io(std::io::Error::last_os_error()));
        }
        let st = unsafe { st.assume_init() };
        if st.st_size <= 0 {
            unsafe {
                libc::close(fd);
            }
            return Err(ShmError::InvalidConfig(
                "shared file is empty; expected initialized region",
            ));
        }

        let len = st.st_size as usize;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            unsafe {
                libc::close(fd);
            }
            return Err(ShmError::Io(std::io::Error::last_os_error()));
        }

        let ptr = NonNull::new(ptr.cast::<u8>())
            .ok_or(ShmError::Protocol("mmap returned null pointer"))?;

        Ok(PosixFileRegion { ptr, len, fd })
    }
}

impl ShmBackend for PosixFileBackend {
    type Region = PosixFileRegion;

    fn create(&self, name: &str, size: usize, perms: u32) -> Result<Self::Region, ShmError> {
        if size == 0 {
            return Err(ShmError::InvalidConfig("size must be non-zero"));
        }
        self.open_internal(
            name,
            libc::O_CREAT | libc::O_RDWR | libc::O_CLOEXEC,
            perms,
            Some(size),
        )
    }

    fn open(&self, name: &str) -> Result<Self::Region, ShmError> {
        self.open_internal(name, libc::O_RDWR | libc::O_CLOEXEC, 0, None)
    }
}
