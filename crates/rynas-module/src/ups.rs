//! A module's UPS layer.
//!
//! The core's UPS support is NUT, the standard Debian way (`rynasd::ups`, `rynasd::ups_setup`):
//! rynas blocks in `/etc/nut`, its own policy file, battery tests sent to upsd, rynasd
//! following upsd for the shutdown. Some system layers run NUT themselves, with their own tool
//! for the driver choice, the tests, a shutdown mode of their own and pre-shutdown hooks. Their
//! module provides an [`UpsLayer`]; rynasd then hands every step to it and never writes
//! `/etc/nut`. The orchestration (the wizard's candidates and test runs, the configuration
//! keys `ups.safemode` and `ups.device`, the `ups.test` task, stopping rynas's work before the
//! shutdown) stays in the core.

use std::path::Path;
use std::time::Duration;

use rynas_api_types::ups::{
    BatteryTest, UpsBackend, UpsDevice, UpsProbeState, UpsSettings, UpsSetupBlock,
};
use rynas_config::ManagedFile;
use rynas_tasks::{Cmd, CmdOutput};

/// Runs a command and returns its output whatever the exit code (the host, or a simulation
/// under `--dry-system` and in tests).
pub trait Runner: Send + Sync {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, String>;
}

pub trait UpsLayer: Send + Sync {
    /// What the API reports ([`UpsBackend::Module`]).
    fn backend(&self) -> UpsBackend {
        UpsBackend::Module
    }

    // ------------------------------------------------------------ `ups.safemode` (policy)

    /// The file that holds the shutdown policy (below the configuration root).
    fn policy_path(&self) -> &'static str;

    /// The policy as the layer's file gets it.
    fn render_policy(&self, s: &UpsSettings) -> ManagedFile;

    /// The policy read back from that file.
    fn parse_policy(&self, text: &str) -> UpsSettings;

    /// Logged after an apply (how the layer picks the change up).
    fn policy_note(&self) -> &'static str;

    /// Title and body of the critical notification recorded when the layer's pre-shutdown hook
    /// asks rynasd to stop its work (`POST /v1/ups/safe-mode`).
    fn stop_notice(&self, reason: &str) -> (String, String);

    // ------------------------------------------------------------ battery tests

    /// Starts (or stops) a battery test.
    fn test_cmd(&self, kind: BatteryTest) -> Cmd;

    /// Reads the last test result.
    fn result_cmd(&self) -> Cmd;

    /// The result text of [`UpsLayer::result_cmd`]'s output.
    fn parse_result(&self, stdout: &str) -> String;

    // ------------------------------------------------------------ the setup wizard

    /// Why the wizard cannot run with this layer (e.g. a version without driver setup).
    fn setup_blocked(&self, runner: &dyn Runner) -> Option<UpsSetupBlock>;

    /// The message of [`UpsSetupBlock::ModuleTooOld`].
    fn too_old_message(&self) -> &'static str;

    /// One test run of `d` within `timeout`: upsc-format variables on stdout, the driver's
    /// lines on stderr.
    fn probe_cmd(&self, d: &UpsDevice, timeout: Duration) -> Cmd;

    /// A state of its own for an exit code of the test run (`None`: read the output).
    fn probe_exit_state(&self, _code: Option<i32>) -> Option<UpsProbeState> {
        None
    }

    /// Sets the layer up with `to`, or back to its default driver.
    fn setup_cmd(&self, to: Option<&UpsDevice>) -> Cmd;

    /// The driver the layer runs now (`None`: its default, or nothing set up).
    fn current_device(
        &self,
        runner: &dyn Runner,
        read: &dyn Fn(&Path) -> Option<String>,
    ) -> Option<UpsDevice>;

    /// `--dry-system`: the simulated output of the layer's own tool (`None`: not its tool).
    fn dry_output(&self, _cmd: &Cmd) -> Option<CmdOutput> {
        None
    }
}
