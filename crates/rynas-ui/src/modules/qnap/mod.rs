//! The QNAP module of the web interface (D-MODULE): the Hardware › QNAP tab, shown only where
//! the module's API answers (an identified machine, `docs/plugins/qnap.md`).

pub mod i18n;
mod page;

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::i18n::t;
use crate::live::use_live;

pub use page::panel;

/// Label key of the Hardware tab.
pub const TAB: &str = "qnap.tab";

/// The module is present: the live provider is the module's, or its status answers. Probed once
/// per visit, only when the provider does not already tell.
pub fn present() -> Memo<bool> {
    let hw = use_live().hardware;
    let probed = RwSignal::new(false);
    let asked = StoredValue::new(false);
    Effect::new(move || {
        let Some(provider) = hw.with(|h| h.as_ref().map(|h| h.provider.clone())) else { return };
        if provider.as_deref() == Some("qnap") || asked.get_value() {
            return;
        }
        asked.set_value(true);
        spawn_local(async move {
            let ok = crate::api::get_text("/api/v1/modules/qnap/status").await.is_ok();
            probed.set(ok);
        });
    });
    Memo::new(move |_| probed.get() || hw.with(|h| h.as_ref().is_some_and(|h| h.provider.as_deref() == Some("qnap"))))
}

/// The module's notification sources.
pub fn source_label(source: &str) -> Option<&'static str> {
    source.starts_with("qnap:").then(|| t("nt.src.qnap"))
}
