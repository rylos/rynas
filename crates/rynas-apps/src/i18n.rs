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
//! behind silently. English stays the source: whatever a translation leaves out is shown in
//! English.
//!
//! The server picks the translation for the language a client asks for ([`pick`]) and sends
//! it as [`AppTexts`] next to the English fields, which keep their meaning.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use rynas_api_types::apps::{AppFieldTexts, AppLabelTexts, AppTexts};
use serde::Deserialize;

use crate::catalog::{CatalogApp, file_name, list};
use crate::error::Issues;
use crate::form::FormSchema;
use crate::manifest::AppManifest;

/// The folder of the translations inside an entry.
pub const DIR: &str = "i18n";

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
    #[serde(default)]
    pub user_photos: Option<DescriptionText>,
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

/// Reads `i18n/` of an entry: every `<lang>.toml` and `<lang>.md`, by language. Problems go
/// to `issues`; [`Translation::check`] compares each one with the entry afterwards.
pub(crate) fn load(dir: &Path, app: &str, issues: &mut Issues) -> BTreeMap<String, Translation> {
    let mut out: BTreeMap<String, Translation> = BTreeMap::new();
    for p in list(&dir.join(DIR)) {
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
    /// Compares the translation with the entry: every key names a field the entry has, with
    /// a text, lists have the entry's length, no text is empty, the card line fits.
    pub(crate) fn check(&self, app: &str, code: &str, m: &AppManifest, form: &FormSchema, issues: &mut Issues) {
        let mut c = Checker { app, rel: format!("{DIR}/{code}.toml"), issues };
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
        if let Some(t) = &self.user_photos {
            match &m.user_photos {
                Some(u) => c.text("user_photos.description", Some(&t.description), has(&u.description)),
                None => c.err("`user_photos`: the entry has no [user_photos]".into()),
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
