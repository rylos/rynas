//! Dashboard: 12-column widget grid fed by the live store, customizable ("Customize" mode):
//! drag & drop or arrow keys to reorder, S/M/L sizes, hide/show, reset. The layout lives in
//! localStorage (see `layout.rs`). Nothing of the editing machinery is mounted outside
//! Customize mode: the normal dashboard costs exactly what a static grid costs.

use leptos::html::Div;
use leptos::prelude::*;
use rynas_api_types::{MetricsSample, ZramDevice};
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, PointerEvent};

use crate::alerts::{self, array_status, fan_label, fs_pct, storage_health};
use crate::chart::{LiveChart, Series, span_minutes};
use crate::i18n::{self, t, tf, tr, try_t};
use crate::icons::{self, Icon};
use crate::layout::{self, Item, Size, W};
use crate::live::{Live, Res, use_live};
use crate::prefs::{storage_get, storage_set, use_ui};
use crate::ui::{EmptyState, Meter, Pending, Skeleton, Status, Tone, WidgetHead, reduced_motion, temp_tone, usage_tone};

const LAYOUT_KEY: &str = "rynas.dashboard";

/// Everything the cells need to edit the layout.
#[derive(Clone, Copy)]
struct Board {
    layout: RwSignal<Vec<Item>>,
    /// Widgets on screen, in order (hidden ones and Hardware without a provider left out).
    visible: Memo<Vec<W>>,
    grid: NodeRef<Div>,
    /// Text for the aria-live region (keyboard moves).
    announce: RwSignal<String>,
    dragging: RwSignal<Option<W>>,
}

#[component]
pub fn Dashboard() -> impl IntoView {
    let ui = use_ui();
    let live = use_live();
    let layout = RwSignal::new(layout::decode(&storage_get(LAYOUT_KEY).unwrap_or_default()));
    let has_hw = Memo::new(move |_| live.hardware.with(|h| h.as_ref().is_some_and(|h| h.provider.is_some())));
    let visible = Memo::new(move |_| {
        layout.with(|l| l.iter().filter(|i| !i.hidden && (i.w != W::Hardware || has_hw.get())).map(|i| i.w).collect::<Vec<_>>())
    });
    let board = Board { layout, visible, grid: NodeRef::new(), announce: RwSignal::new(String::new()), dragging: RwSignal::new(None) };
    let edit = ui.dash_edit;

    // persist (the default layout is not stored, so future defaults reach untouched installs)
    Effect::new(move |prev: Option<()>| {
        let l = layout.get();
        if prev.is_some() {
            let enc = layout::encode(&l);
            storage_set(LAYOUT_KEY, if enc == layout::encode(&layout::default_layout()) { "" } else { &enc });
        }
    });
    // the entrance animation plays once: moving nodes later must not replay it
    let ready = RwSignal::new(false);
    set_timeout(move || { ready.try_set(true); }, std::time::Duration::from_millis(700));
    on_cleanup(move || edit.set(false));

    let hidden = Memo::new(move |_| layout.with(|l| l.iter().filter(|i| i.hidden && (i.w != W::Hardware || has_hw.get())).map(|i| i.w).collect::<Vec<_>>()));

    view! {
        <div class="page">
            <header class="page-head">
                <div class="page-icon"><Icon icon=icons::LAYOUT_DASHBOARD size=20 /></div>
                <div>
                    <h1>{tr("dash.title")}</h1>
                    <p class="page-sub">{tr("dash.subtitle")}</p>
                </div>
                <div class="page-actions">
                    <Show
                        when=move || edit.get()
                        fallback=move || view! {
                            <button class="btn ghost" on:click=move |_| edit.set(true)>
                                <Icon icon=icons::SETTINGS_2 size=16 />
                                <span class="btn-label">{tr("dash.edit")}</span>
                            </button>
                        }
                    >
                        <button class="btn ghost" on:click=move |_| {
                            flip(board, None, move || layout.set(layout::default_layout()));
                            ui.relayout_soon();
                        }>
                            <Icon icon=icons::ROTATE_CCW size=16 />
                            <span class="btn-label">{tr("dash.reset")}</span>
                        </button>
                        <button class="btn primary" on:click=move |_| edit.set(false)>
                            <Icon icon=icons::CHECK size=16 />{tr("dash.done")}
                        </button>
                    </Show>
                </div>
            </header>
            <Show when=move || edit.get()>
                <div class="edit-bar" role="region" aria-label=tr("dash.editing")>
                    <p id="dash-hint" class="edit-hint">
                        <Icon icon=icons::GRIP_VERTICAL size=16 />{tr("dash.edit_hint")}
                    </p>
                    <Show when=move || !hidden.with(Vec::is_empty)>
                        <div class="tray">
                            <span class="tray-label">{tr("dash.hidden")}</span>
                            <For each=move || hidden.get() key=|w| *w let:w>
                                <button class="tray-item" on:click=move |_| {
                                    flip(board, None, move || layout.update(|l| if let Some(i) = l.iter_mut().find(|i| i.w == w) { i.hidden = false }));
                                }>
                                    <Icon icon=icons::PLUS size=14 />{move || t(w.title())}
                                </button>
                            </For>
                        </div>
                    </Show>
                </div>
            </Show>
            <div class="sr-only" aria-live="polite">{move || board.announce.get()}</div>
            <div class="grid dash" class:editing=move || edit.get() class:ready=move || ready.get() node_ref=board.grid>
                <For each=move || visible.get() key=|w| *w let:w>
                    <Cell w board />
                </For>
            </div>
            <Show when=move || visible.with(Vec::is_empty) && !edit.get()>
                <div class="card soon">
                    <EmptyState
                        icon=icons::LAYOUT_DASHBOARD
                        title=Signal::derive(|| t("dash.all_hidden").to_string())
                        text=Signal::derive(|| t("dash.all_hidden_text").to_string())
                    >
                        <button class="btn primary" on:click=move |_| edit.set(true)>{tr("dash.edit")}</button>
                    </EmptyState>
                </div>
            </Show>
        </div>
    }
}

fn widget(w: W) -> AnyView {
    match w {
        W::System => view! { <SystemWidget /> }.into_any(),
        W::Cpu => view! { <CpuWidget /> }.into_any(),
        W::Memory => view! { <MemoryWidget /> }.into_any(),
        W::Network => view! { <NetworkWidget /> }.into_any(),
        W::Disk => view! { <DiskWidget /> }.into_any(),
        W::Health => view! { <HealthWidget /> }.into_any(),
        W::Volumes => view! { <VolumesWidget /> }.into_any(),
        W::Hardware => view! { <HardwareWidget /> }.into_any(),
    }
}

const HEIGHTS_KEY: &str = "rynas.dash.h";
/// Heights remembered (one per widget, size and window width).
const HEIGHTS_MAX: usize = 48;

/// Whether the data a widget shows has arrived (its placeholder is gone).
fn loaded(live: Live, w: W) -> bool {
    match w {
        W::System => live.system.with(Option::is_some) || live.system_err.with(Option::is_some),
        W::Cpu | W::Memory | W::Network | W::Disk => live.sample.with(Option::is_some),
        W::Health | W::Volumes => live.storage.with(Option::is_some) || live.storage_err.with(Option::is_some),
        W::Hardware => true,
    }
}

/// Typical height of a widget once loaded (a DS920+ at 1280 px), for the very first visit.
fn default_height(w: W) -> u32 {
    match w {
        W::System | W::Cpu => 383,
        W::Memory | W::Network | W::Disk => 458,
        W::Health | W::Volumes => 313,
        W::Hardware => 0,
    }
}

/// Key of a widget's remembered height: its size and the layout width (window width in 40 px
/// steps, sidebar state), since both change how tall it gets.
fn height_key(w: W, span: u8, collapsed: bool) -> String {
    let vw = window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1280.0) as u32;
    format!("{}.{span}.{}.{}", w.id(), vw / 40, u8::from(collapsed))
}

fn heights() -> Vec<(String, u32)> {
    storage_get(HEIGHTS_KEY)
        .unwrap_or_default()
        .split(' ')
        .filter_map(|kv| kv.split_once('=').and_then(|(k, v)| Some((k.to_string(), v.parse().ok()?))))
        .collect()
}

/// Remembers `px` for `key` (most recent last, the oldest dropped past [`HEIGHTS_MAX`]).
fn remember_height(key: &str, px: u32) {
    let mut all = heights();
    if all.iter().any(|(k, v)| k == key && *v == px) {
        return;
    }
    all.retain(|(k, _)| k != key);
    all.push((key.to_string(), px));
    let skip = all.len().saturating_sub(HEIGHTS_MAX);
    let enc: Vec<String> = all.iter().skip(skip).map(|(k, v)| format!("{k}={v}")).collect();
    storage_set(HEIGHTS_KEY, &enc.join(" "));
}

#[component]
fn Cell(w: W, board: Board) -> impl IntoView {
    let ui = use_ui();
    let size = Memo::new(move |_| board.layout.with(|l| l.iter().find(|i| i.w == w).map(|i| i.size).unwrap_or(Size::S)));
    let i = board.visible.with_untracked(|v| v.iter().position(|x| *x == w).unwrap_or(0));
    // Until its data arrives a widget shows a placeholder at the height it had last time (or a
    // typical one), so nothing below moves when the content replaces it; once loaded, its height
    // is remembered for the next visit.
    let node = NodeRef::<Div>::new();
    let live = use_live();
    let ready = Memo::new(move |_| loaded(live, w));
    let key = Memo::new(move |_| height_key(w, w.span(size.get()), ui.sidebar_collapsed.get()));
    let reserve = Memo::new(move |_| {
        if ready.get() {
            return None;
        }
        let k = key.get();
        let px = heights().into_iter().find(|(x, _)| *x == k).map(|(_, v)| v).unwrap_or_else(|| default_height(w));
        (px > 0).then_some(px)
    });
    // (may run after the page was left: then the reactive values are gone and it does nothing)
    let measure = move || {
        if let (Some(Some(el)), Some(true), Some(k)) = (node.try_get_untracked(), ready.try_get_untracked(), key.try_get_untracked())
            && el.is_connected()
        {
            let h = el.get_bounding_client_rect().height().round() as u32;
            if h > 0 {
                remember_height(&k, h);
            }
        }
    };
    Effect::new(move |_| {
        if ready.get() {
            // once laid out, and again when the slower resources have arrived too
            request_animation_frame(move || request_animation_frame(measure));
            set_timeout(measure, std::time::Duration::from_secs(3));
        }
    });
    view! {
        <div
            class=move || {
                let drag = if board.dragging.get() == Some(w) { " dragging" } else { "" };
                format!("cell w-{}{drag}", w.span(size.get()))
            }
            data-w=w.id()
            node_ref=node
            style=move || match reserve.get() {
                Some(px) => format!("--i:{i}; min-height:{px}px"),
                None => format!("--i:{i}"),
            }
        >
            {widget(w)}
            <Show when=move || ui.dash_edit.get()>
                <CellTools w board size />
            </Show>
        </div>
    }
}

/// Edit controls of one widget: drag handle (also arrow keys), size, hide.
#[component]
fn CellTools(w: W, board: Board, size: Memo<Size>) -> impl IntoView {
    let ui = use_ui();
    let set_size = move |z: Size| {
        flip(board, None, move || board.layout.update(|l| if let Some(i) = l.iter_mut().find(|i| i.w == w) { i.size = z }));
        ui.relayout_soon();
    };
    let on_key = move |e: leptos::ev::KeyboardEvent| {
        let delta: isize = match e.key().as_str() {
            "ArrowLeft" | "ArrowUp" => -1,
            "ArrowRight" | "ArrowDown" => 1,
            "Home" => -100,
            "End" => 100,
            _ => return,
        };
        e.prevent_default();
        let vis = board.visible.get_untracked();
        let mut moved = None;
        flip(board, None, || {
            board.layout.update(|l| moved = layout::move_by(l, &vis, w, delta));
        });
        if let Some(pos) = moved {
            let (pos, count) = ((pos + 1).to_string(), vis.len().to_string());
            board.announce.set(tf("dash.moved", &[("widget", t(w.title())), ("pos", &pos), ("count", &count)]));
            // moving the node drops the focus: give it back once the DOM is updated
            request_animation_frame(move || focus_handle(w));
        }
    };
    view! {
        <div class="cell-tools">
            <button
                class="tool drag-handle"
                aria-label=move || format!("{}: {}", t("dash.move"), t(w.title()))
                aria-describedby="dash-hint"
                title=tr("dash.move")
                on:pointerdown=move |e| start_drag(board, w, e)
                on:keydown=on_key
            >
                <Icon icon=icons::GRIP_VERTICAL size=16 />
            </button>
            <span class="tool-title">{move || t(w.title())}</span>
            {(w.sizes().len() > 1).then(|| view! {
                <div class="size-seg" role="radiogroup" aria-label=tr("dash.size")>
                    {w.sizes().iter().map(|(z, _)| {
                        let z = *z;
                        view! {
                            <button
                                role="radio"
                                aria-checked=move || (size.get() == z).to_string()
                                class:on=move || size.get() == z
                                title=move || t(match z { Size::S => "dash.size.S", Size::M => "dash.size.M", Size::L => "dash.size.L" })
                                on:click=move |_| set_size(z)
                            >{z.code()}</button>
                        }
                    }).collect_view()}
                </div>
            })}
            <button
                class="tool"
                aria-label=move || format!("{}: {}", t("dash.hide"), t(w.title()))
                title=tr("dash.hide")
                on:click=move |_| flip(board, None, move || board.layout.update(|l| if let Some(i) = l.iter_mut().find(|i| i.w == w) { i.hidden = true }))
            >
                <Icon icon=icons::EYE_OFF size=16 />
            </button>
        </div>
    }
}

fn focus_handle(w: W) {
    if let Ok(Some(el)) = document().query_selector(&format!("[data-w='{}'] .drag-handle", w.id()))
        && let Ok(el) = el.dyn_into::<HtmlElement>()
    {
        let _ = el.focus();
    }
}

fn grid_el(board: Board) -> Option<HtmlElement> {
    board.grid.get_untracked().map(|d| d.unchecked_into::<HtmlElement>())
}

fn cells(grid: &HtmlElement) -> Vec<HtmlElement> {
    let list = grid.children();
    (0..list.length())
        .filter_map(|i| list.item(i))
        .filter_map(|e| e.dyn_into::<HtmlElement>().ok())
        .filter(|e| e.has_attribute("data-w"))
        .collect()
}

/// FLIP: remember where every cell is, apply `mutate`, then animate each cell from its old
/// place to the new one with `transform` only (no layout work per frame). `skip` is the cell
/// being dragged, which follows the pointer instead.
fn flip(board: Board, skip: Option<W>, mutate: impl FnOnce()) {
    let Some(grid) = grid_el(board) else {
        mutate();
        return;
    };
    if reduced_motion() {
        mutate();
        return;
    }
    let g = grid.get_bounding_client_rect();
    let before: Vec<(String, f64, f64)> = cells(&grid)
        .iter()
        .map(|el| {
            let r = el.get_bounding_client_rect();
            (el.get_attribute("data-w").unwrap_or_default(), r.left() - g.left(), r.top() - g.top())
        })
        .collect();
    mutate();
    // the reactive update lands in a microtask, before the next frame: measure there
    request_animation_frame(move || {
        let skip = skip.map(W::id);
        let mut moved = Vec::new();
        for el in cells(&grid) {
            let id = el.get_attribute("data-w").unwrap_or_default();
            if Some(id.as_str()) == skip {
                continue;
            }
            let Some((_, x, y)) = before.iter().find(|b| b.0 == id) else { continue };
            let (dx, dy) = (x - el.offset_left() as f64, y - el.offset_top() as f64);
            if dx.abs() < 0.5 && dy.abs() < 0.5 {
                continue;
            }
            let st = el.style();
            let _ = st.set_property("transition", "none");
            let _ = st.set_property("transform", &format!("translate({dx}px, {dy}px)"));
            moved.push(el);
        }
        if moved.is_empty() {
            return;
        }
        let _ = grid.offset_height(); // commit the inverted positions
        for el in moved {
            let st = el.style();
            let _ = st.set_property("transition", "transform 260ms var(--ease)");
            let _ = st.remove_property("transform");
        }
    });
}

/// Pointer drag of a cell: the cell follows the pointer, the others make room (FLIP) as it
/// passes over them. Esc cancels. Window listeners exist only while dragging.
fn start_drag(board: Board, w: W, e: PointerEvent) {
    if e.button() != 0 {
        return;
    }
    e.prevent_default();
    let Some(grid) = grid_el(board) else { return };
    let Some(el) = cells(&grid).into_iter().find(|c| c.get_attribute("data-w").as_deref() == Some(w.id())) else { return };
    let r = el.get_bounding_client_rect();
    let grab = (e.client_x() as f64 - r.left(), e.client_y() as f64 - r.top());
    let original = board.layout.get_untracked();
    let pointer = StoredValue::new((e.client_x() as f64, e.client_y() as f64));
    let last = StoredValue::new((None::<W>, 0.0f64));
    let _ = el.style().set_property("transition", "none");
    board.dragging.set(Some(w));

    // place the dragged cell under the pointer, relative to its current slot in the grid
    let follow = {
        let (grid, el) = (grid.clone(), el.clone());
        move || {
            let (x, y) = pointer.get_value();
            let g = grid.get_bounding_client_rect();
            let tx = x - grab.0 - (g.left() + el.offset_left() as f64);
            let ty = y - grab.1 - (g.top() + el.offset_top() as f64);
            let _ = el.style().set_property("transform", &format!("translate({tx}px, {ty}px) scale(1.02)"));
        }
    };
    let follow = StoredValue::new_local(follow);

    let on_move = window_event_listener(leptos::ev::pointermove, move |e| {
        let (x, y) = (e.client_x() as f64, e.client_y() as f64);
        pointer.set_value((x, y));
        follow.with_value(|f| f());
        // the dragged cell ignores the pointer (CSS), so this finds what is under it
        let target = document()
            .element_from_point(x as f32, y as f32)
            .and_then(|el| el.closest("[data-w]").ok().flatten())
            .and_then(|el| el.get_attribute("data-w"))
            .and_then(|id| layout::ALL.into_iter().find(|w| w.id() == id));
        let Some(target) = target.filter(|t| *t != w) else { return };
        let now = js_sys::Date::now();
        let (prev, at) = last.get_value();
        if prev == Some(target) && now - at < 400.0 {
            return;
        }
        last.set_value((Some(target), now));
        flip(board, Some(w), move || {
            board.layout.update(|l| {
                layout::move_to(l, w, target);
            });
        });
        request_animation_frame(move || follow.with_value(|f| f()));
    });

    let listeners: StoredValue<Vec<WindowListenerHandle>, LocalStorage> = StoredValue::new_local(vec![on_move]);
    let finish = move |cancel: bool| {
        listeners.update_value(|v| v.drain(..).for_each(WindowListenerHandle::remove));
        if cancel {
            let original = original.clone();
            flip(board, Some(w), move || board.layout.set(original));
        }
        let _ = el.style().set_property("transition", "transform 220ms var(--ease)");
        request_animation_frame({
            let el = el.clone();
            move || {
                let _ = el.style().remove_property("transform");
            }
        });
        set_timeout(move || { board.dragging.try_set(None); }, std::time::Duration::from_millis(220));
    };
    let finish = StoredValue::new_local(finish);
    let up = window_event_listener(leptos::ev::pointerup, move |_| finish.with_value(|f| f(false)));
    let cancel = window_event_listener(leptos::ev::pointercancel, move |_| finish.with_value(|f| f(false)));
    let esc = window_event_listener(leptos::ev::keydown, move |e| {
        if e.key() == "Escape" {
            e.prevent_default();
            finish.with_value(|f| f(true));
        }
    });
    listeners.update_value(|v| v.extend([up, cancel, esc]));
}

// ---------------------------------------------------------------- widgets

fn row(label: &'static str, value: impl IntoView + 'static) -> impl IntoView {
    view! {
        <div class="kv">
            <dt>{move || t(label)}</dt>
            <dd>{value}</dd>
        </div>
    }
}

#[component]
fn SystemWidget() -> impl IntoView {
    let live = use_live();
    let sys = live.system;
    let field = move |f: fn(&rynas_api_types::SystemInfo) -> String| move || sys.with(|s| s.as_ref().map(f).unwrap_or_default());
    let uptime = move || sys.with(|s| s.as_ref().map(|s| i18n::duration(s.uptime_s + live.uptime_offset.get())).unwrap_or_default());
    let load = move || {
        live.sample.with(|s| {
            s.as_ref()
                .map(|s| s.load.iter().map(|l| i18n::num(*l as f64, 2)).collect::<Vec<_>>().join(" · "))
                .unwrap_or_default()
        })
    };
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::SERVER title="w.system" />
            <Show when=move || sys.with(Option::is_some) fallback=|| view! { <Pending lines=6 res=Res::System /> }>
                <div class="sys-hero">
                    <div class="sys-model">{move || sys.with(|s| s.as_ref().map(|s| if s.model.is_empty() { s.hostname.clone() } else { s.model.clone() }).unwrap_or_default())}</div>
                    <div class="sys-host mono">{field(|s| s.hostname.clone())}</div>
                </div>
                <dl class="kvs">
                    {row("f.os", field(|s| s.os.clone()))}
                    {row("f.kernel", view! { <span class="mono">{field(|s| s.kernel.clone())}</span> })}
                    {row("f.cpu", field(|s| format!("{} · {} {}", s.cpu_model, s.cpu_threads, t("f.threads"))))}
                    {row("f.uptime", uptime)}
                    {row("f.load", load)}
                    {row("f.version", field(|s| s.rynas_version.clone()))}
                </dl>
            </Show>
        </section>
    }
}

/// "Last N min" under a chart: follows the window the chart really shows.
fn span_label() -> impl Fn() -> String + Copy {
    let live = use_live();
    move || {
        live.tick.track();
        let n = live.history.with_value(|h| h.len());
        tf("last_min", &[("n", &span_minutes(n, 300).to_string())])
    }
}

#[component]
fn CpuWidget() -> impl IntoView {
    let live = use_live();
    let s = live.sample;
    let cpu = move || s.with(|s| s.as_ref().map(|s| s.cpu_pct).unwrap_or(0.0));
    let threads = move || s.with(|s| s.as_ref().map(|s| s.cpu_per_thread.len()).unwrap_or(0));
    let temp = Memo::new(move |_| s.with(|s| s.as_ref().and_then(|s| s.temps.iter().find(|t| t.id == "cpu").and_then(|t| t.celsius))));
    let span = span_label();
    view! {
        <section class="card widget cpu-widget">
            <WidgetHead icon=icons::CPU title="w.cpu">
                <Show when=move || temp.get().is_some()>
                    <Status
                        tone=Signal::derive(move || temp.get().map(|c| temp_tone("cpu", c)).unwrap_or(Tone::Unknown))
                        text=Signal::derive(move || temp.get().map(i18n::celsius).unwrap_or_default())
                    />
                </Show>
            </WidgetHead>
            <Show when=move || s.with(Option::is_some) fallback=|| view! { <Skeleton lines=4 /> }>
                <div class="cpu-body">
                    <div class="kpi-col">
                        <div class="kpi">
                            <span class="kpi-num">{move || i18n::num(cpu() as f64, 0)}</span>
                            <span class="kpi-unit">"%"</span>
                        </div>
                        <div class="kpi-sub">{span}</div>
                        <div class="threads" aria-label=tr("per_thread")>
                            <For each=move || 0..threads() key=|i| *i let:i>
                                {
                                    let v = move || s.with(|s| s.as_ref().and_then(|s| s.cpu_per_thread.get(i).copied()).unwrap_or(0.0));
                                    view! {
                                        <div class="thread" title=move || format!("#{i} · {}", i18n::pct(v() as f64))>
                                            <div class="thread-bar">
                                                <div class="thread-fill" style:transform=move || format!("scaleY({})", (v() / 100.0).clamp(0.0, 1.0))></div>
                                            </div>
                                        </div>
                                    }
                                }
                            </For>
                        </div>
                        <div class="kpi-sub">{tr("per_thread")}</div>
                    </div>
                    <div class="chart-wrap tall">
                        <LiveChart
                            series=vec![Series::new("--c1", true, |m: &MetricsSample| m.cpu_pct)]
                            max=100.0
                            label=|v| format!("{v:.0}%")
                            aria=Signal::derive(move || format!("{} {}", t("w.cpu"), i18n::pct(cpu() as f64)))
                        />
                    </div>
                </div>
            </Show>
        </section>
    }
}

/// zram over every device: "11.6 GiB → 5.01 GiB (lz4, 2.3×)" and the detail line (RAM used,
/// RAM saved, capacity); `None` without zram.
pub fn zram_text(z: &[ZramDevice]) -> Option<(String, String)> {
    if z.is_empty() {
        return None;
    }
    let (data, comp, used, size) =
        z.iter().fold((0u64, 0u64, 0u64, 0u64), |a, d| (a.0 + d.data, a.1 + d.compressed, a.2 + d.mem_used, a.3 + d.disksize));
    let mut algs: Vec<&str> = z.iter().map(|d| d.algorithm.as_str()).filter(|a| !a.is_empty()).collect();
    algs.dedup();
    let mut info = algs.join("+");
    if comp > 0 && data > 0 {
        if !info.is_empty() {
            info.push_str(", ");
        }
        info.push_str(&format!("{}×", i18n::num(data as f64 / comp as f64, 1)));
    }
    let main = if info.is_empty() {
        format!("{} → {}", i18n::bytes(data), i18n::bytes(comp))
    } else {
        format!("{} → {} ({info})", i18n::bytes(data), i18n::bytes(comp))
    };
    let detail = tf(
        "m.zram_detail",
        &[("used", &i18n::bytes(used)), ("saved", &i18n::bytes(data.saturating_sub(used))), ("size", &i18n::bytes(size))],
    );
    Some((main, detail))
}

#[component]
fn MemoryWidget() -> impl IntoView {
    let live = use_live();
    let s = live.sample;
    let mem = move || s.with(|s| s.as_ref().map(|s| s.mem).unwrap_or_default());
    let share = move |v: u64| {
        let m = mem();
        if m.total == 0 { 0.0 } else { v as f64 * 100.0 / m.total as f64 }
    };
    let swap_pct = Signal::derive(move || {
        let m = mem();
        if m.swap_total == 0 { 0.0 } else { m.swap_used as f64 * 100.0 / m.swap_total as f64 }
    });
    let free = move || {
        let m = mem();
        m.total.saturating_sub(m.used).saturating_sub(m.cache)
    };
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::MEMORY_STICK title="w.memory">
                <span class="w-meta tnum">{move || i18n::pct(share(mem().used))}</span>
            </WidgetHead>
            <Show when=move || s.with(Option::is_some) fallback=|| view! { <Skeleton lines=4 /> }>
                <div class="kpi">
                    <span class="kpi-num">{move || i18n::bytes(mem().used)}</span>
                    <span class="kpi-unit">{move || format!("{} {}", t("m.of"), i18n::bytes(mem().total))}</span>
                </div>
                <div class="stack" role="img" aria-label=move || format!("{} {}", t("m.used"), i18n::pct(share(mem().used)))>
                    <div class="stack-seg c1" style:flex-grow=move || share(mem().used).to_string()></div>
                    <div class="stack-seg c2" style:flex-grow=move || share(mem().cache.min(mem().total.saturating_sub(mem().used))).to_string()></div>
                    <div class="stack-seg rest" style:flex-grow=move || share(free()).to_string()></div>
                </div>
                <ul class="legend">
                    <li><span class="dot c1"></span>{tr("m.used")}<span class="tnum">{move || i18n::bytes(mem().used)}</span></li>
                    <li><span class="dot c2"></span>{tr("m.cache")}<span class="tnum">{move || i18n::bytes(mem().cache)}</span></li>
                    <li><span class="dot rest"></span>{tr("m.free")}<span class="tnum">{move || i18n::bytes(free())}</span></li>
                </ul>
                <Show when=move || { mem().swap_total > 0 }>
                    <div class="sub-row">
                        <span>{tr("m.swap")}</span>
                        <span class="tnum muted">{move || format!("{} / {}", i18n::bytes(mem().swap_used), i18n::bytes(mem().swap_total))}</span>
                    </div>
                    <Meter pct=swap_pct tone=Signal::derive(move || usage_tone(swap_pct.get())) />
                </Show>
                {move || s.with(|s| s.as_ref().and_then(|s| zram_text(&s.zram))).map(|(main, detail)| view! {
                    <div class="sub-row" title=detail>
                        <span>"zram"</span>
                        <span class="tnum muted">{main}</span>
                    </div>
                })}
                <div class="chart-wrap spark">
                    <LiveChart
                        series=vec![Series::new("--c2", true, |m: &MetricsSample| if m.mem.total == 0 { 0.0 } else { (m.mem.used as f64 * 100.0 / m.mem.total as f64) as f32 })]
                        max=100.0
                        grid=false
                        class="chart"
                        aria=Signal::derive(move || t("w.memory").to_string())
                    />
                </div>
            </Show>
        </section>
    }
}

/// Devices to list: the newest sample's, then any seen in the last minute that it lacks, so a
/// sample missing a device for a moment does not make its row vanish and come back (the list,
/// and the card, keep their size).
fn device_names(live: Live, of: fn(&MetricsSample) -> Vec<String>) -> Vec<String> {
    live.tick.track();
    live.history.with_value(|h| {
        let mut out = h.back().map(of).unwrap_or_default();
        for m in h.iter().rev().skip(1).take(60) {
            for n in of(m) {
                if !out.contains(&n) {
                    out.push(n);
                }
            }
        }
        out
    })
}

fn net_total(m: &MetricsSample) -> (u64, u64) {
    m.net.iter().filter(|n| n.up).fold((0, 0), |(r, w), n| (r + n.rx_bps, w + n.tx_bps))
}

#[component]
fn NetworkWidget() -> impl IntoView {
    let live = use_live();
    let s = live.sample;
    let tot = move || s.with(|s| s.as_ref().map(net_total).unwrap_or_default());
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::NETWORK title="w.network" />
            <Show when=move || s.with(Option::is_some) fallback=|| view! { <Skeleton lines=4 /> }>
                <div class="duo">
                    <div class="duo-item">
                        <span class="duo-label"><span class="dot c1"></span><Icon icon=icons::ARROW_DOWN size=14 />{tr("n.rx")}</span>
                        <span class="duo-num">{move || i18n::bitrate(tot().0)}</span>
                    </div>
                    <div class="duo-item">
                        <span class="duo-label"><span class="dot c2"></span><Icon icon=icons::ARROW_UP size=14 />{tr("n.tx")}</span>
                        <span class="duo-num">{move || i18n::bitrate(tot().1)}</span>
                    </div>
                </div>
                <div class="chart-wrap">
                    <LiveChart
                        series=vec![
                            // in bit/s, so the 1-2-5 scale rounds in the unit shown
                            Series::new("--c1", true, |m: &MetricsSample| net_total(m).0 as f32 * 8.0),
                            Series::new("--c2", false, |m: &MetricsSample| net_total(m).1 as f32 * 8.0),
                        ]
                        label=|v| i18n::bitrate(v as u64 / 8)
                        aria=Signal::derive(move || format!("{} {} / {}", t("w.network"), i18n::bitrate(tot().0), i18n::bitrate(tot().1)))
                    />
                </div>
                <ul class="rows">
                    <For
                        each=move || device_names(live, |m| m.net.iter().map(|n| n.name.clone()).collect())
                        key=|n| n.clone()
                        let:name
                    >
                        {
                            let n2 = name.clone();
                            let iface = Memo::new(move |_| s.with(|s| s.as_ref().and_then(|s| s.net.iter().find(|n| n.name == n2).cloned())));
                            view! {
                                <li class="row">
                                    <span class=move || if iface.get().is_some_and(|i| i.up) { "led on" } else { "led off" } aria-hidden="true"></span>
                                    <span class="mono">{name.clone()}</span>
                                    <span class="muted small">{move || iface.get().and_then(|i| i.speed_mbps).map(i18n::link_speed).unwrap_or_default()}</span>
                                    <span class="row-end tnum small">
                                        {move || match iface.get() {
                                            Some(i) if i.up => format!("↓ {}  ↑ {}", i18n::bitrate(i.rx_bps), i18n::bitrate(i.tx_bps)),
                                            _ => t("n.down").to_string(),
                                        }}
                                    </span>
                                </li>
                            }
                        }
                    </For>
                </ul>
            </Show>
        </section>
    }
}

/// Avoid double counting: when md arrays exist, their sdX members are left out of the total.
fn disk_total(m: &MetricsSample) -> (u64, u64) {
    let has_md = m.disks.iter().any(|d| d.name.starts_with("md"));
    m.disks
        .iter()
        .filter(|d| !(has_md && d.name.starts_with("sd")))
        .fold((0, 0), |(r, w), d| (r + d.read_bps, w + d.write_bps))
}

#[component]
fn DiskWidget() -> impl IntoView {
    let live = use_live();
    let s = live.sample;
    let tot = move || s.with(|s| s.as_ref().map(disk_total).unwrap_or_default());
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::ACTIVITY title="w.disk_io" />
            <Show when=move || s.with(Option::is_some) fallback=|| view! { <Skeleton lines=4 /> }>
                <div class="duo">
                    <div class="duo-item">
                        <span class="duo-label"><span class="dot c3"></span>{tr("d.read")}</span>
                        <span class="duo-num">{move || i18n::rate(tot().0)}</span>
                    </div>
                    <div class="duo-item">
                        <span class="duo-label"><span class="dot c4"></span>{tr("d.write")}</span>
                        <span class="duo-num">{move || i18n::rate(tot().1)}</span>
                    </div>
                </div>
                <div class="chart-wrap">
                    <LiveChart
                        series=vec![
                            Series::new("--c3", true, |m: &MetricsSample| disk_total(m).0 as f32),
                            Series::new("--c4", false, |m: &MetricsSample| disk_total(m).1 as f32),
                        ]
                        label=|v| i18n::rate(v as u64)
                        binary=true
                        aria=Signal::derive(move || format!("{} {} / {}", t("w.disk_io"), i18n::rate(tot().0), i18n::rate(tot().1)))
                    />
                </div>
                <ul class="rows">
                    <For
                        each=move || device_names(live, |m| m.disks.iter().map(|d| d.name.clone()).collect())
                        key=|n| n.clone()
                        let:name
                    >
                        {
                            let n2 = name.clone();
                            let d = Memo::new(move |_| s.with(|s| s.as_ref().and_then(|s| s.disks.iter().find(|d| d.name == n2).cloned())));
                            let busy = Signal::derive(move || d.get().map(|d| d.busy_pct as f64).unwrap_or(0.0));
                            view! {
                                <li class="row disk-row">
                                    <span class="mono">{name.clone()}</span>
                                    <span class="row-end tnum small muted">{move || d.get().map(|d| format!("{} · {}", i18n::rate(d.read_bps), i18n::rate(d.write_bps))).unwrap_or_default()}</span>
                                    <span class="busy tnum small" title=tr("d.busy")>{move || i18n::pct(busy.get())}</span>
                                    <div class="row-meter"><Meter pct=busy /></div>
                                </li>
                            }
                        }
                    </For>
                </ul>
            </Show>
        </section>
    }
}

/// Storage at a glance: overall state, counts, fullest volume, hottest drive.
#[component]
fn HealthWidget() -> impl IntoView {
    let live = use_live();
    let st = live.storage;
    let health = Memo::new(move |_| st.with(|s| s.as_ref().map(storage_health)));
    let counts = Memo::new(move |_| {
        st.with(|s| {
            s.as_ref().map(|s| {
                let standby = s.disks.iter().filter(|d| d.power_state == rynas_api_types::storage::PowerState::Standby).count();
                let local = s.filesystems.iter().filter(|f| !alerts::is_network_fs(&f.fstype)).count();
                (s.disks.len(), standby, s.arrays.len(), local)
            })
        })
    });
    let fullest = Memo::new(move |_| {
        st.with(|s| {
            s.as_ref().and_then(|s| {
                s.filesystems
                    .iter()
                    .filter(|f| !alerts::is_network_fs(&f.fstype))
                    .map(|f| (f.mountpoint.clone(), fs_pct(f.used, f.size)))
                    .max_by(|a, b| a.1.total_cmp(&b.1))
            })
        })
    });
    let hottest = Memo::new(move |_| {
        let hw = live.hardware.get();
        live.sample.with(|s| {
            alerts::all_temps(s.as_ref(), hw.as_ref())
                .into_iter()
                .filter(|(id, _, _)| id != "cpu" && !id.starts_with("nic:"))
                .max_by(|a, b| a.2.total_cmp(&b.2))
        })
    });
    view! {
        <section class="card widget health-widget">
            <WidgetHead icon=icons::SHIELD_CHECK title="w.health" />
            <Show when=move || health.get().is_some() fallback=|| view! { <Skeleton lines=4 /> }>
                {move || {
                    let (tone, key) = health.get().unwrap_or((Tone::Unknown, "health.unknown"));
                    let icon = if matches!(tone, Tone::Ok | Tone::Info) { icons::SHIELD_CHECK } else { icons::SHIELD_ALERT };
                    view! {
                        <div class=format!("health-hero {}", tone.class())>
                            <span class="health-icon"><Icon icon=icon size=26 /></span>
                            <div>
                                <div class="health-title">{t(key)}</div>
                                <div class="small muted">{move || t(health_sub(tone))}</div>
                            </div>
                        </div>
                    }
                }}
                <div class="tiles">
                    <div class="tile">
                        <span class="tile-num tnum">{move || counts.get().map(|c| c.0).unwrap_or(0)}</span>
                        <span class="tile-label">{tr("h.disks")}</span>
                        <span class="tile-sub">{move || counts.get().filter(|c| c.1 > 0).map(|c| format!("{} {}", c.1, t("h.asleep"))).unwrap_or_default()}</span>
                    </div>
                    <div class="tile">
                        <span class="tile-num tnum">{move || counts.get().map(|c| c.2).unwrap_or(0)}</span>
                        <span class="tile-label">{tr("h.arrays")}</span>
                        <span class="tile-sub">{move || if counts.get().is_some_and(|c| c.2 == 0) { t("h.no_arrays") } else { "" }}</span>
                    </div>
                    <div class="tile">
                        <span class="tile-num tnum">{move || counts.get().map(|c| c.3).unwrap_or(0)}</span>
                        <span class="tile-label">{tr("h.volumes")}</span>
                        <span class="tile-sub"></span>
                    </div>
                </div>
                {move || fullest.get().map(|(mp, pct)| view! {
                    <div class="health-row">
                        <div class="sub-row small">
                            <span class="muted">{t("h.fullest")}</span>
                            <span><span class="mono">{mp}</span>" · "<span class="tnum">{i18n::pct(pct)}</span></span>
                        </div>
                        <Meter pct=pct tone=Signal::derive(move || usage_tone(pct)) />
                    </div>
                })}
                {move || hottest.get().map(|(id, label, c)| view! {
                    <div class="sub-row small">
                        <span class="muted"><Icon icon=icons::THERMOMETER size=14 class="inline" />{temp_label(&id, &label)}</span>
                        <span class=format!("tnum strong tone-text {}", temp_tone(&id, c).class())>{i18n::celsius(c)}</span>
                    </div>
                })}
            </Show>
        </section>
    }
}

fn health_sub(tone: Tone) -> &'static str {
    match tone {
        Tone::Ok => "h.sub_ok",
        Tone::Info => "h.sub_sync",
        Tone::Warn | Tone::Err => "h.sub_warn",
        _ => "health.unknown",
    }
}

/// Local volumes shown in the dashboard widget; the rest is one click away.
const VOLUMES_SHOWN: usize = 6;

/// Localized sensor label: bays and CPU are named by the UI, the rest as the server says.
pub fn temp_label(id: &str, label: &str) -> String {
    match id.strip_prefix("bay") {
        Some(n) if n.chars().all(|c| c.is_ascii_digit()) => format!("{} {n}", t("sto.bay")),
        _ => label.to_string(),
    }
}

/// Volumes (file systems with usage bars) and RAID arrays with state and resync progress.
#[component]
fn VolumesWidget() -> impl IntoView {
    let live = use_live();
    let st = live.storage;
    view! {
        <section class="card widget volumes-widget">
            <WidgetHead icon=icons::DATABASE title="w.volumes">
                <a class="w-link" href="/storage">{tr("sto.title")}<Icon icon=icons::CHEVRON_RIGHT size=14 /></a>
            </WidgetHead>
            <Show when=move || st.with(Option::is_some) fallback=|| view! { <Pending lines=4 res=Res::Storage /> }>
                {move || {
                    let s = st.get().unwrap_or_default();
                    if s.filesystems.is_empty() && s.arrays.is_empty() {
                        return view! {
                            <div class="mini-empty"><Icon icon=icons::DATABASE size=20 /><strong>{t("v.none")}</strong><span class="small muted">{t("v.none_text")}</span></div>
                        }.into_any();
                    }
                    let arrays = s.arrays.clone();
                    let (local, remote): (Vec<_>, Vec<_>) = s.filesystems.iter().partition(|f| !alerts::is_network_fs(&f.fstype));
                    let more = local.len().saturating_sub(VOLUMES_SHOWN);
                    let remote_n = remote.len();
                    view! {
                        <ul class="volumes">
                            {local.into_iter().take(VOLUMES_SHOWN).map(|f| {
                                let pct = fs_pct(f.used, f.size);
                                let tone = usage_tone(pct);
                                view! {
                                    <li class="volume">
                                        <div class="vol-top">
                                            <span class="mono vol-mp">{f.mountpoint.clone()}</span>
                                            <span class="chip">{f.fstype.clone()}</span>
                                            <span class="muted small mono vol-src">{f.source.clone()}</span>
                                            <span class=format!("row-end tnum vol-pct tone-text {}", tone.class())>{i18n::pct(pct)}</span>
                                        </div>
                                        <Meter pct=pct tone=Signal::derive(move || tone) />
                                        <div class="vol-bottom small muted tnum">
                                            <span>{format!("{} {} {}", i18n::bytes(f.used), t("m.of"), i18n::bytes(f.size))}</span>
                                            <span>{format!("{} {}", i18n::bytes(f.avail), t("v.free"))}</span>
                                        </div>
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                        {(more > 0 || remote_n > 0).then(|| view! {
                            <a class="vol-more" href="/storage">
                                {(more > 0).then(|| view! { <span>{tf("v.more", &[("n", &more.to_string())])}</span> })}
                                {(remote_n > 0).then(|| view! { <span class="chip"><Icon icon=icons::NETWORK size=12 />{tf("v.network", &[("n", &remote_n.to_string())])}</span> })}
                                <Icon icon=icons::CHEVRON_RIGHT size=14 />
                            </a>
                        })}
                        {(!arrays.is_empty()).then(|| view! {
                            <div class="mini-title">{t("v.arrays")}</div>
                            <ul class="raid-list">
                                {arrays.iter().map(|a| {
                                    let (tone, key) = array_status(a);
                                    view! {
                                        <li class=format!("raid {}", tone.class())>
                                            <div class="raid-top">
                                                <span class="mono strong">{a.name.clone()}</span>
                                                <span class="chip accent">{a.level.to_uppercase()}</span>
                                                <span class="small muted tnum">{format!("{} · {} {}", i18n::bytes_si(a.size), a.members.len(), t("sto.members_n"))}</span>
                                                <span class="row-end"><Status tone=tone text=t(key).to_string() /></span>
                                            </div>
                                            {a.sync_pct.map(|p| view! {
                                                <div class="raid-sync">
                                                    <Meter pct=p as f64 tone=Signal::derive(|| Tone::Info) />
                                                    <span class="small tnum">{i18n::pct(p as f64)}</span>
                                                </div>
                                            })}
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                        })}
                    }.into_any()
                }}
            </Show>
        </section>
    }
}

/// The colour of the front status LED as words ("Blinking orange").
pub fn led_text(led: &str) -> String {
    try_t(&format!("led.{led}")).map(str::to_string).unwrap_or_else(|| led.to_string())
}

/// What the front status LED says about the machine (the provider drives it from the health:
/// green when all is well, blinking orange on a fan failure or a degraded array): the tone,
/// the state in words and, for a tooltip, the LED colour ("Status LED: Green").
pub fn led_state(led: &str) -> (Tone, String, String) {
    let (tone, key) = match led {
        "green" => (Tone::Ok, "hw.st_normal"),
        "green-blink" => (Tone::Info, "hw.st_starting"),
        "orange" | "orange-blink" => (Tone::Warn, "hw.st_attention"),
        "off" => (Tone::Err, "hw.st_fault"),
        _ => (Tone::Unknown, "health.unknown"),
    };
    (tone, t(key).to_string(), format!("{}: {}", t("hw.led"), led_text(led)))
}


#[component]
fn HardwareWidget() -> impl IntoView {
    let live = use_live();
    let hw = live.hardware;
    let h = move || hw.get().unwrap_or_default();
    // the status changes with every reading: the LED pill is built again only when it changes
    let led = Memo::new(move |_| hw.with(|h| h.as_ref().and_then(|h| h.status_led.clone())));
    view! {
        <section class="card widget">
            <WidgetHead icon=icons::FAN title="w.hardware">
                {move || led.get().map(|l| {
                    let (tone, state, colour) = led_state(&l);
                    view! { <span title=colour><Status tone=tone text=state /></span> }
                })}
            </WidgetHead>
            <div class="hw-mini">
                <div class="hw-col">
                    <div class="mini-title">{tr("hw.fans")}</div>
                    {move || h().fans.into_iter().map(|f| view! {
                        <div class="fan-row">
                            <span class=if f.alarm { "fan-icon alarm" } else { "fan-icon spin" }><Icon icon=icons::FAN size=16 /></span>
                            <span>{fan_label(&f.label)}</span>
                            <span class="row-end tnum">{f.rpm.map(|r| format!("{r} rpm")).unwrap_or_else(|| "—".into())}</span>
                        </div>
                    }).collect_view()}
                </div>
                <div class="hw-col">
                    <div class="mini-title">{tr("hw.temps")}</div>
                    <div class="temp-chips">
                        {move || {
                            let h = hw.get();
                            let v = live.sample.with(|s| alerts::all_temps(s.as_ref(), h.as_ref()));
                            v.into_iter().map(|(id, l, c)| view! {
                                <span class=format!("temp-chip {}", temp_tone(&id, c).class())>
                                    <Icon icon=icons::THERMOMETER size=14 />{temp_label(&id, &l)}<strong class="tnum">{i18n::celsius(c)}</strong>
                                </span>
                            }).collect_view()
                        }}
                    </div>
                </div>
            </div>
            <div class="mini-title">{tr("hw.events")}</div>
            <ul class="events compact">
                {move || {
                    let ev = h().events;
                    if ev.is_empty() {
                        return view! { <li class="muted small">{t("hw.no_events")}</li> }.into_any();
                    }
                    ev.into_iter().rev().take(3).map(|(ts, txt)| view! {
                        <li><span class="mono small muted">{ts}</span><span>{txt}</span></li>
                    }).collect_view().into_any()
                }}
            </ul>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(alg: &str, data: u64, compressed: u64) -> ZramDevice {
        ZramDevice { name: "zram0".into(), algorithm: alg.into(), disksize: 12 << 30, data, compressed, mem_used: compressed + (100 << 20), huge_pages: 0 }
    }

    #[test]
    fn zram_summary() {
        assert_eq!(zram_text(&[]), None);
        let (main, detail) = zram_text(&[dev("lz4", 12_473_589_760, 5_382_274_290)]).unwrap();
        assert_eq!(main, "11.6 GiB → 5.01 GiB (lz4, 2.3×)");
        assert!(detail.contains("12.0 GiB"), "{detail}");
        // two devices with the same algorithm: summed, the algorithm once
        let (main, _) = zram_text(&[dev("zstd", 3 << 30, 1 << 30), dev("zstd", 1 << 30, 1 << 29)]).unwrap();
        assert_eq!(main, "4.00 GiB → 1.50 GiB (zstd, 2.7×)");
        // an empty device: no ratio
        let (main, _) = zram_text(&[dev("lz4", 0, 0)]).unwrap();
        assert_eq!(main, "0 B → 0 B (lz4)");
    }
}
