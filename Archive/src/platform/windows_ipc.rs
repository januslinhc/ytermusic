#![cfg(windows)]

use std::{ffi::OsStr, io};

use tokio::net::windows::named_pipe::ClientOptions;
use uuid::Uuid;

use crate::player::transport::AsyncReadWrite;

#[derive(Debug)]
pub struct WindowsIpcEndpoint {
    name: String,
}

impl WindowsIpcEndpoint {
    pub(super) fn new() -> Self {
        Self {
            name: format!(r"\\.\pipe\ytermusic-{}", Uuid::new_v4()),
        }
    }

    pub(super) fn as_os_str(&self) -> &OsStr {
        OsStr::new(&self.name)
    }
}

pub(super) async fn connect(endpoint: &WindowsIpcEndpoint) -> io::Result<Box<dyn AsyncReadWrite>> {
    ClientOptions::new()
        .open(endpoint.as_os_str())
        .map(|stream| Box::new(stream) as Box<dyn AsyncReadWrite>)
}
