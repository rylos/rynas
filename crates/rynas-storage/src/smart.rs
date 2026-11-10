//! SMART from `smartctl --json=c -n standby -a [-g apm -g wcache]` (smartmontools 7.4 on
//! Debian 13; the JSON format is versioned by `json_format_version` [1, 0]) and the verdict
//! rynas computes from it.
//!
//! `-n standby` makes smartctl check the power mode first and give up, without touching the
//! medium, when the disk is spun down: it prints "Device is in STANDBY mode, exit(2)".

use rynas_api_types::storage::{
    DiskKind, HealthReason, HealthVerdict, NvmeHealth, PowerState, ReasonArgs, SelfTest, Severity,
    SmartAttribute, SmartInfo,
};
use serde_json::Value;

/// Arguments for one disk, before the device path. NVMe has no spin-down and no ATA "get"
/// options, so it gets the plain `-a`.
pub fn smartctl_args(nvme: bool) -> &'static [&'static str] {
    if nvme {
        &["--json=c", "-a"]
    } else {
        &[
            "--json=c", "-n", "standby", "-a", "-g", "apm", "-g", "wcache",
        ]
    }
}

/// Attributes that feed the verdict (see `docs/research/system-reliability-tweaks.md` §6).
pub const WATCHED_ATA: &[u8] = &[5, 9, 187, 188, 193, 194, 197, 198, 199, 177, 202, 231, 233];

/// What one smartctl run told us.
#[derive(Debug, Clone, PartialEq)]
pub enum SmartRead {
    Data(Box<SmartData>),
    /// The disk is in a low-power mode and was left alone.
    Asleep(PowerState),
    /// The device has no SMART smartctl can reach (flash modules, USB bridges that do not
    /// pass it through): nothing to read, now or later.
    Unsupported(String),
    /// smartctl failed (no permission, device gone, ...).
    Failed(String),
}

/// smartctl's words for a device without reachable SMART.
fn unsupported_message(m: &str) -> bool {
    const WORDS: &[&str] = &[
        "Unknown USB bridge",
        "Unsupported USB bridge",
        "specify device type with the -d option",
        "lacks SMART capability",
        "SMART support is: Unavailable",
        "Device does not support SMART",
    ];
    WORDS.iter().any(|w| m.contains(w))
}

/// Parsed SMART plus a few settings smartctl reports along the way.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SmartData {
    pub info: SmartInfo,
    pub model_family: Option<String>,
    pub rotation_rpm: Option<u32>,
    pub firmware: Option<String>,
    /// APM level (1..=254), Some(None) when APM is supported but disabled.
    pub apm: Option<Option<u32>>,
    pub write_cache: Option<bool>,
    pub trim_supported: Option<bool>,
}

fn u64_at(v: &Value, path: &[&str]) -> Option<u64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_u64().or_else(|| cur.as_f64().map(|f| f as u64))
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str()
}

fn bool_at(v: &Value, path: &[&str]) -> Option<bool> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_bool()
}

/// Low-power mode named in smartctl's messages, if it skipped the disk.
fn asleep_from_messages(v: &Value) -> Option<PowerState> {
    let msgs = v.get("smartctl")?.get("messages")?.as_array()?;
    for m in msgs {
        let s = m.get("string").and_then(Value::as_str).unwrap_or("");
        if let Some(rest) = s.strip_prefix("Device is in ") {
            let mode = rest.split_whitespace().next().unwrap_or("");
            return Some(match mode {
                "STANDBY" => PowerState::Standby,
                "SLEEP" => PowerState::Sleep,
                "IDLE" | "IDLE_A" | "IDLE_B" | "IDLE_C" => PowerState::Idle,
                _ => PowerState::Standby,
            });
        }
    }
    None
}

/// Parse one smartctl JSON document. `now_ms` stamps the result.
pub fn parse(json: &str, now_ms: u64) -> SmartRead {
    let v: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => return SmartRead::Failed(format!("smartctl output: {e}")),
    };
    if let Some(p) = asleep_from_messages(&v) {
        return SmartRead::Asleep(p);
    }
    let exit_status = u64_at(&v, &["smartctl", "exit_status"]).unwrap_or(0) as u32;
    let messages: Vec<String> = v
        .get("smartctl")
        .and_then(|s| s.get("messages"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("string").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // bit 1: device open failed; bit 0: command line error
    if exit_status & 0b11 != 0 {
        if let Some(m) = messages.iter().find(|m| unsupported_message(m)) {
            return SmartRead::Unsupported(m.clone());
        }
        let msg = messages
            .first()
            .cloned()
            .unwrap_or_else(|| format!("smartctl exit status {exit_status}"));
        return SmartRead::Failed(msg);
    }
    if bool_at(&v, &["smart_support", "available"]) == Some(false) {
        return SmartRead::Unsupported(
            messages
                .iter()
                .find(|m| unsupported_message(m))
                .cloned()
                .unwrap_or_else(|| "SMART support is: Unavailable".into()),
        );
    }
    let version = v
        .get("smartctl")
        .and_then(|s| s.get("version"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_u64)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(".")
        })
        .unwrap_or_default();
    let protocol = str_at(&v, &["device", "protocol"])
        .unwrap_or("")
        .to_string();

    let mut info = SmartInfo {
        read_ms: now_ms,
        smartctl_version: version,
        protocol: protocol.clone(),
        exit_status,
        passed: bool_at(&v, &["smart_status", "passed"]),
        smart_enabled: bool_at(&v, &["smart_support", "enabled"]),
        temperature_c: u64_at(&v, &["temperature", "current"]).map(|t| t as f32),
        power_on_hours: u64_at(&v, &["power_on_time", "hours"]),
        power_cycles: u64_at(&v, &["power_cycle_count"]),
        messages,
        ..Default::default()
    };

    // ATA attributes
    if let Some(table) = v
        .get("ata_smart_attributes")
        .and_then(|a| a.get("table"))
        .and_then(Value::as_array)
    {
        for a in table {
            let id = a.get("id").and_then(Value::as_u64).unwrap_or(0) as u8;
            info.attributes.push(SmartAttribute {
                id,
                name: a
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                value: u64_at(a, &["value"]).unwrap_or(0) as u32,
                worst: u64_at(a, &["worst"]).unwrap_or(0) as u32,
                thresh: u64_at(a, &["thresh"]).unwrap_or(0) as u32,
                raw: u64_at(a, &["raw", "value"]).unwrap_or(0),
                raw_string: str_at(a, &["raw", "string"]).unwrap_or("").to_string(),
                prefailure: bool_at(a, &["flags", "prefailure"]).unwrap_or(false),
                when_failed: a
                    .get("when_failed")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                watched: WATCHED_ATA.contains(&id),
            });
        }
    }
    let sd = v.get("ata_smart_data");
    if let Some(sd) = sd {
        info.short_test_minutes =
            u64_at(sd, &["self_test", "polling_minutes", "short"]).map(|m| m as u32);
        info.extended_test_minutes =
            u64_at(sd, &["self_test", "polling_minutes", "extended"]).map(|m| m as u32);
        // status value 0xF? = in progress, low nibble = remaining tenths
        if let Some(st) = u64_at(sd, &["self_test", "status", "value"])
            && st >> 4 == 0xf
        {
            info.self_test_running = Some(
                u64_at(sd, &["self_test", "status", "remaining_percent"]).unwrap_or((st & 0xf) * 10)
                    as u32,
            );
        }
    }
    if let Some(table) = v
        .get("ata_smart_self_test_log")
        .and_then(|l| l.get("standard"))
        .and_then(|s| s.get("table"))
        .and_then(Value::as_array)
    {
        for t in table {
            info.self_tests.push(SelfTest {
                kind: str_at(t, &["type", "string"]).unwrap_or("").to_string(),
                status: str_at(t, &["status", "string"]).unwrap_or("").to_string(),
                passed: bool_at(t, &["status", "passed"]),
                lifetime_hours: u64_at(t, &["lifetime_hours"]),
                lba_first_error: u64_at(t, &["lba"]),
            });
        }
    }
    info.error_log_count = u64_at(&v, &["ata_smart_error_log", "summary", "count"])
        .or_else(|| u64_at(&v, &["ata_smart_error_log", "extended", "count"]));

    // NVMe
    if let Some(h) = v.get("nvme_smart_health_information_log") {
        let g = |k: &str| u64_at(h, &[k]).unwrap_or(0);
        // one data unit = 1000 512-byte blocks
        let tb = |units: u64| (units as f64 * 512_000.0) / 1e12;
        let n = NvmeHealth {
            critical_warning: g("critical_warning") as u8,
            temperature_c: u64_at(h, &["temperature"]).map(|t| t as f32),
            available_spare_pct: g("available_spare") as u32,
            available_spare_threshold_pct: g("available_spare_threshold") as u32,
            percentage_used: g("percentage_used") as u32,
            data_units_read: g("data_units_read"),
            data_units_written: g("data_units_written"),
            tb_written: (tb(g("data_units_written")) * 100.0).round() / 100.0,
            tb_read: (tb(g("data_units_read")) * 100.0).round() / 100.0,
            power_cycles: g("power_cycles"),
            power_on_hours: g("power_on_hours"),
            unsafe_shutdowns: g("unsafe_shutdowns"),
            media_errors: g("media_errors"),
            error_log_entries: g("num_err_log_entries"),
            warning_temp_minutes: g("warning_temp_time"),
            critical_temp_minutes: g("critical_comp_time"),
        };
        info.temperature_c = info.temperature_c.or(n.temperature_c);
        info.power_on_hours = info.power_on_hours.or(Some(n.power_on_hours));
        info.error_log_count = Some(n.error_log_entries);
        info.life_remaining_pct = Some(100u32.saturating_sub(n.percentage_used));
        info.bytes_written = Some(n.data_units_written.saturating_mul(512_000));
        info.nvme = Some(n);
    }
    if let Some(log) = v.get("nvme_self_test_log") {
        if let Some(cur) = u64_at(log, &["current_self_test_operation", "value"])
            && cur != 0
        {
            info.self_test_running = Some(100u32.saturating_sub(
                u64_at(log, &["current_self_test_completion_percent"]).unwrap_or(0) as u32,
            ));
        }
        if let Some(table) = log.get("table").and_then(Value::as_array) {
            for t in table {
                let result = u64_at(t, &["self_test_result", "value"]);
                info.self_tests.push(SelfTest {
                    kind: str_at(t, &["self_test_code", "string"])
                        .unwrap_or("")
                        .to_string(),
                    status: str_at(t, &["self_test_result", "string"])
                        .unwrap_or("")
                        .to_string(),
                    // 0 = completed without error; 1/2 = aborted by host/reset (not a fault)
                    passed: result.map(|r| r == 0 || r == 1 || r == 2),
                    lifetime_hours: u64_at(t, &["power_on_hours"]),
                    lba_first_error: u64_at(t, &["lba"]),
                });
            }
        }
    }

    // SATA SSD wear and writes
    if info.nvme.is_none() {
        info.life_remaining_pct = ssd_life_remaining(&info.attributes);
        info.bytes_written = ata_bytes_written(&info.attributes);
    }

    let apm = apm_of(&v);
    SmartRead::Data(Box::new(SmartData {
        model_family: str_at(&v, &["model_family"]).map(str::to_string),
        rotation_rpm: u64_at(&v, &["rotation_rate"])
            .filter(|&r| r > 1)
            .map(|r| r as u32),
        firmware: str_at(&v, &["firmware_version"]).map(str::to_string),
        apm,
        write_cache: bool_at(&v, &["write_cache", "enabled"]),
        trim_supported: bool_at(&v, &["trim", "supported"]),
        info,
    }))
}

fn apm_of(v: &Value) -> Option<Option<u32>> {
    v.get("ata_apm").map(|a| {
        if bool_at(a, &["enabled"]) == Some(true) {
            u64_at(a, &["level"]).map(|l| l as u32)
        } else {
            None
        }
    })
}

/// Arguments of the quick re-read of the settings of an ATA disk after rynas changed them
/// (APM and write cache only, no SMART data), before the device path. `-n standby`: a disk
/// that sleeps is left alone.
pub const SETTINGS_ARGS: &[&str] = &["--json=c", "-n", "standby", "-g", "apm", "-g", "wcache"];

/// What [`SETTINGS_ARGS`] reported.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SettingsRead {
    /// APM (as [`SmartData::apm`]) and write cache.
    Data {
        apm: Option<Option<u32>>,
        write_cache: Option<bool>,
    },
    /// The disk sleeps (or smartctl failed): nothing was read.
    Skipped,
}

/// Parse the output of [`SETTINGS_ARGS`].
pub fn parse_settings(json: &str) -> SettingsRead {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return SettingsRead::Skipped;
    };
    let exit_status = u64_at(&v, &["smartctl", "exit_status"]).unwrap_or(0);
    if asleep_from_messages(&v).is_some() || exit_status & 0b11 != 0 {
        return SettingsRead::Skipped;
    }
    SettingsRead::Data {
        apm: apm_of(&v),
        write_cache: bool_at(&v, &["write_cache", "enabled"]),
    }
}

/// Normalized value of the vendor wear attribute, which counts down from 100.
fn ssd_life_remaining(attrs: &[SmartAttribute]) -> Option<u32> {
    // Samsung 177 Wear_Leveling_Count, Crucial/Micron 202 Percent_Lifetime_Remain,
    // Kingston/SanDisk 231 SSD_Life_Left, Intel 233 Media_Wearout_Indicator
    for id in [177u8, 202, 231, 233] {
        if let Some(a) = attrs.iter().find(|a| a.id == id)
            && a.value <= 100
            && (a.name.contains("Wear") || a.name.contains("Life") || a.name.contains("Lifetime"))
        {
            return Some(a.value);
        }
    }
    None
}

/// Host writes from the vendor attribute, by its name's unit.
fn ata_bytes_written(attrs: &[SmartAttribute]) -> Option<u64> {
    attrs.iter().find_map(|a| {
        let n = a.name.as_str();
        let unit: u64 = if n.contains("LBAs_Written") || n.contains("Host_Sector_Write") {
            512
        } else if n.contains("Writes_32MiB") {
            32 << 20
        } else if n.contains("Writes_GiB") {
            1 << 30
        } else {
            return None;
        };
        Some(a.raw.saturating_mul(unit))
    })
}

fn reason(severity: Severity, code: &str, message: String, args: ReasonArgs) -> HealthReason {
    let acknowledgeable = acknowledgeable(code, severity, args.n);
    HealthReason {
        severity,
        code: code.to_string(),
        message,
        args,
        acknowledgeable,
        acknowledged: None,
    }
}

/// Reason codes an administrator may mark as seen ("acknowledge", as in Unraid): counters
/// the drive never brings back to zero, so an old event would keep the disk yellow forever.
/// The acknowledgement records the count; when it grows the reason weighs in again.
///
/// - `command_timeouts` (188), `crc_errors` (199), `reported_uncorrect` (187): lifetime
///   counts, usually cable/power events of the past.
/// - `error_log`: the ATA error log count (only grows).
/// - `reallocated_sectors` (5): only while it is a warning (below 100); remapped sectors stay
///   remapped.
/// - `media_errors`: the NVMe media and data integrity error count (only grows).
///
/// Never: anything Critical, `pending_sectors` / `offline_uncorrectable` (they go up and
/// down), failed self-tests, temperature, age, wear and life, `load_cycles` (it grows with
/// normal use, an acknowledgement would not last), SMART overall FAILED, NVMe warning bits.
pub const ACKNOWLEDGEABLE: &[&str] = &[
    "command_timeouts",
    "crc_errors",
    "reported_uncorrect",
    "error_log",
    "reallocated_sectors",
    "media_errors",
];

/// A reason with this code, severity and count may be marked as seen ([`ACKNOWLEDGEABLE`]).
pub fn acknowledgeable(code: &str, severity: Severity, n: Option<f64>) -> bool {
    severity != Severity::Critical
        && n.is_some_and(|n| n.is_finite() && n >= 0.0)
        && ACKNOWLEDGEABLE.contains(&code)
}

/// [`ReasonArgs`] with only `n`.
fn n_arg(n: f64) -> ReasonArgs {
    ReasonArgs {
        n: Some(n),
        ..Default::default()
    }
}

/// Temperature (°C) above which a disk type gets a warning / critical reason.
pub fn temp_limits(kind: DiskKind) -> (f32, f32) {
    match kind {
        DiskKind::Hdd => (50.0, 60.0),
        DiskKind::Ssd => (60.0, 70.0),
        DiskKind::Nvme => (70.0, 80.0),
        DiskKind::Unknown => (50.0, 60.0),
    }
}

/// rynas' verdict: the drive's own PASSED flag is not enough (drives "pass" with thousands of
/// pending sectors), so the watched counters weigh in too.
///
/// Reason codes and their [`ReasonArgs`]: `smart_failed`, `nvme_temperature`,
/// `nvme_reliability`, `nvme_read_only`, `nvme_backup_failed`, `wear_out` (none);
/// `attribute_failing`, `attribute_failed_past` (`name`: attribute and id);
/// `reallocated_sectors`, `pending_sectors`, `offline_uncorrectable`, `reported_uncorrect`,
/// `command_timeouts`, `crc_errors`, `load_cycles`, `media_errors`, `error_log` (`n`: count);
/// `old_disk` (`n`: years); `temperature` (`n`: °C); `wear` (`n`: % left); `nvme_spare_low`
/// (`n`: spare %, `limit`: threshold %); `self_test_failed` (`name`: kind, `detail`: status).
pub fn verdict(info: &SmartInfo, kind: DiskKind) -> (HealthVerdict, Vec<HealthReason>) {
    use Severity::*;
    let none = ReasonArgs::default;
    let mut r = Vec::new();
    if info.passed == Some(false) {
        r.push(reason(
            Critical,
            "smart_failed",
            "the drive reports SMART overall-health FAILED".into(),
            none(),
        ));
    }
    let raw = |id: u8| info.attributes.iter().find(|a| a.id == id).map(|a| a.raw);
    for a in &info.attributes {
        let name = || ReasonArgs {
            name: Some(format!("{} ({})", a.name, a.id)),
            ..Default::default()
        };
        match a.when_failed.as_str() {
            "now" => r.push(reason(
                Critical,
                "attribute_failing",
                format!("{} ({}) is below its failure threshold", a.name, a.id),
                name(),
            )),
            "past" if a.prefailure => r.push(reason(
                Warning,
                "attribute_failed_past",
                format!(
                    "{} ({}) was below its failure threshold in the past",
                    a.name, a.id
                ),
                name(),
            )),
            _ => {}
        }
    }
    if let Some(n) = raw(5).filter(|&n| n > 0) {
        let sev = if n >= 100 { Critical } else { Warning };
        r.push(reason(
            sev,
            "reallocated_sectors",
            format!("{n} reallocated sectors"),
            n_arg(n as f64),
        ));
    }
    if let Some(n) = raw(197).filter(|&n| n > 0) {
        let sev = if n >= 10 { Critical } else { Warning };
        r.push(reason(
            sev,
            "pending_sectors",
            format!("{n} sectors pending reallocation (unreadable)"),
            n_arg(n as f64),
        ));
    }
    if let Some(n) = raw(198).filter(|&n| n > 0) {
        r.push(reason(
            Warning,
            "offline_uncorrectable",
            format!("{n} offline uncorrectable sectors"),
            n_arg(n as f64),
        ));
    }
    if let Some(n) = raw(187).filter(|&n| n > 0) {
        r.push(reason(
            Warning,
            "reported_uncorrect",
            format!("{n} errors that ECC could not correct"),
            n_arg(n as f64),
        ));
    }
    // Seagate packs three 16-bit counters in 188: all timeouts (the low word), then those
    // queued over 5 s and over 7.5 s, which the first already counts (smartmontools #551).
    // Other vendors keep a plain count below 2^16, so the low word is the count for all.
    if let Some(n) = raw(188).map(|n| n & 0xffff)
        && n > 0
    {
        r.push(reason(
            Warning,
            "command_timeouts",
            format!("{n} command timeouts (often power or cable)"),
            n_arg(n as f64),
        ));
    }
    if let Some(n) = raw(199).filter(|&n| n > 0) {
        r.push(reason(
            Info,
            "crc_errors",
            format!("{n} interface CRC errors: check the cable or the bay connector"),
            n_arg(n as f64),
        ));
    }
    if kind == DiskKind::Hdd
        && let Some(n) = raw(193).filter(|&n| n > 300_000)
    {
        let sev = if n > 600_000 { Warning } else { Info };
        r.push(reason(
            sev,
            "load_cycles",
            format!("{n} head load cycles (rated 300-600k): aggressive head parking"),
            n_arg(n as f64),
        ));
    }
    if let Some(h) = info.power_on_hours.filter(|&h| h > 6 * 8766) {
        let years = (h as f64 / 8766.0 * 10.0).round() / 10.0;
        r.push(reason(
            Info,
            "old_disk",
            format!("{years:.1} years powered on"),
            n_arg(years),
        ));
    }
    if let Some(t) = info.temperature_c {
        let (warn, crit) = temp_limits(kind);
        let sev = if t >= crit {
            Some(Critical)
        } else if t >= warn {
            Some(Warning)
        } else {
            None
        };
        if let Some(sev) = sev {
            r.push(reason(
                sev,
                "temperature",
                format!("{t:.0} °C"),
                n_arg(t.round() as f64),
            ));
        }
    }
    if let Some(n) = &info.nvme {
        let cw = n.critical_warning;
        if cw & 0b0000_0001 != 0 || n.available_spare_pct <= n.available_spare_threshold_pct {
            r.push(reason(
                Critical,
                "nvme_spare_low",
                format!(
                    "available spare {}% (threshold {}%)",
                    n.available_spare_pct, n.available_spare_threshold_pct
                ),
                ReasonArgs {
                    n: Some(n.available_spare_pct as f64),
                    limit: Some(n.available_spare_threshold_pct as f64),
                    ..Default::default()
                },
            ));
        }
        if cw & 0b0000_0010 != 0 {
            r.push(reason(
                Warning,
                "nvme_temperature",
                "temperature out of range".into(),
                none(),
            ));
        }
        if cw & 0b0000_0100 != 0 {
            r.push(reason(
                Critical,
                "nvme_reliability",
                "reliability degraded by media or internal errors".into(),
                none(),
            ));
        }
        if cw & 0b0000_1000 != 0 {
            r.push(reason(
                Critical,
                "nvme_read_only",
                "media is read-only".into(),
                none(),
            ));
        }
        if cw & 0b0001_0000 != 0 {
            r.push(reason(
                Warning,
                "nvme_backup_failed",
                "volatile memory backup device failed".into(),
                none(),
            ));
        }
        if n.media_errors > 0 {
            r.push(reason(
                Warning,
                "media_errors",
                format!("{} media and data integrity errors", n.media_errors),
                n_arg(n.media_errors as f64),
            ));
        }
    }
    if let Some(life) = info.life_remaining_pct {
        if life == 0 {
            r.push(reason(
                Critical,
                "wear_out",
                "rated endurance used up (100% or more)".into(),
                none(),
            ));
        } else if life <= 20 {
            r.push(reason(
                Warning,
                "wear",
                format!("{life}% of rated endurance left"),
                n_arg(life as f64),
            ));
        }
    }
    // the newest completed self-test decides: older failures may have been fixed
    if let Some(t) = info.self_tests.first()
        && t.passed == Some(false)
    {
        r.push(reason(
            Critical,
            "self_test_failed",
            format!("last self-test ({}): {}", t.kind, t.status),
            ReasonArgs {
                name: Some(t.kind.clone()),
                detail: Some(t.status.clone()),
                ..Default::default()
            },
        ));
    }
    if let Some(n) = info.error_log_count.filter(|&n| n > 0)
        && info.nvme.is_none()
    {
        r.push(reason(
            Info,
            "error_log",
            format!("{n} errors in the drive's error log"),
            n_arg(n as f64),
        ));
    }
    (verdict_of(&r), r)
}

/// The verdict of a list of reasons: the worst severity among those not marked as seen.
pub fn verdict_of(reasons: &[HealthReason]) -> HealthVerdict {
    let worst = reasons
        .iter()
        .filter(|x| x.acknowledged.is_none())
        .map(|x| x.severity)
        .max();
    match worst {
        Some(Severity::Critical) => HealthVerdict::Failing,
        Some(Severity::Warning) => HealthVerdict::Warning,
        _ => HealthVerdict::Healthy,
    }
}

/// Shingled (SMR) drives that are sold without saying so. Device-managed SMR stalls on RAID
/// rebuilds; host-managed/aware zoned drives are detected from sysfs instead.
pub fn known_smr(model: &str) -> bool {
    const PREFIXES: &[&str] = &[
        // WD Red (not Plus/Pro) 2-6 TB EFAX
        "WDC WD20EFAX",
        "WDC WD30EFAX",
        "WDC WD40EFAX",
        "WDC WD60EFAX",
        // WD Blue
        "WDC WD20EZAZ",
        "WDC WD40EZAZ",
        "WDC WD60EZAZ",
        // Seagate Barracuda / Desktop
        "ST2000DM005",
        "ST2000DM008",
        "ST3000DM007",
        "ST4000DM004",
        "ST4000DM005",
        "ST6000DM003",
        "ST8000DM004",
        "ST5000LM000",
        "ST4000LM024",
        "ST2000LM015",
        // Seagate Archive
        "ST8000AS0002",
        "ST8000AS0003",
        // Toshiba P300 / DT02
        "TOSHIBA HDWD240",
        "TOSHIBA HDWD260",
        "TOSHIBA DT02ABA",
    ];
    let m = model.trim();
    PREFIXES.iter().any(|p| m.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(json: &str) -> SmartData {
        match parse(json, 1) {
            SmartRead::Data(d) => *d,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sata_hdd() {
        let d = data(include_str!("../tests/fixtures/smartctl-sata-hdd.json"));
        let i = &d.info;
        assert_eq!(i.protocol, "ATA");
        assert_eq!(i.smartctl_version, "7.4");
        assert_eq!(i.passed, Some(true));
        assert_eq!(i.temperature_c, Some(36.0));
        assert_eq!(i.power_on_hours, Some(31_337));
        assert_eq!(d.rotation_rpm, Some(5400));
        assert_eq!(d.apm, Some(Some(164)));
        assert_eq!(d.write_cache, Some(true));
        let a199 = i.attributes.iter().find(|a| a.id == 199).unwrap();
        assert_eq!(a199.raw, 3);
        assert!(a199.watched);
        let a194 = i.attributes.iter().find(|a| a.id == 194).unwrap();
        assert_eq!(a194.raw_string, "36 (Min/Max 19/47)");
        assert_eq!(i.self_tests.len(), 3);
        assert_eq!(i.self_tests[0].kind, "Short offline");
        assert_eq!(i.self_tests[0].passed, Some(true));
        assert_eq!(i.extended_test_minutes, Some(1003));
        assert_eq!(i.error_log_count, Some(0));
        assert_eq!(i.life_remaining_pct, None);
        let (v, r) = verdict(i, DiskKind::Hdd);
        // CRC errors only: informational
        assert_eq!(v, HealthVerdict::Healthy, "{r:?}");
        assert_eq!(r[0].code, "crc_errors");
    }

    #[test]
    fn sata_hdd_failing() {
        let d = data(include_str!(
            "../tests/fixtures/smartctl-sata-hdd-failing.json"
        ));
        let (v, r) = verdict(&d.info, DiskKind::Hdd);
        assert_eq!(v, HealthVerdict::Failing);
        let codes: Vec<_> = r.iter().map(|x| x.code.as_str()).collect();
        assert!(codes.contains(&"reallocated_sectors"), "{codes:?}");
        assert!(codes.contains(&"pending_sectors"));
        assert!(codes.contains(&"offline_uncorrectable"));
        assert!(codes.contains(&"self_test_failed"));
        assert!(codes.contains(&"load_cycles"));
        // the values of the messages travel as arguments, for the translated text
        let by = |c: &str| r.iter().find(|x| x.code == c).unwrap();
        let re = by("reallocated_sectors");
        assert!(re.args.n.is_some_and(|n| re.message.starts_with(&format!("{n} "))), "{re:?}");
        let st = by("self_test_failed");
        assert!(st.args.name.is_some() && st.args.detail.is_some(), "{st:?}");
        assert_eq!(d.info.self_tests[0].lba_first_error, Some(1_953_525_000));
        assert_eq!(d.info.exit_status, 0xc0);
    }

    #[test]
    fn sata_ssd() {
        let d = data(include_str!("../tests/fixtures/smartctl-sata-ssd.json"));
        let i = &d.info;
        assert_eq!(i.life_remaining_pct, Some(97));
        // 246 Total_LBAs_Written × 512
        assert_eq!(i.bytes_written, Some(41_250_121_744 * 512));
        assert_eq!(d.trim_supported, Some(true));
        assert_eq!(d.rotation_rpm, None);
        assert_eq!(d.apm, Some(None));
        let (v, _) = verdict(i, DiskKind::Ssd);
        assert_eq!(v, HealthVerdict::Healthy);
    }

    #[test]
    fn nvme() {
        let d = data(include_str!("../tests/fixtures/smartctl-nvme.json"));
        let i = &d.info;
        assert_eq!(i.protocol, "NVMe");
        let n = i.nvme.as_ref().unwrap();
        assert_eq!(n.percentage_used, 3);
        assert_eq!(n.available_spare_pct, 100);
        assert_eq!(n.data_units_written, 54_123_456);
        // 54_123_456 × 512_000 bytes = 27.71 TB
        assert_eq!(n.tb_written, 27.71);
        assert_eq!(i.life_remaining_pct, Some(97));
        assert_eq!(i.temperature_c, Some(41.0));
        assert_eq!(i.self_tests.len(), 2);
        assert_eq!(i.self_tests[0].kind, "Short");
        assert_eq!(i.self_tests[0].passed, Some(true));
        assert_eq!(d.write_cache, None);
        let (v, r) = verdict(i, DiskKind::Nvme);
        assert_eq!(v, HealthVerdict::Healthy, "{r:?}");
    }

    #[test]
    fn nvme_critical_warning() {
        let j = include_str!("../tests/fixtures/smartctl-nvme.json")
            .replace("\"critical_warning\":0", "\"critical_warning\":4")
            .replace("\"media_errors\":0", "\"media_errors\":12");
        let d = data(&j);
        let (v, r) = verdict(&d.info, DiskKind::Nvme);
        assert_eq!(v, HealthVerdict::Failing);
        assert!(r.iter().any(|x| x.code == "nvme_reliability"));
        assert!(r.iter().any(|x| x.code == "media_errors"));
    }

    #[test]
    fn standby_is_left_alone() {
        let r = parse(include_str!("../tests/fixtures/smartctl-standby.json"), 1);
        assert_eq!(r, SmartRead::Asleep(PowerState::Standby));
        let s = parse_settings(include_str!("../tests/fixtures/smartctl-standby.json"));
        assert_eq!(s, SettingsRead::Skipped);
    }

    #[test]
    fn devices_without_smart() {
        // a flash module behind a bridge smartctl does not know
        let bridge = r#"{"smartctl":{"version":[7,4],"exit_status":1,"messages":[{"string":"/dev/sdc: Unknown USB bridge [0x1005:0xb155 (0x100)]","severity":"error"},{"string":"Please specify device type with the -d option.","severity":"error"}]}}"#;
        assert!(matches!(parse(bridge, 1), SmartRead::Unsupported(m) if m.contains("USB bridge")));
        // opened, but no SMART in the device
        let none = r#"{"smartctl":{"version":[7,4],"exit_status":4},"device":{"protocol":"SCSI"},"smart_support":{"available":false}}"#;
        assert!(matches!(parse(none, 1), SmartRead::Unsupported(_)));
        // other failures stay failures (the next view tries again)
        let denied = r#"{"smartctl":{"exit_status":2,"messages":[{"string":"Smartctl open device: /dev/sda failed: Permission denied"}]}}"#;
        assert!(matches!(parse(denied, 1), SmartRead::Failed(_)));
    }

    #[test]
    fn settings_reread() {
        let j = r#"{"smartctl":{"exit_status":0},"ata_apm":{"enabled":true,"level":127},"write_cache":{"enabled":true}}"#;
        assert_eq!(
            parse_settings(j),
            SettingsRead::Data { apm: Some(Some(127)), write_cache: Some(true) }
        );
        let off = r#"{"smartctl":{"exit_status":0},"ata_apm":{"enabled":false}}"#;
        assert_eq!(parse_settings(off), SettingsRead::Data { apm: Some(None), write_cache: None });
        assert_eq!(parse_settings("not json"), SettingsRead::Skipped);
    }

    #[test]
    fn permission_denied() {
        let j = r#"{"json_format_version":[1,0],"smartctl":{"version":[7,4],"messages":[{"string":"Smartctl open device: /dev/sda failed: Permission denied","severity":"error"}],"exit_status":2}}"#;
        assert!(matches!(parse(j, 1), SmartRead::Failed(m) if m.contains("Permission denied")));
    }

    #[test]
    fn smr_models() {
        assert!(known_smr("WDC WD40EFAX-68JH4N1"));
        assert!(known_smr("ST2000DM008-2FR102"));
        assert!(!known_smr("WDC WD80EFZZ-68BTXN0"));
        assert!(!known_smr("WDC WD40EFZX-68AWUN0"));
    }

    /// Seagate's 188 as on an ST4000DM000 (`smartctl -v 188,raw16` shows "2 2 131"): 131
    /// timeouts in all, 2 of them over 5 s and over 7.5 s, which are among the 131.
    #[test]
    fn seagate_command_timeouts() {
        let raw: u64 = (2 << 32) | (2 << 16) | 131;
        let j = format!(
            r#"{{"smartctl":{{"exit_status":0}},"device":{{"protocol":"ATA"}},"ata_smart_attributes":{{"table":[{{"id":188,"name":"Command_Timeout","value":100,"worst":88,"thresh":0,"raw":{{"value":{raw},"string":"2 2 131"}}}}]}}}}"#
        );
        let (_, r) = verdict(&data(&j).info, DiskKind::Hdd);
        let t = r.iter().find(|x| x.code == "command_timeouts").expect("a timeouts reason");
        assert!(t.message.starts_with("131 "), "{}", t.message);
    }
}
