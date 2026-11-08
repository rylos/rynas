//! Backup: one tab per kind of backup the NAS keeps. Today "PCs" (btrbk snapshots PCs push
//! here, [`pcs`]); other kinds add a tab key and a panel below.

pub mod pcs;

use leptos::prelude::*;

use crate::form::tabs;
use crate::icons;
use crate::ui::PageHeader;

#[component]
pub fn Backup() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    view! {
        <div class="page">
            <PageHeader icon=icons::ARCHIVE_RESTORE title="bk.title" subtitle="bk.subtitle" />
            <section class="card widget w-12">
                {tabs("bkp", &["bk.tab.pcs"], tab, "bk.title")}
                <div role="tabpanel" id="bkp-p0" aria-labelledby="bkp-t0" hidden=move || tab.get() != 0>{pcs::panel()}</div>
            </section>
        </div>
    }
}
