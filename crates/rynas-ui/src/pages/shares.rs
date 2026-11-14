//! Shared folders (DSM Control Panel › Shared Folder, TrueNAS Shares, OMV Shared folders): list
//! with location, the folder's own size (measured in the background by rynasd, never
//! waking sleeping disks) and protocols; editor with general settings (new subvolume on a
//! volume or existing folder), SMB and NFS export settings, and the DSM-style permissions
//! matrix users/groups × (not set / read only / read-write / no access) with the server's
//! explained effective permission (`/shares/{name}/effective`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_query_map;
use rynas_api_types::{
    ADMIN_GROUP, Access, EffectiveAccess, EffectivePermission, GroupView, NfsRule, NfsSquash, Permission, PrincipalKind, Share, ShareStatus,
    ShareSize, ShareSizes, ShareSubvolume, ShareView, ShareWarningCode, SmbPreset, UnsharedFolder, UserView, storage::VolumePlace,
};

use crate::api;
use crate::form::{In, area_field, lens, lens_bool, save_bar, search_box, select_field, switch, tabs, text_field};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::live::use_live;
use crate::pages::share_crypt;
use crate::pages::share_folders::{self, Target};
use crate::toast::{saved_applied, toast};
use crate::ui::{self, EmptyState, Meter, PageHeader, Remote, Tone};

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

/// The sizes of the shares' folders (`/api/v1/share-sizes`): asked when the list arrives or
/// changes, then again while rynasd still measures some in the background (every 2 s, slower
/// up to 15 s), until the page is left. The list never waits for them. A failed request is
/// kept as the error (shown in the size column) instead of stopping in silence; the next
/// change of the list asks again.
fn share_sizes(shares: Remote<Vec<ShareView>>) -> (RwSignal<Option<ShareSizes>>, RwSignal<Option<String>>) {
    let sizes = RwSignal::new(None::<ShareSizes>);
    let failed = RwSignal::new(None::<String>);
    let polling = StoredValue::new(false);
    // the list changed while a request was out: ask once more at the end
    let again = StoredValue::new(false);
    Effect::new(move |_| {
        if !shares.data.with(|d| d.as_ref().is_some_and(|v| !v.is_empty())) {
            return;
        }
        if polling.get_value() {
            again.set_value(true);
            return;
        }
        polling.set_value(true);
        spawn_local(async move {
            let mut wait = 2000;
            loop {
                let r = match api::get::<ShareSizes>("/api/v1/share-sizes").await {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = failed.try_set(Some(e.text()));
                        break;
                    }
                };
                let _ = failed.try_set(None);
                let more = r.measuring || again.try_get_value().unwrap_or(false);
                let _ = again.try_set_value(false);
                if sizes.try_set(Some(r)).is_some() || !more {
                    break; // the page was left, or nothing more to wait for
                }
                api::sleep(wait).await;
                wait = (wait * 3 / 2).min(15_000);
            }
            let _ = polling.try_set_value(false);
        });
    });
    (sizes, failed)
}

/// The share's own size, a bar for its fraction of the volume, the volume by name and why
/// the size is missing or old; the exact numbers in the tooltip. `failed`: the last request
/// for the sizes failed (its error text).
fn size_cell(path: &str, size: Option<ShareSize>, fss: &[rynas_api_types::Filesystem], failed: Option<String>) -> AnyView {
    let Some(z) = size else {
        return match failed {
            Some(e) => view! {
                <div class="cell-main" title=e>
                    <span class="muted">{ui::DASH}</span>
                    <span class="cell-sub danger-text">{tr("shares.size_failed")}</span>
                </div>
            }
            .into_any(),
            None => view! { <span class="muted">{ui::DASH}</span> }.into_any(),
        };
    };
    let volume = z.volume.as_deref().map(crate::places::name);
    let capacity = fs_for(path, fss).map(|(_, _, size)| size).filter(|c| *c > 0);
    let note = match (z.standby, z.at_ms) {
        _ if failed.is_some() => Some(t("shares.size_failed").to_string()),
        (true, Some(at)) => Some(tf("shares.size_asleep_as_of", &[("when", &i18n::datetime(at))])),
        (true, None) => Some(t("shares.size_asleep").to_string()),
        _ if z.measuring && (z.bytes.is_none() || z.partial) => Some(t("shares.size_measuring").to_string()),
        _ => None,
    };
    let Some(bytes) = z.bytes else {
        return view! {
            <div class="cell-main share-size" title=failed.unwrap_or_default()>
                <span class="muted">{ui::DASH}</span>
                <span class="cell-sub">{note.or(volume)}</span>
            </div>
        }
        .into_any();
    };
    let pct = capacity.map(|c| bytes as f64 * 100.0 / c as f64);
    // number and unit stay together: a narrow column wraps "at least" above them instead
    let size = i18n::bytes(bytes).replace(' ', "\u{a0}");
    let text = if z.partial { tf("shares.size_min", &[("size", &size)]) } else { size };
    let vol_name = volume.clone().unwrap_or_else(|| t("shares.volume").to_string());
    let sub = note.or_else(|| match pct {
        Some(p) => Some(tf("shares.size_of_volume", &[("pct", &i18n::pct(p)), ("volume", &vol_name)])),
        None => volume.clone(),
    });
    let mut tip = vec![format!("{text} · {}", tf("shares.size_bytes", &[("n", &i18n::num(bytes as f64, 0))]))];
    if let Some(c) = capacity {
        tip.push(tf("shares.size_capacity", &[("volume", &vol_name), ("capacity", &i18n::bytes(c))]));
    }
    if z.exact {
        tip.push(t("shares.size_exact").to_string());
    }
    tip.push(t("shares.size_shared").to_string());
    if let Some(at) = z.at_ms {
        tip.push(tf("shares.size_as_of", &[("when", &i18n::datetime(at))]));
    }
    if let Some(e) = failed {
        tip.push(e);
    }
    view! {
        <div class="cell-main share-size" title=tip.join("\n")>
            <div class="usage-cell">
                {pct.map(|p| view! { <Meter pct=Signal::stored(p) /> })}
                <span class="tnum small">{text}</span>
            </div>
            <span class="cell-sub">{sub}</span>
        </div>
    }
    .into_any()
}

/// The permissions a new folder starts with (rynasd creates it owned by root, closed to
/// everybody else): the signed-in user, when given, and the administrators group when it
/// exists (`admins`: a listed user is a member; the server refuses unknown groups), both
/// read and write. Visible and editable in the editor.
fn default_permissions(me: Option<&str>, admins: bool) -> Vec<Permission> {
    let mut v = Vec::new();
    if let Some(me) = me.filter(|m| !m.is_empty() && *m != "root") {
        v.push(Permission { kind: PrincipalKind::User, name: me.to_string(), access: Access::Rw });
    }
    if admins {
        v.push(Permission { kind: PrincipalKind::Group, name: ADMIN_GROUP.to_string(), access: Access::Rw });
    }
    v
}

/// The server reports that only root can use the share's folder.
fn reported_root_only(st: &ShareStatus) -> bool {
    st.warning_items.iter().any(|w| w.code == ShareWarningCode::NoPermissionsRootOnly)
}

/// Saving `s` as it is leaves a folder only root can use: a new folder (subvolume, also an
/// encrypted one) without permissions, or a folder the server already reports as such.
fn root_only_save(s: &Share, is_new: bool, reported: bool) -> bool {
    s.permissions.is_empty() && ((is_new && s.subvolume.is_some()) || reported)
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
    let (sizes, sizes_failed) = share_sizes(shares);
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
        let known = sizes.get().map(|s| s.shares).unwrap_or_default();
        let failed = sizes_failed.get();
        view! {
            {ui::table("shares-table", &["shares.name", "shares.location", "shares.size", "shares.protocols", "shares.recycle", "col.status", ""], rows.into_iter().map(|v| {
                let size = known.iter().find(|s| s.name == v.share.name).cloned();
                share_row(v, size, failed.clone(), &fss, edit, del, cact)
            }).collect::<Vec<_>>())}
            // deduplication, copies and snapshots share data: sizes do not add up
            <p class="muted small table-note">{tr("shares.size_shared")}</p>
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
    } else if reported_root_only(st) {
        (Tone::Err, "shares.st_root_only")
    } else if !st.warnings.is_empty() {
        (Tone::Warn, "shares.st_warnings")
    } else {
        (Tone::Ok, "shares.st_ready")
    }
}

fn share_row(v: ShareView, size: Option<ShareSize>, size_failed: Option<String>, fss: &[rynas_api_types::Filesystem], edit: RwSignal<Option<ShareEdit>>, del: RwSignal<Option<String>>, cact: RwSignal<Option<share_crypt::Action>>) -> AnyView {
    let s = &v.share;
    let path = share_path(s);
    let size_view = size_cell(&path, size, fss, size_failed);
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
                    {s.dedup.as_ref().is_some_and(|d| d.enabled).then(|| view! { " "{ui::chip(Tone::Info, tr("dd.short").into_any())} })}
                </span>
            </div>
        }
        .into_any()),
        ui::cell("shares.size", "", size_view),
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
    // a folder only root can use stands out from the other remarks, with the way out
    let (root_warn, warnings): (Vec<String>, Vec<String>) = e
        .view
        .as_ref()
        .map(|v| {
            let mut split = (Vec::new(), Vec::new());
            for (i, text) in share_folders::warnings(&v.status).into_iter().enumerate() {
                if v.status.warning_items.get(i).is_some_and(|w| w.code == ShareWarningCode::NoPermissionsRootOnly) {
                    split.0.push(text);
                } else {
                    split.1.push(text);
                }
            }
            split
        })
        .unwrap_or_default();
    let nocow = e.view.as_ref().and_then(|v| super::share_nocow::existing(v.share.name.clone(), &v.status));
    // deduplication: no copy-on-write folders cannot share extents
    let dedup_nocow = e.view.as_ref().is_some_and(|v| v.status.nocow == Some(true));
    let saved_name = e.view.as_ref().map(|v| v.share.name.clone());
    let crypt_existing = e.view.as_ref().and_then(share_crypt::existing);
    // a brand new folder (not a folder shared again): starts with the default permissions
    let fresh = is_new && e.prefill.is_none();
    let reported = e.view.as_ref().is_some_and(|v| reported_root_only(&v.status));
    let d = RwSignal::new(saved.clone().or(e.prefill).unwrap_or_else(|| Share {
        subvolume: Some(ShareSubvolume { volume: String::new(), compression: true, nocow: false }),
        ..Share::default()
    }));
    // the defaults, once the user list tells who is a local user (an account of the internal
    // realm has no folder permissions to get) and whether the administrators group exists
    if fresh {
        let me = crate::auth::auth().session.with_untracked(|s| s.as_ref().map(|s| s.user.username.clone()));
        let seeded = StoredValue::new(false);
        Effect::new(move |_| {
            let Some((me_listed, admins)) = users.data.with(|u| {
                u.as_ref().map(|u| (me.as_deref().filter(|m| u.iter().any(|x| x.user.name == *m)).map(String::from), u.iter().any(|x| x.user.admin)))
            }) else {
                return;
            };
            if seeded.get_value() {
                return;
            }
            seeded.set_value(true);
            d.update(|s| {
                for p in default_permissions(me_listed.as_deref(), admins) {
                    if !s.permissions.iter().any(|x| x.kind == p.kind && x.name == p.name) {
                        s.permissions.push(p);
                    }
                }
            });
        });
    }
    // an empty matrix that leaves the folder to root: asked once more before saving
    let ask_empty = RwSignal::new(false);
    let empty_ok = RwSignal::new(false);
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
        if d.with_untracked(|s| root_only_save(s, is_new, reported)) && !empty_ok.get_untracked() {
            ask_empty.set(true);
            return;
        }
        empty_ok.set(false);
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
        {super::storage::dedup::share_fields(d, dedup_nocow, saved_name)}
        {let (v, s) = lens_bool(d, |s| s.show_snapshots, |s, v| s.show_snapshots = v);
         view! { <div hidden=move || encrypted.get()>{switch("shares.show_snapshots", Some("shares.show_snapshots_help"), v, s)}</div> }}
        {(!root_warn.is_empty()).then(|| ui::note(Tone::Err, icons::TRIANGLE_ALERT, view! {
            <div>
                {root_warn.into_iter().map(|w| view! { <p>{w}</p> }).collect_view()}
                <button type="button" class="btn ghost sm" on:click=move |_| tab.set(3)>{tr("perm.set_now")}</button>
            </div>
        }.into_any()))}
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
                {permissions_matrix(d, users, groups, Signal::derive(move || d.with(|s| root_only_save(s, is_new, reported))))}
                // an existing folder may keep its own permissions: no ACL from rynas
                <div class="btn-row" hidden=move || !(is_new && d.with(|s| s.subvolume.is_none() && !s.permissions.is_empty()))>
                    <button type="button" class="btn ghost sm" on:click=move |_| d.update(|s| s.permissions.clear())>{tr("perm.keep_own")}</button>
                </div>
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
    let ask = move || {
        ask_empty.get().then(|| {
            ui::confirm(ui::Confirm::new(
                t("perm.empty_confirm_title").to_string(),
                tf("perm.empty_confirm_text", &[("path", &d.with_untracked(share_path))]),
                "perm.empty_confirm_ok",
                true,
                Callback::new(move |_| {
                    ask_empty.set(false);
                    empty_ok.set(true);
                    save.run(());
                }),
                Callback::new(move |_| {
                    ask_empty.set(false);
                    tab.set(3);
                }),
            ))
        })
    };
    view! { {ui::drawer_wide(title, close, body, foot)}{reveal}{ask} }.into_any()
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
            {crate::form::segmented(
                "shares.location",
                &[("sub", "shares.loc_subvolume"), ("folder", "shares.loc_folder")],
                Signal::derive(move || if use_sub.get() { "sub" } else { "folder" }.to_string()),
                Callback::new(move |v: String| set_mode(v == "sub")),
            )}
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
            {ui::fold(None, "advanced", false, None, view! { 
                <div class="form">
                    {area_field("shares.veto", veto, set_veto, Some("shares.veto_help"), "")}
                    {area_field("shares.extra", extra, set_extra, Some("smb.extra_help"), "")}
                </div>
             }.into_any())}
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

/// `root_only`: saving the matrix as it is leaves the folder to root ([`root_only_save`]).
fn permissions_matrix(d: RwSignal<Share>, users: Remote<Vec<UserView>>, groups: Remote<Vec<GroupView>>, root_only: Signal<bool>) -> AnyView {
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
                    let sub = if k == PrincipalKind::Group && p.name == ADMIN_GROUP { t("perm.admins").to_string() } else { String::new() };
                    list.push((p.name.clone(), sub));
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
        {move || if root_only.get() {
            ui::note(Tone::Err, icons::TRIANGLE_ALERT, view! { <span>{tr("perm.root_only_help")}</span> }.into_any())
        } else {
            view! { <p class="field-help">{if n_entries() == 0 { t("perm.empty_help") } else { t("perm.rule_help") }}</p> }.into_any()
        }}
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
                                        ui::empty_line("eff.no_source")
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

    #[test]
    fn a_new_folder_starts_with_its_creator_and_the_administrators() {
        let rw = |kind, name: &str| Permission { kind, name: name.into(), access: Access::Rw };
        assert_eq!(default_permissions(Some("marco"), true), vec![rw(PrincipalKind::User, "marco"), rw(PrincipalKind::Group, ADMIN_GROUP)]);
        // root gets in anyway; a signed-in user rynas does not list is left out
        assert_eq!(default_permissions(Some("root"), true), vec![rw(PrincipalKind::Group, ADMIN_GROUP)]);
        assert_eq!(default_permissions(None, true), vec![rw(PrincipalKind::Group, ADMIN_GROUP)]);
        // no administrators group on the system: the server would refuse it
        assert_eq!(default_permissions(Some("marco"), false), vec![rw(PrincipalKind::User, "marco")]);
        assert!(default_permissions(None, false).is_empty());
    }

    #[test]
    fn an_empty_matrix_on_a_new_folder_needs_a_confirmation() {
        let sub = Some(ShareSubvolume { volume: "system".into(), compression: true, nocow: false });
        let new = Share { name: "sistema".into(), subvolume: sub, ..Share::default() };
        assert!(root_only_save(&new, true, false));
        // with the defaults, or on an existing folder that keeps its own permissions: no question
        assert!(!root_only_save(&Share { permissions: default_permissions(None, true), ..new.clone() }, true, false));
        let adopted = Share { name: "data".into(), path: "/srv/data".into(), ..Share::default() };
        assert!(!root_only_save(&adopted, true, false));
        // a saved share the server reports as root only
        assert!(root_only_save(&adopted, false, true));
        assert!(!root_only_save(&new, false, false));
        let mut st = ShareStatus::default();
        assert!(!reported_root_only(&st));
        st.warning_items.push(rynas_api_types::ShareWarning { code: ShareWarningCode::NoPermissionsRootOnly, args: Default::default() });
        st.warnings.push("x".into());
        assert!(reported_root_only(&st));
        assert_eq!(share_status(&ShareStatus { path_ok: true, ..st }), (Tone::Err, "shares.st_root_only"));
    }
}
