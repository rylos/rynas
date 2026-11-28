//! Translations of an entry's texts (`docs/phase2/docker-apps.md` § 6.14):
//!
//! ```text
//! apps/<id>/i18n/<lang>.toml   the short texts of app.toml and form.json, by field
//! apps/<id>/i18n/<lang>.md     the long description (description.md)
//! ```
//!
//! Keys name the field they translate (`ports.<name>.purpose`, `form.<FIELD>.title`,
//! `[[notes]]` in the order of `app.toml`), never the English text. The loader refuses a key
//! whose field is not in the entry, a text for a field the entry leaves empty and lists whose
//! length differs, so a renamed port or a removed form field cannot leave a translation
//! behind silently. Each translated text also carries a stamp of the English it was made
//! from (`[sources]`, [`stamp`]): a reworded English text, or notes moved around, fail the
//! entry until someone reviews the translation and stamps it again ([`restamp`], the
//! `catalog-i18n` example). English stays the source: whatever a translation leaves out is
//! shown in English.
//!
//! The server picks the translation for the language a client asks for ([`pick`]) and sends
//! it as [`AppTexts`] next to the English fields, which keep their meaning.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rynas_api_types::apps::{AppFieldTexts, AppLabelTexts, AppTexts};
use serde::Deserialize;

use crate::catalog::{CatalogApp, file_name};
use crate::error::Issues;
use crate::form::FormSchema;
use crate::manifest::AppManifest;
use crate::version::Version;

/// The folder of the translations inside an entry.
pub const DIR: &str = "i18n";

/// The first rynas that loads `i18n/`: an older loader refuses an entry with it, and in an
/// online catalogue (§ 6.12) a failing entry is skipped only when its `min_rynas` is above the
/// running version, otherwise the whole download is refused. So an entry with translations
/// needs at least this `min_rynas`.
pub const I18N_MIN_RYNAS: Version = Version::new(0, 9, 21);

/// The key of the long description (`<lang>.md`) in `[sources]`.
pub const LONG_DESCRIPTION: &str = "description.md";

/// The comment written above `[sources]` by [`restamp`].
const SOURCES_COMMENT: &str = "# Stamps of the English each text was translated from: after a review, refresh them with\n# cargo run -p rynas-apps --example catalog-i18n -- apps/<id>";

/// Longest translated card line, in characters (`app.toml` `description` has 140 bytes of
/// English).
pub const SUMMARY_MAX_CHARS: usize = 140;

/// `i18n/<lang>.toml`, with the long description of `i18n/<lang>.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Translation {
    /// `app.toml` `description`: the card line.
    #[serde(default)]
    pub description: Option<String>,
    /// `i18n/<lang>.md`: the text of `description.md`.
    #[serde(skip)]
    pub long_description: Option<String>,
    /// `[[ports]]` by `name`.
    #[serde(default)]
    pub ports: BTreeMap<String, PortText>,
    /// `[[folders]]` by `env`.
    #[serde(default)]
    pub folders: BTreeMap<String, DescriptionText>,
    /// `[[shares]]` by `env`.
    #[serde(default)]
    pub shares: BTreeMap<String, LabelText>,
    #[serde(default)]
    pub share_set: Option<LabelText>,
    /// `[[secrets]]` by `env`.
    #[serde(default)]
    pub secrets: BTreeMap<String, DescriptionText>,
    /// `[[overlays]]` by the name of their file (`overlays/<name>.yaml`).
    #[serde(default)]
    pub overlays: BTreeMap<String, DescriptionText>,
    /// `[[named_volumes]]` by `name`.
    #[serde(default)]
    pub named_volumes: BTreeMap<String, DescriptionText>,
    /// `[[notes]]`: one per note of `app.toml`, in the same order (a note without `text` is
    /// not translated).
    #[serde(default)]
    pub notes: Vec<NoteText>,
    /// `form.json` properties by name.
    #[serde(default)]
    pub form: BTreeMap<String, FieldText>,
    /// `form.json` `x-groups`: one per group, in the same order.
    #[serde(default)]
    pub groups: Vec<GroupText>,
    /// The [`stamp`] of the English each text was translated from, by the text's key
    /// (`description`, `description.md`, `ports.web.purpose`, `notes[0].text`,
    /// `form.FIELD.enum_labels`…).
    #[serde(default)]
    pub sources: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortText {
    pub purpose: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptionText {
    pub description: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelText {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoteText {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldText {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `x-ui.placeholder`.
    #[serde(default)]
    pub placeholder: Option<String>,
    /// `x-ui.enum_labels`: all of them, same order.
    #[serde(default)]
    pub enum_labels: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupText {
    #[serde(default)]
    pub title: Option<String>,
}

/// A language code a translation may use: a BCP 47 tag in its usual case, as in the UI's
/// language table (`it`, `zh-CN`, `pt-BR`, `zh-Hant`, `es-419`). The primary language is 2-3
/// lowercase letters, a region 2 uppercase letters or 3 digits, a script 4 letters with a
/// capital first; plain `en` is the entry itself.
pub fn is_lang_code(s: &str) -> bool {
    let mut parts = s.split('-');
    let primary = parts.next().unwrap_or_default();
    let subtag_ok = |p: &str| {
        let b = p.as_bytes();
        match b.len() {
            2 => b.iter().all(u8::is_ascii_uppercase),
            3 => b.iter().all(u8::is_ascii_digit),
            4 => b[0].is_ascii_uppercase() && b[1..].iter().all(u8::is_ascii_lowercase),
            5..=8 => b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            _ => false,
        }
    };
    s != "en" && s.len() <= 35 && (2..=3).contains(&primary.len()) && primary.bytes().all(|c| c.is_ascii_lowercase()) && parts.all(subtag_ok)
}

/// The stamp of an English text: 8 hex digits of its FNV-1a hash, after trimming and with
/// Windows line ends read as `\n` (a checkout may convert them). Not a security measure: it
/// only notices that the English changed.
pub fn stamp(english: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in english.replace("\r\n", "\n").trim().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", (h >> 32) ^ (h & 0xffff_ffff))
}

/// The files of `i18n/`, sorted; `None` (with the problem in `issues`) when `i18n` is not a
/// readable folder. An entry without `i18n` has none.
fn files(dir: &Path, app: &str, issues: &mut Issues) -> Option<Vec<PathBuf>> {
    let root = dir.join(DIR);
    match fs::metadata(&root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(e) => {
            issues.push(app, DIR, format!("cannot read: {e}"));
            return None;
        }
        Ok(m) if !m.is_dir() => {
            issues.push(app, DIR, "must be a folder of translations");
            return None;
        }
        Ok(_) => {}
    }
    match fs::read_dir(&root) {
        Ok(rd) => {
            let mut v = Vec::new();
            for e in rd {
                match e {
                    Ok(e) => v.push(e.path()),
                    Err(e) => issues.push(app, DIR, format!("cannot read: {e}")),
                }
            }
            v.sort();
            Some(v)
        }
        Err(e) => {
            issues.push(app, DIR, format!("cannot read: {e}"));
            None
        }
    }
}

/// Reads `i18n/` of an entry: every `<lang>.toml` and `<lang>.md`, by language. Problems go
/// to `issues`; [`Translation::check`] compares each one with the entry afterwards.
pub(crate) fn load(dir: &Path, app: &str, issues: &mut Issues) -> BTreeMap<String, Translation> {
    let mut out: BTreeMap<String, Translation> = BTreeMap::new();
    for p in files(dir, app, issues).unwrap_or_default() {
        let name = file_name(&p);
        let rel = format!("{DIR}/{name}");
        let (code, md) = match (name.strip_suffix(".toml"), name.strip_suffix(".md")) {
            (Some(c), _) => (c, false),
            (_, Some(c)) => (c, true),
            _ => {
                issues.push(app, &rel, "translations are <lang>.toml or <lang>.md");
                continue;
            }
        };
        if !is_lang_code(code) {
            issues.push(app, &rel, format!("`{code}` is not a language code such as it, zh-CN or pt-BR (English is the entry itself)"));
            continue;
        }
        let text = match fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) => {
                issues.push(app, &rel, format!("cannot read: {e}"));
                continue;
            }
        };
        if md {
            if text.trim().is_empty() {
                issues.push(app, &rel, "empty");
            }
            out.entry(code.to_owned()).or_default().long_description = Some(text);
        } else {
            match toml::from_str::<Translation>(&text) {
                Ok(t) => {
                    let slot = out.entry(code.to_owned()).or_default();
                    let long = slot.long_description.take();
                    *slot = Translation { long_description: long, ..t };
                }
                Err(e) => issues.push(app, &rel, e.to_string()),
            }
        }
    }
    out
}

/// Problems of one translation file.
struct Checker<'a> {
    app: &'a str,
    rel: String,
    issues: &'a mut Issues,
}

impl Checker<'_> {
    fn err(&mut self, msg: String) {
        self.issues.push(self.app, &self.rel, msg);
    }

    /// A text for `key`: `source` says whether the entry has that field (with a text).
    fn text(&mut self, key: &str, text: Option<&str>, source: bool) {
        let Some(text) = text else { return };
        if !source {
            self.err(format!("`{key}`: the entry has no such text to translate"));
        } else if text.trim().is_empty() {
            self.err(format!("`{key}` is empty (leave it out to show the English text)"));
        }
    }

    /// A key naming an item of the entry (a port, a folder…): it must exist.
    fn item(&mut self, key: &str, exists: bool) -> bool {
        if !exists {
            self.err(format!("`{key}`: no such item in the entry"));
        }
        exists
    }
}

fn has(s: &str) -> bool {
    !s.trim().is_empty()
}

/// `overlays/<name>.yaml` → `<name>`.
fn overlay_name(file: &str) -> Option<&str> {
    file.strip_prefix("overlays/")?.strip_suffix(".yaml")
}

impl Translation {
    /// Every text the translation has whose English the entry has, by key, with that English
    /// (the keys of `[sources]`).
    pub fn translated(&self, m: &AppManifest, form: &FormSchema, long: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut add = |key: String, translated: bool, english: Option<&str>| {
            if let (true, Some(en)) = (translated, english.filter(|e| has(e))) {
                out.push((key, en.to_owned()));
            }
        };
        add("description".into(), self.description.is_some(), Some(m.description.as_str()));
        add(LONG_DESCRIPTION.into(), self.long_description.is_some(), Some(long));
        for p in &m.ports {
            add(format!("ports.{}.purpose", p.name), self.ports.contains_key(&p.name), Some(p.purpose.as_str()));
        }
        for f in &m.folders {
            add(format!("folders.{}.description", f.env), self.folders.contains_key(&f.env), Some(f.description.as_str()));
        }
        for s in &m.shares {
            let t = self.shares.get(&s.env);
            add(format!("shares.{}.label", s.env), t.is_some_and(|t| t.label.is_some()), Some(s.label.as_str()));
            add(format!("shares.{}.description", s.env), t.is_some_and(|t| t.description.is_some()), Some(s.description.as_str()));
        }
        if let (Some(s), Some(t)) = (&m.share_set, &self.share_set) {
            add("share_set.label".into(), t.label.is_some(), Some(s.label.as_str()));
            add("share_set.description".into(), t.description.is_some(), Some(s.description.as_str()));
        }
        for s in &m.secrets {
            add(format!("secrets.{}.description", s.env), self.secrets.contains_key(&s.env), Some(s.description.as_str()));
        }
        for o in &m.overlays {
            let name = overlay_name(&o.file).unwrap_or_default();
            add(format!("overlays.{name}.description"), self.overlays.contains_key(name), Some(o.description.as_str()));
        }
        for v in &m.named_volumes {
            add(format!("named_volumes.{}.description", v.name), self.named_volumes.contains_key(&v.name), Some(v.description.as_str()));
        }
        if self.notes.len() == m.notes.len() {
            for (i, (t, n)) in self.notes.iter().zip(&m.notes).enumerate() {
                add(format!("notes[{i}].text"), t.text.is_some(), Some(n.text.as_str()));
            }
        }
        for (name, f) in &form.properties {
            let Some(t) = self.form.get(name) else { continue };
            let ui = f.ui.as_ref();
            add(format!("form.{name}.title"), t.title.is_some(), Some(f.title.as_str()));
            add(format!("form.{name}.description"), t.description.is_some(), f.description.as_deref());
            add(format!("form.{name}.placeholder"), t.placeholder.is_some(), ui.and_then(|u| u.placeholder.as_deref()));
            let labels = ui.map(|u| u.enum_labels.join("\n"));
            add(format!("form.{name}.enum_labels"), t.enum_labels.is_some(), labels.as_deref());
        }
        if self.groups.len() == form.groups.len() {
            for (i, (t, g)) in self.groups.iter().zip(&form.groups).enumerate() {
                add(format!("groups[{i}].title"), t.title.is_some(), Some(g.title.as_str()));
            }
        }
        out
    }

    /// Compares the translation with the entry: every key names a field the entry has, with
    /// a text, lists have the entry's length, no text is empty, the card line fits, and every
    /// text is stamped with the English the entry has now (`long`: `description.md`).
    pub(crate) fn check(&self, app: &str, code: &str, m: &AppManifest, form: &FormSchema, long: &str, issues: &mut Issues) {
        let mut c = Checker { app, rel: format!("{DIR}/{code}.toml"), issues };
        let translated = self.translated(m, form, long);
        for (key, english) in &translated {
            match self.sources.get(key) {
                None => c.err(format!("`{key}` has no stamp in [sources] (stamp it: cargo run -p rynas-apps --example catalog-i18n -- apps/{app})")),
                Some(s) if *s != stamp(english) => c.err(format!(
                    "`{key}`: the English text changed since it was translated (review the translation, then stamp it again: cargo run -p rynas-apps --example catalog-i18n -- apps/{app})"
                )),
                Some(_) => {}
            }
        }
        for key in self.sources.keys() {
            if !translated.iter().any(|(k, _)| k == key) {
                c.err(format!("[sources] `{key}` stamps no translated text"));
            }
        }
        c.text("description", self.description.as_deref(), true);
        if self.description.as_ref().is_some_and(|d| d.chars().count() > SUMMARY_MAX_CHARS) {
            c.err(format!("description is longer than {SUMMARY_MAX_CHARS} characters (the long text goes in {code}.md)"));
        }
        for (name, t) in &self.ports {
            let key = format!("ports.{name}");
            if c.item(&key, m.port(name).is_some()) {
                c.text(&format!("{key}.purpose"), Some(&t.purpose), true);
            }
        }
        for (env, t) in &self.folders {
            let key = format!("folders.{env}");
            if c.item(&key, m.folders.iter().any(|f| &f.env == env)) {
                c.text(&format!("{key}.description"), Some(&t.description), true);
            }
        }
        for (env, t) in &self.shares {
            let key = format!("shares.{env}");
            if let Some(s) = m.shares.iter().find(|s| &s.env == env) {
                c.label(&key, t, has(&s.label), has(&s.description));
            } else {
                c.item(&key, false);
            }
        }
        if let Some(t) = &self.share_set {
            match &m.share_set {
                Some(s) => c.label("share_set", t, has(&s.label), has(&s.description)),
                None => c.err("`share_set`: the entry has no [share_set]".into()),
            }
        }
        for (env, t) in &self.secrets {
            let key = format!("secrets.{env}");
            if let Some(s) = m.secrets.iter().find(|s| &s.env == env) {
                c.text(&format!("{key}.description"), Some(&t.description), has(&s.description));
            } else {
                c.item(&key, false);
            }
        }
        for (name, t) in &self.overlays {
            let key = format!("overlays.{name}");
            if c.item(&key, m.overlays.iter().any(|o| overlay_name(&o.file) == Some(name.as_str()))) {
                c.text(&format!("{key}.description"), Some(&t.description), true);
            }
        }
        for (name, t) in &self.named_volumes {
            let key = format!("named_volumes.{name}");
            if c.item(&key, m.named_volumes.iter().any(|v| &v.name == name)) {
                c.text(&format!("{key}.description"), Some(&t.description), true);
            }
        }
        if !self.notes.is_empty() && self.notes.len() != m.notes.len() {
            c.err(format!("{} [[notes]] for the {} notes of app.toml (one per note, in order)", self.notes.len(), m.notes.len()));
        } else {
            for (i, t) in self.notes.iter().enumerate() {
                c.text(&format!("notes[{i}].text"), t.text.as_deref(), true);
            }
        }
        for (name, t) in &self.form {
            let key = format!("form.{name}");
            let Some(f) = form.properties.get(name) else {
                c.item(&key, false);
                continue;
            };
            let ui = f.ui.as_ref();
            c.text(&format!("{key}.title"), t.title.as_deref(), true);
            c.text(&format!("{key}.description"), t.description.as_deref(), f.description.as_deref().is_some_and(has));
            c.text(&format!("{key}.placeholder"), t.placeholder.as_deref(), ui.and_then(|u| u.placeholder.as_deref()).is_some_and(has));
            if let Some(labels) = &t.enum_labels {
                let source = ui.map_or(0, |u| u.enum_labels.len());
                if source == 0 {
                    c.err(format!("`{key}.enum_labels`: the field has no enum_labels to translate"));
                } else if labels.len() != source {
                    c.err(format!("`{key}.enum_labels` has {} labels, the field {source}", labels.len()));
                } else if labels.iter().any(|l| !has(l)) {
                    c.err(format!("`{key}.enum_labels` has an empty label"));
                }
            }
            if t.title.is_none() && t.description.is_none() && t.placeholder.is_none() && t.enum_labels.is_none() {
                c.err(format!("`{key}` translates nothing"));
            }
        }
        if !self.groups.is_empty() && self.groups.len() != form.groups.len() {
            c.err(format!("{} [[groups]] for the {} x-groups of form.json (one per group, in order)", self.groups.len(), form.groups.len()));
        } else {
            for (i, t) in self.groups.iter().enumerate() {
                c.text(&format!("groups[{i}].title"), t.title.as_deref(), true);
            }
        }
    }

    /// The API form of the translation for `app`; `card`: only what [`rynas_api_types::apps::AppCard`]
    /// carries (the summary and the ports' purposes). `None` when it translates none of that.
    pub fn texts(&self, lang: &str, app: &CatalogApp, card: bool) -> Option<AppTexts> {
        let desc = |m: &BTreeMap<String, DescriptionText>| -> BTreeMap<String, String> { m.iter().map(|(k, v)| (k.clone(), v.description.clone())).collect() };
        let label = |t: &LabelText| AppLabelTexts { label: t.label.clone(), description: t.description.clone() };
        let numbered = |v: Vec<Option<&String>>| -> BTreeMap<u32, String> {
            v.into_iter().enumerate().filter_map(|(i, t)| Some((u32::try_from(i).ok()?, t?.clone()))).collect()
        };
        let mut out = AppTexts {
            lang: lang.to_owned(),
            summary: self.description.clone(),
            ports: self.ports.iter().map(|(k, v)| (k.clone(), v.purpose.clone())).collect(),
            ..AppTexts::default()
        };
        if card {
            return (out.summary.is_some() || !out.ports.is_empty()).then_some(out);
        }
        out.description = self.long_description.clone();
        out.folders = desc(&self.folders);
        out.shares = self.shares.iter().map(|(k, v)| (k.clone(), label(v))).collect();
        out.share_set = self.share_set.as_ref().map(label);
        out.secrets = desc(&self.secrets);
        out.overlays = app
            .manifest
            .overlays
            .iter()
            .filter_map(|o| Some((o.file.clone(), self.overlays.get(overlay_name(&o.file)?)?.description.clone())))
            .collect();
        out.named_volumes = desc(&self.named_volumes);
        out.notes = numbered(self.notes.iter().map(|n| n.text.as_ref()).collect());
        out.form = self
            .form
            .iter()
            .map(|(k, v)| {
                let t = AppFieldTexts {
                    title: v.title.clone(),
                    description: v.description.clone(),
                    placeholder: v.placeholder.clone(),
                    enum_labels: v.enum_labels.clone().unwrap_or_default(),
                };
                (k.clone(), t)
            })
            .collect();
        out.groups = numbered(self.groups.iter().map(|g| g.title.as_ref()).collect());
        Some(out)
    }
}

impl Checker<'_> {
    /// A label and description for an item whose label and description exist or not.
    fn label(&mut self, key: &str, t: &LabelText, label: bool, description: bool) {
        if t.label.is_none() && t.description.is_none() {
            self.err(format!("`{key}` translates nothing"));
        }
        self.text(&format!("{key}.label"), t.label.as_deref(), label);
        self.text(&format!("{key}.description"), t.description.as_deref(), description);
    }
}

/// The translation for the language `want` (a BCP 47 code, any case): the one with that
/// code, else the one of its primary language (`it` for `it-CH`). Never another region or
/// script of the same language (`zh-TW` does not get `zh-CN`), never for English.
pub fn pick<'a>(translations: &'a BTreeMap<String, Translation>, want: &str) -> Option<(&'a str, &'a Translation)> {
    let want = want.trim();
    let find = |code: &str| translations.iter().find(|(k, _)| k.eq_ignore_ascii_case(code)).map(|(k, t)| (k.as_str(), t));
    if want.is_empty() {
        return None;
    }
    find(want).or_else(|| find(want.split_once('-')?.0))
}

/// Stamps every translation of the entry at `entry` with the English it has now: rewrites the
/// `[sources]` table of each `i18n/<lang>.toml` (made for a `<lang>.md` alone), the rest of
/// the file as it was. For after a review of the translations; returns the files written.
/// Fails, writing nothing, on a file it cannot read or parse.
pub fn restamp(entry: &Path) -> Result<Vec<PathBuf>, String> {
    let read = |p: &Path| fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
    let m: AppManifest = toml::from_str(&read(&entry.join("app.toml"))?).map_err(|e| format!("app.toml: {e}"))?;
    let form_path = entry.join("form.json");
    let form = if form_path.exists() { FormSchema::parse(&read(&form_path)?).map_err(|e| format!("form.json: {e}"))? } else { FormSchema::empty() };
    let long = read(&entry.join("description.md"))?;
    let mut issues = Issues::default();
    let translations = load(entry, &m.id, &mut issues);
    if !issues.is_empty() {
        return Err(issues.to_string());
    }
    let mut writes = Vec::new();
    for (code, t) in &translations {
        let path = entry.join(DIR).join(format!("{code}.toml"));
        let text = if path.exists() { read(&path)? } else { String::new() };
        let mut block = format!("{SOURCES_COMMENT}\n[sources]\n");
        for (key, english) in t.translated(&m, &form, &long) {
            block.push_str(&format!("\"{key}\" = \"{}\"\n", stamp(&english)));
        }
        let new = with_sources(&text, &block);
        if new != text {
            writes.push((path, new));
        }
    }
    let mut out = Vec::new();
    for (p, text) in writes {
        fs::write(&p, text).map_err(|e| format!("{}: {e}", p.display()))?;
        out.push(p);
    }
    Ok(out)
}

/// `text` (a translation file) with its `[sources]` table, and the comment above it, replaced
/// by `block` at the end.
fn with_sources(text: &str, block: &str) -> String {
    let comment: Vec<&str> = SOURCES_COMMENT.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut in_sources = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_sources = t == "[sources]";
        }
        if !in_sources && !comment.contains(&line) {
            kept.push(line);
        }
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    if kept.is_empty() { block.to_owned() } else { format!("{}\n\n{block}", kept.join("\n")) }
}

impl CatalogApp {
    /// Every translated text of the entry in the language `want` asks for ([`pick`]);
    /// `None` without a translation for it.
    pub fn texts(&self, want: &str) -> Option<AppTexts> {
        let (code, t) = pick(&self.translations, want)?;
        t.texts(code, self, false)
    }

    /// The card's texts (summary, port purposes) in the language `want` asks for.
    pub fn card_texts(&self, want: &str) -> Option<AppTexts> {
        let (code, t) = pick(&self.translations, want)?;
        t.texts(code, self, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes() {
        for ok in ["it", "zh-CN", "pt-BR", "zh-Hant", "zh-Hant-TW", "es-419", "ca-valencia", "fil"] {
            assert!(is_lang_code(ok), "{ok}");
        }
        for bad in ["en", "", "zh-cn", "ZH-CN", "zh_CN", "zh-", "-CN", "i", "abcd", "zh-hant", "it/../x", "de-CH-1901-x"] {
            assert!(!is_lang_code(bad), "{bad}");
        }
    }

    #[test]
    fn pick_falls_back_to_the_primary_language_only() {
        let mut t = BTreeMap::new();
        t.insert("zh-CN".to_string(), Translation { description: Some("中文".into()), ..Translation::default() });
        t.insert("it".to_string(), Translation { description: Some("italiano".into()), ..Translation::default() });
        let code = |want: &str| pick(&t, want).map(|(c, _)| c);
        assert_eq!(code("zh-CN"), Some("zh-CN"));
        assert_eq!(code("zh-cn"), Some("zh-CN"));
        assert_eq!(code(" it "), Some("it"));
        assert_eq!(code("it-CH"), Some("it"));
        assert_eq!(code("zh-TW"), None);
        assert_eq!(code("zh"), None);
        assert_eq!(code("en"), None);
        assert_eq!(code("en-US"), None);
        assert_eq!(code(""), None);
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(toml::from_str::<Translation>("summary = \"x\"").is_err());
        assert!(toml::from_str::<Translation>("[ports.web]\npurpose = \"x\"\nlabel = \"y\"").is_err());
        assert!(toml::from_str::<Translation>("[form.X]\nhelp = \"x\"").is_err());
        let t: Translation = toml::from_str("description = \"x\"\n[ports]\nweb.purpose = \"y\"\n[[notes]]\n[[notes]]\ntext = \"z\"").unwrap();
        assert_eq!(t.ports["web"].purpose, "y");
        assert_eq!(t.notes, vec![NoteText { text: None }, NoteText { text: Some("z".into()) }]);
    }
}
