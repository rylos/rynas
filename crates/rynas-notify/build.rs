//! Event texts for the Italian channel messages: the `nev.*` (event codes) and `task.*` (task
//! names) entries of the web interface dictionary, `crates/rynas-ui/src/i18n.rs`, so the web
//! interface and the channels share one translation. Without the file (the crate built on its
//! own) the table is empty and messages keep the English title and body.

use std::path::Path;

fn main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rynas-ui/src/i18n.rs");
    println!("cargo:rerun-if-changed={}", src.display());
    let text = std::fs::read_to_string(&src).unwrap_or_default();
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
