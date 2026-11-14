//! Preferences of the web interface kept on the NAS for each user (`/api/v1/prefs/…`), so
//! they follow the account across browsers and devices. Today: the sidebar layout.
//!
//! The sidebar is a list of groups (the visual blocks between separators), each a list of
//! entry ids: the built-in entries of [`SIDEBAR_DEFAULT`] and the user's pinned pages
//! ([`SidebarPin`]). The server stores a layout only after [`SidebarLayout::validate`] and
//! keeps nothing for the default one; on load the interface merges what it gets with the
//! default of its own release ([`merge`]): entries added by a later release land in their
//! default place instead of being hidden by an older saved layout, entries a release
//! removed are dropped.

use serde::{Deserialize, Serialize};

/// Version of [`SidebarLayout`] this release writes and reads.
pub const SIDEBAR_VERSION: u32 = 1;
/// Largest stored layout, in bytes of JSON (`PUT` answers 413 above it).
pub const SIDEBAR_MAX_BYTES: usize = 16 * 1024;
/// Most pinned pages.
pub const MAX_PINS: usize = 16;
/// Most groups.
pub const MAX_GROUPS: usize = 16;
/// Longest name of a pinned page, in characters.
pub const PIN_LABEL_MAX: usize = 48;
/// Longest path of a pinned page, in bytes.
pub const PIN_PATH_MAX: usize = 256;

/// The built-in entries of the sidebar, by group, in their default order. A new entry is
/// added here (and to the interface's navigation table) by the release that brings it.
pub const SIDEBAR_DEFAULT: &[&[&str]] = &[
    &["dashboard"],
    &["storage", "files", "shares", "snapshots", "backup"],
    &["users", "services", "docker", "apps"],
    &["network", "hardware", "system"],
];

/// The icons a pinned page may use (names of the interface's icon set).
pub const PIN_ICONS: &[&str] = &[
    "pin",
    "hard-drive",
    "database",
    "layers",
    "folder",
    "folder-open",
    "share",
    "camera",
    "archive-restore",
    "users",
    "user",
    "server",
    "container",
    "box",
    "puzzle",
    "network",
    "link",
    "cpu",
    "memory-stick",
    "activity",
    "settings",
    "calendar-clock",
    "clock",
    "history",
    "download",
    "bell",
    "shield-check",
    "key",
    "lock",
    "usb",
    "disc",
    "file-text",
    "life-buoy",
    "power",
];

/// A page of the interface pinned as an entry of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarPin {
    /// `p` followed by 1 to 8 lowercase letters or digits; unique in the layout.
    pub id: String,
    /// The page: a path of the interface (see [`valid_pin_path`]).
    pub path: String,
    /// The name shown, 1 to [`PIN_LABEL_MAX`] characters.
    pub label: String,
    /// One of [`PIN_ICONS`].
    pub icon: String,
}

/// The saved sidebar of a user (`PUT /api/v1/prefs/sidebar`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarLayout {
    /// [`SIDEBAR_VERSION`].
    pub version: u32,
    /// Entry ids by group, in order: every built-in entry once, and every pin once.
    pub groups: Vec<Vec<String>>,
    /// Entries the user hid (they stay in `groups`, so they come back in place).
    #[serde(default)]
    pub hidden: Vec<String>,
    /// The built-in entries the interface knew when it saved: an entry missing from this list
    /// is newer than the layout and goes to its default place.
    #[serde(default)]
    pub known: Vec<String>,
    #[serde(default)]
    pub pins: Vec<SidebarPin>,
}

/// `GET /api/v1/prefs/sidebar`: the saved layout, `None` when the user keeps the default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarPrefs {
    pub layout: Option<SidebarLayout>,
}

/// Why a layout is refused: the field and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutError {
    pub field: &'static str,
    pub message: String,
}

fn err(field: &'static str, message: impl Into<String>) -> LayoutError {
    LayoutError { field, message: message.into() }
}

/// Whether `id` is a built-in entry.
pub fn is_builtin(id: &str) -> bool {
    SIDEBAR_DEFAULT.iter().any(|g| g.contains(&id))
}

fn builtin_ids() -> impl Iterator<Item = &'static str> {
    SIDEBAR_DEFAULT.iter().flat_map(|g| g.iter().copied())
}

/// Whether `id` has the shape of a pin id.
pub fn valid_pin_id(id: &str) -> bool {
    let rest = id.strip_prefix('p').unwrap_or("");
    (1..=8).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Whether `path` may be pinned: a path of the interface itself, `/` then URL characters
/// (a query is allowed, a fragment is not), never another site (`//host`, a scheme), the API,
/// a backslash, a `.` or `..` segment, or an encoded dot (`%2e`, which could decode to one).
pub fn valid_pin_path(path: &str) -> bool {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b"-._~/%?=&+:@!$'()*,;".contains(&b);
    let route = path.split('?').next().unwrap_or("");
    path.len() <= PIN_PATH_MAX
        && path.starts_with('/')
        && !path.starts_with("//")
        && path.bytes().all(allowed)
        && route != "/api"
        && !route.starts_with("/api/")
        && !route.split('/').any(|s| s == "." || s == "..")
        // an encoded dot could decode to a `.`/`..` segment: never in a pinned address
        && !path.to_ascii_lowercase().contains("%2e")
}

fn valid_label(label: &str) -> bool {
    let n = label.chars().count();
    !label.trim().is_empty() && n <= PIN_LABEL_MAX && !label.chars().any(char::is_control)
}

impl SidebarLayout {
    /// The layout of this release, with nothing hidden or pinned.
    pub fn default_layout() -> Self {
        SidebarLayout {
            version: SIDEBAR_VERSION,
            groups: SIDEBAR_DEFAULT.iter().map(|g| g.iter().map(|s| s.to_string()).collect()).collect(),
            hidden: Vec::new(),
            known: builtin_ids().map(str::to_string).collect(),
            pins: Vec::new(),
        }
    }

    /// Same entries in the same places as the default: nothing to store.
    pub fn is_default(&self) -> bool {
        let d = Self::default_layout();
        self.groups == d.groups && self.hidden.is_empty() && self.pins.is_empty()
    }

    pub fn pin(&self, id: &str) -> Option<&SidebarPin> {
        self.pins.iter().find(|p| p.id == id)
    }

    /// The entry ids in order, all groups together.
    pub fn order(&self) -> impl Iterator<Item = &str> {
        self.groups.iter().flatten().map(String::as_str)
    }

    /// What the server accepts: this version; every built-in entry of this release exactly
    /// once and nothing unknown; every pin valid and placed exactly once; no empty group;
    /// `hidden` naming placed entries only; `known` naming built-in entries only.
    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.version != SIDEBAR_VERSION {
            return Err(err("version", format!("unsupported version {} (expected {SIDEBAR_VERSION})", self.version)));
        }
        if self.groups.is_empty() || self.groups.len() > MAX_GROUPS {
            return Err(err("groups", format!("1 to {MAX_GROUPS} groups")));
        }
        if self.pins.len() > MAX_PINS {
            return Err(err("pins", format!("at most {MAX_PINS} pinned pages")));
        }
        for (i, p) in self.pins.iter().enumerate() {
            if !valid_pin_id(&p.id) || self.pins[..i].iter().any(|q| q.id == p.id) {
                return Err(err("pins", format!("invalid or repeated pin id {:?}", p.id)));
            }
            if !valid_pin_path(&p.path) {
                return Err(err("pins", format!("pin {}: not a page of the interface", p.id)));
            }
            if !valid_label(&p.label) {
                return Err(err("pins", format!("pin {}: name of 1 to {PIN_LABEL_MAX} characters", p.id)));
            }
            if !PIN_ICONS.contains(&p.icon.as_str()) {
                return Err(err("pins", format!("pin {}: unknown icon {:?}", p.id, p.icon)));
            }
        }
        let mut seen: Vec<&str> = Vec::new();
        for g in &self.groups {
            if g.is_empty() {
                return Err(err("groups", "empty group"));
            }
            for id in g {
                if !is_builtin(id) && self.pin(id).is_none() {
                    return Err(err("groups", format!("unknown entry {id:?}")));
                }
                if seen.contains(&id.as_str()) {
                    return Err(err("groups", format!("entry {id:?} twice")));
                }
                seen.push(id);
            }
        }
        if let Some(id) = builtin_ids().find(|id| !seen.contains(id)) {
            return Err(err("groups", format!("entry {id:?} missing")));
        }
        if let Some(p) = self.pins.iter().find(|p| !seen.contains(&p.id.as_str())) {
            return Err(err("groups", format!("pin {} not placed", p.id)));
        }
        if let Some(h) = self.hidden.iter().find(|h| !seen.contains(&h.as_str())) {
            return Err(err("hidden", format!("unknown entry {h:?}")));
        }
        if let Some((_, h)) = self.hidden.iter().enumerate().find(|(i, h)| self.hidden[..*i].contains(h)) {
            return Err(err("hidden", format!("entry {h:?} twice")));
        }
        if let Some(k) = self.known.iter().find(|k| !is_builtin(k)) {
            return Err(err("known", format!("unknown entry {k:?}")));
        }
        Ok(())
    }
}

/// The layout to show: `saved` (if any) brought to this release. Unknown entries, invalid
/// pins, repeats and empty groups are dropped; a built-in entry missing from it goes to its
/// default place: after the nearest entry that precedes it in the default order of its group,
/// else before the nearest that follows it, else as a group of its own at its default index.
/// It is shown even when hidden by the saved list only if it is new (not in `known`).
/// The result always passes [`SidebarLayout::validate`]. A saved layout of another version is
/// ignored.
pub fn merge(saved: Option<&SidebarLayout>) -> SidebarLayout {
    let Some(s) = saved.filter(|s| s.version == SIDEBAR_VERSION) else {
        return SidebarLayout::default_layout();
    };
    let mut pins: Vec<SidebarPin> = Vec::new();
    for p in &s.pins {
        let ok = pins.len() < MAX_PINS
            && valid_pin_id(&p.id)
            && valid_pin_path(&p.path)
            && valid_label(&p.label)
            && PIN_ICONS.contains(&p.icon.as_str())
            && !pins.iter().any(|q| q.id == p.id);
        if ok {
            pins.push(p.clone());
        }
    }
    let mut seen: Vec<String> = Vec::new();
    let mut groups: Vec<Vec<String>> = Vec::new();
    for g in s.groups.iter().take(MAX_GROUPS) {
        let mut out = Vec::new();
        for id in g {
            let known_id = is_builtin(id) || pins.iter().any(|p| p.id == *id);
            if known_id && !seen.contains(id) {
                seen.push(id.clone());
                out.push(id.clone());
            }
        }
        if !out.is_empty() {
            groups.push(out);
        }
    }
    // pins that were never placed go at the end of the last group
    for p in &pins {
        if !seen.contains(&p.id) {
            seen.push(p.id.clone());
            match groups.last_mut() {
                Some(g) => g.push(p.id.clone()),
                None => groups.push(vec![p.id.clone()]),
            }
        }
    }
    let mut new_ids: Vec<&str> = Vec::new();
    for (gi, dg) in SIDEBAR_DEFAULT.iter().enumerate() {
        for (pos, id) in dg.iter().enumerate() {
            if seen.iter().any(|s| s == id) {
                continue;
            }
            if !s.known.iter().any(|k| k == id) {
                new_ids.push(id);
            }
            place_default(&mut groups, gi, pos, id);
            seen.push(id.to_string());
        }
    }
    let hidden = s
        .hidden
        .iter()
        .filter(|h| seen.contains(h) && !new_ids.contains(&h.as_str()))
        .fold(Vec::<String>::new(), |mut v, h| {
            if !v.contains(h) {
                v.push(h.clone());
            }
            v
        });
    SidebarLayout {
        version: SIDEBAR_VERSION,
        groups,
        hidden,
        known: builtin_ids().map(str::to_string).collect(),
        pins,
    }
}

/// Puts the built-in entry at `SIDEBAR_DEFAULT[gi][pos]` in its default place (see [`merge`]).
fn place_default(groups: &mut Vec<Vec<String>>, gi: usize, pos: usize, id: &str) {
    let dg = SIDEBAR_DEFAULT[gi];
    let find = |groups: &Vec<Vec<String>>, x: &str| {
        groups.iter().enumerate().find_map(|(g, v)| v.iter().position(|e| e == x).map(|i| (g, i)))
    };
    for prev in dg[..pos].iter().rev() {
        if let Some((g, i)) = find(groups, prev) {
            groups[g].insert(i + 1, id.to_string());
            return;
        }
    }
    for next in &dg[pos + 1..] {
        if let Some((g, i)) = find(groups, next) {
            groups[g].insert(i, id.to_string());
            return;
        }
    }
    let at = gi.min(groups.len());
    if groups.len() < MAX_GROUPS {
        groups.insert(at, vec![id.to_string()]);
    } else if let Some(g) = groups.get_mut(at.min(MAX_GROUPS - 1)) {
        g.push(id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(groups: &[&[&str]]) -> Vec<Vec<String>> {
        groups.iter().map(|g| g.iter().map(|s| s.to_string()).collect()).collect()
    }

    fn pin(id: &str, path: &str) -> SidebarPin {
        SidebarPin { id: id.into(), path: path.into(), label: "Task Scheduler".into(), icon: "calendar-clock".into() }
    }

    #[test]
    fn default_is_valid_and_default() {
        let d = SidebarLayout::default_layout();
        assert_eq!(d.validate(), Ok(()));
        assert!(d.is_default());
        assert_eq!(merge(None), d);
        assert_eq!(merge(Some(&d)), d);
    }

    #[test]
    fn pin_paths() {
        for ok in ["/", "/system/scheduler", "/storage/volumes/data", "/docker?stack=web", "/files/share%20a"] {
            assert!(valid_pin_path(ok), "{ok}");
        }
        for bad in [
            "",
            "system",
            "//evil.example",
            "https://evil.example",
            "/api/v1/system",
            "/api",
            "/a/../api",
            "/a/./b",
            "/a/..",
            "/a/%2e%2e/api",
            "/a/%2E%2E/api",
            "/a/.%2E/api",
            "/%2e/x",
            "/a\\b",
            "/a#x",
            "/a b",
            "/<script>",
            "javascript:alert(1)",
        ] {
            assert!(!valid_pin_path(bad), "{bad}");
        }
        assert!(!valid_pin_path(&format!("/{}", "a".repeat(PIN_PATH_MAX))));
    }

    #[test]
    fn validation() {
        let mut l = SidebarLayout::default_layout();
        l.groups[1].swap(0, 1);
        l.hidden.push("snapshots".into());
        l.pins.push(pin("p1", "/system/scheduler"));
        l.groups[3].push("p1".into());
        assert_eq!(l.validate(), Ok(()));
        assert!(!l.is_default());

        let bad = |f: &dyn Fn(&mut SidebarLayout), field: &str| {
            let mut x = l.clone();
            f(&mut x);
            assert_eq!(x.validate().map_err(|e| e.field), Err(field), "{x:?}");
        };
        bad(&|x| x.version = 2, "version");
        bad(&|x| x.groups[0].push("bogus".into()), "groups");
        bad(&|x| x.groups[0].push("storage".into()), "groups");
        bad(&|x| x.groups[1].retain(|e| e != "files"), "groups");
        bad(&|x| x.groups.push(Vec::new()), "groups");
        bad(&|x| x.groups[3].retain(|e| e != "p1"), "groups");
        bad(&|x| x.hidden.push("bogus".into()), "hidden");
        bad(&|x| x.hidden.push("snapshots".into()), "hidden");
        bad(&|x| x.known.push("p1".into()), "known");
        bad(&|x| x.pins[0].path = "https://evil.example".into(), "pins");
        bad(&|x| x.pins[0].path = "/api/v1/users".into(), "pins");
        bad(&|x| x.pins[0].icon = "skull".into(), "pins");
        bad(&|x| x.pins[0].label = " ".into(), "pins");
        bad(&|x| x.pins[0].label = "x".repeat(PIN_LABEL_MAX + 1), "pins");
        bad(&|x| x.pins[0].label = "a\nb".into(), "pins");
        bad(&|x| x.pins[0].id = "P1".into(), "pins");
        bad(
            &|x| {
                x.pins.push(pin("p1", "/"));
            },
            "pins",
        );
        bad(
            &|x| {
                for i in 0..=MAX_PINS {
                    x.pins.push(pin(&format!("q{i}"), "/"));
                }
            },
            "pins",
        );
        // the label limit counts characters, not bytes
        let mut x = l.clone();
        x.pins[0].label = "è".repeat(PIN_LABEL_MAX);
        assert_eq!(x.validate(), Ok(()));
    }

    #[test]
    fn merge_places_new_entries_by_default() {
        // a layout saved by an older release: no "snapshots" nor "apps", reordered, and
        // "files" hidden
        let saved = SidebarLayout {
            version: 1,
            groups: ids(&[&["network", "hardware", "system"], &["dashboard"], &["backup", "storage", "files", "shares"], &["users", "services", "docker"]]),
            hidden: vec!["files".into()],
            known: ["dashboard", "storage", "files", "shares", "backup", "users", "services", "docker", "network", "hardware", "system"]
                .map(String::from)
                .to_vec(),
            pins: vec![],
        };
        let m = merge(Some(&saved));
        assert_eq!(m.validate(), Ok(()));
        // after the entry that precedes it by default (shares), after docker
        assert_eq!(
            m.groups,
            ids(&[
                &["network", "hardware", "system"],
                &["dashboard"],
                &["backup", "storage", "files", "shares", "snapshots"],
                &["users", "services", "docker", "apps"],
            ])
        );
        assert_eq!(m.hidden, ["files"]);
    }

    #[test]
    fn merge_new_entry_first_of_group_and_new_group() {
        // "storage" first of its group, missing: before "files"; a whole missing group comes
        // back at its default index
        let saved = SidebarLayout {
            version: 1,
            groups: ids(&[&["dashboard"], &["files", "shares", "snapshots", "backup"], &["network", "hardware", "system"]]),
            hidden: vec!["storage".into(), "users".into()],
            known: vec![],
            pins: vec![],
        };
        let m = merge(Some(&saved));
        assert_eq!(m.validate(), Ok(()));
        assert_eq!(m.groups[1][0], "storage");
        assert_eq!(m.groups[2], ["users", "services", "docker", "apps"]);
        // new entries are shown even if a hidden list names them
        assert!(m.hidden.is_empty());
    }

    #[test]
    fn merge_drops_unknown_and_invalid() {
        let mut saved = SidebarLayout::default_layout();
        saved.groups[0].push("from-a-newer-release".into());
        saved.groups[1].push("storage".into());
        saved.groups.push(vec!["p9".into()]);
        saved.groups.push(vec![]);
        saved.pins = vec![pin("p1", "/system/scheduler"), pin("p9", "//evil.example"), pin("p1", "/again")];
        saved.hidden = vec!["bogus".into(), "docker".into(), "docker".into()];
        let m = merge(Some(&saved));
        assert_eq!(m.validate(), Ok(()));
        assert_eq!(m.pins, [pin("p1", "/system/scheduler")]);
        // the unplaced pin goes at the end
        assert_eq!(m.groups.last().unwrap().last().unwrap(), "p1");
        assert_eq!(m.hidden, ["docker"]);
        assert_eq!(m.groups[0], ["dashboard"]);
        // another version: the default
        saved.version = 7;
        assert_eq!(merge(Some(&saved)), SidebarLayout::default_layout());
    }

    #[test]
    fn json_shape() {
        let l: SidebarLayout = serde_json::from_str(r#"{"version":1,"groups":[["dashboard"]]}"#).unwrap();
        assert!(l.hidden.is_empty() && l.pins.is_empty() && l.known.is_empty());
        let p = SidebarPrefs { layout: None };
        assert_eq!(serde_json::to_string(&p).unwrap(), r#"{"layout":null}"#);
    }
}
