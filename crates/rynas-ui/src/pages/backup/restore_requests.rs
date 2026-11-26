//! Restores waiting for an administrator's approval (`docs/phase2/pc-restore-approval.md`):
//! a restore TUI on a dead PC's live system asked the NAS instead of typing a restore code.
//! Each card shows what the requester claims (the PC), where it came from, the compare code
//! the PC's screen shows too and the certificate it saw; Approve (choosing the PC when the
//! claim matched none or several) or Deny. Shown on top of the Backup page and in the
//! request's notification; it reads the list again on the events `pcbackup.restore_request`
//! and `pcbackup.restore_resolved` (nothing polled).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::pcagent::{RestorablePc, RestoreRequestApprove, RestoreRequestView, restore_code_text};

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

fn card(r: RestoreRequestView, pcs: Remote<Vec<RestorablePc>>, list: Remote<Vec<RestoreRequestView>>, class: &'static str) -> AnyView {
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let matched = r.device.clone();
    let chosen = RwSignal::new(matched.clone().unwrap_or_default());
    let id = StoredValue::new(r.id.clone());
    let decide = move |approve: bool| {
        let device = chosen.get_untracked();
        if approve && device.is_empty() {
            error.set(Some(t("rr.choose_pc").to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        let base = format!("{LIST}/{}", api::encode(&id.get_value()));
        spawn_local(async move {
            let res = if approve {
                api::post_empty(&format!("{base}/approve"), Some(&RestoreRequestApprove { device: Some(device) })).await
            } else {
                api::post_empty::<()>(&format!("{base}/deny"), None).await
            };
            let _ = busy.try_set(false);
            match res {
                Ok(()) => {
                    crate::toast::toast(Tone::Ok, t(if approve { "rr.approved" } else { "rr.denied" }).to_string());
                    list.reload();
                }
                Err(e) => {
                    error.set(Some(e.text()));
                    list.reload();
                }
            }
        });
    };
    let pc_field = match matched {
        Some(d) => ui::kv_text("rr.pc_matched", d, "mono"),
        None => view! {
            <div class="kv">
                <dt>{tr("rr.pc_matched")}</dt>
                <dd>
                    <select class="input" aria-label=tr("rr.pc_matched") on:change=move |ev| chosen.set(event_target_value(&ev))>
                        <option value="" selected=true>{tr("rr.pc_none")}</option>
                        {move || pcs.data.get().unwrap_or_default().into_iter().map(|p| {
                            let label = if p.hostname.is_empty() { p.device.clone() } else { format!("{} ({})", p.device, p.hostname) };
                            view! { <option value=p.device.clone()>{label}</option> }
                        }).collect_view()}
                    </select>
                </dd>
            </div>
        }
        .into_any(),
    };
    let code = restore_code_text(&r.code);
    view! {
        <section class=class>
            {ui::note(Tone::Warn, icons::LIFE_BUOY, ui::text(tf("rr.title", &[("pc", &r.pc)])))}
            <p class="muted small">{tr("rr.intro")}</p>
            <code class="secret mono" aria-label=tr("rr.code")>{code}</code>
            <dl class="kvs">
                {ui::kv_text("rr.pc_claimed", r.pc.clone(), "")}
                {pc_field}
                {ui::kv_text("rr.from", r.from_ip.clone(), "mono")}
                {(!r.fingerprint.is_empty()).then(|| ui::kv("pca.fingerprint", ui::breakable(&r.fingerprint, "mono small")))}
                {ui::kv_text("rr.until", i18n::datetime(r.expires_ms), "tnum")}
            </dl>
            {form_error(error)}
            <div class="sec-actions">
                <button class="btn ghost" disabled=move || busy.get() on:click=move |_| decide(false)><Icon icon=icons::X size=14 />{tr("rr.deny")}</button>
                <button class="btn primary" disabled=move || busy.get() on:click=move |_| decide(true)>{spinner(busy)}<Icon icon=icons::CHECK size=14 />{tr("rr.approve")}</button>
            </div>
        </section>
    }
    .into_any()
}
