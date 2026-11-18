//! Processes as the host sees them: which container each is in (its libpod cgroup), what
//! Wine prefix it runs in, and whether it is a Windows program. Instance processes are
//! told apart by cgroup and `WINEPREFIX`, never by an executable name alone (names match
//! the person's own game, and `pgrep -f` matched the agent's own shell).

use std::collections::BTreeMap;
use std::path::Path;

/// One process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// `/proc/<pid>/comm` (at most 15 bytes).
    pub comm: String,
    /// The first argument (Wine puts the Windows path of the program there).
    pub argv0: String,
    /// The libpod container id (64 hex), when the process is in a podman container.
    pub container: Option<String>,
    pub wineprefix: Option<String>,
}

impl Proc {
    /// The program's file name: the last component of argv0 (`/` or `\` separated), else comm.
    pub fn program(&self) -> &str {
        let a = self.argv0.rsplit(['/', '\\']).next().unwrap_or("");
        if a.is_empty() { &self.comm } else { a }
    }

    /// A Windows program (Wine names the process after its `.exe`).
    pub fn is_windows(&self) -> bool {
        self.program().to_ascii_lowercase().ends_with(".exe") || self.comm.to_ascii_lowercase().ends_with(".exe")
    }

    /// Part of Wine or Proton itself rather than a program someone started.
    pub fn is_wine_service(&self) -> bool {
        let p = self.program().to_ascii_lowercase();
        WINE_SERVICES.contains(&p.as_str())
            || matches!(self.comm.as_str(), "wineserver" | "wine" | "wine64" | "wine-preloader" | "wine64-preloader")
    }

    /// A Wine process of any kind (programs and services, the server, the preloader).
    pub fn is_wine(&self) -> bool {
        self.is_windows() || self.is_wine_service() || self.wineprefix.is_some() && self.comm.starts_with("wine")
    }
}

/// Wine's and Proton's own processes (lowercase file names).
pub const WINE_SERVICES: &[&str] = &[
    "services.exe",
    "winedevice.exe",
    "plugplay.exe",
    "explorer.exe",
    "svchost.exe",
    "rpcss.exe",
    "tabtip.exe",
    "conhost.exe",
    "rundll32.exe",
    "steam.exe",
    "wineboot.exe",
    "start.exe",
    "winemenubuilder.exe",
    "xalia.exe",
    "mscorsvw.exe",
    "sc.exe",
    "wineserver",
];

/// The libpod container id in a `/proc/<pid>/cgroup` text: `…/libpod-<id>.scope/…`
/// (systemd cgroup manager) or `…/libpod-<id>/…` (cgroupfs). `libpod-conmon-<id>` is
/// podman's monitor, which runs on the host side: not in the container.
pub fn libpod_id(cgroup: &str) -> Option<String> {
    for part in cgroup.split(['/', '\n']) {
        let Some(rest) = part.strip_prefix("libpod-") else { continue };
        if rest.starts_with("conmon-") {
            continue;
        }
        let id = rest.strip_suffix(".scope").unwrap_or(rest);
        if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(id.to_owned());
        }
    }
    None
}

fn env_var(environ: &[u8], name: &str) -> Option<String> {
    let key = format!("{name}=");
    environ.split(|&b| b == 0).find_map(|kv| kv.strip_prefix(key.as_bytes())).map(|v| String::from_utf8_lossy(v).into_owned())
}

/// Read one process (`None` if it went away or isn't readable).
pub fn read(proc_root: &Path, pid: u32) -> Option<Proc> {
    let dir = proc_root.join(pid.to_string());
    let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
    // "pid (comm) state ppid …": comm may hold spaces and parentheses.
    let after = &stat[stat.rfind(')')? + 2..];
    let ppid = after.split_whitespace().nth(1)?.parse().ok()?;
    let comm = std::fs::read_to_string(dir.join("comm")).ok()?.trim_end().to_owned();
    let cmdline = std::fs::read(dir.join("cmdline")).unwrap_or_default();
    let argv0 = String::from_utf8_lossy(cmdline.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
    let cgroup = std::fs::read_to_string(dir.join("cgroup")).unwrap_or_default();
    let environ = std::fs::read(dir.join("environ")).unwrap_or_default();
    Some(Proc { pid, ppid, comm, argv0, container: libpod_id(&cgroup), wineprefix: env_var(&environ, "WINEPREFIX") })
}

/// Every process under `proc_root` (`/proc`).
pub fn scan_in(proc_root: &Path) -> Vec<Proc> {
    let mut out: Vec<Proc> = std::fs::read_dir(proc_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| read(proc_root, pid))
        .collect();
    out.sort_by_key(|p| p.pid);
    out
}

pub fn scan() -> Vec<Proc> {
    scan_in(Path::new("/proc"))
}

/// Windows programs running outside every podman container: the person's games (or
/// another Wine program of theirs). A lab launch is refused while there is one.
pub fn host_games(procs: &[Proc]) -> Vec<&Proc> {
    procs.iter().filter(|p| p.container.is_none() && p.is_windows() && !p.is_wine_service()).collect()
}

/// The processes in a container.
pub fn in_container<'a>(procs: &'a [Proc], container_id: &str) -> Vec<&'a Proc> {
    procs.iter().filter(|p| p.container.as_deref() == Some(container_id)).collect()
}

/// Wine processes of an instance: in its container and (for anything but the server
/// itself) in its prefix. What `kill-all` takes down.
pub fn instance_wine<'a>(procs: &'a [Proc], container_id: &str) -> Vec<&'a Proc> {
    in_container(procs, container_id).into_iter().filter(|p| p.is_wine() || p.comm == "python3" && p.wineprefix.is_some()).collect()
}

/// The instance's game: Windows programs in its container named like `exe` (compared
/// case-insensitively, as Windows does).
pub fn instance_game<'a>(procs: &'a [Proc], container_id: &str, exe: &str) -> Vec<&'a Proc> {
    let want = exe.to_ascii_lowercase();
    // comm is cut at 15 bytes: "ACOrigins.exe" fits, "ACCGame-Win32-Shipping.exe" doesn't.
    let short: String = want.chars().take(15).collect();
    in_container(procs, container_id)
        .into_iter()
        .filter(|p| p.program().to_ascii_lowercase() == want || p.comm.to_ascii_lowercase() == short)
        .collect()
}

/// Ubisoft Connect's processes in an instance (there must never be one).
pub fn connect_in<'a>(procs: &'a [Proc], container_id: &str) -> Vec<&'a Proc> {
    in_container(procs, container_id)
        .into_iter()
        .filter(|p| {
            let n = p.program().to_ascii_lowercase();
            crate::steamrun::CONNECT_EXES.iter().any(|c| c.to_ascii_lowercase() == n)
        })
        .collect()
}

/// Processes holding an NVIDIA device open (`/dev/nvidia<N>`), by pid. Reads only the
/// processes we may read (our own).
pub fn gpu_pids_in(proc_root: &Path, pids: &[u32]) -> Vec<u32> {
    pids.iter()
        .copied()
        .filter(|pid| {
            std::fs::read_dir(proc_root.join(pid.to_string()).join("fd")).into_iter().flatten().flatten().any(|fd| {
                std::fs::read_link(fd.path()).is_ok_and(|t| {
                    let t = t.to_string_lossy();
                    t.strip_prefix("/dev/nvidia").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
            })
        })
        .collect()
}

/// Who a process belongs to, for logs: `host`, `lab:<instance>`, or `container:<name>`.
pub fn owner(p: &Proc, containers: &BTreeMap<String, String>) -> String {
    match &p.container {
        None => "host".into(),
        Some(id) => match containers.get(id) {
            Some(name) => match name.strip_prefix(super::podman::CONTAINER_PREFIX) {
                Some(inst) => format!("lab:{inst}"),
                None => format!("container:{name}"),
            },
            None => format!("container:{}", &id[..12.min(id.len())]),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn p(pid: u32, comm: &str, argv0: &str, container: Option<&str>, pfx: Option<&str>) -> Proc {
        Proc { pid, ppid: 1, comm: comm.into(), argv0: argv0.into(), container: container.map(str::to_owned), wineprefix: pfx.map(str::to_owned) }
    }

    #[test]
    fn cgroups() {
        let sd = format!("0::/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-{ID}.scope/container\n");
        assert_eq!(libpod_id(&sd).as_deref(), Some(ID));
        assert_eq!(libpod_id(&format!("0::/libpod_parent/libpod-{ID}")).as_deref(), Some(ID));
        assert_eq!(libpod_id(&format!("0::/user.slice/libpod-conmon-{ID}.scope")), None);
        assert_eq!(libpod_id("0::/user.slice/user-1000.slice/session-2.scope"), None);
    }

    #[test]
    fn the_persons_games_are_outside_containers() {
        let procs = vec![
            p(10, "ACOrigins.exe", r"C:\Games\ACOrigins.exe", None, Some("/home/u/Games/ubi-prefixes/aco")),
            p(11, "services.exe", r"C:\windows\system32\services.exe", None, Some("/pf")),
            p(12, "ACOrigins.exe", r"C:\Games\ACOrigins.exe", Some(ID), Some("/pf")),
            p(13, "bash", "bash", None, None),
            p(14, "AlanWake.exe", "Z:\\home\\u\\Games\\AlanWake.exe", None, None),
        ];
        let host: Vec<u32> = host_games(&procs).iter().map(|p| p.pid).collect();
        assert_eq!(host, [10, 14]);
        assert_eq!(instance_game(&procs, ID, "acorigins.EXE").iter().map(|p| p.pid).collect::<Vec<_>>(), [12]);
        // Same name on the host, not the instance's.
        assert!(instance_game(&procs, &"f".repeat(64), "ACOrigins.exe").is_empty());
    }

    #[test]
    fn long_names_match_on_argv0_or_cut_comm() {
        let procs = vec![
            p(20, "ACCGame-Win32-S", r"Z:\g\Binaries\Win32\ACCGame-Win32-Shipping.exe", Some(ID), Some("/pf")),
            p(21, "ACCGame-Win32-S", "", Some(ID), Some("/pf")),
        ];
        assert_eq!(instance_game(&procs, ID, "ACCGame-Win32-Shipping.exe").len(), 2);
    }

    #[test]
    fn wine_and_connect() {
        let procs = vec![
            p(30, "wineserver", "/p/files/bin/wineserver", Some(ID), Some("/pf")),
            p(31, "upc.exe", r"C:\Program Files (x86)\Ubisoft\Ubisoft Game Launcher\upc.exe", Some(ID), Some("/pf")),
            p(32, "gamescope", "gamescope", Some(ID), None),
            p(33, "catatonit", "/usr/libexec/podman/catatonit", Some(ID), None),
            p(34, "python3", "python3", Some(ID), Some("/pf")),
            p(35, "quiver-bridge.e", r"C:\quiver\quiver-bridge.exe", Some(ID), Some("/pf")),
        ];
        let w: Vec<u32> = instance_wine(&procs, ID).iter().map(|p| p.pid).collect();
        assert_eq!(w, [30, 31, 34, 35]);
        assert_eq!(connect_in(&procs, ID).iter().map(|p| p.pid).collect::<Vec<_>>(), [31]);
    }

    #[test]
    fn owners() {
        let names = BTreeMap::from([(ID.to_owned(), "quiver-lab-t1".to_owned())]);
        assert_eq!(owner(&p(1, "x", "x", Some(ID), None), &names), "lab:t1");
        assert_eq!(owner(&p(1, "x", "x", None, None), &names), "host");
        let other = BTreeMap::from([(ID.to_owned(), "aco-t2".to_owned())]);
        assert_eq!(owner(&p(1, "x", "x", Some(ID), None), &other), "container:aco-t2");
    }

    #[test]
    fn reads_this_process() {
        let me = read(Path::new("/proc"), std::process::id()).unwrap();
        assert_eq!(me.pid, std::process::id());
        assert!(!me.comm.is_empty());
        assert!(!scan().is_empty());
    }
}
