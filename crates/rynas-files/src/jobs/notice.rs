//! The notification of a finished transfer (`files.job_done` / `files.job_failed`,
//! docs/phase2/mobile-devices.md §4): only for jobs that ran longer than [`NOTIFY_AFTER`], and
//! only when they ended as done, failed, or canceled with errors. The event carries the job's
//! owner in `user`: the notification engine delivers it to that user's own devices only.
//!
//! `title`/`body` are the English text; the fields rebuild them in every language
//! (`nev.files.job_*` in the interface's dictionary, `{kind}` and `{op}` read as closed sets).

use std::time::Duration;

use rynas_api_types::notify::{EventState, NewEvent, NotifyCategory, NotifySeverity};
use rynas_api_types::units::human_bytes;

use super::manager::{JobView, State};
use super::protocol::Op;

/// A job that ended sooner is not worth a message: its dialog showed the end.
pub const NOTIFY_AFTER: Duration = Duration::from_secs(30);

/// The job kind in the event (`kind`, and the closed-set keys `nev.files.job_*.kind.<kind>`);
/// `None` for the clean-up after an interrupted job, which nobody asked for.
fn kind_of(op: Op) -> Option<&'static str> {
    Some(match op {
        Op::Copy => "copy",
        Op::Move => "move",
        Op::Delete => "delete",
        Op::Compress => "compress",
        Op::Extract => "extract",
        Op::Sync => "sync",
        Op::Cleanup => return None,
    })
}

/// The English name of the operation in a title.
fn noun(kind: &str) -> &'static str {
    match kind {
        "copy" => "Copy",
        "move" => "Move",
        "delete" => "Delete",
        "compress" => "Compression",
        "extract" => "Extraction",
        _ => "Sync",
    }
}

/// The last part of a path ("/Video/Film.mkv" → "Film.mkv").
fn base_name(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    match t.rsplit_once('/') {
        Some((_, n)) if !n.is_empty() => n,
        _ if t.is_empty() => p,
        _ => t,
    }
}

/// The folder of a path ("/Video/Film.mkv" → "/Video", "/Film.mkv" → "/").
fn parent(p: &str) -> String {
    let t = p.trim_end_matches('/');
    match t.rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((d, _)) => d.into(),
    }
}

/// The event of a job that just ended after running for `ran`, owned by `owner`; `None`
/// when it gets none (too short, canceled cleanly, the clean-up kind).
pub fn finished_event(v: &JobView, owner: &str, ran: Duration) -> Option<NewEvent> {
    if ran <= NOTIFY_AFTER {
        return None;
    }
    let kind = kind_of(v.op)?;
    let (failed, state) = match v.state {
        State::Done => (false, "done"),
        State::Failed => (true, "failed"),
        State::Cancelled if !v.errors.is_empty() => (true, "canceled"),
        _ => return None,
    };
    let first = v.info.sources.first().map(String::as_str).unwrap_or("");
    // a deletion has no destination: where the items were
    let (op, dest) = match (&v.info.dest, v.op) {
        (Some(d), _) if v.op != Op::Delete => ("→", d.clone()),
        _ => ("from", parent(first)),
    };
    let size = human_bytes(v.progress.bytes_done.max(if failed { 0 } else { v.progress.bytes_total }));
    let minutes = ran.as_secs().div_ceil(60).max(1);
    let count = v.info.sources.len();
    let what = if count > 1 { format!("{count} items") } else { base_name(first).to_string() };
    let mut fields = vec![
        ("job_id", v.id.to_string()),
        ("state", state.to_string()),
        ("user", owner.to_string()),
        ("kind", kind.to_string()),
        ("op", kind.to_string()),
        ("dest", dest.clone()),
        ("size", size.clone()),
        ("minutes", minutes.to_string()),
    ];
    if count > 1 {
        fields.push(("count", count.to_string()));
    } else {
        fields.push(("name", what.clone()));
    }
    let (code, severity, title, body) = if failed {
        let error = v
            .errors
            .first()
            .map(|e| if e.path.is_empty() { e.msg.clone() } else { format!("{}: {}", e.path, e.msg) })
            .filter(|e| !e.trim().is_empty())
            .or_else(|| Some(v.msg.clone()).filter(|m| !m.trim().is_empty()))
            .unwrap_or_else(|| "the job failed".into());
        fields.push(("error", error.clone()));
        ("files.job_failed", NotifySeverity::Error, format!("{} failed", noun(kind)), error)
    } else {
        let body = format!("{what} {op} {dest} · {size} in {minutes} min");
        ("files.job_done", NotifySeverity::Info, format!("{} finished", noun(kind)), body)
    };
    Some(NewEvent {
        source: "files".into(),
        category: NotifyCategory::Files,
        severity,
        key: Some(format!("files:job:{}", v.id)),
        state: EventState::Oneshot,
        title,
        body,
        fields: fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        code: code.into(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::manager::{ErrorItem, Info};
    use crate::jobs::protocol::{Policy, Progress};

    fn view(op: Op, state: State, sources: &[&str], dest: Option<&str>) -> JobView {
        JobView {
            id: 42,
            op,
            info: Info { sources: sources.iter().map(|s| s.to_string()).collect(), dest: dest.map(str::to_string), permanent: false },
            state,
            created: 0,
            started: Some(0),
            finished: Some(1),
            active_ms: 0,
            progress: Progress { bytes_done: 49_177_336_627, bytes_total: 49_177_336_627, ..Progress::default() },
            conflict: None,
            errors: Vec::new(),
            msg: String::new(),
            code: String::new(),
            verify: false,
            limit: 0,
            policy: Policy::default(),
            samples: Vec::new(),
            interrupted: false,
            cleaning: false,
            task_id: None,
            stalled_s: 0,
            cancelling: false,
        }
    }

    const LONG: Duration = Duration::from_secs(7 * 60 - 20);

    #[test]
    fn only_long_jobs_that_ended_get_one() {
        let done = view(Op::Copy, State::Done, &["/Movies/Film.mkv"], Some("/Video"));
        assert!(finished_event(&done, "alice", Duration::from_secs(30)).is_none());
        assert!(finished_event(&done, "alice", Duration::from_secs(31)).is_some());
        for s in [State::Queued, State::Running, State::Paused, State::Conflict, State::Cancelled] {
            let v = view(Op::Copy, s, &["/a"], Some("/b"));
            assert!(finished_event(&v, "alice", LONG).is_none(), "{s:?}");
        }
        // the clean-up after a restart is nobody's request
        let clean = view(Op::Cleanup, State::Done, &["/a"], Some("/b"));
        assert!(finished_event(&clean, "alice", LONG).is_none());
    }

    #[test]
    fn a_finished_copy() {
        let v = view(Op::Copy, State::Done, &["/Movies/Film.mkv"], Some("/Video"));
        let ev = finished_event(&v, "alice", LONG).unwrap();
        assert_eq!((ev.code.as_str(), ev.severity, ev.category), ("files.job_done", NotifySeverity::Info, NotifyCategory::Files));
        assert_eq!(ev.source, "files");
        assert_eq!(ev.key.as_deref(), Some("files:job:42"));
        assert_eq!(ev.title, "Copy finished");
        assert_eq!(ev.body, "Film.mkv → /Video · 45.8 GiB in 7 min");
        let f = |k: &str| ev.fields.get(k).map(String::as_str);
        assert_eq!((f("job_id"), f("state"), f("user"), f("kind")), (Some("42"), Some("done"), Some("alice"), Some("copy")));
        assert_eq!((f("name"), f("count"), f("dest"), f("minutes")), (Some("Film.mkv"), None, Some("/Video"), Some("7")));
    }

    #[test]
    fn many_items_and_deletions() {
        let v = view(Op::Move, State::Done, &["/a/x", "/a/y", "/a/z"], Some("/b"));
        let ev = finished_event(&v, "bob", Duration::from_secs(40)).unwrap();
        assert_eq!(ev.title, "Move finished");
        assert_eq!(ev.body, "3 items → /b · 45.8 GiB in 1 min");
        assert_eq!((ev.fields.get("count").map(String::as_str), ev.fields.get("name")), (Some("3"), None));
        let v = view(Op::Delete, State::Done, &["/Video/old/"], None);
        let ev = finished_event(&v, "bob", LONG).unwrap();
        assert_eq!(ev.title, "Delete finished");
        assert!(ev.body.starts_with("old from /Video · "), "{}", ev.body);
    }

    #[test]
    fn failures_and_cancels_with_errors() {
        let mut v = view(Op::Extract, State::Failed, &["/a/x.zip"], Some("/b"));
        v.msg = "no space left on the destination".into();
        let ev = finished_event(&v, "alice", LONG).unwrap();
        assert_eq!((ev.code.as_str(), ev.severity), ("files.job_failed", NotifySeverity::Error));
        assert_eq!(ev.title, "Extraction failed");
        assert_eq!(ev.body, "no space left on the destination");
        assert_eq!(ev.fields["state"], "failed");
        // canceled after errors: a failure with state canceled, the first error as the body
        let mut v = view(Op::Copy, State::Cancelled, &["/a/x"], Some("/b"));
        v.errors.push(ErrorItem { path: "/a/x/1.jpg".into(), msg: "permission denied".into(), code: rynas_api_types::error_codes::FORBIDDEN.into() });
        v.errors.push(ErrorItem { path: "/a/x/2.jpg".into(), msg: "permission denied".into(), code: rynas_api_types::error_codes::FORBIDDEN.into() });
        let ev = finished_event(&v, "alice", LONG).unwrap();
        assert_eq!(ev.code, "files.job_failed");
        assert_eq!(ev.fields["state"], "canceled");
        assert_eq!(ev.body, "/a/x/1.jpg: permission denied");
        assert_eq!(ev.fields["error"], ev.body);
    }

    #[test]
    fn paths() {
        assert_eq!(base_name("/Video/Film.mkv"), "Film.mkv");
        assert_eq!(base_name("/Video/old/"), "old");
        assert_eq!(base_name("/"), "/");
        assert_eq!(parent("/Video/Film.mkv"), "/Video");
        assert_eq!(parent("/Film.mkv"), "/");
    }
}
