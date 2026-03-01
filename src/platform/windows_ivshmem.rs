use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::NonNull;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::platform::{SharedRegion, ShmBackend, ShmError};

const IVSHMEM_IFACE_GUID_STR: &str = "{df576976-569d-4672-95a0-f57e4ea0b210}";

const IVSHMEM_CACHE_CACHED: u8 = 1;

const FILE_DEVICE_UNKNOWN: u32 = 0x0000_0022;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;
const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

const IOCTL_IVSHMEM_REQUEST_SIZE: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_ANY_ACCESS);
const IOCTL_IVSHMEM_REQUEST_MMAP: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_ANY_ACCESS);
const IOCTL_IVSHMEM_RELEASE_MMAP: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_ANY_ACCESS);

#[repr(C)]
#[derive(Clone, Copy)]
struct IvshmemMmapConfig {
    cache_mode: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IvshmemMmap {
    peer_id: u16,
    size: u64,
    ptr: *mut c_void,
    vectors: u16,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsIvshmemBackend;

pub struct WindowsIvshmemRegion {
    ptr: NonNull<u8>,
    len: usize,
    dev_handle: HANDLE,
    mapped: bool,
}

unsafe impl Send for WindowsIvshmemRegion {}
unsafe impl Sync for WindowsIvshmemRegion {}

impl SharedRegion for WindowsIvshmemRegion {
    fn as_ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for WindowsIvshmemRegion {
    fn drop(&mut self) {
        if self.mapped {
            let mut bytes_ret = 0u32;
            unsafe {
                let _ = DeviceIoControl(
                    self.dev_handle,
                    IOCTL_IVSHMEM_RELEASE_MMAP,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut bytes_ret,
                    std::ptr::null_mut(),
                );
            }
            self.mapped = false;
        }

        unsafe {
            CloseHandle(self.dev_handle);
        }
    }
}

impl WindowsIvshmemBackend {
    fn to_utf16(s: &str) -> Vec<u16> {
        let mut w: Vec<u16> = s.encode_utf16().collect();
        w.push(0);
        w
    }

    fn normalize_target(name: &str) -> Result<String, ShmError> {
        if name.is_empty() {
            return Err(ShmError::InvalidConfig(
                "ivshmem device target cannot be empty",
            ));
        }
        if name.starts_with(r"\\?\") {
            return Ok(name.to_string());
        }

        let norm = name.replace('/', "\\");
        if norm.starts_with(r"\\.\") {
            return Ok(norm);
        }
        if norm.contains("#{") {
            return Ok(format!(r"\\?\{norm}"));
        }
        if norm.to_ascii_uppercase().starts_with("PCI\\") {
            let iface = norm.replace('\\', "#");
            return Ok(format!(r"\\?\{iface}#{}", IVSHMEM_IFACE_GUID_STR));
        }

        Err(ShmError::InvalidBackendSpec(
            "ivshmem target must be PCI instance id or full interface path".to_string(),
        ))
    }

    fn request_size(handle: HANDLE) -> Result<u64, ShmError> {
        let mut size = 0u64;
        let mut bytes_ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_IVSHMEM_REQUEST_SIZE,
                std::ptr::null(),
                0,
                (&mut size as *mut u64).cast::<c_void>(),
                size_of::<u64>() as u32,
                &mut bytes_ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(ShmError::Io(std::io::Error::last_os_error()));
        }
        if bytes_ret != size_of::<u64>() as u32 || size == 0 {
            return Err(ShmError::Protocol("invalid ivshmem size response"));
        }
        Ok(size)
    }

    fn request_map(handle: HANDLE) -> Result<IvshmemMmap, ShmError> {
        let in_cfg = IvshmemMmapConfig {
            cache_mode: IVSHMEM_CACHE_CACHED,
        };
        let mut out = IvshmemMmap::default();
        let mut bytes_ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_IVSHMEM_REQUEST_MMAP,
                (&in_cfg as *const IvshmemMmapConfig).cast::<c_void>(),
                size_of::<IvshmemMmapConfig>() as u32,
                (&mut out as *mut IvshmemMmap).cast::<c_void>(),
                size_of::<IvshmemMmap>() as u32,
                &mut bytes_ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(ShmError::Io(std::io::Error::last_os_error()));
        }
        if bytes_ret != size_of::<IvshmemMmap>() as u32 {
            return Err(ShmError::Protocol("invalid ivshmem mmap response size"));
        }
        if out.ptr.is_null() || out.size == 0 {
            return Err(ShmError::Protocol(
                "ivshmem driver returned null/zero mapping",
            ));
        }
        Ok(out)
    }
}

impl ShmBackend for WindowsIvshmemBackend {
    fn create(
        &self,
        _name: &str,
        _size: usize,
        _perms: u32,
    ) -> Result<Box<dyn SharedRegion>, ShmError> {
        Err(ShmError::UnsupportedBackend(
            "ivshmem backend is attach-only; use open() from reader side",
        ))
    }

    fn open(&self, name: &str) -> Result<Box<dyn SharedRegion>, ShmError> {
        let target = Self::normalize_target(name)?;
        let wname = Self::to_utf16(&target);
        let handle = unsafe {
            CreateFileW(
                wname.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(ShmError::Io(std::io::Error::last_os_error()));
        }

        let requested_size = match Self::request_size(handle) {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(e);
            }
        };
        let mapped = match Self::request_map(handle) {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(e);
            }
        };
        if mapped.size != requested_size {
            unsafe {
                let mut bytes_ret = 0u32;
                let _ = DeviceIoControl(
                    handle,
                    IOCTL_IVSHMEM_RELEASE_MMAP,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut bytes_ret,
                    std::ptr::null_mut(),
                );
                CloseHandle(handle);
            }
            return Err(ShmError::Protocol(
                "ivshmem size mismatch between size/mmap ioctls",
            ));
        }

        let ptr = NonNull::new(mapped.ptr.cast::<u8>())
            .ok_or(ShmError::Protocol("ivshmem mapped pointer was null"))?;

        Ok(Box::new(WindowsIvshmemRegion {
            ptr,
            len: mapped.size as usize,
            dev_handle: handle,
            mapped: true,
        }))
    }
}
