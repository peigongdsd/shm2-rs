use std::fmt;
use std::ptr::NonNull;

pub mod posix_file;

#[derive(Debug)]
pub enum ShmError {
    Io(std::io::Error),
    InvalidConfig(&'static str),
    Protocol(&'static str),
    Exhausted,
    RingFull,
    RingEmpty,
}

impl fmt::Display for ShmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShmError::Io(e) => write!(f, "io error: {e}"),
            ShmError::InvalidConfig(s) => write!(f, "invalid config: {s}"),
            ShmError::Protocol(s) => write!(f, "protocol error: {s}"),
            ShmError::Exhausted => write!(f, "allocator exhausted"),
            ShmError::RingFull => write!(f, "ring full"),
            ShmError::RingEmpty => write!(f, "ring empty"),
        }
    }
}

impl std::error::Error for ShmError {}

impl From<std::io::Error> for ShmError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub trait SharedRegion {
    fn as_ptr(&self) -> NonNull<u8>;
    fn len(&self) -> usize;
}

pub trait ShmBackend {
    type Region: SharedRegion;

    fn create(&self, name: &str, size: usize, perms: u32) -> Result<Self::Region, ShmError>;
    fn open(&self, name: &str) -> Result<Self::Region, ShmError>;
}
