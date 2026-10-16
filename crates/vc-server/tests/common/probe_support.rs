//! Official-server probe observability, also tested without a TeamSpeak server.
use std::{
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub struct AudioCounts([AtomicU64; 256]);
impl Default for AudioCounts {
    fn default() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }
}
impl AudioCounts {
    /// Call in the receive callback, before any helper or bounded event queue.
    pub fn record(&self, payload: &[u8]) {
        // The native 10 ms tail is repacketized as code 2, with its length
        // prefix before the marker. Count both ordinary and paired controls.
        let index = if payload.first().is_some_and(|toc| toc & 3 == 2) {
            if payload.get(1).is_some_and(|n| *n >= 252) { 3 } else { 2 }
        } else {
            1
        };
        if let Some(&marker) = payload.get(index) {
            self.0[usize::from(marker)].fetch_add(payload.len() as u64, Ordering::Relaxed);
        }
    }
    pub fn bytes(&self, marker: u8) -> u64 {
        self.0[usize::from(marker)].load(Ordering::Relaxed)
    }
}

pub async fn control<S, F>(mut send: S, mut received: impl FnMut() -> bool, sequence: &mut u64)
where
    S: FnMut(u64) -> F,
    F: Future<Output = ()>,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        while !received() {
            send(*sequence).await;
            *sequence += 1;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("working plaintext audio control before deadline");
}
