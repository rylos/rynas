//! Hardware: the provider-neutral overview (fans, temperatures, status LED and events), the
//! hardware module's tab where a module answers (Synology: `crate::modules::synology`), the
//! weekly power schedule ([`power`]), the UPS ([`ups`]) and the removable drives
//! ([`removable`], also `#removable`). A tab is mounted on its first visit and kept afterwards
//! (drafts survive a tab switch). The module tab exists only where the module is present
//! (G8d, D-MODULE).

mod power;
mod removable;
mod ups;
mod ups_net;
mod ups_setup;

use leptos::prelude::*;

use crate::i18n::{self, t, tr};
use crate::icons::{self, Icon};
use crate::live::{Res, use_live};
use crate::alerts;
use crate::modules::synology;
use crate::alerts::fan_label;
use crate::pages::dashboard::{led_state, temp_label};
use crate::form::tabs_shown;
use crate::ui::{self, EmptyState, Meter, PageHeader, Pending, Status, Tone, cpu_offset, temp_limits, temp_tone};

/// Index of the hardware module's tab.
const SYN_TAB: usize = 1;
/// Index of the removable drives tab.
const RM_TAB: usize = 4;

#[component]
pub fn Hardware() -> impl IntoView {
    let tab = RwSignal::new(0usize);
    // `/hardware#removable` (the removable drives menu's settings link) opens that tab
    let location = leptos_router::hooks::use_location();
    Effect::new(move || {
        if location.hash.get() == "#removable" {
            tab.set(RM_TAB);
        }
    });
    let syn = synology::present();
    // a selected tab that stops existing falls back to the overview
    Effect::new(move || {
        if !syn.get() && tab.get_untracked() == SYN_TAB {
            tab.set(0);
        }
    });
    let seen = RwSignal::new([true, false, false, false, false]);
    Effect::new(move || {
        let i = tab.get();
        if !seen.with_untracked(|s| s[i]) {
            seen.update(|s| s[i] = true);
        }
    });
    let panel = move |i: usize, body: fn(Signal<bool>) -> AnyView| {
        let active: Signal<bool> = Memo::new(move |_| tab.get() == i).into();
        // memo: the first visit of another tab must not build this one again (drafts)
        let mounted = Memo::new(move |_| seen.with(|s| s[i]));
        view! {
            <div class="hw-panel" role="tabpanel" id=format!("hwt-p{i}") aria-labelledby=format!("hwt-t{i}") hidden=move || !active.get()>
                {move || mounted.get().then(|| body(active))}
            </div>
        }
    };
    view! {
        <div class="page">
            <PageHeader icon=icons::CPU title="nav.hardware" subtitle="hw.subtitle" />
            <div class="card hw-tabs">{tabs_shown("hwt", &["hw.tab_overview", synology::TAB, "ps.title", "ups.tab", "rm.title"], tab, "nav.hardware", move |i| i != SYN_TAB || syn.get())}</div>
            {panel(0, overview)}
            {move || syn.get().then(|| panel(SYN_TAB, synology::panel))}
            {panel(2, power::panel)}
            {panel(3, ups::panel)}
            {panel(RM_TAB, removable::panel)}
        </div>
    }
}

fn overview(_active: Signal<bool>) -> AnyView {
    let live = use_live();
    let hw = live.hardware;
    let loaded = move || hw.with(Option::is_some);
    let has_provider = move || hw.with(|h| h.as_ref().is_some_and(|h| h.provider.is_some()));
    let h = move || hw.get().unwrap_or_default();
    // the status changes with every reading: the LED pill is built again only when it changes
    let led = Memo::new(move |_| hw.with(|h| h.as_ref().and_then(|h| h.status_led.clone())));

    view! {
        <div class="hw-panel">
            <Show when=loaded fallback=|| view! { <div class="card"><Pending lines=6 res=Res::Hardware /></div> }>
                <Show
                    when=has_provider
                    fallback=|| view! {
                        <div class="card soon">
                            <EmptyState
                                icon=icons::CPU
                                title=Signal::derive(|| t("hw.none_title").to_string())
                                text=Signal::derive(|| t("hw.none_text").to_string())
                            >
                                <a class="btn ghost" href="/">{tr("back_home")}</a>
                            </EmptyState>
                        </div>
                    }
                >
                    <div class="hw-summary card" style="--i:0">
                        <div class="hw-sum-item">
                            <span class="muted small">{tr("hw.provider")}</span>
                            <strong>{move || h().provider.unwrap_or_default()}</strong>
                        </div>
                        <div class="hw-sum-item">
                            <span class="muted small">{tr("f.model")}</span>
                            <strong>{move || h().model}</strong>
                        </div>
                        <div class="hw-sum-item">
                            <span class="muted small">{tr("hw.led")}</span>
                            {move || match led.get() {
                                Some(l) => {
                                    let (tone, state, colour) = led_state(&l);
                                    view! {
                                        <span class="led-big" title=colour>
                                            <span class=format!("led-dot {l}") aria-hidden="true"></span>
                                            <Status tone=tone text=state />
                                        </span>
                                    }.into_any()
                                }
                                None => view! { <span>"—"</span> }.into_any(),
                            }}
                        </div>
                        <div class="hw-sum-item">
                            <span class="muted small">{tr("hw.duty")}</span>
                            <strong class="tnum">{move || h().fan_duty.map(|d| format!("{d}%")).unwrap_or_else(|| "—".into())}</strong>
                        </div>
                        <div class="hw-sum-item">
                            <span class="muted small">{tr("hw.updated")}</span>
                            {move || {
                                let age = h().age_s;
                                let stale = age.is_some_and(|a| a > 30);
                                let txt = age.map(|a| format!("{} {}", i18n::duration(a), t("hw.ago"))).unwrap_or_else(|| "—".into());
                                if stale {
                                    view! { <Status tone=Tone::Warn text=format!("{} · {txt}", t("hw.stale")) /> }.into_any()
                                } else {
                                    view! { <strong class="tnum">{txt}</strong> }.into_any()
                                }
                            }}
                        </div>
                    </div>
                    {move || h().fan_fail.then(|| view! {
                        {ui::note(Tone::Err, icons::TRIANGLE_ALERT, view! { {t("hw.fan_fail")} }.into_any())}
                    })}
                    <div class="grid">
                        <section class="card widget w-6" style="--i:1">
                            <div class="w-head">
                                <span class="w-icon"><Icon icon=icons::FAN size=16 /></span>
                                <h2 class="w-title">{tr("hw.fans")}</h2>
                            </div>
                            <div class="fans">
                                {move || h().fans.into_iter().map(|f| {
                                    let rpm = f.rpm.unwrap_or(0);
                                    // spin speed follows rpm (purely decorative)
                                    let dur = if rpm > 0 { format!("{:.2}s", 60.0 / rpm as f64 * 12.0) } else { "0s".into() };
                                    view! {
                                        <div class=if f.alarm { "fan-card alarm" } else { "fan-card" }>
                                            <span class="fan-big spin" style:animation-duration=dur><Icon icon=icons::FAN size=32 /></span>
                                            <div>
                                                <div class="fan-label">{fan_label(&f.label)}</div>
                                                <div class="fan-rpm tnum">{f.rpm.map(|r| r.to_string()).unwrap_or_else(|| "—".into())}<span class="kpi-unit">" rpm"</span></div>
                                            </div>
                                            {f.alarm.then(|| view! { <Status tone=Tone::Err text=t("hw.alarm").to_string() /> })}
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        </section>
                        <section class="card widget w-6" style="--i:2">
                            <div class="w-head">
                                <span class="w-icon"><Icon icon=icons::THERMOMETER size=16 /></span>
                                <h2 class="w-title">{tr("hw.temps")}</h2>
                            </div>
                            <ul class="temps">
                                {move || {
                                    let hw = hw.get();
                                    let v = live.sample.with(|s| alerts::all_temps(s.as_ref(), hw.as_ref()));
                                    v.into_iter().map(|(id, l, c)| {
                                        let l = temp_label(&id, &l);
                                        let tone = temp_tone(&id, c);
                                        view! {
                                            <li class="temp-row">
                                                <span>{l}</span>
                                                <Meter pct={(c as f64 / temp_limits(&id, cpu_offset()).1 as f64 * 100.0).min(100.0)} tone=Signal::derive(move || tone) />
                                                <span class=format!("tnum temp-val {}", tone.class())>{i18n::celsius(c)}</span>
                                            </li>
                                        }
                                    }).collect_view()
                                }}
                            </ul>
                        </section>
                        <section class="card widget w-12" style="--i:3">
                            <div class="w-head">
                                <span class="w-icon"><Icon icon=icons::HISTORY size=16 /></span>
                                <h2 class="w-title">{tr("hw.events")}</h2>
                            </div>
                            <ul class="events">
                                {move || {
                                    let ev = h().events;
                                    if ev.is_empty() {
                                        return view! { <li class="muted">{t("hw.no_events")}</li> }.into_any();
                                    }
                                    ev.into_iter().rev().map(|(ts, txt)| view! {
                                        <li><span class="mono small muted">{ts}</span><span>{txt}</span></li>
                                    }).collect_view().into_any()
                                }}
                            </ul>
                        </section>
                    </div>
                </Show>
            </Show>
        </div>
    }
    .into_any()
}
