#![cfg(unix)]

use std::{
    ffi::OsStr,
    io,
    os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
    path::{Path, PathBuf},
};

use tempfile::TempDir;
use tokio::net::UnixStream;

use crate::player::transport::AsyncReadWrite;

const CONSERVATIVE_SOCKET_PATH_LIMIT: usize = 100;

#[derive(Debug)]
pub struct UnixIpcEndpoint {
    directory: TempDir,
    socket_path: PathBuf,
}

impl UnixIpcEndpoint {
    pub(super) fn new() -> io::Result<Self> {
        // `/tmp` keeps the socket path short on macOS, where `TMPDIR` is often
        // deeply nested and Unix-domain sockets have a 104-byte path limit.
        // The random child directory, not the shared root, is the security
        // boundary and is explicitly mode 0700 below.
        let directory = private_tempdir(Path::new("/tmp"))?;
        let socket_path = directory.path().join("m.sock");

        if socket_path.as_os_str().as_bytes().len() >= CONSERVATIVE_SOCKET_PATH_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "temporary directory is too long for a Unix-domain socket",
            ));
        }

        Ok(Self {
            directory,
            socket_path,
        })
    }

    pub(super) fn as_os_str(&self) -> &OsStr {
        self.socket_path.as_os_str()
    }

    pub(super) fn path(&self) -> &Path {
        &self.socket_path
    }

    pub(super) fn directory(&self) -> &Path {
        self.directory.path()
    }
}

fn private_tempdir(root: &Path) -> io::Result<TempDir> {
    let directory = tempfile::Builder::new().prefix("ytm-").tempdir_in(root)?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    Ok(directory)
}

pub(super) async fn connect(endpoint: &UnixIpcEndpoint) -> io::Result<Box<dyn AsyncReadWrite>> {
    UnixStream::connect(endpoint.path())
        .await
        .map(|stream| Box::new(stream) as Box<dyn AsyncReadWrite>)
}
