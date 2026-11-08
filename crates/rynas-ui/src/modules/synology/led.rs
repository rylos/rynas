//! Front LED brightness schedule (synodeb ≥ 4.8): the `led_schedule` / `led_reduced_level`
//! settings of synohwd on the shared week grid (`crate::week_grid`), the current level from the
//! status, and the import of a schedule exported from the Synology system (the raw 168-character
//! string or `synoinfo.conf` lines). Shown only where the status says `led_schedule_supported`.

use leptos::prelude::*;
use rynas_api_types::synology::{LED_SCHEDULE_CODES, LedMode, SynologySettings, SynologyStatus};

use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, DlgKind, Remote, Tone, WidgetHead};
use crate::week_grid::{GridLevel, GridTone, from_dsm, normalize, summary_text, week_grid};

/// Normal, reduced, off: the characters of `led_schedule`.
pub const LEVELS: &[GridLevel] = &[
    GridLevel { code: '0', label_key: "syn.ledsch_normal", tone: GridTone::Full },
    GridLevel { code: '1', label_key: "syn.ledsch_reduced", tone: GridTone::Half },
    GridLevel { code: '2', label_key: "syn.ledsch_off", tone: GridTone::Off },
];

/// What the import text gave.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DsmImport {
    pub schedule: Option<String>,
    pub level: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportErr {
    /// Neither a schedule nor a level.
    Nothing,
    /// A schedule of the wrong length or with other characters (its length).
    BadSchedule(usize),
}

fn unquote(v: &str) -> &str {
    let v = v.trim();
    for q in ['"', '\''] {
        if let Some(x) = v.strip_prefix(q).and_then(|x| x.strip_suffix(q)) {
            return x.trim();
        }
    }
    v
}

fn schedule_of(v: &str) -> Result<String, ImportErr> {
    from_dsm(v, &LED_SCHEDULE_CODES).map_err(|_| ImportErr::BadSchedule(v.chars().count()))
}

/// The pasted text: the bare schedule (quotes and white space allowed), or `synoinfo.conf`
/// lines with `led_brightness_schedule="…"` and optionally `led_brightness_adjusted_level="N"`
/// (other lines and `#` comments are skipped).
pub fn parse_dsm_import(text: &str) -> Result<DsmImport, ImportErr> {
    if !text.contains('=') {
        let compact: String = unquote(text).chars().filter(|c| !c.is_whitespace()).collect();
        if compact.is_empty() || !compact.chars().all(|c| LED_SCHEDULE_CODES.contains(&c)) {
            return Err(ImportErr::Nothing);
        }
        return Ok(DsmImport { schedule: Some(schedule_of(&compact)?), level: None });
    }
    let mut out = DsmImport::default();
    for line in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "led_brightness_schedule" => out.schedule = Some(schedule_of(unquote(v))?),
            "led_brightness_adjusted_level" => out.level = unquote(v).parse().ok(),
            _ => {}
        }
    }
    if out == DsmImport::default() { Err(ImportErr::Nothing) } else { Ok(out) }
}

/// The front LED scale of the model: (dimmest, brightest, normal).
fn scale(st: &SynologyStatus) -> Option<(u8, u8, u8)> {
    let (lo, hi) = (st.led_level_min?, st.led_level_max?);
    Some((lo, hi, st.led_level_default.unwrap_or(hi)))
}

/// Levels offered as "reduced": dimmer than the normal one and not 0 (which is Off in the
/// grid), else the whole scale; the current value always stays choosable.
pub fn reduced_choices(lo: u8, hi: u8, normal: u8, current: u8) -> Vec<u8> {
    let mut v: Vec<u8> = (lo.max(1)..=hi).filter(|&l| l < normal).collect();
    if v.is_empty() {
        v = (lo..=hi).collect();
    }
    if !v.contains(&current) {
        v.push(current);
        v.sort_unstable();
    }
    v
}

fn mode_key(m: LedMode) -> &'static str {
    match m {
        LedMode::Normal => "syn.ledsch_normal",
        LedMode::Reduced => "syn.ledsch_reduced",
        LedMode::Off => "syn.ledsch_off",
    }
}

/// The card; built once while the status says the chip is driven (see [`super::page`]).
pub fn card(d: RwSignal<SynologySettings>, st: Remote<SynologyStatus>) -> AnyView {
    let sc = Memo::new(move |_| st.data.with(|v| v.as_ref().and_then(scale)));
    let now = Memo::new(move |_| st.data.with(|v| v.as_ref().and_then(|v| Some((v.led_mode_now?, v.led_level_now?)))));
    let enabled = Memo::new(move |_| d.with(|s| !s.led_schedule.is_empty()));
    // the grid edits its own signal; it follows the draft (reset, server change) and writes back
    let grid = RwSignal::new(d.with_untracked(|s| normalize(&s.led_schedule, &LED_SCHEDULE_CODES, '0')));
    Effect::new(move || {
        let s = d.with(|s| s.led_schedule.clone());
        if !s.is_empty() && grid.with_untracked(|g| *g != s) {
            grid.set(normalize(&s, &LED_SCHEDULE_CODES, '0'));
        }
    });
    Effect::new(move || {
        let g = grid.get();
        if enabled.get_untracked() && d.with_untracked(|s| s.led_schedule != g) {
            d.update(|s| s.led_schedule = g);
        }
    });
    let on = Signal::derive(move || enabled.get());
    let set_on = Callback::new(move |v: bool| {
        d.update(|s| s.led_schedule = if v { grid.get_untracked() } else { String::new() });
    });
    let level_id = crate::form::fid();
    let help_id = format!("{level_id}h");
    let choices = move || {
        let cur = d.with(|s| s.led_reduced_level);
        sc.get().map(|(lo, hi, normal)| reduced_choices(lo, hi, normal, cur)).unwrap_or_else(|| vec![cur])
    };
    let level_help = move || {
        sc.get()
            .map(|(lo, hi, normal)| {
                let first = reduced_choices(lo, hi, normal, normal).first().copied().unwrap_or(lo);
                tf("syn.ledsch_level_help", &[("lo", &first.to_string()), ("normal", &normal.to_string())])
            })
            .unwrap_or_default()
    };
    let importing = RwSignal::new(false);
    view! {
        <section class="card widget w-12">
            <WidgetHead icon=icons::CALENDAR_CLOCK title="syn.ledsch">
                <button type="button" class="btn ghost sm" on:click=move |_| importing.set(true)>
                    <Icon icon=icons::IMPORT size=14 />{tr("syn.ledsch_import")}
                </button>
            </WidgetHead>
            <div class="form">
                {move || now.get().map(|(m, l)| view! {
                    <p class="sub-row"><span>{tr("syn.ledsch_now")}</span><strong class="tnum">{tf("syn.ledsch_now_value", &[("mode", t(mode_key(m))), ("level", &l.to_string())])}</strong></p>
                })}
                {crate::form::switch("syn.ledsch_on", Some("syn.ledsch_on_help"), on, set_on)}
                <Show when=move || enabled.get()>
                    {week_grid(LEVELS, grid, "syn.ledsch_grid")}
                    <div class="field">
                        <label class="field-label" for=level_id.clone()>{tr("syn.ledsch_level")}</label>
                        <select id=level_id.clone() class="input syn-level" aria-describedby=help_id.clone()
                            on:change=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<u8>() { d.update(|s| s.led_reduced_level = v) } }>
                            {move || choices().into_iter().map(|l| view! {
                                <option value=l.to_string() selected=move || d.with(|s| s.led_reduced_level == l)>{l.to_string()}</option>
                            }).collect_view()}
                        </select>
                        <span class="field-help" id=help_id.clone()>{level_help}</span>
                    </div>
                </Show>
                <p class="field-help">{tr("syn.ledsch_scope")}</p>
            </div>
            <Show when=move || importing.get()>
                {import_dialog(d, sc, Callback::new(move |_| importing.set(false)))}
            </Show>
        </section>
    }
    .into_any()
}

/// "Import from the Synology system": paste, see what was recognised, apply to the draft.
fn import_dialog(d: RwSignal<SynologySettings>, sc: Memo<Option<(u8, u8, u8)>>, close: Callback<()>) -> AnyView {
    let text = RwSignal::new(String::new());
    let parsed = Memo::new(move |_| text.with(|s| (!s.trim().is_empty()).then(|| parse_dsm_import(s))));
    // the level, when the model's scale takes it
    let level_ok = move |l: u8| sc.get_untracked().is_none_or(|(lo, hi, _)| (lo..=hi).contains(&l));
    let usable = move || parsed.with(|p| matches!(p, Some(Ok(i)) if i.schedule.is_some() || i.level.is_some_and(level_ok)));
    let apply = move |_| {
        let Some(Ok(i)) = parsed.get_untracked() else { return };
        d.update(|s| {
            if let Some(sch) = i.schedule {
                s.led_schedule = sch;
            }
            if let Some(l) = i.level.filter(|&l| level_ok(l)) {
                s.led_reduced_level = l;
            }
        });
        close.run(());
    };
    let id = crate::form::fid();
    let hid = format!("{id}h");
    let result = move || {
        parsed.get().map(|p| match p {
            Ok(i) => {
                let mut lines = Vec::new();
                if let Some(s) = &i.schedule {
                    lines.push(tf("syn.ledsch_found_schedule", &[("summary", &summary_text(s, LEVELS))]));
                }
                if let Some(l) = i.level {
                    match sc.get() {
                        Some((lo, hi, _)) if !(lo..=hi).contains(&l) => lines.push(tf(
                            "syn.ledsch_level_outside",
                            &[("level", &l.to_string()), ("lo", &lo.to_string()), ("hi", &hi.to_string())],
                        )),
                        _ => lines.push(tf("syn.ledsch_found_level", &[("level", &l.to_string())])),
                    }
                }
                ui::note(Tone::Ok, icons::CIRCLE_CHECK, view! { <ul class="syn-found">{lines.into_iter().map(|l| view! { <li>{l}</li> }).collect_view()}</ul> }.into_any())
            }
            Err(ImportErr::BadSchedule(n)) => ui::note(Tone::Err, icons::CIRCLE_X, ui::text(tf("syn.ledsch_err_len", &[("n", &n.to_string())]))),
            Err(ImportErr::Nothing) => ui::note(Tone::Err, icons::CIRCLE_X, ui::text(t("syn.ledsch_err_none").to_string())),
        })
    };
    let body = view! {
        <div class="field">
            <label class="field-label" for=id.clone()>{tr("syn.ledsch_import_text")}</label>
            <textarea id=id class="input area mono syn-import" rows="5" spellcheck="false" aria-describedby=hid.clone()
                placeholder="led_brightness_schedule=\"…\""
                prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))></textarea>
            <span class="field-help" id=hid>{tr("syn.ledsch_import_help")}</span>
        </div>
        <div aria-live="polite">{result}</div>
    }
    .into_any();
    let foot = view! {
        <button type="button" class="btn ghost" on:click=move |_| close.run(())>{tr("cancel")}</button>
        <button type="button" class="btn primary" disabled=move || !usable() on:click=apply>
            <Icon icon=icons::IMPORT size=16 />{tr("syn.ledsch_apply")}
        </button>
    }
    .into_any();
    ui::dialog(DlgKind::Normal, Signal::derive(|| t("syn.ledsch_import").to_string()), close, body, Some(foot))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example schedule of the demo check (stored order, Sunday = 0): every day 08–17
    /// normal, 17–01 reduced, 01–07 off, 07–08 reduced; on Saturday and Sunday the night stays
    /// reduced until 02.
    pub fn example_schedule() -> String {
        (0..7)
            .flat_map(|day| {
                (0..24).map(move |h| match h {
                    0 => '1',
                    1 if day == 0 || day == 6 => '1',
                    1..=6 => '2',
                    7 => '1',
                    8..=16 => '0',
                    _ => '1',
                })
            })
            .collect()
    }

    #[test]
    fn example_is_a_valid_schedule() {
        let s = example_schedule();
        assert_eq!(s.len(), 168);
        assert!(rynas_api_types::synology::led_schedule_valid(&s));
        // Monday (stored day 1): 01–07 off
        assert_eq!(&s[24..48], "122222210000000001111111");
        // Saturday (day 6): reduced until 02
        assert_eq!(&s[6 * 24..6 * 24 + 3], "112");
    }

    #[test]
    fn import_raw_string() {
        let s = example_schedule();
        let want = Ok(DsmImport { schedule: Some(s.clone()), level: None });
        assert_eq!(parse_dsm_import(&s), want);
        assert_eq!(parse_dsm_import(&format!("  \"{s}\"\n")), want);
        assert_eq!(parse_dsm_import(&format!("'{s}'")), want);
        // wrapped over lines when pasted
        assert_eq!(parse_dsm_import(&format!("{}\n{}", &s[..84], &s[84..])), want);
        assert_eq!(parse_dsm_import(&s[..100]), Err(ImportErr::BadSchedule(100)));
        assert_eq!(parse_dsm_import(&format!("{s}0")), Err(ImportErr::BadSchedule(169)));
    }

    #[test]
    fn import_synoinfo_lines() {
        let s = example_schedule();
        let conf = format!(
            "# exported\nled_brightness=\"3\"\nled_brightness_schedule=\"{s}\"\nled_brightness_adjusted_level=\"1\"\nsupport_led_brightness_adjustment=\"yes\"\n"
        );
        assert_eq!(parse_dsm_import(&conf), Ok(DsmImport { schedule: Some(s.clone()), level: Some(1) }));
        // spaces around '=', no quotes, schedule only
        assert_eq!(
            parse_dsm_import(&format!("led_brightness_schedule = {s}")),
            Ok(DsmImport { schedule: Some(s.clone()), level: None })
        );
        // level only
        assert_eq!(parse_dsm_import("led_brightness_adjusted_level=\"2\""), Ok(DsmImport { schedule: None, level: Some(2) }));
        // a broken schedule line is an error, not skipped
        assert_eq!(parse_dsm_import("led_brightness_schedule=\"0123\""), Err(ImportErr::BadSchedule(4)));
        // commented out: not taken
        assert_eq!(parse_dsm_import(&format!("#led_brightness_schedule=\"{s}\"")), Err(ImportErr::Nothing));
    }

    #[test]
    fn import_garbage() {
        assert_eq!(parse_dsm_import(""), Err(ImportErr::Nothing));
        assert_eq!(parse_dsm_import("   \n"), Err(ImportErr::Nothing));
        assert_eq!(parse_dsm_import("hello world"), Err(ImportErr::Nothing));
        assert_eq!(parse_dsm_import("a=b\nc=d"), Err(ImportErr::Nothing));
        assert_eq!(parse_dsm_import("led_brightness_adjusted_level=\"lots\""), Err(ImportErr::Nothing));
        assert_eq!(parse_dsm_import("led_brightness_adjusted_level=\"-1\""), Err(ImportErr::Nothing));
    }

    #[test]
    fn reduced_levels_offered() {
        // DS923+: 0..3, normal 3 → 1 and 2 (0 is Off in the grid, 3 is normal)
        assert_eq!(reduced_choices(0, 3, 3, 1), vec![1, 2]);
        // a value from elsewhere stays choosable
        assert_eq!(reduced_choices(0, 3, 3, 0), vec![0, 1, 2]);
        // a two-level scale: nothing in between, the whole scale
        assert_eq!(reduced_choices(0, 1, 1, 1), vec![0, 1]);
        assert_eq!(reduced_choices(1, 7, 7, 3), vec![1, 2, 3, 4, 5, 6]);
    }
}
