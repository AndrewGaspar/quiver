//! Moonlight compatibility: quiverd's GameStream server (`quiver-gamestream`; design/REMOTE.md,
//! phase 1b), so Moonlight clients (TVs, phones, other PCs) can stream from this machine.
//!
//! Off by default (`remote.moonlight.enabled`). Moonlight pairs with a PIN it shows, which
//! the server's owner enters here (`Request::MoonlightPin`, from the app on any paired machine
//! or `quiver remote moonlight pin`). Paired Moonlight clients are kept apart from Quiver's
//! own peers and may only stream.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The codec names `codecs` takes, in Moonlight's order of preference.
pub const CODECS: [&str; 5] = ["h264", "hevc", "hevc10", "av1", "av1-10"];

/// Moonlight's default HTTP port; the others follow from it.
pub const DEFAULT_PORT: u16 = 47989;

/// `remote.moonlight` in config.json.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoonlightSettings {
    /// The server is on. Off by default.
    #[serde(default)]
    pub enabled: bool,
    /// What it listens on: addresses (`100.101.102.103`, `fd7a:115c:a1e0::1`) or interface
    /// names (`tailscale0`). Empty: every address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bind: Vec<String>,
    /// The codecs offered ([`CODECS`]). Empty: all of them (NVENC does all five).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codecs: Vec<String>,
    /// The HTTP port (default [`DEFAULT_PORT`]); the others move with it ([`ports`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Advertise the server on the LAN over mDNS (`_nvstream._tcp`, as GeForce Experience and
    /// Sunshine do), so Moonlight lists it without an address typed. `None`: on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mdns: Option<bool>,
}

impl MoonlightSettings {
    pub fn is_default(&self) -> bool {
        *self == MoonlightSettings::default()
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_PORT)
    }

    pub fn mdns(&self) -> bool {
        self.mdns.unwrap_or(true)
    }
}

/// One port the server uses, for firewalls and Tailscale ACLs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoonlightPort {
    /// `tcp` or `udp`.
    pub protocol: String,
    pub port: u16,
    pub what: String,
}

/// Every port, for HTTP port `http` (Moonlight's layout: HTTPS 5 below, video, control and
/// audio 9, 10 and 11 above, RTSP 21 above).
pub fn ports(http: u16) -> Vec<MoonlightPort> {
    let p = |protocol: &str, port: u16, what: &str| MoonlightPort { protocol: protocol.into(), port, what: what.into() };
    vec![
        p("tcp", http - 5, "HTTPS: app list, launch"),
        p("tcp", http, "HTTP: server info, pairing"),
        p("tcp", http + 21, "RTSP"),
        p("udp", http + 9, "video"),
        p("udp", http + 10, "control and input"),
        p("udp", http + 11, "audio"),
    ]
}

/// A Moonlight client waiting for its PIN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoonlightPairing {
    pub device_name: String,
    pub address: String,
}

/// A paired Moonlight client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoonlightClient {
    /// What Moonlight called itself while pairing (often "roth", Moonlight's placeholder).
    pub name: String,
    /// Where it paired from.
    pub address: String,
    /// SHA-256 of its certificate, hex: what `MoonlightUnpair` takes.
    pub fingerprint: String,
    pub paired_at: DateTime<Utc>,
}

/// What Moonlight launched, while it is launched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoonlightStream {
    pub app: String,
    pub client: String,
    /// Streaming now (else launched, and resumable).
    pub streaming: bool,
}

/// `Request::MoonlightStatus`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoonlightStatus {
    pub enabled: bool,
    /// What it is set to listen on (empty: every address).
    #[serde(default)]
    pub bind: Vec<String>,
    /// Where it listens now (empty while off or failed: `error`).
    #[serde(default)]
    pub listening: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub ports: Vec<MoonlightPort>,
    #[serde(default)]
    pub codecs: Vec<String>,
    /// Advertising on the LAN is on (`remote.moonlight.mdns`).
    #[serde(default)]
    pub mdns: bool,
    /// The interfaces it is advertised on now (never `tailscale0`: mDNS doesn't cross a
    /// tailnet, where Moonlight takes the address or MagicDNS name).
    #[serde(default)]
    pub advertised_on: Vec<String>,
    /// The name Moonlight shows for this machine.
    #[serde(default)]
    pub name: String,
    /// A client waiting for its PIN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<MoonlightPairing>,
    #[serde(default)]
    pub clients: Vec<MoonlightClient>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<MoonlightStream>,
}

/// Whether an address is a tailnet IPv6 address (`fd7a:115c:a1e0::/48`): Moonlight treats those
/// as local and asks for 1392-byte packets, too big for Tailscale's 1280 MTU, so it should be
/// set to a 1024-byte packet size there.
pub fn is_tailnet_v6(addr: &str) -> bool {
    addr.trim_matches(['[', ']']).parse::<std::net::Ipv6Addr>().is_ok_and(|a| {
        let s = a.segments();
        s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_layout() {
        let p = ports(DEFAULT_PORT);
        let tcp: Vec<u16> = p.iter().filter(|p| p.protocol == "tcp").map(|p| p.port).collect();
        let udp: Vec<u16> = p.iter().filter(|p| p.protocol == "udp").map(|p| p.port).collect();
        assert_eq!(tcp, [47984, 47989, 48010]);
        assert_eq!(udp, [47998, 47999, 48000]);
    }

    #[test]
    fn settings_round_trip() {
        let s: MoonlightSettings = serde_json::from_str(r#"{"enabled":true,"bind":["tailscale0"]}"#).unwrap();
        assert_eq!((s.enabled, s.bind.as_slice(), s.port()), (true, &["tailscale0".to_owned()][..], 47989));
        assert!(MoonlightSettings::default().is_default());
        assert_eq!(serde_json::to_string(&MoonlightSettings::default()).unwrap(), r#"{"enabled":false}"#);
    }

    #[test]
    fn not_local_only() {
        // A paired Quiver client enters the PIN for a headless server.
        for r in [crate::Request::MoonlightPin { pin: "1234".into() }, crate::Request::MoonlightEnable { bind: None, codecs: None, mdns: None }, crate::Request::MoonlightStatus] {
            assert_eq!(crate::remote::local_only(&r), None);
        }
    }

    #[test]
    fn tailnet_v6() {
        assert!(is_tailnet_v6("fd7a:115c:a1e0::1"));
        assert!(is_tailnet_v6("[fd7a:115c:a1e0:ab12::5]"));
        assert!(!is_tailnet_v6("100.100.1.1"));
        assert!(!is_tailnet_v6("fd00::1"));
    }
}
