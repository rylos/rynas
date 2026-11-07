//! Docker events → notification transitions (D-NOTIFY, only on a change of state):
//! a container turns unhealthy / healthy again, runs out of memory, restarts in a loop (N
//! restarts by the engine in M minutes) and calms down; the data-root filesystem fills up and
//! frees again; an image update shows up (once per new registry digest).
//!
//! Containers are tracked by name: compose keeps the name when it recreates a container.

use std::collections::{HashMap, HashSet, VecDeque};

use bollard::models as bm;

/// The container events rynas follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Unhealthy,
    Healthy,
    Oom,
    /// The main process ended (`exitCode`).
    Die(Option<i64>),
    Start,
    /// Stopped on purpose (`docker stop`, compose down, a restart by hand).
    Stop,
    Destroy,
}

/// One container event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ev {
    pub kind: Kind,
    pub name: String,
    pub stack: Option<String>,
    pub image: String,
    pub time_s: i64,
}

/// The container event of an engine event message, when rynas follows it.
pub fn parse_event(e: &bm::EventMessage) -> Option<Ev> {
    if e.typ != Some(bm::EventMessageTypeEnum::CONTAINER) {
        return None;
    }
    let action = e.action.as_deref()?;
    let attrs = e
        .actor
        .as_ref()
        .and_then(|a| a.attributes.clone())
        .unwrap_or_default();
    let kind = match action {
        "health_status: unhealthy" => Kind::Unhealthy,
        "health_status: healthy" => Kind::Healthy,
        "health_status" => match attrs.get("health_status").map(String::as_str) {
            Some("unhealthy") => Kind::Unhealthy,
            Some("healthy") => Kind::Healthy,
            _ => return None,
        },
        "oom" => Kind::Oom,
        "die" => Kind::Die(attrs.get("exitCode").and_then(|c| c.parse().ok())),
        "start" => Kind::Start,
        "stop" | "kill" => Kind::Stop,
        "destroy" => Kind::Destroy,
        _ => return None,
    };
    Some(Ev {
        kind,
        name: attrs.get("name").cloned().unwrap_or_default(),
        stack: attrs.get(crate::model::LABEL_PROJECT).cloned(),
        image: attrs.get("image").cloned().unwrap_or_default(),
        time_s: e.time.unwrap_or_else(crate::now_s),
    })
}

/// A transition to notify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    Unhealthy {
        name: String,
        stack: Option<String>,
    },
    Recovered {
        name: String,
        stack: Option<String>,
    },
    Oom {
        name: String,
        stack: Option<String>,
    },
    CrashLoop {
        name: String,
        stack: Option<String>,
        restarts: u32,
        minutes: u32,
    },
    CrashLoopOver {
        name: String,
        stack: Option<String>,
    },
}

#[derive(Default)]
struct Track {
    stack: Option<String>,
    /// A `die` not (yet) explained by a stop on purpose.
    died_at: Option<i64>,
    restarts: VecDeque<i64>,
    looping: bool,
    unhealthy: bool,
    last_oom: Option<i64>,
}

/// Follows container events and answers the transitions.
pub struct Tracker {
    /// Restarts by the engine within `window_s` that make a crash loop.
    pub restarts: u32,
    pub window_s: i64,
    by_name: HashMap<String, Track>,
}

impl Tracker {
    pub fn new(restarts: u32, window_min: u32) -> Self {
        Self {
            restarts: restarts.max(2),
            window_s: i64::from(window_min.max(1)) * 60,
            by_name: HashMap::new(),
        }
    }

    pub fn set_limits(&mut self, restarts: u32, window_min: u32) {
        self.restarts = restarts.max(2);
        self.window_s = i64::from(window_min.max(1)) * 60;
    }

    /// One event.
    pub fn on_event(&mut self, ev: &Ev) -> Vec<Alert> {
        if ev.name.is_empty() {
            return Vec::new();
        }
        let window = self.window_s;
        let limit = self.restarts;
        let t = self.by_name.entry(ev.name.clone()).or_default();
        if ev.stack.is_some() {
            t.stack = ev.stack.clone();
        }
        let mut out = Vec::new();
        match ev.kind {
            Kind::Unhealthy if !t.unhealthy => {
                t.unhealthy = true;
                let (name, stack) = (ev.name.clone(), t.stack.clone());
                out.push(Alert::Unhealthy { name, stack });
            }
            Kind::Healthy if t.unhealthy => {
                t.unhealthy = false;
                let (name, stack) = (ev.name.clone(), t.stack.clone());
                out.push(Alert::Recovered { name, stack });
            }
            Kind::Unhealthy | Kind::Healthy => {}
            Kind::Oom => {
                if t.last_oom.is_none_or(|l| ev.time_s - l >= window) {
                    t.last_oom = Some(ev.time_s);
                    let (name, stack) = (ev.name.clone(), t.stack.clone());
                    out.push(Alert::Oom { name, stack });
                }
            }
            Kind::Die(_) => t.died_at = Some(ev.time_s),
            Kind::Stop => t.died_at = None,
            Kind::Start => {
                if t.died_at.take().is_some() {
                    t.restarts.push_back(ev.time_s);
                    while t.restarts.front().is_some_and(|x| ev.time_s - x > window) {
                        t.restarts.pop_front();
                    }
                    if !t.looping && t.restarts.len() as u32 >= limit {
                        t.looping = true;
                        let (name, stack) = (ev.name.clone(), t.stack.clone());
                        out.push(Alert::CrashLoop {
                            name,
                            stack,
                            restarts: t.restarts.len() as u32,
                            minutes: (window / 60) as u32,
                        });
                    }
                }
            }
            Kind::Destroy => {
                // a recreated container (same name) starts clean; an unhealthy or looping
                // one that is gone resolves at the next sweep
                t.died_at = None;
                t.restarts.clear();
            }
        }
        out
    }

    /// Periodic sweep: crash loops without a restart for a whole window are over; unhealthy
    /// containers that no longer exist (`present`) are resolved.
    pub fn sweep(&mut self, now_s: i64, present: &HashSet<String>) -> Vec<Alert> {
        let window = self.window_s;
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for (name, t) in self.by_name.iter_mut() {
            while t.restarts.front().is_some_and(|x| now_s - x > window) {
                t.restarts.pop_front();
            }
            if t.looping && t.restarts.is_empty() {
                t.looping = false;
                out.push(Alert::CrashLoopOver {
                    name: name.clone(),
                    stack: t.stack.clone(),
                });
            }
            if !present.contains(name) {
                if t.unhealthy {
                    t.unhealthy = false;
                    out.push(Alert::Recovered {
                        name: name.clone(),
                        stack: t.stack.clone(),
                    });
                }
                if t.looping {
                    t.looping = false;
                    out.push(Alert::CrashLoopOver {
                        name: name.clone(),
                        stack: t.stack.clone(),
                    });
                }
                if !t.unhealthy && !t.looping {
                    gone.push(name.clone());
                }
            }
        }
        for g in gone {
            self.by_name.remove(&g);
        }
        out.sort_by_key(alert_name);
        out
    }

    /// The first reading after a (re)connection: containers unhealthy now are reported
    /// (first observation counts as a transition; the engine ignores repeats), those that
    /// recovered while nobody listened are resolved.
    pub fn seed(&mut self, unhealthy_now: &[(String, Option<String>)]) -> Vec<Alert> {
        let now: HashSet<&str> = unhealthy_now.iter().map(|(n, _)| n.as_str()).collect();
        let mut out = Vec::new();
        for (name, stack) in unhealthy_now {
            let t = self.by_name.entry(name.clone()).or_default();
            t.stack = stack.clone().or(t.stack.take());
            if !t.unhealthy {
                t.unhealthy = true;
                out.push(Alert::Unhealthy {
                    name: name.clone(),
                    stack: t.stack.clone(),
                });
            }
        }
        for (name, t) in self.by_name.iter_mut() {
            if t.unhealthy && !now.contains(name.as_str()) {
                t.unhealthy = false;
                out.push(Alert::Recovered {
                    name: name.clone(),
                    stack: t.stack.clone(),
                });
            }
        }
        out
    }
}

fn alert_name(a: &Alert) -> String {
    match a {
        Alert::Unhealthy { name, .. }
        | Alert::Recovered { name, .. }
        | Alert::Oom { name, .. }
        | Alert::CrashLoop { name, .. }
        | Alert::CrashLoopOver { name, .. } => name.clone(),
    }
}

/// Data-root filesystem fill level with 2 points of hysteresis.
#[derive(Debug, Default)]
pub struct DiskWatch {
    full: Option<bool>,
}

/// A fill-level transition: `Some(true)` full, `Some(false)` back under the limit.
impl DiskWatch {
    pub fn check(&mut self, used_pct: f64, limit_pct: u8) -> Option<bool> {
        let limit = f64::from(limit_pct);
        let full = match self.full {
            Some(true) => used_pct >= limit - 2.0,
            _ => used_pct >= limit,
        };
        let changed = self.full != Some(full);
        let first = self.full.is_none();
        self.full = Some(full);
        // the first reading reports only a full disk
        (changed && (!first || full)).then_some(full)
    }
}

/// Image updates already notified: image → registry digest.
#[derive(Debug, Default)]
pub struct UpdateWatch {
    seen: HashMap<String, String>,
}

impl UpdateWatch {
    /// `true` when `image` has an update at `remote` not notified before.
    pub fn is_new(&mut self, image: &str, remote: &str) -> bool {
        if self.seen.get(image).map(String::as_str) == Some(remote) {
            return false;
        }
        self.seen.insert(image.to_string(), remote.to_string());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: Kind, name: &str, t: i64) -> Ev {
        Ev {
            kind,
            name: name.into(),
            stack: Some("s".into()),
            image: "x".into(),
            time_s: t,
        }
    }

    #[test]
    fn health_transitions_once() {
        let mut tr = Tracker::new(5, 10);
        assert_eq!(tr.on_event(&ev(Kind::Healthy, "a", 0)), vec![]);
        assert!(matches!(
            tr.on_event(&ev(Kind::Unhealthy, "a", 1))[..],
            [Alert::Unhealthy { .. }]
        ));
        assert_eq!(tr.on_event(&ev(Kind::Unhealthy, "a", 2)), vec![]);
        assert!(matches!(
            tr.on_event(&ev(Kind::Healthy, "a", 3))[..],
            [Alert::Recovered { .. }]
        ));
        assert_eq!(tr.on_event(&ev(Kind::Healthy, "a", 4)), vec![]);
    }

    #[test]
    fn crash_loop_counts_engine_restarts_only() {
        let mut tr = Tracker::new(3, 10);
        // a restart by hand: kill, die, stop, start
        for k in [Kind::Stop, Kind::Die(Some(143)), Kind::Stop, Kind::Start] {
            assert!(tr.on_event(&ev(k, "a", 0)).is_empty());
        }
        let mut out = Vec::new();
        for i in 0..3 {
            out.extend(tr.on_event(&ev(Kind::Die(Some(1)), "a", 10 + i * 30)));
            out.extend(tr.on_event(&ev(Kind::Start, "a", 11 + i * 30)));
        }
        assert_eq!(
            out,
            vec![Alert::CrashLoop {
                name: "a".into(),
                stack: Some("s".into()),
                restarts: 3,
                minutes: 10
            }]
        );
        // more restarts: no repeat
        tr.on_event(&ev(Kind::Die(Some(1)), "a", 200));
        assert!(tr.on_event(&ev(Kind::Start, "a", 201)).is_empty());
        let present: HashSet<String> = ["a".to_string()].into();
        assert!(tr.sweep(500, &present).is_empty());
        assert_eq!(
            tr.sweep(201 + 601, &present),
            vec![Alert::CrashLoopOver {
                name: "a".into(),
                stack: Some("s".into())
            }]
        );
    }

    #[test]
    fn restarts_outside_the_window_do_not_add_up() {
        let mut tr = Tracker::new(3, 1);
        let mut out = Vec::new();
        for i in 0..5 {
            out.extend(tr.on_event(&ev(Kind::Die(Some(1)), "a", i * 100)));
            out.extend(tr.on_event(&ev(Kind::Start, "a", i * 100 + 1)));
        }
        assert!(out.is_empty());
    }

    #[test]
    fn oom_dedup_and_gone_containers() {
        let mut tr = Tracker::new(5, 10);
        assert_eq!(tr.on_event(&ev(Kind::Oom, "a", 0)).len(), 1);
        assert!(tr.on_event(&ev(Kind::Oom, "a", 60)).is_empty());
        assert_eq!(tr.on_event(&ev(Kind::Oom, "a", 700)).len(), 1);
        tr.on_event(&ev(Kind::Unhealthy, "b", 0));
        tr.on_event(&ev(Kind::Destroy, "b", 1));
        let out = tr.sweep(2, &HashSet::new());
        assert_eq!(
            out,
            vec![Alert::Recovered {
                name: "b".into(),
                stack: Some("s".into())
            }]
        );
        assert!(tr.sweep(3, &HashSet::new()).is_empty());
    }

    #[test]
    fn seed_after_reconnect() {
        let mut tr = Tracker::new(5, 10);
        tr.on_event(&ev(Kind::Unhealthy, "old", 0));
        let out = tr.seed(&[("new".into(), None)]);
        assert_eq!(
            out,
            vec![
                Alert::Unhealthy {
                    name: "new".into(),
                    stack: None
                },
                Alert::Recovered {
                    name: "old".into(),
                    stack: Some("s".into())
                }
            ]
        );
        assert!(tr.seed(&[("new".into(), None)]).is_empty());
    }

    #[test]
    fn parse_engine_events() {
        let m: bm::EventMessage = serde_json::from_str(
            r#"{"Type":"container","Action":"health_status: unhealthy","Actor":{"ID":"abc","Attributes":{"name":"immich_server","image":"x","com.docker.compose.project":"immich"}},"time":1790000000}"#,
        )
        .unwrap();
        let e = parse_event(&m).unwrap();
        assert_eq!(
            (e.kind, e.name.as_str(), e.stack.as_deref(), e.time_s),
            (
                Kind::Unhealthy,
                "immich_server",
                Some("immich"),
                1_790_000_000
            )
        );
        let m: bm::EventMessage = serde_json::from_str(
            r#"{"Type":"container","Action":"die","Actor":{"ID":"abc","Attributes":{"name":"a","exitCode":"137"}},"time":1}"#,
        )
        .unwrap();
        assert_eq!(parse_event(&m).unwrap().kind, Kind::Die(Some(137)));
        let m: bm::EventMessage =
            serde_json::from_str(r#"{"Type":"network","Action":"connect","time":1}"#).unwrap();
        assert!(parse_event(&m).is_none());
        let m: bm::EventMessage =
            serde_json::from_str(r#"{"Type":"container","Action":"exec_start: sh","time":1}"#)
                .unwrap();
        assert!(parse_event(&m).is_none());
    }

    #[test]
    fn disk_and_updates() {
        let mut d = DiskWatch::default();
        assert_eq!(d.check(50.0, 90), None);
        assert_eq!(d.check(91.0, 90), Some(true));
        assert_eq!(d.check(89.0, 90), None, "hysteresis");
        assert_eq!(d.check(87.5, 90), Some(false));
        let mut d = DiskWatch::default();
        assert_eq!(d.check(95.0, 90), Some(true), "full at the first reading");
        let mut u = UpdateWatch::default();
        assert!(u.is_new("nginx:1", "sha256:a"));
        assert!(!u.is_new("nginx:1", "sha256:a"));
        assert!(u.is_new("nginx:1", "sha256:b"));
    }
}
