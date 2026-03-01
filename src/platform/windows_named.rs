use std::ffi::c_void;
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};

use crate::platform::{SharedRegion, ShmBackend, ShmError};

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsNamedBackend;

pub struct WindowsNamedRegion {
    ptr: NonNull<u8>,
    len: usize,
    mapping: HANDLE,
}

unsafe impl Send for WindowsNamedRegion {}
unsafe impl Sync for WindowsNamedRegion {}

impl SharedRegion for WindowsNamedRegion {
    fn as_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for WindowsNamedRegion {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.ptr.as_ptr().cast::<c_void>(),
            });
            CloseHandle(self.mapping);
        }
    }
}

impl WindowsNamedBackend {
    fn to_utf16(name: &str) -> Vec<u16> {
        let mut w: Vec<u16> = name.encode_utf16().collect();
        w.push(0);
        w
    }

    fn map_and_wrap(mapping: HANDLE) -> Result<WindowsNamedRegion, ShmError> {
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        if view.Value.is_null() {
            unsafe {
                CloseHandle(mapping);
            }
            return Err(ShmError::Io(std::io::Error::last_os_error()));
        }

        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let queried = unsafe {
            VirtualQuery(
                view.Value,
                &mut mbi as *mut MEMORY_BASIC_INFORMATION,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if queried == 0 || mbi.RegionSize == 0 {
            unsafe {
                UnmapViewOfFile(view);
                CloseHandle(mapping);
            }
            return Err(ShmError::Protocol(
                "VirtualQuery failed for named shared memory view",
            ));
        }

        let ptr = NonNull::new(view.Value.cast::<u8>())
            .ok_or(ShmError::Protocol("MapViewOfFile returned null pointer"))?;

        Ok(WindowsNamedRegion {
            ptr,
            len: mbi.RegionSize,
            mapping,
        })
    }
}

impl ShmBackend for WindowsNamedBackend {
    fn create(
        &self,
        name: &str,
        size: usize,
        _perms: u32,
    ) -> Result<Box<dyn SharedRegion>, ShmError> {
        if size == 0 {
            return Err(ShmError::InvalidConfig("size must be non-zero"));
        }
        if name.is_empty() {
            return Err(ShmError::InvalidConfig(
                "named mapping object cannot be empty",
            ));
        }

        let size_hi = ((size as u64) >> 32) as u32;
        let size_lo = (size as u64 & 0xFFFF_FFFF) as u32;
        let wname = Self::to_utf16(name);
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                size_hi,
                size_lo,
                wname.as_ptr(),
            )
        };
        if mapping.is_null() {
            return Err(ShmError::Io(std::io::Error::last_os_error()));
        }

        let region = Self::map_and_wrap(mapping)?;
        Ok(Box::new(region))
    }

    fn open(&self, name: &str) -> Result<Box<dyn SharedRegion>, ShmError> {
        if name.is_empty() {
            return Err(ShmError::InvalidConfig(
                "named mapping object cannot be empty",
            ));
        }
        let wname = Self::to_utf16(name);
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wname.as_ptr()) };
        if mapping.is_null() {
            return Err(ShmError::Io(std::io::Error::last_os_error()));
        }

        let region = Self::map_and_wrap(mapping)?;
        Ok(Box::new(region))
    }
}
