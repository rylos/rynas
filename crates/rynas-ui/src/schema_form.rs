//! `SchemaForm`: one generic form rendered from a JSON Schema subset (the install questions of
//! a catalogue entry, `form.json`; `docs/phase2/docker-apps.md` §6.6), reusable by anything
//! that ships the same subset (plugins later).
//!
//! Supported, and nothing else (the catalogue lint refuses other keywords):
//! - top level: `properties`, `required`, `x-groups` (sections and order; a property missing
//!   from every group is shown in a last untitled section);
//! - property: `type` `string` | `integer` | `boolean`, `title`, `description`, `default`,
//!   `enum`, `minimum`, `maximum`, `minLength`, `maxLength`, `pattern` (anchored, checked with
//!   the browser's `RegExp`), `format` `hostname` | `uri` (http/https) | `email`;
//! - `x-ui`: `widget` `text` | `password` | `textarea` | `select` | `switch`, `placeholder`,
//!   `advanced` (folded under "Advanced options"), `enum_labels`, `visible_if` (a hidden field
//!   is neither required nor sent);
//! - overlays (`AppOverlay::when` = a boolean field, optionally `!`-negated): the switch shows
//!   what turning it on (or off) adds.
//!
//! Titles and descriptions come in English from the entry and are shown as they are
//! (`lang="en"`); messages of the client checks are translated. Values are JSON values
//! (string, integer, boolean) keyed by property name: [`defaults`] → edits → [`answers`].

use std::collections::BTreeMap;

use leptos::prelude::*;
use rynas_api_types::apps::{AppForm, AppOverlay, FormField, FormFieldType, FormFormat, FormWidget};
use serde_json::Value;

use crate::form::fid;
use crate::i18n::{t, tf, tr};
use crate::icons::{self, Icon};

/// Answers by property name.
pub type Values = BTreeMap<String, Value>;
/// Error text by property name (translated, maybe with the server's detail).
pub type Errors = BTreeMap<String, String>;

/// The initial answers: every `default` of the form.
pub fn defaults(form: &AppForm) -> Values {
    form.properties.iter().filter_map(|(k, f)| f.default.clone().map(|d| (k.clone(), d))).collect()
}

/// The value of `name` (its default when unanswered).
fn value_of<'a>(form: &'a AppForm, values: &'a Values, name: &str) -> Option<&'a Value> {
    values.get(name).or_else(|| form.properties.get(name).and_then(|f| f.default.as_ref()))
}

/// Every `visible_if` condition holds.
pub fn visible(form: &AppForm, values: &Values, field: &FormField) -> bool {
    field.ui.as_ref().is_none_or(|ui| ui.visible_if.iter().all(|(k, want)| value_of(form, values, k) == Some(want)))
}

/// Empty: not answered (an empty string counts as unanswered).
fn empty(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        _ => false,
    }
}

/// The answers to send: visible fields only, unanswered optional ones left out, integers typed
/// as text parsed.
pub fn answers(form: &AppForm, values: &Values) -> Values {
    let mut out = Values::new();
    for (k, f) in &form.properties {
        if !visible(form, values, f) {
            continue;
        }
        let Some(v) = value_of(form, values, k) else { continue };
        if empty(Some(v)) {
            continue;
        }
        let v = match (f.ty, v) {
            (FormFieldType::Integer, Value::String(s)) => s.trim().parse::<i64>().map(Value::from).unwrap_or_else(|_| v.clone()),
            _ => v.clone(),
        };
        out.insert(k.clone(), v);
    }
    out
}

/// Why a value fails the form's rules: an i18n key and its `{n}` argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub key: &'static str,
    pub n: Option<i64>,
}

fn problem(key: &'static str, n: Option<i64>) -> Option<Problem> {
    Some(Problem { key, n })
}

/// Pattern checker: `Some(matches)`, `None` when the pattern does not compile (the server
/// decides then).
pub type PatternCheck = fn(&str, &str) -> Option<bool>;

/// The browser's `RegExp`, anchored like JSON Schema patterns of the catalogue.
pub fn js_pattern(pattern: &str, value: &str) -> Option<bool> {
    use wasm_bindgen::{JsCast, JsValue};
    // `new RegExp` throws on a pattern it cannot compile: construct through `Reflect` to get
    // the exception as an error instead of an abort
    let ctor: js_sys::Function = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("RegExp")).ok()?.dyn_into().ok()?;
    let args = js_sys::Array::of2(&JsValue::from_str(&format!("^(?:{pattern})$")), &JsValue::from_str("u"));
    let re: js_sys::RegExp = js_sys::Reflect::construct(&ctor, &args).ok()?.dyn_into().ok()?;
    Some(re.test(value))
}

fn hostname_ok(s: &str) -> bool {
    s.len() <= 253
        && s.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

fn uri_ok(s: &str) -> bool {
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/') && !r.contains(char::is_whitespace))
}

fn email_ok(s: &str) -> bool {
    s.split_once('@').is_some_and(|(a, b)| !a.is_empty() && b.contains('.') && !b.starts_with('.') && !b.ends_with('.') && !s.contains(char::is_whitespace))
}

/// The first rule `value` breaks (required, type, enum, range, length, pattern, format).
pub fn check(field: &FormField, required: bool, value: Option<&Value>, pattern: PatternCheck) -> Option<Problem> {
    if empty(value) {
        return if required && field.ty != FormFieldType::Boolean { problem("sf.err.required", None) } else { None };
    }
    let v = value?;
    if let Some(allowed) = &field.enum_values
        && !allowed.contains(v)
        && !matches!(v, Value::String(s) if allowed.iter().any(|a| a.as_i64().is_some_and(|n| s.trim().parse::<i64>() == Ok(n))))
    {
        return problem("sf.err.enum", None);
    }
    match field.ty {
        FormFieldType::Boolean => None,
        FormFieldType::Integer => {
            let n = match v {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            };
            let Some(n) = n else { return problem("sf.err.integer", None) };
            if let Some(min) = field.minimum
                && n < min
            {
                return problem("sf.err.min", Some(min));
            }
            if let Some(max) = field.maximum
                && n > max
            {
                return problem("sf.err.max", Some(max));
            }
            None
        }
        FormFieldType::String => {
            let s = v.as_str().unwrap_or_default();
            let len = s.chars().count() as i64;
            if let Some(min) = field.min_length
                && len < i64::from(min)
            {
                return problem("sf.err.min_len", Some(i64::from(min)));
            }
            if let Some(max) = field.max_length
                && len > i64::from(max)
            {
                return problem("sf.err.max_len", Some(i64::from(max)));
            }
            match field.format {
                Some(FormFormat::Hostname) if !hostname_ok(s) => return problem("sf.err.hostname", None),
                Some(FormFormat::Uri) if !uri_ok(s) => return problem("sf.err.uri", None),
                Some(FormFormat::Email) if !email_ok(s) => return problem("sf.err.email", None),
                _ => {}
            }
            if let Some(p) = &field.pattern
                && pattern(p, s) == Some(false)
            {
                return problem("sf.err.pattern", None);
            }
            None
        }
    }
}

/// Every problem of the visible fields, as translated texts.
pub fn errors(form: &AppForm, values: &Values, only: Option<&[String]>, pattern: PatternCheck) -> Errors {
    form.properties
        .iter()
        .filter(|(k, f)| only.is_none_or(|o| o.contains(k)) && visible(form, values, f))
        .filter_map(|(k, f)| check(f, form.required.contains(k), value_of(form, values, k), pattern).map(|p| (k.clone(), problem_text(&p))))
        .collect()
}

pub fn problem_text(p: &Problem) -> String {
    match p.n {
        Some(n) => tf(p.key, &[("n", &n.to_string())]),
        None => t(p.key).to_string(),
    }
}

/// The sections to draw: (title, fields) in `x-groups` order, then the properties no group
/// names. `only` keeps those fields (an update asking for the missing answers).
pub fn sections(form: &AppForm, only: Option<&[String]>) -> Vec<(String, Vec<String>)> {
    let keep = |k: &String| form.properties.contains_key(k) && only.is_none_or(|o| o.contains(k));
    let mut out: Vec<(String, Vec<String>)> = form.groups.iter().map(|g| (g.title.clone(), g.fields.iter().filter(|k| keep(k)).cloned().collect())).collect();
    let rest: Vec<String> = form.properties.keys().filter(|k| keep(k) && !form.groups.iter().any(|g| g.fields.contains(k))).cloned().collect();
    if !rest.is_empty() {
        out.push((String::new(), rest));
    }
    out.retain(|(_, f)| !f.is_empty());
    out
}

/// The text of a JSON value in a text input.
fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// What a form draws and reads: the schema, the answers, the overlays (switch hints), the
/// server's field errors and whether every client error shows (after a Next).
#[derive(Clone)]
pub struct SchemaForm {
    pub form: StoredValue<AppForm>,
    pub overlays: StoredValue<Vec<AppOverlay>>,
    pub values: RwSignal<Values>,
    pub server: Signal<Errors>,
    /// Show every client error now (a Next or Save was pressed), not only those of left fields.
    pub reveal: Signal<bool>,
    pub only: Option<StoredValue<Vec<String>>>,
}

impl SchemaForm {
    /// The form as a view: one `fieldset` per section, advanced fields folded at its end.
    pub fn view(self) -> AnyView {
        let only = self.only.map(|o| o.get_value());
        let secs = self.form.with_value(|f| sections(f, only.as_deref()));
        if secs.is_empty() {
            return view! { <p class="muted small">{tr("sf.no_questions")}</p> }.into_any();
        }
        secs.into_iter()
            .map(|(title, fields)| {
                let advanced = |k: &String| self.form.with_value(|f| f.properties.get(k).and_then(|p| p.ui.as_ref()).is_some_and(|u| u.advanced));
                let (adv, main): (Vec<String>, Vec<String>) = fields.into_iter().partition(advanced);
                let main = main.into_iter().map(|k| self.clone().field(k)).collect_view();
                let adv = (!adv.is_empty()).then(|| {
                    let rows = adv.into_iter().map(|k| self.clone().field(k)).collect_view();
                    view! {
                        <details class="sf-adv">
                            <summary><Icon icon=icons::CHEVRON_RIGHT size=14 />{tr("sf.advanced")}</summary>
                            <div class="form">{rows}</div>
                        </details>
                    }
                });
                view! {
                    <fieldset class="sf-group">
                        {(!title.is_empty()).then(|| view! { <legend class="mini-title" lang="en">{title}</legend> })}
                        <div class="form">{main}{adv}</div>
                    </fieldset>
                }
                .into_any()
            })
            .collect_view()
            .into_any()
    }

    /// One property: the widget its type and `x-ui` ask for, hidden while `visible_if` fails.
    fn field(self, name: String) -> AnyView {
        let Some(f) = self.form.with_value(|fm| fm.properties.get(&name).cloned()) else { return ().into_any() };
        let (form, values) = (self.form, self.values);
        let key = StoredValue::new(name.clone());
        let shown = Memo::new(move |_| form.with_value(|fm| values.with(|v| fm.properties.get(&key.get_value()).is_some_and(|f| visible(fm, v, f)))));
        let required = form.with_value(|fm| fm.required.contains(&name));
        let touched = RwSignal::new(false);
        let (server, reveal) = (self.server, self.reveal);
        let field_def = StoredValue::new(f.clone());
        let err = Signal::derive(move || {
            let k = key.get_value();
            if let Some(e) = server.with(|s| s.get(&k).cloned()) {
                return Some(e);
            }
            if !(touched.get() || reveal.get()) {
                return None;
            }
            let v = values.with(|v| form.with_value(|fm| value_of(fm, v, &k).cloned()));
            field_def.with_value(|f| check(f, required, v.as_ref(), js_pattern)).map(|p| problem_text(&p))
        });
        let get = Signal::derive(move || values.with(|v| form.with_value(|fm| value_of(fm, v, &key.get_value()).cloned())));
        let set = Callback::new(move |v: Value| values.update(|m| {
            m.insert(key.get_value(), v);
        }));
        let widget = f.ui.as_ref().and_then(|u| u.widget);
        let body = match (f.ty, widget) {
            (FormFieldType::Boolean, _) | (_, Some(FormWidget::Switch)) => self.switch(&name, &f, get, set),
            (_, Some(FormWidget::Select)) => select(&f, required, get, set, err),
            _ if f.enum_values.is_some() => select(&f, required, get, set, err),
            _ => input(&f, required, widget, get, set, err, touched),
        };
        view! { <div class="sf-field" hidden=move || !shown.get()>{body}</div> }.into_any()
    }

    /// A boolean: a switch, with what its overlays add.
    fn switch(&self, name: &str, f: &FormField, get: Signal<Option<Value>>, set: Callback<Value>) -> AnyView {
        let on_adds: Vec<String> = self.overlays.with_value(|o| o.iter().filter(|o| o.when == name).map(|o| o.description.clone()).collect());
        let off_adds: Vec<String> = self.overlays.with_value(|o| o.iter().filter(|o| o.when.strip_prefix('!') == Some(name)).map(|o| o.description.clone()).collect());
        let checked = move || get.with(|v| matches!(v, Some(Value::Bool(true))));
        let hint = |key: &'static str, list: Vec<String>| {
            (!list.is_empty()).then(|| view! { <span class="field-help">{tr(key)}" "<span lang="en">{list.join(" · ")}</span></span> })
        };
        view! {
            <label class="switch-row">
                <input type="checkbox" role="switch" class="switch" prop:checked=checked on:change=move |ev| set.run(Value::Bool(event_target_checked(&ev))) />
                <span class="switch-text">
                    <span class="switch-label" lang="en">{f.title.clone()}</span>
                    {f.description.clone().map(|d| view! { <span class="field-help" lang="en">{d}</span> })}
                    {hint("sf.when_on", on_adds)}
                    {hint("sf.when_off", off_adds)}
                </span>
            </label>
        }
        .into_any()
    }
}

/// Label with the "optional" marker.
fn label(f: &FormField, required: bool, id: String) -> AnyView {
    view! {
        <label class="field-label" for=id>
            <span lang="en">{f.title.clone()}</span>
            {(!required).then(|| view! { " "<span class="muted small">{tr("sf.optional")}</span> })}
        </label>
    }
    .into_any()
}

fn help_err(f: &FormField, hid: &str, eid: String, err: Signal<Option<String>>) -> AnyView {
    view! {
        {f.description.clone().map(|d| view! { <span class="field-help" id=hid.to_string() lang="en">{d}</span> })}
        <span class="field-error" id=eid aria-live="polite">
            {move || err.get().map(|e| view! { <Icon icon=icons::CIRCLE_X size=12 />{crate::ui::err_view(e)} })}
        </span>
    }
    .into_any()
}

/// Text, password, textarea and integer fields.
fn input(f: &FormField, required: bool, widget: Option<FormWidget>, get: Signal<Option<Value>>, set: Callback<Value>, err: Signal<Option<String>>, touched: RwSignal<bool>) -> AnyView {
    let id = fid();
    let (hid, eid) = (format!("{id}h"), format!("{id}e"));
    let desc = {
        let (hid, eid) = (hid.clone(), eid.clone());
        let has_help = f.description.is_some();
        move || {
            let mut v = Vec::new();
            if err.with(Option::is_some) {
                v.push(eid.clone());
            }
            if has_help {
                v.push(hid.clone());
            }
            (!v.is_empty()).then(|| v.join(" "))
        }
    };
    let integer = f.ty == FormFieldType::Integer;
    let text = move || get.with(|v| text_of(v.as_ref()));
    let on_input = move |ev: leptos::ev::Event| {
        let s = event_target_value(&ev);
        let v = if integer { s.trim().parse::<i64>().map(Value::from).unwrap_or(Value::String(s)) } else { Value::String(s) };
        set.run(v);
    };
    let placeholder = f.ui.as_ref().and_then(|u| u.placeholder.clone()).unwrap_or_default();
    let maxlength = f.max_length.unwrap_or(4096);
    let control = if widget == Some(FormWidget::Textarea) {
        view! {
            <textarea id=id.clone() class="input area" rows="4" spellcheck="false" placeholder=placeholder maxlength=maxlength
                aria-invalid=move || err.with(Option::is_some).then_some("true") aria-describedby=desc
                prop:value=text on:input=on_input on:blur=move |_| touched.set(true)></textarea>
        }
        .into_any()
    } else {
        let kind = match (integer, widget, f.format) {
            (true, _, _) => "number",
            (_, Some(FormWidget::Password), _) => "password",
            (_, _, Some(FormFormat::Email)) => "email",
            (_, _, Some(FormFormat::Uri)) => "url",
            _ => "text",
        };
        let auto = if kind == "password" { "new-password" } else { "off" };
        view! {
            <input id=id.clone() class=if integer { "input tnum" } else { "input" } type=kind placeholder=placeholder autocomplete=auto spellcheck="false"
                maxlength=(!integer).then_some(maxlength) min=f.minimum max=f.maximum inputmode=integer.then_some("numeric")
                aria-required=required.then_some("true") aria-invalid=move || err.with(Option::is_some).then_some("true") aria-describedby=desc
                prop:value=text on:input=on_input on:blur=move |_| touched.set(true) />
        }
        .into_any()
    };
    view! {
        <div class="field">
            {label(f, required, id)}
            {control}
            {help_err(f, &hid, eid, err)}
        </div>
    }
    .into_any()
}

/// An `enum`: a select with the `enum_labels` (or the values), an empty choice when optional.
fn select(f: &FormField, required: bool, get: Signal<Option<Value>>, set: Callback<Value>, err: Signal<Option<String>>) -> AnyView {
    let id = fid();
    let (hid, eid) = (format!("{id}h"), format!("{id}e"));
    let values: Vec<Value> = f.enum_values.clone().unwrap_or_default();
    let labels: Vec<String> = f.ui.as_ref().map(|u| u.enum_labels.clone()).unwrap_or_default();
    let integer = f.ty == FormFieldType::Integer;
    let opts = StoredValue::new(values.clone());
    let cur = move || get.with(|v| v.as_ref().map(|v| text_of(Some(v))).unwrap_or_default());
    let options = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let val = text_of(Some(v));
            let text = labels.get(i).cloned().unwrap_or_else(|| val.clone());
            let v2 = val.clone();
            view! { <option value=val selected=move || cur() == v2 lang="en">{text}</option> }
        })
        .collect_view();
    view! {
        <div class="field">
            {label(f, required, id.clone())}
            <select id=id class="input" aria-describedby=hid.clone() aria-invalid=move || err.with(Option::is_some).then_some("true")
                on:change=move |ev| {
                    let s = event_target_value(&ev);
                    let v = opts.with_value(|o| o.iter().find(|v| text_of(Some(v)) == s).cloned()).unwrap_or_else(|| if integer { s.parse::<i64>().map(Value::from).unwrap_or(Value::String(s)) } else { Value::String(s) });
                    set.run(v);
                }>
                {(!required).then(|| view! { <option value="" selected=move || cur().is_empty()>{tr("sf.none")}</option> })}
                {options}
            </select>
            {help_err(f, &hid, eid, err)}
        </div>
    }
    .into_any()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(p: &str, s: &str) -> Option<bool> {
        // the tests use literal patterns only: `[a-z]+` style classes
        Some(match p {
            "[A-Za-z0-9_.@-]+" => !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_.@-".contains(c)),
            "https://.*" => s.starts_with("https://"),
            _ => true,
        })
    }

    fn form() -> AppForm {
        serde_json::from_str(
            r#"{
            "type": "object",
            "properties": {
              "USER": {"type": "string", "title": "User", "default": "admin", "minLength": 1, "maxLength": 8, "pattern": "[A-Za-z0-9_.@-]+"},
              "PASS": {"type": "string", "title": "Password", "minLength": 10, "x-ui": {"widget": "password"}},
              "URL": {"type": "string", "title": "URL", "format": "uri", "pattern": "https://.*", "x-ui": {"advanced": true}},
              "ML": {"type": "boolean", "title": "ML", "default": false},
              "WORKERS": {"type": "integer", "title": "Workers", "minimum": 1, "maximum": 8, "default": 2, "x-ui": {"visible_if": {"ML": true}}},
              "MODE": {"type": "string", "title": "Mode", "enum": ["a", "b"], "x-ui": {"enum_labels": ["A", "B"]}},
              "LOOSE": {"type": "string", "title": "Not grouped"}
            },
            "required": ["USER", "PASS", "WORKERS"],
            "x-groups": [{"title": "Admin", "fields": ["USER", "PASS"]}, {"title": "More", "fields": ["URL", "ML", "WORKERS", "MODE"]}]
          }"#,
        )
        .expect("form")
    }

    #[test]
    fn defaults_and_answers() {
        let f = form();
        let v = defaults(&f);
        assert_eq!(v.get("USER"), Some(&Value::from("admin")));
        assert_eq!(v.get("ML"), Some(&Value::Bool(false)));
        // WORKERS is hidden while ML is off: not sent
        let a = answers(&f, &v);
        assert!(!a.contains_key("WORKERS"));
        assert_eq!(a.get("ML"), Some(&Value::Bool(false)));
        let mut v2 = v.clone();
        v2.insert("ML".into(), Value::Bool(true));
        v2.insert("WORKERS".into(), Value::from("4"));
        v2.insert("LOOSE".into(), Value::from("  "));
        let a = answers(&f, &v2);
        assert_eq!(a.get("WORKERS"), Some(&Value::from(4)));
        assert!(!a.contains_key("LOOSE"), "an unanswered optional field is left out");
    }

    #[test]
    fn checks() {
        let f = form();
        let mut v = defaults(&f);
        let e = errors(&f, &v, None, pat);
        assert_eq!(e.keys().collect::<Vec<_>>(), ["PASS"], "required password, hidden WORKERS not required");
        v.insert("PASS".into(), Value::from("short"));
        v.insert("USER".into(), Value::from("bad user"));
        v.insert("URL".into(), Value::from("http://x.lan"));
        v.insert("ML".into(), Value::Bool(true));
        v.insert("WORKERS".into(), Value::from(9));
        v.insert("MODE".into(), Value::from("c"));
        let p = |k: &str| check(&f.properties[k], f.required.contains(&k.to_string()), v.get(k), pat).map(|p| p.key);
        assert_eq!(p("PASS"), Some("sf.err.min_len"));
        assert_eq!(p("USER"), Some("sf.err.pattern"));
        assert_eq!(p("URL"), Some("sf.err.pattern"));
        assert_eq!(p("WORKERS"), Some("sf.err.max"));
        assert_eq!(p("MODE"), Some("sf.err.enum"));
        v.insert("WORKERS".into(), Value::from("two"));
        assert_eq!(check(&f.properties["WORKERS"], true, v.get("WORKERS"), pat).map(|p| p.key), Some("sf.err.integer"));
        assert!(hostname_ok("cloud.example.lan") && !hostname_ok("-bad.lan") && !hostname_ok("a..b"));
        assert!(uri_ok("https://docs.example.lan/x") && !uri_ok("ftp://x") && !uri_ok("https://"));
        assert!(email_ok("a@b.lan") && !email_ok("a@b") && !email_ok("@b.lan"));
    }

    #[test]
    fn sections_follow_the_groups() {
        let f = form();
        let s = sections(&f, None);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], ("Admin".to_string(), vec!["USER".to_string(), "PASS".to_string()]));
        assert_eq!(s[2], (String::new(), vec!["LOOSE".to_string()]));
        let only = vec!["PASS".to_string()];
        assert_eq!(sections(&f, Some(&only)), vec![("Admin".to_string(), vec!["PASS".to_string()])]);
    }
}
