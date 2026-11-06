//! Users & Groups (DSM Control Panel › User & Group, TrueNAS Credentials › Local Users): local
//! Linux accounts managed by rynasd. Tables with search, a side drawer to create or edit,
//! password with strength meter, deletion with confirmation, group members editor. Every write
//! stages a pending change (`/config/apply` applies it).

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use rynas_api_types::{GroupRequest, GroupView, HOME_SYSTEM, HomesState, LocalGroup, LocalUser, PasswordRequest, PendingOp, ShellAccess, UserBadge, UserStatus, UserView};

use crate::api::{self, ApiErr};
use crate::form::{self, In, lens, lens_bool, save_bar, search_box, select_field, switch, tabs, text_field};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::toast::{saved_applied, saved_pending, toast};
use crate::ui::{self, Confirm, EmptyState, PageHeader, Remote, Tone, confirm, drawer};
use super::user_homes::{MoveAsk, move_dialog, place_of, place_select, places};

/// `[a-z_][a-z0-9_-]{0,31}` (user and group names).
pub fn valid_account_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn valid_email(e: &str) -> bool {
    e.is_empty() || (!e.contains(char::is_whitespace) && e.split_once('@').is_some_and(|(a, b)| !a.is_empty() && b.contains('.') && !b.ends_with('.')))
}

fn valid_date(d: &str) -> bool {
    let b = d.as_bytes();
    d.is_empty() || (b.len() == 10 && b[4] == b'-' && b[7] == b'-' && d.chars().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit()))
}

/// Error text of a field: the client rule when broken (after a save attempt or once the
/// field has text), else the server's error for that field.
fn field_err(
    show: RwSignal<bool>,
    server: RwSignal<Option<(String, String)>>,
    name: &'static str,
    value: Signal<String>,
    rule: fn(&str) -> bool,
    key: &'static str,
) -> Signal<Option<String>> {
    Signal::derive(move || {
        let v = value.get();
        if (show.get() || !v.is_empty()) && !rule(&v) {
            return Some(t(key).to_string());
        }
        server.with(|s| s.as_ref().filter(|(f, _)| f == name).map(|(_, m)| m.clone()))
    })
}

/// Records a failed write: inline when the server names a field, else in the form banner.
fn fail(e: &ApiErr, server: RwSignal<Option<(String, String)>>, error: RwSignal<Option<String>>) {
    match e.field() {
        Some(f) => {
            server.set(Some((f.to_string(), e.text())));
            error.set(Some(t("form.check").to_string()));
        }
        None => error.set(Some(e.text())),
    }
}

#[component]
pub fn Users() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    let users = Remote::<Vec<UserView>>::with_config("/api/v1/users");
    let groups = Remote::<Vec<GroupView>>::with_config("/api/v1/groups");
    let homes = Remote::<HomesState>::with_config("/api/v1/services/homes");
    view! {
        <div class="page">
            <PageHeader icon=icons::USERS title="nav.users" subtitle="users.subtitle" />
            <section class="card widget w-12">
                {tabs("ug", &["users.tab_users", "users.tab_groups", "users.tab_homes"], tab, "nav.users")}
                <div role="tabpanel" id="ug-p0" aria-labelledby="ug-t0" hidden=move || tab.get() != 0>{users_panel(users, groups, homes)}</div>
                <div role="tabpanel" id="ug-p1" aria-labelledby="ug-t1" hidden=move || tab.get() != 1>{groups_panel(users, groups)}</div>
                <div role="tabpanel" id="ug-p2" aria-labelledby="ug-t2" hidden=move || tab.get() != 2>{super::user_homes::homes_panel(homes)}</div>
            </section>
        </div>
    }
}

// ------------------------------------------------------------------ users

#[derive(Clone)]
struct UserEdit {
    user: LocalUser,
    status: Option<UserStatus>,
    /// Open on the password section.
    password: bool,
}

fn user_status(s: &UserStatus) -> (Tone, &'static str) {
    let has = |b: UserBadge| s.badges.contains(&b);
    if has(UserBadge::Missing) {
        (Tone::Err, "users.st_missing")
    } else if has(UserBadge::Disabled) {
        (Tone::Unknown, "users.st_disabled")
    } else if has(UserBadge::Expired) {
        (Tone::Warn, "users.st_expired")
    } else if s.needs_password || has(UserBadge::SmbPasswordReset) {
        (Tone::Warn, "users.st_password")
    } else if !s.managed && s.pending.is_none() {
        (Tone::Unknown, "users.st_unmanaged")
    } else {
        (Tone::Ok, "users.st_active")
    }
}

fn shell_key(s: ShellAccess) -> &'static str {
    match s {
        ShellAccess::None => "users.shell_none",
        ShellAccess::Sftp => "users.shell_sftp",
        ShellAccess::Shell => "users.shell_shell",
    }
}

fn users_panel(users: Remote<Vec<UserView>>, groups: Remote<Vec<GroupView>>, homes: Remote<HomesState>) -> AnyView {
    let q = RwSignal::new(String::new());
    let edit = RwSignal::new(None::<UserEdit>);
    let del = RwSignal::new(None::<String>);
    // a home move asked from the editor (it runs after the editor closes)
    let mv = RwSignal::new(None::<MoveAsk>);
    let create = move || edit.set(Some(UserEdit { user: LocalUser::default(), status: None, password: false }));

    let table = move || {
        if let Some(v) = users.gate(5) {
            return v;
        }
        let needle = q.get().to_lowercase();
        let rows: Vec<UserView> = users.data.with(|d| {
            d.iter()
                .flatten()
                .filter(|u| {
                    needle.is_empty()
                        || u.user.name.contains(&needle)
                        || u.user.full_name.to_lowercase().contains(&needle)
                        || u.user.email.to_lowercase().contains(&needle)
                })
                .cloned()
                .collect()
        });
        if rows.is_empty() {
            return if needle.is_empty() {
                view! {
                    <EmptyState icon=icons::USERS title=Signal::derive(|| t("users.empty").to_string()) text=Signal::derive(|| t("users.empty_text").to_string())>
                        <button class="btn primary" on:click=move |_| create()><Icon icon=icons::USER_PLUS size=16 />{tr("users.create")}</button>
                    </EmptyState>
                }
                .into_any()
            } else {
                view! { <p class="muted table-none">{tr("table.no_match")}</p> }.into_any()
            };
        }
        view! {
            {ui::table("", &["users.name", "users.email", "users.groups", "users.access", "col.status", ""], rows.into_iter().map(|v| user_row(v, edit, del)).collect::<Vec<_>>())}
        }
        .into_any()
    };

    let editor = move || {
        edit.get().map(|e| {
            let names: Vec<String> = groups.data.with_untracked(|g| g.iter().flatten().map(|g| g.group.name.clone()).collect());
            let hs = homes.data.get_untracked();
            user_editor(e, names, hs, Callback::new(move |a| mv.set(Some(a))), Callback::new(move |_| edit.set(None)))
        })
    };
    let mover = move || {
        mv.get().map(|a| {
            let done = Callback::new(move |_| {
                homes.reload();
                users.reload();
            });
            move_dialog(a, homes, Callback::new(move |_| mv.set(None)), done)
        })
    };

    let deleter = move || del.get().map(|name| user_delete_dialog(name, Callback::new(move |_| del.set(None))));

    view! {
        <div class="toolbar">
            {search_box("users.search", q)}
            <button class="btn primary" on:click=move |_| create()>
                <Icon icon=icons::USER_PLUS size=16 /><span class="btn-label">{tr("users.create")}</span>
            </button>
        </div>
        {table}
        {editor}
        {deleter}
        {mover}
    }
    .into_any()
}

fn user_row(v: UserView, edit: RwSignal<Option<UserEdit>>, del: RwSignal<Option<String>>) -> AnyView {
    let u = &v.user;
    let (tone, key) = user_status(&v.status);
    let name = u.name.clone();
    let deleting = v.status.pending == Some(PendingOp::Delete);
    let can_password = v.status.on_system && !deleting;
    let more = u.groups.len().saturating_sub(3);
    let groups: Vec<String> = u.groups.iter().take(3).cloned().collect();
    let open = {
        let v = v.clone();
        move |password: bool| edit.set(Some(UserEdit { user: v.user.clone(), status: Some(v.status.clone()), password }))
    };
    let open2 = open.clone();
    let open3 = open.clone();
    let name_label = format!("{} {}", t("edit"), u.name);
    let pw_label = format!("{} {}", t("users.set_password"), u.name);
    let del_label = format!("{} {}", t("delete"), u.name);
    ui::row(vec![
        ui::cell("users.name", "", view! {
            <div class="cell-main">
                <button class="link-strong mono" aria-label=name_label disabled=deleting on:click=move |_| open(false)>{u.name.clone()}</button>
                <span class="cell-sub">{u.full_name.clone()}</span>
            </div>
        }
        .into_any()),
        ui::cell("users.email", "muted", view! { {u.email.clone()} }.into_any()),
        ui::cell("users.groups", "", view! {
            <div class="chips">
                {groups.into_iter().map(|g| view! { <span class="chip">{g}</span> }).collect_view()}
                {(more > 0).then(|| view! { <span class="chip">{format!("+{more}")}</span> })}
            </div>
        }
        .into_any()),
        ui::cell("users.access", "", view! {
            <div class="chips">
                {u.admin.then(|| view! { <span class="chip accent">{tr("users.admin_chip")}</span> })}
                {u.smb.then(|| view! { <span class="chip">"SMB"</span> })}
                {(u.shell != ShellAccess::None).then(|| view! { <span class="chip">{tr(shell_key(u.shell))}</span> })}
            </div>
        }
        .into_any()),
        ui::cell("col.status", "", view! { <div class="chips">{(v.status.pending != Some(PendingOp::Add)).then(|| ui::status(tone, t(key).to_string()))}{ui::pending_status(v.status.pending)}</div> }.into_any()),
        ui::actions(view! {
            <button class="icon-btn sm" title=tr("edit") aria-label=format!("{} {}", t("edit"), u.name) disabled=deleting on:click=move |_| open2(false)>
                <Icon icon=icons::PENCIL size=16 />
            </button>
            <button class="icon-btn sm" title=tr("users.set_password") aria-label=pw_label disabled=!can_password on:click=move |_| open3(true)>
                <Icon icon=icons::KEY size=16 />
            </button>
            <button class="icon-btn sm danger" title=tr("delete") aria-label=del_label disabled=deleting on:click=move |_| del.set(Some(name.clone()))>
                <Icon icon=icons::TRASH_2 size=16 />
            </button>
        }
        .into_any()),
    ])
}

const SHELL_OPTS: &[(&str, &str)] = &[("none", "users.shell_none"), ("sftp", "users.shell_sftp"), ("shell", "users.shell_shell")];

fn user_editor(e: UserEdit, all_groups: Vec<String>, homes: Option<HomesState>, on_move: Callback<MoveAsk>, close: Callback<()>) -> AnyView {
    let is_new = e.status.is_none();
    let on_system = e.status.as_ref().is_some_and(|s| s.on_system);
    let d = RwSignal::new(e.user.clone());
    let show = RwSignal::new(false);
    let server = RwSignal::new(None::<(String, String)>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let (name, set_name) = lens(d, |u| u.name.clone(), |u, v| u.name = v.trim().to_ascii_lowercase());
    let (full, set_full) = lens(d, |u| u.full_name.clone(), |u, v| u.full_name = v);
    let (email, set_email) = lens(d, |u| u.email.clone(), |u, v| u.email = v.trim().to_string());
    let (descr, set_descr) = lens(d, |u| u.description.clone(), |u, v| u.description = v);
    let (expires, set_expires) = lens(d, |u| u.expires.clone().unwrap_or_default(), |u, v| u.expires = (!v.is_empty()).then_some(v));
    let (shell, set_shell) = lens(
        d,
        |u| match u.shell {
            ShellAccess::None => "none".into(),
            ShellAccess::Sftp => "sftp".into(),
            ShellAccess::Shell => "shell".into(),
        },
        |u, v| {
            u.shell = match v.as_str() {
                "sftp" => ShellAccess::Sftp,
                "shell" => ShellAccess::Shell,
                _ => ShellAccess::None,
            }
        },
    );
    let (admin, set_admin) = lens_bool(d, |u| u.admin, |u, v| u.admin = v);
    let (smb, set_smb) = lens_bool(d, |u| u.smb, |u, v| u.smb = v);
    let (disabled, set_disabled) = lens_bool(d, |u| u.disabled, |u, v| u.disabled = v);

    let name_err = field_err(show, server, "name", name, valid_account_name, "v.account_name");
    let full_err = field_err(show, server, "full_name", full, |s| !s.contains([':', ',']), "v.full_name");
    let email_err = field_err(show, server, "email", email, valid_email, "v.email");
    let exp_err = field_err(show, server, "expires", expires, valid_date, "v.date");
    let none = Signal::derive(|| None::<String>);

    // where the home is or goes: a volume or the system disk
    let hs = homes.unwrap_or_default();
    let mine = hs.homes.iter().find(|h| h.user == e.user.name).cloned();
    let (here, place_opts) = if is_new {
        let mut o: Vec<(String, String)> = if hs.settings.enabled {
            hs.volumes
                .iter()
                .map(|v| (v.path.clone(), tf("homes.volume_opt", &[("path", &v.path), ("fs", &v.fs_type), ("free", &crate::i18n::bytes(v.avail_bytes))])))
                .collect()
        } else {
            Vec::new()
        };
        o.push((HOME_SYSTEM.to_string(), t("homes.dest_system").to_string()));
        let want = if hs.settings.enabled && !hs.settings.volume.is_empty() { hs.settings.volume.clone() } else { HOME_SYSTEM.to_string() };
        // the default volume not offered (not mounted): the first place, sent as chosen
        let here = if o.iter().any(|(v, _)| *v == want) {
            want
        } else {
            let first = o[0].0.clone();
            d.update_untracked(|u| u.home_volume = first.clone());
            first
        };
        (here, o)
    } else {
        match mine.as_ref().filter(|m| m.moved.is_none() && on_system) {
            Some(m) => {
                let here = place_of(m);
                let mut o = places(&hs);
                // where it is now, also when that place is not offered as a destination
                if !o.iter().any(|(v, _)| *v == here) {
                    o.insert(0, (here.clone(), tf("homes.dest_here", &[("path", &m.home)])));
                }
                (here, o)
            }
            None => (String::new(), Vec::new()),
        }
    };
    let place = RwSignal::new(here.clone());
    let place_field = (place_opts.len() > 1).then(|| {
        let set = Callback::new(move |v: String| {
            if is_new {
                let hv = v.clone();
                d.update(|u| u.home_volume = hv);
            }
            place.set(v);
        });
        place_select("users.home_place", Some(if is_new { "users.home_place_help" } else { "users.home_move_help" }), place_opts, place.into(), set)
    });
    let user_name = e.user.name.clone();
    let move_ask = move || (!is_new && place.get_untracked() != here).then(|| MoveAsk { users: vec![user_name.clone()], to: place.get_untracked() });
    let original = e.user.clone();

    let valid = move || d.with(|u| (!is_new || valid_account_name(&u.name)) && !u.full_name.contains([':', ',']) && valid_email(&u.email));
    let save = Callback::new(move |_| {
        show.set(true);
        server.set(None);
        if !valid() || !exp_err.with_untracked(Option::is_none) {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        let ask = move_ask();
        let body = d.get_untracked();
        // only the place changed: straight to the move
        if let Some(a) = ask.clone()
            && body == original
        {
            close.run(());
            on_move.run(a);
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = if is_new {
                api::post::<_, UserView>("/api/v1/users", Some(&body)).await
            } else {
                api::put::<_, UserView>(&format!("/api/v1/users/{}", api::encode(&body.name)), &body).await
            };
            match r {
                Ok(_) => {
                    if crate::config::apply_now(&[crate::config::keys::USERS], error).await {
                        saved_applied();
                        close.run(());
                        if let Some(a) = ask {
                            on_move.run(a);
                        }
                    }
                }
                Err(e) => fail(&e, server, error),
            }
            let _ = busy.try_set(false);
        });
    });

    let title = {
        let n = e.user.name.clone();
        Signal::derive(move || if is_new { t("users.create").to_string() } else { tf("users.edit_title", &[("name", &n)]) })
    };

    let group_list = view! {
        <fieldset class="fieldset">
            <legend class="field-label">{tr("users.groups")}</legend>
            {if all_groups.is_empty() {
                view! { <p class="field-help">{tr("users.no_groups")}</p> }.into_any()
            } else {
                view! {
                    <div class="check-grid">
                        {all_groups.into_iter().map(|g| {
                            let g2 = g.clone();
                            let g3 = g.clone();
                            view! {
                                <label class="check">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || d.with(|u| u.groups.contains(&g2))
                                        on:change=move |ev| {
                                            let on = event_target_checked(&ev);
                                            let g = g3.clone();
                                            d.update(|u| {
                                                u.groups.retain(|x| *x != g);
                                                if on {
                                                    u.groups.push(g);
                                                }
                                            });
                                        }
                                    />
                                    <span class="mono">{g}</span>
                                </label>
                            }
                        }).collect_view()}
                    </div>
                }.into_any()
            }}
        </fieldset>
    };

    let password = if is_new {
        view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("pw.after_apply")}</p> }.into_any()
    } else if on_system {
        password_section(e.user.name.clone(), e.password)
    } else {
        view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("pw.not_applied")}</p> }.into_any()
    };

    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            <h3 class="form-section">{tr("users.sec_info")}</h3>
            {is_new.then(|| text_field("users.name", name, set_name, name_err, In { help: Some("users.name_help"), mono: true, maxlength: 32, ..In::default() }))}
            {text_field("users.full_name", full, set_full, full_err, In { autocomplete: "name", ..In::default() })}
            {text_field("users.email", email, set_email, email_err, In { kind: "email", help: Some("users.email_help"), autocomplete: "email", ..In::default() })}
            {text_field("users.description", descr, set_descr, none, In::default())}
            {group_list}
            // where the home is; another place starts a home move after saving
            {(!is_new && !e.user.home.is_empty()).then(|| view! {
                <dl class="kvs">{ui::kv_row("users.home", Signal::stored(e.user.home.clone()), "mono")}</dl>
            })}
            {place_field}
            <h3 class="form-section">{tr("users.sec_access")}</h3>
            {switch("users.admin", Some("users.admin_help"), admin, set_admin)}
            {switch("users.smb", Some("users.smb_help"), smb, set_smb)}
            {select_field("users.shell", Some("users.shell_help"), SHELL_OPTS, shell, set_shell)}
            {switch("users.disabled", Some("users.disabled_help"), disabled, set_disabled)}
            {text_field("users.expires", expires, set_expires, exp_err, In { kind: "date", help: Some("users.expires_help"), ..In::default() })}
            <button type="submit" hidden=true></button>
        </form>
        <h3 class="form-section">{tr("users.sec_password")}</h3>
        {password}
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if is_new { "create" } else { "save" }, close, save, error);
    drawer(title, close, body, foot)
}

/// Set a user's password (task `user.password`): strength meter, confirmation field, the
/// server's rules checked before sending.
fn password_section(user: String, focus: bool) -> AnyView {
    let pw = RwSignal::new(String::new());
    let pw2 = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let u2 = user.clone();
    let score = Signal::derive(move || {
        let p = pw.get();
        (!p.is_empty()).then(|| form::strength(&p, &u2))
    });
    let u3 = user.clone();
    let rule = Signal::derive(move || {
        let p = pw.get();
        (!p.is_empty()).then(|| form::password_problem(&p, &u3)).flatten().map(|k| t(k).to_string())
    });
    let mismatch = Signal::derive(move || (!pw2.get().is_empty() && pw.get() != pw2.get()).then(|| t("pw.mismatch").to_string()));
    let ok = move || !pw.get().is_empty() && rule.with(Option::is_none) && pw.get() == pw2.get();
    let submit = move || {
        if !ok() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let body = serde_json::to_string(&PasswordRequest { password: pw.get_untracked() }).ok();
        let user = user.clone();
        spawn_local(async move {
            // the change runs as a task (chpasswd, smbpasswd): follow it to its outcome
            let r = crate::tasks::run_task(&format!("/api/v1/users/{}/password", api::encode(&user)), body).await;
            let _ = busy.try_set(false);
            match r {
                Ok(task) if task.status == rynas_api_types::TaskStatus::Ok => {
                    let _ = pw.try_set(String::new());
                    let _ = pw2.try_set(String::new());
                    toast(Tone::Ok, tf("pw.done", &[("name", &user)]));
                }
                Ok(task) => toast(Tone::Err, crate::tasks::task_error(&task)),
                Err(e) if e.code() == rynas_api_types::error_codes::NOT_APPLIED => {
                    let _ = error.try_set(Some(t("pw.not_applied").to_string()));
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    };
    let submit2 = submit.clone();
    let p1 = text_field(
        "pw.new",
        Signal::derive(move || pw.get()),
        Callback::new(move |v| pw.set(v)),
        rule,
        In { kind: "password", autocomplete: "new-password", ..In::default() },
    );
    let p2 = text_field(
        "pw.confirm",
        Signal::derive(move || pw2.get()),
        Callback::new(move |v| pw2.set(v)),
        mismatch,
        In { kind: "password", autocomplete: "new-password", ..In::default() },
    );
    let form_ref = NodeRef::<leptos::html::Form>::new();
    if focus {
        // after the drawer has focused its first field
        request_animation_frame(move || {
            request_animation_frame(move || {
                if let Some(f) = form_ref.get_untracked()
                    && let Ok(Some(i)) = f.query_selector("input")
                {
                    let _ = i.unchecked_into::<web_sys::HtmlElement>().focus();
                }
            })
        });
    }
    view! {
        <form class="form pw-form" node_ref=form_ref on:submit=move |ev| { ev.prevent_default(); submit2(); }>
            {p1}
            {form::strength_meter(score)}
            {p2}
            <p class="field-help">{tr("pw.rules")}</p>
            {crate::form::form_error(error)}
            <div class="sec-actions">
                <button type="submit" class="btn ghost" disabled=move || busy.get() || !ok()>
                    {crate::form::spinner(busy)}<Icon icon=icons::KEY size=16 />{tr("users.set_password")}
                </button>
            </div>
        </form>
    }
    .into_any()
}

fn user_delete_dialog(name: String, close: Callback<()>) -> AnyView {
    let remove_home = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let n2 = name.clone();
    let typed = Signal::derive(move || remove_home.get().then(|| n2.clone()));
    let n3 = name.clone();
    let on_confirm = Callback::new(move |_| {
        busy.set(true);
        error.set(None);
        let rh = remove_home.get_untracked();
        let n = n3.clone();
        spawn_local(async move {
            let mut url = format!("/api/v1/users/{}?remove_home={rh}", api::encode(&n));
            if rh {
                url.push_str(&format!("&confirm={}", api::encode(&n)));
            }
            let r = api::delete_empty(&url).await;
            busy.set(false);
            match r {
                Ok(()) => {
                    saved_pending();
                    crate::config::changed();
                    close.run(());
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    });
    confirm(Confirm {
        title: tf("users.delete_title", &[("name", &name)]),
        text: t("users.delete_text").to_string(),
        action: "delete",
        danger: true,
        extra: Some(switch("users.remove_home", Some("users.remove_home_help"), remove_home.into(), Callback::new(move |v| remove_home.set(v)))),
        typed,
        busy,
        error,
        on_confirm,
        on_cancel: close,
    })
}

// ------------------------------------------------------------------ groups

fn groups_panel(users: Remote<Vec<UserView>>, groups: Remote<Vec<GroupView>>) -> AnyView {
    let q = RwSignal::new(String::new());
    let edit = RwSignal::new(None::<GroupView>);
    let del = RwSignal::new(None::<String>);
    let create = move || edit.set(Some(GroupView::default()));

    let table = move || {
        if let Some(v) = groups.gate(4) {
            return v;
        }
        let needle = q.get().to_lowercase();
        let rows: Vec<GroupView> = groups.data.with(|d| {
            d.iter()
                .flatten()
                .filter(|g| needle.is_empty() || g.group.name.contains(&needle) || g.group.description.to_lowercase().contains(&needle))
                .cloned()
                .collect()
        });
        if rows.is_empty() {
            return if needle.is_empty() {
                view! {
                    <EmptyState icon=icons::USERS_ROUND title=Signal::derive(|| t("groups.empty").to_string()) text=Signal::derive(|| t("groups.empty_text").to_string())>
                        <button class="btn primary" on:click=move |_| create()><Icon icon=icons::PLUS size=16 />{tr("groups.create")}</button>
                    </EmptyState>
                }
                .into_any()
            } else {
                view! { <p class="muted table-none">{tr("table.no_match")}</p> }.into_any()
            };
        }
        view! {
            {ui::table("", &["groups.name", "#groups.gid", "users.description", "groups.members", "col.status", ""], rows.into_iter().map(|g| {
                            let deleting = g.pending == Some(PendingOp::Delete);
                            let name = g.group.name.clone();
                            let g2 = g.clone();
                            let g3 = g.clone();
                            let more = g.members.len().saturating_sub(4);
                            let status = match (g.pending, g.managed) {
                                (Some(PendingOp::Add), _) => None,
                                (_, true) => Some(ui::status(Tone::Ok, t("users.st_active").to_string())),
                                _ => Some(ui::status(Tone::Unknown, t("users.st_unmanaged").to_string())),
                            };
                            ui::row(vec![
                                ui::cell("groups.name", "", view! { <button class="link-strong mono" disabled=deleting on:click=move |_| edit.set(Some(g2.clone()))>{g.group.name.clone()}</button> }.into_any()),
                                ui::cell("groups.gid", "num", view! { {(g.group.gid != 0).then(|| g.group.gid.to_string())} }.into_any()),
                                ui::cell("users.description", "muted", view! { {g.group.description.clone()} }.into_any()),
                                ui::cell("groups.members", "", view! {
                                    <div class="chips">
                                        {g.members.iter().take(4).map(|m| view! { <span class="chip mono">{m.clone()}</span> }).collect_view()}
                                        {(more > 0).then(|| view! { <span class="chip">{format!("+{more}")}</span> })}
                                        {g.members.is_empty().then(|| view! { <span class="muted small">{tr("groups.no_members")}</span> })}
                                    </div>
                                }
                                .into_any()),
                                ui::cell("col.status", "", view! { <div class="chips">{status}{ui::pending_status(g.pending)}</div> }.into_any()),
                                ui::actions(view! {
                                    <button class="icon-btn sm" title=tr("edit") aria-label=format!("{} {}", t("edit"), g.group.name) disabled=deleting on:click=move |_| edit.set(Some(g3.clone()))>
                                        <Icon icon=icons::PENCIL size=16 />
                                    </button>
                                    <button class="icon-btn sm danger" title=tr("delete") aria-label=format!("{} {}", t("delete"), g.group.name) disabled=deleting on:click=move |_| del.set(Some(name.clone()))>
                                        <Icon icon=icons::TRASH_2 size=16 />
                                    </button>
                                }
                                .into_any()),
                            ])
                        }).collect::<Vec<_>>())}
        }
        .into_any()
    };

    let editor = move || {
        edit.get().map(|g| {
            let names: Vec<String> = users.data.with_untracked(|u| u.iter().flatten().map(|u| u.user.name.clone()).collect());
            group_editor(g, names, Callback::new(move |_| edit.set(None)))
        })
    };
    let deleter = move || {
        del.get().map(|name| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| del.set(None));
            let n = name.clone();
            confirm(Confirm {
                title: tf("groups.delete_title", &[("name", &name)]),
                text: t("groups.delete_text").to_string(),
                action: "delete",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    busy.set(true);
                    let n = n.clone();
                    spawn_local(async move {
                        let r = api::delete_empty(&format!("/api/v1/groups/{}", api::encode(&n))).await;
                        busy.set(false);
                        match r {
                            Ok(()) => {
                                saved_pending();
                                crate::config::changed();
                                close.run(());
                            }
                            Err(e) => error.set(Some(e.text())),
                        }
                    });
                }),
                on_cancel: close,
            })
        })
    };

    view! {
        <div class="toolbar">
            {search_box("groups.search", q)}
            <button class="btn primary" on:click=move |_| create()>
                <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("groups.create")}</span>
            </button>
        </div>
        {table}
        {editor}
        {deleter}
    }
    .into_any()
}

fn group_editor(g: GroupView, all_users: Vec<String>, close: Callback<()>) -> AnyView {
    let is_new = g.group.name.is_empty();
    let d = RwSignal::new(g.group.clone());
    let members = RwSignal::new(g.members.clone());
    let show = RwSignal::new(false);
    let server = RwSignal::new(None::<(String, String)>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let q = RwSignal::new(String::new());
    let (name, set_name) = lens(d, |g: &LocalGroup| g.name.clone(), |g, v| g.name = v.trim().to_ascii_lowercase());
    let (descr, set_descr) = lens(d, |g: &LocalGroup| g.description.clone(), |g, v| g.description = v);
    let name_err = field_err(show, server, "name", name, valid_account_name, "v.account_name");
    let none = Signal::derive(|| None::<String>);

    let save = Callback::new(move |_| {
        show.set(true);
        server.set(None);
        if is_new && !d.with_untracked(|g| valid_account_name(&g.name)) {
            error.set(Some(t("form.check").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let body = GroupRequest { group: d.get_untracked(), members: Some(members.get_untracked()) };
        spawn_local(async move {
            let r = if is_new {
                api::post::<_, GroupView>("/api/v1/groups", Some(&body)).await
            } else {
                api::put::<_, GroupView>(&format!("/api/v1/groups/{}", api::encode(&body.group.name)), &body).await
            };
            match r {
                Ok(_) => {
                    if crate::config::apply_now(&[crate::config::keys::USERS], error).await {
                        saved_applied();
                        close.run(());
                    }
                }
                Err(e) => fail(&e, server, error),
            }
            let _ = busy.try_set(false);
        });
    });

    let n = g.group.name.clone();
    let title = Signal::derive(move || if is_new { t("groups.create").to_string() } else { tf("groups.edit_title", &[("name", &n)]) });
    let count = move || members.with(Vec::len);
    let total = all_users.len();
    let list = all_users
        .into_iter()
        .map(|u| {
            let (u2, u3, u4) = (u.clone(), u.clone(), u.clone());
            view! {
                <label class="check" hidden=move || { let n = q.get().to_lowercase(); !n.is_empty() && !u4.contains(&n) }>
                    <input
                        type="checkbox"
                        prop:checked=move || members.with(|m| m.contains(&u2))
                        on:change=move |ev| {
                            let on = event_target_checked(&ev);
                            let u = u3.clone();
                            members.update(|m| {
                                m.retain(|x| *x != u);
                                if on {
                                    m.push(u);
                                }
                            });
                        }
                    />
                    <span class="mono">{u}</span>
                </label>
            }
        })
        .collect_view();

    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save.run(()); }>
            {is_new.then(|| text_field("groups.name", name, set_name, name_err, In { help: Some("groups.name_help"), mono: true, maxlength: 32, ..In::default() }))}
            {text_field("users.description", descr, set_descr, none, In::default())}
            <fieldset class="fieldset">
                <legend class="field-label">
                    {tr("groups.members")}" "<span class="muted tnum">{move || format!("{} / {total}", count())}</span>
                </legend>
                {if total == 0 {
                    view! { <p class="field-help">{tr("groups.no_users")}</p> }.into_any()
                } else {
                    view! {
                        {search_box("users.search", q)}
                        <div class="check-grid">{list}</div>
                    }.into_any()
                }}
            </fieldset>
            <button type="submit" hidden=true></button>
        </form>
    }
    .into_any();
    let foot = save_bar(busy, Signal::derive(|| true), if is_new { "create" } else { "save" }, close, save, error);
    drawer(title, close, body, foot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_names() {
        assert!(valid_account_name("alice"));
        assert!(valid_account_name("_svc-backup1"));
        assert!(!valid_account_name("Alice"));
        assert!(!valid_account_name("1bob"));
        assert!(!valid_account_name(""));
        assert!(!valid_account_name(&"a".repeat(33)));
        assert!(valid_email("alice@example.com"));
        assert!(valid_email(""));
        assert!(!valid_email("alice@"));
        assert!(!valid_email("a b@example.com"));
        assert!(valid_date("2027-01-31"));
        assert!(!valid_date("31/01/2027"));
    }
}
