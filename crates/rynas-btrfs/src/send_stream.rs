//! A guard in front of `btrfs receive` for streams sent by `rynas-agent` (PC backup, btrfs
//! method over HTTPS, `docs/research/pc-backup.md` §11.9): the stream is passed on one whole
//! command at a time, and only when it has the shape the NAS asked for.
//!
//! - one stream header (`btrfs-stream\0`, version 1–3), never a second one;
//! - the first command creates exactly the expected subvolume: `SUBVOL` for a full send,
//!   `SNAPSHOT` whose clone UUID is the expected parent's for an incremental one, with
//!   `PATH` = the expected name (so the receive can only create that name, directly in the
//!   folder);
//! - no other `SUBVOL`/`SNAPSHOT` later (a multi-subvolume stream would create more);
//! - nothing after `END`.
//!
//! A command is held back until it is read whole (at most [`MAX_COMMAND`] bytes), so a refused
//! one never reaches `btrfs receive`. The format: a header `{le32 len, le16 cmd, le32 crc}`
//! then `len` bytes of attributes `{le16 type, le16 len, data}` (in v2+ the data attribute of
//! a write runs to the end of the command; only the first command's attributes are read here,
//! and it has none of those). The checksum is left to `btrfs receive`.

use std::io::{self, Read};

pub const MAGIC: &[u8; 13] = b"btrfs-stream\0";
/// Largest command passed on (v2 writes carry up to 128 KiB + headers; generous margin).
pub const MAX_COMMAND: usize = 2 << 20;

const CMD_SUBVOL: u16 = 1;
const CMD_SNAPSHOT: u16 = 2;
const CMD_END: u16 = 21;
const ATTR_UUID: u16 = 1;
const ATTR_PATH: u16 = 15;
const ATTR_CLONE_UUID: u16 = 20;

/// What the stream must create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expect {
    /// The subvolume's name (`@home.20261112T0300`).
    pub name: String,
    /// The parent's received UUID (text form) for an incremental send; `None`: a full send.
    pub parent_uuid: Option<String>,
}

/// The UUID of the received subvolume, once the first command was read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    pub uuid: Option<String>,
    pub ended: bool,
}

pub struct Guard<R> {
    inner: R,
    expect: Expect,
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

fn uuid_text(b: &[u8]) -> Option<String> {
    if b.len() != 16 || b.iter().all(|&x| x == 0) {
        return None;
    }
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

/// Reads exactly `n` bytes, or `None` at a clean end of input before the first byte.
fn read_n(r: &mut impl Read, n: usize) -> io::Result<Option<Vec<u8>>> {
    let mut v = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match r.read(&mut v[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ends inside a command")),
            Ok(k) => got += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(v))
}

/// The attributes of a command body: (type, data) pairs.
fn attrs(body: &[u8]) -> io::Result<Vec<(u16, &[u8])>> {
    let mut out = Vec::new();
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
        out.push((t, &body[i..i + l]));
        i += l;
    }
    Ok(out)
}

impl<R: Read> Guard<R> {
    pub fn new(inner: R, expect: Expect) -> Self {
        Guard { inner, expect, buf: Vec::new(), pos: 0, state: State::Header, seen: Seen::default() }
    }

    /// Checks the first command (`cmd` with attribute bytes `body`).
    fn check_first(&mut self, cmd: u16, body: &[u8]) -> io::Result<()> {
        let a = attrs(body)?;
        let get = |t: u16| a.iter().find(|(k, _)| *k == t).map(|(_, v)| *v);
        let path = get(ATTR_PATH).ok_or_else(|| bad("the first command has no path"))?;
        if path != self.expect.name.as_bytes() {
            return Err(bad(format!(
                "the stream creates {:?}, not {}",
                String::from_utf8_lossy(path),
                self.expect.name
            )));
        }
        match (cmd, &self.expect.parent_uuid) {
            (CMD_SUBVOL, None) => {}
            (CMD_SNAPSHOT, Some(p)) => {
                let clone = get(ATTR_CLONE_UUID).and_then(uuid_text);
                if clone.as_deref() != Some(p.as_str()) {
                    return Err(bad("the incremental stream is not based on the parent named"));
                }
            }
            (CMD_SUBVOL, Some(_)) => return Err(bad("a full stream where an incremental one was announced")),
            (CMD_SNAPSHOT, None) => return Err(bad("an incremental stream where a full one was announced")),
            _ => return Err(bad("the stream does not start with a subvolume")),
        }
        self.seen.uuid = get(ATTR_UUID).and_then(uuid_text);
        Ok(())
    }

    /// Reads the next unit (the stream header or one command) into `buf`; `false` at the end.
    fn fill(&mut self) -> io::Result<bool> {
        self.buf.clear();
        self.pos = 0;
        match self.state {
            State::Header => {
                let h = read_n(&mut self.inner, MAGIC.len() + 4)?.ok_or_else(|| bad("empty stream"))?;
                if &h[..MAGIC.len()] != MAGIC {
                    return Err(bad("not a btrfs send stream"));
                }
                let v = u32::from_le_bytes(h[MAGIC.len()..].try_into().unwrap_or_default());
                if !(1..=3).contains(&v) {
                    return Err(bad(format!("send stream version {v} is not supported")));
                }
                self.buf = h;
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
                let Some(h) = read_n(&mut self.inner, 10)? else {
                    // the sender stopped before END: the receive fails on its own; never a
                    // clean end without a subvolume
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ends before its end command"));
                };
                let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
                let cmd = u16::from_le_bytes([h[4], h[5]]);
                if len > MAX_COMMAND {
                    return Err(bad(format!("a command of {len} bytes")));
                }
                let body = read_n(&mut self.inner, len)?.unwrap_or_default();
                if body.len() != len {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the stream ends inside a command"));
                }
                if self.state == State::First {
                    self.check_first(cmd, &body)?;
                    self.state = State::Rest;
                } else if cmd == CMD_SUBVOL || cmd == CMD_SNAPSHOT {
                    return Err(bad("a second subvolume in the stream"));
                }
                if cmd == CMD_END {
                    self.state = State::Ended;
                    self.seen.ended = true;
                }
                self.buf = h;
                self.buf.extend_from_slice(&body);
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

/// Test streams: a header and commands with the given attributes (crc left 0).
pub mod fake {
    use super::*;

    pub fn cmd(cmd: u16, attrs: &[(u16, &[u8])]) -> Vec<u8> {
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

    pub fn uuid_bytes(text: &str) -> Vec<u8> {
        let h: String = text.chars().filter(|c| *c != '-').collect();
        (0..16).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0)).collect()
    }

    /// A stream that creates `name` (full, or incremental on `parent_uuid`), with one write.
    pub fn stream(name: &str, uuid: &str, parent_uuid: Option<&str>) -> Vec<u8> {
        let mut s = MAGIC.to_vec();
        s.extend_from_slice(&1u32.to_le_bytes());
        let u = uuid_bytes(uuid);
        match parent_uuid {
            None => s.extend(cmd(CMD_SUBVOL, &[(ATTR_PATH, name.as_bytes()), (ATTR_UUID, &u)])),
            Some(p) => {
                let pu = uuid_bytes(p);
                s.extend(cmd(CMD_SNAPSHOT, &[(ATTR_PATH, name.as_bytes()), (ATTR_UUID, &u), (ATTR_CLONE_UUID, &pu)]))
            }
        }
        s.extend(cmd(15, &[(ATTR_PATH, b"f"), (19, b"data")]));
        s.extend(cmd(CMD_END, &[]));
        s
    }

    pub const SUBVOL: u16 = CMD_SUBVOL;
    pub const END: u16 = CMD_END;
    pub const PATH: u16 = ATTR_PATH;
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    const U: &str = "0d7dc2d7-878a-5446-8596-6b5b6b021473";
    const P: &str = "4704ff2c-f735-d741-b649-cc1f58aea7f6";

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

    #[test]
    fn good_streams_pass_unchanged() {
        let s = stream("@home.20261112T0100", U, None);
        let (out, seen) = run(&s, "@home.20261112T0100", None).unwrap();
        assert_eq!(out, s);
        assert_eq!(seen.uuid.as_deref(), Some(U));
        assert!(seen.ended);
        let s = stream("@home.20261112T0200", U, Some(P));
        assert_eq!(run(&s, "@home.20261112T0200", Some(P)).unwrap().0, s);
    }

    #[test]
    fn everything_else_is_refused() {
        let n = "@home.20261112T0100";
        // another name, a path below, full vs incremental, another parent
        assert!(run(&stream("evil", U, None), n, None).is_err());
        assert!(run(&stream("@home.20261112T0100/x", U, None), n, None).is_err());
        assert!(run(&stream(n, U, None), n, Some(P)).is_err());
        assert!(run(&stream(n, U, Some(P)), n, None).is_err());
        assert!(run(&stream(n, U, Some(U)), n, Some(P)).is_err());
        // not a stream, empty, bad version
        assert!(run(b"hello", n, None).is_err());
        assert!(run(b"", n, None).is_err());
        let mut v = MAGIC.to_vec();
        v.extend_from_slice(&9u32.to_le_bytes());
        assert!(run(&v, n, None).is_err());
        // a second subvolume, data after END, a second stream, no END, a huge command
        let mut two = stream(n, U, None);
        let end = cmd(END, &[]);
        two.truncate(two.len() - end.len());
        two.extend(cmd(SUBVOL, &[(PATH, b"other.20261112T0100")]));
        two.extend(end.clone());
        assert!(run(&two, n, None).is_err());
        let mut after = stream(n, U, None);
        after.push(0);
        assert!(run(&after, n, None).is_err());
        let mut again = stream(n, U, None);
        again.extend(stream("other.20261112T0100", U, None));
        assert!(run(&again, n, None).is_err());
        let mut short = stream(n, U, None);
        short.truncate(short.len() - end.len());
        assert!(run(&short, n, None).is_err());
        let mut huge = MAGIC.to_vec();
        huge.extend_from_slice(&1u32.to_le_bytes());
        huge.extend_from_slice(&((MAX_COMMAND as u32) + 1).to_le_bytes());
        huge.extend_from_slice(&[1, 0, 0, 0, 0, 0]);
        assert!(run(&huge, n, None).is_err());
    }

    #[test]
    fn a_refused_command_is_never_passed_on() {
        // the header passes, the bad first command does not
        let evil = stream("evil", U, None);
        let mut g = Guard::new(&evil[..], Expect { name: "@.20261112T0100".into(), parent_uuid: None });
        let mut out = Vec::new();
        let r = g.read_to_end(&mut out);
        assert!(r.is_err());
        assert_eq!(out.len(), MAGIC.len() + 4);
    }
}
