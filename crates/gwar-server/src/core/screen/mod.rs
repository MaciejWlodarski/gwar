//! Screen shares (docs/screen-share.md, "Protocol and permissions"): who shares what where, and
//! who watches it. A share belongs to its sharer's voice participation and ends with it; a watch
//! belongs to the viewer's session and is checked again whenever its access can change.
//!
//! Media runs in the screen SFU task (`media::screen`). A share is announced once its publish
//! transport connects. Each watch holds a slot of the viewer's watch transport through a
//! [`WatchGrant`], ended here the moment access ends, before the SFU hears of it.
mod guests;

pub(super) use guests::Guest;
pub use guests::{GuestMsg, PrivateWatchLinks, PrivateWatchReceipt, WatchUrl};

use super::{
    voice::{bad, stale},
    *,
};
use crate::media::screen::{Lease, PUBLISH_CONNECT, PublishSignal, ScreenCmd, ScreenOrigin, WatchGrant, publish_ssrcs};
use gwar_proto::{
    SCREEN_SLOTS, ScreenPauseReason, ScreenPreset, ScreenPublishOffer, ScreenShare, ScreenStart, ScreenStopReason,
    ScreenSurface, ScreenWatchOffer, VoiceParticipation,
};

/// Every slot reservation is charged this times its share's cap, for RTX, padding probes and
/// headers (docs/screen-share.md, "Admission and resource limits"); the egress cap enforces it.
const SLOT_CHARGE: f64 = 1.3;
/// `screen.start` per session: a burst of 4, then 4 a minute.
const STARTS_BURST: f64 = 4.0;
const STARTS_PER_SECOND: f64 = 4.0 / 60.0;
/// `screen.*.offer` per session: 4 a minute.
const OFFERS_BURST: f64 = 4.0;
const OFFERS_PER_SECOND: f64 = 4.0 / 60.0;
/// Largest SDP offer or answer.
const SDP_LIMIT: usize = 64 * 1024;

pub(super) struct Share {
    sharer: SessionId,
    channel: ChannelId,
    /// The call it was started in: an encrypted share is watched from inside it.
    call: String,
    preset: ScreenPreset,
    surface: ScreenSurface,
    /// The sharer asked for a TeamSpeak watch link (`screen.start {guest_link}`).
    guest_link: bool,
    /// The watch link was turned off, for good.
    link_revoked: bool,
    /// SHA-256 of the watch link's token while the link works (`guests::open_watch_link`).
    link: Option<[u8; 32]>,
    /// Cancels queued TeamSpeak posts as soon as the link ends.
    link_lease: Option<Arc<Lease>>,
    private_links: Option<Arc<PrivateWatchLinks>>,
    viewers: BTreeSet<SessionId>,
    /// TeamSpeak guests watching through the link; they count among the share's viewers.
    guests: BTreeSet<SessionId>,
    origin: Arc<ScreenOrigin>,
    /// The publish transport connected and the share was announced.
    live: bool,
    publish: Option<Publish>,
}

/// A share's publish transport, negotiated once.
struct Publish {
    offer: ScreenPublishOffer,
    answer: Option<String>,
}

/// A session's watch transport and what its slots carry.
pub(super) struct Watch {
    run: u64,
    sdp: String,
    browser: bool,
    h264: Vec<gwar_proto::H264Capability>,
    answer: Option<String>,
    connected: bool,
    slots: [Option<(String, Arc<WatchGrant>)>; SCREEN_SLOTS],
    /// Ends with this value: the screen task closes the transport even without `EndWatch`.
    lease: Arc<Lease>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.lease.end();
    }
}

/// How long a share may take to connect: from its start to its answer, and again from the
/// answer to the connection (the SFU's own `PUBLISH_CONNECT`, with slack).
const CONNECT_DEADLINE: Duration = Duration::from_secs(PUBLISH_CONNECT.as_secs() + 5);

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
            .filter(|(_, s)| s.live && self.admitted(viewer, s.channel))
            .map(|(id, _)| self.share_view(id))
            .collect()
    }

    /// The share `session` names, if it can see its channel and the share is live, or it is
    /// the session's own: hidden, unknown, not yet announced and malformed ids answer the same.
    fn visible_share(&self, session: SessionId, id: &str) -> Result<&Share, ErrorBody> {
        let share = self.shares.get(id).ok_or_else(not_found)?;
        let own = share.sharer == session;
        if own || (share.live && self.admitted(Some(&self.sessions[&session]), share.channel)) {
            Ok(share)
        } else {
            Err(not_found())
        }
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

    fn take_offer(&mut self, session: SessionId) -> Result<(), ErrorBody> {
        self.screen_offers
            .entry(session)
            .or_insert_with(|| voice::Bucket::new(OFFERS_PER_SECOND, OFFERS_BURST))
            .take()
            .map_err(|_| err(ErrorCode::RateLimited, "screen transports are negotiated too often"))
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
        if self.sessions[&session].out.is_none() {
            return Err(bad("remote session has no WebRTC transport"));
        }
        if self.sessions[&session].sharing.is_some() {
            return Err(err(ErrorCode::Conflict, "you are already sharing"));
        }
        let limits = self.screen_limits;
        if self.shares.values().filter(|s| s.channel == channel).count() >= limits.shares_per_channel as usize {
            return Err(err(ErrorCode::Busy, "this channel has as many shares as allowed"));
        }
        if self.shares.len() >= limits.shares_per_server as usize {
            return Err(err(ErrorCode::Busy, "the server has as many shares as allowed"));
        }
        let cap = limits.share_cap_kbps as u64 * 1000;
        // A share's ingress costs CPU too: its cap is reserved for as long as it runs.
        self.reclaim_guest_reservations(cap, None)?;
        let id = files::random_id(16);
        let private = self.channels.get(&channel).is_some_and(|c| c.private);
        let share = Share {
            sharer: session,
            channel,
            call: binding.call,
            preset: start.preset,
            surface: start.surface,
            guest_link: start.guest_link,
            link_revoked: false,
            link: None,
            link_lease: None,
            private_links: None,
            viewers: BTreeSet::new(),
            guests: BTreeSet::new(),
            origin: ScreenOrigin::new(session, id.clone(), channel, false, private, cap),
            live: false,
            publish: None,
        };
        self.shares.insert(id.clone(), share);
        self.sessions.get_mut(&session).expect("checked by caller").sharing = Some(id.clone());
        // Announced when the publish transport connects. One never negotiated ends here; one
        // negotiated gets its own deadline from its answer.
        self.connect_deadline(&id, false);
        Ok(Response::ScreenShare { share: id })
    }

    /// Ends share `id` with `transport` unless, `CONNECT_DEADLINE` from now, it is live, or
    /// (for the deadline set at its start, `answered` false) its transport was answered.
    fn connect_deadline(&self, id: &str, answered: bool) {
        let Some(tx) = self.me.upgrade() else { return };
        let share = id.to_owned();
        tokio::spawn(async move {
            tokio::time::sleep(CONNECT_DEADLINE).await;
            let check = move |core: &mut Core| {
                let Some(s) = core.shares.get(&share) else { return };
                let negotiated = s.publish.as_ref().is_some_and(|p| p.answer.is_some());
                if !s.live && (answered || !negotiated) {
                    let sharer = s.sharer;
                    core.end_share(&share, ScreenStopReason::Transport, None);
                    core.client_updated(sharer);
                }
            };
            let _ = tx.send(CoreMsg::Resume(Box::new(check))).await;
        });
    }

    /// Negotiates the share's publish transport, once.
    pub(super) fn screen_publish_offer(
        &mut self,
        session: SessionId,
        id: u32,
        offer: ScreenPublishOffer,
    ) -> Option<Reply> {
        let checked = (|| {
            let share = self.visible_share(session, &offer.share)?;
            if share.sharer != session {
                return Err(err(ErrorCode::Forbidden, "this is not your share"));
            }
            if let Some(publish) = &share.publish {
                if publish.offer != offer {
                    return Err(err(ErrorCode::Conflict, "the share's transport is already negotiated"));
                }
                return publish
                    .answer
                    .clone()
                    .map(|sdp| Some(Response::ScreenAnswer { sdp }))
                    .ok_or_else(|| err(ErrorCode::Unavailable, "negotiation pending"));
            }
            if offer.sdp.len() > SDP_LIMIT {
                return Err(err(ErrorCode::TooLarge, "SDP too large"));
            }
            if publish_ssrcs(&offer.sdp) != Some((offer.ssrc, offer.rtx_ssrc)) {
                return Err(bad("offer must carry exactly one video stream with its RTX, as named"));
            }
            Ok(None)
        })();
        match checked {
            Err(e) => return Some(Err(e)),
            Ok(Some(answer)) => return Some(Ok(answer)),
            Ok(None) => {}
        }
        if let Err(e) = self.take_offer(session) {
            return Some(Err(e));
        }
        let Ok(permit) = self.screen.clone().try_reserve_owned() else {
            return Some(Err(err(ErrorCode::Unavailable, "screen engine busy")));
        };
        let share = self.shares.get_mut(&offer.share).expect("checked above");
        share.publish = Some(Publish { offer: offer.clone(), answer: None });
        let (reply, rx) = oneshot::channel();
        permit.send(ScreenCmd::Publish { origin: share.origin.clone(), ssrc: offer.ssrc, sdp: offer.sdp, reply });
        let tx = self.me.upgrade()?;
        let name = offer.share;
        tokio::spawn(async move {
            let answer = negotiated(rx).await;
            let resume = move |core: &mut Core| {
                let Some(share) = core.shares.get_mut(&name) else { return };
                let result = match answer {
                    Ok(sdp) => {
                        if let Some(publish) = &mut share.publish {
                            publish.answer = Some(sdp.clone());
                        }
                        core.connect_deadline(&name, true);
                        Ok(Response::ScreenAnswer { sdp })
                    }
                    Err(message) => {
                        core.end_share(&name, ScreenStopReason::Transport, None);
                        core.client_updated(session);
                        Err(bad(message))
                    }
                };
                core.reply(session, id, result);
            };
            let _ = tx.send(CoreMsg::Resume(Box::new(resume))).await;
        });
        None
    }

    /// Negotiates the session's watch transport. A greater run replaces it and ends its watches.
    pub(super) fn screen_watch_offer(&mut self, session: SessionId, id: u32, offer: ScreenWatchOffer) -> Option<Reply> {
        if self.sessions[&session].out.is_none() {
            return Some(Err(bad("remote session has no WebRTC transport")));
        }
        let s = &self.sessions[&session];
        if offer.run < s.watch_run {
            return Some(Err(stale()));
        }
        if offer.run == s.watch_run {
            // The run's transport failed or closed: retired for good.
            let Some(watch) = &s.watch else { return Some(Err(stale())) };
            if watch.sdp != offer.sdp || watch.browser != offer.browser || watch.h264 != offer.h264 {
                return Some(Err(err(ErrorCode::Conflict, "run reused with a different offer")));
            }
            return Some(
                watch
                    .answer
                    .clone()
                    .map(|sdp| Response::ScreenAnswer { sdp })
                    .ok_or_else(|| err(ErrorCode::Unavailable, "negotiation pending")),
            );
        }
        if offer.sdp.len() > SDP_LIMIT {
            return Some(Err(err(ErrorCode::TooLarge, "SDP too large")));
        }
        if let Err(e) = self.take_offer(session) {
            return Some(Err(e));
        }
        let Ok(permit) = self.screen.clone().try_reserve_owned() else {
            return Some(Err(err(ErrorCode::Unavailable, "screen engine busy")));
        };
        self.drop_watches(session, false);
        let lease = Lease::new();
        let s = self.sessions.get_mut(&session).expect("checked above");
        s.watch_run = offer.run;
        s.watch = Some(Watch {
            run: offer.run,
            sdp: offer.sdp.clone(),
            browser: offer.browser,
            h264: offer.h264.clone(),
            answer: None,
            connected: false,
            slots: Default::default(),
            lease: lease.clone(),
        });
        let (reply, rx) = oneshot::channel();
        permit.send(ScreenCmd::Watch {
            session,
            run: offer.run,
            lease,
            slots: SCREEN_SLOTS,
            sdp: offer.sdp,
            browser: offer.browser,
            h264: offer.h264.clone(),
            reply,
        });
        let tx = self.me.upgrade()?;
        let run = offer.run;
        tokio::spawn(async move {
            let answer = negotiated(rx).await;
            let resume = move |core: &mut Core| {
                let Some(watch) = core.sessions.get_mut(&session).and_then(|s| s.watch.as_mut()) else { return };
                if watch.run != run {
                    return;
                }
                let result = match answer {
                    Ok(sdp) => {
                        watch.answer = Some(sdp.clone());
                        Ok(Response::ScreenAnswer { sdp })
                    }
                    Err(message) => {
                        core.sessions.get_mut(&session).expect("found above").watch = None;
                        Err(bad(message))
                    }
                };
                core.reply(session, id, result);
            };
            let _ = tx.send(CoreMsg::Resume(Box::new(resume))).await;
        });
        None
    }

    /// The screen SFU reports on a publish transport.
    pub(super) fn screen_published(&mut self, id: &str, signal: PublishSignal) {
        let Some(share) = self.shares.get_mut(id) else { return };
        match signal {
            PublishSignal::Connected => {
                if std::mem::replace(&mut share.live, true) {
                    return;
                }
                let (sharer, channel) = (share.sharer, share.channel);
                if let Some(s) = self.sessions.get_mut(&sharer) {
                    s.shared = true;
                }
                let audience = self.share_audience(channel);
                self.broadcast(Event::ScreenStarted(self.share_view(id)), |s| audience.contains(&s.id));
                self.client_updated(sharer);
                // The link, asked for at the start, exists only from the announcement on.
                self.open_watch_link(id);
            }
            PublishSignal::Ended(reason) => {
                let sharer = share.sharer;
                self.end_share(id, reason, None);
                self.client_updated(sharer);
            }
        }
    }

    /// The screen SFU reports on a watch transport: once it is gone its watches end.
    pub(super) fn screen_watching(&mut self, session: SessionId, run: u64, connected: bool) {
        if self.guests.contains_key(&session) {
            self.guest_watching(session, run, connected);
            return;
        }
        let Some(watch) = self.sessions.get_mut(&session).and_then(|s| s.watch.as_mut()) else { return };
        if watch.run != run {
            return;
        }
        watch.connected = connected;
        if !connected {
            self.drop_watches(session, true);
            self.sessions.get_mut(&session).expect("found above").watch = None;
        }
    }

    /// The screen SFU paused (`reason`) or resumed (`None`) a viewer's slot: the viewer hears of
    /// it only while the exact assignment is still current and alive on that transport.
    /// Refuse just this assignment. Delayed failures cannot stop a newer watch of the share.
    pub(super) fn screen_incompatible(&mut self, session: SessionId, run: u64, grant: &Arc<WatchGrant>) {
        if self.guests.contains_key(&session) {
            self.guest_incompatible(session, run, grant);
            return;
        }
        let current = self.sessions.get(&session).and_then(|s| s.watch.as_ref()).is_some_and(|w| {
            w.run == run && grant.alive() && w.slots.iter().flatten().any(|(_, g)| Arc::ptr_eq(g, grant))
        });
        if !current {
            return;
        }
        let id = &grant.share;
        self.free_slot(session, id, true);
        if let Some(share) = self.shares.get_mut(id) {
            share.viewers.remove(&session);
        }
        self.send_event(session, Event::ScreenStopped { share: id.clone(), reason: ScreenStopReason::Codec, by: None });
        self.send_viewers(id);
    }

    pub(super) fn screen_paused(
        &mut self,
        session: SessionId,
        run: u64,
        grant: &Arc<WatchGrant>,
        reason: Option<ScreenPauseReason>,
    ) {
        if self.guests.contains_key(&session) {
            self.guest_paused(session, run, grant, reason);
            return;
        }
        let watching = self.sessions.get(&session).and_then(|s| s.watch.as_ref()).is_some_and(|w| {
            w.run == run && grant.alive() && w.slots.iter().flatten().any(|(_, g)| Arc::ptr_eq(g, grant))
        });
        if !watching {
            return;
        }
        let share = grant.share.clone();
        let event = match reason {
            Some(reason) => Event::ScreenPaused { share, reason },
            None => Event::ScreenResumed { share },
        };
        self.send_event(session, event);
    }

    /// Ends every watch of `session` (its transport is replaced or gone), telling it per slot
    /// when `tell`.
    fn drop_watches(&mut self, session: SessionId, tell: bool) {
        let watched: Vec<String> =
            self.shares.iter().filter(|(_, s)| s.viewers.contains(&session)).map(|(id, _)| id.clone()).collect();
        for id in watched {
            self.free_slot(session, &id, tell);
            self.shares.get_mut(&id).expect("listed above").viewers.remove(&session);
            self.send_viewers(&id);
        }
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
        self.turn_off_link(id);
        Ok(Response::Empty {})
    }

    /// Who hears of a share's watch link (`screen.link`): its sharer, and once the share is
    /// announced the channel's moderators. Before that only its sharer knows of it.
    fn link_audience(&self, id: &str) -> Vec<SessionId> {
        let share = &self.shares[id];
        let (sharer, channel, live) = (share.sharer, share.channel, share.live);
        self.share_audience(channel)
            .into_iter()
            .filter(|&s| s == sharer || (live && self.can_in(s, channel, Permission::MuteMembers)))
            .collect()
    }

    /// The bytes per second all reservations hold: every running share's cap, and 1.3 times
    /// its cap for each of its viewers. Reserved by cap, not by current rate, so static content
    /// cannot be admitted cheaply and then grow.
    pub(super) fn screen_reserved(&self) -> u64 {
        self.shares
            .values()
            .map(|s| {
                s.origin.cap_bps
                    + (SLOT_CHARGE * s.origin.cap_bps as f64) as u64
                        * (s.viewers.len() + self.reserved_guests(s)) as u64
            })
            .sum()
    }

    fn screen_budget(&self) -> u64 {
        self.screen_limits.budget_mbps as u64 * 1_000_000
    }

    /// Watches `id` in a free slot of the session's watch transport. Watching a share again
    /// answers its slot and, if the slot is paused, tries again (Retry).
    pub(super) fn screen_watch(&mut self, session: SessionId, id: &str) -> Reply {
        let share = self.visible_share(session, id)?;
        if share.sharer == session {
            return Err(err(ErrorCode::BadRequest, "you cannot watch your own share"));
        }
        self.may_watch(session, share)?;
        let watching = share.viewers.contains(&session);
        let charge = (SLOT_CHARGE * share.origin.cap_bps as f64) as u64;
        let Some(watch) = self.sessions[&session].watch.as_ref().filter(|w| w.connected) else {
            return Err(err(ErrorCode::Unavailable, "connect a watch transport first"));
        };
        if watching {
            let slot = watch.slots.iter().position(|s| s.as_ref().is_some_and(|(share, _)| share == id));
            let _ = self.screen.try_send(ScreenCmd::Retry { session, share: id.to_owned() });
            return Ok(Response::ScreenSlot { slot: slot.expect("a watch holds a slot") as u32 });
        }
        if self.egress_stopped() {
            return Err(err(ErrorCode::Busy, "this month's outbound transfer allowance is used up"));
        }
        let Some(slot) = watch.slots.iter().position(Option::is_none) else {
            return Err(err(ErrorCode::Busy, "you are watching as many shares as allowed"));
        };
        let run = watch.run;
        let grant = WatchGrant::new(share.origin.clone());
        // Room for the assignment first: nothing is recorded that the SFU never hears of.
        let Ok(permit) = self.screen.clone().try_reserve_owned() else {
            return Err(err(ErrorCode::Unavailable, "screen engine busy"));
        };
        self.reclaim_guest_reservations(charge, Some(id))?;
        let watch = self.sessions.get_mut(&session).and_then(|s| s.watch.as_mut()).expect("checked above");
        watch.slots[slot] = Some((id.to_owned(), grant.clone()));
        self.shares.get_mut(id).expect("checked above").viewers.insert(session);
        permit.send(ScreenCmd::Assign { session, run, slot: slot as u32, grant });
        self.send_event(session, Event::ScreenSlot { run, slot: slot as u32, share: Some(id.to_owned()) });
        self.send_viewers(id);
        Ok(Response::ScreenSlot { slot: slot as u32 })
    }

    pub(super) fn screen_unwatch(&mut self, session: SessionId, id: &str) -> Reply {
        self.visible_share(session, id)?;
        if self.shares.get_mut(id).expect("checked above").viewers.remove(&session) {
            self.free_slot(session, id, true);
            self.send_viewers(id);
        }
        Ok(Response::Empty {})
    }

    /// The share on the viewer's stage.
    pub(super) fn screen_focus(&mut self, session: SessionId, id: &str) -> Reply {
        if !self.visible_share(session, id)?.viewers.contains(&session) {
            return Err(err(ErrorCode::NotFound, "you are not watching this share"));
        }
        let _ = self.screen.try_send(ScreenCmd::Focus { session, share: id.to_owned() });
        Ok(Response::Empty {})
    }

    pub(super) fn screen_decode_failed(
        &mut self,
        session: SessionId,
        share: &str,
        slot: u32,
        run: u64,
        generation: u16,
    ) -> Reply {
        let current = self.sessions.get(&session).and_then(|s| s.watch.as_ref()).is_some_and(|w| {
            w.run == run
                && w.slots
                    .get(slot as usize)
                    .and_then(Option::as_ref)
                    .is_some_and(|(id, grant)| id == share && grant.alive())
        });
        if !current {
            return Err(stale());
        }
        self.screen
            .try_send(ScreenCmd::DecodeFailed { session, run, slot, generation, share: share.into() })
            .map_err(|_| err(ErrorCode::Unavailable, "screen engine busy"))?;
        Ok(Response::Empty {})
    }

    fn current_screen_grant(&self, session: SessionId, run: u64, grant: &Arc<WatchGrant>) -> bool {
        let watch = self
            .sessions
            .get(&session)
            .and_then(|s| s.watch.as_ref())
            .or_else(|| self.guests.get(&session).and_then(|g| g.watch.as_ref()));
        watch.is_some_and(|w| {
            w.run == run && grant.alive() && w.slots.iter().flatten().any(|(_, g)| Arc::ptr_eq(g, grant))
        })
    }

    pub(super) fn screen_decode_keyframe(
        &mut self,
        session: SessionId,
        run: u64,
        slot: u32,
        generation: u16,
        grant: &Arc<WatchGrant>,
    ) {
        if !self.current_screen_grant(session, run, grant) {
            return;
        }
        let event = Event::ScreenKeyframe { share: grant.share.clone(), run, slot, generation };
        if self.guests.contains_key(&session) {
            self.send_guest_event(session, event);
        } else {
            self.send_event(session, event);
        }
    }

    pub(super) fn screen_recover(&mut self, origin: &Arc<ScreenOrigin>, plan: gwar_proto::ScreenRecovery) {
        let Some(share) = self.shares.get_mut(&origin.share) else { return };
        if !origin.alive() || !Arc::ptr_eq(&share.origin, origin) {
            return;
        }
        // Only an SFU-authorized recovery/restoration reopens this one-shot negotiation.
        share.publish = None;
        let sharer = share.sharer;
        self.send_event(sharer, Event::ScreenRecover(plan));
        self.connect_deadline(&origin.share, false);
    }

    /// A viewer's decryptor lost sync: a keyframe, within the viewer's budget.
    pub(super) fn screen_keyframe(&mut self, session: SessionId, id: &str) -> Reply {
        if !self.visible_share(session, id)?.viewers.contains(&session) {
            return Err(err(ErrorCode::NotFound, "you are not watching this share"));
        }
        let _ = self.screen.try_send(ScreenCmd::Keyframe { session, share: id.to_owned() });
        Ok(Response::Empty {})
    }

    /// Releases the slot that carries `id` for `session`: its grant ends now, the SFU wipes it
    /// next, and the viewer hears of it when `tell`.
    fn free_slot(&mut self, session: SessionId, id: &str, tell: bool) {
        let Some(watch) = self.sessions.get_mut(&session).and_then(|s| s.watch.as_mut()) else { return };
        let Some(slot) = watch.slots.iter().position(|s| s.as_ref().is_some_and(|(share, _)| share == id)) else {
            return;
        };
        let (_, grant) = watch.slots[slot].take().expect("found above");
        grant.end();
        let run = watch.run;
        let _ = self.screen.try_send(ScreenCmd::Release { session, run, slot: slot as u32 });
        if tell {
            self.send_event(session, Event::ScreenSlot { run, slot: slot as u32, share: None });
        }
    }

    /// Tells the sharer how many watch their share.
    fn send_viewers(&mut self, id: &str) {
        let Some(share) = self.shares.get(id) else { return };
        let (sharer, members, guests) = (share.sharer, share.viewers.len() as u32, share.guests.len() as u32);
        self.send_event(sharer, Event::ScreenViewers { share: id.to_owned(), members, guests });
    }

    /// Ends a share: for everyone who can see its channel once it was announced, else for its
    /// sharer alone. The caller updates the sharer's `Client.sharing` (a move already does).
    fn end_share(&mut self, id: &str, reason: ScreenStopReason, by: Option<String>) {
        let Some(share) = self.shares.get(id) else { return };
        share.origin.end();
        let viewers: Vec<_> = share.viewers.iter().copied().collect();
        for viewer in viewers {
            self.free_slot(viewer, id, true);
        }
        // Its guests are told and let go; the link stops working and the bridge forgets it.
        self.close_watch_link(id, guests::LinkEnd::Ended(reason));
        let share = self.shares.remove(id).expect("found above");
        let _ = self.screen.try_send(ScreenCmd::EndPublish { share: id.to_owned() });
        if let Some(s) = self.sessions.get_mut(&share.sharer) {
            s.sharing = None;
            s.shared = false;
        }
        let event = Event::ScreenStopped { share: id.to_owned(), reason, by };
        if share.live {
            let audience = self.share_audience(share.channel);
            self.broadcast(event, |s| audience.contains(&s.id));
        } else {
            self.send_event(share.sharer, event);
        }
    }

    /// Ends one viewer's watch whose access is gone, exactly like `screen.unwatch`, and says so.
    fn end_watch(&mut self, session: SessionId, id: &str) {
        let Some(share) = self.shares.get_mut(id) else { return };
        if !share.viewers.remove(&session) {
            return;
        }
        self.free_slot(session, id, true);
        self.send_event(
            session,
            Event::ScreenStopped { share: id.to_owned(), reason: ScreenStopReason::Access, by: None },
        );
        self.send_viewers(id);
    }

    /// Checks again every watch of a share whose channel is `within`, and every sharer there for
    /// `share_screen` (docs/screen-share.md, "Requests": beside `enforce_connect`, same scope).
    pub(super) fn enforce_watch_where(&mut self, within: impl Fn(&Self, ChannelId) -> bool) {
        // A channel that stopped being bridged (now private) keeps no watch link.
        self.recheck_watch_links();
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
        self.screen_offers.remove(&session);
        self.drop_watches(session, false);
        if self.sessions.get_mut(&session).and_then(|s| s.watch.take()).is_some() {
            let _ = self.screen.try_send(ScreenCmd::EndWatch { session });
        }
    }

    /// `session` can no longer see `channel`: its shares are gone for it, watched or not. Runs
    /// for every session that lost it before any of them is taken out of it, so each hears of
    /// each share once.
    pub(super) fn screen_hidden(&mut self, session: SessionId, channel: ChannelId) {
        let ids: Vec<String> =
            self.shares.iter().filter(|(_, s)| s.channel == channel).map(|(id, _)| id.clone()).collect();
        for id in ids {
            let share = self.shares.get_mut(&id).expect("listed above");
            let live = share.live;
            if share.viewers.remove(&session) {
                self.free_slot(session, &id, true);
                self.send_viewers(&id);
            }
            if live {
                self.send_event(
                    session,
                    Event::ScreenStopped { share: id, reason: ScreenStopReason::Access, by: None },
                );
            }
        }
    }

    /// `session` can see `channel` now: it learns of the shares running there.
    pub(super) fn screen_shown(&mut self, session: SessionId, channel: ChannelId) {
        let shares: Vec<_> = self
            .shares
            .iter()
            .filter(|(_, s)| s.live && s.channel == channel)
            .map(|(id, _)| self.share_view(id))
            .collect();
        for share in shares {
            self.send_event(session, Event::ScreenStarted(share));
        }
    }
}

/// Waits for the SFU's answer to an offer.
async fn negotiated(rx: oneshot::Receiver<Result<String, &'static str>>) -> Result<String, &'static str> {
    tokio::time::timeout(Duration::from_secs(30), rx)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(Err("media negotiation stopped"))
        .and_then(|sdp| if sdp.len() <= SDP_LIMIT { Ok(sdp) } else { Err("answer too large") })
}
