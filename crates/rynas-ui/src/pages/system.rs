//! Sistema: machine facts and interface preferences (theme, density, language, time format,
//! the sidebar: customize, reset).

use leptos::prelude::*;

use crate::form::{language_select, segmented};
use crate::i18n::{self, HourCycle, Lang, tr};
use crate::icons::{self, Icon};
use crate::live::{Res, use_live};
use crate::prefs::{Density, ThemePref, use_ui};
use crate::ui::{PageHeader, Pending};

const THEMES: &[(&str, &str)] = &[("auto", "theme.auto"), ("light", "theme.light"), ("dark", "theme.dark")];
const DENSITIES: &[(&str, &str)] = &[("comfortable", "density.comfortable"), ("compact", "density.compact")];
const HOUR_CYCLES: &[(&str, &str)] = &[("", "hour_cycle.auto"), ("h23", "hour_cycle.h23"), ("h12", "hour_cycle.h12")];

fn theme_code(t: ThemePref) -> &'static str {
    match t {
        ThemePref::Auto => "auto",
        ThemePref::Light => "light",
        ThemePref::Dark => "dark",
    }
}

#[component]
pub fn System() -> impl IntoView {
    let ui = use_ui();
    let nav = crate::sidebar::nav_layout();
    let live = use_live();
    let lang_pref = RwSignal::new(i18n::lang_pref());
    let sys = live.system;
    let f = move |g: fn(&rynas_api_types::SystemInfo) -> String| move || sys.with(|s| s.as_ref().map(g).unwrap_or_default());
    let editing_model = RwSignal::new(false);

    view! {
        <div class="page">
            <PageHeader icon=icons::SETTINGS title="nav.system" subtitle="sys.subtitle" />
            <Show when=move || editing_model.get()>
                {move || model_dialog(Callback::new(move |_| editing_model.set(false)))}
            </Show>
            <div class="grid">
                <section class="card widget w-6" style="--i:0">
                    <h2 class="section-title">{tr("sys.prefs")}</h2>
                    {segmented(
                        "bar.theme",
                        THEMES,
                        Signal::derive(move || theme_code(ui.theme.get()).to_string()),
                        Callback::new(move |v: String| {
                            ui.theme.set(match v.as_str() {
                                "light" => ThemePref::Light,
                                "dark" => ThemePref::Dark,
                                _ => ThemePref::Auto,
                            })
                        }),
                    )}
                    {segmented(
                        "density",
                        DENSITIES,
                        Signal::derive(move || (if ui.density.get() == Density::Compact { "compact" } else { "comfortable" }).to_string()),
                        Callback::new(move |v: String| ui.density.set(if v == "compact" { Density::Compact } else { Density::Comfortable })),
                    )}
                    {language_select(
                        "language",
                        None,
                        Some("language.auto"),
                        Signal::derive(move || lang_pref.get().map_or("", Lang::code).to_string()),
                        Callback::new(move |v: String| {
                            let l = Lang::from_code(&v);
                            lang_pref.set(l);
                            i18n::set_lang(l);
                        }),
                    )}
                    {segmented(
                        "hour_cycle",
                        HOUR_CYCLES,
                        Signal::derive(move || i18n::hour_cycle().code().to_string()),
                        Callback::new(move |v: String| i18n::set_hour_cycle(HourCycle::from_code(&v))),
                    )}
                    <div class="field">
                        <span class="field-label">{tr("sys.sidebar")}</span>
                        <div class="field-actions">
                            <button type="button" class="btn ghost" disabled=move || !nav.loaded.get() on:click=move |_| nav.open_editor(ui)>
                                <Icon icon=icons::PENCIL size=16 />{tr("nav.customize")}
                            </button>
                            <button type="button" class="btn ghost" disabled=move || !nav.loaded.get() on:click=move |_| nav.reset_ask.set(true)>
                                <Icon icon=icons::ROTATE_CCW size=16 />{tr("reset")}
                            </button>
                        </div>
                        <span class="field-help">{tr("sys.sidebar_help")}</span>
                    </div>
                </section>
                <section class="card widget w-6" style="--i:1">
                    <h2 class="section-title">{tr("sys.info")}</h2>
                    <Show when=move || sys.with(Option::is_some) fallback=|| view! { <Pending lines=6 res=Res::System /> }>
                        <dl class="kvs">
                            <div class="kv"><dt>{tr("f.host")}</dt><dd class="mono">{f(|s| s.hostname.clone())}</dd></div>
                            <div class="kv"><dt>{tr("f.model")}</dt><dd>
                                {f(|s| s.model.clone())}" "
                                <button type="button" class="btn ghost sm" title=tr("sys.model.edit") aria-label=tr("sys.model.edit") on:click=move |_| editing_model.set(true)><Icon icon=icons::PENCIL size=14 /></button>
                            </dd></div>
                            <div class="kv"><dt>{tr("f.os")}</dt><dd>{f(|s| s.os.clone())}</dd></div>
                            <div class="kv"><dt>{tr("f.kernel")}</dt><dd class="mono">{f(|s| s.kernel.clone())}</dd></div>
                            <div class="kv"><dt>{tr("f.cpu")}</dt><dd>{f(|s| s.cpu_model.clone())}</dd></div>
                            <div class="kv"><dt>{tr("w.memory")}</dt><dd class="tnum">{f(|s| i18n::bytes(s.mem_total))}</dd></div>
                            <div class="kv"><dt>{tr("f.version")}</dt><dd class="tnum">{f(|s| s.rynas_version.clone())}</dd></div>
                        </dl>
                    </Show>
                </section>
                <section class="card widget w-12 link-card" style="--i:2">
                    <span class="w-icon"><Icon icon=icons::LOCK size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("tls.title")}</h2>
                        <p class="muted small">{tr("tls.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/network/web-access?tab=certificate">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:2">
                    <span class="w-icon"><Icon icon=icons::CALENDAR_CLOCK size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("time.title")}</h2>
                        <p class="muted small">{tr("time.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/time">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:2">
                    <span class="w-icon"><Icon icon=icons::MEMORY_STICK size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("mem.title")}</h2>
                        <p class="muted small">{tr("mem.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/memory">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:2">
                    <span class="w-icon"><Icon icon=icons::ACTIVITY size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("proc.title")}</h2>
                        <p class="muted small">{tr("proc.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/processes">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:2">
                    <span class="w-icon"><Icon icon=icons::DOWNLOAD size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("upd.title")}</h2>
                        <p class="muted small">{tr("upd.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/updates">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:3">
                    <span class="w-icon"><Icon icon=icons::CALENDAR_CLOCK size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("sv.title")}</h2>
                        <p class="muted small">{tr("sv.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/schedule">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:3">
                    <span class="w-icon"><Icon icon=icons::CALENDAR_CLOCK size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("sch.title")}</h2>
                        <p class="muted small">{tr("sch.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/scheduler">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:4">
                    <span class="w-icon"><Icon icon=icons::BELL_RING size=18 /></span>
                    <div class="link-card-text">
                        <h2 class="w-title">{tr("nt.settings_title")}</h2>
                        <p class="muted small">{tr("nt.open_text")}</p>
                    </div>
                    <a class="btn ghost" href="/system/notifications">{tr("open")}<Icon icon=icons::CHEVRON_RIGHT size=16 /></a>
                </section>
                <section class="card widget w-12 link-card" style="--i:5">
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

/// The model's name shown for the machine (`PUT /system/model-name`, applied at once): empty
/// goes back to the name read from the firmware or recognised by the hardware module.
fn model_dialog(close: Callback<()>) -> AnyView {
    use leptos::task::spawn_local;
    use rynas_api_types::{MODEL_NAME_MAX, ModelNameConfig};

    use crate::api;
    use crate::i18n::t;
    use crate::ui::{DlgKind, dialog};

    let live = use_live();
    let value = RwSignal::new(String::new());
    let loaded = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    spawn_local(async move {
        match api::get::<ModelNameConfig>("/api/v1/system/model-name").await {
            Ok(c) => {
                let _ = value.try_set(c.name);
            }
            Err(e) => {
                let _ = error.try_set(Some(e.text()));
            }
        }
        let _ = loaded.try_set(true);
    });
    let invalid = Signal::derive(move || {
        let v = value.get();
        let v = v.trim();
        (v.chars().count() > MODEL_NAME_MAX || v.starts_with('#') || v.chars().any(char::is_control)).then(|| t("sys.model.invalid").to_string())
    });
    let can_save = Signal::derive(move || loaded.get() && invalid.get().is_none());
    let save = move |_| {
        if !can_save.get_untracked() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        let body = ModelNameConfig { name: value.get_untracked().trim().to_string() };
        spawn_local(async move {
            match api::put::<_, ModelNameConfig>("/api/v1/system/model-name", &body).await {
                Ok(_) => {
                    // the dashboard, the app bar and this page read it from the system facts
                    live.refresh_system();
                    close.try_run(());
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            let _ = busy.try_set(false);
        });
    };
    let body = view! {
        <form class="form" on:submit=move |ev| { ev.prevent_default(); save(()); }>
            {crate::form::text_field(
                "f.model",
                value.into(),
                Callback::new(move |v: String| value.set(v)),
                invalid,
                crate::form::In { maxlength: MODEL_NAME_MAX as u32, help: Some("sys.model.help"), ..crate::form::In::default() },
            )}
            {crate::form::form_error(error)}
        </form>
    }
    .into_any();
    let foot = crate::form::save_bar_with(crate::form::SaveBar { can_save, ..crate::form::SaveBar::new(busy, "save", close, Callback::new(save)) });
    dialog(DlgKind::Normal, Signal::derive(|| t("sys.model.edit").to_string()), close, body, Some(foot))
}
