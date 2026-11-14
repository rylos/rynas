//! Sizes in server texts (task logs, error messages, event fields): one rule for every
//! process, the same as the web interface's `i18n::bytes` — binary units (KiB, MiB…), three
//! significant digits, the next unit as soon as the number would need four digits, a `.` as
//! the decimal mark (the interface turns it into the language's mark in event fields, see
//! `notify::event_text`). Interfaces format numbers themselves when the API gives them.

/// The binary units, in steps of 1024.
pub const BINARY_UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// `1536` → `"1.50 KiB"`, `1022 MiB` → `"1.00 GiB"`, `512` → `"512 B"`.
pub fn human_bytes(n: u64) -> String {
    if n < 1000 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 999.5 && i < BINARY_UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    let d = if v < 9.995 {
        2
    } else if v < 99.95 {
        1
    } else {
        0
    };
    format!("{v:.d$} {}", BINARY_UNITS[i])
}

/// Whether `unit` is a size unit [`human_bytes`] or a decimal size writes (`GiB`, `kB`, `GB`…).
pub fn is_size_unit(unit: &str) -> bool {
    BINARY_UNITS.contains(&unit) || ["kB", "MB", "GB", "TB", "PB", "EB"].contains(&unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_significant_digits() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1000), "0.98 KiB");
        assert_eq!(human_bytes(1536), "1.50 KiB");
        assert_eq!(human_bytes(100 * 1024 * 1024), "100 MiB");
        assert_eq!(human_bytes(1022 * 1024 * 1024), "1.00 GiB");
        assert_eq!(human_bytes(5_000_000_000), "4.66 GiB");
        assert_eq!(human_bytes(u64::MAX), "16.0 EiB");
        assert!(is_size_unit("GiB") && is_size_unit("kB") && !is_size_unit("KB") && !is_size_unit("files"));
    }
}
