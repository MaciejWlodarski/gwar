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
///
/// A storm is a keyframe request the SFU's limits do not pay for. Each unknown
/// source's one-packet seed is no keyframe, so its share asks for one before anyone
/// watches; every later request is the viewer's and comes out of its budget
/// (`VIEWER_KEYFRAMES_BURST` plus `VIEWER_KEYFRAMES_PER_SECOND`). A busy machine
/// starts slower, and the budget refills meanwhile, so the bound grows with the
/// startup instead of being a fixed count.
#[tokio::test]
async fn four_unknown_or_large_sources_deliver_within_30s_over_srtp() {
    use gwar_server::media::screen::{VIEWER_KEYFRAMES_BURST, VIEWER_KEYFRAMES_PER_SECOND};
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
        if !known {
            // The shares' own requests for their first keyframe, before any viewer.
            let asked = Instant::now();
            while sources.iter().enumerate().any(|(i, s)| s.publisher(&shares[i]).keyframe_requests() == 0) {
                assert!(
                    asked.elapsed() < Duration::from_secs(5),
                    "a share waiting for its first keyframe asks for one"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        let seeds: Vec<_> =
            sources.iter().enumerate().map(|(i, s)| s.publisher(&shares[i]).keyframe_requests()).collect();
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
        let times: Vec<Vec<_>> = sources
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let times = s.publisher(&shares[i]).request_times.lock().unwrap().clone();
                times.iter().map(|at| at.saturating_duration_since(start).as_millis()).collect()
            })
            .collect();
        eprintln!(
            "LARGE_STARTUP known={known} completed={complete:?} PLIs={requests:?} seeds={seeds:?} at_ms={times:?}"
        );
        assert!(complete.iter().all(|delay| delay.is_some_and(|delay| delay < Duration::from_secs(30))));
        assert!(seeds.iter().all(|&n| n <= 1), "a share asks once for its first keyframe: {seeds:?}");
        let viewer_requests: u64 = requests.iter().sum::<u64>() - seeds.iter().sum::<u64>();
        let last = times.iter().flatten().max().copied().unwrap_or(0) as f64 / 1000.0;
        let budget = VIEWER_KEYFRAMES_BURST + VIEWER_KEYFRAMES_PER_SECOND * last;
        assert!(
            viewer_requests as f64 <= budget,
            "startup must not create a PLI storm: {viewer_requests} viewer requests by {last:.1} s, budget {budget:.1}"
        );
    }
}
