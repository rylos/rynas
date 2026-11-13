//! A small HTTP/1.1 client for the NAS: TLS with the NAS certificate **pinned** by its
//! SHA-256 (no CA store: a live system may have none, and the NAS usually has its own local
//! CA), keep-alive connections reused, `Content-Length` and chunked answers. Plain HTTP only
//! when asked for (`--allow-http`, tests on a trusted link).
//!
//! The pin is checked on the end-entity certificate and the handshake signature is verified
//! against that certificate's key, so a copy of the certificate without its key gets nowhere.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

const TIMEOUT: Duration = Duration::from_secs(120);
const MAX_HEADER: usize = 64 << 10;
/// A read on the socket waits at most this long before it looks at Cancel again.
const READ_SLICE: Duration = Duration::from_secs(1);

/// The flag a run that honours Cancel (SIGTERM) sets: a read waiting on the NAS (even the
/// hour-long wait for a receive's answer) stops within [`READ_SLICE`].
static CANCEL: std::sync::OnceLock<&'static std::sync::atomic::AtomicBool> = std::sync::OnceLock::new();

/// Makes every read of every client stop once `flag` is set.
pub fn cancel_on(flag: &'static std::sync::atomic::AtomicBool) {
    let _ = CANCEL.set(flag);
}

fn cancelled() -> bool {
    CANCEL.get().is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst))
}

/// The TCP socket under TLS: a read waits in slices of [`READ_SLICE`] (Cancel is looked at
/// between them) for up to `limit` without data, as one `SO_RCVTIMEO` of `limit` would.
struct Patient {
    s: TcpStream,
    limit: Duration,
}

impl Read for Patient {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let start = std::time::Instant::now();
        loop {
            match self.s.read(b) {
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    if cancelled() {
                        return Err(io::Error::other("cancelled"));
                    }
                    if start.elapsed() >= self.limit {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "no answer from the NAS"));
                    }
                }
                r => return r,
            }
        }
    }
}

impl Write for Patient {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.s.write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.s.flush()
    }
}

/// `AA:BB:…` (upper case hex), as the NAS shows it.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// A fingerprint typed by the user, normalized (`aa bb`, `AA:BB`, `aabb` all match).
pub fn normalize_fp(s: &str) -> String {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_uppercase();
    hex.as_bytes()
        .chunks(2)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(":")
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts the certificate whose SHA-256 is `pin`; with `pin` empty, any certificate,
/// remembering the one seen (only to show it before pairing).
#[derive(Debug)]
struct Pinned {
    pin: String,
    seen: Mutex<Option<String>>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity.as_ref());
        if let Ok(mut s) = self.seen.lock() {
            *s = Some(fp.clone());
        }
        if self.pin.is_empty() || self.pin == fp {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "the NAS certificate changed: expected {}, got {fp}",
                self.pin
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Where the NAS is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub tls: bool,
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    /// `https://nas:8443`, `nas:8443` (https), `http://…` (plain, only with `allow_http`).
    pub fn parse(url: &str) -> Result<Endpoint, String> {
        let url = url.trim().trim_end_matches('/');
        let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            (true, url)
        };
        if rest.contains('/') || rest.is_empty() {
            return Err(format!("{url}: give the NAS address only, as https://nas:8443"));
        }
        let (host, port) = if let Some(h) = rest.strip_prefix('[') {
            let (h, p) = h.split_once(']').ok_or("bad IPv6 address")?;
            (h.to_string(), p.strip_prefix(':'))
        } else {
            match rest.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), Some(p)),
                None => (rest.to_string(), None),
            }
        };
        let port = match port {
            Some(p) => p.parse().map_err(|_| format!("{url}: bad port"))?,
            None if tls => 443,
            None => 80,
        };
        Ok(Endpoint { tls, host, port })
    }

    pub fn url(&self) -> String {
        let h = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        format!("{}://{h}:{}", if self.tls { "https" } else { "http" }, self.port)
    }
}

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

type Conn = BufReader<Box<dyn Stream>>;

pub struct Client {
    pub ep: Endpoint,
    /// Pinned fingerprint (empty: accept any, pairing only).
    verifier: Arc<Pinned>,
    tls: Option<Arc<rustls::ClientConfig>>,
    auth: Option<String>,
    idle: Mutex<Vec<Conn>>,
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}


/// A body read as it arrives ([`Client::get_stream`]).
pub struct BodyStream {
    conn: Conn,
    chunked: bool,
    /// Bytes left in the current chunk (chunked) or in the body.
    left: u64,
    /// No length and not chunked: the body ends with the connection.
    to_eof: bool,
    done: bool,
}

impl Read for BodyStream {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        if self.chunked && self.left == 0 {
            let mut sz = String::new();
            if self.conn.read_line(&mut sz)? == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the answer was cut off"));
            }
            let n = u64::from_str_radix(sz.trim().split(';').next().unwrap_or(""), 16).map_err(|_| io::Error::other("bad chunk size"))?;
            if n == 0 {
                // the trailer, up to its empty line
                loop {
                    let mut t = String::new();
                    if self.conn.read_line(&mut t)? == 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the answer was cut off"));
                    }
                    if t.trim().is_empty() {
                        break;
                    }
                }
                self.done = true;
                return Ok(0);
            }
            self.left = n;
        }
        if !self.chunked && !self.to_eof && self.left == 0 {
            self.done = true;
            return Ok(0);
        }
        let want = (out.len() as u64).min(self.left) as usize;
        let n = self.conn.read(&mut out[..want])?;
        if n == 0 {
            if self.to_eof {
                self.done = true;
                return Ok(0);
            }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the answer was cut off"));
        }
        if !self.to_eof {
            self.left -= n as u64;
        }
        if self.chunked && self.left == 0 {
            let mut crlf = [0u8; 2];
            self.conn.read_exact(&mut crlf)?;
            if crlf != *b"\r\n" {
                return Err(io::Error::other("bad chunk end (the answer is garbled)"));
            }
        }
        Ok(n)
    }
}

impl Response {
    #[allow(dead_code)]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The API error text of a failed answer.
    /// The machine-readable `code` of an error answer (`busy`, `already_exists`…).
    pub fn error_code(&self) -> Option<String> {
        serde_json::from_slice::<serde_json::Value>(&self.body).ok()?.get("code")?.as_str().map(str::to_string)
    }

    pub fn error_text(&self) -> String {
        let msg = serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body).chars().take(300).collect());
        format!("HTTP {}: {msg}", self.status)
    }
}

impl Client {
    /// A client for `ep` with the certificate `pin` (`AA:BB…`; empty while pairing).
    pub fn new(ep: Endpoint, pin: &str) -> Client {
        let p = provider();
        let verifier = Arc::new(Pinned {
            pin: normalize_fp(pin),
            seen: Mutex::new(None),
            algs: p.signature_verification_algorithms,
        });
        let tls = ep.tls.then(|| {
            let cfg = rustls::ClientConfig::builder_with_provider(p)
                .with_safe_default_protocol_versions()
                .expect("TLS versions")
                .dangerous()
                .with_custom_certificate_verifier(verifier.clone())
                .with_no_client_auth();
            Arc::new(cfg)
        });
        Client {
            ep,
            verifier,
            tls,
            auth: None,
            idle: Mutex::new(Vec::new()),
        }
    }

    /// Sends `Authorization: Bearer <token>` with every request.
    pub fn with_token(mut self, token: &str) -> Client {
        self.auth = Some(format!("Bearer {token}"));
        self
    }

    /// The certificate the last handshake saw.
    pub fn seen_fingerprint(&self) -> Option<String> {
        self.verifier.seen.lock().ok().and_then(|s| s.clone())
    }

    fn connect(&self) -> io::Result<Conn> {
        self.connect_with(TIMEOUT)
    }

    /// A new connection whose reads give up after `read` without a byte.
    fn connect_with(&self, read: Duration) -> io::Result<Conn> {
        let addrs = (self.ep.host.as_str(), self.ep.port).to_socket_addrs()?;
        let mut last = io::Error::other(format!("{}: no address", self.ep.host));
        for a in addrs {
            match TcpStream::connect_timeout(&a, Duration::from_secs(15)) {
                Ok(s) => {
                    s.set_read_timeout(Some(READ_SLICE.min(read)))?;
                    s.set_write_timeout(Some(TIMEOUT))?;
                    let _ = s.set_nodelay(true);
                    let s = Patient { s, limit: read };
                    let stream: Box<dyn Stream> = match &self.tls {
                        None => Box::new(s),
                        Some(cfg) => {
                            let name = ServerName::try_from(self.ep.host.clone())
                                .map_err(|e| io::Error::other(e.to_string()))?;
                            let c = rustls::ClientConnection::new(cfg.clone(), name)
                                .map_err(|e| io::Error::other(e.to_string()))?;
                            let mut t = rustls::StreamOwned::new(c, s);
                            // the handshake now, so a pin mismatch is the error
                            while t.conn.is_handshaking() {
                                t.conn.complete_io(&mut t.sock)?;
                            }
                            Box::new(t)
                        }
                    };
                    return Ok(BufReader::with_capacity(256 << 10, stream));
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// One request; the body is given as slices (written in order, never joined).
    pub fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[&[u8]]) -> io::Result<Response> {
        // a reused connection the server closed meanwhile: one retry on a fresh one
        for attempt in 0..2 {
            let reused = self.idle.lock().ok().and_then(|mut v| v.pop());
            let fresh = reused.is_none();
            let mut conn = match reused {
                Some(c) => c,
                None => self.connect()?,
            };
            match self.exchange(&mut conn, method, path, headers, body) {
                Ok((resp, keep)) => {
                    if keep && let Ok(mut v) = self.idle.lock() {
                        v.push(conn);
                    }
                    return Ok(resp);
                }
                Err(e) if !fresh && attempt == 0 => {
                    let _ = e;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::other("unreachable"))
    }

    fn exchange(
        &self,
        conn: &mut Conn,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[&[u8]],
    ) -> io::Result<(Response, bool)> {
        let len: usize = body.iter().map(|b| b.len()).sum();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rynas-agent/{}\r\nConnection: keep-alive\r\n",
            self.ep.host,
            env!("CARGO_PKG_VERSION")
        );
        if let Some(a) = &self.auth {
            head.push_str(&format!("Authorization: {a}\r\n"));
        }
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if method != "GET" && method != "HEAD" || len > 0 {
            head.push_str(&format!("Content-Length: {len}\r\n"));
        }
        head.push_str("\r\n");
        {
            let w = conn.get_mut();
            w.write_all(head.as_bytes())?;
            for b in body {
                w.write_all(b)?;
            }
            w.flush()?;
        }
        Self::read_response(conn, method)
    }

    /// `POST path` with a body of unknown length read from `body` (`Transfer-Encoding:
    /// chunked`), on a fresh connection that is not reused. When `body` fails the upload
    /// stops without the last chunk (the server sees a broken body, never a short complete
    /// one) and the error is returned. When the server answers early (an error before the
    /// whole body) and the write fails, that answer is returned. Reads of the answer wait up
    /// to `answer_wait` (the server answers once it has finished with the body).
    pub fn upload(&self, path: &str, headers: &[(&str, &str)], body: &mut dyn Read, answer_wait: Duration) -> io::Result<Response> {
        let mut conn = self.connect_with(answer_wait)?;
        let mut head = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rynas-agent/{}\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n",
            self.ep.host,
            env!("CARGO_PKG_VERSION")
        );
        if let Some(a) = &self.auth {
            head.push_str(&format!("Authorization: {a}\r\n"));
        }
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let written = (|| -> io::Result<Result<(), io::Error>> {
            let w = conn.get_mut();
            w.write_all(head.as_bytes())?;
            let mut buf = vec![0u8; 256 << 10];
            loop {
                let n = match body.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    // the body failed: no last chunk
                    Err(e) => return Ok(Err(e)),
                };
                w.write_all(format!("{n:x}\r\n").as_bytes())?;
                w.write_all(&buf[..n])?;
                w.write_all(b"\r\n")?;
            }
            w.write_all(b"0\r\n\r\n")?;
            w.flush()?;
            Ok(Ok(()))
        })();
        match written {
            Ok(Ok(())) => Self::read_response(&mut conn, "POST").map(|(r, _)| r),
            Ok(Err(body_err)) => Err(body_err),
            // the server stopped reading: its answer says why, when there is one
            Err(write_err) => Self::read_response(&mut conn, "POST").map(|(r, _)| r).map_err(|_| write_err),
        }
    }

    /// The status line and headers of an answer.
    fn read_head(conn: &mut Conn) -> io::Result<(u16, Vec<(String, String)>)> {
        let mut line = String::new();
        let mut total = 0;
        let mut read_line = |conn: &mut Conn, line: &mut String| -> io::Result<()> {
            line.clear();
            let n = conn.read_line(line)?;
            total += n;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"));
            }
            if total > MAX_HEADER {
                return Err(io::Error::other("answer headers too long"));
            }
            Ok(())
        };
        read_line(conn, &mut line)?;
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| io::Error::other(format!("bad status line {line:?}")))?;
        let mut hdrs = Vec::new();
        loop {
            read_line(conn, &mut line)?;
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                hdrs.push((k.trim().to_string(), v.trim().to_string()));
            }
        }
        Ok((status, hdrs))
    }

    /// `GET path` whose body is read as it arrives, on a fresh connection that is closed
    /// afterwards; reads wait up to `read_wait` without a byte (Cancel is honoured). The
    /// reader ends at the body's end and fails when the body is cut (a chunked body without
    /// its last chunk, a short `Content-Length` body). An error answer is read whole and
    /// returned as `Err(answer)`.
    pub fn get_stream(&self, path: &str, headers: &[(&str, &str)], read_wait: Duration) -> io::Result<Result<BodyStream, Response>> {
        let mut conn = self.connect_with(read_wait)?;
        let mut head = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: rynas-agent/{}\r\nConnection: close\r\n",
            self.ep.host,
            env!("CARGO_PKG_VERSION")
        );
        if let Some(a) = &self.auth {
            head.push_str(&format!("Authorization: {a}\r\n"));
        }
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        {
            let w = conn.get_mut();
            w.write_all(head.as_bytes())?;
            w.flush()?;
        }
        let (status, hdrs) = Self::read_head(&mut conn)?;
        let get = |n: &str| hdrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
        let chunked = get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        let length = get("content-length").and_then(|v| v.parse::<u64>().ok());
        let mut body = BodyStream { conn, chunked, left: if chunked { 0 } else { length.unwrap_or(u64::MAX) }, to_eof: !chunked && length.is_none(), done: false };
        if !(200..300).contains(&status) {
            let mut b = Vec::new();
            (&mut body).take(1 << 20).read_to_end(&mut b)?;
            return Ok(Err(Response { status, headers: hdrs, body: b }));
        }
        Ok(Ok(body))
    }

    /// The status line, headers and body of an answer; `true` when the connection can be
    /// reused.
    fn read_response(conn: &mut Conn, method: &str) -> io::Result<(Response, bool)> {
        let (status, hdrs) = Self::read_head(conn)?;
        let get = |n: &str| hdrs.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
        let close = get("connection").is_some_and(|v| v.eq_ignore_ascii_case("close"));
        let mut body_out = Vec::new();
        let no_body = method == "HEAD" || status == 204 || status == 304 || (100..200).contains(&status);
        if !no_body {
            if get("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
                loop {
                    let mut sz = String::new();
                    conn.read_line(&mut sz)?;
                    let n = usize::from_str_radix(sz.trim().split(';').next().unwrap_or("0"), 16)
                        .map_err(|_| io::Error::other("bad chunk size"))?;
                    if n == 0 {
                        // trailers until the empty line
                        loop {
                            let mut t = String::new();
                            conn.read_line(&mut t)?;
                            if t.trim().is_empty() {
                                break;
                            }
                        }
                        break;
                    }
                    let start = body_out.len();
                    body_out.resize(start + n, 0);
                    conn.read_exact(&mut body_out[start..])?;
                    let mut crlf = [0u8; 2];
                    conn.read_exact(&mut crlf)?;
                }
            } else if let Some(n) = get("content-length").and_then(|v| v.parse::<usize>().ok()) {
                body_out.resize(n, 0);
                conn.read_exact(&mut body_out)?;
            } else {
                conn.read_to_end(&mut body_out)?;
                return Ok((
                    Response {
                        status,
                        headers: hdrs,
                        body: body_out,
                    },
                    false,
                ));
            }
        }
        Ok((
            Response {
                status,
                headers: hdrs,
                body: body_out,
            },
            !close,
        ))
    }

    pub fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let r = self
            .request("GET", path, &[("Accept", "application/json")], &[])
            .map_err(|e| format!("{}: {e}", self.ep.url()))?;
        if !r.ok() {
            return Err(r.error_text());
        }
        serde_json::from_slice(&r.body).map_err(|e| format!("bad answer from the NAS: {e}"))
    }

    pub fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(&self, path: &str, body: &B) -> Result<T, String> {
        let b = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let r = self
            .request("POST", path, &[("Content-Type", "application/json"), ("Accept", "application/json")], &[&b])
            .map_err(|e| format!("{}: {e}", self.ep.url()))?;
        if !r.ok() {
            return Err(r.error_text());
        }
        if r.body.is_empty() {
            return serde_json::from_slice(b"null").map_err(|e| e.to_string());
        }
        serde_json::from_slice(&r.body).map_err(|e| format!("bad answer from the NAS: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(
            Endpoint::parse("https://nas.lan:8443/").unwrap(),
            Endpoint { tls: true, host: "nas.lan".into(), port: 8443 }
        );
        assert_eq!(Endpoint::parse("nas").unwrap().port, 443);
        assert!(!Endpoint::parse("http://10.0.2.2:18650").unwrap().tls);
        assert_eq!(Endpoint::parse("https://[fd00::1]:8443").unwrap().host, "fd00::1");
        assert!(Endpoint::parse("https://nas/api").is_err());
        assert_eq!(Endpoint::parse("https://[fd00::1]:8443").unwrap().url(), "https://[fd00::1]:8443");
    }

    #[test]
    fn fingerprints() {
        assert_eq!(normalize_fp("aa bb:cc"), "AA:BB:CC");
        assert_eq!(fingerprint(b"").len(), 95);
    }

    #[test]
    fn chunked_and_keepalive() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            for _ in 0..2 {
                let mut line = String::new();
                let mut len = 0;
                loop {
                    line.clear();
                    r.read_line(&mut line).unwrap();
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if line.trim().is_empty() {
                        break;
                    }
                }
                let mut b = vec![0; len];
                r.read_exact(&mut b).unwrap();
                s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n").unwrap();
            }
        });
        let c = Client::new(Endpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap(), "");
        let r = c.request("POST", "/x", &[], &[b"12", b"345"]).unwrap();
        assert_eq!(r.body, b"abcde");
        let r = c.request("GET", "/y", &[], &[]).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, b"abcde".as_slice()));
    }

    /// A chunked upload arrives whole; a body that fails never sends the last chunk.
    #[test]
    fn chunked_upload() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = l.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                let mut chunked = false;
                loop {
                    line.clear();
                    r.read_line(&mut line).unwrap();
                    chunked |= line.to_ascii_lowercase().starts_with("transfer-encoding: chunked");
                    if line.trim().is_empty() {
                        break;
                    }
                }
                assert!(chunked);
                let mut body = Vec::new();
                let complete = loop {
                    line.clear();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        break false;
                    }
                    let n = usize::from_str_radix(line.trim(), 16).unwrap();
                    if n == 0 {
                        break true;
                    }
                    let start = body.len();
                    body.resize(start + n, 0);
                    r.read_exact(&mut body[start..]).unwrap();
                    let mut crlf = [0u8; 2];
                    r.read_exact(&mut crlf).unwrap();
                };
                seen.push((complete, body.len()));
                let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len().to_string().len(), body.len()).as_bytes());
            }
            seen
        });
        let c = Client::new(Endpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap(), "").with_token("t");
        let data = vec![9u8; 700_000];
        let r = c.upload("/up", &[("Content-Type", "application/zstd")], &mut data.as_slice(), Duration::from_secs(10)).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, b"700000".as_slice()));
        struct Failing(usize);
        impl Read for Failing {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::Error::other("btrfs send failed"));
                }
                self.0 -= 1;
                b[0] = 1;
                Ok(1)
            }
        }
        let e = c.upload("/up", &[], &mut Failing(3), Duration::from_secs(10)).unwrap_err();
        assert!(e.to_string().contains("btrfs send failed"));
        drop(c);
        let seen = server.join().unwrap();
        assert_eq!(seen, [(true, 700_000), (false, 3)]);
    }
}
