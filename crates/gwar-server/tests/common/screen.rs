//! Screen-sharing peers for the server tests: a str0m publisher writing synthetic VP8 frames
//! and a str0m viewer with the watch transport's slots, both in RTP mode over real SRTP.
#![allow(dead_code)]
use super::Client;
use serde_json::json;
use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    change::SdpAnswer,
    format::Codec,
    media::{Direction, MediaKind, Mid},
    net::{Protocol, Receive},
    rtp::RtpWrite,
};
use tokio::{net::UdpSocket, sync::mpsc};

/// One RTP packet as the publisher writes it or a viewer receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub sequence: u64,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
}

/// A VP8 payload: descriptor (S, partition 0) and payload header (keyframe bit), then `body`.
pub fn vp8(start: bool, keyframe: bool, body: &[u8]) -> Vec<u8> {
    let mut payload = vec![if start { 0x10 } else { 0x00 }, if keyframe { 0x00 } else { 0x01 }];
    payload.extend_from_slice(body);
    payload
}

/// A frame of `packets` packets with timestamp `timestamp`, each tagged with `tag` and its index.
/// Packets are near full size, like a real share's: str0m sizes its probes in payload bytes,
/// and on tiny packets the headers alone would overflow a viewer's extras reserve.
pub fn frame(sequence: u64, timestamp: u32, keyframe: bool, packets: usize, tag: u8) -> Vec<Packet> {
    (0..packets)
        .map(|i| {
            let mut body = vec![0x55; BODY];
            body[..4].copy_from_slice(&[tag, i as u8, 0xaa, 0x55]);
            Packet {
                sequence: sequence + i as u64,
                timestamp,
                marker: i + 1 == packets,
                payload: vp8(i == 0, keyframe, &body),
            }
        })
        .collect()
}

/// A test packet's body: its tag, index and filler.
const BODY: usize = 1000;

/// Which outgoing or incoming RTP datagrams a peer drops on purpose, by sequence number on the
/// given stream: each listed number is dropped once, so its repair gets through.
#[derive(Default, Clone)]
pub struct Loss(Arc<Mutex<HashSet<u16>>>);

/// The SSRC of an RTP datagram (the header stays clear under SRTP); `None` for anything else.
fn rtp_ssrc(datagram: &[u8]) -> Option<u32> {
    (datagram.len() >= 12 && datagram[0] >> 6 == 2 && !(192..=223).contains(&(datagram[1] & 0x7f)))
        .then(|| u32::from_be_bytes(datagram[8..12].try_into().unwrap()))
}

impl Loss {
    pub fn drop_once(&self, sequences: impl IntoIterator<Item = u16>) {
        self.0.lock().unwrap().extend(sequences);
    }
    /// Whether to drop this datagram: plain RTP on `ssrc` with a listed sequence number.
    fn take(&self, datagram: &[u8], ssrc: Option<u32>) -> bool {
        if datagram.len() < 12 || datagram[0] >> 6 != 2 || (192..=223).contains(&(datagram[1] & 0x7f)) {
            return false;
        }
        let found = u32::from_be_bytes(datagram[8..12].try_into().unwrap());
        if ssrc.is_some_and(|ssrc| ssrc != found) {
            return false;
        }
        let sequence = u16::from_be_bytes([datagram[2], datagram[3]]);
        self.0.lock().unwrap().remove(&sequence)
    }
}

pub struct Publisher {
    pub share: String,
    pub ssrc: u32,
    pub rtx: u32,
    tx: mpsc::Sender<Packet>,
    /// Keyframe requests (PLI/FIR) the server sent.
    pub keyframe_requests: Arc<AtomicU64>,
    /// Retransmissions sent on the RTX stream, answering the server's NACKs.
    pub repairs: Arc<AtomicU64>,
    /// Outgoing originals to lose on the way to the server.
    pub loss: Loss,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn socket() -> (UdpSocket, std::net::SocketAddr) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local = socket.local_addr().unwrap();
    (socket, local)
}

fn rtc(local: std::net::SocketAddr) -> Rtc {
    let mut rtc =
        Rtc::builder().set_rtp_mode(true).clear_codecs().enable_vp8(true).enable_vp9(true).build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    rtc
}

fn video_ssrcs(sdp: &str) -> (u32, u32) {
    let group = sdp.lines().find_map(|l| l.strip_prefix("a=ssrc-group:FID ")).expect("RTX group");
    let mut ids = group.split_whitespace().map(|s| s.parse().unwrap());
    (ids.next().unwrap(), ids.next().unwrap())
}

impl Publisher {
    /// Negotiates the publish transport of `share` and waits until it is connected.
    pub async fn start(client: &mut Client, share: &str) -> Self {
        let (socket, local) = socket().await;
        let mut rtc = rtc(local);
        let mut api = rtc.sdp_api();
        let mid = api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, pending) = api.apply().unwrap();
        let sdp = offer.to_sdp_string();
        let (ssrc, rtx) = video_ssrcs(&sdp);
        let answer =
            client.ok("screen.publish.offer", json!({"share": share, "ssrc": ssrc, "rtx_ssrc": rtx, "sdp": sdp})).await;
        rtc.sdp_api()
            .accept_answer(pending, SdpAnswer::from_sdp_string(answer["sdp"].as_str().unwrap()).unwrap())
            .unwrap();
        let pt = rtc.codec_config().params().iter().find(|p| p.spec().codec == Codec::Vp8).unwrap().pt();
        let (tx, mut commands) = mpsc::channel::<Packet>(4096);
        let keyframe_requests = Arc::new(AtomicU64::new(0));
        let requests = keyframe_requests.clone();
        let repairs = Arc::new(AtomicU64::new(0));
        let repaired = repairs.clone();
        let loss = Loss::default();
        let lose = loss.clone();
        let (ready, connected) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready);
            let mut bytes = [0; 2048];
            loop {
                let deadline = loop {
                    match rtc.poll_output().unwrap() {
                        Output::Timeout(at) => break at,
                        Output::Transmit(t) => {
                            if rtp_ssrc(&t.contents) == Some(rtx) {
                                repaired.fetch_add(1, Ordering::Relaxed);
                            }
                            if !lose.take(&t.contents, Some(ssrc)) {
                                let _ = socket.send_to(&t.contents, t.destination).await;
                            }
                        }
                        Output::Event(Event::Connected) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(());
                            }
                        }
                        Output::Event(Event::KeyframeRequest(_)) => {
                            requests.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {}
                    }
                };
                tokio::select! {
                    packet = commands.recv() => {
                        let Some(p) = packet else { return };
                        let mut direct = rtc.direct_api();
                        let stream = direct.stream_tx_by_mid(mid, None).unwrap();
                        stream.write_rtp(
                            RtpWrite::new(pt, p.sequence.into(), p.timestamp, Instant::now(), p.payload)
                                .marker(p.marker)
                                .nackable(true),
                        );
                    },
                    got = socket.recv_from(&mut bytes) => {
                        let (n, source) = got.unwrap();
                        let receive = Receive { proto: Protocol::Udp, source, destination: local, contents: bytes[..n].try_into().unwrap() };
                        let _ = rtc.handle_input(Input::Receive(Instant::now(), receive));
                    },
                    _ = tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_millis(50)).into()) => {}
                }
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
            }
        });
        tokio::time::timeout(Duration::from_secs(5), connected).await.expect("publish transport connects").unwrap();
        Self { share: share.to_owned(), ssrc, rtx, tx, keyframe_requests, repairs, loss, task }
    }

    pub async fn send(&self, packets: impl IntoIterator<Item = Packet>) {
        for packet in packets {
            self.tx.send(packet).await.unwrap();
        }
    }

    pub fn keyframe_requests(&self) -> u64 {
        self.keyframe_requests.load(Ordering::Relaxed)
    }

    pub fn repairs(&self) -> u64 {
        self.repairs.load(Ordering::Relaxed)
    }
}

/// Holds a peer's outgoing datagrams back while closed (its NACKs, say), and lets them all go
/// when opened.
#[derive(Default, Clone)]
pub struct Gate(Arc<Mutex<(bool, Vec<Held>)>>);

type Held = (Vec<u8>, std::net::SocketAddr);

impl Gate {
    pub fn close(&self) {
        self.0.lock().unwrap().0 = true;
    }

    pub fn open(&self) {
        self.0.lock().unwrap().0 = false;
    }

    /// Whether anything is held.
    pub fn holding(&self) -> bool {
        !self.0.lock().unwrap().1.is_empty()
    }

    /// Holds the datagram if closed; otherwise what may go now, held ones first.
    fn pass(&self, datagram: Option<(&[u8], std::net::SocketAddr)>) -> Vec<Held> {
        let mut gate = self.0.lock().unwrap();
        if let Some((contents, to)) = datagram {
            gate.1.push((contents.to_vec(), to));
        }
        if gate.0 { Vec::new() } else { std::mem::take(&mut gate.1) }
    }
}

pub struct Viewer {
    pub run: u64,
    rx: mpsc::Receiver<(usize, Packet)>,
    /// Incoming datagrams to lose on the way from the server, on any stream.
    pub loss: Loss,
    /// Outgoing datagrams to hold back.
    pub gate: Gate,
    /// Drops every incoming repair (RTX: NACK answers and padding that reuses media) while set.
    pub drop_repairs: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Viewer {
    /// Negotiates a watch transport with run `run` and waits until it is connected.
    pub async fn start(client: &mut Client, run: u64) -> Self {
        Self::negotiate(gwar_proto::SCREEN_SLOTS, run, move |sdp| async move {
            let answer = client.ok("screen.watch.offer", json!({"run": run, "sdp": sdp})).await;
            answer["sdp"].as_str().unwrap().to_owned()
        })
        .await
    }

    /// A watch transport of `slots` receive-only video slots, negotiated by `offer` (which
    /// answers the SDP answer); waits until it is connected.
    pub async fn negotiate<F: std::future::Future<Output = String>>(
        slots: usize,
        run: u64,
        offer: impl FnOnce(String) -> F,
    ) -> Self {
        let (socket, local) = socket().await;
        let mut rtc = rtc(local);
        // str0m's random mids can collide within one offer: redraw.
        let (mids, sdp, pending) = loop {
            let mut api = rtc.sdp_api();
            let mids: Vec<Mid> =
                (0..slots).map(|_| api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None)).collect();
            let unique: HashSet<_> = mids.iter().collect();
            if unique.len() == mids.len() {
                let (offer, pending) = api.apply().unwrap();
                break (mids, offer, pending);
            }
        };
        let answer = offer(sdp.to_sdp_string()).await;
        rtc.sdp_api().accept_answer(pending, SdpAnswer::from_sdp_string(&answer).unwrap()).unwrap();
        let (received, rx) = mpsc::channel(8192);
        let loss = Loss::default();
        let lose = loss.clone();
        let gate = Gate::default();
        let held = gate.clone();
        let drop_repairs = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let repairs = drop_repairs.clone();
        let rtx: HashSet<u8> = rtc.codec_config().params().iter().filter_map(|p| p.resend()).map(|pt| *pt).collect();
        let (ready, connected) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready);
            let mut bytes = [0; 2048];
            loop {
                let deadline = loop {
                    match rtc.poll_output().unwrap() {
                        Output::Timeout(at) => break at,
                        Output::Transmit(t) => {
                            for (contents, to) in held.pass(Some((&t.contents, t.destination))) {
                                let _ = socket.send_to(&contents, to).await;
                            }
                        }
                        Output::Event(Event::Connected) => {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(());
                            }
                        }
                        Output::Event(Event::RtpPacket(p)) => {
                            let Some(slot) = rtc.direct_api().stream_rx(&p.header.ssrc).map(|s| s.mid()) else {
                                continue;
                            };
                            let Some(slot) = mids.iter().position(|m| *m == slot) else { continue };
                            let packet = Packet {
                                sequence: *p.seq_no,
                                timestamp: p.header.timestamp,
                                marker: p.header.marker,
                                payload: p.payload.to_vec(),
                            };
                            let _ = received.try_send((slot, packet));
                        }
                        _ => {}
                    }
                };
                tokio::select! {
                    got = socket.recv_from(&mut bytes) => {
                        let (n, source) = got.unwrap();
                        if lose.take(&bytes[..n], None) {
                            continue;
                        }
                        let repair = rtp_ssrc(&bytes[..n]).is_some() && rtx.contains(&(bytes[1] & 0x7f));
                        if repair && repairs.load(Ordering::Relaxed) {
                            continue;
                        }
                        let receive = Receive { proto: Protocol::Udp, source, destination: local, contents: bytes[..n].try_into().unwrap() };
                        let _ = rtc.handle_input(Input::Receive(Instant::now(), receive));
                    },
                    _ = tokio::time::sleep_until(deadline.into()) => {}
                }
                let _ = rtc.handle_input(Input::Timeout(Instant::now()));
                for (contents, to) in held.pass(None) {
                    let _ = socket.send_to(&contents, to).await;
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), connected).await.expect("watch transport connects").unwrap();
        Self { run, rx, loss, gate, drop_repairs, task }
    }

    /// The next packet received in any slot.
    pub async fn next(&mut self) -> (usize, Packet) {
        tokio::time::timeout(Duration::from_secs(3), self.rx.recv()).await.expect("screen media delivery").unwrap()
    }

    /// Every packet that arrives within `window`.
    pub async fn collect(&mut self, window: Duration) -> Vec<(usize, Packet)> {
        let until = tokio::time::Instant::now() + window;
        let mut packets = Vec::new();
        while let Ok(Some(packet)) = tokio::time::timeout_at(until, self.rx.recv()).await {
            packets.push(packet);
        }
        packets
    }
}
