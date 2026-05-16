//! Minimal `sd_notify` for systemd `Type=notify` integration.
//!
//! When `$NOTIFY_SOCKET` is set, send a datagram with the given state
//! string (`READY=1`, `STOPPING=1`, `RELOADING=1\nMONOTONIC_USEC=…`,
//! `STATUS=…`). When the env var is not set (running interactively, in a
//! container without notify, on macOS for development), every call is a
//! cheap no-op — no panics, no errors propagated.

#![cfg(unix)]

use std::os::unix::net::UnixDatagram;

/// Tell systemd we're ready to serve.
pub fn ready() {
    send("READY=1");
}

/// Tell systemd we're shutting down.
pub fn stopping() {
    send("STOPPING=1");
}

/// Tell systemd a reload is in progress. Should be followed by another
/// `ready()` when the reload completes.
pub fn reloading() {
    send("RELOADING=1");
}

/// Set a short human-readable status string.
pub fn status(msg: &str) {
    send(&format!("STATUS={msg}"));
}

fn send(payload: &str) {
    let Ok(addr) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    if addr.is_empty() {
        return;
    }
    // Abstract sockets (leading '@') aren't portable; skip on platforms
    // that don't natively expose them via std.
    if addr.starts_with('@') {
        return;
    }
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    let _ = sock.send_to(payload.as_bytes(), &addr);
}

#[cfg(not(unix))]
mod _windows_stub {
    pub fn ready() {}
    pub fn stopping() {}
    pub fn reloading() {}
    pub fn status(_: &str) {}
}
