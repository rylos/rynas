//! The SSH service, OpenSSH's sshd (`/api/v1/services/ssh`), administrators only (D-SSH,
//! `docs/phase2/ssh.md`).
//!
//! rynas writes one drop-in, `/etc/ssh/sshd_config.d/00-rynas.conf`, read before every other
//! file (sshd keeps the first value it reads for most settings). A change that can shut
//! someone out (service, port, password login, root login, who may log in, forwarding) runs
//! as a trial: it is undone by itself unless it is confirmed within the timeout.

use serde::{Deserialize, Serialize};

/// Groups always allowed when only listed accounts may log in: the two SSH levels of the
/// user page and the system accounts of rynas features that receive over SSH (PC backup,
/// the certificate drop folder).
pub const SSH_BASE_GROUPS: [&str; 4] = ["rynas-ssh", "rynas-sftp", "rynas-pcbackup", "rynas-cert-drop"];
/// Members get SFTP only (sshd `ForceCommand internal-sftp`, no forwarding, no terminal).
pub const SSH_SFTP_GROUP: &str = "rynas-sftp";

/// Confirmation window of a trial, in seconds.
pub const SSH_TIMEOUT_DEFAULT_S: u32 = 120;
pub const SSH_TIMEOUT_MIN_S: u32 = 60;
pub const SSH_TIMEOUT_MAX_S: u32 = 600;

/// `PermitRootLogin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SshRootLogin {
    /// `no`.
    #[default]
    No,
    /// `prohibit-password`: keys only.
    Keys,
    /// `yes`.
    Yes,
}

/// Who may log in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SshAllow {
    /// Every account with a valid login (no `AllowGroups`).
    #[default]
    Everyone,
    /// `AllowGroups` = [`SSH_BASE_GROUPS`] + [`SshSettings::allow_groups`].
    Listed,
}

/// What the administrator chooses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSettings {
    /// `ssh.service` enabled and running.
    pub enabled: bool,
    /// 1-65535.
    pub port: u16,
    /// `PasswordAuthentication`; off = keys only.
    pub password_auth: bool,
    #[serde(default)]
    pub root_login: SshRootLogin,
    #[serde(default)]
    pub allow: SshAllow,
    /// More groups allowed with [`SshAllow::Listed`] (e.g. `sudo` for accounts made outside
    /// rynas).
    #[serde(default)]
    pub allow_groups: Vec<String>,
    /// `AllowTcpForwarding`, `AllowStreamLocalForwarding`, `PermitTunnel`.
    #[serde(default)]
    pub tcp_forwarding: bool,
    /// `AllowAgentForwarding`.
    #[serde(default)]
    pub agent_forwarding: bool,
    /// `X11Forwarding`.
    #[serde(default)]
    pub x11_forwarding: bool,
}

impl Default for SshSettings {
    fn default() -> Self {
        SshSettings {
            enabled: true,
            port: 22,
            password_auth: true,
            root_login: SshRootLogin::No,
            allow: SshAllow::Everyone,
            allow_groups: Vec::new(),
            tcp_forwarding: false,
            agent_forwarding: false,
            x11_forwarding: false,
        }
    }
}

/// A host key's fingerprint, to check on first connect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHostKey {
    /// `ED25519`, `ECDSA`, `RSA`.
    pub kind: String,
    pub bits: u32,
    /// `SHA256:…` as `ssh-keygen -lf` prints it.
    pub fingerprint: String,
}

/// What an account can do over SSH, from its login shell and groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshLevel {
    Shell,
    Sftp,
    /// A login shell such as `nologin`: no session.
    None,
}

/// An account that may hold keys: root and the local users (uid 1000-59999).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshAccount {
    pub name: String,
    pub uid: u32,
    /// Member of `rynas-admin` or `sudo` (root counts too).
    pub admin: bool,
    pub level: SshLevel,
    /// Every group it is in (for the allow-list preview).
    pub groups: Vec<String>,
    /// Usable keys in `~/.ssh/authorized_keys`.
    pub keys: u32,
    /// The home is on a disk that sleeps: its keys were not read (`keys` is 0), so as not to
    /// wake it for a page view.
    #[serde(default)]
    pub keys_unread: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshProblemCode {
    /// openssh-server is not installed.
    NotInstalled,
    /// `/etc/ssh/sshd_config` does not include `sshd_config.d/*.conf`: the drop-in is not read.
    NotIncluded,
    /// Another file sets `subject` (`keyword file`) before rynas: that value wins.
    Overridden,
    /// sshd also listens on `subject` (ports set in another file).
    ExtraPorts,
    /// The drop-in was changed by hand; saving writes it again.
    Drift,
    /// sshd ran through socket activation (`ssh.socket`); rynas switches it to `ssh.service`.
    SocketActivation,
    /// The service is enabled but not running.
    NotRunning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshProblem {
    pub code: SshProblemCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// A change waiting for its confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTrial {
    /// Sent back by `POST /services/ssh/confirm`; only the administrator who applied gets it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    pub owner: String,
    pub deadline_ms: u64,
    pub timeout_s: u32,
    /// The port to test with (the new one).
    pub port: u16,
}

/// `GET /services/ssh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SshState {
    pub installed: bool,
    /// The drop-in exists; otherwise [`SshState::settings`] is read from the running
    /// configuration and the first save adopts it.
    pub managed: bool,
    pub settings: SshSettings,
    pub running: bool,
    /// Ports sshd listens on (effective configuration).
    pub ports: Vec<u16>,
    /// The drop-in's path.
    pub dropin: String,
    pub host_keys: Vec<SshHostKey>,
    pub accounts: Vec<SshAccount>,
    pub problems: Vec<SshProblem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<SshTrial>,
}

/// `PUT /services/ssh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshApply {
    pub settings: SshSettings,
    /// Confirmation window ([`SSH_TIMEOUT_MIN_S`]-[`SSH_TIMEOUT_MAX_S`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u32>,
}

/// Answer of `PUT /services/ssh`: the trial when the change waits for a confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SshApplied {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<SshTrial>,
}

/// `POST /services/ssh/confirm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfirm {
    pub token: String,
}

/// One key of an `authorized_keys` file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshKey {
    /// `ssh-ed25519`, `ssh-rsa`, `ecdsa-sha2-nistp256`, `sk-ssh-ed25519@openssh.com`…
    pub kind: String,
    pub bits: u32,
    pub fingerprint: String,
    pub comment: String,
    /// The line has options (`restrict`, `command=`, `from=`…).
    pub options: bool,
    /// A type OpenSSH no longer accepts (`ssh-dss`) or an RSA key under 2048 bits.
    pub weak: bool,
}

/// `GET /services/ssh/keys/{user}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SshKeyList {
    pub user: String,
    pub keys: Vec<SshKey>,
    /// Lines that are not keys (kept as they are).
    pub other_lines: u32,
}

/// `POST /services/ssh/keys/{user}`: one public key line as `ssh-keygen` writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshKeyAdd {
    pub key: String,
}

/// `POST /services/ssh/keys/{user}/remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshKeyRemove {
    pub fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshLoginKind {
    /// Logged in (`method` says how).
    Accepted,
    /// Wrong password or key for an existing account.
    Failed,
    /// An account that does not exist.
    InvalidUser,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshLoginEvent {
    pub at_ms: u64,
    pub kind: SshLoginKind,
    pub user: String,
    pub address: String,
    /// `publickey`, `password`, `keyboard-interactive`… (empty when not said).
    #[serde(default)]
    pub method: String,
}

/// `GET /services/ssh/logins`: the last day from the journal, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SshLogins {
    pub events: Vec<SshLoginEvent>,
    pub failed: u32,
    pub accepted: u32,
    /// Addresses with the most failures, `(address, count)`.
    pub top_failed: Vec<(String, u32)>,
    /// The day held more journal lines than are read: the counts start here (ms), not 24
    /// hours ago. `None`: the whole day is counted.
    #[serde(default)]
    pub since_ms: Option<u64>,
}

/// `AllowGroups` of [`SshAllow::Listed`]: the base groups, then the administrator's.
pub fn allowed_groups(s: &SshSettings) -> Vec<String> {
    let mut v: Vec<String> = SSH_BASE_GROUPS.iter().map(|g| g.to_string()).collect();
    for g in &s.allow_groups {
        if !v.contains(g) {
            v.push(g.clone());
        }
    }
    v
}

/// Does an account in `groups` pass the allow list of `s` (sshd's `AllowGroups` sees the
/// primary group too)?
pub fn passes(groups: &[String], s: &SshSettings) -> bool {
    match s.allow {
        SshAllow::Everyone => true,
        SshAllow::Listed => {
            let allowed = allowed_groups(s);
            groups.iter().any(|g| allowed.contains(g))
        }
    }
}

/// Can `a` open a shell as an administrator under `s`? The lockout guard (server) and the
/// preview of the page use this one rule.
pub fn admin_can_log_in(a: &SshAccount, s: &SshSettings) -> bool {
    if !s.enabled || !a.admin || a.level != SshLevel::Shell || !passes(&a.groups, s) {
        return false;
    }
    if a.uid == 0 {
        match s.root_login {
            SshRootLogin::No => false,
            SshRootLogin::Keys => a.keys > 0,
            SshRootLogin::Yes => a.keys > 0 || s.password_auth,
        }
    } else {
        a.keys > 0 || s.password_auth
    }
}
