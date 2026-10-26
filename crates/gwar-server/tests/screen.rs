//! Screen SFU end to end (docs/screen-share.md, "The SFU"): a str0m publisher and str0m viewers
//! over real SRTP through an in-process server.

mod common;

use std::time::Duration;

use common::{
    screen::{Packet, frame},
    *,
};
use serde_json::json;

/// Frames as the viewer saw them in `slot`: (tag, packet index) of each payload, in arrival order.
fn tags(received: &[(usize, Packet)], slot: usize) -> Vec<(u8, u8)> {
    received.iter().filter(|(s, _)| *s == slot).map(|(_, p)| (p.payload[2], p.payload[3])).collect()
}

/// Sends `frames` frames of `packets` packets each, 30 ms apart, the first a keyframe when
/// `keyframe`; tags count from `tag`. Returns the next sequence number and timestamp.
async fn stream(
    publisher: &screen::Publisher,
    mut sequence: u64,
    mut timestamp: u32,
    tag: u8,
    frames: u8,
    packets: usize,
    keyframe: bool,
) -> (u64, u32) {
    for i in 0..frames {
        publisher.send(frame(sequence, timestamp, keyframe && i == 0, packets, tag + i)).await;
        sequence += packets as u64;
        timestamp += 2700;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    (sequence, timestamp)
}

#[tokio::test]
async fn optional_capability_query_keeps_legacy_clients_and_welcome_compatible() {
    let server = TestServer::start().await;
    let mut client = Client::connect(&server, "legacy").await;
    assert!(
        serde_json::to_value(&client.welcome).unwrap().get("screen_h264").is_none(),
        "legacy strict Welcome schema stays unchanged"
    );
    assert_eq!(client.request("screen.capabilities", json!({})).await.unwrap(), json!({"screen_h264":true}));
    // An unknown optional query must not retire an old server's connection.
    assert_eq!(
        client.request("screen.future_capabilities", json!({})).await.unwrap_err().code,
        gwar_proto::ErrorCode::BadRequest
    );
    assert_eq!(client.request("screen.capabilities", json!({})).await.unwrap(), json!({"screen_h264":true}));
}

#[tokio::test]
async fn a_late_viewer_starts_at_a_keyframe_and_gets_every_byte_in_order() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let share = alice.share_in(seed_channel(&alice, "General")).await;
    let (sequence, timestamp) = stream(alice.publisher(&share), 1000, 90_000, 1, 5, 3, true).await;

    let slot = bob.watch(&share).await as usize;
    let event = bob.next_event("screen.slot").await;
    assert_eq!(event, json!({"run": 1, "slot": slot, "share": share}));
    // Watching asks the sharer for a keyframe; deltas before it are not forwarded.
    let requested = tokio::time::timeout(Duration::from_secs(2), async {
        while alice.publisher(&share).keyframe_requests() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(requested.is_ok(), "a watch asks for a keyframe");
    let (sequence, timestamp) = stream(alice.publisher(&share), sequence, timestamp, 10, 2, 3, false).await;
    stream(alice.publisher(&share), sequence, timestamp, 20, 4, 5, true).await;

    let received = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(500)).await;
    let expected: Vec<_> = (20..24).flat_map(|tag| (0..5).map(move |i| (tag, i))).collect();
    assert_eq!(tags(&received, slot), expected);
    // Byte for byte, and numbered without gaps from the keyframe on.
    let packets: Vec<_> = received.iter().map(|(_, p)| p).collect();
    assert!(packets.windows(2).all(|w| w[1].sequence == w[0].sequence + 1));
    assert_eq!(packets[0].payload, screen::frame(0, 0, true, 1, 20)[0].payload);
    assert!(packets.iter().filter(|p| p.marker).count() == 4, "markers pass through");
}

#[tokio::test]
async fn publisher_loss_is_repaired_into_place_and_viewer_loss_never_reaches_the_sharer() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let share = alice.share_in(seed_channel(&alice, "General")).await;
    let slot = bob.watch(&share).await as usize;

    // The path to the server loses two packets of the third frame once; the server's NACK
    // brings them back over RTX, after their successors.
    alice.publisher(&share).loss.drop_once([2011, 2013]);
    stream(alice.publisher(&share), 2000, 0, 1, 6, 5, true).await;
    let received = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(800)).await;
    assert!(alice.publisher(&share).repairs() >= 2, "the server asked the sharer for its losses");
    let mut got = tags(&received, slot);
    assert_eq!(got.len(), 30, "every packet arrived: {got:?}");
    got.sort();
    let expected: Vec<_> = (1..7).flat_map(|tag| (0..5).map(move |i| (tag, i))).collect();
    assert_eq!(got, expected);
    // A repaired packet lands on its original number: output order matches source order.
    let mut by_number: Vec<_> = received.iter().map(|(_, p)| (p.sequence, (p.payload[2], p.payload[3]))).collect();
    by_number.sort();
    assert_eq!(by_number.iter().map(|(_, t)| *t).collect::<Vec<_>>(), expected);
    assert!(by_number.windows(2).all(|w| w[1].0 == w[0].0 + 1));

    // The viewer's own losses are repaired by the server, without a single repair or keyframe
    // from the sharer.
    let (repairs, requests) = (alice.publisher(&share).repairs(), alice.publisher(&share).keyframe_requests());
    let first = by_number.last().unwrap().0 as u16;
    bob.viewer.as_ref().unwrap().loss.drop_once([first.wrapping_add(3), first.wrapping_add(7)]);
    stream(alice.publisher(&share), 2030, 6 * 2700, 7, 4, 5, false).await;
    let received = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(800)).await;
    let mut got = tags(&received, slot);
    got.sort();
    assert_eq!(got, (7..11).flat_map(|tag| (0..5).map(move |i| (tag, i))).collect::<Vec<_>>());
    assert_eq!(alice.publisher(&share).repairs(), repairs);
    assert_eq!(alice.publisher(&share).keyframe_requests(), requests);
}

#[tokio::test]
async fn unwatching_stopping_and_losing_access_end_the_media_at_once() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let general = seed_channel(&root, "General");
    let share = alice.share_in(general).await;
    let slot = bob.watch(&share).await as usize;
    let (sequence, timestamp) = stream(alice.publisher(&share), 0, 0, 1, 2, 2, true).await;
    assert_eq!(tags(&bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await, slot).len(), 4);

    bob.ok("screen.unwatch", json!({"share": share})).await;
    let released = bob.next_event_where("screen.slot", |e| e["share"].is_null()).await;
    assert_eq!(released, json!({"run": 1, "slot": slot, "share": null}));
    let (sequence, timestamp) = stream(alice.publisher(&share), sequence, timestamp, 3, 2, 2, true).await;
    assert!(bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await.is_empty(), "unwatched");

    // Watching again starts over from a keyframe, in the same slot.
    assert_eq!(bob.watch(&share).await as usize, slot);
    let (sequence, timestamp) = stream(alice.publisher(&share), sequence, timestamp, 5, 2, 2, true).await;
    assert_eq!(tags(&bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await, slot).len(), 4);

    // Only members of a private channel see its shares: shutting bob out ends the media.
    let secret = channel_id(
        &root.create_channel(json!({"name": "secret", "private": true, "access": {"roles": [], "users": [alice.welcome.uid, bob.welcome.uid]}})).await,
    );
    let hidden = alice.screen_start().await;
    assert_eq!(hidden.unwrap_err().code, gwar_proto::ErrorCode::Conflict, "one share at a time");
    alice.ok("screen.stop", json!({"share": share})).await;
    bob.next_event_where("screen.slot", |e| e["share"].is_null()).await;
    let (sequence, timestamp) = stream(alice.publisher(&share), sequence, timestamp, 7, 2, 2, true).await;
    assert!(bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await.is_empty(), "stopped");

    let private = alice.share_in(secret).await;
    let slot = bob.watch(&private).await as usize;
    stream(alice.publisher(&private), 0, 0, 9, 2, 2, true).await;
    assert_eq!(tags(&bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await, slot).len(), 4);
    let only_alice = json!({"roles": [], "users": [alice.welcome.uid]});
    root.ok("channel.update", json!({"channel": secret, "access": only_alice})).await;
    let ended = bob.next_event_where("screen.stopped", |e| e["share"] == private).await;
    assert_eq!(ended, json!({"share": private, "reason": "access"}));
    stream(alice.publisher(&private), 4, 5400, 11, 2, 2, true).await;
    assert!(bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await.is_empty(), "shut out");
    // The old share's publisher is gone: its packets reach no one.
    stream(alice.publisher(&share), sequence, timestamp, 13, 2, 2, true).await;
    assert!(bob.viewer.as_mut().unwrap().collect(Duration::from_millis(200)).await.is_empty());
}

#[tokio::test]
async fn an_overrun_cuts_the_rest_until_a_keyframe_and_three_end_the_share() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let share = alice.share_in(seed_channel(&alice, "General")).await;
    let slot = bob.watch(&share).await as usize;
    let publisher = alice.publisher(&share);
    let (mut sequence, mut timestamp) = stream(publisher, 0, 0, 1, 1, 3, true).await;
    let before = publisher.keyframe_requests();
    let viewer = bob.viewer.as_mut().unwrap();
    for i in 0..3 {
        assert_eq!(viewer.next().await, (slot, frame(0, 0, true, 3, 1)[i].clone()));
    }

    // A frame that never ends: its continuation 2.1 s later breaks the 2 s bound.
    let overrun = |sequence: &mut u64, timestamp: &mut u32| {
        let start = frame(*sequence, *timestamp, false, 2, 40);
        let late = Packet {
            sequence: *sequence + 2,
            timestamp: *timestamp,
            marker: true,
            payload: screen::vp9(false, false, &[41, 0, 0xaa, 0x55]),
        };
        *sequence += 3;
        *timestamp += 2700;
        (start, late)
    };
    // The frame's span starts once the server has its first packets: the viewer has them too.
    let (start, late) = overrun(&mut sequence, &mut timestamp);
    publisher.send(start).await;
    assert_eq!(tags(&[viewer.next().await, viewer.next().await], slot), [(40, 0), (40, 1)], "the prefix stays");
    tokio::time::sleep(Duration::from_millis(2100)).await;
    publisher.send([late]).await;
    // Deltas after the cut drop too, until the next keyframe.
    let (next_sequence, next_timestamp) = stream(publisher, sequence, timestamp, 50, 2, 2, false).await;
    let received = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await;
    assert!(received.is_empty(), "cut: {received:?}");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(publisher.keyframe_requests() > before, "the sharer is asked for a keyframe");
    let (sequence, timestamp) = stream(publisher, next_sequence, next_timestamp, 60, 1, 2, true).await;
    let received = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await;
    assert_eq!(tags(&received, slot), [(60, 0), (60, 1)], "a keyframe resumes");

    // Two more overruns within 10 s end the share.
    let (mut sequence, mut timestamp) = (sequence, timestamp);
    for _ in 0..2 {
        publisher.send(frame(sequence, timestamp, true, 1, 70)).await;
        sequence += 1;
        timestamp += 2700;
        let (start, late) = overrun(&mut sequence, &mut timestamp);
        publisher.send(start).await;
        let viewer = bob.viewer.as_mut().unwrap();
        let got = [viewer.next().await, viewer.next().await, viewer.next().await];
        assert_eq!(tags(&got, slot), [(70, 0), (40, 0), (40, 1)]);
        tokio::time::sleep(Duration::from_millis(2100)).await;
        publisher.send([late]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(alice.next_event("screen.stopped").await, json!({"share": share, "reason": "limit"}));
    assert_eq!(bob.next_event("screen.stopped").await, json!({"share": share, "reason": "limit"}));
}

#[tokio::test]
async fn keyframe_requests_are_throttled_per_share_and_per_viewer() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let share = alice.share_in(seed_channel(&alice, "General")).await;
    bob.watch(&share).await;
    stream(alice.publisher(&share), 0, 0, 1, 1, 2, true).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let before = alice.publisher(&share).keyframe_requests();
    // A flood within one second: at most two requests reach the sharer (500 ms apart).
    for _ in 0..30 {
        bob.ok("screen.keyframe", json!({"share": share})).await;
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let flood = alice.publisher(&share).keyframe_requests() - before;
    assert!((1..=3).contains(&flood), "{flood} requests");
    // Coalesced duplicates did not exhaust the viewer's credits: distinct later
    // requests still work, while at most ten new pending PLIs a minute are funded.
    let spent = alice.publisher(&share).keyframe_requests();
    for _ in 0..3 {
        bob.ok("screen.keyframe", json!({"share": share})).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    assert!((1..=4).contains(&(alice.publisher(&share).keyframe_requests() - spent)));
    for _ in 0..12 {
        bob.ok("screen.keyframe", json!({"share": share})).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    // Includes the initial flood and the continuously refilling credit: no more
    // than the ten-credit burst plus two refills over this interval reached RTP.
    assert!(alice.publisher(&share).keyframe_requests() - before <= 12);
    // A new watch transport does not bring a new budget: watching again and asking again
    // reach the sharer at most once (what refilled meanwhile).
    let spent = alice.publisher(&share).keyframe_requests();
    bob.viewer = Some(screen::Viewer::start(&mut bob, 2).await);
    bob.ok("screen.watch", json!({"share": share})).await;
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(600)).await;
        bob.ok("screen.keyframe", json!({"share": share})).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(alice.publisher(&share).keyframe_requests() - spent <= 1, "the budget carried over");
    assert_eq!(bob.fails("screen.keyframe", json!({"share": "AAAAAAAAAAAAAAAAAAAAAA"})).await, "not_found");
}

#[tokio::test]
async fn transports_are_negotiated_strictly() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let general = seed_channel(&alice, "General");
    alice.ok("channel.join", json!({"channel": general})).await;
    let share = alice.screen_start().await.unwrap()["share"].as_str().unwrap().to_owned();

    // Not announced until its transport connects: no one else can even name it.
    assert_eq!(bob.fails("screen.watch", json!({"share": share})).await, "not_found");
    assert!(bob.drain_events("screen.started").await.is_empty());
    let audio = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=ssrc-group:FID 1 2\r\na=ssrc:1 c\r\na=ssrc:2 c\r\n";
    let offer = json!({"share": share, "ssrc": 1, "rtx_ssrc": 2, "sdp": audio});
    assert_eq!(alice.fails("screen.publish.offer", offer).await, "bad_request");
    assert_eq!(
        bob.fails("screen.publish.offer", json!({"share": share, "ssrc": 1, "rtx_ssrc": 2, "sdp": ""})).await,
        "not_found"
    );
    alice.publish(&share).await;
    assert_eq!(bob.next_event("screen.started").await["share"], share);
    // Once negotiated, never again.
    let again = json!({"share": share, "ssrc": 5, "rtx_ssrc": 6, "sdp": audio});
    assert_eq!(alice.fails("screen.publish.offer", again).await, "conflict");

    // Watching needs a connected watch transport; runs only go up.
    assert_eq!(bob.fails("screen.watch", json!({"share": share})).await, "unavailable");
    let client = &mut bob;
    let viewer = screen::Viewer::negotiate(gwar_proto::SCREEN_SLOTS, 1, move |sdp| async move {
        let answer = client.ok("screen.watch.offer", json!({"run": 1, "sdp": sdp})).await;
        assert_eq!(
            client.fails("screen.watch.offer", json!({"run": 1, "sdp": sdp, "browser": true})).await,
            "conflict",
            "a cached run cannot change its decoder policy"
        );
        answer["sdp"].as_str().unwrap().to_owned()
    })
    .await;
    bob.viewer = Some(viewer);
    bob.watch(&share).await;
    assert_eq!(bob.fails("screen.watch.offer", json!({"run": 1, "sdp": "v=0"})).await, "conflict");
    // A new run replaces the transport and ends its watches.
    bob.viewer = Some(screen::Viewer::start(&mut bob, 2).await);
    assert_eq!(alice.next_event_where("screen.viewers", |v| v["members"] == 0).await["share"], share);
    assert_eq!(bob.fails("screen.watch.offer", json!({"run": 1, "sdp": "v=0"})).await, "stale");
    assert_eq!(bob.ok("screen.watch", json!({"share": share})).await["slot"], 0);
    // A run that failed is retired with everything below it, even with no transport left.
    assert_eq!(bob.fails("screen.watch.offer", json!({"run": 9, "sdp": "v=0"})).await, "bad_request");
    assert_eq!(bob.fails("screen.watch.offer", json!({"run": 9, "sdp": "v=0"})).await, "stale");
    assert_eq!(bob.fails("screen.watch.offer", json!({"run": 3, "sdp": "v=0"})).await, "stale");
    bob.viewer = Some(screen::Viewer::start(&mut bob, 10).await);
}

/// Streams into `slot` until the viewer has lost one packet and holds its NACK back; returns
/// where the stream got to.
async fn nack_in_flight(
    viewer: &mut Client,
    publisher: &screen::Publisher,
    slot: usize,
    at: (u64, u32),
    tag: u8,
) -> (u64, u32) {
    let (sequence, timestamp) = stream(publisher, at.0, at.1, tag, 2, 2, true).await;
    let received = viewer.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await;
    let last =
        received.iter().filter(|(s, _)| *s == slot).map(|(_, p)| p.sequence).max().expect("the viewer gets media");
    let v = viewer.viewer.as_ref().unwrap();
    v.gate.close();
    v.drop_repairs.store(true, std::sync::atomic::Ordering::Relaxed);
    v.loss.drop_once([(last + 2) as u16]);
    let next = stream(publisher, sequence, timestamp, tag + 2, 2, 2, false).await;
    let received = viewer.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await;
    assert_eq!(tags(&received, slot).len(), 3, "one packet lost");
    assert!(viewer.viewer.as_ref().unwrap().gate.holding(), "its NACK waits");
    next
}

/// Once the slot ended: the held NACK reaches the server and the share goes on, but nothing of
/// it reaches the viewer, repairs included.
async fn nothing_after(viewer: &mut Client, publisher: &screen::Publisher, at: (u64, u32), tag: u8) -> (u64, u32) {
    let v = viewer.viewer.as_ref().unwrap();
    v.drop_repairs.store(false, std::sync::atomic::Ordering::Relaxed);
    v.gate.open();
    let next = stream(publisher, at.0, at.1, tag, 3, 2, true).await;
    let received = viewer.viewer.as_mut().unwrap().collect(Duration::from_millis(600)).await;
    assert!(received.is_empty(), "nothing after the slot ended: {:?}", tags(&received, 0));
    next
}

fn group_id(reply: &serde_json::Value) -> u64 {
    reply["group"]["id"].as_u64().expect("group id")
}

#[tokio::test]
async fn a_viewer_outside_voice_watches_until_the_channel_is_hidden_from_it() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    // A private channel both may see, until bob is taken off its list.
    let both = json!({"roles": [], "users": [alice.welcome.uid, bob.welcome.uid]});
    let room = channel_id(&root.create_channel(json!({"name": "Room", "private": true, "access": both})).await);
    let share = alice.share_in(room).await;
    bob.ok("channel.leave", json!({})).await;
    assert!(bob.ok("voice.state", json!({})).await["voice"].is_null(), "bob is not in voice");
    let slot = bob.watch(&share).await as usize;

    let at = nack_in_flight(&mut bob, alice.publisher(&share), slot, (0, 0), 1).await;
    let only_alice = json!({"roles": [], "users": [alice.welcome.uid]});
    root.ok("channel.update", json!({"channel": room, "access": only_alice})).await;
    let ended = bob.next_event_where("screen.stopped", |e| e["share"] == share).await;
    assert_eq!(ended["reason"], "access");
    nothing_after(&mut bob, alice.publisher(&share), at, 10).await;
}

#[tokio::test]
async fn losing_bypass_password_ends_the_media_nack_answers_included() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let keys = group_id(&root.ok("group.create", json!({"name": "Keys", "permissions": ["bypass_password"]})).await);
    let uid = bob.welcome.uid.clone();
    let roles = |groups: serde_json::Value| json!({"uid": uid, "groups": groups});
    root.ok("member.groups", roles(json!([2, keys]))).await;
    let vault = channel_id(&root.create_channel(json!({"name": "Vault", "password": "s3cret"})).await);
    alice.ok("channel.join", json!({"channel": vault, "password": "s3cret"})).await;
    let share = alice.share_here().await;
    bob.ok("channel.leave", json!({})).await;
    let override_keys =
        |deny: serde_json::Value| json!({"channel": vault, "override": {"role": keys, "allow": [], "deny": deny}});

    let mut at = (0, 0);
    for (step, cause) in ["role edit", "override", "role removal"].into_iter().enumerate() {
        let slot = bob.watch(&share).await as usize;
        at = nack_in_flight(&mut bob, alice.publisher(&share), slot, at, 20 * step as u8 + 1).await;
        match cause {
            "role edit" => drop(root.ok("group.update", json!({"group": keys, "permissions": []})).await),
            "override" => drop(root.ok("channel.override", override_keys(json!(["bypass_password"]))).await),
            _ => drop(root.ok("member.groups", roles(json!([2]))).await),
        }
        let ended = bob.next_event_where("screen.stopped", |e| e["share"] == share).await;
        assert_eq!(ended["reason"], "access", "{cause}");
        at = nothing_after(&mut bob, alice.publisher(&share), at, 20 * step as u8 + 10).await;
        // Back for the next step.
        match cause {
            "role edit" => {
                drop(root.ok("group.update", json!({"group": keys, "permissions": ["bypass_password"]})).await)
            }
            "override" => drop(root.ok("channel.override", override_keys(json!([]))).await),
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_watch_from_inside_a_password_channel_ends_with_the_viewers_way_out() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let general = seed_channel(&root, "General");
    let vault = channel_id(&root.create_channel(json!({"name": "Vault", "password": "s3cret"})).await);
    let enter = json!({"channel": vault, "password": "s3cret"});
    alice.ok("channel.join", enter.clone()).await;
    let share = alice.share_here().await;
    let bob_role = group_id(&root.ok("group.create", json!({"name": "Bob", "permissions": []})).await);
    root.ok("member.groups", json!({"uid": bob.welcome.uid, "groups": [2, bob_role]})).await;

    let mut at = (0, 0);
    // The bridge never moves our own users (a moved puppet follows its user); the server's own
    // move out, losing `connect`, stands for it.
    for (step, exit) in ["leave", "switch", "moved", "connect"].into_iter().enumerate() {
        bob.ok("channel.join", enter.clone()).await;
        let slot = bob.watch(&share).await as usize;
        at = nack_in_flight(&mut bob, alice.publisher(&share), slot, at, 20 * step as u8 + 1).await;
        let deny = |deny: serde_json::Value| json!({"channel": vault, "override": {"role": bob_role, "allow": [], "deny": deny}});
        match exit {
            "leave" => drop(bob.ok("channel.leave", json!({})).await),
            "switch" => drop(bob.ok("channel.join", json!({"channel": general})).await),
            "moved" => drop(root.ok("client.move", json!({"client": bob.id(), "channel": general})).await),
            _ => drop(root.ok("channel.override", deny(json!(["connect"]))).await),
        }
        let ended = bob.next_event_where("screen.stopped", |e| e["share"] == share).await;
        assert_eq!(ended["reason"], "access", "{exit}");
        at = nothing_after(&mut bob, alice.publisher(&share), at, 20 * step as u8 + 10).await;
    }
}

#[tokio::test]
async fn screen_limits_are_server_settings_and_the_egress_counter_reports_the_month() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let mut carol = Client::connect(&server, "carol").await;

    let egress = root.ok("server.egress", json!({})).await;
    let defaults = json!({
        "shares_per_channel": 4, "shares_per_server": 32, "viewers_per_share": 25, "share_cap_kbps": 12000,
        "budget_mbps": 300, "monthly_egress_tb": 10, "monthly_egress_stop": false,
    });
    assert_eq!(egress["limits"], defaults);
    assert!(["host", "container", "unavailable"].contains(&egress["scope"].as_str().unwrap()), "{egress}");
    let month = egress["month"].as_str().unwrap();
    assert!(month.len() == 7 && month.as_bytes()[4] == b'-', "{month}");
    assert!(egress["incomplete"].is_boolean());
    assert_eq!((egress["screen_members"].as_u64(), egress["screen_guests"].as_u64()), (Some(0), Some(0)));
    assert_eq!(alice.fails("server.egress", json!({})).await, "forbidden");

    let mut limits = defaults.clone();
    limits["budget_mbps"] = json!(0);
    assert_eq!(root.fails("server.update", json!({"screen": limits})).await, "bad_request");
    limits["budget_mbps"] = json!(300);
    limits["viewers_per_share"] = json!(1);
    root.ok("server.update", json!({"screen": limits})).await;
    assert_eq!(root.ok("server.egress", json!({})).await["limits"], limits);

    let share = alice.share_in(seed_channel(&root, "General")).await;
    bob.watch(&share).await;
    carol.viewer = Some(screen::Viewer::start(&mut carol, 1).await);
    assert_eq!(carol.fails("screen.watch", json!({"share": share})).await, "busy", "one viewer per share");

    // A share reserves its cap: a budget below it admits none.
    limits["budget_mbps"] = json!(1);
    root.ok("server.update", json!({"screen": limits})).await;
    alice.ok("screen.stop", json!({"share": share})).await;
    assert_eq!(alice.screen_start().await.unwrap_err().code, gwar_proto::ErrorCode::Busy);
}

/// Canonical H.264 IDR prefix followed by FU-A IDR data; the server never decodes it.
fn h264_frame(source: u64, timestamp: u32) -> Vec<Packet> {
    [
        vec![0x67, 0x42, 0xe0, 0x1f, 0xf4, 0x05, 0x01, 0x6c, 0x80],
        vec![0x68, 0xce, 0x06],
        vec![0x7c, 0x85, 0x88],
        vec![0x7c, 0x45, 0x84],
    ]
    .into_iter()
    .enumerate()
    .map(|(i, payload)| Packet { sequence: source + i as u64, timestamp, marker: i == 3, payload })
    .collect()
}

#[tokio::test]
async fn h264_srtp_forwarding_late_join_pli_and_mixed_codec_slots() {
    use str0m::format::Codec;
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "h264").await;
    let mut carol = Client::connect(&server, "vp9").await;
    let mut bob = Client::connect(&server, "viewer").await;
    let mut late = Client::connect(&server, "late").await;
    let channel = seed_channel(&alice, "General");
    alice.ok("channel.join", json!({"channel": channel})).await;
    carol.ok("channel.join", json!({"channel": channel})).await;
    let h264 = alice.share_here_codec(Codec::H264).await;
    let vp9 = carol.share_here_codec(Codec::Vp9).await;
    let hs = bob.watch(&h264).await as usize;
    let vs = bob.watch(&vp9).await as usize;
    alice.publisher(&h264).send(h264_frame(1000, 90000)).await;
    let vp9_packet = Packet { sequence: 700, timestamp: 30000, marker: true, payload: vec![0x08, 0, 0xaa] };
    carol.publisher(&vp9).send([vp9_packet.clone()]).await;
    let received = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(400)).await;
    let hp: Vec<_> = received.iter().filter(|(s, _)| *s == hs).map(|(_, p)| p).collect();
    assert_eq!(hp.len(), 4, "complete H.264 access unit");
    assert_eq!(hp[0].sequence, 0, "publisher sequence 1000 is rewritten");
    assert_eq!(hp[0].timestamp, 90000, "source clock is retained in the first segment");
    {
        let headers = bob.viewer.as_ref().unwrap().headers.lock().unwrap();
        let h264_headers: Vec<_> = headers.iter().filter(|(slot, _, _)| *slot == hs).collect();
        assert_eq!(h264_headers.len(), 4);
        assert!(
            h264_headers.iter().all(|(_, ssrc, pt)| *ssrc != alice.publisher(&h264).ssrc && *pt == 116),
            "publisher H.264 PT 118 and SSRC are rewritten for the viewer's PT 116 slot"
        );
    }
    assert_eq!(
        hp.iter().map(|p| &p.payload).collect::<Vec<_>>(),
        h264_frame(0, 0).iter().map(|p| &p.payload).collect::<Vec<_>>()
    );
    assert!(hp.windows(2).all(|w| w[1].sequence == w[0].sequence + 1));
    assert_eq!(received.iter().find(|(s, _)| *s == vs).unwrap().1.payload, vp9_packet.payload);

    let ls = late.watch(&h264).await as usize;
    let before = alice.publisher(&h264).keyframe_requests();
    // SPS and PPS without an IDR cannot open the late viewer's slot.
    let mut invalid = h264_frame(1004, 93000);
    invalid.truncate(2);
    invalid[1].marker = true;
    alice.publisher(&h264).send(invalid).await;
    assert!(late.viewer.as_mut().unwrap().collect(Duration::from_millis(600)).await.is_empty());
    assert!(alice.publisher(&h264).keyframe_requests() > before, "missing IDR requests a new keyframe");
    let expected = h264_frame(1006, 96000);
    alice.publisher(&h264).send(expected.clone()).await;
    let a = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await;
    let b = late.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await;
    assert_eq!(b.len(), 4);
    assert!(b.iter().all(|(s, _)| *s == ls));
    assert_eq!(
        b.iter().map(|(_, p)| &p.payload).collect::<Vec<_>>(),
        expected.iter().map(|p| &p.payload).collect::<Vec<_>>()
    );
    assert_eq!(a.iter().filter(|(s, p)| *s == hs && expected.iter().any(|e| e.payload == p.payload)).count(), 6);

    // H.264 publisher loss repairs over its RTX PT into both already-open segments.
    let repairs = alice.publisher(&h264).repairs();
    alice.publisher(&h264).loss.drop_once([1012]);
    alice.publisher(&h264).send(h264_frame(1010, 99000)).await;
    let got = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(800)).await;
    let late_got = late.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await;
    assert!(alice.publisher(&h264).repairs() > repairs, "H.264 ingress loss is repaired over RTX");
    let mut packets: Vec<_> = got.iter().filter(|(s, _)| *s == hs).map(|(_, p)| p).collect();
    packets.sort_by_key(|p| p.sequence);
    assert_eq!(packets.iter().map(|p| p.sequence).collect::<Vec<_>>(), [10, 11, 12, 13]);
    assert_eq!(
        packets.iter().map(|p| &p.payload).collect::<Vec<_>>(),
        h264_frame(0, 0).iter().map(|p| &p.payload).collect::<Vec<_>>()
    );
    assert_eq!(late_got.len(), 4);

    // Viewer-side loss is repaired on its own H.264 RTX PT, without an upstream repair.
    let repairs = alice.publisher(&h264).repairs();
    bob.viewer.as_ref().unwrap().loss.drop_once([16]);
    alice.publisher(&h264).send(h264_frame(1014, 102000)).await;
    let got = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(800)).await;
    let mut packets: Vec<_> = got.iter().filter(|(s, _)| *s == hs).map(|(_, p)| p).collect();
    packets.sort_by_key(|p| p.sequence);
    assert_eq!(packets.iter().map(|p| p.sequence).collect::<Vec<_>>(), [14, 15, 16, 17]);
    assert_eq!(alice.publisher(&h264).repairs(), repairs, "viewer NACK stays on its own leg");
}

#[tokio::test]
async fn quality_specs_require_negotiation_and_broadcast_only_to_negotiated_clients() {
    let server = TestServer::start().await;
    let mut sharer = Client::connect(&server, "new-sharer").await;
    let mut legacy = Client::connect(&server, "legacy-viewer").await;
    let mut modern = Client::connect(&server, "new-viewer").await;
    let channel = seed_channel(&sharer, "General");
    sharer.ok("channel.join", json!({"channel":channel})).await;
    let voice = sharer.ok("voice.state", json!({})).await["voice"].clone();
    let spec = json!({"mode":"smooth","height":1440,"fps":60});
    let start = json!({"call":voice["call"],"participation":voice["participation"],"preset":"text","spec":spec,"surface":"screen","guest_link":false});
    assert_eq!(
        sharer.request("screen.start", start.clone()).await.unwrap_err().code,
        gwar_proto::ErrorCode::BadRequest
    );
    assert_eq!(sharer.ok("screen.spec.capabilities", json!({})).await, json!({"screen_spec":true}));
    modern.ok("screen.spec.capabilities", json!({})).await;
    assert_eq!(legacy.ok("screen.capabilities", json!({})).await, json!({"screen_h264":true}));
    let id = sharer.ok("screen.start", start).await["share"].as_str().unwrap().to_owned();
    sharer.publish(&id).await;
    let old_event = legacy.next_event("screen.started").await;
    assert_eq!(old_event["preset"], "game", "server derives the closest legacy preset from the spec");
    assert_eq!(old_event.as_object().unwrap().len(), 5, "old strict web schema receives exactly the legacy fields");
    assert!(old_event.get("spec").is_none());
    let new_event = modern.next_event("screen.started").await;
    assert_eq!(new_event["spec"], spec);
    assert_eq!(new_event["preset"], "game");
    let late = Client::connect(&server, "late-legacy").await;
    let welcome = serde_json::to_value(&late.welcome).unwrap();
    assert_eq!(welcome["shares"][0]["preset"], "game");
    assert!(welcome["shares"][0].get("spec").is_none(), "Welcome precedes negotiation");
    sharer.ok("screen.stop", json!({"share":id})).await;
    // An old sender is normalized even when a new receiver negotiated quality specs.
    let id = legacy.share_in(channel).await;
    let event = modern.next_event("screen.started").await;
    assert_eq!(event["spec"], json!({"mode":"sharp","height":1080,"fps":15}));
    assert_eq!(event["preset"], "text");
    legacy.ok("screen.stop", json!({"share":id})).await;
}

/// Regression: connected publishers acknowledge transport but do not create RTP
/// automatically. A PLI is required to wake an idle/static third source.
#[tokio::test]
async fn review_idle_third_publisher_gets_its_first_keyframe_request() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut carol = Client::connect(&server, "carol").await;
    let mut dave = Client::connect(&server, "dave").await;
    let mut bob = Client::connect(&server, "bob").await;
    let channel = seed_channel(&alice, "General");
    let a = alice.share_in(channel).await;
    let b = carol.share_in(channel).await;
    let c = dave.share_in(channel).await;
    // Establish each receive stream before the late viewer joins, then hold media.
    alice.publisher(&a).send(frame(1, 9000, true, 1, 1)).await;
    carol.publisher(&b).send(frame(1, 9000, true, 1, 2)).await;
    dave.publisher(&c).send(frame(1, 9000, true, 1, 3)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    bob.watch(&a).await;
    bob.watch(&b).await;
    bob.watch(&c).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let counts = [
        alice.publisher(&a).keyframe_requests(),
        carol.publisher(&b).keyframe_requests(),
        dave.publisher(&c).keyframe_requests(),
    ];
    eprintln!("actual SRTP publisher PLI counts after 8s: {counts:?}");
    assert!(counts.iter().all(|count| *count > 0), "every waiting share receives an initial request");
    assert!(
        counts.iter().max().unwrap() - counts.iter().min().unwrap() <= 1,
        "PLI-ignoring publishers share retry opportunities fairly: {counts:?}"
    );
    let budget = 10 + (started.elapsed().as_secs() / 6);
    assert!(counts.iter().sum::<u64>() <= budget, "the shared viewer credit budget still holds");
}

#[tokio::test]
async fn review_spaced_request_on_an_active_slot_is_not_lost() {
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut carol = Client::connect(&server, "carol").await;
    let mut bob = Client::connect(&server, "bob").await;
    let channel = seed_channel(&alice, "General");
    let a = alice.share_in(channel).await;
    let b = carol.share_in(channel).await;
    alice.publisher(&a).send(frame(1, 9000, true, 1, 1)).await;
    carol.publisher(&b).send(frame(1, 9000, true, 1, 2)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    bob.watch(&a).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    alice.publisher(&a).send(frame(2, 12000, true, 1, 3)).await;
    bob.viewer.as_mut().unwrap().next().await;
    // The SFU segment is now active. Allow the initial per-viewer spacing to expire.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let before = alice.publisher(&a).keyframe_requests();
    bob.watch(&b).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    bob.ok("screen.keyframe", json!({"share": a})).await;
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let after = alice.publisher(&a).keyframe_requests();
    eprintln!(
        "active share PLIs before={before}, after={after}; second share PLIs={}",
        carol.publisher(&b).keyframe_requests()
    );
    assert!(after > before, "spacing dropped an active slot's recovery request instead of deferring it");
}

/// Regression for the round-2 startup repro: connected SRTP transports, fresh
/// credits and established small static streams must complete prompt startup waves.
#[tokio::test]
async fn fresh_credit_startup_for_one_to_four_publishers_takes_under_200ms() {
    use std::time::Instant;
    let limit = Duration::from_millis(200);
    for count in 1..=4 {
        for trial in 0..5 {
            let server = TestServer::start().await;
            let mut bob = Client::connect(&server, "viewer").await;
            let channel = seed_channel(&bob, "General");
            let mut sources = Vec::new();
            let mut shares = Vec::new();
            for i in 0..count {
                let mut source = Client::connect(&server, &format!("source-{i}")).await;
                let share = source.share_in(channel).await;
                let mut seed = frame(1, 9000, true, 8, 1);
                for packet in &mut seed {
                    packet.payload.resize(1000, 0x55);
                }
                source.publisher(&share).send(seed).await;
                sources.push(source);
                shares.push(share);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            bob.viewer = Some(screen::Viewer::start(&mut bob, 1).await);
            let mut granted = Vec::new();
            let mut slots = Vec::new();
            for share in &shares {
                // Start before the grant round trip: peer receipt bounds server
                // send latency too, without adding production timing probes.
                granted.push(Instant::now());
                let reply = bob.ok("screen.watch", json!({"share": share})).await;
                slots.push(reply["slot"].as_u64().unwrap() as usize);
            }
            let mut first_pli = vec![None; count];
            let mut first_key = vec![None; count];
            let end = Instant::now() + Duration::from_secs(8);
            while first_key.iter().any(Option::is_none) && Instant::now() < end {
                for (i, source) in sources.iter().enumerate() {
                    let publisher = source.publisher(&shares[i]);
                    if first_pli[i].is_none() {
                        let observed = publisher.request_times.lock().unwrap().first().copied();
                        if let Some(at) = observed {
                            first_pli[i] = Some(at.saturating_duration_since(granted[i]));
                            let mut response = frame(9, 12000, true, 8, 10 + i as u8);
                            for packet in &mut response {
                                packet.payload.resize(1000, 0x55);
                            }
                            publisher.send(response).await;
                        }
                    }
                }
                for (slot, packet) in bob.viewer.as_mut().unwrap().collect(Duration::from_millis(1)).await {
                    if packet.payload[2] >= 10 && packet.marker {
                        let i = slots.iter().position(|s| *s == slot).unwrap();
                        first_key[i].get_or_insert(Instant::now().saturating_duration_since(granted[i]));
                    }
                }
            }
            eprintln!(
                "STARTUP publishers={count} trial={trial} grant_to_pli={first_pli:?} grant_to_forwarded_keyframe={first_key:?}"
            );
            assert!(
                first_pli.iter().chain(&first_key).all(|delay| delay.is_some_and(|delay| delay < limit)),
                "fresh-credit startup exceeds 200ms: publishers={count}, trial={trial}, PLI={first_pli:?}, keyframe={first_key:?}"
            );
        }
    }
}

/// Packs `(value, width)` fields most significant bit first.
fn bits(fields: &[(u32, usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (at, bit) in
        fields.iter().flat_map(|&(value, width)| (0..width).rev().map(move |s| (value >> s) & 1)).enumerate()
    {
        if at % 8 == 0 {
            out.push(0);
        }
        *out.last_mut().unwrap() |= (bit as u8) << (7 - at % 8);
    }
    out
}

/// An AV1 keyframe in one packet: a 1920x1080 profile 0 sequence header at level 4.1 (1080p60),
/// main tier, and a shown KEY_FRAME frame OBU, both length-prefixed (W=0, N=1).
fn av1_keyframe() -> Vec<u8> {
    av1_keyframe_at(9)
}

/// A 1080p AV1 keyframe whose sequence declares `level` (main tier).
fn av1_keyframe_at(level: u32) -> Vec<u8> {
    let mut sequence = vec![0x08];
    sequence.extend(bits(&[
        (0, 3),
        (0, 1),
        (0, 1),
        (0, 1),
        (0, 1),
        (0, 5),
        (0, 12),
        (level, 5),
        (0, 1),
        (15, 4),
        (15, 4),
        (1919, 16),
        (1079, 16),
    ]));
    let frame = [0x30, 0x10, 0xaa, 0xbb];
    let mut payload = vec![0x08, sequence.len() as u8];
    payload.extend(sequence);
    payload.push(frame.len() as u8);
    payload.extend(frame);
    payload
}

// The AV1 RTP specification's L1T3 descriptor: start and end of frame 0x1234, structure attached
// (20 bytes, so the packet needs RFC 8285's two-byte header form).
const ATTACHED: [u8; 20] = [
    0xfd, 0x12, 0x34, 0x87, 0xa2, 0x14, 0xea, 0xaa, 0x44, 0x10, 0x4d, 0x14, 0x10, 0x20, 0x84, 0x27, 0x07, 0x7f, 0x04,
    0x37,
];

#[tokio::test]
async fn av1_srtp_forwarding_keeps_descriptors_and_gives_way_to_h264_for_a_viewer_without_av1() {
    use str0m::format::Codec;
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "av1").await;
    let mut bob = Client::connect(&server, "viewer").await;
    let mut dave = Client::connect(&server, "second").await;
    let mut carol = Client::connect(&server, "h264-only").await;
    assert_eq!(alice.ok("screen.av1.capabilities", json!({})).await, json!({"screen_av1": true}));
    let channel = seed_channel(&alice, "General");
    alice.ok("channel.join", json!({"channel": channel})).await;
    let share = alice.share_here_codec(Codec::Av1).await;
    let hd = json!({"width": 1920, "height": 1080});
    let bs = bob.watch_av1(&share, hd.clone()).await as usize;
    let ds = dave.watch_av1(&share, hd).await as usize;

    let keyframe = Packet { sequence: 500, timestamp: 90000, marker: true, payload: av1_keyframe() };
    // A delta frame with only the mandatory descriptor fields (one-byte header form).
    let inter =
        Packet { sequence: 501, timestamp: 93000, marker: true, payload: vec![0x00, 0x04, 0x30, 0x30, 0xaa, 0xbb] };
    alice
        .publisher(&share)
        .send_described([(keyframe.clone(), Some(ATTACHED.to_vec())), (inter.clone(), Some(vec![0xc1, 0x12, 0x35]))])
        .await;
    for (client, slot) in [(&mut bob, bs), (&mut dave, ds)] {
        let viewer = client.viewer.as_mut().unwrap();
        let received = viewer.collect(Duration::from_millis(400)).await;
        let payloads: Vec<_> = received.iter().filter(|(s, _)| *s == slot).map(|(_, p)| &p.payload).collect();
        assert_eq!(payloads, [&keyframe.payload, &inter.payload], "AV1 payloads are forwarded byte for byte");
        let headers = viewer.headers.lock().unwrap();
        assert!(headers.iter().all(|&(_, _, pt)| pt == 47), "publisher PT 41 is rewritten for the viewer's PT 47");
        let descriptors: Vec<_> = viewer.descriptors.lock().unwrap().iter().map(|(_, _, dd)| dd.clone()).collect();
        // The slot's first segment starts at the publisher's frame number, so both are unchanged.
        assert_eq!(descriptors, [Some(ATTACHED.to_vec()), Some(vec![0xc1, 0x12, 0x35])]);
    }

    // A viewer who cannot decode AV1 (no capability, no AV1 PT) turns the share to H.264 once.
    let cs = carol.watch(&share).await as usize;
    alice
        .publisher(&share)
        .send_described([(Packet { sequence: 502, timestamp: 96000, ..inter.clone() }, Some(vec![0xc1, 0x12, 0x36]))])
        .await;
    let recover = alice.next_event("screen.recover").await;
    assert_eq!(recover, json!({"share": share, "codec": "h264", "width": 1920, "height": 1080, "fps": 60}));
    let carol_got = carol.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await;
    assert!(carol_got.iter().all(|(s, _)| *s != cs), "AV1 never reaches a slot without AV1");
}

#[tokio::test]
async fn an_av1_sequence_beyond_the_sfu_limits_turns_the_share_to_h264_once() {
    use str0m::format::Codec;
    let server = TestServer::start().await;
    let mut alice = Client::connect(&server, "av1").await;
    assert_eq!(alice.ok("screen.av1.capabilities", json!({})).await, json!({"screen_av1": true}));
    let channel = seed_channel(&alice, "General");
    alice.ok("channel.join", json!({"channel": channel})).await;
    let share = alice.share_here_codec(Codec::Av1).await;
    // seq_level_idx 31 (unconstrained): never forwarded, and every keyframe repeats it.
    let keyframe = |sequence, timestamp| Packet { sequence, timestamp, marker: true, payload: av1_keyframe_at(31) };
    alice.publisher(&share).send_described([(keyframe(500, 90000), Some(ATTACHED.to_vec()))]).await;
    let recover = alice.next_event("screen.recover").await;
    assert_eq!(recover, json!({"share": share, "codec": "h264", "width": 1920, "height": 1080, "fps": 60}));
    // Further refused keyframes from the old transport ask for nothing more.
    alice.publisher(&share).send_described([(keyframe(501, 93000), Some(ATTACHED.to_vec()))]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(alice.drain_events("screen.recover").await.is_empty());
}
