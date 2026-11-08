//! Weekly schedule grid: 7 days × 24 hours, one level per hour (docs/phase2/week-grid.md).
//!
//! The value is a 168-character string, **Sunday first**, one character per hour
//! (`value[day * 24 + hour]`, day 0 = Sunday): the format of the LED brightness, backup and
//! sync schedules of the Synology NAS UI, so a schedule read from synohwd or a migrated
//! configuration is stored as is ([`from_dsm`] / [`to_dsm`] only validate). The grid shows the
//! week Monday first (the app has no locale setting for the first day; en and it both start on
//! Monday here): [`index`] maps a display cell to the stored one.
//!
//! - [`week_grid`]: the editor (pick a level, click or drag to paint, day/hour headers fill a
//!   row/column, keyboard with a roving focus, ARIA grid).
//! - [`week_grid_preview`]: read-only, compact (lists and cards), its summary as the label.
//! - [`week_grid_legend`]: the levels with their swatches.
//!
//! Level codes are ASCII characters; 2 to 4 levels; the first level is the default one (left
//! out of the summary, used for unreadable cells).
#![cfg_attr(not(feature = "demo"), allow(dead_code))]

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::i18n::{t, tf, tr};
use crate::pages::scheduler::DAY_KEYS;

/// Hours in a week, the length of a value.
pub const CELLS: usize = 168;

/// Below this width (px) of the grid's box, days become columns and hours rows (keep in step
/// with `@container wg` in main.css).
const NARROW_PX: i32 = 560;

/// Long day names, Monday first (display order).
const DAY_LONG: [&str; 7] = ["day.long.1", "day.long.2", "day.long.3", "day.long.4", "day.long.5", "day.long.6", "day.long.7"];

/// How a level is drawn: a colour from the theme tokens plus a pattern, so levels differ
/// without colour too (solid, stripes, dots, empty outlined).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GridTone {
    Full,
    Half,
    Low,
    Off,
}

impl GridTone {
    fn class(self) -> &'static str {
        match self {
            GridTone::Full => "wg-full",
            GridTone::Half => "wg-half",
            GridTone::Low => "wg-low",
            GridTone::Off => "wg-off",
        }
    }
}

/// One level of a grid: its character in the value, its name, its look.
#[derive(Clone, Copy, Debug)]
pub struct GridLevel {
    pub code: char,
    pub label_key: &'static str,
    pub tone: GridTone,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GridError {
    /// Not 168 characters (the length found).
    Length(usize),
    /// A character that is no level (stored index, character).
    Code { at: usize, ch: char },
}

/// `Ok` when `value` has 168 characters, each one of `codes`.
pub fn validate(value: &str, codes: &[char]) -> Result<(), GridError> {
    let n = value.chars().count();
    if n != CELLS {
        return Err(GridError::Length(n));
    }
    match value.chars().enumerate().find(|(_, c)| !codes.contains(c)) {
        Some((at, ch)) => Err(GridError::Code { at, ch }),
        None => Ok(()),
    }
}

/// A schedule string of the Synology UI (Sunday first, one character per hour) as a grid
/// value: the same string, validated.
pub fn from_dsm(s: &str, codes: &[char]) -> Result<String, GridError> {
    validate(s, codes)?;
    Ok(s.to_string())
}

/// A grid value as a schedule string of the Synology UI: the same string, validated.
#[allow(dead_code, reason = "API for the features that store a schedule (LED, backup windows), tested here")]
pub fn to_dsm(value: &str, codes: &[char]) -> Result<String, GridError> {
    from_dsm(value, codes)
}

/// The whole week at one level.
pub fn filled(code: char) -> String {
    std::iter::repeat_n(code, CELLS).collect()
}

/// `value` made valid: a wrong length becomes all `base`, an unknown character `base`.
pub fn normalize(value: &str, codes: &[char], base: char) -> String {
    if value.chars().count() != CELLS {
        return filled(base);
    }
    value.chars().map(|c| if codes.contains(&c) { c } else { base }).collect()
}

/// Stored index of the cell in display row `row` (0 = Monday … 6 = Sunday) and `hour`.
pub fn index(row: usize, hour: usize) -> usize {
    (row + 1) % 7 * 24 + hour
}

/// The value in display order (Monday first, `row * 24 + hour`). `value` must be valid.
#[allow(dead_code, reason = "API for the features that store a schedule (LED, backup windows), tested here")]
pub fn rotate_for_display(value: &str) -> String {
    let b = value.as_bytes();
    (0..CELLS).map(|di| b[index(di / 24, di % 24)] as char).collect()
}

/// Inverse of [`rotate_for_display`].
#[allow(dead_code, reason = "API for the features that store a schedule (LED, backup windows), tested here")]
pub fn rotate_to_storage(display: &str) -> String {
    let b = display.as_bytes();
    (0..CELLS).map(|si| b[(si / 24 + 6) % 7 * 24 + si % 24] as char).collect()
}

/// `value` with the rectangle between display cells `a` and `b` (`row * 24 + hour`, any two
/// corners) set to `code`. A value that is not 168 ASCII characters comes back unchanged.
pub fn set_rect(value: &str, a: usize, b: usize, code: char) -> String {
    if value.len() != CELLS || !value.is_ascii() || !code.is_ascii() || a >= CELLS || b >= CELLS {
        return value.to_string();
    }
    let mut cells = value.as_bytes().to_vec();
    let (r0, r1) = ((a / 24).min(b / 24), (a / 24).max(b / 24));
    let (h0, h1) = ((a % 24).min(b % 24), (a % 24).max(b % 24));
    for r in r0..=r1 {
        for h in h0..=h1 {
            cells[index(r, h)] = code as u8;
        }
    }
    String::from_utf8(cells).unwrap_or_else(|_| value.to_string())
}

/// Days with the same hours, for a readable summary.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SummaryGroup {
    /// Display rows (0 = Monday), as inclusive ranges of consecutive days.
    pub days: Vec<(usize, usize)>,
    /// Runs of hours `[from, to)` (to ≤ 24) and their level, `skip` left out.
    pub runs: Vec<Run>,
}

/// Hours `[from, to)` of one day at one level.
pub type Run = (usize, usize, char);

/// The week as groups of days sharing the same runs, Monday first; hours at `skip` (the
/// default level) are left out and days entirely at `skip` too. Runs stop at midnight. `value`
/// must be valid.
pub fn summary(value: &str, skip: Option<char>) -> Vec<SummaryGroup> {
    let b = value.as_bytes();
    let mut groups: Vec<(Vec<usize>, Vec<Run>)> = Vec::new();
    for row in 0..7 {
        let mut runs = Vec::new();
        let mut h = 0;
        while h < 24 {
            let c = b[index(row, h)] as char;
            let start = h;
            while h < 24 && b[index(row, h)] as char == c {
                h += 1;
            }
            if Some(c) != skip {
                runs.push((start, h, c));
            }
        }
        if runs.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|g| g.1 == runs) {
            Some(g) => g.0.push(row),
            None => groups.push((vec![row], runs)),
        }
    }
    groups
        .into_iter()
        .map(|(rows, runs)| {
            let mut days: Vec<(usize, usize)> = Vec::new();
            for r in rows {
                match days.last_mut() {
                    Some(last) if last.1 + 1 == r => last.1 = r,
                    _ => days.push((r, r)),
                }
            }
            SummaryGroup { days, runs }
        })
        .collect()
}

fn codes_of(levels: &[GridLevel]) -> Vec<char> {
    levels.iter().map(|l| l.code).collect()
}

fn level_label(levels: &[GridLevel], code: char) -> &'static str {
    levels.iter().find(|l| l.code == code).map_or("", |l| t(l.label_key))
}

fn tone_of(levels: &[GridLevel], code: Option<u8>) -> &'static str {
    let found = code.and_then(|c| levels.iter().find(|l| l.code as u32 == u32::from(c)));
    found.unwrap_or(&levels[0]).tone.class()
}

/// "Mon–Fri 08–17 Normal, 17–24 Reduced; Sat–Sun 00–24 Off", in the current language.
pub fn summary_text(value: &str, levels: &[GridLevel]) -> String {
    let base = levels[0].code;
    let v = normalize(value, &codes_of(levels), base);
    let groups = summary(&v, Some(base));
    if groups.is_empty() {
        return tf("wg.all_week", &[("level", level_label(levels, base))]);
    }
    groups
        .iter()
        .map(|g| {
            let days = if g.days == [(0, 6)] {
                t("sch.rep.daily").to_string()
            } else {
                g.days
                    .iter()
                    .map(|&(a, b)| if a == b { t(DAY_KEYS[a]).to_string() } else { format!("{}–{}", t(DAY_KEYS[a]), t(DAY_KEYS[b])) })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let runs = g.runs.iter().map(|&(a, b, c)| format!("{a:02}–{b:02} {}", level_label(levels, c))).collect::<Vec<_>>().join(", ");
            format!("{days} {runs}")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The display cell (`row * 24 + hour`) under the point, when it belongs to `grid`.
fn cell_at(grid: &web_sys::Element, x: i32, y: i32) -> Option<usize> {
    let el = document().element_from_point(x as f32, y as f32)?;
    let cell = el.closest("[data-i]").ok().flatten()?;
    if !grid.contains(Some(&cell)) {
        return None;
    }
    cell.get_attribute("data-i")?.parse().ok().filter(|&i| i < CELLS)
}

/// Segmented control of the level to paint with (radio group, arrows, swatch per level).
fn level_picker(levels: &'static [GridLevel], active: RwSignal<usize>) -> impl IntoView {
    let n = levels.len();
    view! {
        <div class="segmented wg-levels" role="radiogroup" aria-label=tr("wg.level")>
            {levels
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let on = move || active.get() == i;
                    view! {
                        <button
                            type="button"
                            role="radio"
                            aria-checked=move || on().to_string()
                            tabindex=move || if on() { "0" } else { "-1" }
                            class:on=on
                            on:click=move |_| active.set(i)
                            on:keydown=move |ev| {
                                let next = match ev.key().as_str() {
                                    "ArrowRight" | "ArrowDown" => (i + 1) % n,
                                    "ArrowLeft" | "ArrowUp" => (i + n - 1) % n,
                                    _ => return,
                                };
                                ev.prevent_default();
                                active.set(next);
                                if let Some(p) = event_target::<web_sys::HtmlElement>(&ev).parent_element()
                                    && let Some(b) = p.children().item(next as u32)
                                {
                                    let _ = b.unchecked_into::<web_sys::HtmlElement>().focus();
                                }
                            }
                        >
                            <span class=format!("wg-swatch {}", l.tone.class()) aria-hidden="true"></span>
                            {tr(l.label_key)}
                            <span class="wg-key" aria-hidden="true">{(i + 1).to_string()}</span>
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}

/// The levels with their swatches (beside a [`week_grid_preview`]).
pub fn week_grid_legend(levels: &'static [GridLevel]) -> AnyView {
    view! {
        <ul class="wg-legend">
            {levels
                .iter()
                .map(|l| view! { <li><span class=format!("wg-swatch {}", l.tone.class()) aria-hidden="true"></span>{tr(l.label_key)}</li> })
                .collect_view()}
        </ul>
    }
    .into_any()
}

/// Read-only compact grid (24 × 7 small cells, Monday first); its label is the summary.
pub fn week_grid_preview(levels: &'static [GridLevel], value: Signal<String>, label_key: &'static str) -> AnyView {
    let label = move || format!("{}: {}", t(label_key), value.with(|v| summary_text(v, levels)));
    view! {
        <div class="wg-mini" role="img" aria-label=label title=label>
            {(0..CELLS)
                .map(|di| {
                    let si = index(di / 24, di % 24);
                    view! { <span class=move || value.with(|v| tone_of(levels, v.as_bytes().get(si).copied()))></span> }
                })
                .collect_view()}
        </div>
    }
    .into_any()
}

/// The editor. `value` holds the 168-character schedule (Sunday first); `label_key` names the
/// grid for assistive technology. Painting writes `value` at once (a drag updates it live).
pub fn week_grid(levels: &'static [GridLevel], value: RwSignal<String>, label_key: &'static str) -> AnyView {
    debug_assert!((2..=4).contains(&levels.len()) && levels.iter().all(|l| l.code.is_ascii()));
    let base = levels[0].code;
    let active = RwSignal::new(0usize);
    // the cell with tabindex 0 (roving focus), display index
    let focus = RwSignal::new(0usize);
    // anchor, last cell, value before the drag
    let drag = StoredValue::new(None::<(usize, usize, String)>);
    let wrap = NodeRef::<leptos::html::Div>::new();
    let grid = NodeRef::<leptos::html::Div>::new();
    let help_id = crate::form::fid();

    let current = move || value.with_untracked(|v| normalize(v, &codes_of(levels), base));
    let invalid = move || value.with(|v| validate(v, &codes_of(levels)).is_err());
    let paint = move |from: &str, a: usize, b: usize| value.set(set_rect(from, a, b, levels[active.get_untracked()].code));
    let vertical = move || wrap.get_untracked().is_some_and(|w| w.client_width() < NARROW_PX);
    let focus_cell = move |di: usize| {
        focus.set(di);
        if let Some(g) = grid.get_untracked()
            && let Ok(Some(el)) = g.query_selector(&format!("[data-i=\"{di}\"]"))
        {
            let _ = el.unchecked_into::<web_sys::HtmlElement>().focus();
        }
    };
    let level_name = move || t(levels[active.get()].label_key);

    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        let key = ev.key();
        if key.len() == 1
            && let Some(d) = key.chars().next().and_then(|c| c.to_digit(10))
        {
            if (1..=levels.len()).contains(&(d as usize)) {
                ev.prevent_default();
                active.set(d as usize - 1);
            }
            return;
        }
        // keys of the cells only (the header buttons have their own)
        if event_target::<web_sys::Element>(&ev).get_attribute("data-i").is_none() {
            return;
        }
        let di = focus.get_untracked();
        let (r, h) = (di / 24, di % 24);
        let v = vertical();
        let (dr, dh): (isize, isize) = match key.as_str() {
            "ArrowRight" if v => (1, 0),
            "ArrowRight" => (0, 1),
            "ArrowLeft" if v => (-1, 0),
            "ArrowLeft" => (0, -1),
            "ArrowDown" if v => (0, 1),
            "ArrowDown" => (1, 0),
            "ArrowUp" if v => (0, -1),
            "ArrowUp" => (-1, 0),
            "Home" => (0, -(h as isize)),
            "End" => (0, 23 - h as isize),
            " " | "Enter" => {
                ev.prevent_default();
                let from = current();
                if ev.shift_key() { paint(&from, r * 24, r * 24 + 23) } else { paint(&from, di, di) }
                return;
            }
            _ => return,
        };
        ev.prevent_default();
        let nr = (r as isize + dr).clamp(0, 6) as usize;
        let nh = (h as isize + dh).clamp(0, 23) as usize;
        focus_cell(nr * 24 + nh);
    };
    let on_down = move |ev: web_sys::PointerEvent| {
        let Some(g) = grid.get_untracked() else { return };
        if ev.button() != 0 {
            return;
        }
        let Some(di) = cell_at(&g, ev.client_x(), ev.client_y()) else { return };
        ev.prevent_default();
        let _ = g.set_pointer_capture(ev.pointer_id());
        let from = current();
        paint(&from, di, di);
        drag.set_value(Some((di, di, from)));
        focus_cell(di);
    };
    let on_move = move |ev: web_sys::PointerEvent| {
        if drag.with_value(Option::is_none) {
            return;
        }
        let Some(g) = grid.get_untracked() else { return };
        let Some(di) = cell_at(&g, ev.client_x(), ev.client_y()) else { return };
        let code = levels[active.get_untracked()].code;
        let mut next = None;
        drag.update_value(|d| {
            if let Some((a, last, from)) = d
                && *last != di
            {
                *last = di;
                next = Some(set_rect(from, *a, di, code));
            }
        });
        if let Some(v) = next {
            value.set(v);
            focus.set(di);
        }
    };
    let end = move || {
        // the keyboard goes on from where the drag ended
        if let Some((_, last, _)) = drag.get_value() {
            drag.set_value(None);
            focus_cell(last);
        }
    };
    let cancel = move |_| {
        if let Some((_, _, from)) = drag.get_value() {
            value.set(from);
        }
        drag.set_value(None);
    };

    let hour_text = |h: usize| format!("{h:02}:00");
    let header = view! {
        <div role="row">
            <div class="wg-corner" role="columnheader"><span class="sr-only">{tr("wg.hours")}</span></div>
            {(0..24)
                .map(|h| {
                    let label = move || tf("wg.fill_hour", &[("from", &hour_text(h)), ("to", &hour_text(h + 1)), ("level", level_name())]);
                    let fill = move |_| paint(&current(), h, 6 * 24 + h);
                    view! {
                        <div class="wg-hour" role="columnheader" style=format!("--h:{h}")>
                            <button type="button" tabindex="-1" aria-label=label title=label on:click=fill>{format!("{h:02}")}</button>
                        </div>
                    }
                })
                .collect_view()}
        </div>
    };
    let rows = (0..7)
        .map(|r| {
            let day_label = move || tf("wg.fill_day", &[("day", t(DAY_LONG[r])), ("level", level_name())]);
            let fill = move |_| paint(&current(), r * 24, r * 24 + 23);
            view! {
                <div role="row">
                    <div class="wg-day" role="rowheader" style=format!("--r:{r}")>
                        <button type="button" tabindex="-1" aria-label=day_label title=day_label on:click=fill>{tr(DAY_KEYS[r])}</button>
                    </div>
                    {(0..24)
                        .map(|h| {
                            let di = r * 24 + h;
                            let si = index(r, h);
                            let code = move || value.with(|v| v.as_bytes().get(si).copied());
                            let label = move || {
                                let c = code().map_or(base, char::from);
                                let c = if levels.iter().any(|l| l.code == c) { c } else { base };
                                tf("wg.cell", &[("day", t(DAY_LONG[r])), ("from", &hour_text(h)), ("to", &hour_text(h + 1)), ("level", level_label(levels, c))])
                            };
                            view! {
                                <div
                                    role="gridcell"
                                    class=move || format!("wg-cell {}", tone_of(levels, code()))
                                    style=format!("--r:{r};--h:{h}")
                                    data-i=di.to_string()
                                    tabindex=move || if focus.get() == di { "0" } else { "-1" }
                                    aria-label=label
                                ></div>
                            }
                        })
                        .collect_view()}
                </div>
            }
        })
        .collect_view();

    view! {
        <div class="wg" node_ref=wrap>
            {level_picker(levels, active)}
            <div
                class="wg-grid"
                role="grid"
                node_ref=grid
                aria-label=tr(label_key)
                aria-describedby=help_id.clone()
                on:keydown=on_keydown
                on:focusin=move |ev| {
                    // a cell focused by click or script becomes the roving one
                    if let Some(i) = event_target::<web_sys::Element>(&ev).get_attribute("data-i").and_then(|i| i.parse().ok()) {
                        focus.set(i);
                    }
                }
                on:pointerdown=on_down
                on:pointermove=on_move
                on:pointerup=move |_| end()
                on:lostpointercapture=move |_| end()
                on:pointercancel=cancel
            >
                {header}
                {rows}
            </div>
            <Show when=invalid>
                <p class="wg-help">{move || tf("wg.invalid", &[("level", t(levels[0].label_key))])}</p>
            </Show>
            <p class="wg-summary" aria-live="polite">{move || value.with(|v| summary_text(v, levels))}</p>
            <p class="wg-help" id=help_id>{move || tf("wg.help", &[("n", &levels.len().to_string())])}</p>
        </div>
    }
    .into_any()
}

/// Dev-only page with sample grids (demo build: `/dev/week-grid`), for screenshot checks.
#[cfg(feature = "demo")]
#[component]
pub fn Showcase() -> impl IntoView {
    use crate::icons;
    use crate::ui::{PageHeader, WidgetHead};
    const LED: &[GridLevel] = &[
        GridLevel { code: '0', label_key: "wgd.normal", tone: GridTone::Full },
        GridLevel { code: '1', label_key: "wgd.reduced", tone: GridTone::Half },
        GridLevel { code: '2', label_key: "wgd.off", tone: GridTone::Off },
    ];
    const WINDOW: &[GridLevel] = &[
        GridLevel { code: '0', label_key: "wgd.paused", tone: GridTone::Off },
        GridLevel { code: '1', label_key: "wgd.allowed", tone: GridTone::Full },
    ];
    const SPEED: &[GridLevel] = &[
        GridLevel { code: 'u', label_key: "wgd.unlimited", tone: GridTone::Full },
        GridLevel { code: 'l', label_key: "wgd.limited", tone: GridTone::Half },
        GridLevel { code: 's', label_key: "wgd.slow", tone: GridTone::Low },
        GridLevel { code: 'p', label_key: "wgd.paused", tone: GridTone::Off },
    ];
    // nights off, evenings reduced
    let mut led = set_rect(&filled('0'), 0, 6 * 24 + 6, '2');
    led = set_rect(&led, 19, 6 * 24 + 23, '1');
    let led = RwSignal::new(led);
    // nights every day, weekend all day
    let window = RwSignal::new(set_rect(&set_rect(&filled('0'), 1, 6 * 24 + 5, '1'), 5 * 24, 6 * 24 + 23, '1'));
    let speed = RwSignal::new(set_rect(&set_rect(&set_rect(&filled('u'), 8, 4 * 24 + 17, 'l'), 12, 4 * 24 + 13, 's'), 5 * 24 + 2, 6 * 24 + 3, 'p'));
    let card = move |title: &'static str, levels: &'static [GridLevel], v: RwSignal<String>| {
        view! {
            <section class="card widget w-12">
                <WidgetHead icon=icons::CALENDAR_CLOCK title=title />
                {week_grid(levels, v, title)}
            </section>
        }
    };
    let mini = move |title: &'static str, levels: &'static [GridLevel], v: RwSignal<String>| {
        view! {
            <div class="wgd-item">
                <strong>{tr(title)}</strong>
                {week_grid_preview(levels, v.into(), title)}
                {week_grid_legend(levels)}
                <code class="wgd-raw">{move || v.get()}</code>
            </div>
        }
    };
    view! {
        <div class="page">
            <PageHeader icon=icons::CALENDAR_CLOCK title="wgd.title" subtitle="wgd.subtitle" />
            <div class="grid">
                {card("wgd.led", LED, led)}
                {card("wgd.window", WINDOW, window)}
                {card("wgd.bandwidth", SPEED, speed)}
                <section class="card widget w-12">
                    <WidgetHead icon=icons::CALENDAR_CLOCK title="wgd.preview" />
                    <div class="wgd-minis">
                        {mini("wgd.led", LED, led)}
                        {mini("wgd.window", WINDOW, window)}
                        {mini("wgd.bandwidth", SPEED, speed)}
                    </div>
                </section>
            </div>
        </div>
    }
}

/// Without the demo build the showcase route is a plain "not found".
#[cfg(not(feature = "demo"))]
#[component]
pub fn Showcase() -> impl IntoView {
    view! { <crate::pages::NotFound /> }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODES: &[char] = &['0', '1', '2'];
    const LEVELS: &[GridLevel] = &[
        GridLevel { code: '0', label_key: "wgd.normal", tone: GridTone::Full },
        GridLevel { code: '1', label_key: "wgd.reduced", tone: GridTone::Half },
        GridLevel { code: '2', label_key: "wgd.off", tone: GridTone::Off },
    ];

    #[test]
    fn validation() {
        assert_eq!(validate(&filled('0'), CODES), Ok(()));
        assert_eq!(validate("012", CODES), Err(GridError::Length(3)));
        let mut s = filled('1');
        s.replace_range(30..31, "x");
        assert_eq!(validate(&s, CODES), Err(GridError::Code { at: 30, ch: 'x' }));
        // a multi-byte character counts once
        let s = format!("é{}", "0".repeat(167));
        assert_eq!(validate(&s, CODES), Err(GridError::Code { at: 0, ch: 'é' }));
        assert_eq!(normalize(&s, CODES, '1').chars().next(), Some('1'));
        assert_eq!(normalize("00", CODES, '2'), filled('2'));
    }

    #[test]
    fn dsm_strings_are_kept_as_they_are() {
        let s: String = (0..CELLS).map(|i| CODES[i % 3]).collect();
        assert_eq!(from_dsm(&s, CODES).as_deref(), Ok(s.as_str()));
        assert_eq!(to_dsm(&s, CODES).as_deref(), Ok(s.as_str()));
        assert!(from_dsm(&s[1..], CODES).is_err());
    }

    #[test]
    fn display_is_monday_first() {
        // stored Sunday 00:00 is index 0; displayed in the last row
        assert_eq!(index(6, 0), 0);
        assert_eq!(index(0, 0), 24);
        assert_eq!(index(5, 23), 6 * 24 + 23);
        let s: String = (0..CELLS).map(|i| char::from(b'a' + (i / 24) as u8)).collect();
        let d = rotate_for_display(&s);
        assert!(d.starts_with(&"b".repeat(24)) && d.ends_with(&"a".repeat(24)));
        assert_eq!(rotate_to_storage(&d), s);
    }

    #[test]
    fn rectangles_in_display_coordinates() {
        // Monday–Friday 08–16, corners given backwards
        let v = set_rect(&filled('0'), 4 * 24 + 16, 8, '1');
        let d = rotate_for_display(&v);
        for row in 0..7 {
            for h in 0..24 {
                let want = if row <= 4 && (8..=16).contains(&h) { '1' } else { '0' };
                assert_eq!(d.as_bytes()[row * 24 + h] as char, want, "row {row} hour {h}");
            }
        }
        assert_eq!(v.matches('1').count(), 5 * 9);
        // invalid input comes back unchanged
        assert_eq!(set_rect("00", 0, 1, '1'), "00");
    }

    #[test]
    fn summary_groups_days() {
        let v = set_rect(&filled('0'), 17, 4 * 24 + 23, '1');
        let v = set_rect(&v, 5 * 24, 6 * 24 + 23, '2');
        assert_eq!(
            summary(&v, Some('0')),
            vec![SummaryGroup { days: vec![(0, 4)], runs: vec![(17, 24, '1')] }, SummaryGroup { days: vec![(5, 6)], runs: vec![(0, 24, '2')] }]
        );
        // non-consecutive days with the same hours share a group
        let v = set_rect(&set_rect(&filled('0'), 8, 8, '1'), 2 * 24 + 8, 2 * 24 + 8, '1');
        assert_eq!(summary(&v, Some('0')), vec![SummaryGroup { days: vec![(0, 0), (2, 2)], runs: vec![(8, 9, '1')] }]);
        assert!(summary(&filled('0'), Some('0')).is_empty());
        assert_eq!(summary(&filled('2'), None), vec![SummaryGroup { days: vec![(0, 6)], runs: vec![(0, 24, '2')] }]);
    }

    #[test]
    fn summary_text_reads_well() {
        let v = set_rect(&filled('0'), 17, 4 * 24 + 23, '1');
        let v = set_rect(&v, 5 * 24, 6 * 24 + 23, '2');
        assert_eq!(summary_text(&v, LEVELS), "Mon–Fri 17–24 Reduced; Sat–Sun 00–24 Off");
        assert_eq!(summary_text(&filled('0'), LEVELS), "All week: Normal");
        assert_eq!(summary_text(&set_rect(&filled('0'), 0, 6 * 24 + 6, '2'), LEVELS), "Every day 00–07 Off");
        // an unreadable value reads as the default level
        assert_eq!(summary_text("bad", LEVELS), "All week: Normal");
    }
}
