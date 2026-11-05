use std::thread::sleep;

use audiopus::{Application, Bitrate, Channels, SampleRate, coder::Encoder};

use super::*;

fn encoder() -> Encoder {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio).unwrap();
    encoder.set_bitrate(Bitrate::BitsPerSecond(64_000)).unwrap();
    encoder
}

/// One 20 ms stereo packet of a tone on the left and on the right.
fn packet(encoder: &mut Encoder, position: u64, left_hz: f32, right_hz: f32) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(2 * FRAME);
    for i in 0..FRAME as u64 {
        let t = (position + i) as f32 / RATE as f32;
        pcm.push(0.3 * (std::f32::consts::TAU * left_hz * t).sin());
        pcm.push(0.3 * (std::f32::consts::TAU * right_hz * t).sin());
    }
    let mut out = vec![0; 1275];
    let n = encoder.encode_float(&pcm, &mut out).unwrap();
    out.truncate(n);
    out
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
}

/// Tone strength by correlation, independent of phase.
fn tone(samples: &[f32], hz: f32) -> f32 {
    let (mut c, mut s) = (0.0, 0.0);
    for (i, v) in samples.iter().enumerate() {
        let phase = std::f32::consts::TAU * hz * i as f32 / RATE as f32;
        c += v * phase.cos();
        s += v * phase.sin();
    }
    2.0 * (c * c + s * s).sqrt() / samples.len().max(1) as f32
}

/// A worker without its thread: the test drives it, so timing is exact.
fn worker() -> (Worker, Arc<ShareBus>, SharePlayout) {
    let (playout, worker) = SharePlayout::parts();
    (worker, playout.bus(), playout)
}

/// Plays `blocks` of 10 ms against the worker and returns the left and right speakers.
fn drain(worker: &mut Worker, bus: &ShareBus, blocks: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for _ in 0..blocks {
        worker.produce();
        let mut speakers = [[0f32; 2]; 480];
        bus.mix(&mut speakers, false);
        left.extend(speakers.iter().map(|pair| pair[0]));
        right.extend(speakers.iter().map(|pair| pair[1]));
    }
    (left, right)
}

fn feed(
    worker: &mut Worker,
    encoder: &mut Encoder,
    (slot, run): (usize, u64),
    first: u64,
    count: u64,
    hz: (f32, f32),
    at: Instant,
) {
    for n in first..first + count {
        let position = n * FRAME as u64;
        let opus = packet(encoder, position, hz.0, hz.1);
        let arrived = at + Duration::from_millis(20 * (n - first));
        worker.receive(ShareFrame { slot, run, rtp_time: position as u32, opus }, arrived);
    }
}

#[test]
fn stereo_survives_decoding_and_reaches_each_speaker() {
    let (mut worker, bus, playout) = worker();
    let mut encoder = encoder();
    playout.open(2, 7, 1.0);
    let (mut left, mut right) = (Vec::new(), Vec::new());
    for n in 0..40 {
        feed(&mut worker, &mut encoder, (2, 7), n, 1, (440.0, 1000.0), Instant::now());
        let (l, r) = drain(&mut worker, &bus, 2);
        left.extend(l);
        right.extend(r);
    }
    // 440 Hz only on the left, 1 kHz only on the right.
    let (left, right) = (&left[8000..], &right[8000..]);
    assert!(tone(left, 440.0) > 0.2, "left {}", tone(left, 440.0));
    assert!(tone(right, 1000.0) > 0.2, "right {}", tone(right, 1000.0));
    assert!(tone(left, 1000.0) < 0.04 && tone(right, 440.0) < 0.04);
}

#[test]
fn the_voice_pipeline_never_sees_a_share_that_is_not_there() {
    let (mut worker, bus, _playout) = worker();
    assert!(!bus.active());
    worker.produce();
    assert_eq!(worker.ring.buffer().capacity(), worker.ring.slots(), "an idle engine writes nothing");
    let mut speakers = [[0.25f32; 2]; 480];
    bus.mix(&mut speakers, false);
    assert_eq!(speakers, [[0.25; 2]; 480], "voice unchanged");
    assert_eq!(bus.counters.ring_underruns.load(Ordering::Relaxed), 0);
}

#[test]
fn a_loud_share_clips_itself_never_the_voice_under_it() {
    let (mut worker, bus, _playout) = worker();
    bus.active.store(true, Ordering::Relaxed);
    let _ = worker.ring.push([1.0, 1.0]);
    let _ = worker.ring.push([-1.0, 1.0]);
    let mut speakers = [[0.9f32; 2], [-0.9; 2]];
    bus.mix(&mut speakers, false);
    assert_eq!(speakers[0], [1.0, 1.0], "the share fills what voice leaves");
    let centre = 0.5 * (speakers[1][0] + speakers[1][1]);
    assert!((centre + 0.9).abs() < 1e-6, "the voice stays where it was: {centre}");
    assert!((speakers[1][0] - speakers[1][1]).abs() <= 0.2 + 1e-6, "{:?}", speakers[1]);
    let _ = worker.ring.push([1.0, 1.0]);
    let mut speakers = [[0.5f32; 2]];
    bus.mix(&mut speakers, true);
    assert_eq!(speakers[0], [0.5, 0.5], "deafen plays no share sound");
    assert_eq!(worker.ring.buffer().capacity(), worker.ring.slots(), "and still consumes it");
}

#[test]
fn only_sound_really_played_counts_as_played() {
    let (mut worker, bus, _playout) = worker();
    let _ = worker.ring.push([0.2, -0.2]);
    assert!(bus.mix(&mut [[0.0; 2]], false));
    let _ = worker.ring.push([0.0, 0.0]);
    assert!(!bus.mix(&mut [[0.5; 2]], false), "a muted share plays silence");
    let _ = worker.ring.push([0.2, 0.2]);
    assert!(!bus.mix(&mut [[0.0; 2]], true), "deafened plays nothing");
    assert!(!bus.mix(&mut [[0.5; 2]], false), "an empty ring plays nothing");
}

#[test]
fn a_held_mix_lock_costs_the_pipeline_nothing() {
    let (_worker, bus, _playout) = worker();
    bus.active.store(true, Ordering::Relaxed);
    let _held = bus.ring.lock().unwrap();
    let mut speakers = [[0.5f32; 2]; 480];
    let started = Instant::now();
    bus.mix(&mut speakers, false);
    assert!(started.elapsed() < Duration::from_millis(5), "the pipeline waited");
    assert_eq!(speakers, [[0.5; 2]; 480]);
}

#[test]
fn reordered_packets_play_in_order_and_duplicates_are_refused() {
    let mut jitter = Jitter::new();
    let now = Instant::now();
    for time in [960u32, 0, 1920, 960] {
        let packet = Queued { rtp_time: time, opus: vec![] };
        jitter.push(packet, now);
    }
    assert_eq!(jitter.queue.iter().map(|q| q.rtp_time).collect::<Vec<_>>(), [0, 960, 1920]);
    assert_eq!(jitter.pop().unwrap().rtp_time, 0);
    jitter.next = Some(960);
    assert!(!jitter.push(Queued { rtp_time: 0, opus: vec![] }, now), "a packet past its turn is late");
}

#[test]
fn rtp_order_survives_the_wrap() {
    assert!(ahead(5, u32::MAX - 5) > 0);
    let mut jitter = Jitter::new();
    let now = Instant::now();
    jitter.push(Queued { rtp_time: 200, opus: vec![] }, now);
    jitter.push(Queued { rtp_time: u32::MAX - 759, opus: vec![] }, now);
    assert_eq!(jitter.queue[0].rtp_time, u32::MAX - 759);
}

#[test]
fn delay_stays_in_its_bounds_and_follows_late_arrivals() {
    let mut jitter = Jitter::new();
    let start = Instant::now();
    assert_eq!(jitter.delay(), MIN_DELAY);
    for n in 0..10u32 {
        jitter.observe(n * 960, start + Duration::from_millis(20 * u64::from(n)));
    }
    assert_eq!(jitter.delay(), MIN_DELAY);
    jitter.observe(10 * 960, start + Duration::from_millis(200 + 50));
    assert!(jitter.delay() > Duration::from_millis(100));
    // Late, but not so late that the stream must have started over: see `Jitter::discontinuity`.
    jitter.observe(11 * 960, start + Duration::from_millis(220 + 200));
    assert_eq!(jitter.delay(), MAX_DELAY);
}

#[test]
fn a_stall_a_held_queue_rides_out_is_learned_as_jitter() {
    // With sound held for the picture the queue lasts up to `MAX_HOLD` longer before it runs dry,
    // so a stall that long is the network's jitter, which the delay must learn, not a new start.
    assert!(MAX_DISCONTINUITY >= MAX_DELAY + MAX_HOLD);
    let mut jitter = Jitter::new();
    jitter.set_hold(MAX_HOLD);
    assert!(jitter.discontinuity() >= MAX_DELAY + MAX_HOLD);
    let start = Instant::now();
    for n in 0..10u32 {
        jitter.observe(n * 960, start + Duration::from_millis(20 * u64::from(n)));
    }
    let before = jitter.delay();
    jitter.observe(10 * 960, start + Duration::from_millis(200 + 500));
    assert!(jitter.delay() > before, "a 500 ms stall under a full hold was not learned: {before:?}");
}

/// With nothing held, a pause past what the delay and concealment cover is a new start: learnt
/// as jitter, it put share sound 160 ms late for seconds, and the picture waited for it.
#[test]
fn with_nothing_held_a_pause_past_the_delay_is_not_jitter() {
    let mut jitter = Jitter::new();
    let start = Instant::now();
    for n in 0..10u32 {
        jitter.observe(n * 960, start + Duration::from_millis(20 * u64::from(n)));
    }
    assert!(jitter.discontinuity() < MAX_DELAY + CONCEAL + Duration::from_millis(1));
    jitter.observe(10 * 960, start + Duration::from_millis(200 + 400));
    assert_eq!(jitter.delay(), MIN_DELAY, "a 400 ms pause with nothing held became jitter");
    // The same pause under a hold the queue rode it out with is still jitter.
    let mut held = Jitter::new();
    held.set_hold(Duration::from_millis(300));
    for n in 0..10u32 {
        held.observe(n * 960, start + Duration::from_millis(20 * u64::from(n)));
    }
    held.observe(10 * 960, start + Duration::from_millis(200 + 400));
    assert_eq!(held.delay(), Duration::from_millis(300) + MAX_DELAY);
}

/// The server keeps share sound's RTP time contiguous across a pause in the sound
/// (gwar-server's media/screen/audio.rs), so the first packet after one arrives seconds after its
/// predecessor for 20 ms of RTP time. That is a new start, not jitter: the delay must not grow.
#[test]
fn a_pause_in_the_sound_does_not_hold_it_behind_the_picture() {
    let (mut worker, bus, playout) = worker();
    let mut encoder = encoder();
    playout.open(0, 1, 1.0);
    let at = Instant::now();
    feed(&mut worker, &mut encoder, (0, 1), 0, 50, (440.0, 440.0), at);
    drain(&mut worker, &bus, 150);
    let stream = worker.streams[0].as_ref().unwrap();
    assert!(!stream.jitter.started, "the queue ran dry and the stream started over");
    assert_eq!(stream.jitter.delay(), MIN_DELAY);
    feed(&mut worker, &mut encoder, (0, 1), 50, 50, (440.0, 440.0), at + Duration::from_secs(3));
    let (left, _) = drain(&mut worker, &bus, 60);
    let stream = worker.streams[0].as_ref().unwrap();
    assert_eq!(stream.jitter.delay(), MIN_DELAY, "the pause became 'jitter'");
    assert!(rms(&left[left.len() - 4800..]) > 0.05, "and the sound came back");
}

/// A network stall long enough to run the queue dry and start the stream over is still the
/// network's: the delay grows to absorb the next one instead of losing it again.
#[test]
fn a_stall_that_ran_the_queue_dry_still_raises_the_delay() {
    let (mut worker, bus, playout) = worker();
    let mut encoder = encoder();
    playout.open(0, 1, 1.0);
    let at = Instant::now();
    feed(&mut worker, &mut encoder, (0, 1), 0, 50, (440.0, 440.0), at);
    drain(&mut worker, &bus, 120);
    assert!(!worker.streams[0].as_ref().unwrap().jitter.started, "the stall ran it dry");
    let late = at + Duration::from_millis(49 * 20 + 220);
    feed(&mut worker, &mut encoder, (0, 1), 50, 10, (440.0, 440.0), late);
    assert_eq!(worker.streams[0].as_ref().unwrap().jitter.delay(), MAX_DELAY);
}

/// The same pause while sound was still queued, so the stream never started over.
#[test]
fn an_arrival_long_after_its_predecessor_is_not_jitter() {
    let mut jitter = Jitter::new();
    let start = Instant::now();
    for n in 0..10u32 {
        jitter.observe(n * 960, start + Duration::from_millis(20 * u64::from(n)));
    }
    jitter.observe(10 * 960, start + Duration::from_secs(3));
    assert_eq!(jitter.delay(), MIN_DELAY);
    jitter.observe(11 * 960, start + Duration::from_secs(3) + Duration::from_millis(20));
    assert_eq!(jitter.delay(), MIN_DELAY, "it starts a new stretch");
}

#[test]
fn a_lost_packet_is_concealed_and_a_stopped_stream_goes_quiet() {
    let (mut worker, bus, playout) = worker();
    let mut encoder = encoder();
    playout.open(0, 1, 1.0);
    let at = Instant::now();
    feed(&mut worker, &mut encoder, (0, 1), 0, 20, (440.0, 440.0), at);
    feed(&mut worker, &mut encoder, (0, 1), 21, 20, (440.0, 440.0), at);
    let (mid, _) = drain(&mut worker, &bus, 70);
    assert!(worker.counters.concealed.load(Ordering::Relaxed) >= 1);
    let (mid2, _) = drain(&mut worker, &bus, 40);
    assert!(rms(&mid[..]) > 0.05);
    assert!(rms(&mid2[mid2.len() - 4800..]) < 0.001, "silence once it stopped: {}", rms(&mid2[mid2.len() - 4800..]));
    assert!(worker.streams[0].as_ref().unwrap().concealing < CONCEAL_LIMIT);
}

#[test]
fn a_stall_backlog_is_dropped_not_replayed() {
    let (mut worker, bus, playout) = worker();
    let mut encoder = encoder();
    playout.open(0, 1, 1.0);
    let start = Instant::now();
    feed(&mut worker, &mut encoder, (0, 1), 0, 3, (440.0, 440.0), start);
    drain(&mut worker, &bus, 4);
    // Half a second lands at once, then sound runs on at its own pace: that queue will not drain.
    feed(&mut worker, &mut encoder, (0, 1), 3, 25, (440.0, 440.0), start + Duration::from_millis(40));
    for n in 0..30 {
        let at = start + Duration::from_millis(40 + 20 * n);
        feed(&mut worker, &mut encoder, (0, 1), 28 + n, 1, (440.0, 440.0), at);
        drain(&mut worker, &bus, 2);
    }
    let stream = worker.streams[0].as_ref().unwrap();
    assert!(worker.counters.backlog_drops.load(Ordering::Relaxed) > 0);
    assert!(stream.jitter.queued() <= stream.jitter.delay() + BACKLOG_MARGIN);
}

/// Its limit grows with the delay, so the shortest the queue has been can fall below it mid-watch.
#[test]
fn a_delay_that_grows_while_the_queue_is_watched_drops_nothing() {
    let mut jitter = Jitter::new();
    jitter.started = true;
    let packets = |range: std::ops::Range<u32>| range.map(|n| Queued { rtp_time: n * FRAME as u32, opus: vec![] });
    jitter.queue.extend(packets(0..8));
    assert_eq!(jitter.trim(BACKLOG_WATCH / 2), 0);
    jitter.deviation = 0.07;
    jitter.queue.extend(packets(8..13));
    assert_eq!(jitter.trim(BACKLOG_WATCH), 0);
    assert_eq!(jitter.queue.len(), 13);
}

#[test]
fn sound_already_played_stays_out_after_the_queue_ran_dry() {
    let (mut worker, bus, playout) = worker();
    let mut encoder = encoder();
    playout.open(0, 1, 1.0);
    let start = Instant::now();
    feed(&mut worker, &mut encoder, (0, 1), 0, 4, (440.0, 440.0), start);
    drain(&mut worker, &bus, 12);
    assert!(worker.counters.concealed.load(Ordering::Relaxed) >= 1);
    let late = worker.counters.late_drops.load(Ordering::Relaxed);
    feed(&mut worker, &mut encoder, (0, 1), 1, 1, (440.0, 440.0), start);
    assert_eq!(worker.counters.late_drops.load(Ordering::Relaxed), late + 1, "a duplicate of played sound");
    feed(&mut worker, &mut encoder, (0, 1), 4, 1, (440.0, 440.0), start);
    assert_eq!(worker.counters.late_drops.load(Ordering::Relaxed), late + 1, "the first sound it was missing");
}

/// A saturated machine hands sound over in lumps. All of it must still play: a queue that empties
/// again before the next lump is not behind, however long it was for a moment.
#[test]
fn sound_arriving_in_lumps_is_not_mistaken_for_a_backlog() {
    for lump_ms in [200u64, 300, 400, 500] {
        let (mut worker, bus, playout) = worker();
        let mut encoder = encoder();
        playout.open(0, 1, 1.0);
        let start = Instant::now();
        let lump = lump_ms / 20;
        for round in 0..20 {
            let at = start + Duration::from_millis(lump_ms * round);
            for n in round * lump..(round + 1) * lump {
                feed(&mut worker, &mut encoder, (0, 1), n, 1, (440.0, 440.0), at);
            }
            drain(&mut worker, &bus, (lump_ms / 10) as usize);
        }
        let counters = &worker.counters;
        let (decoded, backlog) =
            (counters.decoded.load(Ordering::Relaxed), counters.backlog_drops.load(Ordering::Relaxed));
        assert_eq!(backlog, 0, "{lump_ms} ms lumps: {decoded} decoded of {}", 20 * lump);
        assert!(decoded >= 18 * lump, "{lump_ms} ms lumps: {decoded} decoded of {}", 20 * lump);
    }
}

#[test]
fn the_queue_is_bounded_and_malformed_frames_are_refused() {
    let (mut worker, _bus, playout) = worker();
    playout.open(0, 1, 1.0);
    let mut encoder = encoder();
    feed(&mut worker, &mut encoder, (0, 1), 0, MAX_QUEUED as u64 + 8, (440.0, 440.0), Instant::now());
    assert_eq!(worker.streams[0].as_ref().unwrap().jitter.queue.len(), MAX_QUEUED);
    assert_eq!(worker.counters.late_drops.load(Ordering::Relaxed), 8);
    let before = worker.counters.late_drops.load(Ordering::Relaxed);
    for opus in [vec![], vec![0u8; 1276], vec![0xff]] {
        worker.receive(ShareFrame { slot: 0, run: 1, rtp_time: 1, opus }, Instant::now());
    }
    assert_eq!(worker.counters.late_drops.load(Ordering::Relaxed), before + 3);
    worker.receive(ShareFrame { slot: 9, run: 1, rtp_time: 1, opus: vec![] }, Instant::now());
}

#[test]
fn a_newer_run_fences_frames_of_the_previous_share_on_that_slot() {
    let (mut worker, _bus, playout) = worker();
    let mut encoder = encoder();
    playout.open(1, 5, 1.0);
    feed(&mut worker, &mut encoder, (1, 5), 0, 4, (440.0, 440.0), Instant::now());
    assert!(playout.open(1, 6, 1.0));
    worker.apply_controls();
    assert!(worker.streams[1].as_ref().unwrap().jitter.queue.is_empty(), "the old queue is gone");
    feed(&mut worker, &mut encoder, (1, 5), 4, 2, (440.0, 440.0), Instant::now());
    assert!(worker.streams[1].as_ref().unwrap().jitter.queue.is_empty(), "a delayed old frame is dropped");
    assert!(!playout.open(1, 5, 1.0), "an older run is told it lost");
    worker.apply_controls();
    assert_eq!(worker.streams[1].as_ref().unwrap().run, 6, "an older run cannot come back");
    assert!(playout.close(1, 6));
    worker.apply_controls();
    assert!(worker.streams[1].is_none());
    assert!(!worker.bus.active());
}

#[test]
fn a_full_frame_queue_cannot_lose_a_replacement() {
    let (mut worker, bus, playout) = worker();
    assert!(playout.open(0, 1, 1.0));
    worker.apply_controls();
    let mut encoder = encoder();
    for n in 0..INPUT_QUEUE as u64 + 8 {
        let opus = packet(&mut encoder, n, 440.0, 440.0);
        playout.push(ShareFrame { slot: 0, run: 1, rtp_time: n as u32 * FRAME as u32, opus });
    }
    assert!(playout.stats().input_drops > 0, "the queue really was full");
    assert!(playout.open(0, 2, 1.0), "the replacement is in force");
    while let Ok(message) = worker.inbox.try_recv() {
        worker.take(message);
    }
    assert_eq!(worker.streams[0].as_ref().map(|stream| stream.run), Some(2));
    assert!(worker.streams[0].as_ref().unwrap().jitter.queue.is_empty(), "the old run's backlog stays out");
    assert!(playout.close(0, 2));
    for n in 0..INPUT_QUEUE as u64 + 8 {
        playout.push(ShareFrame { slot: 0, run: 2, rtp_time: n as u32, opus: vec![0xfc, 0xff, 0xfe] });
    }
    worker.produce();
    assert!(!bus.active(), "a close is not lost either");
}

#[test]
fn a_late_command_of_an_old_run_cannot_touch_its_replacement() {
    let (mut worker, _bus, playout) = worker();
    assert!(playout.open(1, 5, 0.25));
    assert!(playout.open(1, 6, 0.75));
    worker.apply_controls();
    assert!(!playout.close(1, 5), "the old receiver's close comes after its replacement opened");
    assert!(!playout.set_gain(1, 5, 0.0), "and so does its volume");
    worker.apply_controls();
    assert_eq!(worker.streams[1].as_ref().map(|stream| stream.run), Some(6), "the new run keeps playing");
    assert_eq!(f32::from_bits(worker.gain[1].load(Ordering::Relaxed)), 0.75);
}

#[test]
fn a_closed_slot_still_remembers_the_run_that_closed_it() {
    let (mut worker, _bus, playout) = worker();
    assert!(playout.open(1, 6, 1.0));
    assert!(playout.close(1, 6));
    assert!(!playout.open(1, 5, 1.0), "an open from before the close stays out");
    assert!(!playout.set_gain(1, 5, 0.0));
    worker.apply_controls();
    assert!(worker.streams[1].is_none());
    assert!(playout.open(1, 6, 1.0), "the same run may watch the slot again");
    worker.apply_controls();
    assert_eq!(worker.streams[1].as_ref().map(|stream| stream.run), Some(6));
    assert!(!playout.open(SCREEN_SLOTS, 7, 1.0) && !playout.close(SCREEN_SLOTS, 7));
    assert!(!playout.set_gain(SCREEN_SLOTS, 7, 1.0));
}

#[test]
fn opening_a_slot_twice_keeps_its_stream() {
    let (mut worker, _bus, playout) = worker();
    let mut encoder = encoder();
    assert!(playout.open(0, 1, 1.0));
    feed(&mut worker, &mut encoder, (0, 1), 0, 4, (440.0, 440.0), Instant::now());
    assert!(playout.open(0, 1, 0.5), "already in force");
    worker.apply_controls();
    assert_eq!(worker.streams[0].as_ref().unwrap().jitter.queue.len(), 4);
    assert_eq!(f32::from_bits(worker.gain[0].load(Ordering::Relaxed)), 0.5);
}

#[test]
fn volume_mute_and_two_shares_mix_independently() {
    let (mut worker, bus, playout) = worker();
    let mut one = encoder();
    let mut two = encoder();
    playout.open(0, 1, 1.0);
    playout.open(3, 1, 1.0);
    worker.gain[0].store(0.5f32.to_bits(), Ordering::Relaxed);
    worker.gain[3].store(0.0f32.to_bits(), Ordering::Relaxed);
    let mut mid = Vec::new();
    for n in 0..30 {
        feed(&mut worker, &mut one, (0, 1), n, 1, (440.0, 440.0), Instant::now());
        feed(&mut worker, &mut two, (3, 1), n, 1, (1000.0, 1000.0), Instant::now());
        mid.extend(drain(&mut worker, &bus, 2).0);
    }
    let tail = &mid[12_000..];
    assert!((tone(tail, 440.0) - 0.15).abs() < 0.03, "half volume: {}", tone(tail, 440.0));
    assert!(tone(tail, 1000.0) < 0.01, "muted slot: {}", tone(tail, 1000.0));
}

#[test]
fn clock_drift_is_absorbed_without_dropping_packets() {
    let (mut worker, bus, playout) = worker();
    let mut encoder = encoder();
    playout.open(0, 1, 1.0);
    let start = Instant::now();
    let mut n = 0;
    for round in 0..3000u64 {
        // The sender's clock runs fast: 1,001 packets for every 1,000 the output plays.
        while n * 1000 <= round * 1001 / 2 {
            feed(&mut worker, &mut encoder, (0, 1), n, 1, (440.0, 440.0), start + Duration::from_millis(10 * round));
            n += 1;
        }
        drain(&mut worker, &bus, 1);
    }
    let stream = worker.streams[0].as_ref().unwrap();
    assert_eq!(worker.counters.backlog_drops.load(Ordering::Relaxed), 0);
    assert!(stream.jitter.queued() <= stream.jitter.delay() + DRIFT_MARGIN + Duration::from_millis(40));
}

#[test]
fn the_thread_serves_a_real_stream_and_drops_rather_than_blocks() {
    let playout = SharePlayout::new().unwrap();
    let mut encoder = encoder();
    assert!(playout.open(0, 1, f32::NAN));
    assert!(playout.set_gain(0, 1, 1.0));
    let started = Instant::now();
    for n in 0..20_000u64 {
        let position = n * FRAME as u64;
        playout.push(ShareFrame { slot: 0, run: 1, rtp_time: position as u32, opus: vec![0xfc, 0xff, 0xfe] });
    }
    assert!(started.elapsed() < Duration::from_secs(1), "pushing never waits: {:?}", started.elapsed());
    sleep(Duration::from_millis(40));
    let packet = packet(&mut encoder, 0, 440.0, 440.0);
    playout.push(ShareFrame { slot: 0, run: 1, rtp_time: 0, opus: packet });
    let stats = playout.stats();
    assert_eq!(stats.frames, 20_001);
    assert!(stats.input_drops > 0 || stats.late_drops > 0);
    assert!(playout.bus().active());
}

/// With nothing open the share thread has nothing to do until a frame or a command comes, and
/// it lives as long as the page: it must sleep, not wake every few milliseconds.
#[test]
fn an_idle_share_thread_sleeps_and_a_command_wakes_it() {
    let playout = SharePlayout::new().unwrap();
    let wakeups = || playout.counters.wakeups.load(Ordering::Relaxed);
    sleep(Duration::from_millis(200));
    assert!(wakeups() <= 1, "woke {} times with nothing open", wakeups());
    assert!(playout.open(0, 1, 1.0));
    let started = Instant::now();
    while !playout.bus().active() && started.elapsed() < Duration::from_secs(2) {
        sleep(Duration::from_millis(1));
    }
    assert!(playout.bus().active(), "an open command alone wakes it");
    let serving = wakeups();
    sleep(Duration::from_millis(100));
    assert!(wakeups() >= serving + 5, "it tops the ring up while a stream is open");
    assert!(playout.close(0, 1));
    sleep(Duration::from_millis(50));
    let closed = wakeups();
    sleep(Duration::from_millis(200));
    assert!(wakeups() <= closed + 1, "and sleeps again once it is closed: {} wakeups", wakeups() - closed);
}

#[tokio::test]
async fn an_interrupted_stream_keeps_its_consent_and_a_failed_start_is_only_closed() {
    let queue = Arc::new(CaptureQueue::default());
    queue.stream_interrupted();
    assert!(queue.is_closed() && queue.interrupted_after().is_none(), "it never came up: a failed start");
    let queue = Arc::new(CaptureQueue::default());
    assert!(queue.adopt());
    let up = Instant::now();
    queue.stream_started_at(up);
    queue.stream_interrupted_at(up + Duration::from_millis(500));
    assert!(queue.is_closed());
    assert_eq!(queue.interrupted_after(), Some(Duration::from_millis(500)), "when it died, not when it was noticed");
    assert!(!queue.was_ended_by_system(), "a lost device is not the user taking the sound back");
    let again = queue.restarted();
    assert!(again.adopt(), "the same share may capture again");
    again.write(&[0.5, -0.5]);
    assert_eq!(again.read(Duration::from_millis(5)).await, [0.5, -0.5]);
    again.stream_started_at(up + Duration::from_secs(70));
    again.stream_interrupted_at(up + Duration::from_secs(75));
    assert_eq!(again.interruptions_within(Duration::from_secs(60)), 1, "a minute ago is forgotten");
    assert_eq!(again.interruptions_within(Duration::from_secs(80)), 2, "both are under one consent");
}

#[tokio::test]
async fn the_capture_queue_drops_old_sound_and_keeps_channels_in_step() {
    let queue = CaptureQueue::default();
    assert!(queue.read(Duration::from_millis(5)).await.is_empty(), "a timeout gives nothing");
    let block: Vec<f32> = (0..2000).flat_map(|i| [i as f32, -(i as f32)]).collect();
    for _ in 0..30 {
        queue.write(&block);
    }
    assert!(queue.dropped() > 0);
    let samples = queue.read(Duration::from_millis(5)).await;
    assert!(samples.len() <= CAPTURE_LIMIT);
    assert!(samples.as_chunks::<2>().0.iter().all(|[left, right]| *left == -*right), "channels swapped");
    queue.close();
    assert!(queue.read(Duration::from_secs(10)).await.is_empty(), "closing ends the wait at once");
}

#[tokio::test]
async fn no_sound_is_read_once_the_system_ended_a_capture() {
    let queue = Arc::new(CaptureQueue::default());
    queue.write(&[0.5, -0.5]);
    queue.end_by_system();
    queue.write(&[0.5, -0.5]);
    assert!(queue.read(Duration::from_secs(1)).await.is_empty(), "the stream runs on for a moment");
}

#[tokio::test]
async fn a_reader_wakes_as_soon_as_sound_arrives() {
    let queue = Arc::new(CaptureQueue::default());
    let reader = {
        let queue = queue.clone();
        tokio::spawn(async move { queue.read(Duration::from_secs(5)).await })
    };
    tokio::time::sleep(Duration::from_millis(5)).await;
    let started = Instant::now();
    queue.write(&[0.5, -0.5]);
    assert_eq!(reader.await.unwrap(), [0.5, -0.5]);
    assert!(started.elapsed() < Duration::from_secs(1));
}

fn batch(frames: &[(u8, u32, &[u8])]) -> Vec<u8> {
    let mut body = [7u64.to_le_bytes(), 9u64.to_le_bytes()].concat();
    for (slot, time, opus) in frames {
        body.push(*slot);
        body.extend(time.to_le_bytes());
        body.extend((opus.len() as u16).to_le_bytes());
        body.extend(*opus);
    }
    body
}

#[test]
fn a_batch_keeps_its_frames_and_their_order() {
    let (owner, frames) = parse_frames(&batch(&[(1, 960, &[1, 2]), (3, 1920, &[3])])).unwrap();
    assert_eq!(owner, 7);
    assert_eq!(frames.len(), 2);
    assert_eq!((frames[0].slot, frames[0].run, frames[0].rtp_time), (1, 9, 960));
    assert_eq!(frames[0].opus, [1, 2]);
    assert_eq!((frames[1].slot, frames[1].rtp_time), (3, 1920));
}

#[test]
fn truncated_or_oversized_batches_are_refused_whole() {
    assert!(parse_frames(&[0; 15]).is_none());
    let mut cut = batch(&[(1, 960, &[1, 2])]);
    cut.pop();
    assert!(parse_frames(&cut).is_none());
    assert!(parse_frames(&vec![0; MAX_BATCH + 1]).is_none());
    assert_eq!(parse_frames(&batch(&[])).unwrap().1.len(), 0);
}

#[test]
fn the_clock_says_when_a_sample_reaches_the_speaker() {
    let (mut worker, bus, playout) = worker();
    assert!(playout.open(1, 7, 1.0));
    worker.apply_controls();
    assert_eq!(playout.clock(1, 7), None, "nothing has been written yet");
    let mut encoder = encoder();
    let at = Instant::now();
    feed(&mut worker, &mut encoder, (1, 7), 0, 12, (440.0, 440.0), at);
    worker.produce();
    assert_eq!(playout.clock(1, 7), None, "no voice pipeline has told where the speaker is");

    // 480 samples stand before the speaker, and the mark's sample is the ring's first.
    bus.note_output(480);
    let first = playout.clock(1, 7).unwrap();
    let (written, rtp_time) = {
        let marks = playout.marks.lock().unwrap();
        (marks[1].unwrap().written, marks[1].unwrap().rtp_time)
    };
    assert_eq!(written, 0);
    assert_eq!(first.rtp_time, rtp_time);
    let output = bus.output.lock().unwrap().unwrap();
    assert_eq!(first.at, output.at + Duration::from_millis(10));

    // Taking 10 ms from the ring brings a later mark no closer than it was: its position moved too.
    let mut speakers = [[0f32; 2]; 480];
    bus.mix(&mut speakers, false);
    worker.produce();
    bus.note_output(480);
    let later = playout.clock(1, 7).unwrap();
    let output = bus.output.lock().unwrap().unwrap();
    let ahead = later.at.duration_since(output.at).as_secs_f64() * f64::from(RATE);
    let held = (playout.marks.lock().unwrap()[1].unwrap().written - 480) as f64;
    assert!((ahead - 480.0 - held).abs() < 1.0, "{ahead} samples ahead");
    assert_ne!(later.rtp_time, first.rtp_time);
}

#[test]
fn the_clock_belongs_to_one_run_and_one_slot() {
    let (mut worker, bus, playout) = worker();
    assert!(playout.open(0, 7, 1.0));
    worker.apply_controls();
    let mut encoder = encoder();
    feed(&mut worker, &mut encoder, (0, 7), 0, 12, (440.0, 440.0), Instant::now());
    worker.produce();
    bus.note_output(0);
    assert!(playout.clock(0, 7).is_some());
    assert_eq!(playout.clock(0, 8), None);
    assert_eq!(playout.clock(1, 7), None);
    assert_eq!(playout.clock(SCREEN_SLOTS, 7), None);
}

#[test]
fn the_clock_reads_system_time() {
    let now = Instant::now();
    let unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1000.0;
    let ahead = ShareClock { rtp_time: 0, at: now + Duration::from_millis(100) };
    assert!((ahead.unix_ms() - unix - 100.0).abs() < 50.0);
}

/// Plays `seconds` of a stream arriving on time.
fn play_on_time(
    worker: &mut Worker,
    bus: &ShareBus,
    encoder: &mut Encoder,
    slot: (usize, u64),
    first: u64,
    seconds: u64,
) {
    let start = Instant::now();
    for n in 0..seconds * 50 {
        feed(worker, encoder, slot, first + n, 1, (440.0, 440.0), start + Duration::from_millis(20 * n));
        drain(worker, bus, 2);
    }
}

#[test]
fn sound_reaches_a_hold_for_the_picture_without_a_jump() {
    let (mut worker, bus, playout) = worker();
    assert!(playout.open(0, 7, 1.0));
    worker.apply_controls();
    let mut encoder = encoder();
    play_on_time(&mut worker, &bus, &mut encoder, (0, 7), 0, 2);
    let before = worker.streams[0].as_ref().unwrap().jitter.queued();
    assert!(playout.set_hold(0, 7, 60));

    // One second at 1 % is 10 ms: the queue may not move faster than that, in either direction.
    play_on_time(&mut worker, &bus, &mut encoder, (0, 7), 100, 1);
    let early = worker.streams[0].as_ref().unwrap().jitter.queued();
    // 10 ms of hold, and one packet of rounding in what is queued (early review S9: 40 ms let
    // four times the declared rate through).
    assert!(early <= before + Duration::from_millis(10 + u64::from(FRAME_MS)), "{before:?} to {early:?} in a second");
    play_on_time(&mut worker, &bus, &mut encoder, (0, 7), 150, 7);
    let held = worker.streams[0].as_ref().unwrap().jitter.queued();
    assert!(held >= before + Duration::from_millis(60), "{before:?} to {held:?}");
    let stats = playout.stats();
    assert_eq!((stats.late_drops, stats.backlog_drops, stats.concealed), (0, 0, 0));

    assert!(playout.set_hold(0, 7, 0));
    play_on_time(&mut worker, &bus, &mut encoder, (0, 7), 500, 10);
    let jitter = &worker.streams[0].as_ref().unwrap().jitter;
    let (released, delay) = (jitter.queued(), jitter.delay());
    assert!(released <= delay + DRIFT_MARGIN + Duration::from_millis(20), "{released:?} over {delay:?}");
    let stats = playout.stats();
    assert_eq!((stats.late_drops, stats.backlog_drops, stats.concealed), (0, 0, 0));
}

#[test]
fn a_hold_given_back_leaves_no_gap_in_the_sound() {
    let (mut worker, bus, playout) = worker();
    assert!(playout.open(0, 7, 1.0));
    worker.apply_controls();
    let mut encoder = encoder();
    play_on_time(&mut worker, &bus, &mut encoder, (0, 7), 0, 2);
    assert!(playout.set_hold(0, 7, 120));
    play_on_time(&mut worker, &bus, &mut encoder, (0, 7), 100, 14);
    assert!(playout.set_hold(0, 7, 0));

    let start = Instant::now();
    let mut gaps = 0;
    for n in 0..14 * 50 {
        feed(&mut worker, &mut encoder, (0, 7), 800 + n, 1, (440.0, 440.0), start + Duration::from_millis(20 * n));
        gaps += drain(&mut worker, &bus, 2).0.iter().filter(|sample| **sample == 0.0).count();
    }
    assert_eq!(gaps, 0, "samples the mixer left empty with packets queued");
    assert_eq!(playout.stats().concealed, 0);
}

#[test]
fn a_hold_is_bounded_and_a_new_run_starts_without_one() {
    let (mut worker, _bus, playout) = worker();
    assert!(playout.open(0, 7, 1.0));
    worker.apply_controls();
    assert!(playout.set_hold(0, 7, 60_000));
    assert_eq!(playout.hold[0].load(Ordering::Relaxed), MAX_HOLD.as_millis() as u32);
    // Early review S4: at a full hold the queue's limit sat at the trim's own keep, so a lump
    // was dropped newest first instead of trimmed oldest first.
    assert!(
        MAX_HOLD + MAX_DELAY + BACKLOG_MARGIN + JUMP_HEADROOM
            <= Duration::from_millis(u64::from(FRAME_MS) * MAX_QUEUED as u64)
    );
    assert!(playout.open(0, 8, 1.0));
    assert_eq!(playout.hold[0].load(Ordering::Relaxed), 0);
    assert!(!playout.set_hold(0, 7, 100), "an older run has no say");
    assert_eq!(playout.hold[0].load(Ordering::Relaxed), 0);
}

#[test]
fn a_reached_hold_keeps_the_sound_in_tune_under_jitter() {
    // Opus review r1 (S17): the queue is counted in 20 ms packets, and a dead zone of 10 ms each
    // side let jitter flip the speed between the full 1 % and none: 17 cents of slow wander on
    // music. Packets arrive up to 10 ms early or late; once the hold is reached, the speed
    // stays close to what drift asks for.
    let (mut worker, bus, playout) = worker();
    assert!(playout.open(0, 7, 1.0));
    worker.apply_controls();
    let mut encoder = encoder();
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut jitter = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % 21) as i64 - 10
    };
    let start = Instant::now();
    let arrivals: Vec<i64> = (0..30 * 50).map(|n| 40 + 20 * n + jitter()).collect();
    let mut next = 0usize;
    let mut speeds = Vec::new();
    for tick in 0..30 * 100i64 {
        if tick == 200 {
            assert!(playout.set_hold(0, 7, 60));
        }
        while next < arrivals.len() && arrivals[..=next].iter().max().copied().unwrap() <= 10 * tick {
            feed(
                &mut worker,
                &mut encoder,
                (0, 7),
                next as u64,
                1,
                (440.0, 440.0),
                start + Duration::from_millis(10 * tick as u64),
            );
            next += 1;
        }
        drain(&mut worker, &bus, 1);
        if tick >= 15 * 100 {
            speeds.push(worker.streams[0].as_ref().unwrap().speed);
        }
    }
    let (low, high) = speeds.iter().fold((f64::MAX, f64::MIN), |(l, h), s| (l.min(*s), h.max(*s)));
    // The old all-or-nothing speed spanned 0.9959 to 1.0092 here.
    assert!(high - low <= HOLD_RATE / 2.0, "speed {low:.4} to {high:.4}");
    let stats = playout.stats();
    assert_eq!((stats.late_drops, stats.backlog_drops), (0, 0));
}
