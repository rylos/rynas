//! Checks of the file manager's Web Component (`files-app/`, ported from SynoFile, see
//! `pages/files.rs`): its texts follow the same rules as the dictionary of `i18n.rs` (stable
//! keys, English source, one translation file per language, no other product named: G8, G8c)
//! and its stylesheet the no-sideways-scroll rule (G7).
//!
//! The component's texts are keys in the code (`t("files.<area>.<name>")`, `count(n,
//! "files.….count.…")` for plural forms, `keyText()` for key names `files.key.*`), English in
//! `files-app/js/lang/en.js` (one `"key": "English",` entry per line) and every other
//! language in `assets/i18n/<code>/files.txt` (docs/i18n.md), checked like the core's parts by
//! `crate::i18n_check`.

use std::collections::HashSet;
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

/// The English table, in its order: key → English text (plain strings only).
fn english() -> Vec<(String, String)> {
    let text = std::fs::read_to_string(app_dir().join("js/lang/en.js")).expect("en.js");
    let start = text.find("export default {").expect("en.js: export default {") + "export default ".len();
    let end = text.rfind("};").expect("en.js: closing };");
    // the object is JSON but for the trailing commas
    let body = text[start..=end].trim_end_matches([';', '\n']).replace(",\n}", "\n}");
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&body).expect("en.js is a JSON object with string keys");
    let mut out: Vec<(String, String)> = map
        .into_iter()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => (k, s),
            other => panic!("en.js: {k}: plain strings only (plural forms are key.one / key.other), got {other}"),
        })
        .collect();
    // serde_json keeps no order: the file's own order, for the reports
    out.sort_by_key(|(k, _)| text.find(&format!("\"{k}\":")).unwrap_or(usize::MAX));
    out
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

/// Every `"files.…"` string literal of the sources (the keys the code names), with its file.
fn key_literals() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (file, text) in sources() {
        let mut rest = text.as_str();
        while let Some(i) = rest.find("\"files.") {
            rest = &rest[i + 1..];
            if let Some(k) = literal(rest)
                && !k.ends_with('.')
                && k.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
            {
                out.push((file.clone(), k));
            }
        }
    }
    out
}

/// The base of a plural form key (`files.common.count.items.one` → `files.common.count.items`).
fn plural_base(key: &str) -> Option<&str> {
    key.rsplit_once('.').filter(|(_, c)| crate::i18n_check::CATEGORIES.contains(c)).map(|(b, _)| b)
}

/// Every key the code names is in the English table (a plural text by its base, which has an
/// `other` form), and every English text is a single line.
#[test]
fn files_keys_exist() {
    let en = english();
    let keys: HashSet<&str> = en.iter().map(|(k, _)| k.as_str()).collect();
    let mut bad: Vec<String> = key_literals()
        .into_iter()
        .filter(|(_, k)| !keys.contains(k.as_str()) && !keys.contains(format!("{k}.other").as_str()))
        .map(|(f, k)| format!("{f}: {k}: not in en.js"))
        .collect();
    for (k, v) in &en {
        if !k.starts_with("files.") {
            bad.push(format!("en.js: {k}: keys start with files."));
        }
        if v.is_empty() || v.contains(['\t', '\n']) {
            bad.push(format!("en.js: {k}: empty, or a tab or newline in the text"));
        }
        if let Some(base) = plural_base(k)
            && !keys.contains(format!("{base}.other").as_str())
        {
            bad.push(format!("en.js: {k}: plural form without `{base}.other`"));
        }
    }
    assert!(bad.is_empty(), "file manager keys: {bad:#?}");
}

/// Every English text is shown somewhere: its key (or its plural base) is named by the code; the
/// key names (`files.key.*`) are looked up by `keyText()` in `i18n.js`.
#[test]
fn files_english_is_not_stale() {
    let used: HashSet<String> = key_literals().into_iter().map(|(_, k)| k).collect();
    let key_text = sources().iter().any(|(f, t)| f == "i18n.js" && t.contains("files.key.${"));
    let stale: Vec<String> = english()
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| !used.contains(k) && !plural_base(k).is_some_and(|b| used.contains(b)) && !(key_text && k.starts_with("files.key.")))
        .collect();
    assert!(stale.is_empty(), "en.js texts no code shows: {stale:#?}");
}

/// Every language of the table but English has the File app's file, complete and current
/// (missing and stale keys, placeholders, plural forms of the language: `i18n_check`).
#[test]
fn files_translations_are_complete() {
    let en = english();
    let source: Vec<(&str, &str)> = en.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut bad = Vec::new();
    for lang in crate::i18n::languages().iter().skip(1) {
        match crate::i18n_check::file(&lang.code, "files") {
            Some(text) => bad.extend(crate::i18n_check::problems("files", lang, &source, &text)),
            None => bad.push(format!("{}/files.txt: no file", lang.code)),
        }
    }
    assert!(bad.is_empty(), "File app translations: {bad:#?}");
}

/// The File app writes durations, relative times and ratios with the same unit words and
/// templates as the core (`i18n::duration`, `elapsed`, `approx`, `ago`, `of`): its
/// `files.dur.*` / `files.fmt.*` texts are the core's `dur.*` / `fmt.*`, in English and in
/// every translation.
#[test]
fn files_formats_match_core() {
    use crate::i18n::{Lang, text_in, translation_lines};
    const PAIRS: &[(&str, &str)] = &[
        ("dur.d", "files.dur.d"),
        ("dur.h", "files.dur.h"),
        ("dur.m", "files.dur.m"),
        ("dur.s", "files.dur.s"),
        ("dur.hm", "files.dur.hm"),
        ("dur.ms", "files.dur.ms"),
        ("fmt.ago", "files.fmt.ago"),
        ("fmt.of", "files.fmt.of"),
    ];
    let en: std::collections::HashMap<String, String> = english().into_iter().collect();
    let used: HashSet<String> = key_literals().into_iter().map(|(_, k)| k).collect();
    for (core, files) in PAIRS {
        assert_eq!(text_in(Lang::EN, core), en.get(*files).map(String::as_str), "{core}: English differs from the File app");
        assert!(used.contains(*files), "{files}: not used by the File app any more");
    }
    for lang in crate::i18n::languages().iter().skip(1) {
        let core_file = crate::i18n_check::file(&lang.code, "core").unwrap_or_default();
        let files_file = crate::i18n_check::file(&lang.code, "files").unwrap_or_default();
        let core: std::collections::HashMap<&str, &str> = translation_lines(&core_file).collect();
        let files: std::collections::HashMap<&str, &str> = translation_lines(&files_file).collect();
        for (c, f) in PAIRS {
            if let Some(want) = core.get(c) {
                assert_eq!(files.get(f), Some(want), "{}: {f} differs from the core's {c}", lang.code);
            }
        }
    }
}

/// G8c and the neutral product rule: no other NAS system or its apps named in any text, English
/// or translated (DSM's own folder names such as `@eaDir` are file names, not texts).
#[test]
fn files_texts_name_no_other_product() {
    const WORDS: &[&str] = &["dsm", "synology", "synofile", "truenas", "unraid", "qnap", "omv"];
    const PHRASES: &[&str] = &[
        "file station", "package center", "centro pacchetti", "storage manager", "control panel", "pannello di controllo",
        "login portal", "synology photos", "hyper backup",
    ];
    let mut texts: Vec<(String, String)> = english().into_iter().map(|(_, v)| ("en.js".to_string(), v)).collect();
    for lang in crate::i18n::languages().iter().skip(1) {
        let file = crate::i18n_check::file(&lang.code, "files").unwrap_or_default();
        texts.extend(crate::i18n::translation_lines(&file).map(|(_, t)| (format!("{}/files.txt", lang.code), t.to_string())));
    }
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

/// English is the only table bundled with the app: the other languages are translation files
/// (`assets/i18n/<code>/files.txt`), fetched when chosen; the other SynoFile tables are not
/// shipped (D-LANG2).
#[test]
fn files_bundle_only_english() {
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
