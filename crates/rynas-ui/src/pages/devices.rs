//! Mobile devices (`docs/phase2/mobile-devices.md` §6): the paired phones (rename inline,
//! revoke with a confirmation; administrators see every user's) and "Pair a device": a QR
//! code with a countdown, then the claiming phone and its compare code, approved or rejected
//! with an editable name. A pairing needs a password check in the last 10 minutes: the dialog
//! asks for it when the server says so. Above the list and at the top of the pairing dialog,
//! where to get the Android app ([`crate::android_app`]).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::devices::{Device, DeviceRename, PAIRING_TTL_S, PairingApprove, PairingClaim, PairingCreated, PairingState, PairingStatus};
use rynas_api_types::{ReauthRequest, error_codes};

use crate::api;
use crate::auth::{auth, countdown, mmss};
use crate::form::{form_error, spinner};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::qr::QrSvg;
use crate::toast::toast;
use crate::ui::{self, Confirm, EmptyState, Field, Modal, PageHeader, Remote, Skeleton, Tone, WidgetHead, confirm};

const LIST: &str = "/api/v1/devices";

#[component]
pub fn Devices() -> impl IntoView {
    let devices = Remote::<Vec<Device>>::new(LIST);
    let pairing = RwSignal::new(false);
    let revoke = RwSignal::new(None::<(String, String)>);
    let renaming = RwSignal::new(None::<String>);
    let draft = RwSignal::new(String::new());
    let me = move || auth().session.with(|s| s.as_ref().map(|s| s.user.username.clone()).unwrap_or_default());

    let save_name = move |id: String| {
        let name = draft.get_untracked().trim().to_string();
        renaming.set(None);
        if name.is_empty() {
            return;
        }
        spawn_local(async move {
            match api::patch::<_, Device>(&format!("{LIST}/{}", api::encode(&id)), &DeviceRename { name }).await {
                Ok(_) => devices.reload(),
                Err(e) => toast(Tone::Err, e.text()),
            }
        });
    };

    let table = move || {
        if let Some(v) = devices.gate(3) {
            return v;
        }
        let rows = devices.data.get().unwrap_or_default();
        if rows.is_empty() {
            return view! {
                <EmptyState icon=icons::SMARTPHONE title=Signal::derive(|| t("dev.none").to_string()) text=Signal::derive(|| t("dev.none_text").to_string()) />
            }
            .into_any();
        }
        let others = rows.iter().any(|d| d.user != me());
        let mut heads = vec!["dev.col.name", "dev.col.model"];
        if others {
            heads.push("dev.col.user");
        }
        heads.extend(["nt.last", "dev.col.address", "dev.col.created", ""]);
        let body = rows
            .into_iter()
            .map(|d| {
                let id = d.id.clone();
                let (id2, id3) = (id.clone(), id.clone());
                let name = d.name.clone();
                let (n2, n3, n4) = (name.clone(), name.clone(), name.clone());
                let editing = Signal::derive(move || renaming.get().as_deref() == Some(id.as_str()));
                let model = [d.model.as_str(), d.os.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ");
                let mut cells = vec![ui::cell(
                    "dev.col.name",
                    "",
                    view! {
                        <Show
                            when=move || editing.get()
                            fallback=move || {
                                let (id, n) = (id2.clone(), n2.clone());
                                view! {
                                    <button class="link-strong" title=tr("dev.rename") on:click=move |_| { draft.set(n.clone()); renaming.set(Some(id.clone())); }>
                                        {n2.clone()}
                                    </button>
                                }
                            }
                        >
                            {
                                let id = id3.clone();
                                let id_b = id3.clone();
                                view! {
                                    <input
                                        class="input"
                                        maxlength="64"
                                        aria-label=tr("dev.col.name")
                                        data-autofocus=""
                                        prop:value=move || draft.get()
                                        on:input=move |ev| draft.set(event_target_value(&ev))
                                        on:keydown=move |ev: leptos::ev::KeyboardEvent| match ev.key().as_str() {
                                            "Enter" => save_name(id.clone()),
                                            "Escape" => renaming.set(None),
                                            _ => {}
                                        }
                                        on:blur=move |_| {
                                            if renaming.get_untracked().as_deref() == Some(id_b.as_str()) {
                                                save_name(id_b.clone());
                                            }
                                        }
                                    />
                                }
                            }
                        </Show>
                        {d.current.then(|| view! { <span class="chip">{tr("dev.this_one")}</span> })}
                    }
                    .into_any(),
                )];
                cells.push(ui::text_cell("dev.col.model", "muted", model));
                if others {
                    cells.push(ui::text_cell("dev.col.user", "mono", d.user.clone()));
                }
                cells.push(ui::text_cell("nt.last", "tnum", i18n::short_when(d.last_seen_ms)));
                cells.push(ui::text_cell("dev.col.address", "mono", d.last_ip.clone()));
                cells.push(ui::text_cell("dev.col.created", "tnum", i18n::datetime(d.created_ms)));
                let rid = d.id.clone();
                cells.push(ui::actions(
                    view! {
                        <button class="icon-btn sm" title=tr("dev.rename") aria-label=ui::verb_name("dev.rename", &n3)
                            on:click={ let (id, n) = (d.id.clone(), n3.clone()); move |_| { draft.set(n.clone()); renaming.set(Some(id.clone())); } }>
                            <Icon icon=icons::PENCIL size=16 />
                        </button>
                        <button class="icon-btn sm danger" title=tr("dev.revoke") aria-label=ui::verb_name("dev.revoke", &n4)
                            on:click=move |_| revoke.set(Some((rid.clone(), n4.clone())))>
                            <Icon icon=icons::TRASH_2 size=16 />
                        </button>
                    }
                    .into_any(),
                ));
                ui::row(cells)
            })
            .collect::<Vec<_>>();
        ui::table("", &heads, body)
    };

    let revoker = move || {
        revoke.get().map(|(id, name)| {
            let busy = RwSignal::new(false);
            let error = RwSignal::new(None::<String>);
            let close = Callback::new(move |_| revoke.set(None));
            confirm(Confirm {
                title: tf("dev.revoke_title", &[("name", &name)]),
                text: t("dev.revoke_text").to_string(),
                action: "dev.revoke",
                danger: true,
                extra: None,
                typed: Signal::stored(None),
                busy,
                error,
                on_confirm: Callback::new(move |_| {
                    busy.set(true);
                    let id = id.clone();
                    spawn_local(async move {
                        let r = api::delete_empty(&format!("{LIST}/{}", api::encode(&id))).await;
                        busy.set(false);
                        match r {
                            Ok(()) => {
                                devices.reload();
                                close.try_run(());
                            }
                            Err(e) => error.set(Some(e.text())),
                        }
                    });
                }),
                on_cancel: close,
            })
        })
    };

    view! {
        <div class="page">
            <PageHeader icon=icons::SMARTPHONE title="nav.devices" subtitle="dev.subtitle">
                <button class="btn primary" on:click=move |_| pairing.set(true)>
                    <Icon icon=icons::PLUS size=16 /><span class="btn-label">{tr("dev.pair")}</span>
                </button>
            </PageHeader>
            <crate::android_app::AndroidAppCard />
            <section class="card widget w-12" style="--i:1">
                <WidgetHead icon=icons::SMARTPHONE title="dev.list" />
                {table}
            </section>
            {revoker}
            <Show when=move || pairing.get()>
                <PairDialog on_close=Callback::new(move |_| { pairing.set(false); devices.reload(); }) />
            </Show>
        </div>
    }
}

#[derive(Clone, PartialEq)]
enum Stage {
    Starting,
    Reauth,
    Qr(PairingCreated),
    Claimed(String, PairingClaim),
    Approved(String),
    Ended(&'static str),
}

/// "Pair a device": QR and countdown, then the claim with its compare code.
#[component]
fn PairDialog(on_close: Callback<()>) -> impl IntoView {
    let stage = RwSignal::new(Stage::Starting);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let totp = RwSignal::new(String::new());
    let alive = StoredValue::new(true);
    let (left, start_left) = countdown();
    let totp_on = move || auth().session.with_untracked(|s| s.as_ref().is_some_and(|s| s.totp_enabled));
    on_cleanup(move || alive.set_value(false));

    // follows the pairing until it is claimed, expired or gone
    let follow = move |id: String| {
        spawn_local(async move {
            loop {
                api::sleep(1500).await;
                if alive.try_get_value() != Some(true) {
                    return;
                }
                let still = stage.with_untracked(|s| matches!(s, Stage::Qr(p) if p.pairing_id == id));
                if !still {
                    return;
                }
                match api::get::<PairingStatus>(&format!("/api/v1/devices/pairing/{}", api::encode(&id))).await {
                    Ok(st) => match (st.state, st.claim) {
                        (PairingState::Claimed, Some(c)) => {
                            name.set(c.device.name.clone());
                            let _ = stage.try_set(Stage::Claimed(id.clone(), c));
                            return;
                        }
                        (PairingState::Expired, _) => {
                            let _ = stage.try_set(Stage::Ended("dev.expired"));
                            return;
                        }
                        (PairingState::Rejected, _) => {
                            let _ = stage.try_set(Stage::Ended("dev.rejected"));
                            return;
                        }
                        _ => {}
                    },
                    Err(e) if e.status() == 404 => {
                        let _ = stage.try_set(Stage::Ended("dev.expired"));
                        return;
                    }
                    Err(_) => {}
                }
            }
        });
    };

    let start = move || {
        error.set(None);
        stage.set(Stage::Starting);
        spawn_local(async move {
            match api::post::<(), PairingCreated>("/api/v1/devices/pairing", None).await {
                Ok(p) => {
                    let id = p.pairing_id.clone();
                    start_left(PAIRING_TTL_S as u32);
                    let _ = stage.try_set(Stage::Qr(p));
                    follow(id);
                }
                Err(e) if e.code() == error_codes::REAUTH_REQUIRED => {
                    let _ = stage.try_set(Stage::Reauth);
                }
                Err(e) => {
                    let _ = stage.try_set(Stage::Ended("dev.failed"));
                    error.set(Some(e.text()));
                }
            }
        });
    };
    start();

    let reauth = move || {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let body = ReauthRequest {
            password: password.get_untracked(),
            totp: Some(totp.get_untracked()).filter(|c| !c.trim().is_empty()),
        };
        spawn_local(async move {
            let r = api::post_empty("/api/v1/auth/reauth", Some(&body)).await;
            busy.set(false);
            match r {
                Ok(()) => {
                    password.set(String::new());
                    totp.set(String::new());
                    start();
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };

    let decide = move |approve: bool| {
        let Stage::Claimed(id, _) = stage.get_untracked() else { return };
        busy.set(true);
        error.set(None);
        let n = name.get_untracked().trim().to_string();
        spawn_local(async move {
            let url = format!("/api/v1/devices/pairing/{}/{}", api::encode(&id), if approve { "approve" } else { "reject" });
            let r = if approve {
                let body = PairingApprove { name: Some(n).filter(|n| !n.is_empty()) };
                api::post::<_, Device>(&url, Some(&body)).await.map(|d| Some(d.name))
            } else {
                api::post_empty::<()>(&url, None).await.map(|()| None)
            };
            busy.set(false);
            match r {
                Ok(Some(n)) => {
                    let _ = stage.try_set(Stage::Approved(n));
                }
                Ok(None) => {
                    let _ = stage.try_set(Stage::Ended("dev.rejected"));
                }
                Err(e) => error.set(Some(e.text())),
            }
        });
    };

    let close = move |_| {
        // a pairing still open goes away with the dialog
        let open = stage.with_untracked(|s| match s {
            Stage::Qr(p) => Some(p.pairing_id.clone()),
            Stage::Claimed(id, _) => Some(id.clone()),
            _ => None,
        });
        if let Some(id) = open {
            spawn_local(async move {
                let _ = api::delete_empty(&format!("/api/v1/devices/pairing/{}", api::encode(&id))).await;
            });
        }
        on_close.run(());
    };
    let close = Callback::new(close);

    let body = move || match stage.get() {
        Stage::Starting => view! { <Skeleton lines=4 /> }.into_any(),
        Stage::Reauth => view! {
            <p class="muted">{tr("dev.reauth_text")}</p>
            <form class="code-form" on:submit=move |ev| { ev.prevent_default(); reauth(); }>
                <Field label="login.password">
                    <input class="input" type="password" autocomplete="current-password" data-autofocus=""
                        prop:value=move || password.get() on:input=move |ev| password.set(event_target_value(&ev)) />
                </Field>
                {totp_on().then(|| view! {
                    <Field label="login.totp_code">
                        <input class="input mono code-input" inputmode="numeric" autocomplete="one-time-code" maxlength="32"
                            prop:value=move || totp.get() on:input=move |ev| totp.set(event_target_value(&ev)) />
                    </Field>
                })}
                <div class="sec-actions">
                    <button class="btn primary" type="submit" disabled=move || busy.get() || password.with(String::is_empty)>
                        {spinner(busy)}{tr("dev.continue")}
                    </button>
                </div>
            </form>
        }
        .into_any(),
        Stage::Qr(p) => view! {
            <ol class="enroll">
                <li class="enroll-step">
                    <span class="step-num" aria-hidden="true">"1"</span>
                    <div class="enroll-body">
                        <strong>{tr("dev.step_scan")}</strong>
                        <p class="muted small">{tr("dev.step_scan_text")}</p>
                        <div class="enroll-qr">
                            <QrSvg text=p.qr.clone() />
                        </div>
                        <p class="muted small tnum" aria-live="polite">{move || tf("dev.expires_in", &[("t", &mmss(left.get()))])}</p>
                    </div>
                </li>
                <li class="enroll-step">
                    <span class="step-num" aria-hidden="true">"2"</span>
                    <div class="enroll-body">
                        <strong>{tr("dev.step_compare")}</strong>
                        <p class="muted small">{tr("dev.waiting")}</p>
                    </div>
                </li>
            </ol>
        }
        .into_any(),
        Stage::Claimed(_, c) => {
            let model = [c.device.model.as_str(), c.device.os.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ");
            view! {
                <p class="muted">{tr("dev.compare_text")}</p>
                <code class="secret mono" aria-label=tr("dev.compare_code")>{c.compare_code.clone()}</code>
                <dl class="kvs">
                    {ui::kv_text("dev.col.model", model, "")}
                    {ui::kv_text("dev.col.address", c.ip.clone(), "mono")}
                    {ui::kv_text("apps.upd.app_version", c.device.app_version.clone(), "mono")}
                </dl>
                <Field label="dev.col.name">
                    <input class="input" maxlength="64" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                </Field>
                <p class="muted small tnum">{move || tf("dev.expires_in", &[("t", &mmss(left.get()))])}</p>
            }
            .into_any()
        }
        Stage::Approved(n) => ui::note(Tone::Ok, icons::CIRCLE_CHECK, view! { {tf("dev.paired", &[("name", &n)])} }.into_any()),
        Stage::Ended(key) => view! {
            <p class="muted">{tr(key)}</p>
        }
        .into_any(),
    };

    let foot = move || match stage.get() {
        Stage::Claimed(..) => view! {
            <button type="button" class="btn ghost" disabled=move || busy.get() on:click=move |_| decide(false)>{tr("dev.reject")}</button>
            <button type="button" class="btn primary" disabled=move || busy.get() || left.get() == 0 on:click=move |_| decide(true)>
                {spinner(busy)}{tr("dev.approve")}
            </button>
        }
        .into_any(),
        Stage::Ended(_) => view! {
            <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button>
            <button type="button" class="btn primary" on:click=move |_| start()>{tr("dev.again")}</button>
        }
        .into_any(),
        _ => view! { <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("close")}</button> }.into_any(),
    };

    view! {
        <Modal title=Signal::derive(|| t("dev.pair").to_string()) on_close=close foot=ViewFn::from(foot)>
            <Show when=move || matches!(stage.get(), Stage::Starting | Stage::Qr(_))>
                <details class="fold sub">
                    <summary><Icon icon=icons::CHEVRON_RIGHT size=14 />{tr("dev.app_first")}</summary>
                    <crate::android_app::AndroidAppBox />
                </details>
            </Show>
            {body}
            {form_error(error)}
        </Modal>
    }
}
