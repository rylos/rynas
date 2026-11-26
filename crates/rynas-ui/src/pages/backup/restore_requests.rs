//! Restores waiting for an administrator's approval (`docs/phase2/pc-restore-approval.md`):
//! a restore TUI on a dead PC's live system asked the NAS instead of typing a restore code.
//! Each card shows what the requester claims (the PC), where it came from, red warnings
//! (plain HTTP, other requests for the same PC), the PC whose backups it may read (a match
//! from the claim preselected and said to be one); the approver types the six digits the PC's
//! screen shows and approves, or denies (also blocking the address for an hour). Shown on top of the Backup page and in the
//! request's notification; it reads the list again on the events `pcbackup.restore_request`
//! and `pcbackup.restore_resolved` (nothing polled).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::pcagent::{RestorablePc, RestoreRequestApprove, RestoreRequestDeny, RestoreRequestView};

use crate::api;
use crate::form::{form_error, spinner};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Remote, Tone};

const LIST: &str = "/api/v1/pcbackup/restore-requests";
const PCS: &str = "/api/v1/pcbackup/devices";

/// The pending requests (`only`: just that one, as in its notification); nothing when none.
pub fn restore_requests(only: Option<String>, class: &'static str) -> AnyView {
    let list = Remote::<Vec<RestoreRequestView>>::new(LIST);
    let pcs = Remote::<Vec<RestorablePc>>::new(PCS);
    let live = crate::live::use_live();
    // pushed: a new request or one that ended
    Effect::new(move |prev: Option<u64>| {
        let g = live.pcbackup_gen.get();
        if prev.is_some_and(|p| p != g) {
            list.reload();
            pcs.reload();
        }
        g
    });
    let only = StoredValue::new(only);
    let cards = move || {
        let items: Vec<RestoreRequestView> = list
            .data
            .get()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| only.with_value(|o| o.as_ref().is_none_or(|id| *id == r.id)))
            .collect();
        items.into_iter().map(|r| card(r, pcs, list, class)).collect_view()
    };
    view! { {cards} }.into_any()
}

/// What a card's button does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Approve,
    Deny,
    DenyAndBlock,
}

fn card(r: RestoreRequestView, pcs: Remote<Vec<RestorablePc>>, list: Remote<Vec<RestoreRequestView>>, class: &'static str) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let suggested = r.device.clone();
    let chosen = RwSignal::new(suggested.clone().unwrap_or_default());
    // the approver types what the PC's screen shows: the NAS never tells the code
    let typed = RwSignal::new(String::new());
    let id = StoredValue::new(r.id.clone());
    let decide = move |d: Decision| {
        let device = chosen.get_untracked();
        let code: String = typed.get_untracked().chars().filter(char::is_ascii_digit).collect();
        if d == Decision::Approve {
            if code.len() != 6 {
                error.set(Some(t("rr.type_code").to_string()));
                return;
            }
            if device.is_empty() {
                error.set(Some(t("rr.choose_pc").to_string()));
                return;
            }
        }
        busy.set(true);
        error.set(None);
        let base = format!("{LIST}/{}", api::encode(&id.get_value()));
        spawn_local(async move {
            let res = match d {
                Decision::Approve => api::post_empty(&format!("{base}/approve"), Some(&RestoreRequestApprove { code, device: Some(device) })).await,
                Decision::Deny => api::post_empty::<()>(&format!("{base}/deny"), None).await,
                Decision::DenyAndBlock => api::post_empty(&format!("{base}/deny"), Some(&RestoreRequestDeny { block: true })).await,
            };
            let _ = busy.try_set(false);
            match res {
                Ok(()) => {
                    let key = match d {
                        Decision::Approve => "rr.approved",
                        Decision::Deny => "rr.denied",
                        Decision::DenyAndBlock => "rr.denied_blocked",
                    };
                    crate::toast::toast(Tone::Ok, t(key).to_string());
                    list.reload();
                }
                Err(e) => {
                    error.set(Some(e.text()));
                    list.reload();
                }
            }
        });
    };
    // always a choice; a match from the claimed name is preselected and said to be one
    let pc_field = view! {
        <div class="kv">
            <dt>{tr("rr.pc_matched")}</dt>
            <dd>
                <select class="input" aria-label=tr("rr.pc_matched") prop:value=move || chosen.get() on:change=move |ev| chosen.set(event_target_value(&ev))>
                    <option value="">{tr("rr.pc_none")}</option>
                    {move || pcs.data.get().unwrap_or_default().into_iter().map(|p| {
                        let label = if p.hostname.is_empty() { p.device.clone() } else { format!("{} ({})", p.device, p.hostname) };
                        view! { <option value=p.device.clone()>{label}</option> }
                    }).collect_view()}
                </select>
                {suggested.as_ref().map(|_| view! { <p class="muted small">{tr("rr.suggested")}</p> })}
            </dd>
        </div>
    };
    view! {
        <section class=class>
            {ui::note(Tone::Warn, icons::LIFE_BUOY, ui::text(tf("rr.title", &[("pc", &r.pc)])))}
            {(!r.tls).then(|| ui::note(Tone::Err, icons::SHIELD_ALERT, ui::text(t("rr.http").to_string())))}
            {(r.same_pc > 0).then(|| ui::note(Tone::Err, icons::TRIANGLE_ALERT, ui::text(tf("rr.same_pc", &[("n", &r.same_pc.to_string())]))))}
            <p class="muted small">{tr("rr.intro")}</p>
            <dl class="kvs">
                {ui::kv_text("rr.pc_claimed", r.pc.clone(), "")}
                {pc_field}
                {ui::kv_text("rr.from", r.from_ip.clone(), "mono")}
                {ui::kv_text("rr.until", i18n::datetime(r.expires_ms), "tnum")}
                <div class="kv">
                    <dt>{tr("rr.code")}</dt>
                    <dd>
                        <input class="input mono" inputmode="numeric" autocomplete="off" maxlength="7" placeholder="000 000" aria-label=tr("rr.code")
                            prop:value=move || typed.get() on:input=move |ev| typed.set(event_target_value(&ev)) />
                    </dd>
                </div>
            </dl>
            {form_error(error)}
            <div class="sec-actions">
                <button class="btn ghost" disabled=move || busy.get() on:click=move |_| decide(Decision::DenyAndBlock)><Icon icon=icons::SHIELD_ALERT size=14 />{tr("rr.deny_block")}</button>
                <button class="btn ghost" disabled=move || busy.get() on:click=move |_| decide(Decision::Deny)><Icon icon=icons::X size=14 />{tr("rr.deny")}</button>
                <button class="btn primary" disabled=move || busy.get() on:click=move |_| decide(Decision::Approve)>{spinner(busy)}<Icon icon=icons::CHECK size=14 />{tr("rr.approve")}</button>
            </div>
        </section>
    }
    .into_any()
}
