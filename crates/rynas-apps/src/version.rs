//! The package version of a catalogue entry (`version` in `app.toml`) and `min_rynas`:
//! plain `MAJOR.MINOR.PATCH`, no pre-release or build parts. The upstream version
//! (`app_version`) is free text and never compared.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// A `MAJOR.MINOR.PATCH` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    pub const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self { major, minor, patch }
    }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err(format!("`{s}` is not MAJOR.MINOR.PATCH"));
        }
        let mut n = [0u64; 3];
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
                return Err(format!("`{s}` is not MAJOR.MINOR.PATCH"));
            }
            n[i] = p.parse().map_err(|_| format!("`{s}`: number too large"))?;
        }
        Ok(Self::new(n[0], n[1], n[2]))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl serde::Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Compares two version strings; `None` when either is not a valid version.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    Some(a.parse::<Version>().ok()?.cmp(&b.parse::<Version>().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_order() {
        let a: Version = "1.2.3".parse().unwrap();
        assert_eq!(a, Version::new(1, 2, 3));
        assert!(Version::new(1, 10, 0) > Version::new(1, 9, 9));
        assert_eq!(a.to_string(), "1.2.3");
        for bad in ["1.2", "1.2.3.4", "v1.2.3", "1.02.3", "1..3", "1.2.3-rc1"] {
            assert!(bad.parse::<Version>().is_err(), "{bad}");
        }
        assert_eq!(compare("2.0.0", "10.0.0"), Some(Ordering::Less));
        assert_eq!(compare("x", "1.0.0"), None);
    }
}
