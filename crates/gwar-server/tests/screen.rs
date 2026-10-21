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
    assert_eq!(packets[0].payload, screen::vp8(true, true, &[20, 0, 0xaa, 0x55]));
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

    // A frame that never ends: its continuation 600 ms later breaks the 500 ms bound.
    let overrun = |sequence: &mut u64, timestamp: &mut u32| {
        let start = frame(*sequence, *timestamp, false, 2, 40);
        let late = Packet {
            sequence: *sequence + 2,
            timestamp: *timestamp,
            marker: true,
            payload: screen::vp8(false, false, &[41, 0, 0xaa, 0x55]),
        };
        *sequence += 3;
        *timestamp += 2700;
        (start, late)
    };
    let (start, late) = overrun(&mut sequence, &mut timestamp);
    publisher.send(start).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    publisher.send([late]).await;
    // Deltas after the cut drop too, until the next keyframe.
    let (next_sequence, next_timestamp) = stream(publisher, sequence, timestamp, 50, 2, 2, false).await;
    let received = bob.viewer.as_mut().unwrap().collect(Duration::from_millis(300)).await;
    assert_eq!(tags(&received, slot), [(1, 0), (1, 1), (1, 2), (40, 0), (40, 1)], "the prefix stays");
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
        tokio::time::sleep(Duration::from_millis(600)).await;
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
    // The viewer's budget (10 a minute, the watch used one) is spent: later requests do nothing.
    let spent = alice.publisher(&share).keyframe_requests();
    for _ in 0..3 {
        bob.ok("screen.keyframe", json!({"share": share})).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    assert!(alice.publisher(&share).keyframe_requests() - spent <= 2);
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
    bob.watch(&share).await;
    assert_eq!(bob.fails("screen.watch.offer", json!({"run": 1, "sdp": "v=0"})).await, "conflict");
    // A new run replaces the transport and ends its watches.
    bob.viewer = Some(screen::Viewer::start(&mut bob, 2).await);
    assert_eq!(alice.next_event_where("screen.viewers", |v| v["members"] == 0).await["share"], share);
    assert_eq!(bob.fails("screen.watch.offer", json!({"run": 1, "sdp": "v=0"})).await, "stale");
    assert_eq!(bob.ok("screen.watch", json!({"share": share})).await["slot"], 0);
}
