//! Voice exporters are live queries against a separately validated parent.
//! They are never epoch records or history evidence in the chat store.
use super::*;

#[derive(Debug, PartialEq, Eq)]
pub struct VoiceKey {
    pub leaf: u32,
    pub device: String,
    pub key: [u8; 32],
    pub kid: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub struct VoiceKeys {
    pub epoch: u64,
    pub own_leaf: u32,
    pub members: Vec<VoiceKey>,
}

/// SFrame carries the low 32 bits of the epoch; all state uses the full epoch.
pub fn voice_kid(epoch: u64, leaf: u32) -> u64 {
    ((epoch & 0xffff_ffff) << 32) | u64::from(leaf)
}

impl Agent {
    fn voice_parent(&self, server: &str, id: &[u8; 32], parent: &Agent) -> Result<(MlsGroup, MlsGroup)> {
        if !self.voice_only || parent.voice_only {
            return Err(Invalid("voice and parent stores"));
        }
        if self.ready || parent.ready {
            return Err(AgentError::Unpersisted);
        }
        let voice = self.mls(&group_key(server, id))?;
        let d = client_context(voice.public_group().group_context(), server)?;
        let parent_id = fixed::<32>(d.descriptor.channel_group_id.as_deref().ok_or(Invalid("voice parent"))?)
            .ok_or(Invalid("voice parent"))?;
        let key = group_key(server, &parent_id);
        let record = parent.groups.get(&key).ok_or(AgentError::Blocked)?;
        if record.status != Status::Active || record.server_pending || parent.outbox.contains_key(&key) {
            return Err(AgentError::Blocked);
        }
        let channel = parent.mls(&key)?;
        let p = client_context(channel.public_group().group_context(), server)?;
        if p.descriptor.kind != crate::Kind::Channel
            || p.descriptor.private
            || d.descriptor.conversation.strip_prefix("voice:") != p.descriptor.conversation.strip_prefix("channel:")
            || parent.live(server, &p.descriptor.conversation)?.is_none_or(|live| live.group_id() != parent_id)
        {
            return Err(Invalid("voice parent binding"));
        }
        Ok((voice, channel))
    }
    /// A missing parent member blocks sending even while the parent is catching
    /// up. Only a caller that completed synchronization may turn it into Remove.
    pub fn voice_parent_removals(
        &self,
        server: &str,
        id: &[u8; 32],
        parent: &Agent,
        synchronized: bool,
    ) -> Result<Vec<u32>> {
        if !synchronized {
            return Err(AgentError::Blocked);
        }
        let (voice, parent) = self.voice_parent(server, id, parent)?;
        let members: BTreeSet<_> = parent
            .public_group()
            .treesync()
            .full_leaves()
            .filter_map(|(_, l)| leaf_credential(l))
            .map(|c| (c.uid(), c.device_key().to_owned()))
            .collect();
        Ok(voice
            .public_group()
            .treesync()
            .full_leaves()
            .filter_map(|(i, l)| {
                let c = leaf_credential(l)?;
                (!members.contains(&(c.uid(), c.device_key().to_owned()))).then_some(i.u32())
            })
            .collect())
    }
    /// Export only the current accepted epoch, after checking the pinned parent
    /// tree and today's credentials. Historical commits use structural rules only.
    pub fn voice_keys(
        &self,
        server: &str,
        id: &[u8; 32],
        parent: &Agent,
        now: i64,
        revocations: &dyn Revocations,
    ) -> Result<VoiceKeys> {
        if !self.gate(server, id, now, revocations)?.can_send() {
            return Err(AgentError::Blocked);
        }
        let (voice, channel) = self.voice_parent(server, id, parent)?;
        let members: BTreeSet<_> = channel
            .public_group()
            .treesync()
            .full_leaves()
            .filter_map(|(_, l)| leaf_credential(l))
            .filter(|c| c.is_current(now, revocations))
            .map(|c| (c.uid(), c.device_key().to_owned()))
            .collect();
        let epoch = voice.epoch().as_u64();
        let mut keys = Vec::new();
        for (i, l) in voice.public_group().treesync().full_leaves() {
            let c = leaf_credential(l).ok_or(Invalid("voice credential"))?;
            if !members.contains(&(c.uid(), c.device_key().to_owned())) {
                return Err(AgentError::Blocked);
            }
            let key = voice
                .export_secret(self.provider.crypto(), "gwar voice v1", &i.u32().to_be_bytes(), 32)
                .map_err(err("voice exporter"))?
                .try_into()
                .map_err(err("voice key length"))?;
            keys.push(VoiceKey { leaf: i.u32(), device: c.device_key().into(), key, kid: voice_kid(epoch, i.u32()) });
        }
        Ok(VoiceKeys { epoch, own_leaf: voice.own_leaf_index().u32(), members: keys })
    }
}
