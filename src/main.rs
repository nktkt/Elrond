//! Elrond — a Rust-native Nginx alternative.

mod app;
mod body;
mod config;
mod http_date;
mod proxy;
mod request_ctx;
mod server;
mod static_files;
mod supervisor;
mod template;
mod tls;

use std::path::PathBuf;
use std::process::ExitCode;

use tracing::{error, info, warn};

use crate::supervisor::Supervisor;

const VERSION: &str = env!("CARGO_PKG_VERSION");

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

    init_tracing();
    tls::install_crypto_provider();

    let cfg = match config::load(&config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match app::build(&cfg) {
        Ok(rt) => rt,
        Err(e) => {
            error!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if test_only {
        println!(
            "config '{}' is valid: {} server block(s)",
            config_path.display(),
            runtime.servers.len()
        );
        return ExitCode::SUCCESS;
    }

    info!("elrond {VERSION} starting (pid {})", std::process::id());
    if let Some(wp) = &cfg.worker_processes {
        info!("worker_processes {wp} (single-process, multi-threaded)");
    }

    let mut supervisor = match Supervisor::start(config_path.clone(), runtime).await {
        Ok(s) => s,
        Err(e) => {
            error!("failed to start listeners: {e}");
            return ExitCode::FAILURE;
        }
    };
    info!(
        "elrond ready; {} listener(s); SIGHUP reloads config, SIGINT/SIGTERM shuts down",
        supervisor.listener_count()
    );

    wait_for_signals(&mut supervisor).await;

    info!("shutting down; draining listeners");
    supervisor.shutdown().await;
    info!("elrond stopped");
    ExitCode::SUCCESS
}

#[cfg(unix)]
async fn wait_for_signals(supervisor: &mut Supervisor) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut hup = match signal(SignalKind::hangup()) {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("could not install SIGHUP handler: {e}");
            None
        }
    };
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("could not install SIGTERM handler: {e}");
            None
        }
    };

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
                supervisor.reload().await;
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

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("ELROND_LOG")
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("the fallback filter 'info' is always valid");
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
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
    println!("    SIGINT, SIGTERM       Graceful shutdown");
    println!();
    println!("ENVIRONMENT:");
    println!("    ELROND_LOG            Log filter, e.g. 'info', 'debug' (default: info)");
}
