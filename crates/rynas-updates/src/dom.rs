//! `synodeb-dom status --json` (schema 1, fields only ever added; see
//! `docs/synodeb-integration.md`, S5).

use rynas_api_types::updates::{DomKernel, DomStatus, DomTrial};
use serde::Deserialize;

#[derive(Deserialize)]
struct Wire {
    schema: u32,
    #[serde(default)]
    entries: Vec<String>,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    fallback: Vec<String>,
    #[serde(default)]
    trial: Option<String>,
    #[serde(default)]
    running: Option<String>,
    #[serde(default)]
    outdated: Option<String>,
    #[serde(default)]
    kernels: Kernels,
    #[serde(default)]
    dom_free_kb: Option<u64>,
}

#[derive(Deserialize, Default)]
struct Kernels {
    #[serde(default, rename = "DEBIAN")]
    debian: Option<DomKernel>,
    #[serde(default, rename = "DEBIAN_OLD")]
    debian_old: Option<DomKernel>,
}

/// Parses the JSON; schemas other than 1 are refused (a newer one would say so).
pub fn parse_status(text: &str) -> Result<DomStatus, String> {
    let w: Wire =
        serde_json::from_str(text.trim()).map_err(|e| format!("synodeb-dom status: {e}"))?;
    if w.schema != 1 {
        return Err(format!("synodeb-dom status: unknown schema {}", w.schema));
    }
    let trial = match w.trial.as_deref() {
        Some("armed") => DomTrial::Armed,
        Some("trying") => DomTrial::Trying,
        _ => DomTrial::None,
    };
    Ok(DomStatus {
        entries: w.entries,
        default: w.default,
        fallback: w.fallback,
        trial,
        running: w.running.filter(|s| !s.is_empty()),
        outdated: w.outdated.filter(|s| !s.is_empty()),
        kernel: w.kernels.debian,
        kernel_old: w.kernels.debian_old,
        free_bytes: w.dom_free_kb.map(|k| k * 1024),
    })
}

/// Kernel releases from the names in `/boot` (`vmlinuz-<release>`), newest first.
pub fn installed_kernels<I: IntoIterator<Item = String>>(names: I) -> Vec<String> {
    let mut v: Vec<String> = names
        .into_iter()
        .filter_map(|n| n.strip_prefix("vmlinuz-").map(str::to_string))
        .filter(|r| !r.is_empty() && !r.ends_with(".old"))
        .collect();
    v.sort_by(|a, b| natural_cmp(b, a));
    v.dedup();
    v
}

/// Compares numbers inside version strings by value (`6.12.9` < `6.12.48`).
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let nb = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let va: u64 = std::str::from_utf8(&a[..na]).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
                let vb: u64 = std::str::from_utf8(&b[..nb]).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
                if va != vb {
                    return va.cmp(&vb);
                }
                (a, b) = (&a[na..], &b[nb..]);
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                (a, b) = (&a[1..], &b[1..]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::SYNODEB_DOM;

    #[test]
    fn fixture() {
        let s = parse_status(SYNODEB_DOM).unwrap();
        assert_eq!(s.entries.len(), 4);
        assert_eq!(s.default.as_deref(), Some("DEBIAN"));
        assert_eq!(s.fallback, ["DEBIAN_OLD", "SYNOLOGY_1"]);
        assert_eq!(s.trial, DomTrial::None);
        assert_eq!(s.outdated, None);
        assert_eq!(s.kernel.unwrap().release.as_deref(), Some("6.12.48-synodeb"));
        assert_eq!(s.free_bytes, Some(48120 * 1024));
    }

    #[test]
    fn minimal_and_newer_fields() {
        let s = parse_status(r#"{"schema":1,"trial":"trying","outdated":"rewrite","kernels":{"DEBIAN":null,"DEBIAN_OLD":null},"new_field":3}"#).unwrap();
        assert_eq!(s.trial, DomTrial::Trying);
        assert_eq!(s.outdated.as_deref(), Some("rewrite"));
        assert!(s.kernel.is_none());
        assert!(parse_status(r#"{"schema":2}"#).is_err());
        assert!(parse_status("not json").is_err());
    }

    #[test]
    fn kernels_sorted() {
        let v = installed_kernels(
            ["vmlinuz-6.12.9-amd64", "config-6.12.9", "vmlinuz-6.12.48-synodeb", "vmlinuz-6.12.48-synodeb.old"]
                .map(String::from),
        );
        assert_eq!(v, ["6.12.48-synodeb", "6.12.9-amd64"]);
    }
}
