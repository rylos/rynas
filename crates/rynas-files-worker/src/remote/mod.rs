//! Remote servers: SMB and SFTP, browsed and copied to and from like the shared folders.
//! The connections themselves (paths `/:<id>/…`, the store) are the core's
//! (`rynas_files::conns`); this module adds the clients.

pub mod api;
pub mod sftp;
pub mod smb;
mod srvsvc;

use std::io::{self, Read, Seek, Write};

use serde::Serialize;

pub use rynas_files::conns::*;

/// Opens a connection to its server.
pub trait Connect {
    fn connect(&self) -> io::Result<Box<dyn Fs>>;
}

impl Connect for Conn {
    fn connect(&self) -> io::Result<Box<dyn Fs>> {
        Ok(match self.kind {
            Kind::Smb => Box::new(smb::Smb::connect(self)?),
            Kind::Sftp => Box::new(sftp::Sftp::connect(self)?),
        })
    }
}

/// An item on a server, the way fsops lists the local ones.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    /// d directory, f file, l symlink
    pub kind: char,
    pub size: u64,
    pub mtime: i64,
    pub mode: u32,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == 'd'
    }
}

#[allow(clippy::len_without_is_empty)]
pub trait Reader: Read + Seek + Send {
    fn len(&self) -> u64;
}

pub trait Writer: Write + Send {
    /// Flushes and closes; an error here means the data may not be there.
    fn finish(self: Box<Self>) -> io::Result<()>;
}

/// A server, as the API and the worker use it. Paths are the parts below
/// the connection: for SMB the share first.
pub trait Fs: Send {
    fn list(&self, dir: &[String]) -> io::Result<Vec<Entry>>;
    fn stat(&self, path: &[String]) -> io::Result<Entry>;
    fn reader(&self, path: &[String]) -> io::Result<Box<dyn Reader>>;
    /// `None`: a new file, replacing any; `Some(n)`: an existing file,
    /// written from byte n on (an upload that resumes).
    fn writer(&self, path: &[String], at: Option<u64>) -> io::Result<Box<dyn Writer>>;
    fn mkdir(&self, path: &[String]) -> io::Result<()>;
    /// Never replaces: fails when `to` exists.
    fn rename(&self, from: &[String], to: &[String]) -> io::Result<()>;
    fn remove(&self, path: &[String], dir: bool) -> io::Result<()>;
    /// free and total bytes where `path` is
    fn space(&self, path: &[String]) -> Option<(u64, u64)>;
    fn set_mtime(&self, path: &[String], mtime: i64);
    /// whether a path names the server itself (SMB: the list of shares)
    fn is_top(&self, path: &[String]) -> bool {
        let _ = path;
        false
    }
}
