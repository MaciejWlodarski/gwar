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
#[derive(Debug, Clone, PartialEq, Eq)]
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
            (fields.get(1) == Some(&"00000000")).then(|| (metric, fields[0].to_owned()))
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
    let interface = default_interface(&std::fs::read_to_string("/proc/net/route").ok()?)?;
    let transmitted = transmitted(&std::fs::read_to_string("/proc/net/dev").ok()?, &interface)?;
    let scope = if in_container() { EgressScope::Container } else { EgressScope::Host };
    Some(Reading { interface, transmitted, scope })
}

/// `YYYY-MM` of a Unix time in milliseconds (UTC).
fn month(ms: i64) -> String {
    crate::members::civil_date(ms)[..7].to_owned()
}

/// The month's totals, mirrored from the store.
#[derive(Debug, Default)]
pub(super) struct Egress {
    month: String,
    scope: Option<EgressScope>,
    total: u64,
    screen_members: u64,
    screen_guests: u64,
    /// The highest warning given this month: 0, 80 or 100.
    warned: u32,
    /// The screen SFU's cumulative counters at the last tick.
    seen: (u64, u64),
}

impl Egress {
    /// The month's totals as stored (a restart keeps counting).
    pub(super) fn load(store: &Store) -> Result<Self> {
        let number = |key: &str| -> Result<u64> { Ok(store.meta(key)?.and_then(|v| v.parse().ok()).unwrap_or(0)) };
        Ok(Self {
            month: store.meta("egress_month")?.unwrap_or_default(),
            scope: None,
            total: number("egress_total")?,
            screen_members: number("egress_screen_members")?,
            screen_guests: number("egress_screen_guests")?,
            warned: number("egress_warned")? as u32,
            seen: (0, 0),
        })
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

    /// One minute's reading, at `now` (Unix ms). The interface counter's delta since the last
    /// read joins the month's total; a counter lower than the last read (a reboot or a wrap),
    /// or another interface, counts from zero. The first read ever only sets the baseline.
    pub(super) fn egress_tick(&mut self, reading: Option<Reading>, shared: (u64, u64), now: i64) -> Result<()> {
        let month = month(now);
        if self.egress.month != month {
            self.egress = Egress { month: month.clone(), seen: self.egress.seen, ..Egress::default() };
            self.store.set_meta("egress_month", &month)?;
            self.store.set_meta("egress_warned", "0")?;
        }
        let (members, guests) = shared;
        self.egress.screen_members += members.saturating_sub(self.egress.seen.0);
        self.egress.screen_guests += guests.saturating_sub(self.egress.seen.1);
        self.egress.seen = shared;
        self.egress.scope = Some(reading.as_ref().map_or(EgressScope::Unavailable, |r| r.scope));
        if let Some(reading) = reading {
            let last = self.store.meta("egress_counter")?;
            let delta = match last.as_deref().and_then(|l| l.rsplit_once(':')) {
                Some((interface, value)) if interface == reading.interface => match value.parse::<u64>() {
                    Ok(value) if reading.transmitted >= value => reading.transmitted - value,
                    _ => reading.transmitted,
                },
                Some(_) => reading.transmitted,
                None => 0,
            };
            self.egress.total += delta;
            self.store.set_meta("egress_counter", &format!("{}:{}", reading.interface, reading.transmitted))?;
        }
        self.store.set_meta("egress_total", &self.egress.total.to_string())?;
        self.store.set_meta("egress_screen_members", &self.egress.screen_members.to_string())?;
        self.store.set_meta("egress_screen_guests", &self.egress.screen_guests.to_string())?;
        self.egress_warn()
    }

    /// Warns admins once a month for each of 80 % and 100 % of the limit.
    fn egress_warn(&mut self) -> Result<()> {
        let limit = self.egress_limit();
        let total = self.egress.total;
        let percent = [100, 80].into_iter().find(|&p| total as f64 >= limit as f64 * p as f64 / 100.0);
        let Some(percent) = percent.filter(|&p| p > self.egress.warned) else { return Ok(()) };
        self.egress.warned = percent;
        self.store.set_meta("egress_warned", &percent.to_string())?;
        warn!(
            month = self.egress.month,
            total, limit, "this month's outbound transfer reached {percent} % of the limit"
        );
        let admins: Vec<SessionId> =
            self.sessions.keys().copied().filter(|&s| self.require(s, Permission::ManageServer).is_ok()).collect();
        let event = Event::ServerEgressWarning { month: self.egress.month.clone(), percent, total, limit };
        self.broadcast(event, |s| admins.contains(&s.id));
        Ok(())
    }

    /// `server.egress`, for the server settings.
    pub(super) fn server_egress(&self, session: SessionId) -> Reply {
        self.require(session, Permission::ManageServer)?;
        let scope = self.egress.scope.unwrap_or(EgressScope::Unavailable);
        Ok(Response::ServerEgress(ServerEgress {
            month: if self.egress.month.is_empty() { month(now_ms()) } else { self.egress.month.clone() },
            scope,
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

    #[test]
    fn limits_are_checked() {
        assert!(check_limits(&ScreenLimits::default()).is_ok());
        assert!(check_limits(&ScreenLimits { budget_mbps: 0, ..Default::default() }).is_err());
        assert!(check_limits(&ScreenLimits { share_cap_kbps: 50, ..Default::default() }).is_err());
    }
}
