//! ubi-bridge: stands in for Ubisoft Connect inside a game's Wine prefix.
//!
//! The game's unmodified `uplay_r164.dll` / `uplay_aux_r164.dll` reach Connect over named
//! pipes (4-byte big-endian length + protobuf). The bridge owns those pipes:
//! - `terminal_1_uplay_aux_ipc_pipe_` (`uplayauxdll`): init, then Denuvo activation, which
//!   is relayed to the native broker (`ubi broker`) on the host
//! - `terminal_1_uplay_api_process_ipc_pipe_` (`uplaydll`): per-process handshake
//! - `terminal_1_uplay_ipc_pipe_` (`uplaydll`): `InitReq`, then the SDK API
//!
//! Static data comes from a [`ubi_ipc::LaunchContext`] JSON file (`ubi launch-context`),
//! given as the first argument or via `UBI_BRIDGE_CONTEXT`.

#[cfg(not(windows))]
fn main() {
    eprintln!("ubi-bridge runs inside Wine: build with --target x86_64-pc-windows-gnu");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(win::run())
}

#[cfg(windows)]
mod win;
