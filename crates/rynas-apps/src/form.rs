//! `form.json`: the questions of the install wizard, a small subset of JSON Schema
//! (draft 2020-12 keywords) plus `x-` UI hints, so one generic schema-driven form in the
//! Leptos UI renders every entry.
//!
//! Supported:
//! - top level: `$schema` (ignored), `type: "object"`, `properties`, `required`,
//!   `x-groups` (ordered `[{ "title", "fields": [...] }]`, every property exactly once);
//! - per property: `type` (`string` | `integer` | `boolean`), `title`, `description`,
//!   `default`, `enum`, `minimum`, `maximum`, `minLength`, `maxLength`, `pattern`
//!   (anchored, regex-lite syntax), `format` (`hostname` | `uri` | `email`), and `x-ui`
//!   with `widget` (`text` | `password` | `textarea` | `select` | `switch`), `placeholder`,
//!   `advanced` (folded by default), `enum_labels`, `visible_if` (`{ "FIELD": value }`).
//!
//! Property names are `.env` names (`[A-Z][A-Z0-9_]*`): the answer is written to `.env`
//! under that name, booleans as `true`/`false`. Unknown keywords are refused.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Issues, RenderError};
use crate::manifest::is_env_name;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormSchema {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub properties: BTreeMap<String, Field>,
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(rename = "x-groups", default)]
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    pub title: String,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Integer,
    Boolean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Hostname,
    Uri,
    Email,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    #[serde(rename = "type")]
    pub ty: FieldType,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(rename = "enum", default, skip_serializing_if = "Option::is_none")]
    pub enum_values: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<i64>,
    #[serde(rename = "minLength", default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u32>,
    #[serde(rename = "maxLength", default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    #[serde(rename = "x-ui", default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiHints>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Widget {
    Text,
    Password,
    Textarea,
    Select,
    Switch,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiHints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<Widget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub advanced: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enum_labels: Vec<String>,
    /// Shown (and required) only when every listed field has the given value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub visible_if: BTreeMap<String, Value>,
}

impl FormSchema {
    /// The empty form (an entry without `form.json`).
    pub fn empty() -> Self {
        Self { schema: None, ty: "object".into(), properties: BTreeMap::new(), required: Vec::new(), groups: Vec::new() }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }

    /// Checks the schema itself. `taken` are names already used by app.toml or rynas.
    pub(crate) fn check(&self, app: &str, taken: &[&str], issues: &mut Issues) {
        let mut err = |m: String| issues.push(app, "form.json", m);
        if self.ty != "object" {
            err("top-level type must be \"object\"".into());
        }
        for r in &self.required {
            if !self.properties.contains_key(r) {
                err(format!("required `{r}` is not a property"));
            }
        }
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for g in &self.groups {
            if g.title.trim().is_empty() {
                err("a group has an empty title".into());
            }
            for f in &g.fields {
                *seen.entry(f.as_str()).or_default() += 1;
            }
        }
        for (name, n) in &seen {
            if !self.properties.contains_key(*name) {
                err(format!("x-groups lists unknown field `{name}`"));
            } else if *n > 1 {
                err(format!("x-groups lists `{name}` {n} times"));
            }
        }
        for (name, field) in &self.properties {
            if !seen.contains_key(name.as_str()) {
                err(format!("`{name}` is in no x-groups entry"));
            }
            if !is_env_name(name) {
                err(format!("property `{name}` is not an env name ([A-Z][A-Z0-9_]*)"));
            }
            if taken.contains(&name.as_str()) {
                err(format!("property `{name}` collides with a name declared in app.toml or provided by rynas"));
            }
            for m in field.check_schema(name, self) {
                err(m);
            }
        }
    }

    /// Validates the answers, applies defaults and returns the `.env` strings for every
    /// property (an optional field without answer or default becomes empty).
    pub fn resolve(&self, answers: &BTreeMap<String, Value>) -> Result<BTreeMap<String, String>, RenderError> {
        if let Some(k) = answers.keys().find(|k| !self.properties.contains_key(*k)) {
            return Err(RenderError::UnknownValue(k.clone()));
        }
        // effective values first (visible_if refers to them)
        let mut eff: BTreeMap<&str, Option<&Value>> = BTreeMap::new();
        for (name, field) in &self.properties {
            let v = answers.get(name).filter(|v| !v.is_null()).or(field.default.as_ref());
            eff.insert(name, v);
        }
        let mut out = BTreeMap::new();
        for (name, field) in &self.properties {
            let visible = field.ui.as_ref().is_none_or(|ui| {
                ui.visible_if.iter().all(|(k, want)| eff.get(k.as_str()).copied().flatten() == Some(want))
            });
            let value = eff[name.as_str()];
            let s = match value {
                None => {
                    if visible && self.required.contains(name) {
                        return Err(RenderError::MissingValue(name.clone()));
                    }
                    String::new()
                }
                Some(v) => field.validate(name, v)?,
            };
            if visible && s.is_empty() && self.required.contains(name) {
                return Err(RenderError::MissingValue(name.clone()));
            }
            out.insert(name.clone(), s);
        }
        Ok(out)
    }

    /// Names of boolean properties.
    pub fn booleans(&self) -> impl Iterator<Item = &str> {
        self.properties.iter().filter(|(_, f)| f.ty == FieldType::Boolean).map(|(k, _)| k.as_str())
    }
}

impl Field {
    fn check_schema(&self, name: &str, form: &FormSchema) -> Vec<String> {
        let mut out = Vec::new();
        if self.title.trim().is_empty() {
            out.push(format!("{name}: title is empty"));
        }
        let is_str = self.ty == FieldType::String;
        let is_int = self.ty == FieldType::Integer;
        if !is_str && (self.min_length.is_some() || self.max_length.is_some() || self.pattern.is_some() || self.format.is_some()) {
            out.push(format!("{name}: minLength/maxLength/pattern/format apply to strings only"));
        }
        if !is_int && (self.minimum.is_some() || self.maximum.is_some()) {
            out.push(format!("{name}: minimum/maximum apply to integers only"));
        }
        if let (Some(a), Some(b)) = (self.minimum, self.maximum)
            && a > b
        {
            out.push(format!("{name}: minimum > maximum"));
        }
        if let (Some(a), Some(b)) = (self.min_length, self.max_length)
            && a > b
        {
            out.push(format!("{name}: minLength > maxLength"));
        }
        if let Some(p) = &self.pattern
            && let Err(e) = regex_lite::Regex::new(&format!("^(?:{p})$"))
        {
            out.push(format!("{name}: pattern does not compile: {e}"));
        }
        if let Some(values) = &self.enum_values {
            if values.is_empty() {
                out.push(format!("{name}: enum is empty"));
            }
            if self.ty == FieldType::Boolean {
                out.push(format!("{name}: enum on a boolean"));
            }
            for v in values {
                if let Err(e) = self.validate_type_and_bounds(name, v) {
                    out.push(format!("enum value: {e}"));
                }
            }
        }
        if let Some(d) = &self.default
            && let Err(e) = self.validate(name, d)
        {
            out.push(format!("default: {e}"));
        }
        if let Some(ui) = &self.ui {
            let lens = self.enum_values.as_ref().map_or(0, Vec::len);
            if !ui.enum_labels.is_empty() && ui.enum_labels.len() != lens {
                out.push(format!("{name}: x-ui.enum_labels must match enum"));
            }
            match ui.widget {
                Some(Widget::Switch) if self.ty != FieldType::Boolean => out.push(format!("{name}: switch widget on a non-boolean")),
                Some(Widget::Select) if self.enum_values.is_none() => out.push(format!("{name}: select widget without enum")),
                Some(Widget::Password | Widget::Textarea | Widget::Text) if !is_str => out.push(format!("{name}: text widgets need a string")),
                _ => {}
            }
            for k in ui.visible_if.keys() {
                if k == name || !form.properties.contains_key(k) {
                    out.push(format!("{name}: visible_if refers to unknown field `{k}`"));
                }
            }
        }
        out
    }

    fn validate_type_and_bounds(&self, name: &str, v: &Value) -> Result<String, RenderError> {
        let bad = |reason: String| RenderError::InvalidValue { field: name.to_owned(), reason };
        match self.ty {
            FieldType::Boolean => match v {
                Value::Bool(b) => Ok(b.to_string()),
                _ => Err(bad("expected true or false".into())),
            },
            FieldType::Integer => {
                let n = v.as_i64().ok_or_else(|| bad("expected an integer".into()))?;
                if self.minimum.is_some_and(|m| n < m) || self.maximum.is_some_and(|m| n > m) {
                    return Err(bad(format!("{n} is out of range")));
                }
                Ok(n.to_string())
            }
            FieldType::String => {
                let s = v.as_str().ok_or_else(|| bad("expected a string".into()))?;
                let len = s.chars().count() as u32;
                if self.min_length.is_some_and(|m| len < m) && !s.is_empty() {
                    return Err(bad(format!("shorter than {} characters", self.min_length.unwrap_or(0))));
                }
                if self.max_length.is_some_and(|m| len > m) {
                    return Err(bad(format!("longer than {} characters", self.max_length.unwrap_or(0))));
                }
                if s.is_empty() {
                    return Ok(String::new());
                }
                if let Some(p) = &self.pattern {
                    let re = regex_lite::Regex::new(&format!("^(?:{p})$")).map_err(|e| bad(e.to_string()))?;
                    if !re.is_match(s) {
                        return Err(bad("does not match the expected format".into()));
                    }
                }
                match self.format {
                    Some(Format::Hostname) if !is_hostname(s) => return Err(bad("not a host name".into())),
                    Some(Format::Uri) if !is_uri(s) => return Err(bad("not an http(s) URL".into())),
                    Some(Format::Email) if !is_email(s) => return Err(bad("not an e-mail address".into())),
                    _ => {}
                }
                Ok(s.to_owned())
            }
        }
    }

    /// Validates one value and returns its `.env` string.
    pub fn validate(&self, name: &str, v: &Value) -> Result<String, RenderError> {
        let s = self.validate_type_and_bounds(name, v)?;
        if let Some(values) = &self.enum_values
            && !values.contains(v)
        {
            return Err(RenderError::InvalidValue { field: name.to_owned(), reason: "not one of the allowed values".into() });
        }
        Ok(s)
    }
}

fn is_hostname(s: &str) -> bool {
    s.len() <= 253
        && s.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

fn is_uri(s: &str) -> bool {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"));
    match rest {
        Some(r) => {
            let host = r.split(['/', '?', '#']).next().unwrap_or("");
            let host = host.rsplit_once(':').map_or(host, |(h, p)| if p.bytes().all(|c| c.is_ascii_digit()) { h } else { host });
            !host.is_empty() && (is_hostname(host) || host.starts_with('['))
        }
        None => false,
    }
}

fn is_email(s: &str) -> bool {
    match s.split_once('@') {
        Some((l, d)) => !l.is_empty() && !l.contains(char::is_whitespace) && d.contains('.') && is_hostname(d),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form() -> FormSchema {
        FormSchema::parse(
            r#"{
              "type": "object",
              "properties": {
                "DOMAIN": {"type": "string", "title": "URL", "format": "uri"},
                "SIGNUPS": {"type": "boolean", "title": "Signups", "default": false},
                "LEVEL": {"type": "string", "title": "Level", "enum": ["a", "b"], "default": "a",
                          "x-ui": {"widget": "select", "enum_labels": ["A", "B"]}},
                "WORKERS": {"type": "integer", "title": "Workers", "minimum": 1, "maximum": 8, "default": 2},
                "INVITE": {"type": "string", "title": "Invite", "pattern": "[a-z]+",
                           "x-ui": {"visible_if": {"SIGNUPS": true}}}
              },
              "required": ["DOMAIN", "INVITE"],
              "x-groups": [{"title": "General", "fields": ["DOMAIN", "SIGNUPS", "INVITE"]},
                           {"title": "Advanced", "fields": ["LEVEL", "WORKERS"]}]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn schema_is_valid() {
        let mut issues = Issues::default();
        form().check("t", &[], &mut issues);
        assert!(issues.is_empty(), "{issues}");
    }

    #[test]
    fn resolve_defaults_and_visibility() {
        let f = form();
        let mut a = BTreeMap::new();
        a.insert("DOMAIN".into(), json!("https://vault.example.lan"));
        let v = f.resolve(&a).unwrap();
        assert_eq!(v["SIGNUPS"], "false");
        assert_eq!(v["WORKERS"], "2");
        assert_eq!(v["INVITE"], "", "hidden required field is not required");
        a.insert("SIGNUPS".into(), json!(true));
        assert_eq!(f.resolve(&a), Err(RenderError::MissingValue("INVITE".into())));
        a.insert("INVITE".into(), json!("Abc"));
        assert!(matches!(f.resolve(&a), Err(RenderError::InvalidValue { .. })));
        a.insert("INVITE".into(), json!("abc"));
        a.insert("WORKERS".into(), json!(9));
        assert!(matches!(f.resolve(&a), Err(RenderError::InvalidValue { .. })));
        a.insert("WORKERS".into(), json!(8));
        a.insert("LEVEL".into(), json!("c"));
        assert!(matches!(f.resolve(&a), Err(RenderError::InvalidValue { .. })));
        a.insert("LEVEL".into(), json!("b"));
        a.insert("NOPE".into(), json!(1));
        assert_eq!(f.resolve(&a), Err(RenderError::UnknownValue("NOPE".into())));
    }

    #[test]
    fn bad_schema_is_reported() {
        let f = FormSchema::parse(
            r#"{"type":"object","properties":{"x":{"type":"integer","title":"","minLength":1,"default":"s"}},
                "required":["Y"],"x-groups":[{"title":"G","fields":["x","x"]}]}"#,
        )
        .unwrap();
        let mut issues = Issues::default();
        f.check("t", &[], &mut issues);
        let text = issues.to_string();
        for needle in ["required `Y`", "2 times", "not an env name", "title is empty", "strings only", "default:"] {
            assert!(text.contains(needle), "{needle} missing in\n{text}");
        }
        assert!(FormSchema::parse(r#"{"type":"object","properties":{"X":{"type":"string","title":"t","oneOf":[]}}}"#).is_err());
    }
}
