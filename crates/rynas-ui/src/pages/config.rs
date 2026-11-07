//! System → Configuration: applied revisions (diff, restore) and drift (files or system state
//! changed outside rynas: import into the configuration, or overwrite with it).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{DriftReport, DriftStatus, RevisionDetail, RevisionSummary, StagedChanges, StateDrift, StateDriftCode, StateDriftKind, TaskStatus, error_codes};

use crate::api::{self, ApiErr};
use crate::config::{changed, diff_table, key_label};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks::{run_task, task_error};
use crate::ui::{self, Confirm, Notice, PageHeader, Remote, Tone, cell, confirm, row, row_flag, status, table, text};

/// Revisions listed (the server keeps 200 / 365 days).
const REVISIONS: u32 = 50;

/// Why a system object drifted, translated from its code; the server's English text
/// (`true`: mark it `lang="en"`) for codes this build does not know and older servers.
fn drift_detail(s: &StateDrift) -> (String, bool) {
    let a = &s.detail_args;
    let key = match s.detail_code {
        Some(StateDriftCode::SambaWithoutUser) => "cfg.sd.samba_without_user",
        Some(StateDriftCode::UserUnmanaged) => "cfg.sd.user_unmanaged",
        Some(StateDriftCode::UserMissing) => "cfg.sd.user_missing",
        Some(StateDriftCode::UserMismatch) => "cfg.sd.user_mismatch",
        Some(StateDriftCode::UserWithoutSamba) => "cfg.sd.user_without_samba",
        Some(StateDriftCode::GroupMissing) => "cfg.sd.group_missing",
        Some(StateDriftCode::GroupUnmanaged) => "cfg.sd.group_unmanaged",
        Some(StateDriftCode::SharePathMissing) => "cfg.sd.share_path_missing",
        Some(StateDriftCode::ShareAclDiffers) => "cfg.sd.share_acl_differs",
        // keys whose servers send no code: the kind says enough
        None if s.key == crate::config::keys::UPS_NETSERVER => match s.kind {
            StateDriftKind::Missing => "cfg.sd.upsn_missing",
            _ => "cfg.sd.upsn_changed",
        },
        Some(StateDriftCode::Other) | None => return (s.detail.clone(), true),
    };
    let id = a.id.map(|n| n.to_string()).unwrap_or_default();
    let list = |v: &[String]| if v.is_empty() { "—".to_string() } else { v.join(", ") };
    let (missing, extra) = (list(&a.missing), list(&a.extra));
    let args = [
        ("name", a.name.as_deref().unwrap_or("")),
        ("id", id.as_str()),
        ("path", a.path.as_deref().unwrap_or("")),
        ("missing", missing.as_str()),
        ("extra", extra.as_str()),
    ];
    (tf(key, &args), false)
}

/// Text for a refused action; a 403 explains that administrators are needed.
fn refusal(e: &ApiErr, forbidden_key: &'static str) -> String {
    if e.code() == error_codes::FORBIDDEN { t(forbidden_key).to_string() } else { e.text() }
}

fn staged_text(n: u32) -> String {
    if n == 1 { t("cfg.staged_1").to_string() } else { tf("cfg.staged_n", &[("n", &n.to_string())]) }
}

#[component]
pub fn Configuration() -> impl IntoView {
    // an apply adds a revision and settles drift: both read again on every configuration change
    let revisions = Remote::<Vec<RevisionSummary>>::with_config(format!("/api/v1/config/revisions?limit={REVISIONS}"));
    let drift = Remote::<DriftReport>::with_config("/api/v1/config/drift");
    view! {
        <div class="page">
            <PageHeader icon=icons::HISTORY title="cfg.title" subtitle="cfg.subtitle">
                <a class="btn ghost" href="/system"><Icon icon=icons::ARROW_LEFT size=16 /><span class="btn-label">{tr("nav.system")}</span></a>
                <button class="btn ghost" on:click=move |_| { revisions.reload(); drift.reload(); }>
                    <Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("cfg.refresh")}</span>
                </button>
            </PageHeader>
            <div class="grid">
                <DriftCard drift=drift />
                <RevisionsCard revisions=revisions />
            </div>
        </div>
    }
}

/// Which drift action waits for confirmation.
#[derive(Clone)]
struct Overwrite {
    key: String,
    paths: Vec<String>,
}

#[component]
fn DriftCard(drift: Remote<DriftReport>) -> impl IntoView {
    let notice = RwSignal::new(None::<(Tone, String)>);
    let busy = RwSignal::new(None::<String>);
    let ask = RwSignal::new(false);
    let target = RwSignal::new(None::<Overwrite>);

    let import = move |key: String| {
        busy.set(Some(key.clone()));
        notice.set(None);
        spawn_local(async move {
            let url = format!("/api/v1/config/drift/{}/import", api::encode(&key));
            match api::post::<(), StagedChanges>(&url, None).await {
                Ok(s) => {
                    notice.set(Some((Tone::Ok, format!("{} {}", tf("cfg.imported", &[("key", &key_label(&key))]), staged_text(s.staged)))));
                    changed();
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => notice.set(Some((Tone::Err, refusal(&e, "cfg.forbidden_import")))),
            }
            busy.set(None);
        });
    };
    let overwrite = move || {
        let Some(o) = target.get_untracked() else { return };
        let key = o.key;
        busy.set(Some(key.clone()));
        notice.set(None);
        spawn_local(async move {
            let url = format!("/api/v1/config/drift/{}/overwrite", api::encode(&key));
            match run_task(&url, None).await {
                Ok(t) if t.status == TaskStatus::Ok => notice.set(Some((Tone::Ok, tf("cfg.overwritten", &[("key", &key_label(&key))])))),
                Ok(t) => notice.set(Some((Tone::Err, task_error(&t)))),
                Err(e) if e.status() == 401 => {}
                Err(e) => notice.set(Some((Tone::Err, refusal(&e, "cfg.forbidden_overwrite")))),
            }
            busy.set(None);
            drift.reload();
        });
    };

    // one row per drifted file or object: what, setting, state, Import / Restore
    let drift_row = move |what: AnyView, key: String, tone: Tone, state: &'static str, paths: Vec<String>| -> AnyView {
        let k1 = key.clone();
        let k2 = key.clone();
        let is_busy = move || busy.with(|b| b.as_deref() == Some(k1.as_str()));
        let any_busy = move || busy.with(Option::is_some);
        let actions = view! {
            <div class="row-actions">
                <button class="btn ghost sm" disabled=any_busy title=tr("cfg.import_hint") on:click=move |_| import(k2.clone())>
                    <Icon icon=icons::IMPORT size=14 />{tr("cfg.import")}
                </button>
                <button class="btn ghost sm" disabled=any_busy title=tr("cfg.overwrite_hint") on:click={
                    let key = key.clone();
                    move |_| {
                        target.set(Some(Overwrite { key: key.clone(), paths: paths.clone() }));
                        ask.set(true);
                    }
                }>
                    {move || is_busy().then(|| view! { <span class="btn-spinner" aria-hidden="true"></span> })}
                    <Icon icon=icons::FILE_INPUT size=14 />{tr("cfg.overwrite")}
                </button>
            </div>
        }
        .into_any();
        row(vec![
            cell("cfg.col_what", "wrap", what),
            cell("cfg.col_key", "", text(key_label(&key))),
            cell("cfg.col_state", "", status(tone, t(state).to_string())),
            cell("col.actions", "num", actions),
        ])
    };

    let content = move || {
        let Some(d) = drift.data.get() else { return ().into_any() };
        let checked = tf("cfg.checked", &[("n", &d.checked.to_string())]);
        if d.entries.is_empty() && d.state.is_empty() {
            return view! {
                <div class="drift-ok">
                    {status(Tone::Ok, t("cfg.no_drift").to_string())}
                    <span class="muted small">{checked}</span>
                </div>
            }
            .into_any();
        }
        let mut rows: Vec<AnyView> = d
            .entries
            .iter()
            .map(|e| {
                let (tone, st) = match e.status {
                    DriftStatus::Modified => (Tone::Warn, "cfg.drift_modified"),
                    DriftStatus::Missing => (Tone::Err, "cfg.drift_missing"),
                };
                let paths: Vec<String> = d.entries.iter().filter(|x| x.key == e.key).map(|x| x.path.clone()).collect();
                let what = view! { <span class="mono">{e.path.clone()}</span> }.into_any();
                drift_row(what, e.key.clone(), tone, st, paths)
            })
            .collect();
        rows.extend(d.state.iter().map(|s| {
            let st = match s.kind {
                StateDriftKind::Missing => "cfg.state_missing",
                StateDriftKind::Unmanaged => "cfg.state_unmanaged",
                StateDriftKind::Mismatch => "cfg.state_mismatch",
                StateDriftKind::MissingInSamba => "cfg.state_missing_samba",
                StateDriftKind::MissingInPasswd => "cfg.state_missing_passwd",
            };
            let (detail, en) = drift_detail(s);
            let what = view! {
                <span class="mono">{s.object.clone()}</span>
                <span class="muted small cell-note" lang=en.then_some("en")>{detail}</span>
            }
            .into_any();
            drift_row(what, s.key.clone(), Tone::Warn, st, Vec::new())
        }));
        view! {
            <p class="muted small">{tr("cfg.drift_text")}" "{checked}</p>
            {table("", &["cfg.col_what", "cfg.col_key", "cfg.col_state", ""], rows)}
        }
        .into_any()
    };

    let confirm_text = Signal::derive(move || {
        target.with(|o| {
            o.as_ref()
                .map(|o| {
                    let mut s = tf("cfg.overwrite_text", &[("key", &key_label(&o.key))]);
                    if !o.paths.is_empty() {
                        s.push(' ');
                        s.push_str(&o.paths.join(", "));
                    }
                    s
                })
                .unwrap_or_default()
        })
    });
    let on_overwrite = Callback::new(move |_| overwrite());
    let confirm_word = Signal::derive(move || target.with(|o| o.as_ref().map(|o| o.key.clone()).unwrap_or_default()));

    view! {
        <section class="card widget w-12" style="--i:0" aria-labelledby="drift-title">
            <h2 class="section-title" id="drift-title"><Icon icon=icons::FILE_WARNING size=18 />{tr("cfg.drift")}</h2>
            <Notice msg=notice />
            {drift.view(3, content)}
        </section>
        {move || ask.get().then(|| {
            let mut c = Confirm::new(
                t("cfg.overwrite_title").to_string(),
                confirm_text.get_untracked(),
                "cfg.overwrite",
                true,
                Callback::new(move |_| {
                    ask.set(false);
                    on_overwrite.run(());
                }),
                Callback::new(move |_| ask.set(false)),
            );
            c.typed = Signal::stored(Some(confirm_word.get_untracked()));
            confirm(c)
        })}
    }
}

#[component]
fn RevisionsCard(revisions: Remote<Vec<RevisionSummary>>) -> impl IntoView {
    let notice = RwSignal::new(None::<(Tone, String)>);
    let expanded = RwSignal::new(None::<u64>);
    let ask = RwSignal::new(false);
    let target = RwSignal::new(None::<u64>);
    let restoring = RwSignal::new(false);

    let restore = move || {
        let Some(n) = target.get_untracked() else { return };
        restoring.set(true);
        notice.set(None);
        spawn_local(async move {
            let ns = n.to_string();
            match api::post::<(), StagedChanges>(&format!("/api/v1/config/revisions/{n}/restore"), None).await {
                Ok(s) if s.staged == 0 => notice.set(Some((Tone::Info, tf("cfg.restore_noop", &[("n", &ns)])))),
                Ok(s) => {
                    notice.set(Some((Tone::Ok, format!("{} {}", tf("cfg.restored", &[("n", &ns)]), staged_text(s.staged)))));
                    changed();
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => notice.set(Some((Tone::Err, refusal(&e, "cfg.forbidden_restore")))),
            }
            restoring.set(false);
        });
    };

    let content = move || {
        let list = revisions.data.get().unwrap_or_default();
        if list.is_empty() {
            return view! { <p class="muted">{tr("cfg.no_revisions")}</p> }.into_any();
        }
        let latest = list.first().map(|r| r.id);
        let mut rows = Vec::with_capacity(list.len() * 2);
        for r in list {
            let id = r.id;
            let open = Signal::derive(move || expanded.get() == Some(id));
            let rev = view! {
                <button class="row-link" aria-expanded=move || open.get().to_string() aria-controls=format!("rev-{id}")
                    on:click=move |_| expanded.update(|e| *e = if *e == Some(id) { None } else { Some(id) })>
                    <span class="row-chev" class:open=move || open.get()><Icon icon=icons::CHEVRON_RIGHT size=14 /></span>
                    {format!("#{id}")}
                </button>
                {(latest == Some(id)).then(|| view! { <span class="chip accent">{tr("cfg.current")}</span> })}
            }
            .into_any();
            let mut chips: Vec<AnyView> = r.keys.iter().map(|k| view! { <span class="chip" title=k.clone()>{key_label(k)}</span> }.into_any()).collect();
            if let Some(f) = r.restored_from {
                chips.push(view! { <span class="chip accent">{tf("cfg.restored_from", &[("n", &f.to_string())])}</span> }.into_any());
            }
            let comment = if r.comment.is_empty() { "—".to_string() } else { r.comment.clone() };
            let action = view! {
                <button class="btn ghost sm" disabled=move || restoring.get() on:click=move |_| { target.set(Some(id)); ask.set(true); }>
                    <Icon icon=icons::ROTATE_CCW size=14 />{tr("cfg.restore")}
                </button>
            }
            .into_any();
            rows.push(row_flag(
                vec![
                    cell("cfg.col_rev", "tnum", rev),
                    cell("cfg.col_date", "tnum", text(i18n::datetime(r.ts_ms))),
                    cell("cfg.col_author", "", text(r.author.clone())),
                    cell("cfg.col_changes", "", view! { <div class="chips">{chips}</div> }.into_any()),
                    cell("cfg.col_comment", "wrap", text(comment)),
                    ui::actions(action),
                ],
                "selected",
                open,
            ));
            rows.push(
                (move || {
                    open.get().then(|| view! { <tr class="detail-row"><td colspan="6" id=format!("rev-{id}")><RevisionDiff id=id /></td></tr> })
                })
                .into_any(),
            );
        }
        table("revisions", &["cfg.col_rev", "cfg.col_date", "cfg.col_author", "cfg.col_changes", "cfg.col_comment", ""], rows)
    };

    let on_restore = Callback::new(move |_| restore());
    let confirm_text = Signal::derive(move || target.get().map(|n| tf("cfg.restore_text", &[("n", &n.to_string())])).unwrap_or_default());
    view! {
        <section class="card widget w-12" style="--i:1" aria-labelledby="rev-title">
            <h2 class="section-title" id="rev-title"><Icon icon=icons::HISTORY size=18 />{tr("cfg.revisions")}</h2>
            <Notice msg=notice />
            {revisions.view(6, content)}
        </section>
        {move || ask.get().then(|| confirm(Confirm::new(
            t("cfg.restore_title").to_string(),
            confirm_text.get_untracked(),
            "cfg.restore",
            false,
            Callback::new(move |_| {
                ask.set(false);
                on_restore.run(());
            }),
            Callback::new(move |_| ask.set(false)),
        )))}
    }
}

/// Diff and written files of one revision (fetched when expanded).
#[component]
fn RevisionDiff(id: u64) -> impl IntoView {
    let detail = Remote::<RevisionDetail>::new(format!("/api/v1/config/revisions/{id}"));
    let content = move || {
        let Some(d) = detail.data.get() else { return ().into_any() };
        let files: Vec<AnyView> = d.files.iter().map(|f| view! { <li>{f.clone()}</li> }.into_any()).collect();
        view! {
            <div class="rev-detail">
                {diff_table(&d.diff, None)}
                {(!files.is_empty()).then(|| view! {
                    <div class="rev-files">
                        <span class="field-label">{tr("cfg.files")}</span>
                        <ul class="mono small">{files}</ul>
                    </div>
                })}
            </div>
        }
        .into_any()
    };
    detail.view(3, content)
}

#[cfg(test)]
mod drift_tests {
    use rynas_api_types::StateDriftArgs;

    use super::*;

    #[test]
    fn drift_detail_is_translated_from_its_code() {
        let mut s = StateDrift {
            detail: "eve (uid 1500) was created outside rynas".into(),
            detail_code: Some(StateDriftCode::UserUnmanaged),
            detail_args: StateDriftArgs { name: Some("eve".into()), id: Some(1500), ..Default::default() },
            ..Default::default()
        };
        assert_eq!(drift_detail(&s), ("eve (UID 1500) was created outside rynas.".into(), false));
        s.detail_code = Some(StateDriftCode::ShareAclDiffers);
        s.detail_args = StateDriftArgs { path: Some("/v/p".into()), extra: vec!["user:bob:r-x".into()], ..Default::default() };
        assert_eq!(
            drift_detail(&s).0,
            "The permissions of /v/p differ (group membership changed or edited by hand). Missing: —. Unexpected: user:bob:r-x."
        );
        // older servers and unknown codes: the English text
        s.detail_code = None;
        assert_eq!(drift_detail(&s), (s.detail.clone(), true));
        s.detail_code = Some(StateDriftCode::Other);
        assert!(drift_detail(&s).1);
        let n = StateDrift { key: "ups.netserver".into(), kind: StateDriftKind::Mismatch, detail_args: StateDriftArgs { path: Some("/etc/nut/upsd.users".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(drift_detail(&n), ("The network UPS server accounts in /etc/nut/upsd.users differ from the configuration (edited by hand or by another tool).".into(), false));
    }
}
