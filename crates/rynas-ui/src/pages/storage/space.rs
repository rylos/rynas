//! Storage › Space (`docs/phase2/space.md`, D-SPACE): what takes room on each volume and how to
//! get it back. Per volume a bar by category and what cleaning would free; the snapshot sources
//! (sortable by size or growth) with their snapshots to select, pin, preview and delete; the safe
//! actions as cards with their numbers; the review list; the guardrails. Every deletion is a
//! task; the preview measures the exact number before the confirmation.

use std::collections::BTreeSet;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::space::{
    MeasureMethod, PinRequest, Protection, ReviewDeleteRequest, ReviewItem, ReviewKind, SafeAction, SafeActionKind, SnapshotSource,
    SourceBudget, SourceKind, SpaceCategory, SpaceDeleteRequest, SpaceOverview, SpacePreview, SpacePreviewRequest, SpaceSettings, VolumeSpace,
};

use super::SubNav;
use crate::api;
use crate::form::{self, Draft, SaveBar, spinner};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks;
use crate::toast::toast;
use crate::ui::{self, Confirm, Modal, PageHeader, Remote, Skeleton, Tone, confirm};

const URL: &str = "/api/v1/space";
const SETTINGS: &str = "/api/v1/space/settings";

fn category_key(c: SpaceCategory) -> &'static str {
    match c {
        SpaceCategory::Snapshots => "sp.cat.snapshots",
        SpaceCategory::RecycleBins => "sp.cat.recycle_bins",
        SpaceCategory::Docker => "sp.cat.docker",
        SpaceCategory::System => "sp.cat.system",
        SpaceCategory::Leftovers => "sp.cat.leftovers",
        SpaceCategory::Data => "sp.cat.data",
    }
}

fn category_class(c: SpaceCategory) -> &'static str {
    match c {
        SpaceCategory::Snapshots => "c1",
        SpaceCategory::RecycleBins => "c2",
        SpaceCategory::Docker => "c3",
        SpaceCategory::System => "c4",
        SpaceCategory::Leftovers => "c5",
        SpaceCategory::Data => "c6",
    }
}

fn kind_key(k: SourceKind) -> &'static str {
    match k {
        SourceKind::ShareJob => "sp.kind.share_job",
        SourceKind::OrphanJob => "sp.kind.orphan_job",
        SourceKind::AppStack => "sp.kind.app_stack",
        SourceKind::SystemPoints => "snap.tab.system",
        SourceKind::PcBackup => "sp.kind.pc_backup",
        SourceKind::Replica => "sp.rev.removed_target_replicas",
        SourceKind::Received => "sp.rev.received_unknown_owner",
        SourceKind::IncomingReplica => "sp.kind.incoming_replica",
    }
}

fn protection_key(p: Protection) -> &'static str {
    match p {
        Protection::Newest => "sp.prot.newest",
        Protection::ReplicationParent => "sp.prot.replication_parent",
        Protection::NewestBackup => "sp.prot.newest_backup",
        Protection::BootReference => "sp.prot.boot_reference",
        Protection::Pinned => "sp.prot.pinned",
        Protection::Writable => "sp.prot.writable",
        Protection::IncomingReplica => "sp.prot.incoming_replica",
    }
}

fn action_key(k: SafeActionKind) -> (&'static str, &'static str, &'static str) {
    match k {
        SafeActionKind::JournalVacuum => ("sp.act.journal_vacuum", "sp.act.journal_vacuum_text", "journal-vacuum"),
        SafeActionKind::AptClean => ("sp.act.apt_clean", "sp.act.apt_clean_text", "apt-clean"),
        SafeActionKind::DockerDangling => ("sp.act.docker_dangling", "sp.act.docker_dangling_text", "docker-dangling"),
        SafeActionKind::RecycleBins => ("sp.act.recycle_bins", "sp.act.recycle_bins_text", "recycle-bins"),
        SafeActionKind::Thumbnails => ("sp.act.thumbnails", "sp.act.thumbnails_text", "thumbnails"),
        SafeActionKind::CatalogSerials => ("sp.act.catalog_serials", "sp.act.catalog_serials_text", "catalog-serials"),
        SafeActionKind::IdenticalAppSnapshots => ("sp.act.identical_app_snapshots", "sp.act.identical_app_snapshots_text", "identical-app-snapshots"),
    }
}

fn review_key(k: ReviewKind) -> &'static str {
    match k {
        ReviewKind::RollbackCopy => "sp.rev.rollback_copy",
        ReviewKind::RestoreCopy => "sp.rev.restore_copy",
        ReviewKind::AdoptCopy => "sp.rev.adopt_copy",
        ReviewKind::StackCopy => "sp.rev.stack_copy",
        ReviewKind::OrphanSnapshots => "sp.rev.orphan_snapshots",
        ReviewKind::RemovedTargetReplicas => "sp.rev.removed_target_replicas",
        ReviewKind::RemovedPcFolder => "sp.rev.removed_pc_folder",
        ReviewKind::ReceivedUnknownOwner => "sp.rev.received_unknown_owner",
        ReviewKind::OldKernel => "sp.rev.old_kernel",
        ReviewKind::UnusedImage => "sp.rev.unused_image",
        ReviewKind::UnusedVolume => "sp.rev.unused_volume",
    }
}

/// "≈ 12.3 GB", "at least 1.2 GB" or "not measured".
fn freed_text(freed: Option<u64>, at_least: Option<u64>, method: MeasureMethod) -> String {
    match (freed, at_least) {
        (Some(f), _) if method == MeasureMethod::Qgroup => i18n::bytes(f),
        (Some(f), _) => tf("sp.approx", &[("size", &i18n::bytes(f))]),
        (None, Some(a)) => tf("shares.size_min", &[("size", &i18n::bytes(a))]),
        (None, None) => t("sp.not_measured").to_string(),
    }
}

fn growth_text(g: Option<i64>) -> String {
    match g {
        None => ui::DASH.to_string(),
        Some(0) => "0".into(),
        Some(g) if g > 0 => format!("+{}", i18n::bytes(g as u64)),
        Some(g) => format!("−{}", i18n::bytes(g.unsigned_abs())),
    }
}

/// The bar of a volume: the categories, then the free space.
fn volume_card(v: VolumeSpace) -> AnyView {
    let total = v.total_bytes.max(1) as f64;
    let share = move |b: u64| format!("{:.4}", b as f64 * 100.0 / total);
    let segs = v
        .categories
        .iter()
        .filter(|c| c.bytes > 0)
        .map(|c| view! { <div class=format!("stack-seg {}", category_class(c.category)) style:flex-grow=share(c.bytes) title=format!("{}: {}", t(category_key(c.category)), i18n::bytes(c.bytes))></div> })
        .collect_view();
    let legend = v
        .categories
        .iter()
        .map(|c| {
            let approx = if c.exact { "" } else { "≈ " };
            view! { <li><span class=format!("dot {}", category_class(c.category))></span>{tr(category_key(c.category))}<span class="tnum">{format!("{approx}{}", i18n::bytes(c.bytes))}</span></li> }
        })
        .collect_view();
    let name = crate::places::name(&v.volume);
    let label = tf("sp.used_of", &[("size", &i18n::bytes_of(v.used_bytes, v.total_bytes))]);
    view! {
        <div class="space-vol">
            <div class="space-vol-head">
                <h3 class="space-vol-name"><Icon icon=icons::DATABASE size=16 />{name}</h3>
                <span class="muted small mono">{v.mount.clone()}</span>
                {v.low_free.then(|| ui::status(Tone::Warn, t("sp.low_free").to_string()))}
                {v.auto_free.then(|| ui::chip(Tone::Info, ui::text(t("sp.auto_on").to_string())))}
            </div>
            <div class="stack tall" role="img" aria-label=label>
                {segs}
                <div class="stack-seg rest" style:flex-grow=share(v.free_bytes)></div>
            </div>
            <ul class="legend">
                {legend}
                <li><span class="dot rest"></span>{tr("sp.free")}<span class="tnum">{i18n::bytes(v.free_bytes)}</span></li>
            </ul>
            <p class="small">
                <strong>{tr("sp.reclaimable")}</strong>" "
                <span class="tnum">{if v.btrfs || v.reclaimable_bytes > 0 { tf("sp.approx", &[("size", &i18n::bytes(v.reclaimable_bytes))]) } else { t("sp.no_snapshots").to_string() }}</span>
                <span class="muted">{v.scanned_at_ms.map(|ms| format!(" · {}", tf("sp.scanned", &[("when", &i18n::datetime(ms))])))}</span>
            </p>
        </div>
    }
    .into_any()
}

/// The preview dialog's state: the task running, then its answer.
#[derive(Clone)]
enum PreviewState {
    Measuring,
    Ready(SpacePreview),
}

#[component]
pub fn SpacePage() -> impl IntoView {
    let ov = Remote::<SpaceOverview>::new(URL);
    // a scan running: read again until it ends
    crate::live::poll_while(3000, move || ov.data.with(|o| o.as_ref().is_some_and(|o| o.scanning)), move || ov.reload());
    let scan_busy = RwSignal::new(false);
    let selected = RwSignal::new(BTreeSet::<String>::new());
    let open = RwSignal::new(BTreeSet::<String>::new());
    let sort = RwSignal::new("size".to_string());
    let preview = RwSignal::new(None::<PreviewState>);
    let delete_busy = RwSignal::new(false);
    let delete_err = RwSignal::new(None::<String>);
    let review_sel = RwSignal::new(BTreeSet::<String>::new());
    let review_ask = RwSignal::new(false);
    let action_ask = RwSignal::new(None::<SafeAction>);
    let budget = RwSignal::new(None::<SnapshotSource>);

    let scan = move |_| {
        scan_busy.set(true);
        spawn_local(async move {
            match api::start_task(&format!("{URL}/scan"), None).await {
                Ok(_) => ov.reload(),
                Err(e) => toast(Tone::Err, e.text()),
            }
            scan_busy.set(false);
        });
    };

    let start_preview = move |_| {
        let paths: Vec<String> = selected.get_untracked().into_iter().collect();
        if paths.is_empty() {
            return;
        }
        preview.set(Some(PreviewState::Measuring));
        delete_err.set(None);
        spawn_local(async move {
            let body = serde_json::to_string(&SpacePreviewRequest { paths }).ok();
            // the task store brings its progress and end ([`follow_preview`])
            if let Err(e) = api::start_task(&format!("{URL}/preview"), body).await {
                let _ = preview.try_set(None);
                toast(Tone::Err, e.text());
            }
        });
    };
    follow_preview(preview);

    let pin = move |path: String, on: bool| {
        spawn_local(async move {
            let url = format!("{URL}/{}", if on { "pin" } else { "unpin" });
            match api::post_empty(&url, Some(&PinRequest { path })).await {
                Ok(()) => ov.reload(),
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };

    let sources_view = move || {
        let Some(o) = ov.data.get() else { return ().into_any() };
        let mut sources: Vec<SnapshotSource> = o.volumes.iter().flat_map(|v| v.sources.clone()).filter(|s| s.count > 0).collect();
        if sort.get() == "growth" {
            sources.sort_by_key(|s| std::cmp::Reverse(s.growth_7d_bytes.unwrap_or(i64::MIN)));
        } else {
            sources.sort_by_key(|s| std::cmp::Reverse(s.freed_bytes.or(s.freed_at_least_bytes).unwrap_or(0)));
        }
        if sources.is_empty() {
            return ui::empty_line("sp.no_sources");
        }
        sources.into_iter().map(|s| source_row(s, open, selected, budget, pin)).collect_view().into_any()
    };

    let actions_view = move || {
        let Some(o) = ov.data.get() else { return ().into_any() };
        o.safe_actions
            .into_iter()
            .map(|a| {
                let (title, text, _) = action_key(a.kind);
                let a2 = a.clone();
                let numbers = if a.kind == SafeActionKind::IdenticalAppSnapshots {
                    tf("sp.items", &[("n", &a.items.to_string())])
                } else if a.items > 0 {
                    format!("{} · {}", i18n::bytes(a.bytes), tf("sp.items", &[("n", &a.items.to_string())]))
                } else {
                    i18n::bytes(a.bytes)
                };
                view! {
                    <div class="space-action">
                        <h4 class="space-action-title">{tr(title)}</h4>
                        <p class="small muted">{tr(text)}</p>
                        <div class="space-action-foot">
                            <span class="tnum">{numbers}</span>
                            <button class="btn sm" disabled=!a.available on:click=move |_| action_ask.set(Some(a2.clone()))>{tr("sp.clean")}</button>
                        </div>
                    </div>
                }
            })
            .collect_view()
            .into_any()
    };

    let review_view = move || {
        let Some(o) = ov.data.get() else { return ().into_any() };
        if o.review.is_empty() {
            return ui::empty_line("sp.review_empty");
        }
        let rows = o.review.into_iter().map(|r| review_row(r, review_sel)).collect_view();
        view! {
            <ul class="space-review">{rows}</ul>
            <div class="row-end space-review-foot">
                <span class="muted small">{move || tf("sp.selected", &[("n", &review_sel.with(|s| s.len()).to_string())])}</span>
                <button class="btn danger sm" disabled=move || review_sel.with(BTreeSet::is_empty) on:click=move |_| review_ask.set(true)>
                    <Icon icon=icons::TRASH_2 size=16 />{tr("sp.remove_selected")}
                </button>
            </div>
        }
        .into_any()
    };

    let header_line = move || {
        ov.data.with(|o| {
            o.as_ref().map(|o| {
                let when = o.scanned_at_ms.map(|ms| tf("sp.scanned", &[("when", &i18n::datetime(ms))])).unwrap_or_else(|| t("sp.never_scanned").to_string());
                view! {
                    <p class="muted small space-scan-line">
                        {o.scanning.then(|| ui::status(Tone::Info, t("sp.scanning").to_string()))}
                        <span>{when}</span>
                    </p>
                }
            })
        })
    };

    view! {
        <div class="page">
            <PageHeader icon=icons::HARD_DRIVE title="sto.title" subtitle="sp.subtitle">
                <button class="btn" disabled=move || scan_busy.get() || ov.data.with(|o| o.as_ref().is_some_and(|o| o.scanning)) on:click=scan>
                    {spinner(scan_busy)}<Icon icon=icons::REFRESH_CW size=16 /><span class="btn-label">{tr("sp.scan_now")}</span>
                </button>
            </PageHeader>
            <SubNav />
            {move || ov.view(6, move || view! {
                {header_line}
                <section class="card widget w-12" style="--i:0">
                    <h2 class="section-title"><Icon icon=icons::DATABASE size=16 />{tr("sp.volumes")}</h2>
                    <div class="space-vols">{move || ov.data.get().map(|o| o.volumes.into_iter().map(volume_card).collect_view())}</div>
                </section>
                <section class="card widget w-12" style="--i:1">
                    <div class="space-head">
                        <h2 class="section-title"><Icon icon=icons::CAMERA size=16 />{tr("sp.sources")}</h2>
                        {form::segmented("sp.sort", &[("size", "sp.sort.size"), ("growth", "sp.sort.growth")], sort.into(), Callback::new(move |v| sort.set(v)))}
                    </div>
                    <p class="muted small">{tr("sp.sources_text")}</p>
                    {sources_view}
                    <div class="space-select-bar">
                        <span class="muted small">{move || tf("sp.selected", &[("n", &selected.with(|s| s.len()).to_string())])}</span>
                        <button class="btn ghost sm" disabled=move || selected.with(BTreeSet::is_empty) on:click=move |_| selected.set(BTreeSet::new())>{tr("sp.clear_selection")}</button>
                        <button class="btn primary sm" disabled=move || selected.with(BTreeSet::is_empty) on:click=start_preview>{tr("sp.preview")}</button>
                    </div>
                </section>
                <section class="card widget w-12" style="--i:2">
                    <h2 class="section-title"><Icon icon=icons::SPARKLES size=16 />{tr("sp.safe_actions")}</h2>
                    <div class="space-actions">{actions_view}</div>
                </section>
                <section class="card widget w-12" style="--i:3">
                    <h2 class="section-title"><Icon icon=icons::LIST_CHECKS size=16 />{tr("sp.review")}</h2>
                    <p class="muted small">{tr("sp.review_text")}</p>
                    {review_view}
                </section>
                <Guardrails
                    volumes=Signal::derive(move || ov.data.with(|o| o.as_ref().map(|o| o.volumes.iter().map(|v| v.volume.clone()).collect::<Vec<_>>()).unwrap_or_default()))
                    journal=Signal::derive(move || ov.data.with(|o| o.as_ref().map(|o| (o.journal_cap_bytes, o.journal_cap_by_admin)).unwrap_or_default()))
                />
            }.into_any())}
            {move || preview.get().map(|p| preview_dialog(p, selected, preview, delete_busy, delete_err, ov))}
            {move || review_ask.get().then(|| {
                let ids: Vec<String> = review_sel.get_untracked().into_iter().collect();
                let n = ids.len() as u32;
                let c = Confirm::new(
                    t("sp.review_ask_title").to_string(),
                    i18n::tn(if ids.iter().any(|i| i.starts_with("volume:")) { "sp.review_ask_volumes" } else { "sp.review_ask" }, u64::from(n), &[]),
                    "sp.remove_selected",
                    true,
                    Callback::new(move |_| {}),
                    Callback::new(move |_| review_ask.set(false)),
                );
                let (busy, error) = (c.busy, c.error);
                let go = Callback::new(move |_| {
                    let body = serde_json::to_string(&ReviewDeleteRequest { ids: ids.clone(), confirm: n }).ok();
                    let url = format!("{URL}/review/delete");
                    tasks::run_and_report(async move { api::start_task(&url, body).await }, t("sp.removed").to_string(), busy, error, Callback::new(move |_| {
                        review_ask.set(false);
                        review_sel.set(BTreeSet::new());
                        ov.reload();
                    }));
                });
                confirm(Confirm { on_confirm: go, ..c })
            })}
            {move || action_ask.get().map(|a| {
                let (title, text, slug) = action_key(a.kind);
                let c = Confirm::new(t(title).to_string(), t(text).to_string(), "sp.clean", false, Callback::new(move |_| {}), Callback::new(move |_| action_ask.set(None)));
                let (busy, error) = (c.busy, c.error);
                let go = Callback::new(move |_| {
                    let url = format!("{URL}/actions/{slug}");
                    tasks::run_and_report(async move { api::start_task(&url, None).await }, t("sp.cleaned").to_string(), busy, error, Callback::new(move |_| {
                        action_ask.set(None);
                        ov.reload();
                    }));
                });
                confirm(Confirm { on_confirm: go, ..c })
            })}
            {move || budget.get().map(|s| view! { <BudgetDialog source=s on_close=Callback::new(move |_| { budget.set(None); ov.reload(); }) /> })}
        </div>
    }
}

/// One source: its summary (always visible) and, opened, its snapshots.
fn source_row(
    s: SnapshotSource,
    open: RwSignal<BTreeSet<String>>,
    selected: RwSignal<BTreeSet<String>>,
    budget: RwSignal<Option<SnapshotSource>>,
    pin: impl Fn(String, bool) + Copy + Send + Sync + 'static,
) -> AnyView {
    let id = s.id.clone();
    let id2 = id.clone();
    let is_open = move || open.with(|o| o.contains(&id));
    let freed = freed_text(s.freed_bytes, s.freed_at_least_bytes, s.method);
    let oldest = s.oldest_ms.map(i18n::date).unwrap_or_else(|| ui::DASH.to_string());
    let caps = {
        let mut v = Vec::new();
        if let Some(k) = s.caps.keep_count {
            v.push(tf("sp.keeps", &[("n", &k.to_string())]));
        }
        if let Some(n) = s.caps.max_count {
            v.push(i18n::tn("sp.budget_count", u64::from(n), &[]));
        }
        if let Some(d) = s.caps.max_age_days {
            v.push(i18n::tn("sp.budget_age", u64::from(d), &[]));
        }
        if let Some(b) = s.caps.max_bytes {
            v.push(tf("sp.budget_bytes", &[("size", &i18n::bytes(b))]));
        }
        v.join(" · ")
    };
    let s2 = s.clone();
    let snaps = s
        .snapshots
        .iter()
        .map(|x| {
            let path = x.path.clone();
            let (p1, p2, p3) = (path.clone(), path.clone(), path.clone());
            let protected = x.protected;
            let checked = move || selected.with(|sel| sel.contains(&p1));
            let when = x.created_ms.map(i18n::datetime).unwrap_or_else(|| x.name.clone());
            let pinned = x.pinned;
            view! {
                <li class="space-snap">
                    <label class="check">
                        <input type="checkbox" disabled=protected.is_some() prop:checked=checked aria-label=x.name.clone()
                            on:change=move |ev| {
                                let on = event_target_checked(&ev);
                                selected.update(|sel| { if on { sel.insert(p2.clone()); } else { sel.remove(&p2); } });
                            } />
                        <span class="tnum">{when}</span>
                    </label>
                    <span class="muted small mono space-snap-name">{x.name.clone()}</span>
                    {protected.map(|p| ui::chip(if p == Protection::Pinned { Tone::Info } else { Tone::Unknown }, ui::text(t(protection_key(p)).to_string())))}
                    {x.identical.then(|| ui::chip(Tone::Unknown, ui::text(t("sp.identical").to_string())))}
                    <button class="icon-btn sm row-end" title=if pinned { tr("sp.unpin") } else { tr("sp.pin") } aria-label=ui::verb_name(if pinned { "sp.unpin" } else { "sp.pin" }, &x.name)
                        on:click=move |_| pin(p3.clone(), !pinned)>
                        <Icon icon=if pinned { icons::PIN_OFF } else { icons::PIN } size=16 />
                    </button>
                </li>
            }
        })
        .collect_view();
    let all: Vec<String> = s.snapshots.iter().filter(|x| x.protected.is_none()).map(|x| x.path.clone()).collect();
    let all2 = all.clone();
    view! {
        <details class="fold space-source" open=is_open on:toggle=move |ev| {
            let on = event_target::<web_sys::Element>(&ev).has_attribute("open");
            if open.with_untracked(|o| o.contains(&id2)) != on {
                open.update(|o| { if on { o.insert(id2.clone()); } else { o.remove(&id2); } });
            }
        }>
            <summary>
                <Icon icon=icons::CHEVRON_RIGHT size=14 />
                <span class="space-source-main">
                    <span class="fold-title">{s.name.clone()}</span>
                    <span class="muted small">{format!("{} · {}", t(kind_key(s.kind)), crate::places::name(&s.volume))}</span>
                </span>
                <span class="space-source-nums">
                    <span class="tnum">{freed}</span>
                    <span class="muted small tnum">{format!("{} · {} · {}", tf("sp.count", &[("n", &s.count.to_string())]), tf("sp.oldest", &[("when", &oldest)]), tf("sp.growth", &[("size", &growth_text(s.growth_7d_bytes))]))}</span>
                </span>
                {s.over_budget.then(|| ui::status(Tone::Warn, t("sp.over_budget").to_string()))}
            </summary>
            <div class="fold-body">
                <p class="muted small mono space-dir">{s.dir.clone()}</p>
                {(!caps.is_empty()).then(|| view! { <p class="small">{caps.clone()}</p> })}
                <div class="space-source-tools">
                    <button class="btn ghost sm" disabled=all.is_empty() on:click=move |_| selected.update(|sel| sel.extend(all2.iter().cloned()))>{tr("sp.select_all")}</button>
                    <button class="btn ghost sm" on:click=move |_| budget.set(Some(s2.clone()))>{tr("sp.budget")}</button>
                </div>
                <ul class="space-snaps">{snaps}</ul>
            </div>
        </details>
    }
    .into_any()
}

fn review_row(r: ReviewItem, sel: RwSignal<BTreeSet<String>>) -> AnyView {
    let (id, id2) = (r.id.clone(), r.id.clone());
    let size = r.bytes.map(i18n::bytes).unwrap_or_else(|| ui::DASH.to_string());
    view! {
        <li class="space-review-item">
            <label class="check">
                <input type="checkbox" prop:checked=move || sel.with(|s| s.contains(&id)) aria-label=r.label.clone()
                    on:change=move |ev| {
                        let on = event_target_checked(&ev);
                        sel.update(|s| { if on { s.insert(id2.clone()); } else { s.remove(&id2); } });
                    } />
                <span>{tr(review_key(r.kind))}</span>
            </label>
            <span class="mono small space-review-label">{r.label.clone()}</span>
            <span class="tnum small row-end">{size}</span>
            {(r.kind == ReviewKind::UnusedVolume).then(|| view! { <span class="small muted space-review-note">{tr("sp.volume_data_note")}</span> })}
        </li>
    }
    .into_any()
}

/// Where the last preview shown (or closed) is remembered, so that it opens once only.
const PREVIEW_SEEN: &str = "rynas.space.preview.seen";

/// The preview runs on the server (`space.preview`): the dialog follows the newest one the
/// signed-in user started, from the task store, also when it was started before a reload or
/// in a tab since closed. Its answer opens once ([`PREVIEW_SEEN`]); a measuring dialog the
/// user closed stays closed.
fn follow_preview(preview: RwSignal<Option<PreviewState>>) {
    let me = crate::auth::auth().session.with_untracked(|s| s.as_ref().map(|s| s.user.username.clone())).unwrap_or_default();
    let job = tasks::use_task_by("space.preview", move |t| t.user == me);
    let seen = StoredValue::new(crate::prefs::storage_get(PREVIEW_SEEN));
    // the preview this page opened the measuring dialog for
    let opened = StoredValue::new(None::<String>);
    Effect::new(move |_| {
        let Some(task) = job.task.get() else { return };
        if seen.with_value(|s| s.as_deref() == Some(task.id.as_str())) {
            return;
        }
        let mine = opened.with_value(|o| o.as_deref() == Some(task.id.as_str()));
        let closed = mine && preview.with_untracked(Option::is_none);
        if !task.status.is_finished() {
            if !mine {
                opened.set_value(Some(task.id.clone()));
                preview.set(Some(PreviewState::Measuring));
            }
            return;
        }
        seen.set_value(Some(task.id.clone()));
        crate::prefs::storage_set(PREVIEW_SEEN, &task.id);
        if closed {
            return;
        }
        if task.status != rynas_api_types::TaskStatus::Ok {
            preview.set(None);
            crate::toast::toast_task(tasks::task_tone(&task), tasks::task_error(&task), task.id);
            return;
        }
        let id = task.id;
        spawn_local(async move {
            match api::get::<SpacePreview>(&format!("{URL}/previews/{}", api::encode(&id))).await {
                Ok(p) => {
                    let _ = preview.try_set(Some(PreviewState::Ready(p)));
                }
                Err(e) => {
                    let _ = preview.try_set(None);
                    toast(Tone::Err, e.text());
                }
            }
        });
    });
}

fn preview_dialog(
    p: PreviewState,
    selected: RwSignal<BTreeSet<String>>,
    preview: RwSignal<Option<PreviewState>>,
    busy: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    ov: Remote<SpaceOverview>,
) -> AnyView {
    let close = Callback::new(move |_| preview.set(None));
    match p {
        PreviewState::Measuring => view! {
            <Modal title=Signal::derive(|| t("sp.preview_title").to_string()) on_close=close foot=move || ui::close_foot(close)>
                <p class="dialog-text">{tr("sp.measuring")}</p>
                <Skeleton lines=2 />
            </Modal>
        }
        .into_any(),
        PreviewState::Ready(r) => {
            let n = r.paths.len() as u32;
            let paths = r.paths.clone();
            let freed = freed_text(r.freed_bytes, r.freed_at_least_bytes, r.method);
            let refused = r
                .refused
                .iter()
                .map(|x| view! { <li class="small"><span class="mono">{x.path.clone()}</span>" — "{x.protected.map(|p| t(protection_key(p))).unwrap_or(t("sp.unknown_snapshot"))}</li> })
                .collect_view();
            let go = Callback::new(move |_| {
                let body = serde_json::to_string(&SpaceDeleteRequest { paths: paths.clone(), confirm: n }).ok();
                let url = format!("{URL}/delete");
                tasks::run_and_report(async move { api::start_task(&url, body).await }, t("sp.deleted").to_string(), busy, error, Callback::new(move |_| {
                    preview.set(None);
                    selected.set(BTreeSet::new());
                    ov.reload();
                }));
            });
            let foot = move || {
                form::save_bar_with(SaveBar {
                    can_save: Signal::stored(n > 0),
                    danger: Signal::stored(true),
                    icon: Some(icons::TRASH_2),
                    label: Signal::derive(move || tf("sp.delete_n", &[("n", &n.to_string())])),
                    error: Some(error),
                    ..SaveBar::new(busy, "sp.delete", close, go)
                })
            };
            view! {
                <Modal title=Signal::derive(|| t("sp.preview_title").to_string()) on_close=close foot=foot>
                    <p class="dialog-text">{i18n::tn("sp.preview_text", n as u64, &[("size", &freed)])}</p>
                    {r.freed_at_least_bytes.map(|a| view! { <p class="small muted">{tf("sp.preview_at_least", &[("size", &i18n::bytes(a))])}</p> })}
                    {r.timed_out_minutes.map(|m| view! { <p class="small muted">{i18n::tn("sp.preview_timed_out", m as u64, &[])}</p> })}
                    {(!r.refused.is_empty()).then(|| view! {
                        <p class="small"><strong>{tr("sp.kept_title")}</strong></p>
                        <ul class="space-refused">{refused}</ul>
                    })}
                    <p class="small muted">{tr("sp.freed_later")}</p>
                </Modal>
            }
            .into_any()
        }
    }
}

/// A source's budget: most snapshots, oldest age, most bytes (saved and applied at once).
#[component]
fn BudgetDialog(source: SnapshotSource, on_close: Callback<()>) -> impl IntoView {
    let count = RwSignal::new(source.caps.max_count.map(|n| n.to_string()).unwrap_or_default());
    let age = RwSignal::new(source.caps.max_age_days.map(|n| n.to_string()).unwrap_or_default());
    let gib = RwSignal::new(source.caps.max_bytes.map(|b| (b >> 30).to_string()).unwrap_or_default());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let id = source.id.clone();
    let save = Callback::new(move |_| {
        let num = |s: String| s.trim().parse::<u64>().ok().filter(|n| *n > 0);
        let b = SourceBudget {
            source: id.clone(),
            max_count: num(count.get_untracked()).map(|n| n.min(u32::MAX as u64) as u32),
            max_age_days: num(age.get_untracked()).map(|n| n.min(u32::MAX as u64) as u32),
            max_bytes: num(gib.get_untracked()).map(|n| n << 30),
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let r = async {
                let mut s: SpaceSettings = api::get(SETTINGS).await?;
                s.budgets.retain(|x| x.source != b.source);
                if b.max_count.is_some() || b.max_age_days.is_some() || b.max_bytes.is_some() {
                    s.budgets.push(b);
                }
                api::put::<_, SpaceSettings>(SETTINGS, &s).await
            }
            .await;
            match r {
                Ok(_) => {
                    if crate::config::apply_now(&["space"], error).await {
                        on_close.run(());
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            busy.set(false);
        });
    });
    let foot = move || form::save_bar_with(SaveBar { error: Some(error), ..SaveBar::new(busy, "save", on_close, save) });
    let title = tf("sp.budget_title", &[("name", &source.name)]);
    view! {
        <Modal title=Signal::stored(title) on_close=on_close foot=foot>
            <p class="dialog-text">{tr("sp.budget_text")}</p>
            {form::number_field("sp.budget_max_count", None, count.into(), Callback::new(move |v| count.set(v)), 0, 100_000)}
            {form::number_field("sp.budget_max_age", None, age.into(), Callback::new(move |v| age.set(v)), 0, 36_500)}
            {form::number_field("sp.budget_max_gib", None, gib.into(), Callback::new(move |v| gib.set(v)), 0, 1_000_000)}
        </Modal>
    }
}

/// The guardrails (key `space`): notifications, automatic cleanup per volume, journal cap,
/// recycle bins and thumbnails. Save stages and applies the key.
#[component]
fn Guardrails(volumes: Signal<Vec<String>>, journal: Signal<(Option<u64>, bool)>) -> impl IntoView {
    let server = Remote::<SpaceSettings>::new(SETTINGS);
    let dr = Draft::new(SpaceSettings::default(), move || server.data.get());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let num_field = move |label: &'static str, help: Option<&'static str>, get: fn(&SpaceSettings) -> u32, set: fn(&mut SpaceSettings, u32), max: i64| {
        form::number_field(
            label,
            help,
            Signal::derive(move || dr.d.with(|s| get(s).to_string())),
            Callback::new(move |v: String| {
                if let Ok(n) = v.trim().parse::<u32>() {
                    dr.d.update(|s| set(s, n));
                }
            }),
            0,
            max,
        )
    };
    let save = Callback::new(move |_| {
        let v = dr.d.get_untracked();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::put::<_, SpaceSettings>(SETTINGS, &v).await {
                Ok(_) => {
                    if crate::config::apply_now(&["space"], error).await {
                        server.reload();
                        toast(Tone::Ok, t("sp.saved").to_string());
                    }
                }
                Err(e) => error.set(Some(e.text())),
            }
            busy.set(false);
        });
    });
    let auto = move || {
        volumes
            .get()
            .into_iter()
            .map(|v| {
                let (v1, v2) = (v.clone(), v.clone());
                let label = tf("sp.auto_free_on", &[("volume", &crate::places::name(&v))]);
                view! {
                    <label class="check">
                        <input type="checkbox" prop:checked=move || dr.d.with(|s| s.auto_free.contains(&v1))
                            on:change=move |ev| {
                                let on = event_target_checked(&ev);
                                dr.d.update(|s| { s.auto_free.retain(|x| *x != v2); if on { s.auto_free.push(v2.clone()); } });
                            } />
                        <span>{label}</span>
                    </label>
                }
            })
            .collect_view()
    };
    view! {
        <section class="card widget w-12" style="--i:4">
            <h2 class="section-title"><Icon icon=icons::SHIELD_CHECK size=16 />{tr("sp.guardrails")}</h2>
            {move || server.gate(4).unwrap_or_else(|| view! {
                {form::stale_notice(dr.stale, Callback::new(move |_| dr.reset()))}
                <div class="form-row three">
                    {form::switch("sp.notify_low_free", Some("sp.notify_low_free_help"), Signal::derive(move || dr.d.with(|s| s.notify_low_free)), Callback::new(move |v| dr.d.update(|s| s.notify_low_free = v)))}
                    {num_field("sp.low_free_pct", None, |s| u32::from(s.low_free_pct), |s, n| s.low_free_pct = n.min(100) as u8, 50)}
                    {num_field("sp.recover_pct", None, |s| u32::from(s.recover_pct), |s, n| s.recover_pct = n.min(100) as u8, 90)}
                    {form::switch("sp.notify_growth", Some("sp.notify_growth_help"), Signal::derive(move || dr.d.with(|s| s.notify_growth)), Callback::new(move |v| dr.d.update(|s| s.notify_growth = v)))}
                    {num_field("sp.growth_gib", None, |s| s.growth_gib, |s, n| s.growth_gib = n, 100_000)}
                    {num_field("sp.growth_pct", None, |s| s.growth_pct, |s, n| s.growth_pct = n, 1000)}
                </div>
                <div class="field">
                    <span class="field-label">{tr("sp.auto_free")}</span>
                    <p class="small muted">{tr("sp.auto_free_help")}</p>
                    <div class="space-auto">{auto}</div>
                </div>
                <p class="small muted">{move || match journal.get() {
                    (Some(b), true) => tf("sp.journal_admin", &[("size", &i18n::bytes(b))]),
                    (Some(b), false) => tf("sp.journal_in_force", &[("size", &i18n::bytes(b))]),
                    (None, _) => t("sp.journal_default").to_string(),
                }}</p>
                <div class="form-row three">
                    {form::number_field("sp.journal_cap", Some("sp.journal_cap_help"),
                        Signal::derive(move || dr.d.with(|s| s.journal_cap_mib.map(|n| n.to_string()).unwrap_or_default())),
                        Callback::new(move |v: String| dr.d.update(|s| s.journal_cap_mib = v.trim().parse::<u32>().ok())), 0, 1_000_000)}
                    {form::switch("sp.recycle_daily", Some("sp.recycle_daily_help"), Signal::derive(move || dr.d.with(|s| s.recycle_daily)), Callback::new(move |v| dr.d.update(|s| s.recycle_daily = v)))}
                    {num_field("sp.thumbs_cap", None, |s| s.thumbs_cap_mib, |s, n| s.thumbs_cap_mib = n, 1_000_000)}
                </div>
                <div class="row-end space-save">
                    {form::save_bar_with(SaveBar { can_save: dr.dirty(), error: Some(error), ..SaveBar::new(busy, "save", Callback::new(move |_| dr.reset()), save) })}
                </div>
            }.into_any())}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_enum_has_a_text() {
        for c in [SpaceCategory::Snapshots, SpaceCategory::RecycleBins, SpaceCategory::Docker, SpaceCategory::System, SpaceCategory::Leftovers, SpaceCategory::Data] {
            assert!(i18n::try_t(category_key(c)).is_some(), "{c:?}");
        }
        for k in [
            SourceKind::ShareJob,
            SourceKind::OrphanJob,
            SourceKind::AppStack,
            SourceKind::SystemPoints,
            SourceKind::PcBackup,
            SourceKind::Replica,
            SourceKind::Received,
            SourceKind::IncomingReplica,
        ] {
            assert!(i18n::try_t(kind_key(k)).is_some(), "{k:?}");
        }
        for p in [
            Protection::Newest,
            Protection::ReplicationParent,
            Protection::NewestBackup,
            Protection::BootReference,
            Protection::Pinned,
            Protection::Writable,
            Protection::IncomingReplica,
        ] {
            assert!(i18n::try_t(protection_key(p)).is_some(), "{p:?}");
        }
        for a in [
            SafeActionKind::JournalVacuum,
            SafeActionKind::AptClean,
            SafeActionKind::DockerDangling,
            SafeActionKind::RecycleBins,
            SafeActionKind::Thumbnails,
            SafeActionKind::CatalogSerials,
            SafeActionKind::IdenticalAppSnapshots,
        ] {
            let (title, text, slug) = action_key(a);
            assert!(i18n::try_t(title).is_some() && i18n::try_t(text).is_some(), "{a:?}");
            // the path segment is the API's kind
            assert_eq!(serde_json::to_value(a).unwrap(), slug.replace('-', "_"));
        }
        for r in [
            ReviewKind::RollbackCopy,
            ReviewKind::RestoreCopy,
            ReviewKind::AdoptCopy,
            ReviewKind::StackCopy,
            ReviewKind::OrphanSnapshots,
            ReviewKind::RemovedTargetReplicas,
            ReviewKind::RemovedPcFolder,
            ReviewKind::ReceivedUnknownOwner,
            ReviewKind::OldKernel,
            ReviewKind::UnusedImage,
            ReviewKind::UnusedVolume,
        ] {
            assert!(i18n::try_t(review_key(r)).is_some(), "{r:?}");
        }
    }

    #[test]
    fn freed_and_growth_texts() {
        assert_eq!(growth_text(None), ui::DASH);
        assert!(growth_text(Some(2048)).starts_with('+'));
        assert!(growth_text(Some(-2048)).starts_with('−'));
        assert_eq!(freed_text(None, None, MeasureMethod::None), t("sp.not_measured"));
    }
}
