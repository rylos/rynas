//! No copy-on-write for a shared folder on btrfs: the option when a new subvolume is created,
//! and for an existing folder its state, turning it on (empty folder only) or off as task
//! `shares.nocow`, and the manual way to move files into a folder without copy-on-write.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::{Share, ShareNocowRequest, ShareStatus, TaskStatus};

use crate::api;
use crate::form::switch;
use crate::i18n::{t, tf, tr};
use crate::icons;
use crate::toast::toast;
use crate::ui::{self, Confirm, Tone};

/// The warning with what a folder without copy-on-write gives up (also for a subvolume
/// created on the storage page).
pub fn costs_note() -> AnyView {
    ui::note(Tone::Warn, icons::TRIANGLE_ALERT, costs())
}

/// What a folder without copy-on-write gives up, as a list.
fn costs() -> AnyView {
    let keys = ["shares.nocow_t_sum", "shares.nocow_t_comp", "shares.nocow_t_snap"];
    view! {
        <span>{tr("shares.nocow_cost")}</span>
        <ul class="nocow-costs">{keys.into_iter().map(|k| view! { <li>{tr(k)}</li> }).collect_view()}</ul>
    }
    .into_any()
}

/// The switch of a new subvolume; compression is hidden while it is on (it does not apply).
pub fn create_switch(d: RwSignal<Share>) -> AnyView {
    let on = Signal::derive(move || d.with(|s| s.subvolume.as_ref().is_some_and(|v| v.nocow)));
    let set = Callback::new(move |v: bool| {
        d.update(|s| {
            if let Some(sv) = s.subvolume.as_mut() {
                sv.nocow = v;
            }
        })
    });
    view! {
        {switch("shares.nocow", Some("shares.nocow_help"), on, set)}
        {move || on.get().then(costs_note)}
    }
    .into_any()
}

/// Steps to move existing files into a folder without copy-on-write (not done by rynas: it
/// needs the apps stopped and twice the space for a while).
fn move_steps() -> AnyView {
    view! {
        {ui::fold(None, "shares.nocow_move", false, None, view! { 
            <ol class="steps-list small">
                <li>{tr("shares.nocow_move_1")}</li>
                <li>{tr("shares.nocow_move_2")}</li>
                <li>{move || ui::text_code(t("shares.nocow_move_3"))}</li>
                <li>{tr("shares.nocow_move_4")}</li>
            </ol>
            <p class="small muted">{tr("shares.nocow_move_space")}</p>
         }.into_any())}
    }
    .into_any()
}

/// The state of an existing share folder on btrfs, with the action that changes it; nothing
/// for other filesystems, a folder not created yet or a state that cannot be read.
pub fn existing(name: String, st: &ShareStatus) -> Option<AnyView> {
    let state = st.nocow.filter(|_| st.path_ok && st.fs_type == "btrfs")?;
    let on = RwSignal::new(state);
    let ask = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let n = StoredValue::new(name);
    let go = Callback::new(move |_| {
        let want = !on.get_untracked();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let name = n.get_value();
            let url = format!("/api/v1/shares/{}/nocow", api::encode(&name));
            let body = serde_json::to_string(&ShareNocowRequest { enabled: want }).ok();
            // the dialog stays open until the task has finished
            let r = crate::tasks::run_task(&url, body).await;
            let _ = busy.try_set(false);
            match r {
                Ok(task) if task.status == TaskStatus::Ok => {
                    let _ = on.try_set(want);
                    let _ = ask.try_set(false);
                    toast(Tone::Ok, tf(if want { "shares.nocow_done_on" } else { "shares.nocow_done_off" }, &[("name", &name)]));
                    crate::config::changed();
                }
                Ok(task) => {
                    let _ = error.try_set(Some(crate::tasks::task_error(&task)));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
        });
    });
    let dialog = move || {
        ask.get().then(|| {
            let want = !on.get_untracked();
            let name = n.get_value();
            let mut c = Confirm::new(
                tf(if want { "shares.nocow_on_title" } else { "shares.nocow_off_title" }, &[("name", &name)]),
                t(if want { "shares.nocow_on_text" } else { "shares.nocow_off_text" }).to_string(),
                if want { "shares.nocow_turn_on" } else { "shares.nocow_turn_off" },
                false,
                go,
                Callback::new(move |_| ask.set(false)),
            );
            c.busy = busy;
            c.error = error;
            // turning it on needs an empty folder: no line about files already there
            c.extra = want.then(costs_note);
            ui::confirm(c)
        })
    };
    Some(
        view! {
            <fieldset class="fieldset">
                <legend class="field-label">{tr("shares.nocow")}</legend>
                <div class="nocow-row">
                    {move || if on.get() { ui::status(Tone::Info, t("on").to_string()) } else { ui::status(Tone::Ok, t("off").to_string()) }}
                    <button type="button" class="btn ghost sm" on:click=move |_| { error.set(None); ask.set(true); }>
                        {move || tr(if on.get() { "shares.nocow_turn_off" } else { "shares.nocow_turn_on" })}
                    </button>
                </div>
                <span class="field-help">{move || tr(if on.get() { "shares.nocow_is_on" } else { "shares.nocow_help_existing" })}</span>
                {move || on.get().then(|| view! { <span class="field-help">{tr("shares.nocow_snap_hint")}</span> })}
                {move || (!on.get()).then(move_steps)}
            </fieldset>
            {dialog}
        }
        .into_any(),
    )
}
