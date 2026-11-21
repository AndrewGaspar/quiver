//! What the check looks at, gathered without root and without changing anything: files under
//! a root folder (`/`, or a fake one in tests), the network interfaces, which firewall runs,
//! polkit's answer for lingering, and which of Quiver's servers are on.
//!
//! Everything that isn't a file under the root is a field here, so tests set it directly.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// One network interface with its addresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Iface {
    pub name: String,
    /// A virtual device (`/sys/devices/virtual/net`): loopback, bridges, tunnels, Tailscale.
    pub virtual_: bool,
    /// Up and running.
    pub up: bool,
    /// (address, prefix length)
    pub addrs: Vec<(IpAddr, u8)>,
}

/// Which firewall front ends are present and running.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Services {
    /// `ufw` is installed and `/etc/ufw/ufw.conf` says `ENABLED=yes`.
    pub ufw: bool,
    pub firewalld: bool,
    /// `nftables.service` or `iptables.service` loads a ruleset of its own.
    pub nftables: bool,
}

/// The features whose ports a firewall must allow (config.json's `remote`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Features {
    /// Quiver's remote listener (`quiver remote enable`).
    pub remote: bool,
    pub remote_port: u16,
    pub stream_port: u16,
    /// The Moonlight server (`quiver remote moonlight enable`).
    pub moonlight: bool,
    pub moonlight_http: u16,
    pub mdns: bool,
    /// Moonlight listens on the tailnet only (`--bind tailscale0` or tailnet addresses), so
    /// LAN rules would open nothing it uses.
    #[serde(default)]
    pub moonlight_tailnet_only: bool,
    /// The remote listener binds a tailnet address only.
    #[serde(default)]
    pub remote_tailnet_only: bool,
}

/// An address or interface Quiver binds is on the tailnet: `tailscale*`, 100.64.0.0/10 or
/// fd7a:115c:a1e0::/48 (with or without a port).
pub fn tailnet_bind(b: &str) -> bool {
    use super::firewall::{Net, TAILNET_V4, TAILNET_V6};
    if b.starts_with("tailscale") {
        return true;
    }
    let host = b.strip_prefix('[').and_then(|r| r.split(']').next()).unwrap_or_else(|| if b.matches(':').count() == 1 { b.split(':').next().unwrap_or(b) } else { b });
    let Some(n) = Net::parse(host) else { return false };
    Net::new(TAILNET_V4.0.into(), TAILNET_V4.1).contains(&n) || Net::new(TAILNET_V6.0.into(), TAILNET_V6.1).contains(&n)
}

impl Default for Features {
    fn default() -> Self {
        Features {
            remote: false,
            remote_port: quiver_remote::DEFAULT_PORT,
            stream_port: crate::remote::STREAM_PORT,
            moonlight: false,
            moonlight_http: crate::moonlight::DEFAULT_PORT,
            mdns: true,
            moonlight_tailnet_only: false,
            remote_tailnet_only: false,
        }
    }
}

impl Features {
    /// From config.json's `remote` section.
    pub fn from_settings(r: &crate::remote::RemoteSettings) -> Self {
        let remote_port = r.listen().rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(quiver_remote::DEFAULT_PORT);
        Features {
            remote: r.enabled,
            remote_port,
            stream_port: r.stream_port.unwrap_or(crate::remote::STREAM_PORT),
            moonlight: r.moonlight.enabled,
            moonlight_http: r.moonlight.port(),
            mdns: r.moonlight.mdns(),
            moonlight_tailnet_only: !r.moonlight.bind.is_empty() && r.moonlight.bind.iter().all(|b| tailnet_bind(b)),
            remote_tailnet_only: tailnet_bind(&r.listen()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    /// Where `/etc`, `/usr/lib`, `/run/udev`, `/var/lib` are read from: `/`, or a test's
    /// fake root.
    pub root: PathBuf,
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    /// The person's supplementary groups.
    pub groups: Vec<u32>,
    /// The person has a session on a seat (logind gives it the uaccess ACLs, /dev/uinput's
    /// among them).
    pub seated: bool,
    /// /dev/uinput's group now.
    pub uinput_gid: Option<u32>,
    pub ifaces: Vec<Iface>,
    pub services: Services,
    pub linger: bool,
    /// Turning lingering on needs root (polkit doesn't allow `set-self-linger` without
    /// asking). `None`: couldn't tell.
    pub linger_needs_root: Option<bool>,
    /// quiverd.service is enabled (starts with the user manager).
    pub boot_enabled: bool,
    pub features: Features,
    /// The installed Quiver's copies of the shipped files (`<libdir>/contrib`).
    pub contrib: Option<PathBuf>,
}

impl Facts {
    /// A path under the root.
    pub fn path(&self, abs: &str) -> PathBuf {
        self.root.join(abs.trim_start_matches('/'))
    }

    pub fn read(&self, abs: &str) -> std::io::Result<String> {
        std::fs::read_to_string(self.path(abs))
    }

    /// The facts of this machine, now. Reads only what any user may read; runs `systemctl
    /// is-active`, `systemctl --user show` and `pkcheck` (which only asks polkit).
    pub fn gather() -> Facts {
        let (user, _home) = crate::service::passwd().unwrap_or_else(|| (std::env::var("USER").unwrap_or_default(), PathBuf::new()));
        let root = PathBuf::from("/");
        let ufw_conf = std::fs::read_to_string("/etc/ufw/ufw.conf").unwrap_or_default();
        let services = Services {
            ufw: on_path("ufw") && ufw_conf.lines().any(|l| l.trim() == "ENABLED=yes"),
            firewalld: active("firewalld.service"),
            nftables: active("nftables.service") || active("iptables.service") || active("ip6tables.service"),
        };
        let linger = crate::service::linger(&user);
        let sd = crate::service::Systemd::from_env();
        let boot_enabled = sd.unit_state(crate::service::UNIT).is_some_and(|u| u.enabled());
        let features = config_features().unwrap_or_default();
        Facts {
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            groups: my_groups(),
            seated: seated(Path::new("/run/systemd/sessions"), &user),
            uinput_gid: std::fs::metadata("/dev/uinput").ok().map(|m| std::os::unix::fs::MetadataExt::gid(&m)),
            ifaces: interfaces(Path::new("/sys/class/net")),
            services,
            linger,
            linger_needs_root: if linger { Some(false) } else { linger_needs_root() },
            boot_enabled,
            features,
            contrib: find_contrib(),
            user,
            root,
        }
    }
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false)
        || ["/usr/bin", "/usr/sbin", "/sbin", "/bin"].iter().any(|d| Path::new(d).join(tool).is_file())
}

fn active(unit: &str) -> bool {
    Command::new("systemctl").args(["is-active", "--quiet", unit]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// polkit: may this process turn lingering on for its own user without asking?
fn linger_needs_root() -> Option<bool> {
    let st = Command::new("pkcheck")
        .args(["--action-id", "org.freedesktop.login1.set-self-linger", "--process", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    match st.code() {
        Some(0) => Some(false),
        // 1: not authorized, 2: needs authentication (a password prompt we never show).
        Some(1) | Some(2) | Some(3) => Some(true),
        _ => None,
    }
}

fn my_groups() -> Vec<u32> {
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if n <= 0 {
        return Vec::new();
    }
    let mut v = vec![0 as libc::gid_t; n as usize];
    let n = unsafe { libc::getgroups(n, v.as_mut_ptr()) };
    v.truncate(n.max(0) as usize);
    v
}

/// The person has a logind session on a seat, or a local graphical one (see
/// `quiver-stream-host`'s `seat.rs`, which decides pads the same way).
pub fn seated(sessions: &Path, user: &str) -> bool {
    std::fs::read_dir(sessions).into_iter().flatten().flatten().any(|e| {
        if e.file_name().to_string_lossy().ends_with(".ref") {
            return false;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { return false };
        let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix('='))).unwrap_or("");
        let graphical = matches!(get("TYPE"), "x11" | "wayland" | "mir");
        get("USER") == user && get("REMOTE") != "1" && matches!(get("CLASS"), "user" | "") && (!get("SEAT").is_empty() || graphical)
    })
}

/// The interfaces from `/sys/class/net` and `getifaddrs`.
pub fn interfaces(sys_class_net: &Path) -> Vec<Iface> {
    let mut out: Vec<Iface> = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list we free below; we only read it in between.
    if unsafe { libc::getifaddrs(&mut ifap) } != 0 {
        return out;
    }
    let mut p = ifap;
    while !p.is_null() {
        let ifa = unsafe { &*p };
        p = ifa.ifa_next;
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
        let up = ifa.ifa_flags & (libc::IFF_UP as u32) != 0 && ifa.ifa_flags & (libc::IFF_RUNNING as u32) != 0;
        let entry = match out.iter_mut().position(|i| i.name == name) {
            Some(i) => &mut out[i],
            None => {
                let link = std::fs::read_link(sys_class_net.join(&name)).map(|l| l.to_string_lossy().into_owned()).unwrap_or_default();
                out.push(Iface { virtual_: link.is_empty() || link.contains("/virtual/"), up, addrs: Vec::new(), name });
                out.last_mut().unwrap()
            }
        };
        entry.up |= up;
        if let (Some(a), Some(m)) = (sockaddr_ip(ifa.ifa_addr), sockaddr_ip(ifa.ifa_netmask)) {
            entry.addrs.push((a, prefix_len(m)));
        }
    }
    unsafe { libc::freeifaddrs(ifap) };
    out
}

fn sockaddr_ip(sa: *const libc::sockaddr) -> Option<IpAddr> {
    if sa.is_null() {
        return None;
    }
    // SAFETY: getifaddrs' sockaddrs are as long as their family says.
    unsafe {
        match (*sa).sa_family as i32 {
            libc::AF_INET => {
                let s = &*(sa as *const libc::sockaddr_in);
                Some(IpAddr::from(u32::from_be(s.sin_addr.s_addr).to_be_bytes()))
            }
            libc::AF_INET6 => {
                let s = &*(sa as *const libc::sockaddr_in6);
                Some(IpAddr::from(s.sin6_addr.s6_addr))
            }
            _ => None,
        }
    }
}

fn prefix_len(mask: IpAddr) -> u8 {
    match mask {
        IpAddr::V4(m) => u32::from(m).count_ones() as u8,
        IpAddr::V6(m) => u128::from(m).count_ones() as u8,
    }
}

/// config.json's `remote` section, read only.
fn config_features() -> Option<Features> {
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    let text = std::fs::read_to_string(base.join("quiver/config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let r: crate::remote::RemoteSettings = v.get("remote").map(|r| serde_json::from_value(r.clone()).unwrap_or_default()).unwrap_or_default();
    Some(Features::from_settings(&r))
}

/// The installed Quiver's `contrib` folder: `<exe dir>/../lib/quiver/contrib`, where
/// `packaging/layout.sh` puts it (`~/.local/lib/quiver/contrib`, `/usr/lib/quiver/contrib`).
/// `QUIVER_CONTRIB_DIR` overrides it; a build run from a checkout falls back to the
/// checkout's own `contrib`.
pub fn find_contrib() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("QUIVER_CONTRIB_DIR") {
        return Some(PathBuf::from(d));
    }
    let exe = std::env::current_exe().ok()?;
    let installed = exe.parent()?.join("../lib/quiver/contrib");
    if installed.join(super::shipped::SYSUSERS.src).is_file() {
        return installed.canonicalize().ok();
    }
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib");
    (checkout.join(super::shipped::SYSUSERS.src).is_file() && exe.components().any(|c| c.as_os_str() == "target")).then(|| checkout.canonicalize().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seats() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("2"), "USER=ajg\nREMOTE=0\nTYPE=wayland\nCLASS=user\nSEAT=seat0\n").unwrap();
        std::fs::write(t.path().join("5"), "USER=bob\nREMOTE=1\nTYPE=tty\nCLASS=user\n").unwrap();
        assert!(seated(t.path(), "ajg"));
        assert!(!seated(t.path(), "bob"), "ssh isn't a seat");
        assert!(!seated(Path::new("/nonexistent"), "ajg"));
    }

    #[test]
    fn features_from_config() {
        let r: crate::remote::RemoteSettings = serde_json::from_str(r#"{"enabled":true,"listen":"0.0.0.0:48100","stream_port":48700,"moonlight":{"enabled":true,"mdns":false}}"#).unwrap();
        let f = Features::from_settings(&r);
        assert_eq!((f.remote, f.remote_port, f.stream_port, f.moonlight, f.moonlight_http, f.mdns), (true, 48100, 48700, true, 47989, false));
        assert_eq!(Features::from_settings(&Default::default()), Features::default());
        let r: crate::remote::RemoteSettings = serde_json::from_str(r#"{"enabled":true,"listen":"100.70.1.2:47990","moonlight":{"enabled":true,"bind":["tailscale0","fd7a:115c:a1e0::7"]}}"#).unwrap();
        let f = Features::from_settings(&r);
        assert!(f.moonlight_tailnet_only && f.remote_tailnet_only);
        for (b, t) in [("tailscale0", true), ("100.101.12.7", true), ("[fd7a:115c:a1e0::7]:47990", true), ("0.0.0.0:47990", false), ("192.168.50.189", false), ("eth0", false)] {
            assert_eq!(tailnet_bind(b), t, "{b}");
        }
    }

    #[test]
    fn this_machines_interfaces_parse() {
        // Whatever this machine has: loopback is virtual and has 127.0.0.1/8.
        let ifs = interfaces(Path::new("/sys/class/net"));
        if let Some(lo) = ifs.iter().find(|i| i.name == "lo") {
            assert!(lo.virtual_);
            assert!(lo.addrs.contains(&("127.0.0.1".parse().unwrap(), 8)), "{lo:?}");
        }
    }
}
