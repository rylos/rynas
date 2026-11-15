//! Shared pieces of every storage change: the plan as the planner returned it (capacity,
//! fault tolerance, warnings, ordered steps with what they erase). The task that runs it is
//! shown with [`crate::tasks::TaskRun`].

use leptos::prelude::*;
use rynas_api_types::storage::{FlexTier, PlanAction, PlanArgs, PlanStep, PlanStepKind, PlanWarningCode, RaidLevel, StoragePlan};

use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Status, Tone};

/// A translated plan text, or the server's English one (`lang="en"`) when this build has no
/// key for its code.
struct Text {
    text: String,
    en: bool,
}

/// The JSON name of a code (`no_smart_data`), which names its i18n key.
fn code_name(code: &impl serde::Serialize) -> String {
    serde_json::to_string(code).unwrap_or_default().trim_matches('"').to_string()
}

/// `<prefix><code>` with the values of `args`, or None when the key does not exist or names a
/// value the server did not send.
fn coded(prefix: &str, code: String, args: &PlanArgs) -> Option<String> {
    let mut s = i18n::try_t(&format!("{prefix}{code}"))?.to_string();
    let level = args.level.map(|l| t(super::level_key(l)).to_string());
    let values: [(&str, Option<String>); 6] = [
        ("subject", args.subject.clone()),
        ("other", args.other.clone()),
        ("items", (!args.items.is_empty()).then(|| args.items.join(", "))),
        ("size", args.size_bytes.map(i18n::bytes_si)),
        ("n", args.count.map(|n| n.to_string())),
        ("level", level),
    ];
    for (k, v) in values {
        let key = format!("{{{k}}}");
        if s.contains(&key) {
            s = s.replace(&key, &v?);
        }
    }
    Some(s)
}

fn text_or_en(translated: Option<String>, en: Option<&String>) -> Text {
    match translated {
        Some(text) => Text { text, en: false },
        None => Text { text: en.cloned().unwrap_or_default(), en: true },
    }
}

/// The errors of a plan, translated from their codes (`issues`); an older server without
/// codes gets its English texts.
fn error_texts(p: &StoragePlan) -> Vec<Text> {
    if p.issues.len() != p.errors.len() {
        return p.errors.iter().map(|e| text_or_en(None, Some(e))).collect();
    }
    p.issues
        .iter()
        .zip(&p.errors)
        .map(|(i, e)| {
            let args = PlanArgs { subject: i.subject.clone(), ..Default::default() };
            text_or_en(coded("plan.e.", code_name(&i.code), &args), Some(e))
        })
        .collect()
}

/// The warnings of a plan, translated from `warning_items`; English for unknown codes and
/// older servers.
fn warning_texts(p: &StoragePlan) -> Vec<Text> {
    if p.warning_items.len() != p.warnings.len() {
        return p.warnings.iter().map(|w| text_or_en(None, Some(w))).collect();
    }
    p.warning_items
        .iter()
        .zip(&p.warnings)
        .map(|(w, en)| {
            let mut args = w.args.clone();
            if w.code == PlanWarningCode::SmartWarnings {
                // SMART reason codes: translated where a text exists, readable otherwise
                for r in &mut args.items {
                    *r = i18n::try_t(&format!("sto.reason.{r}")).map(str::to_string).unwrap_or_else(|| r.replace('_', " "));
                }
            }
            text_or_en(coded("plan.w.", code_name(&w.code), &args), Some(en))
        })
        .collect()
}

fn step_text(s: &PlanStep) -> Text {
    let translated = match (s.kind, &s.args) {
        // a flexible RAID: every tier partition of a new disk in one command, or one tier
        // partition added to a disk in use
        (Some(PlanStepKind::CreatePartition), PlanArgs { subject: Some(d), count: Some(n), .. }) => {
            Some(i18n::tn("plan.s.create_partitions", *n, &[("subject", d)]))
        }
        (Some(PlanStepKind::CreatePartition), PlanArgs { other: Some(_), .. }) => coded("plan.s.", "create_tier_partition".into(), &s.args),
        (Some(k), args) => coded("plan.s.", code_name(&k), args),
        (None, _) => None,
    };
    text_or_en(translated, Some(&s.description))
}

fn text_span(t: Text) -> AnyView {
    if t.en {
        view! { <span lang="en">{t.text}</span> }.into_any()
    } else {
        view! { <span>{t.text}</span> }.into_any()
    }
}

/// The errors of a plan as form errors (`role="alert"`), nothing when there are none.
pub fn plan_errors(p: &StoragePlan) -> Option<AnyView> {
    let errors = error_texts(p);
    (!errors.is_empty()).then(|| {
        view! {
            <div class="form-errors" role="alert">
                {errors.into_iter().map(|e| view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 />{text_span(e)}</p> }).collect_view()}
            </div>
        }
        .into_any()
    })
}

/// The warnings of a plan as a list, nothing when there are none.
pub fn plan_warnings(p: &StoragePlan) -> Option<AnyView> {
    let warnings = warning_texts(p);
    (!warnings.is_empty()).then(|| {
        view! {
            <ul class="plan-warnings">
                {warnings.into_iter().map(|w| view! { <li class="warn"><Icon icon=icons::TRIANGLE_ALERT size=14 />{text_span(w)}</li> }).collect_view()}
            </ul>
        }
        .into_any()
    })
}

pub fn cap_bar(raw: u64, usable: u64, wasted: u64) -> AnyView {
    let raw_f = raw.max(1) as f64;
    // everything that is neither usable nor unused: parity or mirrors, and the little the
    // partitions and RAID metadata take; the three parts add up to the raw size
    let protect = raw.saturating_sub(usable).saturating_sub(wasted);
    let seg = |cls: &'static str, v: u64| {
        view! { <span class=format!("cap-seg {cls}") style:width=format!("{:.2}%", v as f64 * 100.0 / raw_f)></span> }
    };
    let leg = |cls: &'static str, key: &'static str, v: String| {
        view! { <span class="cap-leg"><span class=format!("dot {cls}")></span>{tr(key)}<strong class="tnum">{v}</strong></span> }
    };
    // decimal units like the drive sizes, so the parts add up as shown
    view! {
        <div class="cap">
            <div class="cap-bar" role="img" aria-label=format!("{} {}", t("plan.usable"), i18n::bytes_si(usable))>
                {seg("use", usable)}{seg("prot", protect)}{seg("waste", wasted)}
            </div>
            <div class="cap-legend">
                {leg("use", "plan.usable", i18n::bytes_si(usable))}
                {(protect > 0).then(|| leg("prot", "plan.protection", i18n::bytes_si(protect)))}
                {(wasted > 0).then(|| leg("waste", "plan.wasted", i18n::bytes_si(wasted)))}
            </div>
        </div>
    }
    .into_any()
}

fn step_detail(a: &PlanAction) -> String {
    match a {
        PlanAction::Command { argv, stdin } => {
            let mut s = argv.join(" ");
            if let Some(n) = stdin {
                s.push_str(&format!(" < [{n}]"));
            }
            s
        }
        PlanAction::WriteSysfs { path, value } => format!("echo {value} > {path}"),
        PlanAction::Config { file, entry } => format!("{file}: {entry}"),
        PlanAction::Wait { condition } => condition.clone(),
    }
}

/// The plan: errors, warnings, capacity (when it has one), named results and the steps.
pub fn plan_view(p: &StoragePlan) -> AnyView {
    let has_cap = p.raw_bytes > 0;
    let n = p.steps.len();
    let erases: usize = p.steps.iter().filter(|s| !s.destroys.is_empty()).count();
    let open = erases > 0;
    view! {
        <div class="plan">
            {plan_errors(p)}
            {plan_warnings(p)}
            {(p.valid && has_cap).then(|| view! {
                <div class="tiles plan-tiles">
                    <div class="tile"><span class="tile-label">{tr("plan.usable")}</span><span class="tile-num tnum">{i18n::bytes_si(p.usable_bytes)}</span></div>
                    <div class="tile">
                        <span class="tile-label">{tr("plan.tolerance")}</span>
                        <span class="tile-num tnum">{p.fault_tolerance}</span>
                        <span class="tile-sub">{tolerance_text(p.fault_tolerance)}</span>
                    </div>
                    <div class="tile"><span class="tile-label">{tr("plan.raw")}</span><span class="tile-num tnum">{i18n::bytes_si(p.raw_bytes)}</span></div>
                </div>
                {cap_bar(p.raw_bytes, p.usable_bytes, p.wasted_bytes)}
            })}
            {p.valid.then(|| tier_preview(p))}
            {(!p.summary.is_empty()).then(|| view! {
                <dl class="kvs plan-summary">
                    {p.summary.iter().map(|(k, v)| {
                        let label = i18n::try_t(&format!("plan.k.{k}")).map(str::to_string).unwrap_or_else(|| k.clone());
                        // the level in words ("flex1" → "Flexible RAID, 1-drive tolerance")
                        let level = (k == "level")
                            .then(|| serde_json::from_value::<RaidLevel>(serde_json::Value::String(v.clone())).ok())
                            .flatten();
                        match level {
                            Some(l) => view! { <div class="kv"><dt>{label}</dt><dd>{tr(super::level_key(l))}</dd></div> }.into_any(),
                            None => view! { <div class="kv"><dt>{label}</dt><dd class="mono">{v.clone()}</dd></div> }.into_any(),
                        }
                    }).collect_view()}
                </dl>
            })}
            {(n > 0).then(|| ui::fold_with(
                view! {
                    {i18n::tn("plan.steps", n as u64, &[])}
                    {(erases > 0).then(|| view! { " "<span class="chip err"><Icon icon=icons::TRIANGLE_ALERT size=12 />{tr("plan.destructive")}</span> })}
                }
                .into_any(),
                open,
                view! {
                    <ol class="plan-steps">
                        {p.steps.iter().enumerate().map(|(i, s)| view! {
                            <li class:destroy=!s.destroys.is_empty()>
                                <span class="step-num">{i + 1}</span>
                                <div class="plan-step-body">
                                    {text_span(step_text(s))}
                                    <code class="mono small muted">{step_detail(&s.action)}</code>
                                    {(!s.destroys.is_empty()).then(|| view! {
                                        <span class="plan-erase">
                                            {s.destroys.iter().map(|d| view! {
                                                <span class="chip err"><Icon icon=icons::TRIANGLE_ALERT size=12 />{tf("plan.erases", &[("dev", d)])}</span>
                                            }).collect_view()}
                                        </span>
                                    })}
                                </div>
                            </li>
                        }).collect_view()}
                    </ol>
                }
                .into_any(),
            ))}
        </div>
    }
    .into_any()
}

pub fn tolerance_text(n: u32) -> String {
    match n {
        0 => t("plan.tol0").to_string(),
        1 => t("plan.tol1").to_string(),
        n => tf("plan.toln", &[("n", &n.to_string())]),
    }
}

// ------------------------------------------------------------------ flexible RAID tiers

/// A size difference smaller than this is rounding, not a tier of its own (the planner's
/// smallest tier): disks that close count as the same size.
use rynas_api_types::capacity::MIN_TIER;

/// The colour class of a tier (`c1`…`c5`, repeating), shared by the drive map and the lists.
fn tier_class(index: u32) -> String {
    format!("c{}", index.saturating_sub(1) % 5 + 1)
}

fn tier_level_text(tier: &FlexTier) -> String {
    match tier.level {
        Some(l) => t(super::level_key(l)).to_string(),
        None => t("sto.tier_unused").to_string(),
    }
}

pub fn tier_map(tiers: &[FlexTier], unused: Option<&[(String, u64)]>, extra: &[String]) -> AnyView {
    let s = super::sto();
    let mut names: Vec<String> = tiers.iter().flat_map(|x| x.members.iter().cloned()).chain(extra.iter().cloned()).collect();
    names.sort();
    names.dedup();
    struct Seg {
        cls: String,
        bytes: u64,
        title: String,
        /// The tier number written on the segment (tiers differ by more than colour).
        n: Option<u32>,
    }
    struct Row {
        name: String,
        size: u64,
        segs: Vec<Seg>,
    }
    let rows: Vec<Row> = names
        .into_iter()
        .map(|name| {
            let mut held: Vec<&FlexTier> = tiers.iter().filter(|x| x.members.contains(&name)).collect();
            held.sort_by_key(|x| x.start);
            let end = held.iter().map(|x| x.end).max().unwrap_or(0);
            let size = untrack(|| s.disk(&name)).map(|d| d.size).unwrap_or(end).max(end);
            let mut segs: Vec<Seg> = held
                .iter()
                .map(|x| Seg {
                    cls: if x.level.is_some() { tier_class(x.index) } else { "waste".to_string() },
                    bytes: x.end.saturating_sub(x.start),
                    title: format!("{} · {}", tf("sto.tier", &[("n", &x.index.to_string())]), tier_level_text(x)),
                    n: x.level.is_some().then_some(x.index),
                })
                .collect();
            let rest = match unused {
                Some(u) => u.iter().find(|(d, _)| *d == name).map(|(_, b)| *b).unwrap_or(0),
                None => size.saturating_sub(end),
            };
            if rest >= MIN_TIER {
                segs.push(Seg {
                    cls: "waste".to_string(),
                    bytes: rest,
                    title: format!("{} · {}", t("sto.unused_space"), i18n::bytes_si(rest)),
                    n: None,
                });
            }
            Row { name, size, segs }
        })
        .collect();
    let max = rows.iter().map(|r| r.size).max().unwrap_or(1).max(1) as f64;
    let any_unused = rows.iter().any(|r| r.segs.iter().any(|x| x.cls == "waste"));
    let legend: Vec<(String, u32, String)> = tiers
        .iter()
        .filter(|x| x.level.is_some())
        .map(|x| (tier_class(x.index), x.index, format!("{} · {}", tf("sto.tier", &[("n", &x.index.to_string())]), tier_level_text(x))))
        .collect();
    view! {
        <div class="tier-map" role="list" aria-label=tr("sto.tier_map")>
            {rows.into_iter().map(|r| {
                let total = r.size.max(1) as f64;
                let label = format!("{} · {}", r.name, r.segs.iter().map(|x| x.title.clone()).collect::<Vec<_>>().join(", "));
                view! {
                    <div class="tier-row" role="listitem">
                        <span class="tier-disk"><span class="mono strong">{r.name.clone()}</span><span class="muted small tnum">{i18n::bytes_si(r.size)}</span></span>
                        <span class="tier-track">
                            <span class="tier-bar" role="img" aria-label=label style:width=format!("{:.2}%", r.size as f64 * 100.0 / max)>
                                {r.segs.into_iter().map(|x| view! {
                                    <span class=format!("tier-seg {}", x.cls) title=x.title style:width=format!("{:.2}%", x.bytes as f64 * 100.0 / total)>
                                        {x.n.map(|n| view! { <span class="tier-n" aria-hidden="true">{n}</span> })}
                                    </span>
                                }).collect_view()}
                            </span>
                        </span>
                    </div>
                }
            }).collect_view()}
        </div>
        <div class="cap-legend">
            {legend.into_iter().map(|(cls, n, text)| view! {
                <span class="cap-leg"><span class=format!("dot num {cls}") aria-hidden="true">{n}</span>{text}</span>
            }).collect_view()}
            {any_unused.then(|| view! { <span class="cap-leg"><span class="dot waste"></span>{tr("sto.unused_space")}</span> })}
        </div>
    }
    .into_any()
}

/// The tiers as a list: level, drives, usable space; `live` adds the health, the sync progress
/// and the missing members (a volume's tiers, not a plan's).
pub fn tier_list(tiers: &[FlexTier], live: bool) -> AnyView {
    view! {
        <ul class="tier-list">
            {tiers.iter().map(|x| {
                let (tone, key) = super::array_health(x.health);
                let lost = (x.degraded as usize).max(x.missing.len());
                let members = format!("{}: {}", i18n::tn("sto.members", x.members.len() as u64, &[]), x.members.join(", "));
                let sub = match (&x.array, live) {
                    (Some(a), true) => format!("{members} · {a}"),
                    _ => members,
                };
                view! {
                    <li class="tier-item">
                        <div class="tier-head">
                            {if x.level.is_some() {
                                view! { <span class=format!("dot num {}", tier_class(x.index)) aria-hidden="true">{x.index}</span> }.into_any()
                            } else {
                                view! { <span class="dot waste"></span> }.into_any()
                            }}
                            <strong>{tf("sto.tier", &[("n", &x.index.to_string())])}</strong>
                            {ui::chip(if x.level.is_some() { Tone::Info } else { Tone::Warn }, view! { {tier_level_text(x)} }.into_any())}
                            {x.level.is_some().then(|| view! { <span class="muted small tnum">{tf("sto.tier_usable", &[("size", &i18n::bytes_si(x.usable))])}</span> })}
                            {live.then(|| view! { <span class="row-end"><Status tone=tone text=t(key).to_string() /></span> })}
                        </div>
                        <div class="muted small tier-sub">{sub}</div>
                        {live.then(|| x.sync.as_ref().map(super::sync_row))}
                        {(live && lost > 0).then(|| {
                            let names = if x.missing.is_empty() { String::new() } else { format!(" ({})", x.missing.join(", ")) };
                            view! { <div class="warn tone-text small tier-warn"><Icon icon=icons::TRIANGLE_ALERT size=14 /><span>{tf("sto.missing_members", &[("n", &lost.to_string())])}{names}</span></div> }
                        })}
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

/// A flexible RAID plan's tiers: the drive map after the change, and the tier lists before and
/// after it side by side (only after for a new volume). Nothing for other plans.
pub fn tier_preview(p: &StoragePlan) -> Option<AnyView> {
    if p.tiers_after.is_empty() {
        return None;
    }
    let before = (!p.tiers_before.is_empty()).then(|| tier_list(&p.tiers_before, false));
    Some(
        view! {
            <div class="tier-preview">
                {tier_map(&p.tiers_after, None, &[])}
                {match before {
                    Some(b) => view! {
                        <div class="tier-compare">
                            <div><div class="mini-title">{tr("plan.before")}</div>{b}</div>
                            <div><div class="mini-title">{tr("plan.after")}</div>{tier_list(&p.tiers_after, false)}</div>
                        </div>
                    }
                    .into_any(),
                    None => tier_list(&p.tiers_after, false),
                }}
            </div>
        }
        .into_any(),
    )
}
