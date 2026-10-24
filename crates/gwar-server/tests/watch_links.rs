//! TeamSpeak watch links end to end (docs/screen-share.md, "TeamSpeak watch links"): the link a
//! share gets, guests on `/ws/watch` watching through it over real SRTP, and everything that ends
//! it. A channel stands in for the TeamSpeak bridge, so no TeamSpeak server runs.

mod common;

use std::{collections::VecDeque, time::Duration};

use common::{
    screen::{Viewer, frame},
    *,
};
use gwar_server::core::bridge::{BridgeMsg, BridgeNote};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error, Message, client::IntoClientRequest},
};

/// A server with a public URL whose clients' addresses come from `x-real-ip`, and a stand-in for
/// the TeamSpeak bridge.
async fn linked_server(
    configure: impl FnOnce(&mut gwar_server::Config),
) -> (TestServer, UnboundedReceiver<BridgeNote>) {
    let server = TestServer::start_with(|c| {
        c.public_url = Some("https://watch.test".into());
        c.client_ip_header = Some("x-real-ip".into());
        configure(c);
    })
    .await;
    let (tx, mut notes) = tokio::sync::mpsc::unbounded_channel();
    server.running.core.bridge(BridgeMsg::Attach(tx)).await;
    assert!(matches!(notes.recv().await, Some(BridgeNote::Snapshot { .. })));
    (server, notes)
}

/// The next watch link the bridge is given: (share, token).
async fn next_link(notes: &mut UnboundedReceiver<BridgeNote>) -> (String, String) {
    within("a watch link for the bridge", async {
        loop {
            if let Some(BridgeNote::WatchLink { share, url, .. }) = notes.recv().await {
                let base = "https://gwar.app/watch?server=wss%3A%2F%2Fwatch.test%2Fws#";
                let token = url.0.strip_prefix(base).unwrap_or_else(|| panic!("a link on the web app: {url:?}"));
                return (share, token.to_owned());
            }
        }
    })
    .await
}

/// Every note the bridge has been given so far.
fn pending(notes: &mut UnboundedReceiver<BridgeNote>) -> Vec<BridgeNote> {
    std::iter::from_fn(|| notes.try_recv().ok()).collect()
}

fn is_link(note: &BridgeNote) -> bool {
    matches!(note, BridgeNote::WatchLink { .. })
}

fn is_gone(note: &BridgeNote, id: &str) -> bool {
    matches!(note, BridgeNote::WatchLinkGone { share } if share == id)
}

/// Joins `channel` and shares there with a watch link asked for; the share once it is live.
async fn share_linked(client: &mut Client, channel: u32) -> String {
    client.ok("channel.join", json!({"channel": channel})).await;
    let share = client.screen_start_linked(true).await.expect("screen.start");
    let share = share["share"].as_str().unwrap().to_owned();
    client.publish(&share).await;
    share
}

/// A guest connection on `/ws/watch`, from `ip`.
struct Guest {
    ws: Ws,
    next: u32,
    events: VecDeque<Value>,
}

impl Guest {
    async fn open(server: &TestServer, ip: &str) -> Result<Self, u16> {
        let mut request = format!("ws://{}/ws/watch", server.running.http).into_client_request().unwrap();
        request.headers_mut().insert("x-real-ip", ip.parse().unwrap());
        match within("guest connect", connect_async(request)).await {
            Ok((ws, _)) => Ok(Self { ws, next: 1, events: VecDeque::new() }),
            Err(Error::Http(response)) => Err(response.status().as_u16()),
            Err(e) => panic!("guest connect: {e}"),
        }
    }

    async fn connect(server: &TestServer, ip: &str) -> Self {
        Self::open(server, ip).await.expect("guest connection")
    }

    async fn send(&mut self, op: &str, d: Value) -> u32 {
        let id = self.next;
        self.next += 1;
        self.ws.send_frame(json!({"id": id, "op": op, "d": d})).await;
        id
    }

    /// The reply to a request: `Ok(ok)` or `Err(err)`. Events meanwhile are kept.
    async fn request(&mut self, op: &str, d: Value) -> Result<Value, Value> {
        let id = self.send(op, d).await;
        loop {
            let frame = recv(&mut self.ws).await.unwrap_or_else(|| panic!("closed before the reply to {op}"));
            if frame["re"] == id {
                return match frame.get("ok") {
                    Some(ok) => Ok(ok.clone()),
                    None => Err(frame["err"].clone()),
                };
            }
            self.events.push_back(frame);
        }
    }

    async fn watch(&mut self, token: &str) -> Result<Value, Value> {
        self.request("guest.watch", json!({"token": token})).await
    }

    /// Connects the guest's one-slot watch transport.
    async fn offer(&mut self) -> Viewer {
        Viewer::negotiate(1, 1, |sdp| async move {
            let answer = self.request("guest.offer", json!({"sdp": sdp})).await.expect("guest.offer");
            answer["sdp"].as_str().unwrap().to_owned()
        })
        .await
    }

    async fn next_event(&mut self) -> Value {
        if let Some(event) = self.events.pop_front() {
            return event;
        }
        recv(&mut self.ws).await.expect("an event before the connection closes")
    }

    /// The connection closes with nothing more said.
    async fn closed(&mut self) {
        assert!(self.events.is_empty(), "unread: {:?}", self.events);
        if let Some(frame) = recv(&mut self.ws).await {
            panic!("expected the connection to close, got {frame}");
        }
    }
}

trait SendFrame {
    async fn send_frame(&mut self, frame: Value);
}

impl SendFrame for Ws {
    async fn send_frame(&mut self, frame: Value) {
        use futures::SinkExt;
        // A connection the server just closed may refuse it; the reads say what happened.
        let _ = self.send(Message::text(frame.to_string())).await;
    }
}

/// Sends `frames` frames of 3 packets, 30 ms apart, the first a keyframe.
async fn stream(publisher: &screen::Publisher, sequence: u64, frames: u8) {
    for i in 0..frames {
        let first = sequence + u64::from(i) * 3;
        publisher.send(frame(first, u32::from(i) * 2700, i == 0, 3, i)).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
async fn a_guest_watches_a_share_through_its_link_until_it_ends() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    let share = share_linked(&mut alice, general).await;
    let (linked, token) = next_link(&mut notes).await;
    assert_eq!(linked, share);
    assert_eq!(token.len(), 43, "32 random bytes in base64url");
    assert_eq!(alice.next_event("screen.link").await, json!({"share": share, "active": true}));

    let mut guest = Guest::connect(&server, "198.51.100.1").await;
    let welcome = guest.watch(&token).await.expect("guest.watch");
    assert_eq!(
        welcome,
        json!({"share": share, "sharer": "alice", "preset": "text", "ice_servers": [], "screen_h264": true})
    );
    // The legacy guest validates only these fields and ignores extensions. It still
    // sends the unextended guest.offer below and receives the share through its slot.
    #[derive(serde::Deserialize, serde::Serialize)]
    struct LegacyWatching {
        share: String,
        sharer: String,
        preset: String,
        ice_servers: Vec<Value>,
    }
    let legacy: LegacyWatching = serde_json::from_value(welcome).expect("legacy guest ignores screen_h264");
    assert_eq!(
        serde_json::to_value(legacy).unwrap(),
        json!({"share": share, "sharer": "alice", "preset": "text", "ice_servers": []})
    );
    assert_eq!(alice.next_event("screen.viewers").await, json!({"share": share, "members": 0, "guests": 1}));

    let mut viewer = guest.offer().await;
    // The slot opens at a keyframe, which the watch asks the sharer for.
    within("a keyframe request for the guest", async {
        while alice.publisher(&share).keyframe_requests() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    stream(alice.publisher(&share), 100, 4).await;
    let received = viewer.collect(Duration::from_millis(500)).await;
    assert_eq!(received.len(), 12, "every packet, in the guest's one slot");
    assert!(received.iter().all(|(slot, _)| *slot == 0));
    assert_eq!(guest.request("screen.keyframe", json!({"share": share})).await, Ok(json!({})));
    assert_eq!(guest.request("ping", json!({})).await, Ok(json!({})));

    alice.ok("screen.stop", json!({"share": share})).await;
    // No name of who stopped it, then the socket closes.
    assert_eq!(guest.next_event().await, json!({"ev": "screen.stopped", "d": {"share": share, "reason": "stopped"}}));
    guest.closed().await;
    assert!(pending(&mut notes).iter().any(|n| is_gone(n, &share)), "the bridge forgets the link");
    let mut late = Guest::connect(&server, "198.51.100.2").await;
    assert_eq!(late.watch(&token).await.unwrap_err()["code"], "not_found");
}

#[tokio::test]
async fn private_links_are_presence_bound_at_the_guest_gateway_and_end_with_the_share() {
    use gwar_server::media::screen::Lease;
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    let share = share_linked(&mut alice, general).await;
    let (shared, links) = within("private token registry for the announced share", async {
        loop {
            if let Some(BridgeNote::WatchLink { url, private_links, .. }) = notes.recv().await {
                break (url, private_links);
            }
        }
    })
    .await;
    let membership = Lease::new();
    let (url, receipt) = links.mint("recipient", membership.clone()).unwrap();
    let token = url.0.rsplit_once('#').unwrap().1;
    assert_ne!(url, shared);
    let miss = json!({"code": "not_found", "message": "this link does not work"});
    let mut pending = Guest::connect(&server, "198.51.100.10").await;
    assert_eq!(pending.watch(token).await.unwrap_err(), miss);
    pending.closed().await;
    receipt.acknowledge();
    let mut present = Guest::connect(&server, "198.51.100.1").await;
    assert_eq!(present.watch(token).await.unwrap()["share"], share);
    assert_eq!(alice.next_event("screen.viewers").await, json!({"share": share, "members": 0, "guests": 1}));
    let mut viewer = present.offer().await;
    within("a keyframe request for the private guest", async {
        while alice.publisher(&share).keyframe_requests() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    stream(alice.publisher(&share), 100, 4).await;
    assert_eq!(viewer.collect(Duration::from_millis(500)).await.len(), 12);
    membership.end();
    // Write fresh publisher media immediately: do not wait for socket/admission teardown.
    stream(alice.publisher(&share), 200, 4).await;
    assert!(viewer.collect(Duration::from_millis(500)).await.is_empty(), "no RTP after membership revocation");
    present.closed().await;
    assert_eq!(alice.next_event("screen.viewers").await, json!({"share": share, "members": 0, "guests": 0}));
    let mut departed = Guest::connect(&server, "198.51.100.2").await;
    assert_eq!(
        departed.watch(token).await.unwrap_err(),
        json!({"code": "not_found", "message": "this link does not work"})
    );
    departed.closed().await;
    let (returned, returned_receipt) = links.mint("recipient", Lease::new()).unwrap();
    let fresh = returned.0.rsplit_once('#').unwrap().1;
    returned_receipt.acknowledge();
    let mut present = Guest::connect(&server, "198.51.100.3").await;
    assert_eq!(present.watch(fresh).await.unwrap()["share"], share);
    let requests = alice.publisher(&share).keyframe_requests();
    let mut returned_viewer = present.offer().await;
    within("a keyframe request for the returning guest", async {
        while alice.publisher(&share).keyframe_requests() <= requests {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    stream(alice.publisher(&share), 300, 4).await;
    assert_eq!(returned_viewer.collect(Duration::from_millis(500)).await.len(), 12, "the publisher remains active");
    assert!(viewer.collect(Duration::from_millis(500)).await.is_empty(), "revoked transport receives no RTP");
    alice.ok("screen.stop", json!({"share": share})).await;
    assert_eq!(present.next_event().await, json!({"ev": "screen.stopped", "d": {"share": share, "reason": "stopped"}}));
    present.closed().await;
    assert!(links.mint("recipient", Lease::new()).is_none());
    for (i, token) in [token, fresh, shared.0.rsplit_once('#').unwrap().1].into_iter().enumerate() {
        let mut ended = Guest::connect(&server, &format!("198.51.100.{}", i + 10)).await;
        assert_eq!(ended.watch(token).await.unwrap_err()["code"], "not_found");
        ended.closed().await;
    }
}

#[tokio::test]
async fn unknown_ended_and_turned_off_tokens_answer_the_same_not_found() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");

    let ended = share_linked(&mut alice, general).await;
    let (_, ended_token) = next_link(&mut notes).await;
    alice.ok("screen.stop", json!({"share": ended})).await;

    alice.ok("channel.leave", json!({})).await;
    let revoked = share_linked(&mut alice, general).await;
    let (_, revoked_token) = next_link(&mut notes).await;
    alice.ok("screen.link.revoke", json!({"share": revoked})).await;

    let unknown = "A".repeat(43);
    let mut refusals = Vec::new();
    for (i, token) in [unknown.as_str(), &ended_token, &revoked_token, &"x".repeat(5000)].into_iter().enumerate() {
        let mut guest = Guest::connect(&server, &format!("198.51.100.{}", i + 1)).await;
        refusals.push(guest.watch(token).await.unwrap_err());
        guest.closed().await;
    }
    assert_eq!(refusals[0], json!({"code": "not_found", "message": "this link does not work"}));
    assert!(refusals.iter().all(|r| *r == refusals[0]), "{refusals:?}");
}

#[tokio::test]
async fn turning_the_link_off_disconnects_every_guest_at_once() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    let share = share_linked(&mut alice, general).await;
    let (_, token) = next_link(&mut notes).await;
    let mut watching = Guest::connect(&server, "198.51.100.1").await;
    watching.watch(&token).await.unwrap();
    let _viewer = watching.offer().await;
    let mut waiting = Guest::connect(&server, "198.51.100.2").await;
    waiting.watch(&token).await.unwrap();
    alice.next_event_where("screen.viewers", |v| v["guests"] == 2).await;
    // The channel's moderators heard of the link once the share was announced.
    root.next_event_where("screen.link", |l| l["share"] == share && l["active"] == true).await;

    root.ok("screen.link.revoke", json!({"share": share})).await;
    for guest in [&mut watching, &mut waiting] {
        assert_eq!(guest.next_event().await, json!({"ev": "screen.link", "d": {"share": share, "active": false}}));
        guest.closed().await;
    }
    alice.next_event_where("screen.link", |l| l["active"] == false).await;
    root.next_event_where("screen.link", |l| l["active"] == false).await;
    alice.next_event_where("screen.viewers", |v| v["guests"] == 0).await;
    assert!(pending(&mut notes).iter().any(|n| is_gone(n, &share)), "the bridge forgets the link");
    // The share goes on; the link is off for good.
    let mut again = Guest::connect(&server, "198.51.100.3").await;
    assert_eq!(again.watch(&token).await.unwrap_err()["code"], "not_found");
    alice.drain_events("screen.link").await;
    alice.ok("screen.link.revoke", json!({"share": share})).await;
    assert!(alice.drain_events("screen.link").await.is_empty(), "off once, told once");
}

#[tokio::test]
async fn guests_can_send_only_the_watch_ops() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    let share = share_linked(&mut alice, general).await;
    let (_, token) = next_link(&mut notes).await;

    // Before `guest.watch`: anything else is refused and the connection closes.
    for (i, (op, d)) in
        [("hello", json!({})), ("screen.watch", json!({"share": share})), ("ping", json!({}))].into_iter().enumerate()
    {
        let mut guest = Guest::connect(&server, &format!("198.51.100.{}", 10 + i)).await;
        assert!(guest.request(op, d).await.is_err(), "{op} first");
        guest.closed().await;
    }
    // After it: a member's op, or a second watch, closes the connection unanswered.
    let attempts = [
        ("chat.send", json!({"target": {"channel": 1}, "text": "hi"})),
        ("screen.watch", json!({"share": share})),
        ("screen.watch.offer", json!({"run": 1, "sdp": "v=0"})),
        ("channel.join", json!({"channel": 1})),
        ("guest.watch", json!({"token": token})),
    ];
    for (i, (op, d)) in attempts.into_iter().enumerate() {
        let mut guest = Guest::connect(&server, &format!("198.51.100.{}", 20 + i)).await;
        guest.watch(&token).await.unwrap();
        guest.send(op, d).await;
        guest.closed().await;
    }
    // One offer per connection: a second one closes it, and its watch ends.
    let mut guest = Guest::connect(&server, "198.51.100.30").await;
    guest.watch(&token).await.unwrap();
    let _viewer = guest.offer().await;
    guest.send("guest.offer", json!({"sdp": "v=0"})).await;
    guest.closed().await;
    alice.sync().await;
    let counts = alice.drain_events("screen.viewers").await;
    assert_eq!(counts.last().unwrap()["guests"], 0, "every guest is gone: {counts:?}");
}

#[tokio::test]
async fn watch_connections_are_held_to_the_preauth_limits_by_subscriber() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    share_linked(&mut alice, general).await;
    let (_, token) = next_link(&mut notes).await;
    // Three guests from one IPv6 /64, whatever their addresses in it.
    let mut held = Vec::new();
    for host in ["2001:db8:7:1::1", "2001:db8:7:1::2", "2001:db8:7:1:abcd::9"] {
        let mut guest = Guest::connect(&server, host).await;
        guest.watch(&token).await.unwrap();
        held.push(guest);
    }
    assert_eq!(Guest::open(&server, "2001:db8:7:1::77").await.err(), Some(429), "a fourth from the same /64");
    assert!(Guest::open(&server, "2001:db8:7:2::1").await.is_ok(), "the next /64 is another subscriber");
    // IPv4 alike; an IPv4-mapped address is the same subscriber.
    for _ in 0..3 {
        held.push(Guest::connect(&server, "192.0.2.50").await);
    }
    assert_eq!(Guest::open(&server, "::ffff:192.0.2.50").await.err(), Some(429));
    // A guest who leaves makes room.
    held.pop();
    within("the slot to free", async {
        while Guest::open(&server, "192.0.2.50").await.is_err() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn guests_that_stop_reading_free_their_places_once_let_go() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    let share = share_linked(&mut alice, general).await;
    let (_, token) = next_link(&mut notes).await;
    // Three guests from one subscriber that never read again once admitted: no frame, no reply
    // to the server's close.
    let mut silent = Vec::new();
    for _ in 0..3 {
        let mut guest = Guest::connect(&server, "192.0.2.80").await;
        guest.watch(&token).await.unwrap();
        silent.push(guest);
    }
    assert_eq!(Guest::open(&server, "192.0.2.80").await.err(), Some(429));
    alice.ok("screen.link.revoke", json!({"share": share})).await;
    alice.next_event_where("screen.viewers", |v| v["guests"] == 0).await;
    // Their connections end within the flush bound, well before any offer or idle deadline,
    // and the subscriber may connect again.
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(8), async {
        while Guest::open(&server, "192.0.2.80").await.is_err() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the silent guests' places to free");
    assert!(started.elapsed() < Duration::from_secs(8));
    drop(silent);
}

#[tokio::test]
async fn a_second_watch_or_offer_closes_the_connection_even_past_the_rate_limit() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    share_linked(&mut alice, general).await;
    let (_, token) = next_link(&mut notes).await;
    for (i, offer_first) in [false, true].into_iter().enumerate() {
        let mut guest = Guest::connect(&server, &format!("198.51.100.{}", 60 + i)).await;
        guest.watch(&token).await.unwrap();
        let _viewer = if offer_first { Some(guest.offer().await) } else { None };
        // Empty the bucket: the next frames are past the rate limit.
        for _ in 0..45 {
            guest.send("ping", json!({})).await;
        }
        if offer_first {
            guest.send("guest.offer", json!({"sdp": "v=0"})).await;
        } else {
            guest.send("guest.watch", json!({"token": token})).await;
        }
        // Answers to the pings, and refusals, may come first; then the connection closes.
        within("the connection to close", async {
            while let Some(frame) = recv(&mut guest.ws).await {
                let ok = frame.get("ok").is_some() || frame["err"]["code"] == "rate_limited";
                assert!(ok, "unexpected {frame}");
            }
        })
        .await;
    }
}

#[tokio::test]
async fn ten_guests_per_share_at_most() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    share_linked(&mut alice, general).await;
    let (_, token) = next_link(&mut notes).await;
    let mut guests = Vec::new();
    for i in 0..10 {
        let mut guest = Guest::connect(&server, &format!("203.0.113.{i}")).await;
        guest.watch(&token).await.unwrap();
        guests.push(guest);
    }
    let mut eleventh = Guest::connect(&server, "203.0.113.100").await;
    let refused = eleventh.watch(&token).await.unwrap_err();
    assert_eq!(refused, json!({"code": "busy", "message": "this share has as many guests as allowed"}));
    eleventh.closed().await;
    drop(guests.pop());
    alice.next_event_where("screen.viewers", |v| v["guests"] == 9).await;
    let mut next = Guest::connect(&server, "203.0.113.101").await;
    assert!(next.watch(&token).await.is_ok(), "a guest leaving makes room");
}

#[tokio::test]
async fn no_link_without_the_checkbox_and_none_before_the_announcement() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    alice.ok("channel.join", json!({"channel": general})).await;

    // Checkbox off: no token, no post, nothing to tell.
    let share = alice.screen_start_linked(false).await.unwrap()["share"].as_str().unwrap().to_owned();
    alice.publish(&share).await;
    alice.ok("screen.stop", json!({"share": share})).await;
    alice.sync().await;
    assert!(!pending(&mut notes).iter().any(is_link), "no link without the checkbox");
    assert!(alice.drain_events("screen.link").await.is_empty());

    // Checkbox on: nothing until the share is announced, then one post.
    let share = alice.screen_start_linked(true).await.unwrap()["share"].as_str().unwrap().to_owned();
    alice.sync().await;
    assert!(!pending(&mut notes).iter().any(is_link), "no link before the share is announced");
    alice.publish(&share).await;
    let (linked, _) = next_link(&mut notes).await;
    assert_eq!(linked, share);
}

#[tokio::test]
async fn settings_channels_and_the_bridge_rule_links_out() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");

    // Members-only files: the working link stops, and its guests go.
    let share = share_linked(&mut alice, general).await;
    let (_, token) = next_link(&mut notes).await;
    let mut guest = Guest::connect(&server, "198.51.100.1").await;
    guest.watch(&token).await.unwrap();
    root.ok("server.update", json!({"file_access": "members"})).await;
    assert_eq!(guest.next_event().await["ev"], "screen.link");
    guest.closed().await;
    alice.next_event_where("screen.link", |l| l["active"] == false).await;
    alice.ok("screen.stop", json!({"share": share})).await;
    alice.ok("channel.leave", json!({})).await;
    // While files are members-only, no link at all.
    let share = share_linked(&mut alice, general).await;
    alice.ok("screen.stop", json!({"share": share})).await;
    alice.ok("channel.leave", json!({})).await;
    root.ok("server.update", json!({"file_access": "link", "screen_guest_links": false})).await;
    // Watch links turned off on the server: none.
    let share = share_linked(&mut alice, general).await;
    alice.ok("screen.stop", json!({"share": share})).await;
    alice.ok("channel.leave", json!({})).await;
    alice.sync().await;
    assert!(!pending(&mut notes).iter().any(is_link), "no link where one is not possible");
}

#[tokio::test]
async fn private_channels_and_a_detached_bridge_have_no_link() {
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    // A private channel is not bridged: none.
    let secret = channel_id(
        &root
            .create_channel(
                json!({"name": "secret", "private": true, "access": {"roles": [], "users": [alice.welcome.uid]}}),
            )
            .await,
    );
    let share = share_linked(&mut alice, secret).await;
    alice.ok("screen.stop", json!({"share": share})).await;
    alice.ok("channel.leave", json!({})).await;
    alice.sync().await;
    assert!(!pending(&mut notes).iter().any(is_link), "no link where one is not possible");

    // The bridge detaching takes the link with it.
    share_linked(&mut alice, general).await;
    let (_, token) = next_link(&mut notes).await;
    let mut guest = Guest::connect(&server, "198.51.100.2").await;
    guest.watch(&token).await.unwrap();
    drop(notes);
    server.running.core.bridge(BridgeMsg::Detached).await;
    assert_eq!(guest.next_event().await["ev"], "screen.link");
    guest.closed().await;
    alice.next_event_where("screen.link", |l| l["active"] == false).await;
    let mut late = Guest::connect(&server, "198.51.100.3").await;
    assert_eq!(late.watch(&token).await.unwrap_err()["code"], "not_found");
}

/// The heartbeat (docs/screen-share.md, "Guest sessions"): WebSocket pings do not keep a guest
/// connected, the `ping` request every 20 s does. Takes `IDLE_TIMEOUT` and a little more.
#[tokio::test]
async fn a_guest_that_stops_pinging_is_closed_after_the_idle_timeout() {
    use futures::StreamExt;
    let (server, mut notes) = linked_server(|_| {}).await;
    let mut alice = Client::connect(&server, "alice").await;
    let general = seed_channel(&alice, "General");
    share_linked(&mut alice, general).await;
    let (_, token) = next_link(&mut notes).await;
    let mut pinging = Guest::connect(&server, "198.51.100.2").await;
    pinging.watch(&token).await.unwrap();
    let _pinging_viewer = pinging.offer().await;
    let mut silent = Guest::connect(&server, "198.51.100.1").await;
    silent.watch(&token).await.unwrap();
    let _silent_viewer = silent.offer().await;
    let started = tokio::time::Instant::now();
    let watcher = tokio::spawn(async move {
        // Reading answers the server's WebSocket pings, which must not count.
        let closed = tokio::time::timeout(Duration::from_secs(70), async {
            while let Some(Ok(message)) = silent.ws.next().await {
                assert!(!message.is_text(), "nothing to say to a silent guest: {message:?}");
                if message.is_close() {
                    break;
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "the silent guest was not closed");
        started.elapsed()
    });
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(pinging.request("ping", json!({})).await, Ok(json!({})), "the pinging guest stays");
        // The sharer keeps its own connection alive the same way.
        alice.sync().await;
    }
    let closed_after = watcher.await.unwrap();
    assert!(
        (Duration::from_secs(59)..Duration::from_secs(65)).contains(&closed_after),
        "closed after {closed_after:?}"
    );
}

#[test]
fn links_open_this_servers_page_or_the_web_app_with_the_server() {
    use gwar_server::watch_base;
    assert_eq!(
        watch_base(Some("https://voice.example.com/"), true).as_deref(),
        Some("https://voice.example.com/watch#")
    );
    assert_eq!(
        watch_base(Some("https://voice.example.com:8443"), false).as_deref(),
        Some("https://gwar.app/watch?server=wss%3A%2F%2Fvoice.example.com%3A8443%2Fws#")
    );
    assert_eq!(watch_base(None, true), None, "no public URL, no links");
}

#[tokio::test]
async fn the_watch_page_is_served_without_referrers_or_framing() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("index.html"), "app").unwrap();
    std::fs::write(root.path().join("watch.html"), "watch page").unwrap();
    let dir = root.path().to_owned();
    let server = TestServer::start_with(move |c| c.web_root = Some(dir)).await;
    for path in ["/watch", "/watch.html"] {
        let response = reqwest::get(format!("{}{path}", server.http())).await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        let headers = response.headers();
        assert_eq!(headers["referrer-policy"], "no-referrer", "{path}");
        // No other site frames it.
        assert_eq!(headers["content-security-policy"], "frame-ancestors 'none'", "{path}");
        assert_eq!(headers["x-frame-options"], "DENY", "{path}");
        assert_eq!(headers["cache-control"], "no-cache", "{path}");
        assert_eq!(response.text().await.unwrap(), "watch page");
    }
}
