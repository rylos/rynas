//! A bare-metal restore approved from a phone (`docs/phase2/pc-restore-approval.md`, threat
//! model in `docs/research/pc-backup.md` §4.2.2).
//!
//! The restore TUI runs on a live system with no state. Instead of a restore code typed from
//! the web interface it may ask for an approval:
//!
//! 1. `POST /v1/pcbackup/agent/restore-requests` (no session, small body, only from the
//!    allowed addresses, limited per client and in all): the PC as the person claims it, the
//!    certificate fingerprint the TUI sees (over TLS it must be this NAS' own) and a
//!    **commitment** to a 256-bit secret of the TUI. The answer is the request's id, a
//!    **poll token** (the TUI's credential: a QR code seen by others gives nothing), a nonce
//!    made after the commitment, and the NAS' `sid`.
//! 2. `POST …/{id}/reveal` with the secret: the NAS checks it against the commitment and only
//!    then makes the compare code (ZRTP-style: SHA-256 of secret, nonce and its own
//!    certificate, [`restore_compare_input`]) and shows the request to administrators. A
//!    party in the middle can neither change the secret after the commitment nor know it
//!    when it picks the nonce the TUI sees, so its codes match the NAS' only by chance.
//! 3. The administrators' phones get a push (`action` `pc_restore`, without the code; at most
//!    one every [`PUSH_EVERY`] while requests already wait), the web interface a quiet
//!    notification and the event `pcbackup.restore_request`.
//! 4. An administrator **types the six digits the PC's screen shows** and approves (choosing
//!    the PC when the claim matched none or several), or denies (optionally blocking the
//!    address for [`BLOCK_FOR`]). A one-shot timer expires the request.
//! 5. The TUI's long poll (`GET …/{id}?wait=`, the poll token as bearer) hears the outcome;
//!    an approval carries a restore code made exactly as the web interface makes one
//!    ([`super::issue_restore_code`]: one-time, read-only, one PC, 1 hour), which the TUI
//!    uses with `POST /v1/pcbackup/agent/restore/login` as before.
//!
//! An unapproved requester learns nothing: not whether its claim matched a PC, no list, no
//! names, no code. A request holding a valid poll token is never caught by the wrong-code
//! lockout of its address. Requests live in memory only (a restart of rynasd drops them).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rynas_api_types::devices::PushAction;
use rynas_api_types::error_codes;
use rynas_api_types::notify::{EventAction, EventState, NewEvent, NotifyCategory, NotifySeverity};
use rynas_api_types::pcagent::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use super::{PaState, PcAgent, audit, bearer, code_hash, json_body, new_token, now_ms, pa, random, sha_hex, source_of};
use crate::ops::{Error, Res, user};

/// Requests waiting at once (revealed or not), from everybody …
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
/// While requests already wait, a new one is pushed only if the last push is this old (the
/// list and the notification show every one).
pub const PUSH_EVERY: Duration = Duration::from_secs(300);
/// "Deny and block": new requests from that client are refused this long.
pub const BLOCK_FOR: Duration = Duration::from_secs(3600);
/// Long polls of one request at once.
pub const MAX_POLLS: u8 = 2;
/// Requests remembered at most (the oldest ended ones go first).
const MAX_RECORDS: usize = 64;
/// Clients remembered by the creation limit and the blocks at most.
const MAX_SOURCES: usize = 4096;

/// Events of the task stream (`?pcbackup=1`), forwarded by rynas-web on `/api/v1/events`
/// under the same names.
pub const EV_REQUEST: &str = "pcbackup.restore_request";
pub const EV_RESOLVED: &str = "pcbackup.restore_resolved";
/// The subscriber missed events: read `GET /v1/pcbackup/restore-requests` again.
pub const EV_RESYNC: &str = "pcbackup_resync";
/// Event code of the request's notification and the push's `category`.
pub const CODE_REQUEST: &str = "pcbackup.restore_request";
/// The same over plain HTTP (its texts say there is no certificate check).
pub const CODE_REQUEST_HTTP: &str = "pcbackup.restore_request_http";
/// Event code of the security notification sent on approval.
pub const CODE_APPROVED: &str = "pcbackup.restore_approved";

/// Stores the request's (quiet) notification and pushes it with its action to the
/// administrators' phones (main: the notification engine and the paired devices).
pub type RestorePush = Arc<dyn Fn(NewEvent, PushAction) + Send + Sync>;

struct Req {
    id: String,
    poll_sha: String,
    /// The rate limit's name of the client ([`source_of`]).
    source: String,
    /// hex SHA-256 of the TUI's commitment input.
    commit: String,
    nonce: String,
    /// The certificate the code binds: this NAS' own (or empty over plain HTTP).
    fp: String,
    /// Revealed: administrators see it; the compare code is known.
    code: Option<String>,
    view: RestoreRequestView,
    state: RestoreRequestState,
    /// An approval is issuing its restore code (outside the lock).
    busy: bool,
    wrong: u8,
    polls: u8,
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

    fn shown(&self, now: u64) -> bool {
        self.code.is_some() && self.state_at(now) == RestoreRequestState::Pending
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
    /// Clients refused until then ("Deny and block").
    blocked: Mutex<HashMap<String, Instant>>,
    last_push: Mutex<Option<Instant>>,
    /// Woken on every change of a request (the TUIs' long polls).
    changed: tokio::sync::Notify,
    events: broadcast::Sender<(&'static str, Arc<str>)>,
}

impl Default for Approvals {
    fn default() -> Self {
        Self {
            reqs: Mutex::new(Vec::new()),
            created: Mutex::new(Created::default()),
            blocked: Mutex::new(HashMap::new()),
            last_push: Mutex::new(None),
            changed: tokio::sync::Notify::new(),
            events: broadcast::channel(32).0,
        }
    }
}

/// Other shown requests for the same PC as `r` (the same match, or the same claim).
fn same_pc(all: &[Req], r: &Req, now: u64) -> u32 {
    let claim = r.view.pc.to_lowercase();
    all.iter()
        .filter(|x| x.id != r.id && x.shown(now))
        .filter(|x| x.view.pc.to_lowercase() == claim || (r.view.device.is_some() && x.view.device == r.view.device))
        .count() as u32
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

    /// The requests administrators see (revealed and pending), oldest first.
    pub fn pending(&self) -> Vec<RestoreRequestView> {
        let now = now_ms();
        let r = self.lock();
        r.iter()
            .filter(|x| x.shown(now))
            .map(|x| RestoreRequestView {
                same_pc: same_pc(&r, x, now),
                ..x.view.clone()
            })
            .collect()
    }

    /// The restore code with this SHA-256 was used to log in: its request is done.
    pub fn code_used(&self, code_sha: &str) {
        self.lock().retain(|r| r.code_sha.as_deref() != Some(code_sha));
    }

    /// Whether `source` is blocked now.
    fn is_blocked(&self, source: &str) -> bool {
        let mut b = self.blocked.lock().unwrap_or_else(|p| p.into_inner());
        b.retain(|_, until| *until > Instant::now());
        b.contains_key(source)
    }

    fn block(&self, source: &str) {
        let mut b = self.blocked.lock().unwrap_or_else(|p| p.into_inner());
        b.retain(|_, until| *until > Instant::now());
        if b.len() < MAX_SOURCES || b.contains_key(source) {
            b.insert(source.to_string(), Instant::now() + BLOCK_FOR);
        }
    }

    /// Whether the request `id` holds the poll token `token`.
    fn verify(&self, id: &str, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let sha = sha_hex(token.as_bytes());
        self.lock().iter().any(|x| x.id == id && x.poll_sha == sha)
    }

    /// The poll's answer for the request `id` (its token checked by the caller).
    fn status(&self, id: &str) -> Option<RestoreRequestPoll> {
        let now = now_ms();
        let r = self.lock();
        let x = r.iter().find(|x| x.id == id)?;
        let state = x.state_at(now);
        Some(RestoreRequestPoll {
            state,
            expires_ms: x.view.expires_ms,
            approval: (state == RestoreRequestState::Approved).then(|| x.approval.clone()).flatten(),
        })
    }

    /// Ends the pending request `id` with `outcome` (denied, expired, canceled): `Ok((view,
    /// source))` when it was pending, else the error a decision gets (404, 410, 409).
    fn end(&self, id: &str, outcome: RestoreRequestState) -> Res<(RestoreRequestView, String)> {
        let now = now_ms();
        let mut r = self.lock();
        let x = r.iter_mut().find(|x| x.id == id).ok_or_else(not_found)?;
        check_pending(x, now)?;
        x.state = outcome;
        x.keep_until = now + KEEP_ENDED_MS;
        Ok((x.view.clone(), x.source.clone()))
    }
}

fn not_found() -> Error {
    Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, "no such restore request".into())
}

/// A decision on `x` at `now`: 404 before it is revealed, 410 once its time ran out, 409 once
/// decided or while another approval runs.
fn check_pending(x: &Req, now: u64) -> Res<()> {
    match x.state_at(now) {
        RestoreRequestState::Pending if x.code.is_none() => Err(not_found()),
        RestoreRequestState::Pending if x.busy => Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CONFLICT,
            "the restore request is being approved".into(),
        )),
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

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The commitment to `secret` (hex SHA-256 of [`restore_commit_input`]).
pub fn commitment(secret: &str) -> String {
    hex(&Sha256::digest(restore_commit_input(secret).as_bytes()))
}

/// The compare code of `secret`, `nonce` and `fingerprint` ([`restore_compare_input`]).
pub fn compare_code(secret: &str, nonce: &str, fingerprint: &str) -> String {
    restore_compare_code(&Sha256::digest(restore_compare_input(secret, nonce, fingerprint).as_bytes()))
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

/// The client's address as rynas-web saw it (`None` without one).
fn client_ip(h: &HeaderMap) -> Option<std::net::IpAddr> {
    let raw = h.get(rynas_auth::ipc::CLIENT_ADDR_HEADER)?.to_str().ok()?.trim();
    match raw.parse::<std::net::IpAddr>().ok()? {
        std::net::IpAddr::V6(a) => Some(a.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(std::net::IpAddr::V6(a))),
        a => Some(a),
    }
}

/// The client reached rynas-web over TLS (rynas-web says so; never a client's header).
fn over_tls(h: &HeaderMap) -> bool {
    h.get(rynas_auth::ipc::CLIENT_TLS_HEADER).is_some_and(|v| v == "1")
}

fn key_of(id: &str) -> String {
    format!("pcbackup:restore-request:{id}")
}

/// The request's notification (English; clients translate it from the code and fields).
/// Quiet: stored and shown, sent to no channel (the phones get their own push).
pub fn request_event(v: &RestoreRequestView) -> NewEvent {
    let http = if v.tls { "" } else { " Warning: it came over plain HTTP, no certificate binds its code." };
    let mut ev = NewEvent {
        source: "pcbackup".into(),
        category: NotifyCategory::Backup,
        severity: NotifySeverity::Info,
        key: Some(key_of(&v.id)),
        state: EventState::Firing,
        title: format!("Restore of {} waits for approval", v.pc),
        body: format!(
            "A bare-metal restore from {} asks to read the backups of {}. Approve it only if you started it, typing the code the PC's screen shows.{http}",
            v.from_ip, v.pc
        ),
        code: if v.tls { CODE_REQUEST } else { CODE_REQUEST_HTTP }.into(),
        actions: vec![EventAction {
            label: "Review".into(),
            url: format!("/backup?restore={}", v.id),
        }],
        quiet: true,
        ..Default::default()
    };
    for (k, val) in [("request", v.id.clone()), ("pc", v.pc.clone()), ("from_ip", v.from_ip.clone())] {
        ev.fields.insert(k.into(), val);
    }
    ev
}

/// The end of the request's notification: stored, sent to no one.
fn resolved_event(v: &RestoreRequestView) -> NewEvent {
    NewEvent {
        state: EventState::Resolved,
        actions: Vec::new(),
        ..request_event(v)
    }
}

/// The security notification of an approval (normal rules: every channel hears it).
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

/// The push action of a request (never the code).
pub fn push_action(v: &RestoreRequestView) -> PushAction {
    PushAction {
        kind: PUSH_ACTION_PC_RESTORE.into(),
        request: v.id.clone(),
        pc: v.pc.clone(),
        device: v.device.clone(),
        from_ip: v.from_ip.clone(),
        tls: v.tls,
        same_pc: v.same_pc,
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

/// A one-shot timer: at `at_ms`, the request still pending ends; `unrevealed` only drops one
/// never revealed (silently: nobody saw it).
fn timer(svc: &Arc<PcAgent>, id: String, at_ms: u64, unrevealed: bool) {
    let me = Arc::downgrade(svc);
    // the deadline is taken now, not when the task first runs
    let deadline = tokio::time::Instant::now() + Duration::from_millis(at_ms.saturating_sub(now_ms()));
    tokio::spawn(async move {
        tokio::time::sleep_until(deadline).await;
        let Some(svc) = me.upgrade() else { return };
        if unrevealed {
            let mut r = svc.approvals.lock();
            r.retain(|x| !(x.id == id && x.code.is_none() && x.state == RestoreRequestState::Pending));
            drop(r);
            svc.approvals.changed.notify_waiters();
            return;
        }
        if let Ok((v, _)) = svc.approvals.end(&id, RestoreRequestState::Expired) {
            tracing::info!(target: "rynas::audit", "pc backup: restore request {id} for \"{}\" expired", v.pc);
            ended(&svc, &v, RestoreRequestState::Expired);
        }
    });
}

fn refused(m: &str) -> Error {
    Error::new(StatusCode::FORBIDDEN, error_codes::FORBIDDEN, m.into())
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
    let settings = svc.reg.lock().unwrap_or_else(|p| p.into_inner()).settings.clone();
    let ip = client_ip(&h);
    if !ip.is_some_and(|ip| restore_source_allowed(&settings.restore_request_sources, ip)) {
        return Err(refused(
            "restore requests are accepted only from the local network (Backup › PC backup settings name other addresses); use a restore code",
        ));
    }
    if svc.approvals.is_blocked(&source) {
        return Err(refused("an administrator blocked restore requests from this address for an hour"));
    }
    let tls = over_tls(&h);
    if !tls && !settings.restore_requests_over_http {
        return Err(refused("restore requests over plain HTTP are turned off on this NAS: use https, or a restore code"));
    }
    let req = json_body(body)?;
    let pc = clean_pc(&req.pc).ok_or_else(|| {
        Error::invalid("pc", format!("the PC's name: 1 to {RESTORE_REQUEST_MAX_PC} characters"))
    })?;
    let commit = req.commit.trim().to_ascii_lowercase();
    if !is_hex(&commit, 64) {
        return Err(Error::invalid("commit", "a SHA-256 commitment (64 hex digits)"));
    }
    let seen = fp_hex(&req.fingerprint);
    if !(seen.is_empty() || seen.len() == 64) {
        return Err(Error::invalid("fingerprint", "a SHA-256 fingerprint"));
    }
    // over TLS the TUI saw a certificate: it must say which, and it must be ours (a party in
    // the middle forwarding its own is refused; one rewriting it to ours gets other digits,
    // since the TUI makes its code from the certificate it really saw)
    let own = fp_hex(&svc.fingerprint().unwrap_or_default());
    if tls && seen.is_empty() {
        return Err(Error::invalid("fingerprint", "the certificate the client sees is needed over TLS"));
    }
    if !seen.is_empty() && seen != own {
        return Err(Error::new(
            StatusCode::CONFLICT,
            error_codes::CERTIFICATE_MISMATCH,
            "this NAS does not serve the certificate you see: something in between answers for it, or the address goes through another proxy (use the address of the NAS' web interface)".into(),
        )
        .field("fingerprint"));
    }
    let sid = crate::devices::load_sid(&svc.state_dir.join("nas-sid")).map_err(Error::internal)?;
    let device = match_device(&super::restorable(&svc), &pc);
    let now = now_ms();
    let (id, token, nonce, expires_ms) = {
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
        // made after the commitment arrived
        let nonce = hex(&random(16)?);
        let view = RestoreRequestView {
            id: id.clone(),
            pc,
            device,
            from_ip: ip.map(|i| i.to_string()).unwrap_or_default(),
            tls,
            same_pc: 0,
            created_ms: now,
            expires_ms: now + RESTORE_REQUEST_TTL_MS,
        };
        let expires_ms = view.expires_ms;
        reqs.push(Req {
            id: id.clone(),
            poll_sha: sha_hex(token.as_bytes()),
            source,
            commit,
            nonce: nonce.clone(),
            fp: if seen.is_empty() { String::new() } else { own },
            code: None,
            view,
            state: RestoreRequestState::Pending,
            busy: false,
            wrong: 0,
            polls: 0,
            approval: None,
            code_sha: None,
            keep_until: expires_ms + KEEP_ENDED_MS,
        });
        (id, token, nonce, expires_ms)
    };
    timer(&svc, id.clone(), now + RESTORE_REVEAL_WITHIN_MS, true);
    timer(&svc, id.clone(), expires_ms, false);
    Ok((
        StatusCode::CREATED,
        Json(RestoreRequestCreated {
            id,
            poll_token: token,
            nonce,
            sid,
            expires_ms,
        }),
    )
        .into_response())
}

/// The token of a TUI call on request `id`: `Ok` when it holds the request's poll token
/// (never slowed or locked out: a valid 256-bit token is no guess); otherwise the address'
/// lockout applies and the miss counts like a wrong code (404).
async fn tui_token(svc: &PcAgent, h: &HeaderMap, id: &str) -> Res<()> {
    let token = bearer(h).unwrap_or_default();
    if svc.approvals.verify(id, &token) {
        return Ok(());
    }
    let source = source_of(h);
    svc.check_rate(&source).await?;
    svc.failures.lock().unwrap_or_else(|p| p.into_inner()).record(&source);
    tracing::warn!(target: "rynas::audit", "pc backup: a restore request call with an unknown id or token");
    Err(not_found())
}

/// `POST /v1/pcbackup/agent/restore-requests/{id}/reveal` (poll token as bearer): the secret
/// behind the commitment. 204: administrators see the request now. 400 (the request is
/// dropped) when it does not match, 409 when already revealed.
pub async fn reveal_post(
    State(st): State<PaState>,
    h: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Result<Json<RestoreRequestReveal>, JsonRejection>,
) -> Res<StatusCode> {
    let svc = pa(&st)?;
    tui_token(&svc, &h, &id).await?;
    let secret = json_body(body)?.secret.trim().to_ascii_lowercase();
    let now = now_ms();
    let view = {
        let mut reqs = svc.approvals.lock();
        let i = reqs.iter().position(|x| x.id == id).ok_or_else(not_found)?;
        let x = &mut reqs[i];
        if x.code.is_some() {
            return Err(Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, "already revealed".into()));
        }
        if x.state_at(now) != RestoreRequestState::Pending {
            return Err(Error::new(StatusCode::GONE, error_codes::REQUEST_EXPIRED, "the restore request is over".into()));
        }
        if !is_hex(&secret, 64) || commitment(&secret) != x.commit {
            // the one chance is spent: a party in the middle cannot try other secrets
            reqs.remove(i);
            return Err(Error::invalid("secret", "the secret does not match the commitment"));
        }
        x.code = Some(compare_code(&secret, &x.nonce, &x.fp));
        let x = &reqs[i];
        RestoreRequestView {
            same_pc: same_pc(&reqs, x, now),
            ..x.view.clone()
        }
    };
    tracing::info!(
        target: "rynas::audit",
        "pc backup: restore request {} for \"{}\" from {}{} (matches {})",
        view.id,
        view.pc,
        view.from_ip,
        if view.tls { "" } else { " over plain HTTP" },
        view.device.as_deref().unwrap_or("no single PC")
    );
    // one push per request, but at most one every PUSH_EVERY while others already wait
    let others_wait = svc.approvals.pending().len() > 1;
    let push_now = {
        let mut last = svc.approvals.last_push.lock().unwrap_or_else(|p| p.into_inner());
        let ok = !others_wait || last.is_none_or(|t| t.elapsed() >= PUSH_EVERY);
        if ok {
            *last = Some(Instant::now());
        }
        ok
    };
    let ev = request_event(&view);
    match svc.restore_push.get() {
        Some(push) if push_now => push(ev, push_action(&view)),
        _ => svc.notifier.emit(ev),
    }
    svc.approvals.publish(EV_REQUEST, &view);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, Default)]
pub struct PollQuery {
    /// Seconds to wait for a decision (at most [`RESTORE_REQUEST_POLL_WAIT_S`]).
    #[serde(default)]
    pub wait: Option<u64>,
}

/// One long poll of a request, counted while it runs ([`MAX_POLLS`]).
struct PollSlot<'a>(&'a Approvals, String);

impl Drop for PollSlot<'_> {
    fn drop(&mut self) {
        if let Some(x) = self.0.lock().iter_mut().find(|x| x.id == self.1) {
            x.polls = x.polls.saturating_sub(1);
        }
    }
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
    tui_token(&svc, &h, &id).await?;
    let wait = q.map(|Query(q)| q.wait.unwrap_or(0)).unwrap_or(0).min(RESTORE_REQUEST_POLL_WAIT_S);
    let _slot = {
        let mut r = svc.approvals.lock();
        let x = r.iter_mut().find(|x| x.id == id).ok_or_else(not_found)?;
        if x.polls >= MAX_POLLS {
            return Err(Error::new(StatusCode::TOO_MANY_REQUESTS, error_codes::RATE_LIMITED, "this request is already being polled".into()));
        }
        x.polls += 1;
        PollSlot(&svc.approvals, id.clone())
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let changed = svc.approvals.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let p = svc.approvals.status(&id).ok_or_else(not_found)?;
        if p.state != RestoreRequestState::Pending || tokio::time::Instant::now() >= deadline {
            return Ok(Json(p));
        }
        // a decision, a withdrawal and the request's own end (its timer) all wake this
        let _ = tokio::time::timeout_at(deadline, changed).await;
    }
}

/// `DELETE /v1/pcbackup/agent/restore-requests/{id}` (poll token as bearer): the TUI
/// withdraws its request (Esc). An issued restore code is revoked unless it was used. 204
/// also for a request that had ended.
pub async fn cancel_delete(State(st): State<PaState>, h: HeaderMap, UrlPath(id): UrlPath<String>) -> Res<StatusCode> {
    let svc = pa(&st)?;
    tui_token(&svc, &h, &id).await?;
    let now = now_ms();
    // one lock: the state and the code are taken together (an approval issuing its code
    // meanwhile sees the request withdrawn and revokes that code itself)
    let (was, sha, view) = {
        let mut r = svc.approvals.lock();
        let Some(i) = r.iter().position(|x| x.id == id) else { return Ok(StatusCode::NO_CONTENT) };
        let x = &mut r[i];
        let was = x.state_at(now);
        let sha = x.code_sha.take();
        let view = x.view.clone();
        let shown = x.code.is_some();
        if was == RestoreRequestState::Pending {
            x.state = RestoreRequestState::Canceled;
            x.keep_until = now + KEEP_ENDED_MS;
        } else {
            r.remove(i);
        }
        (if shown { Some(was) } else { None }, sha, view)
    };
    if let Some(sha) = sha {
        svc.edit(|r| {
            r.restore_codes.retain(|c| c.code_sha != sha);
            Ok(())
        })?;
        tracing::info!(target: "rynas::audit", "pc backup: restore request {id} withdrawn after its approval: its code is revoked");
    }
    match was {
        Some(RestoreRequestState::Pending) => {
            tracing::info!(target: "rynas::audit", "pc backup: restore request {id} for \"{}\" withdrawn", view.pc);
            ended(&svc, &view, RestoreRequestState::Canceled);
        }
        _ => svc.approvals.changed.notify_waiters(),
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/pcbackup/restore-requests` (administrators): the pending requests.
pub async fn list_get(State(st): State<PaState>) -> Res<Json<Vec<RestoreRequestView>>> {
    let svc = pa(&st)?;
    Ok(Json(svc.approvals.pending()))
}

/// The digits of a typed code.
fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// Constant-time comparison of two short strings.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `POST /v1/pcbackup/restore-requests/{id}/approve` (administrators): body
/// [`RestoreRequestApprove`] with the code typed from the PC's screen. 204; 400
/// `code_mismatch` (after [`RESTORE_MAX_WRONG_CODES`] the request is denied), 400 `device`
/// missing, 404 unknown, 410 expired, 409 already decided; the PC's own errors (404 no such
/// PC, 409 nothing to restore from).
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
    let typed = digits(&want.code);
    // checked and reserved under the lock; the code's disk write happens outside it
    let reserved = {
        let mut reqs = svc.approvals.lock();
        let x = reqs.iter_mut().find(|x| x.id == id).ok_or_else(not_found)?;
        check_pending(x, now)?;
        if typed.is_empty() {
            return Err(Error::invalid("code", "type the six digits the PC's screen shows"));
        }
        if !x.code.as_deref().is_some_and(|c| same(c, &typed)) {
            x.wrong += 1;
            if x.wrong >= RESTORE_MAX_WRONG_CODES {
                x.state = RestoreRequestState::Denied;
                x.keep_until = now + KEEP_ENDED_MS;
                Err(x.view.clone())
            } else {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    error_codes::CODE_MISMATCH,
                    "not the code the PC's screen shows".into(),
                )
                .field("code"));
            }
        } else {
            let device = want
                .device
                .filter(|d| !d.is_empty())
                .or_else(|| x.view.device.clone())
                .ok_or_else(|| Error::invalid("device", "choose the PC whose backups the restore may read"))?;
            if !rynas_api_types::pcagent::valid_device(&device) {
                return Err(Error::new(StatusCode::NOT_FOUND, error_codes::NOT_FOUND, "no such PC".into()));
            }
            x.busy = true;
            Ok((x.view.clone(), device))
        }
    };
    let (view, device) = match reserved {
        Ok(v) => v,
        Err(view) => {
            audit(&who, "typed wrong codes for the restore request, denied", &format!("{id} of \"{}\" from {}", view.pc, view.from_ip));
            ended(&svc, &view, RestoreRequestState::Denied);
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                error_codes::CODE_MISMATCH,
                "not the code the PC's screen shows: too many wrong codes, the request is denied".into(),
            )
            .field("code"));
        }
    };
    // the very code the web interface makes (one-time, read-only, this PC, 1 hour)
    let issued = super::issue_restore_code(&svc, &device);
    let revoke = {
        let mut reqs = svc.approvals.lock();
        let x = reqs.iter_mut().find(|x| x.id == id);
        match (x, issued) {
            (Some(x), Ok((code, expires_ms))) if x.state == RestoreRequestState::Pending => {
                x.busy = false;
                x.state = RestoreRequestState::Approved;
                x.code_sha = Some(code_hash(&code));
                x.approval = Some(RestoreApproval {
                    code,
                    device: device.clone(),
                    expires_ms,
                });
                x.keep_until = expires_ms;
                None
            }
            (x, Ok((code, _))) => {
                // withdrawn or gone meanwhile: the code must not live on
                if let Some(x) = x {
                    x.busy = false;
                }
                Some(code_hash(&code))
            }
            (x, Err(e)) => {
                if let Some(x) = x {
                    x.busy = false;
                }
                return Err(e);
            }
        }
    };
    if let Some(sha) = revoke {
        svc.edit(|r| {
            r.restore_codes.retain(|c| c.code_sha != sha);
            Ok(())
        })?;
        return Err(Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, "the restore request was withdrawn meanwhile".into()));
    }
    audit(&who, "approved the restore request", &format!("{id} of \"{}\" from {} for {device}", view.pc, view.from_ip));
    svc.notifier.emit(approved_event(&view, &device, &who));
    ended(&svc, &view, RestoreRequestState::Approved);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/pcbackup/restore-requests/{id}/deny` (administrators; body optional
/// [`RestoreRequestDeny`]): 204; 404, 410, 409 as for an approval. `block`: new requests from
/// that address are refused for [`BLOCK_FOR`].
pub async fn deny_post(State(st): State<PaState>, h: HeaderMap, UrlPath(id): UrlPath<String>, body: Bytes) -> Res<StatusCode> {
    let svc = pa(&st)?;
    let who = user(&h)?;
    let want: RestoreRequestDeny = if body.iter().all(u8::is_ascii_whitespace) {
        RestoreRequestDeny::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| Error::new(StatusCode::BAD_REQUEST, error_codes::INVALID_JSON, e.to_string()))?
    };
    let (view, source) = svc.approvals.end(&id, RestoreRequestState::Denied)?;
    if want.block {
        svc.approvals.block(&source);
    }
    let what = if want.block { "denied the restore request and blocked its address" } else { "denied the restore request" };
    audit(&who, what, &format!("{id} of \"{}\" from {}", view.pc, view.from_ip));
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
        assert_eq!(match_device(&pcs, "desk2").as_deref(), Some("desk2"));
        // two PCs carry that host name: the approver chooses
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
    fn compare_codes_follow_secret_nonce_and_certificate() {
        let fp = "AB:CD:".repeat(15) + "AB:CD";
        let s = "11".repeat(32);
        let a = compare_code(&s, "00ff", &fp);
        assert_eq!(a.len(), 6);
        assert!(a.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(compare_code(&s, "00ff", &fp.to_lowercase().replace(':', "")), a);
        assert_ne!(compare_code(&s, "00ff", &fp.replace("AB", "AC")), a);
        assert_ne!(compare_code(&s, "00fe", &fp), a);
        assert_ne!(compare_code(&"12".repeat(32), "00ff", &fp), a);
        assert_eq!(commitment(&s).len(), 64);
        assert_ne!(commitment(&s), commitment(&"12".repeat(32)));
    }

    #[test]
    fn the_creation_limit_counts_per_client_and_in_all() {
        let mut c = Created::default();
        for _ in 0..MAX_CREATED_PER_SOURCE {
            c.admit("192.0.2.1").unwrap();
        }
        assert!(c.admit("192.0.2.1").is_err());
        c.admit("192.0.2.2").unwrap();
        let mut c = Created::default();
        for i in 0..MAX_CREATED {
            c.admit(&format!("198.51.100.{}", i % 250)).unwrap();
        }
        assert!(c.admit("203.0.113.9").is_err(), "everybody together is limited too");
    }

    #[test]
    fn typed_codes() {
        assert_eq!(digits(" 482 913 "), "482913");
        assert!(same("482913", "482913"));
        assert!(!same("482913", "482914"));
        assert!(!same("482913", "48291"));
    }
}
