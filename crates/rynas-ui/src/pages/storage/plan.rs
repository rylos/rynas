//! Shared pieces of every storage change: the plan as the planner returned it (capacity,
//! fault tolerance, warnings, ordered steps with what they erase). The task that runs it is
//! shown with [`crate::tasks::TaskRun`].

use leptos::prelude::*;
use rynas_api_types::storage::{PlanAction, PlanArgs, PlanStep, PlanWarningCode, StoragePlan};

use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui;

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
    let translated = s.kind.and_then(|k| coded("plan.s.", code_name(&k), &s.args));
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

/// Usable / redundancy / unused capacity of the selected disks as one bar with a legend.
pub fn cap_bar(raw: u64, usable: u64, wasted: u64) -> AnyView {
    let raw_f = raw.max(1) as f64;
    let protect = raw.saturating_sub(usable).saturating_sub(wasted);
    let seg = |cls: &'static str, v: u64| {
        view! { <span class=format!("cap-seg {cls}") style:width=format!("{:.2}%", v as f64 * 100.0 / raw_f)></span> }
    };
    let leg = |cls: &'static str, key: &'static str, v: String| {
        view! { <span class="cap-leg"><span class=format!("dot {cls}")></span>{tr(key)}<strong class="tnum">{v}</strong></span> }
    };
    view! {
        <div class="cap">
            <div class="cap-bar" role="img" aria-label=format!("{} {}", t("plan.usable"), i18n::bytes(usable))>
                {seg("use", usable)}{seg("prot", protect)}{seg("waste", wasted)}
            </div>
            <div class="cap-legend">
                {leg("use", "plan.usable", i18n::bytes(usable))}
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
                    <div class="tile"><span class="tile-label">{tr("plan.usable")}</span><span class="tile-num tnum">{i18n::bytes(p.usable_bytes)}</span></div>
                    <div class="tile">
                        <span class="tile-label">{tr("plan.tolerance")}</span>
                        <span class="tile-num tnum">{p.fault_tolerance}</span>
                        <span class="tile-sub">{tolerance_text(p.fault_tolerance)}</span>
                    </div>
                    <div class="tile"><span class="tile-label">{tr("plan.raw")}</span><span class="tile-num tnum">{i18n::bytes_si(p.raw_bytes)}</span></div>
                </div>
                {cap_bar(p.raw_bytes, p.usable_bytes, p.wasted_bytes)}
            })}
            {(!p.summary.is_empty()).then(|| view! {
                <dl class="kvs plan-summary">
                    {p.summary.iter().map(|(k, v)| {
                        let label = i18n::try_t(&format!("plan.k.{k}")).map(str::to_string).unwrap_or_else(|| k.clone());
                        view! { <div class="kv"><dt>{label}</dt><dd class="mono">{v.clone()}</dd></div> }
                    }).collect_view()}
                </dl>
            })}
            {(n > 0).then(|| ui::fold_with(
                view! {
                    {tf("plan.steps", &[("n", &n.to_string())])}
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
