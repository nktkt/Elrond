//! Elrond — a Rust-native Nginx alternative.
//!
//! v0.1.0: Nginx-style config, an HTTP/1.1 server, prefix routing, static file
//! serving, a reverse proxy with round-robin load balancing, and graceful
//! shutdown.

mod app;
mod body;
mod config;
mod proxy;
mod server;
mod static_files;

use std::path::PathBuf;
use std::process::ExitCode;

use tokio::sync::watch;
use tracing::{error, info};

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

    // Load and validate the configuration.
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

    info!("elrond {VERSION} starting");
    if let Some(wp) = &cfg.worker_processes {
        info!("worker_processes {wp} (v0.1.0 runs single-process, multi-threaded)");
    }

    // Spawn one task per listener; they share a shutdown signal.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut handles = Vec::new();
    for (addr, state) in runtime.servers {
        let rx = shutdown_rx.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = server::run(addr, state, rx).await {
                error!("listener on {addr} failed: {e}");
            }
        }));
    }
    drop(shutdown_rx);

    match tokio::signal::ctrl_c().await {
        Ok(()) => info!("shutdown signal received; draining"),
        Err(e) => error!("failed to listen for shutdown signal: {e}"),
    }
    let _ = shutdown_tx.send(true);

    for handle in handles {
        let _ = handle.await;
    }
    info!("elrond stopped");
    ExitCode::SUCCESS
}

/// Initialize `tracing`. Verbosity is controlled by the `ELROND_LOG`
/// environment variable (e.g. `ELROND_LOG=debug`); defaults to `info`.
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
    println!("ENVIRONMENT:");
    println!("    ELROND_LOG            Log filter, e.g. 'info', 'debug' (default: info)");
}
