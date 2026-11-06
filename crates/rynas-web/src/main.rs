use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use rynas_auth::backend::{RolePolicy, TotpBackend};
use rynas_auth::realm::{Realm, RealmAuthenticator};
use rynas_auth::session::SessionConfig;
use rynas_auth::totp::{FileStorage, MemoryStorage, TotpManager};
use rynas_auth::{AuthConfig, AuthService, Backend, SystemClock};
use rynas_web::app::{self, AppState};
use rynas_web::metrics::{Collector, Store};
use rynas_web::rynasd_client::RynasdClient;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Directory of the built web interface
    /// [default: crates/rynas-ui/dist if present, else /usr/share/rynas/ui].
    #[arg(long)]
    ui_dir: Option<PathBuf>,
    /// Root of `/sys` and `/run` where the hardware provider is looked for (tests: a fake
    /// tree).
    #[arg(long, default_value = "/")]
    hw_root: PathBuf,
    /// NUT upsd (UPS status, plain-text protocol).
    #[arg(long, default_value = rynas_web::ups::UPSD)]
    upsd: String,
    /// Socket of the root daemon rynasd (PAM users and their TOTP secrets).
    #[arg(long, default_value = rynas_auth::ipc::DEFAULT_SOCKET)]
    rynasd_socket: PathBuf,
    /// Internal realm: TOML file of web-only users with argon2 hashes (development, tests).
    /// Tried before PAM. Generate hashes with `rynas-web hash-password`.
    #[arg(long)]
    users_file: Option<PathBuf>,
    /// Directory for the TOTP secrets of --users-file users [default: kept in memory].
    #[arg(long)]
    auth_state_dir: Option<PathBuf>,
    /// Always mark the session cookie Secure (behind an HTTPS reverse proxy).
    #[arg(long)]
    secure_cookies: bool,
    /// Members of these groups are administrators (repeatable).
    #[arg(long = "admin-group", default_values = ["rynas-admin", "sudo"])]
    admin_groups: Vec<String>,
    /// Session idle timeout in seconds.
    #[arg(long, default_value_t = 1800)]
    session_idle_timeout: u64,
    /// Session absolute lifetime in seconds.
    #[arg(long, default_value_t = 43200)]
    session_max_age: u64,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Read a password from stdin and print its argon2id hash for --users-file.
    HashPassword,
}

/// Read one line from stdin, without echo when it is a terminal.
fn read_password() -> std::io::Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    let tty = stdin.is_terminal();
    // SAFETY: termios calls on fd 0 with a zeroed struct filled by tcgetattr.
    let saved = tty.then(|| unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut t) != 0 {
            return None;
        }
        let saved = t;
        t.c_lflag &= !libc::ECHO;
        libc::tcsetattr(0, libc::TCSANOW, &t);
        Some(saved)
    });
    if tty {
        eprint!("Password: ");
        std::io::stderr().flush()?;
    }
    let mut line = String::new();
    let r = stdin.lock().read_line(&mut line);
    if let Some(Some(t)) = saved {
        // SAFETY: restores the attributes read above.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        eprintln!();
    }
    r?;
    Ok(line.trim_end_matches(['\n', '\r']).to_owned())
}

fn hash_password() -> std::io::Result<()> {
    let pw = read_password()?;
    if pw.is_empty() {
        return Err(std::io::Error::other("empty password"));
    }
    let h = rynas_auth::realm::hash_password(&pw).map_err(std::io::Error::other)?;
    println!("{h}");
    Ok(())
}

fn auth_service(args: &Args, rynasd: Arc<RynasdClient>) -> std::io::Result<Arc<AuthService>> {
    let clock = Arc::new(SystemClock);
    let mut backends = Vec::new();
    if let Some(path) = &args.users_file {
        let realm = Realm::load(path).map_err(std::io::Error::other)?;
        tracing::info!(
            "internal realm: {} users from {}",
            realm.len(),
            path.display()
        );
        let totp: Arc<dyn TotpBackend> = match &args.auth_state_dir {
            Some(dir) => Arc::new(TotpManager::new(
                FileStorage::new(dir.join("totp"))?,
                clock.clone(),
                "rynas",
            )),
            None => {
                tracing::warn!("no --auth-state-dir: TOTP of internal users is lost on restart");
                Arc::new(TotpManager::new(
                    MemoryStorage::default(),
                    clock.clone(),
                    "rynas",
                ))
            }
        };
        backends.push(Backend {
            authenticator: Arc::new(RealmAuthenticator::new(realm)),
            totp,
        });
    }
    backends.push(Backend {
        authenticator: rynasd.clone(),
        totp: rynasd,
    });
    let cfg = AuthConfig {
        session: SessionConfig {
            idle_timeout: Duration::from_secs(args.session_idle_timeout),
            absolute_timeout: Duration::from_secs(args.session_max_age),
            ..Default::default()
        },
        policy: RolePolicy {
            admin_groups: args.admin_groups.clone(),
        },
        host: rynas_web::system::system_info().hostname,
        ..Default::default()
    };
    Ok(Arc::new(AuthService::with_clock(backends, cfg, clock)))
}

fn default_ui_dir() -> PathBuf {
    let dev = PathBuf::from("crates/rynas-ui/dist");
    if dev.is_dir() {
        dev
    } else {
        PathBuf::from("/usr/share/rynas/ui")
    }
}

/// glibc malloc tuning (guidelines M5), before any other thread exists:
/// - one arena per allocating thread (up to 8 x cores) is far more than this process needs;
/// - a *fixed* mmap threshold: otherwise glibc raises it after the first large free, and from
///   then on every large transient buffer (argon2's 19 MiB, a compression window, a big JSON
///   answer) lands in the heap and stays resident forever (measured: 6 MB -> 65 MB after five
///   logins, see docs/quality/review-2026-11-05.md, C1);
/// - free memory at the top of the heap is returned above 1 MiB.
///
/// The units also set `GLIBC_TUNABLES` with the same values (for binaries started by hand
/// the calls below are what counts).
fn tune_malloc() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: plain configuration calls, made before any other thread exists.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 << 10);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 1 << 20);
    }
}

fn init_logging() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::INFO);
    use std::io::IsTerminal;
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

fn main() -> std::io::Result<()> {
    tune_malloc();
    let args = Args::parse();
    if let Some(Command::HashPassword) = args.command {
        return hash_password();
    }
    init_logging();

    // One event-loop thread is enough: handlers do microseconds of work, the collector has
    // its own thread and file system reads go to the blocking pool. A single worker means a
    // single stack and malloc arena, which is what keeps the idle footprint low.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .thread_keep_alive(Duration::from_secs(10))
        .thread_stack_size(512 * 1024)
        .build()?;

    rt.block_on(async move {
        let ui_dir = args.ui_dir.clone().unwrap_or_else(default_ui_dir);
        let store = Store::new();
        let hw = rynas_web::hardware::detect(&args.hw_root);
        Collector::new(store.clone(), hw.clone()).spawn()?;
        let rynasd = Arc::new(RynasdClient::new(&args.rynasd_socket));
        let auth = auth_service(&args, rynasd.clone())?;
        rynas_web::auth::spawn_session_reaper(auth.clone());
        let state = AppState {
            store,
            hw,
            auth,
            notifier: rynas_web::notify::WebNotifier::new(rynasd.clone()),
            rynasd,
            mounts: rynas_web::storage::MountProber::new(),
            secure_cookies: args.secure_cookies,
            streams: Default::default(),
            ups: rynas_web::ups::UpsMonitor::new(&args.upsd),
        };
        state.ups.spawn();
        rynas_web::notify::spawn_hardware_watch(state.clone());
        let app = app::router(state, ui_dir.clone());

        let listener = tokio::net::TcpListener::bind(args.listen).await?;
        tracing::info!(
            "rynas-web {} on http://{} (ui: {}, hardware root: {}, rynasd: {})",
            env!("CARGO_PKG_VERSION"),
            listener.local_addr()?,
            ui_dir.display(),
            args.hw_root.display(),
            args.rynasd_socket.display()
        );
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<rynas_web::conn::ConnAddrs>(),
        )
        .with_graceful_shutdown(shutdown_signal());
        // SSE clients never hang up by themselves: do not wait for them more than a moment
        tokio::select! {
            r = server.into_future() => r,
            _ = async { shutdown_signal().await; tokio::time::sleep(Duration::from_secs(2)).await } => {
                tracing::info!("shutdown: closing open streams");
                Ok(())
            }
        }
    })
}
