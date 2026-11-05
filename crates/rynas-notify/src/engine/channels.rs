//! Delivery to one target: SMTP (lettre), ntfy, Telegram, Gotify, webhook.
//!
//! Every send has a timeout; errors are one line without secrets (URLs are dropped from
//! HTTP errors, secret values redacted), suitable for the target status and the log.

use std::collections::BTreeMap;
use std::time::Duration;

use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use rynas_api_types::notify::{
    NotifyLanguage, NotifySeverity, SmtpSecurity, TargetConfig, WebhookMethod,
};

use super::secrets::redact;
use super::template::{Message, Templates};

/// Longest wait for one delivery.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(20);
/// Most bytes of a server answer quoted in an error.
const ERR_BODY: usize = 200;

/// A target as the sender needs it.
pub struct Dest<'a> {
    pub config: &'a TargetConfig,
    pub language: NotifyLanguage,
    pub secrets: &'a BTreeMap<String, String>,
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(SEND_TIMEOUT)
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(3))
        .user_agent(concat!("rynas/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("HTTP client with native TLS")
}

pub async fn send(
    http: &reqwest::Client,
    tpl: &Templates,
    d: &Dest<'_>,
    msg: &Message,
) -> Result<(), String> {
    let r = match d.config {
        TargetConfig::Smtp { .. } => smtp(d, msg).await,
        TargetConfig::Ntfy { .. } => ntfy(http, d, msg).await,
        TargetConfig::Telegram { .. } => telegram(http, d, msg).await,
        TargetConfig::Gotify { .. } => gotify(http, d, msg).await,
        TargetConfig::Webhook { .. } => webhook(http, tpl, d, msg).await,
        TargetConfig::Ui {} => Ok(()),
    };
    r.map_err(|e| {
        let mut e = redact(&e, d.secrets);
        e.retain(|c| !c.is_control());
        e.chars().take(300).collect()
    })
}

fn secret<'a>(d: &'a Dest<'_>, name: &str) -> Option<&'a str> {
    d.secrets
        .get(name)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
}

async fn smtp(d: &Dest<'_>, msg: &Message) -> Result<(), String> {
    let TargetConfig::Smtp {
        host,
        port,
        security,
        username,
        from,
        to,
    } = d.config
    else {
        unreachable!()
    };
    let from: Mailbox = from.parse().map_err(|e| format!("from address: {e}"))?;
    let mut b = lettre::Message::builder()
        .from(from)
        .subject(msg.subject.clone())
        .header(ContentType::TEXT_PLAIN);
    for t in to {
        b = b.to(t.parse().map_err(|e| format!("address {t}: {e}"))?);
    }
    let email = b.body(msg.text.clone()).map_err(|e| e.to_string())?;
    let builder = match security {
        SmtpSecurity::Starttls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| e.to_string())?
        }
        SmtpSecurity::Tls => {
            AsyncSmtpTransport::<Tokio1Executor>::relay(host).map_err(|e| e.to_string())?
        }
        SmtpSecurity::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
    };
    let mut builder = builder.port(*port).timeout(Some(SEND_TIMEOUT));
    if let Some(u) = username.as_deref().filter(|u| !u.is_empty()) {
        let pw = secret(d, "password").unwrap_or_default();
        builder = builder.credentials(Credentials::new(u.to_string(), pw.to_string()));
    }
    let transport = builder.build();
    match tokio::time::timeout(SEND_TIMEOUT * 2, transport.send(email)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("SMTP: {e}")),
        Err(_) => Err("SMTP: timed out".into()),
    }
}

async fn check(r: Result<reqwest::Response, reqwest::Error>) -> Result<(), String> {
    let r = r.map_err(|e| e.without_url().to_string())?;
    let status = r.status();
    if status.is_success() {
        return Ok(());
    }
    let body = r.text().await.unwrap_or_default();
    let body: String = body.chars().take(ERR_BODY).collect();
    Err(format!("HTTP {}: {}", status.as_u16(), body.trim()))
}

fn url(base: &str, path: &str) -> Result<reqwest::Url, String> {
    let s = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    reqwest::Url::parse(&s).map_err(|e| format!("URL: {e}"))
}

fn ntfy_priority(s: NotifySeverity) -> &'static str {
    match s {
        NotifySeverity::Info => "2",
        NotifySeverity::Notice => "3",
        NotifySeverity::Warning | NotifySeverity::Error => "4",
        NotifySeverity::Critical => "5",
    }
}

async fn ntfy(http: &reqwest::Client, d: &Dest<'_>, msg: &Message) -> Result<(), String> {
    let TargetConfig::Ntfy {
        server,
        topic,
        username,
    } = d.config
    else {
        unreachable!()
    };
    let mut u = url(server, topic)?;
    // title and priority as URL parameters: HTTP headers do not carry UTF-8 (as synodeb)
    u.query_pairs_mut()
        .append_pair("title", &msg.subject)
        .append_pair("priority", ntfy_priority(msg.severity))
        .append_pair("tags", &format!("rynas,{}", msg.severity.as_str()));
    let mut req = http.post(u).body(msg.text.clone());
    if let Some(t) = secret(d, "token") {
        req = req.bearer_auth(t);
    } else if let Some(user) = username.as_deref().filter(|u| !u.is_empty()) {
        req = req.basic_auth(user, secret(d, "password"));
    }
    check(req.send().await).await
}

fn json_body(req: reqwest::RequestBuilder, v: &serde_json::Value) -> reqwest::RequestBuilder {
    req.header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(v.to_string())
}

async fn telegram(http: &reqwest::Client, d: &Dest<'_>, msg: &Message) -> Result<(), String> {
    let TargetConfig::Telegram { chat_id, api_url } = d.config else {
        unreachable!()
    };
    let token = secret(d, "bot_token").ok_or("bot_token is not set")?;
    let base = api_url.as_deref().unwrap_or("https://api.telegram.org");
    let u = url(base, &format!("bot{token}/sendMessage"))?;
    let mut text = format!("{}\n\n{}", msg.subject, msg.text);
    if text.chars().count() > 4000 {
        text = text.chars().take(4000).collect();
    }
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": text,
        "disable_notification": msg.severity == NotifySeverity::Info,
    });
    check(json_body(http.post(u), &body).send().await).await
}

fn gotify_priority(s: NotifySeverity) -> u8 {
    match s {
        NotifySeverity::Info => 2,
        NotifySeverity::Notice => 4,
        NotifySeverity::Warning => 6,
        NotifySeverity::Error => 8,
        NotifySeverity::Critical => 10,
    }
}

async fn gotify(http: &reqwest::Client, d: &Dest<'_>, msg: &Message) -> Result<(), String> {
    let TargetConfig::Gotify { server } = d.config else {
        unreachable!()
    };
    let token = secret(d, "token").ok_or("token is not set")?;
    let body = serde_json::json!({
        "title": msg.subject,
        "message": msg.text,
        "priority": gotify_priority(msg.severity),
    });
    let req = http
        .post(url(server, "message")?)
        .header("X-Gotify-Key", token);
    check(json_body(req, &body).send().await).await
}

/// The JSON document a webhook gets without a body template.
pub fn default_webhook_body(tpl: &Templates, msg: &Message) -> serde_json::Value {
    serde_json::json!({
        "hostname": tpl.hostname(),
        "subject": msg.subject,
        "message": msg.text,
        "severity": msg.severity.as_str(),
        "events": msg.events,
    })
}

async fn webhook(
    http: &reqwest::Client,
    tpl: &Templates,
    d: &Dest<'_>,
    msg: &Message,
) -> Result<(), String> {
    let TargetConfig::Webhook {
        url: u,
        method,
        headers,
        body,
    } = d.config
    else {
        unreachable!()
    };
    let render = |s: &str| tpl.render_user(s, msg, d.language, d.secrets);
    let target = reqwest::Url::parse(render(u)?.trim()).map_err(|e| format!("URL: {e}"))?;
    let mut req = match method {
        WebhookMethod::Get => http.get(target),
        WebhookMethod::Post => http.post(target),
        WebhookMethod::Put => http.put(target),
    };
    for h in headers {
        let v = render(&h.value)?;
        let v = reqwest::header::HeaderValue::from_str(v.trim())
            .map_err(|_| format!("header {}: invalid value", h.name))?;
        req = req.header(h.name.as_str(), v);
    }
    if *method != WebhookMethod::Get {
        req = match body {
            Some(b) => req.body(render(b)?),
            None => json_body(req, &default_webhook_body(tpl, msg)),
        };
    }
    check(req.send().await).await
}
