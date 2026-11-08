//! `/etc/synohwd.toml`: the keys of synohwd (`DEFAULTS`, `RANGES`, `CHOICES`,
//! `COMMAND_KEYS` in synohwd.py), the strict validation of the API, the renderer and a port of
//! synohwd's own lenient `validate_conf` (to read a file rynas did not write the way synohwd
//! reads it).

use rynas_api_types::synology::{
    FanMode, LED_SCHEDULE_LEN, PowerButtonAction, ResetButtonAction, SynologySettings,
    SynologyStatus, led_schedule_valid,
};

pub const CONF_PATH: &str = "/etc/synohwd.toml";

/// The reset-button handler of rynas (D-NET double press → network rescue), set through
/// synohwd's `command:` hook. `command:` values are reserved for rynas handlers.
pub const RESET_HANDLER: &str = "/usr/libexec/rynas/rynasctl net reset-button";

/// Integer keys with synohwd's ranges (`RANGES`).
pub const RANGES: [(&str, i64, i64); 6] = [
    ("fan_period", 5, 300),
    ("fan_min_duty", 0, 99),
    ("hysteresis", 0, 20),
    ("led_brightness", 1, 255),
    ("disk_standby_minutes", 0, 330),
    ("led_reduced_level", 0, 7),
];

/// String keys with their choices (`CHOICES`); the buttons also take `command:<shell>`.
pub const CHOICES: [(&str, &[&str]); 3] = [
    ("fan_mode", &["low", "high", "full"]),
    ("power_button", &["poweroff", "ignore", "log"]),
    ("reset_button", &["log", "ignore"]),
];

pub const COMMAND_KEYS: [&str; 2] = ["power_button", "reset_button"];

/// Every key of synohwd, in `DEFAULTS` order.
pub const KEYS: [&str; 16] = [
    "fan_mode",
    "fan_period",
    "fan_min_duty",
    "hysteresis",
    "thermal_shutdown",
    "power_button",
    "reset_button",
    "beep_on_start",
    "beep_on_error",
    "disk_leds",
    "led_brightness",
    "disk_standby_minutes",
    "wol",
    "ac_recovery",
    "led_schedule",
    "led_reduced_level",
];

fn range(key: &str) -> (i64, i64) {
    RANGES
        .iter()
        .find(|(k, ..)| *k == key)
        .map(|&(_, lo, hi)| (lo, hi))
        .expect("known range key")
}

/// Checks settings against synohwd's ranges: rynas refuses what synohwd would clamp.
pub fn validate(s: &SynologySettings) -> Result<(), String> {
    let check = |key: &str, field: &str, v: i64| {
        let (lo, hi) = range(key);
        if (lo..=hi).contains(&v) {
            Ok(())
        } else {
            Err(format!("{field} must be {lo} to {hi} (got {v})"))
        }
    };
    check("fan_period", "fan_period_s", s.fan_period_s as i64)?;
    check(
        "fan_min_duty",
        "fan_min_duty_pct",
        s.fan_min_duty_pct as i64,
    )?;
    check("hysteresis", "hysteresis_c", s.hysteresis_c as i64)?;
    check("led_brightness", "led_brightness", s.led_brightness as i64)?;
    check(
        "disk_standby_minutes",
        "disk_standby_minutes",
        s.disk_standby_minutes as i64,
    )?;
    check(
        "led_reduced_level",
        "led_reduced_level",
        s.led_reduced_level as i64,
    )?;
    if !led_schedule_valid(&s.led_schedule) {
        return Err(format!(
            "led_schedule must be empty or {LED_SCHEDULE_LEN} characters 0, 1 or 2 (got {} characters)",
            s.led_schedule.chars().count()
        ));
    }
    Ok(())
}

/// The front LED settings against what the running synohwd reports (`status`): the reduced
/// level within the model's scale, and no new schedule for a synohwd without the LED chip.
/// Nothing to check while synohwd is not running (the scale is unknown; synohwd clamps).
/// `applied` is the current value: a schedule or level already in the file is not refused again.
pub fn validate_led(
    s: &SynologySettings,
    applied: &SynologySettings,
    status: &SynologyStatus,
) -> Result<(), String> {
    if !status.running {
        return Ok(());
    }
    if !status.led_schedule_supported {
        if !s.led_schedule.is_empty() && s.led_schedule != applied.led_schedule {
            return Err(
                "this synohwd has no front LED brightness schedule (synodeb 4.8 or later on a model with the LED level chip)"
                    .into(),
            );
        }
        return Ok(());
    }
    // like the schedule: only a changed level is checked, so a value already in the file
    // (synohwd clamps it) does not block saving other settings
    if let (Some(lo), Some(hi)) = (status.led_level_min, status.led_level_max)
        && s.led_reduced_level != applied.led_reduced_level
        && !(lo..=hi).contains(&s.led_reduced_level)
    {
        return Err(format!(
            "led_reduced_level must be {lo} to {hi} on this model (got {})",
            s.led_reduced_level
        ));
    }
    Ok(())
}

fn power_value(a: PowerButtonAction) -> &'static str {
    match a {
        PowerButtonAction::Poweroff => "poweroff",
        PowerButtonAction::Log => "log",
        PowerButtonAction::Ignore => "ignore",
    }
}

fn reset_value(a: ResetButtonAction) -> String {
    match a {
        ResetButtonAction::Log => "log".into(),
        ResetButtonAction::Ignore => "ignore".into(),
        ResetButtonAction::NetworkRescue => format!("command:{RESET_HANDLER}"),
    }
}

/// TOML basic string.
fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if c.is_control() => o.push_str(&format!("\\u{:04X}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The file body (the managed header is added by the config engine). Every key is written,
/// in synohwd's order, so the file says exactly what synohwd runs with.
pub fn render(s: &SynologySettings) -> String {
    let mut o = String::from(
        "# synohwd configuration, written by rynas (Hardware > Synology).\n\
         # Change it from rynas: edits made here are reported as drift.\n",
    );
    let mut kv = |k: &str, v: String| o.push_str(&format!("{k} = {v}\n"));
    kv("fan_mode", quote(s.fan_mode.as_str()));
    kv("fan_period", s.fan_period_s.to_string());
    kv("fan_min_duty", s.fan_min_duty_pct.to_string());
    kv("hysteresis", s.hysteresis_c.to_string());
    kv("thermal_shutdown", s.thermal_shutdown.to_string());
    kv("power_button", quote(power_value(s.power_button)));
    kv("reset_button", quote(&reset_value(s.reset_button)));
    kv("beep_on_start", s.beep_on_start.to_string());
    kv("beep_on_error", s.beep_on_error.to_string());
    kv("disk_leds", s.disk_leds.to_string());
    kv("led_brightness", s.led_brightness.to_string());
    kv("disk_standby_minutes", s.disk_standby_minutes.to_string());
    kv("wol", s.wol.to_string());
    kv("ac_recovery", s.ac_recovery.to_string());
    kv("led_schedule", quote(&s.led_schedule));
    kv("led_reduced_level", s.led_reduced_level.to_string());
    o
}

/// A value of the TOML file, as synohwd's `validate_conf` sees its type.
#[derive(Debug, Clone, PartialEq)]
pub enum Raw {
    Bool(bool),
    Int(i64),
    Str(String),
    /// Floats, arrays, tables, dates: never the type synohwd expects.
    Other,
}

/// What synohwd runs with, from the raw keys of its file: a port of `validate_conf`
/// (wrong types → default, out-of-range integers clamped, unknown choices → default, unknown
/// keys ignored). Also returns the `command:` values rynas does not manage (they are replaced
/// by the default action in the result).
pub fn lenient(raw: &[(String, Raw)]) -> (SynologySettings, Vec<String>) {
    let mut s = SynologySettings::default();
    let mut foreign = Vec::new();
    for (k, v) in raw {
        let int = |v: &Raw| match v {
            Raw::Int(i) => {
                let (lo, hi) = range(k);
                Some((*i).clamp(lo, hi))
            }
            _ => None,
        };
        let boolean = |v: &Raw| match v {
            Raw::Bool(b) => Some(*b),
            _ => None,
        };
        let choice = |v: &Raw| -> Option<String> {
            let Raw::Str(s) = v else { return None };
            let allowed = CHOICES.iter().find(|(c, _)| c == k).map(|(_, a)| *a)?;
            let cmd = COMMAND_KEYS.contains(&k.as_str())
                && s.strip_prefix("command:")
                    .is_some_and(|c| !c.trim().is_empty());
            (allowed.contains(&s.as_str()) || cmd).then(|| s.clone())
        };
        match k.as_str() {
            "fan_mode" => {
                if let Some(m) = choice(v) {
                    s.fan_mode = match m.as_str() {
                        "high" => FanMode::High,
                        "full" => FanMode::Full,
                        _ => FanMode::Low,
                    };
                }
            }
            "fan_period" => s.fan_period_s = int(v).map_or(s.fan_period_s, |i| i as u16),
            "fan_min_duty" => s.fan_min_duty_pct = int(v).map_or(s.fan_min_duty_pct, |i| i as u8),
            "hysteresis" => s.hysteresis_c = int(v).map_or(s.hysteresis_c, |i| i as u8),
            "led_brightness" => s.led_brightness = int(v).map_or(s.led_brightness, |i| i as u8),
            "disk_standby_minutes" => {
                s.disk_standby_minutes = int(v).map_or(s.disk_standby_minutes, |i| i as u16)
            }
            "thermal_shutdown" => s.thermal_shutdown = boolean(v).unwrap_or(s.thermal_shutdown),
            "beep_on_start" => s.beep_on_start = boolean(v).unwrap_or(s.beep_on_start),
            "beep_on_error" => s.beep_on_error = boolean(v).unwrap_or(s.beep_on_error),
            "disk_leds" => s.disk_leds = boolean(v).unwrap_or(s.disk_leds),
            "wol" => s.wol = boolean(v).unwrap_or(s.wol),
            "ac_recovery" => s.ac_recovery = boolean(v).unwrap_or(s.ac_recovery),
            "led_reduced_level" => {
                s.led_reduced_level = int(v).map_or(s.led_reduced_level, |i| i as u8)
            }
            // any other string is discarded by synohwd (always normal)
            "led_schedule" => {
                if let Raw::Str(t) = v
                    && led_schedule_valid(t)
                {
                    s.led_schedule = t.clone();
                }
            }
            "power_button" => match choice(v).as_deref() {
                Some("poweroff") => s.power_button = PowerButtonAction::Poweroff,
                Some("log") => s.power_button = PowerButtonAction::Log,
                Some("ignore") => s.power_button = PowerButtonAction::Ignore,
                Some(c) => foreign.push(format!("power_button = {c}")),
                None => {}
            },
            "reset_button" => match choice(v).as_deref() {
                Some("log") => s.reset_button = ResetButtonAction::Log,
                Some("ignore") => s.reset_button = ResetButtonAction::Ignore,
                Some(c) if c.strip_prefix("command:").map(str::trim) == Some(RESET_HANDLER) => {
                    s.reset_button = ResetButtonAction::NetworkRescue
                }
                Some(c) => foreign.push(format!("reset_button = {c}")),
                None => {}
            },
            // led_activity_hz (gone) and unknown keys: ignored, like synohwd
            _ => {}
        }
    }
    (s, foreign)
}

/// Parses the TOML text of `/etc/synohwd.toml` into raw values (feature `domain`).
#[cfg(feature = "domain")]
pub fn parse_toml(text: &str) -> Result<Vec<(String, Raw)>, String> {
    let t: toml::Table = toml::from_str(text).map_err(|e| format!("{CONF_PATH}: {e}"))?;
    Ok(t.into_iter()
        .map(|(k, v)| {
            let r = match v {
                toml::Value::Boolean(b) => Raw::Bool(b),
                toml::Value::Integer(i) => Raw::Int(i),
                toml::Value::String(s) => Raw::Str(s),
                _ => Raw::Other,
            };
            (k, r)
        })
        .collect())
}

/// What synohwd runs with given the file text (missing file: defaults; a file it cannot
/// parse: defaults too, as synohwd logs and does).
#[cfg(feature = "domain")]
pub fn effective(text: Option<&str>) -> (SynologySettings, Vec<String>) {
    match text.map(parse_toml) {
        Some(Ok(raw)) => lenient(&raw),
        _ => (SynologySettings::default(), Vec::new()),
    }
}

#[cfg(all(test, feature = "domain"))]
mod tests {
    use super::*;

    fn s(k: &str, v: Raw) -> (String, Raw) {
        (k.to_string(), v)
    }

    /// synohwd's own test (`test_synohwd.py`, "conf" section), case by case.
    #[test]
    fn lenient_matches_synohwd_validate_conf() {
        assert_eq!(lenient(&[]).0, SynologySettings::default(), "conf defaults");
        let bad = vec![
            s("power_button", Raw::Bool(true)),
            s("fan_period", Raw::Int(0)),
            s("fan_min_duty", Raw::Other), // 12.5
            s("hysteresis", Raw::Int(-1)),
            s("fan_mode", Raw::Str("turbo".into())),
            s("led_brightness", Raw::Int(1000)),
            s("disk_standby_minutes", Raw::Str("20".into())),
            s("reset_button", Raw::Str("command:".into())),
            s("wol", Raw::Int(1)),
            s("nonsense", Raw::Int(3)),
            s("led_activity_hz", Raw::Int(4)),
        ];
        let (c, foreign) = lenient(&bad);
        assert_eq!(
            c.power_button,
            PowerButtonAction::Poweroff,
            "bool for a string -> default"
        );
        assert_eq!(c.fan_period_s, 5, "fan_period 0 -> clamped to 5");
        assert_eq!(c.fan_min_duty_pct, 0, "float duty -> default");
        assert_eq!(c.hysteresis_c, 0, "hysteresis -1 -> 0");
        assert_eq!(c.fan_mode, FanMode::Low, "unknown fan_mode -> default");
        assert_eq!(c.led_brightness, 255, "led_brightness 1000 -> 255");
        assert_eq!(c.disk_standby_minutes, 0, "string minutes -> default");
        assert_eq!(
            c.reset_button,
            ResetButtonAction::Log,
            "empty command -> default"
        );
        assert!(!c.wol, "int for a bool -> default");
        assert!(foreign.is_empty());
        let good = vec![
            s("power_button", Raw::Str("command:/usr/local/bin/x".into())),
            s("fan_period", Raw::Int(60)),
            s("fan_min_duty", Raw::Int(70)),
            s("fan_mode", Raw::Str("high".into())),
            s("wol", Raw::Bool(true)),
            s("reset_button", Raw::Str("ignore".into())),
        ];
        let (c, foreign) = lenient(&good);
        assert_eq!(
            (
                c.fan_period_s,
                c.fan_min_duty_pct,
                c.fan_mode,
                c.wol,
                c.reset_button
            ),
            (60, 70, FanMode::High, true, ResetButtonAction::Ignore)
        );
        // a command rynas does not manage: kept by synohwd, reported by rynas
        assert_eq!(
            foreign,
            vec!["power_button = command:/usr/local/bin/x".to_string()]
        );
        let (c, _) = lenient(&[s(
            "reset_button",
            Raw::Str(format!("command:{RESET_HANDLER}")),
        )]);
        assert_eq!(c.reset_button, ResetButtonAction::NetworkRescue);
    }

    /// The API refuses every value synohwd would clamp; accepts every bound.
    #[test]
    fn strict_validation_parity() {
        let ok = SynologySettings::default();
        assert!(validate(&ok).is_ok());
        for (key, lo, hi) in RANGES {
            for (v, good) in [(lo - 1, false), (lo, true), (hi, true), (hi + 1, false)] {
                let mut t = ok.clone();
                let fits = match key {
                    "fan_period" => u16::try_from(v).map(|x| t.fan_period_s = x).is_ok(),
                    "fan_min_duty" => u8::try_from(v).map(|x| t.fan_min_duty_pct = x).is_ok(),
                    "hysteresis" => u8::try_from(v).map(|x| t.hysteresis_c = x).is_ok(),
                    "led_brightness" => u8::try_from(v).map(|x| t.led_brightness = x).is_ok(),
                    "disk_standby_minutes" => {
                        u16::try_from(v).map(|x| t.disk_standby_minutes = x).is_ok()
                    }
                    "led_reduced_level" => {
                        u8::try_from(v).map(|x| t.led_reduced_level = x).is_ok()
                    }
                    _ => unreachable!(),
                };
                if !fits {
                    // not representable in the API type: refused by deserialization
                    assert!(!good, "{key} = {v}");
                    continue;
                }
                assert_eq!(validate(&t).is_ok(), good, "{key} = {v}");
                // what the API accepts, synohwd keeps unchanged
                if good {
                    let raw = parse_toml(&render(&t)).unwrap();
                    assert_eq!(lenient(&raw).0, t, "{key} = {v} survives synohwd");
                }
            }
        }
        // the JSON form refuses out-of-type values before validation
        let bad: Result<SynologySettings, _> = serde_json::from_str(r#"{"fan_mode": "turbo"}"#);
        assert!(bad.is_err());
        let bad: Result<SynologySettings, _> =
            serde_json::from_str(r#"{"power_button": "command:/bin/x"}"#);
        assert!(bad.is_err(), "command: is reserved for rynas handlers");
    }

    #[test]
    fn golden_defaults() {
        let text = render(&SynologySettings::default());
        assert_eq!(
            text,
            "# synohwd configuration, written by rynas (Hardware > Synology).\n\
             # Change it from rynas: edits made here are reported as drift.\n\
             fan_mode = \"low\"\n\
             fan_period = 20\n\
             fan_min_duty = 0\n\
             hysteresis = 3\n\
             thermal_shutdown = true\n\
             power_button = \"poweroff\"\n\
             reset_button = \"log\"\n\
             beep_on_start = true\n\
             beep_on_error = true\n\
             disk_leds = true\n\
             led_brightness = 255\n\
             disk_standby_minutes = 0\n\
             wol = false\n\
             ac_recovery = true\n\
             led_schedule = \"\"\n\
             led_reduced_level = 1\n"
        );
        // every key of synohwd, nothing else, and synohwd reads back exactly the settings
        let raw = parse_toml(&text).unwrap();
        let keys: Vec<&str> = raw.iter().map(|(k, _)| k.as_str()).collect();
        let mut want = KEYS.to_vec();
        want.sort();
        let mut got = keys.clone();
        got.sort();
        assert_eq!(got, want);
        assert_eq!(lenient(&raw), (SynologySettings::default(), vec![]));
    }

    /// What rynas writes on a Synology it adopts with synodeb's defaults: synohwd's defaults
    /// and network rescue on the reset button (`domain::startup_value`), one `reset_button`.
    #[test]
    fn golden_rynas_default() {
        let s = SynologySettings {
            reset_button: ResetButtonAction::NetworkRescue,
            ..Default::default()
        };
        let text = render(&s);
        assert_eq!(
            text,
            "# synohwd configuration, written by rynas (Hardware > Synology).\n\
             # Change it from rynas: edits made here are reported as drift.\n\
             fan_mode = \"low\"\n\
             fan_period = 20\n\
             fan_min_duty = 0\n\
             hysteresis = 3\n\
             thermal_shutdown = true\n\
             power_button = \"poweroff\"\n\
             reset_button = \"command:/usr/libexec/rynas/rynasctl net reset-button\"\n\
             beep_on_start = true\n\
             beep_on_error = true\n\
             disk_leds = true\n\
             led_brightness = 255\n\
             disk_standby_minutes = 0\n\
             wol = false\n\
             ac_recovery = true\n\
             led_schedule = \"\"\n\
             led_reduced_level = 1\n"
        );
        assert!(!text.contains("rynas network"));
        assert_eq!(lenient(&parse_toml(&text).unwrap()), (s, vec![]));
    }

    #[test]
    fn golden_custom() {
        let s = SynologySettings {
            fan_mode: FanMode::High,
            fan_period_s: 60,
            fan_min_duty_pct: 30,
            hysteresis_c: 5,
            thermal_shutdown: false,
            power_button: PowerButtonAction::Log,
            reset_button: ResetButtonAction::NetworkRescue,
            beep_on_start: false,
            beep_on_error: true,
            disk_leds: false,
            led_brightness: 64,
            disk_standby_minutes: 20,
            wol: true,
            ac_recovery: false,
            led_schedule: "0".repeat(100) + &"1".repeat(60) + &"2".repeat(8),
            led_reduced_level: 2,
        };
        let text = render(&s);
        assert!(
            text.contains(
                "reset_button = \"command:/usr/libexec/rynas/rynasctl net reset-button\"\n"
            )
        );
        assert!(text.contains("fan_mode = \"high\"\nfan_period = 60\n"));
        let raw = parse_toml(&text).unwrap();
        assert_eq!(lenient(&raw), (s, vec![]));
        assert_eq!(quote("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000A\"");
    }

    #[test]
    fn effective_of_files() {
        assert_eq!(effective(None).0, SynologySettings::default());
        assert_eq!(
            effective(Some("not = [toml")).0,
            SynologySettings::default()
        );
        // the commented default file of synodeb is all defaults
        let synodeb = "# fan_mode = \"low\"\n# fan_period = 20\n";
        assert_eq!(effective(Some(synodeb)).0, SynologySettings::default());
        let (c, _) = effective(Some("fan_mode = \"full\"\nled_brightness = 0\n"));
        assert_eq!((c.fan_mode, c.led_brightness), (FanMode::Full, 1));
    }

    /// `led_schedule`: "" or 168 × [012]; synohwd discards anything else, rynas refuses it.
    #[test]
    fn led_schedule_validation_and_file() {
        let week = "0".repeat(24) + &"1".repeat(120) + &"2".repeat(24);
        let ok = SynologySettings {
            led_schedule: week.clone(),
            led_reduced_level: 2,
            ..Default::default()
        };
        assert!(validate(&ok).is_ok());
        let text = render(&ok);
        assert!(text.contains(&format!("\nled_schedule = \"{week}\"\nled_reduced_level = 2\n")));
        assert_eq!(lenient(&parse_toml(&text).unwrap()).0, ok);
        for bad in [
            "0".repeat(167),
            "0".repeat(169),
            "3".repeat(168),
            format!("{}x", "0".repeat(167)),
            format!("é{}", "0".repeat(166)),
            " ".repeat(168),
        ] {
            let t = SynologySettings {
                led_schedule: bad.clone(),
                ..Default::default()
            };
            assert!(validate(&t).is_err(), "{bad:?}");
            // what synohwd does with it: always normal
            let (c, _) = effective(Some(&format!("led_schedule = {}\n", quote(&bad))));
            assert_eq!(c.led_schedule, "", "{bad:?}");
        }
        // wrong type, out of range: like synohwd
        let (c, _) = effective(Some("led_schedule = 1\nled_reduced_level = 12\n"));
        assert_eq!((c.led_schedule.as_str(), c.led_reduced_level), ("", 7));
        let (c, _) = effective(Some("led_reduced_level = -3\n"));
        assert_eq!(c.led_reduced_level, 0);
        // a file of an older synohwd (no LED keys) reads as the defaults
        assert_eq!(effective(Some("wol = true\n")).0.led_reduced_level, 1);
    }

    fn led_status(supported: bool) -> SynologyStatus {
        SynologyStatus {
            running: true,
            led_schedule_supported: supported,
            led_level_min: supported.then_some(0),
            led_level_max: supported.then_some(3),
            led_level_default: supported.then_some(3),
            ..Default::default()
        }
    }

    #[test]
    fn led_settings_against_the_model() {
        let applied = SynologySettings::default();
        let week = "1".repeat(168);
        let mut s = SynologySettings {
            led_schedule: week.clone(),
            led_reduced_level: 3,
            ..Default::default()
        };
        assert!(validate_led(&s, &applied, &led_status(true)).is_ok());
        s.led_reduced_level = 4;
        let e = validate_led(&s, &applied, &led_status(true)).unwrap_err();
        assert!(e.contains("0 to 3"), "{e}");
        // no chip: a new schedule is refused, the defaults and an existing one pass
        s.led_reduced_level = 1;
        assert!(validate_led(&s, &applied, &led_status(false)).is_err());
        assert!(validate_led(&applied, &applied, &led_status(false)).is_ok());
        assert!(validate_led(&s, &s.clone(), &led_status(false)).is_ok());
        // synohwd stopped: the scale is unknown, nothing refused
        let stopped = SynologyStatus::default();
        s.led_reduced_level = 7;
        assert!(validate_led(&s, &applied, &stopped).is_ok());
        // an out-of-scale level already applied does not block saving unrelated fields
        let old = SynologySettings {
            led_reduced_level: 7,
            ..Default::default()
        };
        let other = SynologySettings {
            led_reduced_level: 7,
            led_brightness: old.led_brightness.saturating_sub(1).max(1),
            wol: !old.wol,
            ..Default::default()
        };
        assert!(validate_led(&other, &old, &led_status(true)).is_ok());
        // changing it to another out-of-scale value is still refused
        let other = SynologySettings {
            led_reduced_level: 6,
            ..other
        };
        assert!(validate_led(&other, &old, &led_status(true)).is_err());
    }
}
