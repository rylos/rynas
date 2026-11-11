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
        language: NotifyLanguage::En,
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
            language: NotifyLanguage::It,
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
            language: NotifyLanguage::En,
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
