//! quiver-bridge: stands in for Ubisoft Connect inside a game's Wine prefix.
//!
//! The game's unmodified `uplay_r164.dll` / `uplay_aux_r164.dll` reach Connect over named
//! pipes (4-byte big-endian length + protobuf). The bridge owns those pipes:
//! - `terminal_1_uplay_aux_ipc_pipe_` (`uplayauxdll`): init, then Denuvo activation, which
//!   is relayed to the native broker (`quiver broker`) on the host
//! - `terminal_1_uplay_api_process_ipc_pipe_` (`uplaydll`): per-process handshake
//! - `terminal_1_uplay_ipc_pipe_` (`uplaydll`): `InitReq`, then the SDK API
//!
//! Static data comes from a [`quiver_ipc::LaunchContext`] JSON file (`quiver context`),
//! given as the first argument or via `QUIVER_BRIDGE_CONTEXT` (or `UBI_BRIDGE_CONTEXT`).
//!
//! Launcher mode, `quiver-bridge.exe --run <context.json> [--log <file>] -- <game.exe> [args…]`
//! (used by `quiver steam-run`): own the pipes, start the game with `UPLAY_ARGUMENTS` as
//! Connect would, exit with its exit code. The context file is deleted once read.

// GUI subsystem: no console window. Logging goes to QUIVER_BRIDGE_LOG.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("quiver-bridge runs inside Wine: build with --target x86_64-pc-windows-gnu");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(win::run())
}

/// Log to stdout and to `QUIVER_BRIDGE_LOG` if set (stdout is unreliable under Proton).
macro_rules! log {
    ($($t:tt)*) => {{
        let line = format!("{} {}", crate::timestamp(), format!($($t)*));
        println!("{line}");
        if let Some(f) = crate::LOG.lock().unwrap().as_mut() {
            use std::io::Write as _;
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }};
}

// Platform-independent parts, tested on the host; on Linux the binary itself does nothing.
#[cfg_attr(not(windows), allow(dead_code))]
mod respond;
#[cfg(windows)]
mod win;

static LOG: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

static CONTEXT: std::sync::OnceLock<Option<quiver_ipc::LaunchContext>> = std::sync::OnceLock::new();

#[cfg_attr(not(windows), allow(dead_code))]
pub fn context() -> Option<&'static quiver_ipc::LaunchContext> {
    CONTEXT.get().and_then(Option::as_ref)
}

/// Local wall-clock time as HH:MM:SS.mmm, matching the SDK proxy's log.
#[cfg(windows)]
pub fn timestamp() -> String {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime { y: u16, mo: u16, dow: u16, d: u16, h: u16, mi: u16, s: u16, ms: u16 }
    unsafe extern "system" {
        fn GetLocalTime(t: *mut SystemTime);
    }
    let mut t = SystemTime::default();
    unsafe { GetLocalTime(&mut t) };
    format!("{:02}:{:02}:{:02}.{:03}", t.h, t.mi, t.s, t.ms)
}

/// UTC on the host (tests only).
#[cfg(not(windows))]
#[allow(dead_code)]
pub fn timestamp() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let s = t.as_secs();
    format!("{:02}:{:02}:{:02}.{:03}", s / 3600 % 24, s / 60 % 60, s % 60, t.subsec_millis())
}

/// The environment variable `name`, else `legacy` (its `UBI_*` name before the rename to
/// Quiver, still set by older scripts and containers).
#[cfg(windows)]
pub fn env_var(name: &str, legacy: &str) -> Option<String> {
    std::env::var(name).or_else(|_| std::env::var(legacy)).ok()
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests;
