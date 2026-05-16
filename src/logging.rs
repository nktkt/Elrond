//! File-based access / error logs with `SIGUSR1` reopen support.
//!
//! Two streams are routed independently:
//! - `target: "access"` records → access log (default stdout)
//! - everything else → error log (default stderr)
//!
//! Both writers can be backed by a file that is **re-opened** when
//! `logging::reopen_all()` is called from the `SIGUSR1` handler. This is
//! the integration point for `logrotate`'s `copytruncate`-free workflow:
//!
//! ```text
//! /etc/logrotate.d/elrond:
//!   /var/log/elrond/*.log {
//!       daily
//!       missingok
//!       rotate 14
//!       postrotate
//!           /bin/kill -USR1 $(cat /run/elrond.pid)
//!       endscript
//!   }
//! ```

use std::fs::{File, OpenOptions};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::filter::FilterFn;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Registry};

/// A file handle that can be replaced atomically — the writer side just
/// locks a `Mutex<File>`, so a `reopen()` from any thread swaps the
/// underlying descriptor for *subsequent* writes without disturbing the
/// caller mid-write.
#[derive(Clone)]
pub struct ReopenLog {
    path: PathBuf,
    file: Arc<Mutex<File>>,
}

impl ReopenLog {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let file = open_append(&path)?;
        Ok(Self {
            path,
            file: Arc::new(Mutex::new(file)),
        })
    }

    pub fn reopen(&self) -> io::Result<()> {
        let new = open_append(&self.path)?;
        *self
            .file
            .lock()
            .expect("log file mutex poisoned") = new;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// The actual `io::Write` handed to `tracing`'s fmt layer per event.
pub struct ReopenLogWriter {
    file: Arc<Mutex<File>>,
}

impl Write for ReopenLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file
            .lock()
            .expect("log file mutex poisoned")
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .lock()
            .expect("log file mutex poisoned")
            .flush()
    }
}

impl<'a> MakeWriter<'a> for ReopenLog {
    type Writer = ReopenLogWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ReopenLogWriter {
            file: self.file.clone(),
        }
    }
}

static REOPENABLE: OnceLock<Mutex<Vec<ReopenLog>>> = OnceLock::new();

fn register(log: ReopenLog) {
    REOPENABLE
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap()
        .push(log);
}

/// Re-open every registered file. Called from the `SIGUSR1` handler.
pub fn reopen_all() {
    if let Some(m) = REOPENABLE.get() {
        for log in m.lock().unwrap().iter() {
            if let Err(e) = log.reopen() {
                eprintln!(
                    "elrond: failed to reopen log '{}': {e}",
                    log.path.display()
                );
            }
        }
    }
}

/// Initialize the global subscriber. Routes the `access` target to
/// `access_log` (or stdout) and everything else to `error_log` (or
/// stderr).
pub fn install(access_log: Option<&Path>, error_log: Option<&Path>) -> io::Result<()> {
    let env_filter = EnvFilter::try_from_env("ELROND_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let access_writer: BoxMakeWriter = match access_log {
        Some(p) => {
            let log = ReopenLog::new(p.to_path_buf())?;
            register(log.clone());
            BoxMakeWriter::new(log)
        }
        None => BoxMakeWriter::new(io::stdout),
    };
    let error_writer: BoxMakeWriter = match error_log {
        Some(p) => {
            let log = ReopenLog::new(p.to_path_buf())?;
            register(log.clone());
            BoxMakeWriter::new(log)
        }
        None => BoxMakeWriter::new(io::stderr),
    };

    let access_layer = fmt::layer()
        .with_writer(access_writer)
        .with_ansi(false)
        .with_target(false)
        .with_filter(FilterFn::new(|m| m.target() == "access"));

    let error_layer = fmt::layer()
        .with_writer(error_writer)
        .with_ansi(false)
        .with_filter(FilterFn::new(|m| m.target() != "access"));

    let _ = Registry::default()
        .with(env_filter)
        .with(access_layer)
        .with(error_layer)
        .try_init();

    Ok(())
}
