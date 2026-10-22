//! Limits shared by all connections, before identity and password verification.
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const MAX_IPS: usize = 8192;
const MAX_CONNECTIONS: usize = 1024;
const PER_IP: usize = 16;
/// Concurrent TeamSpeak guest connections (`/ws/watch`) per subscriber, within `PER_IP`.
const GUESTS_PER_IP: usize = 3;
const BURST: usize = 20;
const TTL: Duration = Duration::from_secs(300);

struct Bucket {
    tokens: f64,
    at: Instant,
}
impl Bucket {
    fn new(now: Instant) -> Self {
        Self { tokens: BURST as f64, at: now }
    }
    fn take(&mut self, now: Instant) -> bool {
        self.tokens = (self.tokens + now.duration_since(self.at).as_secs_f64()).min(BURST as f64);
        self.at = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}
struct Ip {
    connections: usize,
    /// Of those, guest connections.
    guests: usize,
    attempts: Bucket,
    hellos: Bucket,
}
#[derive(Default)]
struct Inner {
    ips: HashMap<IpAddr, Ip>,
    connections: usize,
}

#[derive(Clone)]
pub struct PreAuth {
    inner: Arc<Mutex<Inner>>,
    pub passwords: Arc<Semaphore>,
}
impl Default for PreAuth {
    fn default() -> Self {
        Self { inner: Arc::default(), passwords: Arc::new(Semaphore::new(4)) }
    }
}

pub struct Connection {
    limits: PreAuth,
    ip: IpAddr,
    guest: bool,
}
impl Drop for Connection {
    fn drop(&mut self) {
        let mut inner = self.limits.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ip) = inner.ips.get_mut(&self.ip) {
            ip.connections -= 1;
            if self.guest {
                ip.guests -= 1;
            }
        }
        inner.connections -= 1;
    }
}
/// The unit address limits and bans apply to: an IPv4 address, or an IPv6 /64.
/// One IPv6 host commonly owns a whole /64, so per-address limits on single IPv6
/// addresses would not hold anyone back.
pub fn subscriber(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6((u128::from(v6) & !((1u128 << 64) - 1)).into()),
        },
    }
}

impl PreAuth {
    pub fn connection(&self, ip: IpAddr) -> Option<Connection> {
        self.connection_at(subscriber(ip), Instant::now(), false)
    }
    /// A TeamSpeak guest's connection: one of the subscriber's connections, under the same
    /// limits, and also one of at most [`GUESTS_PER_IP`] guests.
    pub fn guest(&self, ip: IpAddr) -> Option<Connection> {
        self.connection_at(subscriber(ip), Instant::now(), true)
    }
    fn connection_at(&self, ip: IpAddr, now: Instant, guest: bool) -> Option<Connection> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.connections >= MAX_CONNECTIONS {
            return None;
        }
        if !inner.ips.contains_key(&ip) {
            inner.ips.retain(|_, i| i.connections != 0 || now.duration_since(i.attempts.at.max(i.hellos.at)) < TTL);
            if inner.ips.len() >= MAX_IPS {
                return None;
            }
        }
        let entry = inner.ips.entry(ip).or_insert_with(|| Ip {
            connections: 0,
            guests: 0,
            attempts: Bucket::new(now),
            hellos: Bucket::new(now),
        });
        if entry.connections >= PER_IP || (guest && entry.guests >= GUESTS_PER_IP) || !entry.attempts.take(now) {
            return None;
        }
        entry.connections += 1;
        entry.guests += usize::from(guest);
        inner.connections += 1;
        Some(Connection { limits: self.clone(), ip, guest })
    }
    pub fn hello(&self, ip: IpAddr) -> bool {
        self.hello_at(subscriber(ip), Instant::now())
    }
    fn hello_at(&self, ip: IpAddr, now: Instant) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).ips.get_mut(&ip).is_some_and(|i| i.hellos.take(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connections_are_limited_per_ip_before_upgrade_and_release_on_drop() {
        let limits = PreAuth::default();
        let ip = "127.0.0.1".parse().unwrap();
        let now = Instant::now();
        let mut connections = Vec::new();
        for _ in 0..PER_IP {
            connections.push(limits.connection_at(ip, now, false).unwrap());
        }
        assert!(limits.connection_at(ip, now, false).is_none());
        connections.pop();
        assert!(limits.connection_at(ip, now, false).is_some());
        drop(connections);
        for _ in PER_IP + 1..BURST {
            drop(limits.connection_at(ip, now, false).unwrap());
        }
        assert!(limits.connection_at(ip, now, false).is_none());
        assert!(limits.connection_at(ip, now + Duration::from_secs(1), false).is_some());
    }
    #[test]
    fn hello_rate_and_ip_table_are_bounded() {
        let limits = PreAuth::default();
        let ip = "127.0.0.1".parse().unwrap();
        let now = Instant::now();
        let _connection = limits.connection_at(ip, now, false).unwrap();
        for _ in 0..BURST {
            assert!(limits.hello_at(ip, now));
        }
        assert!(!limits.hello_at(ip, now));
        assert!(limits.hello_at(ip, now + Duration::from_secs(10)));
        for i in 1..MAX_IPS {
            drop(limits.connection_at(IpAddr::from(std::net::Ipv4Addr::from(i as u32)), now, false).unwrap());
        }
        assert!(limits.connection_at("255.255.255.255".parse().unwrap(), now, false).is_none());
        assert!(limits.connection_at("255.255.255.255".parse().unwrap(), now + TTL, false).is_some());
    }
    #[test]
    fn one_ipv6_host_is_one_subscriber() {
        let limits = PreAuth::default();
        let host = |i: u16| -> IpAddr { format!("2001:db8:1:2::{i:x}").parse().unwrap() };
        let mut held: Vec<_> = (0..PER_IP as u16).map(|i| limits.connection(host(i)).unwrap()).collect();
        // The whole /64 shares one budget; the next /64 has its own.
        assert!(limits.connection(host(999)).is_none());
        assert!(limits.connection("2001:db8:1:3::1".parse().unwrap()).is_some());
        held.clear();
        assert_eq!(subscriber(host(7)), "2001:db8:1:2::".parse::<IpAddr>().unwrap());
        assert_eq!(subscriber("::ffff:192.0.2.1".parse().unwrap()), "192.0.2.1".parse::<IpAddr>().unwrap());
        assert_eq!(subscriber("192.0.2.1".parse().unwrap()), "192.0.2.1".parse::<IpAddr>().unwrap());
    }
    #[test]
    fn guests_are_few_per_subscriber_and_count_among_its_connections() {
        let limits = PreAuth::default();
        let host = |i: u16| -> IpAddr { format!("2001:db8:5:6::{i:x}").parse().unwrap() };
        // Three guests from anywhere in one /64; the fourth waits for one to leave.
        let mut guests: Vec<_> = (0..GUESTS_PER_IP as u16).map(|i| limits.guest(host(i)).unwrap()).collect();
        assert!(limits.guest(host(99)).is_none(), "a fourth guest from the same /64");
        assert!(limits.guest("2001:db8:5:7::1".parse().unwrap()).is_some(), "the next /64 has its own");
        // Members still connect, and guests fill the same per-subscriber connection limit.
        let members: Vec<_> = (0..(PER_IP - GUESTS_PER_IP) as u16).map(|i| limits.connection(host(100 + i))).collect();
        assert!(members.iter().all(Option::is_some));
        assert!(limits.connection(host(500)).is_none(), "guests count toward the subscriber's connections");
        guests.pop();
        assert!(limits.guest(host(600)).is_some(), "a guest leaving makes room for one more");
        let v4: IpAddr = "192.0.2.9".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.0.2.9".parse().unwrap();
        let _held = (limits.guest(v4).unwrap(), limits.guest(mapped).unwrap(), limits.guest(v4).unwrap());
        assert!(limits.guest(mapped).is_none(), "an IPv4-mapped address is the same subscriber");
    }
    #[tokio::test]
    async fn password_work_has_no_unbounded_queue() {
        let limits = PreAuth::default();
        let permits: Vec<_> = (0..4).map(|_| limits.passwords.clone().try_acquire_owned().unwrap()).collect();
        assert!(limits.passwords.clone().try_acquire_owned().is_err());
        drop(permits);
        assert!(limits.passwords.try_acquire().is_ok());
    }
}
