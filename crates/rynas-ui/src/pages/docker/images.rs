//! Docker › Images & volumes: read-only lists with their sizes.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::docker::{ImagePrunePlan, ImagePruneRequest, ImageSummary, VolumeSummary};

use super::{done_toast, empty, image_ref, remote};
use crate::api;
use crate::form::{SaveBar, save_bar_with};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::tasks;
use crate::ui::{self, DlgKind, Remote, WidgetHead};

/// `sha256:0123456789ab…` → `0123456789ab`.
fn short_id(id: &str) -> String {
    id.strip_prefix("sha256:").unwrap_or(id).chars().take(12).collect()
}

/// "Used by n" / dangling / unused.
fn use_pill(n: Option<u32>, dangling: bool) -> AnyView {
    match n {
        Some(n) if n > 0 => ui::status(ui::Tone::Ok, tf("dk.i.used_by", &[("n", &n.to_string())])),
        Some(_) if dangling => ui::status(ui::Tone::Warn, t("dk.i.dangling").to_string()),
        Some(_) => ui::status(ui::Tone::Unknown, t("dk.i.unused").to_string()),
        None => ui::text(ui::DASH.into()),
    }
}

fn image_row(i: &ImageSummary) -> AnyView {
    let tags = if i.tags.is_empty() { vec![t("dk.i.untagged").to_string()] } else { i.tags.clone() };
    ui::row(vec![
        ui::cell("dk.col.image", "wrap", view! {
            <div class="cell-main">
                {tags.iter().map(|tg| image_ref(tg)).collect_view()}
                <span class="cell-sub mono">{short_id(&i.id)}</span>
            </div>
        }.into_any()),
        ui::text_cell("dk.col.size", "num", i18n::bytes(i.size_bytes)),
        ui::text_cell("dk.col.created", "", i18n::datetime(i.created_s.max(0) as u64 * 1000)),
        ui::cell("col.status", "", use_pill(Some(i.containers.max(0) as u32), i.dangling)),
    ])
}

fn volume_row(v: &VolumeSummary) -> AnyView {
    ui::row(vec![
        ui::cell("dk.col.volume", "wrap", ui::two_lines(view! { <span class="mono small">{v.name.clone()}</span> }.into_any(), Some(v.driver.clone()), "cell-sub")),
        ui::text_cell("dk.col.stack", "", v.stack.clone().unwrap_or_else(|| ui::DASH.into())),
        ui::text_cell("dk.col.size", "num", v.size_bytes.map(i18n::bytes).unwrap_or_else(|| ui::DASH.into())),
        ui::cell("col.status", "", use_pill(v.in_use, false)),
    ])
}

/// "Remove unused": what goes (the daemon's preview), then the removal as a task.
fn prune_dialog(images: Remote<Vec<ImageSummary>>, close: Callback<()>) -> AnyView {
    const URL: &str = "/api/v1/docker/images/prune";
    let plan = RwSignal::new(None::<Result<ImagePrunePlan, String>>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    spawn_local(async move {
        let r = api::post::<_, ImagePrunePlan>(URL, Some(&ImagePruneRequest { dry_run: true })).await;
        let _ = plan.try_set(Some(r.map_err(|e| e.text())));
    });
    let count = move || plan.with(|p| p.as_ref().and_then(|p| p.as_ref().ok()).map_or(0, |p| p.images.len()));
    let go = move |_| {
        busy.set(true);
        spawn_local(async move {
            let body = serde_json::to_string(&ImagePruneRequest { dry_run: false }).ok();
            let r = tasks::run_task(URL, body).await;
            let _ = busy.try_set(false);
            done_toast(r);
            images.reload();
            close.run(());
        });
    };
    let body = move || match plan.get() {
        None => ui::loading(Signal::stored(None), Callback::new(|_| ()), 3),
        Some(Err(e)) => ui::note(ui::Tone::Err, icons::CIRCLE_X, ui::text(e)),
        Some(Ok(p)) if p.images.is_empty() => ui::empty_line("dk.i.prune_none"),
        Some(Ok(p)) => {
            let rows = p
                .images
                .iter()
                .map(|i| {
                    let name = i.tags.first().or(i.digests.first()).cloned().unwrap_or_else(|| short_id(&i.id));
                    view! { <li class="kv"><span class="mono">{name}</span><span class="num">{i18n::bytes(i.size_bytes)}</span></li> }
                })
                .collect_view();
            view! {
                <p class="small">{tf("dk.i.prune_total", &[("n", &p.images.len().to_string()), ("size", &i18n::bytes(p.bytes))])}</p>
                <ul class="plain-list prune-list">{rows}</ul>
            }
            .into_any()
        }
    };
    let body = view! {
        <p class="small muted">{t("dk.i.prune_text")}</p>
        {body}
    }
    .into_any();
    let foot = save_bar_with(SaveBar {
        can_save: Signal::derive(move || count() > 0),
        icon: Some(icons::TRASH_2),
        danger: Signal::stored(true),
        error: Some(error),
        ..SaveBar::new(busy, "dk.i.prune_go", close, Callback::new(go))
    });
    ui::dialog(DlgKind::Normal, Signal::stored(t("dk.i.prune_title").to_string()), close, body, Some(foot))
}

pub fn panel() -> AnyView {
    let images = remote::<Vec<ImageSummary>>("/api/v1/docker/images", 3);
    let pruning = RwSignal::new(false);
    let volumes = remote::<Vec<VolumeSummary>>("/api/v1/docker/volumes", 3);
    let total = move |sizes: Vec<u64>| tf("dk.i.total", &[("n", &sizes.len().to_string()), ("size", &i18n::bytes(sizes.iter().sum()))]);
    let image_rows = move || {
        images.data.with(|l| match l.as_deref() {
            None | Some([]) => empty(icons::LAYERS, "dk.i.none", None),
            Some(l) => ui::table("dk-table", &["dk.col.image", "#dk.col.size", "dk.col.created", "col.status"], l.iter().map(image_row).collect()),
        })
    };
    let volume_rows = move || {
        volumes.data.with(|l| match l.as_deref() {
            None | Some([]) => empty(icons::HARD_DRIVE, "dk.v.none", None),
            Some(l) => ui::table("dk-table", &["dk.col.volume", "dk.col.stack", "#dk.col.size", "col.status"], l.iter().map(volume_row).collect()),
        })
    };
    view! {
        <div class="grid">
            <section class="card widget w-12">
                <WidgetHead icon=icons::LAYERS title="dk.i.title">
                    <span class="w-meta">{move || images.data.with(|l| l.as_ref().map(|l| total(l.iter().map(|i| i.size_bytes).collect())))}</span>
                    <button type="button" class="btn ghost sm" on:click=move |_| pruning.set(true)
                        disabled=move || images.data.with(|l| !l.as_ref().is_some_and(|l| l.iter().any(|i| i.containers <= 0)))>
                        <Icon icon=icons::TRASH_2 size=14 /><span class="btn-label">{tr("dk.i.prune")}</span>
                    </button>
                </WidgetHead>
                {move || images.view(5, image_rows)}
            </section>
            {move || pruning.get().then(|| prune_dialog(images, Callback::new(move |_| pruning.set(false))))}
            <section class="card widget w-12">
                <WidgetHead icon=icons::HARD_DRIVE title="dk.v.title">
                    <span class="w-meta">{move || volumes.data.with(|l| l.as_ref().map(|l| total(l.iter().filter_map(|v| v.size_bytes).collect())))}</span>
                </WidgetHead>
                {move || volumes.view(4, volume_rows)}
            </section>
        </div>
    }
    .into_any()
}
