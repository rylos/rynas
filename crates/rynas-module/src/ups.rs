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
    BatteryTest, UpsBackend, UpsClientRole, UpsDevice, UpsNetServerConfig, UpsNetSource,
    UpsProbeState, UpsSettings, UpsSetupBlock,
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

    // ------------------------------------------------------------ network UPS server

    /// The layer's own network UPS server (`None`: the layer has none, the server is not
    /// offered with it).
    fn netserver(&self) -> Option<&dyn NetServerLayer> {
        None
    }
}

/// A module layer's network UPS server (`ups.netserver` with a layer): the layer runs upsd on
/// every address and a firewall of its own; rynas hands it the allowed hosts and the accounts.
/// Its tool answers the generic exit codes: 0 applied, 2 refused values (nothing changed, the
/// reason on stderr), anything else not applied.
pub trait NetServerLayer: Send + Sync {
    /// Reads the server's state (exit 0 when the layer has the server; a layer too old for
    /// it fails).
    fn status_cmd(&self) -> Cmd;

    /// The state of [`NetServerLayer::status_cmd`]'s output.
    fn parse_status(&self, out: &CmdOutput) -> NetLayerRead;

    /// The layer's own limits on a normalized value (field path and message, like
    /// `rynas_nut::netserver::normalize`).
    fn check(&self, c: &UpsNetServerConfig) -> Result<(), (String, String)>;

    /// Turns the server on with exactly these allowed hosts and accounts. `passwords` are the
    /// accounts whose password is new or changed; they never go on the command line (the
    /// standard input), an account without one keeps the layer's password.
    fn set_cmd(
        &self,
        allow: &[String],
        users: &[(String, UpsClientRole)],
        passwords: &[(String, String)],
    ) -> Cmd;

    /// Turns the server off (upsd only on the NAS itself; the accounts of the server removed).
    fn off_cmd(&self) -> Cmd;
}

/// What [`NetServerLayer::parse_status`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetLayerRead {
    Status(NetLayerStatus),
    /// The layer has no network UPS server (a version before it).
    Unsupported,
    /// The tool failed or its answer has an unknown format.
    Unreadable(String),
}

/// The layer's network UPS server now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetLayerStatus {
    pub enabled: bool,
    pub listen: Vec<String>,
    pub allow: Vec<String>,
    /// Accounts of the server, with their role.
    pub users: Vec<(String, UpsClientRole)>,
    /// The layer's own monitoring account, when it exists.
    pub primary_account: Option<String>,
    pub firewall_active: bool,
    pub server_active: bool,
    pub source: UpsNetSource,
}
