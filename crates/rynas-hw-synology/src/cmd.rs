//! Microcontroller commands through synohwd: its control socket (`/run/synohwd/control.sock`,
//! synohwd 4.3, synodeb S8) when it answers `ping`, otherwise its FIFO (`/run/synohwd/cmd`,
//! `synohwd send`). The socket reports synohwd's own refusal (`{"ok":false,"error":…}`), so a
//! UI action gets a real error; the FIFO is fire-and-forget. See [`Commander`].
//!
//! Only what synohwd itself accepts goes out: a sequence of `V<nn>` (exactly two digits) or
//! single commands of `2-9 : ; D R t u q p s r l k @ A B` (`UP_CMD` in synohwd.py). `1` (hard
//! power off) and `C` (reboot) are never sent: the kernel sends them at the end of a
//! poweroff or reboot. synohwd validates again before writing to the UART.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rynas_hw::ControlError;
use serde_json::{Value, json};

pub const FIFO: &str = "/run/synohwd/cmd";
/// synohwd 4.3's control socket (root only, one JSON line per request, schema 1).
pub const CONTROL_SOCKET: &str = "/run/synohwd/control.sock";
/// Connect, write and read timeout of one socket request (synohwd answers at once).
pub const SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a ping result (socket up or not) is trusted.
pub const PING_TTL: Duration = Duration::from_secs(30);
/// Longest answer line read from the socket (a full `status` is a few KiB).
const MAX_ANSWER: u64 = 256 * 1024;

/// Short beep.
pub const BEEP_SHORT: &str = "2";
/// Long beep.
pub const BEEP_LONG: &str = "3";

/// Single-character commands of `UP_CMD`.
const SINGLE: &[u8] = b"23456789:;DRtuqpsrlk@AB";

/// Longest command line rynas writes (synohwd reads 256 bytes at a time).
pub const MAX_CMD: usize = 64;

/// synohwd's `check_cmd`: the whole string must be a sequence of known commands.
pub fn check_cmd(cmd: &str) -> Result<(), String> {
    if cmd.is_empty() {
        return Err("empty command".into());
    }
    if cmd.len() > MAX_CMD {
        return Err(format!("command longer than {MAX_CMD} characters"));
    }
    let b = cmd.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'V'
            && b.len() >= i + 3
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit()
        {
            i += 3;
        } else if SINGLE.contains(&b[i]) {
            i += 1;
        } else {
            // synohwd's message for the hard power-off
            if without_duties(cmd).contains('1') {
                return Err(format!("{cmd:?}: '1' is the uP hard power-off, refused"));
            }
            return Err(format!(
                "{cmd:?}: not a sequence of uP commands (V<nn> or one of 2-9:;DRtuqpsrlk@AB)"
            ));
        }
    }
    Ok(())
}

/// `re.sub(r"V\d\d", "", cmd)`.
fn without_duties(cmd: &str) -> String {
    let b = cmd.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'V'
            && b.len() >= i + 3
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit()
        {
            i += 3;
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// Writes one command line to the FIFO like `synohwd send` does: non-blocking open (no reader
/// = synohwd is not running), one write.
pub fn send(fifo: &Path, cmd: &str) -> Result<(), ControlError> {
    check_cmd(cmd).map_err(ControlError::Invalid)?;
    let down = |why: &str| ControlError::Unavailable(format!("synohwd is not running ({why})"));
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(fifo)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(down("no command FIFO"));
        }
        Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
            return Err(down("nobody reads the command FIFO"));
        }
        Err(e) => return Err(ControlError::Failed(format!("synohwd command FIFO: {e}"))),
    };
    use std::os::unix::fs::FileTypeExt;
    if !f
        .metadata()
        .map(|m| m.file_type().is_fifo())
        .unwrap_or(false)
    {
        return Err(ControlError::Failed(
            "the synohwd command path is not a FIFO".into(),
        ));
    }
    match f.write_all(format!("{cmd}\n").as_bytes()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(ControlError::Busy(
            "synohwd is busy (command FIFO full), try again".into(),
        )),
        Err(e) => Err(ControlError::Failed(format!("synohwd command FIFO: {e}"))),
    }
}

/// Why a socket request did not give an answer.
#[derive(Debug)]
enum SockError {
    /// Nothing was written (no socket, refused, permission): the FIFO may be used.
    Unreachable(String),
    /// The request went out but no valid answer came back: it may or may not have run.
    NoAnswer(String),
}

/// One request on the control socket: one JSON line out, one JSON line back.
fn request(sock: &Path, req: &Value, timeout: Duration) -> Result<Value, SockError> {
    let s = UnixStream::connect(sock).map_err(|e| SockError::Unreachable(e.to_string()))?;
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    let mut line = req.to_string();
    line.push('\n');
    (&s).write_all(line.as_bytes())
        .map_err(|e| SockError::Unreachable(e.to_string()))?;
    let mut answer = String::new();
    BufReader::new((&s).take(MAX_ANSWER))
        .read_line(&mut answer)
        .map_err(|e| SockError::NoAnswer(e.to_string()))?;
    if answer.trim().is_empty() {
        return Err(SockError::NoAnswer("empty answer".into()));
    }
    serde_json::from_str(answer.trim()).map_err(|e| SockError::NoAnswer(format!("bad answer: {e}")))
}

fn is_ok(a: &Value) -> bool {
    a.get("ok") == Some(&Value::Bool(true))
}

fn answer_error(a: &Value) -> String {
    a.get("error")
        .and_then(Value::as_str)
        .unwrap_or("refused without a reason")
        .chars()
        .take(300)
        .collect()
}

/// `{"cmd":"ping"}` answers `{"ok":true,"schema":1}`.
pub fn ping(sock: &Path, timeout: Duration) -> bool {
    request(sock, &json!({"cmd": "ping"}), timeout)
        .is_ok_and(|a| is_ok(&a) && a.get("schema").and_then(Value::as_u64) == Some(1))
}

/// synohwd's current status through the socket (`{"cmd":"status"}`), the same object as
/// `status.json`. Only where freshness matters: the periodic status reads `status.json`.
pub fn socket_status(sock: &Path, timeout: Duration) -> Result<Value, String> {
    match request(sock, &json!({"cmd": "status"}), timeout) {
        Ok(mut a) if is_ok(&a) => a
            .get_mut("status")
            .map(Value::take)
            .ok_or_else(|| "no status in the answer".into()),
        Ok(a) => Err(answer_error(&a)),
        Err(SockError::Unreachable(e) | SockError::NoAnswer(e)) => Err(e),
    }
}

/// Sends microcontroller commands: through the control socket when it answers `ping`
/// (cached for [`PING_TTL`]), through the FIFO otherwise.
pub struct Commander {
    socket: PathBuf,
    fifo: PathBuf,
    timeout: Duration,
    /// `(checked at, socket answered)`.
    ping: Mutex<Option<(Instant, bool)>>,
}

impl Commander {
    pub fn new(socket: impl Into<PathBuf>, fifo: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            fifo: fifo.into(),
            timeout: SOCKET_TIMEOUT,
            ping: Mutex::new(None),
        }
    }

    /// The paths of a machine below `root` (`/` in production).
    pub fn under(root: &Path) -> Self {
        Self::new(
            rynas_hw::under(root, CONTROL_SOCKET),
            rynas_hw::under(root, FIFO),
        )
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Whether the socket answers `ping` (cached for [`PING_TTL`]).
    pub fn socket_up(&self) -> bool {
        let mut g = self.ping.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, up)) = *g
            && at.elapsed() < PING_TTL
        {
            return up;
        }
        let up = ping(&self.socket, self.timeout);
        *g = Some((Instant::now(), up));
        up
    }

    fn forget_ping(&self) {
        *self.ping.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// A fresh status from the socket; `None` when only the FIFO is there.
    pub fn status(&self) -> Option<Value> {
        if !self.socket_up() {
            return None;
        }
        socket_status(&self.socket, self.timeout).ok()
    }

    /// Validates `cmd` and sends it. Through the socket synohwd's refusal comes back as
    /// [`ControlError::Failed`] with its text; a socket that cannot be reached falls back to
    /// the FIFO; a request sent without an answer is not sent again (it may have run).
    pub fn send(&self, cmd: &str) -> Result<(), ControlError> {
        check_cmd(cmd).map_err(ControlError::Invalid)?;
        if self.socket_up() {
            match request(
                &self.socket,
                &json!({"cmd": "send", "up": cmd}),
                self.timeout,
            ) {
                Ok(a) if is_ok(&a) => return Ok(()),
                Ok(a) => {
                    return Err(ControlError::Failed(format!(
                        "synohwd refused {cmd:?}: {}",
                        answer_error(&a)
                    )));
                }
                Err(SockError::NoAnswer(e)) => {
                    self.forget_ping();
                    return Err(ControlError::Unavailable(format!(
                        "synohwd did not answer on its control socket ({e})"
                    )));
                }
                Err(SockError::Unreachable(e)) => {
                    tracing::debug!("synohwd control socket: {e}; using the FIFO");
                    self.forget_ping();
                }
            }
        }
        send(&self.fifo, cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;

    /// A fake synohwd control socket: answers each request with `answer(request)` (`None`:
    /// close without answering) and records the requests.
    fn fake_socket(
        path: &Path,
        answer: impl Fn(&Value) -> Option<Value> + Send + 'static,
    ) -> Arc<Mutex<Vec<Value>>> {
        let l = UnixListener::bind(path).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(c) = c else { break };
                let mut line = String::new();
                if BufReader::new(&c).read_line(&mut line).is_err() {
                    continue;
                }
                let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
                s2.lock().unwrap().push(req.clone());
                if let Some(a) = answer(&req) {
                    let _ = (&c).write_all(format!("{a}\n").as_bytes());
                }
            }
        });
        seen
    }

    fn synohwd_43(req: &Value) -> Option<Value> {
        Some(match req["cmd"].as_str() {
            Some("ping") => json!({"ok": true, "schema": 1}),
            Some("status") => json!({"ok": true, "status": {"fans": {"1": 900}}}),
            Some("send") if req["up"] == "3" => json!({"ok": false, "error": "UART write failed"}),
            Some("send") => json!({"ok": true}),
            _ => json!({"ok": false, "error": "unknown command"}),
        })
    }

    fn reader_fifo(path: &Path) -> std::fs::File {
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        // SAFETY: plain mkfifo on a path we own
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn socket_first_with_real_errors() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("control.sock");
        let seen = fake_socket(&sock, synohwd_43);
        let c =
            Commander::new(&sock, d.path().join("cmd")).with_timeout(Duration::from_millis(500));
        assert!(c.socket_up());
        c.send("2").unwrap();
        let e = c.send("3").unwrap_err();
        assert!(
            matches!(&e, ControlError::Failed(m) if m.contains("UART write failed")),
            "{e:?}"
        );
        // validated before anything goes out
        assert!(matches!(c.send("1"), Err(ControlError::Invalid(_))));
        assert_eq!(c.status().unwrap()["fans"]["1"], 900);
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs[0], json!({"cmd": "ping"}));
        assert_eq!(reqs[1], json!({"cmd": "send", "up": "2"}));
        assert_eq!(reqs[2], json!({"cmd": "send", "up": "3"}));
        assert_eq!(reqs[3], json!({"cmd": "status"}));
        assert_eq!(reqs.len(), 4, "the ping is cached");
    }

    #[test]
    fn falls_back_to_the_fifo() {
        let d = tempfile::tempdir().unwrap();
        let fifo = d.path().join("cmd");
        let mut r = reader_fifo(&fifo);
        // no socket at all (synohwd before 4.3)
        let c = Commander::new(d.path().join("none.sock"), &fifo);
        assert!(!c.socket_up());
        assert!(c.status().is_none());
        c.send("2").unwrap();
        let mut buf = [0u8; 16];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"2\n");
        // a socket that answers something else than schema 1 is not used
        let sock = d.path().join("odd.sock");
        let seen = fake_socket(&sock, |_| Some(json!({"ok": true, "schema": 2})));
        let c = Commander::new(&sock, &fifo).with_timeout(Duration::from_millis(500));
        c.send(BEEP_LONG).unwrap();
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"3\n");
        assert_eq!(seen.lock().unwrap().len(), 1, "only the ping");
    }

    #[test]
    fn a_silent_socket_times_out_and_is_not_retried_on_the_fifo() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("control.sock");
        let fifo = d.path().join("cmd");
        let mut r = reader_fifo(&fifo);
        let _seen = fake_socket(&sock, |req| {
            (req["cmd"] == "ping").then(|| json!({"ok": true, "schema": 1}))
        });
        let c = Commander::new(&sock, &fifo).with_timeout(Duration::from_millis(200));
        let t = Instant::now();
        assert!(matches!(c.send("2"), Err(ControlError::Unavailable(_))));
        assert!(t.elapsed() < Duration::from_secs(2));
        // nothing went to the FIFO
        // SAFETY: valid descriptor
        unsafe {
            let fd = std::os::fd::AsRawFd::as_raw_fd(&r);
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let mut buf = [0u8; 4];
        assert!(r.read(&mut buf).is_err());
    }

    /// The cases of synohwd's own test (`test_synohwd.py`: "'1' blocking", "malformed uP
    /// commands", accepted commands).
    #[test]
    fn parity_with_synohwd_check_cmd() {
        for (cmd, ok) in [
            ("V10", true),
            ("V99", true),
            ("V01", true),
            ("V10V11", true),
            ("2", true),
            ("1", false),
            ("21", false),
            ("V1", false),
            ("V101", false),
            ("LA1", false),
            ("1V10", false),
            ("", false),
        ] {
            assert_eq!(check_cmd(cmd).is_ok(), ok, "{cmd:?}");
        }
        for cmd in [
            "VV11", "V", "V9", "VX1", "C", "2C", "LA1", "x", "V10 ", "V1a", "VVV",
        ] {
            assert!(check_cmd(cmd).is_err(), "refuses {cmd:?}");
        }
        for cmd in ["D", "R", "t", "u", "@", "AB", ":;", "V0099"] {
            assert!(check_cmd(cmd).is_ok(), "accepts {cmd:?}");
        }
        assert!(check_cmd("21").unwrap_err().contains("hard power-off"));
        assert!(check_cmd("V10").is_ok() && check_cmd("V1x").is_err());
        assert!(check_cmd(&"2".repeat(MAX_CMD + 1)).is_err());
        assert!(
            check_cmd("2\n3").is_err(),
            "no second line through the FIFO"
        );
    }

    #[test]
    fn fifo_send() {
        let d = tempfile::tempdir().unwrap();
        let fifo = d.path().join("cmd");
        // no FIFO: synohwd is not running
        assert!(matches!(
            send(&fifo, "2"),
            Err(ControlError::Unavailable(_))
        ));
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: plain mkfifo on a path we own
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // FIFO without a reader: ENXIO
        assert!(matches!(
            send(&fifo, "2"),
            Err(ControlError::Unavailable(_))
        ));
        // a reader like synohwd's (O_RDWR keeps it open)
        let mut r = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        send(&fifo, BEEP_LONG).unwrap();
        assert!(matches!(send(&fifo, "1"), Err(ControlError::Invalid(_))));
        let mut buf = [0u8; 16];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"3\n");
        // a regular file is refused
        let plain = d.path().join("plain");
        std::fs::write(&plain, "").unwrap();
        assert!(matches!(send(&plain, "2"), Err(ControlError::Failed(_))));
    }
}
