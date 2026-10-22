//! The monthly egress counter (docs/screen-share.md, "Cost on the deployment VM"). Oracle's free
//! outbound transfer is counted per calendar month, so once a minute the server reads what the
//! host's default-route interface sent (`/proc/net/dev`, Linux), adds the delta to the month's
//! total in the store and warns the admins at 80 % and 100 % of `monthly_egress_tb`. The total
//! includes every service on the host, so it errs high; the screen SFU's own bytes are kept
//! beside it as a breakdown.
use super::*;
use crate::media::screen::ScreenShared;
use gwar_proto::{EgressScope, ScreenLimits, ServerEgress};
use std::sync::atomic::Ordering;

/// How often the counter is read.
pub(super) const EGRESS_EVERY: Duration = Duration::from_secs(60);
const TB: f64 = 1e12;

/// One read of the interface's transmitted-bytes counter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reading {
    pub interface: String,
    pub transmitted: u64,
    pub scope: EgressScope,
}

/// The default-route interface in `/proc/net/route`: destination 0 with the lowest metric.
fn default_interface(route: &str) -> Option<String> {
    route
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let metric: u64 = fields.get(6)?.parse().ok()?;
            let flags = u32::from_str_radix(fields.get(3)?, 16).ok()?;
            (fields.get(1) == Some(&"00000000")
                && fields.get(7) == Some(&"00000000")
                && flags & 1 != 0
                && flags & 0x200 == 0)
                .then(|| (metric, fields[0].to_owned()))
        })
        .min()
        .map(|(_, interface)| interface)
}

/// IPv6 default routes: zero destination/prefix, UP, lowest hexadecimal metric.
fn default_ipv6_interface(route: &str) -> Option<String> {
    route
        .lines()
        .filter_map(|line| {
            let f: Vec<_> = line.split_whitespace().collect();
            let metric = u64::from_str_radix(f.get(5)?, 16).ok()?;
            let flags = u32::from_str_radix(f.get(8)?, 16).ok()?;
            (f.first() == Some(&"00000000000000000000000000000000")
                && f.get(1) == Some(&"00")
                && f.get(3) == Some(&"00")
                && flags & 1 != 0
                && flags & 0x200 == 0)
                .then_some((metric, f.get(9)?.to_string()))
        })
        .min()
        .map(|(_, interface)| interface)
}

/// The transmitted-bytes counter of `interface` in `/proc/net/dev`.
fn transmitted(dev: &str, interface: &str) -> Option<u64> {
    dev.lines().find_map(|line| {
        let (name, counters) = line.split_once(':')?;
        if name.trim() != interface {
            return None;
        }
        // Receive: bytes packets errs drop fifo frame compressed multicast; then transmit bytes.
        counters.split_whitespace().nth(8)?.parse().ok()
    })
}

/// Inside a container, `/proc/net/dev` shows the container's own interface.
fn in_container() -> bool {
    std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
        || std::env::var_os("container").is_some()
        || std::fs::read_to_string("/proc/1/cgroup")
            .is_ok_and(|cgroup| ["docker", "kubepods", "lxc", "containerd"].iter().any(|c| cgroup.contains(c)))
}

/// Reads the host's counter; `None` off Linux or when the files are unreadable.
pub fn read() -> Option<Reading> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let interface =
        std::fs::read_to_string("/proc/net/route").ok().and_then(|route| default_interface(&route)).or_else(|| {
            std::fs::read_to_string("/proc/net/ipv6_route").ok().and_then(|route| default_ipv6_interface(&route))
        })?;
    let transmitted = transmitted(&std::fs::read_to_string("/proc/net/dev").ok()?, &interface)?;
    let scope = if in_container() { EgressScope::Container } else { EgressScope::Host };
    Some(Reading { interface, transmitted, scope })
}

/// `YYYY-MM` of a Unix time in milliseconds (UTC).
fn month(ms: i64) -> String {
    crate::members::civil_date(ms)[..7].to_owned()
}

/// The month's totals, mirrored from the store.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub(super) struct Egress {
    month: String,
    scope: Option<EgressScope>,
    total: u64,
    screen_members: u64,
    screen_guests: u64,
    /// The highest warning given this month: 0, 80 or 100.
    warned: u32,
    /// The screen SFU's cumulative counters at the last tick.
    #[serde(skip)]
    seen: (u64, u64),
    checkpoint: Option<(Reading, i64)>,
    /// Some interval could not be attributed to this month, or was not observed.
    incomplete: bool,
}

impl Egress {
    /// The month's totals as stored (a restart keeps counting).
    pub(super) fn load(store: &Store) -> Result<Self> {
        store
            .meta("egress_checkpoint")?
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
            .map(|value| value.unwrap_or_default())
    }
}

/// The screen limits as stored, or the defaults.
pub(super) fn load_limits(store: &Store) -> Result<ScreenLimits> {
    Ok(store.meta("screen_limits")?.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default())
}

/// Checks limits an admin sets.
pub(super) fn check_limits(limits: &ScreenLimits) -> Result<(), ErrorBody> {
    let within = |value: u32, low: u32, high: u32, what: &str| {
        if (low..=high).contains(&value) {
            Ok(())
        } else {
            Err(err(ErrorCode::BadRequest, &format!("{what} must be {low}–{high}")))
        }
    };
    within(limits.shares_per_channel, 1, 16, "shares per channel")?;
    within(limits.shares_per_server, 1, 1024, "shares per server")?;
    within(limits.viewers_per_share, 1, 1000, "viewers per share")?;
    within(limits.share_cap_kbps, 100, 100_000, "the bitrate cap per share (kbps)")?;
    within(limits.budget_mbps, 1, 100_000, "the screen budget (Mbps)")?;
    within(limits.monthly_egress_tb, 1, 100_000, "the monthly egress warning (TB)")
}

impl Core {
    fn egress_limit(&self) -> u64 {
        (self.screen_limits.monthly_egress_tb as f64 * TB) as u64
    }

    /// The month's transfer reached `monthly_egress_tb` and new watches are to be refused.
    pub(super) fn egress_stopped(&self) -> bool {
        self.screen_limits.monthly_egress_stop && self.egress.total >= self.egress_limit()
    }

    /// One atomic checkpoint. An interface switch or an interval crossing UTC months
    /// rebaselines instead of assigning lifetime/downtime traffic to the current month.
    pub(super) fn egress_tick(&mut self, reading: Option<Reading>, shared: (u64, u64), now: i64) -> Result<()> {
        let current = month(now);
        let changed_month = self.egress.month != current;
        let mut next = if changed_month {
            Egress { month: current, seen: shared, incomplete: true, ..Egress::default() }
        } else {
            self.egress.clone()
        };
        next.screen_members += shared.0.saturating_sub(next.seen.0);
        next.screen_guests += shared.1.saturating_sub(next.seen.1);
        next.seen = shared;
        next.scope = Some(reading.as_ref().map_or(EgressScope::Unavailable, |r| r.scope));
        if let Some(reading) = reading {
            if let Some((last, at)) = &next.checkpoint {
                if month(*at) == next.month && last.interface == reading.interface {
                    next.total += if reading.transmitted >= last.transmitted {
                        reading.transmitted - last.transmitted
                    } else {
                        reading.transmitted
                    };
                } else {
                    next.incomplete = true;
                }
            }
            if !changed_month && next.checkpoint.is_none() {
                next.incomplete = true;
            }
            next.checkpoint = Some((reading, now));
        } else {
            next.incomplete = true;
        }
        let percent =
            [100, 80].into_iter().find(|&p| next.total as f64 >= self.egress_limit() as f64 * p as f64 / 100.0);
        let warning = percent.filter(|&p| p > next.warned);
        if let Some(percent) = warning {
            next.warned = percent;
        }
        let checkpoint = serde_json::to_string(&next)?;
        self.store.atomically(|store| store.set_meta("egress_checkpoint", &checkpoint))?;
        self.egress = next;
        if let Some(percent) = warning {
            self.broadcast_egress_warning(percent);
        }
        Ok(())
    }

    /// Warns admins once a month for each of 80 % and 100 % of the limit.
    fn egress_warn(&mut self) -> Result<()> {
        let limit = self.egress_limit();
        let total = self.egress.total;
        let percent = [100, 80].into_iter().find(|&p| total as f64 >= limit as f64 * p as f64 / 100.0);
        let Some(percent) = percent.filter(|&p| p > self.egress.warned) else { return Ok(()) };
        let mut next = self.egress.clone();
        next.warned = percent;
        let checkpoint = serde_json::to_string(&next)?;
        self.store.atomically(|store| store.set_meta("egress_checkpoint", &checkpoint))?;
        self.egress = next;
        self.broadcast_egress_warning(percent);
        Ok(())
    }

    fn broadcast_egress_warning(&mut self, percent: u32) {
        let (total, limit) = (self.egress.total, self.egress_limit());
        warn!(
            month = self.egress.month,
            total, limit, "this month's outbound transfer reached {percent} % of the limit"
        );
        let admins: Vec<SessionId> =
            self.sessions.keys().copied().filter(|&s| self.require(s, Permission::ManageServer).is_ok()).collect();
        let event = Event::ServerEgressWarning { month: self.egress.month.clone(), percent, total, limit };
        self.broadcast(event, |s| admins.contains(&s.id));
    }

    /// `server.egress`, for the server settings.
    pub(super) fn server_egress(&self, session: SessionId) -> Reply {
        self.require(session, Permission::ManageServer)?;
        let scope = self.egress.scope.unwrap_or(EgressScope::Unavailable);
        Ok(Response::ServerEgress(ServerEgress {
            month: if self.egress.month.is_empty() { month(now_ms()) } else { self.egress.month.clone() },
            scope,
            incomplete: self.egress.incomplete,
            total: (scope != EgressScope::Unavailable).then_some(self.egress.total),
            screen_members: self.egress.screen_members,
            screen_guests: self.egress.screen_guests,
            limits: self.screen_limits,
        }))
    }

    /// New screen limits from `server.update`; the screen task's budget follows at once.
    pub(super) fn set_screen_limits(&mut self, limits: ScreenLimits) -> Result<()> {
        self.store.set_meta("screen_limits", &serde_json::to_string(&limits)?)?;
        self.screen_limits = limits;
        self.screen_shared.budget_bps.store(limits.budget_mbps as u64 * 1_000_000, Ordering::Relaxed);
        self.egress_warn()
    }
}

/// The SFU's cumulative counters, for `egress_tick`.
pub(super) fn screen_bytes(shared: &ScreenShared) -> (u64, u64) {
    (shared.member_bytes.load(Ordering::Relaxed), shared.guest_bytes.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_route_and_its_counter_are_found() {
        let route = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
                     docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\n\
                     wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\n\
                     enp0s6\t00000000\t0100000A\t0003\t0\t0\t100\t00000000\n";
        assert_eq!(default_interface(route).as_deref(), Some("enp0s6"));
        assert_eq!(default_interface("Iface\tDestination\n"), None);
        let dev = "Inter-|   Receive  |  Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes ...\n\
                       lo: 100 1 0 0 0 0 0 0 100 1 0 0 0 0 0 0\n\
                   enp0s6: 5000 40 0 0 0 0 0 0 123456789 900 0 0 0 0 0 0\n";
        assert_eq!(transmitted(dev, "enp0s6"), Some(123_456_789));
        assert_eq!(transmitted(dev, "eth0"), None);
        assert_eq!(month(1_760_000_000_000), "2025-10");
    }

    fn core() -> Core {
        let signer = ed25519_dalek::SigningKey::from_bytes(&[99; 32]);
        let (_tx, _rx) = mpsc::channel(16);
        Core::new(
            Store::in_memory().unwrap(),
            CoreConfig {
                max_clients: 10,
                ice_servers: vec![],
                version: "test".into(),
                password: Arc::new(ArcSwap::from_pointee(None)),
                password_plaintext: None,
                upload_limit: 0,
                upload_quota: 0,
                storage_limit: 0,
                files_dir: PathBuf::new(),
                connect_url: None,
                server_key: URL_SAFE_NO_PAD.encode(signer.verifying_key().as_bytes()),
                server_signer: Arc::new(signer),
                teamspeak: false,
                screen: ScreenShared::new(0),
            },
            Arc::new(ArcSwap::from_pointee(Routing::default())),
            mpsc::channel(8).0,
            mpsc::channel(8).0,
            _tx.downgrade(),
        )
        .unwrap()
    }

    #[test]
    fn the_counter_adds_deltas_by_month_and_warns_once_per_threshold() {
        let mut core = core();
        let read = |interface: &str, transmitted: u64| {
            Some(Reading { interface: interface.into(), transmitted, scope: EgressScope::Host })
        };
        let october = 1_760_000_000_000;
        let november = october + 30 * 86_400_000;
        core.egress_tick(read("eth0", 1_000), (0, 0), october).unwrap();
        assert_eq!(core.egress.total, 0, "the first read is the baseline");
        core.egress_tick(read("eth0", 5_000), (100, 7), october).unwrap();
        assert_eq!((core.egress.total, core.egress.screen_members, core.egress.screen_guests), (4_000, 100, 7));
        core.egress_tick(read("eth0", 2_000), (150, 7), october).unwrap();
        assert_eq!(core.egress.total, 6_000, "a lower counter (a reboot) counts from zero");
        core.egress_tick(read("eth1", 500), (150, 7), october).unwrap();
        assert_eq!(core.egress.total, 6_000, "another interface rebaselines");
        core.egress_tick(None, (150, 7), october).unwrap();
        assert_eq!((core.egress.total, core.egress.scope), (6_000, Some(EgressScope::Unavailable)));

        // 1 TB: 80 % once, then 100 %.
        core.set_screen_limits(ScreenLimits { monthly_egress_tb: 1, ..Default::default() }).unwrap();
        core.egress_tick(read("eth1", 500 + 800_000_000_000), (150, 7), october).unwrap();
        assert_eq!(core.egress.warned, 80);
        core.egress_tick(read("eth1", 500 + 800_000_000_001), (150, 7), october).unwrap();
        assert_eq!(core.egress.warned, 80);
        assert!(!core.egress_stopped(), "only warned unless told to stop");
        core.egress_tick(read("eth1", 500 + 1_000_000_000_000), (150, 7), october).unwrap();
        assert_eq!(core.egress.warned, 100);
        core.set_screen_limits(ScreenLimits { monthly_egress_tb: 1, monthly_egress_stop: true, ..Default::default() })
            .unwrap();
        assert!(core.egress_stopped());
        assert_eq!(core.screen_shared.budget_bps.load(Ordering::Relaxed), 300_000_000);

        // A restart keeps the month's totals; a new month starts from zero.
        let stored = Egress::load(&core.store).unwrap();
        assert_eq!((stored.month.as_str(), stored.total, stored.warned), ("2025-10", core.egress.total, 100));
        core.egress_tick(read("eth1", 500 + 1_000_000_001_000), (160, 7), november).unwrap();
        assert_eq!((core.egress.month.as_str(), core.egress.total, core.egress.warned), ("2025-11", 0, 0));
        assert_eq!((core.egress.screen_members, core.egress.screen_guests), (0, 0));
        assert!(!core.egress_stopped());
    }

    #[test]
    fn route_selection_rejects_half_defaults_down_routes_and_uses_ipv6() {
        let route = "Iface Destination Gateway Flags RefCnt Use Metric Mask\n\
            reject 00000000 00000000 0201 0 0 0 00000000\n\
            half 00000000 00000000 0001 0 0 0 00000080\n\
            down 00000000 00000000 0000 0 0 0 00000000\n\
            eth0 00000000 00000000 0001 0 0 100 00000000\n";
        assert_eq!(default_interface(route).as_deref(), Some("eth0"));
        let zero = "00000000000000000000000000000000";
        let routes = format!(
            "{zero} 01 {zero} 00 {zero} 00000000 0 0 00000001 half\n\
            {zero} 00 {zero} 00 {zero} 00000001 0 0 00000201 reject\n\
            {zero} 00 {zero} 00 {zero} 00000001 0 0 00000000 down\n\
            {zero} 00 {zero} 00 {zero} 000000ff 0 0 00000001 eth1\n\
            {zero} 00 {zero} 00 {zero} 00000010 0 0 00000001 eth0"
        );
        assert_eq!(default_ipv6_interface(&routes).as_deref(), Some("eth0"));
        let rejected = "Iface Destination Gateway Flags RefCnt Use Metric Mask\n\
            reject 00000000 00000000 0201 0 0 0 00000000\n";
        assert_eq!(default_interface(rejected), None);
        assert_eq!(
            default_interface(rejected).or_else(|| default_ipv6_interface(&routes)).as_deref(),
            Some("eth0"),
            "an UP|REJECT IPv4 default must not hide the usable IPv6 default"
        );
    }

    #[test]
    fn failed_checkpoints_leave_memory_and_storage_unchanged_then_retry_once() {
        let mut core = core();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("counter.db");
        core.store = Store::open(&path).unwrap();
        let now = 1_760_000_000_000;
        let read = |n| Some(Reading { interface: "eth0".into(), transmitted: n, scope: EgressScope::Host });
        core.egress_tick(read(1000), (0, 0), now).unwrap();
        let before = core.egress.clone();
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE UPDATE ON meta WHEN NEW.key='egress_checkpoint' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        assert!(core.egress_tick(read(2000), (100, 7), now + 60_000).is_err());
        assert_eq!(core.egress, before);
        assert_eq!(Egress::load(&core.store).unwrap(), before);
        db.execute_batch("DROP TRIGGER fail_checkpoint;").unwrap();
        core.egress_tick(read(2000), (100, 7), now + 60_000).unwrap();
        assert_eq!((core.egress.total, core.egress.screen_members), (1000, 100));
        core.egress = Egress::load(&core.store).unwrap();
        core.egress_tick(read(2500), (0, 0), now + 120_000).unwrap();
        assert_eq!((core.egress.total, core.egress.screen_members), (1500, 100));
        core.egress_tick(read(9000), (0, 0), now + 30 * 86_400_000).unwrap();
        assert_eq!(core.egress.total, 0);
        assert!(core.egress.incomplete);
        assert!(Egress::load(&core.store).unwrap().incomplete);
    }

    #[test]
    fn limits_are_checked() {
        assert!(check_limits(&ScreenLimits::default()).is_ok());
        assert!(check_limits(&ScreenLimits { budget_mbps: 0, ..Default::default() }).is_err());
        assert!(check_limits(&ScreenLimits { share_cap_kbps: 50, ..Default::default() }).is_err());
    }
}
