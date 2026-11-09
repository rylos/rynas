//! Compressed swap in RAM (`/api/v1/system/zram`), administrators only (D-ZRAM).

use serde::{Deserialize, Serialize};

/// What the administrator chooses; also the body of `POST /system/zram` (task `zram.apply`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZramSettings {
    pub enabled: bool,
    /// Size of the device: how much data it can hold before compression.
    pub size_mib: u32,
    /// One of [`ZramState::algorithms`].
    pub algorithm: String,
    /// `vm.swappiness`, 0-200.
    pub swappiness: u32,
    /// `vm.page-cluster`, 0-5.
    pub page_cluster: u32,
}

/// The device as it runs now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZramRunning {
    pub size: u64,
    pub algorithm: String,
    /// Data held, before compression.
    pub data: u64,
    /// RAM it takes.
    pub mem_used: u64,
}

/// A swap device other than zram, from `/proc/swaps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSwap {
    pub path: String,
    pub size: u64,
    pub used: u64,
    pub priority: i32,
    /// `None`: not known (a swap file).
    pub rotational: Option<bool>,
}

/// `GET /system/zram`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZramState {
    /// The running kernel has zram.
    pub supported: bool,
    /// `systemd-zram-generator` is there; without it, enabling installs it first.
    pub installed: bool,
    /// A device set up by something else than the generator: rynas leaves it alone.
    pub external: bool,
    /// The saved choice, or what runs when rynas never saved one.
    pub settings: ZramSettings,
    /// The values advised for this machine (`enabled` is true).
    pub recommended: ZramSettings,
    pub running: Option<ZramRunning>,
    /// The saved values are not what runs: zram held too much to be emptied, or a start
    /// failed. Applying again or a restart settles it.
    pub pending_restart: bool,
    pub algorithms: Vec<String>,
    pub mem_total: u64,
    /// The largest accepted [`ZramSettings::size_mib`].
    pub max_size_mib: u32,
    /// Other swap devices, used after zram (zram has the highest priority).
    pub disk_swaps: Vec<DiskSwap>,
    /// zswap is on: a second compression layer in front of zram, to turn off.
    pub zswap: bool,
}
