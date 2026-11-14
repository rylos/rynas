//! Encrypted shared folders (D-CRYPT E1, `docs/phase2/encrypted-folders.md`): the option of
//! a new folder (password, automatic unlock, limits, gocryptfs install), the recovery key shown
//! once after the creation, and for an existing encrypted folder its state, unlock, lock,
//! password change and automatic unlock.
//!
//! Passwords live in signals of the open dialog only and are cleared when it closes.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::storage::RecoveryKey;
use rynas_api_types::{
    CryptInfo, CryptPasswordResponse, CryptState, CryptStatus, CryptUnlockResponse, ShareView, TaskStatus, error_codes,
};
use serde_json::json;

use crate::api;
use crate::form::{In, SaveBar, form_error, save_bar_with, segmented, spinner, strength, strength_meter, switch, text_field};
use crate::pages::storage::volume::{key_once, key_once_foot};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::toast;
use crate::ui::{self, Modal, Remote, Tone};

/// Password length in characters, as the server checks it (`rynas_services::crypt`).
const MIN_LEN: usize = 8;
const MAX_LEN: usize = 256;
/// `maxlength` of the password inputs: it counts UTF-16 units, so 256 characters may need
/// twice as many; the character limit itself is [`password_problem`]'s.
const INPUT_MAX: u32 = 2 * MAX_LEN as u32;

/// The encryption part of the new-folder form.
#[derive(Clone, Copy)]
pub struct CryptForm {
    pub on: RwSignal<bool>,
    pub pw: RwSignal<String>,
    pub pw2: RwSignal<String>,
    pub auto: RwSignal<bool>,
}

impl CryptForm {
    pub fn new() -> Self {
        let f = CryptForm { on: RwSignal::new(false), pw: RwSignal::new(String::new()), pw2: RwSignal::new(String::new()), auto: RwSignal::new(false) };
        // the passwords must not linger after the editor
        on_cleanup(move || {
            let _ = f.pw.try_set(String::new());
            let _ = f.pw2.try_set(String::new());
        });
        f
    }

    /// i18n key of the first problem of the passwords.
    pub fn problem(&self) -> Option<&'static str> {
        password_problem(&self.pw.get_untracked(), &self.pw2.get_untracked())
    }
}

fn password_problem(pw: &str, pw2: &str) -> Option<&'static str> {
    if pw.chars().count() < MIN_LEN {
        Some("pw.too_short")
    } else if pw.chars().count() > MAX_LEN {
        Some("pw.too_long")
    } else if pw.chars().any(char::is_control) {
        Some("pw.control")
    } else if pw != pw2 {
        Some("pw.mismatch")
    } else {
        None
    }
}

/// What an encrypted folder gives up, as a list.
fn limits() -> AnyView {
    let keys = ["crypt.l_speed", "crypt.l_space", "crypt.l_snap", "crypt.l_apps", "crypt.l_convert"];
    view! {
        <span>{tr("crypt.experimental")}</span>
        <ul class="nocow-costs">{keys.into_iter().map(|k| view! { <li>{tr(k)}</li> }).collect_view()}</ul>
    }
    .into_any()
}

/// The automatic unlock's trade-off, with or without a TPM.
fn auto_help(tpm2: bool) -> AnyView {
    view! { <span class="field-help">{tr(if tpm2 { "crypt.auto_help_tpm" } else { "crypt.auto_help" })}</span> }.into_any()
}

/// The option of a new folder (inside "New subvolume"): switch, then gocryptfs install or
/// the password fields.
pub fn create_fields(f: CryptForm, show: RwSignal<bool>) -> AnyView {
    let info = Remote::<CryptInfo>::new("/api/v1/crypt");
    let installing = RwSignal::new(false);
    let ierr = RwSignal::new(None::<String>);
    let install = move |_| {
        installing.set(true);
        ierr.set(None);
        spawn_local(async move {
            let r = crate::tasks::run_task("/api/v1/crypt/install", None).await;
            let _ = installing.try_set(false);
            match r {
                Ok(task) if task.status == TaskStatus::Ok => info.reload(),
                Ok(task) => {
                    let _ = ierr.try_set(Some(crate::tasks::task_error(&task)));
                }
                Err(e) => {
                    let _ = ierr.try_set(Some(e.text()));
                }
            }
        });
    };
    let on = Signal::derive(move || f.on.get());
    let set_on = Callback::new(move |v: bool| f.on.set(v));
    let score = Signal::derive(move || f.pw.with(|p| (!p.is_empty()).then(|| strength(p, ""))));
    let pw_err = Signal::derive(move || {
        let p = f.pw.get();
        ((show.get() || !p.is_empty()) && p.chars().count() < MIN_LEN).then(|| t("pw.too_short").to_string())
    });
    let pw2_err = Signal::derive(move || {
        let (a, b) = (f.pw.get(), f.pw2.get());
        ((show.get() || !b.is_empty()) && a != b).then(|| t("pw.mismatch").to_string())
    });
    let auto = Signal::derive(move || f.auto.get());
    let set_auto = Callback::new(move |v: bool| f.auto.set(v));
    view! {
        {switch("crypt.encrypt", Some("crypt.encrypt_help"), on, set_on)}
        {move || on.get().then(|| {
            let Some(i) = info.data.get() else {
                return ui::loading(info.err.into(), Callback::new(move |_| info.reload()), 1);
            };
            if !i.installed {
                return view! {
                    {ui::note(Tone::Info, icons::INFO, view! { {tr("crypt.not_installed")} }.into_any())}
                    <div class="card-actions">
                        <button type="button" class="btn ghost sm" disabled=move || installing.get() on:click=install>
                            {spinner(installing)}<Icon icon=icons::DOWNLOAD size=14 />{tr("crypt.install")}
                        </button>
                    </div>
                    {form_error(ierr)}
                }
                .into_any();
            }
            if !i.supported {
                let v = i.version.clone().unwrap_or_else(|| "?".into());
                return ui::note(Tone::Err, icons::TRIANGLE_ALERT, view! { {tf("crypt.too_old", &[("version", &v)])} }.into_any());
            }
            view! {
                {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, limits())}
                {text_field("crypt.password", f.pw.into(), Callback::new(move |v| f.pw.set(v)), pw_err, In { kind: "password", autocomplete: "new-password", maxlength: INPUT_MAX, help: Some("crypt.password_help"), ..In::default() })}
                {strength_meter(score)}
                {text_field("pw.confirm", f.pw2.into(), Callback::new(move |v| f.pw2.set(v)), pw2_err, In { kind: "password", autocomplete: "new-password", maxlength: INPUT_MAX, ..In::default() })}
                {switch("crypt.auto", None, auto, set_auto)}
                {auto_help(i.tpm2)}
            }
            .into_any()
        })}
    }
    .into_any()
}

/// Creates the folder (task `crypt.create`) and fetches its recovery key at once, so the key
/// is not lost when a later step (the share, the apply) fails. `Err`: no folder; `Ok(Err)`:
/// the folder exists but its key could not be fetched (shown again later with the password).
pub async fn create_folder(volume: &str, name: &str, f: CryptForm) -> Result<Result<String, String>, String> {
    let body = json!({ "volume": volume, "name": name, "password": f.pw.get_untracked(), "auto_unlock": f.auto.get_untracked() });
    let task = crate::tasks::run_task("/api/v1/crypt/folders", Some(body.to_string())).await.map_err(|e| e.text())?;
    if task.status != TaskStatus::Ok {
        return Err(crate::tasks::task_error(&task));
    }
    let base = format!("/api/v1/crypt/folders/{}/{}", api::encode(volume), api::encode(name));
    let once = api::post::<(), RecoveryKey>(&format!("{base}/reveal-recovery-key"), None).await;
    // the kept key is gone (rynasd restarted in between): read it again with the password
    let key = match once {
        Ok(k) => Ok(k.key),
        Err(_) => {
            let body = json!({ "password": f.pw.get_untracked() });
            api::post::<_, RecoveryKey>(&format!("{base}/recovery-key"), Some(&body)).await.map(|k| k.key).map_err(|e| e.text())
        }
    };
    let _ = f.pw.try_set(String::new());
    let _ = f.pw2.try_set(String::new());
    Ok(key)
}

/// The recovery key of a new encrypted folder, shown once.
#[component]
pub fn RecoveryDialog(name: String, key: String, on_done: Callback<()>) -> impl IntoView {
    let key = StoredValue::new(key);
    // the key must not linger in memory after the dialog
    on_cleanup(move || key.set_value(String::new()));
    let file = format!("recovery-key-{name}.txt");
    let stored = RwSignal::new(false);
    view! {
        <Modal title=tf("sto.rk_title", &[("name", &name)]) on_close=Callback::new(|_| ()) foot=move || key_once_foot(stored, on_done)>
            <p class="muted small">{tr("crypt.rk_text")}</p>
            {key_once(key.get_value(), file, "crypt.rk_keep", stored)}
        </Modal>
    }
}

/// The state chip of an encrypted folder.
pub fn state_chip(st: &CryptStatus) -> AnyView {
    match st.state {
        CryptState::Unlocked => ui::status(Tone::Ok, t("crypt.unlocked").to_string()),
        CryptState::Locked => ui::status(Tone::Warn, t("crypt.locked").to_string()),
        CryptState::InUse => ui::status(Tone::Warn, t("crypt.in_use").to_string()),
        CryptState::Missing => ui::status(Tone::Err, t("crypt.missing").to_string()),
    }
}

/// A lock or unlock asked from the list or the editor.
#[derive(Clone, PartialEq)]
pub enum Action {
    Unlock(String),
    Lock(String),
    Password(String),
    AutoOn(String),
    /// The recovery key again: the share's name and its folder's path.
    Reveal(String, String),
}

/// The dialog of an [`Action`]; `done` gets the share's new view (none for a password).
pub fn action_dialog(a: Action, close: Callback<()>, done: Callback<Option<ShareView>>) -> AnyView {
    match a {
        Action::Unlock(n) => view! { <UnlockDialog name=n on_close=close on_done=done /> }.into_any(),
        Action::Lock(n) => view! { <LockDialog name=n on_close=close on_done=done /> }.into_any(),
        Action::Password(n) => view! { <PasswordDialog name=n on_close=close on_done=done /> }.into_any(),
        Action::AutoOn(n) => view! { <AutoDialog name=n on_close=close on_done=done /> }.into_any(),
        Action::Reveal(n, path) => view! { <RevealDialog name=n path=path on_close=close /> }.into_any(),
    }
}

fn share_url(name: &str, tail: &str) -> String {
    format!("/api/v1/shares/{}/{tail}", api::encode(name))
}

fn error_text(e: &api::ApiErr) -> String {
    // the server's English detail adds nothing to a wrong password
    if e.code() == error_codes::CRYPT_WRONG_PASSWORD { crate::i18n::err_text(e.code()).to_string() } else { e.text() }
}

#[component]
fn UnlockDialog(name: String, on_close: Callback<()>, on_done: Callback<Option<ShareView>>) -> impl IntoView {
    let pass = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // running containers that still see the locked (empty) folder after the unlock
    let stale = RwSignal::new(Vec::<String>::new());
    let n = StoredValue::new(name.clone());
    on_cleanup(move || {
        let _ = pass.try_set(String::new());
    });
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let p = pass.get_untracked();
        if p.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let name = n.get_value();
            let r = api::post::<_, CryptUnlockResponse>(&share_url(&name, "unlock"), Some(&json!({ "password": p }))).await;
            let _ = busy.try_set(false);
            let _ = pass.try_set(String::new());
            match r {
                Ok(v) => {
                    toast(Tone::Ok, tf("crypt.unlocked_toast", &[("name", &name)]));
                    on_done.run(Some(v.share));
                    if v.containers.is_empty() {
                        on_close.run(());
                    } else {
                        let _ = stale.try_set(v.containers);
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(error_text(&e)));
                }
            }
        });
    };
    let restart = move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let mut failed = Vec::new();
            for c in stale.get_untracked() {
                let url = format!("/api/v1/docker/containers/{}/restart", api::encode(&c));
                match crate::tasks::run_task(&url, None).await {
                    Ok(task) if task.status == TaskStatus::Ok => {}
                    Ok(task) => failed.push(format!("{c}: {}", crate::tasks::task_error(&task))),
                    Err(e) => failed.push(format!("{c}: {}", e.text())),
                }
            }
            let _ = busy.try_set(false);
            if failed.is_empty() {
                toast(Tone::Ok, t("crypt.restarted").to_string());
                on_close.run(());
            } else {
                let _ = error.try_set(Some(failed.join("; ")));
            }
        });
    };
    const FORM: &str = "crypt-unlock";
    let stale_shown = Memo::new(move |_| !stale.with(Vec::is_empty));
    let foot = move || {
        if stale_shown.get() {
            // the folder is open: "Later" leaves the containers as they are
            view! {
                <button class="btn ghost" type="button" disabled=move || busy.get() on:click=move |_| on_close.run(())>{tr("crypt.later")}</button>
                <button class="btn primary" type="button" disabled=move || busy.get() on:click=restart>{spinner(busy)}{tr("crypt.restart")}</button>
            }
            .into_any()
        } else {
            save_bar_with(SaveBar { can_save: Signal::derive(move || !pass.with(String::is_empty)), form: Some(FORM), ..SaveBar::new(busy, "crypt.unlock", on_close, Callback::new(|_| {})) })
        }
    };
    view! {
        <Modal title=tf("sto.unlock_title", &[("name", &name)]) on_close=on_close foot=foot>
            {move || if stale.with(Vec::is_empty) {
                view! {
                    <form class="form" id=FORM on:submit=submit>
                        <p class="muted small">{tr("crypt.unlock_text")}</p>
                        <label class="field">
                            <span class="field-label">{tr("crypt.password")}</span>
                            <input class="input" type="password" autocomplete="current-password" maxlength=INPUT_MAX prop:value=move || pass.get() on:input=move |ev| pass.set(event_target_value(&ev)) />
                        </label>
                        {form_error(error)}
                    </form>
                }
                .into_any()
            } else {
                view! {
                    <div class="form">
                        {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { {tr("crypt.stale_text")} }.into_any())}
                        <ul class="nocow-costs">{stale.get().into_iter().map(|c| view! { <li class="mono">{c}</li> }).collect_view()}</ul>
                        {form_error(error)}
                    </div>
                }
                .into_any()
            }}
        </Modal>
    }
}

#[component]
fn LockDialog(name: String, on_close: Callback<()>, on_done: Callback<Option<ShareView>>) -> impl IntoView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let n = StoredValue::new(name.clone());
    let go = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let name = n.get_value();
            let r = api::post::<(), ShareView>(&share_url(&name, "lock"), None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(v) => {
                    toast(Tone::Ok, tf("crypt.locked_toast", &[("name", &name)]));
                    on_done.run(Some(v));
                    on_close.run(());
                }
                Err(e) if e.code() == error_codes::CRYPT_BUSY => {
                    let _ = error.try_set(Some(format!("{}{}{}", t("crypt.busy"), api::DETAIL_SEP, e.message())));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    });
    let mut c = ui::Confirm::new(tf("crypt.lock_title", &[("name", &name)]), t("crypt.lock_text").to_string(), "crypt.lock", false, go, on_close);
    c.busy = busy;
    c.error = error;
    ui::confirm(c)
}

#[component]
fn PasswordDialog(name: String, on_close: Callback<()>, on_done: Callback<Option<ShareView>>) -> impl IntoView {
    const MODES: &[(&str, &str)] = &[("password", "crypt.pw_current"), ("recovery", "crypt.pw_recovery")];
    let mode = RwSignal::new("password".to_string());
    let cur = RwSignal::new(String::new());
    let pw = RwSignal::new(String::new());
    let pw2 = RwSignal::new(String::new());
    let show = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let n = StoredValue::new(name.clone());
    on_cleanup(move || {
        for s in [cur, pw, pw2] {
            let _ = s.try_set(String::new());
        }
    });
    let score = Signal::derive(move || pw.with(|p| (!p.is_empty()).then(|| strength(p, ""))));
    let pw_err = Signal::derive(move || {
        let p = pw.get();
        ((show.get() || !p.is_empty()) && p.chars().count() < MIN_LEN).then(|| t("pw.too_short").to_string())
    });
    let pw2_err = Signal::derive(move || {
        let (a, b) = (pw.get(), pw2.get());
        ((show.get() || !b.is_empty()) && a != b).then(|| t("pw.mismatch").to_string())
    });
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        show.set(true);
        if cur.with_untracked(String::is_empty) {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        if let Some(k) = password_problem(&pw.get_untracked(), &pw2.get_untracked()) {
            error.set(Some(t(k).to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let body = json!({ "current": cur.get_untracked(), "recovery": mode.get_untracked() == "recovery", "new_password": pw.get_untracked() });
        spawn_local(async move {
            let name = n.get_value();
            let r = api::post::<_, CryptPasswordResponse>(&share_url(&name, "crypt/password"), Some(&body)).await;
            let _ = busy.try_set(false);
            match r {
                Ok(res) => {
                    toast(Tone::Ok, tf("crypt.pw_done", &[("name", &name)]));
                    if let Some(why) = res.auto_unlock_off {
                        // changed, but the folder no longer unlocks by itself: say so and
                        // show the new state
                        toast(Tone::Warn, format!("{}{}{why}", t("crypt.auto_off"), api::DETAIL_SEP));
                        let v = api::get::<ShareView>(&format!("/api/v1/shares/{}", api::encode(&name))).await.ok();
                        on_done.run(v);
                    }
                    on_close.run(());
                }
                Err(e) => {
                    let _ = error.try_set(Some(error_text(&e)));
                }
            }
        });
    };
    const FORM: &str = "crypt-password";
    view! {
        <Modal title=tf("crypt.pw_title", &[("name", &name)]) on_close=on_close foot=move || save_bar_with(SaveBar { form: Some(FORM), ..SaveBar::new(busy, "crypt.pw_change", on_close, Callback::new(|_| {})) })>
            <form class="form" id=FORM on:submit=submit>
                {segmented("crypt.pw_with", MODES, mode.into(), Callback::new(move |v| { mode.set(v); cur.set(String::new()); }))}
                {move || if mode.get() == "recovery" {
                    text_field("crypt.pw_recovery", cur.into(), Callback::new(move |v| cur.set(v)), Signal::derive(|| None), In { mono: true, maxlength: 200, help: Some("crypt.pw_recovery_help"), ..In::default() })
                } else {
                    text_field("crypt.pw_current", cur.into(), Callback::new(move |v| cur.set(v)), Signal::derive(|| None), In { kind: "password", autocomplete: "current-password", maxlength: INPUT_MAX, ..In::default() })
                }}
                {text_field("pw.new", pw.into(), Callback::new(move |v| pw.set(v)), pw_err, In { kind: "password", autocomplete: "new-password", maxlength: INPUT_MAX, ..In::default() })}
                {strength_meter(score)}
                {text_field("pw.confirm", pw2.into(), Callback::new(move |v| pw2.set(v)), pw2_err, In { kind: "password", autocomplete: "new-password", maxlength: INPUT_MAX, ..In::default() })}
                {form_error(error)}
            </form>
        </Modal>
    }
}

#[component]
fn AutoDialog(name: String, on_close: Callback<()>, on_done: Callback<Option<ShareView>>) -> impl IntoView {
    let info = Remote::<CryptInfo>::new("/api/v1/crypt");
    let pass = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let n = StoredValue::new(name.clone());
    on_cleanup(move || {
        let _ = pass.try_set(String::new());
    });
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let p = pass.get_untracked();
        if p.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let name = n.get_value();
            let r = api::put::<_, ShareView>(&share_url(&name, "crypt/auto-unlock"), &json!({ "enabled": true, "password": p })).await;
            let _ = busy.try_set(false);
            match r {
                Ok(v) => {
                    on_done.run(Some(v));
                    on_close.run(());
                }
                Err(e) => {
                    let _ = pass.try_set(String::new());
                    let _ = error.try_set(Some(error_text(&e)));
                }
            }
        });
    };
    const FORM: &str = "crypt-auto";
    let foot = move || save_bar_with(SaveBar { can_save: Signal::derive(move || !pass.with(String::is_empty)), form: Some(FORM), ..SaveBar::new(busy, "sec.activate", on_close, Callback::new(|_| {})) });
    view! {
        <Modal title=tf("crypt.auto_title", &[("name", &name)]) on_close=on_close foot=foot>
            <form class="form" id=FORM on:submit=submit>
                {move || auto_help(info.data.with(|i| i.as_ref().is_some_and(|i| i.tpm2)))}
                <label class="field">
                    <span class="field-label">{tr("crypt.password")}</span>
                    <input class="input" type="password" autocomplete="current-password" maxlength=INPUT_MAX prop:value=move || pass.get() on:input=move |ev| pass.set(event_target_value(&ev)) />
                </label>
                {form_error(error)}
            </form>
        </Modal>
    }
}

/// The recovery key again, read with the folder's password.
#[component]
fn RevealDialog(name: String, path: String, on_close: Callback<()>) -> impl IntoView {
    let pass = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let key = RwSignal::new(None::<String>);
    let url = rynas_api_types::storage::split_volume_path(&path)
        .map(|(v, n)| format!("/api/v1/crypt/folders/{}/{}/recovery-key", api::encode(&v), api::encode(n)));
    let url = StoredValue::new(url);
    on_cleanup(move || {
        let _ = pass.try_set(String::new());
        let _ = key.try_set(None);
    });
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let p = pass.get_untracked();
        let Some(u) = url.get_value() else { return };
        if p.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = api::post::<_, RecoveryKey>(&u, Some(&json!({ "password": p }))).await;
            let _ = busy.try_set(false);
            let _ = pass.try_set(String::new());
            match r {
                Ok(k) => {
                    let _ = key.try_set(Some(k.key));
                }
                Err(e) => {
                    let _ = error.try_set(Some(error_text(&e)));
                }
            }
        });
    };
    let file = format!("recovery-key-{name}.txt");
    const FORM: &str = "crypt-reveal";
    let stored = RwSignal::new(false);
    let shown = Memo::new(move |_| key.with(Option::is_some));
    let foot = move || {
        if shown.get() {
            key_once_foot(stored, on_close)
        } else {
            save_bar_with(SaveBar { can_save: Signal::derive(move || !pass.with(String::is_empty)), form: Some(FORM), ..SaveBar::new(busy, "sto.rk_reveal", on_close, Callback::new(|_| {})) })
        }
    };
    view! {
        <Modal title=tf("sto.rk_title", &[("name", &name)]) on_close=on_close foot=foot>
            {move || match key.get() {
                Some(k) => view! {
                    <p class="muted small">{tr("crypt.rk_text")}</p>
                    {key_once(k, file.clone(), "crypt.rk_keep", stored)}
                }
                .into_any(),
                None => view! {
                    <form class="form" id=FORM on:submit=submit>
                        <p class="muted small">{tr("crypt.reveal_text")}</p>
                        <label class="field">
                            <span class="field-label">{tr("crypt.password")}</span>
                            <input class="input" type="password" autocomplete="current-password" maxlength=INPUT_MAX prop:value=move || pass.get() on:input=move |ev| pass.set(event_target_value(&ev)) />
                        </label>
                        {form_error(error)}
                    </form>
                }
                .into_any(),
            }}
        </Modal>
    }
}

/// The encryption section of an existing encrypted folder's editor.
pub fn existing(v: &ShareView) -> Option<AnyView> {
    let st = RwSignal::new(v.status.crypt.clone()?);
    let name = StoredValue::new(v.share.name.clone());
    let path = StoredValue::new(v.share.path.clone());
    let act = RwSignal::new(None::<Action>);
    let off_busy = RwSignal::new(false);
    let off_err = RwSignal::new(None::<String>);
    let done = Callback::new(move |nv: Option<ShareView>| {
        if let Some(c) = nv.and_then(|v| v.status.crypt) {
            st.set(c);
        }
        crate::config::changed();
    });
    let auto_off = move || {
        off_busy.set(true);
        off_err.set(None);
        spawn_local(async move {
            let r = api::put::<_, ShareView>(&share_url(&name.get_value(), "crypt/auto-unlock"), &json!({ "enabled": false })).await;
            let _ = off_busy.try_set(false);
            match r {
                Ok(v) => done.run(Some(v)),
                Err(e) => {
                    let _ = off_err.try_set(Some(e.text()));
                }
            }
        });
    };
    let auto = Signal::derive(move || st.with(|s| s.auto_unlock));
    let set_auto = Callback::new(move |on: bool| {
        if on {
            act.set(Some(Action::AutoOn(name.get_value())));
        } else {
            auto_off();
        }
    });
    let dialog = move || act.get().map(|a| action_dialog(a, Callback::new(move |_| act.set(None)), done));
    Some(
        view! {
            <fieldset class="fieldset">
                <legend class="field-label">{tr("crypt.title")}</legend>
                <div class="nocow-row">
                    {move || st.with(state_chip)}
                    {move || {
                        let locked = st.with(|s| s.state == CryptState::Locked);
                        let unlocked = st.with(|s| matches!(s.state, CryptState::Unlocked | CryptState::InUse));
                        view! {
                            {locked.then(|| view! {
                                <button type="button" class="btn ghost sm" on:click=move |_| act.set(Some(Action::Unlock(name.get_value())))>
                                    <Icon icon=icons::LOCK_OPEN size=14 />{tr("crypt.unlock")}
                                </button>
                            })}
                            {unlocked.then(|| view! {
                                <button type="button" class="btn ghost sm" on:click=move |_| act.set(Some(Action::Lock(name.get_value())))>
                                    <Icon icon=icons::LOCK size=14 />{tr("crypt.lock")}
                                </button>
                            })}
                        }
                    }}
                    <button type="button" class="btn ghost sm" on:click=move |_| act.set(Some(Action::Password(name.get_value())))>
                        <Icon icon=icons::KEY size=14 />{tr("crypt.pw_change")}
                    </button>
                    <button type="button" class="btn ghost sm" on:click=move |_| act.set(Some(Action::Reveal(name.get_value(), path.get_value())))>
                        <Icon icon=icons::KEY size=14 />{tr("sto.rk_reveal")}
                    </button>
                </div>
                <span class="field-help">{tr("crypt.existing_help")}</span>
                <span class="field-help mono crypt-dir">{move || st.with(|s| s.cipher_dir.clone())}</span>
                {switch("crypt.auto", None, auto, set_auto)}
                {form_error(off_err)}
                {move || off_busy.get().then(|| view! { <span class="field-help">{tr("loading")}</span> })}
            </fieldset>
            {dialog}
        }
        .into_any(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_rules_match_the_server() {
        assert_eq!(password_problem("short", "short"), Some("pw.too_short"));
        assert_eq!(password_problem("correct horse", "correct hors"), Some("pw.mismatch"));
        assert_eq!(password_problem("tab\tinside!", "tab\tinside!"), Some("pw.control"));
        assert_eq!(password_problem(&"x".repeat(257), &"x".repeat(257)), Some("pw.too_long"));
        assert_eq!(password_problem("correct horse", "correct horse"), None);
    }
}
