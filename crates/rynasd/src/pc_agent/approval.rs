//! A bare-metal restore approved from a phone (`docs/phase2/pc-restore-approval.md`, threat
//! model in `docs/research/pc-backup.md` §4.2 "Restore approved from a phone").
//!
//! The restore TUI runs on a live system with no state. Instead of a restore code typed from
//! the web interface it may ask for an approval:
//!
//! 1. `POST /v1/pcbackup/agent/restore-requests` (no session, small body, limited per client
//!    and in all): the PC as the person claims it and the certificate fingerprint the TUI
//!    sees, which must be this NAS' own. The answer is the request's id, a **poll token**
//!    (the TUI's secret: a QR code seen by others gives nothing), a nonce and the NAS' `sid`.
//!    The compare code is made on both sides from the nonce and the fingerprint
//!    ([`restore_compare_input`]): the NAS never sends it, so a party in the middle (its own
//!    certificate) cannot make the TUI show the code the approver sees.
//! 2. The administrators' phones get a push with the request (`action` `pc_restore`), the
//!    web interface a notification and the event `pcbackup.restore_request`.
//! 3. An administrator approves (choosing the PC when the claim matched none or several) or
//!    denies; a one-shot timer expires the request after [`RESTORE_REQUEST_TTL_MS`].
//! 4. The TUI's long poll (`GET …/{id}?wait=`, the poll token as bearer) hears the outcome;
//!    an approval carries a restore code made exactly as the web interface makes one
//!    ([`super::issue_restore_code`]: one-time, read-only, one PC, 1 hour), which the TUI
//!    uses with `POST /v1/pcbackup/agent/restore/login` as before.
//!
//! An unapproved requester learns nothing: not whether its claim matched a PC, no list, no
//! names; every request is answered the same. Requests live in memory only (a restart of
//! rynasd drops them: the TUI says so and asks again).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rynas_api_types::devices::PushAction;
use rynas_api_types::error_codes;
use rynas_api_types::notify::{EventAction, EventState, NewEvent, NotifyCategory, NotifySeverity};
use rynas_api_types::pcagent::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use super::{PaState, PcAgent, audit, bearer, code_hash, json_body, new_token, now_ms, pa, random, sha_hex, source_of};
use crate::ops::{Error, Res, user};

/// Requests waiting at once, from everybody …
pub const MAX_PENDING: usize = 16;
/// … and from one client (an IPv4 address, an IPv6 /64).
pub const MAX_PENDING_PER_SOURCE: usize = 2;
/// Requests one client may make in [`CREATE_WINDOW`] …
pub const MAX_CREATED_PER_SOURCE: usize = 6;
/// … and everybody together.
pub const MAX_CREATED: usize = 60;
pub const CREATE_WINDOW: Duration = Duration::from_secs(600);
/// A request that ended (denied, expired, withdrawn) stays this long, so that the TUI's poll
/// hears how it ended instead of "unknown".
pub const KEEP_ENDED_MS: u64 = 10 * 60 * 1000;
/// Requests remembered at most (the oldest ended ones go first).
const MAX_RECORDS: usize = 64;
/// Clients remembered by the creation limit at most.
const MAX_SOURCES: usize = 4096;

/// Events of the task stream (`?pcbackup=1`), forwarded by rynas-web on `/api/v1/events`
/// under the same names.
pub const EV_REQUEST: &str = "pcbackup.restore_request";
pub const EV_RESOLVED: &str = "pcbackup.restore_resolved";
/// The subscriber missed events: read `GET /v1/pcbackup/restore-requests` again.
pub const EV_RESYNC: &str = "pcbackup_resync";
/// Event code of the request's notification and the push's `category`.
pub const CODE_REQUEST: &str = "pcbackup.restore_request";
/// Event code of the security notification sent on approval.
pub const CODE_APPROVED: &str = "pcbackup.restore_approved";

/// Stores the request's notification and pushes it with its action to the administrators'
/// phones (main: the notification engine and the paired devices).
pub type RestorePush = Arc<dyn Fn(NewEvent, PushAction) + Send + Sync>;

struct Req {
    id: String,
    poll_sha: String,
    /// The rate limit's name of the client ([`source_of`]).
    source: String,
    view: RestoreRequestView,
    state: RestoreRequestState,
    approval: Option<RestoreApproval>,
    /// SHA-256 of the approval's restore code: the request goes once the code is used.
    code_sha: Option<String>,
    /// The record is dropped after this.
    keep_until: u64,
}

impl Req {
    /// The state at `now`: pending past its end is expired (the timer may be a moment late).
    fn state_at(&self, now: u64) -> RestoreRequestState {
        if self.state == RestoreRequestState::Pending && self.view.expires_ms <= now {
            RestoreRequestState::Expired
        } else {
            self.state
        }
    }
}

/// Requests made lately, per client and in all.
#[derive(Default)]
struct Created {
    by_source: HashMap<String, VecDeque<Instant>>,
    all: VecDeque<Instant>,
}

impl Created {
    fn admit(&mut self, source: &str) -> Result<(), &'static str> {
        let old = |t: &Instant| t.elapsed() > CREATE_WINDOW;
        while self.all.front().is_some_and(old) {
            self.all.pop_front();
        }
        self.by_source.retain(|_, q| {
            while q.front().is_some_and(old) {
                q.pop_front();
            }
            !q.is_empty()
        });
        if self.all.len() >= MAX_CREATED {
            return Err("too many restore requests lately: try again in a few minutes");
        }
        if self.by_source.get(source).is_some_and(|q| q.len() >= MAX_CREATED_PER_SOURCE) {
            return Err("too many restore requests from this address: try again in ten minutes");
        }
        if !self.by_source.contains_key(source) && self.by_source.len() >= MAX_SOURCES {
            return Err("too many restore requests lately: try again in a few minutes");
        }
        let now = Instant::now();
        self.all.push_back(now);
        self.by_source.entry(source.to_string()).or_default().push_back(now);
        Ok(())
    }
}

/// The restore requests of this rynasd (memory only).
pub struct Approvals {
    reqs: Mutex<Vec<Req>>,
    created: Mutex<Created>,
    /// Woken on every change of a request (the TUIs' long polls).
    changed: tokio::sync::Notify,
    events: broadcast::Sender<(&'static str, Arc<str>)>,
}

impl Default for Approvals {
    fn default() -> Self {
        Self {
            reqs: Mutex::new(Vec::new()),
            created: Mutex::new(Created::default()),
            changed: tokio::sync::Notify::new(),
            events: broadcast::channel(32).0,
        }
    }
}

impl Approvals {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Req>> {
        let mut r = self.reqs.lock().unwrap_or_else(|p| p.into_inner());
        let now = now_ms();
        r.retain(|x| x.keep_until > now);
        while r.len() > MAX_RECORDS {
            // the oldest ended one, else the oldest
            let i = r.iter().position(|x| x.state_at(now) != RestoreRequestState::Pending).unwrap_or(0);
            r.remove(i);
        }
        r
    }

    /// The events `pcbackup.restore_request` (data [`RestoreRequestView`]) and
    /// `pcbackup.restore_resolved` (data [`RestoreRequestResolved`]).
    pub fn subscribe(&self) -> broadcast::Receiver<(&'static str, Arc<str>)> {
        self.events.subscribe()
    }

    fn publish<T: serde::Serialize>(&self, name: &'static str, data: &T) {
        if let Ok(j) = serde_json::to_string(data) {
            let _ = self.events.send((name, j.into()));
        }
        self.changed.notify_waiters();
    }

    /// The pending requests, oldest first.
    pub fn pending(&self) -> Vec<RestoreRequestView> {
        let now = now_ms();
        self.lock()
            .iter()
            .filter(|r| r.state_at(now) == RestoreRequestState::Pending)
            .map(|r| r.view.clone())
            .collect()
    }

    /// The restore code with this SHA-256 was used to log in: its request is done.
    pub fn code_used(&self, code_sha: &str) {
        self.lock().retain(|r| r.code_sha.as_deref() != Some(code_sha));
    }

    /// The poll's answer for the request `id` with the poll token `token`; `None` for an
    /// unknown request or a wrong token (told apart from nobody).
    fn status(&self, id: &str, token: &str) -> Option<RestoreRequestPoll> {
        let now = now_ms();
        let sha = sha_hex(token.as_bytes());
        let r = self.lock();
        let x = r.iter().find(|x| x.id == id && !token.is_empty() && x.poll_sha == sha)?;
        let state = x.state_at(now);
        Some(RestoreRequestPoll {
            state,
            expires_ms: x.view.expires_ms,
            approval: (state == RestoreRequestState::Approved).then(|| x.approval.clone()).flatten(),
        })
    }

    /// Ends the pending request `id` with `outcome` (denied, expired, canceled): `Ok(view)`
    /// when it was pending, else the error a decision gets (404, 410, 409).
    fn end(&self, id: &str, outcome: RestoreRequestState) -> Res<RestoreRequestView> {
        let now = now_ms();
        let mut r = self.lock();
        let x = r.iter_mut().find(|x| x.id == id).ok_or_else(not_found)?;
        check_pending(x, now)?;
        x.state = outcome;
        x.keep_until = now + KEEP_ENDED_MS;
        Ok(x.view.clone())
    }
}

fn not_found() -> Error {
    Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, "no such restore request".into())
}

/// A decision on `x` at `now`: 410 once its time ran out, 409 once decided.
fn check_pending(x: &Req, now: u64) -> Res<()> {
    match x.state_at(now) {
        RestoreRequestState::Pending => Ok(()),
        RestoreRequestState::Expired => Err(Error::new(
            StatusCode::GONE,
            error_codes::REQUEST_EXPIRED,
            "the restore request expired".into(),
        )),
        s => Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            format!("the restore request is already {}", s.as_str()),
        )),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A fingerprint as upper-case hex without separators.
fn fp_hex(s: &str) -> String {
    s.chars().filter(char::is_ascii_hexdigit).map(|c| c.to_ascii_uppercase()).collect()
}

/// The compare code of `nonce` and `fingerprint` ([`restore_compare_input`]).
pub fn compare_code(nonce: &str, fingerprint: &str) -> String {
    restore_compare_code(&Sha256::digest(restore_compare_input(nonce, fingerprint).as_bytes()))
}

/// The claimed PC, trimmed: 1 to [`RESTORE_REQUEST_MAX_PC`] characters, no control
/// characters.
fn clean_pc(s: &str) -> Option<String> {
    let t = s.trim();
    let n = t.chars().count();
    (n > 0 && n <= RESTORE_REQUEST_MAX_PC && !t.chars().any(char::is_control)).then(|| t.to_string())
}

/// The backup PC a claim names when exactly one does: its name on the NAS or its host name
/// (whole, or the first label of either side), without regard to case.
pub fn match_device(pcs: &[RestorablePc], claim: &str) -> Option<String> {
    let c = claim.trim().to_lowercase();
    let short = |s: &str| s.split('.').next().unwrap_or(s).to_string();
    let hits: Vec<&RestorablePc> = pcs
        .iter()
        .filter(|p| {
            let h = p.hostname.to_lowercase();
            p.device.to_lowercase() == c || (!h.is_empty() && (h == c || short(&h) == short(&c)))
        })
        .collect();
    match hits.as_slice() {
        [one] => Some(one.device.clone()),
        _ => None,
    }
}

/// The client's address as rynas-web saw it (whole, for the approver; the rate limit uses
/// [`source_of`]).
fn from_ip(h: &HeaderMap) -> String {
    let raw = h
        .get(rynas_auth::ipc::CLIENT_ADDR_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim();
    match raw.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(a)) => a.to_ipv4_mapped().map(|v| v.to_string()).unwrap_or_else(|| a.to_string()),
        Ok(a) => a.to_string(),
        Err(_) => "local".into(),
    }
}

fn key_of(id: &str) -> String {
    format!("pcbackup:restore-request:{id}")
}

/// The request's notification (English; clients translate it from the code and fields).
pub fn request_event(v: &RestoreRequestView) -> NewEvent {
    let code = restore_code_text(&v.code);
    let mut ev = NewEvent {
        source: "pcbackup".into(),
        category: NotifyCategory::Backup,
        severity: NotifySeverity::Info,
        key: Some(key_of(&v.id)),
        state: EventState::Firing,
        title: format!("Restore of {} waits for approval", v.pc),
        body: format!(
            "A bare-metal restore from {} asks to read the backups of {} (code {code}). Approve it only if you started it and the PC's screen shows the same code.",
            v.from_ip, v.pc
        ),
        code: CODE_REQUEST.into(),
        actions: vec![EventAction {
            label: "Review".into(),
            url: format!("/backup?restore={}", v.id),
        }],
        ..Default::default()
    };
    for (k, val) in [("request", v.id.clone()), ("pc", v.pc.clone()), ("from_ip", v.from_ip.clone()), ("code", code)] {
        ev.fields.insert(k.into(), val);
    }
    ev
}

/// The end of the request's notification: stored, sent to no one.
fn resolved_event(v: &RestoreRequestView) -> NewEvent {
    NewEvent {
        state: EventState::Resolved,
        quiet: true,
        actions: Vec::new(),
        ..request_event(v)
    }
}

/// The security notification of an approval.
fn approved_event(v: &RestoreRequestView, device: &str, who: &str) -> NewEvent {
    let mut ev = NewEvent {
        source: "pcbackup".into(),
        category: NotifyCategory::Security,
        severity: NotifySeverity::Warning,
        key: Some(format!("pcbackup:restore-approved:{}", v.id)),
        title: format!("Restore of {device} approved by {who}"),
        body: format!(
            "A bare-metal restore from {} (asked as {}) may read the backups of {device}, read-only, for an hour.",
            v.from_ip, v.pc
        ),
        code: CODE_APPROVED.into(),
        ..Default::default()
    };
    for (k, val) in [("device", device), ("user", who), ("from_ip", v.from_ip.as_str()), ("pc", v.pc.as_str())] {
        ev.fields.insert(k.into(), val.to_string());
    }
    ev
}

/// The push action of a request.
pub fn push_action(v: &RestoreRequestView) -> PushAction {
    PushAction {
        kind: PUSH_ACTION_PC_RESTORE.into(),
        request: v.id.clone(),
        pc: v.pc.clone(),
        device: v.device.clone(),
        code: v.code.clone(),
        from_ip: v.from_ip.clone(),
        expires_ms: v.expires_ms,
    }
}

/// `request` ended with `outcome`: the event, the notification resolved, the polls woken.
fn ended(svc: &PcAgent, v: &RestoreRequestView, outcome: RestoreRequestState) {
    svc.notifier.emit(resolved_event(v));
    svc.approvals.publish(
        EV_RESOLVED,
        &RestoreRequestResolved {
            id: v.id.clone(),
            outcome,
        },
    );
}

/// The one-shot timer of a request: at its end, a request still pending expires.
fn expire_at(svc: &Arc<PcAgent>, id: String, at_ms: u64) {
    let me = Arc::downgrade(svc);
    // the deadline is taken now, not when the task first runs
    let deadline = tokio::time::Instant::now() + Duration::from_millis(at_ms.saturating_sub(now_ms()));
    tokio::spawn(async move {
        tokio::time::sleep_until(deadline).await;
        let Some(svc) = me.upgrade() else { return };
        if let Ok(v) = svc.approvals.end(&id, RestoreRequestState::Expired) {
            tracing::info!(target: "rynas::audit", "pc backup: restore request {id} for \"{}\" expired", v.pc);
            ended(&svc, &v, RestoreRequestState::Expired);
        }
    });
}

/// `POST /v1/pcbackup/agent/restore-requests` (no session).
pub async fn create_post(
    State(st): State<PaState>,
    h: HeaderMap,
    body: Result<Json<RestoreRequestCreate>, JsonRejection>,
) -> Res<Response> {
    let svc = pa(&st)?;
    let source = source_of(&h);
    // a client locked out for wrong codes or poll tokens makes no requests either
    svc.check_rate(&source).await?;
    let req = json_body(body)?;
    let pc = clean_pc(&req.pc).ok_or_else(|| {
        Error::invalid("pc", format!("the PC's name: 1 to {RESTORE_REQUEST_MAX_PC} characters"))
    })?;
    let seen = fp_hex(&req.fingerprint);
    if !(seen.is_empty() || seen.len() == 64) {
        return Err(Error::invalid("fingerprint", "a SHA-256 fingerprint"));
    }
    // the code is made from the certificate the TUI claims to see: this NAS' own, or none (a
    // TUI on plain HTTP, `--allow-http`). A party in the middle that forwards its own
    // certificate's fingerprint is refused; one that rewrites it to ours or to none makes the
    // two codes differ, since the TUI makes its code from the certificate it really saw.
    let own_fp = svc.fingerprint().unwrap_or_default();
    let own = fp_hex(&own_fp);
    if !(seen.is_empty() || seen == own) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CERTIFICATE_MISMATCH,
            "this NAS does not serve the certificate you see: something in between answers for it, or the address goes through another proxy (use the address of the NAS' web interface)".into(),
        )
        .field("fingerprint"));
    }
    let sid = crate::devices::load_sid(&svc.state_dir.join("nas-sid")).map_err(Error::internal)?;
    let now = now_ms();
    let view = {
        let mut reqs = svc.approvals.lock();
        let pending: Vec<&Req> = reqs.iter().filter(|r| r.state_at(now) == RestoreRequestState::Pending).collect();
        let busy = if pending.len() >= MAX_PENDING {
            Some("too many restore requests wait for a decision: try again later")
        } else if pending.iter().filter(|r| r.source == source).count() >= MAX_PENDING_PER_SOURCE {
            Some("this address already has restore requests waiting: withdraw one or wait")
        } else {
            None
        };
        if let Some(m) = busy {
            return Err(Error::new(StatusCode::TOO_MANY_REQUESTS, error_codes::RATE_LIMITED, m.into()));
        }
        svc.approvals
            .created
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .admit(&source)
            .map_err(|m| Error::new(StatusCode::TOO_MANY_REQUESTS, error_codes::RATE_LIMITED, m.into()))?;
        // 64 bits: the id names the request, the poll token is the secret; short, so that the
        // TUI's QR code stays small enough for an 80×25 text console
        let id = hex(&random(8)?);
        let token = new_token()?;
        let nonce = hex(&random(16)?);
        let view = RestoreRequestView {
            id: id.clone(),
            device: match_device(&super::restorable(&svc), &pc),
            pc,
            code: compare_code(&nonce, &seen),
            from_ip: from_ip(&h),
            fingerprint: if seen.is_empty() { String::new() } else { own_fp },
            created_ms: now,
            expires_ms: now + RESTORE_REQUEST_TTL_MS,
        };
        reqs.push(Req {
            id,
            poll_sha: sha_hex(token.as_bytes()),
            source,
            view: view.clone(),
            state: RestoreRequestState::Pending,
            approval: None,
            code_sha: None,
            keep_until: view.expires_ms + KEEP_ENDED_MS,
        });
        (view, token, nonce)
    };
    let (view, poll_token, nonce) = view;
    tracing::info!(
        target: "rynas::audit",
        "pc backup: restore request {} for \"{}\" from {} (matches {})",
        view.id,
        view.pc,
        view.from_ip,
        view.device.as_deref().unwrap_or("no single PC")
    );
    expire_at(&svc, view.id.clone(), view.expires_ms);
    let ev = request_event(&view);
    match svc.restore_push.get() {
        Some(push) => push(ev, push_action(&view)),
        None => svc.notifier.emit(ev),
    }
    svc.approvals.publish(EV_REQUEST, &view);
    Ok((
        StatusCode::CREATED,
        Json(RestoreRequestCreated {
            id: view.id,
            poll_token,
            nonce,
            sid,
            expires_ms: view.expires_ms,
        }),
    )
        .into_response())
}

#[derive(Debug, Deserialize, Default)]
pub struct PollQuery {
    /// Seconds to wait for a decision (at most [`RESTORE_REQUEST_POLL_WAIT_S`]).
    #[serde(default)]
    pub wait: Option<u64>,
}

/// An unknown request or a wrong poll token: counted like a wrong code, answered 404.
fn unknown(svc: &PcAgent, source: &str) -> Error {
    svc.failures.lock().unwrap_or_else(|p| p.into_inner()).record(source);
    tracing::warn!(target: "rynas::audit", "pc backup: a restore request poll with an unknown id or token");
    not_found()
}

/// `GET /v1/pcbackup/agent/restore-requests/{id}?wait=<s>` (poll token as bearer): the
/// state, waiting up to `wait` seconds while it is pending.
pub async fn poll_get(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    q: Result<Query<PollQuery>, axum::extract::rejection::QueryRejection>,
) -> Res<Json<RestoreRequestPoll>> {
    let svc = pa(&st)?;
    let source = source_of(&h);
    svc.check_rate(&source).await?;
    let wait = q.map(|Query(q)| q.wait.unwrap_or(0)).unwrap_or(0).min(RESTORE_REQUEST_POLL_WAIT_S);
    let token = bearer(&h).unwrap_or_default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let changed = svc.approvals.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let Some(p) = svc.approvals.status(&id, &token) else {
            return Err(unknown(&svc, &source));
        };
        if p.state != RestoreRequestState::Pending || tokio::time::Instant::now() >= deadline {
            return Ok(Json(p));
        }
        // a decision, a withdrawal and the request's own end (its timer) all wake this
        let _ = tokio::time::timeout_at(deadline, changed).await;
    }
}

/// `DELETE /v1/pcbackup/agent/restore-requests/{id}` (poll token as bearer): the TUI
/// withdraws its request (Esc). An approved one's restore code is revoked unless used.
/// 204 also for a request that had ended.
pub async fn cancel_delete(State(st): State<PaState>, h: HeaderMap, UrlPath(id): UrlPath<String>) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let source = source_of(&h);
    svc.check_rate(&source).await?;
    let token = bearer(&h).unwrap_or_default();
    let Some(p) = svc.approvals.status(&id, &token) else {
        return Err(unknown(&svc, &source));
    };
    match p.state {
        RestoreRequestState::Pending => {
            if let Ok(v) = svc.approvals.end(&id, RestoreRequestState::Canceled) {
                tracing::info!(target: "rynas::audit", "pc backup: restore request {id} for \"{}\" withdrawn", v.pc);
                ended(&svc, &v, RestoreRequestState::Canceled);
            }
        }
        RestoreRequestState::Approved => {
            let sha = {
                let mut r = svc.approvals.lock();
                let sha = r.iter().find(|x| x.id == id).and_then(|x| x.code_sha.clone());
                r.retain(|x| x.id != id);
                sha
            };
            if let Some(sha) = sha {
                svc.edit(|r| {
                    r.restore_codes.retain(|c| c.code_sha != sha);
                    Ok(())
                })?;
                tracing::info!(target: "rynas::audit", "pc backup: restore request {id} withdrawn after its approval: its code is revoked");
            }
            svc.approvals.changed.notify_waiters();
        }
        _ => {}
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/pcbackup/restore-requests` (administrators): the pending requests.
pub async fn list_get(State(st): State<PaState>) -> Res<Json<Vec<RestoreRequestView>>> {
    let svc = pa(&st)?;
    Ok(Json(svc.approvals.pending()))
}

/// `POST /v1/pcbackup/restore-requests/{id}/approve` (administrators): body
/// [`RestoreRequestApprove`] (may be empty when the request matched a PC). 204; 404 unknown,
/// 410 expired, 409 already decided; the PC's own errors (404 no such PC, 409 nothing to
/// restore from).
pub async fn approve_post(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    let want: RestoreRequestApprove = if body.iter().all(u8::is_ascii_whitespace) {
        RestoreRequestApprove::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_JSON, e.to_string()))?
    };
    let now = now_ms();
    let (view, device) = {
        let mut reqs = svc.approvals.lock();
        let x = reqs.iter_mut().find(|x| x.id == id).ok_or_else(not_found)?;
        check_pending(x, now)?;
        let device = want
            .device
            .filter(|d| !d.is_empty())
            .or_else(|| x.view.device.clone())
            .ok_or_else(|| Error::invalid("device", "choose the PC whose backups the restore may read"))?;
        if !rynas_api_types::pcagent::valid_device(&device) {
            return Err(Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, "no such PC".into()));
        }
        // the very code the web interface makes (one-time, read-only, this PC, 1 hour)
        let (code, expires_ms) = super::issue_restore_code(&svc, &device)?;
        x.state = RestoreRequestState::Approved;
        x.code_sha = Some(code_hash(&code));
        x.approval = Some(RestoreApproval {
            code,
            device: device.clone(),
            expires_ms,
        });
        x.keep_until = expires_ms;
        (x.view.clone(), device)
    };
    audit(&who, "approved the restore request", &format!("{id} of \"{}\" from {} for {device}", view.pc, view.from_ip));
    svc.notifier.emit(approved_event(&view, &device, &who));
    ended(&svc, &view, RestoreRequestState::Approved);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/pcbackup/restore-requests/{id}/deny` (administrators): 204; 404, 410, 409 as
/// for an approval.
pub async fn deny_post(State(st): State<PaState>, h: HeaderMap, UrlPath(id): UrlPath<String>) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    let view = svc.approvals.end(&id, RestoreRequestState::Denied)?;
    audit(&who, "denied the restore request", &format!("{id} of \"{}\" from {}", view.pc, view.from_ip));
    ended(&svc, &view, RestoreRequestState::Denied);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc(device: &str, hostname: &str) -> RestorablePc {
        RestorablePc {
            device: device.into(),
            hostname: hostname.into(),
            files: true,
            ..Default::default()
        }
    }

    #[test]
    fn claims_match_one_pc_or_none() {
        let pcs = [pc("laptop", "thinkpad.home.arpa"), pc("desk", "desk"), pc("desk2", "desk")];
        assert_eq!(match_device(&pcs, "Laptop").as_deref(), Some("laptop"));
        assert_eq!(match_device(&pcs, " thinkpad ").as_deref(), Some("laptop"));
        assert_eq!(match_device(&pcs, "thinkpad.home.arpa").as_deref(), Some("laptop"));
        // two PCs carry that host name: the approver chooses
        assert_eq!(match_device(&pcs, "desk2").as_deref(), Some("desk2"));
        assert_eq!(match_device(&[pc("a", "x"), pc("b", "x")], "x"), None);
        assert_eq!(match_device(&pcs, "nothing"), None);
    }

    #[test]
    fn claimed_names_are_checked() {
        assert_eq!(clean_pc("  my pc ").as_deref(), Some("my pc"));
        assert_eq!(clean_pc("   "), None);
        assert_eq!(clean_pc("a\nb"), None);
        assert_eq!(clean_pc(&"x".repeat(65)), None);
        assert!(clean_pc(&"è".repeat(64)).is_some());
    }

    #[test]
    fn compare_codes_follow_the_certificate() {
        let fp = "AB:CD:".repeat(15) + "AB:CD";
        let a = compare_code("00ff", &fp);
        assert_eq!(a.len(), 6);
        assert!(a.bytes().all(|b| b.is_ascii_digit()));
        // the same certificate written another way: the same code
        assert_eq!(compare_code("00ff", &fp.to_lowercase().replace(':', "")), a);
        // another certificate or nonce: another code (with these inputs)
        assert_ne!(compare_code("00ff", &fp.replace("AB", "AC")), a);
        assert_ne!(compare_code("00fe", &fp), a);
    }

    #[test]
    fn the_creation_limit_counts_per_client_and_in_all() {
        let mut c = Created::default();
        for _ in 0..MAX_CREATED_PER_SOURCE {
            c.admit("192.0.2.1").unwrap();
        }
        assert!(c.admit("192.0.2.1").is_err());
        // another client still may
        c.admit("192.0.2.2").unwrap();
        let mut c = Created::default();
        for i in 0..MAX_CREATED {
            c.admit(&format!("198.51.100.{}", i % 250)).unwrap();
        }
        assert!(c.admit("203.0.113.9").is_err(), "everybody together is limited too");
    }
}
