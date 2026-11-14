//! Create-volume wizard: drives → RAID level (every possible level planned at once, so the
//! capacity and fault tolerance can be compared side by side, like a RAID calculator) →
//! options (name, encryption, compression) → review of the exact plan with the typed volume
//! name → the task, followed until it ends; an encrypted volume then offers its recovery key
//! and LUKS header backup right away.

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStatus;
use rynas_api_types::storage::{RaidLevel, StoragePlan};
use serde_json::json;

use crate::tasks::TaskRun;
use super::plan::{cap_bar, plan_errors, plan_view, plan_warnings, tolerance_text};
use super::volume::{RevealDialog, Stage, disk_checklist, level_json, start_task};
use super::{free_disks, level_key, loading, serial_of, use_refresh, valid_label, volumes};
use crate::api;
use crate::form::{form_error, spinner};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, PageHeader, Skeleton, Tone, typed_confirm};

const LEVELS: [RaidLevel; 6] = [RaidLevel::Single, RaidLevel::Raid1, RaidLevel::Raid5, RaidLevel::Raid6, RaidLevel::Raid10, RaidLevel::Raid0];

fn allowed(l: RaidLevel, n: usize) -> bool {
    match l {
        RaidLevel::Single => n == 1,
        RaidLevel::Raid0 | RaidLevel::Raid1 => n >= 2,
        RaidLevel::Raid5 => n >= 3,
        RaidLevel::Raid6 => n >= 4,
        RaidLevel::Raid10 => n >= 4 && n.is_multiple_of(2),
    }
}

fn min_key(l: RaidLevel) -> &'static str {
    match l {
        RaidLevel::Single => "wiz.min_single",
        RaidLevel::Raid0 | RaidLevel::Raid1 => "wiz.min2",
        RaidLevel::Raid5 => "wiz.min3",
        RaidLevel::Raid6 => "wiz.min4",
        RaidLevel::Raid10 => "wiz.min_even4",
    }
}

fn desc_key(l: RaidLevel) -> &'static str {
    match l {
        RaidLevel::Single => "wiz.d_single",
        RaidLevel::Raid0 => "wiz.d_raid0",
        RaidLevel::Raid1 => "wiz.d_raid1",
        RaidLevel::Raid5 => "wiz.d_raid5",
        RaidLevel::Raid6 => "wiz.d_raid6",
        RaidLevel::Raid10 => "wiz.d_raid10",
    }
}

/// What most NAS vendors suggest for n drives: a mirror for two, single parity up to four,
/// double parity beyond (rebuilds of large drives take long).
fn recommended(n: usize) -> RaidLevel {
    match n {
        0 | 1 => RaidLevel::Single,
        2 => RaidLevel::Raid1,
        3 | 4 => RaidLevel::Raid5,
        _ => RaidLevel::Raid6,
    }
}

type PlanRes = Option<Result<StoragePlan, String>>;

#[component]
pub fn CreateVolume() -> impl IntoView {
    let s = use_refresh();
    let step = RwSignal::new(1u8);
    let chosen = RwSignal::new(Vec::<String>::new());
    let level = RwSignal::new(RaidLevel::Single);
    let label = RwSignal::new(String::new());
    let encrypt = RwSignal::new(false);
    let pass1 = RwSignal::new(String::new());
    let pass2 = RwSignal::new(String::new());
    let compress = RwSignal::new(true);
    let plans = RwSignal::new(Vec::<(RaidLevel, PlanRes)>::new());
    let final_plan = RwSignal::new(None::<Result<StoragePlan, String>>);
    let typed = RwSignal::new(String::new());
    let stage = RwSignal::new(Stage::Form);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let reveal = RwSignal::new(false);
    let hdr_msg = RwSignal::new(None::<String>);

    let taken = Memo::new(move |_| volumes(s).into_iter().map(|v| v.id).collect::<Vec<_>>());
    // first free "volumeN", like DSM's "Volume 1"
    Effect::new(move || {
        if label.with_untracked(String::is_empty) && s.tree.with(Option::is_some) {
            let t = taken.get();
            let n = (1..).find(|n| !t.contains(&format!("volume{n}"))).unwrap_or(1);
            label.set(format!("volume{n}"));
        }
    });

    let body = move |l: RaidLevel, real: bool| {
        json!({
            "disks": chosen.get_untracked(),
            "level": level_json(l),
            "encrypt": real && encrypt.get_untracked(),
            "compress": !real || compress.get_untracked(),
            "label": label.get_untracked(),
        })
    };
    let plan_levels = move || {
        let n = chosen.with_untracked(Vec::len);
        let list: Vec<RaidLevel> = LEVELS.into_iter().filter(|l| allowed(*l, n)).collect();
        if !allowed(level.get_untracked(), n) {
            level.set(recommended(n));
        }
        plans.set(list.iter().map(|l| (*l, None)).collect());
        for l in list {
            let b = body(l, false);
            spawn_local(async move {
                let r = api::post::<_, StoragePlan>("/api/v1/storage/plan/volume", Some(&b)).await.map_err(|e| e.text());
                let _ = plans.try_update(|p| {
                    if let Some(e) = p.iter_mut().find(|(x, _)| *x == l) {
                        e.1 = Some(r);
                    }
                });
            });
        }
    };
    let plan_final = move || {
        final_plan.set(None);
        typed.set(String::new());
        let b = body(level.get_untracked(), true);
        spawn_local(async move {
            let r = api::post::<_, StoragePlan>("/api/v1/storage/plan/volume", Some(&b)).await.map_err(|e| e.text());
            let _ = final_plan.try_set(Some(r));
        });
    };

    let label_err = move || -> Option<&'static str> {
        let l = label.get();
        if !valid_label(&l) {
            Some("wiz.label_invalid")
        } else if taken.with(|t| t.contains(&l)) {
            Some("wiz.label_taken")
        } else {
            None
        }
    };
    let pass_err = move || -> Option<&'static str> {
        if !encrypt.get() {
            return None;
        }
        let n = pass1.with(|p| p.chars().count());
        if !(8..=512).contains(&n) {
            Some("wiz.pass_len")
        } else if pass1.get() != pass2.get() {
            Some("wiz.pass_mismatch")
        } else {
            None
        }
    };
    let can_next = move || match step.get() {
        1 => !chosen.with(Vec::is_empty),
        2 => {
            let l = level.get();
            plans.with(|p| p.iter().any(|(x, r)| *x == l && matches!(r, Some(Ok(p)) if p.valid)))
        }
        3 => label_err().is_none() && pass_err().is_none(),
        _ => false,
    };
    let next = move |_| {
        if !can_next() {
            return;
        }
        let n = step.get_untracked() + 1;
        match n {
            2 => plan_levels(),
            4 => plan_final(),
            _ => {}
        }
        step.set(n);
    };
    let back = move |_| step.update(|s| *s = s.saturating_sub(1).max(1));
    let create = move |_| {
        let mut b = body(level.get_untracked(), true);
        let disks = chosen.get_untracked();
        b["confirm"] = json!(label.get_untracked());
        b["serials"] = json!(disks.iter().map(|n| s.disk(n).map(|d| serial_of(&d)).unwrap_or_else(|| n.clone())).collect::<Vec<_>>());
        b["dry_run"] = json!(false);
        if encrypt.get_untracked() {
            b["passphrase"] = json!(pass1.get_untracked());
        }
        start_task("/api/v1/storage/volumes".into(), Some(b), false, stage, busy, error);
    };
    // passphrases leave memory with the page
    on_cleanup(move || {
        let _ = pass1.try_set(String::new());
        let _ = pass2.try_set(String::new());
    });

    let steps = ["wiz.s_disks", "wiz.s_level", "wiz.s_options", "wiz.s_review"];
    view! {
        <div class="page">
            <PageHeader icon=icons::PLUS title="sto.create" subtitle="wiz.subtitle">
                <a class="btn ghost" href="/storage">{tr("cancel")}</a>
            </PageHeader>
            <Show when=move || s.disks.with(Option::is_some) && s.tree.with(Option::is_some) fallback=move || loading(s, 6)>
                <ol class="stepper" aria-label=tr("wiz.steps")>
                    {steps.iter().enumerate().map(|(i, k)| {
                        let n = i as u8 + 1;
                        view! {
                            <li
                                class=move || { if step.get() == n { "current" } else if step.get() > n { "done" } else { "" } }
                                aria-current=move || (step.get() == n).then_some("step")
                            >
                                <span class="step-num">{move || if step.get() > n { view! { <Icon icon=icons::CHECK size=12 /> }.into_any() } else { n.to_string().into_any() }}</span>
                                <span class="step-label">{tr(k)}</span>
                            </li>
                        }
                    }).collect_view()}
                </ol>
                <section class="card wizard">
                    {move || match stage.get() {
                        Stage::Form | Stage::Plan(_) => match step.get() {
                            1 => untrack(|| step_disks(chosen)).into_any(),
                            2 => step_level(chosen, level, plans).into_any(),
                            3 => step_options(label, encrypt, pass1, pass2, compress, label_err, pass_err).into_any(),
                            _ => step_review(final_plan, label, typed, chosen, plan_final).into_any(),
                        },
                        Stage::Running(id) => view! {
                            <h2 class="section-title"><Icon icon=icons::DATABASE size=16 />{tf("wiz.creating", &[("name", &label.get_untracked())])}</h2>
                            <TaskRun id=id on_done=Callback::new(move |t: rynas_api_types::TaskInfo| { s.load(); stage.set(Stage::Done(t)); }) />
                        }
                        .into_any(),
                        Stage::Done(task) => {
                            let ok = task.status == TaskStatus::Ok;
                            let name = label.get_untracked();
                            let href = format!("/storage/volumes/{}", api::encode(&name));
                            if ok {
                                view! {
                                    {ui::note(Tone::Ok, icons::CIRCLE_CHECK, view! { {tf("wiz.created", &[("name", &name)])} }.into_any())}
                                    {encrypt.get_untracked().then(|| view! {
                                        <div class="secure-box">
                                            <h3><Icon icon=icons::KEY size=16 />{tr("wiz.secure_title")}</h3>
                                            <p class="muted small">{tr("wiz.secure_text")}</p>
                                            <div class="card-actions">
                                                <button class="btn primary sm" type="button" on:click=move |_| reveal.set(true)><Icon icon=icons::KEY size=14 />{tr("sto.rk_reveal")}</button>
                                                <button class="btn ghost sm" type="button" on:click=move |_| {
                                                    let name = label.get_untracked();
                                                    spawn_local(async move {
                                                        let r = api::download(&format!("/api/v1/storage/volumes/{}/luks-header", api::encode(&name)), &format!("luks-header-{name}.img")).await;
                                                        let _ = hdr_msg.try_set(Some(match r { Ok(()) => t("sto.hdr_saved").to_string(), Err(e) => e.text() }));
                                                    });
                                                }><Icon icon=icons::DOWNLOAD size=14 />{tr("sto.hdr_download")}</button>
                                            </div>
                                            {move || hdr_msg.get().map(|m| view! { <p class="muted small" role="status">{m}</p> })}
                                        </div>
                                    })}
                                    <div class="form-actions"><a class="btn primary" href=href>{tr("wiz.open_volume")}</a></div>
                                }
                                .into_any()
                            } else {
                                let err = task.error.clone().unwrap_or_default();
                                view! {
                                    {ui::note(Tone::Err, icons::CIRCLE_X, view! { {tr("sto.done_err")} }.into_any())}
                                    {(!err.is_empty()).then(|| view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 />{ui::tech_detail(err)}</p> })}
                                    <div class="form-actions">
                                        <button class="btn ghost" type="button" on:click=move |_| { stage.set(Stage::Form); plan_final(); }>{tr("wiz.back_review")}</button>
                                    </div>
                                }
                                .into_any()
                            }
                        }
                    }}
                    {move || matches!(stage.get(), Stage::Form | Stage::Plan(_)).then(|| view! {
                        {form_error(error)}
                        <div class="wizard-foot">
                            <button class="btn ghost" type="button" disabled=move || step.get() == 1 on:click=back>
                                <Icon icon=icons::ARROW_LEFT size=14 />{tr("back")}
                            </button>
                            {move || if step.get() < 4 {
                                view! {
                                    <button class="btn primary" type="button" disabled=move || !can_next() on:click=next>
                                        {tr("wiz.next")}<Icon icon=icons::CHEVRON_RIGHT size=14 />
                                    </button>
                                }
                                .into_any()
                            } else {
                                let ok = move || {
                                    final_plan.with(|p| matches!(p, Some(Ok(p)) if p.valid)) && typed.with(|t| t.trim() == label.get())
                                };
                                view! {
                                    <button class="btn danger" type="button" disabled=move || !ok() || busy.get() on:click=create>
                                        {spinner(busy)}{tr("wiz.create_go")}
                                    </button>
                                }
                                .into_any()
                            }}
                        </div>
                    })}
                </section>
            </Show>
            {move || reveal.get().then(|| view! { <RevealDialog id=label.get_untracked() on_close=Callback::new(move |_| reveal.set(false)) /> })}
        </div>
    }
}

fn step_disks(chosen: RwSignal<Vec<String>>) -> impl IntoView {
    let s = super::sto();
    let free = free_disks(s);
    let in_use = s.disks.with(|d| d.as_ref().map(|d| d.len()).unwrap_or(0)) - free.len();
    let total = Memo::new(move |_| chosen.with(|c| c.iter().filter_map(|n| s.disk(n)).map(|d| d.size).sum::<u64>()));
    view! {
        <h2 class="section-title"><Icon icon=icons::HARD_DRIVE size=16 />{tr("wiz.pick_disks")}</h2>
        <p class="muted small">{tr("wiz.pick_text")}</p>
        {disk_checklist(free, chosen)}
        {(in_use > 0).then(|| view! { <p class="muted small free-note"><Icon icon=icons::INFO size=14 />{tf("wiz.in_use", &[("n", &in_use.to_string())])}</p> })}
        <p class="wiz-sum" aria-live="polite">
            {move || tf("wiz.selected", &[("n", &chosen.with(Vec::len).to_string()), ("size", &i18n::bytes_si(total.get()))])}
        </p>
    }
}

fn step_level(chosen: RwSignal<Vec<String>>, level: RwSignal<RaidLevel>, plans: RwSignal<Vec<(RaidLevel, PlanRes)>>) -> impl IntoView {
    let n = chosen.with_untracked(Vec::len);
    let rec = recommended(n);
    view! {
        <h2 class="section-title"><Icon icon=icons::LAYERS size=16 />{tr("wiz.pick_level")}</h2>
        <div class="level-grid" role="radiogroup" aria-label=tr("sto.level")>
            {LEVELS.into_iter().map(|l| {
                let ok = allowed(l, n);
                let plan = move || plans.with(|p| p.iter().find(|(x, _)| *x == l).and_then(|(_, r)| r.clone()));
                view! {
                    <button
                        class="level-card"
                        type="button"
                        role="radio"
                        aria-checked=move || (level.get() == l).to_string()
                        disabled=!ok
                        on:click=move |_| level.set(l)
                    >
                        <span class="level-top">
                            <strong>{tr(level_key(l))}</strong>
                            {(ok && l == rec).then(|| view! { <span class="chip accent">{tr("wiz.recommended")}</span> })}
                        </span>
                        <span class="muted small">{tr(desc_key(l))}</span>
                        {if ok {
                            view! {
                                <span class="level-num tnum">
                                    {move || match plan() {
                                        None => "…".to_string(),
                                        Some(Ok(p)) if p.valid => i18n::bytes(p.usable_bytes),
                                        Some(_) => ui::DASH.to_string(),
                                    }}
                                </span>
                                <span class="small">{move || plan().and_then(Result::ok).filter(|p| p.valid).map(|p| tolerance_text(p.fault_tolerance))}</span>
                            }
                            .into_any()
                        } else {
                            view! { <span class="muted small">{tr(min_key(l))}</span> }.into_any()
                        }}
                    </button>
                }
            }).collect_view()}
        </div>
        {move || {
            let l = level.get();
            match plans.with(|p| p.iter().find(|(x, _)| *x == l).and_then(|(_, r)| r.clone())) {
                None => view! { <Skeleton lines=2 /> }.into_any(),
                Some(Err(e)) => view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 /><span>{e}</span></p> }.into_any(),
                Some(Ok(p)) => view! {
                    <div class="level-detail">
                        {p.valid.then(|| cap_bar(p.raw_bytes, p.usable_bytes, p.wasted_bytes))}
                        {plan_errors(&p)}
                        {plan_warnings(&p)}
                    </div>
                }
                .into_any(),
            }
        }}
    }
}

fn step_options(
    label: RwSignal<String>,
    encrypt: RwSignal<bool>,
    pass1: RwSignal<String>,
    pass2: RwSignal<String>,
    compress: RwSignal<bool>,
    label_err: impl Fn() -> Option<&'static str> + Copy + Send + Sync + 'static,
    pass_err: impl Fn() -> Option<&'static str> + Copy + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <h2 class="section-title"><Icon icon=icons::SETTINGS size=16 />{tr("wiz.s_options")}</h2>
        <div class="form">
            <label class="field">
                <span class="field-label">{tr("wiz.label")}</span>
                <input class="input mono" maxlength="16" autocomplete="off" spellcheck="false" autocapitalize="off"
                    aria-invalid=move || label_err().is_some().to_string()
                    prop:value=move || label.get() on:input=move |ev| label.set(event_target_value(&ev)) />
                <span class=move || if label_err().is_some() { "small warn-text" } else { "muted small" }>
                    {move || match label_err() { Some(k) => t(k).to_string(), None => tf("wiz.label_hint", &[("path", &format!("/{}", label.get()))]) }}
                </span>
            </label>
            <label class="check"><input type="checkbox" prop:checked=move || compress.get() on:change=move |ev| compress.set(event_target_checked(&ev)) />{tr("wiz.compress")}</label>
            <p class="muted small">{tr("wiz.compress_hint")}</p>
            <label class="check"><input type="checkbox" prop:checked=move || encrypt.get() on:change=move |ev| encrypt.set(event_target_checked(&ev)) />{tr("wiz.encrypt")}</label>
            <p class="muted small">{tr("wiz.encrypt_hint")}</p>
            {move || encrypt.get().then(|| view! {
                <div class="form sub-form">
                    <label class="field">
                        <span class="field-label">{tr("sto.passphrase")}</span>
                        <input class="input" type="password" autocomplete="new-password" prop:value=move || pass1.get() on:input=move |ev| pass1.set(event_target_value(&ev)) />
                    </label>
                    <label class="field">
                        <span class="field-label">{tr("wiz.pass_again")}</span>
                        <input class="input" type="password" autocomplete="new-password" prop:value=move || pass2.get() on:input=move |ev| pass2.set(event_target_value(&ev)) />
                    </label>
                    <span class=move || if pass_err().is_some() { "small warn-text" } else { "muted small" }>
                        {move || t(pass_err().unwrap_or("wiz.pass_hint"))}
                    </span>
                </div>
            })}
        </div>
    }
}

fn step_review(
    final_plan: RwSignal<Option<Result<StoragePlan, String>>>,
    label: RwSignal<String>,
    typed: RwSignal<String>,
    chosen: RwSignal<Vec<String>>,
    retry: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let s = super::sto();
    view! {
        <h2 class="section-title"><Icon icon=icons::LIST_CHECKS size=16 />{tr("wiz.s_review")}</h2>
        {move || match final_plan.get() {
            None => view! { <Skeleton lines=6 /> }.into_any(),
            Some(Err(e)) => view! {
                <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 /><span>{e}</span></p>
                <button class="btn ghost sm" type="button" on:click=move |_| retry()>{tr("err.retry")}</button>
            }
            .into_any(),
            Some(Ok(p)) => view! {
                {p.valid.then(|| view! {
                    <div class="erase-box" role="note">
                        <strong><Icon icon=icons::TRIANGLE_ALERT size=16 />{tr("wiz.erase_title")}</strong>
                        <ul>
                            {chosen.get_untracked().into_iter().map(|n| {
                                let d = untrack(|| s.disk(&n));
                                view! { <li><span class="mono strong">{n}</span>" "<span class="muted small">{d.map(|d| format!("{} · {} · {}", d.model, d.serial, i18n::bytes_si(d.size))).unwrap_or_default()}</span></li> }
                            }).collect_view()}
                        </ul>
                    </div>
                })}
                {plan_view(&p)}
                {p.valid.then(|| typed_confirm(label.get_untracked(), typed))}
            }
            .into_any(),
        }}
    }
}
