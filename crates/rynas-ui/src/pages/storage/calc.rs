//! Capacity calculator of the create-volume wizard: drive sizes typed by hand (drives not
//! bought yet too) → every RAID level that takes that many drives, with its usable space,
//! fault tolerance and unused space. The numbers come from [`rynas_api_types::capacity`], the
//! arithmetic the planner builds volumes with (partitions, md metadata, LVM, the flexible
//! RAID tiers), unencrypted.

use leptos::prelude::*;
use rynas_api_types::capacity::{self, Capacity, MIN_TIER};
use rynas_api_types::storage::RaidLevel;

use super::level_key;
use super::plan::tolerance_text;
use super::wizard::{classic_twin, ordered, recommended, same_as_twin, sizes_mixed};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Modal};

/// Quick sizes (TB) that add a drive.
const PRESETS: [u64; 7] = [1, 2, 4, 8, 12, 16, 20];
/// Most drives the calculator takes.
const MAX_DRIVES: usize = 24;
const TB: f64 = 1e12;
const GB: f64 = 1e9;
/// The largest size a row takes: 10 PB.
const MAX_BYTES: f64 = 1e16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Tb,
    Gb,
}

impl Unit {
    fn factor(self) -> f64 {
        match self {
            Unit::Tb => TB,
            Unit::Gb => GB,
        }
    }
}

#[derive(Clone, PartialEq)]
struct Drive {
    id: u32,
    text: String,
    unit: Unit,
    /// The exact size of a real drive, until the text is edited.
    exact: Option<u64>,
}

/// The size a row stands for: a number between 1 GB and 10 PB in the row's unit.
fn size_of(d: &Drive) -> Option<u64> {
    if let Some(b) = d.exact {
        return Some(b);
    }
    let mark = if i18n::decimal_mark() == "," { ',' } else { '.' };
    let v = parse_number(&d.text, mark)?;
    let b = v * d.unit.factor();
    (b.is_finite() && (GB..=MAX_BYTES).contains(&b)).then(|| b.round() as u64)
}

/// A typed number, read without guessing: the locale's decimal mark (`decimal`) or the other
/// one as decimal separator, except that a separator followed by exactly three digits, in
/// groups after one to three digits, is a thousands separator when it is not the locale's
/// decimal mark ("1.000" is 1000 in Italian, "1,000" is 1000 in English; "1.5" and "1,5" are
/// 1.5 everywhere). Both marks in one number: the last one is the decimal separator. Spaces
/// (also the narrow ones some locales group with) are ignored; anything else is not a number.
fn parse_number(text: &str, decimal: char) -> Option<f64> {
    let s: String = text.chars().filter(|c| !c.is_whitespace() && !matches!(c, '\u{a0}' | '\u{202f}' | '\'')).collect();
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit() || c == '.' || c == ',') {
        return None;
    }
    // digits in groups of three after the first: "1.234.567" split at `sep`
    let grouped = |s: &str, sep: char| {
        let mut parts = s.split(sep);
        let first = parts.next().unwrap_or("");
        (1..=3).contains(&first.len()) && parts.clone().count() > 0 && parts.all(|p| p.len() == 3)
    };
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let (int, frac): (String, Option<String>) = match (s.rfind('.'), s.rfind(',')) {
        (Some(a), Some(b)) => {
            let (dec_at, group) = if a > b { (a, ',') } else { (b, '.') };
            let (head, tail) = (&s[..dec_at], &s[dec_at + 1..]);
            if !grouped(head, group) || !digits(tail) {
                return None;
            }
            (head.replace(group, ""), Some(tail.to_string()))
        }
        (Some(_), None) | (None, Some(_)) => {
            let sep = if s.contains('.') { '.' } else { ',' };
            if sep != decimal && grouped(&s, sep) {
                (s.replace(sep, ""), None)
            } else if s.matches(sep).count() == 1 {
                let (head, tail) = s.split_once(sep)?;
                if !digits(head) || !digits(tail) {
                    return None;
                }
                (head.to_string(), Some(tail.to_string()))
            } else {
                return None;
            }
        }
        (None, None) => (s.clone(), None),
    };
    if !digits(&int) {
        return None;
    }
    match frac {
        Some(f) => format!("{int}.{f}").parse().ok(),
        None => int.parse().ok(),
    }
}

/// A size as typed: whole numbers bare, otherwise up to two decimals in the user's locale.
fn size_text(bytes: u64, unit: Unit) -> String {
    let v = bytes as f64 / unit.factor();
    let r = (v * 100.0).round() / 100.0;
    if r.fract() == 0.0 { format!("{r:.0}") } else { i18n::num(r, 2).trim_end_matches('0').to_string() }
}

fn level_name(l: RaidLevel) -> (&'static str, Option<&'static str>) {
    match l {
        RaidLevel::Flex1 => ("sto.flex", Some("sto.flex_tol1")),
        RaidLevel::Flex2 => ("sto.flex", Some("sto.flex_tol2")),
        _ => (level_key(l), None),
    }
}

#[component]
pub fn CapacityCalculator(start: Vec<u64>, on_close: Callback<()>) -> impl IntoView {
    let next_id = StoredValue::new(0u32);
    let new_id = move || {
        let id = next_id.get_value();
        next_id.set_value(id + 1);
        id
    };
    let drives = RwSignal::new(
        start
            .into_iter()
            .take(MAX_DRIVES)
            .map(|b| {
                let unit = if (b as f64) < TB { Unit::Gb } else { Unit::Tb };
                Drive { id: new_id(), text: size_text(b, unit), unit, exact: Some(b) }
            })
            .collect::<Vec<_>>(),
    );
    let add = move |tb: u64| {
        drives.update(|v| {
            if v.len() < MAX_DRIVES {
                v.push(Drive { id: new_id(), text: tb.to_string(), unit: Unit::Tb, exact: None });
            }
        })
    };
    let remove = move |id: u32| {
        let at = drives.with_untracked(|v| v.iter().position(|x| x.id == id));
        drives.update(|v| v.retain(|x| x.id != id));
        // focus goes to the row now in its place (its remove button), else to the add buttons
        if let Some(i) = at {
            request_animation_frame(move || focus_after_remove(i));
        }
    };
    let sizes = Memo::new(move |_| drives.with(|v| v.iter().filter_map(size_of).collect::<Vec<u64>>()));
    // a row without a valid size (empty too) is left out, and says so
    let bad = Memo::new(move |_| drives.with(|v| v.iter().any(|d| size_of(d).is_none())));
    let full = move || drives.with(Vec::len) >= MAX_DRIVES;
    let show_all = RwSignal::new(false);

    let row = move |d: Drive| {
        let id = d.id;
        let n = move || drives.with(|v| v.iter().position(|x| x.id == id).map_or(0, |i| i + 1)).to_string();
        let get = move || drives.with(|v| v.iter().find(|x| x.id == id).cloned());
        let edit = move |f: &dyn Fn(&mut Drive)| {
            drives.update(|v| {
                if let Some(x) = v.iter_mut().find(|x| x.id == id) {
                    f(x);
                }
            })
        };
        let invalid = move || get().is_some_and(|d| size_of(&d).is_none());
        view! {
            <li class="calc-drive">
                <span class="calc-drive-n muted small">{move || tf("wiz.calc.drive", &[("n", &n())])}</span>
                <input class="input tnum calc-size" inputmode="decimal" autocomplete="off" spellcheck="false"
                    aria-label=move || tf("wiz.calc.size", &[("n", &n())])
                    aria-invalid=move || invalid().to_string()
                    aria-describedby=move || invalid().then_some("calc-bad")
                    placeholder=tr("wiz.calc.placeholder")
                    prop:value=move || get().map(|d| d.text).unwrap_or_default()
                    on:input=move |ev| {
                        let text = event_target_value(&ev);
                        edit(&|x: &mut Drive| { x.text = text.clone(); x.exact = None; });
                    } />
                <select class="input calc-unit" aria-label=move || tf("wiz.calc.unit", &[("n", &n())])
                    prop:value=move || if get().is_some_and(|d| d.unit == Unit::Gb) { "gb" } else { "tb" }
                    on:change=move |ev| {
                        let unit = if event_target_value(&ev) == "gb" { Unit::Gb } else { Unit::Tb };
                        // a real drive's size stays the same size in the other unit
                        edit(&|x: &mut Drive| {
                            if let Some(b) = x.exact {
                                x.text = size_text(b, unit);
                            }
                            x.unit = unit;
                        });
                    }>
                    <option value="tb">{tr("fmt.unit.tb")}</option>
                    <option value="gb">{tr("fmt.unit.gb")}</option>
                </select>
                {move || ui::icon_btn(icons::TRASH_2, tf("wiz.calc.remove", &[("n", &n())]), true, Signal::stored(false),
                    Callback::new(move |_| remove(id)))}
            </li>
        }
    };

    // the levels these drives take, in the wizard's order (the recommended one first)
    let levels = Memo::new(move |_| {
        let sizes = sizes.get();
        let n = sizes.len();
        let mixed = sizes_mixed(&sizes);
        let rows: Vec<(RaidLevel, Capacity)> = ordered(n, mixed)
            .into_iter()
            .filter_map(|l| capacity::volume(l, &sizes, false, MIN_TIER).map(|c| (l, c)))
            .collect();
        (recommended(n, mixed), rows)
    });
    // the line screen readers hear after each change: the drives and the best level
    let summary = move || {
        let sizes = sizes.get();
        if sizes.is_empty() {
            return None;
        }
        let total = i18n::tn("wiz.calc.total", sizes.len() as u64, &[("size", &i18n::bytes_si(sizes.iter().sum()))]);
        let best = levels.with(|(rec, rows)| {
            rows.iter().find(|(l, _)| l == rec).map(|(l, c)| {
                let name = if l.is_flexible() { t("sto.flex") } else { t(level_key(*l)) };
                tf("wiz.calc.best", &[("level", name), ("tolerance", &tolerance_text(c.tolerance)), ("size", &i18n::bytes_si(c.usable))])
            })
        });
        Some((total, best))
    };

    let results = move || {
        let (rec, rows) = levels.get();
        if rows.is_empty() {
            let key = if sizes.with(Vec::is_empty) { "wiz.calc.empty" } else { "wiz.calc.none" };
            return view! { <p class="muted small">{tr(key)}</p> }.into_any();
        }
        let n = sizes.with(Vec::len);
        let mixed = sizes.with(|s| sizes_mixed(s));
        let max = sizes.with(|s| s.iter().copied().max().unwrap_or(1).max(1)) as f64;
        let usable_of = |l: RaidLevel| rows.iter().find(|(x, _)| *x == l).map(|(_, c)| c.usable);
        let hidden = rows.iter().filter(|(l, _)| uncommon(*l, n)).count();
        let shown: Vec<(RaidLevel, Capacity)> =
            rows.iter().filter(|(l, _)| show_all.get() || !uncommon(*l, n)).cloned().collect();
        let notes = |l: RaidLevel, c: &Capacity| -> Option<String> {
            if let Some(tw) = classic_twin(l, n)
                && usable_of(tw).is_some_and(|u| same_as_twin(c.usable, u))
            {
                return Some(tf("wiz.same_as_twin", &[("level", t(level_key(tw)))]));
            }
            (l == RaidLevel::Raid0 && mixed && c.wasted > 0).then(|| t("wiz.calc.raid0_note").to_string())
        };
        view! {
            <ul class="calc-list" aria-label=tr("wiz.calc.levels")>
                {shown.into_iter().map(|(l, c)| {
                    let (name, variant) = level_name(l);
                    let wasted = if c.wasted > 0 { i18n::bytes_si(c.wasted) } else { ui::DASH.to_string() };
                    let note = notes(l, &c);
                    view! {
                        <li class="calc-row" class:rec=l == rec>
                            <div class="calc-name">
                                <strong>{tr(name)}</strong>
                                {variant.map(|k| view! { <span class="small">{tr(k)}</span> })}
                                {(l == rec).then(|| view! { <span class="chip accent">{tr("wiz.recommended")}</span> })}
                            </div>
                            <div class="calc-val"><span class="calc-k">{tr("plan.usable")}</span><strong class="tnum">{i18n::bytes_si(c.usable)}</strong></div>
                            <div class="calc-val"><span class="calc-k">{tr("plan.tolerance")}</span><span>{tolerance_text(c.tolerance)}</span></div>
                            <div class="calc-val"><span class="calc-k">{tr("plan.wasted")}</span><span class="tnum">{wasted}</span></div>
                            <div class="calc-disks" role="img" aria-label=disk_label(&c)>
                                {c.disks.iter().map(|d| view! {
                                    <span class="calc-bar" style:width=format!("{:.2}%", d.size as f64 * 100.0 / max)>
                                        <span class="calc-seg used" style:width=format!("{:.2}%", d.used as f64 * 100.0 / (d.used + d.unused).max(1) as f64)></span>
                                        {(d.unused > 0).then(|| view! { <span class="calc-seg waste" style:flex="1"></span> })}
                                    </span>
                                }).collect_view()}
                            </div>
                            {note.map(|x| view! { <p class="calc-note muted small">{x}</p> })}
                        </li>
                    }
                }).collect_view()}
            </ul>
            {(hidden > 0).then(|| view! {
                <button type="button" class="btn ghost sm calc-more" aria-expanded=move || show_all.get().to_string()
                    on:click=move |_| show_all.update(|v| *v = !*v)>
                    {move || if show_all.get() { t("wiz.calc.fewer").to_string() } else { i18n::tn("wiz.calc.more", hidden as u64, &[]) }}
                </button>
            })}
            <div class="cap-legend">
                <span class="cap-leg"><span class="dot calc-used"></span>{tr("wiz.calc.used")}</span>
                <span class="cap-leg"><span class="dot waste"></span>{tr("sto.unused_space")}</span>
            </div>
        }
        .into_any()
    };

    view! {
        <Modal title=Signal::derive(|| t("wiz.calc").to_string()) on_close=on_close wide=true foot=move || ui::close_foot(on_close)>
            <p class="muted small">{tr("wiz.calc.text")}</p>
            <div class="calc-input">
                <h3 class="mini-title">{tr("wiz.s_disks")}</h3>
                <ul class="calc-drives">
                    <For each=move || drives.get() key=|d| d.id children=row />
                </ul>
                <div class="calc-presets">
                    <span class="muted small">{tr("wiz.calc.add")}</span>
                    <div class="chips">
                        {PRESETS.into_iter().map(|tb| view! {
                            <button type="button" class="btn ghost sm" disabled=full on:click=move |_| add(tb)>
                                <Icon icon=icons::PLUS size=12 />{format!("{tb} {}", t("fmt.unit.tb"))}
                            </button>
                        }).collect_view()}
                    </div>
                </div>
                <p id="calc-bad" class="small warn-text" hidden=move || !bad.get()>{tr("wiz.calc.bad")}</p>
            </div>
            <div class="calc-out">
                <div class="calc-sum" aria-live="polite" aria-atomic="true">
                    {move || summary().map(|(total, best)| view! {
                        <p class="wiz-sum">{total}</p>
                        {best.map(|b| view! { <p class="calc-best"><Icon icon=icons::CIRCLE_CHECK size=16 /><span>{b}</span></p> })}
                    })}
                </div>
                {results}
            </div>
        </Modal>
    }
}

/// Levels that rarely make sense for this many drives, behind "show all": a mirror of more
/// than four drives (every drive a full copy).
fn uncommon(l: RaidLevel, n: usize) -> bool {
    l == RaidLevel::Raid1 && n > 4
}

/// After the drive at `index` was removed: focus the remove button of the drive now at that
/// place (or the last one), else the first enabled add button.
fn focus_after_remove(index: usize) {
    use wasm_bindgen::JsCast;
    let doc = document();
    let rows = doc.query_selector_all(".calc-drives > li").ok();
    let len = rows.as_ref().map_or(0, |r| r.length() as usize);
    let target = if len > 0 {
        let i = index.min(len - 1);
        doc.query_selector(&format!(".calc-drives > li:nth-child({}) button", i + 1)).ok().flatten()
    } else {
        doc.query_selector(".calc-presets button:not([disabled])").ok().flatten()
    };
    if let Some(el) = target.and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()) {
        let _ = el.focus();
    }
}

/// What the drive bars of a level show, for screen readers.
fn disk_label(c: &Capacity) -> String {
    c.disks
        .iter()
        .enumerate()
        .map(|(i, d)| {
            tf("wiz.calc.disk_use", &[
                ("n", &(i + 1).to_string()),
                ("used", &i18n::bytes_si(d.used)),
                ("unused", &i18n::bytes_si(d.unused)),
            ])
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::parse_number;

    #[test]
    fn numbers_are_read_without_guessing() {
        // Italian: comma decimal, a dot before exactly three digits groups thousands
        assert_eq!(parse_number("1.000", ','), Some(1000.0));
        assert_eq!(parse_number("12.000.000", ','), Some(12_000_000.0));
        assert_eq!(parse_number("1,5", ','), Some(1.5));
        assert_eq!(parse_number("1.5", ','), Some(1.5));
        assert_eq!(parse_number("1.000,5", ','), Some(1000.5));
        assert_eq!(parse_number("1 000", ','), Some(1000.0));
        assert_eq!(parse_number("1\u{202f}000,25", ','), Some(1000.25));
        // English: dot decimal, a comma before exactly three digits groups thousands
        assert_eq!(parse_number("1,000", '.'), Some(1000.0));
        assert_eq!(parse_number("1.000", '.'), Some(1.0));
        assert_eq!(parse_number("1,5", '.'), Some(1.5));
        assert_eq!(parse_number("1,000.5", '.'), Some(1000.5));
        assert_eq!(parse_number("16", '.'), Some(16.0));
        // not numbers
        for bad in ["", " ", "abc", "1..0", "1,2,3", "1.000,5.5", "1,00.5", "-4", "1e3", ".", "4,", ",5"] {
            assert_eq!(parse_number(bad, ','), None, "{bad:?} (it)");
            assert_eq!(parse_number(bad, '.'), None, "{bad:?} (en)");
        }
    }
}
