//! Docker module state shared by the shell (the navigation entry), the dashboard widget and
//! the Docker page (`docs/phase2/docker-apps.md` §5.1).
//!
//! - `GET /api/v1/docker/status` is read at sign-in, on a `resync` of the `docker` stream and
//!   when a `docker.*` task ends; the navigation entry and the widget exist only while Docker
//!   is installed or being installed ([`Docker::present`]). Without Docker nothing else asks
//!   the Docker routes.
//! - `docker.stats` events (every 5 s, the latest on connect) feed [`Docker::stats`]: the
//!   widget and the CPU/memory cells of the containers table read it in place.
//! - `docker.changed` events bump [`Docker::generation`]: the page's lists read again;
//!   [`Docker::ended`] counts the `docker.*` tasks that ended (the update report changes only
//!   then).

use std::cell::Cell;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::docker::{ContainerStat, DockerStatus};
use serde::Deserialize;

use crate::api;

/// One running container in [`Stats::usage`]: id, % of one CPU, bytes.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Usage {
    pub id: String,
    pub cpu: f32,
    pub mem: u64,
}

/// The `docker.stats` event (`rynas_api_types::docker::DockerStats`). `usage`, every running
/// container of the sample, is read when the server sends it; older servers send only `top`.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Stats {
    pub running: u32,
    pub total: u32,
    /// Sum over running containers, % of one CPU.
    pub cpu_pct: f32,
    pub cpus: u32,
    pub mem_bytes: u64,
    pub mem_total_bytes: u64,
    /// Top 5 by CPU then memory.
    pub top: Vec<ContainerStat>,
    #[serde(default)]
    pub usage: Vec<Usage>,
}

impl Stats {
    /// CPU (% of the whole machine) and memory of one container: `usage`, else `top`.
    pub fn of(&self, id: &str) -> Option<(f64, u64)> {
        let cpus = self.cpus.max(1) as f64;
        match self.usage.iter().find(|u| u.id == id) {
            Some(u) => Some((u.cpu as f64 / cpus, u.mem)),
            None => self.top.iter().find(|x| x.id == id).map(|x| (x.cpu_pct as f64 / cpus, x.mem_bytes)),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Docker {
    /// The engine as rynasd sees it (`None` until the first answer).
    pub status: RwSignal<Option<DockerStatus>>,
    /// Code of the last failed status read.
    pub status_err: RwSignal<Option<String>>,
    /// The latest aggregated sample.
    pub stats: RwSignal<Option<Stats>>,
    /// Bumped by `docker.changed`, a `resync` and the end of a `docker.*` task.
    pub generation: RwSignal<u64>,
    /// Bumped when a `docker.*` task ends or the stream resyncs (not by `docker.changed`).
    pub ended: RwSignal<u64>,
}

thread_local! {
    static DOCKER: Cell<Option<Docker>> = const { Cell::new(None) };
}

/// The app's Docker store (also reachable from stream callbacks).
pub fn docker() -> Docker {
    DOCKER.with(|c| match c.get() {
        Some(d) => d,
        None => {
            let d = Docker {
                status: RwSignal::new(None),
                status_err: RwSignal::new(None),
                stats: RwSignal::new(None),
                generation: RwSignal::new(0),
                ended: RwSignal::new(0),
            };
            c.set(Some(d));
            d
        }
    })
}

/// Creates the store outside any reactive owner (called from `main` before mounting).
pub fn init() {
    docker();
}

/// Equal apart from the read time (a status read that changed nothing changes no signal).
fn same(a: &DockerStatus, b: &DockerStatus) -> bool {
    a.read_at_ms == b.read_at_ms || DockerStatus { read_at_ms: b.read_at_ms, ..a.clone() } == *b
}

/// The one reader of `DockerStatus` (kept out of line: one copy of the deserializer).
#[inline(never)]
fn parse_status(text: &str) -> Option<DockerStatus> {
    serde_json::from_str(text).ok()
}

/// A `docker.stats` event (one copy of the deserializer).
#[inline(never)]
pub fn parse_stats(text: &str) -> Option<Stats> {
    serde_json::from_str(text).ok()
}

/// Wait after a change before reading a list again (a compose up sends several hints).
const RELOAD_DEBOUNCE_MS: u64 = 300;

/// Runs `reload` after `trigger` changes ([`Docker::generation`] or [`Docker::ended`]):
/// debounced, and only while `visible`; a change seen while hidden runs it once visible again.
pub fn follow(trigger: RwSignal<u64>, visible: Signal<bool>, reload: Callback<()>) {
    let read = StoredValue::new(trigger.get_untracked());
    let pending = StoredValue::new(false);
    Effect::new(move || {
        let now = trigger.get();
        if !visible.get() || read.get_value() == now {
            return;
        }
        read.set_value(now);
        if pending.get_value() {
            return;
        }
        pending.set_value(true);
        set_timeout(
            move || {
                if pending.try_set_value(false).is_none() {
                    reload.try_run(());
                }
            },
            std::time::Duration::from_millis(RELOAD_DEBOUNCE_MS),
        );
    });
}

impl Docker {
    /// Docker is installed, or its installation runs: the navigation entry and the widget.
    pub fn present(self) -> bool {
        self.status.with(|s| s.as_ref().is_some_and(|s| s.installed || s.task_id.is_some()))
    }

    /// The engine answers (lists and actions work).
    pub fn running(self) -> bool {
        self.status.with(|s| s.as_ref().is_some_and(|s| s.running))
    }

    /// Installed and known not to answer (the widget says so instead of waiting).
    pub fn stopped(self) -> bool {
        self.status.with(|s| s.as_ref().is_some_and(|s| s.installed && !s.running))
    }

    pub fn refresh(self) {
        spawn_local(async move {
            let r = api::get_text("/api/v1/docker/status").await;
            match r.map(|t| parse_status(&t)) {
                Ok(Some(s)) => {
                    if self.status_err.with_untracked(Option::is_some) {
                        let _ = self.status_err.try_set(None);
                    }
                    if self.status.with_untracked(|cur| cur.as_ref().is_none_or(|c| !same(c, &s))) {
                        let _ = self.status.try_set(Some(s));
                    }
                }
                Ok(None) => {
                    let _ = self.status_err.try_set(Some("decode".into()));
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => {
                    let _ = self.status_err.try_set(Some(e.code().to_string()));
                }
            }
        });
    }

    /// `docker.stats` event.
    pub fn on_stats(self, s: Stats) {
        if self.stats.with_untracked(|cur| cur.as_ref() != Some(&s)) {
            self.stats.set(Some(s));
        }
    }

    /// `docker.changed` event: the lists changed.
    pub fn on_changed(self) {
        self.generation.update(|g| *g += 1);
    }

    /// Missed events, or a `docker.*` task ended: read the status and the lists again.
    pub fn resync(self) {
        self.refresh();
        self.ended.update(|g| *g += 1);
        self.on_changed();
    }

    /// Signed out.
    pub fn clear(self) {
        self.status.set(None);
        self.status_err.set(None);
        self.stats.set(None);
    }
}
