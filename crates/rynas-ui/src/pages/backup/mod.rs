//! Backup: one tab per kind of backup. [`restic`] is the backup of this NAS to another NAS,
//! a disk or the cloud (D-BACKUP); [`pcs`] receives the btrbk snapshots PCs push here
//! (D-PCBACKUP).

pub mod pcs;
mod restic;

use leptos::prelude::*;

use crate::form::tabs;
use crate::icons;
use crate::ui::PageHeader;

const TABS: &[&str] = &["bk.tab.restic", "bk.tab.pcs"];

#[component]
pub fn BackupPage() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    let edit = RwSignal::new(None::<Option<rynas_api_types::backup::BackupJobView>>);
    view! {
        <div class="page">
            <PageHeader icon=icons::ARCHIVE_RESTORE title="bk.title" subtitle="bk.subtitle">
                {move || (tab.get() == 0).then(|| restic::create_button(edit))}
            </PageHeader>
            <section class="card widget w-12">
                {tabs("bkp", TABS, tab, "bk.title")}
                <div role="tabpanel" id="bkp-p0" aria-labelledby="bkp-t0" hidden=move || tab.get() != 0>{restic::panel(edit)}</div>
                <div role="tabpanel" id="bkp-p1" aria-labelledby="bkp-t1" hidden=move || tab.get() != 1>{pcs::panel()}</div>
            </section>
        </div>
    }
}
