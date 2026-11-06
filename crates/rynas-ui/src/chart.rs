//! Tiny canvas renderer for live time series (no JS chart library).
//!
//! Charts redraw only their `<canvas>`: no DOM re-render. Points are placed by their server
//! timestamp and the right edge follows the (estimated) server clock one sample behind, so the
//! series scrolls continuously instead of jumping once per sample; lines are smooth curves.
//! Colors come from CSS custom properties read with `getComputedStyle`, so they follow the
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

/// Samples always spanned by the x axis: with fewer (right after start) the chart shows the
/// last minute instead of a lone stub on the right of a 5 minute axis, and widens as data comes.
pub const MIN_WINDOW: usize = 60;

/// Samples across the width for `n` available samples and a full window of `points`.
pub fn window_len(n: usize, points: usize) -> usize {
    n.clamp(MIN_WINDOW.min(points), points.max(2))
}

/// Minutes covered by the chart, for the "Last N min" captions.
pub fn span_minutes(n: usize, points: usize) -> usize {
    window_len(n, points).div_ceil(60).max(1)
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
}

impl Snap {
    /// Sample spacing in ms: the median of the recent gaps, rounded to 10 ms (1 s when unknown).
    /// Stable from one sample to the next, so the horizontal scale never twitches.
    fn step_ms(&self) -> f64 {
        let mut gaps: Vec<f64> = self.ts.windows(2).rev().take(31).map(|w| w[1] - w[0]).collect();
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

/// Smooth path through the points: monotone cubic ([`tangents`]).
fn trace(ctx: &CanvasRenderingContext2d, pts: &[(f64, f64)]) {
    let Some(&(x0, y0)) = pts.first() else { return };
    ctx.move_to(x0, y0);
    let m = tangents(pts);
    for k in 0..pts.len() - 1 {
        let [(ax, ay), (bx, by)] = controls(pts, &m, k);
        let (x, y) = pts[k + 1];
        ctx.bezier_curve_to(ax, ay, bx, by, x, y);
    }
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

/// Draw the snapshot with the right edge of the chart at server time `t` (ms) and the scale
/// top at `top` (eased); the label shows `label_top`, where the scale is going: a round value.
fn draw(canvas: &HtmlCanvasElement, series: &[Series], snap: &Snap, o: Opts, t: f64, top: f32, label_top: f32) {
    let (cw, ch) = (canvas.client_width() as f64, canvas.client_height() as f64);
    if cw <= 0.0 || ch <= 0.0 {
        return;
    }
    let dpr = window().device_pixel_ratio().max(1.0);
    let (pw, ph) = ((cw * dpr).round() as u32, (ch * dpr).round() as u32);
    if canvas.width() != pw || canvas.height() != ph {
        canvas.set_width(pw);
        canvas.set_height(ph);
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
    let pad_top = if o.label.is_some() { 14.0 } else { 2.0 };
    let h = ch - pad_top - 1.0;

    if o.grid {
        ctx.set_stroke_style_str(&snap.grid);
        ctx.set_line_width(1.0);
        let _ = ctx.set_line_dash(&js_sys::Array::of2(&3.into(), &3.into()));
        for i in 0..4 {
            let y = (pad_top + h * i as f64 / 4.0).round() + 0.5;
            ctx.begin_path();
            ctx.move_to(0.0, y);
            ctx.line_to(cw, y);
            ctx.stroke();
        }
        let _ = ctx.set_line_dash(&js_sys::Array::new());
    }

    let n_max = snap.data.iter().map(Vec::len).max().unwrap_or(0);
    let span = (window_len(n_max, o.points) - 1) as f64 * snap.step_ms();
    ctx.save();
    ctx.begin_path();
    ctx.rect(0.0, 0.0, cw, ch);
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
            .map(|(&x, &v)| (cw - (t - x) / span * cw, pad_top + h - (clamp_value(v, top) / top) as f64 * h))
            .collect();
        let (x0, y0) = pts[0];
        let xn = pts[n - 1].0;
        if s.fill {
            let grad = ctx.create_linear_gradient(0.0, pad_top, 0.0, ch);
            let _ = grad.add_color_stop(0.0, &hex_rgba(color, 0.32));
            let _ = grad.add_color_stop(1.0, &hex_rgba(color, 0.0));
            ctx.begin_path();
            trace(&ctx, &pts);
            ctx.line_to(xn, ch);
            ctx.line_to(x0, ch);
            ctx.close_path();
            ctx.set_fill_style_canvas_gradient(&grad);
            ctx.fill();
        }
        ctx.begin_path();
        trace(&ctx, &pts);
        ctx.set_stroke_style_str(color);
        ctx.set_line_width(if o.grid { 1.75 } else { 1.5 });
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
    }
    ctx.restore();

    if let Some(fmt) = o.label {
        ctx.set_fill_style_str(&snap.label);
        ctx.set_font("500 11px Inter, system-ui, sans-serif");
        let _ = ctx.fill_text(&fmt(label_top), 2.0, 10.0);
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

/// A live chart bound to the metrics history. It scrolls continuously (requestAnimationFrame at
/// 30-60 fps, paused in hidden tabs); static with reduced motion.
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
    let live = use_live();
    let ui = use_ui();
    let node = NodeRef::<Canvas>::new();
    let opts = Opts { max, points, grid, label, binary };
    let series = StoredValue::new(series);
    let snap = StoredValue::new(Snap::default());
    let running = StoredValue::new(false);
    let scale = StoredValue::new(Scale::default());

    // new sample, theme or layout change: refresh the snapshot and redraw once
    Effect::new(move || {
        live.tick.track();
        ui.dark.track();
        ui.layout_tick.track();
        ui.sidebar_collapsed.track();
        let Some(canvas) = node.get() else { return };
        let style = window().get_computed_style(&canvas).ok().flatten();
        let var = |name: &str| style.as_ref().and_then(|s| s.get_property_value(name).ok()).unwrap_or_default();
        let next = series.with_value(|ss| {
            live.history.with_value(|h| {
                // two extra samples on the left: the line enters from the edge while scrolling and
                // the first visible piece already has both of its tangents
                let skip = h.len().saturating_sub(points + 2);
                Snap {
                    ts: h.iter().skip(skip).map(|m| m.ts_ms as f64).collect(),
                    data: ss.iter().map(|s| h.iter().skip(skip).map(|m| (s.get)(m)).collect()).collect(),
                    colors: ss.iter().map(|s| var(s.color)).collect(),
                    grid: var("--chart-grid"),
                    label: var("--text-subtle"),
                    still: reduced_motion(),
                }
            })
        });
        let now = js_sys::Date::now();
        let (top, still) = (next_top(peak(&next), scale.get_value().to, opts.max, opts.binary), next.still);
        scale.update_value(|s| s.retarget(top, now, !still));
        snap.set_value(next);
        let sc = scale.get_value();
        series.with_value(|ss| snap.with_value(|sn| draw(&canvas, ss, sn, opts, edge_time(sn), sc.at(now), sc.to)));
        if !running.get_value() && !still {
            running.set_value(true);
            frame(canvas, series, snap, scale, running, opts, 0.0);
        }
    });

    fn frame(
        canvas: HtmlCanvasElement,
        series: StoredValue<Vec<Series>>,
        snap: StoredValue<Snap>,
        scale: StoredValue<Scale>,
        running: StoredValue<bool>,
        o: Opts,
        last: f64,
    ) {
        // the component is gone (stored values disposed), the canvas left the page or the user
        // asked for reduced motion: stop (the next sample restarts the loop if needed)
        if !canvas.is_connected() || snap.try_with_value(|sn| sn.still).unwrap_or(true) {
            running.try_set_value(false);
            return;
        }
        let now = js_sys::Date::now();
        let mut drawn = last;
        if !document().hidden() {
            let due = snap
                .try_with_value(|sn| {
                    let n = sn.ts.len().max(2);
                    let span = (window_len(n, o.points) - 1) as f64 * sn.step_ms();
                    let px_per_ms = canvas.client_width() as f64 * window().device_pixel_ratio().max(1.0) / span;
                    // at least 30 fps: steep pieces move vertically much more than the scroll
                    // itself, so a slower rate shows steps; 60 fps only when the scroll is fast
                    (0.25 / px_per_ms.max(1e-9)).clamp(16.0, 33.0)
                })
                .unwrap_or(250.0);
            let sc = scale.try_get_value().unwrap_or_default();
            // a rescale runs at the display frame rate
            let due = if sc.moving(now) { 0.0 } else { due };
            if now - last >= due {
                series.with_value(|ss| snap.with_value(|sn| draw(&canvas, ss, sn, o, edge_time(sn), sc.at(now), sc.to)));
                drawn = now;
            }
        }
        request_animation_frame(move || frame(canvas, series, snap, scale, running, o, drawn));
    }

    view! { <canvas node_ref=node class=class role="img" aria-label=aria></canvas> }
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
