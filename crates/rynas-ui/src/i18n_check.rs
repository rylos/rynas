//! Completeness of the translations (docs/i18n.md): for every language of the table
//! (`assets/i18n/languages.txt`) and every part (core, modules, File app), the translation
//! file `assets/i18n/<code>/<part>.txt` against the English source of that part: missing
//! keys, stale keys (in the translation, not in the source), placeholders that differ, plural
//! forms the language needs, empty texts and keys written twice.
//!
//! The checker is shared by every part: the core and the modules give their tables
//! (`crate::i18n`), the File app its English table (`files_check.rs`), rynas-notify has the
//! same rules for its own part.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use crate::i18n::{Language, languages, translation_lines};

/// The folder of the translation files.
pub(crate) fn dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/i18n"))
}

/// The translation file of `part` in the language `code`, or `None` when there is none.
pub(crate) fn file(code: &str, part: &str) -> Option<String> {
    std::fs::read_to_string(dir().join(code).join(format!("{part}.txt"))).ok()
}

/// The `{name}` placeholders of a text, sorted, each once.
pub(crate) fn placeholders(s: &str) -> BTreeSet<&str> {
    s.split('{').skip(1).filter_map(|p| p.split_once('}').map(|x| x.0)).filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).collect()
}

/// The CLDR plural categories a key may end with.
pub(crate) const CATEGORIES: &[&str] = &["zero", "one", "two", "few", "many", "other"];

/// `(base, category)` of a plural form key (`disk.count.one` → `disk.count`, `one`) when the
/// source has the base's English forms, `one` and `other` (a lone `x.other` is a plain key:
/// "Other" of a list).
fn plural_of<'a>(key: &'a str, has: &dyn Fn(&str) -> bool) -> Option<(&'a str, &'a str)> {
    let (base, cat) = key.rsplit_once('.')?;
    (CATEGORIES.contains(&cat) && has(&format!("{base}.one")) && has(&format!("{base}.other"))).then_some((base, cat))
}

/// The problems of the translation `text` of `part` in `lang` against the English `source`
/// (key, English text), one line each.
pub(crate) fn problems(part: &str, lang: &Language, source: &[(&str, &str)], text: &str) -> Vec<String> {
    let src: HashMap<&str, &str> = source.iter().copied().collect();
    let has_src = |k: &str| src.contains_key(k);
    let mut out = Vec::new();
    let mut seen: HashMap<&str, &str> = HashMap::new();
    for (k, t) in translation_lines(text) {
        if seen.insert(k, t).is_some() {
            out.push(format!("{}/{part}: {k}: written twice", lang.code));
        }
    }
    let at = |k: &str| format!("{}/{part}: {k}", lang.code);
    // plural bases of the source: their forms follow the language, not English
    let bases: BTreeSet<&str> = source.iter().filter_map(|(k, _)| plural_of(k, &has_src)).map(|(b, _)| b).collect();
    for (k, _) in source {
        if plural_of(k, &has_src).is_none() && !seen.contains_key(k) {
            out.push(format!("{}: missing", at(k)));
        }
    }
    for base in &bases {
        for cat in &lang.plurals {
            let k = format!("{base}.{cat}");
            if !seen.contains_key(k.as_str()) {
                out.push(format!("{}: missing (plural form `{cat}` of {})", at(&k), lang.code));
            }
        }
    }
    for (k, t) in &seen {
        if t.trim().is_empty() {
            out.push(format!("{}: empty", at(k)));
        }
        if t.contains(['\t', '\r']) {
            out.push(format!("{}: tab or carriage return in the text", at(k)));
        }
        let (en, plural) = match plural_of(k, &has_src) {
            Some((base, cat)) => {
                if !lang.plurals.iter().any(|c| c == cat) {
                    out.push(format!("{}: stale (`{cat}` is not a plural form of {})", at(k), lang.code));
                    continue;
                }
                (src[format!("{base}.other").as_str()], true)
            }
            None => match src.get(k) {
                Some(en) => (*en, false),
                None if k.rsplit_once('.').is_some_and(|(b, c)| CATEGORIES.contains(&c) && bases.contains(b)) => continue,
                None => {
                    out.push(format!("{}: stale (not in the source)", at(k)));
                    continue;
                }
            },
        };
        let (mut want, mut got) = (placeholders(en), placeholders(t));
        if plural {
            // a form may spell the count out ("un disco"): `{n}` is optional in each form
            want.remove("n");
            got.remove("n");
        }
        if want != got {
            out.push(format!("{}: placeholders {got:?}, English has {want:?}", at(k)));
        }
    }
    out.sort();
    out
}

/// The core and module parts: (part, its English entries).
fn shell_parts() -> Vec<(String, Vec<(&'static str, &'static str)>)> {
    crate::i18n::parts().into_iter().map(|(part, entries)| (part.to_string(), entries)).collect()
}

/// Every language but English has a file for every part of the interface, and each file is
/// complete and current.
#[test]
fn translations_are_complete() {
    let langs = languages();
    assert_eq!(langs[0].code, "en", "English is the first language of the table (the source)");
    let mut bad = Vec::new();
    for lang in langs.iter().skip(1) {
        for (part, entries) in shell_parts() {
            match file(&lang.code, &part) {
                Some(text) => bad.extend(problems(&part, lang, &entries, &text)),
                None => bad.push(format!("{}/{part}.txt: no file", lang.code)),
            }
        }
    }
    assert!(bad.is_empty(), "translations: {bad:#?}");
}

/// No folder of translations without its line in the table, no file that is not a part.
#[test]
fn translation_folders_match_the_table() {
    let codes: Vec<String> = languages().into_iter().map(|l| l.code).collect();
    let mut parts: Vec<String> = shell_parts().into_iter().map(|p| p.0).collect();
    parts.extend(["files".to_string(), "notify".to_string()]);
    let mut bad = Vec::new();
    for e in std::fs::read_dir(dir()).expect("assets/i18n").flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if e.path().is_dir() {
            if !codes.contains(&name) || name == "en" {
                bad.push(format!("{name}/: not a translated language of languages.txt"));
            }
            for f in std::fs::read_dir(e.path()).expect("language folder").flatten() {
                let f = f.file_name().to_string_lossy().into_owned();
                if !f.strip_suffix(".txt").is_some_and(|p| parts.iter().any(|x| x == p)) {
                    bad.push(format!("{name}/{f}: not a part"));
                }
            }
        } else if name != "languages.txt" {
            bad.push(format!("{name}: unexpected file"));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

/// The checker itself, on a made-up language with four plural forms (the mechanism for a
/// third language, without shipping one).
#[test]
fn checker_finds_every_kind_of_problem() {
    let pl = Language { code: "pl".into(), name: "Polski".into(), locale: "pl-PL".into(), plurals: ["one", "few", "many", "other"].map(String::from).to_vec(), english: "Polish".into() };
    let source = [("a.b", "Disk {name}"), ("c.d", "Save"), ("x.one", "{n} drive"), ("x.other", "{n} drives"), ("gone", "Old")];
    let ok = "a.b\tDysk {name}\nc.d\tZapisz\nx.one\tjeden dysk\nx.few\t{n} dyski\nx.many\t{n} dysków\nx.other\t{n} dysku\ngone\tStary\n";
    assert_eq!(problems("t", &pl, &source, ok), Vec::<String>::new());
    let bad = "# comment\na.b\tDysk {nazwa}\nx.one\tjeden\nx.other\t{n} dysku\nx.two\t{n}\nnew.key\tNowy\ngone\t \ngone\tStary\n";
    let p = problems("t", &pl, &source, bad);
    for want in ["pl/t: a.b: placeholders", "pl/t: c.d: missing", "pl/t: x.few: missing", "pl/t: x.many: missing", "pl/t: x.two: stale", "pl/t: new.key: stale", "pl/t: gone: written twice"] {
        assert!(p.iter().any(|l| l.starts_with(want)), "{want} not found in {p:#?}");
    }
}
