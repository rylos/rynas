//! Shared pieces of every storage change: the plan as the planner returned it (capacity,
//! fault tolerance, warnings, ordered steps with what they erase). The task that runs it is
//! shown with [`crate::tasks::TaskRun`].

use leptos::prelude::*;
use rynas_api_types::storage::{PlanAction, StoragePlan};

use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};

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
    let errors = p.errors.clone();
    let warnings = p.warnings.clone();
    let has_cap = p.raw_bytes > 0;
    let n = p.steps.len();
    let erases: usize = p.steps.iter().filter(|s| !s.destroys.is_empty()).count();
    let open = erases > 0;
    view! {
        <div class="plan">
            {(!errors.is_empty()).then(|| view! {
                <div class="form-errors" role="alert">
                    {errors.into_iter().map(|e| view! { <p class="form-error"><Icon icon=icons::CIRCLE_X size=16 /><span lang="en">{e}</span></p> }).collect_view()}
                </div>
            })}
            {(!warnings.is_empty()).then(|| view! {
                <ul class="plan-warnings">
                    {warnings.into_iter().map(|w| view! { <li class="warn"><Icon icon=icons::TRIANGLE_ALERT size=14 /><span lang="en">{w}</span></li> }).collect_view()}
                </ul>
            })}
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
            {(n > 0).then(|| view! {
                <details class="plan-steps-wrap" open=open>
                    <summary>
                        {tf("plan.steps", &[("n", &n.to_string())])}
                        {(erases > 0).then(|| view! { <span class="chip danger-chip"><Icon icon=icons::TRIANGLE_ALERT size=12 />{tr("plan.destructive")}</span> })}
                    </summary>
                    <ol class="plan-steps">
                        {p.steps.iter().enumerate().map(|(i, s)| view! {
                            <li class:destroy=!s.destroys.is_empty()>
                                <span class="step-num">{i + 1}</span>
                                <div class="plan-step-body">
                                    <span lang="en">{s.description.clone()}</span>
                                    <code class="mono small muted">{step_detail(&s.action)}</code>
                                    {(!s.destroys.is_empty()).then(|| view! {
                                        <span class="plan-erase">
                                            {s.destroys.iter().map(|d| view! {
                                                <span class="chip danger-chip"><Icon icon=icons::TRIANGLE_ALERT size=12 />{tf("plan.erases", &[("dev", d)])}</span>
                                            }).collect_view()}
                                        </span>
                                    })}
                                </div>
                            </li>
                        }).collect_view()}
                    </ol>
                </details>
            })}
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
