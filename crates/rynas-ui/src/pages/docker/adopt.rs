//! Docker › Stacks: adopting compose projects where they are (§5.7 of
//! `docs/phase2/docker-apps.md`). "Adopt" records a running project from its labels,
//! "Import folder" registers a folder with compose files; nothing on disk changes. An
//! adopted stack can be released (the record goes, files and containers stay) or moved
//! into the rynas stacks folder (a task: stop, copy with the data, check the binds, start).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::docker::{AdoptPreview, AdoptRequest, AdoptWarning, AdoptedInfo, ImportPreviewRequest, ImportStackRequest, ManagerKind, MoveStackRequest, StackAdopted, valid_stack_name};

use super::stacks::launch;
use super::{done_toast, manager_name};
use crate::api;
use crate::form::{In, form_error, spinner, switch, text_field};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks::{self, tasks};
use crate::toast::toast;
use crate::ui::{self, Confirm, DlgKind, Tone, kv_row};

fn url(name: &str, rest: &str) -> String {
    format!("/api/v1/docker/stacks/{}{rest}", api::encode(name))
}

fn origin_name(o: Option<ManagerKind>) -> &'static str {
    o.map(manager_name).unwrap_or("—")
}

/// Profiles typed as `a, b c`.
fn split_list(s: &str) -> Vec<String> {
    s.split([',', ' ']).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()
}

fn warning_text(w: AdoptWarning, p: &AdoptPreview) -> (Tone, String) {
    match w {
        AdoptWarning::OtherManager => (Tone::Info, tf("dk.ad.w_other", &[("manager", origin_name(p.origin))])),
        AdoptWarning::Portainer => (Tone::Warn, t("dk.ad.w_portainer").to_string()),
        AdoptWarning::NameDiffers => (Tone::Warn, tf("dk.ad.w_name", &[("file", p.name_in_file.as_deref().unwrap_or("")), ("name", &p.name)])),
        AdoptWarning::NameInUse => (Tone::Err, tf("dk.ad.w_in_use", &[("name", &p.name)])),
        AdoptWarning::ExternalNetworks => (Tone::Info, tf("dk.ad.w_networks", &[("networks", &p.external_networks.join(", "))])),
        AdoptWarning::Profiles => (Tone::Info, tf("dk.ad.w_profiles", &[("profiles", &p.profiles_found.join(", "))])),
        AdoptWarning::Include => (Tone::Info, t("dk.ad.w_include").to_string()),
        AdoptWarning::OutsideDir => (Tone::Info, t("dk.ad.w_outside").to_string()),
        AdoptWarning::MissingFile => (Tone::Err, t("dk.ad.w_missing").to_string()),
    }
}

/// What will be recorded, and why it deserves a look.
fn preview_view(p: &AdoptPreview) -> AnyView {
    let env = if p.env_files.is_empty() { t("dk.ad.env_default").to_string() } else { p.env_files.join(", ") };
    let notes = p
        .warnings
        .iter()
        .map(|w| {
            let (tone, text) = warning_text(*w, p);
            let icon = if tone == Tone::Info { icons::INFO } else { icons::TRIANGLE_ALERT };
            ui::note(tone, icon, ui::text(text))
        })
        .collect_view();
    view! {
        <dl class="kvs">
            {kv_row("dk.s.folder", Signal::stored(p.dir.clone()), "mono")}
            {kv_row("dk.s.files", Signal::stored(if p.files.is_empty() { "—".to_string() } else { p.files.join(", ") }), "mono")}
            {kv_row("dk.ad.env", Signal::stored(env), "mono")}
        </dl>
        {notes}
    }
    .into_any()
}

/// After an adoption or import: lists refreshed, a toast, the start task followed.
async fn adopted_done(r: StackAdopted, close: Callback<()>) {
    toast(Tone::Ok, tf("dk.ad.done", &[("name", &r.name)]));
    crate::docker::docker().on_changed();
    close.run(());
    if let Some(id) = r.task_id {
        tasks().show(&id);
        done_toast(tasks::follow(&id).await);
    }
}

/// "Adopt" a running project: its labels say where its files are.
pub(super) fn adopt_dialog(name: String, close: Callback<()>) -> AnyView {
    let preview = RwSignal::new(None::<AdoptPreview>);
    let load_err = RwSignal::new(None::<String>);
    let dir = RwSignal::new(String::new());
    let profiles = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let dir_err = RwSignal::new(None::<String>);
    let n = StoredValue::new(name.clone());
    let load = move || {
        spawn_local(async move {
            match api::get::<AdoptPreview>(&url(&n.get_value(), "/adopt")).await {
                Ok(p) => {
                    let _ = dir.try_set(p.dir.clone());
                    let _ = preview.try_set(Some(p));
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => {
                    let _ = load_err.try_set(Some(e.code().to_string()));
                }
            }
        });
    };
    load();
    let go = move |_| {
        let Some(p) = preview.get_untracked() else { return };
        busy.set(true);
        error.set(None);
        dir_err.set(None);
        let d = dir.get_untracked().trim().to_string();
        let body = AdoptRequest { dir: (d != p.dir && !d.is_empty()).then_some(d), profiles: split_list(&profiles.get_untracked()), ..Default::default() };
        spawn_local(async move {
            let r = api::post::<_, StackAdopted>(&url(&n.get_value(), "/adopt"), Some(&body)).await;
            let _ = busy.try_set(false);
            match r {
                Ok(r) => adopted_done(r, close).await,
                Err(e) if e.field() == Some("dir") => {
                    let _ = dir_err.try_set(Some(e.text()));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let body = move || {
        let Some(p) = preview.get() else { return ui::loading(load_err.into(), Callback::new(move |_| load()), 4) };
        let missing = p.warnings.contains(&AdoptWarning::MissingFile);
        let has_profiles = !p.profiles_found.is_empty();
        view! {
            <p class="dialog-text">{t(if p.read_only { "dk.ad.text_ro" } else { "dk.ad.text" })}</p>
            {preview_view(&p)}
            <div class="form">
                {missing.then(|| text_field("dk.ad.dir", dir.into(), Callback::new(move |v: String| dir.set(v)), dir_err.into(), In { mono: true, help: Some("dk.ad.dir_help"), maxlength: 1024, ..In::default() }))}
                {has_profiles.then(|| text_field("dk.ad.profiles", profiles.into(), Callback::new(move |v: String| profiles.set(v)), Signal::stored(None), In { mono: true, help: Some("dk.ad.profiles_help"), maxlength: 300, ..In::default() }))}
            </div>
        }
        .into_any()
    };
    let ready = move || preview.with(|p| p.as_ref().is_some_and(|p| !p.warnings.contains(&AdoptWarning::MissingFile) || !dir.with(String::is_empty)));
    let foot = view! {
        {form_error(error)}
        <button type="button" class="btn ghost" data-autofocus="" disabled=move || busy.get() on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || busy.get() || !ready() on:click=go>{spinner(busy)}<Icon icon=icons::IMPORT size=16 />{tr("dk.ad.adopt")}</button>
    }
    .into_any();
    ui::dialog(DlgKind::Normal, Signal::stored(tf("dk.ad.title", &[("name", &name)])), close, view! { {body} }.into_any(), Some(foot))
}

/// "Import folder": a folder with compose files (restored, never started) registered where
/// it is, started on request.
pub(super) fn import_dialog(close: Callback<()>) -> AnyView {
    let dir = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let profiles = RwSignal::new(String::new());
    let start = RwSignal::new(false);
    let preview = RwSignal::new(None::<AdoptPreview>);
    let checking = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let dir_err = RwSignal::new(None::<String>);
    let name_err = RwSignal::new(None::<String>);
    let check = move || {
        checking.set(true);
        error.set(None);
        dir_err.set(None);
        let body = ImportPreviewRequest { dir: dir.get_untracked().trim().to_string() };
        spawn_local(async move {
            let r = api::post::<_, AdoptPreview>("/api/v1/docker/stacks/import/preview", Some(&body)).await;
            let _ = checking.try_set(false);
            match r {
                Ok(p) => {
                    let _ = name.try_set(p.name.clone());
                    let _ = preview.try_set(Some(p));
                }
                Err(e) if e.field() == Some("dir") => {
                    let _ = dir_err.try_set(Some(e.text()));
                    crate::ui::focus_invalid();
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let go = move |_| {
        let Some(p) = preview.get_untracked() else { return };
        busy.set(true);
        error.set(None);
        name_err.set(None);
        let body = ImportStackRequest { dir: p.dir.clone(), name: Some(name.get_untracked()), profiles: split_list(&profiles.get_untracked()), start: start.get_untracked(), ..Default::default() };
        spawn_local(async move {
            let r = api::post::<_, StackAdopted>("/api/v1/docker/stacks/import", Some(&body)).await;
            let _ = busy.try_set(false);
            match r {
                Ok(r) => adopted_done(r, close).await,
                Err(e) if e.field() == Some("name") => {
                    let _ = name_err.try_set(Some(e.text()));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let name_bad = Signal::derive(move || name_err.get().or_else(|| (!name.with(|v| v.is_empty() || valid_stack_name(v))).then(|| t("dk.ed.name_invalid").to_string())));
    let found = move || {
        preview.get().map(|p| {
            let has_profiles = !p.profiles_found.is_empty();
            view! {
                {preview_view(&p)}
                <div class="form">
                    {text_field("dk.ed.name", name.into(), Callback::new(move |v: String| { name.set(v.trim().to_lowercase()); name_err.set(None); }), name_bad, In { mono: true, help: Some("dk.im.name_help"), maxlength: 63, ..In::default() })}
                    {has_profiles.then(|| text_field("dk.ad.profiles", profiles.into(), Callback::new(move |v: String| profiles.set(v)), Signal::stored(None), In { mono: true, help: Some("dk.ad.profiles_help"), maxlength: 300, ..In::default() }))}
                    {(!p.read_only).then(|| switch("dk.ad.start_import", None, start.into(), Callback::new(move |v| start.set(v))))}
                </div>
            }
        })
    };
    let body = view! {
        <p class="dialog-text">{tr("dk.im.text")}</p>
        <div class="form">
            {text_field("dk.ad.dir", dir.into(), Callback::new(move |v: String| { dir.set(v); preview.set(None); dir_err.set(None); }), dir_err.into(), In { mono: true, help: Some("dk.im.dir_help"), maxlength: 1024, ..In::default() })}
        </div>
        {found}
    }
    .into_any();
    let can_import = move || !busy.get() && preview.with(Option::is_some) && name.with(|v| valid_stack_name(v));
    let foot = view! {
        {form_error(error)}
        <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn ghost" disabled=move || checking.get() || dir.with(|d| d.trim().is_empty()) on:click=move |_| check()>{spinner(checking)}<Icon icon=icons::SEARCH size=16 />{tr("dk.im.check")}</button>
        <button type="button" class="btn primary" disabled=move || !can_import() on:click=go>{spinner(busy)}<Icon icon=icons::IMPORT size=16 />{tr("dk.im.import")}</button>
    }
    .into_any();
    ui::dialog(DlgKind::Normal, Signal::stored(t("dk.im.title").to_string()), close, body, Some(foot))
}

/// The adoption facts of the detail drawer: chips and notes.
pub(super) fn adopted_notes(a: &AdoptedInfo) -> AnyView {
    view! {
        {a.changed_outside.then(|| ui::note(Tone::Warn, icons::FILE_WARNING, ui::text(t("dk.ad.changed").to_string())))}
        {if a.read_only {
            Some(ui::note(Tone::Info, icons::INFO, ui::text(t("dk.ad.ro_note").to_string())))
        } else {
            a.origin.map(|o| ui::note(Tone::Info, icons::INFO, ui::text(tf("dk.ad.w_other", &[("manager", manager_name(o))]))))
        }}
    }
    .into_any()
}

/// "Release": the record goes, files and containers stay.
pub(super) fn release_dialog(name: String, cancel: Callback<()>, done: Callback<()>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let u = url(&name, "/release");
    let go = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        let u = u.clone();
        spawn_local(async move {
            let r = api::post_empty::<()>(&u, None).await;
            let _ = busy.try_set(false);
            match r {
                Ok(()) => {
                    toast(Tone::Ok, t("dk.ad.released").to_string());
                    crate::docker::docker().on_changed();
                    done.run(());
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    });
    ui::confirm(Confirm { busy, error, ..Confirm::new(tf("dk.ad.release_title", &[("name", &name)]), t("dk.ad.release_text").to_string(), "dk.ad.release", false, go, cancel) })
}

/// "Move to the stacks folder": a task, confirmed with the stack name.
pub(super) fn move_dialog(name: String, dir: String, target: Option<String>, cancel: Callback<()>) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let u = url(&name, "/move");
    let n2 = name.clone();
    let go = Callback::new(move |_| {
        let body = MoveStackRequest { confirm: n2.clone() };
        launch(u.clone(), serde_json::to_string(&body).ok(), busy, error, cancel);
    });
    let target = target.unwrap_or_else(|| "—".into());
    let text = tf("dk.ad.move_text", &[("from", &dir), ("to", &target)]);
    let typed = name.clone();
    // what the copy costs, from the mounted file systems: the target's free space and
    // whether both folders are on one btrfs file system (cp --reflink=auto shares the data
    // blocks there; anywhere else every byte is copied while the stack is stopped). The
    // folder's size is not known to the interface.
    let live = crate::live::use_live();
    let summary = move || {
        live.storage.with(|s| {
            let fss: Vec<&rynas_api_types::Filesystem> = s.iter().flat_map(|s| s.filesystems.iter()).collect();
            let fs_of = |p: &str| fss.iter().filter(|f| p == f.mountpoint || p.starts_with(&format!("{}/", f.mountpoint.trim_end_matches('/')))).max_by_key(|f| f.mountpoint.len()).copied();
            let (src, dst) = (fs_of(&dir), fs_of(&target));
            let reflink = matches!((src, dst), (Some(a), Some(b)) if a.mountpoint == b.mountpoint && a.fstype == "btrfs");
            let free = dst.map(|f| crate::i18n::bytes(f.avail)).unwrap_or_else(|| "—".into());
            view! {
                <dl class="kvs">
                    {kv_row("dk.ad.move_free", Signal::stored(free), "tnum")}
                    {kv_row("dk.ad.move_copy", Signal::stored(t(if reflink { "dk.ad.move_reflink" } else { "dk.ad.move_full" }).to_string()), "")}
                    {kv_row("dk.ad.move_down", Signal::stored(t(if reflink { "dk.ad.move_down_short" } else { "dk.ad.move_down_long" }).to_string()), "")}
                </dl>
            }
        })
    };
    let extra = view! { {summary} }.into_any();
    ui::confirm(Confirm { extra: Some(extra), typed: Signal::stored(Some(typed)), busy, error, ..Confirm::new(tf("dk.ad.move_title", &[("name", &name)]), text, "dk.ad.move", false, go, cancel) })
}
