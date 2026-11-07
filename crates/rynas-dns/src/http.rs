//! The HTTP side of the provider clients: blocking requests with timeouts, answers as
//! status + text, every error message redacted with the credential's values.

use std::time::Duration;

use crate::{Creds, DnsError, redact};

/// Largest answer read (zone lists are small; a runaway answer is cut).
const MAX_BODY: usize = 4 << 20;

pub struct Http {
    client: reqwest::blocking::Client,
    creds: Creds,
}

/// An answer.
#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub body: String,
}

impl Resp {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Result<serde_json::Value, DnsError> {
        if self.body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&self.body).map_err(|e| {
            DnsError::Network(format!("unreadable answer (HTTP {}): {e}", self.status))
        })
    }
}

impl Http {
    pub fn new(creds: Creds) -> Result<Self, DnsError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(8))
            .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| DnsError::Network(e.to_string()))?;
        Ok(Self { client, creds })
    }

    /// `text` without the credential's values.
    pub fn clean(&self, text: &str) -> String {
        let mut t = redact(text, &self.creds);
        if t.len() > 300 {
            let mut cut = 300;
            while !t.is_char_boundary(cut) {
                cut -= 1;
            }
            t.truncate(cut);
            t.push('…');
        }
        t
    }

    /// One request. `body` is sent as JSON when given. A `429` is retried once after its
    /// `Retry-After` when that is at most [`MAX_RETRY_AFTER`]; otherwise (or when the retry is
    /// throttled too) it is [`DnsError::RateLimited`].
    pub fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&serde_json::Value>,
    ) -> Result<Resp, DnsError> {
        match self.send_once(method, url, headers, body)? {
            Sent::Done(r) => Ok(r),
            Sent::Throttled(Some(wait)) if wait <= MAX_RETRY_AFTER => {
                std::thread::sleep(wait);
                match self.send_once(method, url, headers, body)? {
                    Sent::Done(r) => Ok(r),
                    Sent::Throttled(_) => Err(DnsError::RateLimited(
                        "HTTP 429 again after waiting".into(),
                    )),
                }
            }
            Sent::Throttled(w) => Err(DnsError::RateLimited(match w {
                Some(w) => format!("HTTP 429, retry after {} s", w.as_secs()),
                None => "HTTP 429".into(),
            })),
        }
    }

    fn send_once(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&serde_json::Value>,
    ) -> Result<Sent, DnsError> {
        let m = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| DnsError::Network(e.to_string()))?;
        let mut req = self.client.request(m, url);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        if let Some(b) = body {
            req = req
                .header("Content-Type", "application/json")
                .body(b.to_string());
        }
        let resp = req
            .send()
            .map_err(|e| DnsError::Network(self.clean(&strip_url(&e))))?;
        let status = resp.status().as_u16();
        if status == 429 {
            let wait = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after);
            return Ok(Sent::Throttled(wait));
        }
        let bytes = resp
            .bytes()
            .map_err(|e| DnsError::Network(self.clean(&strip_url(&e))))?;
        let body = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BODY)]).into_owned();
        Ok(Sent::Done(Resp { status, body }))
    }
}

/// Longest `Retry-After` waited for once (a longer one becomes [`DnsError::RateLimited`]).
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);

enum Sent {
    Done(Resp),
    Throttled(Option<Duration>),
}

/// `Retry-After` in seconds (the HTTP-date form is not waited for).
fn retry_after(v: &str) -> Option<Duration> {
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// reqwest errors print the URL, which may carry a token in its query (DuckDNS): drop it.
fn strip_url(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    if let Some(u) = e.url() {
        s = s.replace(u.as_str(), "<url>");
    }
    s
}

/// Percent-encodes one path segment or query value (RFC 3986 unreserved kept, `*` too).
pub fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'*') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// A server answering each connection with the next of `answers` (status, Retry-After).
    fn serve(answers: Vec<(u16, Option<&'static str>)>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for (status, retry) in answers {
                let Ok((mut s, _)) = l.accept() else { return };
                let mut rd = BufReader::new(s.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if rd.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let extra = retry
                    .map(|r| format!("Retry-After: {r}\r\n"))
                    .unwrap_or_default();
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\n{extra}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                );
            }
        });
        base
    }

    #[test]
    fn throttling_waits_once_then_gives_rate_limited() {
        let h = Http::new(Creds::new()).unwrap();
        let base = serve(vec![(429, Some("1")), (200, None)]);
        let t = std::time::Instant::now();
        assert_eq!(h.send("GET", &base, &[], None).unwrap().status, 200);
        assert!(t.elapsed() >= Duration::from_secs(1));
        let base = serve(vec![(429, Some("0")), (429, Some("0"))]);
        assert!(matches!(
            h.send("GET", &base, &[], None),
            Err(DnsError::RateLimited(_))
        ));
        let base = serve(vec![(429, Some("60"))]);
        let t = std::time::Instant::now();
        assert!(matches!(
            h.send("GET", &base, &[], None),
            Err(DnsError::RateLimited(m)) if m.contains("60")
        ));
        assert!(t.elapsed() < Duration::from_secs(5), "a long wait is not waited for");
    }
}
