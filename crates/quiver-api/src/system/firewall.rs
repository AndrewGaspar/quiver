//! Firewalls: which one runs, which ports Quiver's servers need, from where, and whether the
//! rules already there (the person's own included) allow them.
//!
//! **Where from.** Only the LAN subnets of the active non-virtual interfaces and the tailnet
//! (`tailscale0`, or Tailscale's address ranges where a firewall can't match an interface).
//! Never from anywhere.
//!
//! **ufw** keeps its rules in `/etc/ufw/user.rules` and `user6.rules`, one `### tuple ###`
//! line per rule (`allow tcp 47984,47989,48010 0.0.0.0/0 any 192.168.50.0/24 in`, an app rule
//! adds `Quiver%20Moonlight -` before the direction, a comment is `comment=<hex>`). Arch's ufw
//! leaves them world-readable; where they aren't, the check says it can't tell without root
//! and the script asks `ufw status` before adding. Rules Quiver adds carry the comment
//! `quiver`, which is how `--undo` finds them and only them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};

use super::facts::{Facts, Features};

/// The ufw comment on every rule Quiver adds (`ufw status` shows `# quiver`).
pub const TAG: &str = "quiver";
pub const TAILSCALE_IFACE: &str = "tailscale0";
/// Tailscale's address ranges (CGNAT and its ULA), for firewalls that match sources only.
pub const TAILNET_V4: (Ipv4Addr, u8) = (Ipv4Addr::new(100, 64, 0, 0), 10);
pub const TAILNET_V6: (Ipv6Addr, u8) = (Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 0), 48);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Ufw,
    Firewalld,
    /// nftables or iptables rules loaded by their own service: Quiver doesn't edit them.
    Nftables,
    None,
}

impl Kind {
    pub fn of(f: &Facts) -> Kind {
        // A front end first: ufw and firewalld both run on nftables.
        if f.services.ufw {
            Kind::Ufw
        } else if f.services.firewalld {
            Kind::Firewalld
        } else if f.services.nftables {
            Kind::Nftables
        } else {
            Kind::None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Ufw => "ufw",
            Kind::Firewalld => "firewalld",
            Kind::Nftables => "nftables",
            Kind::None => "none",
        }
    }
}

/// An address block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Net {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Net {
    pub fn new(addr: IpAddr, prefix: u8) -> Net {
        Net { addr: mask(addr, prefix), prefix }
    }

    pub fn any(v6: bool) -> Net {
        if v6 { Net::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0) } else { Net::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0) }
    }

    pub fn parse(s: &str) -> Option<Net> {
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (a, p.parse().ok()?),
            None => (s, if s.contains(':') { 128 } else { 32 }),
        };
        let addr: IpAddr = a.parse().ok()?;
        Some(Net::new(addr, p))
    }

    pub fn is_any(&self) -> bool {
        self.prefix == 0
    }

    pub fn v6(&self) -> bool {
        self.addr.is_ipv6()
    }

    /// `other` lies wholly inside this block.
    pub fn contains(&self, other: &Net) -> bool {
        self.v6() == other.v6() && self.prefix <= other.prefix && mask(other.addr, self.prefix) == self.addr
    }
}

impl std::fmt::Display for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

fn mask(a: IpAddr, prefix: u8) -> IpAddr {
    match a {
        IpAddr::V4(v) => {
            let m = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix.min(32) as u32) };
            IpAddr::V4(Ipv4Addr::from(u32::from(v) & m))
        }
        IpAddr::V6(v) => {
            let m = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix.min(128) as u32) };
            IpAddr::V6(Ipv6Addr::from(u128::from(v) & m))
        }
    }
}

/// Where a rule allows a feature from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Scope {
    /// A LAN subnet on a real interface.
    Lan { iface: String, net: Net },
    /// The tailnet, through `tailscale0` (both address families).
    Tailnet,
}

impl Scope {
    pub fn describe(&self) -> String {
        match self {
            Scope::Lan { iface, net } => format!("{net} ({iface})"),
            Scope::Tailnet => format!("the tailnet ({TAILSCALE_IFACE})"),
        }
    }
}

/// Which scopes `--firewall` asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wanted {
    pub lan: bool,
    pub tailnet: bool,
}

impl Wanted {
    pub fn parse(list: &[String]) -> Result<Wanted, String> {
        let mut w = Wanted { lan: false, tailnet: false };
        for s in list.iter().flat_map(|s| s.split(',')).map(str::trim).filter(|s| !s.is_empty()) {
            match s {
                "lan" => w.lan = true,
                "tailnet" | "tailscale" => w.tailnet = true,
                "any" | "anywhere" | "all" | "everywhere" => return Err("Quiver only allows its ports from your LAN and your tailnet, never from anywhere (--firewall lan,tailnet)".into()),
                other => return Err(format!("--firewall takes lan and tailnet, not '{other}'")),
            }
        }
        Ok(w)
    }
}

/// The LAN subnets of the active non-virtual interfaces, and the tailnet when `tailscale0` is
/// there; only those `want` asks for (`None`: every scope this machine has).
pub fn scopes(f: &Facts, want: Option<Wanted>) -> Vec<Scope> {
    let want = want.unwrap_or(Wanted { lan: true, tailnet: true });
    let mut v = Vec::new();
    if want.lan {
        for i in f.ifaces.iter().filter(|i| i.up && !i.virtual_) {
            for (a, p) in &i.addrs {
                let lan = match a {
                    IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
                    // Link-local is no subnet to name; ULA and global prefixes are.
                    IpAddr::V6(v6) => !v6.is_loopback() && (v6.segments()[0] & 0xffc0) != 0xfe80 && !v6.is_unspecified(),
                };
                let net = Net::new(*a, *p);
                let s = Scope::Lan { iface: i.name.clone(), net };
                if lan && *p > 0 && !v.contains(&s) {
                    v.push(s);
                }
            }
        }
    }
    if want.tailnet && f.ifaces.iter().any(|i| i.name == TAILSCALE_IFACE) {
        v.push(Scope::Tailnet);
    }
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn name(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// A server whose ports a firewall must allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Server {
    Moonlight,
    Remote,
}

impl Server {
    /// The ufw profile name / firewalld service.
    pub fn profile(self) -> &'static str {
        match self {
            Server::Moonlight => "Quiver Moonlight",
            Server::Remote => "Quiver Remote",
        }
    }

    pub fn service(self) -> &'static str {
        match self {
            Server::Moonlight => "quiver-moonlight",
            Server::Remote => "quiver-remote",
        }
    }

    /// The port groups, as the profile lists them (`47984,47989,48010/tcp`), for these
    /// features. `lan`: mDNS too (it never crosses a tailnet).
    pub fn groups(self, f: &Features, lan: bool) -> Vec<PortGroup> {
        match self {
            Server::Moonlight => {
                let h = f.moonlight_http;
                let mut v = vec![
                    PortGroup { proto: Proto::Tcp, ranges: vec![(h - 5, h - 5), (h, h), (h + 21, h + 21)] },
                    PortGroup { proto: Proto::Udp, ranges: vec![(h + 9, h + 11)] },
                ];
                if lan && f.mdns {
                    v.push(PortGroup { proto: Proto::Udp, ranges: vec![(5353, 5353)] });
                }
                v
            }
            Server::Remote => vec![
                PortGroup { proto: Proto::Tcp, ranges: vec![(f.remote_port, f.remote_port)] },
                PortGroup { proto: Proto::Udp, ranges: vec![(f.stream_port, f.stream_port + crate::remote::STREAM_PORTS - 1)] },
            ],
        }
    }

    /// The shipped profile covers these features' ports exactly (default ports), so a rule
    /// can name the profile; otherwise it names the ports.
    pub fn profile_fits(self, f: &Features) -> bool {
        let d = Features::default();
        match self {
            Server::Moonlight => f.moonlight_http == d.moonlight_http,
            Server::Remote => f.remote_port == d.remote_port && f.stream_port == d.stream_port,
        }
    }

    pub fn on(self, f: &Features) -> bool {
        match self {
            Server::Moonlight => f.moonlight,
            Server::Remote => f.remote,
        }
    }

    /// It listens on the tailnet only, so only the tailnet needs rules.
    pub fn tailnet_only(self, f: &Features) -> bool {
        match self {
            Server::Moonlight => f.moonlight_tailnet_only,
            Server::Remote => f.remote_tailnet_only,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Server::Moonlight => "Moonlight",
            Server::Remote => "Quiver remote play",
        }
    }
}

/// Ports of one protocol, as ufw writes them (`47984,47989,48010/tcp`, `47998:48000/udp`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortGroup {
    pub proto: Proto,
    pub ranges: Vec<(u16, u16)>,
}

impl PortGroup {
    /// `47984,47989,48010` / `47998:48000` (ufw's form).
    pub fn ports(&self) -> String {
        self.ranges.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}:{b}") }).collect::<Vec<_>>().join(",")
    }

    pub fn spec(&self) -> String {
        format!("{}/{}", self.ports(), self.proto.name())
    }

    /// `TCP 47984, 47989, 48010` / `UDP 47998–48000`, for people.
    pub fn human(&self) -> String {
        let p = self.ranges.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}–{b}") }).collect::<Vec<_>>().join(", ");
        format!("{} {p}", self.proto.name().to_uppercase())
    }
}

// ------------------------------------------------------------------------------------------
// ufw

/// One `### tuple ###` rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UfwRule {
    pub action: String,
    /// `None`: any protocol.
    pub proto: Option<Proto>,
    /// `None`: any port.
    pub dports: Option<Vec<(u16, u16)>>,
    pub dst: Net,
    pub src: Net,
    pub app: Option<String>,
    /// `in`, `out`.
    pub direction: String,
    pub iface_in: Option<String>,
    pub comment: Option<String>,
}

impl UfwRule {
    pub fn tagged(&self) -> bool {
        self.comment.as_deref() == Some(TAG)
    }
}

fn parse_ports(s: &str) -> Option<Option<Vec<(u16, u16)>>> {
    if s == "any" {
        return Some(None);
    }
    let mut v = Vec::new();
    for part in s.split(',') {
        let (a, b) = part.split_once(':').unwrap_or((part, part));
        v.push((a.parse().ok()?, b.parse().ok()?));
    }
    Some(Some(v))
}

fn unhex(s: &str) -> Option<String> {
    let b: Option<Vec<u8>> = (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect();
    String::from_utf8(b?).ok()
}

/// The rules in a `user.rules` / `user6.rules` file.
pub fn parse_ufw(text: &str) -> Vec<UfwRule> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(t) = line.strip_prefix("### tuple ###") else { continue };
        let (t, comment) = match t.split_once(" comment=") {
            Some((t, c)) => (t, unhex(c.trim())),
            None => (t, None),
        };
        let f: Vec<&str> = t.split_whitespace().collect();
        // action proto dport dst sport src [dapp sapp] direction[_iface]
        if f.len() != 7 && f.len() != 9 {
            continue;
        }
        let (Some(dports), Some(dst), Some(src)) = (parse_ports(f[2]), Net::parse(f[3]), Net::parse(f[5])) else { continue };
        let proto = match f[1] {
            "tcp" => Some(Proto::Tcp),
            "udp" => Some(Proto::Udp),
            "any" => None,
            _ => continue,
        };
        let app = (f.len() == 9 && f[6] != "-").then(|| f[6].replace("%20", " "));
        let last = f[f.len() - 1];
        let (direction, iface) = match last.split_once('_') {
            Some((d, i)) => (d.to_owned(), Some(i.split('!').next().unwrap_or(i).to_owned())),
            None => (last.to_owned(), None),
        };
        let action = f[0].split(':').next_back().unwrap_or(f[0]).to_owned();
        let routed = f[0].starts_with("route:");
        if routed {
            continue;
        }
        out.push(UfwRule { action, proto, dports, dst, src, app, iface_in: (direction == "in").then_some(iface).flatten(), direction, comment });
    }
    out
}

fn rule_allows(r: &UfwRule, proto: Proto, port: u16, scope: &Scope, v6: bool) -> bool {
    if r.action != "allow" || r.direction != "in" || r.src.v6() != v6 || !r.dst.is_any() {
        return false;
    }
    if r.proto.is_some_and(|p| p != proto) {
        return false;
    }
    if let Some(d) = &r.dports
        && !d.iter().any(|(a, b)| (*a..=*b).contains(&port))
    {
        return false;
    }
    match scope {
        Scope::Lan { iface, net } => r.iface_in.as_ref().is_none_or(|i| i == iface) && r.src.contains(net),
        Scope::Tailnet => {
            let range = if v6 { Net::new(IpAddr::V6(TAILNET_V6.0), TAILNET_V6.1) } else { Net::new(IpAddr::V4(TAILNET_V4.0), TAILNET_V4.1) };
            match r.iface_in.as_deref() {
                Some(i) => i == TAILSCALE_IFACE && (r.src.is_any() || r.src.contains(&range)),
                None => r.src.contains(&range),
            }
        }
    }
}

/// How one scope stands for one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub scope: Scope,
    /// What isn't allowed yet (empty: all of it is).
    pub missing: Vec<PortGroup>,
    /// Some of it is allowed by rules Quiver didn't add (the person's own).
    pub by_hand: bool,
    /// Some of it is allowed by Quiver's tagged rules.
    pub by_quiver: bool,
    /// A rule allows some of it from anywhere (not Quiver's doing; said so).
    pub from_anywhere: bool,
}

/// Which of `groups` the rules allow from `scope` (both families for the tailnet; a LAN
/// subnet's own family).
pub fn ufw_coverage(v4: &[UfwRule], v6: &[UfwRule], groups: &[PortGroup], scope: &Scope) -> Coverage {
    let families: Vec<bool> = match scope {
        Scope::Lan { net, .. } => vec![net.v6()],
        Scope::Tailnet => vec![false, true],
    };
    let mut c = Coverage { scope: scope.clone(), missing: Vec::new(), by_hand: false, by_quiver: false, from_anywhere: false };
    for g in groups {
        let mut miss: Vec<(u16, u16)> = Vec::new();
        for &(a, b) in &g.ranges {
            for port in a..=b {
                let mut ok = true;
                for &fam in &families {
                    let rules = if fam { v6 } else { v4 };
                    match rules.iter().find(|r| rule_allows(r, g.proto, port, scope, fam)) {
                        Some(r) => {
                            if r.tagged() {
                                c.by_quiver = true;
                            } else {
                                c.by_hand = true;
                            }
                            if r.src.is_any() && r.iface_in.is_none() {
                                c.from_anywhere = true;
                            }
                        }
                        None => ok = false,
                    }
                }
                if !ok {
                    match miss.last_mut() {
                        Some((_, e)) if *e + 1 == port => *e = port,
                        _ => miss.push((port, port)),
                    }
                }
            }
        }
        if !miss.is_empty() {
            c.missing.push(PortGroup { proto: g.proto, ranges: miss });
        }
    }
    c
}

// ------------------------------------------------------------------------------------------
// firewalld

/// What the permanent zone files allow, roughly: rich rules naming one of Quiver's services
/// with a source, and services or ports open zone-wide.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FirewalldZones {
    /// (service, source) from rich rules.
    pub rich: Vec<(String, Net)>,
    /// Services enabled for a whole zone.
    pub services: Vec<String>,
    /// (proto, from, to) opened for a whole zone.
    pub ports: Vec<(Proto, u16, u16)>,
}

/// Parse `/etc/firewalld/zones/*.xml` texts (good enough for what firewalld writes).
pub fn parse_firewalld(zones: &[String]) -> FirewalldZones {
    let attr = |tag: &str, name: &str| -> Option<String> {
        let i = tag.find(&format!("{name}=\""))? + name.len() + 2;
        tag[i..].split('"').next().map(str::to_owned)
    };
    let mut z = FirewalldZones::default();
    for text in zones {
        let mut rest = text.as_str();
        // Rich rules first, then cut them out so their <service>/<port> aren't zone-wide.
        let mut flat = String::new();
        while let Some(i) = rest.find("<rule") {
            flat.push_str(&rest[..i]);
            let end = rest[i..].find("</rule>").map(|e| i + e + 7).unwrap_or(rest.len());
            let rule = &rest[i..end];
            let src = rule.find("<source").and_then(|s| attr(&rule[s..], "address")).and_then(|a| Net::parse(&a));
            if let (Some(s), Some(src)) = (rule.find("<service").and_then(|s| attr(&rule[s..], "name")), src)
                && rule.contains("<accept")
            {
                z.rich.push((s, src));
            }
            rest = &rest[end..];
        }
        flat.push_str(rest);
        for (i, _) in flat.match_indices("<service") {
            if let Some(n) = attr(&flat[i..], "name") {
                z.services.push(n);
            }
        }
        for (i, _) in flat.match_indices("<port") {
            let tag = &flat[i..flat[i..].find('>').map(|e| i + e).unwrap_or(flat.len())];
            let proto = match attr(tag, "protocol").as_deref() {
                Some("tcp") => Proto::Tcp,
                Some("udp") => Proto::Udp,
                _ => continue,
            };
            if let Some(p) = attr(tag, "port") {
                let (a, b) = p.split_once('-').unwrap_or((&p, &p));
                if let (Ok(a), Ok(b)) = (a.parse(), b.parse()) {
                    z.ports.push((proto, a, b));
                }
            }
        }
    }
    z
}

/// The same question as [`ufw_coverage`], for firewalld.
pub fn firewalld_coverage(z: &FirewalldZones, server: Server, groups: &[PortGroup], scope: &Scope) -> Coverage {
    let mut c = Coverage { scope: scope.clone(), missing: Vec::new(), by_hand: false, by_quiver: false, from_anywhere: false };
    let sources: Vec<Net> = match scope {
        Scope::Lan { net, .. } => vec![*net],
        Scope::Tailnet => vec![Net::new(IpAddr::V4(TAILNET_V4.0), TAILNET_V4.1), Net::new(IpAddr::V6(TAILNET_V6.0), TAILNET_V6.1)],
    };
    let ours = sources.iter().all(|s| z.rich.iter().any(|(svc, src)| svc == server.service() && src.contains(s)));
    if ours {
        c.by_quiver = true;
        return c;
    }
    if z.services.iter().any(|s| s == server.service()) {
        c.by_hand = true;
        return c;
    }
    for g in groups {
        let miss: Vec<(u16, u16)> = g.ranges.iter().copied().filter(|(a, b)| !z.ports.iter().any(|(p, x, y)| *p == g.proto && x <= a && b <= y)).collect();
        if miss.len() < g.ranges.len() {
            c.by_hand = true;
        }
        if !miss.is_empty() {
            c.missing.push(PortGroup { proto: g.proto, ranges: miss });
        }
    }
    c
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// This machine's /etc/ufw/user.rules (2026-11-21), the person's own rules: Moonlight from
    /// the LAN and tailscale0, no comment.
    pub const USER_RULES: &str = "### RULES ###\n\
### tuple ### allow udp 53317 0.0.0.0/0 any 0.0.0.0/0 in\n\
-A ufw-user-input -p udp --dport 53317 -j ACCEPT\n\
### tuple ### allow udp 53 172.17.0.1 any 172.16.0.0/12 in comment=616c6c6f772d646f636b65722d646e73\n\
### tuple ### allow any any 0.0.0.0/0 any 0.0.0.0/0 in_virbr0\n\
### tuple ### route:allow any any 0.0.0.0/0 any 0.0.0.0/0 in_virbr0\n\
### tuple ### allow any 9757 0.0.0.0/0 any 0.0.0.0/0 WiVRn - in\n\
### tuple ### allow tcp 47984,47989,48010 0.0.0.0/0 any 192.168.50.0/24 in\n\
### tuple ### allow udp 47998:48000 0.0.0.0/0 any 192.168.50.0/24 in\n\
### tuple ### allow udp 5353 0.0.0.0/0 any 192.168.50.0/24 in\n\
### tuple ### allow tcp 47984,47989,48010 0.0.0.0/0 any 0.0.0.0/0 in_tailscale0\n\
### tuple ### allow udp 47998:48000 0.0.0.0/0 any 0.0.0.0/0 in_tailscale0\n\
### END RULES ###\n";
    pub const USER6_RULES: &str = "### tuple ### allow tcp 47984,47989,48010 ::/0 any ::/0 in_tailscale0\n\
### tuple ### allow udp 47998:48000 ::/0 any ::/0 in_tailscale0\n";

    fn lan() -> Scope {
        Scope::Lan { iface: "wlp191s0".into(), net: Net::parse("192.168.50.0/24").unwrap() }
    }

    #[test]
    fn tuples() {
        let r = parse_ufw(USER_RULES);
        assert_eq!(r.len(), 9, "the routed rule is skipped");
        assert_eq!(r[1].comment.as_deref(), Some("allow-docker-dns"));
        assert_eq!(r[2].iface_in.as_deref(), Some("virbr0"));
        assert_eq!(r[3].app.as_deref(), Some("WiVRn"));
        assert_eq!(r[4].dports, Some(vec![(47984, 47984), (47989, 47989), (48010, 48010)]));
        assert_eq!(r[5].dports, Some(vec![(47998, 48000)]));
        let app = parse_ufw("### tuple ### allow tcp 47990 0.0.0.0/0 any 192.168.50.0/24 Quiver%20Remote - in comment=717569766572\n");
        assert_eq!(app[0].app.as_deref(), Some("Quiver Remote"));
        assert!(app[0].tagged());
    }

    #[test]
    fn the_persons_hand_rules_cover_moonlight() {
        let (v4, v6) = (parse_ufw(USER_RULES), parse_ufw(USER6_RULES));
        let f = Features { moonlight: true, remote: true, ..Default::default() };
        let c = ufw_coverage(&v4, &v6, &Server::Moonlight.groups(&f, true), &lan());
        assert!(c.missing.is_empty() && c.by_hand && !c.by_quiver && !c.from_anywhere, "{c:?}");
        // The tailnet needs no mDNS, and both families are allowed on tailscale0.
        let c = ufw_coverage(&v4, &v6, &Server::Moonlight.groups(&f, false), &Scope::Tailnet);
        assert!(c.missing.is_empty() && c.by_hand, "{c:?}");
        // Remote play: only the Moonlight UDP ports overlap.
        let c = ufw_coverage(&v4, &v6, &Server::Remote.groups(&f, true), &lan());
        assert_eq!(c.missing.iter().map(PortGroup::spec).collect::<Vec<_>>(), ["47990/tcp", "48001:48061/udp"]);
        // Without the v6 tailscale0 rules, the tailnet isn't fully allowed.
        let c = ufw_coverage(&v4, &[], &Server::Moonlight.groups(&f, false), &Scope::Tailnet);
        assert!(!c.missing.is_empty());
    }

    #[test]
    fn anywhere_rules_count_and_are_named() {
        let v4 = parse_ufw("### tuple ### allow any 47990 0.0.0.0/0 any 0.0.0.0/0 in\n");
        let g = vec![PortGroup { proto: Proto::Tcp, ranges: vec![(47990, 47990)] }];
        let c = ufw_coverage(&v4, &[], &g, &lan());
        assert!(c.missing.is_empty() && c.from_anywhere);
        // Another subnet, another interface or a deny don't.
        for t in ["allow tcp 47990 0.0.0.0/0 any 10.0.0.0/8 in", "allow tcp 47990 0.0.0.0/0 any 0.0.0.0/0 in_eth1", "deny tcp 47990 0.0.0.0/0 any 0.0.0.0/0 in", "allow tcp 47990 0.0.0.0/0 any 0.0.0.0/0 out"] {
            let r = parse_ufw(&format!("### tuple ### {t}\n"));
            assert!(!ufw_coverage(&r, &[], &g, &lan()).missing.is_empty(), "{t}");
        }
        // A wider subnet covers a narrower one.
        let r = parse_ufw("### tuple ### allow tcp 47990 0.0.0.0/0 any 192.168.0.0/16 in\n");
        assert!(ufw_coverage(&r, &[], &g, &lan()).missing.is_empty());
    }

    #[test]
    fn scopes_are_lan_and_tailnet_only() {
        let f = Facts {
            root: "/".into(),
            user: "p".into(),
            uid: 1000,
            gid: 1000,
            groups: vec![],
            seated: true,
            uinput_gid: None,
            ifaces: vec![
                super::super::facts::Iface { name: "lo".into(), virtual_: true, up: true, addrs: vec![("127.0.0.1".parse().unwrap(), 8)] },
                super::super::facts::Iface { name: "tailscale0".into(), virtual_: true, up: true, addrs: vec![("100.119.207.102".parse().unwrap(), 32)] },
                super::super::facts::Iface { name: "docker0".into(), virtual_: true, up: true, addrs: vec![("172.17.0.1".parse().unwrap(), 16)] },
                super::super::facts::Iface {
                    name: "wlp191s0".into(),
                    virtual_: false,
                    up: true,
                    addrs: vec![("192.168.50.189".parse().unwrap(), 24), ("fe80::5cae:5291:4c71:9167".parse().unwrap(), 64), ("2001:db8:1:2::5".parse().unwrap(), 64)],
                },
                super::super::facts::Iface { name: "enp3s0".into(), virtual_: false, up: false, addrs: vec![("10.0.0.5".parse().unwrap(), 24)] },
            ],
            services: Default::default(),
            linger: false,
            linger_needs_root: None,
            boot_enabled: false,
            features: Default::default(),
            contrib: None,
        };
        let s = scopes(&f, None);
        assert_eq!(s.iter().map(Scope::describe).collect::<Vec<_>>(), ["192.168.50.0/24 (wlp191s0)", "2001:db8:1:2::/64 (wlp191s0)", "the tailnet (tailscale0)"]);
        assert_eq!(scopes(&f, Some(Wanted { lan: false, tailnet: true })), [Scope::Tailnet]);
        assert!(Wanted::parse(&["lan,anywhere".into()]).is_err());
        assert_eq!(Wanted::parse(&["lan,tailnet".into()]), Ok(Wanted { lan: true, tailnet: true }));
    }

    #[test]
    fn firewalld_zones() {
        let zone = r#"<?xml version="1.0" encoding="utf-8"?>
<zone><short>Home</short><service name="ssh"/><port port="47984" protocol="tcp"/>
<rule family="ipv4"><source address="192.168.50.0/24"/><service name="quiver-remote"/><accept/></rule>
</zone>"#;
        let z = parse_firewalld(&[zone.to_owned()]);
        assert_eq!(z.rich, vec![("quiver-remote".to_owned(), Net::parse("192.168.50.0/24").unwrap())]);
        assert_eq!(z.services, ["ssh"], "a rich rule's service isn't zone-wide");
        let f = Features { remote: true, moonlight: true, ..Default::default() };
        assert!(firewalld_coverage(&z, Server::Remote, &Server::Remote.groups(&f, true), &lan()).by_quiver);
        let m = firewalld_coverage(&z, Server::Moonlight, &Server::Moonlight.groups(&f, true), &lan());
        assert!(m.by_hand && m.missing.iter().map(PortGroup::spec).collect::<Vec<_>>() == ["47989,48010/tcp", "47998:48000/udp", "5353/udp"], "{m:?}");
    }
}
