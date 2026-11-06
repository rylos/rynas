//! Event texts for the Italian channel messages: the `nev.*` (event codes) and `task.*` (task
//! names) entries of the web interface dictionary, `crates/rynas-ui/src/i18n.rs` and the
//! hardware modules' tables `crates/rynas-ui/src/modules/<module>/i18n.rs` (D-MODULE), so the
//! web interface and the channels share one translation. Without the files (the crate built
//! on its own) the table is empty and messages keep the English title and body.

use std::path::Path;

fn main() {
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rynas-ui/src");
    let src = ui.join("i18n.rs");
    let modules = ui.join("modules");
    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-changed={}", modules.display());
    let mut files = vec![src.clone()];
    if let Ok(rd) = std::fs::read_dir(&modules) {
        let mut found: Vec<_> = rd
            .flatten()
            .map(|e| e.path().join("i18n.rs"))
            .filter(|p| p.is_file())
            .collect();
        found.sort();
        for f in &found {
            println!("cargo:rerun-if-changed={}", f.display());
        }
        files.extend(found);
    }
    let text: String = files
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    // one entry per line: `"key" => "English", "Italian";` (valid Rust literals, copied as is)
    let mut entries: Vec<(&str, &str)> = text
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("\"nev.") || l.starts_with("\"task."))
        .filter_map(|l| l.strip_suffix(';')?.split_once(" => "))
        .collect();
    entries.sort_unstable_by_key(|e| e.0);
    if entries.is_empty() {
        println!(
            "cargo:warning=no event texts in {}: channel messages stay in English",
            src.display()
        );
    }
    let mut out = String::from("&[\n");
    for (k, texts) in entries {
        out.push_str(&format!("    ({k}, {texts}),\n"));
    }
    out.push(']');
    let dest = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("event_texts.rs");
    std::fs::write(dest, out).expect("write event_texts.rs");
}
