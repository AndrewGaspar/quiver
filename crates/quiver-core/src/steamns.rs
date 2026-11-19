//! Steam's command in a namespace with a launch's mod overlays (design/STEAM-COPIES.md §4,
//! design/STORE-STEAM.md §10).
//!
//! `quiver steam-run` normally `exec`s Steam's command (STORE-STEAM G1): the pid stays, and
//! Steam's process tree is exactly Steam's. A launch with overlays can't: `steam-run` is a
//! multi-threaded process, and a user namespace can only be entered by a single-threaded one.
//! So [`run`] uses the existing holder mechanism ([`crate::overlay::Namespace`]): a short-lived
//! holder process creates the user + mount namespace and mounts the overlays, then exits;
//! Steam's command is spawned as `steam-run`'s child and joins that namespace with `setns` in
//! `pre_exec` (after `fork`, single-threaded). Steam's reaper, pressure-vessel (whose bwrap
//! binds the install folder recursively, so the overlay comes along), Proton and the game then
//! all see the mods.
//!
//! `steam-run` stays the parent for as long as the child lives and never longer:
//!
//! - it forwards the signals a launcher sends (`SIGTERM`, `SIGINT`, `SIGHUP`, `SIGQUIT`,
//!   `SIGUSR1`, `SIGUSR2`) to the child. Steam's Stop signals the launch's process group,
//!   which the child shares (no `setpgid`), so it gets those directly too;
//! - the child gets `PR_SET_PDEATHSIG`(`SIGTERM`), so if `steam-run` is killed outright the
//!   reaper is told to end the game;
//! - it exits with the child's status: the same exit code, or the same signal re-raised.
//!
//! The holder is reaped before [`run`] returns from creating the namespace; the namespace's
//! file descriptors close when `steam-run` exits, and the namespace goes away with the last
//! process in it. The play session follows `steam-run`'s own pid (a pidfd in quiverd), which
//! now lives exactly as long as the child.

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use anyhow::{Context, Result, bail};
use quiver_api::steamlib::{SteamMods, SteamOverlay};

use crate::overlay::{Namespace, OverlaySpec};

/// How `steam-run` starts Steam's command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// `exec` (the pid stays; nothing to mount): every launch without overlays, as before.
    Exec,
    /// Spawn the command as a child inside a namespace with the overlays mounted, wait for
    /// it, and pass its exit through.
    Namespace,
    /// Don't start the game: the mods plan has problems (a mod lost its files, …).
    Refuse,
}

/// The choice for a launch's mods (`None`: no mods plan, e.g. an older quiverd).
pub fn start_for(mods: Option<&SteamMods>) -> Start {
    match mods {
        Some(m) if !m.problems.is_empty() => Start::Refuse,
        Some(m) if !m.overlays.is_empty() => Start::Namespace,
        _ => Start::Exec,
    }
}

/// The overlays as mount specs.
pub fn specs(overlays: &[SteamOverlay]) -> Vec<OverlaySpec> {
    overlays.iter().map(|o| OverlaySpec { target: o.target.clone(), lower: o.lower.clone(), upper: o.upper.clone(), work: o.work.clone(), shared: o.shared.clone() }).collect()
}

/// Signals forwarded to the child.
pub const FORWARDED: [libc::c_int; 6] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT, libc::SIGUSR1, libc::SIGUSR2];

/// Create the namespace for `overlays` (the holder is gone when this returns).
pub fn namespace(overlays: &[SteamOverlay]) -> Result<Namespace> {
    let specs = specs(overlays);
    for s in &specs {
        // A profile's own prefix folder may not exist yet (quiverd makes it; be sure).
        std::fs::create_dir_all(&s.target).with_context(|| format!("creating {}", s.target.display()))?;
    }
    Namespace::create_many(&specs).context("mounting the mods (user + mount namespace)")
}

/// Run `argv` with `env` set inside `ns`, starting in `cwd` (as seen inside), and wait for
/// it, forwarding signals. Must be called from the process's main thread: the child's
/// parent-death signal fires when the thread that forked it exits.
pub async fn run(ns: &Namespace, argv: &[String], env: &[(String, String)], cwd: Option<&Path>) -> Result<ExitStatus> {
    let Some((exe, args)) = argv.split_first() else { bail!("no command") };
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args).envs(env.iter().map(|(k, v)| (k, v)));
    ns.enter(cmd.as_std_mut(), cwd)?;
    // SAFETY: getpid/gettid can't fail.
    let main_thread = unsafe { libc::getpid() == libc::gettid() };
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: raw syscalls only, on values copied into the closure.
    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            if main_thread {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent died before the prctl took effect: don't start at all.
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
            }
            Ok(())
        });
    }
    // Handlers before the spawn, so a signal in between isn't lost or fatal.
    let mut streams = Vec::new();
    for s in FORWARDED {
        streams.push((s, tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(s))?));
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {exe}"))?;
    let pid = child.id().context("child pid")? as libc::pid_t;
    loop {
        let any_signal = futures::future::select_all(streams.iter_mut().map(|(s, st)| {
            let s = *s;
            Box::pin(async move {
                st.recv().await;
                s
            })
        }));
        tokio::select! {
            status = child.wait() => return Ok(status?),
            (sig, _, _) = any_signal => {
                // SAFETY: a plain kill(2) to our own child.
                unsafe { libc::kill(pid, sig) };
            }
        }
    }
}

/// Leave with `status`: its exit code, or its signal re-raised (default action), so whoever
/// started `steam-run` (Steam) sees what the child did.
pub fn exit_like(status: ExitStatus) -> ! {
    if let Some(sig) = status.signal() {
        // SAFETY: resetting a disposition and raising a signal at ourselves.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            libc::raise(sig);
        }
        std::process::exit(128 + sig);
    }
    std::process::exit(status.code().unwrap_or(1))
}

/// The working directory Steam gave the launch, if it can be named.
pub fn cwd() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(overlays: usize, problems: usize) -> SteamMods {
        SteamMods {
            launch: "s-x".into(),
            overlays: (0..overlays).map(|i| SteamOverlay { target: format!("/g/{i}").into(), ..Default::default() }).collect(),
            problems: (0..problems).map(|i| format!("p{i}")).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn spawn_only_when_something_mounts() {
        assert_eq!(start_for(None), Start::Exec, "no mods plan (older quiverd, or quiverd down)");
        assert_eq!(start_for(Some(&mods(0, 0))), Start::Exec, "vanilla, or config patches only");
        assert_eq!(start_for(Some(&mods(1, 0))), Start::Namespace);
        assert_eq!(start_for(Some(&mods(2, 0))), Start::Namespace, "install folder and a profile's prefix folder");
        assert_eq!(start_for(Some(&mods(1, 1))), Start::Refuse);
        assert_eq!(start_for(Some(&mods(0, 1))), Start::Refuse);
        let s = specs(&mods(1, 0).overlays);
        assert_eq!(s[0].target, Path::new("/g/0"));
    }
}
