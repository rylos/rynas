//! A fake of the proxy's admin API on a Unix socket: `POST /load` stores the configuration
//! (or refuses it with 400 when told to), `GET /config/` returns it. Shared by the web
//! access and app tests.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

#[derive(Debug, Default)]
pub struct FakeState {
    pub config: Value,
    /// Every accepted load with its `Cache-Control: must-revalidate` flag.
    pub loads: Vec<(Value, bool)>,
    /// Refuse the next loads with this message.
    pub refuse: Option<String>,
    /// Refused loads.
    pub refused: usize,
}

#[derive(Clone)]
pub struct FakeCaddy {
    pub socket: PathBuf,
    pub state: Arc<Mutex<FakeState>>,
}

impl FakeCaddy {
    pub fn start(dir: &Path) -> Self {
        let socket = dir.join("admin.sock");
        let _ = std::fs::remove_file(&socket);
        let l = UnixListener::bind(&socket).unwrap();
        let state = Arc::new(Mutex::new(FakeState::default()));
        let st = state.clone();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(c) = c else { continue };
                let st = st.clone();
                std::thread::spawn(move || {
                    let mut w = c.try_clone().unwrap();
                    let mut r = BufReader::new(c);
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    let mut len = 0usize;
                    let mut force = false;
                    loop {
                        let mut h = String::new();
                        r.read_line(&mut h).unwrap();
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        let lower = h.to_ascii_lowercase();
                        if let Some(v) = lower.strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap();
                        }
                        if lower.starts_with("cache-control:") && lower.contains("must-revalidate")
                        {
                            force = true;
                        }
                    }
                    let mut body = vec![0u8; len];
                    r.read_exact(&mut body).unwrap();
                    let (status, out) = match (method.as_str(), path.as_str()) {
                        ("POST", "/load") => {
                            let mut s = st.lock().unwrap();
                            if let Some(m) = s.refuse.clone() {
                                s.refused += 1;
                                (400, serde_json::json!({"error": m}).to_string())
                            } else {
                                let v: Value = serde_json::from_slice(&body).unwrap();
                                s.config = v.clone();
                                s.loads.push((v, force));
                                (200, String::new())
                            }
                        }
                        ("GET", "/config/") => (200, st.lock().unwrap().config.to_string()),
                        _ => (404, "{\"error\":\"not found\"}".into()),
                    };
                    let _ = write!(
                        w,
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{out}",
                        out.len()
                    );
                });
            }
        });
        Self { socket, state }
    }

    pub fn loads(&self) -> usize {
        self.state.lock().unwrap().loads.len()
    }

    pub fn config(&self) -> Value {
        self.state.lock().unwrap().config.clone()
    }
}
