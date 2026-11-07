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

    /// One request. `body` is sent as JSON when given.
    pub fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&serde_json::Value>,
    ) -> Result<Resp, DnsError> {
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
        let bytes = resp
            .bytes()
            .map_err(|e| DnsError::Network(self.clean(&strip_url(&e))))?;
        let body = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BODY)]).into_owned();
        Ok(Resp { status, body })
    }
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
