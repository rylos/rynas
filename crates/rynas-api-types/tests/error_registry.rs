//! Guidelines A11/A12: every error code is a constant of `error_codes`; no other crate writes
//! a code as a string literal, and retired codes never come back. Scans the `src/` of the
//! server crates (the UI translates codes and is checked by its own i18n tests).

use std::path::{Path, PathBuf};

use rynas_api_types::error_codes;

/// Codes replaced by `backend_unavailable` (A12): one code per meaning.
const RETIRED: &[&str] = &["storage_unavailable", "provider_error"];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn codes_are_unique_and_snake_case() {
    let mut seen = std::collections::HashSet::new();
    for c in error_codes::ALL {
        assert!(seen.insert(*c), "duplicate code {c}");
        assert!(
            c.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{c} is not snake_case"
        );
    }
    for s in [
        400, 401, 403, 404, 405, 409, 413, 415, 429, 500, 502, 503, 504,
    ] {
        assert!(error_codes::ALL.contains(&error_codes::for_status(s)));
    }
}

#[test]
fn no_code_literals_outside_the_registry() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for name in [
        "rynas-web",
        "rynasd",
        "rynas-auth",
        "rynas-config",
        "rynas-tasks",
        "rynas-storage",
        "rynas-services",
        "rynas-notify",
        "rynas-btrfs",
        "rynas-scheduler",
        "rynas-hw",
        "rynas-hw-synology",
        "rynas-power",
        "rynas-net",
        "rynas-http",
    ] {
        rust_files(&crates.join(name).join("src"), &mut files);
    }
    assert!(
        files.len() > 20,
        "sources not found under {}",
        crates.display()
    );
    let mut bad = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            // an error context on this line or the two before (multi-line constructors);
            // the same words elsewhere ("internal" bitmap of md) are not codes
            let ctx = lines[n.saturating_sub(2)..=n].join(" ");
            let is_error = ["StatusCode", "Error", "code:", "code =", "error_codes"]
                .iter()
                .any(|k| ctx.contains(k));
            for c in error_codes::ALL.iter().chain(RETIRED) {
                let lit = format!("\"{c}\"");
                if line.contains(&lit) && (is_error || RETIRED.contains(c)) {
                    bad.push(format!("{}:{}: \"{c}\"", f.display(), n + 1));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "use the rynas_api_types::error_codes constants instead of literals:\n{}",
        bad.join("\n")
    );
}
