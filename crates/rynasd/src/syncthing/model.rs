//! The in-memory state of one Syncthing instance (`docs/phase2/syncthing.md` §4), built from
//! REST reads and the events of one long-poll subscription, and the alerts it raises (§5).
//! Pure: no I/O, times passed in.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde_json::Value;

/// The part of a conflict copy's name (`<base>.sync-conflict-YYYYMMDD-HHMMSS-<id>.<ext>`).
pub const CONFLICT_MARK: &str = ".sync-conflict-";
/// Conflict copies remembered per folder.
pub const MAX_CONFLICT_FILES: usize = 50;
/// Item errors kept per folder.
pub const MAX_ERRORS: usize = 20;
/// The event types of the one subscription. Fixed: Syncthing keeps one buffer per mask.
pub const EVENT_MASK: &str = "FolderSummary,StateChanged,FolderErrors,DeviceConnected,DeviceDisconnected,DevicePaused,DeviceResumed,FolderPaused,FolderResumed,ItemFinished,ConfigSaved,FolderCompletion";
/// A folder in `error` this long is notified.
pub const ERROR_AFTER_MS: u64 = 5 * 60 * 1000;
/// New conflict copies of a folder are grouped this long into one notification.
pub const CONFLICT_GROUP_MS: u64 = 10 * 60 * 1000;
/// The REST interface silent this long while the app runs is notified.
pub const DOWN_AFTER_MS: u64 = 10 * 60 * 1000;
/// File names listed in a conflict notification.
pub const CONFLICT_FILES_IN_ALERT: usize = 5;
const DAY_MS: u64 = 24 * 3600 * 1000;

/// The first 7 characters of a device id.
pub fn short(id: &str) -> String {
    id.chars().take(7).collect()
}

/// An RFC 3339 time of Syncthing → ms since the epoch (`None` for its zero time).
pub fn time_ms(s: &str) -> Option<u64> {
    let ms = s.parse::<jiff::Timestamp>().ok()?.as_millisecond();
    u64::try_from(ms).ok().filter(|m| *m > 0)
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f.max(0.0) as u64))).unwrap_or(0)
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// A folder as rynas knows it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Folder {
    pub id: String,
    pub label: String,
    pub path: String,
    pub folder_type: String,
    pub paused: bool,
    /// Full ids of the devices it is shared with (this NAS included, as in the config).
    pub devices: Vec<String>,
    pub marker_name: String,
    pub state: String,
    pub error: Option<String>,
    pub errors: Vec<(String, String)>,
    pub error_count: u32,
    pub need_bytes: u64,
    pub global_bytes: u64,
    pub in_sync_bytes: u64,
    /// device id → (completion %, bytes needed).
    pub completion: BTreeMap<String, (f64, u64)>,
    pub last_scan_ms: Option<u64>,
    pub last_file: Option<String>,
    /// Syncthing's conflict counter at the last full read.
    pub conflicts_base: u32,
    /// Conflict copies seen in events since then.
    pub conflicts_seen: u32,
    /// Latest conflict copies, newest first.
    pub conflict_files: VecDeque<String>,
}

impl Folder {
    pub fn conflicts(&self) -> u32 {
        self.conflicts_base.saturating_add(self.conflicts_seen)
    }

    /// Remembers a conflict copy (newest first, no duplicates); `false` when known already.
    pub fn add_conflict(&mut self, path: &str) -> bool {
        if self.conflict_files.iter().any(|p| p == path) {
            return false;
        }
        self.conflict_files.push_front(path.to_string());
        self.conflict_files.truncate(MAX_CONFLICT_FILES);
        self.conflicts_seen = self.conflicts_seen.saturating_add(1);
        true
    }

    /// `/rest/db/status` or the `summary` of a `FolderSummary` event.
    pub fn set_status(&mut self, v: &Value) {
        let st = s(v, "state");
        if !st.is_empty() {
            self.state = st;
        }
        self.need_bytes = u(v, "needBytes");
        self.global_bytes = u(v, "globalBytes");
        self.in_sync_bytes = u(v, "inSyncBytes");
        let err = Some(s(v, "error")).filter(|e| !e.is_empty()).or_else(|| Some(s(v, "invalid")).filter(|e| !e.is_empty()));
        if self.state == "error" {
            if err.is_some() {
                self.error = err;
            }
        } else {
            self.error = None;
        }
    }

    /// The item errors (`/rest/folder/errors` or a `FolderErrors` event).
    pub fn set_errors(&mut self, list: &Value) {
        let all: Vec<(String, String)> = list
            .as_array()
            .map(|a| a.iter().map(|e| (s(e, "path"), s(e, "error"))).collect())
            .unwrap_or_default();
        self.error_count = u32::try_from(all.len()).unwrap_or(u32::MAX);
        self.errors = all.into_iter().take(MAX_ERRORS).collect();
    }
}

/// A remote device (or this NAS) as rynas knows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub paused: bool,
    pub connected: bool,
    pub address: Option<String>,
    pub connection_type: Option<String>,
    pub last_seen_ms: Option<u64>,
}

/// The folders of `/rest/config/folders`.
pub fn folders_of_config(v: &Value) -> Vec<Folder> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|f| {
                    let paused = b(f, "paused");
                    Folder {
                        id: s(f, "id"),
                        label: Some(s(f, "label")).filter(|l| !l.is_empty()).unwrap_or_else(|| s(f, "id")),
                        path: s(f, "path"),
                        folder_type: s(f, "type"),
                        paused,
                        devices: f
                            .get("devices")
                            .and_then(Value::as_array)
                            .map(|d| d.iter().map(|x| s(x, "deviceID")).filter(|x| !x.is_empty()).collect())
                            .unwrap_or_default(),
                        marker_name: Some(s(f, "markerName")).filter(|m| !m.is_empty()).unwrap_or_else(|| super::config::DEFAULT_MARKER.into()),
                        state: if paused { "paused".into() } else { "unknown".into() },
                        ..Default::default()
                    }
                })
                .filter(|f| !f.id.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The devices of `/rest/config/devices`.
pub fn devices_of_config(v: &Value) -> Vec<Device> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|d| Device { id: s(d, "deviceID"), name: s(d, "name"), paused: b(d, "paused"), ..Default::default() })
                .filter(|d| !d.id.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Syncthing's conflict counters by folder, from its Prometheus `/metrics`.
pub fn conflict_counters(metrics: &str) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    for l in metrics.lines() {
        let Some(rest) = l.strip_prefix("syncthing_model_folder_conflicts_total{") else { continue };
        let Some((labels, value)) = rest.split_once('}') else { continue };
        let Some(folder) = labels.split(',').find_map(|kv| kv.trim().strip_prefix("folder=\"")?.strip_suffix('"').map(str::to_string)) else {
            continue;
        };
        if let Ok(n) = value.split_whitespace().next().unwrap_or("").parse::<f64>() {
            out.insert(folder, n.max(0.0).min(u32::MAX as f64) as u32);
        }
    }
    out
}

/// What an event changed beyond the model.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// The configuration changed: read it again.
    pub full_read: bool,
    /// New conflict copies: (folder id, path inside the folder).
    pub conflicts: Vec<(String, String)>,
}

/// Whether a batch of events follows `last` without a hole: `(read again, new last id)`.
/// A first id that is not `last + 1` is a gap (the buffer overflowed) or, smaller, a
/// restarted Syncthing; both mean a full read. `last == 0`: the position is not known yet.
pub fn check_ids(last: u64, events: &[Value]) -> (bool, u64) {
    let ids: Vec<u64> = events.iter().map(|e| u(e, "id")).collect();
    let Some(first) = ids.first().copied() else { return (false, last) };
    let max = ids.iter().copied().max().unwrap_or(last);
    let resync = last > 0 && first != last + 1;
    (resync, if resync { max } else { max.max(last) })
}

/// The state of one instance.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    pub my_id: String,
    pub version: String,
    pub folders: Vec<Folder>,
    pub devices: Vec<Device>,
    /// The last full read (ms).
    pub read_ms: Option<u64>,
}

impl Model {
    pub fn folder_mut(&mut self, id: &str) -> Option<&mut Folder> {
        self.folders.iter_mut().find(|f| f.id == id)
    }

    pub fn folder(&self, id: &str) -> Option<&Folder> {
        self.folders.iter().find(|f| f.id == id)
    }

    pub fn device_mut(&mut self, id: &str) -> Option<&mut Device> {
        self.devices.iter_mut().find(|d| d.id == id)
    }

    /// The name of device `id` (its short id when it has none).
    pub fn device_name(&self, id: &str) -> String {
        self.devices.iter().find(|d| d.id == id).map(|d| d.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| short(id))
    }

    /// A new configuration: folders and devices replaced, what events and reads gathered
    /// about the ones that stay is kept.
    pub fn replace_config(&mut self, folders: Vec<Folder>, devices: Vec<Device>) {
        let old_f = std::mem::take(&mut self.folders);
        self.folders = folders
            .into_iter()
            .map(|mut f| {
                if let Some(o) = old_f.iter().find(|o| o.id == f.id) {
                    let (paused, path) = (f.paused, f.path.clone());
                    let cfg = f;
                    f = Folder {
                        id: cfg.id,
                        label: cfg.label,
                        path: cfg.path,
                        folder_type: cfg.folder_type,
                        paused,
                        devices: cfg.devices,
                        marker_name: cfg.marker_name,
                        ..o.clone()
                    };
                    if paused {
                        f.state = "paused".into();
                    } else if o.paused {
                        f.state = "unknown".into();
                    }
                    if o.path != path {
                        f.conflict_files.clear();
                    }
                }
                f
            })
            .collect();
        let old_d = std::mem::take(&mut self.devices);
        self.devices = devices
            .into_iter()
            .map(|d| match old_d.iter().find(|o| o.id == d.id) {
                Some(o) => Device { id: d.id, name: d.name, paused: d.paused, ..o.clone() },
                None => d,
            })
            .collect();
    }

    /// `/rest/system/connections`.
    pub fn set_connections(&mut self, v: &Value) {
        let Some(m) = v.get("connections").and_then(Value::as_object) else { return };
        for d in &mut self.devices {
            match m.get(&d.id) {
                Some(c) => {
                    d.connected = b(c, "connected");
                    d.address = Some(s(c, "address")).filter(|a| !a.is_empty() && d.connected);
                    d.connection_type = Some(s(c, "type")).filter(|a| !a.is_empty() && d.connected);
                }
                None => {
                    d.connected = false;
                    d.address = None;
                    d.connection_type = None;
                }
            }
        }
    }

    /// `/rest/stats/device`.
    pub fn set_device_stats(&mut self, v: &Value) {
        for d in &mut self.devices {
            if let Some(ms) = v.get(&d.id).map(|x| s(x, "lastSeen")).and_then(|t| time_ms(&t)) {
                d.last_seen_ms = Some(ms.max(d.last_seen_ms.unwrap_or(0)));
            }
        }
    }

    /// `/rest/stats/folder`.
    pub fn set_folder_stats(&mut self, v: &Value) {
        for f in &mut self.folders {
            let Some(st) = v.get(&f.id) else { continue };
            f.last_scan_ms = time_ms(&s(st, "lastScan"));
            f.last_file = st.get("lastFile").map(|lf| s(lf, "filename")).filter(|n| !n.is_empty());
        }
    }

    /// One event of the subscription; `now` stands in for an event without a time.
    pub fn apply(&mut self, ev: &Value, now: u64) -> Applied {
        let mut out = Applied::default();
        let data = ev.get("data").cloned().unwrap_or(Value::Null);
        let at = time_ms(&s(ev, "time")).unwrap_or(now);
        match s(ev, "type").as_str() {
            "StateChanged" => {
                if let Some(f) = self.folder_mut(&s(&data, "folder")) {
                    f.state = s(&data, "to");
                    if f.state == "error" {
                        f.error = Some(s(&data, "error")).filter(|e| !e.is_empty()).or(f.error.take());
                    } else {
                        f.error = None;
                    }
                }
            }
            "FolderSummary" => {
                if let (Some(f), Some(sum)) = (self.folders.iter_mut().find(|f| f.id == s(&data, "folder")), data.get("summary")) {
                    f.set_status(sum);
                }
            }
            "FolderErrors" => {
                if let Some(f) = self.folder_mut(&s(&data, "folder")) {
                    f.set_errors(data.get("errors").unwrap_or(&Value::Null));
                }
            }
            "FolderCompletion" => {
                let dev = s(&data, "device");
                if let Some(f) = self.folder_mut(&s(&data, "folder")) {
                    let pct = data.get("completion").and_then(Value::as_f64).unwrap_or(0.0);
                    f.completion.insert(dev, (pct, u(&data, "needBytes")));
                }
            }
            "DeviceConnected" => {
                if let Some(d) = self.device_mut(&s(&data, "id")) {
                    d.connected = true;
                    d.address = Some(s(&data, "addr")).filter(|a| !a.is_empty());
                    d.connection_type = Some(s(&data, "type")).filter(|a| !a.is_empty());
                    d.last_seen_ms = Some(at);
                }
            }
            "DeviceDisconnected" => {
                if let Some(d) = self.device_mut(&s(&data, "id")) {
                    d.connected = false;
                    d.address = None;
                    d.connection_type = None;
                    d.last_seen_ms = Some(at);
                }
            }
            t @ ("DevicePaused" | "DeviceResumed") => {
                if let Some(d) = self.device_mut(&s(&data, "device")) {
                    d.paused = t == "DevicePaused";
                }
            }
            t @ ("FolderPaused" | "FolderResumed") => {
                if let Some(f) = self.folder_mut(&s(&data, "id")) {
                    f.paused = t == "FolderPaused";
                    f.state = if f.paused { "paused".into() } else { "unknown".into() };
                    if f.paused {
                        f.error = None;
                    }
                }
            }
            "ItemFinished" => {
                let item = s(&data, "item");
                let ok = data.get("error").is_none_or(Value::is_null) && s(&data, "action") != "delete";
                if ok
                    && is_conflict_copy(&item)
                    && let Some(f) = self.folder_mut(&s(&data, "folder"))
                    && f.add_conflict(&item)
                {
                    out.conflicts.push((f.id.clone(), item));
                }
            }
            "ConfigSaved" => out.full_read = true,
            _ => {}
        }
        out
    }
}

/// A file name made by Syncthing for a losing version.
pub fn is_conflict_copy(path: &str) -> bool {
    path.rsplit('/').next().is_some_and(|n| n.contains(CONFLICT_MARK))
}

// ---------------------------------------------------------------------------------------------
// alerts
// ---------------------------------------------------------------------------------------------

/// A notification to send (`syncthing.*` codes, `../../../docs/phase1/plan.md` "Event codes").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    Conflict { folder: String, label: String, count: u32, files: Vec<String> },
    FolderError { folder: String, label: String, error: String },
    FolderRecovered { folder: String, label: String },
    DeviceUnseen { device: String, name: String, days: u64 },
    DeviceSeen { device: String, name: String },
    Down { minutes: u64 },
    /// `quiet`: the app was stopped, nothing to tell.
    Up { quiet: bool },
}

/// What the alerts of one instance remember (memory only: the engine keeps firing states
/// across restarts and counts repeats instead of sending them again).
#[derive(Debug, Clone, Default)]
pub struct Alerts {
    error_since: BTreeMap<String, u64>,
    error_notified: BTreeMap<String, String>,
    /// folder id → (first new copy ms, count, first file names).
    pending: BTreeMap<String, (u64, u32, Vec<String>)>,
    unseen_notified: BTreeSet<String>,
    down_since: Option<u64>,
    down_notified: bool,
}

/// How the instance looks to the alerts.
pub enum Seen<'a> {
    /// Its REST interface answers.
    Reachable(&'a Model),
    /// The app runs but its REST interface does not answer.
    Unreachable,
    /// The app is stopped or gone.
    Stopped,
}

impl Alerts {
    /// New conflict copies, grouped per folder for [`CONFLICT_GROUP_MS`].
    pub fn conflict(&mut self, now: u64, folder: &str, file: &str) {
        let e = self.pending.entry(folder.to_string()).or_insert((now, 0, Vec::new()));
        e.1 = e.1.saturating_add(1);
        let name = file.rsplit('/').next().unwrap_or(file).to_string();
        if e.2.len() < CONFLICT_FILES_IN_ALERT && !e.2.contains(&name) {
            e.2.push(name);
        }
    }

    /// The transitions due at `now`.
    pub fn evaluate(&mut self, now: u64, seen: Seen<'_>, unseen_days: u32, conflicts_alert: bool) -> Vec<Alert> {
        let mut out = Vec::new();
        let m = match seen {
            Seen::Reachable(m) => m,
            Seen::Unreachable => {
                let since = *self.down_since.get_or_insert(now);
                if !self.down_notified && now.saturating_sub(since) >= DOWN_AFTER_MS {
                    self.down_notified = true;
                    out.push(Alert::Down { minutes: now.saturating_sub(since) / 60_000 });
                }
                return out;
            }
            Seen::Stopped => {
                self.down_since = None;
                self.pending.clear();
                if std::mem::take(&mut self.down_notified) {
                    out.push(Alert::Up { quiet: true });
                }
                return out;
            }
        };
        self.down_since = None;
        if std::mem::take(&mut self.down_notified) {
            out.push(Alert::Up { quiet: false });
        }
        // folders in error
        for f in &m.folders {
            if f.state == "error" && !f.paused {
                let since = *self.error_since.entry(f.id.clone()).or_insert(now);
                if !self.error_notified.contains_key(&f.id) && now.saturating_sub(since) >= ERROR_AFTER_MS {
                    self.error_notified.insert(f.id.clone(), f.label.clone());
                    out.push(Alert::FolderError {
                        folder: f.id.clone(),
                        label: f.label.clone(),
                        error: f.error.clone().unwrap_or_default(),
                    });
                }
            } else {
                self.error_since.remove(&f.id);
                if self.error_notified.remove(&f.id).is_some() {
                    out.push(Alert::FolderRecovered { folder: f.id.clone(), label: f.label.clone() });
                }
            }
        }
        let gone: Vec<String> = self.error_notified.keys().filter(|id| m.folder(id).is_none()).cloned().collect();
        for id in gone {
            if let Some(label) = self.error_notified.remove(&id) {
                out.push(Alert::FolderRecovered { folder: id.clone(), label });
            }
            self.error_since.remove(&id);
        }
        // conflict copies
        if !conflicts_alert {
            self.pending.clear();
        }
        let due: Vec<String> = self.pending.iter().filter(|(_, (t, _, _))| now.saturating_sub(*t) >= CONFLICT_GROUP_MS).map(|(k, _)| k.clone()).collect();
        for id in due {
            if let Some((_, count, files)) = self.pending.remove(&id) {
                let label = m.folder(&id).map(|f| f.label.clone()).unwrap_or_else(|| id.clone());
                out.push(Alert::Conflict { folder: id, label, count, files });
            }
        }
        // devices not seen for a while
        for d in m.devices.iter().filter(|d| d.id != m.my_id) {
            if d.connected {
                if self.unseen_notified.remove(&d.id) {
                    out.push(Alert::DeviceSeen { device: d.id.clone(), name: m.device_name(&d.id) });
                }
                continue;
            }
            let Some(last) = d.last_seen_ms else { continue };
            if unseen_days == 0 || d.paused || self.unseen_notified.contains(&d.id) {
                continue;
            }
            let away = now.saturating_sub(last);
            if away >= u64::from(unseen_days) * DAY_MS {
                self.unseen_notified.insert(d.id.clone());
                out.push(Alert::DeviceUnseen { device: d.id.clone(), name: m.device_name(&d.id), days: away / DAY_MS });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> Model {
        let mut m = Model { my_id: "MYSELF-AAAA".into(), ..Default::default() };
        let folders = folders_of_config(&json!([
            {"id": "docs", "label": "Documents", "path": "/shares/docs/work", "type": "sendreceive", "paused": false,
             "devices": [{"deviceID": "MYSELF-AAAA"}, {"deviceID": "PHONE12-BBBB"}]},
            {"id": "pics", "label": "", "path": "/shares/photos", "type": "receiveonly", "paused": true, "markerName": ".stfolder"}
        ]));
        let devices = devices_of_config(&json!([
            {"deviceID": "MYSELF-AAAA", "name": "nas"},
            {"deviceID": "PHONE12-BBBB", "name": "phone"},
            {"deviceID": "LAPTOP3-CCCC", "name": ""}
        ]));
        m.replace_config(folders, devices);
        m
    }

    #[test]
    fn config_and_reads() {
        let mut m = model();
        assert_eq!(m.folders[1].label, "pics");
        assert_eq!(m.folders[1].state, "paused");
        assert_eq!(m.folders[0].marker_name, ".stfolder");
        assert_eq!(m.device_name("LAPTOP3-CCCC"), "LAPTOP3");
        m.set_connections(&json!({"connections": {"PHONE12-BBBB": {"connected": true, "address": "192.0.2.5:22000", "type": "tcp-client"}}}));
        assert!(m.devices[1].connected);
        assert_eq!(m.devices[1].address.as_deref(), Some("192.0.2.5:22000"));
        assert!(!m.devices[2].connected);
        m.set_device_stats(&json!({"LAPTOP3-CCCC": {"lastSeen": "2026-11-20T10:00:00.123456789+01:00"}, "PHONE12-BBBB": {"lastSeen": "0001-01-01T00:00:00Z"}}));
        assert_eq!(m.devices[2].last_seen_ms, Some(1_795_165_200_123));
        assert_eq!(m.devices[1].last_seen_ms, None);
        m.set_folder_stats(&json!({"docs": {"lastScan": "2026-11-24T08:00:00Z", "lastFile": {"filename": "a/b.txt", "at": "2026-11-24T07:00:00Z"}}}));
        assert_eq!(m.folders[0].last_file.as_deref(), Some("a/b.txt"));
        assert!(m.folders[0].last_scan_ms.is_some());
        m.folders[0].set_status(&json!({"state": "error", "error": "folder marker missing", "needBytes": 5, "globalBytes": 10, "inSyncBytes": 5}));
        assert_eq!(m.folders[0].error.as_deref(), Some("folder marker missing"));
        m.folders[0].set_errors(&json!((0..30).map(|i| json!({"path": format!("f{i}"), "error": "denied"})).collect::<Vec<_>>()));
        assert_eq!((m.folders[0].errors.len(), m.folders[0].error_count), (MAX_ERRORS, 30));
        // a new configuration keeps what was read
        m.folders[0].add_conflict("x.sync-conflict-20261124-101010-ABCDEFG.txt");
        let again = folders_of_config(&json!([{"id": "docs", "label": "Docs", "path": "/shares/docs/work", "type": "sendreceive"}]));
        m.replace_config(again, Vec::new());
        assert_eq!(m.folders.len(), 1);
        assert_eq!(m.folders[0].label, "Docs");
        assert_eq!(m.folders[0].conflict_files.len(), 1);
        assert_eq!(m.folders[0].need_bytes, 5);
    }

    #[test]
    fn events() {
        let mut m = model();
        let ev = |t: &str, data: Value| json!({"id": 1, "type": t, "time": "2026-11-24T10:00:00Z", "data": data});
        m.apply(&ev("StateChanged", json!({"folder": "docs", "from": "idle", "to": "syncing"})), 0);
        assert_eq!(m.folders[0].state, "syncing");
        m.apply(&ev("FolderSummary", json!({"folder": "docs", "summary": {"state": "syncing", "needBytes": 25, "globalBytes": 100, "inSyncBytes": 75}})), 0);
        assert_eq!((m.folders[0].need_bytes, m.folders[0].global_bytes), (25, 100));
        m.apply(&ev("FolderCompletion", json!({"folder": "docs", "device": "PHONE12-BBBB", "completion": 42.5, "needBytes": 7})), 0);
        assert_eq!(m.folders[0].completion["PHONE12-BBBB"], (42.5, 7));
        m.apply(&ev("DeviceConnected", json!({"id": "PHONE12-BBBB", "addr": "192.0.2.5:22000", "type": "quic-server"})), 0);
        assert!(m.devices[1].connected);
        assert_eq!(m.devices[1].last_seen_ms, time_ms("2026-11-24T10:00:00Z"));
        m.apply(&ev("DeviceDisconnected", json!({"id": "PHONE12-BBBB", "error": "eof"})), 0);
        assert!(!m.devices[1].connected && m.devices[1].address.is_none());
        m.apply(&ev("FolderPaused", json!({"id": "docs", "label": "Documents"})), 0);
        assert!(m.folders[0].paused);
        assert_eq!(m.folders[0].state, "paused");
        m.apply(&ev("FolderResumed", json!({"id": "docs"})), 0);
        assert!(!m.folders[0].paused);
        let c = m.apply(&ev("ItemFinished", json!({"folder": "docs", "item": "a/r.sync-conflict-20261124-101010-ABCDEFG.odt", "error": null, "action": "update"})), 0);
        assert_eq!(c.conflicts, vec![("docs".to_string(), "a/r.sync-conflict-20261124-101010-ABCDEFG.odt".to_string())]);
        // the same copy again, a deletion, a failed item, a folder named like one: nothing new
        let again = m.apply(&ev("ItemFinished", json!({"folder": "docs", "item": "a/r.sync-conflict-20261124-101010-ABCDEFG.odt", "action": "update"})), 0);
        assert!(again.conflicts.is_empty());
        assert!(m.apply(&ev("ItemFinished", json!({"folder": "docs", "item": "b.sync-conflict-1.txt", "action": "delete"})), 0).conflicts.is_empty());
        assert!(m.apply(&ev("ItemFinished", json!({"folder": "docs", "item": "c.sync-conflict-1.txt", "error": "denied", "action": "update"})), 0).conflicts.is_empty());
        assert!(m.apply(&ev("ItemFinished", json!({"folder": "docs", "item": "x.sync-conflict-1/plain.txt", "action": "update"})), 0).conflicts.is_empty());
        assert_eq!(m.folders[0].conflicts(), 1);
        assert!(m.apply(&ev("ConfigSaved", json!({})), 0).full_read);
        m.apply(&ev("StateChanged", json!({"folder": "docs", "to": "error", "error": "folder path missing"})), 0);
        assert_eq!(m.folders[0].error.as_deref(), Some("folder path missing"));
        m.apply(&ev("FolderErrors", json!({"folder": "docs", "errors": [{"path": "a", "error": "permission denied"}]})), 0);
        assert_eq!(m.folders[0].error_count, 1);
    }

    #[test]
    fn ids_and_counters() {
        let evs = |ids: &[u64]| ids.iter().map(|i| json!({"id": i})).collect::<Vec<_>>();
        assert_eq!(check_ids(0, &evs(&[5, 6])), (false, 6));
        assert_eq!(check_ids(6, &evs(&[7, 8])), (false, 8));
        assert_eq!(check_ids(6, &[]), (false, 6));
        // a hole: events were lost
        assert_eq!(check_ids(6, &evs(&[9, 10])), (true, 10));
        // a smaller id: Syncthing restarted
        assert_eq!(check_ids(500, &evs(&[1, 2])), (true, 2));
        let metrics = "# HELP x\nsyncthing_model_folder_conflicts_total{folder=\"docs\"} 3\nsyncthing_model_folder_conflicts_total{folder=\"pics\",x=\"y\"} 1e0\nother{folder=\"docs\"} 9\n";
        let c = conflict_counters(metrics);
        assert_eq!(c.get("docs"), Some(&3));
        assert_eq!(c.get("pics"), Some(&1));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn alerts() {
        let mut m = model();
        let mut a = Alerts::default();
        let t0 = 1_000_000_000_000;
        // an error notified after 5 minutes, once, then recovered
        m.folders[0].state = "error".into();
        m.folders[0].error = Some("folder marker missing".into());
        assert!(a.evaluate(t0, Seen::Reachable(&m), 7, true).is_empty());
        assert!(a.evaluate(t0 + ERROR_AFTER_MS - 1, Seen::Reachable(&m), 7, true).is_empty());
        let v = a.evaluate(t0 + ERROR_AFTER_MS, Seen::Reachable(&m), 7, true);
        assert_eq!(v, vec![Alert::FolderError { folder: "docs".into(), label: "Documents".into(), error: "folder marker missing".into() }]);
        assert!(a.evaluate(t0 + 2 * ERROR_AFTER_MS, Seen::Reachable(&m), 7, true).is_empty());
        m.folders[0].state = "idle".into();
        assert_eq!(a.evaluate(t0 + 3 * ERROR_AFTER_MS, Seen::Reachable(&m), 7, true), vec![Alert::FolderRecovered { folder: "docs".into(), label: "Documents".into() }]);
        // a paused folder in error is not an error
        m.folders[1].state = "error".into();
        assert!(a.evaluate(t0 + 9 * ERROR_AFTER_MS, Seen::Reachable(&m), 7, true).is_empty());
        // conflicts grouped for 10 minutes
        for i in 0..7 {
            a.conflict(t0, "docs", &format!("d/f{i}.sync-conflict-20261124-101010-ABCDEFG.txt"));
        }
        assert!(a.evaluate(t0 + CONFLICT_GROUP_MS - 1, Seen::Reachable(&m), 7, true).is_empty());
        match &a.evaluate(t0 + CONFLICT_GROUP_MS, Seen::Reachable(&m), 7, true)[..] {
            [Alert::Conflict { label, count, files, .. }] => {
                assert_eq!((label.as_str(), *count, files.len()), ("Documents", 7, CONFLICT_FILES_IN_ALERT));
                assert_eq!(files[0], "f0.sync-conflict-20261124-101010-ABCDEFG.txt");
            }
            other => panic!("{other:?}"),
        }
        // the setting off drops them
        a.conflict(t0, "docs", "z.sync-conflict-1.txt");
        assert!(a.evaluate(t0 + CONFLICT_GROUP_MS, Seen::Reachable(&m), 7, false).is_empty());
        assert!(a.evaluate(t0 + 2 * CONFLICT_GROUP_MS, Seen::Reachable(&m), 7, true).is_empty());
        // a device not seen for 7 days, once per absence; re-armed when it connects
        m.devices[2].last_seen_ms = Some(t0);
        let week = 7 * DAY_MS;
        assert!(a.evaluate(t0 + week - 1, Seen::Reachable(&m), 7, true).is_empty());
        assert_eq!(a.evaluate(t0 + week, Seen::Reachable(&m), 7, true), vec![Alert::DeviceUnseen { device: "LAPTOP3-CCCC".into(), name: "LAPTOP3".into(), days: 7 }]);
        assert!(a.evaluate(t0 + 2 * week, Seen::Reachable(&m), 7, true).is_empty());
        m.devices[2].connected = true;
        assert_eq!(a.evaluate(t0 + 2 * week, Seen::Reachable(&m), 7, true), vec![Alert::DeviceSeen { device: "LAPTOP3-CCCC".into(), name: "LAPTOP3".into() }]);
        m.devices[2].connected = false;
        m.devices[2].last_seen_ms = Some(t0 + 2 * week);
        assert_eq!(a.evaluate(t0 + 3 * week, Seen::Reachable(&m), 7, true).len(), 1);
        // 0 days: off; never seen: never alerted
        let mut b = Alerts::default();
        assert!(b.evaluate(t0 + 50 * week, Seen::Reachable(&m), 0, true).is_empty());
        // down after 10 minutes, once; up when it answers again
        let mut c = Alerts::default();
        assert!(c.evaluate(t0, Seen::Unreachable, 7, true).is_empty());
        assert_eq!(c.evaluate(t0 + DOWN_AFTER_MS, Seen::Unreachable, 7, true), vec![Alert::Down { minutes: 10 }]);
        assert!(c.evaluate(t0 + 2 * DOWN_AFTER_MS, Seen::Unreachable, 7, true).is_empty());
        let quiet = Model { my_id: "X".into(), ..Default::default() };
        assert_eq!(c.evaluate(t0 + 3 * DOWN_AFTER_MS, Seen::Reachable(&quiet), 7, true), vec![Alert::Up { quiet: false }]);
        // stopped while down: resolved quietly
        c.evaluate(t0, Seen::Unreachable, 7, true);
        c.evaluate(t0 + DOWN_AFTER_MS, Seen::Unreachable, 7, true);
        assert_eq!(c.evaluate(t0 + DOWN_AFTER_MS, Seen::Stopped, 7, true), vec![Alert::Up { quiet: true }]);
    }
}
