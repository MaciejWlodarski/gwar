use std::{sync::mpsc::sync_channel, thread::sleep};

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
fn worker() -> (Worker, Arc<ShareBus>, SyncSender<Input>) {
    let (input, inbox) = sync_channel(INPUT_QUEUE);
    let (ring, rx) = RingBuffer::new(RING_CAPACITY);
    let counters = Arc::<Counters>::default();
    let bus = Arc::new(ShareBus { ring: Mutex::new(rx), active: AtomicBool::new(false), counters: counters.clone() });
    let worker = Worker {
        inbox,
        ring,
        streams: (0..SCREEN_SLOTS).map(|_| None).collect(),
        bus: bus.clone(),
        counters,
        gain: Arc::new(std::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits()))),
        stop: Arc::new(AtomicBool::new(false)),
        block: vec![[0.0; 2]; RING_TARGET],
    };
    (worker, bus, input)
}

/// Plays `blocks` of 10 ms against the worker and returns mid and side.
fn drain(worker: &mut Worker, bus: &ShareBus, blocks: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut mid, mut side) = (Vec::new(), Vec::new());
    for _ in 0..blocks {
        worker.produce();
        let (mut m, mut s) = ([0f32; 480], [0f32; 480]);
        bus.mix(&mut m, &mut s, false);
        mid.extend(m);
        side.extend(s);
    }
    (mid, side)
}

fn feed(worker: &mut Worker, encoder: &mut Encoder, slot: usize, run: u64, first: u64, count: u64, hz: (f32, f32), at: Instant) {
    for n in first..first + count {
        let position = n * FRAME as u64;
        let opus = packet(encoder, position, hz.0, hz.1);
        let arrived = at + Duration::from_millis(20 * (n - first));
        worker.handle(Input::Frame(ShareFrame { slot, run, rtp_time: position as u32, opus }, arrived));
    }
}

#[test]
fn stereo_survives_decoding_and_reaches_mid_and_side() {
    let (mut worker, bus, _input) = worker();
    let mut encoder = encoder();
    worker.handle(Input::Open { slot: 2, run: 7 });
    let (mut mid, mut side) = (Vec::new(), Vec::new());
    for n in 0..40 {
        feed(&mut worker, &mut encoder, 2, 7, n, 1, (440.0, 1000.0), Instant::now());
        let (m, s) = drain(&mut worker, &bus, 2);
        mid.extend(m);
        side.extend(s);
    }
    let settled = 8000..mid.len();
    let (mid, side) = (&mid[settled.clone()], &side[settled]);
    // L = M + S, R = M - S: 440 Hz only on the left, 1 kHz only on the right.
    let left: Vec<f32> = mid.iter().zip(side).map(|(m, s)| m + s).collect();
    let right: Vec<f32> = mid.iter().zip(side).map(|(m, s)| m - s).collect();
    assert!(tone(&left, 440.0) > 0.2, "left {}", tone(&left, 440.0));
    assert!(tone(&right, 1000.0) > 0.2, "right {}", tone(&right, 1000.0));
    assert!(tone(&left, 1000.0) < 0.04 && tone(&right, 440.0) < 0.04);
}

#[test]
fn the_voice_pipeline_never_sees_a_share_that_is_not_there() {
    let (mut worker, bus, _input) = worker();
    assert!(!bus.active());
    worker.produce();
    assert_eq!(worker.ring.buffer().capacity(), worker.ring.slots(), "an idle engine writes nothing");
    let mut voice = [0.25f32; 480];
    let mut side = [1f32; 480];
    bus.mix(&mut voice, &mut side, false);
    assert_eq!(voice, [0.25; 480], "voice unchanged");
    assert_eq!(side, [0.0; 480]);
    assert_eq!(bus.counters.ring_underruns.load(Ordering::Relaxed), 0);
}

#[test]
fn a_loud_share_clips_itself_never_the_voice_under_it() {
    let (mut worker, bus, _input) = worker();
    bus.active.store(true, Ordering::Relaxed);
    let _ = worker.ring.push([1.0, 1.0]);
    let _ = worker.ring.push([-1.0, 1.0]);
    let mut voice = [0.9f32, -0.9];
    let mut side = [0f32; 2];
    bus.mix(&mut voice, &mut side, false);
    assert_eq!(voice[0], 1.0);
    assert!((voice[1] + 0.9).abs() < 1e-6, "mid is zero here");
    assert!(side[1].abs() <= 0.1 + 1e-6);
    let _ = worker.ring.push([1.0, 1.0]);
    let mut voice = [0.5f32];
    bus.mix(&mut voice, &mut [0.0], true);
    assert_eq!(voice[0], 0.5, "deafen plays no share sound");
    assert_eq!(worker.ring.buffer().capacity(), worker.ring.slots(), "and still consumes it");
}

#[test]
fn a_held_mix_lock_costs_the_pipeline_nothing() {
    let (_worker, bus, _input) = worker();
    bus.active.store(true, Ordering::Relaxed);
    let _held = bus.ring.lock().unwrap();
    let mut voice = [0.5f32; 480];
    let started = Instant::now();
    bus.mix(&mut voice, &mut [0.0; 480], false);
    assert!(started.elapsed() < Duration::from_millis(5), "the pipeline waited");
    assert_eq!(voice, [0.5; 480]);
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
    jitter.observe(11 * 960, start + Duration::from_secs(10));
    assert_eq!(jitter.delay(), MAX_DELAY);
}

#[test]
fn a_lost_packet_is_concealed_and_a_stopped_stream_goes_quiet() {
    let (mut worker, bus, _input) = worker();
    let mut encoder = encoder();
    worker.handle(Input::Open { slot: 0, run: 1 });
    let at = Instant::now();
    feed(&mut worker, &mut encoder, 0, 1, 0, 20, (440.0, 440.0), at);
    feed(&mut worker, &mut encoder, 0, 1, 21, 20, (440.0, 440.0), at);
    let (mid, _) = drain(&mut worker, &bus, 70);
    assert!(worker.counters.concealed.load(Ordering::Relaxed) >= 1);
    let (mid2, _) = drain(&mut worker, &bus, 40);
    assert!(rms(&mid[..]) > 0.05);
    assert!(rms(&mid2[mid2.len() - 4800..]) < 0.001, "silence once it stopped: {}", rms(&mid2[mid2.len() - 4800..]));
    assert!(worker.streams[0].as_ref().unwrap().concealing < CONCEAL_LIMIT);
}

#[test]
fn a_stall_backlog_is_dropped_not_replayed() {
    let (mut worker, bus, _input) = worker();
    let mut encoder = encoder();
    worker.handle(Input::Open { slot: 0, run: 1 });
    feed(&mut worker, &mut encoder, 0, 1, 0, 3, (440.0, 440.0), Instant::now());
    drain(&mut worker, &bus, 4);
    feed(&mut worker, &mut encoder, 0, 1, 3, 25, (440.0, 440.0), Instant::now());
    drain(&mut worker, &bus, 4);
    let stream = worker.streams[0].as_ref().unwrap();
    assert!(worker.counters.backlog_drops.load(Ordering::Relaxed) > 0);
    assert!(stream.jitter.queued() <= stream.jitter.delay() + BACKLOG_MARGIN);
}

#[test]
fn the_queue_is_bounded_and_malformed_frames_are_refused() {
    let (mut worker, _bus, _input) = worker();
    worker.handle(Input::Open { slot: 0, run: 1 });
    let mut encoder = encoder();
    feed(&mut worker, &mut encoder, 0, 1, 0, 40, (440.0, 440.0), Instant::now());
    assert_eq!(worker.streams[0].as_ref().unwrap().jitter.queue.len(), MAX_QUEUED);
    assert_eq!(worker.counters.late_drops.load(Ordering::Relaxed), 8);
    let before = worker.counters.late_drops.load(Ordering::Relaxed);
    for opus in [vec![], vec![0u8; 1276], vec![0xff]] {
        worker.handle(Input::Frame(ShareFrame { slot: 0, run: 1, rtp_time: 1, opus }, Instant::now()));
    }
    assert_eq!(worker.counters.late_drops.load(Ordering::Relaxed), before + 3);
    worker.handle(Input::Frame(ShareFrame { slot: 9, run: 1, rtp_time: 1, opus: vec![] }, Instant::now()));
}

#[test]
fn a_newer_run_fences_frames_of_the_previous_share_on_that_slot() {
    let (mut worker, _bus, _input) = worker();
    let mut encoder = encoder();
    worker.handle(Input::Open { slot: 1, run: 5 });
    feed(&mut worker, &mut encoder, 1, 5, 0, 4, (440.0, 440.0), Instant::now());
    worker.handle(Input::Open { slot: 1, run: 6 });
    assert!(worker.streams[1].as_ref().unwrap().jitter.queue.is_empty(), "the old queue is gone");
    feed(&mut worker, &mut encoder, 1, 5, 4, 2, (440.0, 440.0), Instant::now());
    assert!(worker.streams[1].as_ref().unwrap().jitter.queue.is_empty(), "a delayed old frame is dropped");
    worker.handle(Input::Open { slot: 1, run: 5 });
    assert_eq!(worker.streams[1].as_ref().unwrap().run, 6, "an older run cannot come back");
    worker.handle(Input::Close { slot: 1 });
    assert!(worker.streams[1].is_none());
    assert!(!worker.bus.active());
}

#[test]
fn volume_mute_and_two_shares_mix_independently() {
    let (mut worker, bus, _input) = worker();
    let mut one = encoder();
    let mut two = encoder();
    worker.handle(Input::Open { slot: 0, run: 1 });
    worker.handle(Input::Open { slot: 3, run: 1 });
    worker.gain[0].store(0.5f32.to_bits(), Ordering::Relaxed);
    worker.gain[3].store(0.0f32.to_bits(), Ordering::Relaxed);
    let mut mid = Vec::new();
    for n in 0..30 {
        feed(&mut worker, &mut one, 0, 1, n, 1, (440.0, 440.0), Instant::now());
        feed(&mut worker, &mut two, 3, 1, n, 1, (1000.0, 1000.0), Instant::now());
        mid.extend(drain(&mut worker, &bus, 2).0);
    }
    let tail = &mid[12_000..];
    assert!((tone(tail, 440.0) - 0.15).abs() < 0.03, "half volume: {}", tone(tail, 440.0));
    assert!(tone(tail, 1000.0) < 0.01, "muted slot: {}", tone(tail, 1000.0));
}

#[test]
fn clock_drift_is_absorbed_without_dropping_packets() {
    let (mut worker, bus, _input) = worker();
    let mut encoder = encoder();
    worker.handle(Input::Open { slot: 0, run: 1 });
    let start = Instant::now();
    let mut n = 0;
    for round in 0..3000u64 {
        // The sender's clock runs fast: 1,001 packets for every 1,000 the output plays.
        while n * 1000 <= round * 1001 / 2 {
            feed(&mut worker, &mut encoder, 0, 1, n, 1, (440.0, 440.0), start + Duration::from_millis(10 * round));
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
    playout.open(0, 1);
    playout.set_gain(0, f32::NAN);
    playout.set_gain(0, 1.0);
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
