//! Tiny canvas renderer for live time series (no JS chart library).
//!
//! Charts redraw only their `<canvas>`: no DOM re-render. Points are placed by their server
//! timestamp and the right edge follows the (estimated) server clock one sample behind, so the
//! series scrolls continuously instead of jumping once per sample; lines are monotone cubics
//! over a gradient, with a head (dot and halo) riding the curve at the right edge of the plot
//! and a mini scale (top and half lines with their values) right of it. Colors come from CSS custom properties read with `getComputedStyle`, so they follow the
//! theme. HiDPI aware (devicePixelRatio). Static when the user prefers reduced motion.

use std::sync::Arc;

use leptos::html::Canvas;
use leptos::prelude::*;
use rynas_api_types::MetricsSample;
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

use crate::live::use_live;
use crate::prefs::use_ui;
use crate::ui::reduced_motion;

pub type Getter = Arc<dyn Fn(&MetricsSample) -> f32 + Send + Sync>;

#[derive(Clone)]
pub struct Series {
    /// CSS custom property holding the color, e.g. "--c1".
    pub color: &'static str,
    pub fill: bool,
    pub get: Getter,
}

impl Series {
    pub fn new(color: &'static str, fill: bool, get: impl Fn(&MetricsSample) -> f32 + Send + Sync + 'static) -> Self {
        Series { color, fill, get: Arc::new(get) }
    }
}

#[derive(Clone, Copy)]
pub struct Opts {
    /// Fixed top of the scale (e.g. 100 for percentages); auto when None.
    pub max: Option<f32>,
    /// Number of samples across the width.
    pub points: usize,
    pub grid: bool,
    /// Formats the top-of-scale label; no label when None.
    pub label: Option<fn(f32) -> String>,
    /// Bytes: round the scale top in powers of 1024 (100 MiB/s, not 95,4 MiB/s).
    pub binary: bool,
}

fn hex_rgba(hex: &str, a: f32) -> String {
    let h = hex.trim().trim_start_matches('#');
    if h.len() == 6 {
        let p = |i| u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(128);
        format!("rgba({},{},{},{a})", p(0), p(2), p(4))
    } else {
        hex.trim().to_string()
    }
}

/// 1-2-5 rounding of the scale top.
fn nice(v: f32) -> f32 {
    if v <= 0.0 {
        return 1.0;
    }
    let e = 10f32.powf(v.log10().floor());
    let f = v / e;
    let n = if f <= 1.0 {
        1.0
    } else if f <= 2.0 {
        2.0
    } else if f <= 5.0 {
        5.0
    } else {
        10.0
    };
    n * e
}

/// [`nice`] in the 1024-based unit of the value: 100 MiB, 2 GiB.
fn nice_binary(v: f32) -> f32 {
    let mut unit = 1f32;
    while v / unit >= 1024.0 {
        unit *= 1024.0;
    }
    nice(v / unit) * unit
}

/// Samples across the width: always the full window, also while the history fills (the curve
/// comes in from the right and the left stays empty). A window that grows with the samples
/// squeezes the whole curve sideways until it is full, which looks like a stutter.
pub fn window_len(points: usize) -> usize {
    points.max(2)
}

/// Minutes covered by the chart, for the "Last N min" captions.
pub fn span_minutes(points: usize) -> usize {
    window_len(points).div_ceil(60).max(1)
}

thread_local! {
    /// Smoothed `client clock - server clock` in ms, learned from live samples.
    static CLOCK: std::cell::Cell<Option<f64>> = const { std::cell::Cell::new(None) };
}

/// A sample with server time `ts_ms` arrived now: refine the clock offset. Samples that arrive
/// early pull the offset quickly, late ones (network jitter, history backfill) barely move it.
pub fn note_sample(ts_ms: u64) {
    let d = js_sys::Date::now() - ts_ms as f64;
    CLOCK.with(|c| {
        let next = match c.get() {
            None => d,
            Some(off) if d < off => off + 0.5 * (d - off),
            Some(off) if d > off + 3000.0 => off,
            Some(off) => off + 0.05 * (d - off),
        };
        c.set(Some(next));
    });
}

/// Current server time as estimated from the samples.
fn server_now() -> f64 {
    js_sys::Date::now() - CLOCK.with(|c| c.get()).unwrap_or(0.0)
}

/// Snapshot taken when a sample arrives; frames only redraw it at a new time.
#[derive(Default)]
struct Snap {
    ts: Vec<f64>,
    data: Vec<Vec<f32>>,
    colors: Vec<String>,
    grid: String,
    label: String,
    /// prefers-reduced-motion: no scrolling, the newest sample sits on the right edge.
    still: bool,
    /// Size of the canvas in CSS pixels and the device pixel ratio, read with each new sample
    /// or layout change: never in the animation loop, where each read forces a layout.
    w: f64,
    h: f64,
    dpr: f64,
}

impl Snap {
    /// Sample spacing in ms: the median of the recent gaps, rounded to 10 ms (1 s when unknown).
    /// Stable from one sample to the next, so the horizontal scale never twitches.
    fn step_ms(&self) -> f64 {
        // the newest gaps: the server samples every 10 s while nobody watches and every second
        // once a dashboard listens, so the step follows within a few seconds
        let mut gaps: Vec<f64> = self.ts.windows(2).rev().take(9).map(|w| w[1] - w[0]).collect();
        if gaps.is_empty() {
            return 1000.0;
        }
        gaps.sort_by(f64::total_cmp);
        ((gaps[gaps.len() / 2] / 10.0).round() * 10.0).clamp(250.0, 60_000.0)
    }
}

/// Tangents of the monotone cubic through `pts` (x increasing), Fritsch–Carlson: zero at local
/// extremes, a weighted harmonic mean of the neighbouring slopes elsewhere, the one-sided slope
/// at both ends. Each piece then stays between its two points: the curve passes through every
/// sample and never overshoots above a peak or below the baseline. The tangent at a point
/// depends on its two neighbours only, so a new sample reshapes the last piece alone.
pub fn tangents(pts: &[(f64, f64)]) -> Vec<f64> {
    let n = pts.len();
    if n < 2 {
        return vec![0.0; n];
    }
    let d: Vec<f64> = pts
        .windows(2)
        .map(|w| {
            let h = w[1].0 - w[0].0;
            if h > 0.0 { (w[1].1 - w[0].1) / h } else { 0.0 }
        })
        .collect();
    let mut m = vec![0.0; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for k in 1..n - 1 {
        let (d0, d1) = (d[k - 1], d[k]);
        if d0 * d1 <= 0.0 {
            continue;
        }
        let (h0, h1) = (pts[k].0 - pts[k - 1].0, pts[k + 1].0 - pts[k].0);
        let (w1, w2) = (2.0 * h1 + h0, h1 + 2.0 * h0);
        m[k] = (w1 + w2) / (w1 / d0 + w2 / d1);
    }
    m
}

/// Bézier control points of piece `k` (from `pts[k]` to `pts[k + 1]`) for tangents `m`.
pub fn controls(pts: &[(f64, f64)], m: &[f64], k: usize) -> [(f64, f64); 2] {
    let ((x0, y0), (x1, y1)) = (pts[k], pts[k + 1]);
    let h = (x1 - x0) / 3.0;
    [(x0 + h, y0 + m[k] * h), (x1 - h, y1 - m[k + 1] * h)]
}

/// Smooth path through the points: monotone cubic with the tangents `m` ([`tangents`]).
fn trace(ctx: &CanvasRenderingContext2d, pts: &[(f64, f64)], m: &[f64]) {
    let Some(&(x0, y0)) = pts.first() else { return };
    ctx.move_to(x0, y0);
    for k in 0..pts.len() - 1 {
        let [(ax, ay), (bx, by)] = controls(pts, m, k);
        let (x, y) = pts[k + 1];
        ctx.bezier_curve_to(ax, ay, bx, by, x, y);
    }
}

/// Height at `x` of the curve [`trace`] draws through `pts` with tangents `m` (clamped to the
/// points). The control points sit at a third and two thirds of each piece's width, so x is
/// linear in the Bézier parameter and y follows directly.
pub fn curve_y(pts: &[(f64, f64)], m: &[f64], x: f64) -> f64 {
    let n = pts.len();
    if n < 2 {
        return pts.first().map_or(0.0, |p| p.1);
    }
    let mut k = n - 2;
    while k > 0 && pts[k].0 > x {
        k -= 1;
    }
    let ((x0, y0), (x1, y1)) = (pts[k], pts[k + 1]);
    if x1 <= x0 {
        return y1;
    }
    let u = ((x - x0) / (x1 - x0)).clamp(0.0, 1.0);
    let [(_, c0), (_, c1)] = controls(pts, m, k);
    let v = 1.0 - u;
    v * v * v * y0 + 3.0 * v * v * u * c0 + 3.0 * v * u * u * c1 + u * u * u * y1
}

/// Peak of the samples drawn (NaN and negative values count as 0).
fn peak(snap: &Snap) -> f32 {
    snap.data.iter().flatten().fold(0f32, |a, &b| if b > a { b } else { a })
}

/// Top of the scale: fixed, or the 1-2-5 rounding above the peak with hysteresis around the
/// current top `cur` (0 = none yet). It grows as soon as the peak gets within 10 % of the top,
/// and shrinks only once the peak fell under 40 % of it, so the axis does not breathe with
/// every sample nor bounce when a spike scrolls out.
pub fn next_top(peak: f32, cur: f32, fixed: Option<f32>, binary: bool) -> f32 {
    if let Some(m) = fixed {
        return m;
    }
    let want = if binary { nice_binary(peak * 1.1) } else { nice(peak * 1.1) };
    if cur <= 0.0 || want > cur || peak < cur * 0.4 { want } else { cur }
}

/// Animated scale: eases from `from` to `to` over [`SCALE_MS`] starting at `start` (ms).
#[derive(Clone, Copy, Default)]
struct Scale {
    from: f32,
    to: f32,
    start: f64,
}

const SCALE_MS: f64 = 600.0;

impl Scale {
    fn at(self, now: f64) -> f32 {
        let p = ((now - self.start) / SCALE_MS).clamp(0.0, 1.0);
        let e = 1.0 - (1.0 - p).powi(3);
        self.from + (self.to - self.from) * e as f32
    }
    fn moving(self, now: f64) -> bool {
        self.from != self.to && now - self.start < SCALE_MS
    }
    /// New target: start from the value shown now (no jump when it changes mid-animation).
    fn retarget(&mut self, to: f32, now: f64, animate: bool) {
        if to == self.to && self.to > 0.0 {
            return;
        }
        self.from = if animate && self.to > 0.0 { self.at(now) } else { to };
        self.to = to;
        self.start = now;
    }
}

/// Room for the head (radius 3.5 and a 6 px halo ring around it) at the top and right edges.
const HALO: f64 = 7.0;

/// Draw the snapshot with the head of the curve at server time `t` (ms) and the scale top at
/// `top` (eased). The mini scale on the right shows the values of the top and half lines for
/// `label_top`, where the scale is going: a round value.
fn draw(canvas: &HtmlCanvasElement, series: &[Series], snap: &Snap, o: Opts, t: f64, top: f32, label_top: f32) {
    let (cw, ch, dpr) = (snap.w, snap.h, snap.dpr);
    if cw <= 0.0 || ch <= 0.0 {
        return;
    }
    let (bw, bh) = ((cw * dpr).round() as u32, (ch * dpr).round() as u32);
    if canvas.width() != bw || canvas.height() != bh {
        canvas.set_width(bw);
        canvas.set_height(bh);
    }
    let Some(ctx) = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into::<CanvasRenderingContext2d>().ok())
    else {
        return;
    };
    let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
    ctx.clear_rect(0.0, 0.0, cw, ch);

    let top = top.max(f32::MIN_POSITIVE);
    let labels = o.label.map(|f| [f(label_top), f(label_top * 0.5)]);
    ctx.set_font("500 10.5px Inter, system-ui, sans-serif");
    // the plot ends where the head is; the labels sit right of it, in 8 px steps so the plot
    // does not change width with every new label
    let right = match &labels {
        Some(l) => {
            let w = l.iter().filter_map(|s| ctx.measure_text(s).ok()).fold(0.0, |a: f64, m| a.max(m.width()));
            ((w + 10.0) / 8.0).ceil() * 8.0
        }
        None => HALO,
    };
    let pw = (cw - right).max(1.0);
    let base = ch - 2.0;
    let h = (base - HALO).max(1.0);
    let line_y = |f: f64| (base - h * f).round() + 0.5;

    if o.grid {
        ctx.set_stroke_style_str(&snap.grid);
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(0.0, line_y(0.0));
        ctx.line_to(pw, line_y(0.0));
        ctx.stroke();
        let _ = ctx.set_line_dash(&js_sys::Array::of2(&2.into(), &4.into()));
        ctx.begin_path();
        for f in [1.0, 0.5] {
            ctx.move_to(0.0, line_y(f));
            ctx.line_to(pw, line_y(f));
        }
        ctx.stroke();
        let _ = ctx.set_line_dash(&js_sys::Array::new());
    }

    let span = (window_len(o.points) - 1) as f64 * snap.step_ms();
    let mut heads = Vec::with_capacity(series.len());
    ctx.save();
    ctx.begin_path();
    // past the head is the stretch to the next sample: it shows as the head gets there
    ctx.rect(0.0, 0.0, pw, ch);
    ctx.clip();
    for ((s, vals), color) in series.iter().zip(&snap.data).zip(&snap.colors) {
        let n = vals.len().min(snap.ts.len());
        if n < 2 {
            continue;
        }
        let ts = &snap.ts[snap.ts.len() - n..];
        let pts: Vec<(f64, f64)> = ts
            .iter()
            .zip(&vals[vals.len() - n..])
            // clamped to the plot: never below the baseline, never above the top
            .map(|(&x, &v)| (pw - (t - x) / span * pw, base - (clamp_value(v, top) / top) as f64 * h))
            .collect();
        let m = tangents(&pts);
        let (x0, y0) = pts[0];
        let xn = pts[n - 1].0;
        if s.fill {
            let grad = ctx.create_linear_gradient(0.0, HALO, 0.0, base);
            let _ = grad.add_color_stop(0.0, &hex_rgba(color, 0.38));
            let _ = grad.add_color_stop(1.0, &hex_rgba(color, 0.03));
            ctx.begin_path();
            trace(&ctx, &pts, &m);
            ctx.line_to(xn, base);
            ctx.line_to(x0, base);
            ctx.close_path();
            ctx.set_fill_style_canvas_gradient(&grad);
            ctx.fill();
        }
        ctx.begin_path();
        trace(&ctx, &pts, &m);
        ctx.set_stroke_style_str(color);
        ctx.set_line_width(2.0);
        ctx.set_line_join("round");
        ctx.set_line_cap("round");
        ctx.stroke();
        // the series starts inside the chart (just after start-up): mark where data begins
        if x0 > 4.0 {
            ctx.begin_path();
            let _ = ctx.arc(x0, y0, 2.5, 0.0, std::f64::consts::TAU);
            ctx.set_fill_style_str(color);
            ctx.fill();
        }
        let hx = xn.min(pw);
        heads.push((hx, curve_y(&pts, &m, hx), color));
    }
    ctx.restore();

    // the heads, on the curve at the right edge of the plot: a dot in a soft halo
    for (x, y, color) in heads {
        ctx.begin_path();
        let _ = ctx.arc(x, y, 3.5, 0.0, std::f64::consts::TAU);
        ctx.set_fill_style_str(color);
        ctx.fill();
        ctx.set_stroke_style_str(&hex_rgba(color, 0.35));
        ctx.set_line_width(6.0);
        ctx.stroke();
    }

    if let Some(l) = labels {
        ctx.set_fill_style_str(&snap.label);
        ctx.set_text_baseline("middle");
        for (s, f) in l.iter().zip([1.0, 0.5]) {
            let _ = ctx.fill_text(s, pw + 8.0, line_y(f));
        }
        ctx.set_text_baseline("alphabetic");
    }
}

/// A sample as plotted: inside 0..=top (NaN as 0).
fn clamp_value(v: f32, top: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(0.0, top) }
}

/// Server time shown at the right edge, 2 samples behind the clock and never past the
/// second-newest sample. The monotone cubic piece before that sample already has both of its
/// tangents (they need the neighbours on both sides), so a new sample only reshapes the piece
/// off-screen: what is visible never changes shape, it just scrolls in at constant speed.
/// Static (newest sample on the right edge) with reduced motion.
fn edge_time(snap: &Snap) -> f64 {
    let Some(&last) = snap.ts.last() else { return 0.0 };
    let step = snap.step_ms();
    if snap.still {
        return last;
    }
    (server_now() - 2.0 * step).clamp(last - 3.0 * step, last - step)
}

/// How long after a chart appears its animation loop starts (ms): under the first page's
/// requests and layouts, canvases drawn 60 times a second are what the main thread can least afford.
const LOOP_START_MS: u64 = 1200;

/// What the animation loop of a [`LiveChart`] reads: handles, so it is `Copy`.
#[derive(Clone, Copy)]
struct Loop {
    series: StoredValue<Vec<Series>>,
    snap: StoredValue<Snap>,
    scale: StoredValue<Scale>,
    running: StoredValue<bool>,
    /// The canvas is in the viewport (IntersectionObserver).
    visible: StoredValue<bool>,
    /// The loop ran before: a restart (back in view, tab shown again) begins at once.
    started: StoredValue<bool>,
    opts: Opts,
}

impl Loop {
    /// Draw the current snapshot once.
    fn draw_now(self, canvas: &HtmlCanvasElement) {
        let Some(sc) = self.scale.try_get_value() else { return };
        let now = js_sys::Date::now();
        self.series.with_value(|ss| self.snap.with_value(|sn| draw(canvas, ss, sn, self.opts, edge_time(sn), sc.at(now), sc.to)));
    }

    /// Start the animation loop unless it runs already, the chart is out of view, the tab is
    /// hidden or the user prefers reduced motion. It stops by itself on any of these.
    fn kick(self, canvas: HtmlCanvasElement) {
        let still = self.snap.try_with_value(|s| s.still).unwrap_or(true);
        if still || self.running.try_get_value().unwrap_or(true) || !self.visible.get_value() || document().hidden() {
            return;
        }
        self.running.set_value(true);
        if self.started.get_value() {
            request_animation_frame(move || frame(canvas, self, 0.0));
        } else {
            self.started.set_value(true);
            // the page is still arriving (requests, layouts): the loop starts once that is done;
            // until then each new sample draws through the chart's effect
            set_timeout(move || frame(canvas, self, 0.0), std::time::Duration::from_millis(LOOP_START_MS));
        }
    }
}

fn frame(canvas: HtmlCanvasElement, l: Loop, last: f64) {
    // the component is gone (stored values disposed), the canvas left the page or the viewport,
    // the tab is hidden or the user asked for reduced motion: stop (a new sample, or the chart
    // coming back into view, starts the loop again)
    if !canvas.is_connected()
        || l.snap.try_with_value(|sn| sn.still).unwrap_or(true)
        || !l.visible.try_get_value().unwrap_or(false)
        || document().hidden()
    {
        l.running.try_set_value(false);
        return;
    }
    // not laid out yet when the sample arrived (just mounted): measure until it is
    if l.snap.with_value(|sn| sn.w <= 0.0) {
        let (w, h) = (canvas.client_width() as f64, canvas.client_height() as f64);
        l.snap.update_value(|sn| {
            sn.w = w;
            sn.h = h;
            sn.dpr = window().device_pixel_ratio().max(1.0);
        });
    }
    let now = js_sys::Date::now();
    let mut drawn = last;
    let due = l.snap.with_value(|sn| {
        let span = (window_len(l.opts.points) - 1) as f64 * sn.step_ms();
        let px_per_ms = sn.w * sn.dpr / span;
        // at least 30 fps: steep pieces move vertically much more than the scroll itself, so a
        // slower rate shows steps; 60 fps only when the scroll is fast
        (0.25 / px_per_ms.max(1e-9)).clamp(16.0, 33.0)
    });
    // a rescale runs at the display frame rate
    let due = if l.scale.get_value().moving(now) { 0.0 } else { due };
    if now - last >= due {
        l.draw_now(&canvas);
        drawn = now;
    }
    request_animation_frame(move || frame(canvas, l, drawn));
}

/// A live chart bound to the metrics history. It scrolls continuously and its head rides the
/// curve into each new sample (requestAnimationFrame at 30-60 fps, only while the chart is in
/// view and the tab visible); static with reduced motion.
#[component]
pub fn LiveChart(
    series: Vec<Series>,
    #[prop(optional)] max: Option<f32>,
    #[prop(default = 300)] points: usize,
    #[prop(default = true)] grid: bool,
    #[prop(optional)] label: Option<fn(f32) -> String>,
    /// The values are bytes (see [`Opts::binary`]).
    #[prop(optional)] binary: bool,
    #[prop(default = "chart")] class: &'static str,
    /// Accessible description of the chart.
    aria: Signal<String>,
) -> impl IntoView {
    use wasm_bindgen::closure::Closure;

    let live = use_live();
    let ui = use_ui();
    let node = NodeRef::<Canvas>::new();
    let l = Loop {
        series: StoredValue::new(series),
        snap: StoredValue::new(Snap::default()),
        scale: StoredValue::new(Scale::default()),
        running: StoredValue::new(false),
        visible: StoredValue::new(true),
        started: StoredValue::new(false),
        opts: Opts { max, points, grid, label, binary },
    };
    let (series, snap, scale, opts) = (l.series, l.snap, l.scale, l.opts);
    // the theme's colours (for `dark`): reading them forces a style recalculation of the page,
    // so once per theme and not once per sample
    let theme = StoredValue::new(None::<(bool, Vec<String>, String, String)>);
    type Watch = (web_sys::IntersectionObserver, Closure<dyn Fn(js_sys::Array)>);
    let watch = StoredValue::new_local(None::<Watch>);
    on_cleanup(move || {
        if let Some(Some((ob, _))) = watch.try_update_value(Option::take) {
            ob.disconnect();
        }
    });

    // new sample, theme or layout change: refresh the snapshot and redraw once
    Effect::new(move || {
        live.tick.track();
        let dark = ui.dark.get();
        ui.layout_tick.track();
        ui.sidebar_collapsed.track();
        let Some(canvas) = node.get() else { return };
        if watch.with_value(Option::is_none) {
            // out of view (scrolled away, a collapsed card): no frames at all until it is back
            let c = canvas.clone();
            let cb = Closure::<dyn Fn(js_sys::Array)>::new(move |entries: js_sys::Array| {
                let Some(on) = entries.iter().last().and_then(|e| e.dyn_into::<web_sys::IntersectionObserverEntry>().ok()).map(|e| e.is_intersecting()) else {
                    return;
                };
                if l.visible.try_update_value(|v| *v = on).is_some() && on {
                    l.draw_now(&c);
                    l.kick(c.clone());
                }
            });
            if let Ok(ob) = web_sys::IntersectionObserver::new(cb.as_ref().unchecked_ref()) {
                ob.observe(&canvas);
                watch.set_value(Some((ob, cb)));
            }
        }
        if !theme.with_value(|t| t.as_ref().is_some_and(|t| t.0 == dark)) {
            let style = window().get_computed_style(&canvas).ok().flatten();
            let var = |name: &str| style.as_ref().and_then(|s| s.get_property_value(name).ok()).unwrap_or_default();
            let colors: Vec<String> = series.with_value(|ss| ss.iter().map(|s| var(s.color)).collect());
            // an empty value means the styles were not applied yet: ask again next time
            if colors.iter().all(|c| !c.is_empty()) {
                theme.set_value(Some((dark, colors, var("--chart-grid"), var("--text-subtle"))));
            }
        }
        let (colors, grid, label) = theme.with_value(|t| t.as_ref().map(|t| (t.1.clone(), t.2.clone(), t.3.clone()))).unwrap_or_default();
        let (w, h, dpr) = (canvas.client_width() as f64, canvas.client_height() as f64, window().device_pixel_ratio().max(1.0));
        let next = series.with_value(|ss| {
            live.history.with_value(|hist| {
                // two extra samples on the left: the line enters from the edge while scrolling and
                // the first visible piece already has both of its tangents
                let skip = hist.len().saturating_sub(points + 2);
                Snap {
                    ts: hist.iter().skip(skip).map(|m| m.ts_ms as f64).collect(),
                    data: ss.iter().map(|s| hist.iter().skip(skip).map(|m| (s.get)(m)).collect()).collect(),
                    colors,
                    grid,
                    label,
                    still: reduced_motion(),
                    w,
                    h,
                    dpr,
                }
            })
        });
        let now = js_sys::Date::now();
        let (top, still) = (next_top(peak(&next), scale.get_value().to, opts.max, opts.binary), next.still);
        scale.update_value(|s| s.retarget(top, now, !still));
        snap.set_value(next);
        // out of view: drawn when it comes back
        if l.visible.get_value() {
            l.draw_now(&canvas);
        }
        l.kick(canvas);
    });

    view! { <canvas node_ref=node class=class role="img" aria-label=aria></canvas> }
}

/// A still chart of series the page keeps itself (not the metrics history, e.g. one process's
/// CPU): drawn again when `data` changes, with the newest sample on the right edge and no
/// animation loop. `data`: timestamps (ms) and one value list per entry of `colors`.
#[component]
pub fn StillChart(
    colors: &'static [&'static str],
    data: Signal<(Vec<f64>, Vec<Vec<f32>>)>,
    #[prop(optional)] max: Option<f32>,
    #[prop(default = 150)] points: usize,
    #[prop(optional)] label: Option<fn(f32) -> String>,
    #[prop(optional)] binary: bool,
    aria: Signal<String>,
) -> impl IntoView {
    let ui = use_ui();
    let node = NodeRef::<Canvas>::new();
    let opts = Opts { max, points, grid: true, label, binary };
    let series = StoredValue::new(colors.iter().map(|c| Series::new(c, true, |_| 0.0)).collect::<Vec<_>>());
    let theme = StoredValue::new(None::<(bool, Vec<String>, String, String)>);
    let top = StoredValue::new(0f32);
    Effect::new(move || {
        let (ts, vals) = data.get();
        let dark = ui.dark.get();
        ui.layout_tick.track();
        let Some(canvas) = node.get() else { return };
        if !theme.with_value(|t| t.as_ref().is_some_and(|t| t.0 == dark)) {
            let style = window().get_computed_style(&canvas).ok().flatten();
            let var = |name: &str| style.as_ref().and_then(|s| s.get_property_value(name).ok()).unwrap_or_default();
            let cs: Vec<String> = colors.iter().map(|c| var(c)).collect();
            if cs.iter().all(|c| !c.is_empty()) {
                theme.set_value(Some((dark, cs, var("--chart-grid"), var("--text-subtle"))));
            }
        }
        let (colors, grid, label) = theme.with_value(|t| t.as_ref().map(|t| (t.1.clone(), t.2.clone(), t.3.clone()))).unwrap_or_default();
        let (w, h, dpr) = (canvas.client_width() as f64, canvas.client_height() as f64, window().device_pixel_ratio().max(1.0));
        let snap = Snap { ts, data: vals, colors, grid, label, still: true, w, h, dpr };
        let t = next_top(peak(&snap), top.get_value(), opts.max, opts.binary);
        top.set_value(t);
        let edge = snap.ts.last().copied().unwrap_or(0.0);
        series.with_value(|ss| draw(&canvas, ss, &snap, opts, edge, t, t));
    });
    view! { <canvas node_ref=node class="chart" role="img" aria-label=aria></canvas> }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points along the drawn curve (8 per piece), as the canvas would trace them.
    fn curve(pts: &[(f64, f64)]) -> Vec<(f64, f64)> {
        let m = tangents(pts);
        let mut out = vec![pts[0]];
        for k in 0..pts.len() - 1 {
            let [(ax, ay), (bx, by)] = controls(pts, &m, k);
            let ((x0, y0), (x1, y1)) = (pts[k], pts[k + 1]);
            for i in 1..=8 {
                let t = i as f64 / 8.0;
                let u = 1.0 - t;
                let b = |p0: f64, a: f64, b: f64, p1: f64| u * u * u * p0 + 3.0 * u * u * t * a + 3.0 * u * t * t * b + t * t * t * p1;
                out.push((b(x0, ax, bx, x1), b(y0, ay, by, y1)));
            }
        }
        out
    }

    #[test]
    fn monotone_cubic_never_overshoots() {
        // a lone spike on a flat zero baseline, uneven spacing, a step and a plateau
        let ys = [0.0, 0.0, 0.0, 100.0, 0.0, 0.0, 3.0, 50.0, 50.0, 49.0, 0.0, 80.0, 0.0];
        let xs = [0.0, 1.0, 2.0, 3.0, 4.5, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 10.2, 11.0];
        let pts: Vec<(f64, f64)> = xs.iter().copied().zip(ys).collect();
        let c = curve(&pts);
        for w in pts.windows(2) {
            let (lo, hi) = (w[0].1.min(w[1].1), w[0].1.max(w[1].1));
            for &(x, y) in c.iter().filter(|(x, _)| *x >= w[0].0 && *x <= w[1].0) {
                assert!(y >= lo - 1e-9 && y <= hi + 1e-9, "y {y} at x {x} outside {lo}..{hi}");
            }
        }
        // it goes through every sample
        for p in &pts {
            assert!(c.iter().any(|q| (q.0 - p.0).abs() < 1e-9 && (q.1 - p.1).abs() < 1e-9));
        }
    }

    #[test]
    fn head_rides_the_drawn_curve() {
        let pts: Vec<(f64, f64)> = [3.0, 7.0, 2.0, 9.0, 4.0].iter().enumerate().map(|(i, &y)| (i as f64 * 10.0, y)).collect();
        let m = tangents(&pts);
        // on every sample, and on the traced points in between
        for p in &pts {
            assert!((curve_y(&pts, &m, p.0) - p.1).abs() < 1e-9);
        }
        for q in curve(&pts) {
            assert!((curve_y(&pts, &m, q.0) - q.1).abs() < 1e-9, "at x {}", q.0);
        }
        // outside the points: clamped to the ends
        assert_eq!(curve_y(&pts, &m, 99.0), 4.0);
        assert_eq!(curve_y(&pts, &m, -5.0), 3.0);
        assert_eq!(curve_y(&pts[..1], &m, 5.0), 3.0);
    }

    #[test]
    fn new_sample_only_reshapes_the_last_piece() {
        let a: Vec<(f64, f64)> = [3.0, 7.0, 2.0, 9.0, 4.0].iter().enumerate().map(|(i, &y)| (i as f64, y)).collect();
        let mut b = a.clone();
        b.push((5.0, 0.0));
        let (ma, mb) = (tangents(&a), tangents(&b));
        // every piece but the one ending at the old last point keeps its control points
        for k in 0..a.len() - 2 {
            assert_eq!(controls(&a, &ma, k), controls(&b, &mb, k));
        }
    }

    #[test]
    fn scale_hysteresis() {
        // first value: the 1-2-5 rounding above the peak
        assert_eq!(next_top(7.0, 0.0, None, false), 10.0);
        // a little lower or higher: unchanged
        assert_eq!(next_top(5.0, 10.0, None, false), 10.0);
        assert_eq!(next_top(8.5, 10.0, None, false), 10.0);
        // near the top: grows right away
        assert_eq!(next_top(9.5, 10.0, None, false), 20.0);
        // a spike scrolled out: shrinks only when well below
        assert_eq!(next_top(4.5, 10.0, None, false), 10.0);
        assert_eq!(next_top(3.0, 10.0, None, false), 5.0);
        // fixed scales stay fixed
        assert_eq!(next_top(250.0, 100.0, Some(100.0), false), 100.0);
        // bytes: round in MiB, not in decimal units
        assert_eq!(next_top(90.0 * 1048576.0, 0.0, None, true), 100.0 * 1048576.0);
        assert_eq!(next_top(700.0, 0.0, None, true), 1000.0);
        assert_eq!(clamp_value(-3.0, 100.0), 0.0);
        assert_eq!(clamp_value(f32::NAN, 100.0), 0.0);
        assert_eq!(clamp_value(130.0, 100.0), 100.0);
    }
}
