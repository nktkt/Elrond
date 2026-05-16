//! Elrond — a Rust-native Nginx alternative.

mod access;
mod app;
mod auth;
mod auth_request;
mod body;
mod cache;
mod config;
mod gzip;
mod health;
mod http3;
mod http_date;
mod limit;
mod logging;
mod metrics;
mod mirror;
#[cfg(unix)]
mod privilege;
mod proxy;
mod request_ctx;
#[cfg(unix)]
mod sd_notify;
mod server;
mod static_files;
mod stream;
mod supervisor;
mod template;
mod tls;

use std::path::PathBuf;
use std::process::ExitCode;

use tracing::{error, info, warn};

use crate::supervisor::Supervisor;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() -> ExitCode {
    let mut config_path = PathBuf::from("elrond.conf");
    let mut test_only = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-c" | "--config" => match args.next() {
                Some(p) => config_path = PathBuf::from(p),
                None => {
                    eprintln!("error: {arg} requires a path argument");
                    return ExitCode::from(2);
                }
            },
            "-t" | "--test" => test_only = true,
            "-v" | "--version" => {
                println!("elrond {VERSION}");
                return ExitCode::SUCCESS;
            }
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unknown argument '{other}'\n");
                print_help();
                return ExitCode::from(2);
            }
        }
    }

    // Config is parsed *before* logging so we can route logs to the files
    // the operator named (`access_log`, `error_log`). Parse errors fall
    // back to stderr.
    let cfg = match config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("elrond: config error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let access_log_path: Option<PathBuf> = cfg
        .http
        .as_ref()
        .and_then(|h| h.access_log.clone())
        .map(PathBuf::from);
    let error_log_path: Option<PathBuf> = cfg.error_log.clone().map(PathBuf::from);

    // `-t` is a config syntax check — never touch the log files. This lets
    // operators validate a config from a workstation without prepping log
    // directories that only exist on the deployment host.
    if !test_only {
        if let Err(e) =
            logging::install(access_log_path.as_deref(), error_log_path.as_deref())
        {
            eprintln!("elrond: failed to install logging: {e}");
            return ExitCode::FAILURE;
        }
    }

    metrics::init();
    tls::install_crypto_provider();

    let runtime = match app::build(&cfg) {
        Ok(rt) => rt,
        Err(e) => {
            if test_only {
                eprintln!("elrond: config error: {e}");
            } else {
                error!("config error: {e}");
            }
            return ExitCode::FAILURE;
        }
    };

    if test_only {
        println!(
            "config '{}' is valid: {} http + {} stream server block(s)",
            config_path.display(),
            runtime.listeners.len(),
            runtime.stream_servers.len()
        );
        return ExitCode::SUCCESS;
    }

    info!("elrond {VERSION} starting (pid {})", std::process::id());

    // Write the PID file if one was requested.
    let pid_path: Option<PathBuf> = cfg.pid.clone().map(PathBuf::from);
    if let Some(path) = &pid_path {
        match std::fs::write(path, format!("{}\n", std::process::id())) {
            Ok(()) => info!("wrote PID file '{}'", path.display()),
            Err(e) => warn!("could not write PID file '{}': {e}", path.display()),
        }
    }

    if let Some(wp) = &cfg.worker_processes {
        info!("worker_processes {wp} (single-process, multi-threaded)");
    }

    // Raise RLIMIT_NOFILE *before* binding sockets so a high-fanout
    // listener doesn't run into EMFILE the moment traffic arrives.
    // Only root can lift the *hard* cap; non-root processes silently
    // clamp to the existing hard limit, which is the same behavior
    // Nginx ships.
    #[cfg(unix)]
    if let Some(target) = cfg.worker_rlimit_nofile {
        match privilege::raise_nofile(target) {
            Ok((old, new)) if old == new => {
                info!(
                    "worker_rlimit_nofile {target}: already at soft limit {old} \
                     (hard cap reached or higher than request)"
                );
            }
            Ok((old, new)) => {
                info!("worker_rlimit_nofile: raised soft limit {old} → {new}");
            }
            Err(e) => warn!("worker_rlimit_nofile: {e} (continuing)"),
        }
    }

    // Resolve the drop-target user *before* `Supervisor::start` so
    // misconfigurations fail fast — better to error out before binding
    // anything than to find out we can't drop after the listeners are
    // up and the PID file is written.
    #[cfg(unix)]
    let drop_target = if let Some(user) = &cfg.user {
        match privilege::resolve(user, cfg.group.as_deref()) {
            Ok(ids) => Some(ids),
            Err(e) => {
                error!("'user' directive: {e}");
                cleanup_pid_file(pid_path.as_deref());
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };

    let mut supervisor = match Supervisor::start(config_path.clone(), runtime).await {
        Ok(s) => s,
        Err(e) => {
            error!("failed to start listeners: {e}");
            cleanup_pid_file(pid_path.as_deref());
            return ExitCode::FAILURE;
        }
    };

    // Drop privileges *after* sockets are bound (so :80/:443 worked
    // while we were root) and *after* the PID file is written (so the
    // PID file lives at its intended ownership). Log files are already
    // open via inherited file descriptors and survive the drop.
    #[cfg(unix)]
    if let Some(ids) = drop_target.as_ref() {
        let was_root = privilege::is_root();
        match privilege::drop_to(ids) {
            Ok(()) => info!(
                "dropped privileges to uid={} gid={} (was root: {was_root})",
                ids.uid, ids.gid
            ),
            Err(e) => {
                error!(
                    "could not drop privileges to uid={} gid={}: {e}",
                    ids.uid, ids.gid
                );
                // Refuse to keep running as a privileged process when
                // the operator explicitly asked for a non-root identity.
                supervisor.shutdown().await;
                cleanup_pid_file(pid_path.as_deref());
                return ExitCode::FAILURE;
            }
        }
    } else if privilege::is_root() {
        warn!(
            "running as root and no 'user' directive set — \
             consider 'user nobody;' (or run as a non-root user) for production"
        );
    }

    info!(
        "elrond ready; {} listener(s); SIGHUP reloads config, \
         SIGUSR1 reopens logs, SIGINT/SIGTERM shuts down",
        supervisor.listener_count()
    );
    #[cfg(unix)]
    {
        sd_notify::status(&format!(
            "ready: {} listener(s)",
            supervisor.listener_count()
        ));
        sd_notify::ready();
    }

    wait_for_signals(&mut supervisor).await;

    info!("shutting down; draining listeners");
    #[cfg(unix)]
    sd_notify::stopping();
    supervisor.shutdown().await;
    info!("elrond stopped");
    cleanup_pid_file(pid_path.as_deref());
    ExitCode::SUCCESS
}

fn cleanup_pid_file(path: Option<&std::path::Path>) {
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(unix)]
async fn wait_for_signals(supervisor: &mut Supervisor) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut hup = signal(SignalKind::hangup()).ok();
    let mut term = signal(SignalKind::terminate()).ok();
    let mut usr1 = signal(SignalKind::user_defined1()).ok();
    if hup.is_none() {
        warn!("could not install SIGHUP handler");
    }
    if term.is_none() {
        warn!("could not install SIGTERM handler");
    }
    if usr1.is_none() {
        warn!("could not install SIGUSR1 handler");
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("SIGINT received");
                break;
            }
            _ = async { term.as_mut().unwrap().recv().await }, if term.is_some() => {
                info!("SIGTERM received");
                break;
            }
            _ = async { hup.as_mut().unwrap().recv().await }, if hup.is_some() => {
                info!("SIGHUP received");
                sd_notify::reloading();
                supervisor.reload().await;
                sd_notify::status(&format!(
                    "reloaded: {} listener(s)",
                    supervisor.listener_count()
                ));
                sd_notify::ready();
            }
            _ = async { usr1.as_mut().unwrap().recv().await }, if usr1.is_some() => {
                info!("SIGUSR1 received — reopening log files");
                logging::reopen_all();
            }
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signals(_supervisor: &mut Supervisor) {
    match tokio::signal::ctrl_c().await {
        Ok(()) => info!("shutdown signal received"),
        Err(e) => error!("failed to listen for shutdown signal: {e}"),
    }
}

fn print_help() {
    println!("elrond {VERSION} - a Rust-native Nginx alternative");
    println!();
    println!("USAGE:");
    println!("    elrond [-c <config>] [-t] [-v] [-h]");
    println!();
    println!("OPTIONS:");
    println!("    -c, --config <path>   Configuration file (default: elrond.conf)");
    println!("    -t, --test            Validate the configuration and exit");
    println!("    -v, --version         Print version and exit");
    println!("    -h, --help            Print this help and exit");
    println!();
    println!("SIGNALS (Unix):");
    println!("    SIGHUP                Re-read the configuration file");
    println!("    SIGUSR1               Reopen access_log / error_log files");
    println!("    SIGINT, SIGTERM       Graceful shutdown");
    println!();
    println!("ENVIRONMENT:");
    println!("    ELROND_LOG            Log filter, e.g. 'info', 'debug' (default: info)");
    println!("    NOTIFY_SOCKET         systemd notify socket (auto-detected when present)");
}
