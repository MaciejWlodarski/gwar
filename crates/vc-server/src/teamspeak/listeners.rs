//! Dedicated ServerQuery connections for channel chat, independent of the
//! query client that moves between channels to post our users' messages.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    time::Duration,
};

use anyhow::{Result, bail};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::warn;

use super::query::Query;

const MAX_LISTENERS: usize = 128;
const RETRY: Duration = Duration::from_secs(5);

pub(super) struct Text {
    pub cid: u64,
    pub generation: u64,
    pub invoker: u16,
    pub text: String,
}

struct Listener {
    generation: u64,
    task: JoinHandle<()>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) struct ChannelListeners {
    addr: SocketAddr,
    password: String,
    listeners: BTreeMap<u64, Listener>,
    events: mpsc::Sender<Text>,
    generation: u64,
    limited: bool,
}

impl ChannelListeners {
    pub fn new(addr: SocketAddr, password: String, events: mpsc::Sender<Text>) -> Self {
        Self { addr, password, listeners: BTreeMap::new(), events, generation: 0, limited: false }
    }

    /// One listener per occupied bridged channel; an empty channel releases it.
    pub fn reconcile(&mut self, channels: impl IntoIterator<Item = u64>) {
        let wanted: BTreeSet<_> = channels.into_iter().collect();
        self.listeners.retain(|cid, _| wanted.contains(cid));
        if wanted.len() > MAX_LISTENERS && !self.limited {
            warn!(occupied = wanted.len(), limit = MAX_LISTENERS, "TeamSpeak channel chat listener limit reached");
        }
        self.limited = wanted.len() > MAX_LISTENERS;
        for cid in wanted {
            if self.listeners.contains_key(&cid) {
                continue;
            }
            if self.listeners.len() >= MAX_LISTENERS {
                break;
            }
            self.generation += 1;
            let generation = self.generation;
            let (addr, password, events) = (self.addr, self.password.clone(), self.events.clone());
            let task = tokio::spawn(async move {
                loop {
                    if let Err(e) = listen(addr, &password, cid, generation, &events).await {
                        warn!(cid, "TeamSpeak channel chat listener: {e:#}; retrying in {RETRY:?}");
                    }
                    tokio::time::sleep(RETRY).await;
                }
            });
            self.listeners.insert(cid, Listener { generation, task });
        }
    }

    /// Ignore queued events from a listener whose channel was released.
    pub fn accepts(&self, text: &Text) -> bool {
        self.listeners.get(&text.cid).is_some_and(|l| l.generation == text.generation)
    }
}

async fn listen(
    addr: SocketAddr,
    password: &str,
    cid: u64,
    generation: u64,
    events: &mpsc::Sender<Text>,
) -> Result<()> {
    let (_query, mut notes) = Query::listen_channel(addr, password, cid).await?;
    while let Some(note) = notes.recv().await {
        if note.name != "notifytextmessage" {
            continue;
        }
        for record in note.records {
            if record.get("targetmode").map(String::as_str) != Some("2") {
                continue;
            }
            let Some(invoker) = record.get("invokerid").and_then(|v| v.parse().ok()) else { continue };
            let text = record.get("msg").cloned().unwrap_or_default();
            if events.send(Text { cid, generation, invoker, text }).await.is_err() {
                return Ok(());
            }
        }
    }
    bail!("ServerQuery connection closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teamspeak::query::tests::{ChannelEvent, channel_server};

    #[tokio::test]
    async fn listeners_follow_moves_share_occupied_channels_and_release_empty_ones() {
        let mut server = channel_server().await;
        let (tx, mut rx) = mpsc::channel(16);
        let mut pool = ChannelListeners::new(server.addr, "p ss".into(), tx);
        pool.reconcile([10, 10]);
        assert_eq!(pool.listeners.len(), 1);
        assert!(matches!(server.event().await, ChannelEvent::Ready { cid: 10, .. }));
        let first = pool.listeners[&10].generation;
        pool.reconcile([10, 20]);
        assert!(matches!(server.event().await, ChannelEvent::Ready { cid: 20, .. }));
        assert_eq!(pool.listeners[&10].generation, first);
        server.message(10, 2, 7, "before the move");
        let old = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
        assert!(pool.accepts(&old));
        pool.reconcile([20]);
        assert!(matches!(server.event().await, ChannelEvent::Closed(10)));
        assert!(!pool.accepts(&old));
        pool.reconcile([]);
        assert!(matches!(server.event().await, ChannelEvent::Closed(20)));
        assert!(pool.listeners.is_empty());
        pool.reconcile([10]);
        assert!(matches!(server.event().await, ChannelEvent::Ready { cid: 10, .. }));
        assert!(!pool.accepts(&old));
        drop(pool);
        assert!(matches!(server.event().await, ChannelEvent::Closed(10)));
    }

    #[tokio::test]
    async fn listeners_reconnect_and_ignore_other_text_modes() {
        let mut server = channel_server().await;
        let (tx, mut rx) = mpsc::channel(16);
        let mut pool = ChannelListeners::new(server.addr, "p ss".into(), tx);
        pool.reconcile([10]);
        assert!(matches!(server.event().await, ChannelEvent::Ready { cid: 10, .. }));
        server.disconnect(10);
        assert!(matches!(server.event().await, ChannelEvent::Closed(10)));
        assert!(matches!(server.event().await, ChannelEvent::Ready { cid: 10, .. }));
        server.message(10, 1, 7, "private");
        server.message(10, 3, 7, "server");
        server.message(10, 2, 7, "channel");
        let text = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
        assert!(pool.accepts(&text));
        assert_eq!((text.cid, text.invoker, text.text.as_str()), (10, 7, "channel"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn listener_pool_is_bounded_and_freed_capacity_is_reused() {
        let (tx, _rx) = mpsc::channel(16);
        let mut pool = ChannelListeners::new("127.0.0.1:0".parse().unwrap(), "p ss".into(), tx);
        pool.reconcile(1..=MAX_LISTENERS as u64 + 10);
        assert_eq!(pool.listeners.len(), MAX_LISTENERS);
        assert!(pool.limited);
        pool.reconcile([1, MAX_LISTENERS as u64 + 1]);
        assert_eq!(pool.listeners.len(), 2);
        assert!(!pool.limited);
        assert!(pool.listeners.contains_key(&(MAX_LISTENERS as u64 + 1)));
    }
}
