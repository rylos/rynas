//! A zip written as a stream (no seek): every entry stored (the PC's files are often
//! compressed already, and the NAS' CPU stays free), sizes and CRC in a data descriptor,
//! zip64 fields always present so any size works. Names are UTF-8 (flag bit 11).

use std::io::{self, Write};

type Res<T> = Result<T, String>;

/// Counts bytes and the CRC-32 of what passes through.
struct Counting<'a, W: Write> {
    inner: &'a mut W,
    crc: crc32fast::Hasher,
    n: u64,
}

impl<W: Write> Write for Counting<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let k = self.inner.write(buf)?;
        self.crc.update(&buf[..k]);
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct Entry {
    name: Vec<u8>,
    dir: bool,
    dos: (u16, u16),
    crc: u32,
    size: u64,
    offset: u64,
}

pub struct ZipStream<W: Write> {
    w: W,
    pos: u64,
    entries: Vec<Entry>,
    bytes: u64,
}

/// (time, date) in MS-DOS format, UTC, clamped to 1980–2107.
fn dos_time(secs: i64) -> (u16, u16) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil from days (H. Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    if y < 1980 {
        return (0, (1 << 5) | 1);
    }
    let y = y.min(2107);
    let time = ((rem / 3600) << 11) | (((rem % 3600) / 60) << 5) | ((rem % 60) / 2);
    let date = ((y - 1980) << 9) | (m << 5) | d;
    (time as u16, date as u16)
}

/// Never an absolute path or a `..` part inside the archive.
fn clean_name(name: &str) -> Vec<u8> {
    let parts: Vec<&str> = name
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .collect();
    parts.join("/").into_bytes()
}

impl<W: Write> ZipStream<W> {
    pub fn new(w: W) -> Self {
        ZipStream {
            w,
            pos: 0,
            entries: Vec::new(),
            bytes: 0,
        }
    }

    fn put(&mut self, b: &[u8]) -> Res<()> {
        self.w.write_all(b).map_err(|e| e.to_string())?;
        self.pos += b.len() as u64;
        Ok(())
    }

    fn local_header(&mut self, name: &[u8], dos: (u16, u16)) -> Res<()> {
        let mut h = Vec::with_capacity(30 + name.len() + 20);
        h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        h.extend_from_slice(&45u16.to_le_bytes());
        // UTF-8 names, sizes and CRC in the data descriptor
        h.extend_from_slice(&0x0808u16.to_le_bytes());
        h.extend_from_slice(&0u16.to_le_bytes()); // stored
        h.extend_from_slice(&dos.0.to_le_bytes());
        h.extend_from_slice(&dos.1.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&u32::MAX.to_le_bytes());
        h.extend_from_slice(&u32::MAX.to_le_bytes());
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&20u16.to_le_bytes());
        h.extend_from_slice(name);
        // zip64 extra: the sizes follow in the descriptor
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        h.extend_from_slice(&0u64.to_le_bytes());
        self.put(&h)
    }

    fn descriptor(&mut self, crc: u32, size: u64) -> Res<()> {
        let mut d = Vec::with_capacity(24);
        d.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
        d.extend_from_slice(&crc.to_le_bytes());
        d.extend_from_slice(&size.to_le_bytes());
        d.extend_from_slice(&size.to_le_bytes());
        self.put(&d)
    }

    pub fn dir(&mut self, name: &str, mtime: i64) -> Res<()> {
        let mut n = clean_name(name);
        if n.is_empty() {
            return Ok(());
        }
        n.push(b'/');
        let dos = dos_time(mtime);
        let offset = self.pos;
        self.local_header(&n, dos)?;
        self.descriptor(0, 0)?;
        self.entries.push(Entry {
            name: n,
            dir: true,
            dos,
            crc: 0,
            size: 0,
            offset,
        });
        Ok(())
    }

    /// One file; `write` writes its content.
    pub fn file(&mut self, name: &str, mtime: i64, write: impl FnOnce(&mut dyn Write) -> Res<()>) -> Res<()> {
        let n = clean_name(name);
        if n.is_empty() {
            return Ok(());
        }
        let dos = dos_time(mtime);
        let offset = self.pos;
        self.local_header(&n, dos)?;
        let mut c = Counting {
            inner: &mut self.w,
            crc: crc32fast::Hasher::new(),
            n: 0,
        };
        write(&mut c)?;
        let (crc, size) = (c.crc.finalize(), c.n);
        self.pos += size;
        self.bytes += size;
        self.descriptor(crc, size)?;
        self.entries.push(Entry {
            name: n,
            dir: false,
            dos,
            crc,
            size,
            offset,
        });
        Ok(())
    }

    /// The central directory and the end records; returns (files, content bytes).
    pub fn finish(mut self) -> Res<(u64, u64)> {
        let start = self.pos;
        let entries = std::mem::take(&mut self.entries);
        for e in &entries {
            let mut h = Vec::with_capacity(46 + e.name.len() + 28);
            h.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            h.extend_from_slice(&((3u16 << 8) | 45).to_le_bytes());
            h.extend_from_slice(&45u16.to_le_bytes());
            h.extend_from_slice(&0x0808u16.to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            h.extend_from_slice(&e.dos.0.to_le_bytes());
            h.extend_from_slice(&e.dos.1.to_le_bytes());
            h.extend_from_slice(&e.crc.to_le_bytes());
            h.extend_from_slice(&u32::MAX.to_le_bytes());
            h.extend_from_slice(&u32::MAX.to_le_bytes());
            h.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            h.extend_from_slice(&28u16.to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // comment
            h.extend_from_slice(&0u16.to_le_bytes()); // disk
            h.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            let mode: u32 = if e.dir { 0o040_755 } else { 0o100_644 };
            let ext = (mode << 16) | if e.dir { 0x10 } else { 0 };
            h.extend_from_slice(&ext.to_le_bytes());
            h.extend_from_slice(&u32::MAX.to_le_bytes());
            h.extend_from_slice(&e.name);
            h.extend_from_slice(&1u16.to_le_bytes());
            h.extend_from_slice(&24u16.to_le_bytes());
            h.extend_from_slice(&e.size.to_le_bytes());
            h.extend_from_slice(&e.size.to_le_bytes());
            h.extend_from_slice(&e.offset.to_le_bytes());
            self.put(&h)?;
        }
        let end = self.pos;
        let count = entries.len() as u64;
        let mut z = Vec::with_capacity(56 + 20 + 22);
        // zip64 end of central directory
        z.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
        z.extend_from_slice(&44u64.to_le_bytes());
        z.extend_from_slice(&((3u16 << 8) | 45).to_le_bytes());
        z.extend_from_slice(&45u16.to_le_bytes());
        z.extend_from_slice(&0u32.to_le_bytes());
        z.extend_from_slice(&0u32.to_le_bytes());
        z.extend_from_slice(&count.to_le_bytes());
        z.extend_from_slice(&count.to_le_bytes());
        z.extend_from_slice(&(end - start).to_le_bytes());
        z.extend_from_slice(&start.to_le_bytes());
        // its locator
        z.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        z.extend_from_slice(&0u32.to_le_bytes());
        z.extend_from_slice(&end.to_le_bytes());
        z.extend_from_slice(&1u32.to_le_bytes());
        // end of central directory, every field pointing to the zip64 record
        z.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z.extend_from_slice(&u16::MAX.to_le_bytes());
        z.extend_from_slice(&u16::MAX.to_le_bytes());
        z.extend_from_slice(&u32::MAX.to_le_bytes());
        z.extend_from_slice(&u32::MAX.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        self.put(&z)?;
        self.w.flush().map_err(|e| e.to_string())?;
        let files = entries.iter().filter(|e| !e.dir).count() as u64;
        Ok((files, self.bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_dates() {
        // 2024-02-29 12:34:56 UTC
        let (t, d) = dos_time(1_709_210_096);
        assert_eq!(d, ((2024 - 1980) << 9) | (2 << 5) | 29);
        assert_eq!(t, (12 << 11) | (34 << 5) | 28);
        assert_eq!(dos_time(0).1, (1 << 5) | 1);
    }

    #[test]
    fn names_stay_inside() {
        assert_eq!(clean_name("/../etc/./passwd"), b"etc/passwd");
    }

    #[test]
    fn unzip_reads_it() {
        let ok = std::process::Command::new("unzip")
            .arg("-v")
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("unzip not installed: archive not checked");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.zip");
        let f = std::fs::File::create(&p).unwrap();
        let mut z = ZipStream::new(f);
        z.dir("docs", 1_700_000_000).unwrap();
        z.file("docs/a.txt", 1_700_000_000, |w| {
            w.write_all(b"hello\n").map_err(|e| e.to_string())
        })
        .unwrap();
        z.file("docs/empty", 1_700_000_000, |_| Ok(())).unwrap();
        assert_eq!(z.finish().unwrap(), (2, 6));
        let t = std::process::Command::new("unzip").arg("-t").arg(&p).output().unwrap();
        assert!(t.status.success(), "{}", String::from_utf8_lossy(&t.stdout));
        let c = std::process::Command::new("unzip")
            .args(["-p"])
            .arg(&p)
            .arg("docs/a.txt")
            .output()
            .unwrap();
        assert_eq!(c.stdout, b"hello\n");
    }
}
