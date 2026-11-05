//! UPS status from NUT's upsd (`127.0.0.1:3493`), over its plain-text protocol through
//! [`rynas_nut`] (`LIST UPS`, `LIST VAR <ups>`, `LIST CMD <ups>`): no `upsc` child process, no
//! login (the read commands need none).
//!
//! [`UpsMonitor`] keeps the last reading and broadcasts it, serialized once, as
//! `ups.status` on `/api/v1/events` when it changes (ignoring `read_at_ms`) or every
//! [`REPEAT`]. upsd is asked only while some event stream listens, every [`POLL`] (every
//! [`POLL_IDLE`] while no UPS answers); `GET /api/v1/ups` reads it on demand when the last
//! reading is older than [`POLL`]. Talking to upsd never touches a disk.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rynas_api_types::ups::{UpsLink, UpsStatus};
pub use rynas_nut::{UPSD, read};
use tokio::sync::broadcast;

/// Poll period while a UPS answers.
pub const POLL: Duration = Duration::from_secs(5);
/// Poll period while upsd is down or has no UPS.
pub const POLL_IDLE: Duration = Duration::from_secs(30);
/// An unchanged status is broadcast again after this long.
pub const REPEAT: Duration = Duration::from_secs(60);

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

    fn reading(flags: &[&str]) -> UpsStatus {
        UpsStatus {
            link: UpsLink::Connected,
            name: Some("ups".into()),
            flags: flags.iter().map(|f| f.to_string()).collect(),
            battery_charge_pct: Some(87.0),
            read_at_ms: 1,
            ..Default::default()
        }
    }

    #[test]
    fn broadcast_on_change_only() {
        let m = UpsMonitor::new(UPSD);
        let mut rx = m.subscribe();
        let s = reading(&["OB", "DISCHRG"]);
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
}
