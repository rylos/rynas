//! What a bay's disk is doing, from rynas's own view: the storage service's SMART verdict and
//! md's failed members. Empty bays have no state.

use std::path::Path;
use std::sync::Arc;

use rynas_api_types::storage::HealthVerdict;
use rynas_storage::Storage;
use rynas_storage::host::SystemHost;

use crate::model::Model;
use crate::provider;

/// The state of one bay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BayState {
    pub bay: u32,
    /// Kernel name of the disk; empty for an empty bay.
    pub device: String,
    pub health: HealthVerdict,
}

pub trait DiskSource: Send {
    /// `(kernel name, verdict)` of every disk.
    fn health(&mut self) -> Vec<(String, HealthVerdict)>;
}

/// The storage service (SMART is cached and never wakes a sleeping disk).
pub struct SystemDisks(Storage);

impl Default for SystemDisks {
    fn default() -> Self {
        Self(Storage::new(Arc::new(SystemHost)))
    }
}

impl DiskSource for SystemDisks {
    fn health(&mut self) -> Vec<(String, HealthVerdict)> {
        match self.0.disks() {
            Ok(d) => d.into_iter().map(|d| (d.name, d.health)).collect(),
            Err(e) => {
                tracing::warn!("qnap: disk state: {e}");
                Vec::new()
            }
        }
    }
}

/// Members md reports as failed: `sdb3[1](F)` → `sdb`.
pub fn failed_members(mdstat: &str) -> Vec<String> {
    mdstat
        .split_whitespace()
        .filter(|w| w.ends_with("(F)"))
        .filter_map(|w| w.split('[').next())
        .map(|p| p.trim_end_matches(|c: char| c.is_ascii_digit()).to_string())
        .collect()
}

/// Every bay of the model and the state of the disk in it.
pub fn bay_states(root: &Path, m: &Model, source: &mut dyn DiskSource) -> Vec<BayState> {
    let found = provider::disks_in_bays(root, m);
    let health = source.health();
    let failed = failed_members(&crate::model::read(root, "/proc/mdstat"));
    m.bays
        .iter()
        .map(|b| {
            let device = found.iter().find(|(n, _)| *n == b.bay).map(|(_, d)| d.clone()).unwrap_or_default();
            let verdict = health
                .iter()
                .find(|(n, _)| *n == device)
                .map(|(_, h)| *h)
                .unwrap_or(HealthVerdict::Unknown);
            let health = if !device.is_empty() && failed.contains(&device) { HealthVerdict::Failing } else { verdict };
            BayState { bay: b.bay, device, health }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_members_of_mdstat() {
        let m = "Personalities : [raid1]\nmd1 : active raid1 sdb3[1](F) sda3[0]\n      100 blocks [2/1] [U_]\n\nunused devices: <none>\n";
        assert_eq!(failed_members(m), vec!["sdb".to_string()]);
        assert!(failed_members("md0 : active raid1 sda1[0] sdb1[1]\n").is_empty());
    }
}
