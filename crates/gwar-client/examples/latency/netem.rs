//! Loopback UDP relay and a pure, seeded packet scheduler. No RTP inspection.
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};

const MAX_QUEUED: usize = 65_536;
/// Browsers open one flow per local ICE candidate that checks the relay.
const MAX_BROWSER_FLOWS: usize = 32;

#[derive(Clone, Debug, Serialize)]
pub struct Spec {
    pub source: String,
    pub seed: u64,
    pub up: Direction,
    pub down: Direction,
}
#[derive(Clone, Debug, Serialize)]
pub struct Direction {
    delay: Duration,
    jitter: Duration,
    normal: bool,
    keep_order: bool,
    loss: f64,
    burst: Option<(usize, f64)>,
    reorder: f64,
    reorder_delay: Duration,
    stall: Option<Stall>,
    seed: Option<u64>,
}
impl Default for Direction {
    fn default() -> Self {
        Self {
            delay: Duration::ZERO,
            jitter: Duration::ZERO,
            normal: false,
            keep_order: false,
            loss: 0.0,
            burst: None,
            reorder: 0.0,
            reorder_delay: Duration::from_millis(20),
            stall: None,
            seed: None,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
struct Stall {
    duration: Duration,
    at: Duration,
    every: Option<Duration>,
}

fn duration(value: &str) -> Result<Duration> {
    let (number, scale) = if let Some(number) = value.strip_suffix("ms") {
        (number, 0.001)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1.0)
    } else {
        bail!("duration needs ms or s: {value}");
    };
    let seconds = number.parse::<f64>().context("invalid duration")? * scale;
    ensure!(seconds.is_finite() && (0.0..=86_400.0).contains(&seconds), "duration must be within 0s..24h");
    Ok(Duration::from_secs_f64(seconds))
}
fn probability(value: &str) -> Result<f64> {
    let percent = value.strip_suffix('%').context("probability needs %")?.parse::<f64>()?;
    ensure!(percent.is_finite() && (0.0..=100.0).contains(&percent), "percentage must be within 0..100");
    Ok(percent / 100.0)
}
impl Spec {
    pub fn parse(source: &str) -> Result<Self> {
        let mut spec = Self { source: source.into(), seed: 1, up: Direction::default(), down: Direction::default() };
        let mut sections = std::collections::HashSet::new();
        for section in source.split(';').map(str::trim) {
            ensure!(!section.is_empty(), "empty LATENCY_NET section");
            if let Some(seed) = section.strip_prefix("seed=") {
                ensure!(sections.insert("seed"), "duplicate seed");
                spec.seed = seed.parse().context("seed must be an unsigned integer")?;
                continue;
            }
            let (name, options) = section.split_once(':').context("expected up:options or down:options")?;
            ensure!(sections.insert(name), "duplicate direction: {name}");
            let config = match name {
                "up" => &mut spec.up,
                "down" => &mut spec.down,
                _ => bail!("unknown direction: {name}"),
            };
            let mut keys = std::collections::HashSet::new();
            let mut every = None;
            for option in options.split(',').map(str::trim) {
                let (key, value) = option.split_once('=').unwrap_or((option, "true"));
                ensure!(keys.insert(key), "duplicate option: {key}");
                match key {
                    "delay" => config.delay = duration(value)?,
                    "jitter" => config.jitter = duration(value)?,
                    "distribution" => {
                        config.normal = match value {
                            "uniform" => false,
                            "normal" => true,
                            _ => bail!("distribution must be uniform or normal"),
                        }
                    }
                    "keep_order" => config.keep_order = value.parse().context("keep_order must be true or false")?,
                    "loss" => config.loss = probability(value)?,
                    "burst" => {
                        let (count, chance) = value.split_once('@').context("expected burst=N@P%")?;
                        let count = count.parse::<usize>()?;
                        ensure!((1..=MAX_QUEUED).contains(&count), "burst length must be within 1..{MAX_QUEUED}");
                        config.burst = Some((count, probability(chance)?));
                    }
                    "reorder" => config.reorder = probability(value)?,
                    "reorder_delay" => config.reorder_delay = duration(value)?,
                    "stall" => {
                        let (length, at) = value.split_once('@').context("expected stall=duration@time")?;
                        let length = duration(length)?;
                        ensure!(!length.is_zero(), "stall duration must be positive");
                        config.stall = Some(Stall { duration: length, at: duration(at)?, every: None });
                    }
                    "every" => every = Some(duration(value)?),
                    "seed" => config.seed = Some(value.parse().context("seed must be an unsigned integer")?),
                    _ => bail!("unknown LATENCY_NET option: {key}"),
                }
            }
            if let Some(every) = every {
                let stall = config.stall.as_mut().context("every requires stall")?;
                ensure!(every > stall.duration, "every must exceed stall duration");
                stall.every = Some(every);
            }
            ensure!(!config.keep_order || config.reorder == 0.0, "keep_order conflicts with reorder");
        }
        Ok(spec)
    }
}

/// SplitMix64 has a fixed algorithm, independent of rand/library versions.
struct Random(u64);
impl Random {
    fn unit(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^= value >> 31;
        (value >> 11) as f64 / (1u64 << 53) as f64
    }
    fn chance(&mut self, probability: f64) -> bool {
        probability > 0.0 && self.unit() < probability
    }
}

#[derive(Default, Debug, Serialize)]
pub struct Counts {
    received: u64,
    forwarded: u64,
    delayed: u64,
    dropped: u64,
    reordered: u64,
    pending: usize,
    stall_windows: Vec<StallWindow>,
}
#[derive(Debug, Serialize)]
struct StallWindow {
    flow: usize,
    start_s: f64,
    end_s: f64,
    first_packet_s: f64,
    packets: u64,
    first_release_s: Option<f64>,
    last_release_s: Option<f64>,
}
impl Counts {
    fn merge(&mut self, other: Self) {
        self.received += other.received;
        self.forwarded += other.forwarded;
        self.delayed += other.delayed;
        self.dropped += other.dropped;
        self.reordered += other.reordered;
        self.pending += other.pending;
        self.stall_windows.extend(other.stall_windows);
    }
}
struct Packet {
    due: Duration,
    sequence: u64,
    data: Vec<u8>,
    window: Option<usize>,
}
struct Scheduler {
    config: Direction,
    random: Random,
    burst_left: usize,
    last_due: Duration,
    highest_sent: Option<u64>,
    queue: BTreeMap<(Duration, u64), Packet>,
    counts: Counts,
    flow: usize,
}
impl Scheduler {
    fn new(config: Direction, seed: u64, flow: usize) -> Self {
        Self {
            config,
            random: Random(seed),
            burst_left: 0,
            last_due: Duration::ZERO,
            highest_sent: None,
            queue: BTreeMap::new(),
            counts: Counts::default(),
            flow,
        }
    }
    fn enqueue(&mut self, at: Duration, data: Vec<u8>) -> Result<()> {
        let sequence = self.counts.received;
        self.counts.received += 1;
        let burst_drop = if self.burst_left > 0 {
            self.burst_left -= 1;
            true
        } else if let Some((length, chance)) = self.config.burst {
            if self.random.chance(chance) {
                self.burst_left = length - 1;
                true
            } else {
                false
            }
        } else {
            false
        };
        if burst_drop || self.random.chance(self.config.loss) {
            self.counts.dropped += 1;
            return Ok(());
        }
        ensure!(self.queue.len() < MAX_QUEUED, "netem queue exceeds {MAX_QUEUED} packets");
        let jitter = if self.config.jitter.is_zero() {
            0.0
        } else if self.config.normal {
            // Normal sigma = bound/3, clipped to the configured bound.
            ((-2.0 * (1.0 - self.random.unit()).ln()).sqrt() * (std::f64::consts::TAU * self.random.unit()).cos() / 3.0)
                .clamp(-1.0, 1.0)
        } else {
            2.0 * self.random.unit() - 1.0
        };
        let delay = (self.config.delay.as_secs_f64() + jitter * self.config.jitter.as_secs_f64()).max(0.0);
        let mut due = at + Duration::from_secs_f64(delay);
        if self.random.chance(self.config.reorder) {
            due += self.config.reorder_delay;
        }
        let mut window = None;
        if let Some(stall) = &self.config.stall
            && due >= stall.at
        {
            let start = match stall.every {
                Some(every) => due - Duration::from_nanos(((due - stall.at).as_nanos() % every.as_nanos()) as u64),
                None => stall.at,
            };
            let end = start + stall.duration;
            if due < end {
                let index = self.counts.stall_windows.iter().position(|w| w.start_s == start.as_secs_f64());
                let index = index.unwrap_or_else(|| {
                    self.counts.stall_windows.push(StallWindow {
                        flow: self.flow,
                        start_s: start.as_secs_f64(),
                        end_s: end.as_secs_f64(),
                        first_packet_s: at.as_secs_f64(),
                        packets: 0,
                        first_release_s: None,
                        last_release_s: None,
                    });
                    self.counts.stall_windows.len() - 1
                });
                self.counts.stall_windows[index].packets += 1;
                window = Some(index);
                due = end;
            }
        }
        if self.config.keep_order {
            due = due.max(self.last_due);
        }
        self.last_due = due;
        self.counts.delayed += u64::from(due > at);
        self.queue.insert((due, sequence), Packet { due, sequence, data, window });
        Ok(())
    }
    fn next_due(&self) -> Option<Duration> {
        self.queue.first_key_value().map(|(key, _)| key.0)
    }
    fn pop(&mut self, now: Duration) -> Option<Packet> {
        if self.next_due()? > now {
            return None;
        }
        self.queue.pop_first().map(|(_, packet)| packet)
    }
    fn sent(&mut self, packet: &Packet, now: Duration) {
        self.counts.forwarded += 1;
        self.counts.reordered += u64::from(self.highest_sent.is_some_and(|highest| packet.sequence < highest));
        self.highest_sent = Some(self.highest_sent.unwrap_or(0).max(packet.sequence));
        if let Some(index) = packet.window {
            let window = &mut self.counts.stall_windows[index];
            window.first_release_s.get_or_insert(now.as_secs_f64());
            window.last_release_s = Some(now.as_secs_f64());
        }
    }
    fn finish(mut self) -> Counts {
        self.counts.pending = self.queue.len();
        self.counts
    }
}

#[derive(Serialize)]
pub struct Report {
    pub spec: Spec,
    pub relay: SocketAddr,
    pub server: SocketAddr,
    pub flows: usize,
    pub up: Counts,
    pub down: Counts,
    /// Browser mode only: downlink datagrams the OS refused to send (see `bind_for_browsers`).
    pub downlink_send_errors: u64,
}
pub struct Relay {
    socket: Arc<UdpSocket>,
    browsers: bool,
    task: Option<JoinHandle<Result<Report>>>,
    stop: Option<oneshot::Sender<()>>,
}
impl Relay {
    pub async fn bind() -> Result<Self> {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.context("bind netem relay")?);
        Ok(Self { socket, browsers: false, task: None, stop: None })
    }
    /// For browser clients (apps/web/e2e/latency.mjs): every local ICE candidate that checks the
    /// relay is its own flow, and a candidate on a host interface reaches this loopback socket
    /// with that interface's source address (only local datagrams can arrive here). macOS may
    /// refuse a reply from loopback to such a candidate; the browser then uses another pair,
    /// so refused downlink sends are counted instead of failing the run.
    #[allow(dead_code)] // examples/web_latency.rs only
    pub async fn bind_for_browsers() -> Result<Self> {
        let mut relay = Self::bind().await?;
        relay.browsers = true;
        Ok(relay)
    }
    pub fn address(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }
    pub fn start(&mut self, server: SocketAddr, spec: Spec, epoch: Instant) {
        let (stop, receive) = oneshot::channel();
        self.stop = Some(stop);
        self.task = Some(tokio::spawn(relay(self.socket.clone(), server, spec, epoch, receive, self.browsers)));
    }
    pub async fn finish(&mut self) -> Result<Report> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.as_mut().context("relay not started")?.await.context("netem task failed")?
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}
struct Flow {
    client: SocketAddr,
    upstream: Arc<UdpSocket>,
    up: Scheduler,
    down: Scheduler,
}
struct Incoming {
    flow: usize,
    at: Duration,
    data: Vec<u8>,
}
async fn relay(
    front: Arc<UdpSocket>,
    server: SocketAddr,
    spec: Spec,
    epoch: Instant,
    mut stop: oneshot::Receiver<()>,
    browsers: bool,
) -> Result<Report> {
    ensure!(server.ip().is_loopback(), "netem is loopback-only");
    ensure!(server != front.local_addr()?, "relay upstream must differ from its advertised address");
    let (incoming, mut replies) = mpsc::channel::<Incoming>(4096);
    let mut readers = JoinSet::<Result<()>>::new();
    let mut flows: Vec<Flow> = Vec::new();
    let mut by_client = HashMap::new();
    let mut buffer = vec![0; 65_536];
    let mut downlink_send_errors = 0;
    loop {
        // Drain due packets before polling arrivals, including the whole stall burst.
        for flow in &mut flows {
            while let Some(packet) = flow.up.pop(epoch.elapsed()) {
                debug_assert!(packet.due <= epoch.elapsed());
                flow.upstream.send(&packet.data).await.context("relay uplink send")?;
                flow.up.sent(&packet, epoch.elapsed());
            }
            while let Some(packet) = flow.down.pop(epoch.elapsed()) {
                match front.send_to(&packet.data, flow.client).await {
                    Err(_) if browsers => downlink_send_errors += 1,
                    sent => {
                        sent.context("relay downlink send")?;
                    }
                }
                flow.down.sent(&packet, epoch.elapsed());
            }
        }
        let next = flows.iter().flat_map(|flow| [flow.up.next_due(), flow.down.next_due()]).flatten().min();
        let deadline = epoch + next.unwrap_or_else(|| epoch.elapsed() + Duration::from_secs(3600));
        tokio::select! {
            _ = &mut stop => break,
            completed = readers.join_next(), if !readers.is_empty() => {
                completed.context("missing relay reader")???;
                bail!("relay reader stopped unexpectedly");
            }
            received = front.recv_from(&mut buffer) => {
                let (size, client) = received.context("relay receive")?;
                ensure!(browsers || client.ip().is_loopback(), "non-loopback relay client");
                let at = epoch.elapsed();
                let flow = if let Some(&flow) = by_client.get(&client) {
                    flow
                } else {
                    let limit = if browsers { MAX_BROWSER_FLOWS } else { 2 };
                    ensure!(flows.len() < limit, "latency relay supports only {limit} flows");
                    let id = flows.len();
                    let upstream = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
                    upstream.connect(server).await?;
                    let socket = upstream.clone();
                    let incoming = incoming.clone();
                    readers.spawn(async move {
                        let mut buffer = vec![0; 65_536];
                        loop {
                            let size = socket.recv(&mut buffer).await.context("relay upstream receive")?;
                            let packet = Incoming { flow: id, at: epoch.elapsed(), data: buffer[..size].to_vec() };
                            if incoming.send(packet).await.is_err() {
                                return Ok(());
                            }
                        }
                    });
                    // Separate random streams prevent cross-flow/direction interleaving
                    // from changing packet decisions. Flow 0 is the first client.
                    let flow_seed = (id as u64).wrapping_mul(0x9e3779b97f4a7c15);
                    let up_seed = spec.up.seed.unwrap_or(spec.seed) ^ flow_seed;
                    let down_seed = spec.down.seed.unwrap_or(spec.seed) ^ flow_seed ^ 0xd1b54a32d192ed03;
                    flows.push(Flow {
                        client,
                        upstream,
                        up: Scheduler::new(spec.up.clone(), up_seed, id),
                        down: Scheduler::new(spec.down.clone(), down_seed, id),
                    });
                    by_client.insert(client, id);
                    id
                };
                flows[flow].up.enqueue(at, buffer[..size].to_vec())?;
            }
            Some(packet) = replies.recv() => flows[packet.flow].down.enqueue(packet.at, packet.data)?,
            _ = tokio::time::sleep_until(deadline.into()) => {},
        }
    }
    readers.abort_all();
    while readers.join_next().await.is_some() {}
    // Account for already-read replies which were queued at shutdown.
    while let Ok(packet) = replies.try_recv() {
        flows[packet.flow].down.enqueue(packet.at, packet.data)?;
    }
    let mut report = Report {
        spec,
        relay: front.local_addr()?,
        server,
        flows: flows.len(),
        up: Counts::default(),
        down: Counts::default(),
        downlink_send_errors,
    };
    for flow in flows {
        report.up.merge(flow.up.finish());
        report.down.merge(flow.down.finish());
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler(source: &str) -> Scheduler {
        let spec = Spec::parse(source).unwrap();
        Scheduler::new(spec.up, spec.seed, 0)
    }
    fn drain(scheduler: &mut Scheduler) -> Vec<(u64, Duration)> {
        let mut sent = Vec::new();
        while let Some(packet) = scheduler.pop(Duration::from_secs(86_400)) {
            scheduler.sent(&packet, packet.due);
            sent.push((packet.sequence, packet.due));
        }
        sent
    }
    #[test]
    fn strict_spec_validation() {
        Spec::parse("seed=42;up:stall=270ms@12s,jitter=5ms,every=10s;down:loss=1%,reorder=0.5%").unwrap();
        for invalid in [
            "",
            "up:",
            "up:delay=1",
            "up:loss=NaN%",
            "up:loss=101%",
            "up:jitter=-1ms",
            "up:stall=0s@1s",
            "up:every=1s",
            "up:stall=2s@0s,every=1s",
            "up:burst=0@1%",
            "up:keep_order,reorder=1%",
            "up:unknown=1",
            "up:loss=1%,loss=2%",
            "up:loss=1%;up:delay=1ms",
            "seed=1;seed=2",
        ] {
            assert!(Spec::parse(invalid).is_err(), "accepted {invalid}");
        }
    }
    #[test]
    fn passthrough_and_base_delay() {
        for (source, delay) in [("seed=1", 0), ("up:delay=10ms", 10)] {
            let mut scheduler = scheduler(source);
            for i in 0..10 {
                scheduler.enqueue(Duration::from_millis(i), vec![i as u8]).unwrap();
            }
            for i in 0..10 {
                let due = Duration::from_millis(i + delay);
                if !due.is_zero() {
                    assert!(scheduler.pop(due - Duration::from_nanos(1)).is_none());
                }
                let packet = scheduler.pop(due).unwrap();
                assert_eq!(packet.data, [i as u8]);
                scheduler.sent(&packet, due);
            }
            assert_eq!(scheduler.counts.delayed, if delay == 0 { 0 } else { 10 });
            assert_eq!(scheduler.counts.forwarded, 10);
        }
    }
    #[test]
    fn seed_reproduces_bounded_jitter_loss_and_reordering() {
        for distribution in ["uniform", "normal"] {
            let spec = format!("seed=17;up:delay=10ms,jitter=10ms,distribution={distribution},loss=10%,burst=3@2%");
            let trace = |seeded: &str| {
                let mut scheduler = scheduler(seeded);
                for i in 0..1000 {
                    scheduler.enqueue(Duration::from_millis(i), Vec::new()).unwrap();
                }
                let sent = drain(&mut scheduler);
                for &(sequence, due) in &sent {
                    assert!(due >= Duration::from_millis(sequence));
                    assert!(due <= Duration::from_millis(sequence + 20));
                }
                assert!(scheduler.counts.dropped > 0);
                assert!(scheduler.counts.reordered > 0);
                sent
            };
            assert_eq!(trace(&spec), trace(&spec));
            assert_ne!(trace(&spec), trace(&spec.replace("seed=17", "seed=18")));
        }
        // Pin the generator so the same seed remains stable after refactors.
        assert!((Random(1).unit() - 0.566561575172281).abs() < 1e-15);
    }
    #[test]
    fn jitter_reorders_unless_keep_order() {
        for keep_order in [false, true] {
            let mut scheduler = scheduler(&format!("up:jitter=10ms,keep_order={keep_order}"));
            for i in 0..100 {
                scheduler.enqueue(Duration::from_millis(i), Vec::new()).unwrap();
            }
            let sent = drain(&mut scheduler);
            assert_eq!(sent.windows(2).all(|pair| pair[0].0 < pair[1].0), keep_order);
            assert_eq!(scheduler.counts.reordered == 0, keep_order);
        }
    }
    #[test]
    fn explicit_reordering_counts_actual_late_packets() {
        let mut scheduler = scheduler("seed=1;up:reorder=50%,reorder_delay=20ms");
        for i in 0..100 {
            scheduler.enqueue(Duration::from_millis(i), Vec::new()).unwrap();
        }
        drain(&mut scheduler);
        assert!(scheduler.counts.reordered > 0);
        assert_eq!(scheduler.counts.forwarded, 100);
    }
    #[test]
    fn stall_releases_a_burst_and_periodic_windows() {
        let mut scheduler = scheduler("up:stall=270ms@12s,every=10s");
        for at in [11_999, 12_000, 12_100, 12_269, 12_270, 22_001, 22_100] {
            scheduler.enqueue(Duration::from_millis(at), Vec::new()).unwrap();
        }
        assert_eq!(scheduler.pop(Duration::from_millis(11_999)).unwrap().sequence, 0);
        assert!(scheduler.pop(Duration::from_millis(12_269)).is_none());
        let sent = drain(&mut scheduler);
        assert_eq!(sent[..4].iter().map(|p| p.1).collect::<Vec<_>>(), vec![Duration::from_millis(12_270); 4]);
        assert_eq!(scheduler.counts.stall_windows.len(), 2);
        assert_eq!(scheduler.counts.stall_windows[0].packets, 3);
        assert_eq!(scheduler.counts.stall_windows[1].packets, 2);
        assert_eq!(scheduler.counts.stall_windows[0].first_release_s, Some(12.27));
        assert_eq!(scheduler.counts.reordered, 0);
    }
    #[test]
    fn stall_holds_packets_already_in_flight() {
        let mut scheduler = scheduler("up:delay=20ms,stall=270ms@12s,keep_order");
        scheduler.enqueue(Duration::from_millis(11_990), Vec::new()).unwrap();
        scheduler.enqueue(Duration::from_millis(12_260), Vec::new()).unwrap();
        assert_eq!(drain(&mut scheduler), [(0, Duration::from_millis(12_270)), (1, Duration::from_millis(12_280))]);
    }
    #[test]
    fn burst_loss_is_consecutive_and_full_loss_drops_every_packet() {
        let mut scheduler = scheduler("seed=5;up:burst=3@20%");
        for i in 0..100 {
            scheduler.enqueue(Duration::from_millis(i), Vec::new()).unwrap();
        }
        let sent = drain(&mut scheduler);
        let sequences: Vec<_> = sent.iter().map(|p| p.0).collect();
        assert!(sequences.windows(2).any(|p| p[1] - p[0] == 4));
        assert!(sequences.windows(2).all(|p| (p[1] - p[0] - 1) % 3 == 0));
        let mut scheduler = Scheduler::new(Spec::parse("up:loss=100%").unwrap().up, 1, 0);
        for _ in 0..10 {
            scheduler.enqueue(Duration::ZERO, Vec::new()).unwrap();
        }
        assert_eq!(scheduler.counts.dropped, 10);
        assert!(scheduler.queue.is_empty());
    }
    #[tokio::test]
    async fn relay_keeps_two_clients_and_return_addresses_separate() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let mut relay = Relay::bind().await.unwrap();
            relay.start(server.local_addr().unwrap(), Spec::parse("seed=1").unwrap(), Instant::now());
            let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let mut buffer = [0; 10];
            let mut sources = Vec::new();
            for (client, data) in [(&a, b"a"), (&b, b"b")] {
                client.send_to(data, relay.address().unwrap()).await.unwrap();
                let (size, source) = server.recv_from(&mut buffer).await.unwrap();
                assert_eq!(&buffer[..size], data);
                sources.push(source);
                server.send_to(&buffer[..size], source).await.unwrap();
                let (size, source) = client.recv_from(&mut buffer).await.unwrap();
                assert_eq!(source, relay.address().unwrap());
                assert_eq!(&buffer[..size], data);
            }
            assert_ne!(sources[0], sources[1]);
            let report = relay.finish().await.unwrap();
            assert_eq!(report.flows, 2);
            assert_eq!((report.up.forwarded, report.down.forwarded), (2, 2));
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn browser_relay_accepts_a_flow_per_candidate() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let mut relay = Relay::bind_for_browsers().await.unwrap();
            relay.start(server.local_addr().unwrap(), Spec::parse("seed=1").unwrap(), Instant::now());
            let mut buffer = [0; 10];
            for _ in 0..3 {
                let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                client.send_to(b"c", relay.address().unwrap()).await.unwrap();
                let (size, source) = server.recv_from(&mut buffer).await.unwrap();
                server.send_to(&buffer[..size], source).await.unwrap();
                client.recv_from(&mut buffer).await.unwrap();
            }
            let report = relay.finish().await.unwrap();
            assert_eq!((report.flows, report.downlink_send_errors), (3, 0));
        })
        .await
        .unwrap();
    }
}
