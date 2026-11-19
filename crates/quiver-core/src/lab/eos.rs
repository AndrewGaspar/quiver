//! Epic Online Services in the lab (design/LAB.md section 3.1): what an EOS game may send to
//! Epic from an instance, and how to tell whether it did.
//!
//! A game on the EOS SDK talks to Epic itself with the person's real licence, all on one host,
//! `api.epicgames.dev`: sign-in and ownership, and also cloud saves, achievements, stats and
//! telemetry. AWR's SDK (1.7.1) is libcurl over a statically linked OpenSSL, so nothing at the
//! Windows API layer sees its plaintext. The lab therefore points `api.epicgames.dev` (and
//! Epic's other names the SDK knows) at the instance's EOS proxy ([`super::eosproxy`]): the
//! container resolves them to 127.0.0.1, and pasta forwards the container's port 443 to the
//! proxy in the instance's supervisor on the host. The proxy terminates TLS with a certificate
//! from the instance's own CA (trusted only in the instance's cloned prefix,
//! [`super::wincert`]), lets through only [`ALLOWLIST`], and answers everything else 403.
//!
//! This module is the policy: the allowlist (the one place to change it), the hosts, which
//! games need it, and reading the decisions back.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// The one host the proxy forwards (allowed requests only).
pub const FORWARDED_HOST: &str = "api.epicgames.dev";

/// Epic's other names in AWR's EOS SDK. The container resolves them to the proxy too, which
/// refuses their TLS (it has no certificate for them) and logs the attempt: the SDK's dev
/// environment, and the NAT/STUN helpers for P2P (UDP: nothing listens).
pub const SINKHOLED_HOSTS: &[&str] = &["api-gamedev.ol.epicgames.net", "geo-nat.ol.epicgames.com", "stun-1-nat.ol.epicgames.com", "stun-2-nat.ol.epicgames.com"];

/// Every name the container resolves to the proxy.
pub fn redirected_hosts() -> Vec<&'static str> {
    std::iter::once(FORWARDED_HOST).chain(SINKHOLED_HOSTS.iter().copied()).collect()
}

/// The proxy's decisions log, in the run folder.
pub const LOG_FILE: &str = "eos-filter.log";

/// One allowed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allow {
    pub method: &'static str,
    /// Matched against the path without its query, segment by segment, case-sensitively:
    /// a literal segment; `{}` any one non-empty segment (an id, a platform); `{v}` a version
    /// segment (`v` and one or two digits: SDK 1.7 says v1, the docs v2-v4).
    pub pattern: &'static str,
    /// What it is, as the log shows it.
    pub why: &'static str,
    /// Where the route comes from.
    pub source: &'static str,
}

const SDK: &str = "route strings in AWR's EOSSDK-Win64-Shipping.dll 1.7.1";
const AUTH: &str = "dev.epicgames.com/docs/web-api-ref/authentication";
const CONN: &str = "dev.epicgames.com/docs/web-api-ref/connect-web-api";
const ECOM: &str = "dev.epicgames.com/docs/web-api-ref/ecom-web-apis";
const MIRROR: &str = "the EOS OpenAPI mirror at apis.io (epic-games-ecom-api), the same resources without /epic";

/// **The EOS allowlist**: the only requests a lab instance may send to `api.epicgames.dev`.
/// Everything else is answered 403 and never reaches Epic: cloud saves (Player Data Storage),
/// Title Storage, achievements, stats, leaderboards, presence, friends, sessions, lobbies, user
/// lookups, metrics and SDK telemetry, and anything unrecognised. An entry only ever allows
/// sign-in and ownership: never add one that writes to the person's account (redeeming
/// entitlements, creating or linking accounts, unlocking, ingesting, uploading) or reports
/// anything. Document an addition in design/LAB.md section 3.1 with its source.
pub const ALLOWLIST: &[Allow] = &[
    // --- SDK configuration (read only) ---
    // "[SDKConfigService] BaseUrl=https://api.epicgames.dev/sdk" + "/v1/default": the SDK's
    // default settings for the platform.
    Allow { method: "GET", pattern: "/sdk/v1/default", why: "SDK config (default)", source: SDK },
    // "/v1/product/{productId}": the product's service URLs and flags; without it the SDK
    // never initialises.
    Allow { method: "GET", pattern: "/sdk/v1/product/{}", why: "SDK config (product)", source: SDK },
    // --- Epic account sign-in (Epic Account Services) ---
    // "/epic/oauth/v1/token" (SDK), POST /epic/oauth/v2/token (docs): the launch's single-use
    // exchange code (and later its refresh token) for the game's access token.
    Allow { method: "POST", pattern: "/epic/oauth/{v}/token", why: "Epic account sign-in (token)", source: AUTH },
    // "/epic/oauth/v1/tokenInfo": checks that token.
    Allow { method: "POST", pattern: "/epic/oauth/{v}/tokenInfo", why: "Epic account token check", source: AUTH },
    Allow { method: "GET", pattern: "/epic/oauth/{v}/tokenInfo", why: "Epic account token check", source: AUTH },
    // "/epic/oauth/v1/revoke": the SDK ending its own token at logout. Only this launch's game
    // session (never quiverd's); better than leaving it live until it expires.
    Allow { method: "POST", pattern: "/epic/oauth/{v}/revoke", why: "Epic account sign-out (the lab's own token)", source: AUTH },
    // GET /epic/oauth/v2/.well-known/jwks.json: Epic's public keys, to verify ID tokens.
    Allow { method: "GET", pattern: "/epic/oauth/{v}/.well-known/jwks.json", why: "Epic account public keys", source: AUTH },
    // --- EOS Connect sign-in ---
    // "/auth/v1/oauth/token" (SDK), POST /auth/v1/oauth/token (docs): the Connect login for the
    // existing product user, and the SDK's client credentials. Not "/auth/v1/users" (creates a
    // product user) or "/auth/v1/accounts*" (links accounts, makes device ids): account writes.
    Allow { method: "POST", pattern: "/auth/v1/oauth/token", why: "EOS Connect sign-in (token)", source: CONN },
    // --- Ownership and entitlements (read only: checks, never redeems) ---
    // "/v1/platforms/{platform}/identities/{identityId}/ownership" (SDK), GET
    // /epic/ecom/v3/platforms/EPIC/identities/{id}/ownership?nsCatalogItemId=… (docs).
    Allow { method: "GET", pattern: "/epic/ecom/{v}/platforms/{}/identities/{}/ownership", why: "ownership check", source: ECOM },
    Allow { method: "GET", pattern: "/ecom/{v}/platforms/{}/identities/{}/ownership", why: "ownership check", source: MIRROR },
    // ".../ownershipToken" (SDK), POST (docs): a signed statement of the same.
    Allow { method: "POST", pattern: "/epic/ecom/{v}/platforms/{}/identities/{}/ownershipToken", why: "ownership token", source: ECOM },
    Allow { method: "POST", pattern: "/ecom/{v}/platforms/{}/identities/{}/ownershipToken", why: "ownership token", source: MIRROR },
    // "/v1/identities/{identityId}/entitlements" (SDK), GET /epic/ecom/v4/identities/{id}/entitlements
    // (docs): the account's entitlements (the game and its DLC). Not ".../entitlements/redeem".
    Allow { method: "GET", pattern: "/epic/ecom/{v}/identities/{}/entitlements", why: "entitlements (read)", source: ECOM },
    Allow { method: "GET", pattern: "/ecom/{v}/identities/{}/entitlements", why: "entitlements (read)", source: MIRROR },
    // POST /epic/ecom/v4/platforms/{platform}/identities/{id}/entitlementToken (docs): a signed
    // statement of the entitlements (newer SDKs).
    Allow { method: "POST", pattern: "/epic/ecom/{v}/platforms/{}/identities/{}/entitlementToken", why: "entitlement token", source: ECOM },
];

fn seg_match(pat: &str, s: &str) -> bool {
    match pat {
        "{}" => !s.is_empty(),
        "{v}" => s.len() >= 2 && s.len() <= 3 && s.starts_with('v') && s[1..].bytes().all(|b| b.is_ascii_digit()),
        _ => pat == s,
    }
}

/// A path that can match at all: absolute, no escapes, backslashes, controls, spaces, empty,
/// `.` or `..` segments.
fn plain(path: &str) -> bool {
    path.starts_with('/')
        && path.bytes().all(|b| b > b' ' && b < 0x7f && b != b'%' && b != b'\\')
        && path[1..].split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

/// The allowlist entry for a request (`path` without its query), or `None`: blocked.
pub fn rule(method: &str, path: &str) -> Option<&'static Allow> {
    if !plain(path) {
        return None;
    }
    ALLOWLIST.iter().find(|a| {
        a.method == method && {
            let (mut p, mut s) = (a.pattern[1..].split('/'), path[1..].split('/'));
            loop {
                match (p.next(), s.next()) {
                    (None, None) => break true,
                    (Some(x), Some(y)) if seg_match(x, y) => continue,
                    _ => break false,
                }
            }
        }
    })
}

/// A request target for the log: the path, and the query's keys only (`?a=…&b=…`): values
/// can be ids or codes.
pub fn shown_target(target: &str) -> String {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut out: String = path.chars().take(300).collect();
    if !target.contains('?') {
        return out;
    }
    let keys: Vec<String> = query.split('&').filter(|p| !p.is_empty()).map(|p| format!("{}=\u{2026}", p.split('=').next().unwrap_or(p).chars().take(40).collect::<String>())).collect();
    out.push('?');
    out.push_str(&keys.join("&"));
    out
}

/// The EOS SDK's DLLs.
const SDK_DLLS: &[&str] = &["eossdk-win64-shipping.dll", "eossdk-win32-shipping.dll"];

/// The EOS SDK DLLs in an install (relative paths).
pub fn eos_sdks(install: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = super::facts::walk(install, 10, 200_000)
        .into_iter()
        .filter(|(p, d)| !d && p.file_name().is_some_and(|n| SDK_DLLS.contains(&n.to_string_lossy().to_ascii_lowercase().as_str())))
        .map(|(p, _)| p)
        .collect();
    out.sort();
    out
}

/// Whether a game's launches go through the EOS proxy's checks.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The game ships the EOS SDK: a launch without a working proxy is refused.
    pub required: bool,
    /// The SDK DLLs found (relative to the install).
    pub sdks: Vec<PathBuf>,
}

/// `None` when the game needs no checks (not an Epic game, no EOS SDK in it). The redirect
/// itself is in every instance's container regardless.
pub fn plan(install: &Path, epic: bool) -> Option<Plan> {
    let sdks = eos_sdks(install);
    (epic || !sdks.is_empty()).then_some(Plan { required: !sdks.is_empty(), sdks })
}

/// The secur32.dll filter an earlier Quiver put in instances (it never saw the SDK's TLS):
/// deleted wherever it is in the instance's install clone. Returns what was deleted.
pub fn remove_old_filter(install_clone: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for (rel, is_dir) in super::facts::walk(install_clone, 10, 200_000) {
        if is_dir || !rel.file_name().is_some_and(|n| n.eq_ignore_ascii_case("secur32.dll")) {
            continue;
        }
        let p = install_clone.join(&rel);
        let ours = std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_file()) && std::fs::read(&p).is_ok_and(|b| b.windows(19).any(|w| w == b"QUIVER-EOS-FILTER/1"));
        if ours && std::fs::remove_file(&p).is_ok() {
            out.push(p);
        }
    }
    out
}

/// What the proxy's log says about a run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub probes: usize,
    pub tls_ok: usize,
    pub tls_failed: usize,
    /// Rejections of the lab's certificate (alerts like unknown_ca).
    pub untrusted: usize,
    pub refused: usize,
    pub allowed: usize,
    pub blocked: usize,
    pub upstream_errors: usize,
    pub no_request: usize,
    /// Connections to the real host's addresses that didn't go through the proxy.
    pub bypasses: usize,
    /// Other :443 connections that didn't go through it.
    pub outside: usize,
    /// "METHOD target" of blocked requests, with how often.
    pub blocked_requests: BTreeMap<String, usize>,
    pub bypass_lines: Vec<String>,
    pub outside_lines: Vec<String>,
}

impl Summary {
    /// The one-line verdict an agent (or the person) reads first.
    pub fn verdict(&self) -> String {
        if self.bypasses > 0 {
            return format!("{} connection(s) to Epic's addresses BYPASSED the proxy (the run was killed); see the BYPASS lines", self.bypasses);
        }
        if self.tls_ok == 0 && self.untrusted + self.tls_failed > 0 {
            return "the game never completed TLS with the proxy (it rejected the lab's certificate or gave up): EOS was offline for this run (fail closed; nothing reached Epic)".into();
        }
        if self.tls_ok == 0 {
            return "the game never connected to api.epicgames.dev through the proxy (no EOS traffic, or it didn't start yet)".into();
        }
        if self.allowed + self.blocked == 0 {
            return "TLS was accepted but no request followed (the SDK may pin Epic's key: look for \"pinned public key\" in the game's log); EOS was offline".into();
        }
        let outside = if self.outside > 0 { format!("; {} other :443 connection(s) went around it, see the OUTSIDE lines", self.outside) } else { String::new() };
        format!("the proxy saw the game's EOS traffic: {} allowed, {} blocked (403){outside}", self.allowed, self.blocked)
    }
}

pub fn summarize(log: &str) -> Summary {
    let mut s = Summary::default();
    for line in log.lines() {
        let rest = line.split_once(' ').map(|(_, r)| r).unwrap_or(line);
        let word = rest.split(' ').next().unwrap_or_default();
        match word {
            "probe" => s.probes += 1,
            "tls" if rest.starts_with("tls ok ") => s.tls_ok += 1,
            "tls" => {
                s.tls_failed += 1;
                if rest.contains("rejected the lab's certificate") {
                    s.untrusted += 1;
                }
            }
            "refused" => s.refused += 1,
            "allow" => s.allowed += 1,
            "block" => {
                s.blocked += 1;
                let what = rest["block ".len().min(rest.len())..].split(" -- ").next().unwrap_or_default().to_owned();
                *s.blocked_requests.entry(what).or_default() += 1;
            }
            "upstream" => s.upstream_errors += 1,
            "closed" if rest.contains("without a request") => s.no_request += 1,
            "!!!" if rest.starts_with("!!! BYPASS") => {
                s.bypasses += 1;
                s.bypass_lines.push(rest.to_owned());
            }
            "!!!" if rest.starts_with("!!! OUTSIDE") => {
                s.outside += 1;
                s.outside_lines.push(rest.to_owned());
            }
            _ => {}
        }
    }
    s
}

/// A TCP connection of a network namespace (`/proc/<pid>/net/tcp` or `tcp6`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TcpConn {
    pub remote: IpAddr,
    pub port: u16,
    /// `ESTABLISHED`, `SYN_SENT`, … (the kernel's state names).
    pub state: &'static str,
}

/// Connections to a remote :443 that aren't to this namespace's loopback (where the proxy
/// is): what bypassed the proxy. Listening sockets and closed ones are left out.
pub fn outside_443(tcp: &str, tcp6: &str) -> Vec<TcpConn> {
    let mut out = Vec::new();
    for (text, v6) in [(tcp, false), (tcp6, true)] {
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (Some(rem), Some(st)) = (f.get(2), f.get(3)) else { continue };
            let state = match *st {
                "01" => "ESTABLISHED",
                "02" => "SYN_SENT",
                "03" => "SYN_RECV",
                "04" => "FIN_WAIT1",
                "05" => "FIN_WAIT2",
                "08" => "CLOSE_WAIT",
                _ => continue,
            };
            let Some((addr, port)) = rem.split_once(':') else { continue };
            let Ok(port) = u16::from_str_radix(port, 16) else { continue };
            let Some(ip) = parse_proc_addr(addr, v6) else { continue };
            let ip = match ip {
                IpAddr::V6(a) => a.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(a)),
                v4 => v4,
            };
            if port == 443 && !ip.is_loopback() && !ip.is_unspecified() {
                out.push(TcpConn { remote: ip, port, state });
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn parse_proc_addr(hex: &str, v6: bool) -> Option<IpAddr> {
    if !v6 {
        let n = u32::from_str_radix(hex, 16).ok()?;
        return Some(IpAddr::V4(std::net::Ipv4Addr::from(n.to_le_bytes())));
    }
    if hex.len() != 32 {
        return None;
    }
    let mut b = [0u8; 16];
    for w in 0..4 {
        let n = u32::from_str_radix(&hex[w * 8..w * 8 + 8], 16).ok()?;
        b[w * 4..w * 4 + 4].copy_from_slice(&n.to_le_bytes());
    }
    Some(IpAddr::V6(std::net::Ipv6Addr::from(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allowlist() {
        for (m, p) in [
            ("GET", "/sdk/v1/default"),
            ("GET", "/sdk/v1/product/prod123"),
            ("POST", "/epic/oauth/v1/token"),
            ("POST", "/epic/oauth/v2/token"),
            ("POST", "/epic/oauth/v1/tokenInfo"),
            ("POST", "/epic/oauth/v1/revoke"),
            ("GET", "/epic/oauth/v2/.well-known/jwks.json"),
            ("POST", "/auth/v1/oauth/token"),
            ("GET", "/epic/ecom/v1/platforms/EPIC/identities/abc/ownership"),
            ("POST", "/epic/ecom/v3/platforms/EPIC/identities/abc/ownershipToken"),
            ("GET", "/epic/ecom/v4/identities/abc/entitlements"),
            ("POST", "/epic/ecom/v4/platforms/EPIC/identities/abc/entitlementToken"),
            ("GET", "/ecom/v1/identities/abc/entitlements"),
        ] {
            assert!(rule(m, p).is_some(), "{m} {p} should be allowed");
        }
        for (m, p) in [
            ("POST", "/achievements/v1/dep/achievements/puid/unlock"),
            ("GET", "/achievements/v2/dep/definitions/achievements"),
            ("POST", "/stats/v1/dep/stats/puid"),
            ("GET", "/leaderboards/v1/dep/leaderboards/top"),
            ("PUT", "/playerdata/v1/dep/puid/data/save.sav"),
            ("GET", "/playerdata/v1/dep/puid/access/save.sav"),
            ("POST", "/telemetry/data/datarouter/api/v1/public/data"),
            ("POST", "/epic/ecom/v1/identities/abc/entitlements/redeem"),
            ("GET", "/epic/ecom/v1/identities/abc/namespaces/ns/offers"),
            ("POST", "/epic/ecom/v1/purchaseToken"),
            ("POST", "/auth/v1/users"),
            ("POST", "/auth/v1/accounts/deviceid"),
            ("POST", "/epic/oauth/v1/deviceAuthorization"),
            ("GET", "/epic/id/v1/accounts"),
            ("PUT", "/presence/v1/ns/acc/presence/conn"),
            ("GET", "/epic/oauth/v1/token"),
            ("DELETE", "/epic/ecom/v1/identities/abc/entitlements"),
            ("POST", "/epic/oauth/v1/token/../../../playerdata/v1/x"),
            ("POST", "/epic/oauth/v1//token"),
            ("POST", "/epic/oauth/%76%31/token"),
            ("POST", "/epic/oauth/v1/./token"),
            ("POST", "/epic/oauth/v1/token/"),
            ("POST", "/epic/oauth/v100/token"),
            ("POST", "/Epic/oauth/v1/token"),
            ("GET", "/sdk/v1/product/"),
            ("GET", "/sdk/v1/product/a/b"),
            ("GET", "*"),
            ("GET", ""),
        ] {
            assert!(rule(m, p).is_none(), "{m} {p} should be blocked");
        }
        // Every entry reachable from its own pattern, and none that reports or writes.
        for a in ALLOWLIST {
            let p = a.pattern.replace("{v}", "v2").replace("{}", "id7");
            assert_eq!(rule(a.method, &p), Some(a), "{}", a.pattern);
            for bad in ["playerdata", "achievement", "stats", "leaderboard", "telemetry", "redeem", "presence", "/auth/v1/users", "/auth/v1/accounts"] {
                assert!(!a.pattern.contains(bad), "{}", a.pattern);
            }
        }
    }

    #[test]
    fn query_values_never_shown() {
        assert_eq!(shown_target("/x/y?SessionID=s3cr3t&AppID=EOSSDK&flag"), "/x/y?SessionID=\u{2026}&AppID=\u{2026}&flag=\u{2026}");
        assert_eq!(shown_target("/sdk/v1/default"), "/sdk/v1/default");
    }

    #[test]
    fn plans() {
        let t = tempfile::tempdir().unwrap();
        assert!(plan(t.path(), false).is_none());
        assert!(!plan(t.path(), true).unwrap().required);
        std::fs::create_dir_all(t.path().join("Engine/Binaries/ThirdParty/EOSSDK/Win64")).unwrap();
        std::fs::write(t.path().join("Engine/Binaries/ThirdParty/EOSSDK/Win64/EOSSDK-Win64-Shipping.dll"), b"").unwrap();
        let p = plan(t.path(), false).unwrap();
        assert!(p.required && p.sdks == [PathBuf::from("Engine/Binaries/ThirdParty/EOSSDK/Win64/EOSSDK-Win64-Shipping.dll")]);
        // The old secur32 filter goes; a game's own secur32.dll stays.
        std::fs::write(t.path().join("secur32.dll"), b"MZ..QUIVER-EOS-FILTER/1 x86_64").unwrap();
        std::fs::write(t.path().join("Engine/Binaries/SECUR32.DLL"), b"MZ the game's").unwrap();
        assert_eq!(remove_old_filter(t.path()), [t.path().join("secur32.dll")]);
        assert!(t.path().join("Engine/Binaries/SECUR32.DLL").exists());
    }

    #[test]
    fn summaries_and_verdicts() {
        let log = "\
2026-11-18T10:00:00.000Z --- EOS proxy for run r3
2026-11-18T10:00:00.100Z probe ok (nonce 1)
2026-11-18T10:00:01.000Z tls ok api.epicgames.dev conn#1 (http/1.1)
2026-11-18T10:00:01.000Z allow GET /sdk/v1/product/p?platformId=\u{2026} -- SDK config (product) -> 200
2026-11-18T10:00:02.000Z allow POST /epic/oauth/v1/token -- Epic account sign-in (token) -> 200
2026-11-18T10:00:03.000Z block PUT /playerdata/v1/d/p/data/s -- not on the allowlist (403)
2026-11-18T10:00:04.000Z block PUT /playerdata/v1/d/p/data/s -- not on the allowlist (403)
2026-11-18T10:00:05.000Z refused api-gamedev.ol.epicgames.net conn#2 -- a sinkholed Epic host
";
        let s = summarize(log);
        assert_eq!((s.probes, s.tls_ok, s.allowed, s.blocked, s.refused), (1, 1, 2, 2, 1));
        assert_eq!(s.blocked_requests["PUT /playerdata/v1/d/p/data/s"], 2);
        assert!(s.verdict().contains("2 allowed, 2 blocked"));
        let bad = summarize("t tls failed api.epicgames.dev conn#1 -- the game rejected the lab's certificate (alert UnknownCA)\n");
        assert_eq!(bad.untrusted, 1);
        assert!(bad.verdict().contains("fail closed"));
        assert!(summarize("").verdict().contains("never connected"));
        let by = summarize("t !!! BYPASS 104.18.124.108:443 ESTABLISHED\n");
        assert!(by.verdict().contains("BYPASSED"));
        let out = summarize(&format!("{log}t !!! OUTSIDE 1.2.3.4:443 (ESTABLISHED): …\n"));
        assert_eq!(out.outside, 1);
        assert!(out.verdict().contains("went around it"));
    }

    #[test]
    fn proc_net_tcp() {
        // 127.0.0.1:443 (through the proxy), 104.18.124.108:443 established, a listener, :80.
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:A1B2 0100007F:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 1
   1: 0A00020F:C001 6C7C1268:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 2
   2: 00000000:01BB 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 3
   3: 0A00020F:C002 6C7C1268:0050 01 00000000:00000000 00:00000000 00000000  1000        0 4
";
        // ::ffff:104.18.125.108 syn-sent over IPv6.
        let tcp6 = "  sl  local_address                         remote_address                        st
   0: 00000000000000000000000000000000:C003 0000000000000000FFFF00006C7D1268:01BB 02 0
";
        let got = outside_443(tcp, tcp6);
        assert_eq!(
            got,
            [
                TcpConn { remote: "104.18.124.108".parse().unwrap(), port: 443, state: "ESTABLISHED" },
                TcpConn { remote: "104.18.125.108".parse().unwrap(), port: 443, state: "SYN_SENT" },
            ]
        );
    }

    #[test]
    fn hosts() {
        assert_eq!(redirected_hosts()[0], "api.epicgames.dev");
        assert!(redirected_hosts().contains(&"api-gamedev.ol.epicgames.net"));
    }
}
