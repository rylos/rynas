//! Security: the signed-in account, its session and two-factor authentication (TOTP):
//! enroll (QR + secret) → confirm with a code → recovery codes shown once; disable with a code.

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{TotpCode, TotpConfirmed, TotpEnrollment};
use wasm_bindgen::{JsCast, JsValue};

use crate::api;
use crate::auth::{Phase, auth, countdown, mmss, role_key};
use crate::form::{form_error, spinner};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::qr::QrSvg;
use crate::ui::{self, Field, PageHeader, Status, Tone, WidgetHead, kv_row};

/// Copy to the clipboard: async Clipboard API when present (secure contexts), else the old
/// textarea + execCommand path (plain-HTTP LAN addresses).
pub fn copy_text(text: &str) -> bool {
    let nav = window().navigator();
    let has_api = js_sys::Reflect::get(&nav, &JsValue::from_str("clipboard")).is_ok_and(|v| !v.is_undefined());
    if has_api && window().is_secure_context() {
        let _ = nav.clipboard().write_text(text);
        return true;
    }
    let doc = document();
    let Ok(ta) = doc.create_element("textarea") else { return false };
    let ta: web_sys::HtmlTextAreaElement = ta.unchecked_into();
    ta.set_value(text);
    let _ = ta.set_attribute("style", "position:fixed;opacity:0;top:0;left:0");
    let Some(body) = doc.body() else { return false };
    let _ = body.append_child(&ta);
    ta.select();
    let ok = doc.unchecked_ref::<web_sys::HtmlDocument>().exec_command("copy").unwrap_or(false);
    ta.remove();
    ok
}


/// "JBSW Y3DP EHPK 3PXP" — easier to type by hand.
fn group4(s: &str) -> String {
    s.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

#[derive(Clone, PartialEq)]
enum Flow {
    Idle,
    Enrolling(TotpEnrollment),
    Codes(Vec<String>),
    Disabling,
}

/// Copy button with a short "Copied" feedback (one shared instantiation).
pub fn copy_button(label: &'static str, text: String) -> impl IntoView {
    let copied = RwSignal::new(false);
    view! {
        <button class="btn ghost sm" type="button" on:click=move |_| {
            if copy_text(&text) {
                copied.set(true);
                set_timeout(move || copied.set(false), std::time::Duration::from_millis(1800));
            }
        }>
            {move || view! { <Icon icon=if copied.get() { icons::CHECK } else { icons::COPY } size=14 /> }}
            {move || if copied.get() { t("copied") } else { t(label) }}
        </button>
    }
}

#[component]
pub fn Security() -> impl IntoView {
    let a = auth();
    let live = crate::live::use_live();
    let demo = move || a.phase.get() == Phase::Demo;
    let session = a.session;
    let totp_on = move || session.with(|s| s.as_ref().is_some_and(|s| s.totp_enabled));
    let flow = RwSignal::new(Flow::Idle);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let code = RwSignal::new(String::new());
    let saved = RwSignal::new(false);
    let (wait, start_wait) = countdown();
    let code_ref = NodeRef::<html::Input>::new();

    let fail = move |e: api::ApiErr| {
        if let Some(s) = e.retry_after() {
            start_wait(s);
        }
        error.set(Some(e.text()));
        code.set(String::new());
        focus_later(code_ref);
    };

    let enroll = move || {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<(), TotpEnrollment>("/api/v1/auth/totp/enroll", None).await;
            busy.set(false);
            match r {
                Ok(e) => {
                    code.set(String::new());
                    flow.set(Flow::Enrolling(e));
                    focus_later(code_ref);
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };

    let submit_code = move || {
        if busy.get_untracked() || wait.get_untracked() > 0 {
            return;
        }
        let c = code.get_untracked().trim().to_string();
        if c.is_empty() {
            return;
        }
        let disabling = flow.with_untracked(|f| matches!(f, Flow::Disabling));
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let body = TotpCode { code: c };
            if disabling {
                let r = api::post_empty("/api/v1/auth/totp/disable", Some(&body)).await;
                busy.set(false);
                match r {
                    Ok(()) => {
                        flow.set(Flow::Idle);
                        session.update(|s| {
                            if let Some(s) = s {
                                s.totp_enabled = false
                            }
                        });
                        a.refresh();
                    }
                    Err(e) => fail(e),
                }
            } else {
                let r = api::post::<_, TotpConfirmed>("/api/v1/auth/totp/confirm", Some(&body)).await;
                busy.set(false);
                match r {
                    Ok(c) => {
                        saved.set(false);
                        flow.set(Flow::Codes(c.recovery_codes));
                        session.update(|s| {
                            if let Some(s) = s {
                                s.totp_enabled = true
                            }
                        });
                        a.refresh();
                    }
                    Err(e) => fail(e),
                }
            }
        });
    };

    let on_code = move |ev: leptos::ev::Event| {
        let raw = event_target_value(&ev);
        let disabling = flow.with_untracked(|f| matches!(f, Flow::Disabling));
        if disabling && raw.chars().any(|c| c.is_ascii_alphabetic()) {
            // a recovery code
            code.set(raw.to_ascii_uppercase());
            return;
        }
        let digits: String = raw.chars().filter(char::is_ascii_digit).take(6).collect();
        if !disabling
            && let Some(el) = code_ref.get_untracked()
            && el.value() != digits
        {
            el.set_value(&digits);
        }
        let full = digits.len() == 6;
        code.set(digits);
        if full && !disabling {
            submit_code();
        }
    };

    let cancel = move |_| {
        flow.set(Flow::Idle);
        error.set(None);
        code.set(String::new());
    };

    let code_field = move |label: &'static str, disabling: bool| {
        view! {
            <form class="code-form" on:submit=move |ev| { ev.prevent_default(); submit_code(); }>
                <Field label=label>
                    <input
                        node_ref=code_ref
                        class="input mono code-input"
                        inputmode=if disabling { "text" } else { "numeric" }
                        autocomplete="one-time-code"
                        autocapitalize="characters"
                        spellcheck="false"
                        maxlength="32"
                        placeholder="000000"
                        prop:value=move || code.get()
                        on:input=on_code
                    />
                </Field>
                <div class="sec-actions">
                    <button class="btn primary" type="submit" disabled=move || { busy.get() || wait.get() > 0 || code.with(String::is_empty) }>
                        {spinner(busy)}
                        {move || if wait.get() > 0 {
                            tf("login.wait", &[("t", &mmss(wait.get()))])
                        } else if disabling {
                            t("sec.disable").to_string()
                        } else {
                            t("sec.activate").to_string()
                        }}
                    </button>
                    <button class="btn ghost" type="button" on:click=cancel>{tr("cancel")}</button>
                </div>
            </form>
        }
    };

    let body = move || {
        if demo() {
            return view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("sec.demo")}</p> }.into_any();
        }
        match flow.get() {
            Flow::Idle => {
                let on = totp_on();
                view! {
                    <p class="muted">{tr(if on { "sec.on_text" } else { "sec.off_text" })}</p>
                    {(!on).then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("sec.other_sessions")}</p> })}
                    <div class="sec-actions">
                        <button
                            class=if on { "btn ghost" } else { "btn primary" }
                            disabled=move || busy.get()
                            on:click=move |_| {
                                error.set(None);
                                if on {
                                    code.set(String::new());
                                    flow.set(Flow::Disabling);
                                    focus_later(code_ref);
                                } else {
                                    enroll();
                                }
                            }
                        >
                            {spinner(busy)}
                            <Icon icon=if on { icons::SHIELD_ALERT } else { icons::SHIELD_CHECK } size=16 />
                            {tr(if on { "sec.disable" } else { "sec.enable" })}
                        </button>
                    </div>
                }
                .into_any()
            }
            Flow::Enrolling(e) => view! {
                <ol class="enroll">
                    <li class="enroll-step">
                        <span class="step-num" aria-hidden="true">"1"</span>
                        <div class="enroll-body">
                            <strong>{tr("sec.step_scan")}</strong>
                            <p class="muted small">{tr("sec.step_scan_text")}</p>
                            <div class="enroll-qr">
                                <QrSvg text=e.otpauth_uri.clone() />
                                <div class="secret-box">
                                    <span class="muted small">{tr("sec.manual")}</span>
                                    <code class="secret mono">{group4(&e.secret)}</code>
                                    <span class="muted small">{format!("{} · {} · {} s", e.account, e.digits, e.period_s)}</span>
                                    {copy_button("sec.copy_secret", e.secret.clone())}
                                </div>
                            </div>
                        </div>
                    </li>
                    <li class="enroll-step">
                        <span class="step-num" aria-hidden="true">"2"</span>
                        <div class="enroll-body">
                            <strong>{tr("sec.step_code")}</strong>
                            <p class="muted small">{tr("sec.other_sessions")}</p>
                            {code_field("login.totp_code", false)}
                        </div>
                    </li>
                </ol>
            }
            .into_any(),
            Flow::Codes(codes) => {
                let all = codes.join("\n");
                let all2 = all.clone();
                view! {
                    {ui::note(Tone::Ok, icons::CIRCLE_CHECK, view! { {tr("sec.enabled_now")} }.into_any())}
                    <strong>{tr("sec.codes_title")}</strong>
                    <p class="muted small">{tr("sec.codes_text")}</p>
                    <ul class="codes mono" aria-label=tr("sec.codes_title")>
                        {codes.into_iter().map(|c| view! { <li>{c}</li> }).collect_view()}
                    </ul>
                    <div class="sec-actions">
                        {copy_button("sec.copy_codes", all)}
                        <button class="btn ghost sm" type="button" on:click=move |_| {
                            let user = session.with_untracked(|s| s.as_ref().map(|s| s.user.username.clone()).unwrap_or_default());
                            let host = live.system.with_untracked(|s| s.as_ref().map(|s| s.hostname.clone()).unwrap_or_default());
                            let head = tf("sec.codes_file_head", &[("user", &user), ("host", &host)]);
                            api::save_text("rynas-recovery-codes.txt", &format!("{head}\n\n{all2}\n"));
                        }>
                            <Icon icon=icons::DOWNLOAD size=14 />{tr("sec.download")}
                        </button>
                    </div>
                    <label class="check">
                        <input type="checkbox" prop:checked=move || saved.get() on:change=move |ev| saved.set(event_target_checked(&ev)) />
                        <span>{tr("sec.saved")}</span>
                    </label>
                    <div class="sec-actions">
                        <button class="btn primary" disabled=move || !saved.get() on:click=move |_| flow.set(Flow::Idle)>{tr("sec.done")}</button>
                    </div>
                }
                .into_any()
            }
            Flow::Disabling => view! {
                <p class="muted">{tr("sec.disable_text")}</p>
                {code_field("sec.code_or_recovery", true)}
            }
            .into_any(),
        }
    };

    let field = move |g: fn(&rynas_api_types::Session) -> String| {
        Signal::derive(move || session.with(|s| s.as_ref().map(g).unwrap_or_default()))
    };
    let tone = Signal::derive(move || if totp_on() { Tone::Ok } else { Tone::Unknown });
    let state = Signal::derive(move || t(if totp_on() { "sec.on" } else { "sec.off" }).to_string());

    view! {
        <div class="page">
            <PageHeader icon=icons::SHIELD_CHECK title="nav.security" subtitle="sec.subtitle" />
            <div class="grid">
                <section class="card widget w-4" style="--i:0">
                    <WidgetHead icon=icons::USER title="sec.account" />
                    <dl class="kvs">
                        {kv_row("sec.user", field(|s| s.user.username.clone()), "mono")}
                        {kv_row("sec.role", field(|s| t(role_key(s.user.role)).to_string()), "")}
                        {kv_row("sec.realm", field(|s| t(if s.user.realm == "pam" { "sec.realm_pam" } else { "sec.realm_rynas" }).to_string()), "")}
                        {kv_row("sec.since", field(|s| i18n::datetime(s.created_ms)), "tnum")}
                        {kv_row("sec.until", field(|s| i18n::datetime(s.expires_ms)), "tnum")}
                        {kv_row("sec.idle", field(|s| tf("sec.idle_min", &[("n", &(s.idle_timeout_s / 60).to_string())])), "tnum")}
                    </dl>
                </section>
                <section class="card widget w-8" style="--i:1">
                    <WidgetHead icon=icons::KEY title="sec.totp">
                        <Status tone=tone text=state />
                    </WidgetHead>
                    <div class="sec-body">
                        {body}
                        {form_error(error)}
                    </div>
                </section>
            </div>
        </div>
    }
}

fn focus_later(r: NodeRef<html::Input>) {
    request_animation_frame(move || {
        if let Some(el) = r.get_untracked() {
            let _ = el.focus();
        }
    });
}
