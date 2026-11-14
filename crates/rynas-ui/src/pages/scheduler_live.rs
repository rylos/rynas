//! Live output of a running scheduled task (issue #12): the view the history drawer shows for
//! the run going on now, timer runs included. It follows
//! `GET /api/v1/scheduler/tasks/{id}/live` (SSE: `run`, `lines`, `end`, `failed`).
//!
//! - Each journal entry is one line; a carriage return rewrites the line (`rsync -P`, `curl`),
//!   so only what follows the last one is shown ([`shown_line`]), without terminal escape
//!   sequences.
//! - What arrives is appended as one text node per event; the oldest nodes go once the view
//!   holds more than [`KEEP_LINES`] lines or [`KEEP_BYTES`] ([`Kept`]): the history keeps the
//!   whole end of the output anyway.
//! - Follow scrolls to the newest line; long lines wrap (no horizontal scroll).
//! - A dropped connection is closed (the browser would reconnect and repeat the backlog) and
//!   opened again after a pause that grows; the view starts again from the backlog.
//!
//! The stream (and the server's `journalctl`) is closed when the view goes away.

use std::collections::VecDeque;
use std::time::Duration;

use leptos::prelude::*;
use rynas_api_types::scheduler::{LiveEnd, LiveRun, live_event};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{EventSource, MessageEvent};

use crate::form::switch;
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};
use crate::ui::{self, Tone};

/// Bytes of output kept in the view.
pub const KEEP_BYTES: usize = 512 * 1024;
/// Lines kept in the view.
pub const KEEP_LINES: usize = 5000;
/// A stream that stayed up this long resets the reconnection pause.
const STABLE_MS: f64 = 30_000.0;

/// What a terminal leaves on screen of one output line: the text after the last carriage
/// return that is followed by something (a trailing `\r` leaves the line as it was), without
/// escape sequences (colors, cursor moves) and other control characters (tabs stay).
pub fn shown_line(raw: &str) -> String {
    let last = raw.split('\r').rev().find(|s| !s.is_empty()).unwrap_or("");
    let mut out = String::with_capacity(last.len());
    let mut chars = last.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters and intermediates, then one final byte @..~
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ESC \
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // two-character sequences (ESC 7, ESC =, ...)
                _ => {}
            },
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Sizes of the text nodes in the view, oldest first, and whether some were dropped.
#[derive(Debug, Default)]
pub struct Kept {
    chunks: VecDeque<(usize, usize)>,
    bytes: usize,
    lines: usize,
    pub dropped: bool,
}

impl Kept {
    /// A new node of `bytes` and `lines`; how many of the oldest nodes to remove to stay
    /// within [`KEEP_BYTES`] and [`KEEP_LINES`] (never the newest one).
    pub fn push(&mut self, bytes: usize, lines: usize) -> usize {
        self.chunks.push_back((bytes, lines));
        self.bytes += bytes;
        self.lines += lines;
        let mut gone = 0;
        while (self.bytes > KEEP_BYTES || self.lines > KEEP_LINES) && self.chunks.len() > 1 {
            let (b, l) = self.chunks.pop_front().unwrap_or_default();
            self.bytes -= b;
            self.lines -= l;
            gone += 1;
        }
        if gone > 0 {
            self.dropped = true;
        }
        gone
    }

    #[cfg(test)]
    pub fn lines(&self) -> usize {
        self.lines
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Connecting,
    Live,
    /// The run ended.
    Ended,
    /// Nothing was running.
    Idle,
    /// The connection dropped (a new one follows).
    Lost,
    /// The server could not read the output.
    Failed,
}

/// Data of a `failed` event.
#[derive(serde::Deserialize)]
struct Failed {
    #[serde(default)]
    message: String,
}

/// The open stream and its callbacks, dropped together (closing the stream).
struct Stream {
    es: EventSource,
    _on_run: Closure<dyn Fn(MessageEvent)>,
    _on_lines: Closure<dyn Fn(MessageEvent)>,
    _on_end: Closure<dyn Fn(MessageEvent)>,
    _on_err: Closure<dyn Fn()>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.es.close();
    }
}

/// The live output of task `id`. `on_run`: the run followed (its start); `on_end`: the run
/// ended (`true`) or nothing was running (`false`).
pub fn live_view(id: &str, on_run: Callback<LiveRun>, on_end: Callback<bool>) -> AnyView {
    let url = format!("/api/v1/scheduler/tasks/{}/live", crate::api::encode(id));
    let follow = RwSignal::new(true);
    let state = RwSignal::new(State::Connecting);
    let failure = RwSignal::new(String::new());
    let dropped = RwSignal::new(false);
    let pre = NodeRef::<leptos::html::Pre>::new();
    let kept = StoredValue::new_local(Kept::default());
    let stream = StoredValue::new_local(None::<Stream>);
    // reconnections in a row since the stream last stayed up for STABLE_MS
    let retries = StoredValue::new(0u32);
    // when the current stream sent `run` (ms since the epoch, 0: not yet)
    let connected_at = StoredValue::new(0f64);
    let close = move || {
        set_timeout(move || { let _ = stream.try_update_value(|s| *s = None); }, Duration::ZERO);
    };
    let to_end = move || {
        if follow.get_untracked()
            && let Some(p) = pre.get_untracked()
        {
            p.set_scroll_top(p.scroll_height());
        }
    };
    let append = move |lines: Vec<String>| {
        let Some(p) = pre.get_untracked() else { return };
        let mut text = String::new();
        for l in &lines {
            text.push_str(&shown_line(l));
            text.push('\n');
        }
        let _ = p.append_with_str_1(&text);
        let gone = kept.try_update_value(|k| k.push(text.len(), lines.len())).unwrap_or(0);
        for _ in 0..gone {
            if let Some(first) = p.first_child() {
                let _ = p.remove_child(&first);
            }
        }
        if gone > 0 && !dropped.get_untracked() {
            dropped.set(true);
        }
        to_end();
    };
    let open = move || {
        stream.set_value(None);
        kept.set_value(Kept::default());
        dropped.set(false);
        if let Some(p) = pre.get_untracked() {
            p.set_text_content(None);
        }
        let Ok(es) = EventSource::new(&url) else {
            state.set(State::Lost);
            return;
        };
        let on_run = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            let _ = connected_at.try_set_value(js_sys::Date::now());
            let _ = state.try_set(State::Live);
            if let Some(txt) = ev.data().as_string()
                && let Ok(r) = crate::api::decode::<LiveRun>(&txt)
            {
                on_run.run(r);
            }
        });
        let _ = es.add_event_listener_with_callback(live_event::RUN, on_run.as_ref().unchecked_ref());
        let on_lines = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(txt) = ev.data().as_string()
                && let Ok(lines) = crate::api::decode::<Vec<String>>(&txt)
            {
                append(lines);
            }
        });
        let _ = es.add_event_listener_with_callback(live_event::LINES, on_lines.as_ref().unchecked_ref());
        // the run ended, nothing ran, or the server could not read the output: no reconnect
        let on_end = Closure::<dyn Fn(MessageEvent)>::new(move |ev: MessageEvent| {
            let data = ev.data().as_string().unwrap_or_default();
            if ev.type_() == live_event::FAILED {
                let why = crate::api::decode::<Failed>(&data).map(|f| f.message).unwrap_or_default();
                let _ = failure.try_set(why);
                let _ = state.try_set(State::Failed);
            } else {
                let _ = retries.try_set_value(0);
                let ran = crate::api::decode::<LiveEnd>(&data).map(|e| e.ran).unwrap_or(true);
                let _ = state.try_set(if ran { State::Ended } else { State::Idle });
                on_end.run(ran);
            }
            close();
        });
        for name in [live_event::END, live_event::FAILED] {
            let _ = es.add_event_listener_with_callback(name, on_end.as_ref().unchecked_ref());
        }
        let on_err = Closure::<dyn Fn()>::new(move || {
            // the browser would reconnect by itself and repeat the backlog: closed here, then
            // opened again after 2, 4, 8 … 30 s (the view starts over)
            if matches!(state.try_get_untracked(), Some(State::Live | State::Connecting)) {
                let _ = state.try_set(State::Lost);
                // the pause grows unless the stream stayed up a while (a stream cut right after
                // it opened, e.g. evicted by newer ones, must not reconnect every 2 s)
                let up = connected_at.try_get_value().unwrap_or(0.0);
                if up > 0.0 && js_sys::Date::now() - up >= STABLE_MS {
                    let _ = retries.try_set_value(0);
                }
                let _ = connected_at.try_set_value(0.0);
                let n = retries.try_update_value(|r| { *r = r.saturating_add(1); *r }).unwrap_or(1);
                let wait = Duration::from_secs((1u64 << n.min(5)).min(30));
                set_timeout(move || {
                    if state.try_get_untracked() == Some(State::Lost) {
                        let _ = state.try_set(State::Connecting);
                    }
                }, wait);
            }
            close();
        });
        es.set_onerror(Some(on_err.as_ref().unchecked_ref()));
        stream.set_value(Some(Stream { es, _on_run: on_run, _on_lines: on_lines, _on_end: on_end, _on_err: on_err }));
    };
    // opens on mount and again after a pause or Retry (state back to Connecting)
    Effect::new(move |_| {
        if state.get() == State::Connecting && stream.with_value(Option::is_none) {
            open();
        }
    });
    Effect::new(move |_| {
        if follow.get() {
            to_end();
        }
    });
    on_cleanup(move || {
        let _ = stream.try_update_value(|s| *s = None);
    });
    let status = move || {
        let (tone, text) = match state.get() {
            State::Connecting => (Tone::Unknown, t("sch.live.connecting").to_string()),
            State::Live => (Tone::Info, t("sch.live.following").to_string()),
            State::Ended => (Tone::Unknown, t("sch.live.ended").to_string()),
            State::Idle => (Tone::Unknown, t("sch.live.idle").to_string()),
            State::Lost => (Tone::Warn, t("dk.log.lost").to_string()),
            // told by the error box below, with Retry
            State::Failed => (Tone::Err, t("sch.run.failed").to_string()),
        };
        ui::status(tone, text)
    };
    let failed_box = move || {
        (state.get() == State::Failed).then(|| {
            ui::error_box_text(tf("sch.live.failed", &[("why", &failure.get())]), Callback::new(move |_| state.set(State::Connecting)))
        })
    };
    view! {
        <div class="live-tools">
            {switch("sch.live.follow", None, follow.into(), Callback::new(move |v| follow.set(v)))}
            <span role="status">{status}</span>
            <button type="button" class="btn ghost sm" hidden=move || state.get() != State::Lost
                on:click=move |_| state.set(State::Connecting)>
                <Icon icon=icons::REFRESH_CW size=14 />{tr("err.retry")}
            </button>
        </div>
        {failed_box}
        <p class="field-help">{tr("sch.live.cr_note")}</p>
        <Show when=move || dropped.get()>
            <p class="field-help">{tr("sch.live.dropped")}</p>
        </Show>
        <pre class="log run-log live-log" node_ref=pre tabindex="0" aria-label=tr("sch.live") aria-live="off" data-empty=tr("sch.live.waiting")></pre>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriage_returns_rewrite_the_line() {
        assert_eq!(shown_line("plain line"), "plain line");
        // rsync -P as journald stores it: every update of the file in one entry
        assert_eq!(
            shown_line("     1,048,576  25%   41.20MB/s    0:00:02\r     4,194,304 100%   41.20MB/s    0:00:00 (xfr#1, to-chk=51/52)"),
            "     4,194,304 100%   41.20MB/s    0:00:00 (xfr#1, to-chk=51/52)"
        );
        // a trailing CR (curl) leaves the last text
        assert_eq!(shown_line("  10  1.2M\r 100  12M\r"), " 100  12M");
        assert_eq!(shown_line("\r\r"), "");
        assert_eq!(shown_line(""), "");
    }

    #[test]
    fn escape_sequences_and_controls_go() {
        assert_eq!(shown_line("\u{1b}[1;31mError\u{1b}[0m: disk full"), "Error: disk full");
        assert_eq!(shown_line("\u{1b}]0;title\u{7}after"), "after");
        assert_eq!(shown_line("\u{1b}]8;;http://x\u{1b}\\link"), "link");
        assert_eq!(shown_line("a\tb\u{8}c\u{0}"), "a\tbc");
        assert_eq!(shown_line("caf\u{fffd}.png"), "caf\u{fffd}.png");
        assert_eq!(shown_line("\u{1b}[2K\rdone"), "done");
    }

    #[test]
    fn keeps_the_newest_lines() {
        let mut k = Kept::default();
        assert_eq!(k.push(100, 10), 0);
        assert!(!k.dropped);
        // over the line cap: the oldest nodes go, the newest stays
        assert_eq!(k.push(100, KEEP_LINES - 5), 1);
        assert!(k.dropped);
        assert_eq!(k.lines(), KEEP_LINES - 5);
        // one node over every cap is still shown
        assert_eq!(k.push(KEEP_BYTES * 2, 1), 1);
        assert_eq!(k.lines(), 1);
        assert_eq!(k.push(10, 1), 1);
        assert_eq!(k.lines(), 1);
    }
}
