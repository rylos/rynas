//! Server-sent events.
//!
//! - `GET /api/v1/metrics/stream`: `sample` every second (data `MetricsSample`).
//! - `GET /api/v1/tasks/stream`: passed through from rynasd ([`crate::proxy`]).
//! - `GET /api/v1/events`: all of them multiplexed on **one** connection per tab
//!   (guidelines A22: HTTP/1.1 browsers allow 6 connections per origin). Event names:
//!   `metrics.sample`, `task.started`, `task.updated`, `task.finished`, `config.pending`
//!   (data `PendingEvent`: some changeset may have changed, re-read `/config/pending` with
//!   `If-None-Match`), `notification`
//!   (data `NotificationEvent`), `hardware.status` (data `HardwareStatus`: the latest one on
//!   connect, then on every change and at least every [`crate::metrics::HW_REPEAT`]),
//!   `ups.status` (data `UpsStatus`, same rule with [`crate::ups::REPEAT`]; upsd is polled
//!   only while a stream listens), and
//!   `resync` with data `{"stream":"metrics"|"tasks"|"config"|"notifications"|"hardware"|"ups"}` when the
//!   client missed events and must re-read that collection. The task and notification halves follow rynasd's streams
//!   (`/v1/tasks/stream`, `/v1/notifications/stream`) and reconnect every [`TASKS_RETRY`]
//!   while rynasd is away; the metrics half never stops.
//!
//! Every stream ends with its session and does not renew the idle timeout; samples are
//! serialized once by the collector, task events once by rynasd. Keep-alive every 15 s.
//! Streams are capped per session and in total ([`crate::streams`]): over the session's
//! cap its oldest stream ends, over the global cap the answer is `429 rate_limited`.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::Extension;
use axum::extract::State;
use axum::http::Method;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::{Stream, StreamExt, stream};
use http_body_util::BodyExt;
use hyper::body::{Bytes, Incoming};
use tokio::sync::broadcast::error::RecvError;

use crate::app::AppState;
use crate::auth::CurrentSession;

/// Keep-alive comment interval of every stream.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// Pause before reconnecting the task half of `/events` to rynasd.
pub const TASKS_RETRY: Duration = Duration::from_secs(5);
/// Longest SSE frame accepted from rynasd (a task event is well under 4 KiB).
const MAX_FRAME: usize = 64 << 10;

type Events = std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

fn resync(stream: &str) -> Event {
    Event::default()
        .event("resync")
        .data(format!("{{\"stream\":\"{stream}\"}}"))
}

/// Metrics samples named `name`, starting with the latest one; `resync` after a lag.
fn metrics(st: &AppState, name: &'static str, resync_data: bool) -> Events {
    // subscribe first: at worst the first sample is sent twice, never skipped
    let rx = st.store.subscribe();
    let head = stream::iter(
        st.store
            .latest_json()
            .map(|j| Ok(Event::default().event(name).data(&*j))),
    );
    let tail = stream::unfold(rx, move |mut rx| async move {
        let ev = match rx.recv().await {
            Ok(j) => Event::default().event(name).data(&*j),
            // a slow client skipped samples: say so (the charts restart from the history)
            Err(RecvError::Lagged(_)) if resync_data => resync("metrics"),
            Err(RecvError::Lagged(_)) => Event::default().event("resync").data("{}"),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok(ev), rx))
    });
    Box::pin(head.chain(tail))
}

/// Hardware status changes as `hardware.status`, starting with the latest one; `resync`
/// after a lag.
fn hardware(st: &AppState) -> Events {
    let rx = st.store.subscribe_hardware();
    let head = stream::iter(
        st.store
            .latest_hardware_json()
            .map(|j| Ok(Event::default().event("hardware.status").data(&*j))),
    );
    let tail = stream::unfold(rx, |mut rx| async move {
        let ev = match rx.recv().await {
            Ok(j) => Event::default().event("hardware.status").data(&*j),
            Err(RecvError::Lagged(_)) => resync("hardware"),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok(ev), rx))
    });
    Box::pin(head.chain(tail))
}

/// UPS readings as `ups.status`, starting with the latest one; `resync` after a lag.
fn ups(st: &AppState) -> Events {
    let rx = st.ups.subscribe();
    let head = stream::iter(
        st.ups
            .latest_json()
            .map(|j| Ok(Event::default().event("ups.status").data(&*j))),
    );
    let tail = stream::unfold(rx, |mut rx| async move {
        let ev = match rx.recv().await {
            Ok(j) => Event::default().event("ups.status").data(&*j),
            Err(RecvError::Lagged(_)) => resync("ups"),
            Err(RecvError::Closed) => return None,
        };
        Some((Ok(ev), rx))
    });
    Box::pin(head.chain(tail))
}

/// Ends `events` when the session `id` ends (logout, idle or absolute timeout).
fn with_session(st: &AppState, id: rynas_auth::session::SessionId, events: Events) -> Events {
    let auth = st.auth.clone();
    Box::pin(events.take_while(move |_| std::future::ready(auth.sessions().is_valid(&id))))
}

/// `events` as the SSE answer, counted against the stream caps of [`crate::streams`]:
/// the session's oldest stream closes over its cap, 429 over the global one.
fn respond(st: &AppState, id: rynas_auth::session::SessionId, events: Events) -> Response {
    match st.streams.limit(id, with_session(st, id, events)) {
        Ok(limited) => Sse::new(limited)
            .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
            .into_response(),
        Err(full) => full.into_response(),
    }
}

pub async fn metrics_stream(
    State(st): State<AppState>,
    Extension(CurrentSession(session)): Extension<CurrentSession>,
) -> Response {
    respond(&st, session.id, metrics(&st, "sample", false))
}

pub async fn events(
    State(st): State<AppState>,
    Extension(CurrentSession(session)): Extension<CurrentSession>,
) -> Response {
    let user = session.user.username.clone();
    let local: Events = Box::pin(stream::select(
        metrics(&st, "metrics.sample", true),
        stream::select(hardware(&st), ups(&st)),
    ));
    let merged: Events = Box::pin(stream::select(
        local,
        stream::select(
            follow(
                st.clone(),
                user.clone(),
                "/v1/tasks/stream?config=1",
                task_event,
            ),
            follow(
                st.clone(),
                user,
                "/v1/notifications/stream",
                notification_event,
            ),
        ),
    ));
    respond(&st, session.id, merged)
}

/// One SSE frame of rynasd: its `event:` and `data:` fields.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub event: String,
    pub data: String,
}

/// Splits complete frames off the front of `buf` (frames end with a blank line; comments
/// and keep-alives are skipped).
pub fn take_frames(buf: &mut Vec<u8>) -> Vec<Frame> {
    let mut out = Vec::new();
    while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
        let block = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end + 2);
        let mut f = Frame {
            event: "message".into(),
            data: String::new(),
        };
        let mut any = false;
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("event:") {
                f.event = v.trim_start().to_string();
                any = true;
            } else if let Some(v) = line.strip_prefix("data:") {
                if !f.data.is_empty() {
                    f.data.push('\n');
                }
                f.data.push_str(v.strip_prefix(' ').unwrap_or(v));
                any = true;
            }
        }
        if any {
            out.push(f);
        }
    }
    out
}

/// rynasd's task events, renamed `task.<kind>`, and its `config_pending` events as
/// `config.pending`; a `resync` from rynasd or after a reconnect is a resync of both the
/// tasks and the pending changes.
fn task_event(f: Frame, out: &mut VecDeque<Event>) {
    match f.event.as_str() {
        "resync" => {
            out.push_back(resync("tasks"));
            out.push_back(resync("config"));
        }
        "config_pending" => out.push_back(Event::default().event("config.pending").data(f.data)),
        kind => out.push_back(Event::default().event(format!("task.{kind}")).data(f.data)),
    }
}

/// rynasd's notification changes as `notification`; `resync` from rynasd or after a
/// reconnect.
fn notification_event(f: Frame, out: &mut VecDeque<Event>) {
    out.push_back(match f.event.as_str() {
        "resync" => resync("notifications"),
        _ => Event::default().event("notification").data(f.data),
    });
}

/// Maps one rynasd frame to the events of `/events`.
type MapFrame = fn(Frame, &mut VecDeque<Event>);

enum Upstream {
    /// Not connected; `true` once a connection was lost (the next one starts with resync).
    Down(bool),
    Up(Incoming, Vec<u8>),
}

/// One rynasd half of `/events`: follows the SSE stream at `path` as `user`, renaming its
/// frames with `map`; after a lost connection the first events are those that `map` gives
/// for a `resync` frame.
fn follow(st: AppState, user: String, path: &'static str, map: MapFrame) -> Events {
    let s = stream::unfold(
        (Upstream::Down(false), VecDeque::<Event>::new()),
        move |(mut up, mut queue)| {
            let st = st.clone();
            let user = user.clone();
            async move {
                loop {
                    if let Some(ev) = queue.pop_front() {
                        return Some((Ok(ev), (up, queue)));
                    }
                    up = match up {
                        Upstream::Down(lost) => {
                            match st
                                .rynasd
                                .send(Method::GET, path, Some(&user), None, Bytes::new())
                                .await
                            {
                                Ok(r) if r.status().is_success() => {
                                    if lost {
                                        map(
                                            Frame {
                                                event: "resync".into(),
                                                data: String::new(),
                                            },
                                            &mut queue,
                                        );
                                    }
                                    Upstream::Up(r.into_body(), Vec::new())
                                }
                                _ => {
                                    tokio::time::sleep(TASKS_RETRY).await;
                                    Upstream::Down(lost)
                                }
                            }
                        }
                        Upstream::Up(mut body, mut buf) => match body.frame().await {
                            Some(Ok(frame)) => {
                                if let Ok(data) = frame.into_data() {
                                    buf.extend_from_slice(&data);
                                    for f in take_frames(&mut buf) {
                                        map(f, &mut queue);
                                    }
                                }
                                if buf.len() > MAX_FRAME {
                                    tracing::warn!(
                                        "rynasd stream {path}: frame too long, reconnecting"
                                    );
                                    Upstream::Down(true)
                                } else {
                                    Upstream::Up(body, buf)
                                }
                            }
                            // rynasd restarted or the stream broke
                            _ => {
                                tokio::time::sleep(TASKS_RETRY).await;
                                Upstream::Down(true)
                            }
                        },
                    };
                }
            }
        },
    );
    Box::pin(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        let mut buf = b"event: started\ndata: {\"a\":1}\n\n: keep-alive\n\nevent: upd".to_vec();
        let f = take_frames(&mut buf);
        assert_eq!(
            f,
            [Frame {
                event: "started".into(),
                data: "{\"a\":1}".into()
            }]
        );
        assert_eq!(buf, b"event: upd");
        buf.extend_from_slice(b"ated\ndata:x\n\n");
        let f = take_frames(&mut buf);
        assert_eq!(f[0].event, "updated");
        assert_eq!(f[0].data, "x");
        assert!(buf.is_empty());
    }
}
