use std::collections::HashSet;
use std::os::fd::FromRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use nix::sys::stat::{Mode, umask};
use nix::unistd::{Group, Uid, User, chown};
use rynas_auth::SystemClock;
use rynas_auth::ipc::DEFAULT_SOCKET;
use rynas_auth::totp::{FileStorage, TotpManager};
use rynas_btrfs::{HostSnapshots, MemorySnapshots, SnapshotHost, SnapshotsDomain};
use rynas_config::{Config, ConfigOptions, HostSystem, HostnameDomain, MemorySystem, System};
use rynas_notify::Notifier;
use rynas_notify::engine::{Engine, EngineOptions};
use rynas_scheduler::SchedulerDomain;
use rynas_scheduler::history::History;
use rynas_scheduler::probe::HostProbe;
use rynas_scheduler::units::Paths;
use rynas_services::{
    Accounts, HostAccounts, HostShares, MemoryAccounts, MemoryShares, ShareHost, SharesDomain,
    UsersDomain,
};
use rynas_tasks::{TaskConfig, TaskManager};
use rynasd::ops::Ops;
use rynasd::pam::{PamAuthenticator, resolve_service};
use rynasd::protection::Protection;
use rynasd::server::{DaemonState, PeerCheckedListener, router};
use rynasd::services::Services;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Unix socket to listen on (ignored under systemd socket activation).
    #[arg(long, default_value = DEFAULT_SOCKET)]
    socket: PathBuf,
    /// Group owning the socket (when running as root).
    #[arg(long, default_value = "rynas")]
    socket_group: String,
    /// Users allowed to connect [default: "rynas" when root, else the current user].
    #[arg(long = "allow-user")]
    allow_users: Vec<String>,
    /// Uids allowed to connect, in addition to --allow-user.
    #[arg(long = "allow-uid")]
    allow_uids: Vec<u32>,
    /// Private state: TOTP secrets (auth/totp/), config.db, tasks/, backup/.
    #[arg(long, default_value = "/var/lib/rynas")]
    state_dir: PathBuf,
    /// Root below which configuration files are written [default: "/", or
    /// <state-dir>/dry-root with --dry-system].
    #[arg(long)]
    config_root: Option<PathBuf>,
    /// Development: never change the running system. Files go below the config root,
    /// hostname and service reloads are only logged.
    #[arg(long)]
    dry_system: bool,
    /// Background tasks running at the same time.
    #[arg(long, default_value_t = 4)]
    max_tasks: usize,
    /// PAM service; "login" is used when /etc/pam.d/<service> does not exist.
    #[arg(long, default_value = "rynas")]
    pam_service: String,
    /// TOTP issuer shown in authenticator apps.
    #[arg(long, default_value = "rynas")]
    totp_issuer: String,
    /// Tests only: attached loop devices count as disks (write side on loop files).
    #[arg(long, hide = true)]
    storage_loop_disks: bool,
    /// Screenshots only (with --dry-system): fake removable drives below --hw-root.
    #[arg(long, hide = true)]
    removable_demo: bool,
    /// Socket on which a hardware module's tools forward their notifications (root only),
    /// opened only when a module provides a forward; empty to disable.
    #[arg(long, default_value = "/run/rynas/notify.sock", value_parser = path_or_empty)]
    notify_socket: PathBuf,
    /// Seconds between storage health checks (disks, arrays, btrfs errors, volume usage);
    /// 0 disables them.
    #[arg(long, default_value_t = 300)]
    storage_check_interval: u64,
    /// Root of /sys, /proc and /run for the hardware module and the power
    /// schedule (tests: a fake tree). Also passed to the generated rynas-power-wake.service.
    #[arg(long, default_value = "/")]
    hw_root: PathBuf,
    /// Tests only: the command a scheduled shutdown runs instead of `systemctl poweroff`.
    #[arg(long, hide = true, num_args = 1.., value_delimiter = ' ')]
    poweroff_command: Vec<String>,
    /// Tests only: the command run instead of systemd's `shutdown` for a manual restart or
    /// shut down (the arguments follow it).
    #[arg(long, hide = true, num_args = 1.., value_delimiter = ' ')]
    shutdown_command: Vec<String>,
    /// Members of these groups are administrators (repeatable; same rule as rynas-web's
    /// --admin-group): only they may stage or run user tasks that run as root.
    #[arg(long = "admin-group", default_values = ["rynas-admin", "sudo"])]
    admin_groups: Vec<String>,
    /// The file manager worker (default: rynas-files-worker next to this binary).
    #[arg(long)]
    files_worker: Option<PathBuf>,
    /// Thumbnail caches of the file manager, one folder per user.
    #[arg(long, default_value = "/var/cache/rynas/files")]
    files_cache: PathBuf,
    /// Port of the web interface (Avahi announcement, candidate URLs of network changes).
    #[arg(long, default_value_t = 8080)]
    web_port: u16,
    /// The web interface is served over HTTPS.
    #[arg(long)]
    web_https: bool,
    /// NUT's upsd (battery tests, the on-battery watcher) [default: 127.0.0.1:3493]. With
    /// --dry-system rynasd talks to upsd only when this is given (a test upsd).
    #[arg(long)]
    upsd: Option<String>,
    /// The Docker engine's socket; empty: rynasd never contacts the engine. With
    /// --dry-system the engine is only read (lists, status), never changed.
    #[arg(long, default_value = rynas_docker::DOCKER_SOCKET, value_parser = path_or_empty)]
    docker_socket: PathBuf,
    /// The app catalogue (`apps/<id>/app.toml`...): the `rynas-apps-catalog` package's
    /// folder; a checkout's `apps/` for development.
    #[arg(long, default_value = rynasd::apps::DEFAULT_CATALOG_DIR)]
    apps_dir: PathBuf,
    /// The admin socket of the web access proxy (rynas-caddy). The proxy counts as installed
    /// when its unit file exists below the configuration root or this socket exists; with
    /// --dry-system that means only a test proxy started on purpose.
    #[arg(long, default_value = rynasd::web_access::ADMIN_SOCKET)]
    caddy_admin: PathBuf,
    /// rynas-web's own HTTPS port (W0), the emergency door the proxy never routes; the
    /// proxy redirects plain HTTP for unknown names there.
    #[arg(long, default_value_t = 8443)]
    web_tls_port: u16,
    /// tailscaled's LocalAPI socket; empty: never contacted. With --dry-system only a socket
    /// given explicitly (a test double) is read; the default one is never contacted.
    #[arg(long, value_parser = path_or_empty)]
    tailscale_socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Write a .rynasbak archive of the configuration and the notification store (secrets
    /// only with a passphrase; the scheduler's run history is not included).
    Export {
        /// Output file.
        #[arg(long)]
        out: PathBuf,
        /// File whose first line is the passphrase that encrypts the secrets (TOTP, the
        /// notification key).
        #[arg(long)]
        passphrase_file: Option<PathBuf>,
    },
    /// Stage the configuration of a .rynasbak archive as pending changes (apply them from
    /// the web interface or the API after reviewing the diff).
    Import {
        file: PathBuf,
        /// File whose first line is the passphrase of the secrets.
        #[arg(long)]
        passphrase_file: Option<PathBuf>,
        /// Owner of the pending changeset: the user who will review and apply it.
        #[arg(long, default_value = "root")]
        owner: String,
        /// Also write the archived secrets (TOTP) that do not exist here yet.
        #[arg(long)]
        restore_secrets: bool,
        /// Replace the notification store (targets, rules, history) with the archived one,
        /// and its key when the archive has it (needs the passphrase). rynasd must be
        /// stopped.
        #[arg(long)]
        restore_notifications: bool,
    },
    /// Make rynas's own HTTPS certificate of the web interface active again, with a new
    /// leaf for the current host name and addresses (after an imported certificate that
    /// clients refuse). rynas-web picks it up within seconds, without a restart.
    TlsReset,
}

/// The certificate service: private store in the state folder, published files for
/// rynas-web below the configuration root.
fn tls_service(args: &Args) -> Arc<rynasd::tls::TlsSvc> {
    rynasd::tls::TlsSvc::new(
        args.state_dir.join("tls"),
        config_root(args).join(rynasd::tls::PUBLISH_DIR),
        (!args.dry_system).then_some("rynas"),
        rynasd::tls::TlsSvc::system_probe(),
    )
}

fn config_root(args: &Args) -> PathBuf {
    match (&args.config_root, args.dry_system) {
        (Some(r), _) => r.clone(),
        (None, true) => args.state_dir.join("dry-root"),
        (None, false) => PathBuf::from("/"),
    }
}

fn running_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".into())
}

/// A dry root starts as a copy of the real files it manages.
fn seed_dry_root(root: &Path) -> std::io::Result<()> {
    for f in ["etc/hostname", "etc/hosts"] {
        let dst = root.join(f);
        if !dst.exists() {
            std::fs::create_dir_all(dst.parent().expect("has a parent"))?;
            let _ = std::fs::copy(Path::new("/").join(f), &dst);
        }
    }
    Ok(())
}

/// The configuration store with its domains, and typed handles on the services domains.
struct Opened {
    config: Config,
    users: Arc<UsersDomain>,
    shares: Arc<SharesDomain>,
    homes: Arc<rynas_services::HomesDomain>,
    scheduler: Arc<SchedulerDomain>,
    snapshots: Arc<SnapshotsDomain>,
    /// The hardware modules of this machine (D-MODULE; none on a plain Debian).
    modules: rynas_module::Modules,
    /// Applied `removable` settings, handed over by the domain.
    removable_settings: tokio::sync::watch::Receiver<rynas_api_types::removable::RemovableSettings>,
    /// Applied `docker` settings, handed over by the domain.
    docker_settings: tokio::sync::watch::Receiver<rynas_api_types::docker::DockerSettings>,
    /// The configuration root.
    root: PathBuf,
    /// The certificate service (the local CA is shared with web access).
    tls: Arc<rynasd::tls::TlsSvc>,
    /// Web access (W1): the `web_access` key's service.
    web_access: Arc<rynasd::web_access::WebAccessSvc>,
}

fn open_config(
    args: &Args,
    system: Arc<dyn System>,
    storage: Option<Arc<rynas_storage::Storage>>,
) -> std::io::Result<Opened> {
    let root = config_root(args);
    if args.dry_system {
        seed_dry_root(&root)?;
    }
    // --dry-system: accounts start as a copy of the host's and change in memory only; paths
    // are probed on the host but ACLs are only logged
    let dry_shares = args.dry_system.then(|| Arc::new(MemoryShares::probing_host()));
    let (accounts, host): (Arc<dyn Accounts>, Arc<dyn ShareHost>) = match &dry_shares {
        Some(m) => (Arc::new(MemoryAccounts::from_host()), m.clone()),
        None => (Arc::new(HostAccounts::new("/")), Arc::new(HostShares)),
    };
    // --dry-system: the homes folder, its link and the homes are kept in memory, paths
    // unknown there are probed on the host (read-only)
    let homes_host: Arc<dyn rynas_services::HomesHost> = if args.dry_system {
        Arc::new(rynas_services::MemoryHomes::probing_host())
    } else {
        Arc::new(rynas_services::HostHomes)
    };
    let homes = Arc::new(rynas_services::HomesDomain::new(
        homes_host.clone(),
        accounts.clone(),
    ));
    // --dry-system: the ownership remap reads the host and changes nothing
    let remap: Arc<dyn rynas_services::idremap::RemapFs> = if args.dry_system {
        Arc::new(rynas_services::idremap::DryRemap)
    } else {
        Arc::new(rynas_services::idremap::HostRemap)
    };
    let users = Arc::new(
        UsersDomain::new(accounts.clone())
            .with_homes(homes_host)
            .with_remap(remap),
    );
    let subvolumes: Arc<dyn rynas_services::SubvolumeCreator> = Arc::new(rynasd::storage_ops::HostSubvolumes {
        dry: args.dry_system,
        storage,
    });
    // encrypted folders (D-CRYPT E1): in memory on a dry system (gocryptfs "installed",
    // folders appear in the in-memory share host)
    let crypt: Arc<dyn rynas_services::crypt::CryptHost> = match &dry_shares {
        Some(m) => Arc::new(rynas_services::crypt::MemoryCrypt::new(true).with_shares(m.clone())),
        None => Arc::new(rynas_services::crypt::HostCrypt::new(Some(subvolumes.clone()), &args.state_dir)),
    };
    let shares = Arc::new(
        SharesDomain::new(host, accounts)
            .with_subvolumes(subvolumes)
            .with_removed(Arc::new(rynas_services::removed::RemovedShares::in_dir(
                &args.state_dir,
            )))
            .with_crypt(crypt),
    );

    // --dry-system: subvolumes probed on the host (read-only), snapshot directories logged
    let snap_host: Arc<dyn SnapshotHost> = if args.dry_system {
        Arc::new(MemorySnapshots::probing_host())
    } else {
        Arc::new(HostSnapshots)
    };
    let snapshots = Arc::new(SnapshotsDomain::new(snap_host));
    let scheduler = Arc::new(SchedulerDomain::new(
        Paths {
            scripts: args.state_dir.join("tasks"),
            rynasctl: PathBuf::from(rynas_btrfs::domain::RYNASCTL),
        },
        Arc::new(rynas_scheduler::domain::system_uid),
    ));
    // hardware modules (D-MODULE): registered only where their hardware or system layer is
    let modules = rynasd::modules::detect(&args.hw_root, &root, args.dry_system);
    for m in modules.iter() {
        tracing::info!("module {}", m.summary());
    }
    let mut domains: Vec<Arc<dyn rynas_config::Applier>> = vec![
        Arc::new(HostnameDomain),
        homes.clone(),
        users.clone(),
        shares.clone(),
        Arc::new(rynas_storage::domain::VolumesDomain),
        Arc::new(rynas_storage::domain::TuningDomain),
        snapshots.clone(),
        Arc::new(rynas_backup::BackupDomain),
        Arc::new(rynas_dedup::DedupDomain::with_removed(Arc::new(rynasd::dedup::job_removed))),
        scheduler.clone(),
        Arc::new(rynas_power::PowerDomain {
            rynasctl: PathBuf::from(rynas_btrfs::domain::RYNASCTL),
            state_dir: args.state_dir.clone(),
            hw_root: args.hw_root.clone(),
            alarm_reader: modules.wake_clock().map(|c| c.read_by_unit.to_string()),
        }),
        Arc::new(rynas_net::rescue::RescueDomain {
            web_port: args.web_port,
            https: args.web_https,
        }),
        Arc::new(rynas_updates::AutoDomain),
    ];
    domains.extend(modules.domains());
    // ups.safemode: a module layer's policy file, else the rynas policy file read by rynasd's
    // NUT watcher
    let ups_layer = modules.ups();
    domains.push(Arc::new(rynasd::ups::UpsDomain {
        layer: ups_layer.clone(),
    }));
    // ups.device: the UPS set up with the wizard (rynas blocks of /etc/nut, or the layer's)
    domains.push(Arc::new(rynasd::ups_setup::DeviceDomain {
        layer: ups_layer.clone(),
    }));
    // ups.netserver: upsd for other machines (LISTEN, accounts, IP filter; not with a layer)
    domains.push(Arc::new(rynasd::ups_netserver::NetServerDomain {
        secret_dir: args.state_dir.join("notify"),
        probe: (!args.dry_system).then(rynasd::ups_netserver::tcp_probe),
        // a layer's own tool: simulated under --dry-system (its state in the dry root)
        runner: if args.dry_system {
            Arc::new(rynasd::ups_setup::DryRunner {
                layer: ups_layer.clone(),
                ..Default::default()
            })
        } else {
            Arc::new(rynasd::ups_setup::HostRunner)
        },
        layer: ups_layer,
    }));
    let (removable_domain, removable_settings) = rynasd::removable::RemovableDomain::new();
    let removable_tx = removable_domain.tx.clone();
    domains.push(Arc::new(removable_domain));
    let (docker_domain, docker_settings) = rynasd::docker::DockerDomain::new(args.dry_system);
    let docker_tx = docker_domain.tx.clone();
    domains.push(Arc::new(docker_domain));
    // web access (W1): the proxy's certificate comes from the same local CA as :8443
    let tls = tls_service(args);
    let web_access = {
        let tls2 = tls.clone();
        let mut svc = rynasd::web_access::WebAccessSvc::new(
            root.clone(),
            args.caddy_admin.clone(),
            args.web_port,
            args.web_tls_port,
            rynasd::web_access::WebAccessSvc::system_facts(),
            Box::new(move || tls2.local_ca()),
            Box::new(rynasd::web_access::diag::SystemProbe),
        );
        svc.state_dir = args.state_dir.clone();
        if args.dry_system {
            // DNS providers are never called in development
            svc.dns = rynasd::web_access::domains::offline_factory();
        }
        let _ = svc.tls.set(tls.clone());
        Arc::new(svc)
    };
    domains.push(Arc::new(rynasd::web_access::WebAccessDomain {
        svc: web_access.clone(),
    }));
    let config = Config::open(ConfigOptions::new(&args.state_dir, &root), system, domains)
        .map_err(std::io::Error::other)?;
    removable_tx.send_replace(rynasd::removable::initial_settings(&config));
    docker_tx.send_replace(rynasd::docker::initial_settings(&config));
    // homes of the first layout (/srv/rynas/homes one link to <volume>/homes): one link per
    // home, so that each home can be on its own volume (the apply of `homes` does it too)
    if let Err(e) = homes.migrate(&|l| tracing::info!("homes: {l}")) {
        tracing::warn!("homes: the folder of links could not be set up: {e}");
    }
    Ok(Opened {
        config,
        users,
        shares,
        homes,
        scheduler,
        snapshots,
        modules,
        removable_settings,
        docker_settings,
        root,
        tls,
        web_access,
    })
}

fn read_passphrase(path: &Option<PathBuf>) -> std::io::Result<Option<String>> {
    let Some(p) = path else { return Ok(None) };
    let text = std::fs::read_to_string(p)?;
    let line = text.lines().next().unwrap_or("").to_string();
    if line.is_empty() {
        return Err(std::io::Error::other("empty passphrase"));
    }
    Ok(Some(line))
}

fn totp_dir(args: &Args) -> PathBuf {
    args.state_dir.join("auth").join("totp")
}

fn tls_reset(args: &Args) -> std::io::Result<()> {
    let tls = tls_service(args);
    tls.ensure(true, true)?;
    let st = tls
        .status()
        .map_err(|e| std::io::Error::other(format!("{e:?}")))?;
    println!(
        "rynas certificate active: {} (SHA-256 {})",
        st.certificate.subject, st.certificate.fingerprint_sha256
    );
    Ok(())
}

fn run_command(args: &Args, cmd: &Command) -> std::io::Result<()> {
    // the CLI never applies anything: an in-memory system is enough
    if let Command::TlsReset = cmd {
        return tls_reset(args);
    }
    let cfg = open_config(args, Arc::new(MemorySystem::new(&running_hostname())), None)?.config;
    match cmd {
        Command::TlsReset => unreachable!("handled above"),
        Command::Export {
            out,
            passphrase_file,
        } => {
            let pass = read_passphrase(passphrase_file)?;
            let r = rynasd::backup::export(
                &cfg,
                &args.state_dir,
                out,
                &running_hostname(),
                pass.as_deref(),
            )?;
            println!(
                "{}: {} keys{}, {}",
                out.display(),
                r.manifest.keys.len(),
                if r.manifest.databases.is_empty() {
                    String::new()
                } else {
                    format!(" + {}", r.manifest.databases.join(", "))
                },
                if r.manifest.secrets {
                    format!("{} secrets encrypted", r.secrets)
                } else {
                    "no secrets (no passphrase given)".into()
                }
            );
        }
        Command::Import {
            file,
            passphrase_file,
            owner,
            restore_secrets,
            restore_notifications,
        } => {
            if *restore_notifications && daemon_running(&args.socket) {
                return Err(std::io::Error::other(
                    "rynasd is running: stop it first (systemctl stop rynasd) to restore the notification store",
                ));
            }
            let pass = read_passphrase(passphrase_file)?;
            let opts = rynasd::backup::ImportOptions {
                owner: owner.clone(),
                restore_secrets: *restore_secrets,
                restore_notifications: *restore_notifications,
                admins: rynasd::roles::AdminPolicy::new(
                    args.admin_groups.clone(),
                    rynasd::groups::user_groups,
                ),
            };
            let r = rynasd::backup::import(&cfg, &args.state_dir, file, pass.as_deref(), &opts)?;
            println!(
                "{}: archive of {} (rynas {}), {} keys staged for {owner}: review and apply them",
                file.display(),
                r.manifest.hostname,
                r.manifest.rynas_version,
                r.staged
            );
            match (r.totp_in_archive, *restore_secrets) {
                (0, _) if r.manifest.secrets && pass.is_none() => {
                    println!("the archive has secrets: give --passphrase-file to read them")
                }
                (0, _) => {}
                (_, true) => println!("{} TOTP secrets restored", r.totp_restored),
                (n, false) => {
                    println!(
                        "{n} TOTP secrets in the archive (use --restore-secrets to write them)"
                    )
                }
            }
            use rynasd::backup::NotifyRestore;
            match r.notifications {
                NotifyRestore::NotInArchive => {}
                NotifyRestore::Skipped => println!(
                    "the archive has the notification store (use --restore-notifications, with rynasd stopped)"
                ),
                NotifyRestore::Restored { key, kept } => {
                    println!("notification store restored");
                    if !key {
                        println!(
                            "warning: the archive has no notification key (no passphrase): enter the targets' passwords and tokens again"
                        );
                    }
                    for k in kept {
                        println!("previous file kept as {}", k.display());
                    }
                }
            }
        }
    }
    Ok(())
}

/// Whether a rynasd answers on `socket`.
fn daemon_running(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

fn init_logging() {
    use std::io::IsTerminal;
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::INFO);
    // journald adds its own timestamps
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let b = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .with_writer(std::io::stderr);
    if journald {
        b.without_time().init();
    } else {
        b.init();
    }
}

/// The listening socket passed by systemd (`rynasd.socket`), if any.
fn systemd_listener() -> Option<std::os::unix::net::UnixListener> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    let fds: u32 = std::env::var("LISTEN_FDS").ok()?.parse().ok()?;
    if pid != std::process::id() || fds < 1 {
        return None;
    }
    // SAFETY: called from `main` before any other thread exists.
    unsafe {
        std::env::remove_var("LISTEN_PID");
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
    }
    // SAFETY: systemd hands over fd 3 (SD_LISTEN_FDS_START) as an open listening socket that
    // nothing else in this process owns.
    Some(unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) })
}

fn bind_socket(path: &Path, group: &str) -> std::io::Result<std::os::unix::net::UnixListener> {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if m.file_type().is_socket() {
            std::fs::remove_file(path)?;
        } else {
            return Err(std::io::Error::other(format!(
                "{} exists and is not a socket",
                path.display()
            )));
        }
    }
    // no window in which the socket is accessible to others
    let old = umask(Mode::from_bits_truncate(0o117));
    let l = std::os::unix::net::UnixListener::bind(path);
    umask(old);
    let l = l?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    if Uid::effective().is_root() {
        match Group::from_name(group) {
            Ok(Some(g)) => chown(path, None, Some(g.gid)).map_err(std::io::Error::from)?,
            _ => tracing::warn!("group {group} not found: socket left to root only"),
        }
    }
    Ok(l)
}

/// A path argument where "" is allowed and means "disabled" (clap's PathBuf parser refuses
/// empty values).
fn path_or_empty(s: &str) -> Result<PathBuf, std::convert::Infallible> {
    Ok(PathBuf::from(s))
}

/// The module forward socket: 0600, owned by rynasd's user (root in production).
fn bind_notify_socket(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if m.file_type().is_socket() {
            std::fs::remove_file(path)?;
        } else {
            return Err(std::io::Error::other("exists and is not a socket"));
        }
    }
    let old = umask(Mode::from_bits_truncate(0o177));
    let l = std::os::unix::net::UnixListener::bind(path);
    umask(old);
    let l = l?;
    l.set_nonblocking(true)?;
    tokio::net::UnixListener::from_std(l)
}

fn allowed_uids(args: &Args) -> HashSet<u32> {
    let mut uids: HashSet<u32> = args.allow_uids.iter().copied().collect();
    let me = Uid::effective();
    let mut names = args.allow_users.clone();
    if names.is_empty() && uids.is_empty() {
        if me.is_root() {
            names.push("rynas".into());
        } else {
            uids.insert(me.as_raw());
        }
    }
    for n in &names {
        match User::from_name(n) {
            Ok(Some(u)) => {
                uids.insert(u.uid.as_raw());
            }
            _ => tracing::warn!("user {n} not found: not allowed"),
        }
    }
    // root can do anything anyway
    uids.insert(0);
    uids.insert(me.as_raw());
    uids
}

/// glibc malloc tuning, the same as rynas-web's (guidelines M5): two arenas, a fixed mmap
/// threshold (large transient buffers such as a 1 MiB task-log chunk or a big JSON answer are
/// mapped and unmapped instead of staying in the heap) and a 1 MiB trim threshold.
fn tune_malloc() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: plain configuration calls, made before any other thread exists.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 << 10);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 1 << 20);
    }
}

fn main() -> std::io::Result<()> {
    tune_malloc();
    let activated = systemd_listener();
    let args = Args::parse();
    init_logging();

    if let Some(cmd) = &args.command {
        return run_command(&args, cmd);
    }

    let allowed = allowed_uids(&args);
    let storage = FileStorage::new(totp_dir(&args))?;
    let service = resolve_service(&args.pam_service);
    if !Uid::effective().is_root() {
        tracing::warn!("not running as root: PAM can only verify the password of the current user");
    }

    // One event-loop thread (handlers only dispatch); the blocking pool runs PAM, SQLite,
    // files, D-Bus and storage tools. Blocking threads get 1 MiB of stack, not tokio's 2 MiB:
    // PAM/NSS modules and libsqlite3 are third-party C code, so not the 512 KiB of rynas-web.
    // Storage calls also start their own short-lived threads (one per disk for SMART).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .thread_keep_alive(std::time::Duration::from_secs(10))
        .thread_stack_size(1 << 20)
        .build()?;
    rt.block_on(async move {
        let own_socket = activated.is_none();
        let std_listener = match activated {
            Some(l) => l,
            None => bind_socket(&args.socket, &args.socket_group)?,
        };
        std_listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(std_listener)?;
        let clock = Arc::new(SystemClock);
        let totp = Arc::new(TotpManager::new(
            storage,
            clock.clone(),
            args.totp_issuer.clone(),
        ));
        let mut state = DaemonState::new(
            Arc::new(PamAuthenticator::new(service.clone())),
            totp,
            clock,
        );
        let system: Arc<dyn System> = if args.dry_system {
            Arc::new(MemorySystem::new(&running_hostname()))
        } else {
            if !Uid::effective().is_root() {
                tracing::warn!("not running as root: configuration applies will fail (use --dry-system for development)");
            }
            Arc::new(HostSystem::new(tokio::runtime::Handle::current()))
        };
        if args.storage_loop_disks {
            tracing::warn!("loop devices count as disks (--storage-loop-disks): tests only");
            let mut s = rynas_storage::Storage::new(Arc::new(rynas_storage::host::SystemHost));
            s.loop_disks = true;
            state.storage = Arc::new(s);
        }
        // SMART warnings marked as seen, per drive: survive restarts
        state
            .storage
            .health_acks()
            .open(&rynasd::storage_ops::health_acks_file(&args.state_dir));
        // the storage's power states keep the folder sizes from spinning up sleeping disks
        let opened = open_config(&args, system, Some(state.storage.clone()))?;
        let config = Arc::new(opened.config);
        let tasks = TaskManager::new(
            args.state_dir.join("tasks"),
            &running_hostname(),
            TaskConfig {
                max_concurrent: args.max_tasks,
                ..Default::default()
            },
        )?;
        let mut ops = Ops::new(config, tasks);
        ops.admins = rynasd::roles::AdminPolicy::new(args.admin_groups.clone(), state.groups);
        // applied disk settings: read APM and write cache again (the SMART cache is 10 min)
        let st = state.storage.clone();
        ops.after_apply = Some(Arc::new(move |keys, log| {
            if keys.iter().any(|k| k == rynas_storage::domain::TUNING_KEY) {
                let read = st.refresh_disk_settings();
                if !read.is_empty() {
                    log(&format!("disk settings read again: {}", read.join(", ")));
                }
            }
        }));
        let ops = Arc::new(ops);
        state.services = Some(Arc::new(Services {
            ops: ops.clone(),
            users: opened.users,
            shares: opened.shares.clone(),
            homes: opened.homes,
            moves: Arc::new(rynas_services::homes::MoveStore::in_dir(&args.state_dir)),
        }));
        state.storage_ops = Some(Arc::new(rynasd::storage_ops::StorageOps::new(
            ops.clone(),
            state.storage.clone(),
            state.storage_lock.clone(),
            args.dry_system,
        )));
        // notifications: engine, its loops, rynasd's own sources, a module's forward socket
        let mut nopts = EngineOptions::new(args.state_dir.join("notify"), &running_hostname());
        nopts.imports = opened.modules.notify_imports();
        let engine = Engine::open(nopts)?;
        tokio::spawn(engine.clone().run());
        let notifier: Arc<dyn Notifier> = Arc::new(engine.clone());
        rynasd::notifications::spawn_task_watch(&ops.tasks, notifier.clone());
        state.notifier = notifier.clone();
        opened.modules.attach(notifier.clone());
        // encrypted folders whose password is sealed on this machine (D-CRYPT E1); a failed
        // automatic unlock is notified
        rynasd::services::crypt::set_notifier(notifier.clone());
        if let Some(s) = &state.services {
            rynasd::services::crypt::auto_unlock_at_start(s);
        }
        // web access (W2): DNS credentials are sealed with the engine's key, certificate
        // trouble goes to the engine
        let _ = opened.web_access.notifier.set(notifier.clone());
        match rynas_notify::engine::secrets::SecretBox::open(&args.state_dir.join("notify")) {
            Ok(b) => {
                let _ = opened.web_access.secret_box.set(Arc::new(b));
            }
            Err(e) => tracing::warn!("web access: no secret store for DNS credentials: {e}"),
        }
        state.notifications = Some(engine.clone());
        let history = History::open(&args.state_dir.join("scheduler.db"))
            .map_err(std::io::Error::other)?;
        // a root rollback mounts in the host's namespace: what one interrupted by a restart
        // of rynasd left mounted below /run/rynas/rollback goes away now
        if !args.dry_system {
            tokio::task::spawn_blocking(|| {
                let dir = Path::new(rynas_btrfs::rootfs::RUN_DIR);
                if let Err(e) = rynas_btrfs::rootfs::clear_mounts(dir, &|m| tracing::warn!("rollback: {m}")) {
                    tracing::error!("rollback: leftover mounts below {}: {e}", dir.display());
                }
            });
        }
        let protection = Protection::new(
            ops.clone(),
            opened.scheduler,
            opened.snapshots,
            Arc::new(HostProbe),
            Arc::new(history),
            notifier.clone(),
            args.state_dir.clone(),
        )
        // root rollback: the machine, or (--dry-system) only logged; the boot half is a
        // module's boot layer (none: refused, a console procedure)
        .with_root(if args.dry_system {
            rynasd::protection::RootBackend::Custom(Arc::new(rynas_btrfs::rootfs::DryRoot::new()))
        } else {
            rynasd::protection::RootBackend::Host
        })
        .with_replicas(if args.dry_system {
            Arc::new(rynas_btrfs::replication::MemoryReplicas::new())
        } else {
            Arc::new(rynas_btrfs::replication::HostReplicas)
        })
        .with_boot(
            opened
                .modules
                .boot()
                .map(|b| b as Arc<dyn rynas_btrfs::rootfs::BootSync>),
        );
        let protection = Arc::new(protection);
        state.protection = Some(protection.clone());
        // backups (D-BACKUP): the worker in a transient unit of its own; --dry-system or
        // RYNAS_BACKUP_WORKER (development, the UI audit) start it as a plain child
        {
            let launch = match std::env::var_os("RYNAS_BACKUP_WORKER") {
                Some(exe) => rynas_backup::Launch::Direct { exe: PathBuf::from(exe) },
                None if args.dry_system => rynas_backup::Launch::Direct {
                    exe: PathBuf::from(rynas_backup::WORKER),
                },
                None => rynas_backup::Launch::unit(PathBuf::from(rynas_backup::WORKER)),
            };
            let svc = rynasd::restic_backup::ResticSvc::new(
                protection.clone(),
                launch,
                args.dry_system,
                args.state_dir.join("notify"),
            );
            let _ = rynasd::restic_backup::SVC.set(svc.clone());
            // runs a stop cut short: leftover worker units, stopped stacks, temporary
            // snapshots, downloads in flight; then the files of removed jobs
            tokio::spawn(rynasd::restic_backup::recover(svc.clone()));
            state.restic = Some(svc);
        }
        // deduplication (D-DEDUP): the engine in transient units; --dry-system plays
        // scripted runs on the places it sees
        {
            let host: Arc<dyn rynasd::dedup::DedupHost> = if args.dry_system {
                let mi = rynasd::places::mountinfo();
                let places = rynasd::places::places_in(&mi)
                    .into_iter()
                    .map(|(l, d)| (l, d, "btrfs".to_string()))
                    .collect();
                Arc::new(rynasd::dedup::MemoryDedup::new(true, places))
            } else {
                Arc::new(rynasd::dedup::HostDedup::new(opened.shares.clone()))
            };
            let svc = Arc::new(rynasd::dedup::DedupSvc::new(
                protection.clone(),
                host,
                Arc::new(rynas_dedup::Oans::default()),
                args.state_dir.join("dedup"),
                opened.modules.dedup_excludes(),
                rynasd::fslock::FsLocks::global(),
            ));
            let _ = rynasd::dedup::SVC.set(svc.clone());
            // runs left by a previous rynasd: stopped before anything takes the lock
            svc.recover();
            state.dedup = Some(svc);
        }
        let ups = Arc::new(rynasd::ups::Ups {
            ops: ops.clone(),
            protection: Some(protection.clone()),
            notifier: notifier.clone(),
            layer: opened.modules.ups(),
            root: opened.root.clone(),
            dry: args.dry_system,
            upsd: args
                .upsd
                .clone()
                .unwrap_or_else(|| rynas_nut::UPSD.to_string()),
            // --dry-system never talks to the host's own upsd
            talk_to_upsd: !args.dry_system || args.upsd.is_some(),
            // --dry-system: the wizard's scans and test runs are simulated
            setup: rynasd::ups_setup::Setup::new(if args.dry_system {
                Arc::new(rynasd::ups_setup::DryRunner {
                    layer: opened.modules.ups(),
                    version_file: Some(rynas_config::files::under_root(
                        &opened.root,
                        std::path::Path::new(rynasd::ups_setup::DRY_NUT_VERSION_FILE),
                    )),
                    ..Default::default()
                })
            } else {
                Arc::new(rynasd::ups_setup::HostRunner)
            }),
            net_secret_dir: args.state_dir.join("notify"),
        });
        // NUT backend: rynasd follows the UPS itself (a module layer does it on its own)
        if ups.layer.is_none() && ups.talk_to_upsd {
            tokio::spawn(rynasd::ups::watch(ups.clone()));
        }
        state.ups = Some(ups);
        state.hardware = Some(Arc::new(rynasd::hardware::Hardware {
            ops: ops.clone(),
            control: opened.modules.hardware_control(),
            wake_rtc: opened.modules.wake_clock().map(|c| c.rtc_device_suffix),
            hw_root: args.hw_root.clone(),
            state_dir: args.state_dir.clone(),
            notifier: notifier.clone(),
            protection: Some(protection),
            poweroff: if args.poweroff_command.is_empty() {
                vec!["systemctl".into(), "poweroff".into()]
            } else {
                args.poweroff_command.clone()
            },
        }));
        // networking (D-NET): the transaction engine on the same root as the configuration
        let net_paths = rynas_net::NetPaths::new(
            args.state_dir.join("net"),
            if args.dry_system {
                args.state_dir.join("run")
            } else {
                PathBuf::from("/run/rynas")
            },
        );
        let net_engine = Arc::new(
            rynas_net::Engine::new(
                Arc::new(rynas_net::HostOs::new(config_root(&args), args.dry_system)),
                net_paths,
            )
            .with_web(rynas_net::txn::WebInfo {
                port: args.web_port,
                https: args.web_https,
            })
            .with_feedback(opened.modules.rescue_feedback()),
        );
        let network = Arc::new(
            rynasd::network::Network::new(ops.clone(), net_engine, notifier.clone(), args.dry_system)
                .with_modules(opened.modules.clone()),
        );
        rynasd::network::spawn_event_flush(network.clone());
        state.network = Some(network);
        // the hardware modules' routes (/v1/modules/<id>/...)
        state.modules = opened.modules.router(&rynas_module::http::Ctx {
            config: ops.config.clone(),
            hw_root: args.hw_root.clone(),
            state_dir: args.state_dir.clone(),
            ack_hardware: {
                let engine = engine.clone();
                Arc::new(move |who: &str| rynasd::hardware::ack_hardware_alerts(&engine, who))
            },
        });
        // rescue paths on by default (the Avahi file); the modules' hooks and settings
        {
            let cfg = ops.config.clone();
            let modules = opened.modules.clone();
            tokio::task::spawn_blocking(move || {
                let key = rynas_net::rescue::RESCUE_KEY;
                if matches!(cfg.applied(key), Ok(None)) {
                    let v = serde_json::to_value(
                        rynas_api_types::network::RescueSettings::default(),
                    )
                    .expect("serializable");
                    if let Err(e) =
                        cfg.apply_value("rynas", key, Some(v), "rescue paths on by default", &|_| {})
                    {
                        tracing::warn!("rescue paths: {e}");
                    }
                }
                modules.start(&cfg, &|l| tracing::info!("{l}"));
            });
        }
        let updates = Arc::new(
            rynasd::updates::Updates::new(
                ops.clone(),
                notifier.clone(),
                args.dry_system,
                config_root(&args),
            )
            .with_boot(opened.modules.boot())
            .with_system_layer(opened.modules.boot().is_some() || opened.modules.ups().is_some()),
        );
        rynasd::updates::spawn_daily(updates.clone());
        state.updates = Some(updates.clone());
        state.zram = Some(Arc::new(rynasd::zram::ZramApi {
            zram: Arc::new(rynasd::zram::Zram::new(config_root(&args), args.dry_system)),
            ops: ops.clone(),
        }));
        // date, time and time zone; the sync state is watched for notifications
        let clock = Arc::new(rynasd::clock::Clock::new(config_root(&args), args.dry_system));
        rynasd::clock::spawn_watch(clock.clone(), notifier.clone());
        state.clock = Some(Arc::new(rynasd::clock::ClockApi { clock, ops: ops.clone() }));
        // manual restart and shut down: systemd's shutdown (never under --dry-system)
        if let Some(hw) = state.hardware.clone() {
            state.system_power = Some(Arc::new(rynasd::system_power::SystemPower {
                hw,
                updates: Some(updates),
                upsd: (!args.dry_system || args.upsd.is_some()).then(|| {
                    args.upsd
                        .clone()
                        .unwrap_or_else(|| rynas_nut::UPSD.to_string())
                }),
                shutdown: if !args.shutdown_command.is_empty() {
                    args.shutdown_command.clone()
                } else if args.dry_system {
                    Vec::new()
                } else {
                    vec!["shutdown".into()]
                },
                grace: std::time::Duration::from_secs(3),
                admins: ops.admins.clone(),
            }));
        }
        // PC backup, receive side: the machine, or (--dry-system) memory with one folder of
        // received snapshots to adopt (/volume1/backup/old-laptop)
        let pc_host: Arc<dyn rynasd::pc_backup::ReceiveHost> = if args.dry_system {
            Arc::new(rynasd::pc_backup::MemoryReceive::demo())
        } else {
            Arc::new(rynasd::pc_backup::HostReceive)
        };
        let pc_backup = rynasd::pc_backup::PcBackup::new(
            ops.clone(),
            pc_host,
            notifier.clone(),
            args.state_dir.clone(),
            ops.admins.clone(),
            running_hostname(),
        );
        rynasd::pc_backup::spawn_watch(pc_backup.clone());
        state.pc_backup = Some(pc_backup);
        // PC backup, file method (agent, restic repositories per backup group): the same
        // worker launch and worker slots as the NAS backups
        if let Some(rs) = state.restic.as_ref() {
            let agent = rynasd::pc_agent::PcAgent::new(
                ops.clone(),
                notifier.clone(),
                args.state_dir.clone(),
                args.dry_system,
                rs.launch.clone(),
                rs.workers.clone(),
                Some(opened.tls.clone()),
                running_hostname(),
            );
            rynasd::pc_agent::maint::spawn_watch(agent.clone());
            state.pc_agent = Some(agent);
        }
        // the SSH service: the machine, or (--dry-system) a seeded dry root and a stand-in
        // for sshd and systemctl
        {
            let root = config_root(&args);
            let runner: Arc<dyn rynas_services::ssh::SshRunner> = if args.dry_system {
                if let Err(e) = rynas_services::ssh::fake::seed(&root) {
                    tracing::warn!("ssh: dry root: {e}");
                }
                Arc::new(rynas_services::ssh::fake::FakeRunner::new(&args.state_dir, &root))
            } else {
                Arc::new(rynas_services::ssh::HostRunner)
            };
            // homes on a sleeping disk: the page shows their keys unread instead of waking it
            let mut ssh = rynas_services::ssh::Ssh::new(root, &args.state_dir, runner);
            if let Some(services) = state.services.as_ref() {
                let shares = services.shares.clone();
                ssh = ssh.with_asleep(Arc::new(move |p: &str| rynasd::services::path_asleep(&shares, p)));
            }
            let svc = Arc::new(rynasd::ssh::SshSvc {
                ssh: Arc::new(ssh),
                admins: ops.admins.clone(),
            });
            rynasd::ssh::spawn_watch(svc.clone());
            state.ssh = Some(svc);
        }
        // the file manager: workers next to rynasd unless told otherwise
        let worker = args.files_worker.clone().unwrap_or_else(|| {
            std::env::current_exe()
                .map(|e| e.with_file_name("rynas-files-worker"))
                .unwrap_or_else(|_| PathBuf::from("/usr/libexec/rynas/rynas-files-worker"))
        });
        let cache = if args.dry_system {
            args.state_dir.join("cache/files")
        } else {
            args.files_cache.clone()
        };
        // the workers in a cgroup of their own (rynasd::cgroup), so that a big job does not
        // throttle rynasd under its own memory.high
        if !args.dry_system {
            match rynasd::cgroup::split_own() {
                Ok((base, procs)) => {
                    rynas_files::jobs::manager::set_worker_cgroup(procs);
                    tracing::info!("file manager workers in {}/{}", base.display(), rynasd::cgroup::WORKERS);
                }
                Err(e) => tracing::info!("file manager workers share rynasd's cgroup: {e}"),
            }
        }
        let files = rynasd::files::Files::new(ops.clone(), worker, &args.state_dir.join("files"), &cache);
        // removable drives: read below --hw-root; --dry-system only logs the actions
        {
            if args.removable_demo && args.dry_system && args.hw_root != Path::new("/") {
                tracing::warn!("fake removable drives below {} (--removable-demo)", args.hw_root.display());
                rynas_removable::fixture::Fixture::demo(&args.hw_root);
            }
            let host: Arc<dyn rynas_removable::host::Host> = if args.dry_system {
                Arc::new(rynas_removable::host::DryHost::new(&args.hw_root))
            } else {
                Arc::new(rynas_removable::host::SystemHost::new(&args.hw_root))
            };
            let samba = rynas_hw::under(&config_root(&args), "/etc/samba").is_dir();
            let svc = Arc::new(rynas_removable::Removable::new(
                host,
                Some(rynasd::removable::names_file(&args.state_dir)),
            ));
            let system_usb_ids = opened.modules.system_disk_usb_ids();
            let removable = rynasd::removable::RemovableSvc::new(
                ops.clone(),
                svc,
                notifier.clone(),
                opened.removable_settings.clone(),
                samba,
                system_usb_ids,
            );
            let _ = ops.removable.set(removable.clone());
            rynasd::removable::spawn_watch(removable.clone());
            // the file manager lists mounted removable volumes and ejects their drives
            state.files = Some(Arc::new(
                files.with_drives(Arc::new(rynasd::removable::FilesDrives::new(removable.clone()))),
            ));
            state.removable = Some(removable);
        }
        // Docker: status, stacks, tasks; the event watcher and the stats sampler
        {
            let socket = (!args.docker_socket.as_os_str().is_empty())
                .then(|| args.docker_socket.to_string_lossy().into_owned());
            let docker = rynasd::docker::DockerSvc::new(
                ops.clone(),
                notifier.clone(),
                config_root(&args),
                args.dry_system,
                socket,
                args.state_dir.join("docker"),
                opened.docker_settings.clone(),
            );
            let _ = ops.docker.set(docker.clone());
            rynasd::docker::spawn_watch(docker.clone());
            rynasd::docker::spawn_stats(docker.clone());
            let apps = rynasd::apps::AppsSvc::new(docker.clone(), args.apps_dir.clone());
            let _ = apps.web.set(opened.web_access.clone());
            // the daily image check and the automatic update window (installed apps too)
            let _ = docker.apps.set(Arc::downgrade(&apps));
            rynasd::docker_auto::spawn(docker.clone());
            // signed online catalogue updates: daily on a real system ("Check now" always)
            let _ = apps.online.set(rynasd::apps::online::Online::new(
                args.state_dir.join("catalog"),
                rynas_apps::online::TRUSTED_KEYS,
            ));
            if !args.dry_system {
                apps.spawn_catalog_updates();
            }
            // the catalogue is parsed on the first apps request and dropped when idle
            {
                let apps = apps.clone();
                tokio::spawn(async move {
                    let mut t = tokio::time::interval(std::time::Duration::from_secs(300));
                    loop {
                        t.tick().await;
                        if apps.trim_idle(std::time::Duration::from_secs(600)) {
                            tracing::debug!("apps: idle catalogue dropped");
                        }
                    }
                });
            }
            state.apps = Some(apps);
            state.docker = Some(docker);
        }
        // processes: sampled only while a page asks (D-PROCESSES)
        {
            let p = rynasd::processes::Processes::new(args.dry_system, state.docker.clone(), state.apps.clone());
            rynasd::processes::spawn_trim(p.clone());
            state.processes = Some(Arc::new(rynasd::processes::ProcessesApi { processes: p, ops: ops.clone() }));
        }
        // Tailscale (remote access): status from tailscaled's LocalAPI, tasks for the rest
        {
            let socket = match (&args.tailscale_socket, args.dry_system) {
                (Some(p), _) if p.as_os_str().is_empty() => None,
                (Some(p), _) => Some(p.clone()),
                (None, true) => None,
                (None, false) => Some(PathBuf::from(rynas_tailscale::SOCKET)),
            };
            let ts = rynasd::tailscale::TailscaleSvc::new(
                ops.clone(),
                config_root(&args),
                args.dry_system,
                socket,
                args.state_dir.join("tailscale"),
                PathBuf::from("/"),
            );
            // an option change left waiting by the previous rynasd: its window goes on
            ts.resume_pending();
            state.tailscale = Some(ts);
        }
        // HTTPS certificate of the web interface: made on the first start, re-issued when the
        // host name or an address changes, published for rynas-web (wave W0)
        {
            let tls = opened.tls.clone();
            rynasd::tls::spawn_watch(tls.clone());
            state.tls = Some(tls);
        }
        // web access (W1): the proxy's configuration from the `web_access` key, anti-drift,
        // host facts (names, subnets) followed
        {
            let web = opened.web_access.clone();
            let _ = web.ops.set(ops.clone());
            // local app names through Avahi's D-Bus API (never under --dry-system)
            if !args.dry_system {
                web.names.spawn();
            }
            rynasd::web_access::spawn_watch(web.clone());
            // watched folders of imported certificates (§5.5)
            rynasd::web_access::imported::spawn_watch(web.clone());
            state.web_access = Some(web);
        }
        state.ops = Some(ops);
        if args.storage_check_interval > 0 {
            rynasd::notifications::spawn_storage_watch(
                state.clone(),
                notifier.clone(),
                std::time::Duration::from_secs(args.storage_check_interval),
                opened.modules.clone(),
            );
        }
        let forward = opened.modules.event_forward();
        if let Some(forward) = forward.filter(|_| !args.notify_socket.as_os_str().is_empty()) {
            match bind_notify_socket(&args.notify_socket) {
                Ok(l) => {
                    // the module's tools run as root
                    let uids: HashSet<u32> = [0, Uid::effective().as_raw()].into();
                    let app = rynasd::notifications::socket_router(engine, forward);
                    tokio::spawn(async move {
                        if let Err(e) = axum::serve(PeerCheckedListener::new(l, uids), app).await {
                            tracing::error!("notify socket: {e}");
                        }
                    });
                }
                Err(e) => tracing::warn!(
                    "notify socket {}: {e} (module forwards disabled)",
                    args.notify_socket.display()
                ),
            }
        }
        let mut uids: Vec<_> = allowed.iter().copied().collect();
        uids.sort();
        tracing::info!(
            "rynasd {} on {} (PAM service {service}, state {}, config root {}{}, allowed uids {uids:?})",
            env!("CARGO_PKG_VERSION"),
            listener
                .local_addr()
                .ok()
                .and_then(|a| a.as_pathname().map(|p| p.display().to_string()))
                .unwrap_or_else(|| "systemd socket".into()),
            args.state_dir.display(),
            config_root(&args).display(),
            if args.dry_system { ", dry system" } else { "" },
        );
        // with the peer credentials of each connection: root-only routes check them
        // event streams (rynas-web's /events proxy) never hang up by themselves: graceful
        // shutdown waits for them, so give open connections a moment and then exit (without
        // this systemd waits TimeoutStopSec and kills the daemon: a "failed" unit on restart)
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let signal = async move {
            shutdown_signal().await;
            let _ = stop_tx.send(());
        };
        let server = rynasd::server::serve(PeerCheckedListener::new(listener, allowed), router(state), signal);
        tokio::select! {
            r = server => r?,
            _ = async {
                if stop_rx.await.is_ok() {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => tracing::info!("shutdown: closing open streams"),
        }
        opened.modules.stop();
        if own_socket {
            let _ = std::fs::remove_file(&args.socket);
        }
        if !args.notify_socket.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&args.notify_socket);
        }
        Ok(())
    })
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}
