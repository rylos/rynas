//! synodeb's kernels (`rynas_module::KernelChannel`): the `longterm` and `stable` series,
//! the feed `kernels.json` synodeb publishes with its apt repository, and the boot-link hook
//! that writes the DOM after a series change.
//!
//! - Series: `linux-image-synodeb-longterm` (the default; `linux-image-synodeb` and the old
//!   `-ds923` names follow it) and `linux-image-synodeb-stable`. With both installed the DOM
//!   boots the newest stable kernel; without the stable one wanted, the newest longterm one
//!   (synodeb's `boot-kernels`).
//! - After a change: `/etc/kernel/postinst.d/zz-synodeb-boot-links` without arguments. The
//!   DOM is written only by the hooks of kernel packages, so removing the stable metapackage
//!   alone (or installing a longterm kernel configured before that removal) would leave the
//!   DOM on the stable kernel. The hook is idempotent: it recomputes the `/boot` links and
//!   runs `synodeb-dom sync`, which copies only what differs, and prints
//!   `synodeb: DEBIAN -> X, DEBIAN_OLD -> Y`. It runs outside rynasd's sandbox (it mounts the
//!   DOM), like `synodeb-dom` ([`crate::boot`]).
//!
//! `rynasd --dry-system`: the hook is only logged.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rynas_module::{KernelChannel, KernelSeries};

/// synodeb's feed of published kernels (format 1).
pub const FEED_URL: &str = "https://rylos.github.io/synodeb/kernels.json";
/// The hook that points the boot links and the DOM at the kernel of the chosen series.
pub const BOOT_LINKS: &str = "/etc/kernel/postinst.d/zz-synodeb-boot-links";
const HOOK_TIMEOUT: Duration = Duration::from_secs(300);

pub const SERIES: &[KernelSeries] = &[
    KernelSeries {
        id: "longterm",
        image: "linux-image-synodeb-longterm",
        headers: "linux-headers-synodeb-longterm",
        followers: &[
            "linux-image-synodeb",
            "linux-headers-synodeb",
            "linux-image-ds923",
            "linux-headers-ds923",
            "linux-image-ds923-longterm",
            "linux-headers-ds923-longterm",
        ],
    },
    KernelSeries {
        id: "stable",
        image: "linux-image-synodeb-stable",
        headers: "linux-headers-synodeb-stable",
        followers: &["linux-image-ds923-stable", "linux-headers-ds923-stable"],
    },
];

pub struct SynodebKernels {
    pub root: PathBuf,
    pub dry: bool,
}

impl SynodebKernels {
    /// synodeb's kernels are in use: its boot-link hook is installed below `root`.
    pub fn find(root: &Path, dry: bool) -> Option<Self> {
        rynas_config::files::under_root(root, Path::new(BOOT_LINKS))
            .is_file()
            .then(|| Self { root: root.to_path_buf(), dry })
    }
}

impl KernelChannel for SynodebKernels {
    fn series(&self) -> &'static [KernelSeries] {
        SERIES
    }

    fn feed_url(&self) -> Option<&'static str> {
        Some(FEED_URL)
    }

    fn after_change(&self) -> Result<String, String> {
        if self.dry {
            tracing::info!("dry: {BOOT_LINKS}");
            return Ok("dry system: boot links not run".into());
        }
        let cmd = rynas_tasks::Outside::new("rynas-synodeb-boot", "rynas: synodeb boot links", HOOK_TIMEOUT)
            .cmd([BOOT_LINKS]);
        let shown = cmd.display();
        match cmd.env_clear().run_blocking() {
            Ok(o) if o.success() => Ok(o.stdout),
            Ok(o) => Err(format!("{shown}: exit code {:?}: {}", o.code, o.stderr.trim())),
            Err(e) => Err(format!("{shown}: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_by_its_hook() {
        let d = tempfile::tempdir().unwrap();
        assert!(SynodebKernels::find(d.path(), true).is_none());
        let p = d.path().join(BOOT_LINKS.trim_start_matches('/'));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        let k = SynodebKernels::find(d.path(), true).unwrap();
        assert_eq!(k.series()[0].id, "longterm");
        assert!(k.after_change().unwrap().contains("dry"));
    }
}
