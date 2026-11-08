//! Backup: copies of this NAS' data elsewhere. Each kind of backup is a tab with its own
//! module; [`restic`] is the backup to another NAS, a disk or the cloud (D-BACKUP).

mod restic;

use leptos::prelude::*;

use crate::form::tabs;
use crate::icons;
use crate::ui::PageHeader;

const TABS: &[&str] = &["bk.tab.restic"];

#[component]
pub fn BackupPage() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    let edit = RwSignal::new(None::<Option<rynas_api_types::backup::BackupJobView>>);
    view! {
        <div class="page">
            <PageHeader icon=icons::ARCHIVE_RESTORE title="bk.title" subtitle="bk.subtitle">
                {restic::create_button(edit)}
            </PageHeader>
            <section class="card widget w-12">
                {tabs("bkp", TABS, tab, "bk.title")}
                <div role="tabpanel" id="bkp-p0" aria-labelledby="bkp-t0" hidden=move || tab.get() != 0>{restic::panel(edit)}</div>
            </section>
        </div>
    }
}
