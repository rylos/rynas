//! Reordering by pointer, shared by the dashboard grid and the sidebar: the FLIP animation of
//! the items of a container, and the pointer drag of one item (it follows the pointer, the
//! others make room as it passes over them, Esc cancels). The keyboard side (arrow keys on the
//! drag handle) is each caller's, since the moves differ (a grid, a list of groups).
//!
//! The items carry their id in an attribute (`data-w`, `data-nav`…) and the container must be
//! their offset parent (positioned), so `offset_left/top` are relative to it.

use leptos::prelude::*;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, PointerEvent};

use crate::ui::reduced_motion;

/// The items of `container` that carry `attr`, in document order.
pub fn items(container: &HtmlElement, attr: &str) -> Vec<HtmlElement> {
    let Ok(list) = container.query_selector_all(&format!("[{attr}]")) else { return Vec::new() };
    (0..list.length())
        .filter_map(|i| list.item(i))
        .filter_map(|n| n.dyn_into::<HtmlElement>().ok())
        .collect()
}

/// FLIP: remember where every item is, apply `mutate`, then animate each item from its old
/// place to the new one with `transform` only (no layout work per frame). `skip` is the item
/// being dragged, which follows the pointer instead.
pub fn flip(container: Option<HtmlElement>, attr: &'static str, skip: Option<String>, mutate: impl FnOnce()) {
    let Some(grid) = container else {
        mutate();
        return;
    };
    if reduced_motion() {
        mutate();
        return;
    }
    let g = grid.get_bounding_client_rect();
    let before: Vec<(String, f64, f64)> = items(&grid, attr)
        .iter()
        .map(|el| {
            let r = el.get_bounding_client_rect();
            (el.get_attribute(attr).unwrap_or_default(), r.left() - g.left(), r.top() - g.top())
        })
        .collect();
    mutate();
    // the reactive update lands in a microtask, before the next frame: measure there
    request_animation_frame(move || {
        let mut moved = Vec::new();
        for el in items(&grid, attr) {
            let id = el.get_attribute(attr).unwrap_or_default();
            if skip.as_deref() == Some(id.as_str()) {
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

/// What a drag does to the model.
pub struct DragHooks {
    /// The pointer is over another item (its id): move the dragged one there (with [`flip`]).
    pub over: Rc<dyn Fn(String)>,
    /// Esc: put everything back.
    pub cancel: Rc<dyn Fn()>,
    /// The drag ended (dropped or canceled), after the item settled.
    pub end: Rc<dyn Fn()>,
}

/// Pointer drag of the item `id` of `container`: the item follows the pointer, `hooks.over`
/// runs when it passes over another item (at most every 400 ms over the same one). Window
/// listeners exist only while dragging. `axis_y` keeps the item in its column (a list).
pub fn start_drag(container: Option<HtmlElement>, attr: &'static str, id: String, e: PointerEvent, axis_y: bool, hooks: DragHooks) {
    if e.button() != 0 {
        return;
    }
    e.prevent_default();
    let Some(grid) = container else { return };
    let Some(el) = items(&grid, attr).into_iter().find(|c| c.get_attribute(attr).as_deref() == Some(id.as_str())) else { return };
    let r = el.get_bounding_client_rect();
    let grab = (e.client_x() as f64 - r.left(), e.client_y() as f64 - r.top());
    let pointer = StoredValue::new((e.client_x() as f64, e.client_y() as f64));
    let last = StoredValue::new((None::<String>, 0.0f64));
    let _ = el.style().set_property("transition", "none");

    // place the dragged item under the pointer, relative to its current slot in the container
    let follow = {
        let (grid, el) = (grid.clone(), el.clone());
        move || {
            let (x, y) = pointer.get_value();
            let g = grid.get_bounding_client_rect();
            let tx = if axis_y { 0.0 } else { x - grab.0 - (g.left() + el.offset_left() as f64) };
            let ty = y - grab.1 - (g.top() + el.offset_top() as f64);
            let _ = el.style().set_property("transform", &format!("translate({tx}px, {ty}px) scale(1.02)"));
        }
    };
    let follow = StoredValue::new_local(follow);

    let over = hooks.over.clone();
    let on_move = window_event_listener(leptos::ev::pointermove, move |e| {
        let (x, y) = (e.client_x() as f64, e.client_y() as f64);
        pointer.set_value((x, y));
        follow.with_value(|f| f());
        // the dragged item ignores the pointer (CSS), so this finds what is under it
        let target = document()
            .element_from_point(x as f32, y as f32)
            .and_then(|el| el.closest(&format!("[{attr}]")).ok().flatten())
            .and_then(|el| el.get_attribute(attr));
        let Some(target) = target.filter(|t| *t != id) else { return };
        let now = js_sys::Date::now();
        let (prev, at) = last.get_value();
        if prev.as_deref() == Some(target.as_str()) && now - at < 400.0 {
            return;
        }
        last.set_value((Some(target.clone()), now));
        over(target);
        request_animation_frame(move || follow.with_value(|f| f()));
    });

    let listeners: StoredValue<Vec<WindowListenerHandle>, LocalStorage> = StoredValue::new_local(vec![on_move]);
    let (cancel, end) = (hooks.cancel, hooks.end);
    let finish = move |canceled: bool| {
        listeners.update_value(|v| v.drain(..).for_each(WindowListenerHandle::remove));
        if canceled {
            cancel();
        }
        let _ = el.style().set_property("transition", "transform 220ms var(--ease)");
        request_animation_frame({
            let el = el.clone();
            move || {
                let _ = el.style().remove_property("transform");
            }
        });
        let end = end.clone();
        set_timeout(move || end(), std::time::Duration::from_millis(220));
    };
    let finish = StoredValue::new_local(finish);
    let up = window_event_listener(leptos::ev::pointerup, move |_| finish.with_value(|f| f(false)));
    let cancel = window_event_listener(leptos::ev::pointercancel, move |_| finish.with_value(|f| f(false)));
    let esc = window_event_listener(leptos::ev::keydown, move |e| {
        if e.key() == "Escape" {
            e.prevent_default();
            e.stop_propagation();
            finish.with_value(|f| f(true));
        }
    });
    listeners.update_value(|v| v.extend([up, cancel, esc]));
}

/// Gives the focus back to the drag handle of item `id` (moving a node drops the focus).
pub fn focus_handle(attr: &str, id: &str) {
    if let Ok(Some(el)) = document().query_selector(&format!("[{attr}='{id}'] .drag-handle"))
        && let Ok(el) = el.dyn_into::<HtmlElement>()
    {
        let _ = el.focus();
    }
}
