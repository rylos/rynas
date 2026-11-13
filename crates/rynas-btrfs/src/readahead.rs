//! A reader that reads ahead on its own thread: the stages of a stream (`btrfs send`,
//! compression, the network, a check, `btrfs receive`) each wait on a different resource,
//! and with only pipe-sized buffers between them every stall of one stops all the others.
//! [`ReadAhead`] keeps up to `depth` chunks of `chunk` bytes read in advance, so the stage
//! before it goes on while the one after it waits. Shared with `rynas-agent` as a source file
//! (`#[path]`): std and libc only.

use std::io::{self, Read};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;

/// Reads `inner` ahead on a thread; [`ReadAhead::finish`] gives `inner` back.
pub struct ReadAhead<R: Read + Send + 'static> {
    rx: Option<Receiver<io::Result<Vec<u8>>>>,
    cur: Vec<u8>,
    pos: usize,
    done: bool,
    thread: Option<JoinHandle<R>>,
}

impl<R: Read + Send + 'static> ReadAhead<R> {
    /// Up to `depth` chunks of `chunk` bytes are kept read in advance (`depth × chunk` bytes
    /// of memory at most).
    pub fn new(inner: R, chunk: usize, depth: usize) -> Self {
        let (tx, rx) = sync_channel(depth.max(1));
        let thread = std::thread::Builder::new()
            .name("read-ahead".into())
            .spawn(move || pump(inner, chunk.max(4096), tx))
            .expect("spawn the read-ahead thread");
        ReadAhead { rx: Some(rx), cur: Vec::new(), pos: 0, done: false, thread: Some(thread) }
    }

    /// Stops reading ahead and gives the inner reader back (whatever it read ahead and was
    /// not consumed is dropped).
    pub fn finish(mut self) -> Option<R> {
        // the thread stops at its next send, or at the end of its stream
        drop(self.rx.take());
        self.thread.take().and_then(|t| t.join().ok())
    }
}

fn pump<R: Read>(mut inner: R, chunk: usize, tx: SyncSender<io::Result<Vec<u8>>>) -> R {
    loop {
        let mut buf = vec![0u8; chunk];
        let mut n = 0;
        // fill the chunk (fewer sends, bigger writes downstream); a short read at the end
        let r = loop {
            match inner.read(&mut buf[n..]) {
                Ok(0) => break Ok(()),
                Ok(k) => {
                    n += k;
                    if n == buf.len() {
                        break Ok(());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => break Err(e),
            }
        };
        let end = n < buf.len();
        buf.truncate(n);
        if n > 0 && tx.send(Ok(buf)).is_err() {
            return inner;
        }
        match r {
            Err(e) => {
                let _ = tx.send(Err(e));
                return inner;
            }
            Ok(()) if end => {
                let _ = tx.send(Ok(Vec::new()));
                return inner;
            }
            Ok(()) => {}
        }
    }
}

impl<R: Read + Send + 'static> Read for ReadAhead<R> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        while self.pos == self.cur.len() {
            if self.done {
                return Ok(0);
            }
            let rx = self.rx.as_ref().ok_or_else(|| io::Error::other("read-ahead finished"))?;
            match rx.recv() {
                Ok(Ok(c)) if c.is_empty() => self.done = true,
                Ok(Ok(c)) => {
                    self.cur = c;
                    self.pos = 0;
                }
                Ok(Err(e)) => {
                    self.done = true;
                    return Err(e);
                }
                Err(_) => return Err(io::Error::other("the read-ahead thread stopped")),
            }
        }
        let n = b.len().min(self.cur.len() - self.pos);
        b[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl<R: Read + Send + 'static> Drop for ReadAhead<R> {
    fn drop(&mut self) {
        drop(self.rx.take());
        // never wait here: the thread may be blocked reading its source; it ends at its
        // next send
    }
}

/// Makes the pipe behind `fd` hold `bytes` (`F_SETPIPE_SZ`; capped by
/// `/proc/sys/fs/pipe-max-size`, 1 MiB by default). Best effort: a pipe that cannot grow
/// stays as it is.
pub fn grow_pipe(fd: std::os::fd::RawFd, bytes: usize) {
    // SAFETY: fcntl on a descriptor the caller owns; an error leaves the pipe unchanged
    let _ = unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, bytes as libc::c_int) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source that hands out `total` bytes in reads of `step`, then fails or ends.
    struct Src {
        left: usize,
        step: usize,
        fail: bool,
        next: u8,
    }

    impl Read for Src {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            if self.left == 0 {
                return if self.fail { Err(io::Error::other("broken")) } else { Ok(0) };
            }
            let n = b.len().min(self.step).min(self.left);
            for x in &mut b[..n] {
                *x = self.next;
                self.next = self.next.wrapping_add(1);
            }
            self.left -= n;
            Ok(n)
        }
    }

    #[test]
    fn same_bytes_in_order_and_the_reader_comes_back() {
        let mut r = ReadAhead::new(Src { left: 1_000_003, step: 777, fail: false, next: 0 }, 64 << 10, 4);
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out.len(), 1_000_003);
        assert!(out.iter().enumerate().all(|(i, b)| *b == (i % 256) as u8));
        let back = r.finish().unwrap();
        assert_eq!(back.left, 0);
    }

    #[test]
    fn an_error_of_the_source_reaches_the_reader_after_its_data() {
        let mut r = ReadAhead::new(Src { left: 10_000, step: 100, fail: true, next: 0 }, 4096, 2);
        let mut out = Vec::new();
        let e = r.read_to_end(&mut out).unwrap_err();
        assert_eq!(e.to_string(), "broken");
        assert_eq!(out.len(), 10_000);
        assert!(r.finish().is_some());
    }

    #[test]
    fn finishing_early_stops_the_thread() {
        let mut r = ReadAhead::new(Src { left: usize::MAX, step: 1 << 16, fail: false, next: 0 }, 1 << 16, 2);
        let mut b = [0u8; 10];
        r.read_exact(&mut b).unwrap();
        // the endless source is left behind: finish joins the thread
        assert!(r.finish().is_some());
    }
}
