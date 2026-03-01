use std::fmt;
use std::ptr::NonNull;

#[cfg(unix)]
pub mod posix_file;
#[cfg(windows)]
pub mod windows_ivshmem;
#[cfg(windows)]
pub mod windows_named;

#[derive(Debug)]
pub enum ShmError {
    Io(std::io::Error),
    InvalidConfig(&'static str),
    InvalidBackendSpec(String),
    UnsupportedBackend(&'static str),
    Protocol(&'static str),
    Exhausted,
    NoConsumer,
    RingFull,
    RingEmpty,
}

impl fmt::Display for ShmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShmError::Io(e) => write!(f, "io error: {e}"),
            ShmError::InvalidConfig(s) => write!(f, "invalid config: {s}"),
            ShmError::InvalidBackendSpec(s) => write!(f, "invalid backend spec: {s}"),
            ShmError::UnsupportedBackend(s) => write!(f, "unsupported backend: {s}"),
            ShmError::Protocol(s) => write!(f, "protocol error: {s}"),
            ShmError::Exhausted => write!(f, "allocator exhausted"),
            ShmError::NoConsumer => write!(f, "no active consumer connected"),
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

pub trait SharedRegion: Send {
    fn as_ptr(&self) -> NonNull<u8>;
    fn len(&self) -> usize;
}

pub trait ShmBackend: Send + Sync {
    fn create(
        &self,
        name: &str,
        size: usize,
        perms: u32,
    ) -> Result<Box<dyn SharedRegion>, ShmError>;
    fn open(&self, name: &str) -> Result<Box<dyn SharedRegion>, ShmError>;
}

pub struct BackendSelection {
    pub backend: Box<dyn ShmBackend>,
    pub name: String,
}

pub fn resolve_backend(spec: &str) -> Result<BackendSelection, ShmError> {
    if spec.is_empty() {
        return Err(ShmError::InvalidBackendSpec(
            "empty shm-path is not allowed".to_string(),
        ));
    }

    if let Some((scheme, raw_name)) = parse_scheme(spec) {
        return resolve_with_scheme(scheme, raw_name);
    }

    #[cfg(unix)]
    {
        return Ok(BackendSelection {
            backend: Box::new(posix_file::PosixFileBackend),
            name: spec.to_string(),
        });
    }

    #[cfg(windows)]
    {
        return Ok(BackendSelection {
            backend: Box::new(windows_named::WindowsNamedBackend),
            name: normalize_windows_name(spec),
        });
    }

    #[allow(unreachable_code)]
    Err(ShmError::UnsupportedBackend(
        "no default backend for this target OS",
    ))
}

fn parse_scheme(spec: &str) -> Option<(&str, &str)> {
    let pos = spec.find("://")?;
    let scheme = &spec[..pos];
    let rest = &spec[(pos + 3)..];
    Some((scheme, rest))
}

fn resolve_with_scheme(scheme: &str, raw_name: &str) -> Result<BackendSelection, ShmError> {
    match scheme {
        "shm" => {
            #[cfg(unix)]
            {
                if raw_name.is_empty() {
                    return Err(ShmError::InvalidBackendSpec(
                        "shm:// URI requires a path".to_string(),
                    ));
                }
                let path = if raw_name.starts_with('/') {
                    raw_name.to_string()
                } else {
                    format!("/{raw_name}")
                };
                return Ok(BackendSelection {
                    backend: Box::new(posix_file::PosixFileBackend),
                    name: path,
                });
            }
            #[cfg(not(unix))]
            {
                let _ = raw_name;
                return Err(ShmError::UnsupportedBackend(
                    "shm:// is only available on unix targets",
                ));
            }
        }
        "winshm" => {
            #[cfg(windows)]
            {
                if raw_name.is_empty() {
                    return Err(ShmError::InvalidBackendSpec(
                        "winshm:// URI requires an object name".to_string(),
                    ));
                }
                return Ok(BackendSelection {
                    backend: Box::new(windows_named::WindowsNamedBackend),
                    name: normalize_windows_name(raw_name),
                });
            }
            #[cfg(not(windows))]
            {
                let _ = raw_name;
                return Err(ShmError::UnsupportedBackend(
                    "winshm:// is only available on windows targets",
                ));
            }
        }
        "ivshmem" => {
            #[cfg(windows)]
            {
                if raw_name.is_empty() {
                    return Err(ShmError::InvalidBackendSpec(
                        "ivshmem:// URI requires a device target".to_string(),
                    ));
                }
                return Ok(BackendSelection {
                    backend: Box::new(windows_ivshmem::WindowsIvshmemBackend),
                    name: raw_name.to_string(),
                });
            }
            #[cfg(not(windows))]
            {
                let _ = raw_name;
                return Err(ShmError::UnsupportedBackend(
                    "ivshmem:// is only available on windows targets",
                ));
            }
        }
        _ => Err(ShmError::InvalidBackendSpec(format!(
            "unsupported URI scheme '{scheme}'"
        ))),
    }
}

#[cfg(windows)]
fn normalize_windows_name(name: &str) -> String {
    name.replace('/', "\\")
}
