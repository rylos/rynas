//! Front LED brightness schedule (synodeb ≥ 4.8): the `led_schedule` / `led_reduced_level`
//! settings of synohwd on the shared week grid (`crate::week_grid`), the current level from the
//! status, and the import of a schedule exported from the Synology system (the raw 168-character
//! string or `synoinfo.conf` lines). Shown only where the status says `led_schedule_supported`.

use leptos::prelude::*;
use rynas_api_types::synology::{LED_SCHEDULE_CODES, LedMode, SynologySettings, SynologyStatus};

use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, DlgKind, Remote, Tone, WidgetHead};
use crate::week_grid::{CELLS, GridLevel, GridTone, normalize, summary_text, week_grid};

/// Normal, reduced, off: the characters of `led_schedule` (the Synology system's
/// `led_brightness_schedule` format: 168 characters, Sunday first, one per hour).
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
    /// A `led_brightness_adjusted_level` that is no number 0–255 (its text): not applied.
    pub bad_level: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportErr {
    /// Neither a schedule nor a level.
    Nothing,
    /// A schedule of only valid characters but the wrong length (its length).
    Length(usize),
    /// A schedule with a character other than 0, 1, 2 (1-based position, character).
    BadChar { pos: usize, ch: char },
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

/// The schedule without white space (a value wrapped over lines when pasted), checked: a wrong
/// character first (where it is), then the length.
fn schedule_of(v: &str) -> Result<String, ImportErr> {
    let s: String = v.chars().filter(|c| !c.is_whitespace()).collect();
    if let Some((i, ch)) = s.chars().enumerate().find(|(_, c)| !LED_SCHEDULE_CODES.contains(c)) {
        return Err(ImportErr::BadChar { pos: i + 1, ch });
    }
    let n = s.chars().count();
    if n != CELLS {
        return Err(ImportErr::Length(n));
    }
    Ok(s)
}

/// A bare pasted text that is meant as a schedule: digits only, or mostly 0/1/2 (so a stray
/// letter is reported where it is), not ordinary words.
fn looks_like_schedule(s: &str) -> bool {
    let n = s.chars().count();
    let valid = s.chars().filter(|c| LED_SCHEDULE_CODES.contains(c)).count();
    n > 0 && (s.chars().all(|c| c.is_ascii_digit()) || valid * 2 > n)
}

/// The pasted text: the bare schedule (quotes and white space allowed), or `synoinfo.conf`
/// lines with `led_brightness_schedule="…"` (its value may run over the next lines) and
/// optionally `led_brightness_adjusted_level="N"` (other lines and `#` comments are skipped).
pub fn parse_dsm_import(text: &str) -> Result<DsmImport, ImportErr> {
    if !text.contains('=') {
        let compact: String = unquote(text).chars().filter(|c| !c.is_whitespace()).collect();
        if !looks_like_schedule(&compact) {
            return Err(ImportErr::Nothing);
        }
        return Ok(DsmImport { schedule: Some(schedule_of(&compact)?), ..Default::default() });
    }
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let mut out = DsmImport::default();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim() {
            "led_brightness_schedule" => {
                let mut v = v.trim().to_string();
                // a value wrapped over lines: an open quote goes on until it is closed, a bare
                // value over the next lines of digits
                loop {
                    let open = match v.chars().next() {
                        Some(q @ ('"' | '\'')) => v.len() < 2 || !v.ends_with(q),
                        _ => lines.get(i).is_some_and(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_digit() || c == '"' || c == '\'')),
                    };
                    match lines.get(i) {
                        Some(next) if open && !next.contains('=') && !next.starts_with('#') => {
                            v.push_str(next);
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.schedule = Some(schedule_of(unquote(&v))?);
            }
            "led_brightness_adjusted_level" => {
                let raw = unquote(v);
                match raw.parse() {
                    Ok(l) => (out.level, out.bad_level) = (Some(l), None),
                    Err(_) => (out.level, out.bad_level) = (None, Some(raw.to_string())),
                }
            }
            _ => {}
        }
    }
    if out.schedule.is_none() && out.level.is_none() { Err(ImportErr::Nothing) } else { Ok(out) }
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
    // while synohwd restarts (after a save) its status is empty: keep the last scale and level
    let sc = Memo::new(move |prev: Option<&Option<(u8, u8, u8)>>| {
        st.data.with(|v| {
            let fresh = v.as_ref().and_then(scale);
            if v.as_ref().is_some_and(|v| v.running) { fresh } else { fresh.or(prev.copied().flatten()) }
        })
    });
    let now = Memo::new(move |prev: Option<&Option<(LedMode, u8)>>| {
        st.data.with(|v| {
            let fresh = v.as_ref().and_then(|v| Some((v.led_mode_now?, v.led_level_now?)));
            if v.as_ref().is_some_and(|v| v.running) { fresh } else { fresh.or(prev.copied().flatten()) }
        })
    });
    let enabled = Memo::new(move |_| d.with(|s| !s.led_schedule.is_empty()));
    let level = Memo::new(move |_| d.with(|s| s.led_reduced_level));
    // the grid edits its own signal: it follows the draft (reset, server change, import) and
    // goes back into the draft once a change is done (not at every cell of a drag)
    let grid = RwSignal::new(d.with_untracked(|s| normalize(&s.led_schedule, &LED_SCHEDULE_CODES, '0')));
    Effect::new(move || {
        d.with(|s| {
            if !s.led_schedule.is_empty() && grid.with_untracked(|g| *g != s.led_schedule) {
                grid.set(normalize(&s.led_schedule, &LED_SCHEDULE_CODES, '0'));
            }
        });
    });
    let commit = Callback::new(move |g: String| {
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
    let choices = Memo::new(move |_| {
        let cur = level.get();
        sc.get().map(|(lo, hi, normal)| reduced_choices(lo, hi, normal, cur)).unwrap_or_else(|| vec![cur])
    });
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
                    {week_grid(LEVELS, grid, "syn.ledsch_grid", Some(commit))}
                    <div class="field">
                        <label class="field-label" for=level_id.clone()>{tr("syn.ledsch_level")}</label>
                        <select id=level_id.clone() class="input syn-level" aria-describedby=help_id.clone()
                            on:change=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<u8>() { d.update(|s| s.led_reduced_level = v) } }>
                            {move || choices.get().into_iter().map(|l| view! {
                                <option value=l.to_string() selected=move || level.get() == l>{l.to_string()}</option>
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

/// An imported reduced level the model takes: within its scale, or 0–7 (the range of the
/// setting) while the scale is unknown.
fn level_fits(sc: Option<(u8, u8, u8)>, l: u8) -> bool {
    match sc {
        Some((lo, hi, _)) => (lo..=hi).contains(&l),
        None => l <= 7,
    }
}

/// "Import from the Synology system": paste, see what was recognised, apply to the draft.
fn import_dialog(d: RwSignal<SynologySettings>, sc: Memo<Option<(u8, u8, u8)>>, close: Callback<()>) -> AnyView {
    let text = RwSignal::new(String::new());
    let parsed = Memo::new(move |_| text.with(|s| (!s.trim().is_empty()).then(|| parse_dsm_import(s))));
    let level_ok = move |l: u8| level_fits(sc.get_untracked(), l);
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
                let mut found = Vec::new();
                let mut warn = Vec::new();
                if let Some(s) = &i.schedule {
                    found.push(tf("syn.ledsch_found_schedule", &[("summary", &summary_text(s, LEVELS))]));
                }
                if let Some(l) = i.level {
                    match sc.get() {
                        Some((lo, hi, _)) if !(lo..=hi).contains(&l) => warn.push(tf(
                            "syn.ledsch_level_outside",
                            &[("level", &l.to_string()), ("lo", &lo.to_string()), ("hi", &hi.to_string())],
                        )),
                        None if !level_fits(None, l) => {
                            warn.push(tf("syn.ledsch_level_outside", &[("level", &l.to_string()), ("lo", "0"), ("hi", "7")]))
                        }
                        _ => found.push(tf("syn.ledsch_found_level", &[("level", &l.to_string())])),
                    }
                }
                if let Some(raw) = &i.bad_level {
                    warn.push(tf("syn.ledsch_level_unreadable", &[("value", raw)]));
                }
                let list = |lines: Vec<String>| view! { <ul class="syn-found">{lines.into_iter().map(|l| view! { <li>{l}</li> }).collect_view()}</ul> }.into_any();
                view! {
                    {(!found.is_empty()).then(|| ui::note(Tone::Ok, icons::CIRCLE_CHECK, list(found)))}
                    {(!warn.is_empty()).then(|| ui::note(Tone::Warn, icons::TRIANGLE_ALERT, list(warn)))}
                }
                .into_any()
            }
            Err(ImportErr::Length(n)) => ui::note(Tone::Err, icons::CIRCLE_X, ui::text(tf("syn.ledsch_err_len", &[("n", &n.to_string())]))).into_any(),
            Err(ImportErr::BadChar { pos, ch }) => ui::note(
                Tone::Err,
                icons::CIRCLE_X,
                ui::text(tf("syn.ledsch_err_char", &[("pos", &pos.to_string()), ("ch", &ch.to_string())])),
            )
            .into_any(),
            Err(ImportErr::Nothing) => ui::note(Tone::Err, icons::CIRCLE_X, ui::text(t("syn.ledsch_err_none").to_string())).into_any(),
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
    let foot = crate::form::save_bar_with(crate::form::SaveBar {
        can_save: Signal::derive(usable),
        icon: Some(icons::IMPORT),
        ..crate::form::SaveBar::new(RwSignal::new(false), "syn.ledsch_apply", close, Callback::new(apply))
    });
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

    fn sched(s: &str) -> DsmImport {
        DsmImport { schedule: Some(s.to_string()), ..Default::default() }
    }

    #[test]
    fn import_raw_string() {
        let s = example_schedule();
        let want = Ok(sched(&s));
        assert_eq!(parse_dsm_import(&s), want);
        assert_eq!(parse_dsm_import(&format!("  \"{s}\"\n")), want);
        assert_eq!(parse_dsm_import(&format!("'{s}'")), want);
        // wrapped over lines when pasted
        assert_eq!(parse_dsm_import(&format!("{}\n{}", &s[..84], &s[84..])), want);
        assert_eq!(parse_dsm_import(&s[..100]), Err(ImportErr::Length(100)));
        assert_eq!(parse_dsm_import(&format!("{s}0")), Err(ImportErr::Length(169)));
        // a wrong character is named with its place, whatever the length
        let mut bad = s.clone();
        bad.replace_range(40..41, "3");
        assert_eq!(parse_dsm_import(&bad), Err(ImportErr::BadChar { pos: 41, ch: '3' }));
        assert_eq!(parse_dsm_import("0120x"), Err(ImportErr::BadChar { pos: 5, ch: 'x' }));
        bad.replace_range(40..41, "x");
        assert_eq!(parse_dsm_import(&bad), Err(ImportErr::BadChar { pos: 41, ch: 'x' }));
    }

    #[test]
    fn import_synoinfo_lines() {
        let s = example_schedule();
        let conf = format!(
            "# exported\nled_brightness=\"3\"\nled_brightness_schedule=\"{s}\"\nled_brightness_adjusted_level=\"1\"\nsupport_led_brightness_adjustment=\"yes\"\n"
        );
        assert_eq!(parse_dsm_import(&conf), Ok(DsmImport { level: Some(1), ..sched(&s) }));
        // spaces around '=', no quotes, schedule only
        assert_eq!(parse_dsm_import(&format!("led_brightness_schedule = {s}")), Ok(sched(&s)));
        // the value wrapped over lines, quoted or not
        let wrapped = format!("led_brightness_schedule=\"{}\n{}\"\nled_brightness_adjusted_level=\"2\"", &s[..84], &s[84..]);
        assert_eq!(parse_dsm_import(&wrapped), Ok(DsmImport { level: Some(2), ..sched(&s) }));
        let wrapped = format!("led_brightness_schedule={}\n{}\n{}\nwol=\"yes\"", &s[..60], &s[60..120], &s[120..]);
        assert_eq!(parse_dsm_import(&wrapped), Ok(sched(&s)));
        // level only
        assert_eq!(parse_dsm_import("led_brightness_adjusted_level=\"2\""), Ok(DsmImport { level: Some(2), ..Default::default() }));
        // a broken schedule line is an error, not skipped
        assert_eq!(parse_dsm_import("led_brightness_schedule=\"0123\""), Err(ImportErr::BadChar { pos: 4, ch: '3' }));
        assert_eq!(parse_dsm_import("led_brightness_schedule=\"0120\""), Err(ImportErr::Length(4)));
        // an unreadable level beside a good schedule: kept as a warning
        assert_eq!(
            parse_dsm_import(&format!("led_brightness_schedule=\"{s}\"\nled_brightness_adjusted_level=\"lots\"")),
            Ok(DsmImport { bad_level: Some("lots".into()), ..sched(&s) })
        );
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
    fn imported_level_without_a_scale() {
        assert!(level_fits(None, 0) && level_fits(None, 7));
        assert!(!level_fits(None, 8) && !level_fits(None, 200));
        assert!(level_fits(Some((1, 3, 3)), 3) && !level_fits(Some((1, 3, 3)), 0));
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
