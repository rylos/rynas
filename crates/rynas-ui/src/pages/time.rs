//! Sistema > Data e ora: time zone, automatic sync, manual time (`/api/v1/system/time`, D-TIME).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStatus;
use rynas_api_types::clock::{ClockSettings, ClockState, MAX_SERVERS, SetClock, TimeDaemon, ZoneInfo};

use crate::form::{Draft, area_field, form_error, fid, spinner, stale_notice, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons;
use crate::toast::toast;
use crate::ui::{self, PageHeader, Remote, Tone};

const URL: &str = "/api/v1/system/time";

/// What the form edits; the servers as typed (one per line).
#[derive(Clone, PartialEq)]
struct Form {
    tz: String,
    sync: bool,
    servers: String,
}

fn form_of(s: &ClockState) -> Form {
    Form { tz: s.timezone.clone(), sync: s.sync_enabled, servers: s.servers.join("\n") }
}

fn servers(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n']).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

fn settings(f: &Form, local_rtc: bool) -> ClockSettings {
    ClockSettings { timezone: f.tz.clone(), sync_enabled: f.sync, servers: servers(&f.servers), local_rtc }
}

/// "UTC+01:00".
fn utc(off: i32) -> String {
    let a = off.unsigned_abs();
    format!("UTC{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, a / 3600, a % 3600 / 60)
}

/// A wall time of the NAS's zone (ms already shifted by its offset) as a `js_sys::Date` in UTC.
fn wall(ms: f64) -> js_sys::Date {
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms))
}

fn wall_text(ms: f64) -> String {
    let opts = js_sys::Object::new();
    for (k, v) in [("timeZone", "UTC"), ("dateStyle", "full"), ("timeStyle", "medium")] {
        let _ = js_sys::Reflect::set(&opts, &k.into(), &v.into());
    }
    wall(ms).to_locale_string(i18n::lang().code(), &opts).as_string().unwrap_or_default()
}

/// `YYYY-MM-DDTHH:MM:SS` for a `datetime-local` input.
fn wall_input(ms: f64) -> String {
    let d = wall(ms);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        d.get_utc_full_year(),
        d.get_utc_month() + 1,
        d.get_utc_date(),
        d.get_utc_hours(),
        d.get_utc_minutes(),
        d.get_utc_seconds()
    )
}

/// Runs a task, toasts `ok`, reloads the state.
fn task(url: &'static str, body: Option<String>, ok: &'static str, busy: RwSignal<bool>, error: RwSignal<Option<String>>, st: Remote<ClockState>) {
    busy.set(true);
    error.set(None);
    spawn_local(async move {
        let r = crate::tasks::run_task(url, body).await;
        let _ = busy.try_set(false);
        match r {
            Ok(task) if task.status == TaskStatus::Ok => toast(Tone::Ok, t(ok).to_string()),
            Ok(task) => {
                let _ = error.try_set(Some(crate::tasks::task_error(&task)));
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
        st.reload();
    });
}

/// Zone picker: a filter and the zones grouped by region with their offset now.
fn zone_field(d: RwSignal<Form>, zones: Remote<Vec<ZoneInfo>>) -> AnyView {
    let q = RwSignal::new(String::new());
    let id = fid();
    let groups = move || {
        let needle = q.get().trim().to_lowercase().replace(' ', "_");
        let cur = d.with(|f| f.tz.clone());
        let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
        zones.data.with(|z| {
            let fallback = [ZoneInfo { name: cur.clone(), offset_s: 0 }];
            let list = z.as_deref().unwrap_or(&fallback);
            for z in list.iter().filter(|z| z.name == cur || z.name.to_lowercase().contains(&needle)) {
                let (region, city) = z.name.split_once('/').unwrap_or(("", &z.name));
                let label = format!("{} · {}", city.replace('_', " "), utc(z.offset_s));
                match out.last_mut() {
                    Some((r, v)) if r == region => v.push((z.name.clone(), label)),
                    _ => out.push((region.to_string(), vec![(z.name.clone(), label)])),
                }
            }
        });
        out.into_iter()
            .map(|(region, v)| {
                let opts = v
                    .into_iter()
                    .map(|(name, label)| {
                        let sel = name == cur;
                        view! { <option value=name selected=sel>{label}</option> }
                    })
                    .collect_view();
                if region.is_empty() { opts.into_any() } else { view! { <optgroup label=region>{opts}</optgroup> }.into_any() }
            })
            .collect_view()
    };
    view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr("ps.time_zone")}</label>
            <input class="input" type="search" placeholder=tr("time.zone_search") aria-label=tr("time.zone_search") prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) />
            <select id=id class="input" on:change=move |ev| {
                let v = event_target_value(&ev);
                d.update(|f| f.tz = v);
            }>{groups}</select>
            <span class="field-help">{tr("time.zone_help")}</span>
        </div>
    }
    .into_any()
}

fn form_card(init: ClockState, st: Remote<ClockState>, zones: Remote<Vec<ZoneInfo>>) -> AnyView {
    let draft = Draft::new(form_of(&init), move || st.data.with(|s| s.as_ref().map(form_of)));
    let d = draft.d;
    let dirty = Signal::derive(move || draft.base.with(|b| d.with(|f| f.tz != b.tz || f.sync != b.sync || servers(&f.servers) != servers(&b.servers))));
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let sync = Signal::derive(move || d.with(|f| f.sync));
    let text = Signal::derive(move || d.with(|f| f.servers.clone()));
    let valid = Signal::derive(move || d.with(|f| servers(&f.servers).len() <= MAX_SERVERS));
    let defaults = move || {
        st.data.with(|s| {
            let s = s.as_ref()?;
            let mut v = Vec::new();
            if !s.default_servers.is_empty() {
                v.push(tf("time.defaults", &[("list", &s.default_servers.join(", "))]));
            }
            if !s.dhcp_servers.is_empty() {
                v.push(tf("time.dhcp", &[("list", &s.dhcp_servers.join(", "))]));
            }
            if s.daemon == TimeDaemon::Chrony {
                v.push(t("time.also_own").to_string());
            }
            (!v.is_empty()).then(|| view! { <p class="muted small">{v.join(" ")}</p> })
        })
    };
    let needs_install = Signal::derive(move || sync.get() && st.data.with(|s| s.as_ref().is_some_and(|s| s.daemon == TimeDaemon::None)));
    let save = move |_| {
        let rtc = st.data.with_untracked(|s| s.as_ref().is_some_and(|s| s.local_rtc));
        let body = serde_json::to_string(&settings(&d.get_untracked(), rtc)).unwrap_or_default();
        task(URL, Some(body), "time.saved", busy, error, st);
    };
    view! {
        <section class="card widget w-6" style="--i:0">
            <h2 class="section-title">{tr("time.settings")}</h2>
            {stale_notice(draft.stale, Callback::new(move |_| draft.reset()))}
            {zone_field(d, zones)}
            {switch("time.sync", Some("time.sync_help"), sync, Callback::new(move |v| d.update(|f| f.sync = v)))}
            <Show when=move || sync.get()>
                {area_field("time.servers", text, Callback::new(move |v| d.update(|f| f.servers = v)), Some("time.servers_help"), "ntp.example.lan")}
                {defaults}
            </Show>
            <Show when=move || needs_install.get()>
                <p class="muted small">{tr("time.will_install")}</p>
            </Show>
            {form_error(error)}
            <div class="card-actions">
                <button type="button" class="btn ghost" disabled=move || busy.get() || !dirty.get() on:click=move |_| draft.reset()>{tr("cancel")}</button>
                <button type="button" class="btn primary" disabled=move || busy.get() || !dirty.get() || !valid.get() on:click=save>
                    {spinner(busy)}{tr("time.apply")}
                </button>
            </div>
        </section>
    }
    .into_any()
}

fn kv(label: &'static str, value: String) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd class="tnum">{value}</dd></div> }.into_any()
}

/// The clock now (ticking) and what the time service says.
fn status_card(st: Remote<ClockState>) -> AnyView {
    let tick = crate::live::use_live().uptime_offset;
    let start = tick.get_untracked();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let clock = move || {
        let secs = tick.get().saturating_sub(start);
        st.data.with(|s| s.as_ref().map(|s| wall_text(s.now_ms as f64 + (secs as f64 + s.utc_offset_s as f64) * 1000.0)))
    };
    let rows = move || {
        st.data.with(|s| {
            let s = s.as_ref()?;
            let (tone, key) = match (s.sync_enabled, s.synchronized) {
                (false, _) => (Tone::Unknown, "time.st_off"),
                (true, true) => (Tone::Ok, "time.st_synced"),
                (true, false) => (Tone::Warn, "time.st_unsynced"),
            };
            let mut v = vec![
                kv("ps.time_zone", format!("{} ({})", s.timezone, utc(s.utc_offset_s))),
                view! { <div class="kv"><dt>{tr("time.state")}</dt><dd>{ui::status(tone, t(key).to_string())}</dd></div> }.into_any(),
            ];
            if let Some(x) = s.source.as_ref() {
                let server = match &x.address {
                    Some(a) if *a != x.server => format!("{} ({a})", x.server),
                    _ => x.server.clone(),
                };
                v.push(kv("time.source", server));
                if let Some(o) = x.offset_us {
                    v.push(kv("time.offset", format!("{} ms", i18n::num(o as f64 / 1000.0, 1))));
                }
                if let Some(at) = x.last_sync_ms {
                    v.push(kv("time.last", i18n::datetime(at)));
                }
                if let Some(p) = x.poll_s {
                    v.push(kv("time.poll", i18n::duration(u64::from(p))));
                }
            }
            Some(v)
        })
    };
    let can_sync = Signal::derive(move || st.data.with(|s| s.as_ref().is_some_and(|s| s.sync_enabled)));
    view! {
        <section class="card widget w-6" style="--i:1">
            <h2 class="section-title">{tr("time.status")}</h2>
            <p class="time-now tnum">{clock}</p>
            <dl class="kvs">{rows}</dl>
            {form_error(error)}
            <Show when=move || can_sync.get()>
                <div class="card-actions">
                    <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| task("/api/v1/system/time/sync-now", None, "time.synced", busy, error, st)>
                        {spinner(busy)}{tr("time.sync_now")}
                    </button>
                </div>
            </Show>
        </section>
    }
    .into_any()
}

/// Date and time by hand, only while automatic sync is off.
fn manual_card(st: Remote<ClockState>) -> AnyView {
    let init = st.data.with_untracked(|s| s.as_ref().map(|s| wall_input(s.now_ms as f64 + s.utc_offset_s as f64 * 1000.0))).unwrap_or_default();
    let value = RwSignal::new(init);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let id = fid();
    let send = move |req: SetClock| task("/api/v1/system/time/set", serde_json::to_string(&req).ok(), "time.set_done", busy, error, st);
    view! {
        <section class="card widget w-6" style="--i:2">
            <h2 class="section-title">{tr("time.manual")}</h2>
            <div class="field">
                <label class="field-label" for=id.clone()>{tr("time.manual_label")}</label>
                <input id=id class="input" type="datetime-local" step="1" prop:value=move || value.get() on:input=move |ev| value.set(event_target_value(&ev)) />
            </div>
            {form_error(error)}
            <div class="card-actions">
                <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| send(SetClock { epoch_ms: Some(js_sys::Date::now() as u64), local: None })>
                    {tr("time.browser")}
                </button>
                <button type="button" class="btn primary" disabled=move || busy.get() || value.with(String::is_empty) on:click=move |_| {
                    let v = value.get_untracked().replace('T', " ");
                    send(SetClock { epoch_ms: None, local: Some(v) });
                }>{spinner(busy)}{tr("time.set")}</button>
            </div>
        </section>
    }
    .into_any()
}

/// A hardware clock in local time, with the move to UTC.
fn rtc_note(st: Remote<ClockState>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let fix = move |_| {
        let Some(s) = st.data.get_untracked() else { return };
        let body = serde_json::to_string(&settings(&form_of(&s), false)).unwrap_or_default();
        task(URL, Some(body), "time.saved", busy, error, st);
    };
    view! {
        <Show when=move || st.data.with(|s| s.as_ref().is_some_and(|s| s.local_rtc))>
            {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! {
                {tr("time.local_rtc")}" "
                <button type="button" class="link-btn" disabled=move || busy.get() on:click=fix>{tr("time.rtc_utc")}</button>
            }.into_any())}
            {form_error(error)}
        </Show>
    }
    .into_any()
}

#[component]
pub fn TimeSettings() -> impl IntoView {
    let st = Remote::<ClockState>::new(URL);
    let zones = Remote::<Vec<ZoneInfo>>::new("/api/v1/system/time/zones");
    let manual = Memo::new(move |_| st.data.with(|s| s.as_ref().is_some_and(|s| !s.sync_enabled)));
    let body = move || {
        let Some(s) = st.data.get_untracked() else {
            return ui::loading(st.err.into(), Callback::new(move |_| st.reload()), 6);
        };
        view! {
            {rtc_note(st)}
            <div class="grid">
                {form_card(s, st, zones)}
                {status_card(st)}
                {move || manual.get().then(|| manual_card(st))}
            </div>
        }
        .into_any()
    };
    // built once, when the first answer comes: later answers update it through its draft
    let first = Memo::new(move |_| st.data.with(Option::is_some));
    view! {
        <div class="page">
            <PageHeader icon=icons::CALENDAR_CLOCK title="time.title" subtitle="time.subtitle" />
            {move || { first.get(); body() }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_and_servers() {
        assert_eq!(utc(3600), "UTC+01:00");
        assert_eq!(utc(-16200), "UTC-04:30");
        assert_eq!(utc(0), "UTC+00:00");
        assert_eq!(servers(" a.example\n\nb.example, c.example "), ["a.example", "b.example", "c.example"]);
    }
}
