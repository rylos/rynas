//! UPS status from NUT's upsd (`127.0.0.1:3493`), over its plain-text protocol (`LIST UPS`,
//! `LIST VAR <ups>`): no `upsc` child process, no login (the read commands need none).
//!
//! [`UpsMonitor`] keeps the last reading and broadcasts it, serialized once, as
//! `ups.status` on `/api/v1/events` when it changes (ignoring `read_at_ms`) or every
//! [`REPEAT`]. upsd is asked only while some event stream listens, every [`POLL`] (every
//! [`POLL_IDLE`] while no UPS answers); `GET /api/v1/ups` reads it on demand when the last
//! reading is older than [`POLL`]. Talking to upsd never touches a disk.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rynas_api_types::ups::{UpsLink, UpsStatus};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

/// upsd's address (synodeb binds it to localhost unless `NETWORK_CLIENTS` is set).
pub const UPSD: &str = "127.0.0.1:3493";
/// Poll period while a UPS answers.
pub const POLL: Duration = Duration::from_secs(5);
/// Poll period while upsd is down or has no UPS.
pub const POLL_IDLE: Duration = Duration::from_secs(30);
/// An unchanged status is broadcast again after this long.
pub const REPEAT: Duration = Duration::from_secs(60);
/// Whole conversation with upsd (connect, two lists, logout).
const TIMEOUT: Duration = Duration::from_secs(2);
/// Largest answer read from upsd (a UPS has well under 100 variables).
const MAX_ANSWER: usize = 64 << 10;

/// One NUT answer line split into words; quoted words unescaped (`\"`, `\\`).
pub fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = line.chars().peekable();
    loop {
        while it.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            it.next();
        }
        let Some(&c) = it.peek() else { break };
        let mut w = String::new();
        if c == '"' {
            it.next();
            while let Some(c) = it.next() {
                match c {
                    '\\' => {
                        if let Some(n) = it.next() {
                            w.push(n);
                        }
                    }
                    '"' => break,
                    c => w.push(c),
                }
            }
        } else {
            while let Some(&c) = it.peek() {
                if c.is_ascii_whitespace() {
                    break;
                }
                w.push(c);
                it.next();
            }
        }
        out.push(w);
    }
    out
}

/// UPS names of a `LIST UPS` answer (`UPS <name> "<description>"` lines).
pub fn parse_list_ups(text: &str) -> Vec<String> {
    text.lines()
        .map(words)
        .filter(|w| w.len() >= 2 && w[0] == "UPS")
        .map(|mut w| w.swap_remove(1))
        .collect()
}

/// `(variable, value)` pairs of a `LIST VAR <ups>` answer (`VAR <ups> <var> "<value>"`).
pub fn parse_list_var(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(words)
        .filter(|w| w.len() >= 4 && w[0] == "VAR")
        .map(|mut w| {
            let v = w.swap_remove(3);
            (w.swap_remove(2), v)
        })
        .collect()
}

/// The status of UPS `name` from its variables.
pub fn status_of(name: &str, vars: &[(String, String)], now_ms: u64) -> UpsStatus {
    let get = |k: &str| {
        vars.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let num = |k: &str| get(k).and_then(|v| v.parse::<f32>().ok());
    UpsStatus {
        link: UpsLink::Connected,
        name: Some(name.to_string()),
        flags: get("ups.status")
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
        battery_charge_pct: num("battery.charge").map(|v| v.clamp(0.0, 100.0)),
        battery_runtime_s: num("battery.runtime")
            .filter(|v| *v >= 0.0)
            .map(|v| v as u64),
        load_pct: num("ups.load").map(|v| v.max(0.0)),
        input_voltage_v: num("input.voltage"),
        output_voltage_v: num("output.voltage"),
        manufacturer: get("device.mfr").or_else(|| get("ups.mfr")),
        model: get("device.model").or_else(|| get("ups.model")),
        serial: get("device.serial").or_else(|| get("ups.serial")),
        beeper: get("ups.beeper.status"),
        battery_date: get("battery.date").or_else(|| get("battery.mfr.date")),
        test_result: get("ups.test.result"),
        driver: get("driver.name"),
        read_at_ms: now_ms,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Sends `cmd` and reads until a line equal to `end` (or starting with `ERR`).
async fn ask(s: &TcpStream, cmd: &str, end: &str, buf: &mut Vec<u8>) -> std::io::Result<String> {
    let mut out = cmd.as_bytes();
    while !out.is_empty() {
        s.writable().await?;
        match s.try_write(out) {
            Ok(n) => out = &out[n..],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    let start = buf.len();
    let mut chunk = [0u8; 4096];
    loop {
        let text = String::from_utf8_lossy(&buf[start..]);
        if let Some(l) = text
            .lines()
            .find(|l| *l == end || l.starts_with("ERR "))
            .filter(|_| text.ends_with('\n'))
        {
            if l.starts_with("ERR ") {
                return Err(std::io::Error::other(l.to_string()));
            }
            return Ok(text.into_owned());
        }
        if buf.len() > MAX_ANSWER {
            return Err(std::io::Error::other("answer too long"));
        }
        s.readable().await?;
        match s.try_read(&mut chunk) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
}

async fn converse(addr: &str) -> std::io::Result<UpsStatus> {
    let s = TcpStream::connect(addr).await?;
    let mut buf = Vec::new();
    let list = ask(&s, "LIST UPS\n", "END LIST UPS", &mut buf).await?;
    let Some(name) = parse_list_ups(&list).into_iter().next() else {
        return Ok(UpsStatus {
            link: UpsLink::NotConfigured,
            read_at_ms: now_ms(),
            ..Default::default()
        });
    };
    if name.is_empty() || name.contains(|c: char| c.is_ascii_whitespace() || c == '"') {
        return Err(std::io::Error::other("invalid UPS name"));
    }
    let vars = ask(
        &s,
        &format!("LIST VAR {name}\n"),
        &format!("END LIST VAR {name}"),
        &mut buf,
    )
    .await?;
    let _ = s.try_write(b"LOGOUT\n");
    Ok(status_of(&name, &parse_list_var(&vars), now_ms()))
}

/// One reading of upsd at `addr`; an unreachable or broken upsd is [`UpsLink::Unreachable`].
pub async fn read(addr: &str) -> UpsStatus {
    match tokio::time::timeout(TIMEOUT, converse(addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::debug!("upsd {addr}: {e}");
            UpsStatus {
                read_at_ms: now_ms(),
                ..Default::default()
            }
        }
        Err(_) => {
            tracing::debug!("upsd {addr}: no answer in {TIMEOUT:?}");
            UpsStatus {
                read_at_ms: now_ms(),
                ..Default::default()
            }
        }
    }
}

struct Last {
    at: Instant,
    sent_at: Option<Instant>,
    status: UpsStatus,
    json: Arc<str>,
}

/// Last UPS reading and its subscribers.
pub struct UpsMonitor {
    addr: String,
    last: Mutex<Option<Last>>,
    tx: broadcast::Sender<Arc<str>>,
    /// One reading at a time (single flight between the poller and `GET`).
    flight: tokio::sync::Mutex<()>,
}

fn same(a: &UpsStatus, b: &UpsStatus) -> bool {
    UpsStatus {
        read_at_ms: 0,
        ..a.clone()
    } == UpsStatus {
        read_at_ms: 0,
        ..b.clone()
    }
}

impl UpsMonitor {
    pub fn new(addr: impl Into<String>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(4);
        Arc::new(Self {
            addr: addr.into(),
            last: Mutex::new(None),
            tx,
            flight: tokio::sync::Mutex::new(()),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.tx.subscribe()
    }

    /// The last broadcast reading, serialized.
    pub fn latest_json(&self) -> Option<Arc<str>> {
        let l = self.last.lock().unwrap();
        l.as_ref()
            .filter(|l| l.sent_at.is_some())
            .map(|l| l.json.clone())
    }

    /// Stores a reading; broadcasts it when it changed or [`REPEAT`] passed. Returns
    /// whether it was broadcast.
    pub fn push(&self, s: UpsStatus) -> bool {
        let now = Instant::now();
        let mut l = self.last.lock().unwrap();
        let send = match l.as_ref() {
            Some(p) => !same(&p.status, &s) || p.sent_at.is_none_or(|t| now - t >= REPEAT),
            None => true,
        };
        let json: Arc<str> = match serde_json::to_string(&s) {
            Ok(j) => j.into(),
            Err(e) => {
                tracing::error!("serializing the UPS status: {e}");
                return false;
            }
        };
        let sent_at = if send {
            // no subscriber is fine
            let _ = self.tx.send(json.clone());
            Some(now)
        } else {
            l.as_ref().and_then(|p| p.sent_at)
        };
        *l = Some(Last {
            at: now,
            sent_at,
            status: s,
            json,
        });
        send
    }

    fn fresh(&self, within: Duration) -> Option<UpsStatus> {
        let l = self.last.lock().unwrap();
        l.as_ref()
            .filter(|l| l.at.elapsed() < within)
            .map(|l| l.status.clone())
    }

    /// The current status: the last reading when younger than [`POLL`], else a new one.
    pub async fn status(&self) -> UpsStatus {
        if let Some(s) = self.fresh(POLL) {
            return s;
        }
        let _g = self.flight.lock().await;
        if let Some(s) = self.fresh(POLL) {
            return s;
        }
        let s = read(&self.addr).await;
        self.push(s.clone());
        s
    }

    /// The poller: asks upsd only while some stream listens.
    pub fn spawn(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let period = if me.tx.receiver_count() == 0 {
                    POLL
                } else {
                    let s = me.status().await;
                    if s.link == UpsLink::Connected {
                        POLL
                    } else {
                        POLL_IDLE
                    }
                };
                tokio::time::sleep(period).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VARS: &str = "BEGIN LIST VAR ups\n\
        VAR ups battery.charge \"87\"\n\
        VAR ups battery.runtime \"2460\"\n\
        VAR ups battery.mfr.date \"2023/04/12\"\n\
        VAR ups device.mfr \"American Power Conversion\"\n\
        VAR ups device.model \"Back-UPS ES 700G\"\n\
        VAR ups device.serial \"5B1234X56789  \"\n\
        VAR ups driver.name \"usbhid-ups\"\n\
        VAR ups input.voltage \"229.0\"\n\
        VAR ups ups.beeper.status \"enabled\"\n\
        VAR ups ups.load \"18\"\n\
        VAR ups ups.status \"OB DISCHRG\"\n\
        VAR ups ups.test.result \"Done and \\\"passed\\\"\"\n\
        END LIST VAR ups\n";

    #[test]
    fn parses_lists() {
        assert_eq!(
            parse_list_ups("BEGIN LIST UPS\nUPS ups \"APC on USB\"\nEND LIST UPS\n"),
            ["ups"]
        );
        assert!(parse_list_ups("BEGIN LIST UPS\nEND LIST UPS\n").is_empty());
        let v = parse_list_var(VARS);
        assert_eq!(v.len(), 12);
        let s = status_of("ups", &v, 5);
        assert_eq!(s.link, UpsLink::Connected);
        assert_eq!(s.flags, ["OB", "DISCHRG"]);
        assert!(s.on_battery() && !s.low_battery());
        assert_eq!(s.battery_charge_pct, Some(87.0));
        assert_eq!(s.battery_runtime_s, Some(2460));
        assert_eq!(s.load_pct, Some(18.0));
        assert_eq!(s.input_voltage_v, Some(229.0));
        assert_eq!(s.output_voltage_v, None);
        assert_eq!(s.model.as_deref(), Some("Back-UPS ES 700G"));
        assert_eq!(s.serial.as_deref(), Some("5B1234X56789"));
        assert_eq!(s.battery_date.as_deref(), Some("2023/04/12"));
        assert_eq!(s.beeper.as_deref(), Some("enabled"));
        assert_eq!(s.test_result.as_deref(), Some("Done and \"passed\""));
        assert_eq!(s.read_at_ms, 5);
    }

    #[test]
    fn broadcast_on_change_only() {
        let m = UpsMonitor::new(UPSD);
        let mut rx = m.subscribe();
        let s = status_of("ups", &parse_list_var(VARS), 1);
        assert!(m.push(s.clone()));
        assert!(!m.push(UpsStatus {
            read_at_ms: 2,
            ..s.clone()
        }));
        let mut t = s.clone();
        t.flags.push("LB".into());
        assert!(m.push(t));
        assert!(rx.try_recv().unwrap().contains("DISCHRG"));
        assert!(rx.try_recv().unwrap().contains("\"LB\""));
        assert!(m.latest_json().unwrap().contains("\"LB\""));
    }

    async fn fake_upsd(answer_vars: bool) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 256];
            let mut lists = 0;
            loop {
                s.readable().await.unwrap();
                match s.try_read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => continue,
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let n = text.matches('\n').count();
                while lists < n {
                    let line = text.lines().nth(lists).unwrap_or("");
                    let reply = if line == "LIST UPS" {
                        if answer_vars {
                            "BEGIN LIST UPS\nUPS ups \"x\"\nEND LIST UPS\n".to_string()
                        } else {
                            "BEGIN LIST UPS\nEND LIST UPS\n".to_string()
                        }
                    } else if line == "LIST VAR ups" {
                        VARS.to_string()
                    } else {
                        "OK Goodbye\n".to_string()
                    };
                    s.writable().await.unwrap();
                    let _ = s.try_write(reply.as_bytes());
                    lists += 1;
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn reads_a_fake_upsd() {
        let addr = fake_upsd(true).await;
        let s = read(&addr).await;
        assert_eq!(s.link, UpsLink::Connected);
        assert_eq!(s.battery_charge_pct, Some(87.0));
        let addr = fake_upsd(false).await;
        assert_eq!(read(&addr).await.link, UpsLink::NotConfigured);
        // nothing listens there
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = l.local_addr().unwrap().to_string();
        drop(l);
        assert_eq!(read(&dead).await.link, UpsLink::Unreachable);
    }
}
