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
}
impl Drop for Connection {
    fn drop(&mut self) {
        let mut inner = self.limits.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ip) = inner.ips.get_mut(&self.ip) {
            ip.connections -= 1;
        }
        inner.connections -= 1;
    }
}
impl PreAuth {
    pub fn connection(&self, ip: IpAddr) -> Option<Connection> {
        self.connection_at(ip, Instant::now())
    }
    fn connection_at(&self, ip: IpAddr, now: Instant) -> Option<Connection> {
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
            attempts: Bucket::new(now),
            hellos: Bucket::new(now),
        });
        if entry.connections >= PER_IP || !entry.attempts.take(now) {
            return None;
        }
        entry.connections += 1;
        inner.connections += 1;
        Some(Connection { limits: self.clone(), ip })
    }
    pub fn hello(&self, ip: IpAddr) -> bool {
        self.hello_at(ip, Instant::now())
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
            connections.push(limits.connection_at(ip, now).unwrap());
        }
        assert!(limits.connection_at(ip, now).is_none());
        connections.pop();
        assert!(limits.connection_at(ip, now).is_some());
        drop(connections);
        for _ in PER_IP + 1..BURST {
            drop(limits.connection_at(ip, now).unwrap());
        }
        assert!(limits.connection_at(ip, now).is_none());
        assert!(limits.connection_at(ip, now + Duration::from_secs(1)).is_some());
    }
    #[test]
    fn hello_rate_and_ip_table_are_bounded() {
        let limits = PreAuth::default();
        let ip = "127.0.0.1".parse().unwrap();
        let now = Instant::now();
        let _connection = limits.connection_at(ip, now).unwrap();
        for _ in 0..BURST {
            assert!(limits.hello_at(ip, now));
        }
        assert!(!limits.hello_at(ip, now));
        assert!(limits.hello_at(ip, now + Duration::from_secs(10)));
        for i in 1..MAX_IPS {
            drop(limits.connection_at(IpAddr::from(std::net::Ipv4Addr::from(i as u32)), now).unwrap());
        }
        assert!(limits.connection_at("255.255.255.255".parse().unwrap(), now).is_none());
        assert!(limits.connection_at("255.255.255.255".parse().unwrap(), now + TTL).is_some());
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
