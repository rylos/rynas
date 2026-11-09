//! A scripted SFTP server for the tests: speaks the protocol on any stream (a pipe, or an SSH
//! channel) and misbehaves on demand. No real hosts, no disks but the memory here.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use russh_sftp::protocol::{Attrs, Data, ExtendedReply, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode, Version};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// What the server does.
#[derive(Clone, Default)]
pub struct Script {
    /// the file every path names
    pub data: Vec<u8>,
    /// the size a stat reports, when not `data`'s (a file that shrank after)
    pub size: Option<u64>,
    pub mtime: u32,
    /// reads are answered in reversed batches of this many
    pub reverse_batches: usize,
    /// at most this many bytes per read reply
    pub short: Option<usize>,
    /// reads from this offset on are never answered
    pub stall_read_at: Option<u64>,
    /// a read from this offset on gets a failure status
    pub refuse_read_at: Option<u64>,
    /// the server hangs up when a read reaches this offset
    pub drop_read_at: Option<u64>,
    /// a write at this offset on gets a failure status
    pub refuse_write_at: Option<u64>,
    /// the server hangs up when a write reaches this offset, without answering it
    pub drop_write_at: Option<u64>,
    /// answers to `fsync@openssh.com` take this long
    pub fsync_delay: Option<Duration>,
    /// limits advertised as `limits@openssh.com` (read, write; 0: not limited)
    pub limits: Option<(u64, u64)>,
    /// the largest packet the limits advertise (256 KiB otherwise)
    pub packet_limit: Option<u64>,
    /// `posix-rename@openssh.com` advertised and carried out
    pub posix_rename: bool,
    /// reads and writes are each answered after this long, one after the other
    pub reply_delay: Option<Duration>,
    /// after a write reaches this offset the server stops reading its stream
    pub stop_reading_at: Option<u64>,
    /// these paths are folders; every other one is the file
    pub dirs: Vec<String>,
    /// no extensions advertised
    pub bare: bool,
}

/// What the server saw and kept.
#[derive(Clone, Default)]
pub struct Seen {
    pub written: Arc<Mutex<Vec<u8>>>,
    pub fsyncs: Arc<Mutex<u32>>,
    pub write_offsets: Arc<Mutex<Vec<u64>>>,
    pub writes_answered: Arc<Mutex<u32>>,
    pub removes: Arc<Mutex<u32>>,
    pub posix_renames: Arc<Mutex<u32>>,
}

fn ok(id: u32) -> Packet {
    Packet::Status(Status { id, status_code: StatusCode::Ok, error_message: "Success".into(), language_tag: "en".into() })
}

fn failure(id: u32) -> Packet {
    Packet::Status(Status { id, status_code: StatusCode::Failure, error_message: "the disk said no".into(), language_tag: "en".into() })
}

fn attrs(script: &Script, path: &str) -> FileAttributes {
    let mut a = FileAttributes::empty();
    a.size = Some(script.size.unwrap_or(script.data.len() as u64));
    a.mtime = Some(script.mtime);
    a.atime = Some(script.mtime);
    a.permissions = Some(if script.dirs.iter().any(|d| d == path) { 0o040755 } else { 0o100644 });
    a
}

async fn send<S: AsyncWrite + Unpin>(out: &mut S, packet: Packet) -> bool {
    let bytes = Bytes::try_from(packet).expect("packet encodes");
    out.write_all(&bytes).await.is_ok() && out.flush().await.is_ok()
}

fn read_reply(id: u32, offset: u64, len: u32, script: &Script) -> Packet {
    let start = (offset as usize).min(script.data.len());
    if start >= script.data.len() {
        return Packet::Status(Status { id, status_code: StatusCode::Eof, error_message: "End of file".into(), language_tag: "en".into() });
    }
    let mut end = (start + len as usize).min(script.data.len());
    if let Some(limit) = script.short {
        end = end.min(start + limit);
    }
    Packet::Data(Data { id, data: script.data[start..end].to_vec() })
}

/// Serves one session until the stream ends or the script hangs up.
pub async fn serve<S>(mut stream: S, script: Script, seen: Seen)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut held: VecDeque<Packet> = VecDeque::new();
    loop {
        // held replies go out when nothing more comes in for a moment: the client's window
        // may be smaller than a batch (first byte alone, so giving up on it loses nothing)
        let first = if held.is_empty() {
            stream.read_u8().await
        } else {
            match tokio::time::timeout(Duration::from_millis(10), stream.read_u8()).await {
                Ok(r) => r,
                Err(_) => {
                    while let Some(packet) = held.pop_back() {
                        if !send(&mut stream, packet).await {
                            return;
                        }
                    }
                    continue;
                }
            }
        };
        let Ok(first) = first else { return };
        let mut rest = [0u8; 3];
        if stream.read_exact(&mut rest).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes([first, rest[0], rest[1], rest[2]]);
        let mut buf = vec![0u8; len as usize];
        if stream.read_exact(&mut buf).await.is_err() {
            return;
        }
        let mut bytes = Bytes::from(buf);
        let Ok(packet) = Packet::try_from(&mut bytes) else { return };
        let reply = match packet {
            Packet::Init(_) => {
                let mut extensions = HashMap::new();
                if !script.bare {
                    extensions.insert("fsync@openssh.com".to_string(), "1".to_string());
                    if script.limits.is_some() {
                        extensions.insert("limits@openssh.com".to_string(), "1".to_string());
                    }
                    if script.posix_rename {
                        extensions.insert("posix-rename@openssh.com".to_string(), "1".to_string());
                    }
                }
                Some(Packet::Version(Version { version: 3, extensions }))
            }
            Packet::RealPath(p) => Some(Packet::Name(Name { id: p.id, files: vec![File::dummy("/fake/home")] })),
            Packet::Stat(p) => Some(Packet::Attrs(Attrs { id: p.id, attrs: attrs(&script, &p.path) })),
            Packet::Lstat(p) => Some(Packet::Attrs(Attrs { id: p.id, attrs: attrs(&script, &p.path) })),
            Packet::Rename(p) => Some(ok(p.id)),
            Packet::SetStat(p) => Some(ok(p.id)),
            Packet::Remove(p) => {
                *seen.removes.lock().unwrap() += 1;
                Some(ok(p.id))
            }
            Packet::Extended(p) if p.request == "posix-rename@openssh.com" => {
                *seen.posix_renames.lock().unwrap() += 1;
                Some(ok(p.id))
            }
            Packet::Open(p) => {
                if p.pflags.contains(OpenFlags::TRUNCATE) {
                    seen.written.lock().unwrap().clear();
                }
                Some(Packet::Handle(Handle { id: p.id, handle: "h".into() }))
            }
            Packet::Close(p) => Some(ok(p.id)),
            Packet::Read(p) => {
                if script.drop_read_at.is_some_and(|at| p.offset >= at) {
                    return;
                }
                if script.stall_read_at.is_some_and(|at| p.offset >= at) {
                    None
                } else if script.refuse_read_at.is_some_and(|at| p.offset >= at) {
                    Some(failure(p.id))
                } else {
                    if let Some(d) = script.reply_delay {
                        tokio::time::sleep(d).await;
                    }
                    let r = read_reply(p.id, p.offset, p.len, &script);
                    if script.reverse_batches > 1 {
                        held.push_back(r);
                        if held.len() >= script.reverse_batches {
                            while let Some(packet) = held.pop_back() {
                                if !send(&mut stream, packet).await {
                                    return;
                                }
                            }
                        }
                        None
                    } else {
                        Some(r)
                    }
                }
            }
            Packet::Write(p) => {
                seen.write_offsets.lock().unwrap().push(p.offset);
                if script.drop_write_at.is_some_and(|at| p.offset >= at) {
                    return;
                }
                if script.stop_reading_at.is_some_and(|at| p.offset >= at) {
                    // the wire backs up from here on: nothing more is read
                    std::future::pending::<()>().await;
                }
                if let Some(d) = script.reply_delay {
                    tokio::time::sleep(d).await;
                }
                if script.refuse_write_at.is_some_and(|at| p.offset >= at) {
                    Some(failure(p.id))
                } else {
                    let mut disk = seen.written.lock().unwrap();
                    let end = p.offset as usize + p.data.len();
                    if disk.len() < end {
                        disk.resize(end, 0);
                    }
                    disk[p.offset as usize..end].copy_from_slice(&p.data);
                    drop(disk);
                    *seen.writes_answered.lock().unwrap() += 1;
                    Some(ok(p.id))
                }
            }
            Packet::Extended(p) if p.request == "fsync@openssh.com" => {
                if let Some(d) = script.fsync_delay {
                    tokio::time::sleep(d).await;
                }
                *seen.fsyncs.lock().unwrap() += 1;
                Some(ok(p.id))
            }
            Packet::Extended(p) if p.request == "limits@openssh.com" => {
                let (read, write) = script.limits.unwrap_or((0, 0));
                let limits = russh_sftp::extensions::LimitsExtension { max_packet_len: script.packet_limit.unwrap_or(256 * 1024), max_read_len: read, max_write_len: write, max_open_handles: 16 };
                let data = russh_sftp::ser::to_bytes(&limits).expect("limits encode").to_vec();
                Some(Packet::ExtendedReply(ExtendedReply { id: p.id, data }))
            }
            _ => None,
        };
        // anything still held back goes out with the next round: no reversed batch lost
        if let Some(packet) = reply
            && !send(&mut stream, packet).await
        {
            return;
        }
    }
}
