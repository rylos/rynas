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

/// A dialog's buttons live in its footer (`Modal`'s `foot`, `ui::dialog`'s `foot`,
/// `form::save_bar_with`, `ui::drawer_foot`), which stays in sight while the body scrolls. A
/// hand-written `.dialog-foot` inside the body scrolled away with a long plan and was written
/// in some 30 different ways (review B.2).
#[test]
fn dialog_buttons_go_in_the_footer() {
    let mut bad = Vec::new();
    for (file, text) in sources() {
        if !(file.starts_with("pages/") || file.starts_with("modules/")) {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            if line.contains("\"dialog-foot\"") {
                bad.push(format!("{file}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(bad.is_empty(), "hand-written .dialog-foot in page code (use the dialog's foot): {bad:#?}");
}

/// Close only closes: it is a ghost button, never the primary one (review B.4). Use
/// `ui::close_foot`.
#[test]
fn close_is_never_primary() {
    let mut bad = Vec::new();
    for (file, text) in sources() {
        if file == "source_check.rs" {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            if line.contains("btn primary") && line.contains("tr(\"close\")") {
                bad.push(format!("{file}:{}: {}", n + 1, line.trim()));
            }
        }
    }
    assert!(bad.is_empty(), "a primary Close button (use ui::close_foot): {bad:#?}");
}

/// Row actions: the accessible name is "<Verb> <name>" (`ui::verb_name`), not "Verb: name";
/// a bin icon is a danger icon button (review B.14, B.19).
#[test]
fn row_actions_are_named_and_toned() {
    let mut bad = Vec::new();
    for (file, text) in sources() {
        if file == "source_check.rs" {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            if line.contains("aria-label=format!(\"{}: {}\", t(") {
                bad.push(format!("{file}:{}: \"Verb: name\" label (use ui::verb_name)", n + 1));
            }
            if line.contains("class=\"icon-btn sm\"") && line.contains("TRASH_2") {
                bad.push(format!("{file}:{}: bin icon without `danger`", n + 1));
            }
        }
    }
    assert!(bad.is_empty(), "row actions: {bad:#?}");
}

/// Classes replaced by a shared component must not come back (review B.5, B.11, B.12): the
/// wide drawer is `DlgKind::WideDrawer`, folds are `ui::fold` (`.fold`), tone chips are
/// `ui::chip` (`.chip.ok/.warn/.err/.info`), the status card is `.status-card`.
#[test]
fn retired_classes_stay_retired() {
    const RETIRED: &[(&str, &str)] = &[
        ("wide-drawer", "ui::drawer_wide / DlgKind::WideDrawer"),
        ("warn-chip", "ui::chip(Tone::Warn, …)"),
        ("danger-chip", "ui::chip(Tone::Err, …)"),
        ("dedup-chip", "ui::chip(Tone::Info, …)"),
        ("pcb-fold", "ui::fold"),
        ("pcb-sub", "class=\"fold sub\""),
        ("pcb-status", ".status-card"),
    ];
    let mut bad = Vec::new();
    for (file, text) in sources() {
        if file == "source_check.rs" {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            for (class, instead) in RETIRED {
                if line.contains(class) {
                    bad.push(format!("{file}:{}: {class} (use {instead})", n + 1));
                }
            }
        }
    }
    assert!(bad.is_empty(), "retired classes: {bad:#?}");
}
