//! What the netlab host and client examples share: keys and addresses from flags or the
//! environment, and JSON-lines output.

// Each example uses only some of these.
#![allow(dead_code)]

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};

use quiver_stream_net::SessionKeys;

/// The media port when an address names only the IP.
pub const DEFAULT_PORT: u16 = 47998;

/// `ip:port`, `[v6]:port`, or a bare IP (then `default_port`).
pub fn parse_addr(s: &str, default_port: u16) -> Result<SocketAddr, String> {
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Ok(a);
    }
    s.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, default_port)).map_err(|_| format!("not an address: {s}"))
}

/// The flag, else the environment variable, else None.
pub fn addr_from(flag: Option<&str>, env: &str, default_port: u16) -> Result<Option<SocketAddr>, String> {
    match flag.map(str::to_owned).or_else(|| std::env::var(env).ok().filter(|v| !v.is_empty())) {
        Some(s) => parse_addr(&s, default_port).map(Some),
        None => Ok(None),
    }
}

/// Keys from `--keys`, `QUIVER_STREAM_KEYS`, or `--keys-file`. The host (`create`) writes
/// fresh keys to the file if it doesn't exist; the client waits up to 10 s for it.
pub fn keys(hex: Option<&str>, file: Option<&Path>, create: bool) -> Result<SessionKeys, String> {
    if let Some(h) = hex.map(str::to_owned).or_else(|| std::env::var("QUIVER_STREAM_KEYS").ok()) {
        return SessionKeys::from_hex(&h).ok_or_else(|| "keys must be 80 hex digits".to_owned());
    }
    let Some(file) = file else {
        return Err("give --keys HEX, QUIVER_STREAM_KEYS, or --keys-file PATH".into());
    };
    if create && !file.exists() {
        let k = SessionKeys::random();
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, k.to_hex()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, file).map_err(|e| format!("{}: {e}", file.display()))?;
        return Ok(k);
    }
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(file) {
            return SessionKeys::from_hex(&s).ok_or_else(|| format!("{}: not 80 hex digits", file.display()));
        }
        if start.elapsed() > Duration::from_secs(10) {
            return Err(format!("{} never appeared", file.display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Prints one JSON object as a line on stdout.
pub fn emit(value: &serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

pub fn median(v: &mut [u32]) -> u32 {
    if v.is_empty() {
        return 0;
    }
    let mid = v.len() / 2;
    *v.select_nth_unstable(mid).1
}
