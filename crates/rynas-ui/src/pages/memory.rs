//! Sistema > Memoria: compressed swap in RAM (`/api/v1/system/zram`).

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::TaskStatus;
use rynas_api_types::zram::{DiskSwap, ZramSettings, ZramState};

use crate::form::{Draft, form_error, fid, lens, lens_bool, number_field, spinner, stale_notice, switch};
use crate::i18n::{self, t, tf, tr};
use crate::icons;
use crate::toast::toast;
use crate::ui::{self, PageHeader, Remote, Tone};

const URL: &str = "/api/v1/system/zram";
const MIN_SIZE: u32 = 64;
/// A number field left empty: outside every limit, so Apply stays off.
const EMPTY: u32 = u32::MAX;

fn shown(v: u32) -> String {
    if v == EMPTY { String::new() } else { v.to_string() }
}

/// One line about the values advised for this machine.
fn advice(r: &ZramSettings) -> String {
    tf(
        "mem.advice",
        &[
            // in the unit of the size field, so the advice can be typed in as it reads
            ("size", &format!("{} MiB", r.size_mib)),
            ("algorithm", &r.algorithm),
            ("swappiness", &r.swappiness.to_string()),
            ("cluster", &r.page_cluster.to_string()),
        ],
    )
}

fn valid(s: &ZramSettings, max: u32) -> bool {
    !s.enabled || ((MIN_SIZE..=max).contains(&s.size_mib) && s.swappiness <= 200 && s.page_cluster <= 5)
}

fn disk_kind(d: &DiskSwap) -> &'static str {
    match d.rotational {
        Some(true) => "mem.disk_hdd",
        Some(false) => "mem.disk_ssd",
        None => "mem.disk_file",
    }
}

fn kv(label: &'static str, value: impl IntoView + 'static) -> AnyView {
    view! { <div class="kv"><dt>{tr(label)}</dt><dd class="tnum">{value}</dd></div> }.into_any()
}

/// What runs now, and the swap on disk that comes after it.
fn status_card(st: Remote<ZramState>) -> AnyView {
    let rows = move || {
        st.data.with(|s| {
            let s = s.as_ref()?;
            let state = view! {
                {kv("mem.state", t(if s.running.is_some() { "mem.on" } else { "mem.off" }))}
            }
            .into_any();
            let running = s.running.as_ref().map(|r| {
                view! {
                    {kv("mem.size", i18n::bytes(r.size))}
                    {kv("mem.algorithm", r.algorithm.clone())}
                    {kv("mem.data", i18n::bytes(r.data))}
                    {kv("mem.ram_used", i18n::bytes(r.mem_used))}
                }
            });
            Some(view! { {state}{running} })
        })
    };
    let disks = move || st.data.with(|s| s.as_ref().map(|s| s.disk_swaps.clone()).unwrap_or_default());
    view! {
        <section class="card widget w-6" style="--i:1">
            <h2 class="section-title">{tr("mem.status")}</h2>
            <dl class="kvs">{rows}</dl>
            <h2 class="section-title">{tr("mem.disk_swaps")}</h2>
            {move || {
                let list = disks();
                if list.is_empty() {
                    return view! { <p class="muted small">{tr("mem.no_disk_swap")}</p> }.into_any();
                }
                view! {
                    <dl class="kvs">
                        {list.into_iter().map(|d| {
                            let text = tf(
                                "mem.disk_line",
                                &[("size", &i18n::bytes(d.size)), ("used", &i18n::bytes(d.used)), ("priority", &d.priority.to_string()), ("kind", t(disk_kind(&d)))],
                            );
                            view! { <div class="kv"><dt class="mono">{d.path.clone()}</dt><dd class="tnum">{text}</dd></div> }
                        }).collect_view()}
                    </dl>
                    <p class="muted small">{tr("mem.disk_help")}</p>
                }.into_any()
            }}
        </section>
    }
    .into_any()
}

fn form_card(init: ZramState, st: Remote<ZramState>) -> AnyView {
    let draft = Draft::new(init.settings.clone(), move || st.data.with(|s| s.as_ref().map(|s| s.settings.clone())));
    let d = draft.d;
    let dirty = draft.dirty();
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let (enabled, set_enabled) = lens_bool(d, |a| a.enabled, |a, v| a.enabled = v);
    let (size, set_size) = lens(d, |a| shown(a.size_mib), |a, v| a.size_mib = v.trim().parse().unwrap_or(EMPTY));
    let (swappiness, set_swappiness) = lens(d, |a| shown(a.swappiness), |a, v| a.swappiness = v.trim().parse().unwrap_or(EMPTY));
    let (cluster, set_cluster) = lens(d, |a| shown(a.page_cluster), |a, v| a.page_cluster = v.trim().parse().unwrap_or(EMPTY));
    let max = init.max_size_mib;
    let algorithms = init.algorithms.clone();
    let recommended = init.recommended.clone();
    let advice_text = advice(&recommended);
    let needs_install = Signal::derive(move || {
        d.with(|a| a.enabled) && st.data.with(|s| s.as_ref().is_some_and(|s| !s.installed))
    });
    // with the saved values not in use yet, Apply tries again
    let pending = Signal::derive(move || st.data.with(|s| s.as_ref().is_some_and(|s| s.pending_restart)));
    let can_save = Signal::derive(move || (dirty.get() || pending.get()) && d.with(|a| valid(a, max)));
    let algo_id = fid();
    let save = move |_| {
        busy.set(true);
        error.set(None);
        // "off" keeps no values: the server then answers the advised ones
        if !d.with_untracked(|a| a.enabled)
            && let Some(r) = st.data.with_untracked(|s| s.as_ref().map(|s| s.recommended.clone()))
        {
            d.set(ZramSettings { enabled: false, ..r });
        }
        let body = serde_json::to_string(&d.get_untracked()).unwrap_or_default();
        spawn_local(async move {
            let r = crate::tasks::run_task(URL, Some(body)).await;
            let _ = busy.try_set(false);
            match r {
                Ok(task) if task.status == TaskStatus::Ok => toast(Tone::Ok, t("mem.saved").to_string()),
                Ok(task) => {
                    let _ = error.try_set(Some(crate::tasks::task_error(&task)));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.text()));
                }
            }
            st.reload();
        });
    };
    view! {
        <section class="card widget w-6" style="--i:0">
            <h2 class="section-title">{tr("mem.zram")}</h2>
            <p class="muted small">{tr("mem.zram_help")}</p>
            {stale_notice(draft.stale, Callback::new(move |_| draft.reset()))}
            {switch("mem.enable", None, enabled, set_enabled)}
            <Show when=move || enabled.get()>
                {ui::note(Tone::Info, icons::INFO, view! {
                    {advice_text.clone()}" "
                    <button type="button" class="link-btn" on:click={
                        let r = recommended.clone();
                        move |_| d.set(r.clone())
                    }>{tr("mem.use_advice")}</button>
                }.into_any())}
                {number_field("mem.size_mib", Some("mem.size_help"), size, set_size, i64::from(MIN_SIZE), i64::from(max))}
                <div class="field">
                    <label class="field-label" for=algo_id.clone()>{tr("mem.algorithm")}</label>
                    <select id=algo_id.clone() class="input" prop:value=move || d.with(|x| x.algorithm.clone()) on:change=move |ev| {
                        let v = event_target_value(&ev);
                        d.update(|a| a.algorithm = v);
                    }>
                        {algorithms.clone().into_iter().map(|a| {
                            let name = a.clone();
                            let shown = a.clone();
                            view! { <option value=a selected=move || d.with(|x| x.algorithm == name)>{shown}</option> }
                        }).collect_view()}
                    </select>
                    <span class="field-help">{tr("mem.algorithm_help")}</span>
                </div>
                {number_field("mem.swappiness", Some("mem.swappiness_help"), swappiness, set_swappiness, 0, 200)}
                {number_field("mem.cluster", Some("mem.cluster_help"), cluster, set_cluster, 0, 5)}
            </Show>
            <Show when=move || needs_install.get()>
                <p class="muted small">{tr("mem.will_install")}</p>
            </Show>
            {form_error(error)}
            <div class="card-actions">
                <button type="button" class="btn ghost" disabled=move || busy.get() || !dirty.get() on:click=move |_| draft.reset()>{tr("cancel")}</button>
                <button type="button" class="btn primary" disabled=move || busy.get() || !can_save.get() on:click=save>
                    {spinner(busy)}{tr("mem.apply")}
                </button>
            </div>
        </section>
    }
    .into_any()
}

/// The notes about this machine that hold for the whole page.
fn notes(st: Remote<ZramState>) -> impl IntoView {
    let one = move |key: &'static str, tone: Tone, on: fn(&ZramState) -> bool| {
        (move || {
            st.data.with(|s| s.as_ref().is_some_and(on)).then(|| {
                let icon = if matches!(tone, Tone::Warn) { icons::TRIANGLE_ALERT } else { icons::INFO };
                ui::note(tone, icon, view! { {tr(key)} }.into_any())
            })
        })
        .into_any()
    };
    view! {
        {one("mem.pending_restart", Tone::Warn, |s| s.pending_restart)}
        {one("mem.zswap", Tone::Warn, |s| s.zswap)}
    }
}

#[component]
pub fn Memory() -> impl IntoView {
    let st = Remote::<ZramState>::new(URL);
    let body = move || {
        let Some(s) = st.data.get_untracked() else {
            return ui::loading(st.err.into(), Callback::new(move |_| st.reload()), 6);
        };
        if !s.supported {
            return ui::note(Tone::Info, icons::INFO, view! { {tr("mem.unsupported")} }.into_any());
        }
        if s.external {
            return view! {
                {ui::note(Tone::Info, icons::INFO, view! { {tr("mem.external")} }.into_any())}
                <div class="grid">{status_card(st)}</div>
            }
            .into_any();
        }
        view! {
            {notes(st)}
            <div class="grid">
                {form_card(s, st)}
                {status_card(st)}
            </div>
        }
        .into_any()
    };
    // the form is built once, when the first answer comes: later answers update it through
    // its draft
    let first = Memo::new(move |_| st.data.with(Option::is_some));
    view! {
        <div class="page">
            <PageHeader icon=icons::MEMORY_STICK title="mem.title" subtitle="mem.subtitle" />
            {move || { first.get(); body() }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(enabled: bool, size_mib: u32, swappiness: u32, page_cluster: u32) -> ZramSettings {
        ZramSettings { enabled, size_mib, algorithm: "zstd".into(), swappiness, page_cluster }
    }

    #[test]
    fn limits() {
        assert!(valid(&s(true, 2048, 180, 0), 4096));
        assert!(!valid(&s(true, 10, 180, 0), 4096));
        assert!(!valid(&s(true, 4097, 180, 0), 4096));
        assert!(!valid(&s(true, 2048, 201, 0), 4096));
        assert!(!valid(&s(true, 2048, 180, 6), 4096));
        assert!(valid(&s(false, 0, 999, 9), 4096), "turning it off needs no values");
    }
}
