//! Our channel tree mirrored onto the TeamSpeak server. Ours is the
//! authority: TeamSpeak channels are created, edited, moved and deleted to
//! match, and the id mapping is persisted so a restart edits instead of
//! recreating.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use vc_proto::{Channel, ChannelId};

use super::query::Record;

/// TeamSpeak limits channel names to 40 characters.
pub const TS_NAME_MAX: usize = 40;

/// The parts of a TeamSpeak channel we keep in sync.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TsChannel {
    pub cid: u64,
    pub parent: u64,
    pub order: u64,
    pub name: String,
    pub topic: String,
    pub max_clients: Option<u32>,
    pub password: bool,
    pub default: bool,
}

impl TsChannel {
    pub fn from_record(r: &Record) -> Option<Self> {
        let num = |key: &str| r.get(key).and_then(|v| v.parse::<i64>().ok());
        let max = num("channel_maxclients").unwrap_or(-1);
        Some(Self {
            cid: num("cid")? as u64,
            parent: num("pid").or_else(|| num("cpid")).unwrap_or(0) as u64,
            order: num("channel_order").unwrap_or(0) as u64,
            name: r.get("channel_name").cloned().unwrap_or_default(),
            topic: r.get("channel_topic").cloned().unwrap_or_default(),
            max_clients: (max >= 0).then_some(max as u32),
            password: num("channel_flag_password") == Some(1),
            default: num("channel_flag_default") == Some(1),
        })
    }
}

/// What the TeamSpeak channel for `channel` should look like (cid unknown).
pub fn desired(channel: &Channel, parent: u64, order: u64) -> TsChannel {
    TsChannel {
        cid: 0,
        parent,
        order,
        name: channel.name.chars().take(TS_NAME_MAX).collect(),
        topic: channel.topic.clone(),
        max_clients: channel.max_clients,
        password: channel.has_password,
        default: false,
    }
}

/// `channeledit` properties turning `current` into `want` (without parent,
/// order and password, which have their own commands/handling).
pub fn edits(current: &TsChannel, want: &TsChannel) -> Vec<(&'static str, String)> {
    let mut args = Vec::new();
    // TeamSpeak rejects setting a channel's name to its current value.
    if current.name != want.name {
        args.push(("channel_name", want.name.clone()));
    }
    if current.topic != want.topic {
        args.push(("channel_topic", want.topic.clone()));
    }
    if current.max_clients != want.max_clients {
        args.extend(max_clients_args(want.max_clients));
    }
    args
}

pub fn max_clients_args(max: Option<u32>) -> Vec<(&'static str, String)> {
    match max {
        Some(max) => vec![("channel_maxclients", max.to_string()), ("channel_flag_maxclients_unlimited", "0".into())],
        None => vec![("channel_flag_maxclients_unlimited", "1".into())],
    }
}

/// Parents before children, siblings in display order.
pub fn tree_order(channels: &BTreeMap<ChannelId, Channel>) -> Vec<ChannelId> {
    let mut out = Vec::with_capacity(channels.len());
    let mut stack: Vec<ChannelId> = siblings(channels, None).into_iter().rev().collect();
    while let Some(channel) = stack.pop() {
        out.push(channel);
        stack.extend(siblings(channels, Some(channel)).into_iter().rev());
    }
    out
}

/// Children of `parent` sorted like our clients show them.
pub fn siblings(channels: &BTreeMap<ChannelId, Channel>, parent: Option<ChannelId>) -> Vec<ChannelId> {
    let mut children: Vec<_> = channels.values().filter(|c| c.parent == parent).collect();
    children.sort_by_key(|c| (c.position, c.id));
    children.into_iter().map(|c| c.id).collect()
}

/// Our channel id ↔ TeamSpeak channel id, saved as JSON.
pub struct ChannelMap {
    path: PathBuf,
    to_ts: HashMap<ChannelId, u64>,
    to_ours: HashMap<u64, ChannelId>,
}

impl ChannelMap {
    pub fn load(path: &Path) -> Result<Self> {
        let pairs: BTreeMap<ChannelId, u64> = match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data).context("invalid TeamSpeak channel map")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        let mut map = Self { path: path.to_owned(), to_ts: HashMap::new(), to_ours: HashMap::new() };
        for (ours, ts) in pairs {
            map.link(ours, ts);
        }
        Ok(map)
    }

    pub fn save(&self) -> Result<()> {
        let pairs: BTreeMap<_, _> = self.to_ts.iter().map(|(k, v)| (*k, *v)).collect();
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&pairs)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn ts(&self, ours: ChannelId) -> Option<u64> {
        self.to_ts.get(&ours).copied()
    }

    pub fn ours(&self, ts: u64) -> Option<ChannelId> {
        self.to_ours.get(&ts).copied()
    }

    pub fn link(&mut self, ours: ChannelId, ts: u64) {
        self.unlink_ours(ours);
        if let Some(previous) = self.to_ours.remove(&ts) {
            self.to_ts.remove(&previous);
        }
        self.to_ts.insert(ours, ts);
        self.to_ours.insert(ts, ours);
    }

    pub fn unlink_ours(&mut self, ours: ChannelId) -> Option<u64> {
        let ts = self.to_ts.remove(&ours)?;
        self.to_ours.remove(&ts);
        Some(ts)
    }

    pub fn retain(&mut self, keep: impl Fn(ChannelId, u64) -> bool) {
        let doomed: Vec<_> = self.to_ts.iter().filter(|(o, t)| !keep(**o, **t)).map(|(o, _)| *o).collect();
        for ours in doomed {
            self.unlink_ours(ours);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: ChannelId, parent: Option<ChannelId>, position: i32) -> Channel {
        Channel {
            id,
            parent,
            name: format!("c{id}"),
            topic: String::new(),
            position,
            has_password: false,
            max_clients: None,
        }
    }

    #[test]
    fn tree_order_puts_parents_first_and_follows_positions() {
        let channels: BTreeMap<_, _> =
            [channel(1, None, 1), channel(2, None, 0), channel(3, Some(1), 0), channel(4, Some(2), 0)]
                .into_iter()
                .map(|c| (c.id, c))
                .collect();
        assert_eq!(tree_order(&channels), vec![2, 4, 1, 3]);
    }

    #[test]
    fn edits_only_touch_changed_fields() {
        let mut c = channel(1, None, 0);
        c.name = "x".repeat(50);
        c.max_clients = Some(5);
        let want = desired(&c, 0, 0);
        assert_eq!(want.name.chars().count(), TS_NAME_MAX);
        let current = TsChannel { name: want.name.clone(), ..Default::default() };
        let args = edits(&current, &want);
        assert_eq!(args, vec![("channel_maxclients", "5".into()), ("channel_flag_maxclients_unlimited", "0".into())]);
    }

    #[test]
    fn map_links_are_one_to_one_and_persist() {
        let dir = std::env::temp_dir().join(format!("vc-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("channels.json");
        let mut map = ChannelMap::load(&path).unwrap();
        map.link(1, 10);
        map.link(2, 10);
        assert_eq!(map.ts(1), None);
        assert_eq!(map.ours(10), Some(2));
        map.save().unwrap();
        let loaded = ChannelMap::load(&path).unwrap();
        assert_eq!(loaded.ts(2), Some(10));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
