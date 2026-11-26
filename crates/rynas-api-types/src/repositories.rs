//! System › Software repositories (`/api/v1/repositories/...`, D-REPOS): the apt sources of
//! the system, read from `/etc/apt/sources.list` and `/etc/apt/sources.list.d` (one-line and
//! deb822 files), classified, checked, and changed through rynasd tasks.
//!
//! Every write carries the [`RepositoriesState::digest`] it was made from: a stale one is
//! refused (409 `conflict`). Design in `docs/phase2/repositories.md`.

use serde::{Deserialize, Serialize};

/// Where an entry comes from and who manages it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RepoKind {
    /// Debian's own archive (main suite, `-security`, `-updates`, `-backports`,
    /// `-proposed-updates`, or another Debian suite).
    Debian,
    /// A file rynas writes (Debian backports, Docker, Tailscale): changed on the page that
    /// manages it ([`RepoEntry::manage_url`]).
    Rynas,
    /// A source on the machine itself: the installation disc (`cdrom:`) or a local folder
    /// (`file:`, `copy:`). Never counted as unsupported; an enabled disc entry is warned
    /// about (apt asks for the disc).
    LocalMedia,
    /// A file of the hardware support's system layer (read-only here).
    SystemLayer,
    /// Anything else.
    #[default]
    ThirdParty,
}

/// How serious an [`RepoInfo`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RepoLevel {
    #[default]
    Info,
    Warning,
    Critical,
}

/// The overall verdict of the checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RepoStatus {
    #[default]
    Good,
    Warning,
    Critical,
}

/// One finding of the checks, tied to a file and an entry when it is about one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepoInfo {
    pub level: RepoLevel,
    /// Stable code (`no_debian`, `no_security`, `testing`…): the UI text is `repo.i.<code>`.
    pub code: String,
    /// Values of the text's placeholders (`suite`, `codename`, `uri`…).
    #[serde(default)]
    pub args: Vec<(String, String)>,
    /// The English text, for logs and clients without the UI's texts.
    pub message: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub index: Option<usize>,
}

/// Why an entry cannot be changed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOnly {
    /// Written by rynas for another page ([`RepoEntry::manage_url`]).
    Rynas,
    /// The hardware support's system layer manages it.
    SystemLayer,
    /// A Debian entry on a system whose hardware support manages the Debian sources.
    DebianOnSystemLayer,
}

/// What the last `apt-get update` said about an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FetchErrorKind {
    /// The signing key is not available (`NO_PUBKEY`).
    NoPubkey,
    /// The signing key has expired (`EXPKEYSIG`, `KEYEXPIRED`).
    ExpiredKey,
    /// A bad signature (`BADSIG`): the repository's key may have changed.
    BadSignature,
    /// The repository is not signed.
    NotSigned,
    /// The Release file is past its `Valid-Until`.
    ExpiredRelease,
    /// 404, or no Release file for the suite.
    NotFound,
    /// Name resolution, connection or timeout.
    Network,
    /// `Origin`, `Label`, `Suite` or `Codename` of the Release changed.
    ReleaseInfoChanged,
    /// The same target is configured twice.
    Duplicate,
    #[default]
    Other,
}

/// One problem of the last `apt-get update`, matched to the entries by URI and suite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FetchError {
    pub kind: FetchErrorKind,
    /// `true`: an `E:` line (apt did not use the repository); `false`: a `W:` line.
    #[serde(default)]
    pub error: bool,
    /// The URI as apt printed it (with the path of the file when it gave one).
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub suite: Option<String>,
    /// The key ids of `NO_PUBKEY`/`EXPKEYSIG`.
    #[serde(default)]
    pub keys: Vec<String>,
    /// apt's line.
    pub message: String,
}

/// The last "Check now" (`apt-get update`) and what it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepoCheck {
    pub at_ms: u64,
    /// apt-get exited with 0.
    pub ok: bool,
    #[serde(default)]
    pub errors: Vec<FetchError>,
}

/// One `deb`/`deb-src` line or deb822 stanza (several URIs, suites and types kept together).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepoEntry {
    /// Position in its file (0-based, disabled entries counted): the id of the writes.
    pub index: usize,
    pub enabled: bool,
    /// `deb`, `deb-src`.
    pub types: Vec<String>,
    pub uris: Vec<String>,
    pub suites: Vec<String>,
    #[serde(default)]
    pub components: Vec<String>,
    /// The other options/fields (`Architectures`, `Signed-By`, `arch`…) as written.
    #[serde(default)]
    pub options: Vec<(String, String)>,
    /// The comment lines just above it, without `#`.
    #[serde(default)]
    pub comment: String,
    pub kind: RepoKind,
    /// `Origin` of the cached Release file (`Debian`, `Docker`…), else guessed from the host.
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub read_only: Option<ReadOnly>,
    /// The page that manages a [`RepoKind::Rynas`] entry.
    #[serde(default)]
    pub manage_url: Option<String>,
    /// When its index files were last downloaded (oldest of its suites; `None`: never).
    #[serde(default)]
    pub last_fetch_ms: Option<u64>,
    /// Problems of the last check that concern it.
    #[serde(default)]
    pub errors: Vec<FetchError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RepoFormat {
    #[default]
    OneLine,
    Deb822,
}

/// One sources file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepoFile {
    pub path: String,
    pub format: RepoFormat,
    #[serde(default)]
    pub entries: Vec<RepoEntry>,
    /// The file could not be read or parsed (apt refuses to run with it): the reason.
    #[serde(default)]
    pub error: Option<String>,
    /// The package that ships it, when one does.
    #[serde(default)]
    pub package: Option<String>,
    /// The file may be removed here (every entry third-party, not a package's).
    #[serde(default)]
    pub removable: bool,
}

/// A file of `sources.list.d` that apt does not read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IgnoredFile {
    pub path: String,
    /// `extension` (not `.list`/`.sources`), `name` (characters apt refuses), `backup`
    /// (`.bak`, `.dpkg-old`… apt skips silently).
    pub reason: String,
}

/// The Debian (official) section: what rynas can write as `debian.sources`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OfficialState {
    /// rynas may change the Debian entries (plain Debian, no hardware system layer).
    pub manageable: bool,
    /// Why not (`not_debian`, `system_layer`).
    #[serde(default)]
    pub reason: Option<String>,
    /// The mirror of the main suite (`http://deb.debian.org/debian` by default).
    pub mirror: String,
    pub security_uri: String,
    /// Enabled suites of this release.
    pub main: bool,
    pub security: bool,
    pub updates: bool,
    pub backports: bool,
    pub proposed: bool,
    /// Backports come from a file rynas did not write (left as it is).
    #[serde(default)]
    pub backports_by_system: bool,
    /// Components of the main suite (`main`, `contrib`, `non-free`, `non-free-firmware`).
    pub components: Vec<String>,
    /// `deb-src` entries are enabled.
    pub sources: bool,
    /// `/etc/apt/sources.list.d/debian.sources` was written by rynas.
    #[serde(default)]
    pub by_rynas: bool,
    /// Known Debian mirrors to choose from (the country mirrors of the form
    /// `http://ftp.<cc>.debian.org/debian`).
    #[serde(default)]
    pub mirrors: Vec<String>,
}

/// `GET /repositories`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepositoriesState {
    /// apt sources exist on this machine.
    pub available: bool,
    /// `VERSION_CODENAME` of a Debian system.
    #[serde(default)]
    pub codename: Option<String>,
    /// The dpkg architecture (`amd64`, `arm64`…).
    #[serde(default)]
    pub arch: String,
    /// sha256 over every sources file (sorted by path): sent back by every write.
    pub digest: String,
    #[serde(default)]
    pub files: Vec<RepoFile>,
    #[serde(default)]
    pub ignored: Vec<IgnoredFile>,
    #[serde(default)]
    pub infos: Vec<RepoInfo>,
    pub status: RepoStatus,
    /// The enabled sources outside what rynas supports (third-party origins, testing, other
    /// distributions, insecure entries): empty when supported.
    #[serde(default)]
    pub unsupported: Vec<String>,
    pub official: OfficialState,
    #[serde(default)]
    pub last_check: Option<RepoCheck>,
    /// One-line files that "Convert to the modern format" would convert.
    #[serde(default)]
    pub convertible: Vec<String>,
    /// A repositories or update task runs (its id).
    #[serde(default)]
    pub running_task: Option<String>,
}

/// `POST /repositories/entries/toggle` → 202 `TaskStarted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ToggleRequest {
    pub digest: String,
    pub path: String,
    pub index: usize,
    pub enabled: bool,
}

/// `PUT /repositories/official` → 202 `TaskStarted`, or 200 [`RepoPreview`] with `preview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct OfficialRequest {
    pub digest: String,
    pub mirror: String,
    pub security: bool,
    pub updates: bool,
    pub backports: bool,
    pub proposed: bool,
    pub components: Vec<String>,
    pub sources: bool,
    #[serde(default)]
    pub preview: bool,
}

/// One file a write would change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FileChange {
    pub path: String,
    /// `write` (new or replaced), `edit`, `rename` (to `rename_to`), `remove`.
    pub action: String,
    #[serde(default)]
    pub rename_to: Option<String>,
    #[serde(default)]
    pub before: String,
    #[serde(default)]
    pub after: String,
}

/// What a write would do (`preview: true`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RepoPreview {
    pub changes: Vec<FileChange>,
}

/// `POST /repositories/modernize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ModernizeRequest {
    pub digest: String,
    #[serde(default)]
    pub preview: bool,
}

/// `POST /repositories/key-preview`: an armored key, or an https URL rynasd downloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct KeyPreviewRequest {
    #[serde(default)]
    pub armored: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// One public key of a keyring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KeyInfo {
    /// Upper-case hex (40 digits for v4 keys, 64 for v6).
    pub fingerprint: String,
    #[serde(default)]
    pub uids: Vec<String>,
    #[serde(default)]
    pub created_ms: Option<u64>,
    /// From the newest self-signature; `None`: does not expire.
    #[serde(default)]
    pub expires_ms: Option<u64>,
    #[serde(default)]
    pub expired: bool,
    #[serde(default)]
    pub subkeys: usize,
}

/// The keys found, and the keyring as rynas will write it (sent back with the add).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KeyPreview {
    pub keys: Vec<KeyInfo>,
    pub armored: String,
}

/// Limits of a key.
pub const KEY_MAX_BYTES: usize = 1024 * 1024;

/// `POST /repositories/third-party` → 202 `TaskStarted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AddRepoRequest {
    pub digest: String,
    /// File name without extension (`[a-z0-9][a-z0-9._-]*`).
    pub name: String,
    pub uris: Vec<String>,
    /// Suites, or one exact path ending in `/` (then no components).
    pub suites: Vec<String>,
    #[serde(default)]
    pub components: Vec<String>,
    /// Empty: the system's.
    #[serde(default)]
    pub architectures: Vec<String>,
    /// Also `deb-src`.
    #[serde(default)]
    pub sources: bool,
    /// The keyring of [`KeyPreview::armored`].
    pub key: String,
    /// The fingerprints the user confirmed: must be exactly the key's.
    pub fingerprints: Vec<String>,
}

/// `POST /repositories/mirror-test`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MirrorTestRequest {
    /// Up to 5 mirrors besides `http://deb.debian.org/debian` (always tested).
    #[serde(default)]
    pub mirrors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MirrorResult {
    pub uri: String,
    /// Time to the answer of the Release file (name lookup included).
    #[serde(default)]
    pub ms: Option<u64>,
    #[serde(default)]
    pub error: Option<MirrorError>,
}

/// Why a mirror failed the test: the UI text is `repo.me.<code>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MirrorErrorCode {
    /// No answer within the time limit.
    Timeout,
    /// The host name did not resolve (or the resolver did not answer in time).
    Dns,
    /// The connection was refused or the host is unreachable.
    Connect,
    /// The TLS handshake failed (certificate, protocol).
    Tls,
    /// The server answered with an HTTP error ([`MirrorError::status`]).
    Http,
    #[default]
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MirrorError {
    pub code: MirrorErrorCode,
    /// The HTTP status of [`MirrorErrorCode::Http`].
    #[serde(default)]
    pub status: Option<u16>,
    /// The underlying error in English, for logs and a tooltip.
    #[serde(default)]
    pub detail: Option<String>,
}

impl MirrorError {
    pub fn new(code: MirrorErrorCode, detail: impl Into<String>) -> Self {
        MirrorError { code, status: None, detail: Some(detail.into()) }
    }

    pub fn http(status: u16) -> Self {
        MirrorError { code: MirrorErrorCode::Http, status: Some(status), detail: None }
    }
}

/// Answer of the mirror test (fastest first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MirrorTest {
    pub results: Vec<MirrorResult>,
}

/// The suites of the official section for `codename`.
pub fn official_suites(codename: &str) -> [String; 5] {
    [
        codename.to_string(),
        format!("{codename}-security"),
        format!("{codename}-updates"),
        format!("{codename}-backports"),
        format!("{codename}-proposed-updates"),
    ]
}

/// The components of Debian's archive, in their order.
pub const DEBIAN_COMPONENTS: [&str; 4] = ["main", "contrib", "non-free", "non-free-firmware"];

/// apt reads a file of `sources.list.d` only when its name has these characters.
pub fn apt_file_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// The name of a new third-party source: `[a-z0-9][a-z0-9._-]{0,62}`, not one rynas or
/// Debian uses.
pub fn repo_name_ok(name: &str) -> bool {
    const RESERVED: &[&str] = &["debian", "docker", "tailscale", "synodeb", "sources", "moved-from-main"];
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.starts_with("rynas")
        && !name.starts_with("extrepo_")
        && !RESERVED.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(apt_file_name_ok("docker.sources"));
        assert!(!apt_file_name_ok("my repo.list"));
        assert!(!apt_file_name_ok("x~"));
        assert!(repo_name_ok("vendor-tools"));
        assert!(repo_name_ok("pkgs.example_1"));
        for bad in ["", "Vendor", "-x", "a b", "debian", "rynas-backports", "extrepo_x", "a..b", "x.", &"a".repeat(64)] {
            assert!(!repo_name_ok(bad), "{bad}");
        }
        assert_eq!(official_suites("trixie")[1], "trixie-security");
    }
}
