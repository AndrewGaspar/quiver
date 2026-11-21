//! LAN discovery over mDNS (design/REMOTE.md §3): `_quiver._tcp`, advertising the machine's
//! name and id, never its library.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};

/// The service type.
pub const SERVICE: &str = "_quiver._tcp.local.";

/// A machine seen on the LAN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovered {
    pub name: String,
    pub id: String,
    /// `host:port` addresses to try, IPv4 first.
    pub addrs: Vec<String>,
}

/// The process's one mDNS responder, shared by every advert (`_quiver._tcp`, Moonlight's
/// `_nvstream._tcp`), made on first use.
fn responder() -> Result<ServiceDaemon> {
    static SHARED: OnceLock<Mutex<Option<ServiceDaemon>>> = OnceLock::new();
    let mut slot = SHARED.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = slot.as_ref() {
        return Ok(d.clone());
    }
    let d = ServiceDaemon::new().context("starting mDNS")?;
    *slot = Some(d.clone());
    Ok(d)
}

/// A service on the LAN while the advert lives; withdrawn when dropped.
pub struct Advert {
    mdns: ServiceDaemon,
    fullname: String,
}

impl Advert {
    /// Advertise `name` and `id` with the listener's `port`.
    pub fn start(name: &str, id: &str, port: u16) -> Result<Advert> {
        let host = format!("quiver-{id}.local.");
        let props: HashMap<String, String> =
            [("id".to_owned(), id.to_owned()), ("name".to_owned(), name.to_owned()), ("v".to_owned(), crate::wire::VERSION.to_string())].into();
        Advert::service(SERVICE, &instance_name(name, id), &host, port, props, &[])
    }

    /// Advertise any service type (`_nvstream._tcp.local.`) as `instance` on `host`
    /// (`name.local.`), with this machine's addresses on `interfaces` (names; empty: every
    /// interface).
    pub fn service(ty: &str, instance: &str, host: &str, port: u16, props: HashMap<String, String>, interfaces: &[String]) -> Result<Advert> {
        let mdns = responder()?;
        let mut info = ServiceInfo::new(ty, instance, host, (), port, props)?.enable_addr_auto();
        if !interfaces.is_empty() {
            info.set_interfaces(interfaces.iter().map(|n| mdns_sd::IfKind::Name(n.clone())).collect());
        }
        let fullname = info.get_fullname().to_owned();
        mdns.register(info).context("advertising on mDNS")?;
        Ok(Advert { mdns, fullname })
    }
}

impl Drop for Advert {
    fn drop(&mut self) {
        let _ = self.mdns.unregister(&self.fullname);
    }
}

/// One instance of a service type seen on the LAN ([`browse_service`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The instance's full name (`Tower._nvstream._tcp.local.`).
    pub fullname: String,
    pub port: u16,
    pub addrs: Vec<std::net::IpAddr>,
}

/// Instances of service type `ty` on the LAN, listening for `wait`. Blocks.
pub fn browse_service(ty: &str, wait: Duration) -> Result<Vec<Found>> {
    let mdns = ServiceDaemon::new().context("starting mDNS")?;
    let rx = mdns.browse(ty)?;
    let end = Instant::now() + wait;
    let mut found: Vec<Found> = Vec::new();
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        let Ok(event) = rx.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(s) = event {
            let mut addrs: Vec<std::net::IpAddr> = s.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
            addrs.sort();
            let f = Found { fullname: s.get_fullname().to_owned(), port: s.port, addrs };
            found.retain(|o| o.fullname != f.fullname);
            found.push(f);
        }
    }
    let _ = mdns.stop_browse(ty);
    let _ = mdns.shutdown();
    Ok(found)
}

/// The instance name: the machine's name, then its id (names can repeat on a LAN).
fn instance_name(name: &str, id: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).take(40).collect();
    format!("{clean} ({})", &id[..id.len().min(6)])
}

/// Machines on the LAN, listening for `wait`. Blocks (run it on a blocking thread).
pub fn browse(wait: Duration) -> Result<Vec<Discovered>> {
    let mdns = ServiceDaemon::new().context("starting mDNS")?;
    let rx = mdns.browse(SERVICE)?;
    let end = Instant::now() + wait;
    let mut found: Vec<Discovered> = Vec::new();
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        let Ok(event) = rx.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(s) = event {
            let (Some(id), Some(name)) = (s.get_property_val_str("id"), s.get_property_val_str("name")) else { continue };
            let mut ips: Vec<std::net::IpAddr> = s.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
            ips.sort_by_key(|ip| (!ip.is_ipv4(), *ip));
            let addrs = ips.iter().map(|ip| std::net::SocketAddr::new(*ip, s.port).to_string()).collect();
            let d = Discovered { name: name.to_owned(), id: id.to_owned(), addrs };
            match found.iter_mut().find(|f| f.id == d.id) {
                Some(f) => *f = d,
                None => found.push(d),
            }
        }
    }
    let _ = mdns.stop_browse(SERVICE);
    let _ = mdns.shutdown();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

#[cfg(test)]
mod tests {
    #[test]
    fn instance_names_carry_the_id() {
        assert_eq!(super::instance_name("Tower", "0123456789abcdef"), "Tower (012345)");
    }
}
