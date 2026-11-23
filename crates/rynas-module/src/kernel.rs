//! A module's kernel channel: the kernels its system layer publishes for the machine.
//!
//! On a plain Debian the kernel comes with the distribution's metapackage (`linux-image-amd64`)
//! and rynas holds or installs it like any other: one series, no feed. A module whose system
//! layer ships its own kernels gives a [`KernelChannel`]: the series the machine can follow
//! (each with its metapackages), the publisher's feed of kernels (format 1 of
//! [`rynas_api_types::updates::KernelFeed`]: dates, change logs, CVEs) and what to run after a
//! kernel change so the machine boots the kernel of the chosen series.

/// A kernel series: its metapackages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelSeries {
    /// Stable id, as the feed names it (`longterm`).
    pub id: &'static str,
    /// The image metapackage that selects the series.
    pub image: &'static str,
    /// Its headers metapackage.
    pub headers: &'static str,
    /// Other metapackages that follow it (transitional names, a default alias): held and
    /// removed with it, installed only when they already are.
    pub followers: &'static [&'static str],
}

pub trait KernelChannel: Send + Sync {
    /// The series, oldest branch first: with several image metapackages installed the
    /// machine boots the last one's kernel.
    fn series(&self) -> &'static [KernelSeries];

    /// The publisher's kernel feed (https).
    fn feed_url(&self) -> Option<&'static str> {
        None
    }

    /// After apt changed the kernels (blocking, as root): makes the boot device follow the
    /// chosen series. Its output is shown in the task log.
    fn after_change(&self) -> Result<String, String> {
        Ok(String::new())
    }
}
