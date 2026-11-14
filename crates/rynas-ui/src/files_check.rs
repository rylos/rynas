//! Checks of the file manager's Web Component (`files-app/`, ported from SynoFile, see
//! `pages/files.rs`): its texts follow the same rules as the dictionary of `i18n.rs` (both
//! languages, no other product named: G8, G8c) and its stylesheet the no-sideways-scroll rule (G7).
//!
//! The component's strings are Italian in the code (`t("…")`) with the English in
//! `files-app/js/lang/en.js`, one `"italiano": "English",` entry per line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn app_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("files-app")
}

/// The JavaScript sources of the app (not the generated syntax tables, the vendored libraries
/// or the language table).
fn sources() -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut add = |p: PathBuf| {
        let text = std::fs::read_to_string(&p).expect("files-app source");
        out.push((p.file_name().unwrap_or_default().to_string_lossy().into_owned(), text));
    };
    add(app_dir().join("rynas-files.js"));
    for e in std::fs::read_dir(app_dir().join("js")).expect("files-app/js").flatten() {
        let p = e.path();
        // eza.js: icon tables (glyphs and file names, no interface text)
        if p.extension().is_some_and(|x| x == "js") && !p.ends_with("eza.js") {
            add(p);
        }
    }
    out
}

/// The English table: Italian key → English text (plural tables as their JSON text).
fn english() -> HashMap<String, String> {
    let text = std::fs::read_to_string(app_dir().join("js/lang/en.js")).expect("en.js");
    let start = text.find("export default {").expect("en.js: export default {") + "export default ".len();
    let end = text.rfind("};").expect("en.js: closing };");
    // the object is JSON but for the trailing commas
    let body = text[start..=end].trim_end_matches([';', '\n']).replace(",\n}", "\n}");
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&body).expect("en.js is a JSON object with string keys");
    map.into_iter()
        .map(|(k, v)| {
            let s = match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            (k, s)
        })
        .collect()
}

/// The JS string literal that starts at `s` (just after its opening quote), unescaped.
fn literal(s: &str) -> Option<String> {
    let mut end = None;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => {
                end = Some(i);
                break;
            }
            _ => escaped = false,
        }
    }
    serde_json::from_str(&format!("\"{}\"", &s[..end?])).ok()
}

/// Every `t("…")` key of the sources, with the file it is in.
fn keys() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (file, text) in sources() {
        let mut rest = text.as_str();
        while let Some(i) = rest.find("t(\"") {
            let before = rest[..i].chars().last();
            rest = &rest[i + 3..];
            // `t(` itself, not the end of another name (`set(`, `count(`)
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
                continue;
            }
            if let Some(k) = literal(rest) {
                out.push((file.clone(), k));
            }
        }
    }
    out
}

/// The Italian texts of the error table of `i18n.js` (`ERRORS`: API code → text).
fn error_texts() -> Vec<String> {
    let text = std::fs::read_to_string(app_dir().join("js/i18n.js")).expect("i18n.js");
    let start = text.find("const ERRORS = {").expect("ERRORS table");
    let end = start + text[start..].find("};").expect("end of ERRORS");
    text[start..end]
        .lines()
        .filter_map(|l| l.split_once(": \"").and_then(|(_, v)| literal(v)))
        .collect()
}

#[test]
fn files_texts_are_translated() {
    let en = english();
    let mut missing: Vec<String> = keys().into_iter().filter(|(_, k)| !en.contains_key(k)).map(|(f, k)| format!("{f}: {k}")).collect();
    missing.extend(error_texts().into_iter().filter(|k| !en.contains_key(k)).map(|k| format!("i18n.js ERRORS: {k}")));
    assert!(missing.is_empty(), "texts of the file manager without English: {missing:#?}");
}

/// The File app writes durations, relative times and ratios with the same unit words and
/// templates as the core (`i18n::duration`, `elapsed`, `approx`, `ago`, `of`): its Italian key
/// is the core's Italian text and its English the core's English.
#[test]
fn files_formats_match_core() {
    use crate::i18n::{Lang, text_in};
    const PAIRS: &[(&str, &str)] = &[
        ("dur.d", "{d} g"),
        ("dur.h", "{h} h"),
        ("dur.m", "{m} min"),
        ("dur.s", "{s} s"),
        ("dur.hm", "{h} h {m} min"),
        ("dur.ms", "{m} min {s} s"),
        ("fmt.ago", "{time} fa"),
        ("fmt.of", "{part} di {total}"),
    ];
    let en = english();
    let used: std::collections::HashSet<String> = keys().into_iter().map(|(_, k)| k).collect();
    for (core, files) in PAIRS {
        assert_eq!(text_in(Lang::It, core), Some(*files), "{core}: Italian differs from the File app");
        assert_eq!(text_in(Lang::En, core), en.get(*files).map(String::as_str), "{core}: English differs from the File app");
        assert!(used.contains(*files), "{files}: not used by the File app any more");
    }
}

/// G8c and the neutral product rule: no other NAS system or its apps named in any text, Italian
/// or English (DSM's own folder names such as `@eaDir` are file names, not texts).
#[test]
fn files_texts_name_no_other_product() {
    const WORDS: &[&str] = &["dsm", "synology", "synofile", "truenas", "unraid", "qnap", "omv"];
    const PHRASES: &[&str] = &[
        "file station", "package center", "centro pacchetti", "storage manager", "control panel", "pannello di controllo",
        "login portal", "synology photos", "hyper backup",
    ];
    let mut texts: Vec<(String, String)> = keys();
    texts.extend(english().into_iter().flat_map(|(k, v)| [("en.js".to_string(), k), ("en.js".to_string(), v)]));
    texts.extend(error_texts().into_iter().map(|k| ("i18n.js".to_string(), k)));
    let mut hits = Vec::new();
    for (file, text) in texts {
        let low = text.to_lowercase();
        let word = low.split(|c: char| !c.is_alphanumeric()).find(|w| WORDS.contains(w));
        if let Some(w) = word.or_else(|| PHRASES.iter().copied().find(|p| low.contains(p))) {
            hits.push(format!("{file}: {w}: {text}"));
        }
    }
    assert!(hits.is_empty(), "file manager texts naming another product: {hits:#?}");
}

/// Only Italian and English: the other SynoFile tables are not shipped (D-LANG2).
#[test]
fn files_languages_are_it_and_en() {
    let mut langs: Vec<String> = std::fs::read_dir(app_dir().join("js/lang"))
        .expect("files-app/js/lang")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    langs.sort();
    assert_eq!(langs, ["en.js"]);
}

/// G7 for the component's stylesheet: what may scroll sideways is listed with its reason.
#[test]
fn files_horizontal_scroll_containers_are_listed() {
    const ALLOWED: &[(&str, &str)] = &[
        (".view.columns", "the columns view: one column per folder level, a navigator wide by nature"),
        (".ql-body pre", "text preview: code and logs keep their lines (wide by nature)"),
    ];
    let css = std::fs::read_to_string(app_dir().join("app.css")).expect("app.css");
    let mut bad = Vec::new();
    let mut rest = css.as_str();
    while let Some(open) = rest.find('{') {
        let sel = rest[..open].rsplit(['}', '/']).next().unwrap_or("").trim().to_string();
        let close = rest[open..].find('}').map(|c| open + c).unwrap_or(rest.len());
        for d in rest[open + 1..close].split(';') {
            let Some((p, v)) = d.split_once(':') else { continue };
            let v = v.trim();
            let x = match p.trim() {
                "overflow-x" => v,
                "overflow" => v.split_whitespace().next().unwrap_or(""),
                _ => continue,
            };
            if (x == "auto" || x == "scroll") && !ALLOWED.iter().any(|(s, _)| *s == sel) {
                bad.push(format!("{sel}: {}", d.trim()));
            }
        }
        rest = &rest[(close + 1).min(rest.len())..];
    }
    assert!(bad.is_empty(), "file manager elements that may scroll sideways (G7), not listed: {bad:#?}");
}
