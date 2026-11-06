//! Checks of the Rust sources for UI bug classes that a review missed once and that a test can
//! catch without a browser.

/// Every `.rs` file under `src/` with its text.
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).expect("src dir").flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p).expect("source file");
            (p.strip_prefix(&root).expect("under src").display().to_string(), text)
        })
        .collect()
}

/// `NodeRef::on_load` runs its closure once: on a node ref that outlives the element (declared
/// outside a `Show` that mounts the element again) the second mount panics in tachys
/// (`node_ref.rs`, `Option::unwrap()` on `None`) and the overlay never opens. The command
/// palette did that on its second opening. Use an `Effect` on `node_ref.get()` instead; only a
/// node ref created together with its element (one call per mount) may use `on_load`.
#[test]
fn node_ref_on_load_only_per_mount() {
    // ui::dialog builds a new NodeRef on every call, next to the element it binds
    const ALLOWED: &[&str] = &["ui.rs"];
    let mut bad = Vec::new();
    for (file, text) in sources() {
        if file == "source_check.rs" || ALLOWED.contains(&file.as_str()) {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            if line.contains(".on_load(") && !line.trim_start().starts_with("//") {
                bad.push(format!("{file}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(bad.is_empty(), "NodeRef::on_load outside a per-mount node ref: {bad:#?}");
}

/// A direct write to `/api/v1/config/*` (discard, restore, drift import: no `config.*` task,
/// so no task event) must call `config::changed()` right after: it bumps the configuration
/// generation that every `Remote::with_config` list reloads on. The global Discard did not:
/// after it the snapshot jobs (and users, shares, ...) kept "Pending: removal", the
/// struck-through name and the switch off until F5. Applies are `config.apply` tasks
/// (`tasks::upsert` calls `changed()` when one ends).
#[test]
fn config_writes_bump_the_generation() {
    let mut bad = Vec::new();
    for (file, text) in sources() {
        if file == "source_check.rs" {
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let write = ["api::post", "api::put", "api::delete"].iter().any(|w| line.contains(w));
            if write && line.contains("/api/v1/config/") && !line.trim_start().starts_with("//") {
                let after = &lines[n..(n + 15).min(lines.len())];
                if !after.iter().any(|l| l.contains("changed()")) {
                    bad.push(format!("{file}:{}: {}", n + 1, line.trim()));
                }
            }
        }
    }
    assert!(bad.is_empty(), "configuration writes without config::changed() (pages keep the old pending state): {bad:#?}");
}
