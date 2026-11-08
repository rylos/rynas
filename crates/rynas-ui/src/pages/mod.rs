//! Routed pages.

pub mod apps;
pub mod backup;
pub mod config;
pub mod dashboard;
pub mod docker;
pub mod files;
pub mod hardware;
pub mod notifications;
pub mod scheduler;
pub mod network;
pub mod replication;
pub mod security;
pub mod services;
pub mod share_crypt;
pub mod share_folders;
pub mod share_nocow;
pub mod shares;
pub mod snapshots;
pub mod storage;
pub mod system;
pub mod updates;
pub mod user_homes;
pub mod user_ids;
pub mod users;
pub mod web_access;

use leptos::prelude::*;

use crate::i18n::{t, tr};
use crate::icons;
use crate::ui::EmptyState;

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
