//! The lab's namespaces: one rootless user namespace owning three network namespaces.
//!
//! ```text
//!  server ns                 router ns                      client ns
//!  eth0 10.77.0.2  ──veth──  srv 10.77.0.1 │ cli 10.77.1.1 ──veth──  eth0 10.77.1.2
//!       fd77::2                  fd77::1     fd77:0:1::1               fd77:0:1::2
//!                     ◄── to_server shaping   to_client shaping ──►
//! ```
//!
//! Shaping lives on the router's two egress interfaces: `cli` carries server → client
//! (`to_client`) and `srv` carries client → server (`to_server`). It is not on the
//! endpoints' own interfaces, because a qdisc on the sender's interface holds packets that
//! still count against the sending socket: a UDP sender then blocks on its send buffer
//! instead of seeing loss, and TCP's small-queue logic throttles itself. A packet entering the
//! router is orphaned from its socket (`ip_rcv`), so the router's queues drop and delay like a
//! real bottleneck.
//!
//! **Lifetime.** [`Lab`] holds four namespace fds: the user namespace and the three network
//! namespaces. A short-lived helper creates them (it unshares, maps the person's uid to root,
//! and parks the namespaces on fds that [`Lab::new`] reopens through `/proc`), then exits.
//! Nothing is mounted (`/run/netns` stays out of it) and no process holds the namespaces but
//! the lab's own: when the `Lab` is dropped, or its process dies in any way, the fds close and
//! the namespaces go away with the last process inside them. Workloads are killed with their
//! process group on drop and get `PR_SET_PDEATHSIG` for a crash.
//!
//! **Credentials.** Inside the lab's user namespace the person's uid is root, which is what
//! `ip` and `tc` need (CAP_NET_ADMIN over the lab's network namespaces, nothing outside).
//! Workloads run one level deeper: each enters the lab user namespace and its network
//! namespace, then unshares a nested user namespace that maps the person's uid back to itself.
//! A workload therefore sees its real uid and gid (Wine, Steam and gamescope refuse uid 0, and
//! D-Bus's EXTERNAL auth compares the uid the client claims with the kernel's), holds no
//! capability over the lab's namespaces (it can't change the shaping), and keeps its
//! supplementary groups (`setgroups` is denied, not cleared), so `/dev/dri`, `/dev/uinput` and
//! the person's ACLs work as before.
//!
//! **What is shared.** Only the network namespace changes. The mount namespace is the host's,
//! so filesystem sockets (Wayland, PipeWire, the D-Bus session bus, quiverd's socket, X11's
//! `/tmp/.X11-unix` fallback) and devices work from inside. Abstract unix sockets are per
//! network namespace and are not reachable. The lab's namespaces have no link to the host's,
//! so nothing inside has a route to the outside world, and the host's interfaces, routes and
//! qdiscs are never touched.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

/// One of the lab's three network namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ns {
    Router,
    Server,
    Client,
}

/// An endpoint: where a workload runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Server,
    Client,
}

impl Side {
    pub fn ns(self) -> Ns {
        match self {
            Side::Server => Ns::Server,
            Side::Client => Ns::Client,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Side::Server => "server",
            Side::Client => "client",
        }
    }
}

/// One direction of the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    /// Server → client: the stream's video and audio.
    ToClient,
    /// Client → server: input and feedback.
    ToServer,
}

impl Dir {
    pub const BOTH: [Dir; 2] = [Dir::ToClient, Dir::ToServer];

    /// The router interface whose egress carries this direction.
    pub fn router_dev(self) -> &'static str {
        match self {
            Dir::ToClient => "cli",
            Dir::ToServer => "srv",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Dir::ToClient => "to_client",
            Dir::ToServer => "to_server",
        }
    }
}

/// The router's addresses and the server's never change. The client's change on a roam.
pub const ROUTER_SERVER_V4: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
pub const SERVER_V4: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 2);
pub const ROUTER_CLIENT_V4: Ipv4Addr = Ipv4Addr::new(10, 77, 1, 1);
pub const CLIENT_V4: Ipv4Addr = Ipv4Addr::new(10, 77, 1, 2);
pub const ROUTER_SERVER_V6: Ipv6Addr = Ipv6Addr::new(0xfd77, 0, 0, 0, 0, 0, 0, 1);
pub const SERVER_V6: Ipv6Addr = Ipv6Addr::new(0xfd77, 0, 0, 0, 0, 0, 0, 2);
pub const ROUTER_CLIENT_V6: Ipv6Addr = Ipv6Addr::new(0xfd77, 0, 1, 0, 0, 0, 0, 1);
pub const CLIENT_V6: Ipv6Addr = Ipv6Addr::new(0xfd77, 0, 1, 0, 0, 0, 0, 2);

/// Where the client is now. A roam moves it to the next host number in the same subnets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientAddrs {
    pub v4: Ipv4Addr,
    pub v6: Ipv6Addr,
}

/// What another process needs to attach to a running lab (`quiver-netlab exec`, `set`):
/// written to `$XDG_RUNTIME_DIR/quiver-netlab/<pid>.json` while the lab lives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabInfo {
    pub pid: u32,
    pub user_fd: RawFd,
    pub router_fd: RawFd,
    pub server_fd: RawFd,
    pub client_fd: RawFd,
    pub client: ClientAddrs,
    /// The control socket (`quiver-netlab set`), if the lab's process serves one.
    pub control: Option<String>,
    pub label: String,
}

/// Why the lab can't start on this machine (no unprivileged user namespaces, no `ip`/`tc`).
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "network lab unavailable: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

/// A running lab. See the module docs.
pub struct Lab {
    user: OwnedFd,
    router: OwnedFd,
    server: OwnedFd,
    client: OwnedFd,
    uid: u32,
    gid: u32,
    /// The client's current addresses (they change on a roam).
    client_addrs: Mutex<ClientAddrs>,
    /// Process groups of the workloads spawned, killed on drop.
    groups: Mutex<Vec<i32>>,
    /// Attached to someone else's lab: don't kill their workloads on drop.
    attached: bool,
}

/// The fds the creating helper parks its namespaces on before it execs `sleep`.
const PARK_ROUTER: RawFd = 100;
const PARK_SERVER: RawFd = 101;
const PARK_CLIENT: RawFd = 102;
/// Where a command that needs a namespace path (`ip link ... netns`) finds the endpoint's.
const EXPOSE_FD: RawFd = 200;

impl Lab {
    /// Creates the namespaces and the topology. Fails with [`Unavailable`] (downcastable)
    /// when user namespaces or the tools aren't there, so tests can skip.
    pub fn new() -> Result<Lab> {
        for tool in ["ip", "tc", "sleep", "true", "sh"] {
            if which(tool).is_none() {
                return Err(Unavailable(format!("`{tool}` is not on PATH")).into());
            }
        }
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let lab = Self::create(uid, gid).map_err(|e| Unavailable(format!("{e:#}")))?;
        lab.setup().context("setting up the lab topology")?;
        Ok(lab)
    }

    fn create(uid: u32, gid: u32) -> Result<Lab> {
        let uid_map = format!("0 {uid} 1\n").into_bytes();
        let gid_map = format!("0 {gid} 1\n").into_bytes();
        let mut cmd = Command::new("sleep");
        cmd.arg("infinity").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: the closure runs in the forked child before exec and only makes
        // async-signal-safe syscalls on memory allocated before the fork.
        unsafe {
            cmd.pre_exec(move || {
                check(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET))?;
                write_proc(c"/proc/self/setgroups", b"deny")?;
                write_proc(c"/proc/self/uid_map", &uid_map)?;
                write_proc(c"/proc/self/gid_map", &gid_map)?;
                park_netns(PARK_ROUTER)?;
                check(libc::unshare(libc::CLONE_NEWNET))?;
                park_netns(PARK_SERVER)?;
                check(libc::unshare(libc::CLONE_NEWNET))?;
                park_netns(PARK_CLIENT)?;
                Ok(())
            });
        }
        let mut helper = cmd.spawn().context("unsharing a user and network namespace")?;
        let pid = helper.id();
        let open = |path: String| -> Result<OwnedFd> {
            let f = std::fs::File::open(&path).with_context(|| format!("opening {path}"))?;
            Ok(OwnedFd::from(f))
        };
        let fds = (|| {
            Ok::<_, anyhow::Error>((
                open(format!("/proc/{pid}/ns/user"))?,
                open(format!("/proc/{pid}/fd/{PARK_ROUTER}"))?,
                open(format!("/proc/{pid}/fd/{PARK_SERVER}"))?,
                open(format!("/proc/{pid}/fd/{PARK_CLIENT}"))?,
            ))
        })();
        let _ = helper.kill();
        let _ = helper.wait();
        let (user, router, server, client) = fds?;
        Ok(Lab {
            user,
            router,
            server,
            client,
            uid,
            gid,
            client_addrs: Mutex::new(ClientAddrs { v4: CLIENT_V4, v6: CLIENT_V6 }),
            groups: Mutex::new(Vec::new()),
            attached: false,
        })
    }

    /// Attaches to a lab another process holds, through its `/proc/<pid>/fd` entries.
    pub fn attach(info: &LabInfo) -> Result<Lab> {
        let open = |fd: RawFd| -> Result<OwnedFd> {
            let path = format!("/proc/{}/fd/{fd}", info.pid);
            Ok(OwnedFd::from(std::fs::File::open(&path).with_context(|| format!("opening {path}"))?))
        };
        Ok(Lab {
            user: open(info.user_fd)?,
            router: open(info.router_fd)?,
            server: open(info.server_fd)?,
            client: open(info.client_fd)?,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            client_addrs: Mutex::new(info.client),
            groups: Mutex::new(Vec::new()),
            attached: true,
        })
    }

    /// What [`Lab::attach`] needs, for this process.
    pub fn info(&self, label: &str, control: Option<String>) -> LabInfo {
        LabInfo {
            pid: std::process::id(),
            user_fd: self.user.as_raw_fd(),
            router_fd: self.router.as_raw_fd(),
            server_fd: self.server.as_raw_fd(),
            client_fd: self.client.as_raw_fd(),
            client: self.client_addrs(),
            control,
            label: label.to_string(),
        }
    }

    fn netns_fd(&self, ns: Ns) -> RawFd {
        match ns {
            Ns::Router => self.router.as_raw_fd(),
            Ns::Server => self.server.as_raw_fd(),
            Ns::Client => self.client.as_raw_fd(),
        }
    }

    fn setup(&self) -> Result<()> {
        // Forwarding on the router, and roams that keep the default route (a secondary
        // address is promoted when the primary goes).
        self.sysctl(Ns::Router, "net/ipv4/ip_forward", "1")?;
        self.sysctl(Ns::Router, "net/ipv6/conf/all/forwarding", "1")?;
        // The veths, created from the router with their peers straight in the endpoints.
        for (dev, peer_ns) in [("srv", Ns::Server), ("cli", Ns::Client)] {
            let netns = format!("/proc/self/fd/{EXPOSE_FD}");
            self.run_exposing(
                Ns::Router,
                &["ip", "link", "add", dev, "type", "veth", "peer", "name", "eth0", "netns", &netns],
                Some(self.netns_fd(peer_ns)),
            )?;
        }
        // GSO off (one segment per skb), so netem drops, delays and counts real packets
        // rather than 64 KiB super-packets.
        self.ip_batch(
            Ns::Router,
            &format!(
                "link set lo up\n\
                 link set srv gso_max_segs 1 up\n\
                 link set cli gso_max_segs 1 up\n\
                 addr add {ROUTER_SERVER_V4}/24 dev srv\n\
                 addr add {ROUTER_CLIENT_V4}/24 dev cli\n\
                 addr add {ROUTER_SERVER_V6}/64 dev srv nodad\n\
                 addr add {ROUTER_CLIENT_V6}/64 dev cli nodad\n"
            ),
        )?;
        for (ns, v4, v6, gw4, gw6) in [
            (Ns::Server, SERVER_V4, SERVER_V6, ROUTER_SERVER_V4, ROUTER_SERVER_V6),
            (Ns::Client, CLIENT_V4, CLIENT_V6, ROUTER_CLIENT_V4, ROUTER_CLIENT_V6),
        ] {
            self.sysctl(ns, "net/ipv4/conf/all/promote_secondaries", "1")?;
            self.sysctl(ns, "net/ipv4/conf/default/promote_secondaries", "1")?;
            // Unprivileged ICMP sockets for the person's gid (root in the lab's user
            // namespace), so `ping` works inside without CAP_NET_RAW.
            self.sysctl(ns, "net/ipv4/ping_group_range", "0 0")?;
            self.ip_batch(
                ns,
                &format!(
                    "link set lo up\n\
                     link set eth0 gso_max_segs 1 up\n\
                     addr add {v4}/24 dev eth0\n\
                     addr add {v6}/64 dev eth0 nodad\n\
                     route add default via {gw4} dev eth0\n\
                     route add ::/0 via {gw6} dev eth0\n"
                ),
            )?;
        }
        // Static neighbours both ways, so the first packet doesn't wait for an ARP or ND
        // exchange that itself crosses the shaped link.
        let srv_mac = self.mac(Ns::Router, "srv")?;
        let cli_mac = self.mac(Ns::Router, "cli")?;
        let server_mac = self.mac(Ns::Server, "eth0")?;
        let client_mac = self.mac(Ns::Client, "eth0")?;
        self.ip_batch(
            Ns::Router,
            &format!(
                "neigh replace {SERVER_V4} lladdr {server_mac} dev srv nud permanent\n\
                 neigh replace {SERVER_V6} lladdr {server_mac} dev srv nud permanent\n\
                 neigh replace {CLIENT_V4} lladdr {client_mac} dev cli nud permanent\n\
                 neigh replace {CLIENT_V6} lladdr {client_mac} dev cli nud permanent\n"
            ),
        )?;
        for (ns, gw4, gw6, mac) in [
            (Ns::Server, ROUTER_SERVER_V4, ROUTER_SERVER_V6, &srv_mac),
            (Ns::Client, ROUTER_CLIENT_V4, ROUTER_CLIENT_V6, &cli_mac),
        ] {
            self.ip_batch(
                ns,
                &format!(
                    "neigh replace {gw4} lladdr {mac} dev eth0 nud permanent\n\
                     neigh replace {gw6} lladdr {mac} dev eth0 nud permanent\n"
                ),
            )?;
        }
        Ok(())
    }

    /// An interface's MAC address.
    fn mac(&self, ns: Ns, dev: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Link {
            address: String,
        }
        let out = self.run(ns, &["ip", "-j", "link", "show", "dev", dev])?;
        let links: Vec<Link> = serde_json::from_str(&out).context("parsing ip -j link")?;
        links.into_iter().next().map(|l| l.address).ok_or_else(|| anyhow!("no link {dev}"))
    }

    /// Writes `/proc/sys/<key>` in a namespace.
    pub fn sysctl(&self, ns: Ns, key: &str, value: &str) -> Result<()> {
        self.run(ns, &["sh", "-c", &format!("echo {value} > /proc/sys/{key}")]).map(|_| ())
    }

    /// Runs `ip -batch -` in a namespace, as the lab's root.
    pub fn ip_batch(&self, ns: Ns, script: &str) -> Result<()> {
        self.run_stdin(ns, &["ip", "-batch", "-"], script).map(|_| ())
    }

    /// Runs `tc -batch -` in a namespace, as the lab's root.
    pub fn tc_batch(&self, ns: Ns, script: &str) -> Result<()> {
        self.run_stdin(ns, &["tc", "-batch", "-"], script).map(|_| ())
    }

    /// Runs a command in a namespace as the lab's root and returns its stdout. A failure
    /// carries the command and its stderr.
    pub fn run(&self, ns: Ns, argv: &[&str]) -> Result<String> {
        self.run_exposing(ns, argv, None)
    }

    fn run_stdin(&self, ns: Ns, argv: &[&str], input: &str) -> Result<String> {
        let mut cmd = self.admin_command(ns, argv, None);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("running {}", argv.join(" ")))?;
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "{} failed ({}): {}\ninput:\n{}",
                argv.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim(),
                input.trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn run_exposing(&self, ns: Ns, argv: &[&str], expose: Option<RawFd>) -> Result<String> {
        let out = self
            .admin_command(ns, argv, expose)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", argv.join(" ")))?;
        if !out.status.success() {
            bail!(
                "{} failed ({}): {}",
                argv.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// A command that runs as the lab's root in `ns` (for `ip`, `tc` and `sysctl`).
    pub fn admin_command(&self, ns: Ns, argv: &[&str], expose: Option<RawFd>) -> Command {
        let mut cmd = Command::new(argv[0]);
        cmd.args(&argv[1..]);
        let user = self.user.as_raw_fd();
        let net = self.netns_fd(ns);
        // SAFETY: async-signal-safe syscalls only, on fds this Lab keeps open.
        unsafe {
            cmd.pre_exec(move || {
                check(libc::setns(user, libc::CLONE_NEWUSER))?;
                check(libc::setns(net, libc::CLONE_NEWNET))?;
                if let Some(fd) = expose {
                    check(libc::dup2(fd, EXPOSE_FD))?;
                }
                Ok(())
            });
        }
        cmd
    }

    /// Prepares `cmd` to run in an endpoint's network namespace as the person's own uid and
    /// gid (see the module docs), in its own process group, killed if this thread exits.
    /// `own_group: false` keeps the caller's process group, for an interactive shell that
    /// needs the terminal. `dup: Some((fd, at))` puts `fd` at `at` last, after the namespace
    /// fds (which may sit at low numbers) have been used.
    pub fn workload_command(&self, side: Side, cmd: &mut Command, own_group: bool, dup: Option<(RawFd, RawFd)>) {
        let user = self.user.as_raw_fd();
        let net = self.netns_fd(side.ns());
        let uid_map = format!("{} 0 1\n", self.uid).into_bytes();
        let gid_map = format!("{} 0 1\n", self.gid).into_bytes();
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: async-signal-safe syscalls only, on memory allocated before the fork.
        unsafe {
            cmd.pre_exec(move || {
                check(libc::setns(user, libc::CLONE_NEWUSER))?;
                check(libc::setns(net, libc::CLONE_NEWNET))?;
                check(libc::unshare(libc::CLONE_NEWUSER))?;
                write_proc(c"/proc/self/setgroups", b"deny")?;
                write_proc(c"/proc/self/uid_map", &uid_map)?;
                write_proc(c"/proc/self/gid_map", &gid_map)?;
                check(libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong))?;
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                if own_group {
                    check(libc::setpgid(0, 0))?;
                }
                if let Some((fd, at)) = dup {
                    check(libc::dup2(fd, at))?;
                }
                Ok(())
            });
        }
    }

    /// Spawns a workload (see [`Lab::workload_command`]) and remembers its process group, so
    /// dropping the lab kills it and its children.
    pub fn spawn(&self, side: Side, cmd: Command) -> Result<Child> {
        self.spawn_with_fd(side, cmd, None)
    }

    /// [`Lab::spawn`], with `fd` given to the workload as fd `at`.
    pub fn spawn_with_fd(&self, side: Side, mut cmd: Command, dup: Option<(RawFd, RawFd)>) -> Result<Child> {
        self.workload_command(side, &mut cmd, true, dup);
        let child = cmd.spawn().with_context(|| format!("spawning {:?} in the {} namespace", cmd, side.name()))?;
        self.groups.lock().unwrap().push(child.id() as i32);
        Ok(child)
    }

    /// Kills the process groups of the workloads [`Lab::spawn`] started: SIGTERM, then
    /// SIGKILL after `grace`.
    pub fn stop_workloads(&self, grace: std::time::Duration) {
        let groups = std::mem::take(&mut *self.groups.lock().unwrap());
        if groups.is_empty() {
            return;
        }
        for &g in &groups {
            unsafe { libc::kill(-g, libc::SIGTERM) };
        }
        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline {
            if groups.iter().all(|&g| unsafe { libc::kill(-g, 0) } != 0) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        for &g in &groups {
            unsafe { libc::kill(-g, libc::SIGKILL) };
        }
    }

    /// A socket of the given type, created in a namespace. Used in this process, it sends
    /// and receives in that namespace: the probes and floods run on threads here.
    pub fn socket(&self, ns: Ns, domain: socket2::Domain, ty: socket2::Type) -> Result<socket2::Socket> {
        let mut pair = [0 as RawFd; 2];
        check(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0, pair.as_mut_ptr()) })?;
        // SAFETY: socketpair just returned these.
        let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
        let user = self.user.as_raw_fd();
        let net = self.netns_fd(ns);
        let tx = theirs.as_raw_fd();
        let domain: libc::c_int = domain.into();
        let ty: libc::c_int = ty.into();
        let mut cmd = Command::new("true");
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: async-signal-safe syscalls only; the message buffers are on the child's
        // stack.
        unsafe {
            cmd.pre_exec(move || {
                check(libc::setns(user, libc::CLONE_NEWUSER))?;
                check(libc::setns(net, libc::CLONE_NEWNET))?;
                let s = libc::socket(domain, ty | libc::SOCK_CLOEXEC, 0);
                check(s)?;
                send_fd(tx, s)
            });
        }
        let status = cmd.status().context("creating a socket in the lab")?;
        drop(theirs);
        if !status.success() {
            bail!("the socket helper failed: {status}");
        }
        let fd = recv_fd(ours.as_raw_fd()).context("receiving the lab socket")?;
        Ok(socket2::Socket::from(fd))
    }

    /// A UDP socket bound in a namespace.
    pub fn udp(&self, ns: Ns, bind: SocketAddr) -> Result<std::net::UdpSocket> {
        let s = self.socket(ns, socket2::Domain::for_address(bind), socket2::Type::DGRAM)?;
        s.set_reuse_address(true)?;
        s.bind(&bind.into()).with_context(|| format!("binding {bind}"))?;
        Ok(s.into())
    }

    /// A TCP listener in a namespace.
    pub fn tcp_listen(&self, ns: Ns, bind: SocketAddr) -> Result<std::net::TcpListener> {
        let s = self.socket(ns, socket2::Domain::for_address(bind), socket2::Type::STREAM)?;
        s.set_reuse_address(true)?;
        s.bind(&bind.into()).with_context(|| format!("binding {bind}"))?;
        s.listen(16)?;
        Ok(s.into())
    }

    /// A TCP connection made from a namespace.
    pub fn tcp_connect(&self, ns: Ns, to: SocketAddr, timeout: std::time::Duration) -> Result<std::net::TcpStream> {
        let s = self.socket(ns, socket2::Domain::for_address(to), socket2::Type::STREAM)?;
        s.connect_timeout(&to.into(), timeout).with_context(|| format!("connecting to {to}"))?;
        Ok(s.into())
    }

    /// The client's addresses now.
    pub fn client_addrs(&self) -> ClientAddrs {
        *self.client_addrs.lock().unwrap()
    }

    /// The address a side is reachable at now.
    pub fn addr(&self, side: Side, v6: bool) -> IpAddr {
        match (side, v6) {
            (Side::Server, false) => SERVER_V4.into(),
            (Side::Server, true) => SERVER_V6.into(),
            (Side::Client, false) => self.client_addrs().v4.into(),
            (Side::Client, true) => self.client_addrs().v6.into(),
        }
    }

    /// Moves the client to the next address in its subnets, as a Wi-Fi roam or a switch
    /// from Wi-Fi to cellular would. The new address is added before the old one goes, so
    /// the default route stays; sockets bound to the old address stop working, and the
    /// server sees the client's packets arrive from a new source.
    pub fn roam(&self) -> Result<ClientAddrs> {
        let old = self.client_addrs();
        let host = old.v4.octets()[3];
        let next_host = if host >= 250 { 2 } else { host + 1 };
        let new = ClientAddrs {
            v4: Ipv4Addr::new(10, 77, 1, next_host),
            v6: Ipv6Addr::new(0xfd77, 0, 1, 0, 0, 0, 0, next_host as u16),
        };
        let mac = self.mac(Ns::Client, "eth0")?;
        self.ip_batch(
            Ns::Router,
            &format!(
                "neigh replace {} lladdr {mac} dev cli nud permanent\n\
                 neigh replace {} lladdr {mac} dev cli nud permanent\n",
                new.v4, new.v6
            ),
        )?;
        self.ip_batch(
            Ns::Client,
            &format!(
                "addr add {}/24 dev eth0\n\
                 addr add {}/64 dev eth0 nodad\n\
                 addr del {}/24 dev eth0\n\
                 addr del {}/64 dev eth0\n\
                 route replace default via {ROUTER_CLIENT_V4} dev eth0\n\
                 route replace ::/0 via {ROUTER_CLIENT_V6} dev eth0\n",
                new.v4, new.v6, old.v4, old.v6
            ),
        )?;
        *self.client_addrs.lock().unwrap() = new;
        Ok(new)
    }

    /// The interfaces a namespace has (`ip -o link`), for the leak check.
    pub fn links(&self, ns: Ns) -> Result<Vec<String>> {
        let out = self.run(ns, &["ip", "-o", "link", "show"])?;
        Ok(out
            .lines()
            .filter_map(|l| l.split(':').nth(1).map(|s| s.trim().split('@').next().unwrap_or("").to_string()))
            .collect())
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        if !self.attached {
            let groups = std::mem::take(&mut *self.groups.lock().unwrap());
            for g in groups {
                unsafe { libc::kill(-g, libc::SIGKILL) };
            }
        }
    }
}

/// The directory where running labs leave their [`LabInfo`] and control sockets.
pub fn runtime_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(Into::into).unwrap_or_else(std::env::temp_dir);
    std::path::Path::new(&base).join("quiver-netlab")
}

/// Labs other processes hold now (their info files whose process is still alive).
pub fn running_labs() -> Vec<LabInfo> {
    let mut labs = Vec::new();
    let Ok(dir) = std::fs::read_dir(runtime_dir()) else { return labs };
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json")
            && let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(info) = serde_json::from_str::<LabInfo>(&text)
        {
            if std::path::Path::new(&format!("/proc/{}", info.pid)).exists() {
                labs.push(info);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    labs.sort_by_key(|l| l.pid);
    labs
}

pub fn which(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(tool)).find(|p| p.is_file())
}

fn check(r: libc::c_int) -> io::Result<()> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Writes a `/proc/self` file in one `write`, as the id-map files demand. Async-signal-safe.
fn write_proc(path: &std::ffi::CStr, data: &[u8]) -> io::Result<()> {
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        check(fd)?;
        let n = libc::write(fd, data.as_ptr().cast(), data.len());
        let err = io::Error::last_os_error();
        libc::close(fd);
        if n != data.len() as isize {
            return Err(err);
        }
    }
    Ok(())
}

/// Opens the current network namespace and moves the fd to `at` (no close-on-exec), so it
/// survives the helper's exec. Async-signal-safe.
fn park_netns(at: RawFd) -> io::Result<()> {
    unsafe {
        let fd = libc::open(c"/proc/self/ns/net".as_ptr(), libc::O_RDONLY);
        check(fd)?;
        check(libc::dup2(fd, at))?;
        libc::close(fd);
    }
    Ok(())
}

/// Sends one fd over a unix datagram socket. Async-signal-safe: everything is on the stack.
fn send_fd(sock: RawFd, fd: RawFd) -> io::Result<()> {
    unsafe {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        let mut buf = [0u64; 8];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = buf.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        if libc::sendmsg(sock, &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn recv_fd(sock: RawFd) -> io::Result<OwnedFd> {
    unsafe {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
        let mut buf = [0u64; 8];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = buf.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&buf) as _;
        let flags = libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC;
        if libc::recvmsg(sock, &mut msg, flags) < 0 {
            return Err(io::Error::last_os_error());
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other("no fd in the message"));
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>());
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

/// Reads a line-oriented control request from a unix stream and returns the reply. The
/// control socket's protocol is one request line, one reply line.
pub fn control_request(path: &str, request: &str) -> Result<String> {
    let mut s = std::os::unix::net::UnixStream::connect(path).with_context(|| format!("connecting to {path}"))?;
    s.write_all(request.as_bytes())?;
    s.write_all(b"\n")?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    s.read_to_string(&mut reply)?;
    let reply = reply.trim().to_string();
    if let Some(err) = reply.strip_prefix("error: ") {
        return Err(anyhow!("{err}"));
    }
    Ok(reply)
}
