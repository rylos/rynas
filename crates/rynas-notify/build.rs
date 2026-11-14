//! Texts of the channel messages in every language of the interface's table
//! (`crates/rynas-ui/assets/i18n/languages.txt`, docs/i18n.md): from each language's
//! translation files, the `nev.*` (event codes) and `task.*` (task names) entries of the web
//! interface's parts (`core.txt`, `module-<module>.txt`: the web interface and the channels
//! share one translation) and every entry of `notify.txt` (the words of the message templates,
//! English in `src/engine/template.rs`). Without the files (the crate built on its own) only
//! English is there.

use std::path::Path;

/// One `("key", "text")` per line, as Rust literals.
fn entries(text: &str, all: bool) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('\t'))
        .filter(|(k, _)| all || k.starts_with("nev.") || k.starts_with("task."))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rynas-ui/assets/i18n");
    let table = dir.join("languages.txt");
    println!("cargo:rerun-if-changed={}", dir.display());
    println!("cargo:rerun-if-changed={}", table.display());
    let codes: Vec<String> = std::fs::read_to_string(&table)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| l.split('\t').next())
        .map(|c| c.trim().to_string())
        .filter(|c| {
            !c.is_empty() && c.len() <= 12 && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .collect();
    if codes.is_empty() {
        println!(
            "cargo:warning=no language table at {}: channel messages stay in English",
            table.display()
        );
    }
    let mut out = String::from("&[\n");
    for code in codes.iter().filter(|c| c.as_str() != "en") {
        let lang = dir.join(code);
        println!("cargo:rerun-if-changed={}", lang.display());
        // the parts in the interface's order (core, the modules, then notify): a key written in
        // two parts keeps the first one's text
        let rank = |name: &str| match name {
            "core" => Some(0),
            "notify" => Some(2),
            n if n.starts_with("module-") => Some(1),
            _ => None,
        };
        let mut files: Vec<(u8, String, std::path::PathBuf)> = std::fs::read_dir(&lang)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        let name = p.file_stem()?.to_str()?.to_string();
                        Some((rank(&name)?, name, p))
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        let mut texts = Vec::new();
        for (_, name, f) in &files {
            println!("cargo:rerun-if-changed={}", f.display());
            texts.extend(entries(&std::fs::read_to_string(f).unwrap_or_default(), name == "notify"));
        }
        // stable: equal keys stay in part order, and dedup keeps the first
        texts.sort_by(|a, b| a.0.cmp(&b.0));
        texts.dedup_by(|a, b| a.0 == b.0);
        out.push_str(&format!("    ({code:?}, &[\n"));
        for (k, v) in texts {
            out.push_str(&format!("        ({k:?}, {v:?}),\n"));
        }
        out.push_str("    ]),\n");
    }
    out.push_str("]\n");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    std::fs::write(Path::new(&out_dir).join("event_texts.rs"), out).expect("write event_texts.rs");
    let langs = format!("&{codes:?}\n");
    std::fs::write(Path::new(&out_dir).join("languages.rs"), langs).expect("write languages.rs");
}