use std::{
    ffi::{OsStr, OsString},
    io,
    path::Path,
};

use crate::player::transport::AsyncReadWrite;
use async_trait::async_trait;

pub mod paths;
pub mod signals;
#[cfg(unix)]
mod unix_ipc;
#[cfg(windows)]
mod windows_ipc;

#[async_trait]
pub trait MpvConnector: Send + Sync {
    async fn connect(&self, endpoint: &IpcEndpoint) -> io::Result<Box<dyn AsyncReadWrite>>;
}

#[derive(Debug)]
pub enum IpcEndpoint {
    #[cfg(unix)]
    Unix(unix_ipc::UnixIpcEndpoint),
    #[cfg(windows)]
    Windows(windows_ipc::WindowsIpcEndpoint),
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

impl IpcEndpoint {
    /// Creates a unique, process-owned local IPC endpoint for this platform.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if a private endpoint cannot be created, or if the
    /// current platform has no native local mpv transport.
    pub fn native() -> io::Result<Self> {
        #[cfg(unix)]
        {
            unix_ipc::UnixIpcEndpoint::new().map(Self::Unix)
        }
        #[cfg(windows)]
        {
            Ok(Self::Windows(windows_ipc::WindowsIpcEndpoint::new()))
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "mpv local IPC is unsupported on this operating system",
            ))
        }
    }

    #[must_use]
    pub fn as_os_str(&self) -> &OsStr {
        match self {
            #[cfg(unix)]
            Self::Unix(endpoint) => endpoint.as_os_str(),
            #[cfg(windows)]
            Self::Windows(endpoint) => endpoint.as_os_str(),
            #[cfg(not(any(unix, windows)))]
            Self::Unsupported => OsStr::new(""),
        }
    }

    #[must_use]
    pub fn to_os_string(&self) -> OsString {
        self.as_os_str().to_os_string()
    }

    #[must_use]
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            #[cfg(unix)]
            Self::Unix(endpoint) => Some(endpoint.path()),
            #[cfg(windows)]
            Self::Windows(_) => None,
            #[cfg(not(any(unix, windows)))]
            Self::Unsupported => None,
        }
    }

    #[must_use]
    pub fn cleanup_directory(&self) -> Option<&Path> {
        match self {
            #[cfg(unix)]
            Self::Unix(endpoint) => Some(endpoint.directory()),
            #[cfg(windows)]
            Self::Windows(_) => None,
            #[cfg(not(any(unix, windows)))]
            Self::Unsupported => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NativeMpvConnector;

#[async_trait]
impl MpvConnector for NativeMpvConnector {
    async fn connect(&self, endpoint: &IpcEndpoint) -> io::Result<Box<dyn AsyncReadWrite>> {
        match endpoint {
            #[cfg(unix)]
            IpcEndpoint::Unix(endpoint) => unix_ipc::connect(endpoint).await,
            #[cfg(windows)]
            IpcEndpoint::Windows(endpoint) => windows_ipc::connect(endpoint).await,
            #[cfg(not(any(unix, windows)))]
            IpcEndpoint::Unsupported => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "mpv local IPC is unsupported on this operating system",
            )),
        }
    }
}
