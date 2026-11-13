//! A guard in front of `btrfs receive` for streams sent by `rynas-agent` (PC backup, btrfs
//! method over HTTPS, `docs/research/pc-backup.md` §11.9): the stream is passed on one whole
//! command at a time, and only when every command has the shape the NAS allows.
//!
//! - one stream header (`btrfs-stream\0`), version 1 only (the agent sends `--proto 1`): v2
//!   commands such as `FALLOCATE` would let a few bytes of stream fill the disk, and v1 has
//!   all a plain (uncompressed, data-carrying) send needs;
//! - every command's CRC32C checked, its type a v1 command (`UPDATE_EXTENT`, only in
//!   `--no-data` streams, refused too), each attribute a v1 type and present once
//!   (`btrfs receive` would use the last copy of a repeated one: a check that reads the first
//!   could be fooled);
//! - the first command creates exactly the expected subvolume: `SUBVOL` for a full send,
//!   `SNAPSHOT` whose clone UUID is the expected parent's for an incremental one, with
//!   `PATH` = the expected name and a UUID; no other `SUBVOL`/`SNAPSHOT` later;
//! - the paths inside the subvolume (`PATH`, `PATH_TO`, `CLONE_PATH`, and `PATH_LINK` of a
//!   hard link) are relative, without `.`/`..`/empty components or NUL: `btrfs receive`
//!   joins them to its folder without resolving `..`. A symlink's target (`PATH_LINK` of
//!   `SYMLINK`) is file content and stays as it is: the receive runs chrooted in an empty
//!   staging folder, so a link followed later leads nowhere else;
//! - `CLONE` only from the subvolume being received or the expected parent (by UUID):
//!   `btrfs receive` looks a clone source up by UUID across the whole filesystem, so another
//!   PC's snapshot could be read otherwise;
//! - nothing after `END`.
//!
//! Device nodes and set-id bits pass: system snapshots hold them (`sudo`, overlayfs
//! whiteouts, container root filesystems). The receive unit may create device nodes but
//! cannot open one (`DevicePolicy=closed`, `DeviceAllow=char-* m`: a `WRITE` into a block
//! node fails), the folder is `0700 root`, and the NAS' copy-out takes regular files,
//! folders and symlinks only.
//!
//! A command is held back until it is read whole (at most [`MAX_COMMAND`] bytes, the v1 send
//! buffer of the kernel and of `btrfs receive`), so a refused one never reaches `btrfs
//! receive`. The format: a header `{le32 len, le16 cmd, le32 crc}` then `len` bytes of
//! attributes `{le16 type, le16 len, data}`; the CRC32C (seed 0, no final inversion, as
//! btrfs computes it) covers the header with its CRC field zeroed and the attributes.

use std::io::{self, Read};

pub const MAGIC: &[u8; 13] = b"btrfs-stream\0";
/// The only stream version accepted.
pub const VERSION: u32 = 1;
/// Largest command, header included (`BTRFS_SEND_BUF_SIZE_V1`).
pub const MAX_COMMAND: usize = 64 << 10;
const HEADER: usize = 10;

const CMD_SUBVOL: u16 = 1;
const CMD_SNAPSHOT: u16 = 2;
const CMD_LINK: u16 = 10;
const CMD_CLONE: u16 = 16;
const CMD_END: u16 = 21;
/// Last v1 command passed on (`END`; `UPDATE_EXTENT` = 22 is refused).
const CMD_MAX: u16 = CMD_END;
const ATTR_UUID: u16 = 1;
const ATTR_PATH: u16 = 15;
const ATTR_PATH_TO: u16 = 16;
const ATTR_PATH_LINK: u16 = 17;
const ATTR_CLONE_UUID: u16 = 20;
const ATTR_CLONE_PATH: u16 = 22;
/// Last v1 attribute (`CLONE_LEN`).
const ATTR_MAX: u16 = 24;

/// What the stream must create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expect {
    /// The subvolume's name (`@home.20261112T0300`).
    pub name: String,
    /// The parent's received UUID (text form) for an incremental send; `None`: a full send.
    pub parent_uuid: Option<String>,
}

/// What the guard saw: the UUID of the received subvolume (from the first command), and
/// whether `END` came.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    pub uuid: Option<String>,
    pub ended: bool,
}

pub struct Guard<R> {
    inner: R,
    expect: Expect,
    /// The expected parent's UUID as bytes (incremental only).
    parent: Option<[u8; 16]>,
    /// The incoming subvolume's UUID, once the first command was read.
    own: Option<[u8; 16]>,
    buf: Vec<u8>,
    pos: usize,
    state: State,
    pub seen: Seen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Header,
    First,
    Rest,
    Ended,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn short() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ends inside a command")
}

fn uuid_text(b: &[u8]) -> Option<String> {
    if b.len() != 16 || b.iter().all(|&x| x == 0) {
        return None;
    }
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// `0d7dc2d7-878a-5446-8596-6b5b6b021473` → its 16 bytes.
pub fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let h: Vec<u8> = text.bytes().filter(|c| *c != b'-').collect();
    if h.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(std::str::from_utf8(&h[i * 2..i * 2 + 2]).ok()?, 16).ok()?;
    }
    Some(out)
}

const fn crc_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut s = 1;
        while s < 8 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            s += 1;
        }
        i += 1;
    }
    t
}

static CRC: [[u32; 256]; 8] = crc_tables();

/// CRC32C (Castagnoli) continued from `crc`, without the usual inversions: btrfs' send
/// stream checksum is `crc32c(0, command)`. Slicing by 8 (well above link speed).
pub fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let lo = crc ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        crc = CRC[7][(lo & 0xff) as usize]
            ^ CRC[6][((lo >> 8) & 0xff) as usize]
            ^ CRC[5][((lo >> 16) & 0xff) as usize]
            ^ CRC[4][(lo >> 24) as usize]
            ^ CRC[3][c[4] as usize]
            ^ CRC[2][c[5] as usize]
            ^ CRC[1][c[6] as usize]
            ^ CRC[0][c[7] as usize];
    }
    for &b in rest {
        crc = CRC[0][((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// The checksum of a whole command (`cmd` = header + attributes), its CRC field as zero.
fn command_crc(cmd: &[u8]) -> u32 {
    let c = crc32c(0, &cmd[..6]);
    let c = crc32c(c, &[0; 4]);
    crc32c(c, &cmd[HEADER..])
}

/// Fills `buf` from `r`: `Ok(false)` at a clean end of input before the first byte.
fn read_exact_or_end(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Ok(false),
            Ok(0) => return Err(short()),
            Ok(k) => got += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// The attributes of a command body, by type (each at most once, v1 types only).
struct Attrs<'a> {
    by_type: [Option<&'a [u8]>; ATTR_MAX as usize + 1],
}

impl<'a> Attrs<'a> {
    fn parse(body: &'a [u8]) -> io::Result<Self> {
        let mut by_type = [None; ATTR_MAX as usize + 1];
        let mut i = 0;
        while i < body.len() {
            if i + 4 > body.len() {
                return Err(bad("truncated attribute"));
            }
            let t = u16::from_le_bytes([body[i], body[i + 1]]);
            let l = u16::from_le_bytes([body[i + 2], body[i + 3]]) as usize;
            i += 4;
            if i + l > body.len() {
                return Err(bad("truncated attribute"));
            }
            if t == 0 || t > ATTR_MAX {
                return Err(bad(format!("attribute type {t} is not a v1 attribute")));
            }
            let slot = &mut by_type[t as usize];
            if slot.is_some() {
                return Err(bad(format!("attribute {t} appears twice in one command")));
            }
            *slot = Some(&body[i..i + l]);
            i += l;
        }
        Ok(Attrs { by_type })
    }

    fn get(&self, t: u16) -> Option<&'a [u8]> {
        self.by_type[t as usize]
    }
}

/// A path inside the received subvolume: empty (its top folder), or relative components
/// without `.`, `..`, empty ones or NUL.
fn safe_path(p: &[u8]) -> bool {
    p.is_empty() || p.split(|&c| c == b'/').all(|c| !c.is_empty() && c != b"." && c != b".." && !c.contains(&0))
}

impl<R: Read> Guard<R> {
    /// A guard for `expect` (an expected parent UUID that is not a UUID matches nothing).
    pub fn new(inner: R, expect: Expect) -> Self {
        let parent = expect.parent_uuid.as_deref().and_then(parse_uuid);
        Guard { inner, expect, parent, own: None, buf: Vec::new(), pos: 0, state: State::Header, seen: Seen::default() }
    }

    /// Checks the first command: exactly the expected subvolume.
    fn check_first(&mut self, cmd: u16, a: &Attrs) -> io::Result<()> {
        let path = a.get(ATTR_PATH).ok_or_else(|| bad("the first command has no path"))?;
        if path != self.expect.name.as_bytes() {
            return Err(bad(format!("the stream creates {:?}, not {}", String::from_utf8_lossy(path), self.expect.name)));
        }
        match (cmd, &self.expect.parent_uuid) {
            (CMD_SUBVOL, None) => {}
            (CMD_SNAPSHOT, Some(_)) => {
                let clone = a.get(ATTR_CLONE_UUID);
                if self.parent.is_none() || clone != self.parent.as_ref().map(|p| &p[..]) {
                    return Err(bad("the incremental stream is not based on the parent named"));
                }
            }
            (CMD_SUBVOL, Some(_)) => return Err(bad("a full stream where an incremental one was announced")),
            (CMD_SNAPSHOT, None) => return Err(bad("an incremental stream where a full one was announced")),
            _ => return Err(bad("the stream does not start with a subvolume")),
        }
        let uuid = a
            .get(ATTR_UUID)
            .filter(|u| u.len() == 16 && u.iter().any(|&b| b != 0))
            .ok_or_else(|| bad("the stream's subvolume has no UUID"))?;
        let mut own = [0u8; 16];
        own.copy_from_slice(uuid);
        self.own = Some(own);
        self.seen.uuid = uuid_text(uuid);
        Ok(())
    }

    /// Checks any later command.
    fn check_rest(&self, cmd: u16, a: &Attrs) -> io::Result<()> {
        if cmd == CMD_SUBVOL || cmd == CMD_SNAPSHOT {
            return Err(bad("a second subvolume in the stream"));
        }
        // a hard link's PATH_LINK is a path in the subvolume; a symlink's is its content
        let link = if cmd == CMD_LINK { Some(ATTR_PATH_LINK) } else { None };
        for t in [ATTR_PATH, ATTR_PATH_TO, ATTR_CLONE_PATH].into_iter().chain(link) {
            if let Some(p) = a.get(t)
                && !safe_path(p)
            {
                return Err(bad(format!("command {cmd}: the path {:?} leaves the subvolume", String::from_utf8_lossy(p))));
            }
        }
        if cmd == CMD_CLONE {
            let from = a.get(ATTR_CLONE_UUID).ok_or_else(|| bad("a clone without a source"))?;
            if ![self.own, self.parent].iter().flatten().any(|u| from == &u[..]) {
                return Err(bad("a clone from a subvolume other than this one or its parent"));
            }
        }
        Ok(())
    }

    /// Reads the next unit (the stream header or one command) into `buf`; `false` at the end.
    fn fill(&mut self) -> io::Result<bool> {
        self.buf.clear();
        self.pos = 0;
        match self.state {
            State::Header => {
                self.buf.resize(MAGIC.len() + 4, 0);
                if !read_exact_or_end(&mut self.inner, &mut self.buf)? {
                    return Err(bad("empty stream"));
                }
                if &self.buf[..MAGIC.len()] != MAGIC {
                    return Err(bad("not a btrfs send stream"));
                }
                let v = u32::from_le_bytes(self.buf[MAGIC.len()..].try_into().unwrap_or_default());
                if v != VERSION {
                    return Err(bad(format!("send stream version {v} is not accepted (only {VERSION})")));
                }
                self.state = State::First;
                Ok(true)
            }
            State::Ended => {
                // nothing may follow END (no second stream, no second subvolume)
                let mut one = [0u8; 1];
                loop {
                    match self.inner.read(&mut one) {
                        Ok(0) => return Ok(false),
                        Ok(_) => return Err(bad("data after the end of the stream")),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            State::First | State::Rest => {
                self.buf.resize(HEADER, 0);
                if !read_exact_or_end(&mut self.inner, &mut self.buf)? {
                    // the sender stopped before END: never a clean end without a subvolume
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ends before its end command"));
                }
                let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
                let cmd = u16::from_le_bytes([self.buf[4], self.buf[5]]);
                let crc = u32::from_le_bytes([self.buf[6], self.buf[7], self.buf[8], self.buf[9]]);
                if len > MAX_COMMAND - HEADER {
                    return Err(bad(format!("a command of {len} bytes")));
                }
                if cmd == 0 || cmd > CMD_MAX {
                    return Err(bad(format!("command {cmd} is not accepted in a v1 stream")));
                }
                self.buf.resize(HEADER + len, 0);
                if len > 0 && !read_exact_or_end(&mut self.inner, &mut self.buf[HEADER..])? {
                    return Err(short());
                }
                if command_crc(&self.buf) != crc {
                    return Err(bad(format!("command {cmd}: bad checksum")));
                }
                let body = std::mem::take(&mut self.buf);
                let checked = Attrs::parse(&body[HEADER..]).and_then(|a| {
                    if self.state == State::First { self.check_first(cmd, &a) } else { self.check_rest(cmd, &a) }
                });
                self.buf = body;
                checked?;
                if self.state == State::First {
                    self.state = State::Rest;
                }
                if cmd == CMD_END {
                    self.state = State::Ended;
                    self.seen.ended = true;
                }
                Ok(true)
            }
        }
    }
}

impl<R: Read> Read for Guard<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() && !self.fill()? {
            return Ok(0);
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// The first command of a stream held in memory: (path, UUID, clone UUID). For stand-ins
/// of `btrfs receive` (tests, `--dry-system`).
pub fn first_subvolume(data: &[u8]) -> Option<(String, Option<String>, Option<String>)> {
    let at = MAGIC.len() + 4;
    let h = data.get(at..at + HEADER)?;
    let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
    let body = data.get(at + HEADER..at + HEADER + len)?;
    let a = Attrs::parse(body).ok()?;
    let path = String::from_utf8(a.get(ATTR_PATH)?.to_vec()).ok()?;
    Some((path, a.get(ATTR_UUID).and_then(uuid_text), a.get(ATTR_CLONE_UUID).and_then(uuid_text)))
}

/// Test streams: a header and commands with the given attributes, with their checksums.
pub mod fake {
    use super::*;

    /// One command with a correct checksum.
    pub fn cmd(cmd: u16, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut v = cmd_bad_crc(cmd, attrs);
        let c = command_crc(&v);
        v[6..10].copy_from_slice(&c.to_le_bytes());
        v
    }

    /// One command with its checksum left 0.
    pub fn cmd_bad_crc(cmd: u16, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (t, d) in attrs {
            body.extend_from_slice(&t.to_le_bytes());
            body.extend_from_slice(&(d.len() as u16).to_le_bytes());
            body.extend_from_slice(d);
        }
        let mut v = (body.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(&cmd.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend(body);
        v
    }

    /// The checksum of a whole command, as the guard computes it.
    pub fn crc_of(command: &[u8]) -> u32 {
        command_crc(command)
    }

    pub fn header(version: u32) -> Vec<u8> {
        let mut s = MAGIC.to_vec();
        s.extend_from_slice(&version.to_le_bytes());
        s
    }

    pub fn uuid_bytes(text: &str) -> Vec<u8> {
        super::parse_uuid(text).map(|b| b.to_vec()).unwrap_or_default()
    }

    /// The first command that creates `name` (full, or incremental on `parent_uuid`).
    pub fn first(name: &str, uuid: &str, parent_uuid: Option<&str>) -> Vec<u8> {
        let u = uuid_bytes(uuid);
        match parent_uuid {
            None => cmd(SUBVOL, &[(PATH, name.as_bytes()), (UUID, &u)]),
            Some(p) => {
                let pu = uuid_bytes(p);
                cmd(SNAPSHOT, &[(PATH, name.as_bytes()), (UUID, &u), (CLONE_UUID, &pu)])
            }
        }
    }

    /// A v1 stream that creates `name` (full, or incremental on `parent_uuid`), with one
    /// file written.
    pub fn stream(name: &str, uuid: &str, parent_uuid: Option<&str>) -> Vec<u8> {
        let body = [cmd(MKFILE, &[(PATH, b"f")]), cmd(WRITE, &[(PATH, b"f"), (FILE_OFFSET, &0u64.to_le_bytes()), (DATA, b"data")])];
        with(name, uuid, parent_uuid, &body)
    }

    /// A v1 stream: the first command, `body`, `END`.
    pub fn with(name: &str, uuid: &str, parent_uuid: Option<&str>, body: &[Vec<u8>]) -> Vec<u8> {
        let mut s = header(VERSION);
        s.extend(first(name, uuid, parent_uuid));
        for c in body {
            s.extend_from_slice(c);
        }
        s.extend(cmd(END, &[]));
        s
    }

    pub const SUBVOL: u16 = CMD_SUBVOL;
    pub const SNAPSHOT: u16 = CMD_SNAPSHOT;
    pub const MKFILE: u16 = 3;
    pub const MKDIR: u16 = 4;
    pub const MKNOD: u16 = 5;
    pub const SYMLINK: u16 = 8;
    pub const RENAME: u16 = 9;
    pub const LINK: u16 = CMD_LINK;
    pub const WRITE: u16 = 15;
    pub const CLONE: u16 = CMD_CLONE;
    pub const CHMOD: u16 = 18;
    pub const END: u16 = CMD_END;
    pub const UPDATE_EXTENT: u16 = 22;
    pub const FALLOCATE: u16 = 23;
    pub const UUID: u16 = ATTR_UUID;
    pub const CTRANSID: u16 = 2;
    pub const SIZE: u16 = 4;
    pub const MODE: u16 = 5;
    pub const RDEV: u16 = 8;
    pub const PATH: u16 = ATTR_PATH;
    pub const PATH_TO: u16 = ATTR_PATH_TO;
    pub const PATH_LINK: u16 = ATTR_PATH_LINK;
    pub const FILE_OFFSET: u16 = 18;
    pub const DATA: u16 = 19;
    pub const CLONE_UUID: u16 = ATTR_CLONE_UUID;
    pub const CLONE_CTRANSID: u16 = 21;
    pub const CLONE_PATH: u16 = ATTR_CLONE_PATH;
    pub const CLONE_OFFSET: u16 = 23;
    pub const CLONE_LEN: u16 = 24;
    pub const FALLOCATE_MODE: u16 = 25;
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    const U: &str = "0d7dc2d7-878a-5446-8596-6b5b6b021473";
    const P: &str = "4704ff2c-f735-d741-b649-cc1f58aea7f6";
    const OTHER: &str = "9a1b2c3d-0000-4000-8000-000000000001";
    const N: &str = "@home.20261112T0100";

    fn run(data: &[u8], name: &str, parent: Option<&str>) -> io::Result<(Vec<u8>, Seen)> {
        let mut g = Guard::new(data, Expect { name: name.into(), parent_uuid: parent.map(str::to_string) });
        let mut out = Vec::new();
        // small reads: commands span several calls
        let mut chunk = [0u8; 7];
        loop {
            let n = g.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
        }
        Ok((out, g.seen))
    }

    fn refused(body: &[Vec<u8>], parent: Option<&str>) -> bool {
        run(&with(N, U, parent, body), N, parent).is_err()
    }

    #[test]
    fn crc32c_matches_btrfs() {
        // the CRC-32C check value (with the usual inversions), slicing = bytewise, and the
        // first command and END of a real `btrfs send --proto 1` stream (btrfs-progs 7.3:
        // seed 0, no inversion), which the guard passes unchanged
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
        let subvol: [u8; 48] = [
            0x26, 0x00, 0x00, 0x00, 0x01, 0x00, 0x9f, 0xee, 0xdd, 0x11, 0x0f, 0x00, 0x02, 0x00, 0x73, 0x31, 0x01, 0x00, 0x10, 0x00, 0x08, 0x9b, 0x03, 0x14,
            0x29, 0x94, 0x3b, 0x48, 0x91, 0x19, 0x99, 0xcf, 0x7f, 0x1c, 0xf9, 0xf9, 0x02, 0x00, 0x08, 0x00, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let end: [u8; 10] = [0x00, 0x00, 0x00, 0x00, 0x15, 0x00, 0x50, 0x6c, 0xc9, 0x9d];
        assert_eq!(command_crc(&subvol), u32::from_le_bytes([0x9f, 0xee, 0xdd, 0x11]));
        assert_eq!(command_crc(&end), u32::from_le_bytes([0x50, 0x6c, 0xc9, 0x9d]));
        let mut real = header(VERSION);
        real.extend_from_slice(&subvol);
        real.extend_from_slice(&end);
        let (out, seen) = run(&real, "s1", None).unwrap();
        assert_eq!(out, real);
        assert_eq!(seen.uuid.as_deref(), Some("089b0314-2994-3b48-9119-99cf7f1cf9f9"));
        let long: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let bytewise = long.iter().fold(0u32, |c, &b| CRC[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8));
        assert_eq!(crc32c(0, &long), bytewise);
        assert_eq!(crc32c(0, &long[..999]), long[..999].iter().fold(0u32, |c, &b| CRC[0][((c ^ b as u32) & 0xff) as usize] ^ (c >> 8)));
    }

    #[test]
    fn good_streams_pass_unchanged() {
        let s = stream(N, U, None);
        let (out, seen) = run(&s, N, None).unwrap();
        assert_eq!(out, s);
        assert_eq!(seen.uuid.as_deref(), Some(U));
        assert!(seen.ended);
        let s = stream("@home.20261112T0200", U, Some(P));
        assert_eq!(run(&s, "@home.20261112T0200", Some(P)).unwrap().0, s);
        // what a system snapshot holds: the top folder's own metadata (empty path), a set-id
        // file, a device node, symlinks to absolute or `..` targets, a hard link, renames,
        // clones from itself and from the parent
        let u = uuid_bytes(U);
        let p = uuid_bytes(P);
        let body = [
            cmd(CHMOD, &[(PATH, b""), (MODE, &0o755u64.to_le_bytes())]),
            cmd(MKFILE, &[(PATH, b"usr/bin/sudo")]),
            cmd(CHMOD, &[(PATH, b"usr/bin/sudo"), (MODE, &0o4755u64.to_le_bytes())]),
            cmd(MKNOD, &[(PATH, b"var/lib/docker/whiteout"), (MODE, &0o20000u64.to_le_bytes()), (RDEV, &0u64.to_le_bytes())]),
            cmd(SYMLINK, &[(PATH, b"etc/alt"), (PATH_LINK, b"/usr/bin/vim")]),
            cmd(SYMLINK, &[(PATH, b"lib/up"), (PATH_LINK, b"../../x")]),
            cmd(LINK, &[(PATH, b"usr/bin/sudoedit"), (PATH_LINK, b"usr/bin/sudo")]),
            cmd(RENAME, &[(PATH, b"o257-5-0"), (PATH_TO, b"home/a")]),
            cmd(CLONE, &[(PATH, b"b"), (CLONE_UUID, &u), (CLONE_PATH, b"a")]),
            cmd(CLONE, &[(PATH, b"c"), (CLONE_UUID, &p), (CLONE_PATH, b"a")]),
        ];
        assert!(!refused(&body, Some(P)));
    }

    #[test]
    fn the_first_command_must_be_the_announced_subvolume() {
        // another name, a path below, full vs incremental, another parent, no UUID
        assert!(run(&stream("evil", U, None), N, None).is_err());
        assert!(run(&stream(&format!("{N}/x"), U, None), N, None).is_err());
        assert!(run(&stream(N, U, None), N, Some(P)).is_err());
        assert!(run(&stream(N, U, Some(P)), N, None).is_err());
        assert!(run(&stream(N, U, Some(U)), N, Some(P)).is_err());
        let mut s = header(VERSION);
        s.extend(cmd(SUBVOL, &[(PATH, N.as_bytes())]));
        s.extend(cmd(END, &[]));
        assert!(run(&s, N, None).is_err());
        // a parent that is not a UUID never matches
        assert!(run(&stream(N, U, Some(P)), N, Some("not-a-uuid")).is_err());
    }

    #[test]
    fn repeated_or_unknown_attributes_are_refused() {
        // PATH twice: btrfs receive would create the second name
        let mut s = header(VERSION);
        s.extend(cmd(SUBVOL, &[(PATH, N.as_bytes()), (UUID, &uuid_bytes(U)), (PATH, b"sneaky")]));
        s.extend(cmd(END, &[]));
        assert!(run(&s, N, None).is_err());
        // CLONE_UUID twice in SNAPSHOT: the parent check reads one, the receive the other
        let mut s = header(VERSION);
        s.extend(cmd(SNAPSHOT, &[(PATH, N.as_bytes()), (UUID, &uuid_bytes(U)), (CLONE_UUID, &uuid_bytes(P)), (CLONE_UUID, &uuid_bytes(OTHER))]));
        s.extend(cmd(END, &[]));
        assert!(run(&s, N, Some(P)).is_err());
        // in any later command too, and types beyond v1 (or 0)
        assert!(refused(&[cmd(MKFILE, &[(PATH, b"a"), (PATH, b"b")])], None));
        let twice = [(PATH, &b"b"[..]), (CLONE_UUID, &uuid_bytes(U)), (CLONE_PATH, b"a"), (CLONE_UUID, &uuid_bytes(OTHER))];
        assert!(refused(&[cmd(CLONE, &twice)], None));
        assert!(refused(&[cmd(MKFILE, &[(PATH, b"a"), (FALLOCATE_MODE, &[0; 4])])], None));
        assert!(refused(&[cmd(MKFILE, &[(PATH, b"a"), (0, b"")])], None));
    }

    #[test]
    fn clones_only_from_this_subvolume_or_its_parent() {
        let o = uuid_bytes(OTHER);
        assert!(refused(&[cmd(CLONE, &[(PATH, b"b"), (CLONE_UUID, &o), (CLONE_PATH, b"a")])], None));
        assert!(refused(&[cmd(CLONE, &[(PATH, b"b"), (CLONE_UUID, &o), (CLONE_PATH, b"a")])], Some(P)));
        // a full stream has no parent to clone from
        assert!(refused(&[cmd(CLONE, &[(PATH, b"b"), (CLONE_UUID, &uuid_bytes(P)), (CLONE_PATH, b"a")])], None));
        assert!(refused(&[cmd(CLONE, &[(PATH, b"b"), (CLONE_PATH, b"a")])], None));
        // and the clone path stays inside the source
        assert!(refused(&[cmd(CLONE, &[(PATH, b"b"), (CLONE_UUID, &uuid_bytes(U)), (CLONE_PATH, b"../x/a")])], None));
    }

    #[test]
    fn paths_that_leave_the_subvolume_are_refused() {
        for bad_path in [&b".."[..], b"../@.20261112T0100", b"a/../../b", b"/etc/passwd", b"a//b", b"./a", b"a/.", b"a\0b", b"a/"] {
            assert!(refused(&[cmd(MKFILE, &[(PATH, bad_path)])], None), "{bad_path:?}");
            assert!(refused(&[cmd(RENAME, &[(PATH, b"a"), (PATH_TO, bad_path)])], None), "{bad_path:?}");
            assert!(refused(&[cmd(LINK, &[(PATH, b"a"), (PATH_LINK, bad_path)])], None), "{bad_path:?}");
        }
        // a symlink's target is content, not a path of the receive
        assert!(!refused(&[cmd(SYMLINK, &[(PATH, b"a"), (PATH_LINK, b"../../..")])], None));
    }

    #[test]
    fn only_v1_streams_and_commands() {
        // v2/v3 headers, v2 commands, UPDATE_EXTENT (a --no-data stream)
        for v in [0u32, 2, 3, 9] {
            let mut s = header(v);
            s.extend(first(N, U, None));
            s.extend(cmd(END, &[]));
            assert!(run(&s, N, None).is_err(), "version {v}");
        }
        let fallocate = [(PATH, &b"f"[..]), (FILE_OFFSET, &0u64.to_le_bytes()), (SIZE, &(150u64 << 20).to_le_bytes())];
        assert!(refused(&[cmd(FALLOCATE, &fallocate)], None));
        assert!(refused(&[cmd(UPDATE_EXTENT, &[(PATH, b"f")])], None));
        assert!(refused(&[cmd(0, &[])], None));
    }

    #[test]
    fn checksums_framing_and_ends() {
        // a bad checksum, anywhere
        assert!(refused(&[cmd_bad_crc(MKFILE, &[(PATH, b"f")])], None));
        let mut s = header(VERSION);
        s.extend(cmd_bad_crc(SUBVOL, &[(PATH, N.as_bytes()), (UUID, &uuid_bytes(U))]));
        s.extend(cmd(END, &[]));
        assert!(run(&s, N, None).is_err());
        let mut flipped = stream(N, U, None);
        let at = flipped.len() - 15;
        flipped[at] ^= 1;
        assert!(run(&flipped, N, None).is_err());
        // not a stream, empty
        assert!(run(b"hello", N, None).is_err());
        assert!(run(b"", N, None).is_err());
        // a second subvolume, data after END, a second stream
        let two = with(N, U, None, &[cmd(SUBVOL, &[(PATH, b"other.20261112T0100"), (UUID, &uuid_bytes(P))])]);
        assert!(run(&two, N, None).is_err());
        let mut after = stream(N, U, None);
        after.push(0);
        assert!(run(&after, N, None).is_err());
        let mut again = stream(N, U, None);
        again.extend(stream("other.20261112T0100", U, None));
        assert!(run(&again, N, None).is_err());
        // truncated: before END, inside a header, inside a body
        let end = cmd(END, &[]);
        let full = stream(N, U, None);
        let mut short = full.clone();
        short.truncate(full.len() - end.len());
        assert!(run(&short, N, None).is_err());
        for cut in [full.len() - end.len() + 3, MAGIC.len() + 4 + 5, MAGIC.len() + 4 + HEADER + 2] {
            assert!(run(&full[..cut], N, None).is_err(), "cut at {cut}");
        }
        // an attribute longer than its command (checksum right)
        let mut lying = header(VERSION);
        let mut c = cmd(SUBVOL, &[(PATH, N.as_bytes()), (UUID, &uuid_bytes(U))]);
        c[HEADER + 2] = 0xff;
        let crc = command_crc(&c);
        c[6..10].copy_from_slice(&crc.to_le_bytes());
        lying.extend(c);
        lying.extend(cmd(END, &[]));
        assert!(run(&lying, N, None).is_err());
        // a command bigger than the v1 buffer
        let mut huge = header(VERSION);
        huge.extend_from_slice(&(MAX_COMMAND as u32 - HEADER as u32 + 1).to_le_bytes());
        huge.extend_from_slice(&[1, 0, 0, 0, 0, 0]);
        assert!(run(&huge, N, None).is_err());
    }

    #[test]
    fn a_refused_command_is_never_passed_on() {
        // the header passes, the bad first command does not
        let evil = stream("evil", U, None);
        let mut g = Guard::new(&evil[..], Expect { name: "@.20261112T0100".into(), parent_uuid: None });
        let mut out = Vec::new();
        assert!(g.read_to_end(&mut out).is_err());
        assert_eq!(out.len(), MAGIC.len() + 4);
        // a bad later command: only what came before it
        let s = with(N, U, None, &[cmd(MKFILE, &[(PATH, b"ok")]), cmd(MKFILE, &[(PATH, b"../no")])]);
        let mut g = Guard::new(&s[..], Expect { name: N.into(), parent_uuid: None });
        let mut out = Vec::new();
        assert!(g.read_to_end(&mut out).is_err());
        assert_eq!(out.len(), MAGIC.len() + 4 + first(N, U, None).len() + cmd(MKFILE, &[(PATH, b"ok")]).len());
    }

    #[test]
    fn first_subvolume_of_a_stream_in_memory() {
        let s = stream(N, U, Some(P));
        assert_eq!(first_subvolume(&s), Some((N.to_string(), Some(U.to_string()), Some(P.to_string()))));
        assert_eq!(first_subvolume(b"x"), None);
    }
}
