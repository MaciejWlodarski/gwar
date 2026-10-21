//! Screen shares (docs/screen-share.md, "Protocol and permissions"): who shares what where, and
//! who watches it. A share belongs to its sharer's voice participation and ends with it; a watch
//! belongs to the viewer's session and is checked again whenever its access can change. No media
//! yet: the SFU's publish and watch transports (PR 3) hang off the seams marked below.
use super::*;
use gwar_proto::{ScreenPreset, ScreenShare, ScreenStart, ScreenStopReason, ScreenSurface, VoiceParticipation};

/// Sharing limits (docs/screen-share.md, "Limits"). Server settings once the SFU enforces the
/// bitrate and budget that go with them.
const SHARES_PER_CHANNEL: usize = 4;
const SHARES_PER_SERVER: usize = 32;
const VIEWERS_PER_SHARE: usize = 25;
/// Shares one session watches at once: the watch transport's slots (`SCREEN_SLOTS`).
const WATCHES_PER_SESSION: usize = 4;
/// `screen.start` per session: a burst of 4, then 4 a minute.
const STARTS_BURST: f64 = 4.0;
const STARTS_PER_SECOND: f64 = 4.0 / 60.0;

pub(super) struct Share {
    sharer: SessionId,
    channel: ChannelId,
    /// The call it was started in: an encrypted share is watched from inside it.
    call: String,
    preset: ScreenPreset,
    surface: ScreenSurface,
    /// The sharer asked for a TeamSpeak watch link; none exists before PR 6.
    guest_link: bool,
    /// The watch link was turned off, for good.
    link_revoked: bool,
    viewers: BTreeSet<SessionId>,
}

fn not_found() -> ErrorBody {
    err(ErrorCode::NotFound, "no such share")
}

impl Core {
    fn share_view(&self, id: &str) -> ScreenShare {
        let s = &self.shares[id];
        ScreenShare { share: id.to_owned(), client: s.sharer, channel: s.channel, preset: s.preset, surface: s.surface }
    }

    /// The running shares `session` can see, for `Welcome`.
    pub(super) fn shares_for(&self, session: SessionId) -> Vec<ScreenShare> {
        let viewer = Some(&self.sessions[&session]);
        self.shares
            .iter()
            .filter(|(_, s)| self.admitted(viewer, s.channel))
            .map(|(id, _)| self.share_view(id))
            .collect()
    }

    /// The share `session` names, if it can see its channel: hidden, unknown and malformed ids
    /// answer the same.
    fn visible_share(&self, session: SessionId, id: &str) -> Result<&Share, ErrorBody> {
        let share = self.shares.get(id).ok_or_else(not_found)?;
        if self.admitted(Some(&self.sessions[&session]), share.channel) { Ok(share) } else { Err(not_found()) }
    }

    /// Our sessions that can see `channel`: the audience of its shares' announcements.
    fn share_audience(&self, channel: ChannelId) -> Vec<SessionId> {
        self.sessions.values().filter(|s| s.out.is_some() && self.admitted(Some(s), channel)).map(|s| s.id).collect()
    }

    /// Whether `session` may watch `share` now (docs/screen-share.md, "Permission"): it sees the
    /// channel; in an encrypted one it is in the share's call; in a password-protected one it is
    /// inside or holds `bypass_password` there.
    fn may_watch(&self, session: SessionId, share: &Share) -> Result<(), ErrorBody> {
        let s = &self.sessions[&session];
        // TeamSpeak users watch through watch links (PR 6), not sessions.
        if s.out.is_none() || !self.admitted(Some(s), share.channel) {
            return Err(not_found());
        }
        let inside = s.channel == Some(share.channel);
        if self.is_encrypted(share.channel) {
            let in_call = self.voice_state(session).is_some_and(|v| v.channel == share.channel && v.call == share.call);
            if !in_call {
                return Err(err(ErrorCode::Forbidden, "an encrypted channel's share is watched from its call"));
            }
        }
        let locked = self.channels.get(&share.channel).is_some_and(|c| c.password_hash.is_some());
        if locked && !inside && !self.can_in(session, share.channel, Permission::BypassPassword) {
            return Err(err(ErrorCode::Forbidden, "join this channel to watch its share"));
        }
        Ok(())
    }

    pub(super) fn screen_start(&mut self, session: SessionId, start: ScreenStart) -> Reply {
        self.screen_starts
            .entry(session)
            .or_insert_with(|| voice::Bucket::new(STARTS_PER_SECOND, STARTS_BURST))
            .take()
            .map_err(|_| err(ErrorCode::RateLimited, "screen shares are started too often"))?;
        let binding = VoiceParticipation { call: start.call, participation: start.participation };
        let channel = self.voice_check(session, &binding)?;
        if self.is_encrypted(channel) {
            return Err(err(ErrorCode::Unavailable, "screen sharing in encrypted channels is not available yet"));
        }
        self.require_in(session, channel, Permission::ShareScreen)?;
        if self.sessions[&session].sharing.is_some() {
            return Err(err(ErrorCode::Conflict, "you are already sharing"));
        }
        if self.shares.values().filter(|s| s.channel == channel).count() >= SHARES_PER_CHANNEL {
            return Err(err(ErrorCode::Busy, "this channel has as many shares as allowed"));
        }
        if self.shares.len() >= SHARES_PER_SERVER {
            return Err(err(ErrorCode::Busy, "the server has as many shares as allowed"));
        }
        let id = files::random_id(16);
        let share = Share {
            sharer: session,
            channel,
            call: binding.call,
            preset: start.preset,
            surface: start.surface,
            guest_link: start.guest_link,
            link_revoked: false,
            viewers: BTreeSet::new(),
        };
        self.shares.insert(id.clone(), share);
        self.sessions.get_mut(&session).expect("checked by caller").sharing = Some(id.clone());
        // PR 3 moves this announcement to the first connection of the share's publish transport.
        let audience = self.share_audience(channel);
        self.broadcast(Event::ScreenStarted(self.share_view(&id)), |s| audience.contains(&s.id));
        self.client_updated(session);
        Ok(Response::ScreenShare { share: id })
    }

    pub(super) fn screen_stop(&mut self, session: SessionId, id: &str) -> Reply {
        if self.visible_share(session, id)?.sharer != session {
            return Err(err(ErrorCode::Forbidden, "this is not your share"));
        }
        self.end_share(id, ScreenStopReason::Stopped, None);
        self.client_updated(session);
        Ok(Response::Empty {})
    }

    /// Ends someone else's share: `mute_members` in its channel and a rank above the sharer.
    pub(super) fn screen_stop_other(&mut self, session: SessionId, id: &str) -> Reply {
        let sharer = self.visible_share(session, id)?.sharer;
        if sharer == session {
            return Err(err(ErrorCode::BadRequest, "stop your own share with screen.stop"));
        }
        self.may_moderate(session, id)?;
        let by = self.sessions[&session].nickname.clone();
        self.end_share(id, ScreenStopReason::Moderator, Some(by));
        self.client_updated(sharer);
        Ok(Response::Empty {})
    }

    /// `mute_members` in the share's channel and a rank above its sharer.
    fn may_moderate(&self, session: SessionId, id: &str) -> Result<(), ErrorBody> {
        let share = &self.shares[id];
        self.require_in(session, share.channel, Permission::MuteMembers)?;
        let sharer = &self.sessions[&share.sharer];
        if !self.outranks(session, &sharer.member, &sharer.groups) {
            return Err(err(ErrorCode::Forbidden, "this member is not below your rank"));
        }
        Ok(())
    }

    /// Turns a share's TeamSpeak watch link off for good: the sharer, or a moderator of the share.
    pub(super) fn screen_link_revoke(&mut self, session: SessionId, id: &str) -> Reply {
        if self.visible_share(session, id)?.sharer != session {
            self.may_moderate(session, id)?;
        }
        let share = self.shares.get_mut(id).expect("checked above");
        // Only a link that was asked for has anyone to tell.
        if std::mem::replace(&mut share.link_revoked, true) || !share.guest_link {
            return Ok(Response::Empty {});
        }
        // PR 6: the link's token stops working here and its guests are disconnected.
        let (sharer, channel) = (share.sharer, share.channel);
        let moderators: Vec<_> = self
            .share_audience(channel)
            .into_iter()
            .filter(|&s| s == sharer || self.can_in(s, channel, Permission::MuteMembers))
            .collect();
        let event = Event::ScreenLink { share: id.to_owned(), active: false };
        self.broadcast(event, |s| moderators.contains(&s.id));
        Ok(Response::Empty {})
    }

    pub(super) fn screen_watch(&mut self, session: SessionId, id: &str) -> Reply {
        let share = self.visible_share(session, id)?;
        if share.sharer == session {
            return Err(err(ErrorCode::BadRequest, "you cannot watch your own share"));
        }
        self.may_watch(session, share)?;
        if share.viewers.contains(&session) {
            return Ok(Response::Empty {});
        }
        if share.viewers.len() >= VIEWERS_PER_SHARE {
            return Err(err(ErrorCode::Busy, "this share has as many viewers as allowed"));
        }
        if self.shares.values().filter(|s| s.viewers.contains(&session)).count() >= WATCHES_PER_SESSION {
            return Err(err(ErrorCode::Busy, "you are watching as many shares as allowed"));
        }
        // PR 3 assigns the watch transport's slot here and answers with it.
        self.shares.get_mut(id).expect("checked above").viewers.insert(session);
        self.send_viewers(id);
        Ok(Response::Empty {})
    }

    pub(super) fn screen_unwatch(&mut self, session: SessionId, id: &str) -> Reply {
        self.visible_share(session, id)?;
        if self.shares.get_mut(id).expect("checked above").viewers.remove(&session) {
            // PR 3 frees the slot here.
            self.send_viewers(id);
        }
        Ok(Response::Empty {})
    }

    /// Tells the sharer how many watch their share.
    fn send_viewers(&mut self, id: &str) {
        let share = &self.shares[id];
        let (sharer, members) = (share.sharer, share.viewers.len() as u32);
        self.send_event(sharer, Event::ScreenViewers { share: id.to_owned(), members, guests: 0 });
    }

    /// Ends a share for everyone who can see its channel. The caller updates the sharer's
    /// `Client.sharing` (a move already does).
    fn end_share(&mut self, id: &str, reason: ScreenStopReason, by: Option<String>) {
        let Some(share) = self.shares.remove(id) else { return };
        // PR 3 closes the share's publish transport and frees its viewers' slots here.
        if let Some(s) = self.sessions.get_mut(&share.sharer) {
            s.sharing = None;
        }
        let audience = self.share_audience(share.channel);
        self.broadcast(Event::ScreenStopped { share: id.to_owned(), reason, by }, |s| audience.contains(&s.id));
    }

    /// Ends one viewer's watch whose access is gone, exactly like `screen.unwatch`, and says so.
    fn end_watch(&mut self, session: SessionId, id: &str) {
        let Some(share) = self.shares.get_mut(id) else { return };
        if !share.viewers.remove(&session) {
            return;
        }
        // PR 3 frees the slot, clears its queue and wipes its RTX cache here.
        self.send_event(
            session,
            Event::ScreenStopped { share: id.to_owned(), reason: ScreenStopReason::Access, by: None },
        );
        self.send_viewers(id);
    }

    /// Checks again every watch of a share whose channel is `within`, and every sharer there for
    /// `share_screen` (docs/screen-share.md, "Requests": beside `enforce_connect`, same scope).
    pub(super) fn enforce_watch_where(&mut self, within: impl Fn(&Self, ChannelId) -> bool) {
        let ids: Vec<String> =
            self.shares.iter().filter(|(_, s)| within(self, s.channel)).map(|(id, _)| id.clone()).collect();
        for id in ids {
            let share = &self.shares[&id];
            let sharer = &self.sessions[&share.sharer];
            if !self
                .channel_permissions(&sharer.member, &sharer.groups, share.channel)
                .contains(&Permission::ShareScreen)
            {
                let sharer = share.sharer;
                self.end_share(&id, ScreenStopReason::Access, None);
                self.client_updated(sharer);
                continue;
            }
            let lost: Vec<_> = share.viewers.iter().copied().filter(|&v| self.may_watch(v, share).is_err()).collect();
            for viewer in lost {
                self.end_watch(viewer, &id);
            }
        }
    }

    /// After `session` changed channel or left voice: its own share ends, and its watches are
    /// checked again (a password channel's watch from inside, an encrypted call's). The caller
    /// then announces the client, with `sharing` already off.
    pub(super) fn screen_moved(&mut self, session: SessionId) {
        if let Some(id) = self.sessions[&session].sharing.clone() {
            self.end_share(&id, ScreenStopReason::Left, None);
        }
        let watched: Vec<String> =
            self.shares.iter().filter(|(_, s)| s.viewers.contains(&session)).map(|(id, _)| id.clone()).collect();
        for id in watched {
            if self.may_watch(session, &self.shares[&id]).is_err() {
                self.end_watch(session, &id);
            }
        }
    }

    /// A session that is going away stops watching; its own share ended when it left voice.
    pub(super) fn screen_session_gone(&mut self, session: SessionId) {
        self.screen_starts.remove(&session);
        let watched: Vec<String> =
            self.shares.iter().filter(|(_, s)| s.viewers.contains(&session)).map(|(id, _)| id.clone()).collect();
        for id in watched {
            self.shares.get_mut(&id).expect("listed above").viewers.remove(&session);
            // PR 3 frees the slot here.
            self.send_viewers(&id);
        }
    }

    /// `session` can no longer see `channel`: its shares are gone for it, watched or not. Runs
    /// before the session is taken out of the channel, so it hears of each share once.
    pub(super) fn screen_hidden(&mut self, session: SessionId, channel: ChannelId) {
        let ids: Vec<String> =
            self.shares.iter().filter(|(_, s)| s.channel == channel).map(|(id, _)| id.clone()).collect();
        for id in ids {
            if self.shares.get_mut(&id).expect("listed above").viewers.remove(&session) {
                // PR 3 frees the slot here.
                self.send_viewers(&id);
            }
            self.send_event(session, Event::ScreenStopped { share: id, reason: ScreenStopReason::Access, by: None });
        }
    }

    /// `session` can see `channel` now: it learns of the shares running there.
    pub(super) fn screen_shown(&mut self, session: SessionId, channel: ChannelId) {
        let shares: Vec<_> =
            self.shares.iter().filter(|(_, s)| s.channel == channel).map(|(id, _)| self.share_view(id)).collect();
        for share in shares {
            self.send_event(session, Event::ScreenStarted(share));
        }
    }
}
