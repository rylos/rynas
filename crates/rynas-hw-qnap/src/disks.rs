//! What a bay's disk is doing: the verdicts of rynasd's own storage check (nothing here reads
//! a disk, so nothing keeps one from spinning down) and md's failed members. Empty bays have
//! no state.

use std::path::Path;

use rynas_api_types::storage::HealthVerdict;

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

/// Members md reports as failed: `sdb3[1](F)` → `sdb`.
pub fn failed_members(mdstat: &str) -> Vec<String> {
    mdstat
        .split_whitespace()
        .filter(|w| w.ends_with("(F)"))
        .filter_map(|w| w.split('[').next())
        .map(|p| p.trim_end_matches(|c: char| c.is_ascii_digit()).to_string())
        .collect()
}

/// Every bay of the model and the state of the disk in it, from `(kernel name, verdict)`.
pub fn bay_states(root: &Path, m: &Model, health: &[(String, HealthVerdict)]) -> Vec<BayState> {
    let found = provider::disks_in_bays(root, m);
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
