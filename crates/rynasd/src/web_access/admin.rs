//! A small blocking HTTP/1.1 client for the engine's admin API on its Unix socket.
//!
//! Caddy's admin endpoint skips the Host and Origin checks on Unix sockets
//! (`admin.go`: `enforceHost` is off for `unix` addresses), so `Host: localhost` is fine.
//! `POST /load` blocks until the new configuration runs and answers 400 with
//! `{"error": "..."}` when it refuses it (the old one keeps running); an identical
//! configuration is not reloaded unless the request carries `Cache-Control: must-revalidate`
//! (https://caddyserver.com/docs/api#post-load). `GET /config/` returns the running one.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// Largest answer read (the running configuration).
const MAX_BODY: usize = 8 << 20;

#[derive(Debug, Clone)]
pub struct AdminClient {
    pub socket: PathBuf,
    /// Connect, read and write timeout of plain calls.
    pub timeout: Duration,
    /// Timeout of `/load` (the engine starts listeners and loads certificates first).
    pub load_timeout: Duration,
}

/// An answer: status and body.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

impl AdminClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            timeout: Duration::from_secs(5),
            load_timeout: Duration::from_secs(60),
        }
    }

    /// The socket exists (the engine is, or was, running).
    pub fn present(&self) -> bool {
        self.socket.exists()
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        extra: &[(&str, &str)],
        timeout: Duration,
    ) -> io::Result<Answer> {
        let mut s = UnixStream::connect(&self.socket)?;
        s.set_read_timeout(Some(timeout))?;
        s.set_write_timeout(Some(timeout))?;
        let mut head =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        for (k, v) in extra {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            ));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes())?;
        if let Some(b) = body {
            s.write_all(b)?;
        }
        s.flush()?;
        read_answer(BufReader::new(s))
    }

    /// Loads a whole configuration. `force` reloads even when it equals the running one
    /// (certificate files changed behind the same paths).
    pub fn load(&self, cfg: &Value, force: bool) -> Result<(), String> {
        let body = serde_json::to_vec(cfg).map_err(|e| e.to_string())?;
        let extra: &[(&str, &str)] = if force {
            &[("Cache-Control", "must-revalidate")]
        } else {
            &[]
        };
        let a = self
            .call("POST", "/load", Some(&body), extra, self.load_timeout)
            .map_err(|e| format!("the proxy's admin socket {}: {e}", self.socket.display()))?;
        if a.status == 200 {
            return Ok(());
        }
        Err(format!(
            "the proxy refused the configuration ({}): {}",
            a.status,
            error_text(&a.body)
        ))
    }

    /// The running configuration (`null` when none is loaded).
    pub fn config(&self) -> Result<Value, String> {
        let a = self
            .call("GET", "/config/", None, &[], self.timeout)
            .map_err(|e| format!("the proxy's admin socket {}: {e}", self.socket.display()))?;
        if a.status != 200 {
            return Err(format!(
                "GET /config/: {} {}",
                a.status,
                error_text(&a.body)
            ));
        }
        if a.body.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&a.body).map_err(|e| format!("GET /config/: {e}"))
    }

    /// Waits until the socket accepts connections.
    pub fn wait_ready(&self, max: Duration) -> bool {
        let step = Duration::from_millis(100);
        let mut waited = Duration::ZERO;
        loop {
            if UnixStream::connect(&self.socket).is_ok() {
                return true;
            }
            if waited >= max {
                return false;
            }
            std::thread::sleep(step);
            waited += step;
        }
    }
}

/// The engine's `{"error": "..."}` text, else the body.
fn error_text(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string())
}

fn bad(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

/// Reads an HTTP/1.1 answer: status line, headers, then a body with `Content-Length`,
/// chunked, or up to the end of the connection.
pub fn read_answer<R: Read>(mut r: BufReader<R>) -> io::Result<Answer> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("not an HTTP answer"))?;
    let mut length = None;
    let mut chunked = false;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                length = v.parse::<usize>().ok();
            } else if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| bad("bad chunk size"))?;
            if size == 0 {
                break;
            }
            if body.len() + size > MAX_BODY {
                return Err(bad("answer too large"));
            }
            let start = body.len();
            body.resize(start + size, 0);
            r.read_exact(&mut body[start..])?;
            line.clear();
            r.read_line(&mut line)?;
        }
    } else if let Some(n) = length {
        if n > MAX_BODY {
            return Err(bad("answer too large"));
        }
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.take(MAX_BODY as u64).read_to_end(&mut body)?;
    }
    Ok(Answer { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_and_sized_answers() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        let a = read_answer(BufReader::new(&raw[..])).unwrap();
        assert_eq!((a.status, a.body.as_slice()), (200, &b"{\"a\":1}"[..]));
        let raw = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 17\r\n\r\n{\"error\":\"nope\"}\n";
        let a = read_answer(BufReader::new(&raw[..])).unwrap();
        assert_eq!(a.status, 400);
        assert_eq!(error_text(&a.body), "nope");
    }
}
