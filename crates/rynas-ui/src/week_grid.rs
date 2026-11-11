//! Weekly schedule grid: 7 days × 24 hours, one level per hour (docs/phase2/week-grid.md).
//!
//! The value is a 168-character string, **Sunday first**, one character per hour
//! (`value[day * 24 + hour]`, day 0 = Sunday), the way the features that use the grid store
//! their schedules: the grid edits it as is ([`validate`] / [`normalize`] check or repair it).
//! The grid shows the week Monday first (the app has no locale setting for the first day; en
//! and it both start on Monday here): [`index`] maps a display cell to the stored one.
//!
//! - [`week_grid`]: the editor (pick a level, click or drag to paint, day/hour headers fill a
//!   row/column, keyboard with a roving focus, ARIA grid). On a touch screen the page scrolls
//!   over the grid: a tap paints one cell, a hold ([`HOLD_MS`]) starts a rectangle.
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

    /// The class of a grid cell at this tone (static, no string built per cell).
    fn cell_class(self) -> &'static str {
        match self {
            GridTone::Full => "wg-cell wg-full",
            GridTone::Half => "wg-cell wg-half",
            GridTone::Low => "wg-cell wg-low",
            GridTone::Off => "wg-cell wg-off",
        }
    }
}

/// How long a finger rests on a cell before it paints a rectangle (ms); a shorter touch is a
/// tap, a touch that moves first is a scroll.
pub const HOLD_MS: u64 = 350;

/// How far (px) a held finger may move before the hold counts as a scroll.
const SLOP_PX: i32 = 8;

/// "00:00" … "24:00", the hour bounds of the cell labels.
const HOUR_TEXT: [&str; 25] = [
    "00:00", "01:00", "02:00", "03:00", "04:00", "05:00", "06:00", "07:00", "08:00", "09:00", "10:00", "11:00", "12:00", "13:00", "14:00", "15:00",
    "16:00", "17:00", "18:00", "19:00", "20:00", "21:00", "22:00", "23:00", "24:00",
];

/// A rectangle being painted: the pointer that paints, the anchor and last display cells, the
/// value before the drag (restored by Esc or a cancelled pointer).
struct Drag {
    id: i32,
    anchor: usize,
    last: usize,
    from: String,
}

/// A finger resting on a cell, before it is a tap, a hold or a scroll.
struct Press {
    id: i32,
    cell: usize,
    x: i32,
    y: i32,
    timer: Option<TimeoutHandle>,
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
            // with only two levels every run is the other one: its name adds nothing
            let label = |c| if levels.len() == 2 { String::new() } else { format!(" {}", level_label(levels, c)) };
            let runs = g.runs.iter().map(|&(a, b, c)| format!("{a:02}–{b:02}{}", label(c))).collect::<Vec<_>>().join(", ");
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
/// grid for assistive technology. Painting writes `value` at once (a drag updates it live);
/// `on_commit` gets the value when a change is done (a click, tap, key or heading, the end of a
/// drag; not a drag cancelled with Esc), so a form can take it once instead of at every cell.
pub fn week_grid(levels: &'static [GridLevel], value: RwSignal<String>, label_key: &'static str, on_commit: Option<Callback<String>>) -> AnyView {
    debug_assert!((2..=4).contains(&levels.len()) && levels.iter().all(|l| l.code.is_ascii()));
    let base = levels[0].code;
    let active = RwSignal::new(0usize);
    // the cell with tabindex 0 (roving focus), display index
    let focus = RwSignal::new(0usize);
    let drag = StoredValue::new(None::<Drag>);
    let press = StoredValue::new(None::<Press>);
    // a rectangle is being painted: the summary waits, a touch no longer scrolls
    let dragging = RwSignal::new(false);
    let wrap = NodeRef::<leptos::html::Div>::new();
    let grid = NodeRef::<leptos::html::Div>::new();
    let help_id = crate::form::fid();

    let current = move || value.with_untracked(|v| normalize(v, &codes_of(levels), base));
    let invalid = Memo::new(move |_| value.with(|v| validate(v, &codes_of(levels)).is_err()));
    let paint = move |from: &str, a: usize, b: usize| value.set(set_rect(from, a, b, levels[active.get_untracked()].code));
    let commit = move || {
        if let Some(cb) = on_commit {
            cb.run(value.get_untracked());
        }
    };
    let vertical = move || wrap.get_untracked().is_some_and(|w| w.client_width() < NARROW_PX);
    let cell_el = move |di: usize| grid.get_untracked().and_then(|g| g.query_selector(&format!("[data-i=\"{di}\"]")).ok().flatten());
    let focus_cell = move |di: usize| {
        focus.set(di);
        if let Some(el) = cell_el(di) {
            let _ = el.unchecked_into::<web_sys::HtmlElement>().focus();
        }
    };
    let level_name = move || t(levels[active.get()].label_key);

    let start = move |id: i32, di: usize| {
        let from = current();
        paint(&from, di, di);
        drag.set_value(Some(Drag { id, anchor: di, last: di, from }));
        dragging.set(true);
        focus_cell(di);
    };
    // the keyboard goes on from where the drag ended
    let end = move |id: i32| {
        if drag.with_value(|d| d.as_ref().is_some_and(|d| d.id == id))
            && let Some(d) = drag.try_update_value(Option::take).flatten()
        {
            dragging.set(false);
            focus_cell(d.last);
            commit();
        }
    };
    // back to the value before the drag (Esc, a cancelled pointer)
    let cancel_drag = move || {
        if let Some(d) = drag.try_update_value(Option::take).flatten() {
            value.set(d.from);
            dragging.set(false);
        }
    };
    let clear_press = move || {
        if let Some(p) = press.try_update_value(Option::take).flatten() {
            if let Some(h) = p.timer {
                h.clear();
            }
            if let Some(el) = cell_el(p.cell) {
                let _ = el.remove_attribute("data-press");
            }
        }
    };

    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        let key = ev.key();
        if key == "Escape" && drag.with_value(Option::is_some) {
            ev.prevent_default();
            cancel_drag();
            return;
        }
        if key.len() == 1
            && !(ev.ctrl_key() || ev.alt_key() || ev.meta_key())
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
                match (key.as_str(), ev.shift_key()) {
                    // Shift+Space: the whole day; Shift+Enter: this hour on every day
                    (" ", true) => paint(&from, r * 24, r * 24 + 23),
                    (_, true) => paint(&from, h, 6 * 24 + h),
                    _ => paint(&from, di, di),
                }
                commit();
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
        // one pointer paints: a second finger or button is ignored
        if ev.button() != 0 || !ev.is_primary() || drag.with_value(Option::is_some) || press.with_value(Option::is_some) {
            return;
        }
        let Some(di) = cell_at(&g, ev.client_x(), ev.client_y()) else { return };
        let id = ev.pointer_id();
        if ev.pointer_type() == "touch" {
            // the page keeps scrolling under a finger: a tap paints one cell, a hold starts a
            // rectangle (and only then the grid keeps the finger)
            if let Some(el) = cell_el(di) {
                let _ = el.set_attribute("data-press", "");
            }
            let timer = set_timeout_with_handle(
                move || {
                    if press.with_value(|p| p.as_ref().is_some_and(|p| p.id == id))
                        && let Some(p) = press.try_update_value(Option::take).flatten()
                    {
                        if let Some(el) = cell_el(p.cell) {
                            let _ = el.remove_attribute("data-press");
                        }
                        if let Some(g) = grid.get_untracked() {
                            let _ = g.set_pointer_capture(id);
                        }
                        start(id, p.cell);
                    }
                },
                std::time::Duration::from_millis(HOLD_MS),
            )
            .ok();
            press.set_value(Some(Press { id, cell: di, x: ev.client_x(), y: ev.client_y(), timer }));
            return;
        }
        ev.prevent_default();
        let _ = g.set_pointer_capture(id);
        start(id, di);
    };
    let on_move = move |ev: web_sys::PointerEvent| {
        let id = ev.pointer_id();
        let (x, y) = (ev.client_x(), ev.client_y());
        // a held finger that moves is scrolling the page
        if press.with_value(|p| p.as_ref().is_some_and(|p| p.id == id && ((p.x - x).abs() > SLOP_PX || (p.y - y).abs() > SLOP_PX))) {
            clear_press();
            return;
        }
        if !drag.with_value(|d| d.as_ref().is_some_and(|d| d.id == id)) {
            return;
        }
        let Some(g) = grid.get_untracked() else { return };
        let Some(di) = cell_at(&g, x, y) else { return };
        let code = levels[active.get_untracked()].code;
        let mut next = None;
        drag.update_value(|d| {
            if let Some(d) = d
                && d.last != di
            {
                d.last = di;
                next = Some(set_rect(&d.from, d.anchor, di, code));
            }
        });
        if let Some(v) = next {
            value.set(v);
        }
    };
    let on_up = move |ev: web_sys::PointerEvent| {
        let id = ev.pointer_id();
        if let Some(cell) = press.with_value(|p| p.as_ref().filter(|p| p.id == id).map(|p| p.cell)) {
            // a tap: that one cell
            clear_press();
            let from = current();
            paint(&from, cell, cell);
            focus_cell(cell);
            commit();
            return;
        }
        end(id);
    };
    let on_cancel = move |ev: web_sys::PointerEvent| {
        let id = ev.pointer_id();
        if press.with_value(|p| p.as_ref().is_some_and(|p| p.id == id)) {
            clear_press();
        }
        if drag.with_value(|d| d.as_ref().is_some_and(|d| d.id == id)) {
            cancel_drag();
        }
    };

    let header = view! {
        <div role="row">
            <div class="wg-corner" role="columnheader"><span class="sr-only">{tr("wg.hours")}</span></div>
            {(0..24)
                .map(|h| {
                    let label = move || tf("wg.fill_hour", &[("from", HOUR_TEXT[h]), ("to", HOUR_TEXT[h + 1]), ("level", level_name())]);
                    let fill = move |_| {
                        paint(&current(), h, 6 * 24 + h);
                        commit();
                    };
                    view! {
                        <div class="wg-hour" role="columnheader" style=format!("--h:{h}")>
                            <button type="button" tabindex="-1" aria-label=label title=label on:click=fill>{&HOUR_TEXT[h][..2]}</button>
                        </div>
                    }
                })
                .collect_view()}
        </div>
    };
    let rows = (0..7)
        .map(|r| {
            let day_label = move || tf("wg.fill_day", &[("day", t(DAY_LONG[r])), ("level", level_name())]);
            let fill = move |_| {
                paint(&current(), r * 24, r * 24 + 23);
                commit();
            };
            view! {
                <div role="row">
                    <div class="wg-day" role="rowheader" style=format!("--r:{r}")>
                        <button type="button" tabindex="-1" aria-label=day_label title=day_label on:click=fill>{tr(DAY_KEYS[r])}</button>
                    </div>
                    {(0..24)
                        .map(|h| {
                            let di = r * 24 + h;
                            let si = index(r, h);
                            // the level of this cell: class and label re-run only when it changes
                            let li = Memo::new(move |_| {
                                value.with(|v| {
                                    let c = v.as_bytes().get(si).copied();
                                    levels.iter().position(|l| Some(l.code as u8) == c).unwrap_or(0) as u8
                                })
                            });
                            let label = move || {
                                let l = &levels[li.get() as usize];
                                tf("wg.cell", &[("day", t(DAY_LONG[r])), ("from", HOUR_TEXT[h]), ("to", HOUR_TEXT[h + 1]), ("level", t(l.label_key))])
                            };
                            view! {
                                <div
                                    role="gridcell"
                                    class=move || levels[li.get() as usize].tone.cell_class()
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
    // read out when a change is done, not at every cell of a drag
    let summary = Memo::new(move |prev: Option<&String>| {
        if dragging.get()
            && let Some(p) = prev
        {
            return p.clone();
        }
        value.with(|v| summary_text(v, levels))
    });

    view! {
        <div class="wg" node_ref=wrap>
            {level_picker(levels, active)}
            <div
                class="wg-grid"
                class:wg-painting=move || dragging.get()
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
                on:pointerup=on_up
                on:lostpointercapture=move |ev: web_sys::PointerEvent| end(ev.pointer_id())
                on:pointercancel=on_cancel
                // the build has no event delegation: this listener sits on the grid itself, so it
                // is not passive and can keep the page still while painting
                on:touchmove=move |ev| {
                    if dragging.get_untracked() && ev.cancelable() {
                        ev.prevent_default();
                    }
                }
                on:contextmenu=move |ev| {
                    if dragging.get_untracked() || press.with_value(Option::is_some) {
                        ev.prevent_default();
                    }
                }
            >
                {header}
                {rows}
            </div>
            <Show when=move || invalid.get()>
                <p class="wg-help">{move || tf("wg.invalid", &[("level", t(levels[0].label_key))])}</p>
            </Show>
            <p class="wg-summary" aria-live="polite">{move || summary.get()}</p>
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
                {week_grid(levels, v, title, None)}
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
