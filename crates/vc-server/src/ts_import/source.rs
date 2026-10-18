//! What the import reads from the TeamSpeak server, over ServerQuery. Only reads: nothing on
//! the source server changes.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::teamspeak::query::{Cmd, Query, QueryError, Record};

/// A group's or channel's own permission assignments: name → (value, negated).
#[derive(Debug, Clone, Default)]
pub struct Perms(pub BTreeMap<String, (i64, bool)>);

impl Perms {
    pub fn from_records(records: &[Record]) -> Self {
        Self(
            records
                .iter()
                .filter_map(|r| {
                    let name = r.get("permsid")?.clone();
                    let value = r.get("permvalue")?.parse().ok()?;
                    Some((name, (value, r.get("permnegated").is_some_and(|n| n == "1"))))
                })
                .collect(),
        )
    }

    /// A `b_` permission granted (and not negated).
    pub fn bool(&self, name: &str) -> bool {
        self.0.get(name).is_some_and(|(v, negated)| *v == 1 && !negated)
    }

    /// An `i_` power (0 when absent or negated).
    pub fn int(&self, name: &str) -> i64 {
        self.0.get(name).filter(|(_, negated)| !negated).map_or(0, |(v, _)| *v)
    }
}

/// Everything an import needs from one virtual server.
#[derive(Debug, Clone, Default)]
pub struct Source {
    pub server: Record,
    pub channels: Vec<Record>,
    pub channel_perms: BTreeMap<u64, Perms>,
    pub groups: Vec<Record>,
    pub group_perms: BTreeMap<u64, Perms>,
    /// Server group → the database ids of its members.
    pub group_members: BTreeMap<u64, Vec<u64>>,
    pub channel_group_perms: BTreeMap<u64, Perms>,
    pub channel_group_members: Vec<Record>,
    pub clients: Vec<Record>,
    pub bans: Vec<Record>,
}

/// A query's "nothing here" answers (empty lists, no permissions) as no records.
async fn list(query: &Query, cmd: Cmd) -> Result<Vec<Record>> {
    match query.call(cmd).await {
        Ok(records) => Ok(records),
        // 1281: "database empty result set".
        Err(e) if e.downcast_ref::<QueryError>().is_some_and(|q| q.id == 1281) => Ok(vec![]),
        Err(e) => Err(e),
    }
}

/// Clients per call when paging the client database.
const PAGE: usize = 200;

pub async fn read(query: &Query) -> Result<Source> {
    let mut source = Source {
        server: query.call(Cmd::new("serverinfo")).await.context("serverinfo")?.into_iter().next().unwrap_or_default(),
        ..Default::default()
    };
    source.channels = list(query, Cmd::new("channellist").flag("topic").flag("flags").flag("voice").flag("limits"))
        .await
        .context("channellist")?;
    for c in &mut source.channels {
        let Some(cid) = c.get("cid").cloned() else { continue };
        // The description is only in channelinfo.
        if let Some(info) = list(query, Cmd::new("channelinfo").arg("cid", &cid)).await?.into_iter().next() {
            for key in ["channel_description", "channel_flag_semi_permanent", "channel_needed_talk_power"] {
                if let Some(v) = info.get(key) {
                    c.entry(key.to_owned()).or_insert_with(|| v.clone());
                }
            }
        }
        let perms = list(query, Cmd::new("channelpermlist").arg("cid", &cid).flag("permsid")).await?;
        source.channel_perms.insert(cid.parse()?, Perms::from_records(&perms));
    }
    source.groups = list(query, Cmd::new("servergrouplist")).await.context("servergrouplist")?;
    for g in source.groups.clone() {
        let Some(sgid) = g.get("sgid").and_then(|s| s.parse::<u64>().ok()) else { continue };
        let perms = list(query, Cmd::new("servergrouppermlist").arg("sgid", sgid).flag("permsid")).await?;
        source.group_perms.insert(sgid, Perms::from_records(&perms));
        if g.get("type").map(String::as_str) == Some("1") {
            let members = list(query, Cmd::new("servergroupclientlist").arg("sgid", sgid)).await?;
            source.group_members.insert(sgid, members.iter().filter_map(|m| m.get("cldbid")?.parse().ok()).collect());
        }
    }
    for g in list(query, Cmd::new("channelgrouplist")).await? {
        let Some(cgid) = g.get("cgid").and_then(|s| s.parse::<u64>().ok()) else { continue };
        let perms = list(query, Cmd::new("channelgrouppermlist").arg("cgid", cgid).flag("permsid")).await?;
        source.channel_group_perms.insert(cgid, Perms::from_records(&perms));
    }
    source.channel_group_members = list(query, Cmd::new("channelgroupclientlist")).await?;
    let mut start = 0;
    loop {
        let page = list(query, Cmd::new("clientdblist").arg("start", start).arg("duration", PAGE)).await?;
        let n = page.len();
        source.clients.extend(page);
        if n < PAGE {
            break;
        }
        start += n;
    }
    source.bans = list(query, Cmd::new("banlist")).await?;
    Ok(source)
}
