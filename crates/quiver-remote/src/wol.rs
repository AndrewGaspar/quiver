//! Wake-on-LAN (design/REMOTE.md §5): the server's MAC, learned at pairing, and the magic
//! packet that wakes it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result, bail};

/// `aa:bb:cc:dd:ee:ff` (or with `-`) as bytes. All-zero and broadcast MACs are refused.
pub fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = s.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        bail!("a MAC address has six parts");
    }
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(p, 16).with_context(|| format!("'{s}' isn't a MAC address"))?;
    }
    if mac == [0; 6] || mac == [0xff; 6] {
        bail!("'{s}' isn't a card's MAC address");
    }
    Ok(mac)
}

/// The magic packet: six 0xFF bytes, then the MAC sixteen times.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut p = [0xffu8; 102];
    for i in 0..16 {
        p[6 + i * 6..12 + i * 6].copy_from_slice(&mac);
    }
    p
}

/// Send the magic packet for `mac` to the LAN's broadcast address on port 9, and, when the
/// machine's last address is an IPv4 one, to its /24's broadcast and to it directly (some
/// routers forward those).
pub async fn wake(mac: &str, last_addr: Option<&str>) -> Result<()> {
    let packet = magic_packet(parse_mac(mac)?);
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    sock.set_broadcast(true)?;
    let mut targets = vec![SocketAddr::from((Ipv4Addr::BROADCAST, 9))];
    if let Some(IpAddr::V4(ip)) = last_addr.and_then(host_ip) {
        let o = ip.octets();
        targets.push(SocketAddr::from((Ipv4Addr::new(o[0], o[1], o[2], 255), 9)));
        targets.push(SocketAddr::from((ip, 9)));
    }
    let mut sent = 0;
    for t in &targets {
        match sock.send_to(&packet, t).await {
            Ok(_) => sent += 1,
            Err(e) => tracing::debug!("wake-on-LAN to {t}: {e}"),
        }
    }
    if sent == 0 {
        bail!("couldn't send the wake-up packet");
    }
    Ok(())
}

/// The IP in `host:port` when the host is a literal address.
fn host_ip(addr: &str) -> Option<IpAddr> {
    addr.parse::<SocketAddr>().map(|a| a.ip()).ok().or_else(|| addr.parse().ok())
}

/// This machine's MAC for the interface that has `local` (the address a connection
/// arrived on), else the first physical card's. `None` when there is no card with one
/// (a VPN-only address has none).
pub fn local_mac(local: Option<IpAddr>) -> Option<String> {
    local.and_then(interface_of).and_then(|name| card_mac(&name)).or_else(first_card_mac)
}

/// The interface name that has address `ip`.
fn interface_of(ip: IpAddr) -> Option<String> {
    let mut out = None;
    // SAFETY: getifaddrs/freeifaddrs as documented; every pointer is checked before use.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() {
                let family = (*ifa.ifa_addr).sa_family as libc::c_int;
                let addr = match family {
                    libc::AF_INET => {
                        let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr))))
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                        Some(IpAddr::V6(sin6.sin6_addr.s6_addr.into()))
                    }
                    _ => None,
                };
                if addr == Some(ip) {
                    out = Some(std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned());
                    break;
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

/// A physical card's MAC (`/sys/class/net/<name>`, with a `device`).
fn card_mac(name: &str) -> Option<String> {
    let dir = std::path::Path::new("/sys/class/net").join(name);
    if !dir.join("device").exists() {
        return None;
    }
    let mac = std::fs::read_to_string(dir.join("address")).ok()?.trim().to_owned();
    parse_mac(&mac).ok().map(|_| mac)
}

fn first_card_mac() -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net").ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    // Wired cards first (`en…`/`eth…`): Wake-on-LAN over Wi-Fi rarely works.
    names.sort_by_key(|n| (!(n.starts_with("en") || n.starts_with("eth")), n.clone()));
    names.iter().find_map(|n| card_mac(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macs_and_packets() {
        let mac = parse_mac("AA-bb-cc-01-02-03").unwrap();
        assert_eq!(mac, [0xaa, 0xbb, 0xcc, 1, 2, 3]);
        assert!(parse_mac("00:00:00:00:00:00").is_err() && parse_mac("aa:bb").is_err() && parse_mac("zz:bb:cc:dd:ee:ff").is_err());
        let p = magic_packet(mac);
        assert_eq!(&p[..6], &[0xff; 6]);
        assert_eq!(&p[96..], &mac);
        assert_eq!(host_ip("10.1.2.3:47990"), Some("10.1.2.3".parse().unwrap()));
        assert_eq!(host_ip("tower:47990"), None);
    }

    #[test]
    fn loopback_has_no_card() {
        assert_eq!(interface_of("127.0.0.1".parse().unwrap()).as_deref(), Some("lo"));
        assert!(card_mac("lo").is_none());
    }
}
