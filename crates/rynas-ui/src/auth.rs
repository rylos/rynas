//! Sign-in and session state: the login screen (password, then TOTP when the account has it),
//! the in-memory CSRF token, session expiry and sign-out.
//!
//! The session cookie is `HttpOnly`: the UI never sees it. What the UI keeps is the [`Session`]
//! answer (user, CSRF token, expiry), in memory only, never in localStorage.

use std::cell::Cell;
use std::time::Duration;

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{LoginRequest, Role, Session, error_codes};

use crate::api;
use crate::i18n::{self, Lang, t, tf, tr};
use crate::icons::{self, Icon};
use crate::prefs::{ThemePref, storage_get, storage_set, use_ui};
use crate::ui::{EmptyState, Field};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Asking the server whether there is a session (boot screen).
    Checking,
    /// Login screen.
    Locked,
    /// Signed in.
    Ready,
    /// `?demo` or no API: demo data, no authentication.
    Demo,
}

/// Why the login screen shows (a hint above the form).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    None,
    Expired,
    SignedOut,
    /// The user asked to sign in again before the session ends: they can go back.
    Renew,
}

#[derive(Clone, Copy)]
pub struct Auth {
    pub phase: RwSignal<Phase>,
    pub session: RwSignal<Option<Session>>,
    pub reason: RwSignal<Reason>,
    /// The shell has been shown once: from then on the login screen covers it instead of
    /// replacing it, so the current page (and route) survive an expired session.
    pub shell_mounted: RwSignal<bool>,
    /// Local clock minus server clock (ms), from the `Date` header of the auth answers.
    skew_ms: RwSignal<f64>,
    /// Local time, refreshed every 15 s (expiry notice).
    now_ms: RwSignal<f64>,
    /// The boot check could not reach the server (non-demo builds keep retrying).
    pub unreachable: RwSignal<bool>,
}

thread_local! {
    static AUTH: Cell<Option<Auth>> = const { Cell::new(None) };
}

/// The app's auth state (also reachable from async tasks, which have no reactive owner).
pub fn auth() -> Auth {
    AUTH.with(Cell::get).expect("Auth::new not called")
}

/// CSRF token of the current session, for state-changing requests.
pub fn csrf_token() -> Option<String> {
    AUTH.with(Cell::get)?.session.with_untracked(|s| s.as_ref().map(|s| s.csrf_token.clone()))
}

/// A request answered 401: back to the login screen (no reload, route kept).
pub fn session_lost() {
    storage_set("rynas.sess", "");
    api::forget_early();
    if let Some(a) = AUTH.with(Cell::get) {
        match a.phase.get_untracked() {
            Phase::Ready => {
                a.reason.set(Reason::Expired);
                a.session.set(None);
                a.phase.set(Phase::Locked);
            }
            Phase::Checking => a.phase.set(Phase::Locked),
            // during "renew" the old session may end meanwhile: nothing to go back to
            Phase::Locked if a.reason.get_untracked() == Reason::Renew => {
                a.reason.set(Reason::Expired);
                a.session.set(None);
            }
            _ => {}
        }
    }
}

fn now() -> f64 {
    js_sys::Date::now()
}

impl Auth {
    pub fn new() -> Self {
        let a = Auth {
            phase: RwSignal::new(Phase::Checking),
            session: RwSignal::new(None),
            reason: RwSignal::new(Reason::None),
            shell_mounted: RwSignal::new(false),
            skew_ms: RwSignal::new(0.0),
            now_ms: RwSignal::new(now()),
            unreachable: RwSignal::new(false),
        };
        AUTH.with(|c| c.set(Some(a)));
        let now_ms = a.now_ms;
        set_interval(
            move || {
                now_ms.set(now());
                a.check_expiry();
            },
            Duration::from_secs(15),
        );
        a
    }

    pub fn is_ready(self) -> bool {
        self.phase.get_untracked() == Phase::Ready
    }

    fn note_skew(self) {
        if let Some(d) = api::last_server_date() {
            // `Date` has 1 s resolution: assume the middle of that second
            self.skew_ms.set(now() - (d + 500.0));
        }
    }

    /// A session answer arrived (login, or the boot check).
    pub fn signed_in(self, s: Session) {
        self.note_skew();
        storage_set("rynas.user", &s.user.username);
        storage_set("rynas.sess", "1"); // the next load asks for the first screen's data at once (index.html)
        self.session.set(Some(s));
        self.reason.set(Reason::None);
        self.unreachable.set(false);
        self.now_ms.set(now());
        self.shell_mounted.set(true);
        self.phase.set(Phase::Ready);
    }

    /// Refresh the session (e.g. after enabling TOTP); a 401 locks the app by itself.
    pub fn refresh(self) {
        spawn_local(async move {
            if let Ok(s) = api::get::<Session>("/api/v1/auth/session").await {
                self.note_skew();
                self.session.set(Some(s));
            }
        });
    }

    pub fn sign_out(self) {
        spawn_local(async move {
            // whatever the answer, the session is over for this tab
            let _ = api::post_empty::<()>("/api/v1/auth/logout", None).await;
            storage_set("rynas.sess", "");
            self.session.set(None);
            self.reason.set(Reason::SignedOut);
            self.phase.set(Phase::Locked);
        });
    }

    /// "Sign in again" from the expiry notice: the old session stays valid meanwhile.
    pub fn renew(self) {
        self.reason.set(Reason::Renew);
        self.phase.set(Phase::Locked);
    }

    /// Back from "renew" to the still valid session.
    pub fn cancel_renew(self) {
        if self.session.with_untracked(Option::is_some) {
            self.reason.set(Reason::None);
            self.phase.set(Phase::Ready);
        }
    }

    /// Milliseconds left before the absolute expiry (server clock), tracked.
    pub fn remaining_ms(self) -> Option<f64> {
        let now = self.now_ms.get() - self.skew_ms.get();
        self.session.with(|s| s.as_ref().map(|s| s.expires_ms as f64 - now))
    }

    fn check_expiry(self) {
        if self.phase.get_untracked() != Phase::Ready {
            return;
        }
        let left = self.session.with_untracked(|s| {
            s.as_ref().map(|s| s.expires_ms as f64 - (now() - self.skew_ms.get_untracked()))
        });
        if left.is_some_and(|l| l <= 0.0) {
            session_lost();
        }
    }
}

pub fn role_key(r: Role) -> &'static str {
    match r {
        Role::Admin => "role.admin",
    }
}

/// The rynas mark (static markup: dynamic attributes would split the SVG template and
/// break the gradient). Sized by CSS where it is not 26 px.
pub fn logo() -> impl IntoView {
    view! {
        <svg class="logo-mark" width="26" height="26" viewBox="0 0 32 32" aria-hidden="true">
            <defs>
                <linearGradient id="lg" x1="0" y1="0" x2="1" y2="1">
                    <stop offset="0" stop-color="var(--accent)" />
                    <stop offset="1" stop-color="var(--accent-2)" />
                </linearGradient>
            </defs>
            <rect x="3" y="3" width="26" height="26" rx="8" fill="url(#lg)" />
            <rect x="9" y="10" width="14" height="4" rx="1.5" fill="var(--on-accent)" opacity=".95" />
            <rect x="9" y="18" width="14" height="4" rx="1.5" fill="var(--on-accent)" opacity=".7" />
            <circle cx="20" cy="12" r="1" fill="url(#lg)" />
            <circle cx="20" cy="20" r="1" fill="url(#lg)" />
        </svg>
    }
}

// ------------------------------------------------------------------ login screen

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Password,
    Totp,
}

/// Seconds counter for `Retry-After` (0 = free to try).
pub fn countdown() -> (RwSignal<u32>, impl Fn(u32) + Copy) {
    let left = RwSignal::new(0u32);
    let start = move |secs: u32| {
        left.set(secs);
        fn tick(left: RwSignal<u32>) {
            set_timeout(
                move || {
                    let n = left.get_untracked().saturating_sub(1);
                    left.set(n);
                    if n > 0 {
                        tick(left);
                    }
                },
                Duration::from_secs(1),
            );
        }
        if secs > 0 {
            tick(left);
        }
    };
    (left, start)
}

pub fn mmss(secs: u32) -> String {
    if secs >= 60 { format!("{}:{:02}", secs / 60, secs % 60) } else { format!("{secs} s") }
}

fn focus(r: NodeRef<html::Input>) {
    request_animation_frame(move || {
        if let Some(el) = r.get_untracked() {
            let _ = el.focus();
            el.select();
        }
    });
}

/// Full-screen sign-in: shown before the first session and over the shell when it ends.
#[component]
pub fn LoginScreen() -> impl IntoView {
    let a = auth();
    let ui = use_ui();
    let renewing = move || a.reason.get() == Reason::Renew;
    let prefill = a
        .session
        .with_untracked(|s| s.as_ref().map(|s| s.user.username.clone()))
        .or_else(|| storage_get("rynas.user"))
        .unwrap_or_default();
    let had_user = !prefill.is_empty();
    let user = RwSignal::new(prefill);
    let pass = RwSignal::new(String::new());
    let code = RwSignal::new(String::new());
    let step = RwSignal::new(Step::Password);
    let recovery = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let caps = RwSignal::new(false);
    let (wait, start_wait) = countdown();
    let user_ref = NodeRef::<html::Input>::new();
    let pass_ref = NodeRef::<html::Input>::new();
    let code_ref = NodeRef::<html::Input>::new();

    // first focus: user name, or password when the name is known
    if had_user { focus(pass_ref) } else { focus(user_ref) }

    let submit = move || {
        if busy.get_untracked() || wait.get_untracked() > 0 {
            return;
        }
        let totp = (step.get_untracked() == Step::Totp).then(|| code.get_untracked().trim().to_string());
        if user.with_untracked(|u| u.trim().is_empty()) || pass.with_untracked(String::is_empty) {
            error.set(Some(t("login.empty").to_string()));
            return;
        }
        if totp.as_ref().is_some_and(String::is_empty) {
            focus(code_ref);
            return;
        }
        let req = LoginRequest { username: user.get_untracked().trim().to_string(), password: pass.get_untracked(), totp };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<_, Session>("/api/v1/auth/login", Some(&req)).await;
            busy.set(false);
            match r {
                Ok(s) => {
                    pass.set(String::new());
                    code.set(String::new());
                    a.signed_in(s);
                }
                Err(e) => match e.code() {
                    error_codes::TOTP_REQUIRED => {
                        step.set(Step::Totp);
                        focus(code_ref);
                    }
                    error_codes::INVALID_TOTP => {
                        error.set(Some(e.text()));
                        code.set(String::new());
                        focus(code_ref);
                    }
                    error_codes::INVALID_CREDENTIALS => {
                        error.set(Some(e.text()));
                        pass.set(String::new());
                        step.set(Step::Password);
                        focus(pass_ref);
                    }
                    error_codes::RATE_LIMITED => {
                        start_wait(e.retry_after().unwrap_or(30));
                        error.set(Some(e.text()));
                    }
                    _ => error.set(Some(e.text())),
                },
            }
        });
    };

    let on_code = move |ev: leptos::ev::Event| {
        let raw = event_target_value(&ev);
        if recovery.get_untracked() {
            code.set(raw.to_ascii_uppercase());
            return;
        }
        // paste-friendly: "123 456" or "123-456" become "123456"
        let digits: String = raw.chars().filter(char::is_ascii_digit).take(6).collect();
        if let Some(el) = code_ref.get_untracked()
            && el.value() != digits
        {
            el.set_value(&digits);
        }
        let full = digits.len() == 6;
        code.set(digits);
        if full {
            submit();
        }
    };

    let caps_check = move |ev: leptos::ev::KeyboardEvent| {
        let on = ev.get_modifier_state("CapsLock");
        if caps.get_untracked() != on {
            caps.set(on);
        }
    };

    let hint = move || match a.reason.get() {
        Reason::Expired => Some(("login.expired", icons::HISTORY)),
        Reason::SignedOut => Some(("login.signed_out", icons::CIRCLE_CHECK)),
        Reason::Renew => Some(("login.renew", icons::REFRESH_CW)),
        Reason::None => None,
    };

    let theme_icon = move || match ui.theme.get() {
        ThemePref::Auto => icons::MONITOR,
        ThemePref::Light => icons::SUN,
        ThemePref::Dark => icons::MOON,
    };
    let lang_now = move || i18n::lang();

    view! {
        <div class="login" role="main">
            <div class="login-glow" aria-hidden="true"></div>
            <div class="login-tools">
                <div class="segmented small-seg" role="radiogroup" aria-label=tr("language")>
                    {[(Lang::En, "EN"), (Lang::It, "IT")]
                        .into_iter()
                        .map(|(l, label)| view! {
                            <button role="radio" aria-checked=move || (lang_now() == l).to_string() class:on=move || lang_now() == l on:click=move |_| i18n::set_lang(Some(l))>
                                {label}
                            </button>
                        })
                        .collect_view()}
                </div>
                <button class="icon-btn" aria-label=tr("bar.theme") title=tr("bar.theme") on:click=move |_| ui.theme.update(|th| *th = th.next())>
                    {move || view! { <Icon icon=theme_icon() /> }}
                </button>
            </div>
            <form class="login-card" novalidate=true on:submit=move |ev| { ev.prevent_default(); submit(); }>
                <div class="login-brand">
                    {logo()}
                    <span class="login-name">"rynas"</span>
                </div>
                {move || hint().map(|(key, icon)| view! {
                    <p class="login-hint"><Icon icon=icon size=16 />{t(key)}</p>
                })}
                {move || (step.get() == Step::Password).then(|| view! {
                    <h1 class="login-title">{tr("login.sign_in")}</h1>
                    <Field label="username">
                        <input
                            node_ref=user_ref
                            class="input"
                            name="username"
                            autocomplete="username"
                            autocapitalize="none"
                            spellcheck="false"
                            required=true
                            prop:value=move || user.get()
                            on:input=move |ev| user.set(event_target_value(&ev))
                            disabled=renewing
                        />
                    </Field>
                    <Field label="login.password">
                        <input
                            node_ref=pass_ref
                            class="input"
                            type="password"
                            name="password"
                            autocomplete="current-password"
                            required=true
                            prop:value=move || pass.get()
                            on:input=move |ev| pass.set(event_target_value(&ev))
                            on:keyup=caps_check
                            on:keydown=caps_check
                        />
                        {move || caps.get().then(|| view! {
                            <span class="field-note warn-text"><Icon icon=icons::TRIANGLE_ALERT size=14 />{tr("login.caps")}</span>
                        })}
                    </Field>
                })}
                {move || (step.get() == Step::Totp).then(|| view! {
                    <div class="login-step">
                        <div class="login-step-icon"><Icon icon=icons::SHIELD_CHECK size=22 /></div>
                        <h1 class="login-title">{tr("sec.totp")}</h1>
                        <p class="muted">{move || if recovery.get() { t("login.recovery_text") } else { t("login.totp_text") }}</p>
                    </div>
                    {move || {
                        // re-created when switching to a recovery code (label and keyboard change)
                        let rec = recovery.get();
                        view! {
                            <Field label=if rec { "login.recovery_code" } else { "login.totp_code" }>
                                <input
                                    node_ref=code_ref
                                    class=if rec { "input mono code-input recovery" } else { "input mono code-input" }
                                    name=if rec { "recovery" } else { "totp" }
                                    inputmode=if rec { "text" } else { "numeric" }
                                    autocomplete=if rec { "off" } else { "one-time-code" }
                                    autocapitalize="characters"
                                    spellcheck="false"
                                    placeholder=if rec { "XXXX-XXXX-XXXX-XXXX" } else { "000000" }
                                    maxlength="32"
                                    prop:value=move || code.get()
                                    on:input=on_code
                                />
                            </Field>
                        }
                    }}
                    <div class="login-links">
                        <button type="button" class="link-btn" on:click=move |_| {
                            recovery.update(|r| *r = !*r);
                            code.set(String::new());
                            error.set(None);
                            focus(code_ref);
                        }>
                            {move || if recovery.get() { t("login.use_app") } else { t("login.use_recovery") }}
                        </button>
                        <button type="button" class="link-btn" on:click=move |_| {
                            step.set(Step::Password);
                            recovery.set(false);
                            code.set(String::new());
                            error.set(None);
                            focus(pass_ref);
                        }>
                            {tr("back")}
                        </button>
                    </div>
                })}
                {crate::form::form_error(error)}
                <button class="btn primary login-submit" type="submit" disabled=move || { busy.get() || wait.get() > 0 } aria-busy=move || busy.get().to_string()>
                    {crate::form::spinner(busy)}
                    {move || if busy.get() {
                        t("login.signing_in").to_string()
                    } else if wait.get() > 0 {
                        tf("login.wait", &[("t", &mmss(wait.get()))])
                    } else if step.get() == Step::Totp {
                        t("login.verify").to_string()
                    } else {
                        t("login.sign_in").to_string()
                    }}
                </button>
                {move || renewing().then(|| view! {
                    <button type="button" class="btn ghost login-cancel" on:click=move |_| a.cancel_renew()>
                        {tr("cancel")}
                    </button>
                })}
            </form>
            <p class="login-foot muted small">{tr("login.foot")}</p>
        </div>
    }
}

/// Boot screen while the session check runs (same look as the static one in index.html), and
/// the offline state when the API does not answer (release builds never fall back to demo data).
#[component]
pub fn BootScreen() -> impl IntoView {
    let a = auth();
    let live = crate::live::use_live();
    view! {
        <div class="boot-screen">
            {move || if a.unreachable.get() {
                view! {
                    <div class="card offline-card" role="alert">
                        <EmptyState
                            icon=icons::NETWORK
                            title=Signal::derive(|| t("err.offline_title").to_string())
                            text=Signal::derive(|| t("err.offline_text").to_string())
                        >
                            <button class="btn primary" on:click=move |_| live.retry_now()>
                                <Icon icon=icons::REFRESH_CW size=16 />{tr("err.retry")}
                            </button>
                        </EmptyState>
                    </div>
                }
                .into_any()
            } else {
                view! { <i aria-hidden="true"></i> }.into_any()
            }}
        </div>
    }
}

/// Discreet notice when the session is about to reach its absolute expiry.
#[component]
pub fn SessionNotice() -> impl IntoView {
    let a = auth();
    let dismissed = RwSignal::new(false);
    let minutes = move || a.remaining_ms().map(|ms| (ms / 60_000.0).round().max(1.0) as u32);
    let show = move || {
        a.phase.get() == Phase::Ready && !dismissed.get() && a.remaining_ms().is_some_and(|ms| ms > 0.0 && ms <= 10.0 * 60_000.0)
    };
    view! {
        <Show when=show>
            <div class="session-note" role="status">
                <Icon icon=icons::HISTORY size=16 />
                <span>{move || {
                    let n = minutes().unwrap_or(1);
                    if n == 1 { t("session.expires_1").to_string() } else { tf("session.expires", &[("n", &n.to_string())]) }
                }}</span>
                <button class="btn primary sm" on:click=move |_| a.renew()>{tr("session.renew")}</button>
                <button class="icon-btn sm" aria-label=tr("close") on:click=move |_| dismissed.set(true)>
                    <Icon icon=icons::X size=14 />
                </button>
            </div>
        </Show>
    }
}
