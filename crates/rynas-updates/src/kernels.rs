//! The kernels installed on the root filesystem (`/boot/vmlinuz-*`), newest first.

/// Kernel releases from the names in `/boot` (`vmlinuz-<release>`), newest first.
pub fn installed_kernels<I: IntoIterator<Item = String>>(names: I) -> Vec<String> {
    let mut v: Vec<String> = names
        .into_iter()
        .filter_map(|n| n.strip_prefix("vmlinuz-").map(str::to_string))
        .filter(|r| !r.is_empty() && !r.ends_with(".old"))
        .collect();
    v.sort_by(|a, b| natural_cmp(b, a));
    v.dedup();
    v
}

/// Compares numbers inside version strings by value (`6.12.9` < `6.12.48`).
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let nb = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let va: u64 = std::str::from_utf8(&a[..na])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let vb: u64 = std::str::from_utf8(&b[..nb])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if va != vb {
                    return va.cmp(&vb);
                }
                (a, b) = (&a[na..], &b[nb..]);
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(y);
                }
                (a, b) = (&a[1..], &b[1..]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernels_sorted() {
        let v = installed_kernels(
            [
                "vmlinuz-6.12.9-amd64",
                "config-6.12.9",
                "vmlinuz-6.12.48-synodeb",
                "vmlinuz-6.12.48-synodeb.old",
            ]
            .map(String::from),
        );
        assert_eq!(v, ["6.12.48-synodeb", "6.12.9-amd64"]);
    }
}
