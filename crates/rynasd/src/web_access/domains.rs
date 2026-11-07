//! Own domains (wave W2, `docs/phase2/docker-apps.md` §5.4): credentials sealed at rest,
//! the names a domain adds, the certificate subjects, the files the engine reads the DNS
//! secrets from, and the LAN records rynas writes through the provider.
//!
//! - **Credentials.** The values arrive write-only (`DnsCredential::secrets`) and are sealed
//!   at once with the notification engine's age key (`<state>/notify/secret.key`, the store
//!   rynas already uses for target passwords, D-NOTIFY): the configuration key holds only the
//!   envelope (`sealed`, base64), so `config.db`, its revisions and a `.rynasbak` without a
//!   passphrase never carry a token in clear. The key travels only inside the
//!   passphrase-encrypted `secrets.age` of an export (D-CFGBAK). Answers never carry
//!   `secrets` or `sealed`.
//! - **Engine.** The Caddy configuration names the secrets with `{file./path}` placeholders
//!   (Caddy ≥ 2.8; the caddy-dns modules replace them in `Provision`). rynasd writes the
//!   values to `/var/lib/rynas-caddy/rynas/dns/<credential>/<field>` (0640
//!   root:rynas-caddy, folder 0750): the rendered JSON, `last-good.json` and `GET /config/`
//!   hold no secret. A changed value with an unchanged JSON forces a reload.
//! - **Records.** `*.<domain>` and `<domain>` A (AAAA with `lan_ipv6`) = the NAS's LAN
//!   address, written with the same token through [`rynas_dns`]; rynasd remembers which
//!   record sets it wrote (`<state>/web-access/dns-records.json`) and deletes only those.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use rynas_api_types::web_access::{
    ChallengeKind, DnsAction, DnsChange, DnsCredential, DnsProvider, DnsSyncReport,
    WebAccessConfig, WebDomain,
};
use rynas_config::Invalid;
use rynas_dns::plan::{self, Wanted};
use rynas_dns::{Creds, DnsApi, DnsError, Rtype};
use rynas_notify::engine::secrets::SecretBox;
use serde::{Deserialize, Serialize};

use super::model::HostFacts;

/// Let's Encrypt.
pub const ACME_PRODUCTION: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Let's Encrypt staging (untrusted certificates, high limits).
pub const ACME_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";
/// Resolvers Caddy checks the challenge record's propagation with: public ones, so a LAN
/// resolver with rebind protection or a split-horizon zone cannot stall issuance.
pub const PROPAGATION_RESOLVERS: &[&str] = &["1.1.1.1:53", "9.9.9.9:53"];
/// Public resolvers of the domain checklist.
pub const PUBLIC_RESOLVERS: &[&str] = &["1.1.1.1:53", "9.9.9.9:53"];
/// A failed record sync is tried again after this long (and on every apply or address change).
pub const RECORDS_RETRY: Duration = Duration::from_secs(600);

/// Builds the record client of a provider (tests inject fakes; `--dry-system` refuses).
pub type DnsFactory =
    Box<dyn Fn(DnsProvider, &Creds) -> Result<Option<Box<dyn DnsApi>>, DnsError> + Send + Sync>;

/// The real clients, at the providers' public endpoints.
pub fn real_factory() -> DnsFactory {
    Box::new(|p, c| rynas_dns::client(p, c, &rynas_dns::Endpoints::default()))
}

/// `--dry-system`: providers are never called.
pub fn offline_factory() -> DnsFactory {
    Box::new(|_, _| {
        Err(DnsError::Unsupported(
            "development mode: DNS providers are not called".into(),
        ))
    })
}

// ---------------------------------------------------------------------------------------------
// credentials
// ---------------------------------------------------------------------------------------------

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// Drops what a client may not set: `sealed` (only rynasd writes it).
pub fn strip_input(c: &mut WebAccessConfig) {
    for cr in &mut c.credentials {
        cr.sealed = None;
    }
}

/// Seals the values given in `secrets` and keeps, for a credential given without them, the
/// envelope of `previous` (same id and provider; the staged value first, then the applied
/// one). A credential with neither, and no envelope of its own, is refused.
pub fn seal_credentials(
    c: &mut WebAccessConfig,
    previous: &[&WebAccessConfig],
    sb: Option<&SecretBox>,
) -> Result<(), Invalid> {
    for (i, cr) in c.credentials.iter_mut().enumerate() {
        let at = format!("credentials[{i}].secrets");
        if !cr.secrets.is_empty() {
            rynas_dns::check_creds(cr.provider, &cr.secrets)
                .map_err(|(f, m)| Invalid::at(format!("{at}.{f}"), m))?;
            let sb = sb.ok_or_else(|| {
                Invalid::at(
                    &at,
                    "this rynasd has no secret store: credentials cannot be saved",
                )
            })?;
            let values: BTreeMap<String, String> = cr
                .secrets
                .iter()
                .map(|(k, v)| (k.clone(), v.trim().to_string()))
                .filter(|(_, v)| !v.is_empty())
                .collect();
            let sealed = sb.seal(&values).map_err(|e| Invalid::at(&at, e))?;
            cr.fields = values.keys().cloned().collect();
            cr.sealed = Some(b64().encode(sealed));
            cr.secrets.clear();
            continue;
        }
        if cr.sealed.is_some() {
            continue;
        }
        let prev = previous
            .iter()
            .flat_map(|p| p.credentials.iter())
            .find(|p| p.id == cr.id && p.provider == cr.provider && p.sealed.is_some());
        match prev {
            Some(p) => {
                cr.sealed = p.sealed.clone();
                cr.fields = p.fields.clone();
            }
            None => {
                return Err(Invalid::at(at, "enter the credential's values"));
            }
        }
    }
    Ok(())
}

/// The values of a credential.
pub fn open_creds(cr: &DnsCredential, sb: Option<&SecretBox>) -> Result<Creds, String> {
    let sealed = cr
        .sealed
        .as_deref()
        .ok_or_else(|| format!("the credential {} has no stored values", cr.id))?;
    let sb = sb.ok_or("this rynasd has no secret store")?;
    let bytes = b64()
        .decode(sealed)
        .map_err(|e| format!("credential {}: {e}", cr.id))?;
    sb.open_sealed(&bytes).map_err(|e| {
        format!(
            "the credential {} cannot be decrypted ({e}): enter its values again (restored without the key?)",
            cr.id
        )
    })
}

/// Overwrites opened credential values before they are freed (review 2026-11-07, L6).
pub fn wipe(mut c: Creds) {
    use zeroize::Zeroize;
    for v in c.values_mut() {
        v.zeroize();
    }
}

/// Whether the credential's envelope opens, without keeping the values.
pub fn creds_open(cr: &DnsCredential, sb: Option<&SecretBox>) -> bool {
    open_creds(cr, sb).map(wipe).is_ok()
}

/// A copy for answers: no `secrets`, no `sealed`.
pub fn for_answer(mut c: WebAccessConfig) -> WebAccessConfig {
    for cr in &mut c.credentials {
        cr.sealed = None;
        cr.secrets.clear();
    }
    c
}

/// The credential of a domain in `c`.
pub fn credential_of<'a>(c: &'a WebAccessConfig, d: &WebDomain) -> Option<&'a DnsCredential> {
    let id = d.credential.as_deref()?;
    c.credentials.iter().find(|x| x.id == id)
}

// ---------------------------------------------------------------------------------------------
// names
// ---------------------------------------------------------------------------------------------

/// Domains that serve names: enabled.
pub fn active(c: &WebAccessConfig) -> impl Iterator<Item = &WebDomain> {
    c.domains.iter().filter(|d| d.enabled)
}

/// `<label>.<domain>` (or `None` past 253 characters).
pub fn route_name(label: &str, domain: &str) -> Option<String> {
    let n = format!("{label}.{domain}");
    (n.len() <= 253).then_some(n)
}

/// The names the certificate of `d` covers: `*.<name>` + `<name>` with a wildcard; with
/// HTTP-01 `<name>` (when the interface is on it) and every route's `<label>.<name>`.
pub fn cert_subjects(c: &WebAccessConfig, d: &WebDomain) -> Vec<String> {
    if d.wildcard && d.challenge != ChallengeKind::Http01 {
        return vec![format!("*.{}", d.name), d.name.clone()];
    }
    let mut v = Vec::new();
    if d.ui && c.settings.ui.enabled {
        v.push(d.name.clone());
    }
    for r in c.routes.iter().filter(|r| r.enabled && r.names.domain) {
        if let Some(n) = route_name(&r.label, &d.name) {
            v.push(n);
        }
    }
    v
}

/// The ACME directory.
pub fn acme_ca(c: &WebAccessConfig) -> &'static str {
    if c.acme.staging {
        ACME_STAGING
    } else {
        ACME_PRODUCTION
    }
}

/// Caddy's storage folder of a CA (`acme-v02.api.letsencrypt.org-directory`).
pub fn issuer_dir(ca: &str) -> String {
    let rest = ca
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    rest.trim_end_matches('/').replace('/', "-")
}

/// certmagic's storage-safe form of a name (`*.x` → `wildcard_.x`).
pub fn safe_key(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .replace(' ', "_")
        .replace('+', "_plus_")
        .replace('*', "wildcard_")
        .replace(':', "-")
        .replace("..", "")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '.' | '-'))
        .collect()
}

/// Path of the certificate of `subject` below Caddy's storage root.
pub fn cert_path(storage: &str, ca: &str, subject: &str) -> String {
    let k = safe_key(subject);
    format!("{storage}/certificates/{}/{k}/{k}.crt", issuer_dir(ca))
}

// ---------------------------------------------------------------------------------------------
// the engine's secret files
// ---------------------------------------------------------------------------------------------

/// Credentials the engine needs (DNS challenge of an enabled domain), by id.
pub fn engine_credentials(c: &WebAccessConfig) -> BTreeSet<String> {
    active(c)
        .filter(|d| d.challenge != ChallengeKind::Http01)
        .filter_map(|d| d.credential.clone())
        .collect()
}

/// Writes the placeholder fields of the needed credentials below `parent/dns`
/// (`dns/<credential>/<field>`, 0640 root:`gid`, folders 0750) and removes what is no longer
/// needed. `parent` is rynasd's own `rynas/` folder, already opened and checked (owned by
/// root): everything below it is reached through descriptors (`openat` with `O_NOFOLLOW`,
/// `fchown`/`fchmod`, `renameat`, `unlinkat`), so a symlink planted anywhere is never
/// followed, chmodded, chowned or crossed by a delete (review 2026-11-07, H1). Returns
/// whether anything changed.
pub fn write_engine_secrets(
    parent: &std::fs::File,
    gid: Option<u32>,
    values: &BTreeMap<String, (DnsProvider, Creds)>,
) -> io::Result<bool> {
    use rynas_docker::safefs;
    let owner = gid.map(|g| (0, g));
    let mut changed = false;
    let dns = if values.is_empty() {
        match safefs::open_dir_at(parent, "dns") {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        }
    } else {
        let (d, made) = safefs::ensure_dir_at(parent, "dns", 0o750, owner)?;
        if !made {
            safefs::set_owner_mode(&d, 0o750, owner)?;
        }
        d
    };
    for (id, (provider, creds)) in values {
        let (d, made) = safefs::ensure_dir_at(&dns, id, 0o750, owner)?;
        if !made {
            safefs::set_owner_mode(&d, 0o750, owner)?;
        }
        let mut keep = BTreeSet::new();
        for spec in rynas_dns::fields(*provider)
            .iter()
            .filter(|s| s.placeholder)
        {
            let Some(v) = creds.get(spec.name) else {
                continue;
            };
            keep.insert(spec.name.to_string());
            if safefs::read_at(&d, spec.name, 64 << 10).is_ok_and(|old| old == *v) {
                continue;
            }
            safefs::write_at(&d, spec.name, v.as_bytes(), 0o640, owner)?;
            changed = true;
        }
        for n in safefs::list_at(&d)? {
            if !keep.contains(&n) {
                safefs::remove_tree_at(&d, &n)?;
                changed = true;
            }
        }
    }
    for n in safefs::list_at(&dns)? {
        if !values.contains_key(&n) {
            safefs::remove_tree_at(&dns, &n)?;
            changed = true;
        }
    }
    Ok(changed)
}

// ---------------------------------------------------------------------------------------------
// records
// ---------------------------------------------------------------------------------------------

/// Record sets rynas wrote for one domain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owned {
    pub name: String,
    pub provider: Option<DnsProvider>,
    pub credential: Option<String>,
    pub records: Vec<OwnedSet>,
}

/// One record set rynas wrote and the values it wrote: a set is deleted (or updated
/// without consent) only while the provider still holds exactly these values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "OwnedSetRepr")]
pub struct OwnedSet {
    pub name: String,
    pub rtype: String,
    /// Empty: not known (state written before the values were kept).
    #[serde(default)]
    pub values: Vec<String>,
}

/// The stored forms: `[name, type]` (first W2 files, values unknown) or the full set.
#[derive(Deserialize)]
#[serde(untagged)]
enum OwnedSetRepr {
    Full {
        name: String,
        rtype: String,
        #[serde(default)]
        values: Vec<String>,
    },
    Pair(String, String),
}

impl From<OwnedSetRepr> for OwnedSet {
    fn from(r: OwnedSetRepr) -> Self {
        match r {
            OwnedSetRepr::Full {
                name,
                rtype,
                values,
            } => Self {
                name,
                rtype,
                values,
            },
            OwnedSetRepr::Pair(name, rtype) => Self {
                name,
                rtype,
                values: Vec::new(),
            },
        }
    }
}

impl Owned {
    /// The sets as the planner takes them.
    pub fn sets(&self) -> Vec<Wanted> {
        self.records
            .iter()
            .filter_map(|s| {
                Some(Wanted {
                    name: s.name.clone(),
                    rtype: Rtype::parse(&s.rtype)?,
                    values: s.values.clone(),
                })
            })
            .collect()
    }

    pub fn find(&self, name: &str, rtype: &str) -> Option<&OwnedSet> {
        self.records
            .iter()
            .find(|s| s.name == name && s.rtype == rtype)
    }
}

/// What rynas owns after a sync of one domain: the wanted sets written now (with the values
/// written), sets it owned that are still right (`keep`) or not known (`unknown`, failed
/// deletes). A set that was there already with the wanted values is not adopted; a
/// `conflict` drops ownership.
pub fn owned_after(prev: Option<&Owned>, changes: &[DnsChange]) -> Vec<OwnedSet> {
    let failed = |c: &DnsChange| c.detail.starts_with("failed");
    let mut out: Vec<OwnedSet> = Vec::new();
    for c in changes.iter().filter(|c| c.rtype != "CNAME") {
        let was = prev.and_then(|p| p.find(&c.name, &c.rtype));
        let set = match c.action {
            DnsAction::Create | DnsAction::Update if !failed(c) => Some(OwnedSet {
                name: c.name.clone(),
                rtype: c.rtype.clone(),
                values: c.wanted.clone(),
            }),
            DnsAction::Create | DnsAction::Update => was.cloned(),
            DnsAction::Keep => was.map(|_| OwnedSet {
                name: c.name.clone(),
                rtype: c.rtype.clone(),
                values: c.wanted.clone(),
            }),
            DnsAction::Delete if failed(c) => was.cloned(),
            DnsAction::Unknown => was.cloned(),
            DnsAction::Delete | DnsAction::Manual | DnsAction::Conflict => None,
        };
        if let Some(s) = set
            && !out.iter().any(|o| o.name == s.name && o.rtype == s.rtype)
        {
            out.push(s);
        }
    }
    out.sort_by(|a, b| (&a.name, &a.rtype).cmp(&(&b.name, &b.rtype)));
    out
}

/// What rynasd remembers about records: written sets (persisted), last reports and tries.
#[derive(Default)]
pub struct DnsMem {
    pub owned: BTreeMap<String, Owned>,
    pub reports: BTreeMap<String, DnsSyncReport>,
    pub tried: BTreeMap<String, Instant>,
    pub loaded: bool,
}

/// Where the written record sets are remembered.
pub fn owned_file(state_dir: &Path) -> PathBuf {
    state_dir.join("web-access").join("dns-records.json")
}

pub fn load_owned(state_dir: &Path) -> BTreeMap<String, Owned> {
    std::fs::read(owned_file(state_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_owned(state_dir: &Path, owned: &BTreeMap<String, Owned>) -> io::Result<()> {
    let p = owned_file(state_dir);
    std::fs::create_dir_all(p.parent().expect("has a parent"))?;
    let text = serde_json::to_vec_pretty(owned).map_err(io::Error::other)?;
    crate::tls::write_atomic(&p, &text, 0o600, None)
}

/// How rynas handles the records of a domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordMode {
    /// Not wanted (`lan_record` off, domain disabled).
    Off,
    /// Written through the provider.
    Managed(DnsProvider),
    /// Listed for the administrator (CNAME delegation, provider without a client).
    Manual(String),
}

pub fn record_mode(d: &WebDomain) -> RecordMode {
    if !d.enabled || !d.lan_record {
        return RecordMode::Off;
    }
    match (d.challenge, d.provider) {
        (ChallengeKind::CnameDelegation, _) => RecordMode::Manual(
            "CNAME delegation: the credential cannot write the main zone, add these records once"
                .into(),
        ),
        (_, Some(p)) if rynas_dns::info(p).records => RecordMode::Managed(p),
        (_, Some(p)) => RecordMode::Manual(format!(
            "rynas cannot write records at {} yet: add these records once",
            p.as_str()
        )),
        (_, None) => RecordMode::Off,
    }
}

/// The LAN record sets of `d` on this host.
pub fn wanted_records(c: &WebAccessConfig, d: &WebDomain, facts: &HostFacts) -> Vec<Wanted> {
    let (wild, covers) = match (record_mode(d), d.provider) {
        // added by hand at the zone's own DNS host: one wildcard is the simplest
        (RecordMode::Manual(_), _) => (true, false),
        (_, Some(DnsProvider::Duckdns)) => (false, true),
        (_, Some(p)) => (rynas_dns::info(p).wildcard_records, false),
        (_, None) => (true, false),
    };
    let names: Vec<String> = c
        .routes
        .iter()
        .filter(|r| r.enabled && r.names.domain)
        .filter_map(|r| route_name(&r.label, &d.name))
        .collect();
    let v6 = if d.lan_ipv6 { facts.primary_v6 } else { None };
    plan::wanted(&d.name, wild, covers, &names, facts.primary_v4, v6)
}

/// The CNAME the administrator adds for a delegated challenge.
pub fn delegation_record(d: &WebDomain) -> Option<DnsChange> {
    let t = d.delegate_to.as_ref()?;
    Some(DnsChange {
        domain: d.name.clone(),
        name: format!("_acme-challenge.{}", d.name),
        rtype: "CNAME".into(),
        action: DnsAction::Manual,
        wanted: vec![t.clone()],
        ttl: rynas_dns::DEFAULT_TTL,
        detail: "CNAME delegation of the ACME challenge".into(),
        ..Default::default()
    })
}

/// The expected addresses of a domain's records (A and AAAA).
pub fn expected_addrs(d: &WebDomain, facts: &HostFacts) -> Vec<IpAddr> {
    let mut v: Vec<IpAddr> = facts.primary_v4.map(IpAddr::V4).into_iter().collect();
    if d.lan_ipv6
        && let Some(a) = facts.primary_v6
    {
        v.push(IpAddr::V6(a));
    }
    v
}

pub fn rtype_of(s: &str) -> Option<Rtype> {
    Rtype::parse(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_keys() {
        assert_eq!(safe_key("*.nas.example.com"), "wildcard_.nas.example.com");
        assert_eq!(
            issuer_dir(ACME_PRODUCTION),
            "acme-v02.api.letsencrypt.org-directory"
        );
        assert_eq!(
            cert_path("/s", ACME_STAGING, "*.nas.example.com"),
            "/s/certificates/acme-staging-v02.api.letsencrypt.org-directory/wildcard_.nas.example.com/wildcard_.nas.example.com.crt"
        );
    }

    #[test]
    fn sealing_round_trip_and_carry_forward() {
        let dir = tempfile::tempdir().unwrap();
        let sb = SecretBox::open(dir.path()).unwrap();
        let mut c = WebAccessConfig::default();
        c.credentials.push(DnsCredential {
            id: "cf".into(),
            provider: DnsProvider::Cloudflare,
            secrets: [("api_token".to_string(), " tok-1234567 ".to_string())].into(),
            ..Default::default()
        });
        seal_credentials(&mut c, &[], Some(&sb)).unwrap();
        let cr = &c.credentials[0];
        assert!(cr.secrets.is_empty());
        assert_eq!(cr.fields, ["api_token"]);
        let text = serde_json::to_string(&c).unwrap();
        assert!(!text.contains("tok-1234567"), "{text}");
        assert_eq!(
            open_creds(cr, Some(&sb)).unwrap()["api_token"],
            "tok-1234567"
        );
        // given back without values: the envelope comes along
        let mut again = for_answer(c.clone());
        assert!(again.credentials[0].sealed.is_none());
        seal_credentials(&mut again, &[&c], Some(&sb)).unwrap();
        assert_eq!(again.credentials[0].sealed, c.credentials[0].sealed);
        // another provider under the same id: values needed
        let mut other = for_answer(c.clone());
        other.credentials[0].provider = DnsProvider::Desec;
        assert!(seal_credentials(&mut other, &[&c], Some(&sb)).is_err());
        // without a key the values cannot be read
        let dir2 = tempfile::tempdir().unwrap();
        let sb2 = SecretBox::open(dir2.path()).unwrap();
        assert!(open_creds(&c.credentials[0], Some(&sb2)).is_err());
    }

    #[test]
    fn engine_secret_files() {
        let dir = tempfile::tempdir().unwrap();
        let parent = rynas_docker::safefs::open_start(dir.path()).unwrap();
        let d = dir.path().join("dns");
        let mut v = BTreeMap::new();
        v.insert(
            "ac".to_string(),
            (
                DnsProvider::Acmedns,
                Creds::from([
                    ("password".to_string(), "pw-1".to_string()),
                    ("username".to_string(), "user".to_string()),
                ]),
            ),
        );
        assert!(write_engine_secrets(&parent, None, &v).unwrap());
        assert_eq!(
            std::fs::read_to_string(d.join("ac/password")).unwrap(),
            "pw-1"
        );
        assert!(!d.join("ac/username").exists(), "only placeholder fields");
        assert!(!write_engine_secrets(&parent, None, &v).unwrap(), "unchanged");
        v.clear();
        assert!(write_engine_secrets(&parent, None, &v).unwrap());
        assert!(!d.join("ac").exists());
    }

    /// Review 2026-11-07 H1: a `dns` symlink (or links below it) planted by the engine's
    /// user is never followed: nothing outside is chmodded, written or deleted.
    #[test]
    fn engine_secrets_never_follow_planted_links() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let t = tempfile::tempdir().unwrap();
        let victim = t.path().join("victim");
        std::fs::create_dir_all(victim.join("etc")).unwrap();
        std::fs::write(victim.join("etc/passwd"), "root").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rynas = t.path().join("rynas");
        std::fs::create_dir(&rynas).unwrap();
        symlink(&victim, rynas.join("dns")).unwrap();
        let parent = rynas_docker::safefs::open_start(&rynas).unwrap();
        // the delete pass (no credentials) and the write pass both refuse the link
        assert!(write_engine_secrets(&parent, None, &BTreeMap::new()).is_err());
        let mut v = BTreeMap::new();
        v.insert(
            "ac".to_string(),
            (
                DnsProvider::Acmedns,
                Creds::from([("password".to_string(), "pw".to_string())]),
            ),
        );
        assert!(write_engine_secrets(&parent, None, &v).is_err());
        assert_eq!(std::fs::read_to_string(victim.join("etc/passwd")).unwrap(), "root");
        assert_eq!(
            std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(!victim.join("ac").exists());
        // a real `dns` folder with links inside: the links go, their targets stay
        std::fs::remove_file(rynas.join("dns")).unwrap();
        std::fs::create_dir(rynas.join("dns")).unwrap();
        symlink(&victim, rynas.join("dns/old")).unwrap();
        std::fs::create_dir(rynas.join("dns/ac")).unwrap();
        symlink(victim.join("etc"), rynas.join("dns/ac/password")).unwrap();
        symlink(&victim, rynas.join("dns/ac/stray")).unwrap();
        assert!(write_engine_secrets(&parent, None, &v).unwrap());
        assert_eq!(std::fs::read_to_string(rynas.join("dns/ac/password")).unwrap(), "pw");
        assert!(!rynas.join("dns/old").exists());
        assert!(!rynas.join("dns/ac/stray").exists());
        assert_eq!(std::fs::read_to_string(victim.join("etc/passwd")).unwrap(), "root");
    }
}
