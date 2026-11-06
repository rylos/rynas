//! synodeb integration (S1 in `docs/synodeb-integration.md`).
//!
//! - [`parse_forward`]: the body synodeb's `notify` posts to `/run/rynas/notify.sock`
//!   (`POST /v1/notify`, JSON or `application/x-www-form-urlencoded`), mapped to a
//!   [`NewEvent`]: priority low/normal/high/urgent → info/notice/warning/critical (synodeb's
//!   own syslog mapping), category from the source, source `synodeb:<source>`, oneshot with
//!   key `synodeb:<source>:<hash of title and message>` (synodeb's repeat suppression).
//!   Optional, backwards compatible: `event` (code `synodeb.<event>`), parameters
//!   (`field.<name>=` in a form, a `fields` object in JSON) and `format` (`text`|`log`);
//!   invalid ones are dropped, never the notification.
//! - [`import`]: `/etc/synodeb/notify.conf` (shell `KEY=value` lines, never sourced) turned
//!   into targets and the minimum severity of an "Imported from synodeb" rule.

use std::collections::BTreeMap;

use rynas_api_types::notify::{SmtpSecurity, TargetConfig, TargetRequest};
use sha2::{Digest, Sha256};

use crate::{BodyFormat, EventState, NewEvent, NotifyCategory as Cat, NotifySeverity as Sev};

/// Largest body accepted on the socket.
pub const MAX_BODY: usize = 64 << 10;

/// synodeb priority → severity.
pub fn severity(priority: &str) -> Sev {
    match priority.trim() {
        "low" => Sev::Info,
        "high" => Sev::Warning,
        "urgent" => Sev::Critical,
        _ => Sev::Notice,
    }
}

/// Category from the synodeb caller.
pub fn category(source: &str) -> Cat {
    let s = source.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| s.contains(w));
    if has(&["smart", "mdadm", "raid", "btrfs", "disk", "storage"]) {
        Cat::Storage
    } else if has(&["ups", "nut"]) {
        Cat::Ups
    } else if has(&["synohwd", "hw", "fan", "thermal", "temp"]) {
        Cat::Hardware
    } else if has(&["dom", "trial", "kernel", "apt", "update", "upgrade"]) {
        Cat::Update
    } else {
        Cat::System
    }
}

/// The source as a short word: lowercase letters, digits, `.`, `_`, `-`; at most 32.
fn source_word(s: &str) -> String {
    let w: String = s
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(32)
        .collect();
    if w.is_empty() { "notify".into() } else { w }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `application/x-www-form-urlencoded` pairs.
pub fn form_decode(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(p), String::new()),
        })
        .collect()
}

#[derive(serde::Deserialize, Default)]
struct Forward {
    title: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    priority: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    force: bool,
    /// Stable event code, `[a-z0-9_]{1,32}` (any other JSON value is ignored).
    #[serde(default)]
    event: serde_json::Value,
    /// Event parameters (JSON: an object of strings; form: `field.<name>=value`).
    #[serde(default)]
    fields: serde_json::Value,
    /// `text` (default) or `log`.
    #[serde(default)]
    format: serde_json::Value,
}

impl Forward {
    fn event(&self) -> &str {
        self.event.as_str().filter(|e| word_ok(e)).unwrap_or("")
    }

    fn format(&self) -> BodyFormat {
        match self.format.as_str() {
            Some("log") => BodyFormat::Log,
            _ => BodyFormat::Text,
        }
    }

    /// The valid parameters (JSON values that are not strings are dropped).
    fn params(&self) -> BTreeMap<String, String> {
        let Some(obj) = self.fields.as_object() else {
            return BTreeMap::new();
        };
        valid_fields(
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))),
        )
    }
}

/// Most parameters of a forward.
pub const MAX_FIELDS: usize = 16;
/// Longest parameter value (characters).
pub const MAX_FIELD_VALUE: usize = 200;

/// An event code or a parameter name: `[a-z0-9_]`, 1 to 32 characters.
pub fn word_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A parameter value: one line of at most [`MAX_FIELD_VALUE`] characters (may be empty).
fn value_ok(v: &str) -> bool {
    v.chars().count() <= MAX_FIELD_VALUE && !v.chars().any(char::is_control)
}

/// The valid parameters, at most [`MAX_FIELDS`] (the first ones by name); the others are
/// dropped.
fn valid_fields(raw: impl IntoIterator<Item = (String, String)>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in raw {
        if out.len() >= MAX_FIELDS {
            break;
        }
        if word_ok(&k) && value_ok(&v) {
            out.insert(k, v);
        }
    }
    out
}

/// synohwd notifications that are conditions of [`crate::checks::HardwareWatch`]: by event
/// code (`fan_failure`, `fans_ok`), or by title for senders without codes. synohwd sends
/// with source `hw` (older builds: `synohwd`).
pub fn synohwd_condition(
    source: &str,
    event: &str,
    title: &str,
) -> Option<(&'static str, EventState)> {
    if source != "hw" && source != "synohwd" {
        return None;
    }
    match (event, title.trim()) {
        ("fan_failure", _) | ("", "fan failure") => Some(("hardware:fan_fail", EventState::Firing)),
        ("fans_ok", _) | ("", "fans ok again") => Some(("hardware:fan_fail", EventState::Resolved)),
        _ => None,
    }
}

/// The event of a forward and whether `-f` (force: no dedup) was given.
pub fn parse_forward(content_type: Option<&str>, body: &[u8]) -> Result<(NewEvent, bool), String> {
    if body.len() > MAX_BODY {
        return Err(format!("body over {MAX_BODY} bytes"));
    }
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let f: Forward = if ct.starts_with("application/json") {
        serde_json::from_slice(body).map_err(|e| format!("JSON: {e}"))?
    } else if ct.starts_with("application/x-www-form-urlencoded") {
        let text = std::str::from_utf8(body).map_err(|_| "body is not UTF-8")?;
        let mut f = Forward::default();
        let mut fields = serde_json::Map::new();
        for (k, v) in form_decode(text) {
            match k.as_str() {
                "title" => f.title = v,
                "message" => f.message = v,
                "priority" => f.priority = v,
                "source" => f.source = v,
                "force" => f.force = matches!(v.as_str(), "1" | "true" | "yes"),
                "event" => f.event = v.into(),
                "format" => f.format = v.into(),
                _ => {
                    // the first value of a repeated parameter counts
                    if let Some(name) = k.strip_prefix("field.")
                        && !fields.contains_key(name)
                    {
                        fields.insert(name.to_string(), v.into());
                    }
                }
            }
        }
        f.fields = fields.into();
        f
    } else {
        return Err(
            "content type must be application/json or application/x-www-form-urlencoded".into(),
        );
    };
    if f.title.trim().is_empty() {
        return Err("title is required".into());
    }
    let src = source_word(&f.source);
    let digest = Sha256::digest(format!("{}\n{}", f.title, f.message).as_bytes());
    let hash: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    // synohwd's fan alarms are the conditions rynas's hardware watch already tracks: same key,
    // so the engine counts them as one (no duplicate notification)
    let event = f.event().to_string();
    let (key, state, sev) = match synohwd_condition(&src, &event, &f.title) {
        Some((key, state)) => (
            key.to_string(),
            state,
            match state {
                EventState::Resolved => Sev::Info,
                _ => Sev::Critical,
            },
        ),
        None => (
            format!("synodeb:{src}:{hash}"),
            EventState::Oneshot,
            severity(&f.priority),
        ),
    };
    let mut fields = f.params();
    let priority = f.priority.trim();
    if !priority.is_empty() {
        fields.insert("priority".to_string(), priority.to_string());
    }
    let body_format = f.format();
    let ev = NewEvent {
        source: format!("synodeb:{src}"),
        category: category(&src),
        severity: sev,
        key: Some(key),
        state,
        title: f.title,
        body: f.message,
        fields,
        actions: Vec::new(),
        code: if event.is_empty() {
            String::new()
        } else {
            format!("synodeb.{event}")
        },
        body_format,
    };
    Ok((ev, f.force))
}

/// What `notify.conf` becomes.
#[derive(Debug, Default)]
pub struct Import {
    pub targets: Vec<TargetRequest>,
    pub min_severity: Sev,
}

/// `KEY=value` lines of a shell file: comments and `export` ignored, one level of single or
/// double quotes removed, no expansion. Trailing comments after a quoted or plain value are
/// cut (`KEY="x"   # note`).
pub fn parse_conf(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let l = line.trim();
        let l = l.strip_prefix("export ").unwrap_or(l);
        if l.starts_with('#') || l.is_empty() {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else {
            continue;
        };
        if k.is_empty()
            || !k
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            continue;
        }
        let v = v.trim_start();
        let value = match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or("").to_string(),
            _ => v.split_whitespace().next().unwrap_or("").to_string(),
        };
        out.insert(k.to_string(), value);
    }
    out
}

/// Targets from `notify.conf`; `read` reads `SMTP_PASSWORD_FILE`.
pub fn import(text: &str, read: &dyn Fn(&str) -> Option<String>) -> Import {
    let c = parse_conf(text);
    let get = |k: &str| c.get(k).map(String::as_str).filter(|v| !v.is_empty());
    let mut targets = Vec::new();
    if let (Some(to), Some(host)) = (get("EMAIL_TO"), get("SMTP_HOST")) {
        let security = match get("SMTP_TLS").unwrap_or("starttls") {
            "tls" | "smtps" => SmtpSecurity::Tls,
            "starttls" => SmtpSecurity::Starttls,
            _ => SmtpSecurity::None,
        };
        let mut secrets = BTreeMap::new();
        if let Some(pw) = get("SMTP_PASSWORD_FILE").and_then(read) {
            let pw = pw.lines().next().unwrap_or("").to_string();
            if !pw.is_empty() {
                secrets.insert("password".to_string(), Some(pw));
            }
        }
        let host_name = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "nas".into());
        targets.push(TargetRequest {
            name: "E-mail (synodeb)".into(),
            enabled: true,
            language: Default::default(),
            config: TargetConfig::Smtp {
                host: host.into(),
                port: get("SMTP_PORT").and_then(|p| p.parse().ok()).unwrap_or(587),
                security,
                username: get("SMTP_USER").map(str::to_string),
                from: get("EMAIL_FROM")
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("root@{host_name}")),
                to: to.split_whitespace().map(str::to_string).collect(),
            },
            secrets,
        });
    }
    if let (Some(token), Some(chat)) = (get("TELEGRAM_TOKEN"), get("TELEGRAM_CHAT_ID")) {
        targets.push(TargetRequest {
            name: "Telegram (synodeb)".into(),
            enabled: true,
            language: Default::default(),
            config: TargetConfig::Telegram {
                chat_id: chat.into(),
                api_url: None,
            },
            secrets: [("bot_token".to_string(), Some(token.to_string()))].into(),
        });
    }
    if let Some(url) = get("NTFY_URL")
        && let Some((server, topic)) = url.trim_end_matches('/').rsplit_once('/')
        && server.contains("://")
        && !server.ends_with(':')
        && !server.ends_with('/')
    {
        let mut secrets = BTreeMap::new();
        if let Some(t) = get("NTFY_TOKEN") {
            secrets.insert("token".to_string(), Some(t.to_string()));
        }
        targets.push(TargetRequest {
            name: "ntfy (synodeb)".into(),
            enabled: true,
            language: Default::default(),
            config: TargetConfig::Ntfy {
                server: server.into(),
                topic: topic.into(),
                username: None,
            },
            secrets,
        });
    }
    Import {
        targets,
        min_severity: severity(get("MIN_PRIORITY").unwrap_or("normal")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_json_and_form() {
        let (e, force) = parse_forward(
            Some("application/json"),
            br#"{"title":"SMART error on sda","message":"pending sectors","priority":"high","source":"smartd"}"#,
        )
        .unwrap();
        assert!(!force);
        assert_eq!(e.source, "synodeb:smartd");
        assert_eq!(e.category, Cat::Storage);
        assert_eq!(e.severity, Sev::Warning);
        assert!(e.key.as_deref().unwrap().starts_with("synodeb:smartd:"));
        let (e2, force) = parse_forward(
            Some("application/x-www-form-urlencoded"),
            b"title=SMART+error+on+sda&message=pending%20sectors&priority=high&source=smartd&force=1",
        )
        .unwrap();
        assert!(force);
        assert_eq!(e2.title, "SMART error on sda");
        assert_eq!(e2.body, "pending sectors");
        // same title and message: same key (dedup)
        assert_eq!(e.key, e2.key);
        let (e, _) = parse_forward(
            Some("application/json"),
            br#"{"title":"UPS on battery","priority":"urgent","source":"synodeb-ups"}"#,
        )
        .unwrap();
        assert_eq!((e.category, e.severity), (Cat::Ups, Sev::Critical));
        let (e, _) = parse_forward(
            Some("application/json"),
            br#"{"title":"x","source":"Evil/../$(rm)"}"#,
        )
        .unwrap();
        assert_eq!(e.source, "synodeb:evil..rm");
        assert_eq!(e.severity, Sev::Notice);
        assert!(parse_forward(Some("application/json"), br#"{"title":" "}"#).is_err());
        assert!(parse_forward(Some("text/plain"), b"title=x").is_err());
        assert_eq!(form_decode("a=%E2%9C%93&b=%zz&c")[0].1, "✓");
        assert_eq!(form_decode("b=%zz")[0].1, "%zz");
    }

    #[test]
    fn synohwd_fan_alarm_is_the_hardware_condition() {
        let (e, _) = parse_forward(
            Some("application/json"),
            br#"{"title":"fan failure","message":"fan stopped","priority":"urgent","source":"synohwd"}"#,
        )
        .unwrap();
        assert_eq!(e.key.as_deref(), Some("hardware:fan_fail"));
        assert_eq!((e.state, e.severity), (EventState::Firing, Sev::Critical));
        assert_eq!(e.category, Cat::Hardware);
        let (e, _) = parse_forward(
            Some("application/json"),
            br#"{"title":"fans ok again","priority":"normal","source":"synohwd"}"#,
        )
        .unwrap();
        assert_eq!(
            (e.key.as_deref(), e.state),
            (Some("hardware:fan_fail"), EventState::Resolved)
        );
        // other synohwd notices (thermal shutdown, buttons) stay oneshot facts
        let (e, _) = parse_forward(
            Some("application/json"),
            br#"{"title":"thermal shutdown","message":"cpu 96","priority":"urgent","source":"synohwd"}"#,
        )
        .unwrap();
        assert_eq!(e.state, EventState::Oneshot);
        assert_eq!(synohwd_condition("smartd", "", "fan failure"), None);
        // current synohwd: source hw, matched by event code whatever the title
        let (e, _) = parse_forward(
            Some("application/x-www-form-urlencoded"),
            b"title=Fan+alarm&priority=urgent&source=hw&event=fan_failure&field.source=hwmon&field.fans=fan1+0+rpm",
        )
        .unwrap();
        assert_eq!(e.key.as_deref(), Some("hardware:fan_fail"));
        assert_eq!(e.code, "synodeb.fan_failure");
        assert_eq!(e.fields["fans"], "fan1 0 rpm");
        let (e, _) = parse_forward(
            Some("application/x-www-form-urlencoded"),
            b"title=fans+ok+again&source=hw&event=fans_ok",
        )
        .unwrap();
        assert_eq!(e.state, EventState::Resolved);
        // title fallback with source hw
        let (e, _) = parse_forward(
            Some("application/json"),
            br#"{"title":"fan failure","source":"hw"}"#,
        )
        .unwrap();
        assert_eq!(e.key.as_deref(), Some("hardware:fan_fail"));
        assert_eq!(e.code, "");
    }

    #[test]
    fn forward_event_fields_and_format() {
        let (e, _) = parse_forward(
            Some("application/x-www-form-urlencoded"),
            b"title=unit+failed%3A+nginx.service&message=x%0Ay&priority=high&source=unit\
              &event=unit_failed&field.unit=nginx.service&format=log",
        )
        .unwrap();
        assert_eq!(e.code, "synodeb.unit_failed");
        assert_eq!(e.body_format, BodyFormat::Log);
        assert_eq!(e.fields["unit"], "nginx.service");
        assert_eq!(e.fields["priority"], "high");
        assert_eq!(e.body, "x\ny");
        let (e, _) = parse_forward(
            Some("application/json"),
            br#"{"title":"RAID md2: Fail","source":"raid","event":"raid_event","format":"log",
                "fields":{"array":"md2","event":"Fail","device":"","n":3}}"#,
        )
        .unwrap();
        assert_eq!(e.code, "synodeb.raid_event");
        assert_eq!(e.body_format, BodyFormat::Log);
        assert_eq!(e.fields["device"], "", "empty values are kept");
        assert!(!e.fields.contains_key("n"), "non-string values are dropped");
        // invalid parts are dropped, the notification stays
        let long = "v".repeat(MAX_FIELD_VALUE + 1);
        let body = format!(
            "title=t&event=Bad-Code&format=html&field.ok=1&field.Bad=2&field.={}&field.long={long}\
             &field.nl=a%0Ab&field.ok=again",
            "x"
        );
        let (e, _) =
            parse_forward(Some("application/x-www-form-urlencoded"), body.as_bytes()).unwrap();
        assert_eq!(e.code, "");
        assert_eq!(e.body_format, BodyFormat::Text);
        assert_eq!(e.fields.len(), 1);
        assert_eq!(e.fields["ok"], "1");
        let (e, _) = parse_forward(
            Some("application/json"),
            br#"{"title":"t","event":7,"fields":"nope","format":["log"]}"#,
        )
        .unwrap();
        assert_eq!((e.code.as_str(), e.body_format), ("", BodyFormat::Text));
        assert!(e.fields.is_empty());
        // at most 16 parameters
        let many: String = (0..20).map(|i| format!("&field.f{i:02}=v")).collect();
        let (e, _) = parse_forward(
            Some("application/x-www-form-urlencoded"),
            format!("title=t{many}").as_bytes(),
        )
        .unwrap();
        assert_eq!(e.fields.len(), MAX_FIELDS);
        // a 32-character code is the longest
        let c32 = "a".repeat(32);
        let (e, _) = parse_forward(
            Some("application/x-www-form-urlencoded"),
            format!("title=t&event={c32}").as_bytes(),
        )
        .unwrap();
        assert_eq!(e.code, format!("synodeb.{c32}"));
        let (e, _) = parse_forward(
            Some("application/x-www-form-urlencoded"),
            format!("title=t&event={c32}a").as_bytes(),
        )
        .unwrap();
        assert_eq!(e.code, "");
    }

    #[test]
    fn notify_conf_import() {
        let conf = r#"
# comment
MIN_PRIORITY=high
EMAIL_TO="admin@example.org ops@example.org"
EMAIL_FROM="nas@example.org"
SMTP_HOST=smtp.example.org
SMTP_PORT=465
SMTP_TLS=tls                     # starttls | tls (port 465) | off
SMTP_USER="nas@example.org"
SMTP_PASSWORD_FILE=/etc/synodeb/smtp.password
TELEGRAM_TOKEN="123456:ABC"
TELEGRAM_CHAT_ID="42"
NTFY_URL="https://ntfy.sh/secret-topic"
NTFY_TOKEN=""
"#;
        let imp = import(conf, &|p| {
            (p == "/etc/synodeb/smtp.password").then(|| "pw\n".into())
        });
        assert_eq!(imp.min_severity, Sev::Warning);
        assert_eq!(imp.targets.len(), 3);
        match &imp.targets[0].config {
            TargetConfig::Smtp {
                port,
                security,
                to,
                username,
                ..
            } => {
                assert_eq!(*port, 465);
                assert_eq!(*security, SmtpSecurity::Tls);
                assert_eq!(to.len(), 2);
                assert_eq!(username.as_deref(), Some("nas@example.org"));
            }
            c => panic!("{c:?}"),
        }
        assert_eq!(imp.targets[0].secrets["password"].as_deref(), Some("pw"));
        assert_eq!(
            imp.targets[1].secrets["bot_token"].as_deref(),
            Some("123456:ABC")
        );
        match &imp.targets[2].config {
            TargetConfig::Ntfy { server, topic, .. } => {
                assert_eq!(
                    (server.as_str(), topic.as_str()),
                    ("https://ntfy.sh", "secret-topic")
                );
            }
            c => panic!("{c:?}"),
        }
        assert!(imp.targets[2].secrets.is_empty());
        assert!(import("# nothing\n", &|_| None).targets.is_empty());
    }
}
