//! Routed pages.

pub mod config;
pub mod dashboard;
pub mod hardware;
pub mod notifications;
pub mod scheduler;
pub mod network;
pub mod security;
pub mod services;
pub mod share_folders;
pub mod shares;
pub mod snapshots;
pub mod storage;
pub mod system;
pub mod updates;
pub mod users;

use leptos::prelude::*;

use crate::i18n::{t, tr};
use crate::icons::{self, Icon};
use crate::ui::EmptyState;

/// Placeholder for sections that are not built yet.
#[component]
pub fn Soon(icon: &'static str, title: &'static str, text: &'static str) -> impl IntoView {
    view! {
        <div class="page">
            <header class="page-head">
                <div class="page-icon"><Icon icon=icon size=20 /></div>
                <div><h1>{move || t(title)}</h1></div>
            </header>
            <div class="card soon">
                <EmptyState icon=icon title=Signal::derive(move || t("soon").to_string()) text=Signal::derive(move || t(text).to_string())>
                    <span class="chip accent2"><Icon icon=icons::SPARKLES size=14 />{tr("soon")}</span>
                </EmptyState>
            </div>
        </div>
    }
}

#[component]
pub fn NotFound() -> impl IntoView {
    view! {
        <div class="page">
            <div class="card soon">
                <EmptyState
                    icon=icons::CIRCLE_DASHED
                    title=Signal::derive(move || t("notfound").to_string())
                    text=Signal::derive(move || t("notfound.text").to_string())
                >
                    <a class="btn primary" href="/">{tr("back_home")}</a>
                </EmptyState>
            </div>
        </div>
    }
}
