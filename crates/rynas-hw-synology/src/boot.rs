//! synodeb's boot layer (`rynas_module::BootLayer`): the DOM (the Synology boot flash with
//! its own GRUB entries `DEBIAN`, `DEBIAN_OLD`, `SYNOLOGY_*`) through `synodeb-dom`.
//!
//! - status: `synodeb-dom status --json` (schema 1, fields only ever added; see
//!   `docs/synodeb-integration.md`, S5) → [`BootImage`];
//! - root rollback: `synodeb-dom sync --root DIR` (synodeb 4.3) rewrites the DOM boot entry
//!   from the kernel of the system at `DIR` and refuses when `DIR` lacks
//!   `/lib/modules/<release>`.
//!
//! `rynasd --dry-system`: nothing is run; the status is a fixture and a sync is only logged.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rynas_api_types::updates::{BootImage, BootKernel, KernelTrial};
use rynas_module::{BootLayer, BootSync};
use rynas_tasks::Cmd;
use serde::Deserialize;

/// Where synodeb installs its DOM tool.
pub const SYNODEB_DOM: [&str; 2] = ["/usr/sbin/synodeb-dom", "/usr/local/sbin/synodeb-dom"];
/// The status of a DS920+ (dry system, tests).
pub const FIXTURE: &str = include_str!("../tests/fixtures/synodeb-dom.json");
/// `synodeb-dom sync` writes a kernel and an initrd to the flash: give it time.
const SYNC_TIMEOUT: Duration = Duration::from_secs(180);
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);

pub struct SynodebDom {
    bin: PathBuf,
    dry: bool,
}

impl SynodebDom {
    /// The tool on this machine. Dry system: the fake hardware tree below `hw_root` has it, or
    /// the module found Synology hardware there.
    pub fn find(hw_root: &Path, dry: bool, hardware: bool) -> Option<Self> {
        if dry {
            let fake = rynas_hw::under(hw_root, SYNODEB_DOM[0]).is_file();
            return (fake || hardware).then(|| Self {
                bin: PathBuf::from(SYNODEB_DOM[0]),
                dry,
            });
        }
        SYNODEB_DOM
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .map(|bin| Self { bin, dry })
    }

    fn run(&self, cmd: Cmd) -> Result<String, String> {
        let shown = cmd.display();
        match cmd.env_clear().run_blocking() {
            Ok(o) if o.success() => Ok(o.stdout),
            Ok(o) => Err(format!(
                "{shown}: exit code {:?}: {}",
                o.code,
                o.stderr.trim()
            )),
            Err(e) => Err(format!("{shown}: {e}")),
        }
    }
}

impl BootSync for SynodebDom {
    fn tool(&self) -> &str {
        "synodeb-dom"
    }

    fn sync_root(&self, root: &Path) -> Result<String, String> {
        if self.dry {
            tracing::info!("dry: {} sync --root {}", self.bin.display(), root.display());
            return Ok("dry system: synodeb-dom not run".into());
        }
        self.run(
            Cmd::new(&self.bin)
                .args(["sync", "--root"])
                .arg(root)
                .timeout(SYNC_TIMEOUT),
        )
    }

    fn undo_hint(&self) -> String {
        "run `synodeb-dom sync --root /`".into()
    }
}

impl BootLayer for SynodebDom {
    fn status(&self) -> Result<BootImage, String> {
        let text = if self.dry {
            FIXTURE.to_string()
        } else {
            self.run(
                Cmd::new(&self.bin)
                    .args(["status", "--json"])
                    .timeout(STATUS_TIMEOUT),
            )?
        };
        parse_status(&text)
    }
}

#[derive(Deserialize)]
struct Wire {
    schema: u32,
    #[serde(default)]
    entries: Vec<String>,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    fallback: Vec<String>,
    #[serde(default)]
    trial: Option<String>,
    #[serde(default)]
    running: Option<String>,
    #[serde(default)]
    outdated: Option<String>,
    #[serde(default)]
    kernels: Kernels,
    #[serde(default)]
    dom_free_kb: Option<u64>,
}

#[derive(Deserialize, Default)]
struct Kernels {
    #[serde(default, rename = "DEBIAN")]
    debian: Option<BootKernel>,
    #[serde(default, rename = "DEBIAN_OLD")]
    debian_old: Option<BootKernel>,
}

/// Parses the JSON; schemas other than 1 are refused (a newer one would say so).
pub fn parse_status(text: &str) -> Result<BootImage, String> {
    let w: Wire =
        serde_json::from_str(text.trim()).map_err(|e| format!("synodeb-dom status: {e}"))?;
    if w.schema != 1 {
        return Err(format!("synodeb-dom status: unknown schema {}", w.schema));
    }
    let trial = match w.trial.as_deref() {
        Some("armed") => KernelTrial::Armed,
        Some("trying") => KernelTrial::Trying,
        _ => KernelTrial::None,
    };
    Ok(BootImage {
        entries: w.entries,
        default: w.default,
        fallback: w.fallback,
        trial,
        running: w.running.filter(|s| !s.is_empty()),
        outdated: w.outdated.filter(|s| !s.is_empty()),
        kernel: w.kernels.debian,
        kernel_old: w.kernels.debian_old,
        free_bytes: w.dom_free_kb.map(|k| k * 1024),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture() {
        let s = parse_status(FIXTURE).unwrap();
        assert_eq!(s.entries.len(), 4);
        assert_eq!(s.default.as_deref(), Some("DEBIAN"));
        assert_eq!(s.fallback, ["DEBIAN_OLD", "SYNOLOGY_1"]);
        assert_eq!(s.trial, KernelTrial::None);
        assert_eq!(s.outdated, None);
        assert_eq!(
            s.kernel.unwrap().release.as_deref(),
            Some("6.12.48-synodeb")
        );
        assert_eq!(s.free_bytes, Some(48120 * 1024));
    }

    #[test]
    fn minimal_and_newer_fields() {
        let s = parse_status(r#"{"schema":1,"trial":"trying","outdated":"rewrite","kernels":{"DEBIAN":null,"DEBIAN_OLD":null},"new_field":3}"#).unwrap();
        assert_eq!(s.trial, KernelTrial::Trying);
        assert_eq!(s.outdated.as_deref(), Some("rewrite"));
        assert!(s.kernel.is_none());
        assert!(parse_status(r#"{"schema":2}"#).is_err());
        assert!(parse_status("not json").is_err());
    }

    #[test]
    fn dry_tool_on_fake_hardware_only() {
        let d = tempfile::tempdir().unwrap();
        assert!(SynodebDom::find(d.path(), true, false).is_none());
        let dom = SynodebDom::find(d.path(), true, true).unwrap();
        assert_eq!(dom.status().unwrap().default.as_deref(), Some("DEBIAN"));
        assert!(dom.sync_root(Path::new("/")).is_ok());
        assert!(dom.undo_hint().contains("synodeb-dom sync"));
    }
}
