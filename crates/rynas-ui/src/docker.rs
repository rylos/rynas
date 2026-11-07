//! Docker module state shared by the shell (the navigation entry), the dashboard widget and
//! the Docker page (`docs/phase2/docker-apps.md` §5.1).
//!
//! - `GET /api/v1/docker/status` is read at sign-in, on a `resync` of the `docker` stream and
//!   when a `docker.*` task ends; the navigation entry and the widget exist only while Docker
//!   is installed or being installed ([`Docker::present`]). Without Docker nothing else asks
//!   the Docker routes.
//! - `docker.stats` events (every 5 s, the latest on connect) feed [`Docker::stats`]: the
//!   widget and the CPU/memory cells of the containers table read it in place.
//! - `docker.changed` events bump [`Docker::generation`]: the page's lists read again.

use std::cell::Cell;

use leptos::prelude::*;
use leptos::task::spawn_local;
use rynas_api_types::docker::{DockerStats, DockerStatus};

use crate::api;

#[derive(Clone, Copy)]
pub struct Docker {
    /// The engine as rynasd sees it (`None` until the first answer).
    pub status: RwSignal<Option<DockerStatus>>,
    /// Code of the last failed status read.
    pub status_err: RwSignal<Option<String>>,
    /// The latest aggregated sample.
    pub stats: RwSignal<Option<DockerStats>>,
    /// Bumped by `docker.changed`, a `resync` and the end of a `docker.*` task.
    pub generation: RwSignal<u64>,
}

thread_local! {
    static DOCKER: Cell<Option<Docker>> = const { Cell::new(None) };
}

/// The app's Docker store (also reachable from stream callbacks).
pub fn docker() -> Docker {
    DOCKER.with(|c| match c.get() {
        Some(d) => d,
        None => {
            let d = Docker { status: RwSignal::new(None), status_err: RwSignal::new(None), stats: RwSignal::new(None), generation: RwSignal::new(0) };
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

impl Docker {
    /// Docker is installed, or its installation runs: the navigation entry and the widget.
    pub fn present(self) -> bool {
        self.status.with(|s| s.as_ref().is_some_and(|s| s.installed || s.task_id.is_some()))
    }

    /// The engine answers (lists and actions work).
    pub fn running(self) -> bool {
        self.status.with(|s| s.as_ref().is_some_and(|s| s.running))
    }

    pub fn refresh(self) {
        spawn_local(async move {
            match api::get::<DockerStatus>("/api/v1/docker/status").await {
                Ok(s) => {
                    if self.status_err.with_untracked(Option::is_some) {
                        let _ = self.status_err.try_set(None);
                    }
                    if self.status.with_untracked(|cur| cur.as_ref().is_none_or(|c| !same(c, &s))) {
                        let _ = self.status.try_set(Some(s));
                    }
                }
                Err(e) if e.status() == 401 => {}
                Err(e) => {
                    let _ = self.status_err.try_set(Some(e.code().to_string()));
                }
            }
        });
    }

    /// `docker.stats` event.
    pub fn on_stats(self, s: DockerStats) {
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
        self.on_changed();
    }

    /// Signed out.
    pub fn clear(self) {
        self.status.set(None);
        self.status_err.set(None);
        self.stats.set(None);
    }
}
