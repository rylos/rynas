//! The NAS' repository as a `rustic_core` backend: the restic REST protocol over
//! [`crate::http::Client`] (pinned TLS, the device token or restore session as bearer).
//!
//! 423 (the NAS maintains the repository) is waited out for up to an hour; other failures of
//! an upload are retried three times with a growing pause.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rustic_core::{
    ErrorKind, FileType, Id, ReadBackend, RepositoryBackends, RusticError, RusticResult, WriteBackend,
};
use rynas_api_types::pcagent::{REST_V2, RestEntry};

use crate::http::Client;

pub struct NasBackend {
    client: Arc<Client>,
    /// `/api/v1/pcbackup/repo/<group>`.
    prefix: String,
}

impl std::fmt::Debug for NasBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NasBackend({}{})", self.client.ep.url(), self.prefix)
    }
}

fn err(msg: impl Into<String>) -> Box<RusticError> {
    RusticError::new(ErrorKind::Backend, msg.into())
}

impl NasBackend {
    pub fn new(client: Arc<Client>, group: &str) -> NasBackend {
        NasBackend {
            client,
            prefix: format!("/api/v1/pcbackup/repo/{group}"),
        }
    }

    pub fn backends(self) -> RepositoryBackends {
        RepositoryBackends::new(Arc::new(self), None)
    }

    fn path(&self, tpe: FileType, id: &Id) -> String {
        match tpe {
            FileType::Config => format!("{}/config", self.prefix),
            _ => format!("{}/{}/{}", self.prefix, tpe.dirname(), id.to_hex().as_str()),
        }
    }

    fn get(&self, path: &str, range: Option<String>) -> RusticResult<Bytes> {
        let mut last = String::new();
        for attempt in 0..4u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(2u64.pow(attempt)));
            }
            let mut h: Vec<(&str, &str)> = Vec::new();
            if let Some(r) = &range {
                h.push(("Range", r));
            }
            match self.client.request("GET", path, &h, &[]) {
                Ok(r) if r.ok() => return Ok(Bytes::from(r.body)),
                Ok(r) if r.status == 404 || r.status == 401 || r.status == 403 => return Err(err(format!("{path}: {}", r.error_text()))),
                Ok(r) => last = r.error_text(),
                Err(e) => last = e.to_string(),
            }
        }
        Err(err(format!("{path}: {last}")))
    }
}

impl ReadBackend for NasBackend {
    fn location(&self) -> String {
        format!("rynas:{}{}", self.client.ep.url(), self.prefix)
    }

    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        if tpe == FileType::Config {
            // the config exists or not: a HEAD
            let r = self
                .client
                .request("HEAD", &format!("{}/config", self.prefix), &[], &[])
                .map_err(|e| err(e.to_string()))?;
            return Ok(if r.ok() { vec![(Id::default(), 0)] } else { Vec::new() });
        }
        let path = format!("{}/{}/", self.prefix, tpe.dirname());
        let r = self
            .client
            .request("GET", &path, &[("Accept", REST_V2)], &[])
            .map_err(|e| err(format!("{path}: {e}")))?;
        if !r.ok() {
            return Err(err(format!("{path}: {}", r.error_text())));
        }
        let list: Vec<RestEntry> = serde_json::from_slice(&r.body).map_err(|e| err(format!("{path}: {e}")))?;
        list.into_iter()
            .map(|e| {
                let id = e.name.parse::<Id>().map_err(|_| err(format!("bad name {}", e.name)))?;
                Ok((id, u32::try_from(e.size).unwrap_or(u32::MAX)))
            })
            .collect()
    }

    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        self.get(&self.path(tpe, id), None)
    }

    fn read_partial(&self, tpe: FileType, id: &Id, _cacheable: bool, offset: u32, length: u32) -> RusticResult<Bytes> {
        let end = u64::from(offset) + u64::from(length).max(1) - 1;
        self.get(&self.path(tpe, id), Some(format!("bytes={offset}-{end}")))
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        self.path(tpe, id)
    }
}

impl WriteBackend for NasBackend {
    fn create(&self) -> RusticResult<()> {
        // the NAS creates repositories
        Ok(())
    }

    fn write_bytes(&self, tpe: FileType, id: &Id, _cacheable: bool, content: rustic_core::BytesList) -> RusticResult<()> {
        let path = self.path(tpe, id);
        let slices: Vec<&[u8]> = content.slice().iter().map(|b| b.as_ref()).collect();
        let mut failures = 0u32;
        let mut waited = Duration::ZERO;
        loop {
            match self
                .client
                .request("POST", &path, &[("Content-Type", "application/octet-stream")], &slices)
            {
                Ok(r) if r.ok() => return Ok(()),
                Ok(r) if r.status == 423 && waited < Duration::from_secs(3600) => {
                    // the NAS is pruning this repository: wait, then send again
                    std::thread::sleep(Duration::from_secs(30));
                    waited += Duration::from_secs(30);
                }
                Ok(r) if (400..500).contains(&r.status) && r.status != 408 && r.status != 429 => {
                    return Err(err(format!("{path}: {}", r.error_text())));
                }
                Ok(r) if failures < 3 => {
                    failures += 1;
                    let _ = r;
                    std::thread::sleep(Duration::from_secs(5 * u64::from(failures)));
                }
                Ok(r) => return Err(err(format!("{path}: {}", r.error_text()))),
                Err(_) if failures < 3 => {
                    failures += 1;
                    std::thread::sleep(Duration::from_secs(5 * u64::from(failures)));
                }
                Err(e) => return Err(err(format!("{path}: {e}"))),
            }
        }
    }

    fn remove(&self, tpe: FileType, id: &Id, _cacheable: bool) -> RusticResult<()> {
        let path = self.path(tpe, id);
        let r = self.client.request("DELETE", &path, &[], &[]).map_err(|e| err(e.to_string()))?;
        if r.ok() || r.status == 404 {
            Ok(())
        } else {
            Err(err(format!("{path}: {}", r.error_text())))
        }
    }
}
