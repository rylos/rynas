//! Hardware › Removable drives: what is plugged in now (the same cards as the app bar menu)
//! and the settings of the configuration key `removable` (auto-mount, read-only mounts,
//! safe-removal notifications, network sharing). Save applies the key at once.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::removable::{RemovableSettings, RemovableSettingsState, ShareAccess};

use crate::api;
use crate::form::{In, Draft, segmented, spinner, stale_notice, switch, text_field};
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::live::use_live;
use crate::toast::saved_applied;
use crate::ui::{Remote, WidgetHead};

const SETTINGS: &str = "/api/v1/removable/settings";
const ACCESS: &[(&str, &str)] = &[("read_write", "rm.set.rw"), ("read_only", "perm.ro")];

pub fn panel(_active: Signal<bool>) -> AnyView {
    let live = use_live();
    if live.removable.with_untracked(Option::is_none) {
        live.refresh_removable();
    }
    let settings = Remote::<RemovableSettingsState>::with_config(SETTINGS);
    let root = move || live.removable.with(|r| r.as_ref().map(|r| r.mount_root.clone()).unwrap_or_else(|| "/media".into()));
    view! {
        <div class="grid">
            <section class="card widget w-6">
                <WidgetHead icon=icons::USB title="rm.drives_now" />
                <p class="sec-note"><Icon icon=icons::INFO size=16 />{move || tf("rm.set.where", &[("root", &root())])}</p>
                {crate::removable::drives_view()}
            </section>
            <section class="card widget w-6">
                <WidgetHead icon=icons::SETTINGS title="rm.settings" />
                {settings.view(6, move || settings.data.get().map(|s| form(settings, &s)).unwrap_or_else(|| ().into_any()))}
            </section>
        </div>
    }
    .into_any()
}

fn form(settings: Remote<RemovableSettingsState>, st: &RemovableSettingsState) -> AnyView {
    let smb = st.smb_available;
    let init = st.pending.clone().unwrap_or_else(|| st.applied.clone());
    let draft = Draft::new(init, move || settings.data.with(|s| s.as_ref().map(|s| s.pending.clone().unwrap_or_else(|| s.applied.clone()))));
    let d = draft.d;
    let dirty = draft.dirty();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let group_err = RwSignal::new(None::<String>);
    let save = move || {
        let body: RemovableSettings = d.get_untracked();
        busy.set(true);
        error.set(None);
        group_err.set(None);
        spawn_local(async move {
            match api::put::<_, RemovableSettingsState>(SETTINGS, &body).await {
                Ok(_) => {
                    if crate::config::apply_now(&[crate::config::keys::REMOVABLE], error).await {
                        saved_applied();
                    }
                }
                Err(e) if e.field().is_some_and(|f| f.ends_with("group")) => group_err.set(Some(e.text())),
                Err(e) => error.set(Some(e.text())),
            }
            let _ = busy.try_set(false);
        });
    };
    let reset = Callback::new(move |_| {
        draft.reset();
        error.set(None);
        group_err.set(None);
    });
    let b = move |f: fn(&RemovableSettings) -> bool| Signal::derive(move || d.with(f));
    let share_on = b(|s| s.share.enabled);
    view! {
        {switch("rm.set.auto_mount", Some("rm.set.auto_mount_help"), b(|s| s.auto_mount), Callback::new(move |v| d.update(|s| s.auto_mount = v)))}
        {switch("rm.set.read_only", Some("rm.set.read_only_help"), b(|s| s.read_only), Callback::new(move |v| d.update(|s| s.read_only = v)))}
        {switch("rm.set.notify", Some("rm.set.notify_help"), b(|s| s.notify_safe_removal), Callback::new(move |v| d.update(|s| s.notify_safe_removal = v)))}
        {text_field(
            "rm.set.group",
            Signal::derive(move || d.with(|s| s.share.group.clone())),
            Callback::new(move |v: String| d.update(|s| s.share.group = v.trim().to_string())),
            group_err.into(),
            In { help: Some("rm.set.group_help"), maxlength: 32, ..In::default() },
        )}
        {(!smb).then(|| view! { <p class="sec-note"><Icon icon=icons::INFO size=16 />{tr("rm.set.no_smb")}</p> })}
        {switch("rm.set.share", Some("rm.set.share_help"), share_on, Callback::new(move |v| d.update(|s| s.share.enabled = v)))}
        <Show when=move || share_on.get()>
            {segmented(
                "rm.set.access",
                ACCESS,
                Signal::derive(move || d.with(|s| if s.share.access == ShareAccess::ReadOnly { "read_only" } else { "read_write" }.to_string())),
                Callback::new(move |v: String| d.update(|s| s.share.access = if v == "read_only" { ShareAccess::ReadOnly } else { ShareAccess::ReadWrite })),
            )}
        </Show>
        {stale_notice(draft.stale, reset)}
        {crate::form::form_error(error)}
        <div class="btn-row">
            <button type="button" class="btn ghost" disabled=move || !dirty.get() on:click=move |_| reset.run(())>{tr("reset")}</button>
            <button type="button" class="btn primary" disabled=move || busy.get() || !dirty.get() on:click=move |_| save()>
                {spinner(busy)}{move || t("save")}
            </button>
        </div>
    }
    .into_any()
}
