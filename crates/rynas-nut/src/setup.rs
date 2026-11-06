//! The UPS setup wizard's pure part: candidates, test-run parsing, `ups.conf` sections.
//!
//! - **Candidates** for a UPS found on USB ([`candidates`]): rynas's small table of USB IDs
//!   known to need a specific driver or options ([`KNOWN`]) first, then `nut-scanner`'s
//!   suggestion, then generic drivers. `nut-scanner` matches USB IDs only and is sometimes
//!   wrong (`0925:1234` is suggested `richcomm_usb`, but the UPS behind that ID often speaks
//!   megatec through `nutdrv_qx` with the `armac` subdriver), so none is trusted until a test
//!   run reads the UPS status.
//! - **Test run** ([`driver_probe_args`], [`parse_dump`], [`probe_of`]): the driver started
//!   on its own with `-s <id>` and `-x` options (no `ups.conf`), `-d 1` to print the data tree
//!   once in upsc format and exit (NUT ≥ 2.8.0). It works when `ups.status` is reported.
//! - **Values** are limited to characters that need no quoting anywhere (`ups.conf`, argv,
//!   synodeb's `--opt NAME=VALUE`): [`check_device`].
//! - **`ups.conf`** sections of the admin's ([`sections`]) and which of them are in the way
//!   of the UPS being set up ([`mark_conflicts`]); the rynas section ([`render_section`]).

use rynas_api_types::ups::{
    NutDriver, UpsCandidate, UpsCandidateSource, UpsDevice, UpsDeviceConfig, UpsOption, UpsProbe,
    UpsProbeState, UpsSection, UpsUsbDevice,
};

/// Test-run id of a driver started with `-s` (its pid file and socket in NUT's state dir).
pub const PROBE_ID: &str = "rynasprobe";
/// At most this many driver options (synodeb's limit too).
pub const MAX_OPTIONS: usize = 32;
/// Options set by the wizard's own fields, never as extra options.
pub const RESERVED_OPTIONS: &[&str] = &["driver", "port", "desc"];
/// Lines of driver output kept for a failed test run.
pub const LOG_TAIL: usize = 20;

// ---------------------------------------------------------------------------------------------
// validation
// ---------------------------------------------------------------------------------------------

fn value_chars(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._:/@+,=-".contains(&b))
}

/// A driver program name: `[a-z0-9_-]`, 1–32 characters.
pub fn valid_driver(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// A port: `auto`, a device path or an address; 1–128 characters of `[A-Za-z0-9._:/@+,=-]`.
pub fn valid_port(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && value_chars(s)
}

/// An option name: `[a-z][a-z0-9_.]*`, at most 32 characters, not a reserved one.
pub fn valid_option_key(s: &str) -> bool {
    s.len() <= 32
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        && !RESERVED_OPTIONS.contains(&s)
}

/// An option value: 1–128 characters of `[A-Za-z0-9._:/@+,=-]` (no spaces or quotes).
pub fn valid_option_value(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && value_chars(s)
}

/// A NUT name for the UPS: `[a-z0-9_-]`, 1–32 characters.
pub fn valid_ups_name(s: &str) -> bool {
    valid_driver(s)
}

/// Checks a device; the error is `(field, message)` with the field as in the JSON body.
pub fn check_device(d: &UpsDevice) -> Result<(), (String, String)> {
    let bad = |f: &str, m: &str| Err((f.to_string(), m.to_string()));
    if !valid_driver(&d.driver) {
        return bad(
            "device.driver",
            "a driver name: a-z, 0-9, _ and -, at most 32",
        );
    }
    if !valid_port(&d.port) {
        return bad(
            "device.port",
            "auto, a device path or an address (no spaces or quotes)",
        );
    }
    if d.options.len() > MAX_OPTIONS {
        return bad("device.options", "at most 32 options");
    }
    for (i, o) in d.options.iter().enumerate() {
        if !valid_option_key(&o.key) {
            return Err((
                format!("device.options.{i}.key"),
                "an option name: a-z first, then a-z, 0-9, _ and . (not driver, port, desc)".into(),
            ));
        }
        if d.options[..i].iter().any(|p| p.key == o.key) {
            return Err((
                format!("device.options.{i}.key"),
                "option given twice".into(),
            ));
        }
        if let Some(v) = &o.value
            && !valid_option_value(v)
        {
            return Err((
                format!("device.options.{i}.value"),
                "letters, digits and ._:/@+,=- only (no spaces or quotes)".into(),
            ));
        }
    }
    Ok(())
}

/// Checks a whole setting (see [`check_device`]).
pub fn check_config(c: &UpsDeviceConfig) -> Result<(), (String, String)> {
    if !valid_ups_name(&c.name) {
        return Err((
            "name".into(),
            "a NUT name: a-z, 0-9, _ and -, at most 32".into(),
        ));
    }
    if let Some(t) = c.take_over.iter().find(|t| !valid_ups_name(t)) {
        return Err(("take_over".into(), format!("not a section name: {t}")));
    }
    check_device(&c.device)
}

// ---------------------------------------------------------------------------------------------
// command lines
// ---------------------------------------------------------------------------------------------

fn option_arg(o: &UpsOption) -> String {
    match &o.value {
        Some(v) => format!("{}={v}", o.key),
        None => o.key.clone(),
    }
}

/// Arguments of a driver test run: `-s rynasprobe -x port=P -x k=v … -d 1`.
pub fn driver_probe_args(d: &UpsDevice) -> Vec<String> {
    let mut a = vec![
        "-s".into(),
        PROBE_ID.into(),
        "-x".into(),
        format!("port={}", d.port),
    ];
    for o in &d.options {
        a.push("-x".into());
        a.push(option_arg(o));
    }
    a.extend(["-d".into(), "1".into()]);
    a
}

/// synodeb's driver arguments: `--driver D --port P --opt k=v …` (setup and probe).
pub fn synodeb_args(d: &UpsDevice) -> Vec<String> {
    let mut a = vec![
        "--driver".into(),
        d.driver.clone(),
        "--port".into(),
        d.port.clone(),
    ];
    for o in &d.options {
        a.push("--opt".into());
        a.push(option_arg(o));
    }
    a
}

// ---------------------------------------------------------------------------------------------
// parsing
// ---------------------------------------------------------------------------------------------

/// `(variable, value)` pairs of upsc-format output (`battery.charge: 100`); the driver's
/// banner and log lines are skipped.
pub fn parse_dump(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(": ")?;
            let k = k.trim();
            let var = !k.is_empty()
                && k.contains('.')
                && k.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_'
                });
            var.then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Outcome of a test run from its variables and output.
pub fn probe_of(vars: &[(String, String)], output: &str, duration_ms: u64) -> UpsProbe {
    let get = |k: &str| {
        vars.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let status = get("ups.status");
    let state = if status.is_some() {
        UpsProbeState::Ok
    } else {
        UpsProbeState::Failed
    };
    UpsProbe {
        state,
        battery_charge_pct: get("battery.charge")
            .and_then(|v| v.parse::<f32>().ok())
            .map(|v| v.clamp(0.0, 100.0).round() as u32),
        manufacturer: get("device.mfr").or_else(|| get("ups.mfr")),
        model: get("device.model").or_else(|| get("ups.model")),
        status,
        log_tail: if state == UpsProbeState::Ok {
            Vec::new()
        } else {
            tail(output)
        },
        duration_ms,
    }
}

/// The last [`LOG_TAIL`] non-empty lines.
pub fn tail(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(LOG_TAIL)..]
        .iter()
        .map(|l| l.chars().take(300).collect())
        .collect()
}

/// A NUT version (`2.8.1`) found in a `-V` banner
/// ("Network UPS Tools - UPS driver controller 2.8.1").
pub fn nut_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split(|c: char| c.is_whitespace() || c == '-' || c == '+' || c == '~')
        .find_map(parse_version)
}

/// `2.8.5` → `(2, 8, 5)`; `2.8` → `(2, 8, 0)`.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim_start_matches('v');
    let mut it = s.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it
        .next()
        .map(|c| {
            c.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Some((a, b, c))
}

pub fn fmt_version((a, b, c): (u32, u32, u32)) -> String {
    format!("{a}.{b}.{c}")
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(v)
        .to_string()
}

/// Lines of an ini-like NUT file as `Some((section, key, value))` / section headers.
enum Line {
    Section(String),
    Pair(String, String),
}

fn ini_lines(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut in_rynas = false;
    for raw in text.lines() {
        if raw.starts_with("# BEGIN rynas ") {
            in_rynas = true;
            continue;
        }
        if in_rynas {
            if raw.starts_with("# END rynas ") {
                in_rynas = false;
            }
            continue;
        }
        let l = raw.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some(n) = l.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push(Line::Section(n.trim().to_string()));
        } else if let Some((k, v)) = l.split_once('=') {
            out.push(Line::Pair(k.trim().to_string(), unquote(v)));
        } else {
            out.push(Line::Pair(l.to_string(), String::new()));
        }
    }
    out
}

/// USB UPSes of `nut-scanner -U` output (`[nutdev1]` sections with `driver`, `port`,
/// `vendorid`, `productid`, `vendor`, `product`).
pub fn parse_scanner(text: &str) -> Vec<UpsUsbDevice> {
    let mut out: Vec<UpsUsbDevice> = Vec::new();
    let mut cur: Option<UpsUsbDevice> = None;
    for l in ini_lines(text) {
        match l {
            Line::Section(_) => {
                if let Some(d) = cur.take() {
                    out.push(d);
                }
                cur = Some(UpsUsbDevice::default());
            }
            Line::Pair(k, v) => {
                let Some(d) = cur.as_mut() else { continue };
                let v = (!v.is_empty()).then_some(v);
                match k.as_str() {
                    "driver" => d.driver = v.filter(|v| valid_driver(v)),
                    "vendorid" => d.vendor_id = v.unwrap_or_default().to_ascii_lowercase(),
                    "productid" => d.product_id = v.unwrap_or_default().to_ascii_lowercase(),
                    "vendor" => d.vendor = v,
                    "product" => d.product = v,
                    _ => {}
                }
            }
        }
    }
    out.extend(cur);
    out.retain(|d| is_usb_id(&d.vendor_id) && is_usb_id(&d.product_id));
    out.dedup_by(|a, b| a.vendor_id == b.vendor_id && a.product_id == b.product_id);
    out
}

fn is_usb_id(s: &str) -> bool {
    s.len() == 4 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A section of the admin's `ups.conf` (outside rynas's blocks).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConfSection {
    pub name: String,
    pub driver: Option<String>,
    pub port: Option<String>,
    pub vendorid: Option<String>,
    pub productid: Option<String>,
}

/// Sections of `ups.conf` outside rynas's blocks.
pub fn sections(text: &str) -> Vec<ConfSection> {
    let mut out: Vec<ConfSection> = Vec::new();
    for l in ini_lines(text) {
        match l {
            Line::Section(n) => out.push(ConfSection {
                name: n,
                ..Default::default()
            }),
            Line::Pair(k, v) => {
                let Some(s) = out.last_mut() else { continue };
                let v = Some(v.to_ascii_lowercase()).filter(|v| !v.is_empty());
                match k.as_str() {
                    "driver" => s.driver = v,
                    "port" => s.port = v,
                    "vendorid" => s.vendorid = v,
                    "productid" => s.productid = v,
                    _ => {}
                }
            }
        }
    }
    out
}

/// Whether section `s` is in the way of `c`: the same name, or both on USB (`port = auto`)
/// without different USB IDs that tell them apart (two drivers cannot share a device).
pub fn in_the_way(s: &ConfSection, c: &UpsDeviceConfig) -> bool {
    if s.name == c.name {
        return true;
    }
    let usb = |p: Option<&str>| p.is_some_and(|p| p.eq_ignore_ascii_case("auto"));
    if !(usb(s.port.as_deref()) && usb(Some(&c.device.port))) {
        return false;
    }
    let opt = |k: &str| {
        c.device
            .option(k)
            .and_then(|o| o.value.as_deref())
            .map(str::to_ascii_lowercase)
    };
    let differ =
        |a: Option<String>, b: Option<String>| matches!((a, b), (Some(a), Some(b)) if a != b);
    !(differ(s.vendorid.clone(), opt("vendorid")) || differ(s.productid.clone(), opt("productid")))
}

/// The admin's sections for the API, `conflict` against `c` when given.
pub fn mark_conflicts(secs: &[ConfSection], c: Option<&UpsDeviceConfig>) -> Vec<UpsSection> {
    secs.iter()
        .map(|s| UpsSection {
            name: s.name.clone(),
            driver: s.driver.clone(),
            port: s.port.clone(),
            conflict: c.is_some_and(|c| in_the_way(s, c)),
        })
        .collect()
}

/// Names of the sections in the way of `c` that it does not take over.
pub fn blocking_sections(secs: &[ConfSection], c: &UpsDeviceConfig) -> Vec<String> {
    secs.iter()
        .filter(|s| in_the_way(s, c) && !c.take_over.contains(&s.name))
        .map(|s| s.name.clone())
        .collect()
}

/// rynas's `ups.conf` section.
pub fn render_section(c: &UpsDeviceConfig) -> String {
    let mut s = format!(
        "# The UPS set up with the rynas UPS wizard (ups.device).\n[{}]\n\tdriver = \"{}\"\n\tport = \"{}\"\n",
        c.name, c.device.driver, c.device.port
    );
    for o in &c.device.options {
        match &o.value {
            Some(v) => s.push_str(&format!("\t{} = \"{v}\"\n", o.key)),
            None => s.push_str(&format!("\t{}\n", o.key)),
        }
    }
    s
}

/// The device of rynas's `ups.conf` block (`import`), with the section name.
pub fn parse_section_block(block: &str) -> Option<(String, UpsDevice)> {
    let mut name = None;
    let mut driver = None;
    let mut port = None;
    let mut options = Vec::new();
    for raw in block.lines() {
        let l = raw.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if let Some(n) = l.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if name.is_some() {
                break;
            }
            name = Some(n.trim().to_string());
            continue;
        }
        let (k, v) = match l.split_once('=') {
            Some((k, v)) => (k.trim().to_string(), Some(unquote(v))),
            None => (l.to_string(), None),
        };
        match k.as_str() {
            "driver" => driver = v,
            "port" => port = v,
            "desc" => {}
            _ => options.push(UpsOption { key: k, value: v }),
        }
    }
    Some((
        name?,
        UpsDevice {
            driver: driver?,
            port: port.unwrap_or_else(|| "auto".into()),
            options,
        },
    ))
}

/// The text between rynas's `# BEGIN rynas <id>` and `# END rynas <id>` markers.
pub fn block_of(text: &str, id: &str) -> Option<String> {
    let begin = format!("# BEGIN rynas {id} ");
    let end = format!("# END rynas {id}");
    let mut body = String::new();
    let mut inside = false;
    for l in text.lines() {
        if inside {
            if l.trim_end() == end {
                return Some(body);
            }
            body.push_str(l);
            body.push('\n');
        } else if l.starts_with(&begin) {
            inside = true;
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// candidates
// ---------------------------------------------------------------------------------------------

/// A USB ID known to need a specific driver setting.
pub struct Known {
    pub vendor_id: &'static str,
    pub product_id: &'static str,
    pub driver: &'static str,
    pub options: &'static [(&'static str, Option<&'static str>)],
    /// Lowest NUT version where the setting works.
    pub min_nut: Option<(u32, u32, u32)>,
}

/// rynas's table, tried before `nut-scanner`'s suggestion. Sources in
/// `docs/research/ups.md` ("Driver wizard").
pub const KNOWN: &[Known] = &[
    // "Lakeview Research UPS USB Mon V2.0" (VULTECH UPS1050VA-PRO, Armac, Accurat Flux):
    // megatec over the Richcomm chip; nut-scanner suggests richcomm_usb. NUT's hardware
    // list (issue 2738, PR 3323), working from NUT 2.8.5.
    Known {
        vendor_id: "0925",
        product_id: "1234",
        driver: "nutdrv_qx",
        options: &[
            ("subdriver", Some("armac")),
            ("protocol", Some("megatec")),
            ("norating", None),
            ("pollfreq", Some("30")),
        ],
        min_nut: Some((2, 8, 5)),
    },
    // Cypress-based megatec UPSes (many small brands)
    Known {
        vendor_id: "0665",
        product_id: "5161",
        driver: "nutdrv_qx",
        options: &[],
        min_nut: None,
    },
];

/// Generic drivers tried for any USB UPS, in this order.
pub const GENERIC_USB: &[&str] = &["usbhid-ups", "nutdrv_qx", "blazer_usb"];

fn usb_device(driver: &str, u: &UpsUsbDevice, extra: &[(&str, Option<&str>)]) -> UpsDevice {
    let mut options = vec![
        UpsOption::new("vendorid", Some(&u.vendor_id)),
        UpsOption::new("productid", Some(&u.product_id)),
    ];
    options.extend(extra.iter().map(|(k, v)| UpsOption::new(k, *v)));
    UpsDevice {
        driver: driver.into(),
        port: "auto".into(),
        options,
    }
}

/// Candidates for the USB UPSes found, in the order they are tried; duplicates (same driver
/// and options) only once.
pub fn candidates(usb: &[UpsUsbDevice]) -> Vec<UpsCandidate> {
    let mut out: Vec<UpsCandidate> = Vec::new();
    let mut push = |device: UpsDevice, source, min_nut: Option<(u32, u32, u32)>| {
        if !out.iter().any(|c| c.device == device) {
            out.push(UpsCandidate {
                device,
                source,
                min_nut: min_nut.map(fmt_version),
                probe: UpsProbe::default(),
            });
        }
    };
    for u in usb {
        for k in KNOWN
            .iter()
            .filter(|k| k.vendor_id == u.vendor_id && k.product_id == u.product_id)
        {
            push(
                usb_device(k.driver, u, k.options),
                UpsCandidateSource::Known,
                k.min_nut,
            );
        }
        if let Some(d) = &u.driver {
            push(usb_device(d, u, &[]), UpsCandidateSource::Scanner, None);
        }
        for g in GENERIC_USB {
            push(usb_device(g, u, &[]), UpsCandidateSource::Generic, None);
        }
    }
    out
}

/// Drivers with the manufacturers NUT's hardware list (`driver.list`) names for them; only
/// `installed` drivers, sorted by name.
pub fn drivers_with_hcl(installed: &[String], driver_list: &str) -> Vec<NutDriver> {
    let mut out: Vec<NutDriver> = installed
        .iter()
        .map(|n| NutDriver {
            name: n.clone(),
            manufacturers: Vec::new(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    for line in driver_list.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let fields: Vec<String> = line
            .split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        let (Some(mfr), Some(drv)) = (fields.first(), fields.get(5)) else {
            continue;
        };
        for word in drv.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
            if let Some(d) = out.iter_mut().find(|d| d.name == word)
                && !d.manufacturers.contains(mfr)
            {
                d.manufacturers.push(mfr.clone());
            }
        }
    }
    for d in &mut out {
        d.manufacturers.sort_by_key(|m| m.to_ascii_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `nut-scanner -U` on Marco's VULTECH UPS1050VA-PRO.
    pub const SCAN_VULTECH: &str = "Scanning USB bus.\n[nutdev1]\n\tdriver = \"richcomm_usb\"\n\
        \tport = \"auto\"\n\tvendorid = \"0925\"\n\tproductid = \"1234\"\n\
        \tproduct = \"UPS USB Mon V2.0\"\n\tvendor = \"Lakeview Research\"\n\tbus = \"001\"\n";

    #[test]
    fn vultech_candidates_put_the_known_setting_first() {
        let usb = parse_scanner(SCAN_VULTECH);
        assert_eq!(usb.len(), 1);
        assert_eq!(usb[0].vendor_id, "0925");
        assert_eq!(usb[0].driver.as_deref(), Some("richcomm_usb"));
        assert_eq!(usb[0].vendor.as_deref(), Some("Lakeview Research"));
        let c = candidates(&usb);
        let drivers: Vec<&str> = c.iter().map(|c| c.device.driver.as_str()).collect();
        // nutdrv_qx armac first, the scanner's richcomm_usb next, then the generic ones;
        // nutdrv_qx without options is a different candidate from the armac one
        assert_eq!(
            drivers,
            [
                "nutdrv_qx",
                "richcomm_usb",
                "usbhid-ups",
                "nutdrv_qx",
                "blazer_usb"
            ]
        );
        assert_eq!(c[0].source, UpsCandidateSource::Known);
        assert_eq!(c[0].min_nut.as_deref(), Some("2.8.5"));
        assert_eq!(
            driver_probe_args(&c[0].device).join(" "),
            "-s rynasprobe -x port=auto -x vendorid=0925 -x productid=1234 -x subdriver=armac \
             -x protocol=megatec -x norating -x pollfreq=30 -d 1"
        );
        assert_eq!(
            synodeb_args(&c[0].device).join(" "),
            "--driver nutdrv_qx --port auto --opt vendorid=0925 --opt productid=1234 \
             --opt subdriver=armac --opt protocol=megatec --opt norating --opt pollfreq=30"
        );
        let cfg = UpsDeviceConfig {
            name: "ups".into(),
            device: c[0].device.clone(),
            take_over: vec![],
        };
        let sec = render_section(&cfg);
        assert!(sec.contains("[ups]\n\tdriver = \"nutdrv_qx\"\n\tport = \"auto\"\n"));
        assert!(sec.contains("\tvendorid = \"0925\"\n") && sec.contains("\tnorating\n"));
        assert_eq!(
            parse_section_block(&sec),
            Some(("ups".into(), cfg.device.clone()))
        );
        assert!(check_config(&cfg).is_ok());
    }

    #[test]
    fn dump_and_probe() {
        let out = "Network UPS Tools - Generic Q* USB/Serial driver 0.41 (2.8.5)\n\
            USB communication driver (libusb 1.0) 0.50\n\
            battery.charge: 100\nbattery.voltage: 13.70\ndevice.mfr: \nups.status: OL\n\
            driver.parameter.port: auto\n";
        let vars = parse_dump(out);
        assert!(vars.iter().any(|(k, v)| k == "ups.status" && v == "OL"));
        assert!(!vars.iter().any(|(k, _)| k.starts_with("USB")));
        let p = probe_of(&vars, out, 1200);
        assert_eq!(p.state, UpsProbeState::Ok);
        assert_eq!(p.battery_charge_pct, Some(100));
        assert_eq!(p.manufacturer, None);
        assert!(p.log_tail.is_empty());
        let fail = "Network UPS Tools - Generic Q* USB/Serial driver 0.32 (2.8.1)\nDevice not supported!\n";
        let p = probe_of(&parse_dump(fail), fail, 900);
        assert_eq!(p.state, UpsProbeState::Failed);
        assert_eq!(
            p.log_tail.last().map(String::as_str),
            Some("Device not supported!")
        );
    }

    #[test]
    fn versions() {
        assert_eq!(
            nut_version("Network UPS Tools - UPS driver controller 2.8.1"),
            Some((2, 8, 1))
        );
        assert_eq!(
            nut_version("Network UPS Tools upsd 2.8.5 release"),
            Some((2, 8, 5))
        );
        assert_eq!(
            nut_version("Network UPS Tools - UPS driver controller 2.8.0.1"),
            Some((2, 8, 0))
        );
        assert_eq!(nut_version("nothing here"), None);
        assert!((2, 8, 1) < (2, 8, 5));
    }

    #[test]
    fn validation() {
        let mut d = UpsDevice {
            driver: "snmp-ups".into(),
            port: "192.168.1.20".into(),
            options: vec![
                UpsOption::new("community", Some("public")),
                UpsOption::new("mibs", Some("auto")),
            ],
        };
        assert!(check_device(&d).is_ok());
        d.options.push(UpsOption::new("port", Some("x")));
        assert_eq!(check_device(&d).unwrap_err().0, "device.options.2.key");
        d.options.pop();
        d.options
            .push(UpsOption::new("community", Some("bad value")));
        assert_eq!(
            check_device(&d).unwrap_err().0,
            "device.options.2.key",
            "twice"
        );
        d.options.pop();
        d.options.push(UpsOption::new("x", Some("a\"b")));
        assert_eq!(check_device(&d).unwrap_err().0, "device.options.2.value");
        d.options.pop();
        d.port = "/dev/ttyS0 ; rm".into();
        assert_eq!(check_device(&d).unwrap_err().0, "device.port");
        d.port = "/dev/ttyUSB0".into();
        d.driver = "../bin/sh".into();
        assert_eq!(check_device(&d).unwrap_err().0, "device.driver");
    }

    #[test]
    fn sections_and_conflicts() {
        let conf = "maxretry = 3\n[myups]\n\tdriver = usbhid-ups\n\tport = auto\n\
            [snmp]\n\tdriver = snmp-ups\n\tport = 10.0.0.5\n\
            # BEGIN rynas ups-device — managed\n[ups]\n\tdriver = \"nutdrv_qx\"\n# END rynas ups-device\n";
        let secs = sections(conf);
        assert_eq!(
            secs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["myups", "snmp"],
            "rynas's block is not the admin's"
        );
        let mut cfg = UpsDeviceConfig {
            name: "ups".into(),
            device: UpsDevice {
                driver: "nutdrv_qx".into(),
                port: "auto".into(),
                options: vec![UpsOption::new("vendorid", Some("0925"))],
            },
            take_over: vec![],
        };
        // a USB section without IDs may be the same device
        assert_eq!(blocking_sections(&secs, &cfg), ["myups"]);
        cfg.take_over = vec!["myups".into()];
        assert!(blocking_sections(&secs, &cfg).is_empty());
        let marked = mark_conflicts(&secs, Some(&cfg));
        assert!(marked[0].conflict && !marked[1].conflict);
        // different USB IDs: two UPSes side by side
        let other = ConfSection {
            name: "b".into(),
            port: Some("auto".into()),
            vendorid: Some("051d".into()),
            ..Default::default()
        };
        assert!(!in_the_way(&other, &cfg));
        // same name is always in the way
        let same = ConfSection {
            name: "ups".into(),
            ..Default::default()
        };
        assert!(in_the_way(&same, &cfg));
    }

    #[test]
    fn hcl() {
        let list = "# comment \"x\"\n\"Armac\"\t\"ups\"\t\"2\"\t\"R/2000I\"\t\"(USB ID 0925:1234)\"\t\"nutdrv_qx\"\n\
            \"SVEN\"\t\"ups\"\t\"1\"\t\"Power Pro+\"\t\"USB\"\t\"richcomm_usb (USB ID 0925:1234)\"\n\
            \"APC\"\t\"ups\"\t\"3\"\t\"Back-UPS\"\t\"\"\t\"usbhid-ups or blazer_usb\"\n";
        let d = drivers_with_hcl(
            &[
                "usbhid-ups".into(),
                "nutdrv_qx".into(),
                "richcomm_usb".into(),
            ],
            list,
        );
        assert_eq!(
            d.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["nutdrv_qx", "richcomm_usb", "usbhid-ups"]
        );
        assert_eq!(d[0].manufacturers, ["Armac"]);
        assert_eq!(d[2].manufacturers, ["APC"]);
    }
}
