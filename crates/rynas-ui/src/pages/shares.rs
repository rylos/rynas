//! Shared folders (DSM Control Panel › Shared Folder, TrueNAS Shares, OMV Shared folders): list
//! with location, volume usage and protocols; editor with general settings (new subvolume on a
//! volume or existing folder), SMB and NFS export settings, and the DSM-style permissions
//! matrix users/groups × (not set / read only / read-write / no access) with the server's
//! explained effective permission (`/shares/{name}/effective`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_query_map;
use rynas_api_types::{
    Access, EffectiveAccess, EffectivePermission, GroupView, NfsRule, NfsSquash, Permission, PrincipalKind, Share, ShareStatus,
    ShareSubvolume, ShareView, SmbPreset, UnsharedFolder, UserView, storage::VolumePlace,
};

use crate::api;
use crate::form::{In, area_field, lens, lens_bool, save_bar, search_box, select_field, switch, tabs, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::live::use_live;
use crate::pages::share_crypt;
use crate::pages::share_folders::{self, Target};
use crate::toast::{saved_applied, toast};
use crate::ui::{self, EmptyState, Meter, PageHeader, Remote, Tone, drawer, usage_tone};

/// `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`.
pub fn valid_share_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty() && b.len() <= 64 && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c))
}

/// Absolute, no `..` component, no `//`.
fn valid_path(p: &str) -> bool {
    p.starts_with('/') && !p.contains("//") && !p.split('/').any(|c| c == "..") && !p.contains(char::is_control)
}

/// Host, IP, CIDR, wildcard or `*`: no spaces, parentheses or commas (exports syntax).
fn valid_client(c: &str) -> bool {
    !c.is_empty() && !c.contains(|ch: char| ch.is_whitespace() || "(),\"".contains(ch))
}

/// Where the share lives: its path, or `/<volume>/<name>` for a subvolume still to create.
pub fn share_path(s: &Share) -> String {
    match (&s.subvolume, s.path.is_empty()) {
        (Some(sv), true) => format!("{}/{}", rynas_api_types::storage::volume_dir(&sv.volume), s.name),
        _ => s.path.clone(),
    }
}

/// The mounted filesystem holding `path` (longest mountpoint prefix): (mountpoint, used, size).
fn fs_for(path: &str, fss: &[rynas_api_types::Filesystem]) -> Option<(String, u64, u64)> {
    fss.iter()
        .filter(|f| path == f.mountpoint || path.starts_with(&format!("{}/", f.mountpoint.trim_end_matches('/'))))
        .max_by_key(|f| f.mountpoint.len())
        .map(|f| (f.mountpoint.clone(), f.used, f.size))
}

#[derive(Clone)]
struct ShareEdit {
    view: Option<ShareView>,
    tab: usize,
    /// A new share filled in beforehand (a folder shared again).
    prefill: Option<Share>,
}

#[component]
pub fn Shares() -> impl IntoView {
    let shares = Remote::<Vec<ShareView>>::with_config("/api/v1/shares");
    let users = Remote::<Vec<UserView>>::with_config("/api/v1/users");
    let groups = Remote::<Vec<GroupView>>::with_config("/api/v1/groups");
    let folders = Remote::<Vec<UnsharedFolder>>::with_config("/api/v1/folders");
    let live = use_live();
    let q = RwSignal::new(String::new());
    let edit = RwSignal::new(None::<ShareEdit>);
    let del = RwSignal::new(None::<String>);
    let fdel = RwSignal::new(None::<UnsharedFolder>);
    let cact = RwSignal::new(None::<share_crypt::Action>);
    let create = move || edit.set(Some(ShareEdit { view: None, tab: 0, prefill: None }));
    let share_again = Callback::new(move |f: UnsharedFolder| edit.set(Some(ShareEdit { view: None, tab: 0, prefill: Some(share_folders::reshare(&f)) })));
    // `?edit=<share>&tab=<n>`: open that share's editor once the list has arrived (the file
    // manager's "Description…" and "Permissions…" link here)
    let query = use_query_map();
    let asked = StoredValue::new(false);
    Effect::new(move || {
        let Some(name) = query.with(|q| q.get("edit")) else { return };
        let tab = query.with(|q| q.get("tab")).and_then(|t| t.parse::<usize>().ok()).filter(|t| *t <= 3).unwrap_or(0);
        let found = shares.data.with(|d| d.iter().flatten().find(|v| v.share.name == name).cloned());
        if let Some(v) = found
            && !asked.get_value()
        {
            asked.set_value(true);
            edit.set(Some(ShareEdit { view: Some(v), tab, prefill: None }));
        }
    });

    let table = move || {
        if let Some(v) = shares.gate(5) {
            return v;
        }
        let needle = q.get().to_lowercase();
        let rows: Vec<ShareView> = shares.data.with(|d| {
            d.iter()
                .flatten()
                .filter(|s| needle.is_empty() || s.share.name.to_lowercase().contains(&needle) || s.share.description.to_lowercase().contains(&needle))
                .cloned()
                .collect()
        });
        if rows.is_empty() {
            return if needle.is_empty() {
                view! {
                    <EmptyState icon=icons::SHARE_2 title=Signal::derive(|| t("shares.empty").to_string()) text=Signal::derive(|| t("shares.empty_text").to_string())>
                        <button class="btn primary" on:click=move |_| create()><Icon icon=icons::FOLDER_PLUS size=16 />{tr("shares.create")}</button>
                    </EmptyState>
                }
                .into_any()
            } else {
                view! { <p class="muted table-none">{tr("table.no_match")}</p> }.into_any()
            };
        }
        let fss = live.storage.with(|s| s.as_ref().map(|s| s.filesystems.clone()).unwrap_or_default());
        view! {
            {ui::table("shares-table", &["shares.name", "shares.location", "shares.volume_usage", "shares.protocols", "shares.recycle", "col.status", ""], rows.into_iter().map(|v| share_row(v, &fss, edit, del, cact)).collect::<Vec<_>>())}
        }
        .into_any()
    };

    let editor = move || edit.get().map(|e| share_editor(e, users, groups, Callback::new(move |_| edit.set(None))));
    let deleter = move || {
        del.get()
            .map(|name| share_folders::delete_dialog(Target::Share(name), Callback::new(move |_| del.set(None)), Callback::new(move |_| ())))
    };
    let folder_deleter = move || {
        fdel.get().map(|f| {
            share_folders::delete_dialog(
                Target::Folder(Box::new(f)),
                Callback::new(move |_| fdel.set(None)),
                Callback::new(move |_| {
                    folders.reload();
                    shares.reload();
                }),
            )
        })
    };

    view! {
        <div class="page">
            <PageHeader icon=icons::SHARE_2 title="nav.shares" subtitle="shares.subtitle" />
            <section class="card widget w-12">
                <div class="toolbar">
                    {search_box("shares.search", q)}
                    <button class="btn primary" on:click=move |_| create()>
                        <Icon icon=icons::FOLDER_PLUS size=16 /><span class="btn-label">{tr("shares.create")}</span>
                    </button>
                </div>
                {table}
            </section>
            {share_folders::unshared_section(folders, share_again, fdel)}
            {editor}
            {deleter}
            {folder_deleter}
            {move || cact.get().map(|a| share_crypt::action_dialog(a, Callback::new(move |_| cact.set(None)), Callback::new(move |_| shares.reload())))}
        </div>
    }
}

fn share_status(st: &ShareStatus) -> (Tone, &'static str) {
    if !st.path_ok {
        if st.pending == Some(rynas_api_types::PendingOp::Add) { (Tone::Info, "shares.st_on_apply") } else { (Tone::Err, "shares.st_missing") }
    } else if !st.warnings.is_empty() {
        (Tone::Warn, "shares.st_warnings")
    } else {
        (Tone::Ok, "shares.st_ready")
    }
}

fn share_row(v: ShareView, fss: &[rynas_api_types::Filesystem], edit: RwSignal<Option<ShareEdit>>, del: RwSignal<Option<String>>, cact: RwSignal<Option<share_crypt::Action>>) -> AnyView {
    let s = &v.share;
    let path = share_path(s);
    let usage = fs_for(&path, fss);
    let (tone, key) = share_status(&v.status);
    let deleting = v.status.pending == Some(rynas_api_types::PendingOp::Delete);
    let name = s.name.clone();
    let open = {
        let v = v.clone();
        move |tab: usize| edit.set(Some(ShareEdit { view: Some(v.clone()), tab, prefill: None }))
    };
    let (o1, o2, o3) = (open.clone(), open.clone(), open);
    let warnings = share_folders::warnings(&v.status).join("\n");
    let st_ok = tone == Tone::Ok;
    let crypt_btn = v.status.crypt.as_ref().filter(|_| !deleting).and_then(|c| {
        let n = s.name.clone();
        let (unlock, icon, label) = match c.state {
            rynas_api_types::CryptState::Locked => (true, icons::LOCK_OPEN, "crypt.unlock"),
            // still served to a container: a lock is tried again (and explains why it waits)
            rynas_api_types::CryptState::Unlocked | rynas_api_types::CryptState::InUse => (false, icons::KEY, "crypt.lock"),
            rynas_api_types::CryptState::Missing => return None,
        };
        let aria = format!("{} {}", t(label), s.name);
        Some(view! {
            <button class="icon-btn sm" title=tr(label) aria-label=aria
                on:click=move |_| cact.set(Some(if unlock { share_crypt::Action::Unlock(n.clone()) } else { share_crypt::Action::Lock(n.clone()) }))>
                <Icon icon=icon size=16 />
            </button>
        })
    });
    let recycle = if s.smb.enabled && s.smb.recycle.enabled {
        let d = s.smb.recycle.retention_days;
        if d == 0 { t("shares.recycle_keep").to_string() } else { tf("shares.recycle_days", &[("n", &d.to_string())]) }
    } else {
        t("off").to_string()
    };
    ui::row(vec![
        ui::cell("shares.name", "", view! {
            <div class="cell-main">
                <button class="link-strong" disabled=deleting on:click=move |_| o1(0)>{s.name.clone()}</button>
                <span class="cell-sub">{s.description.clone()}</span>
            </div>
        }
        .into_any()),
        ui::cell("shares.location", "path-cell", view! {
            <div class="cell-main">
                <span class="mono">{path}</span>
                <span class="cell-sub">
                    {(v.status.subvolume || s.subvolume.is_some()).then(|| t("shares.subvolume"))}
                    {s.encrypted.then(|| t("crypt.encrypted"))}
                    {(!v.status.fs_type.is_empty() && !s.encrypted).then(|| format!(" · {}", v.status.fs_type))}
                    {(v.status.nocow == Some(true)).then(|| format!(" · {}", t("shares.nocow")))}
                </span>
            </div>
        }
        .into_any()),
        ui::cell("shares.volume_usage", "", view! {
            {match usage {
                Some((mp, used, size)) if size > 0 => {
                    let pct = used as f64 * 100.0 / size as f64;
                    view! {
                        <div class="usage-cell" title=format!("{mp}: {} / {}", i18n::bytes(used), i18n::bytes(size))>
                            <Meter pct=Signal::stored(pct) tone=Signal::stored(usage_tone(pct)) />
                            <span class="tnum small">{i18n::pct(pct)}</span>
                        </div>
                        <span class="cell-sub mono">{mp}</span>
                    }.into_any()
                }
                _ => view! { <span class="muted">"—"</span> }.into_any(),
            }}
        }
        .into_any()),
        ui::cell("shares.protocols", "", view! {
            <div class="chips">
                {s.smb.enabled.then(|| view! { <span class="chip accent">"SMB"</span> })}
                {s.nfs.enabled.then(|| view! { <span class="chip accent">"NFS"</span> })}
                {(s.smb.enabled && s.smb.preset == SmbPreset::Timemachine).then(|| view! { <span class="chip">"Time Machine"</span> })}
                {(!s.smb.enabled && !s.nfs.enabled).then(|| view! { <span class="muted small">{tr("shares.no_protocol")}</span> })}
            </div>
        }
        .into_any()),
        ui::cell("shares.recycle", "muted", view! { {recycle} }.into_any()),
        ui::cell("col.status", "", view! {
            <div class="chips" title=warnings>
                {match &v.status.crypt {
                    // an encrypted folder: its lock state says more than the remarks
                    Some(c) if c.state != rynas_api_types::CryptState::Unlocked || st_ok => share_crypt::state_chip(c),
                    _ => ui::status(tone, t(key).to_string()),
                }}
                {ui::pending_status(v.status.pending)}
            </div>
        }.into_any()),
        ui::actions(view! {
            {crypt_btn}
            <button class="icon-btn sm" title=tr("edit") aria-label=format!("{} {}", t("edit"), s.name) disabled=deleting on:click=move |_| o2(0)>
                <Icon icon=icons::PENCIL size=16 />
            </button>
            <button class="icon-btn sm" title=tr("shares.permissions") aria-label=format!("{} {}", t("shares.permissions"), s.name) disabled=deleting on:click=move |_| o3(3)>
                <Icon icon=icons::LOCK size=16 />
            </button>
            <button class="icon-btn sm danger" title=tr("delete") aria-label=format!("{} {}", t("delete"), s.name) disabled=deleting on:click=move |_| del.set(Some(name.clone()))>
                <Icon icon=icons::TRASH_2 size=16 />
            </button>
        }
        .into_any()),
    ])
}

const PRESETS: &[(&str, &str)] =
    &[("standard", "shares.preset_standard"), ("multiprotocol", "shares.preset_multi"), ("timemachine", "shares.preset_tm"), ("hugefolders", "shares.preset_huge")];
const ACCESS_OPTS: &[(&str, &str)] = &[("rw", "perm.rw"), ("ro", "perm.ro")];
const SQUASH_OPTS: &[(&str, &str)] = &[("root", "nfs.squash_root"), ("all", "nfs.squash_all"), ("none", "nfs.squash_none")];

fn preset_str(p: SmbPreset) -> String {
    match p {
        SmbPreset::Standard => "standard",
        SmbPreset::Multiprotocol => "multiprotocol",
        SmbPreset::Timemachine => "timemachine",
        SmbPreset::Hugefolders => "hugefolders",
    }
    .into()
}

fn parse_u32(v: &str) -> u32 {
    v.trim().parse().unwrap_or(0)
}

fn share_editor(e: ShareEdit, users: Remote<Vec<UserView>>, groups: Remote<Vec<GroupView>>, close: Callback<()>) -> AnyView {
    let is_new = e.view.is_none();
    let saved = e.view.as_ref().map(|v| v.share.clone());
    let warnings = e.view.as_ref().map(|v| share_folders::warnings(&v.status)).unwrap_or_default();
    let nocow = e.view.as_ref().and_then(|v| super::share_nocow::existing(v.share.name.clone(), &v.status));
    let crypt_existing = e.view.as_ref().and_then(share_crypt::existing);
    let d = RwSignal::new(saved.clone().or(e.prefill).unwrap_or_else(|| Share {
        subvolume: Some(ShareSubvolume { volume: String::new(), compression: true, nocow: false }),
        ..Share::default()
    }));
    let tab = RwSignal::new(e.tab);
    let show = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // a new encrypted folder: its form, and its recovery key once it exists
    let cf = share_crypt::CryptForm::new();
    let key = RwSignal::new(None::<(String, String)>);
    on_cleanup(move || {
        let _ = key.try_set(None);
    });
    let encrypted = Signal::derive(move || d.with(|s| s.encrypted) || (is_new && cf.on.get() && d.with(|s| s.subvolume.is_some())));

    let (name, set_name) = lens(d, |s| s.name.clone(), |s, v| s.name = v.trim().to_string());
    let name_err = Signal::derive(move || {
        let v = name.get();
        ((show.get() || !v.is_empty()) && !valid_share_name(&v)).then(|| t("v.share_name").to_string())
    });
    let save = Callback::new(move |_| {
        show.set(true);
        let crypt_new = is_new && encrypted.get_untracked();
        let problem = d.with_untracked(|s| {
            let general_bad = (is_new && !valid_share_name(&s.name))
                || (s.subvolume.as_ref().is_some_and(|v| v.volume.is_empty()) && s.path.is_empty())
                || (s.subvolume.is_none() && !valid_path(&s.path))
                || (crypt_new && cf.problem().is_some());
            if general_bad {
                Some(0)
            } else if s.nfs.enabled && s.nfs.rules.iter().any(|r| !valid_client(&r.client)) {
                Some(2)
            } else {
                None
            }
        });
        if let Some(t_) = problem {
            tab.set(t_);
            error.set(Some(t("form.check").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let mut body = d.get_untracked();
        spawn_local(async move {
            // the encrypted folder first (task), its recovery key fetched at once and shown
            // at the end whatever happens next; then the share on its mount point
            let mut new_key = None;
            if crypt_new {
                let volume = body.subvolume.as_ref().map(|v| v.volume.clone()).unwrap_or_default();
                match share_crypt::create_folder(&volume, &body.name, cf).await {
                    Ok(Ok(k)) => new_key = Some((body.name.clone(), k)),
                    // the folder exists: the share goes on, the key is shown later
                    Ok(Err(e)) => toast(Tone::Warn, format!("{}{}{e}", t("crypt.rk_later"), api::DETAIL_SEP)),
                    Err(e) => {
                        let _ = error.try_set(Some(e));
                        let _ = busy.try_set(false);
                        return;
                    }
                }
                body.path = share_path(&body);
                body.subvolume = None;
                body.encrypted = true;
                body.show_snapshots = false;
            }
            let r = if is_new {
                api::post::<_, ShareView>("/api/v1/shares", Some(&body)).await
            } else {
                api::put::<_, ShareView>(&format!("/api/v1/shares/{}", api::encode(&body.name)), &body).await
            };
            let mut done = false;
            match r {
                Ok(_) => {
                    if crate::config::apply_now(&[crate::config::keys::SHARES], error).await {
                        saved_applied();
                        done = true;
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            let _ = busy.try_set(false);
            if new_key.is_some() {
                // the recovery key dialog closes the editor when everything went well
                let _ = key.try_set(new_key);
            } else if done {
                close.run(());
            }
        });
    });

    let title = {
        let n = saved.as_ref().map(|s| s.name.clone()).unwrap_or_default();
        Signal::derive(move || if is_new { t("shares.create").to_string() } else { tf("shares.edit_title", &[("name", &n)]) })
    };

    let general = view! {
        {is_new.then(|| text_field("shares.name", name, set_name, name_err, In { help: Some("shares.name_help"), maxlength: 64, ..In::default() }))}
        {let (v, s) = lens(d, |s| s.description.clone(), |s, v| s.description = v);
         text_field("users.description", v, s, Signal::derive(|| None), In::default())}
        {location_fields(d, is_new, show, cf)}
        {crypt_existing}
        {nocow}
        {let (v, s) = lens_bool(d, |s| s.show_snapshots, |s, v| s.show_snapshots = v);
         view! { <div hidden=move || encrypted.get()>{switch("shares.show_snapshots", Some("shares.show_snapshots_help"), v, s)}</div> }}
        {(!warnings.is_empty()).then(|| view! {

            {ui::note(Tone::Warn, icons::TRIANGLE_ALERT, view! { <ul class="plain-list">{warnings.into_iter().map(|w| view! { <li>{w}</li> }).collect_view()}</ul> }.into_any())}
        })}
    }
    .into_any();

    let saved_perms = saved.as_ref().map(|s| s.permissions.clone());
    let share_name = saved.as_ref().map(|s| s.name.clone());
    let body = view! {
        {tabs("sh", &["shares.tab_general", "shares.tab_smb", "shares.tab_nfs", "shares.permissions"], tab, "shares.editor")}
        <form class="form tab-panels" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            <div role="tabpanel" id="sh-p0" aria-labelledby="sh-t0" class="form" hidden=move || tab.get() != 0>{general}</div>
            <div role="tabpanel" id="sh-p1" aria-labelledby="sh-t1" class="form" hidden=move || tab.get() != 1>{smb_fields(d)}</div>
            <div role="tabpanel" id="sh-p2" aria-labelledby="sh-t2" class="form" hidden=move || tab.get() != 2>{nfs_fields(d, show)}</div>
            <div role="tabpanel" id="sh-p3" aria-labelledby="sh-t3" class="form" hidden=move || tab.get() != 3>
                {permissions_matrix(d, users, groups)}
                {effective_view(d, share_name, saved_perms, users, groups)}
            </div>
            <button type="submit" hidden=true></button>
        </form>
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if is_new { "create" } else { "save" }, close, save, error);
    let reveal = move || {
        key.get().map(|(n, k)| view! { <share_crypt::RecoveryDialog name=n key=k on_done=Callback::new(move |_| {
            key.set(None);
            // a failed share or apply stays on screen with its error
            if error.with_untracked(Option::is_none) {
                close.run(());
            }
        }) /> })
    };
    view! { <div class="wide-drawer">{drawer(title, close, body, foot)}</div>{reveal} }.into_any()
}

/// New subvolume on a rynas volume, or an existing folder.
fn location_fields(d: RwSignal<Share>, is_new: bool, show: RwSignal<bool>, cf: share_crypt::CryptForm) -> AnyView {
    let (path, set_path) = lens(d, |s| s.path.clone(), |s, v| s.path = v.trim().to_string());
    let path_err = Signal::derive(move || {
        let (sub, p) = d.with(|s| (s.subvolume.is_some(), s.path.clone()));
        (!sub && (show.get() || !p.is_empty()) && !valid_path(&p)).then(|| t("v.path").to_string())
    });
    // an encrypted folder stays where it was made
    if !is_new && d.with_untracked(|s| s.encrypted) {
        return view! {
            <dl class="kvs">
                {crate::ui::kv_row("shares.location", Signal::derive(move || d.with(|s| s.path.clone())), "mono")}
            </dl>
        }
        .into_any();
    }
    if !is_new {
        let sv = d.with_untracked(|s| s.subvolume.clone());
        return match sv {
            Some(sv) => view! {
                <dl class="kvs">
                    {crate::ui::kv_row("shares.location", Signal::derive(move || share_path(&d.get())), "mono")}
                    {crate::ui::kv_row("shares.volume", Signal::stored(crate::places::name(&sv.volume)), "mono")}
                </dl>
            }
            .into_any(),
            None => text_field("shares.path", path, set_path, path_err, In { help: Some("shares.path_edit_help"), mono: true, maxlength: 4096, ..In::default() }),
        };
    }
    // the rynas volumes and the system volume (D-ROOTVOL)
    let places = crate::ui::Remote::<Vec<VolumePlace>>::new(crate::places::PATH);
    let use_sub = Signal::derive(move || d.with(|s| s.subvolume.is_some()));
    // the usable ones (the first is the default choice)
    let volumes = move || places.data.with(|p| p.as_ref().map(|p| p.iter().filter(|v| v.unavailable.is_none()).map(|v| (v.label.clone(), v.avail_bytes.unwrap_or(0))).collect::<Vec<_>>()).unwrap_or_default());
    let chosen = move || d.with(|s| s.subvolume.as_ref().map(|sv| sv.volume.clone())).and_then(|l| places.data.with(|p| p.as_ref().and_then(|p| p.iter().find(|v| v.label == l).cloned())));
    // back to "New subvolume": start again on the first volume (the Effect below only runs
    // when the volume list changes, so it would leave the volume empty here)
    let set_mode = move |sub: bool| {
        if sub == d.with_untracked(|s| s.subvolume.is_some()) {
            return;
        }
        let first = sub.then(|| volumes().into_iter().next().map(|v| v.0)).flatten().unwrap_or_default();
        d.update(|s| {
            s.subvolume = sub.then_some(ShareSubvolume { volume: first, compression: true, nocow: false });
            if sub {
                s.path.clear();
            }
        })
    };
    // pick the first volume once the list arrives
    Effect::new(move |_| {
        let v = volumes();
        if let Some((first, _)) = v.first()
            && d.with_untracked(|s| s.subvolume.as_ref().is_some_and(|sv| sv.volume.is_empty()))
        {
            let first = first.clone();
            d.update(|s| {
                if let Some(sv) = s.subvolume.as_mut() {
                    sv.volume = first;
                }
            });
        }
    });
    let (comp, set_comp) = lens_bool(d, |s| s.subvolume.as_ref().is_none_or(|v| v.compression), |s, v| {
        if let Some(sv) = s.subvolume.as_mut() {
            sv.compression = v
        }
    });
    view! {
        <fieldset class="fieldset">
            <legend class="field-label">{tr("shares.location")}</legend>
            <div class="segmented" role="radiogroup" aria-label=tr("shares.location")>
                <button type="button" role="radio" aria-checked=move || use_sub.get().to_string() class:on=move || use_sub.get() on:click=move |_| set_mode(true)>{tr("shares.loc_subvolume")}</button>
                <button type="button" role="radio" aria-checked=move || (!use_sub.get()).to_string() class:on=move || !use_sub.get() on:click=move |_| set_mode(false)>{tr("shares.loc_folder")}</button>
            </div>
            {move || if use_sub.get() {
                let ready = places.data.with(Option::is_some);
                if !ready {
                    return crate::ui::loading(places.err.into(), Callback::new(move |_| places.reload()), 1);
                }
                let vols = places.data.get().unwrap_or_default();
                if vols.is_empty() {
                    return view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("shares.no_volumes")}</p> }.into_any();
                }
                let id = crate::form::fid();
                view! {
                    <div class="field">
                        <label class="field-label" for=id.clone()>{tr("shares.volume")}</label>
                        <select id=id class="input" on:change=move |ev| {
                            let v = event_target_value(&ev);
                            d.update(|s| if let Some(sv) = s.subvolume.as_mut() { sv.volume = v });
                        }>
                            {vols.iter().map(|p| {
                                let l2 = p.label.clone();
                                view! {
                                    <option value=p.label.clone() disabled=p.unavailable.is_some() selected=move || d.with(|s| s.subvolume.as_ref().is_some_and(|sv| sv.volume == l2))>
                                        {crate::places::option_text(p)}
                                    </option>
                                }
                            }).collect_view()}
                        </select>
                        <span class="field-help">{move || tf(if cf.on.get() { "crypt.created_help" } else { "shares.subvolume_help" }, &[("path", &share_path(&d.get()))])}</span>
                        {move || crate::places::note(chosen().as_ref(), true)}
                        {crate::places::unavailable_notes(&vols)}
                    </div>
                    {share_crypt::create_fields(cf, show)}
                    // NOCOW and compression are btrfs properties: not on a plain system disk,
                    // and nothing to gain on ciphertext
                    <div hidden=move || cf.on.get() || chosen().is_some_and(|p| !p.btrfs())>
                        {super::share_nocow::create_switch(d)}
                        <div hidden=move || d.with(|s| s.subvolume.as_ref().is_some_and(|v| v.nocow))>
                            {switch("shares.compression", Some("shares.compression_help"), comp, set_comp)}
                        </div>
                    </div>
                }.into_any()
            } else {
                text_field("shares.path", path, set_path, path_err, In { help: Some("shares.path_help"), mono: true, placeholder: "/srv/data", maxlength: 4096, ..In::default() })
            }}
        </fieldset>
    }
    .into_any()
}

fn smb_fields(d: RwSignal<Share>) -> AnyView {
    let (enabled, set_enabled) = lens_bool(d, |s| s.smb.enabled, |s, v| s.smb.enabled = v);
    let (preset, set_preset) = lens(d, |s| preset_str(s.smb.preset), |s, v| {
        s.smb.preset = match v.as_str() {
            "multiprotocol" => SmbPreset::Multiprotocol,
            "timemachine" => SmbPreset::Timemachine,
            "hugefolders" => SmbPreset::Hugefolders,
            _ => SmbPreset::Standard,
        }
    });
    let (tm, set_tm) = lens(d, |s| s.smb.timemachine_max_size.clone(), |s, v| s.smb.timemachine_max_size = v.trim().to_string());
    let (browse, set_browse) = lens_bool(d, |s| s.smb.browseable, |s, v| s.smb.browseable = v);
    let (rec, set_rec) = lens_bool(d, |s| s.smb.recycle.enabled, |s, v| s.smb.recycle.enabled = v);
    let (days, set_days) = lens(d, |s| s.smb.recycle.retention_days.to_string(), |s, v| s.smb.recycle.retention_days = parse_u32(&v));
    let (dots, set_dots) = lens_bool(d, |s| s.smb.hide_dot_files, |s, v| s.smb.hide_dot_files = v);
    let (encr, set_encr) = lens_bool(d, |s| s.smb.encryption_required, |s, v| s.smb.encryption_required = v);
    let (sc, set_sc) = lens_bool(d, |s| s.smb.shadow_copy.enabled, |s, v| s.smb.shadow_copy.enabled = v);
    let (sc_dir, set_sc_dir) = lens(d, |s| s.smb.shadow_copy.snapshot_dir.clone(), |s, v| s.smb.shadow_copy.snapshot_dir = v.trim().to_string());
    let (sc_pre, set_sc_pre) = lens(d, |s| s.smb.shadow_copy.prefix.clone(), |s, v| s.smb.shadow_copy.prefix = v.trim().to_string());
    let (veto, set_veto) = lens(d, |s| s.smb.veto_files.join("\n"), |s, v| {
        s.smb.veto_files = v.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
    });
    let (extra, set_extra) = lens(d, |s| s.smb.extra.clone(), |s, v| s.smb.extra = v);
    let none = Signal::derive(|| None::<String>);
    let sc_err = Signal::derive(move || {
        d.with(|s| (s.smb.shadow_copy.enabled && !s.smb.shadow_copy.snapshot_dir.is_empty() && !valid_path(&s.smb.shadow_copy.snapshot_dir)).then(|| t("v.path").to_string()))
    });
    view! {
        {switch("shares.smb_on", Some("shares.smb_on_help"), enabled, set_enabled)}
        <div class="form" hidden=move || !enabled.get()>
            {select_field("shares.preset", Some("shares.preset_help"), PRESETS, preset, set_preset)}
            <div hidden=move || preset.get() != "timemachine">
                {text_field("shares.tm_size", tm, set_tm, none, In { help: Some("shares.tm_size_help"), placeholder: "1T", mono: true, maxlength: 16, ..In::default() })}
            </div>
            {switch("shares.browseable", Some("shares.browseable_help"), browse, set_browse)}
            {switch("shares.recycle", Some("shares.recycle_on_help"), rec, set_rec)}
            <div hidden=move || !rec.get()>
                {text_field("shares.retention", days, set_days, none, In { kind: "number", help: Some("shares.retention_help"), maxlength: 5, ..In::default() })}
            </div>
            {switch("shares.hide_dot", None, dots, set_dots)}
            {switch("shares.encrypt", Some("smb.encrypt_help"), encr, set_encr)}
            {switch("shares.shadow", Some("shares.shadow_help"), sc, set_sc)}
            <div class="form" hidden=move || !sc.get()>
                {text_field("snap.f.dir", sc_dir, set_sc_dir, sc_err, In { placeholder: "/vol1/.snapshots", mono: true, maxlength: 4096, ..In::default() })}
                {text_field("shares.snap_prefix", sc_pre, set_sc_pre, none, In { help: Some("shares.snap_prefix_help"), mono: true, maxlength: 64, ..In::default() })}
            </div>
            <details class="advanced">
                <summary>{tr("advanced")}</summary>
                <div class="form">
                    {area_field("shares.veto", veto, set_veto, Some("shares.veto_help"), "")}
                    {area_field("shares.extra", extra, set_extra, Some("smb.extra_help"), "")}
                </div>
            </details>
        </div>
    }
    .into_any()
}

fn nfs_fields(d: RwSignal<Share>, show: RwSignal<bool>) -> AnyView {
    let (enabled, set_enabled) = lens_bool(d, |s| s.nfs.enabled, |s, v| s.nfs.enabled = v);
    let (cross, set_cross) = lens_bool(d, |s| s.nfs.crossmnt, |s, v| s.nfs.crossmnt = v);
    let count = Memo::new(move |_| d.with(|s| s.nfs.rules.len()));
    let rule_row = move |i: usize| {
        let get = move |f: fn(&NfsRule) -> String| Signal::derive(move || d.with(|s| s.nfs.rules.get(i).map(f).unwrap_or_default()));
        let set = move |f: fn(&mut NfsRule, String)| {
            Callback::new(move |v: String| {
                d.update(|s| {
                    if let Some(r) = s.nfs.rules.get_mut(i) {
                        f(r, v)
                    }
                })
            })
        };
        let client = get(|r| r.client.clone());
        let client_err = Signal::derive(move || {
            let c = client.get();
            ((show.get() || !c.is_empty()) && !valid_client(&c)).then(|| t("v.client").to_string())
        });
        view! {
            <div class="rule">
                {text_field("nfs.client", client, set(|r, v| r.client = v.trim().to_string()), client_err, In { placeholder: "192.0.2.0/24", mono: true, help: Some("nfs.client_help"), ..In::default() })}
                {select_field("nfs.access", None, ACCESS_OPTS, get(|r| if r.read_only { "ro".into() } else { "rw".into() }), set(|r, v| r.read_only = v == "ro"))}
                {select_field("nfs.squash", Some("nfs.squash_help"), SQUASH_OPTS, get(|r| match r.squash { NfsSquash::Root => "root".into(), NfsSquash::All => "all".into(), NfsSquash::None => "none".into() }), set(|r, v| r.squash = match v.as_str() { "all" => NfsSquash::All, "none" => NfsSquash::None, _ => NfsSquash::Root }))}
                {text_field("nfs.anonuid", get(|r| r.anonuid.to_string()), set(|r, v| r.anonuid = v.trim().parse().unwrap_or(65534)), Signal::derive(|| None), In { kind: "number", maxlength: 10, ..In::default() })}
                {text_field("nfs.anongid", get(|r| r.anongid.to_string()), set(|r, v| r.anongid = v.trim().parse().unwrap_or(65534)), Signal::derive(|| None), In { kind: "number", maxlength: 10, ..In::default() })}
                {switch("nfs.sync", Some("nfs.sync_help"), Signal::derive(move || d.with(|s| s.nfs.rules.get(i).is_none_or(|r| r.sync))), Callback::new(move |v| d.update(|s| if let Some(r) = s.nfs.rules.get_mut(i) { r.sync = v })))}
                <button type="button" class="btn ghost sm rule-remove" on:click=move |_| d.update(|s| { if i < s.nfs.rules.len() { s.nfs.rules.remove(i); } })>
                    <Icon icon=icons::TRASH_2 size=14 />{tr("nfs.remove_rule")}
                </button>
            </div>
        }
        .into_any()
    };
    let fsid = move || d.with(|s| s.nfs.fsid.clone());
    view! {
        {switch("shares.nfs_on", Some("shares.nfs_on_help"), enabled, set_enabled)}
        <div class="form" hidden=move || !enabled.get()>
            {switch("nfs.crossmnt", Some("nfs.crossmnt_help"), cross, set_cross)}
            <fieldset class="fieldset">
                <legend class="field-label">{tr("nfs.rules")}</legend>
                {move || if count.get() == 0 {
                    view! { <p class="field-help">{tr("nfs.no_rules")}</p> }.into_any()
                } else {
                    (0..count.get()).map(rule_row).collect_view().into_any()
                }}
                <div>
                    <button type="button" class="btn ghost sm" on:click=move |_| d.update(|s| s.nfs.rules.push(NfsRule::default()))>
                        <Icon icon=icons::PLUS size=14 />{tr("add_rule")}
                    </button>
                </div>
            </fieldset>
            {move || (!fsid().is_empty()).then(|| view! { <p class="field-help">"fsid: "<span class="mono">{fsid()}</span></p> })}
        </div>
    }
    .into_any()
}

/// Columns of the matrix: not set, read only, read-write, no access.
const LEVELS: [(Option<Access>, &str); 4] = [(None, "not_set"), (Some(Access::Ro), "perm.ro"), (Some(Access::Rw), "perm.rw"), (Some(Access::Deny), "perm.deny")];

fn permissions_matrix(d: RwSignal<Share>, users: Remote<Vec<UserView>>, groups: Remote<Vec<GroupView>>) -> AnyView {
    let kind = RwSignal::new(PrincipalKind::User);
    let q = RwSignal::new(String::new());
    let set_access = move |k: PrincipalKind, name: String, a: Option<Access>| {
        d.update(|s| {
            s.permissions.retain(|p| !(p.kind == k && p.name == name));
            if let Some(access) = a {
                s.permissions.push(Permission { kind: k, name, access });
            }
        })
    };
    let rows = move || {
        let k = kind.get();
        let gate = match k {
            PrincipalKind::User => users.gate(3),
            PrincipalKind::Group => groups.gate(3),
        };
        if let Some(v) = gate {
            return v;
        }
        let needle = q.get().to_lowercase();
        let mut list: Vec<(String, String)> = match k {
            PrincipalKind::User => users.data.with(|u| u.iter().flatten().map(|u| (u.user.name.clone(), u.user.full_name.clone())).collect()),
            PrincipalKind::Group => groups.data.with(|g| g.iter().flatten().map(|g| (g.group.name.clone(), g.group.description.clone())).collect()),
        };
        // entries of the matrix whose principal is not listed (e.g. system groups) stay visible
        d.with_untracked(|s| {
            for p in s.permissions.iter().filter(|p| p.kind == k) {
                if !list.iter().any(|(n, _)| *n == p.name) {
                    list.push((p.name.clone(), String::new()));
                }
            }
        });
        list.retain(|(n, sub)| needle.is_empty() || n.contains(&needle) || sub.to_lowercase().contains(&needle));
        if list.is_empty() {
            return view! { <p class="muted table-none">{tr(if needle.is_empty() { "perm.nobody" } else { "table.no_match" })}</p> }.into_any();
        }
        let prefix = if k == PrincipalKind::User { "u" } else { "g" };
        view! {
            <div class="table-wrap">
                <table class="table matrix">
                    <thead>
                        <tr>
                            <th scope="col">{tr(if k == PrincipalKind::User { "perm.user" } else { "perm.group" })}</th>
                            {LEVELS.iter().map(|(_, key)| view! { <th scope="col" class="center">{tr(key)}</th> }).collect_view()}
                        </tr>
                    </thead>
                    <tbody>
                        {list.into_iter().map(|(name, sub)| {
                            let group_name = format!("perm-{prefix}-{name}");
                            let current = {
                                let n = name.clone();
                                Memo::new(move |_| d.with(|s| s.permissions.iter().find(|p| p.kind == k && p.name == n).map(|p| p.access)))
                            };
                            view! {
                                <tr>
                                    <th scope="row" class="principal">
                                        <span class="mono">{name.clone()}</span>
                                        {(!sub.is_empty()).then(|| view! { <span class="cell-sub">{sub}</span> })}
                                    </th>
                                    {LEVELS.iter().map(|(level, key)| {
                                        let level = *level;
                                        let n = name.clone();
                                        let label = format!("{}: {}", name, t(key));
                                        view! {
                                            <td class="center" data-label=tr(key)>
                                                <input
                                                    type="radio"
                                                    class=format!("perm-radio {}", match level { Some(Access::Deny) => "deny", Some(Access::Rw) => "rw", Some(Access::Ro) => "ro", None => "unset" })
                                                    name=group_name.clone()
                                                    aria-label=label
                                                    prop:checked=move || current.get() == level
                                                    on:change=move |_| set_access(k, n.clone(), level)
                                                />
                                            </td>
                                        }
                                    }).collect_view()}
                                </tr>
                            }
                        }).collect_view()}
                    </tbody>
                </table>
            </div>
        }
        .into_any()
    };
    let n_entries = move || d.with(|s| s.permissions.len());
    view! {
        <div class="toolbar">
            <div class="segmented" role="radiogroup" aria-label=tr("perm.show")>
                <button type="button" role="radio" aria-checked=move || (kind.get() == PrincipalKind::User).to_string() class:on=move || kind.get() == PrincipalKind::User on:click=move |_| kind.set(PrincipalKind::User)>{tr("users.tab_users")}</button>
                <button type="button" role="radio" aria-checked=move || (kind.get() == PrincipalKind::Group).to_string() class:on=move || kind.get() == PrincipalKind::Group on:click=move |_| kind.set(PrincipalKind::Group)>{tr("users.tab_groups")}</button>
            </div>
            {search_box("perm.search", q)}
        </div>
        <p class="field-help">{move || if n_entries() == 0 { t("perm.empty_help") } else { t("perm.rule_help") }}</p>
        {rows}
    }
    .into_any()
}

fn access_view(a: EffectiveAccess) -> (Tone, &'static str) {
    match a {
        EffectiveAccess::None => (Tone::Unknown, "eff.none"),
        EffectiveAccess::Ro => (Tone::Info, "perm.ro"),
        EffectiveAccess::Rw => (Tone::Ok, "perm.rw"),
        EffectiveAccess::Deny => (Tone::Err, "perm.deny"),
    }
}

fn access_key(a: Access) -> &'static str {
    match a {
        Access::Ro => "perm.ro",
        Access::Rw => "perm.rw",
        Access::Deny => "perm.deny",
    }
}

/// "Who gets what, and why": the server's effective permission for one user or group, with
/// the matrix entries that apply and the deciding one.
fn effective_view(
    d: RwSignal<Share>,
    share: Option<String>,
    saved: Option<Vec<Permission>>,
    users: Remote<Vec<UserView>>,
    groups: Remote<Vec<GroupView>>,
) -> AnyView {
    let Some(share) = share else {
        return view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("eff.save_first")}</p> }.into_any();
    };
    let who = RwSignal::new(String::new());
    let result = RwSignal::new(None::<Result<EffectivePermission, String>>);
    let busy = RwSignal::new(false);
    let dirty = move || d.with(|s| Some(&s.permissions) != saved.as_ref());
    let check = move |p: String| {
        if p.is_empty() {
            result.set(None);
            return;
        }
        busy.set(true);
        let share = share.clone();
        spawn_local(async move {
            let r = api::get::<EffectivePermission>(&format!("/api/v1/shares/{}/effective?principal={}", api::encode(&share), api::encode(&p))).await;
            busy.set(false);
            result.set(Some(r.map_err(|e| e.text())));
        });
    };
    let id = crate::form::fid();
    let options = move || {
        let mut v: Vec<(String, String)> = users.data.with(|u| u.iter().flatten().map(|u| (u.user.name.clone(), u.user.name.clone())).collect());
        groups.data.with(|g| v.extend(g.iter().flatten().map(|g| (format!("@{}", g.group.name), format!("@{} ({})", g.group.name, t("perm.group"))))));
        v
    };
    view! {
        <section class="effective">
            <h3 class="form-section"><Icon icon=icons::EYE size=16 />{tr("eff.title")}</h3>
            <p class="field-help">{tr("eff.help")}</p>
            <div class="field">
                <label class="field-label" for=id.clone()>{tr("eff.principal")}</label>
                <select id=id class="input" on:change=move |ev| { let v = event_target_value(&ev); who.set(v.clone()); check(v); }>
                    <option value="" selected=move || who.with(String::is_empty)>{tr("eff.choose")}</option>
                    {move || options().into_iter().map(|(v, label)| {
                        let v2 = v.clone();
                        view! { <option value=v selected=move || who.with(|w| *w == v2)>{label}</option> }
                    }).collect_view()}
                </select>
            </div>
            {move || dirty().then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("eff.unsaved")}</p> })}
            <div aria-live="polite">
                {move || if busy.get() {
                    Some(view! { <crate::ui::Skeleton lines=2 /> }.into_any())
                } else {
                    result.get().map(|r| match r {
                        Err(e) => view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 />{e}</p> }.into_any(),
                        Ok(p) => {
                            let (tone, key) = access_view(p.access);
                            view! {
                                <div class="eff-result">
                                    <div class="eff-head">
                                        <span class="mono">{p.principal.clone()}</span>
                                        {ui::status(tone, t(key).to_string())}
                                    </div>
                                    {if p.sources.is_empty() {
                                        view! { <p class="muted small">{tr("eff.no_source")}</p> }.into_any()
                                    } else {
                                        view! {
                                            <ul class="eff-sources">
                                                {p.sources.into_iter().map(|s| view! {
                                                    <li class:decisive=s.decisive>
                                                        <span>{t(if s.kind == PrincipalKind::User { "perm.user" } else { "perm.group" })}" "<strong class="mono">{s.name}</strong></span>
                                                        <span>{t(access_key(s.access))}</span>
                                                        {s.decisive.then(|| view! { <span class="chip accent">{tr("eff.decisive")}</span> })}
                                                    </li>
                                                }).collect_view()}
                                            </ul>
                                            <p class="field-help">{tr("eff.rule")}</p>
                                        }.into_any()
                                    }}
                                </div>
                            }.into_any()
                        }
                    })
                }}
            </div>
        </section>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        assert!(valid_share_name("photos"));
        assert!(valid_share_name("Team.Docs_2026"));
        assert!(!valid_share_name(".hidden"));
        assert!(!valid_share_name("a b"));
        assert!(valid_path("/srv/data"));
        assert!(!valid_path("srv/data"));
        assert!(!valid_path("/srv/../etc"));
        assert!(!valid_path("/srv//data"));
        assert!(valid_client("192.0.2.0/24"));
        assert!(valid_client("*.example.lan"));
        assert!(!valid_client("host (rw)"));
    }

    #[test]
    fn usage_picks_the_longest_mountpoint() {
        let fs = |mp: &str| rynas_api_types::Filesystem { mountpoint: mp.into(), size: 100, used: 10, ..Default::default() };
        let list = vec![fs("/"), fs("/vol1"), fs("/vol10")];
        assert_eq!(fs_for("/vol1/photos", &list).map(|f| f.0), Some("/vol1".into()));
        assert_eq!(fs_for("/vol10", &list).map(|f| f.0), Some("/vol10".into()));
        assert_eq!(fs_for("/srv/x", &list).map(|f| f.0), Some("/".into()));
        let sub = Share { name: "photos".into(), subvolume: Some(ShareSubvolume { volume: "vol1".into(), compression: true, nocow: false }), ..Share::default() };
        assert_eq!(share_path(&sub), "/vol1/photos");
    }
}
