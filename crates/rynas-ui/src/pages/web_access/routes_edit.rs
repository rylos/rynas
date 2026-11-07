//! App addresses on extra ports and addresses for other devices or services (§5.6 of
//! `docs/phase2/docker-apps.md`): the fields both drawers share ("Where it answers": extra
//! ports, the standard port, the NAS's own names; "Forwarding" for `custom` routes: the
//! upstream as one `http(s)://host:port` text, certificate check, `Host`, path prefix) and the
//! "Add an address" drawer.
//!
//! The route stays JSON: [`RouteForm::write`] changes only its own fields and keeps the rest.

use leptos::prelude::*;
use serde_json::{Value, json};

use super::{access_radios, put, s_at, b_at, save_config, wa, WaState};
use crate::form::{In, form_error, spinner, switch, text_field};
use crate::i18n::{t, tr};
use crate::icons;
use crate::ui::{self, Tone};

/// `http(s)://host[:port]` → (scheme, host, port); without a port the scheme's own (80, 443);
/// a trailing `/` is allowed, a path is not.
pub fn parse_upstream(s: &str) -> Option<(&'static str, String, u16)> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();
    let (scheme, rest) = if let Some(r) = lower.strip_prefix("https://") {
        ("https", r)
    } else {
        ("http", lower.strip_prefix("http://")?)
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let default = if scheme == "https" { "443" } else { "80" };
    let (host, port) = if let Some(r) = rest.strip_prefix('[') {
        match r.split_once("]:") {
            Some((h, p)) => (h, p),
            None => (r.strip_suffix(']')?, default),
        }
    } else {
        rest.rsplit_once(':').unwrap_or((rest, default))
    };
    let port: u16 = port.parse().ok().filter(|p| *p > 0)?;
    let ok = !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
    ok.then(|| (scheme, host.to_string(), port))
}

/// The upstream of a route JSON as `scheme://host:port` (empty without a port).
pub fn upstream_text(r: &Value) -> String {
    let port = r.pointer("/upstream/port").and_then(Value::as_u64).unwrap_or(0);
    if port == 0 {
        return String::new();
    }
    let host = s_at(r, "/upstream/host", "127.0.0.1");
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    format!("{}://{host}:{port}", s_at(r, "/upstream/scheme", "http"))
}

/// `8444, 8445` → ports from 1024 to 65535 (commas or spaces; empty is none).
pub fn parse_ports(s: &str) -> Option<Vec<u16>> {
    let mut v = Vec::new();
    for p in s.split([',', ' ', ';']).filter(|p| !p.is_empty()) {
        let n: u16 = p.parse().ok().filter(|n| *n >= 1024)?;
        if !v.contains(&n) {
            v.push(n);
        }
    }
    Some(v)
}

/// A label rynasd accepts: `[a-z0-9-]`, 1-40, not starting or ending with `-`.
pub fn label_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && !s.starts_with('-') && !s.ends_with('-') && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The upstream is this NAS (no certificate to check there).
fn loopback(host: &str) -> bool {
    host == "localhost" || host.starts_with("127.") || host == "::1"
}

/// The fields of the two drawers, as text and switches.
#[derive(Clone, Copy)]
pub struct RouteForm {
    pub label: RwSignal<String>,
    pub upstream: RwSignal<String>,
    pub skip_verify: RwSignal<bool>,
    pub upstream_host: RwSignal<bool>,
    pub prefix: RwSignal<String>,
    pub ports: RwSignal<String>,
    pub main_port: RwSignal<bool>,
    pub nas: RwSignal<bool>,
    /// A submit was tried: empty required fields say so.
    pub tried: RwSignal<bool>,
}

impl RouteForm {
    pub fn of(r: &Value) -> Self {
        let ports = r.get("ports").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(|p| p.to_string()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        RouteForm {
            label: RwSignal::new(s_at(r, "/label", "")),
            upstream: RwSignal::new(upstream_text(r)),
            skip_verify: RwSignal::new(b_at(r, "/upstream/skip_verify", false)),
            upstream_host: RwSignal::new(s_at(r, "/upstream/host_header", "client") == "upstream"),
            prefix: RwSignal::new(s_at(r, "/prefix", "")),
            ports: RwSignal::new(ports),
            main_port: RwSignal::new(b_at(r, "/main_port", true)),
            nas: RwSignal::new(b_at(r, "/names/nas", false)),
            tried: RwSignal::new(false),
        }
    }

    /// What is wrong with the label (translated), next to the field.
    pub fn label_err(&self) -> Signal<Option<String>> {
        let (v, tried) = (self.label, self.tried);
        Signal::derive(move || {
            let l = v.with(|s| s.trim().to_ascii_lowercase());
            if l.is_empty() {
                tried.get().then(|| t("wa.f.label_required").to_string())
            } else {
                (!label_ok(&l)).then(|| t("wa.f.label_bad").to_string())
            }
        })
    }

    /// What is wrong with the forwarding address.
    pub fn upstream_err(&self) -> Signal<Option<String>> {
        let (v, tried) = (self.upstream, self.tried);
        Signal::derive(move || {
            let u = v.get();
            if u.trim().is_empty() {
                tried.get().then(|| t("wa.f.upstream_required").to_string())
            } else {
                parse_upstream(&u).is_none().then(|| t("wa.f.upstream_bad").to_string())
            }
        })
    }

    /// What is wrong with the extra ports.
    pub fn ports_err(&self) -> Signal<Option<String>> {
        let v = self.ports;
        Signal::derive(move || parse_ports(&v.get()).is_none().then(|| t("wa.f.ports_bad").to_string()))
    }

    /// Marks the submit as tried; `false` (and the focus on the first bad field) when a field
    /// is wrong. `custom` checks the forwarding fields too.
    pub fn check(&self, custom: bool) -> bool {
        self.tried.set(true);
        let bad = untrack(|| self.ports_err().get().is_some() || (custom && (self.label_err().get().is_some() || self.upstream_err().get().is_some())));
        if bad {
            ui::focus_invalid();
        }
        !bad
    }

    /// Everything the form holds (dirty check).
    pub fn snapshot(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}",
            self.label.get(),
            self.upstream.get(),
            self.skip_verify.get(),
            self.upstream_host.get(),
            self.prefix.get(),
            self.ports.get(),
            self.main_port.get(),
            self.nas.get()
        )
    }

    /// [`Self::snapshot`] without tracking.
    pub fn snapshot_untracked(&self) -> String {
        untrack(|| self.snapshot())
    }

    fn https_elsewhere(&self) -> bool {
        parse_upstream(&self.upstream.get()).is_some_and(|(s, h, _)| s == "https" && !loopback(&h))
    }

    /// Writes the fields into the route; `custom` adds the forwarding ones. `Err` is the
    /// i18n key of what is wrong.
    pub fn write(&self, r: &mut Value, custom: bool) -> Result<(), &'static str> {
        let ports = parse_ports(&self.ports.get_untracked()).ok_or("wa.f.ports_bad")?;
        put(r, "ports", json!(ports));
        let main = self.main_port.get_untracked() || ports.is_empty();
        put(r, "main_port", Value::Bool(main));
        let mut names = r.get("names").cloned().unwrap_or(Value::Null);
        put(&mut names, "nas", Value::Bool(self.nas.get_untracked() && !ports.is_empty()));
        put(r, "names", names);
        if custom {
            let label = self.label.get_untracked().trim().to_ascii_lowercase();
            if !label_ok(&label) {
                return Err("wa.f.label_bad");
            }
            put(r, "label", Value::String(label));
            let (scheme, host, port) = parse_upstream(&self.upstream.get_untracked()).ok_or("wa.f.upstream_bad")?;
            let mut up = r.get("upstream").cloned().unwrap_or(Value::Null);
            put(&mut up, "scheme", Value::String(scheme.into()));
            put(&mut up, "skip_verify", Value::Bool(scheme == "https" && !loopback(&host) && self.skip_verify.get_untracked()));
            put(&mut up, "host", Value::String(host));
            put(&mut up, "port", json!(port));
            put(&mut up, "host_header", Value::String(if self.upstream_host.get_untracked() { "upstream" } else { "client" }.into()));
            put(r, "upstream", up);
            let prefix = self.prefix.get_untracked().trim().to_string();
            put(r, "prefix", if prefix.is_empty() { Value::Null } else { Value::String(prefix) });
        }
        Ok(())
    }
}

/// "Where it answers": extra ports, the standard port, the NAS's names.
pub fn where_fields(f: RouteForm) -> AnyView {
    let has_ports = Memo::new(move |_| parse_ports(&f.ports.get()).is_some_and(|p| !p.is_empty()));
    view! {
        <h3 class="mini-title">{tr("wa.sec.where")}</h3>
        {text_field("wa.f.ports", f.ports.into(), Callback::new(move |v| f.ports.set(v)), f.ports_err(), In { placeholder: "8444", help: Some("wa.f.ports_help"), mono: true, maxlength: 64, ..In::default() })}
        <Show when=move || has_ports.get()>
            {switch("wa.f.main_port", Some("wa.f.main_port_help"), f.main_port.into(), Callback::new(move |v| f.main_port.set(v)))}
            {switch("wa.f.nas", Some("wa.f.nas_help"), f.nas.into(), Callback::new(move |v| f.nas.set(v)))}
        </Show>
    }
    .into_any()
}

/// "Forwarding" of a `custom` route.
pub fn forward_fields(f: RouteForm, new: bool) -> AnyView {
    let https = Memo::new(move |_| f.https_elsewhere());
    view! {
        <h3 class="mini-title">{tr("wa.sec.forward")}</h3>
        {new.then(|| text_field("wa.f.label", f.label.into(), Callback::new(move |v: String| f.label.set(v.to_ascii_lowercase())), f.label_err(), In { placeholder: "router", help: Some("wa.f.label_help"), mono: true, maxlength: 40, ..In::default() }))}
        {text_field("wa.f.upstream", f.upstream.into(), Callback::new(move |v| f.upstream.set(v)), f.upstream_err(), In { placeholder: "http://192.168.1.20:8080", help: Some("wa.f.upstream_help"), mono: true, maxlength: 300, ..In::default() })}
        <Show when=move || https.get()>
            {switch("wa.f.skip_verify", Some("wa.f.skip_verify_help"), f.skip_verify.into(), Callback::new(move |v| f.skip_verify.set(v)))}
            {move || f.skip_verify.get().then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, ui::text(t("wa.f.skip_warn").to_string())))}
        </Show>
        {switch("wa.f.upstream_host", Some("wa.f.upstream_host_help"), f.upstream_host.into(), Callback::new(move |v| f.upstream_host.set(v)))}
        {text_field("wa.f.prefix", f.prefix.into(), Callback::new(move |v| f.prefix.set(v)), Signal::stored(None), In { placeholder: "/grafana", help: Some("wa.f.prefix_help"), mono: true, maxlength: 200, ..In::default() })}
        <p class="small muted">{tr("wa.f.websocket")}</p>
    }
    .into_any()
}

/// A route id `custom-<label>` not in `taken`.
pub fn new_id(label: &str, taken: &[String]) -> String {
    let base = format!("custom-{label}");
    if !taken.contains(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|i| !taken.contains(i)).unwrap_or(base)
}

const ADD_FORM: &str = "wa-add-form";

/// "Add an address": a `custom` route to another device or service.
pub fn add_drawer(open: RwSignal<bool>) -> AnyView {
    let w = wa();
    let close = Callback::new(move |_| open.set(false));
    let f = RouteForm::of(&Value::Null);
    f.ports.set(String::new());
    let access = RwSignal::new("lan".to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if !f.check(true) {
            return;
        }
        let Some(mut cfg) = w.state.data.with_untracked(|s| s.as_ref().map(WaState::current)) else { return };
        let taken: Vec<String> = cfg.get("routes").and_then(Value::as_array).map(|a| a.iter().map(|r| s_at(r, "/id", "")).collect()).unwrap_or_default();
        let mut r = json!({"kind": "custom", "access": access.get_untracked(), "websocket": true, "names": {"local": true, "domain": true}});
        if let Err(k) = f.write(&mut r, true) {
            error.set(Some(t(k).to_string()));
            return;
        }
        let id = new_id(&s_at(&r, "/label", ""), &taken);
        put(&mut r, "id", Value::String(id));
        if let Some(a) = cfg.get_mut("routes").and_then(Value::as_array_mut) {
            a.push(r);
        } else {
            put(&mut cfg, "routes", json!([r]));
        }
        busy.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            let ok = save_config(cfg, error).await;
            let _ = busy.try_set(false);
            w.state.reload();
            if ok {
                let _ = open.try_set(false);
            }
        });
    };
    let body = view! {
        <form id=ADD_FORM class="form" on:submit=submit>
            <p class="small muted">{tr("wa.add.intro")}</p>
            {forward_fields(f, true)}
            {where_fields(f)}
            <h3 class="mini-title">{tr("wa.col.access")}</h3>
            {access_radios("wa-add-access", access, false)}
            {form_error(error)}
        </form>
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="submit" form=ADD_FORM class="btn primary" disabled=move || busy.get()>{spinner(busy)}{tr("wa.add.button")}</button>
    }
    .into_any();
    ui::drawer(Signal::derive(|| t("wa.add.title").to_string()), close, body, foot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstreams_and_ports() {
        assert_eq!(parse_upstream("http://192.168.1.20:8080"), Some(("http", "192.168.1.20".into(), 8080)));
        assert_eq!(parse_upstream(" HTTPS://Router-1:8443/ "), Some(("https", "router-1".into(), 8443)));
        assert_eq!(parse_upstream("https://[fd00::1]:443"), Some(("https", "fd00::1".into(), 443)));
        assert_eq!(parse_upstream("http://host"), Some(("http", "host".into(), 80)));
        assert_eq!(parse_upstream("https://[fd00::1]/"), Some(("https", "fd00::1".into(), 443)));
        for bad in ["192.168.1.20:80", "http://host:0", "http://[fd00::1", "http://host:x", "http://host:80/admin", "ftp://h:21", "http://:80", "http://a b:80"] {
            assert_eq!(parse_upstream(bad), None, "{bad}");
        }
        assert_eq!(upstream_text(&json!({"upstream": {"scheme": "https", "host": "::1", "port": 9443}})), "https://[::1]:9443");
        assert_eq!(upstream_text(&Value::Null), "");
        assert_eq!(parse_ports("8444, 8445 8444"), Some(vec![8444, 8445]));
        assert_eq!(parse_ports(""), Some(vec![]));
        assert_eq!(parse_ports("80"), None);
        assert_eq!(parse_ports("70000"), None);
        assert!(label_ok("router-2") && !label_ok("-x") && !label_ok("Router") && !label_ok(""));
        assert_eq!(new_id("router", &["custom-router".into()]), "custom-router-2");
    }
}
