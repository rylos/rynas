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
use super::wizard::{LEVELS, recommended, sizes_mixed};
use crate::i18n::{self, t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Modal};

/// Quick sizes (TB) that add a drive.
const PRESETS: [u64; 7] = [1, 2, 4, 8, 12, 16, 20];
/// Most drives the calculator takes.
const MAX_DRIVES: usize = 24;
const TB: f64 = 1e12;
const GB: f64 = 1e9;

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

/// The size a row stands for: a number (`.` or `,` as decimal separator) of at least 1 GB.
fn size_of(d: &Drive) -> Option<u64> {
    if let Some(b) = d.exact {
        return Some(b);
    }
    let v: f64 = d.text.trim().replace(',', ".").parse().ok()?;
    let b = v * d.unit.factor();
    (b.is_finite() && (GB..=1e16).contains(&b)).then(|| b.round() as u64)
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

/// The calculator dialog; `start` are the sizes it opens with (the drives chosen in the
/// wizard, if any).
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
    let sizes = Memo::new(move |_| drives.with(|v| v.iter().filter_map(size_of).collect::<Vec<u64>>()));
    let bad = Memo::new(move |_| drives.with(|v| v.iter().any(|d| !d.text.trim().is_empty() && size_of(d).is_none())));
    let full = move || drives.with(Vec::len) >= MAX_DRIVES;

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
        let invalid = move || get().is_some_and(|d| !d.text.trim().is_empty() && size_of(&d).is_none());
        view! {
            <li class="calc-drive">
                <span class="calc-drive-n muted small">{move || tf("wiz.calc.drive", &[("n", &n())])}</span>
                <input class="input tnum calc-size" inputmode="decimal" autocomplete="off" spellcheck="false"
                    aria-label=move || tf("wiz.calc.size", &[("n", &n())])
                    aria-invalid=move || invalid().to_string()
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
                    Callback::new(move |_| drives.update(|v| v.retain(|x| x.id != id))))}
            </li>
        }
    };

    let results = move || {
        let sizes = sizes.get();
        if sizes.is_empty() {
            return view! { <p class="muted small">{tr("wiz.calc.empty")}</p> }.into_any();
        }
        let rec = recommended(sizes.len(), sizes_mixed(&sizes));
        let max = sizes.iter().copied().max().unwrap_or(1).max(1) as f64;
        let rows: Vec<(RaidLevel, Capacity)> = LEVELS
            .into_iter()
            .filter_map(|l| capacity::volume(l, &sizes, false, MIN_TIER).map(|c| (l, c)))
            .collect();
        view! {
            <p class="wiz-sum">{i18n::tn("wiz.calc.total", sizes.len() as u64, &[("size", &i18n::bytes_si(sizes.iter().sum()))])}</p>
            <ul class="calc-list" aria-label=tr("wiz.calc.levels")>
                {rows.into_iter().map(|(l, c)| {
                    let (name, variant) = level_name(l);
                    let wasted = if c.wasted > 0 { i18n::bytes_si(c.wasted) } else { ui::DASH.to_string() };
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
                        </li>
                    }
                }).collect_view()}
            </ul>
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
                {move || bad.get().then(|| view! { <p class="small warn-text">{tr("wiz.calc.bad")}</p> })}
            </div>
            <div class="calc-out" aria-live="polite">{results}</div>
        </Modal>
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
