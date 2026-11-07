//! Plugins: the modules rynas can add. Today the Docker module (D-DOCKER): its card installs
//! Docker where it is missing and leads to the Docker page once it is there. The Apps &
//! Plugins catalogue (D-APPCAT) comes later on this page.

use leptos::prelude::*;

use crate::i18n::{t, tr};
use crate::icons::{self, Icon};
use crate::ui::{EmptyState, PageHeader};

#[component]
pub fn Plugins() -> impl IntoView {
    let d = crate::docker::docker();
    d.refresh();
    let known = Memo::new(move |_| d.status.with(Option::is_some));
    view! {
        <div class="page">
            <PageHeader icon=icons::PUZZLE title="nav.plugins" subtitle="plg.subtitle" />
            <div class="grid">
                {move || if known.get() {
                    super::docker::install_card()
                } else {
                    view! { <div class="card w-12">{crate::ui::loading(d.status_err.into(), Callback::new(move |_| d.refresh()), 4)}</div> }.into_any()
                }}
                <div class="card soon w-12">
                    <EmptyState icon=icons::PUZZLE title=Signal::derive(move || t("soon").to_string()) text=Signal::derive(move || t("soon.plugins").to_string())>
                        <span class="chip accent2"><Icon icon=icons::SPARKLES size=14 />{tr("soon")}</span>
                    </EmptyState>
                </div>
            </div>
        </div>
    }
}
