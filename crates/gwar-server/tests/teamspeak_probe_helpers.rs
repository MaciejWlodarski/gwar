//! Pure probe regressions: no TeamSpeak server or connection.
#[path = "common/probe_support.rs"]
mod support;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[tokio::test]
async fn forbidden_counts_survive_unrelated_helper_consumption() {
    let counts = support::AudioCounts::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    for data in [vec![0xf8, 99, 1], vec![0xf8, 11, 1]] {
        counts.record(&data); // same receive callback operation as the official probe
        tx.send(data).await.unwrap();
    }
    while rx.recv().await.unwrap()[1] != 11 {}
    assert!(rx.try_recv().is_err());
    assert_eq!(counts.bytes(99), 3);
    counts.record(&[0xf8, 99, 2]);
    assert_eq!(counts.bytes(99), 6, "later injections remain cumulative");
}

#[tokio::test(start_paused = true)]
async fn plaintext_control_keeps_sending_past_delayed_confirmation() {
    let observed = Arc::new(AtomicU64::new(0));
    let sent = observed.clone();
    let start = tokio::time::Instant::now();
    let mut sequence = 100;
    support::control(
        |seq| {
            assert!(seq >= 100);
            if start.elapsed() >= std::time::Duration::from_secs(2) {
                sent.fetch_add(1, Ordering::Relaxed);
            }
            std::future::ready(())
        },
        || observed.load(Ordering::Relaxed) > 0,
        &mut sequence,
    )
    .await;
    assert!(sequence > 120, "control stopped at the old 400 ms window");
}
