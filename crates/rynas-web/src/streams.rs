//! Caps on the long-lived event streams (`/events`, `/metrics/stream`, `/tasks/stream`):
//! guidelines M1. Each `/events` holds a TCP connection, a hyper task and two upstream
//! connections to rynasd, so a buggy client, a page kept in the back/forward cache or a
//! script with a stolen cookie must not be able to open them without bound.
//!
//! - **Per session**: at most [`PER_SESSION`] streams. A new stream over the cap closes the
//!   **oldest** stream of the same session instead of being refused: the oldest one is the
//!   one most likely abandoned (a page in the bfcache, a half-open TCP connection the server
//!   has not noticed yet), and refusing the newest would lock the visible page out. The
//!   value is the HTTP/1.1 connection limit of a browser per origin (6), so one browser
//!   never reaches the cap with streams it is really reading, and two live tabs never evict
//!   each other in a loop.
//! - **Globally**: at most [`TOTAL`] streams. Over that the new stream gets
//!   `429 rate_limited` with `Retry-After` (evicting another user's stream would let one
//!   session disturb all the others).
//!
//! A [`StreamSlot`] lives inside the response stream and gives its place back when the
//! stream is dropped (client gone, session ended, evicted). The table never holds more than
//! [`TOTAL`] entries, and a session's entry goes away with its last stream.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{AbortHandle, AbortRegistration, Abortable};
use futures_util::{Stream, StreamExt};
use rynas_api_types::error_codes;
use rynas_auth::session::SessionId;

use crate::error::Error;

/// Streams one session may hold; the oldest is closed beyond this.
pub const PER_SESSION: usize = 6;
/// Streams of all sessions together; new ones are refused beyond this.
pub const TOTAL: usize = 128;
/// `Retry-After` of the answer when [`TOTAL`] is reached.
pub const RETRY_AFTER_S: u64 = 5;

#[derive(Default)]
struct Table {
    next_id: u64,
    total: usize,
    /// Per session, its streams oldest first.
    by_session: HashMap<SessionId, VecDeque<(u64, AbortHandle)>>,
}

/// The stream table, shared through [`crate::app::AppState`].
pub struct StreamLimits {
    per_session: usize,
    total: usize,
    table: Mutex<Table>,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self::new(PER_SESSION, TOTAL)
    }
}

/// A place in the table, released on drop.
pub struct StreamSlot {
    limits: Arc<StreamLimits>,
    session: SessionId,
    id: u64,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut t = self.limits.table.lock().unwrap();
        let Some(q) = t.by_session.get_mut(&self.session) else {
            return;
        };
        // not found: already evicted (and counted out) by a newer stream
        let Some(pos) = q.iter().position(|(id, _)| *id == self.id) else {
            return;
        };
        q.remove(pos);
        if q.is_empty() {
            t.by_session.remove(&self.session);
        }
        t.total -= 1;
    }
}

/// The global cap is reached: answer 429.
#[derive(Debug, PartialEq, Eq)]
pub struct Full;

impl IntoResponse for Full {
    fn into_response(self) -> Response {
        let mut r = Error::new(
            StatusCode::TOO_MANY_REQUESTS,
            error_codes::RATE_LIMITED,
            format!("too many event streams open, retry in {RETRY_AFTER_S} s"),
        )
        .into_response();
        r.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(RETRY_AFTER_S));
        r
    }
}

impl StreamLimits {
    pub fn new(per_session: usize, total: usize) -> Self {
        Self {
            per_session: per_session.max(1),
            total: total.max(1),
            table: Mutex::new(Table::default()),
        }
    }

    /// Takes a place for a new stream of `session`, closing the session's oldest stream if
    /// it already has the maximum.
    pub fn acquire(
        self: &Arc<Self>,
        session: SessionId,
    ) -> Result<(StreamSlot, AbortRegistration), Full> {
        let mut t = self.table.lock().unwrap();
        let mine = t.by_session.get(&session).map_or(0, VecDeque::len);
        if mine >= self.per_session {
            let q = t.by_session.get_mut(&session).expect("counted above");
            if let Some((_, oldest)) = q.pop_front() {
                oldest.abort();
                t.total -= 1;
                tracing::debug!("event stream cap of a session reached: oldest stream closed");
            }
        } else if t.total >= self.total {
            tracing::warn!("{} event streams open: new stream refused", t.total);
            return Err(Full);
        }
        let id = t.next_id;
        t.next_id += 1;
        let (handle, reg) = AbortHandle::new_pair();
        t.by_session
            .entry(session)
            .or_default()
            .push_back((id, handle));
        t.total += 1;
        Ok((
            StreamSlot {
                limits: self.clone(),
                session,
                id,
            },
            reg,
        ))
    }

    /// `stream` limited: it ends when evicted and gives its place back when dropped.
    pub fn limit<S>(
        self: &Arc<Self>,
        session: SessionId,
        stream: S,
    ) -> Result<impl Stream<Item = S::Item> + Send + use<S>, Full>
    where
        S: Stream + Send,
    {
        let (slot, reg) = self.acquire(session)?;
        Ok(Abortable::new(stream, reg).map(move |item| {
            let _held = &slot;
            item
        }))
    }

    /// Streams open now (all sessions).
    pub fn open(&self) -> usize {
        self.table.lock().unwrap().total
    }

    /// Streams open now for `session`.
    pub fn open_for(&self, session: &SessionId) -> usize {
        self.table
            .lock()
            .unwrap()
            .by_session
            .get(session)
            .map_or(0, VecDeque::len)
    }

    /// Sessions with at least one stream (the size of the table).
    pub fn sessions(&self) -> usize {
        self.table.lock().unwrap().by_session.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;

    fn sid(n: u8) -> SessionId {
        [n; 32]
    }

    #[tokio::test]
    async fn oldest_stream_of_a_session_is_closed() {
        let l = Arc::new(StreamLimits::new(2, 10));
        let mut a = Box::pin(l.limit(sid(1), stream::pending::<u8>()).unwrap());
        let _b = l.limit(sid(1), stream::pending::<u8>()).unwrap();
        assert_eq!(l.open_for(&sid(1)), 2);
        let _c = l.limit(sid(1), stream::pending::<u8>()).unwrap();
        // the oldest (a) ended, the count stayed at the cap
        assert_eq!(a.next().await, None);
        assert_eq!(l.open_for(&sid(1)), 2);
        assert_eq!(l.open(), 2);
        // dropping the evicted stream does not count it out twice
        drop(a);
        assert_eq!(l.open(), 2);
    }

    #[test]
    fn global_cap_refuses_and_drop_frees() {
        let l = Arc::new(StreamLimits::new(4, 3));
        let a = l.limit(sid(1), stream::pending::<u8>()).unwrap();
        let _b = l.limit(sid(2), stream::pending::<u8>()).unwrap();
        let _c = l.limit(sid(3), stream::pending::<u8>()).unwrap();
        assert!(matches!(
            l.limit(sid(4), stream::pending::<u8>()),
            Err(Full)
        ));
        assert_eq!(l.sessions(), 3);
        drop(a);
        assert_eq!(l.open(), 2);
        assert_eq!(l.sessions(), 2, "an empty session leaves the table");
        assert!(l.limit(sid(4), stream::pending::<u8>()).is_ok());
    }

    #[test]
    fn a_full_session_may_replace_its_own_stream_at_the_global_cap() {
        let l = Arc::new(StreamLimits::new(1, 2));
        let _a = l.limit(sid(1), stream::pending::<u8>()).unwrap();
        let _b = l.limit(sid(2), stream::pending::<u8>()).unwrap();
        // global cap reached, but session 1 replaces its own stream: no 429
        let _c = l.limit(sid(1), stream::pending::<u8>()).unwrap();
        assert_eq!(l.open(), 2);
    }

    #[test]
    fn full_is_429_with_retry_after() {
        let r = Full.into_response();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(r.headers()[header::RETRY_AFTER], "5");
    }
}
