//! Competing flows for scenarios: a bulk TCP transfer or a paced UDP flood between the
//! lab's namespaces, made here rather than with iperf3. Their sockets live in the lab's
//! namespaces, so the traffic only ever crosses the lab's veths and router, never a real NIC.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;

use crate::ns::{Dir, Lab, Ns, SERVER_V4, Side};
use crate::probe::{big_buffers, udp_flood};
use crate::scenario::FloodProto;

/// A running competing flow. [`Flood::stop`] ends it and returns the bytes delivered.
pub struct Flood {
    stop: Arc<AtomicBool>,
    delivered: Arc<AtomicU64>,
    threads: Vec<JoinHandle<()>>,
}

impl Flood {
    /// Starts a flow in `dir`. `rate_mbit` paces UDP; TCP goes as fast as its congestion
    /// control lets it.
    pub fn start(lab: &Lab, dir: Dir, proto: FloodProto, rate_mbit: Option<f64>, port: u16) -> Result<Flood> {
        let (src, dst, dst_ip) = match dir {
            Dir::ToClient => (Ns::Server, Ns::Client, lab.addr(Side::Client, false)),
            Dir::ToServer => (Ns::Client, Ns::Server, SERVER_V4.into()),
        };
        let any: std::net::IpAddr = "0.0.0.0".parse().unwrap();
        let to = SocketAddr::new(dst_ip, port);
        let stop = Arc::new(AtomicBool::new(false));
        let delivered = Arc::new(AtomicU64::new(0));
        let mut threads = Vec::new();
        match proto {
            FloodProto::Tcp => {
                let listener = lab.tcp_listen(dst, SocketAddr::new(any, port))?;
                let mut tx = lab.tcp_connect(src, to, Duration::from_secs(5))?;
                let (mut rx, _) = listener.accept()?;
                rx.set_read_timeout(Some(Duration::from_millis(100)))?;
                tx.set_write_timeout(Some(Duration::from_millis(100)))?;
                let (s, d) = (stop.clone(), delivered.clone());
                threads.push(std::thread::spawn(move || {
                    let mut buf = vec![0u8; 1 << 16];
                    while !s.load(Ordering::Relaxed) {
                        match rx.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                d.fetch_add(n as u64, Ordering::Relaxed);
                            }
                            Err(_) => continue,
                        }
                    }
                }));
                let s = stop.clone();
                threads.push(std::thread::spawn(move || {
                    let buf = vec![0xA5u8; 1 << 16];
                    while !s.load(Ordering::Relaxed) {
                        let _ = tx.write(&buf);
                    }
                }));
            }
            FloodProto::Udp => {
                let rx = lab.udp(dst, SocketAddr::new(any, port))?;
                big_buffers(&rx);
                rx.set_read_timeout(Some(Duration::from_millis(100)))?;
                let tx = lab.udp(src, SocketAddr::new(any, 0))?;
                big_buffers(&tx);
                let (s, d) = (stop.clone(), delivered.clone());
                threads.push(std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    while !s.load(Ordering::Relaxed) {
                        if let Ok(n) = rx.recv(&mut buf) {
                            d.fetch_add(n as u64, Ordering::Relaxed);
                        }
                    }
                }));
                let s = stop.clone();
                let mbit = rate_mbit.unwrap_or(10.0);
                threads.push(std::thread::spawn(move || {
                    udp_flood(&tx, to, mbit, f64::INFINITY, &s);
                }));
            }
        }
        Ok(Flood { stop, delivered, threads })
    }

    /// Payload bytes delivered so far.
    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }

    pub fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
        self.delivered.load(Ordering::Relaxed)
    }
}
