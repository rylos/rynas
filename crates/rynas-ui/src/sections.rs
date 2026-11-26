//! The optional sections of the interface (navigation entries that exist only on some
//! systems, [`rynas_api_types::sections`]): which exist now, known before the shell's first
//! frame so that no entry pops in a moment later.
//!
//! Sources, the first that knows wins:
//! 1. the session answer (`Session::sections`: rynas-web asks rynasd with a short deadline);
//! 2. the last sections seen in this browser (localStorage `rynas.sections`);
//! 3. the live reads ([`crate::docker`]'s status), which also keep them current afterwards
//!    (an install or an uninstall adds or removes the entry at once) and remembered.

use std::cell::Cell;

use leptos::prelude::*;
use rynas_api_types::{Session, sections};

use crate::prefs::{storage_get, storage_set};

const KEY: &str = "rynas.sections";

thread_local! {
    static KNOWN: Cell<Option<RwSignal<Option<Vec<String>>>>> = const { Cell::new(None) };
}

/// The known sections (`None`: nothing known yet), first filled from the browser's memory.
fn known() -> RwSignal<Option<Vec<String>>> {
    KNOWN.with(|c| match c.get() {
        Some(k) => k,
        None => {
            let k = RwSignal::new(decode(storage_get(KEY).as_deref()));
            c.set(Some(k));
            k
        }
    })
}

/// Creates the store outside any reactive owner (called from `main` before mounting).
pub fn init() {
    known();
}

/// The remembered text: the names separated by commas, `,` for "known: none".
fn encode(v: &[String]) -> String {
    if v.is_empty() { ",".to_string() } else { v.join(",") }
}

/// The remembered sections (`None`: nothing remembered).
fn decode(s: Option<&str>) -> Option<Vec<String>> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    Some(s.split(',').map(str::trim).filter(|n| !n.is_empty()).map(String::from).collect())
}

/// Whether section `name` exists now, tracked; `None` while nothing is known (the caller
/// falls back to its own read).
pub fn has(name: &str) -> Option<bool> {
    known().with(|k| k.as_ref().map(|v| v.iter().any(|s| s == name)))
}

fn set(v: Vec<String>) {
    let k = known();
    if k.with_untracked(|cur| cur.as_ref() != Some(&v)) {
        storage_set(KEY, &encode(&v));
        k.set(Some(v));
    }
}

/// A session answer: the sections it carries (when rynas-web knew them) become the known ones.
pub fn from_session(s: &Session) {
    if let Some(v) = &s.sections {
        set(v.clone());
    }
}

/// Docker's live status: the sections that follow Docker.
pub fn docker_seen(present: bool) {
    let cur = known().get_untracked().unwrap_or_default();
    set(sections::with_docker(&cur, present));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_text_round_trips() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for x in [v(&[]), v(&["docker"]), v(&["docker", "syncthing"])] {
            assert_eq!(decode(Some(&encode(&x))), Some(x));
        }
        assert_eq!(decode(None), None);
        assert_eq!(decode(Some("")), None, "an empty value is no memory");
    }
}
