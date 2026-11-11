//! System › Updates › "Debian backports" (`GET/PUT /v1/updates/backports`, task
//! `updates.backports`, D-BACKPORTS).
//!
//! Debian's backports are newer packages rebuilt for the stable release. Their archive is
//! `NotAutomatic`/`ButAutomaticUpgrades` (apt pin 100): with the source configured nothing
//! is installed or upgraded from it unless asked (`-t <codename>-backports`, or a package
//! already taken from there). rynas uses it for NUT when a UPS needs a newer one
//! ([`crate::ups_update`]).
//!
//! - The switch is shown on when any configured apt source has `<codename>-backports`.
//! - On: writes only [`RYNAS_FILE`] (deb822): the Debian mirror and the components the
//!   system already uses for `<codename>` (from its sources; else `http://deb.debian.org/debian`
//!   and `main`), `Suites: <codename>-backports`, Debian's archive keyring; then
//!   `apt-get update` in the rynas-apt unit.
//! - Off: removes only that file, then `apt-get update`. Backports configured by another
//!   source are reported as "configured by the system" and never touched.
//! - Plain Debian only (`ID=debian` with a codename), never with a hardware module's system
//!   layer (UPS or boot layer: such a system manages its apt sources itself).
//! - `--dry-system`: the files below the dry root, apt not run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use rynas_api_types::error_codes;
use rynas_api_types::updates::{BackportsRequest, BackportsState};
use rynas_config::files;

use crate::ops::{Error, Res, user};
use crate::updates::{UpdState, Updates};

/// The only file the switch writes or removes.
pub const RYNAS_FILE: &str = "/etc/apt/sources.list.d/rynas-backports.sources";
pub const FALLBACK_MIRROR: &str = "http://deb.debian.org/debian";
pub const KEYRING: &str = "/usr/share/keyrings/debian-archive-keyring.gpg";

/// One apt source line or stanza (`deb` only).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AptSource {
    pub uris: Vec<String>,
    pub suites: Vec<String>,
    pub components: Vec<String>,
}

/// `deb [options] uri suite components…` lines (comments, `deb-src` skipped).
pub fn parse_one_line(text: &str) -> Vec<AptSource> {
    text.lines()
        .filter_map(|l| {
            let l = l.split('#').next()?.trim();
            let rest = l
                .strip_prefix("deb ")
                .or_else(|| l.strip_prefix("deb\t"))?
                .trim();
            // options in brackets
            let rest = match rest.strip_prefix('[') {
                Some(r) => r.split_once(']')?.1.trim(),
                None => rest,
            };
            let mut w = rest.split_whitespace();
            let uri = w.next()?.to_string();
            let suite = w.next()?.to_string();
            Some(AptSource {
                uris: vec![uri],
                suites: vec![suite],
                components: w.map(String::from).collect(),
            })
        })
        .collect()
}

/// deb822 stanzas (`Types` with `deb`, `Enabled: no` skipped).
pub fn parse_deb822(text: &str) -> Vec<AptSource> {
    let mut out = Vec::new();
    for stanza in text.split("\n\n") {
        let mut f: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut last: Option<String> = None;
        for line in stanza.lines() {
            if line.trim_start().starts_with('#') || line.trim().is_empty() {
                continue;
            }
            if line.starts_with([' ', '\t']) {
                // continuation of a multi-line field
                if let Some(k) = &last {
                    f.entry(k.clone())
                        .or_default()
                        .push_str(&format!(" {}", line.trim()));
                }
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                f.insert(k.clone(), v.trim().to_string());
                last = Some(k);
            }
        }
        let words = |k: &str| {
            f.get(k)
                .map(|v| v.split_whitespace().map(String::from).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        if !words("types").iter().any(|t| t == "deb")
            || f.get("enabled")
                .is_some_and(|v| v.eq_ignore_ascii_case("no"))
        {
            continue;
        }
        out.push(AptSource {
            uris: words("uris"),
            suites: words("suites"),
            components: words("components"),
        });
    }
    out
}

/// The codename of a Debian os-release (`VERSION_CODENAME`), `None` on another system.
pub fn debian_codename(os_release: &str) -> Option<String> {
    let get = |k: &str| {
        os_release.lines().find_map(|l| {
            l.strip_prefix(k)
                .and_then(|v| v.strip_prefix('='))
                .map(|v| v.trim().trim_matches('"').to_string())
        })
    };
    let c = get("VERSION_CODENAME")
        .filter(|c| !c.is_empty() && c.bytes().all(|b| b.is_ascii_lowercase()))?;
    (get("ID").as_deref() == Some("debian")).then_some(c)
}

/// Every `deb` source of the system: `(file, source)`.
pub fn read_sources(root: &Path) -> Vec<(PathBuf, AptSource)> {
    let mut out = Vec::new();
    let main = files::under_root(root, Path::new("/etc/apt/sources.list"));
    if let Ok(t) = std::fs::read_to_string(&main) {
        out.extend(parse_one_line(&t).into_iter().map(|s| (main.clone(), s)));
    }
    let dir = files::under_root(root, Path::new("/etc/apt/sources.list.d"));
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    names.sort();
    for p in names {
        let Ok(t) = std::fs::read_to_string(&p) else {
            continue;
        };
        match p.extension().and_then(|e| e.to_str()) {
            Some("list") => out.extend(parse_one_line(&t).into_iter().map(|s| (p.clone(), s))),
            Some("sources") => out.extend(parse_deb822(&t).into_iter().map(|s| (p.clone(), s))),
            _ => {}
        }
    }
    out
}

fn valid_uri(u: &str) -> bool {
    (u.starts_with("http://") || u.starts_with("https://"))
        && u.len() <= 256
        && u.bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
}

fn valid_component(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 32
        && c.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// rynas's file for `codename`: the mirror and components of the system's `<codename>`
/// source (else the fallback and `main`).
pub fn render(codename: &str, sources: &[(PathBuf, AptSource)]) -> String {
    let base = sources
        .iter()
        .map(|(_, s)| s)
        .find(|s| s.suites.iter().any(|x| x == codename));
    let uri = base
        .and_then(|s| s.uris.iter().find(|u| valid_uri(u)))
        .map_or(FALLBACK_MIRROR, String::as_str);
    let comps: Vec<&str> = base
        .map(|s| {
            s.components
                .iter()
                .map(String::as_str)
                .filter(|c| valid_component(c))
                .collect()
        })
        .filter(|c: &Vec<&str>| !c.is_empty())
        .unwrap_or_else(|| vec!["main"]);
    format!(
        "# Written by rynas (System > Updates > Debian backports): remove it with that switch.\n\
         # Nothing is installed from backports unless asked (apt pin 100).\n\
         Types: deb\nURIs: {uri}\nSuites: {codename}-backports\nComponents: {}\nSigned-By: {KEYRING}\n",
        comps.join(" ")
    )
}

/// The state shown on the page.
pub fn state(root: &Path, system_layer: bool) -> BackportsState {
    let os = std::fs::read_to_string(files::under_root(root, Path::new("/etc/os-release")))
        .unwrap_or_default();
    let Some(codename) = debian_codename(&os) else {
        return BackportsState::default();
    };
    let suite = format!("{codename}-backports");
    let ours = files::under_root(root, Path::new(RYNAS_FILE));
    let mut st = BackportsState {
        available: !system_layer,
        codename: Some(codename),
        ..Default::default()
    };
    for (file, s) in read_sources(root) {
        if s.suites.contains(&suite) {
            if file == ours {
                st.by_rynas = true;
            } else {
                st.by_system = true;
            }
        }
    }
    st.enabled = st.by_rynas || st.by_system;
    st
}

pub fn router(st: UpdState) -> Router {
    Router::new()
        .route("/v1/updates/backports", get(get_state).put(put_state))
        .with_state(st)
}

fn upd(st: &UpdState) -> Res<Arc<Updates>> {
    st.clone().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            error_codes::BACKEND_UNAVAILABLE,
            "updates are not enabled in this rynasd".into(),
        )
    })
}

async fn get_state(State(st): State<UpdState>) -> Res<Json<BackportsState>> {
    let u = upd(&st)?;
    let r = tokio::task::spawn_blocking(move || state(&u.config_root, u.system_layer()))
        .await
        .map_err(Error::internal)?;
    Ok(Json(r))
}

fn conflict(msg: &str) -> Error {
    Error::new(StatusCode::CONFLICT, error_codes::CONFLICT, msg.into())
}

/// Why the switch cannot go to `on` now.
pub fn refusal(st: &BackportsState, on: bool) -> Result<(), &'static str> {
    if !st.available {
        return Err("Debian backports cannot be switched here (not a plain Debian system)");
    }
    if on && st.enabled {
        return Err("Debian backports are already configured");
    }
    if !on && !st.by_rynas {
        return Err(if st.by_system {
            "Debian backports are configured by the system, not by rynas: rynas leaves them"
        } else {
            "Debian backports are already off"
        });
    }
    Ok(())
}

async fn put_state(
    State(st): State<UpdState>,
    hd: HeaderMap,
    body: Result<Json<BackportsRequest>, JsonRejection>,
) -> Res<Response> {
    let (u, who) = (upd(&st)?, user(&hd)?);
    let Json(req) = body?;
    u.ops.admins.require_admin(&who, "Debian backports")?;
    let root = u.config_root.clone();
    let layer = u.system_layer();
    let cur = tokio::task::spawn_blocking(move || state(&root, layer))
        .await
        .map_err(Error::internal)?;
    refusal(&cur, req.enabled).map_err(conflict)?;
    let codename = cur.codename.unwrap_or_default();
    tracing::info!(target: "rynas::audit", "updates backports {} by {who}", if req.enabled { "on" } else { "off" });
    u.spawn("updates.backports", &who, move |u, ctx| async move {
        let path = files::under_root(&u.config_root, Path::new(RYNAS_FILE));
        if req.enabled {
            let text = render(&codename, &read_sources(&u.config_root));
            ctx.log(&format!("writing {RYNAS_FILE}"));
            write_file(&path, &text).map_err(|e| format!("{RYNAS_FILE}: {e}"))?;
        } else {
            ctx.log(&format!("removing {RYNAS_FILE}"));
            std::fs::remove_file(&path).map_err(|e| format!("{RYNAS_FILE}: {e}"))?;
        }
        u.run(
            &ctx,
            crate::updates::apt(&["update"], Duration::from_secs(600)),
        )
        .await?;
        Ok(())
    })
}

/// 0644 root file, written next to it then renamed.
fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("sources.rynas-new");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRIXIE: &str = "Types: deb\n# http://snapshot.debian.org/archive/debian/20261005T000000Z\nURIs: http://ftp.it.debian.org/debian\nSuites: trixie trixie-updates\nComponents: main contrib non-free-firmware\nSigned-By: /usr/share/keyrings/debian-archive-keyring.pgp\n\nTypes: deb\nURIs: http://deb.debian.org/debian-security\nSuites: trixie-security\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.pgp\n";

    fn root_with(os: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("etc/apt/sources.list.d")).unwrap();
        std::fs::write(d.path().join("etc/os-release"), os).unwrap();
        for (p, t) in files {
            std::fs::write(d.path().join(p.trim_start_matches('/')), t).unwrap();
        }
        d
    }

    const OS: &str =
        "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nVERSION_CODENAME=trixie\nID=debian\n";

    #[test]
    fn parsing_and_rendering() {
        let s = parse_deb822(TRIXIE);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].suites, ["trixie", "trixie-updates"]);
        assert_eq!(s[0].components, ["main", "contrib", "non-free-firmware"]);
        assert!(
            parse_deb822("Types: deb\nEnabled: no\nURIs: http://x\nSuites: trixie\n").is_empty()
        );
        let l = parse_one_line(
            "# old\ndeb [arch=amd64 signed-by=/k.gpg] http://deb.debian.org/debian bookworm-backports main contrib\ndeb-src http://x y main\n",
        );
        assert_eq!(
            l,
            [AptSource {
                uris: vec!["http://deb.debian.org/debian".into()],
                suites: vec!["bookworm-backports".into()],
                components: vec!["main".into(), "contrib".into()]
            }]
        );
        assert_eq!(debian_codename(OS).as_deref(), Some("trixie"));
        assert_eq!(debian_codename("ID=ubuntu\nVERSION_CODENAME=noble\n"), None);
        let r = render(
            "trixie",
            &[(
                PathBuf::from("/etc/apt/sources.list.d/debian.sources"),
                s[0].clone(),
            )],
        );
        assert!(r.contains("URIs: http://ftp.it.debian.org/debian\nSuites: trixie-backports\nComponents: main contrib non-free-firmware\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\n"), "{r}");
        assert_eq!(parse_deb822(&r)[0].suites, ["trixie-backports"]);
        // nothing known: Debian's mirror, main
        assert!(render("trixie", &[]).contains(
            "URIs: http://deb.debian.org/debian\nSuites: trixie-backports\nComponents: main\n"
        ));
    }

    #[test]
    fn state_and_refusals() {
        let d = root_with(OS, &[("/etc/apt/sources.list.d/debian.sources", TRIXIE)]);
        let st = state(d.path(), false);
        assert_eq!(
            st,
            BackportsState {
                available: true,
                codename: Some("trixie".into()),
                ..Default::default()
            }
        );
        assert!(refusal(&st, true).is_ok());
        assert!(refusal(&st, false).is_err());
        // a module's system layer: not offered
        assert!(!state(d.path(), true).available);
        assert!(refusal(&state(d.path(), true), true).is_err());
        // rynas's file
        std::fs::write(
            d.path().join(RYNAS_FILE.trim_start_matches('/')),
            render("trixie", &read_sources(d.path())),
        )
        .unwrap();
        let st = state(d.path(), false);
        assert!(st.enabled && st.by_rynas && !st.by_system);
        assert!(refusal(&st, false).is_ok() && refusal(&st, true).is_err());
        // configured by the system as well (one-line file): read-only
        std::fs::remove_file(d.path().join(RYNAS_FILE.trim_start_matches('/'))).unwrap();
        std::fs::write(
            d.path().join("etc/apt/sources.list"),
            "deb http://deb.debian.org/debian trixie-backports main\n",
        )
        .unwrap();
        let st = state(d.path(), false);
        assert!(st.enabled && st.by_system && !st.by_rynas);
        assert!(refusal(&st, false).unwrap_err().contains("by the system"));
        assert!(refusal(&st, true).is_err());
        // another system
        assert_eq!(
            state(root_with("ID=ubuntu\n", &[]).path(), false),
            BackportsState::default()
        );
    }

    #[test]
    fn written_file_is_0644() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d
            .path()
            .join("etc/apt/sources.list.d/rynas-backports.sources");
        write_file(&p, "x\n").unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "x\n");
    }
}
