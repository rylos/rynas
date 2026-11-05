//! Semantic diff of configuration documents.
//!
//! Objects are compared field by field (`key.field`); lists whose items are all objects with
//! a unique `name` or `id` string are compared item by item (`key.list[media].field`); any
//! other value is compared as a whole.

use rynas_api_types::{DiffEntry, DiffOp};
use serde_json::Value;

/// Differences between `old` and `new` under `path` (normally the configuration key).
pub fn diff(path: &str, old: Option<&Value>, new: Option<&Value>) -> Vec<DiffEntry> {
    let mut out = Vec::new();
    walk(path, old, new, &mut out);
    out
}

fn entry(path: &str, old: Option<&Value>, new: Option<&Value>) -> DiffEntry {
    let op = match (old, new) {
        (None, _) => DiffOp::Added,
        (_, None) => DiffOp::Removed,
        _ => DiffOp::Changed,
    };
    DiffEntry {
        path: path.to_string(),
        op,
        old: old.cloned(),
        new: new.cloned(),
    }
}

fn walk(path: &str, old: Option<&Value>, new: Option<&Value>, out: &mut Vec<DiffEntry>) {
    match (old, new) {
        (None, None) => {}
        (Some(a), Some(b)) if a == b => {}
        (Some(Value::Object(a)), Some(Value::Object(b))) => {
            for (k, va) in a {
                walk(&format!("{path}.{k}"), Some(va), b.get(k), out);
            }
            for (k, vb) in b {
                if !a.contains_key(k) {
                    walk(&format!("{path}.{k}"), None, Some(vb), out);
                }
            }
        }
        (Some(Value::Array(a)), Some(Value::Array(b))) => match (keyed(a), keyed(b)) {
            (Some(ka), Some(kb)) => {
                for (name, va) in &ka {
                    let vb = kb.iter().find(|(n, _)| n == name).map(|(_, v)| *v);
                    walk(&format!("{path}[{name}]"), Some(va), vb, out);
                }
                for (name, vb) in &kb {
                    if !ka.iter().any(|(n, _)| n == name) {
                        walk(&format!("{path}[{name}]"), None, Some(vb), out);
                    }
                }
            }
            _ => out.push(entry(path, old, new)),
        },
        _ => out.push(entry(path, old, new)),
    }
}

/// `(name, item)` pairs when every item is an object with a unique `name` or `id` string.
fn keyed(items: &[Value]) -> Option<Vec<(String, &Value)>> {
    let mut out: Vec<(String, &Value)> = Vec::with_capacity(items.len());
    for it in items {
        let o = it.as_object()?;
        let name = o
            .get("name")
            .or_else(|| o.get("id"))
            .and_then(Value::as_str)?;
        if out.iter().any(|(n, _)| n == name) {
            return None;
        }
        out.push((name.to_string(), it));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(d: &[DiffEntry]) -> Vec<(String, DiffOp)> {
        d.iter().map(|e| (e.path.clone(), e.op)).collect()
    }

    #[test]
    fn objects_field_by_field() {
        let a = json!({"hostname": "nas", "keep": 1, "gone": true});
        let b = json!({"hostname": "nas2", "keep": 1, "new": [1, 2]});
        let d = diff("system.hostname", Some(&a), Some(&b));
        assert_eq!(
            paths(&d),
            [
                ("system.hostname.gone".into(), DiffOp::Removed),
                ("system.hostname.hostname".into(), DiffOp::Changed),
                ("system.hostname.new".into(), DiffOp::Added),
            ]
        );
        assert_eq!(d[1].old, Some(json!("nas")));
        assert_eq!(d[1].new, Some(json!("nas2")));
        assert!(diff("k", Some(&a), Some(&a)).is_empty());
    }

    #[test]
    fn named_lists_item_by_item() {
        let a = json!({"shares": [{"name": "media", "read_only": false}, {"name": "old"}]});
        let b = json!({"shares": [{"name": "media", "read_only": true}, {"name": "docs"}]});
        let d = diff("smb", Some(&a), Some(&b));
        assert_eq!(
            paths(&d),
            [
                ("smb.shares[media].read_only".into(), DiffOp::Changed),
                ("smb.shares[old]".into(), DiffOp::Removed),
                ("smb.shares[docs]".into(), DiffOp::Added),
            ]
        );
        // plain lists are compared whole
        let d = diff("k", Some(&json!([1, 2])), Some(&json!([2, 1])));
        assert_eq!(paths(&d), [("k".into(), DiffOp::Changed)]);
    }

    #[test]
    fn whole_key_added_or_removed() {
        let v = json!({"a": 1});
        assert_eq!(
            paths(&diff("k", None, Some(&v))),
            [("k".into(), DiffOp::Added)]
        );
        assert_eq!(
            paths(&diff("k", Some(&v), None)),
            [("k".into(), DiffOp::Removed)]
        );
    }
}
