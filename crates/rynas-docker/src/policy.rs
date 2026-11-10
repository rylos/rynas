//! Which image updates an automatic update may take (D-UPDATES channel C).
//!
//! Versions come from image tags (`1.27.3`, `v2.17.3`, `2.1.2-alpine`, `2026.11.0`) or, for
//! floating tags (`latest`, `stable`) and short tags (`16`, `1.27`), from the image label
//! `org.opencontainers.image.version` of the old and the new image. Rules:
//!
//! - a leading `v` is dropped; what follows the first `-` or `+` is a suffix: a pre-release
//!   (`rc1`, `beta`…) or a variant (`alpine`, `noble`) that must not change; a trailing
//!   number of a suffix word (`-ls45`, `-r1`, `-1`) is a build counter, not part of the
//!   variant: a newer build of the same version is a patch-level update;
//! - [`UpdatePolicy::Patch`]: same major and minor; [`UpdatePolicy::Minor`]: same major (with
//!   major 0 also the same minor); [`UpdatePolicy::CalverPatch`]: same year and month
//!   (`2026.11.0` → `2026.11.1`); [`UpdatePolicy::Notify`]: never;
//! - a pre-release is never taken, nor a downgrade.

use rynas_api_types::docker::{HoldReason, UpdatePolicy, UpdateVerdict};

/// The OCI label that carries an image's version.
pub const VERSION_LABEL: &str = "org.opencontainers.image.version";

/// What a tag (or a version label) says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagVersion {
    /// 1 to 3 numbers (`16`, `1.27`, `1.27.3`; a fourth number is folded into the patch
    /// compare as text-free extra precision and ignored).
    Version {
        nums: Vec<u64>,
        /// Calendar version: a first number 2000-2199 and a second 1-12.
        calver: bool,
        /// The suffix is a pre-release.
        pre: bool,
        /// The variant suffix (`alpine`, `noble`, `apache`, `ls` of `-ls45`), pre-release
        /// words and build counters removed.
        variant: String,
        /// Build counters of the suffix (`45` of `-ls45`, `1` of `-r1` or `-1`): a newer
        /// build of the same version, compared after the version numbers.
        build: Vec<u64>,
    },
    /// Letters only (`latest`, `stable`, `lts`, `main`).
    Floating,
    /// Anything else (`14-vectorchord0.4.3`, a commit hash…).
    Unknown,
}

const PRE_WORDS: &[&str] = &[
    "rc", "alpha", "beta", "pre", "preview", "dev", "nightly", "snapshot", "canary", "edge", "test",
    "unstable", "next", "a", "b",
];

/// `true` when a suffix part (`rc1`, `beta.2`, `pre`) is a pre-release word.
fn is_pre_word(w: &str) -> bool {
    let letters: String = w.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let rest = &w[letters.len()..];
    let lower = letters.to_ascii_lowercase();
    PRE_WORDS.contains(&lower.as_str())
        && (rest.is_empty() || rest.chars().all(|c| c.is_ascii_digit() || c == '.'))
        && !(lower.len() == 1 && rest.is_empty())
}

/// Reads a tag.
pub fn parse_tag(tag: &str) -> TagVersion {
    let t = tag.trim();
    let t = t
        .strip_prefix('v')
        .or_else(|| t.strip_prefix('V'))
        .filter(|r| r.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(t);
    if t.is_empty() {
        return TagVersion::Unknown;
    }
    if t.chars().all(|c| c.is_ascii_alphabetic()) {
        return TagVersion::Floating;
    }
    let cut = t.find(['-', '+']).unwrap_or(t.len());
    let (core, suffix) = (&t[..cut], t[cut..].trim_start_matches(['-', '+']));
    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty()
        || parts.len() > 4
        || parts
            .iter()
            .any(|p| p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return TagVersion::Unknown;
    }
    let nums: Vec<u64> = parts.iter().filter_map(|p| p.parse().ok()).collect();
    let mut pre = false;
    let mut variant = Vec::new();
    let mut build = Vec::new();
    for w in suffix.split(['-', '+', '_']).filter(|w| !w.is_empty()) {
        if is_pre_word(w) {
            pre = true;
            continue;
        }
        // a build counter: `1` (`-1`), `ls45` (linuxserver), `r1`; the letters are the
        // variant, the trailing number is build metadata (a newer build of the same version)
        let letters = w.len() - w.trim_start_matches(|c: char| c.is_ascii_alphabetic()).len();
        let digits = &w[letters..];
        if !digits.is_empty() && digits.len() <= 9 && digits.bytes().all(|b| b.is_ascii_digit()) {
            if letters > 0 {
                variant.push(w[..letters].to_ascii_lowercase());
            }
            build.push(digits.parse().unwrap_or(0));
            continue;
        }
        // a variant word must start with a letter (`14-vectorchord0.4.3` stays a variant)
        if letters == 0 {
            return TagVersion::Unknown;
        }
        variant.push(w.to_ascii_lowercase());
    }
    let calver = nums.len() >= 2 && (2000..2200).contains(&nums[0]) && (1..=12).contains(&nums[1]);
    TagVersion::Version {
        nums,
        calver,
        pre,
        variant: variant.join("-"),
        build,
    }
}

/// The policy a stack gets when nobody chose one: calendar tags `calver-patch`, versions and
/// floating tags `minor`, anything else `notify`.
pub fn default_policy(tag: &str) -> UpdatePolicy {
    match parse_tag(tag) {
        TagVersion::Version { calver: true, .. } => UpdatePolicy::CalverPatch,
        TagVersion::Version { .. } | TagVersion::Floating => UpdatePolicy::Minor,
        TagVersion::Unknown => UpdatePolicy::Notify,
    }
}

/// The decision about one update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Auto,
    /// Read the version labels after the download, then [`judge_labels`].
    AfterPull,
    Hold(HoldReason),
}

impl Verdict {
    pub fn api(self) -> (UpdateVerdict, Option<HoldReason>) {
        match self {
            Verdict::Auto => (UpdateVerdict::Auto, None),
            Verdict::AfterPull => (UpdateVerdict::AfterPull, None),
            Verdict::Hold(r) => (UpdateVerdict::Hold, Some(r)),
        }
    }

    /// The more restrictive of two verdicts (a stack with several images).
    pub fn and(self, other: Verdict) -> Verdict {
        match (self, other) {
            (Verdict::Hold(r), _) | (_, Verdict::Hold(r)) => Verdict::Hold(r),
            (Verdict::AfterPull, _) | (_, Verdict::AfterPull) => Verdict::AfterPull,
            _ => Verdict::Auto,
        }
    }
}

/// How many leading numbers the policy keeps fixed for `nums` (major 0 counts as a minor
/// under `minor`).
fn fixed(policy: UpdatePolicy, major: u64) -> usize {
    match policy {
        UpdatePolicy::Patch | UpdatePolicy::CalverPatch => 2,
        UpdatePolicy::Minor if major == 0 => 2,
        UpdatePolicy::Minor => 1,
        UpdatePolicy::Notify => usize::MAX,
    }
}

fn num(v: &[u64], i: usize) -> u64 {
    v.get(i).copied().unwrap_or(0)
}

/// From one version to another (two tags of a catalogue update, or the version labels of
/// the old and the new image).
pub fn judge_versions(policy: UpdatePolicy, from: &str, to: &str) -> Verdict {
    if policy == UpdatePolicy::Notify {
        return Verdict::Hold(HoldReason::Policy);
    }
    let (
        TagVersion::Version {
            nums: a,
            calver: ca,
            pre: pa,
            variant: va,
            build: ba,
        },
        TagVersion::Version {
            nums: b,
            calver: cb,
            pre: pb,
            variant: vb,
            build: bb,
        },
    ) = (parse_tag(from), parse_tag(to))
    else {
        return Verdict::Hold(HoldReason::UnknownVersion);
    };
    if pb && !pa {
        return Verdict::Hold(HoldReason::Prerelease);
    }
    if va != vb {
        return Verdict::Hold(HoldReason::Variant);
    }
    let n = a.len().max(b.len());
    // version numbers first, then the build counters (`-ls45` → `-ls46` is a newer build of
    // the same version: a patch-level change every policy but `notify` takes)
    let builds = ba.len().max(bb.len());
    let cmp = (0..n)
        .map(|i| num(&a, i).cmp(&num(&b, i)))
        .chain((0..builds).map(|i| num(&ba, i).cmp(&num(&bb, i))))
        .find(|o| o.is_ne());
    if cmp == Some(std::cmp::Ordering::Greater) {
        return Verdict::Hold(HoldReason::Downgrade);
    }
    let keep = fixed(policy, num(&a, 0));
    for i in 0..keep.min(n) {
        if num(&a, i) != num(&b, i) {
            return Verdict::Hold(match (i, ca && cb, policy) {
                (_, true, _) | (_, _, UpdatePolicy::CalverPatch) => HoldReason::Month,
                (0, _, _) => HoldReason::Major,
                _ => HoldReason::Minor,
            });
        }
    }
    Verdict::Auto
}

/// A new digest of the same tag (a stack's image), before the download. A full version tag
/// (`1.27.3`, `2026.11.0`) is a rebuild of that version; a tag precise enough for the policy
/// (`1.27` under `patch`, `16` under `minor`) cannot leave the allowed band; any other tag
/// (`latest`, `16` under `patch`) is decided by the version labels after the download.
pub fn judge_digest(policy: UpdatePolicy, tag: &str) -> Verdict {
    if policy == UpdatePolicy::Notify {
        return Verdict::Hold(HoldReason::Policy);
    }
    match parse_tag(tag) {
        TagVersion::Version { nums, .. } => {
            if nums.len() >= 3 || nums.len() >= fixed(policy, nums[0]) {
                Verdict::Auto
            } else {
                Verdict::AfterPull
            }
        }
        TagVersion::Floating => Verdict::AfterPull,
        TagVersion::Unknown => Verdict::AfterPull,
    }
}

/// After the download: the version labels of the image the stack ran and the new one.
pub fn judge_labels(policy: UpdatePolicy, old: Option<&str>, new: Option<&str>) -> Verdict {
    match (old, new) {
        (Some(o), Some(n)) if o == n => {
            if policy == UpdatePolicy::Notify {
                Verdict::Hold(HoldReason::Policy)
            } else {
                Verdict::Auto
            }
        }
        (Some(o), Some(n)) => judge_versions(policy, o, n),
        _ => Verdict::Hold(HoldReason::UnknownVersion),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use UpdatePolicy::*;

    fn v(nums: &[u64], calver: bool, pre: bool, variant: &str) -> TagVersion {
        TagVersion::Version {
            nums: nums.to_vec(),
            calver,
            pre,
            variant: variant.into(),
            build: Vec::new(),
        }
    }

    #[test]
    fn build_counters() {
        let built = |nums: &[u64], variant: &str, build: &[u64]| TagVersion::Version {
            nums: nums.to_vec(),
            calver: false,
            pre: false,
            variant: variant.into(),
            build: build.to_vec(),
        };
        assert_eq!(parse_tag("1.2.3-ls45"), built(&[1, 2, 3], "ls", &[45]));
        assert_eq!(parse_tag("3.19.1-r1"), built(&[3, 19, 1], "r", &[1]));
        assert_eq!(parse_tag("2.4.1-1"), built(&[2, 4, 1], "", &[1]));
        assert_eq!(parse_tag("1.2.3-alpine-ls7"), built(&[1, 2, 3], "alpine-ls", &[7]));
        // linuxserver: a newer build of the same version is taken even by `patch`
        assert_eq!(judge_versions(Patch, "1.2.3-ls45", "1.2.3-ls46"), Verdict::Auto);
        assert_eq!(judge_versions(Minor, "1.2.3-ls45", "1.3.0-ls1"), Verdict::Auto);
        assert_eq!(judge_versions(Patch, "1.2.3-ls45", "1.3.0-ls1"), Verdict::Hold(HoldReason::Minor));
        assert_eq!(judge_versions(Minor, "1.2.3-ls46", "1.2.3-ls45"), Verdict::Hold(HoldReason::Downgrade));
        assert_eq!(judge_versions(Patch, "2.4.1-1", "2.4.1-2"), Verdict::Auto);
        assert_eq!(judge_versions(Patch, "3.19.1-r1", "3.19.1-r2"), Verdict::Auto);
        // the letters stay a variant
        assert_eq!(judge_versions(Minor, "1.2.3-ls45", "1.2.4-r1"), Verdict::Hold(HoldReason::Variant));
        assert_eq!(judge_versions(Minor, "1.2.3-ls45", "1.2.4"), Verdict::Hold(HoldReason::Variant));
        assert_eq!(judge_versions(Notify, "1.2.3-ls45", "1.2.3-ls46"), Verdict::Hold(HoldReason::Policy));
        assert_eq!(default_policy("2.4.1-1"), Minor);
    }

    #[test]
    fn tags() {
        assert_eq!(parse_tag("1.27.3"), v(&[1, 27, 3], false, false, ""));
        assert_eq!(parse_tag("v2.17.3"), v(&[2, 17, 3], false, false, ""));
        assert_eq!(parse_tag("2.1.2-alpine"), v(&[2, 1, 2], false, false, "alpine"));
        assert_eq!(parse_tag("15.0.11-rootless"), v(&[15, 0, 11], false, false, "rootless"));
        assert_eq!(parse_tag("2026.11.0"), v(&[2026, 11, 0], true, false, ""));
        assert_eq!(parse_tag("1.5.0-rc1"), v(&[1, 5, 0], false, true, ""));
        assert_eq!(parse_tag("3.0.0-beta.2"), v(&[3, 0, 0], false, true, ""));
        assert_eq!(parse_tag("16"), v(&[16], false, false, ""));
        assert_eq!(parse_tag("latest"), TagVersion::Floating);
        assert_eq!(parse_tag("stable"), TagVersion::Floating);
        assert_eq!(parse_tag("14-vectorchord0.4.3-pgvectors0.2.0"), v(&[14], false, false, "vectorchord0.4.3-pgvectors0.2.0"));
        assert_eq!(parse_tag("sha-1a2b3c"), TagVersion::Unknown);
        assert_eq!(parse_tag("1.2.x"), TagVersion::Unknown);
        assert_eq!(parse_tag("version"), TagVersion::Floating);
        // the variant word `apache` is not the pre-release `a`
        assert_eq!(parse_tag("34.0.5-apache"), v(&[34, 0, 5], false, false, "apache"));
    }

    #[test]
    fn defaults_by_tag() {
        assert_eq!(default_policy("2026.11.0"), CalverPatch);
        assert_eq!(default_policy("1.27"), Minor);
        assert_eq!(default_policy("latest"), Minor);
        assert_eq!(default_policy("sha-abc"), Notify);
        // a year-like first number without a month is not a calendar version
        assert_eq!(default_policy("2026"), Minor);
    }

    #[test]
    fn semver_jumps() {
        assert_eq!(judge_versions(Minor, "1.4.2", "1.5.0"), Verdict::Auto);
        assert_eq!(judge_versions(Minor, "1.4.2", "2.0.0"), Verdict::Hold(HoldReason::Major));
        assert_eq!(judge_versions(Patch, "1.4.2", "1.4.3"), Verdict::Auto);
        assert_eq!(judge_versions(Patch, "1.4.2", "1.5.0"), Verdict::Hold(HoldReason::Minor));
        // 0.x: a minor is breaking
        assert_eq!(judge_versions(Minor, "0.107.80", "0.107.81"), Verdict::Auto);
        assert_eq!(judge_versions(Minor, "0.107.80", "0.108.0"), Verdict::Hold(HoldReason::Minor));
        assert_eq!(judge_versions(Minor, "v2.17.3", "v2.18.0"), Verdict::Auto);
        assert_eq!(judge_versions(Minor, "v2.17.3", "2.17.4"), Verdict::Auto);
        assert_eq!(judge_versions(Notify, "1.0.0", "1.0.1"), Verdict::Hold(HoldReason::Policy));
        assert_eq!(judge_versions(Minor, "1.0.1", "1.0.0"), Verdict::Hold(HoldReason::Downgrade));
    }

    #[test]
    fn suffixes_and_prereleases() {
        assert_eq!(judge_versions(Minor, "2.1.2-alpine", "2.1.3-alpine"), Verdict::Auto);
        assert_eq!(judge_versions(Minor, "2.1.2-alpine", "2.1.3"), Verdict::Hold(HoldReason::Variant));
        assert_eq!(judge_versions(Minor, "1.4.2", "1.5.0-rc1"), Verdict::Hold(HoldReason::Prerelease));
        // from a pre-release to the next one of the same line: the user chose that line
        assert_eq!(judge_versions(Minor, "1.5.0-rc1", "1.5.0-rc2"), Verdict::Auto);
        assert_eq!(judge_versions(Minor, "1.5.0-rc2", "1.5.0"), Verdict::Auto);
        assert_eq!(judge_versions(Minor, "latest", "1.0.0"), Verdict::Hold(HoldReason::UnknownVersion));
    }

    #[test]
    fn calendar_versions() {
        assert_eq!(judge_versions(CalverPatch, "2026.11.0", "2026.11.1"), Verdict::Auto);
        assert_eq!(judge_versions(CalverPatch, "2026.11.3", "2026.12.0"), Verdict::Hold(HoldReason::Month));
        assert_eq!(judge_versions(CalverPatch, "2026.12.4", "2027.1.0"), Verdict::Hold(HoldReason::Month));
        // minor on a calendar version: same year
        assert_eq!(judge_versions(Minor, "2026.10.7", "2026.11.0"), Verdict::Auto);
        assert_eq!(judge_versions(Minor, "2026.12.1", "2027.1.0"), Verdict::Hold(HoldReason::Month));
    }

    #[test]
    fn digests_of_one_tag() {
        assert_eq!(judge_digest(Patch, "1.27.3"), Verdict::Auto);
        assert_eq!(judge_digest(Patch, "1.27"), Verdict::Auto);
        assert_eq!(judge_digest(Patch, "16"), Verdict::AfterPull);
        assert_eq!(judge_digest(Minor, "16"), Verdict::Auto);
        assert_eq!(judge_digest(Minor, "0"), Verdict::AfterPull);
        assert_eq!(judge_digest(CalverPatch, "2026.11"), Verdict::Auto);
        assert_eq!(judge_digest(CalverPatch, "stable"), Verdict::AfterPull);
        assert_eq!(judge_digest(Minor, "latest"), Verdict::AfterPull);
        assert_eq!(judge_digest(Notify, "1.2.3"), Verdict::Hold(HoldReason::Policy));
    }

    #[test]
    fn floating_tags_by_label() {
        assert_eq!(judge_labels(Minor, Some("1.4.2"), Some("1.5.0")), Verdict::Auto);
        assert_eq!(judge_labels(Minor, Some("1.4.2"), Some("2.0.0")), Verdict::Hold(HoldReason::Major));
        assert_eq!(judge_labels(Minor, None, Some("2.0.0")), Verdict::Hold(HoldReason::UnknownVersion));
        assert_eq!(judge_labels(Minor, Some("1.4.2"), Some("1.4.2")), Verdict::Auto);
        assert_eq!(judge_labels(CalverPatch, Some("2026.11.0"), Some("2026.12.0")), Verdict::Hold(HoldReason::Month));
    }

    #[test]
    fn stacks_take_the_strictest() {
        assert_eq!(Verdict::Auto.and(Verdict::AfterPull), Verdict::AfterPull);
        assert_eq!(Verdict::AfterPull.and(Verdict::Hold(HoldReason::Major)), Verdict::Hold(HoldReason::Major));
        assert_eq!(Verdict::Auto.and(Verdict::Auto), Verdict::Auto);
    }
}
