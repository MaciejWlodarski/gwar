mod common;
use common::{
    screen::{self, frame},
    *,
};
use serde_json::json;
use std::time::{Duration, Instant};

#[tokio::test]
async fn growing_initial_keyframes_survive_real_srtp_admission() {
    for trial in 0..5 {
        let server = TestServer::start().await;
        let mut viewer = Client::connect(&server, "viewer").await;
        let channel = seed_channel(&viewer, "General");
        let mut sources = Vec::new();
        let mut shares = Vec::new();
        for i in 0..4 {
            let mut source = Client::connect(&server, &format!("source-{i}")).await;
            let share = source.share_in(channel).await;
            let mut seed = frame(1, 9000, true, 20, 1);
            for p in &mut seed {
                p.payload.resize(1000, 0x55);
            }
            source.publisher(&share).send(seed).await;
            sources.push(source);
            shares.push(share);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        viewer.viewer = Some(screen::Viewer::start(&mut viewer, 1).await);
        let start = Instant::now();
        for share in &shares {
            viewer.ok("screen.watch", json!({"share": share})).await;
        }
        let mut answered = [false; 4];
        let mut counts = [0usize; 4];
        let end = start + Duration::from_secs(3);
        while Instant::now() < end && counts != [80; 4] {
            for (i, source) in sources.iter().enumerate() {
                let publisher = source.publisher(&shares[i]);
                if !answered[i] && publisher.keyframe_requests() > 0 {
                    let mut response = frame(21, 12000, true, 80, 10 + i as u8);
                    for p in &mut response {
                        p.payload.resize(1000, 0x55);
                    }
                    publisher.send(response).await;
                    answered[i] = true;
                }
            }
            for (_, packet) in viewer.viewer.as_mut().unwrap().collect(Duration::from_millis(1)).await {
                if (10..14).contains(&packet.payload[2]) {
                    counts[(packet.payload[2] - 10) as usize] += 1;
                }
            }
        }
        let pli: Vec<_> = sources
            .iter()
            .enumerate()
            .map(|(i, s)| {
                s.publisher(&shares[i])
                    .request_times
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|at| at.saturating_duration_since(start))
                    .collect::<Vec<_>>()
            })
            .collect();
        eprintln!("REAL_BURST trial={trial} received_packets={counts:?} elapsed={:?} pli={pli:?}", start.elapsed());
        assert_eq!(counts, [80; 4], "every first response must arrive without relying on a retry");
    }
}

/// 200-packet (about 208 KiB on the wire) keyframes represent large 1440p
/// responses. Exercise both an unmeasured and an already measured source with
/// real cold-estimator feedback and budgeted retries, without synthetic BWE.
#[tokio::test]
async fn four_unknown_or_large_sources_deliver_within_30s_over_srtp() {
    use std::collections::{HashMap, HashSet};
    for known in [false, true] {
        let server = TestServer::start().await;
        let mut viewer = Client::connect(&server, "viewer").await;
        let channel = seed_channel(&viewer, "General");
        let mut sources = Vec::new();
        let mut shares = Vec::new();
        for i in 0..4 {
            let mut source = Client::connect(&server, &format!("source-{i}")).await;
            let share = source.share_in(channel).await;
            let mut seed = frame(1, 9000, known, if known { 200 } else { 1 }, 1);
            for p in &mut seed {
                p.payload.resize(if known { 1000 } else { 6 }, 0x55);
            }
            source.publisher(&share).send(seed).await;
            sources.push(source);
            shares.push(share);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        viewer.viewer = Some(screen::Viewer::start(&mut viewer, 1).await);
        let start = Instant::now();
        for share in &shares {
            viewer.ok("screen.watch", json!({"share": share})).await;
        }
        let mut answered = [0u64; 4];
        let mut complete = [None; 4];
        let mut frames = HashMap::<(usize, u32), HashSet<u8>>::new();
        while start.elapsed() < Duration::from_secs(30) && complete.iter().any(Option::is_none) {
            for (i, source) in sources.iter().enumerate() {
                let publisher = source.publisher(&shares[i]);
                let requests = publisher.keyframe_requests();
                if requests > answered[i] && complete[i].is_none() {
                    answered[i] = requests;
                    let mut response = frame(1000 * requests, 12000 * requests as u32, true, 200, 10 + i as u8);
                    for p in &mut response {
                        p.payload.resize(1000, 0x55);
                    }
                    publisher.send(response).await;
                }
            }
            for (_, packet) in viewer.viewer.as_mut().unwrap().collect(Duration::from_millis(1)).await {
                if !(10..14).contains(&packet.payload[2]) {
                    continue;
                }
                let i = (packet.payload[2] - 10) as usize;
                let frame = frames.entry((i, packet.timestamp)).or_default();
                frame.insert(packet.payload[3]);
                if frame.len() == 200 {
                    complete[i].get_or_insert(start.elapsed());
                }
            }
        }
        let requests: Vec<_> =
            sources.iter().enumerate().map(|(i, s)| s.publisher(&shares[i]).keyframe_requests()).collect();
        eprintln!("LARGE_STARTUP known={known} completed={complete:?} PLIs={requests:?}");
        assert!(complete.iter().all(|delay| delay.is_some_and(|delay| delay < Duration::from_secs(30))));
        assert!(requests.iter().sum::<u64>() <= 15, "startup must not create a PLI storm");
    }
}
