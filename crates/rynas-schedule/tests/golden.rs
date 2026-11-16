//! The parser and the next-run computation against `systemd-analyze calendar` of Debian
//! trixie (systemd 257.13): `tests/golden/normalize.txt` and `tests/golden/next.txt`,
//! written by `tests/golden/gen.sh` (time zones around the DST changes of 2026 included).

use jiff::Timestamp;
use jiff::tz::TimeZone;
use rynas_schedule::spec::CalendarSpec;

fn golden(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/golden/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn zone_ok(z: &str) -> bool {
    TimeZone::get(z).is_ok()
}

#[test]
fn normalized_forms_match_systemd() {
    let text = golden("normalize.txt");
    let mut bad = Vec::new();
    let mut n = 0;
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let (expr, want) = line.split_once('\t').unwrap();
        n += 1;
        let got = match CalendarSpec::parse_with(expr, zone_ok) {
            Ok(c) => c.to_string(),
            Err(rynas_schedule::Error::Invalid) => "invalid".into(),
            Err(rynas_schedule::Error::Unsupported(_)) => continue,
        };
        if got != want {
            bad.push(format!("{expr:?}: systemd {want:?}, here {got:?}"));
        }
    }
    assert!(n > 100);
    assert!(bad.is_empty(), "{} differences:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn next_runs_match_systemd() {
    let text = golden("next.txt");
    let mut bad = Vec::new();
    let mut n = 0;
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = line.split('\t').collect();
        let (zone, base, iters, expr, norm) = (f[0], f[1], f[2], f[3], f[4]);
        let want: Vec<&str> = f.get(5).map(|s| s.split(' ').filter(|s| !s.is_empty()).collect()).unwrap_or_default();
        let tz = TimeZone::get(zone).unwrap();
        let base: Timestamp = base.parse().unwrap();
        let iters: usize = iters.parse().unwrap();
        n += 1;
        let spec = CalendarSpec::parse_with(expr, zone_ok);
        if norm == "invalid" {
            // refused by the parser, or systemd gave up computing (an endless loop)
            if let Ok(s) = &spec
                && s.next_runs(&tz, base, iters).len() == iters
            {
                bad.push(format!("{zone} {base} {expr:?}: systemd fails, here it works"));
            }
            continue;
        }
        let spec = spec.unwrap_or_else(|e| panic!("{expr}: {e}"));
        assert_eq!(spec.to_string(), norm, "{expr}");
        let got: Vec<String> = spec.next_runs(&tz, base, iters).iter().map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()).collect();
        if got != want {
            bad.push(format!("{zone} base {base} {expr:?}:\n  systemd {}\n  here    {}", want.join(" "), got.join(" ")));
        }
    }
    assert!(n > 1000);
    assert!(bad.is_empty(), "{} of {n} cases differ:\n{}", bad.len(), bad.iter().take(30).cloned().collect::<Vec<_>>().join("\n"));
}
