//! Docker › Images & volumes: read-only lists with their sizes.

use leptos::prelude::*;
use rynas_api_types::docker::{ImageSummary, VolumeSummary};

use super::remote;
use crate::i18n::{self, t, tf};
use crate::icons;
use crate::ui::{self, EmptyState, WidgetHead};

/// `sha256:0123456789ab…` → `0123456789ab`.
fn short_id(id: &str) -> String {
    id.strip_prefix("sha256:").unwrap_or(id).chars().take(12).collect()
}

pub fn panel() -> AnyView {
    let images = remote::<Vec<ImageSummary>>("/api/v1/docker/images");
    let volumes = remote::<Vec<VolumeSummary>>("/api/v1/docker/volumes");
    let total = move |sizes: Vec<u64>| tf("dk.i.total", &[("n", &sizes.len().to_string()), ("size", &i18n::bytes(sizes.iter().sum()))]);
    let image_rows = move || {
        let l = images.data.get().unwrap_or_default();
        if l.is_empty() {
            return view! { <EmptyState icon=icons::LAYERS title=Signal::derive(|| t("dk.i.none").to_string()) text=Signal::derive(String::new) /> }.into_any();
        }
        let rows = l
            .into_iter()
            .map(|i| {
                let tags = if i.tags.is_empty() { vec![t("dk.i.untagged").to_string()] } else { i.tags.clone() };
                ui::row(vec![
                    ui::cell("dk.col.image", "wrap", view! {
                        <div class="cell-main">
                            {tags.into_iter().map(|tg| view! { <span class="mono small">{tg}</span> }).collect_view()}
                            <span class="cell-sub mono">{short_id(&i.id)}</span>
                        </div>
                    }.into_any()),
                    ui::cell("dk.col.size", "num", ui::text(i18n::bytes(i.size_bytes))),
                    ui::cell("dk.col.created", "", ui::text(i18n::datetime(i.created_s.max(0) as u64 * 1000))),
                    ui::cell("col.status", "", if i.containers > 0 {
                        ui::status(ui::Tone::Ok, tf("dk.i.used_by", &[("n", &i.containers.to_string())]))
                    } else if i.dangling {
                        ui::status(ui::Tone::Warn, t("dk.i.dangling").to_string())
                    } else {
                        ui::status(ui::Tone::Unknown, t("dk.i.unused").to_string())
                    }),
                ])
            })
            .collect();
        ui::table("dk-table", &["dk.col.image", "#dk.col.size", "dk.col.created", "col.status"], rows)
    };
    let volume_rows = move || {
        let l = volumes.data.get().unwrap_or_default();
        if l.is_empty() {
            return view! { <EmptyState icon=icons::HARD_DRIVE title=Signal::derive(|| t("dk.v.none").to_string()) text=Signal::derive(String::new) /> }.into_any();
        }
        let rows = l
            .into_iter()
            .map(|v| {
                ui::row(vec![
                    ui::cell("dk.col.volume", "wrap", view! { <div class="cell-main"><span class="mono small">{v.name.clone()}</span><span class="cell-sub">{v.driver.clone()}</span></div> }.into_any()),
                    ui::cell("dk.col.stack", "", ui::text(v.stack.clone().unwrap_or_else(|| "—".into()))),
                    ui::cell("dk.col.size", "num", ui::text(v.size_bytes.map(i18n::bytes).unwrap_or_else(|| "—".into()))),
                    ui::cell("col.status", "", match v.in_use {
                        Some(n) if n > 0 => ui::status(ui::Tone::Ok, tf("dk.i.used_by", &[("n", &n.to_string())])),
                        Some(_) => ui::status(ui::Tone::Unknown, t("dk.i.unused").to_string()),
                        None => ui::text("—".into()),
                    }),
                ])
            })
            .collect();
        ui::table("dk-table", &["dk.col.volume", "dk.col.stack", "#dk.col.size", "col.status"], rows)
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
