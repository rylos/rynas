//! Engine behaviour against in-process fakes: an HTTP server (webhook, ntfy) and an SMTP
//! server. The clock is manual; deliveries are driven with `deliver_due` / `reminders`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rynas_api_types::notify::*;
use rynas_notify::engine::{Engine, EngineOptions, ListQuery, NotifyError, utc_time};
use rynas_notify::event;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

// 2025-11-05 00:00 UTC, a Wednesday
const T0: u64 = 1_762_300_800_000;
const MIN: u64 = 60_000;

struct Env {
    engine: Engine,
    clock: Arc<AtomicU64>,
    dir: tempfile::TempDir,
}

fn options(dir: &std::path::Path, clock: &Arc<AtomicU64>) -> EngineOptions {
    let mut o = EngineOptions::new(dir.join("notify"), "nas");
    let c = clock.clone();
    o.clock = Arc::new(move || c.load(Ordering::SeqCst));
    o.local = utc_time;
    o
}

fn env_with(f: impl FnOnce(&mut EngineOptions)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(AtomicU64::new(T0));
    let mut o = options(dir.path(), &clock);
    f(&mut o);
    Env {
        engine: Engine::open(o).unwrap(),
        clock,
        dir,
    }
}

fn env() -> Env {
    env_with(|_| {})
}

impl Env {
    fn advance(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    fn reopen(self) -> Env {
        let Env { engine, clock, dir } = self;
        drop(engine);
        let o = options(dir.path(), &clock);
        Env {
            engine: Engine::open(o).unwrap(),
            clock,
            dir,
        }
    }
}

// ------------------------------------------------------------------------ fake HTTP

#[derive(Debug, Clone)]
struct Req {
    line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct FakeHttp {
    url: String,
    reqs: Arc<Mutex<Vec<Req>>>,
    status: Arc<AtomicU16>,
}

impl FakeHttp {
    fn taken(&self) -> Vec<Req> {
        std::mem::take(&mut *self.reqs.lock().unwrap())
    }
}

async fn fake_http() -> FakeHttp {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let reqs = Arc::new(Mutex::new(Vec::new()));
    let status = Arc::new(AtomicU16::new(200));
    let (r, s) = (reqs.clone(), status.clone());
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = l.accept().await else {
                return;
            };
            let (r, s) = (r.clone(), s.clone());
            tokio::spawn(async move {
                let mut rd = BufReader::new(sock);
                let mut line = String::new();
                rd.read_line(&mut line).await.unwrap();
                let mut headers = Vec::new();
                loop {
                    let mut h = String::new();
                    rd.read_line(&mut h).await.unwrap();
                    let h = h.trim_end().to_string();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
                let len: usize = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .map(|(_, v)| v.parse().unwrap())
                    .unwrap_or(0);
                let mut body = vec![0; len];
                rd.read_exact(&mut body).await.unwrap();
                r.lock().unwrap().push(Req {
                    line: line.trim_end().to_string(),
                    headers,
                    body: String::from_utf8(body).unwrap(),
                });
                let code = s.load(Ordering::SeqCst);
                let text = if code == 200 { "ok" } else { "boom" };
                let resp = format!(
                    "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                rd.into_inner().write_all(resp.as_bytes()).await.unwrap();
            });
        }
    });
    FakeHttp { url, reqs, status }
}

// ------------------------------------------------------------------------ fake SMTP

async fn fake_smtp() -> (u16, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let mails = Arc::new(Mutex::new(Vec::new()));
    let m = mails.clone();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = l.accept().await else {
                return;
            };
            let m = m.clone();
            tokio::spawn(async move {
                let (rd, mut wr) = sock.into_split();
                let mut rd = BufReader::new(rd);
                wr.write_all(b"220 fake ESMTP\r\n").await.unwrap();
                let mut envelope = String::new();
                loop {
                    let mut line = String::new();
                    if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let cmd = line.trim_end().to_ascii_uppercase();
                    if cmd.starts_with("EHLO") || cmd.starts_with("HELO") {
                        wr.write_all(b"250-fake\r\n250 8BITMIME\r\n").await.unwrap();
                    } else if cmd.starts_with("MAIL") || cmd.starts_with("RCPT") {
                        envelope.push_str(line.trim_end());
                        envelope.push('\n');
                        wr.write_all(b"250 OK\r\n").await.unwrap();
                    } else if cmd == "DATA" {
                        wr.write_all(b"354 go\r\n").await.unwrap();
                        let mut data = envelope.clone();
                        loop {
                            let mut l = String::new();
                            rd.read_line(&mut l).await.unwrap();
                            if l == ".\r\n" {
                                break;
                            }
                            data.push_str(&l);
                        }
                        m.lock().unwrap().push(data);
                        wr.write_all(b"250 queued\r\n").await.unwrap();
                    } else if cmd == "QUIT" {
                        wr.write_all(b"221 bye\r\n").await.unwrap();
                        return;
                    } else {
                        wr.write_all(b"250 OK\r\n").await.unwrap();
                    }
                }
            });
        }
    });
    (port, mails)
}

// ----------------------------------------------------------------------------- helpers

fn webhook(name: &str, url: &str) -> TargetRequest {
    TargetRequest {
        name: name.into(),
        enabled: true,
        language: NotifyLanguage::EN,
        config: TargetConfig::Webhook {
            url: format!("{url}/hook"),
            method: WebhookMethod::Post,
            headers: vec![WebhookHeader {
                name: "Authorization".into(),
                value: "Bearer {{ secrets.token }}".into(),
            }],
            body: None,
        },
        secrets: [("token".to_string(), Some("tok-123456".to_string()))].into(),
    }
}

fn rule(targets: Vec<u64>) -> NotifyRule {
    NotifyRule {
        name: "all".into(),
        enabled: true,
        targets,
        ..Default::default()
    }
}

fn disk_failing(sev: NotifySeverity) -> NewEvent {
    event::firing(
        NotifyCategory::Storage,
        sev,
        "storage",
        "storage:disk:S1:health",
        "Disk sda failing",
    )
}

fn titles(reqs: &[Req]) -> Vec<String> {
    reqs.iter()
        .map(|r| {
            let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
            v["subject"].as_str().unwrap().to_string()
        })
        .collect()
}

// ------------------------------------------------------------------------------- tests

#[tokio::test]
async fn state_machine_transitions_only() {
    let e = env();
    let http = fake_http().await;
    let t = e.engine.create_target(&webhook("hook", &http.url)).unwrap();
    e.engine.create_rule(&rule(vec![t.id])).unwrap();

    let n = e
        .engine
        .ingest(disk_failing(NotifySeverity::Warning), false)
        .unwrap()
        .unwrap();
    assert_eq!(n.state, EventState::Firing);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    let r = http.taken();
    assert_eq!(r[0].line, "POST /hook HTTP/1.1");
    assert_eq!(r[0].header("authorization"), Some("Bearer tok-123456"));
    assert_eq!(titles(&r), ["[nas] Disk sda failing"]);

    // repeat: counted, not sent
    e.advance(MIN);
    let again = e
        .engine
        .ingest(disk_failing(NotifySeverity::Warning), false)
        .unwrap()
        .unwrap();
    assert_eq!((again.id, again.count), (n.id, 2));
    assert_eq!(e.engine.deliver_due().await.unwrap(), 0);
    // escalation: sent, same row
    let esc = e
        .engine
        .ingest(disk_failing(NotifySeverity::Critical), false)
        .unwrap()
        .unwrap();
    assert_eq!((esc.id, esc.severity), (n.id, NotifySeverity::Critical));
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    // lower level again: silent
    e.engine
        .ingest(disk_failing(NotifySeverity::Warning), false)
        .unwrap();
    assert_eq!(e.engine.deliver_due().await.unwrap(), 0);
    http.taken();

    let res = event::resolved(
        NotifyCategory::Storage,
        "storage",
        "storage:disk:S1:health",
        "Disk sda healthy again",
    );
    let rn = e.engine.ingest(res.clone(), false).unwrap().unwrap();
    assert_eq!(rn.state, EventState::Resolved);
    // the resolution carries the severity of the condition, so the same rules match
    assert_eq!(rn.severity, NotifySeverity::Warning);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    assert_eq!(
        titles(&http.taken()),
        ["[nas] Resolved: Disk sda healthy again"]
    );
    // nothing firing: ignored
    assert!(e.engine.ingest(res, false).unwrap().is_none());
    let firing = e
        .engine
        .list(&ListQuery {
            limit: 50,
            state: Some(EventState::Firing),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(firing[0].resolved_ms, Some(T0 + MIN));
    // fires again later: a new notification
    let n2 = e
        .engine
        .ingest(disk_failing(NotifySeverity::Warning), false)
        .unwrap()
        .unwrap();
    assert_ne!(n2.id, n.id);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    http.taken();
    // a quiet resolution closes the condition and sends nothing
    let quiet = event::quietly(event::resolved(
        NotifyCategory::Storage,
        "storage",
        "storage:disk:S1:health",
        "Disk sda healthy again",
    ));
    let qn = e.engine.ingest(quiet, false).unwrap().unwrap();
    assert_eq!(qn.state, EventState::Resolved);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 0);
    assert!(http.taken().is_empty());
}

#[tokio::test]
async fn a_resolution_never_doubles_the_unread_list() {
    let e = env();
    let res = |key: &str| event::resolved(NotifyCategory::Update, "updates", key, "Back to normal");
    // a warning still unread: it stays (shown resolved), the resolution enters read
    let w = e.engine.ingest(disk_failing(NotifySeverity::Warning), false).unwrap().unwrap();
    let r = e.engine.ingest(event::resolved(NotifyCategory::Storage, "storage", "storage:disk:S1:health", "Disk sda healthy again"), false).unwrap().unwrap();
    assert!(r.acked_ms.is_some());
    assert_eq!(e.engine.unacked().unwrap(), 1);
    let unread = e.engine.list(&ListQuery { limit: 50, acked: Some(false), ..Default::default() }).unwrap();
    assert_eq!((unread[0].id, unread[0].resolved_ms.is_some()), (w.id, true));
    // a notice (updates available) that resolves leaves the list with its resolution
    e.engine.ingest(event::firing(NotifyCategory::Update, NotifySeverity::Notice, "updates", "updates", "3 updates available"), false).unwrap();
    assert_eq!(e.engine.unacked().unwrap(), 2);
    e.engine.ingest(res("updates"), false).unwrap();
    assert_eq!(e.engine.unacked().unwrap(), 1);
    // a problem already read: its resolution is news
    let w2 = e.engine.ingest(event::firing(NotifyCategory::Storage, NotifySeverity::Error, "storage", "raid", "RAID md2 degraded"), false).unwrap().unwrap();
    e.engine.ack(w2.id, "admin").unwrap();
    let r2 = e.engine.ingest(res("raid"), false).unwrap().unwrap();
    assert!(r2.acked_ms.is_none());
    assert_eq!(e.engine.unacked().unwrap(), 2);
}

#[tokio::test]
async fn oneshot_dedup_and_force() {
    let e = env();
    let ev = || {
        event::oneshot(
            NotifyCategory::Security,
            NotifySeverity::Warning,
            "auth",
            "auth:lockout:bob",
            "Login locked for bob",
        )
    };
    let a = e.engine.ingest(ev(), false).unwrap().unwrap();
    e.advance(5 * MIN);
    let b = e.engine.ingest(ev(), false).unwrap().unwrap();
    assert_eq!((b.id, b.count), (a.id, 2));
    let c = e.engine.ingest(ev(), true).unwrap().unwrap();
    assert_ne!(c.id, a.id);
    e.advance(11 * MIN);
    let d = e.engine.ingest(ev(), false).unwrap().unwrap();
    assert_ne!(d.id, c.id);
    // a oneshot without key gets source + title
    let mut x = ev();
    x.key = None;
    assert_eq!(
        e.engine.ingest(x, false).unwrap().unwrap().key,
        "auth:Login locked for bob"
    );
    // firing without key: refused
    let mut y = disk_failing(NotifySeverity::Error);
    y.key = None;
    assert!(matches!(
        e.engine.ingest(y, false),
        Err(NotifyError::Invalid(_))
    ));
}

#[tokio::test]
async fn grouping_window() {
    let e = env();
    let http = fake_http().await;
    let t = e.engine.create_target(&webhook("hook", &http.url)).unwrap();
    let mut r = rule(vec![t.id]);
    r.group_window_s = 120;
    e.engine.create_rule(&r).unwrap();
    for i in 0..3 {
        let ev = event::oneshot(
            NotifyCategory::Task,
            NotifySeverity::Error,
            "task",
            format!("task:{i}"),
            format!("Task {i} failed"),
        );
        e.engine.ingest(ev, false).unwrap();
        e.advance(30_000);
    }
    assert_eq!(e.engine.deliver_due().await.unwrap(), 0);
    // the window started with the first event
    assert_eq!(e.engine.next_due().unwrap(), Some(T0 + 120_000));
    e.advance(30_000);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    let r = http.taken();
    assert_eq!(titles(&r), ["[nas] 3 notifications (Error)"]);
    let body: serde_json::Value = serde_json::from_str(&r[0].body).unwrap();
    assert_eq!(body["events"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn quiet_hours_hold_but_never_critical() {
    let e = env(); // 00:00 UTC
    let http = fake_http().await;
    let t = e.engine.create_target(&webhook("hook", &http.url)).unwrap();
    let mut r = rule(vec![t.id]);
    r.quiet_hours = vec![QuietWindow {
        days: vec![],
        start: "23:00".into(),
        end: "07:00".into(),
    }];
    e.engine.create_rule(&r).unwrap();
    e.engine
        .ingest(disk_failing(NotifySeverity::Warning), false)
        .unwrap();
    let crit = event::firing(
        NotifyCategory::Storage,
        NotifySeverity::Critical,
        "storage",
        "storage:md:md2:health",
        "RAID md2 degraded",
    );
    e.engine.ingest(crit, false).unwrap();
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    assert_eq!(titles(&http.taken()), ["[nas] RAID md2 degraded"]);
    assert_eq!(e.engine.next_due().unwrap(), Some(T0 + 7 * 60 * MIN));
    e.advance(7 * 60 * MIN);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    assert_eq!(titles(&http.taken()), ["[nas] Disk sda failing"]);
}

#[tokio::test]
async fn silences_ack_and_list() {
    let e = env();
    let http = fake_http().await;
    let t = e.engine.create_target(&webhook("hook", &http.url)).unwrap();
    e.engine.create_rule(&rule(vec![t.id])).unwrap();
    let s = e
        .engine
        .silence(
            &SilenceRequest {
                key: "storage:disk:*".into(),
                duration_s: Some(3600),
            },
            "alice",
        )
        .unwrap();
    let n = e
        .engine
        .ingest(disk_failing(NotifySeverity::Critical), false)
        .unwrap()
        .unwrap();
    assert!(n.silenced);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 0);
    assert_eq!(e.engine.silences().unwrap().len(), 1);
    // silence over: other keys are delivered again
    e.engine.delete_silence(s.id).unwrap();
    assert!(matches!(
        e.engine.delete_silence(s.id),
        Err(NotifyError::NotFound(_))
    ));
    let crit = event::firing(
        NotifyCategory::Storage,
        NotifySeverity::Critical,
        "storage",
        "storage:md:md2:health",
        "RAID md2 degraded",
    );
    e.engine.ingest(crit, false).unwrap();
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);

    let mut rx = e.engine.subscribe();
    assert_eq!(e.engine.unacked().unwrap(), 2);
    let acked = e.engine.ack(n.id, "alice").unwrap();
    assert_eq!(acked.acked_by.as_deref(), Some("alice"));
    let ev: NotificationEvent = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!((ev.kind, ev.unacked), (NotificationChangeKind::Updated, 1));
    assert_eq!(e.engine.ack_all("bob").unwrap(), 1);
    let ev: NotificationEvent = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
    assert_eq!((ev.kind, ev.unacked), (NotificationChangeKind::AckedAll, 0));
    assert!(matches!(
        e.engine.ack(999, "x"),
        Err(NotifyError::NotFound(_))
    ));

    // filters and cursor
    let all = e
        .engine
        .list(&ListQuery {
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[0].id > all[1].id);
    let older = e
        .engine
        .list(&ListQuery {
            limit: 50,
            before: Some(all[0].id),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(older.len(), 1);
    let q = ListQuery {
        limit: 50,
        min_severity: Some(NotifySeverity::Critical),
        category: Some(NotifyCategory::Storage),
        acked: Some(true),
        ..Default::default()
    };
    assert_eq!(e.engine.list(&q).unwrap().len(), 2);
    let q = ListQuery {
        limit: 50,
        key: Some("storage:md:md2:health".into()),
        ..Default::default()
    };
    assert_eq!(e.engine.list(&q).unwrap().len(), 1);
    assert!(
        e.engine
            .silence(
                &SilenceRequest {
                    key: "*".into(),
                    duration_s: None
                },
                "a"
            )
            .is_err()
    );
}

#[tokio::test]
async fn outbox_retries_with_backoff_and_survives_restart() {
    let e = env();
    let http = fake_http().await;
    let t = e.engine.create_target(&webhook("hook", &http.url)).unwrap();
    e.engine.create_rule(&rule(vec![t.id])).unwrap();
    http.status.store(500, Ordering::SeqCst);
    e.engine
        .ingest(disk_failing(NotifySeverity::Error), false)
        .unwrap();
    assert_eq!(e.engine.deliver_due().await.unwrap(), 0);
    let st = e.engine.target(t.id).unwrap().status;
    assert_eq!(st.queued, 1);
    assert_eq!(st.last_error.as_deref(), Some("HTTP 500: boom"));
    // first retry after 30 s
    assert_eq!(e.engine.next_due().unwrap(), Some(T0 + 30_000));
    e.advance(30_000);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 0);
    // second after 60 s more
    assert_eq!(e.engine.next_due().unwrap(), Some(T0 + 90_000));
    assert_eq!(http.taken().len(), 2);

    // restart: the outbox is still there and goes out once the server is back
    let e = e.reopen();
    http.status.store(200, Ordering::SeqCst);
    e.advance(60_000);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    let st = e.engine.target(t.id).unwrap().status;
    assert_eq!(st.queued, 0);
    assert!(st.last_ok_ms.is_some());
    // state survived too: a repeat is not a new condition
    let n = e
        .engine
        .ingest(disk_failing(NotifySeverity::Error), false)
        .unwrap()
        .unwrap();
    assert_eq!(n.count, 2);
}

#[tokio::test]
async fn outbox_gives_up_after_max_attempts() {
    let e = env_with(|o| {
        o.max_attempts = 2;
    });
    let http = fake_http().await;
    let t = e.engine.create_target(&webhook("hook", &http.url)).unwrap();
    e.engine.create_rule(&rule(vec![t.id])).unwrap();
    http.status.store(503, Ordering::SeqCst);
    e.engine
        .ingest(disk_failing(NotifySeverity::Error), false)
        .unwrap();
    e.engine.deliver_due().await.unwrap();
    e.advance(30_000);
    e.engine.deliver_due().await.unwrap();
    assert_eq!(e.engine.next_due().unwrap(), None);
    assert_eq!(http.taken().len(), 2);
}

#[tokio::test]
async fn rate_limit_per_target() {
    let e = env_with(|o| o.per_target_per_minute = 2);
    let http = fake_http().await;
    let t = e.engine.create_target(&webhook("hook", &http.url)).unwrap();
    e.engine.create_rule(&rule(vec![t.id])).unwrap();
    for i in 0..3 {
        let ev = event::oneshot(
            NotifyCategory::App,
            NotifySeverity::Notice,
            "app",
            format!("k{i}"),
            format!("event {i}"),
        );
        e.engine.ingest(ev, false).unwrap();
    }
    assert_eq!(e.engine.deliver_due().await.unwrap(), 2);
    assert_eq!(e.engine.target(t.id).unwrap().status.queued, 1);
    e.advance(30_000);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
}

#[tokio::test]
async fn reminders_while_firing_until_acked() {
    let e = env();
    let http = fake_http().await;
    let t = e.engine.create_target(&webhook("hook", &http.url)).unwrap();
    let mut r = rule(vec![t.id]);
    r.repeat_interval_s = 3600;
    e.engine.create_rule(&r).unwrap();
    let n = e
        .engine
        .ingest(disk_failing(NotifySeverity::Error), false)
        .unwrap()
        .unwrap();
    e.engine.deliver_due().await.unwrap();
    http.taken();
    e.advance(30 * MIN);
    assert_eq!(e.engine.reminders(T0 + 30 * MIN).unwrap(), 0);
    e.advance(30 * MIN);
    assert_eq!(e.engine.reminders(T0 + 60 * MIN).unwrap(), 1);
    e.engine.deliver_due().await.unwrap();
    assert_eq!(titles(&http.taken()), ["[nas] Reminder: Disk sda failing"]);
    assert_eq!(e.engine.reminders(T0 + 61 * MIN).unwrap(), 0);
    e.engine.ack(n.id, "alice").unwrap();
    assert_eq!(e.engine.reminders(T0 + 200 * MIN).unwrap(), 0);
}

#[tokio::test]
async fn smtp_delivery() {
    let e = env();
    let (port, mails) = fake_smtp().await;
    let t = e
        .engine
        .create_target(&TargetRequest {
            name: "mail".into(),
            enabled: true,
            language: NotifyLanguage::new("it").unwrap(),
            config: TargetConfig::Smtp {
                host: "127.0.0.1".into(),
                port,
                security: SmtpSecurity::None,
                username: None,
                from: "nas@example.org".into(),
                to: vec!["admin@example.org".into()],
            },
            secrets: BTreeMap::new(),
        })
        .unwrap();
    e.engine.create_rule(&rule(vec![t.id])).unwrap();
    e.engine
        .ingest(disk_failing(NotifySeverity::Critical), false)
        .unwrap();
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    let m = mails.lock().unwrap().clone();
    assert_eq!(m.len(), 1);
    assert!(m[0].contains("RCPT TO:<admin@example.org>"), "{}", m[0]);
    assert!(m[0].contains("Subject: [nas] Disk sda failing"), "{}", m[0]);
    assert!(m[0].contains("Critico | Archiviazione"), "{}", m[0]);
    // the test message
    let r = e.engine.test_target(t.id, "alice").await.unwrap();
    assert!(r.ok, "{r:?}");
    assert!(mails.lock().unwrap()[1].contains("Notifica di prova"));
}

#[tokio::test]
async fn ntfy_delivery_and_failed_test() {
    let e = env();
    let http = fake_http().await;
    let t = e
        .engine
        .create_target(&TargetRequest {
            name: "phone".into(),
            enabled: true,
            language: NotifyLanguage::EN,
            config: TargetConfig::Ntfy {
                server: http.url.clone(),
                topic: "nas-alerts".into(),
                username: None,
            },
            secrets: [("token".to_string(), Some("tk_secret_value".to_string()))].into(),
        })
        .unwrap();
    let r = e.engine.test_target(t.id, "alice").await.unwrap();
    assert!(r.ok);
    let req = &http.taken()[0];
    assert!(
        req.line.starts_with(
            "POST /nas-alerts?title=%5Bnas%5D+Test+notification&priority=3&tags=rynas%2Cnotice "
        ),
        "{}",
        req.line
    );
    assert_eq!(req.header("authorization"), Some("Bearer tk_secret_value"));
    assert!(req.body.contains("If you can read this"));
    http.status.store(401, Ordering::SeqCst);
    let r = e.engine.test_target(t.id, "alice").await.unwrap();
    assert_eq!(r.error.as_deref(), Some("HTTP 401: boom"));
    assert_eq!(
        e.engine.target(t.id).unwrap().status.last_error.as_deref(),
        Some("HTTP 401: boom")
    );
    // unreachable server: an error without the secret
    let mut req = webhook("dead", "http://127.0.0.1:9");
    req.config = TargetConfig::Telegram {
        chat_id: "1".into(),
        api_url: Some("http://127.0.0.1:9".into()),
    };
    req.secrets = [(
        "bot_token".to_string(),
        Some("123:SECRET-TOKEN".to_string()),
    )]
    .into();
    let dead = e.engine.create_target(&req).unwrap();
    let r = e.engine.test_target(dead.id, "alice").await.unwrap();
    assert!(!r.ok);
    assert!(!r.error.unwrap().contains("SECRET-TOKEN"));
}

#[tokio::test]
async fn targets_and_rules_crud() {
    let e = env();
    // the built-in web interface target
    let ui = e.engine.targets().unwrap();
    assert_eq!(ui.len(), 1);
    assert_eq!(ui[0].config, TargetConfig::Ui {});
    let t = e
        .engine
        .create_target(&webhook("hook", "http://127.0.0.1:1"))
        .unwrap();
    assert_eq!(t.secrets, ["token"]);
    // stored encrypted
    let raw = std::fs::read(e.dir.path().join("notify/notify.db")).unwrap();
    assert!(!raw.windows(10).any(|w| w == b"tok-123456"));
    assert!(matches!(
        e.engine.create_target(&webhook("hook", "http://x")),
        Err(NotifyError::Exists(_))
    ));
    // update without secrets keeps them; null removes
    let mut upd = webhook("hook2", "http://127.0.0.1:1");
    upd.secrets.clear();
    upd.secrets.insert("extra".into(), Some("abcdef".into()));
    let t2 = e.engine.update_target(t.id, &upd).unwrap();
    assert_eq!(t2.secrets, ["extra", "token"]);
    upd.secrets = [("token".to_string(), None)].into();
    assert_eq!(
        e.engine.update_target(t.id, &upd).unwrap().secrets,
        ["extra"]
    );
    let mut bad = webhook("bad", "http://x");
    bad.config = TargetConfig::Webhook {
        url: "{% if %}".into(),
        method: WebhookMethod::Post,
        headers: vec![],
        body: None,
    };
    assert!(matches!(
        e.engine.create_target(&bad),
        Err(NotifyError::Invalid(_))
    ));
    bad.config = TargetConfig::Ntfy {
        server: "ftp://x".into(),
        topic: "t".into(),
        username: None,
    };
    assert!(matches!(
        e.engine.create_target(&bad),
        Err(NotifyError::Invalid(_))
    ));
    bad.config = TargetConfig::Smtp {
        host: "h".into(),
        port: 25,
        security: SmtpSecurity::None,
        username: None,
        from: "not an address".into(),
        to: vec!["a@b.c".into()],
    };
    assert!(matches!(
        e.engine.create_target(&bad),
        Err(NotifyError::Invalid(_))
    ));

    // rules
    assert!(matches!(
        e.engine.create_rule(&rule(vec![99])),
        Err(NotifyError::Invalid(_))
    ));
    let r = e.engine.create_rule(&rule(vec![t.id, t.id])).unwrap();
    assert_eq!(r.targets, [t.id]);
    assert!(matches!(
        e.engine.create_rule(&rule(vec![t.id])),
        Err(NotifyError::Exists(_))
    ));
    assert!(matches!(
        e.engine.delete_target(t.id),
        Err(NotifyError::Conflict(_))
    ));
    let mut r2 = r.clone();
    r2.min_severity = NotifySeverity::Error;
    assert_eq!(
        e.engine.update_rule(r.id, &r2).unwrap().min_severity,
        NotifySeverity::Error
    );
    assert_eq!(e.engine.rules().unwrap().len(), 1);
    e.engine.delete_rule(r.id).unwrap();
    assert!(matches!(e.engine.rule(r.id), Err(NotifyError::NotFound(_))));
    e.engine.delete_target(t.id).unwrap();
    assert!(matches!(
        e.engine.target(t.id),
        Err(NotifyError::NotFound(_))
    ));
}

#[tokio::test]
async fn imported_settings_once() {
    let dir = tempfile::tempdir().unwrap();
    let http = fake_http().await;
    let imp = rynas_notify::ImportedSettings {
        key: "x_import".into(),
        rule_name: "Imported from x".into(),
        targets: vec![webhook("hook", &http.url)],
        min_severity: NotifySeverity::Critical,
    };
    let clock = Arc::new(AtomicU64::new(T0));
    let mut o = options(dir.path(), &clock);
    o.imports = vec![imp.clone()];
    let e = Engine::open(o).unwrap();
    // the engine's own default target plus the imported one
    let n = e.targets().unwrap().len();
    assert!(n >= 1);
    let rules = e.rules().unwrap();
    let r = rules.iter().find(|r| r.name == "Imported from x").unwrap();
    assert_eq!(r.min_severity, NotifySeverity::Critical);
    drop(e);
    let mut o = options(dir.path(), &clock);
    o.imports = vec![imp];
    let e = Engine::open(o).unwrap();
    assert_eq!(e.targets().unwrap().len(), n, "imported once");
}

#[tokio::test]
async fn emit_and_run_loop() {
    use rynas_notify::Notifier;
    let e = env();
    let http = fake_http().await;
    let t = e.engine.create_target(&webhook("hook", &http.url)).unwrap();
    e.engine.create_rule(&rule(vec![t.id])).unwrap();
    // the real clock is not needed: run() reads the manual one
    tokio::spawn(e.engine.clone().run());
    e.engine.emit(disk_failing(NotifySeverity::Critical));
    for _ in 0..100 {
        if !http.reqs.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(titles(&http.taken()), ["[nas] Disk sda failing"]);
}

#[tokio::test]
async fn retention_keeps_open_conditions() {
    let e = env();
    let open = e
        .engine
        .ingest(disk_failing(NotifySeverity::Error), false)
        .unwrap()
        .unwrap();
    let old = event::oneshot(
        NotifyCategory::App,
        NotifySeverity::Info,
        "app",
        "a",
        "old fact",
    );
    e.engine.ingest(old, false).unwrap();
    let s = SilenceRequest {
        key: "x".into(),
        duration_s: Some(60),
    };
    e.engine.silence(&s, "a").unwrap();
    let later = T0 + 91 * 86_400_000;
    e.clock.store(later, Ordering::SeqCst);
    e.engine.purge(later).unwrap();
    let all = ListQuery {
        limit: 50,
        ..Default::default()
    };
    let left = e.engine.list(&all).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, open.id);
    assert!(e.engine.silences().unwrap().is_empty());
    // still firing after the purge: a repeat is counted on the kept row
    let n = e
        .engine
        .ingest(disk_failing(NotifySeverity::Error), false)
        .unwrap()
        .unwrap();
    assert_eq!((n.id, n.count), (open.id, 2));
}

// ------------------------------------------------------------------- push to mobile devices

/// Requests a fake push service received: line, headers (lower-case names), raw body.
type PushRequests = Arc<Mutex<Vec<(String, Vec<(String, String)>, Vec<u8>)>>>;

/// A fake push service: keeps each request's line, headers and raw body.
struct FakePush {
    url: String,
    reqs: PushRequests,
    status: Arc<AtomicU16>,
}

async fn fake_push() -> FakePush {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let reqs: PushRequests = Arc::default();
    let status = Arc::new(AtomicU16::new(201));
    let (r, s) = (reqs.clone(), status.clone());
    let base = url.clone();
    tokio::spawn(async move {
        while let Ok((sock, _)) = l.accept().await {
            let (r, s, here) = (r.clone(), s.clone(), base.clone());
            tokio::spawn(async move {
                let mut rd = BufReader::new(sock);
                let mut line = String::new();
                rd.read_line(&mut line).await.unwrap();
                let mut headers = Vec::new();
                loop {
                    let mut h = String::new();
                    rd.read_line(&mut h).await.unwrap();
                    let h = h.trim_end().to_string();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
                let len: usize = headers
                    .iter()
                    .find(|(k, _)| k == "content-length")
                    .map(|(_, v)| v.parse().unwrap())
                    .unwrap_or(0);
                let mut body = vec![0; len];
                rd.read_exact(&mut body).await.unwrap();
                r.lock().unwrap().push((line.trim_end().to_string(), headers, body));
                let code = s.load(Ordering::SeqCst);
                // a redirect points back here: a client that followed it would be seen
                let location = if (300..400).contains(&code) { format!("Location: {here}/elsewhere\r\n") } else { String::new() };
                let resp = format!("HTTP/1.1 {code} X\r\n{location}Content-Length: 0\r\nConnection: close\r\n\r\n");
                rd.into_inner().write_all(resp.as_bytes()).await.unwrap();
            });
        }
    });
    FakePush { url, reqs, status }
}

/// The user agent's keys of RFC 8291 Appendix A.
const UA_SECRET: &str = "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94";
const UA_PUBLIC: &str = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const UA_AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

fn b64u(s: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).unwrap()
}

#[derive(Default)]
struct Directory {
    /// Every device's subscription, unless it has one in `subs`.
    sub: Mutex<Option<rynas_api_types::devices::PushSubscription>>,
    subs: Mutex<BTreeMap<String, rynas_api_types::devices::PushSubscription>>,
    /// device id → its user
    owners: Mutex<BTreeMap<String, String>>,
    gone: Mutex<Vec<String>>,
}

impl rynas_notify::engine::webpush::PushDirectory for Directory {
    fn subscription(&self, device_id: &str) -> Option<rynas_api_types::devices::PushSubscription> {
        if let Some(s) = self.subs.lock().unwrap().get(device_id) {
            return Some(s.clone());
        }
        self.sub.lock().unwrap().clone()
    }
    fn owner(&self, device_id: &str) -> Option<String> {
        self.owners.lock().unwrap().get(device_id).cloned()
    }
    fn gone(&self, device_id: &str) {
        self.gone.lock().unwrap().push(device_id.to_string());
        *self.sub.lock().unwrap() = None;
    }
}

fn device_target(id: &str) -> TargetRequest {
    TargetRequest {
        name: "Pixel".into(),
        enabled: true,
        language: NotifyLanguage::EN,
        config: TargetConfig::Device {
            device_id: id.into(),
        },
        secrets: BTreeMap::new(),
    }
}

#[tokio::test]
async fn device_targets_push_encrypted_messages() {
    use rynas_api_types::devices::{PushPayload, PushSubscription};
    use rynas_notify::engine::webpush;
    let push = fake_push().await;
    let e = env_with(|o| o.push_loopback_http = true);
    let dir = Arc::new(Directory::default());
    *dir.sub.lock().unwrap() = Some(PushSubscription {
        endpoint: format!("{}/upXYZ?up=1", push.url),
        p256dh: UA_PUBLIC.into(),
        auth: UA_AUTH.into(),
    });
    e.engine.set_push_directory(dir.clone());
    // a device target has no secrets and names a device id
    let mut bad = device_target("d_00ff");
    bad.secrets.insert("token".into(), Some("x".into()));
    assert!(matches!(e.engine.create_target(&bad), Err(NotifyError::Invalid(_))));
    assert!(matches!(e.engine.create_target(&device_target("x/../y")), Err(NotifyError::Invalid(_))));
    let t = e.engine.create_target(&device_target("d_00ff")).unwrap();
    assert_eq!(t.config.kind(), "device");
    e.engine
        .create_rule(&NotifyRule {
            min_severity: NotifySeverity::Warning,
            ..rule(vec![t.id])
        })
        .unwrap();

    let n = e.engine.ingest(disk_failing(NotifySeverity::Critical), false).unwrap().unwrap();
    assert_eq!(e.engine.deliver_due().await.unwrap(), 1);
    let reqs = std::mem::take(&mut *push.reqs.lock().unwrap());
    assert_eq!(reqs.len(), 1);
    let (line, headers, body) = &reqs[0];
    assert!(line.starts_with("POST /upXYZ?up=1 "), "{line}");
    let h = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(h("content-encoding"), Some("aes128gcm"));
    assert_eq!(h("ttl"), Some("86400"));
    assert_eq!(h("urgency"), Some("high"));
    let secret: [u8; 32] = b64u(UA_SECRET).try_into().unwrap();
    let plain = webpush::decrypt(&secret, &b64u(UA_AUTH), body).unwrap();
    let p: PushPayload = serde_json::from_slice(&plain).unwrap();
    assert_eq!((p.v, p.id, p.title.as_str()), (1, n.id, "Disk sda failing"));
    assert_eq!((p.severity.as_str(), p.category.as_str(), p.source.as_str()), ("critical", "storage", "storage"));
    assert!(plain.len() <= rynas_api_types::devices::PUSH_MAX_PAYLOAD);

    // a test message goes out with normal urgency
    let r = e.engine.test_target(t.id, "admin").await.unwrap();
    assert!(r.ok, "{:?}", r.error);
    let reqs = std::mem::take(&mut *push.reqs.lock().unwrap());
    assert_eq!(reqs[0].1.iter().find(|(n, _)| n == "urgency").map(|(_, v)| v.as_str()), Some("normal"));

    // a redirect is not followed: the message stays for a retry, nothing goes elsewhere
    push.status.store(307, Ordering::SeqCst);
    let r = e.engine.test_target(t.id, "admin").await.unwrap();
    assert!(!r.ok && r.error.as_deref().is_some_and(|m| m.contains("307")), "{:?}", r.error);
    assert_eq!(push.reqs.lock().unwrap().len(), 1, "one request, no second one to a Location");
    push.reqs.lock().unwrap().clear();

    // the push service forgot the subscription: dropped, the message counts as handled
    push.status.store(410, Ordering::SeqCst);
    let warn = event::oneshot(NotifyCategory::System, NotifySeverity::Warning, "test", "t:1", "Fan noisy");
    e.engine.ingest(warn, false).unwrap();
    e.engine.deliver_due().await.unwrap();
    assert_eq!(*dir.gone.lock().unwrap(), vec!["d_00ff".to_string()]);
    e.advance(2 * 3_600_000);
    assert_eq!(e.engine.deliver_due().await.unwrap(), 0);
    assert_eq!(push.reqs.lock().unwrap().len(), 1, "no retry after a 410");
}

/// A restore request (rynasd's `pc_agent::approval`) goes straight to the phones of the
/// accounts allowed, rules aside, with its action and category, in each target's language;
/// a disabled target and other accounts' devices get nothing.
#[tokio::test]
async fn direct_pushes_carry_the_action_to_allowed_accounts_only() {
    use rynas_api_types::devices::{PushAction, PushPayload, PushSubscription};
    use rynas_notify::engine::webpush;
    let push = fake_push().await;
    let e = env_with(|o| o.push_loopback_http = true);
    let dir = Arc::new(Directory::default());
    for (id, user) in [("d_0a", "alice"), ("d_0b", "bob"), ("d_0c", "alice")] {
        dir.owners.lock().unwrap().insert(id.into(), user.into());
        dir.subs.lock().unwrap().insert(
            id.into(),
            PushSubscription { endpoint: format!("{}/up-{id}", push.url), p256dh: UA_PUBLIC.into(), auth: UA_AUTH.into() },
        );
    }
    e.engine.set_push_directory(dir.clone());
    e.engine.create_target(&device_target("d_0a")).unwrap();
    e.engine.create_target(&TargetRequest { name: "Bob".into(), ..device_target("d_0b") }).unwrap();
    e.engine.create_target(&TargetRequest { name: "Off".into(), enabled: false, ..device_target("d_0c") }).unwrap();
    // no rule at all: a direct push does not need one
    let mut ev = event::firing(NotifyCategory::Backup, NotifySeverity::Info, "pcbackup", "pcbackup:restore-request:00ff", "Restore of casa waits for approval");
    ev.code = "pcbackup.restore_request".into();
    let n = e.engine.ingest(ev, false).unwrap().unwrap();
    let action = PushAction {
        kind: "pc_restore".into(),
        request: "00000000000000ff".into(),
        pc: "casa".into(),
        device: None,
        code: "042913".into(),
        from_ip: "192.0.2.10".into(),
        expires_ms: 1,
    };
    let sent = e.engine.push_direct(&n, "pcbackup.restore_request", Some(action.clone()), &|owner| owner == "alice").await;
    assert_eq!(sent, 1);
    let reqs = std::mem::take(&mut *push.reqs.lock().unwrap());
    assert_eq!(reqs.len(), 1);
    let (line, headers, body) = &reqs[0];
    assert!(line.starts_with("POST /up-d_0a "), "{line}");
    assert_eq!(headers.iter().find(|(k, _)| k == "urgency").map(|(_, v)| v.as_str()), Some("high"));
    let secret: [u8; 32] = b64u(UA_SECRET).try_into().unwrap();
    let plain = webpush::decrypt(&secret, &b64u(UA_AUTH), body).unwrap();
    let p: PushPayload = serde_json::from_slice(&plain).unwrap();
    assert_eq!((p.id, p.category.as_str(), p.severity.as_str(), p.source.as_str()), (n.id, "pcbackup.restore_request", "info", "pcbackup"));
    assert_eq!(p.title, "Restore of casa waits for approval");
    assert_eq!(p.action, Some(action));
    // the JSON shape the app reads: `action` with a null device
    let j: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(j["action"]["kind"], "pc_restore");
    assert!(j["action"]["device"].is_null());
    // a plain notification has no action key at all
    assert!(serde_json::to_value(PushPayload::default()).unwrap().get("action").is_none());
}

/// A file transfer's end as rynasd's file manager emits it.
fn files_job(code: &str, sev: NotifySeverity, user: &str, job: u64, state: &str) -> NewEvent {
    let mut ev = event::oneshot(NotifyCategory::Files, sev, "files", format!("files:job:{job}"), "Copy finished");
    ev.body = "Film.mkv → /Video · 45.8 GiB in 7 min".into();
    ev.code = code.into();
    for (k, v) in [("job_id", job.to_string()), ("state", state.into()), ("user", user.into()), ("kind", "copy".into())] {
        ev.fields.insert(k.into(), v);
    }
    ev
}

#[tokio::test]
async fn files_jobs_reach_only_their_owners_devices() {
    use rynas_api_types::devices::{PushPayload, PushSubscription};
    use rynas_notify::engine::webpush;
    let push = fake_push().await;
    let e = env_with(|o| o.push_loopback_http = true);
    let dir = Arc::new(Directory::default());
    for (id, user) in [("d_0a", "alice"), ("d_0b", "bob")] {
        dir.owners.lock().unwrap().insert(id.into(), user.into());
        dir.subs.lock().unwrap().insert(
            id.into(),
            PushSubscription { endpoint: format!("{}/up-{user}", push.url), p256dh: UA_PUBLIC.into(), auth: UA_AUTH.into() },
        );
    }
    e.engine.set_push_directory(dir.clone());
    let ta = e.engine.create_target(&device_target("d_0a")).unwrap();
    let tb = e.engine.create_target(&TargetRequest { name: "Phone".into(), ..device_target("d_0b") }).unwrap();
    // the devices' default rules: warnings and above, and each owner's own transfers
    e.engine.create_rule(&NotifyRule { min_severity: NotifySeverity::Warning, ..rule(vec![ta.id, tb.id]) }).unwrap();
    for (user, t) in [("alice", ta.id), ("bob", tb.id)] {
        e.engine
            .create_rule(&NotifyRule {
                name: format!("files {user}"),
                categories: vec![NotifyCategory::Files],
                fields: vec![FieldMatch { field: "user".into(), value: user.into(), regex: false }],
                ..rule(vec![t])
            })
            .unwrap();
    }
    // an administrator's rule naming the category for both devices: still each owner's only
    e.engine
        .create_rule(&NotifyRule { name: "all files".into(), categories: vec![NotifyCategory::Files], ..rule(vec![ta.id, tb.id]) })
        .unwrap();
    let delivered = || {
        let reqs = std::mem::take(&mut *push.reqs.lock().unwrap());
        let secret: [u8; 32] = b64u(UA_SECRET).try_into().unwrap();
        reqs.iter()
            .map(|(line, _, body)| {
                let plain = webpush::decrypt(&secret, &b64u(UA_AUTH), body).unwrap();
                assert!(plain.len() <= rynas_api_types::devices::PUSH_MAX_PAYLOAD);
                let p: PushPayload = serde_json::from_slice(&plain).unwrap();
                (line.split(' ').nth(1).unwrap().to_string(), p)
            })
            .collect::<Vec<_>>()
    };

    e.engine.ingest(files_job("files.job_done", NotifySeverity::Info, "alice", 7, "done"), false).unwrap().unwrap();
    e.engine.deliver_due().await.unwrap();
    let got = delivered();
    assert_eq!(got.len(), 1, "{got:?}");
    let (path, p) = &got[0];
    assert_eq!(path, "/up-alice");
    assert_eq!((p.job_id.as_str(), p.state.as_str(), p.user.as_str()), ("7", "done", "alice"));
    assert_eq!((p.category.as_str(), p.title.as_str()), ("files", "Copy finished"));

    // an error: the warning rule does not take it to alice's phone, bob's rules take it to his
    e.engine.ingest(files_job("files.job_failed", NotifySeverity::Error, "bob", 8, "canceled"), false).unwrap().unwrap();
    e.engine.deliver_due().await.unwrap();
    let got = delivered();
    assert_eq!(got.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), vec!["/up-bob"]);
    assert_eq!((got[0].1.job_id.as_str(), got[0].1.state.as_str(), got[0].1.user.as_str()), ("8", "canceled", "bob"));

    // nobody's (no user field), or a user without a device: no push
    let mut orphan = files_job("files.job_done", NotifySeverity::Info, "carol", 9, "done");
    e.engine.ingest(orphan.clone(), false).unwrap();
    orphan.fields.remove("user");
    orphan.key = Some("files:job:10".into());
    e.engine.ingest(orphan, false).unwrap();
    e.engine.deliver_due().await.unwrap();
    assert!(delivered().is_empty());

    // other events carry no job fields
    e.engine.ingest(disk_failing(NotifySeverity::Critical), false).unwrap();
    e.engine.deliver_due().await.unwrap();
    let got = delivered();
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|(_, p)| p.job_id.is_empty() && p.state.is_empty() && p.user.is_empty()));
}
