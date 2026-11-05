//! Sistema: machine facts and interface preferences (theme, density, language).

use leptos::prelude::*;

use crate::i18n::{self, Lang, t, tr};
use crate::icons::{self, Icon};
use crate::live::{Res, use_live};
use crate::prefs::{Density, ThemePref, use_ui};
use crate::ui::{PageHeader, Pending};

/// Segmented control (radio group).
fn segmented<T: Copy + PartialEq + Send + Sync + 'static>(
    label: &'static str,
    value: Signal<T>,
    options: Vec<(T, &'static str)>,
    set: impl Fn(T) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <div class="pref">
            <span class="pref-label" id=format!("lbl-{label}")>{move || t(label)}</span>
            <div class="segmented" role="radiogroup" aria-labelledby=format!("lbl-{label}")>
                {options
                    .into_iter()
                    .map(|(v, key)| {
                        view! {
                            <button
                                role="radio"
                                aria-checked=move || (value.get() == v).to_string()
                                class:on=move || value.get() == v
                                on:click=move |_| set(v)
                            >
                                {move || t(key)}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
        </div>
    }
}

#[component]
pub fn System() -> impl IntoView {
    let ui = use_ui();
    let live = use_live();
    let lang_pref = RwSignal::new(i18n::lang_pref());
    let sys = live.system;
    let f = move |g: fn(&rynas_api_types::SystemInfo) -> String| move || sys.with(|s| s.as_ref().map(g).unwrap_or_default());

    view! {
        <div class="page">
            <PageHeader icon=icons::SETTINGS title="nav.system" subtitle="sys.subtitle" />
            <div class="grid">
                <section class="card widget w-6" style="--i:0">
                    <h2 class="section-title">{tr("sys.prefs")}</h2>
                    {segmented(
                        "bar.theme",
                        ui.theme.into(),
                        vec![(ThemePref::Auto, "theme.auto"), (ThemePref::Light, "theme.light"), (ThemePref::Dark, "theme.dark")],
                        move |v| ui.theme.set(v),
                    )}
                    {segmented(
                        "density",
                        ui.density.into(),
                        vec![(Density::Comfortable, "density.comfortable"), (Density::Compact, "density.compact")],
                        move |v| ui.density.set(v),
                    )}
                    {segmented(
                        "language",
                        lang_pref.into(),
                        vec![(None, "language.auto"), (Some(Lang::It), "lang.it"), (Some(Lang::En), "lang.en")],
                        move |v| {
                            lang_pref.set(v);
                            i18n::set_lang(v);
                        },
                    )}
                </section>
                <section class="card widget w-6" style="--i:1">
                    <h2 class="section-title">{tr("sys.info")}</h2>
                    <Show when=move || sys.with(Option::is_some) fallback=|| view! { <Pending lines=6 res=Res::System /> }>
                        <dl class="kvs">
                            <div class="kv"><dt>{tr("f.host")}</dt><dd class="mono">{f(|s| s.hostname.clone())}</dd></div>
                            <div class="kv"><dt>{tr("f.model")}</dt><dd>{f(|s| s.model.clone())}</dd></div>
                            <div class="kv"><dt>{tr("f.os")}</dt><dd>{f(|s| s.os.clone())}</dd></div>
                            <div class="kv"><dt>{tr("f.kernel")}</dt><dd class="mono">{f(|s| s.kernel.clone())}</dd></div>
                            <div class="kv"><dt>{tr("f.cpu")}</dt><dd>{f(|s| s.cpu_model.clone())}</dd></div>
                            <div class="kv"><dt>{tr("w.memory")}</dt><dd class="tnum">{f(|s| i18n::bytes(s.mem_total))}</dd></div>
                            <div class="kv"><dt>{tr("f.version")}</dt><dd class="tnum">{f(|s| s.rynas_version.clone())}</dd></div>
                        </dl>
                    </Show>
                </section>
                <section class="card widget w-12 link-card" style="--i:2">
                    <span class="w-icon"><Icon icon=icons::CALENDAR_CLOCK size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("sch.title")}</h2>
                        <p class="muted small">{tr("sch.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/scheduler">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:3">
                    <span class="w-icon"><Icon icon=icons::BELL_RING size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("nt.settings_title")}</h2>
                        <p class="muted small">{tr("nt.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/notifications">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:4">
                    <span class="w-icon"><Icon icon=icons::HISTORY size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("cfg.title")}</h2>
                        <p class="muted small">{tr("cfg.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/configuration">{tr("cfg.open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
            </div>
            <p class="muted small soon-note">{tr("soon.system")}</p>
        </div>
    }
}
