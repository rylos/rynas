//! SMART warnings marked as seen ("acknowledged", as in Unraid): an administrator records
//! the count of a counter that never goes back to zero (see
//! [`crate::smart::ACKNOWLEDGEABLE`]); while the count stays exactly at the recorded one the
//! reason is still listed but no longer weighs in the disk verdict, so an old disk with
//! historical cable errors turns healthy. When the count grows the reason counts again by
//! itself (verdict, notifications, plans), with no action needed; a count that dropped
//! (vendor reset, a 16-bit counter that wrapped) cancels the acknowledgement too.
//!
//! The recorded count is the lower of what the administrator saw (sent by the client) and
//! the current reading (taken by rynasd): errors that arrived after the page was loaded stay
//! visible, and a client can never raise a value above the drive's own.
//!
//! Acknowledgements belong to a drive, not to a kernel name: they are keyed by serial number
//! and model (and WWN when known), so a different disk in the same bay never inherits them.
//! A serial seen on more than one attached disk (cheap USB bridges report the same one) is
//! not an identity: such disks can neither be acknowledged nor get acknowledgements. They
//! are runtime state of rynasd (not configuration: nothing on the system is rendered from
//! them), kept in a small JSON file in its state directory. Applied in one place:
//! [`crate::inventory::summary`].

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rynas_api_types::storage::{HealthAckCode, HealthReason, ReasonAck};
use serde::{Deserialize, Serialize};

/// Drives remembered at most (the oldest acknowledgement goes first).
const MAX_DISKS: usize = 256;

/// What identifies a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveId<'a> {
    pub serial: &'a str,
    pub model: &'a str,
    pub wwn: Option<&'a str>,
}

/// The acknowledgements of one drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiskAcks {
    pub serial: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wwn: Option<String>,
    /// Reason code → what was seen.
    #[serde(default)]
    pub codes: BTreeMap<String, ReasonAck>,
}

impl DiskAcks {
    /// This record belongs to the drive `id`: same serial (never an empty one), same model,
    /// and the same WWN when both are known.
    pub fn matches(&self, id: DriveId<'_>) -> bool {
        !id.serial.is_empty()
            && self.serial == id.serial
            && self.model.trim() == id.model.trim()
            && match (self.wwn.as_deref(), id.wwn) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => true,
            }
    }

    fn newest_ms(&self) -> u64 {
        self.codes.values().map(|a| a.at_ms).max().unwrap_or(0)
    }
}

/// The file: every drive with acknowledgements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AckBook {
    #[serde(default)]
    pub disks: Vec<DiskAcks>,
}

impl AckBook {
    pub fn find(&self, id: DriveId<'_>) -> Option<&DiskAcks> {
        self.disks.iter().find(|d| d.matches(id))
    }
}

/// Count of a reason as an integer (counters only).
fn count(r: &HealthReason) -> Option<u64> {
    r.args
        .n
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n.round() as u64)
}

/// Marks the reasons whose count equals the acknowledged one. A count that grew or dropped,
/// and reasons that are not acknowledgeable (any more: a reallocated count that turned
/// critical), are never marked.
pub fn apply(reasons: &mut [HealthReason], acks: Option<&DiskAcks>) {
    for r in reasons.iter_mut() {
        r.acknowledged = None;
        let Some(a) = acks.and_then(|a| a.codes.get(&r.code)) else {
            continue;
        };
        if r.acknowledgeable && count(r) == Some(a.n) {
            r.acknowledged = Some(a.clone());
        }
    }
}

/// What [`AckStore::acknowledge`] recorded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AckOutcome {
    pub recorded: Vec<HealthAckCode>,
    /// Codes whose current count is above what was seen: recorded at the seen value, so they
    /// keep warning.
    pub grew: Vec<String>,
}

/// The acknowledgement store: in memory, and in a file once [`AckStore::open`] named one.
#[derive(Debug, Default)]
pub struct AckStore {
    inner: Mutex<(Option<PathBuf>, AckBook)>,
}

impl AckStore {
    /// Memory only (tests, a storage service without a state directory).
    pub fn memory() -> Self {
        Self::default()
    }

    /// Reads `path` and saves there from now on. A missing file is an empty book; a damaged
    /// one is moved aside to `.bad` (kept for inspection, never silently overwritten).
    pub fn open(&self, path: &Path) {
        let book = match std::fs::read_to_string(path) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
                let bad = path.with_extension("json.bad");
                let moved = std::fs::rename(path, &bad);
                tracing::warn!(
                    "{}: {e}: SMART acknowledgements ignored (moved to {}: {moved:?})",
                    path.display(),
                    bad.display()
                );
                AckBook::default()
            }),
            Err(_) => AckBook::default(),
        };
        *self.inner.lock().unwrap() = (Some(path.to_path_buf()), book);
    }

    /// The acknowledgements of one drive.
    pub fn for_disk(&self, id: DriveId<'_>) -> Option<DiskAcks> {
        self.inner.lock().unwrap().1.find(id).cloned()
    }

    /// Records, for every acknowledgeable reason in `current` (a new reading) that the client
    /// `seen`, the lower of the two counts (others are left alone; earlier acknowledgements
    /// of codes not listed now are kept). Nothing is saved when nothing matched.
    pub fn acknowledge(
        &self,
        id: DriveId<'_>,
        current: &[HealthReason],
        seen: &[HealthAckCode],
        by: &str,
        now_ms: u64,
    ) -> std::io::Result<AckOutcome> {
        let mut out = AckOutcome::default();
        if id.serial.is_empty() {
            return Ok(out);
        }
        for r in current.iter().filter(|r| r.acknowledgeable) {
            let (Some(cur), Some(s)) = (count(r), seen.iter().find(|s| s.code == r.code)) else {
                continue;
            };
            if cur > s.n {
                out.grew.push(r.code.clone());
            }
            out.recorded.push(HealthAckCode {
                code: r.code.clone(),
                n: cur.min(s.n),
            });
        }
        if out.recorded.is_empty() {
            return Ok(out);
        }
        let mut g = self.inner.lock().unwrap();
        let (path, book) = &mut *g;
        let mut next = book.clone();
        let i = match next.disks.iter().position(|d| d.matches(id)) {
            Some(i) => i,
            None => {
                next.disks.push(DiskAcks {
                    serial: id.serial.to_string(),
                    model: id.model.trim().to_string(),
                    wwn: id.wwn.map(str::to_string),
                    codes: BTreeMap::new(),
                });
                next.disks.len() - 1
            }
        };
        let d = &mut next.disks[i];
        if d.wwn.is_none() {
            d.wwn = id.wwn.map(str::to_string);
        }
        for s in &out.recorded {
            d.codes.insert(
                s.code.clone(),
                ReasonAck {
                    n: s.n,
                    at_ms: now_ms,
                    by: by.to_string(),
                },
            );
        }
        while next.disks.len() > MAX_DISKS {
            let oldest = (0..next.disks.len())
                .min_by_key(|&i| next.disks[i].newest_ms())
                .unwrap_or(0);
            next.disks.remove(oldest);
        }
        if let Some(p) = path {
            save(p, &next)?;
        }
        *book = next;
        Ok(out)
    }

    /// Forgets the acknowledgements of one drive; `false` when it had none.
    pub fn clear(&self, id: DriveId<'_>) -> std::io::Result<bool> {
        let mut g = self.inner.lock().unwrap();
        let (path, book) = &mut *g;
        let mut next = book.clone();
        let before = next.disks.len();
        next.disks.retain(|d| !d.matches(id));
        if next.disks.len() == before {
            return Ok(false);
        }
        if let Some(p) = path {
            save(p, &next)?;
        }
        *book = next;
        Ok(true)
    }
}

/// Writes the book to a temporary file (0600, synced), renames it over `path` and syncs the
/// directory: never a half-written file, and the rename survives a power cut.
fn save(path: &Path, book: &AckBook) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.new");
    let data = serde_json::to_vec_pretty(book).map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&data)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smart::{acknowledgeable, verdict_of};
    use rynas_api_types::storage::{HealthVerdict, ReasonArgs, Severity};

    fn r(code: &str, sev: Severity, n: f64) -> HealthReason {
        HealthReason {
            severity: sev,
            code: code.into(),
            message: format!("{n} {code}"),
            args: ReasonArgs {
                n: Some(n),
                ..Default::default()
            },
            acknowledgeable: acknowledgeable(code, sev, Some(n)),
            acknowledged: None,
        }
    }

    /// The real case: a Seagate ST4000DM000 with 131 timeouts and 185 CRC errors, both old.
    fn seagate() -> Vec<HealthReason> {
        vec![
            r("command_timeouts", Severity::Warning, 131.0),
            r("crc_errors", Severity::Info, 185.0),
        ]
    }

    /// What the page showed: every count of `reasons`.
    fn seen(reasons: &[HealthReason]) -> Vec<HealthAckCode> {
        reasons
            .iter()
            .map(|r| HealthAckCode {
                code: r.code.clone(),
                n: r.args.n.unwrap() as u64,
            })
            .collect()
    }

    const ID: DriveId<'static> = DriveId {
        serial: "Z1",
        model: "ST4000DM000-1F2168",
        wwn: Some("0x5000c5"),
    };

    #[test]
    fn classification() {
        use Severity::*;
        for c in [
            "command_timeouts",
            "crc_errors",
            "reported_uncorrect",
            "error_log",
            "reallocated_sectors",
            "media_errors",
        ] {
            assert!(acknowledgeable(c, Warning, Some(3.0)), "{c}");
            assert!(acknowledgeable(c, Info, Some(3.0)), "{c}");
            assert!(!acknowledgeable(c, Critical, Some(3.0)), "{c} critical");
            assert!(!acknowledgeable(c, Warning, None), "{c} without a count");
        }
        for c in [
            "pending_sectors",
            "offline_uncorrectable",
            "self_test_failed",
            "temperature",
            "old_disk",
            "wear",
            "load_cycles",
            "smart_failed",
            "nvme_spare_low",
        ] {
            assert!(!acknowledgeable(c, Warning, Some(3.0)), "{c}");
        }
    }

    #[test]
    fn equal_count_is_healthy_growth_or_drop_warns_again() {
        let s = AckStore::memory();
        let mut v = seagate();
        assert_eq!(verdict_of(&v), HealthVerdict::Warning);
        let o = s.acknowledge(ID, &v, &seen(&v), "admin", 1_000).unwrap();
        assert_eq!(o.recorded.len(), 2);
        assert!(o.grew.is_empty());
        apply(&mut v, s.for_disk(ID).as_ref());
        assert!(v.iter().all(|x| x.acknowledged.is_some()));
        assert_eq!(v[0].acknowledged.as_ref().unwrap().by, "admin");
        assert_eq!(
            verdict_of(&v),
            HealthVerdict::Healthy,
            "still listed, not counted"
        );
        assert_eq!(v.len(), 2);

        // one more timeout: the warning comes back by itself
        let mut grown = vec![
            r("command_timeouts", Severity::Warning, 132.0),
            r("crc_errors", Severity::Info, 185.0),
        ];
        apply(&mut grown, s.for_disk(ID).as_ref());
        assert!(grown[0].acknowledged.is_none());
        assert!(grown[1].acknowledged.is_some());
        assert_eq!(verdict_of(&grown), HealthVerdict::Warning);

        // the 16-bit counter wrapped (or the vendor reset it): new errors are not hidden
        let mut wrapped = vec![r("command_timeouts", Severity::Warning, 4.0)];
        apply(&mut wrapped, s.for_disk(ID).as_ref());
        assert!(wrapped[0].acknowledged.is_none());
        assert_eq!(verdict_of(&wrapped), HealthVerdict::Warning);
    }

    /// The count grew after the page was loaded: the seen value is recorded, so the new
    /// errors keep warning; a client can never raise a value above the drive's.
    #[test]
    fn records_the_lower_of_seen_and_current() {
        let s = AckStore::memory();
        let now = vec![r("command_timeouts", Severity::Warning, 133.0)];
        let saw = [HealthAckCode {
            code: "command_timeouts".into(),
            n: 131,
        }];
        let o = s.acknowledge(ID, &now, &saw, "admin", 1).unwrap();
        assert_eq!(o.recorded[0].n, 131);
        assert_eq!(o.grew, ["command_timeouts"]);
        let mut v = now.clone();
        apply(&mut v, s.for_disk(ID).as_ref());
        assert_eq!(verdict_of(&v), HealthVerdict::Warning);

        let inflated = [HealthAckCode {
            code: "command_timeouts".into(),
            n: 99_999,
        }];
        let o = s.acknowledge(ID, &now, &inflated, "admin", 2).unwrap();
        assert_eq!(o.recorded[0].n, 133, "never above the current count");
        assert!(o.grew.is_empty());

        // codes not seen, or not acknowledgeable now, are not recorded
        let temp = vec![r("temperature", Severity::Warning, 60.0)];
        let saw_temp = [HealthAckCode {
            code: "temperature".into(),
            n: 60,
        }];
        let other = DriveId { serial: "Z4", ..ID };
        assert!(
            s.acknowledge(other, &temp, &saw_temp, "a", 1)
                .unwrap()
                .recorded
                .is_empty()
        );
        assert!(
            s.acknowledge(other, &seagate(), &[], "a", 1)
                .unwrap()
                .recorded
                .is_empty()
        );
        assert!(s.for_disk(other).is_none());
    }

    #[test]
    fn never_on_another_disk_nor_critical() {
        let s = AckStore::memory();
        s.acknowledge(ID, &seagate(), &seen(&seagate()), "admin", 1)
            .unwrap();
        for id in [
            DriveId { serial: "Z2", ..ID },
            DriveId {
                wwn: Some("0x5000c6"),
                ..ID
            },
            DriveId {
                model: "ST8000VN004",
                ..ID
            },
            DriveId { serial: "", ..ID },
        ] {
            assert!(s.for_disk(id).is_none(), "{id:?}");
        }
        assert!(
            s.for_disk(DriveId { wwn: None, ..ID }).is_some(),
            "WWN unknown now"
        );
        let blank = DriveId { serial: "", ..ID };
        assert!(
            s.acknowledge(blank, &seagate(), &seen(&seagate()), "a", 1)
                .unwrap()
                .recorded
                .is_empty()
        );

        // reallocated sectors acknowledged at 5, now 120 and critical: counts again
        let z3 = DriveId {
            serial: "Z3",
            wwn: None,
            ..ID
        };
        let mut realloc = vec![r("reallocated_sectors", Severity::Warning, 5.0)];
        s.acknowledge(z3, &realloc, &seen(&realloc), "a", 1)
            .unwrap();
        apply(&mut realloc, s.for_disk(z3).as_ref());
        assert_eq!(verdict_of(&realloc), HealthVerdict::Healthy);
        let mut crit = vec![r("reallocated_sectors", Severity::Critical, 120.0)];
        apply(&mut crit, s.for_disk(z3).as_ref());
        assert_eq!(verdict_of(&crit), HealthVerdict::Failing);
    }

    #[test]
    fn file_round_trip_and_clear() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("storage").join("health-acks.json");
        let s = AckStore::memory();
        s.open(&path);
        s.acknowledge(ID, &seagate(), &seen(&seagate()), "admin", 42)
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!path.with_extension("json.new").exists());

        let again = AckStore::memory();
        again.open(&path);
        let d = again.for_disk(ID).expect("kept across restarts");
        assert_eq!(d.codes["command_timeouts"].n, 131);
        assert_eq!(d.codes["crc_errors"].at_ms, 42);

        assert!(again.clear(ID).unwrap());
        assert!(!again.clear(ID).unwrap());
        let third = AckStore::memory();
        third.open(&path);
        assert!(third.for_disk(ID).is_none());

        // a damaged file is moved aside, not fatal and not overwritten in silence
        std::fs::write(&path, "{").unwrap();
        let broken = AckStore::memory();
        broken.open(&path);
        assert!(broken.for_disk(ID).is_none());
        let bad = path.with_extension("json.bad");
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{");
        assert!(!path.exists());
    }
}
