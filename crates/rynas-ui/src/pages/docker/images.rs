//! Docker › Images & volumes: read-only lists with their sizes.

use leptos::prelude::*;
use rynas_api_types::docker::{ImageSummary, VolumeSummary};

use super::{empty, image_ref, remote};
use crate::i18n::{self, t, tf};
use crate::icons;
use crate::ui::{self, WidgetHead};

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

pub fn panel() -> AnyView {
    let images = remote::<Vec<ImageSummary>>("/api/v1/docker/images", 3);
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
                </WidgetHead>
                {move || images.view(5, image_rows)}
            </section>
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
