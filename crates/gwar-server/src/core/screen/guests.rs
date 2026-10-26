//! TeamSpeak watch links and their guests (docs/screen-share.md, "TeamSpeak watch links").
//!
//! A share in a bridged plaintext channel whose sharer asked for it gets one link once it is
//! announced: 32 random bytes from the OS, in base64url, after `#` in the URL. The core keeps
//! only SHA-256 of the token, mapped to the share; the bridge keeps that URL for the channel
//! post. Private delivery mints separate tokens bound to a TS UID and membership, activated only
//! at the ordered Query acknowledgement, and checked at redemption and throughout the watch.
//! Every token shares the link lease. The link dies with the share, when it is turned off, when
//! a server setting rules it out, and when the bridge detaches.
//!
//! A guest is one `/ws/watch` connection that presented a working token: no identity, no
//! session, one share. Its id comes from the session id space, so the screen SFU serves its
//! one-slot watch transport like a member's.
use super::*;
use crate::media::screen::GUEST_SLOTS;
use rand::{TryRngCore, rngs::OsRng};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

/// Guests per share (`guests_per_share`): a leaked link cannot crowd out members.
pub(in crate::core) const GUESTS_PER_SHARE: usize = 10;
/// A guest's only watch transport run: one `guest.offer` per connection.
const GUEST_RUN: u64 = 1;

/// A watch link, which holds its token: it never shows in logs.
#[derive(Clone, PartialEq, Eq)]
pub struct WatchUrl(pub String);

impl std::fmt::Debug for WatchUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WatchUrl(..)")
    }
}

/// At most this many private tokens can remain live for one share.
const PRIVATE_TOKENS_PER_SHARE: usize = 64;

/// Private links share the channel link's lease and require the same uninterrupted
/// TeamSpeak UID/channel membership at acknowledgement, redemption and while watching.
/// Only hashes are retained; minting alone never authorizes redemption.
pub struct PrivateWatchLinks {
    base: String,
    lease: Arc<Lease>,
    tokens: Mutex<HashMap<[u8; 32], PrivateToken>>,
}

struct PrivateToken {
    uid: String,
    receipt: Arc<PrivateWatchReceipt>,
}

/// A private delivery's acknowledgement and authorization. Contains no bearer token.
/// Delivery success survives departure for receipt deduplication; authorization does not.
pub struct PrivateWatchReceipt {
    confirmed: AtomicBool,
    active: AtomicBool,
    lease: Arc<Lease>,
    membership: Arc<Lease>,
}

impl PrivateWatchReceipt {
    /// Called only when the ordered Query reader consumes the final message's success reply.
    /// Its preceding notifications must already have invalidated any departed membership.
    pub fn acknowledge(&self) {
        self.active.store(self.lease.alive() && self.membership.alive(), Ordering::Release);
        self.confirmed.store(true, Ordering::Release);
    }

    pub fn confirmed(&self) -> bool {
        self.confirmed.load(Ordering::Acquire)
    }

    fn membership(&self) -> Option<Arc<Lease>> {
        (self.active.load(Ordering::Acquire) && self.lease.alive() && self.membership.alive())
            .then(|| self.membership.clone())
    }
}

impl std::fmt::Debug for PrivateWatchLinks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateWatchLinks(..)")
    }
}

impl PrivateWatchLinks {
    pub fn new(base: String, lease: Arc<Lease>) -> Arc<Self> {
        Arc::new(Self { base, lease, tokens: Mutex::default() })
    }

    pub fn lease(&self) -> Arc<Lease> {
        self.lease.clone()
    }

    pub fn mint(&self, uid: &str, membership: Arc<Lease>) -> Option<(WatchUrl, Arc<PrivateWatchReceipt>)> {
        if uid.is_empty() || !self.lease.alive() || !membership.alive() {
            return None;
        }
        let mut tokens = self.tokens.lock().unwrap();
        tokens.retain(|_, token| token.receipt.membership.alive());
        if tokens.len() >= PRIVATE_TOKENS_PER_SHARE {
            return None;
        }
        let mut raw = [0u8; 32];
        OsRng.try_fill_bytes(&mut raw).ok()?;
        let token = URL_SAFE_NO_PAD.encode(raw);
        let receipt = Arc::new(PrivateWatchReceipt {
            confirmed: AtomicBool::new(false),
            active: AtomicBool::new(false),
            lease: self.lease.clone(),
            membership,
        });
        tokens.insert(token_hash(&token), PrivateToken { uid: uid.to_owned(), receipt: receipt.clone() });
        Some((WatchUrl(format!("{}{token}", self.base)), receipt))
    }

    fn membership(&self, hash: &[u8; 32]) -> Option<Arc<Lease>> {
        self.tokens.lock().unwrap().get(hash).filter(|token| !token.uid.is_empty())?.receipt.membership()
    }
}

/// A guest connection's requests, after its `guest.watch` (docs/screen-share.md, "Guest
/// sessions"). The gateway answers `ping` itself and closes a connection that sends anything
/// else, or a second offer.
#[derive(Debug)]
pub enum GuestMsg {
    /// The first frame. The core queues the reply on `out` and answers the guest's id, or `None`
    /// when it refused (the error is queued too). The core holds `held` exactly as long as the
    /// guest: dropping it tells the gateway to stop writing at once, even into a socket the
    /// guest no longer reads.
    Watch {
        id: u32,
        token: WatchToken,
        out: OutboundTx,
        held: oneshot::Sender<()>,
        reply: oneshot::Sender<Option<SessionId>>,
    },
    Offer {
        guest: SessionId,
        id: u32,
        sdp: String,
        browser: bool,
        h264: Vec<gwar_proto::H264Capability>,
    },
    DecodeFailed {
        guest: SessionId,
        id: u32,
        share: String,
        slot: u32,
        run: u64,
        generation: u16,
    },
    Keyframe {
        guest: SessionId,
        id: u32,
        share: String,
    },
    /// The connection closed.
    Gone {
        guest: SessionId,
    },
}

pub(in crate::core) struct Guest {
    share: String,
    out: OutboundTx,
    /// Dropped with the guest: the gateway's writer stops (see [`GuestMsg::Watch`]).
    _held: oneshot::Sender<()>,
    pub(super) watch: Option<Watch>,
    /// Private guests retain the original TeamSpeak membership after admission.
    membership: Option<Arc<Lease>>,
    /// Shared with the transport and its cancellation waiter, even before an offer arrives.
    lease: Arc<Lease>,
    /// Admission order: when 3b's budget reclaims guest reservations for a member, the newest
    /// guest goes first.
    since: u64,
    /// Admission-budget pause order. A paused guest keeps its connection, but no reservation
    /// or live grant; it must pass the current limits again before receiving media.
    paused: Option<u64>,
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.lease.end();
    }
}

/// Why a share's watch link stops working, which its guests are told.
#[derive(Debug, Clone, Copy)]
pub(super) enum LinkEnd {
    /// The share ended: `screen.stopped`.
    Ended(ScreenStopReason),
    /// The link was turned off: `screen.link {active: false}`.
    TurnedOff,
}

impl Core {
    /// Whether a share in `channel` may have a working watch link now (docs/screen-share.md,
    /// "When a link exists"): the channel is bridged (a bridge is attached, the channel is
    /// plaintext and not private), `file_access` is `link`, the server has a public URL and
    /// `screen_guest_links` is on. Encrypted channels are refused here even though they are
    /// never bridged: a second check.
    pub(in crate::core) fn link_possible(&self, channel: ChannelId) -> bool {
        self.watch_base.is_some()
            && self.info.screen_guest_links
            && self.info.file_access == FileAccess::Link
            && self.bridge.as_ref().is_some_and(|b| !b.is_closed())
            && self.channels.get(&channel).is_some_and(|c| !c.encrypted && !c.private)
    }

    /// Makes the watch link of share `id`, just announced, when its sharer asked for one and one
    /// is possible; the bridge posts it.
    pub(super) fn open_watch_link(&mut self, id: &str) {
        let Some(share) = self.shares.get(id) else { return };
        if !share.guest_link || share.link_revoked || share.link.is_some() || !share.live {
            return;
        }
        if !self.link_possible(share.channel) {
            return;
        }
        let Some(base) = self.watch_base.clone() else { return };
        let mut raw = [0u8; 32];
        if let Err(e) = OsRng.try_fill_bytes(&mut raw) {
            warn!("watch link: no randomness from the OS: {e}");
            return;
        }
        let token = URL_SAFE_NO_PAD.encode(raw);
        let hash = token_hash(&token);
        let (session, channel) = (share.sharer, share.channel);
        let lease = Lease::new();
        let share = self.shares.get_mut(id).expect("found above");
        share.link = Some(hash);
        share.link_lease = Some(lease.clone());
        let private_links = PrivateWatchLinks::new(base.clone(), lease.clone());
        share.private_links = Some(private_links.clone());
        self.watch_links.insert(hash, id.to_owned());
        let url = WatchUrl(format!("{base}{token}"));
        let share = id.to_owned();
        self.notify_bridge(|| BridgeNote::WatchLink { share, session, channel, url, private_links });
        let audience = self.link_audience(id);
        self.broadcast(Event::ScreenLink { share: id.to_owned(), active: true }, |s| audience.contains(&s.id));
    }

    /// Stops share `id`'s watch link: the token stops working, the bridge forgets the link and
    /// every guest is told why and let go, all before this returns.
    pub(super) fn close_watch_link(&mut self, id: &str, end: LinkEnd) {
        let Some(share) = self.shares.get_mut(id) else { return };
        if let Some(lease) = share.link_lease.take() {
            lease.end();
        }
        share.private_links = None;
        let guests = std::mem::take(&mut share.guests);
        if let Some(hash) = share.link.take() {
            self.watch_links.remove(&hash);
            let share = id.to_owned();
            self.notify_bridge(|| BridgeNote::WatchLinkGone { share });
        }
        let event = match end {
            // Guests learn nothing of who stopped it.
            LinkEnd::Ended(reason) => Event::ScreenStopped { share: id.to_owned(), reason, by: None },
            LinkEnd::TurnedOff => Event::ScreenLink { share: id.to_owned(), active: false },
        };
        let frame = encode(&ServerFrame::Event(event));
        for guest in guests {
            if let Some(g) = self.guests.get(&guest) {
                let _ = g.out.try_send(Outbound::Frame(frame.clone()));
            }
            self.drop_guest(guest);
        }
    }

    /// Turns share `id`'s watch link off for good, if one was asked for: by its sharer or a
    /// moderator (`screen.link.revoke`), or because the link is no longer possible.
    pub(super) fn turn_off_link(&mut self, id: &str) {
        let Some(share) = self.shares.get_mut(id) else { return };
        // Only a link that was asked for has anyone to tell.
        if std::mem::replace(&mut share.link_revoked, true) || !share.guest_link {
            return;
        }
        let had_guests = !share.guests.is_empty();
        self.close_watch_link(id, LinkEnd::TurnedOff);
        let audience = self.link_audience(id);
        self.broadcast(Event::ScreenLink { share: id.to_owned(), active: false }, |s| audience.contains(&s.id));
        if had_guests {
            self.send_viewers(id);
        }
    }

    /// Turns off every working link a setting or a channel change has ruled out.
    pub(in crate::core) fn recheck_watch_links(&mut self) {
        let ruled_out: Vec<String> = self
            .shares
            .iter()
            .filter(|(_, s)| s.link.is_some() && !self.link_possible(s.channel))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ruled_out {
            self.turn_off_link(&id);
        }
    }

    /// The bridge detached or was replaced: its copies of the links are gone, and so are they.
    pub(in crate::core) fn turn_off_all_links(&mut self) {
        let linked: Vec<String> =
            self.shares.iter().filter(|(_, s)| s.link.is_some()).map(|(id, _)| id.clone()).collect();
        for id in linked {
            self.turn_off_link(&id);
        }
    }

    pub(in crate::core) fn guest_msg(&mut self, msg: GuestMsg) {
        match msg {
            GuestMsg::Watch { id, token, out, held, reply } => {
                let admitted = match self.admit_guest(&token.0, out.clone(), held) {
                    Ok((guest, response)) => {
                        let _ = out.try_send(Outbound::Frame(encode(&ServerFrame::Ok { re: id, ok: response })));
                        Some(guest)
                    }
                    Err(err) => {
                        let _ = out.try_send(Outbound::Frame(encode(&ServerFrame::Err { re: id, err })));
                        None
                    }
                };
                if reply.send(admitted).is_err()
                    && let Some(guest) = admitted
                {
                    self.drop_guest(guest);
                }
            }
            GuestMsg::Offer { guest, id, sdp, browser, h264 } => self.guest_offer(guest, id, sdp, browser, h264),
            GuestMsg::DecodeFailed { guest, id, share, slot, run, generation } => {
                if !self.guest_current(guest) {
                    return;
                }
                let current = slot == 0
                    && self.guests.get(&guest).is_some_and(|g| {
                        g.share == share
                            && g.watch.as_ref().is_some_and(|w| {
                                w.run == run && w.slots[0].as_ref().is_some_and(|(_, grant)| grant.alive())
                            })
                    });
                let result = if current {
                    self.screen
                        .try_send(ScreenCmd::DecodeFailed { session: guest, share, slot, run, generation })
                        .map(|_| Response::Empty {})
                        .map_err(|_| err(ErrorCode::Unavailable, "screen engine busy"))
                } else {
                    Err(stale())
                };
                self.reply_guest(guest, id, result);
            }
            GuestMsg::Keyframe { guest, id, share } => {
                if !self.guest_current(guest) {
                    return;
                }
                let Some(g) = self.guests.get(&guest) else { return };
                let watching = g.share == share && g.watch.as_ref().is_some_and(|w| w.slots[0].is_some());
                let result = if watching {
                    let _ = self.screen.try_send(ScreenCmd::Keyframe { session: guest, share });
                    Ok(Response::Empty {})
                } else {
                    Err(err(ErrorCode::NotFound, "you are not watching this share"))
                };
                self.reply_guest(guest, id, result);
            }
            GuestMsg::Gone { guest } => self.drop_guest(guest),
        }
    }

    /// `guest.watch`: one generic `not_found` for an unknown, ended or turned-off token alike.
    fn admit_guest(
        &mut self,
        token: &str,
        out: OutboundTx,
        held: oneshot::Sender<()>,
    ) -> Result<(SessionId, Response), ErrorBody> {
        let not_found = || err(ErrorCode::NotFound, "this link does not work");
        if token.len() > 64 {
            return Err(not_found());
        }
        let hash = token_hash(token);
        let (id, membership) = self
            .watch_links
            .get(&hash)
            .map(|id| (id.clone(), None))
            .or_else(|| {
                self.shares.iter().find_map(|(id, share)| {
                    share.private_links.as_ref()?.membership(&hash).map(|membership| (id.clone(), Some(membership)))
                })
            })
            .ok_or_else(not_found)?;
        let share = self.shares.get(&id).ok_or_else(not_found)?;
        let valid = membership.as_ref().map_or(share.link == Some(hash), |membership| membership.alive());
        if !valid || share.link_revoked || !share.live || !self.link_possible(share.channel) {
            return Err(not_found());
        }
        if share.guests.len() >= GUESTS_PER_SHARE {
            return Err(err(ErrorCode::Busy, "this share has as many guests as allowed"));
        }
        if share.viewers.len() + self.reserved_guests(share) >= self.screen_limits.viewers_per_share as usize {
            return Err(err(ErrorCode::Busy, "this share has as many viewers as allowed"));
        }
        let charge = (SLOT_CHARGE * share.origin.cap_bps as f64) as u64;
        if self.screen_reserved() + charge > self.screen_budget() {
            return Err(err(ErrorCode::Busy, "the server's screen-sharing budget is full"));
        }
        if self.egress_stopped() {
            return Err(err(ErrorCode::Busy, "this month's outbound transfer allowance is used up"));
        }
        let (sharer, preset) = (share.sharer, share.preset);
        let guest = self.allocate_session();
        let since = self.next_connection();
        let lease = Lease::new();
        if let Some(membership) = membership.clone() {
            let lease = lease.clone();
            let core = self.me.clone();
            tokio::spawn(async move {
                tokio::select! {
                    biased;
                    _ = lease.ended() => return,
                    _ = membership.ended() => {}
                }
                // The grant already fences media; close the transport before awaiting core capacity.
                lease.end();
                if let Some(core) = core.upgrade() {
                    let _ = core.send(CoreMsg::Guest(GuestMsg::Gone { guest })).await;
                }
            });
        }
        self.guests.insert(
            guest,
            Guest { share: id.clone(), out, _held: held, watch: None, membership, lease, since, paused: None },
        );
        self.shares.get_mut(&id).expect("found above").guests.insert(guest);
        self.send_viewers(&id);
        let sharer = self.sessions.get(&sharer).map(|s| s.nickname.clone()).unwrap_or_default();
        Ok((
            guest,
            Response::Watching { share: id, sharer, preset, ice_servers: self.ice_servers.clone(), screen_h264: true },
        ))
    }

    /// Fence requests and delayed media callbacks even if membership cancellation has not
    /// reached the core yet.
    fn guest_current(&mut self, guest: SessionId) -> bool {
        let current = self
            .guests
            .get(&guest)
            .is_some_and(|g| g.lease.alive() && g.membership.as_ref().is_none_or(|membership| membership.alive()));
        if !current {
            self.drop_guest(guest);
        }
        current
    }

    /// `guest.offer`: the guest's one-slot watch transport, negotiated once.
    fn guest_offer(
        &mut self,
        guest: SessionId,
        id: u32,
        sdp: String,
        browser: bool,
        h264: Vec<gwar_proto::H264Capability>,
    ) {
        if !self.guest_current(guest) {
            return;
        }
        let Some(g) = self.guests.get(&guest) else { return };
        if g.watch.is_some() {
            // The gateway closes a connection on its second offer; nothing to answer.
            return;
        }
        if sdp.len() > SDP_LIMIT {
            self.reply_guest(guest, id, Err(err(ErrorCode::TooLarge, "SDP too large")));
            self.drop_guest(guest);
            return;
        }
        let Ok(permit) = self.screen.clone().try_reserve_owned() else {
            self.reply_guest(guest, id, Err(err(ErrorCode::Unavailable, "screen engine busy")));
            self.drop_guest(guest);
            return;
        };
        let g = self.guests.get_mut(&guest).expect("found above");
        let lease = g.lease.clone();
        g.watch = Some(Watch {
            run: GUEST_RUN,
            sdp: sdp.clone(),
            browser,
            h264: h264.clone(),
            answer: None,
            connected: false,
            slots: Default::default(),
            lease: lease.clone(),
        });
        let (reply, rx) = oneshot::channel();
        permit.send(ScreenCmd::Watch {
            session: guest,
            run: GUEST_RUN,
            lease,
            slots: GUEST_SLOTS,
            sdp,
            browser,
            h264: h264.clone(),
            reply,
        });
        let Some(tx) = self.me.upgrade() else { return };
        tokio::spawn(async move {
            let answer = negotiated(rx).await;
            let resume = move |core: &mut Core| {
                if !core.guest_current(guest) {
                    return;
                }
                let Some(watch) = core.guests.get_mut(&guest).and_then(|g| g.watch.as_mut()) else { return };
                match answer {
                    Ok(sdp) => {
                        watch.answer = Some(sdp.clone());
                        core.reply_guest(guest, id, Ok(Response::ScreenAnswer { sdp }));
                    }
                    Err(message) => {
                        core.reply_guest(guest, id, Err(bad(message)));
                        core.drop_guest(guest);
                    }
                }
            };
            let _ = tx.send(CoreMsg::Resume(Box::new(resume))).await;
        });
    }

    /// The SFU reports on a guest's transport: connected, it carries the share in its slot;
    /// gone, the guest is let go.
    pub(super) fn guest_watching(&mut self, guest: SessionId, run: u64, connected: bool) {
        if !self.guest_current(guest) {
            return;
        }
        let Some(g) = self.guests.get(&guest) else { return };
        let Some(watch) = g.watch.as_ref().filter(|w| w.run == run) else { return };
        let assigned = watch.slots[0].is_some();
        if !connected {
            self.drop_guest(guest);
            return;
        }
        self.guests.get_mut(&guest).unwrap().watch.as_mut().unwrap().connected = true;
        let g = &self.guests[&guest];
        if assigned || g.paused.is_some() {
            return;
        }
        let Some(share) = self.shares.get(&g.share) else { return };
        let Some(link) = share.link_lease.clone() else {
            self.drop_guest(guest);
            return;
        };
        let grant = WatchGrant::guest(share.origin.clone(), link, g.membership.clone());
        // Room for the assignment first: nothing is recorded that the SFU never hears of.
        let Ok(permit) = self.screen.clone().try_reserve_owned() else {
            self.drop_guest(guest);
            return;
        };
        let g = self.guests.get_mut(&guest).expect("found above");
        let watch = g.watch.as_mut().expect("found above");
        watch.connected = true;
        watch.slots[0] = Some((g.share.clone(), grant.clone()));
        permit.send(ScreenCmd::Assign { session: guest, run, slot: 0, grant });
    }

    /// Guests holding a reservation, including those still negotiating their transport.
    pub(super) fn reserved_guests(&self, share: &Share) -> usize {
        share.guests.iter().filter(|id| self.guests.get(id).is_some_and(|g| g.paused.is_none())).count()
    }

    /// Plan the whole reclamation before changing anything. An impossible or otherwise
    /// refused member request must not interrupt guests. Per-share slots are reclaimed on
    /// that share first; the remaining server budget comes from the newest guests anywhere.
    pub(super) fn reclaim_guest_reservations(&mut self, charge: u64, within: Option<&str>) -> Result<(), ErrorBody> {
        let mut reserved = self.screen_reserved();
        let mut candidates: Vec<_> = self
            .guests
            .iter()
            .filter(|(_, g)| g.paused.is_none())
            .map(|(&id, g)| (id, g.since, g.share.clone()))
            .collect();
        candidates.sort_unstable_by_key(|(_, since, _)| std::cmp::Reverse(*since));
        let mut reclaim = Vec::new();
        if let Some(id) = within {
            let share = &self.shares[id];
            let mut viewers = share.viewers.len() + self.reserved_guests(share);
            for (guest, _, _) in candidates.iter().filter(|(_, _, share)| share == id) {
                if viewers < self.screen_limits.viewers_per_share as usize {
                    break;
                }
                reclaim.push(*guest);
                reserved -= (SLOT_CHARGE * share.origin.cap_bps as f64) as u64;
                viewers -= 1;
            }
            if viewers >= self.screen_limits.viewers_per_share as usize {
                return Err(err(ErrorCode::Busy, "this share has as many viewers as allowed"));
            }
        }
        for (guest, _, share) in candidates {
            if reserved + charge <= self.screen_budget() {
                break;
            }
            if !reclaim.contains(&guest) {
                reserved -= (SLOT_CHARGE * self.shares[&share].origin.cap_bps as f64) as u64;
                reclaim.push(guest);
            }
        }
        if reserved + charge > self.screen_budget() {
            return Err(err(ErrorCode::Busy, "the server's screen-sharing budget is full"));
        }
        for guest in reclaim {
            let order = self.next_connection();
            let g = self.guests.get_mut(&guest).expect("listed above");
            g.paused = Some(order);
            if let Some(watch) = &mut g.watch
                && let Some((_, grant)) = watch.slots[0].take()
            {
                grant.end();
                // The grant fences media even if the command queue is full.
                let _ = self.screen.try_send(ScreenCmd::Release { session: guest, run: watch.run, slot: 0 });
            }
            let share = g.share.clone();
            self.send_guest_event(guest, Event::ScreenPaused { share, reason: ScreenPauseReason::Budget });
        }
        Ok(())
    }

    /// Retry admission-budget pauses every five seconds, oldest pause first. Reuse the
    /// negotiated transport, but issue a fresh grant only after all current limits pass.
    pub(in crate::core) fn resume_guests(&mut self) {
        let mut paused: Vec<_> = self.guests.iter().filter_map(|(&id, g)| g.paused.map(|order| (order, id))).collect();
        paused.sort_unstable();
        for (_, guest) in paused {
            if !self.guest_current(guest) {
                continue;
            }
            let g = &self.guests[&guest];
            let Some(share) = self.shares.get(&g.share) else {
                self.drop_guest(guest);
                continue;
            };
            if !share.live
                || !share.origin.alive()
                || !self.link_possible(share.channel)
                || share.link_lease.as_ref().is_none_or(|lease| !lease.alive())
            {
                self.drop_guest(guest);
                continue;
            }
            let charge = (SLOT_CHARGE * share.origin.cap_bps as f64) as u64;
            if self.egress_stopped()
                || self.screen_reserved() + charge > self.screen_budget()
                || share.viewers.len() + self.reserved_guests(share) >= self.screen_limits.viewers_per_share as usize
                || share.guests.len() > GUESTS_PER_SHARE
            {
                continue;
            }
            let id = g.share.clone();
            let run = g.watch.as_ref().filter(|w| w.connected).map(|w| w.run);
            // Restore a reservation only when the SFU can accept its assignment.
            if run.is_some() && self.screen.capacity() == 0 {
                continue;
            }
            self.guests.get_mut(&guest).unwrap().paused = None;
            if let Some(run) = run {
                self.guest_watching(guest, run, true);
            }
            self.send_guest_event(guest, Event::ScreenResumed { share: id });
        }
    }

    /// SFU notifications belong only to the guest's current, live assignment. Admission
    /// pauses are owned by the core, so delayed SFU resumes cannot overwrite them.
    pub(super) fn guest_incompatible(&mut self, guest: SessionId, run: u64, grant: &Arc<WatchGrant>) {
        if !self.guest_current(guest) {
            return;
        }
        let current = self.guests.get(&guest).and_then(|g| g.watch.as_ref()).is_some_and(|w| {
            w.run == run && grant.alive() && w.slots[0].as_ref().is_some_and(|(_, g)| Arc::ptr_eq(g, grant))
        });
        if !current {
            return;
        }
        self.send_guest_event(
            guest,
            Event::ScreenStopped { share: grant.share.clone(), reason: ScreenStopReason::Codec, by: None },
        );
        self.drop_guest(guest);
    }

    pub(super) fn guest_paused(
        &mut self,
        guest: SessionId,
        run: u64,
        grant: &Arc<WatchGrant>,
        reason: Option<ScreenPauseReason>,
    ) {
        if !self.guest_current(guest) {
            return;
        }
        let g = &self.guests[&guest];
        if g.share != grant.share
            || g.paused.is_some()
            || !grant.alive()
            || !g
                .watch
                .as_ref()
                .is_some_and(|w| w.run == run && w.slots[0].as_ref().is_some_and(|(_, g)| Arc::ptr_eq(g, grant)))
        {
            return;
        }
        let share = grant.share.clone();
        self.send_guest_event(
            guest,
            match reason {
                Some(reason) => Event::ScreenPaused { share, reason },
                None => Event::ScreenResumed { share },
            },
        );
    }

    pub(super) fn send_guest_event(&mut self, guest: SessionId, event: Event) {
        let Some(g) = self.guests.get(&guest) else { return };
        if g.out.try_send(Outbound::Frame(encode(&ServerFrame::Event(event)))).is_err() {
            self.drop_guest(guest);
        }
    }

    fn reply_guest(&mut self, guest: SessionId, id: u32, result: Reply) {
        let Some(g) = self.guests.get(&guest) else { return };
        let frame = match result {
            Ok(ok) => ServerFrame::Ok { re: id, ok },
            Err(err) => ServerFrame::Err { re: id, err },
        };
        if g.out.try_send(Outbound::Frame(encode(&frame))).is_err() {
            // A guest that falls this far behind is let go, like a member.
            self.drop_guest(guest);
        }
    }

    /// Lets a guest go: its slot and transport end now, and dropping its held sender stops
    /// the gateway writer even if its socket is blocked.
    pub(in crate::core) fn drop_guest(&mut self, guest: SessionId) {
        let Some(mut g) = self.guests.remove(&guest) else { return };
        if let Some(watch) = g.watch.take() {
            if let Some((_, grant)) = &watch.slots[0] {
                grant.end();
            }
            // Dropping the watch ends its lease: the SFU closes it even if this is lost.
            let _ = self.screen.try_send(ScreenCmd::EndWatch { session: guest });
        }
        if self.shares.get_mut(&g.share).is_some_and(|s| s.guests.remove(&guest)) {
            self.send_viewers(&g.share);
        }
    }
}

/// What the core keeps of a token.
fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::expiry_tests::{on_core, test_core_teamspeak};

    async fn session(core: &CoreHandle, seed: u8, nickname: &str) -> SessionId {
        let (out, frames) = mpsc::channel(OUTBOUND_QUEUE);
        // Kept for the test: a closed queue would end the session.
        std::mem::forget(frames);
        core.connect(ConnectRequest {
            request_id: 1,
            hello_key: String::new(),
            certificate: None,
            uid: crate::identity::uid_for_key(&[seed; 32]),
            public_key: URL_SAFE_NO_PAD.encode([seed; 32]),
            nickname: nickname.into(),
            platform: gwar_proto::Platform::Web,
            out,
            ip: None,
            device: None,
            device_expires_at: None,
            invite: None,
            password_ok: true,
        })
        .await
        .unwrap()
    }

    /// A live share "S" of a fresh session in the default channel, with a working link whose
    /// token is returned, and `viewers` made-up members watching.
    async fn linked_share(core: &CoreHandle, viewers: u32) -> String {
        let sharer = session(core, 1, "alice").await;
        on_core(core, move |core| {
            core.watch_base = Some("https://gwar.test/watch#".into());
            let channel = core.info.default_channel;
            let token = "t".repeat(43);
            let hash = token_hash(&token);
            core.shares.insert(
                "S".into(),
                Share {
                    sharer,
                    channel,
                    call: String::new(),
                    preset: ScreenPreset::Smooth,
                    spec: ScreenPreset::Smooth.into(),
                    surface: ScreenSurface::Screen,
                    guest_link: true,
                    link_revoked: false,
                    link: Some(hash),
                    link_lease: Some(Lease::new()),
                    private_links: None,
                    viewers: (10_000..10_000 + viewers).collect(),
                    guests: BTreeSet::new(),
                    origin: ScreenOrigin::new(sharer, "S".into(), channel, false, false, 8_000_000),
                    live: true,
                    publish: None,
                },
            );
            core.watch_links.insert(hash, "S".into());
            assert!(core.link_possible(channel), "a bridged plaintext channel");
            token
        })
        .await
    }

    /// `guest.watch` with `token`: the guest's id, or the error it was answered with.
    async fn guest(core: &CoreHandle, token: &str) -> Result<SessionId, ErrorBody> {
        let (out, mut frames) = mpsc::channel(OUTBOUND_QUEUE);
        let (held, _gone) = oneshot::channel();
        let admitted = core.guest_watch(1, WatchToken(token.into()), out, held).await;
        let Some(Outbound::Frame(frame)) = frames.recv().await else { panic!("a reply") };
        // Kept: a guest whose queue closes is let go.
        std::mem::forget(frames);
        match serde_json::from_str::<ServerFrame>(&frame).unwrap() {
            ServerFrame::Ok { ok: Response::Watching { share, sharer, .. }, .. } => {
                assert_eq!((share.as_str(), sharer.as_str()), ("S", "alice"));
                Ok(admitted.expect("an admitted guest has an id"))
            }
            ServerFrame::Err { err, .. } => {
                assert!(admitted.is_none());
                Err(err)
            }
            other => panic!("unexpected reply {other:?}"),
        }
    }

    async fn core_with_bridge() -> (CoreHandle, mpsc::UnboundedReceiver<BridgeNote>) {
        let store = Store::in_memory().unwrap();
        store.init_server_chat(false).unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let (notes, receiver) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        (core, receiver)
    }

    async fn private_links(core: &CoreHandle) -> Arc<PrivateWatchLinks> {
        on_core(core, |core| {
            let share = core.shares.get_mut("S").unwrap();
            let links = PrivateWatchLinks::new(core.watch_base.clone().unwrap(), share.link_lease.clone().unwrap());
            share.private_links = Some(links.clone());
            links
        })
        .await
    }

    #[tokio::test]
    async fn private_tokens_require_the_original_membership_and_share_lease() {
        for end in [LinkEnd::TurnedOff, LinkEnd::Ended(ScreenStopReason::Stopped)] {
            let (core, _notes) = core_with_bridge().await;
            let shared = linked_share(&core, 0).await;
            let links = private_links(&core).await;
            let membership = Lease::new();
            let (url, receipt) = links.mint("ts-recipient", membership.clone()).unwrap();
            let token = url.0.rsplit_once('#').unwrap().1;
            let miss = guest(&core, "unknown-token").await.unwrap_err();
            let pending = guest(&core, token).await.unwrap_err();
            assert_eq!((pending.code, pending.message), (miss.code, miss.message));
            receipt.acknowledge();
            let admitted = guest(&core, token).await.expect("acknowledged and recipient is still present");
            core.guest(GuestMsg::Gone { guest: admitted }).await;
            membership.end();
            let miss = guest(&core, "unknown-token").await.unwrap_err();
            let departed = guest(&core, token).await.unwrap_err();
            assert_eq!((departed.code, departed.message), (miss.code, miss.message));
            // Rejoining under the same UID cannot revive the revoked token.
            let (returned, returned_receipt) = links.mint("ts-recipient", Lease::new()).unwrap();
            assert_ne!(returned, url);
            assert!(guest(&core, token).await.is_err());
            let returned_token = returned.0.rsplit_once('#').unwrap().1;
            returned_receipt.acknowledge();
            assert!(guest(&core, returned_token).await.is_ok());
            on_core(&core, move |core| core.close_watch_link("S", end)).await;
            assert!(guest(&core, token).await.is_err());
            assert!(guest(&core, returned_token).await.is_err());
            assert!(guest(&core, &shared).await.is_err());
            assert!(links.mint("ts-recipient", Lease::new()).is_none());
        }
    }

    #[tokio::test]
    async fn private_token_count_is_bounded_and_uses_the_existing_guest_limit() {
        let (core, _notes) = core_with_bridge().await;
        linked_share(&core, 0).await;
        let links = private_links(&core).await;
        let membership = Lease::new();
        for i in 0..PRIVATE_TOKENS_PER_SHARE {
            let (url, receipt) = links.mint(&format!("uid-{i}"), membership.clone()).unwrap();
            receipt.acknowledge();
            if i <= GUESTS_PER_SHARE {
                let admitted = guest(&core, url.0.rsplit_once('#').unwrap().1).await;
                if i < GUESTS_PER_SHARE {
                    assert!(admitted.is_ok());
                } else {
                    assert_eq!(admitted.unwrap_err().code, ErrorCode::Busy);
                }
            }
        }
        assert!(links.mint("over-limit", Lease::new()).is_none());
        membership.end();
        assert!(links.mint("new-membership", Lease::new()).is_some());
        assert_eq!(links.tokens.lock().unwrap().len(), 1, "departed tokens release capacity");
    }

    #[cfg(feature = "teamspeak")]
    #[tokio::test]
    async fn private_tokens_wait_for_ordered_acknowledgement_before_redemption() {
        use crate::teamspeak::query::{Cmd, Query, tests::recipient_membership};
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::TcpListener,
        };
        for departure_before_ack in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (dispatched, seen) = oneshot::channel();
            let (execute, execution) = oneshot::channel();
            let peer = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let (read, mut write) = socket.into_split();
                write.write_all(b"TS3\n\rWelcome\n\r").await.unwrap();
                let mut lines = BufReader::new(read).lines();
                while let Some(line) = lines.next_line().await.unwrap() {
                    if line.starts_with("sendtextmessage ") {
                        dispatched.send((line, write)).unwrap();
                        write = execution.await.unwrap();
                        while lines.next_line().await.unwrap().is_some() {
                            if write.write_all(b"error id=0 msg=ok\n\r").await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                    if line.starts_with("clientlist") {
                        write.write_all(b"clid=7 cid=10 client_unique_identifier=recipient\n\r").await.unwrap();
                    }
                    write.write_all(b"error id=0 msg=ok\n\r").await.unwrap();
                }
            });
            let (query, mut notifications) = Query::connect(addr, "serveradmin", "p", 1).await.unwrap();
            query.call(Cmd::new("clientlist").flag("-uid")).await.unwrap();
            let membership = recipient_membership(&query, 7, "recipient", 10);
            let (core, _notes) = core_with_bridge().await;
            linked_share(&core, 0).await;
            let links = private_links(&core).await;
            let (url, receipt) = links.mint("recipient", membership.clone()).unwrap();
            let token = url.0.rsplit_once('#').unwrap().1.to_owned();
            let delivery_query = query.clone();
            let target = query.recipient(7, "recipient", 10).unwrap();
            let acknowledged = receipt.clone();
            let lease = links.lease();
            let delivery = tokio::spawn(async move {
                delivery_query
                    .call_for(
                        Cmd::new("sendtextmessage").arg("targetmode", 1).arg("target", 7).arg("msg", url.0),
                        lease,
                        target,
                        Some(acknowledged),
                    )
                    .await
            });
            let (wire, write) = seen.await.unwrap();
            assert!(wire.contains("target=7") && wire.contains(&token));
            let pending_token = token.clone();
            let write = on_core(&core, move |core| {
                let bytes: &[u8] = if departure_before_ack {
                    b"notifyclientleftview clid=7 reasonid=8\n\rnotifycliententerview clid=7 cid=20 client_unique_identifier=wrong-user\n\rerror id=0 msg=ok\n\r"
                } else {
                    b"error id=0 msg=ok\n\r"
                };
                assert_eq!(write.try_write(bytes).unwrap(), bytes.len());
                // This synchronous core turn cannot yield to Query on this single-thread
                // runtime: departure and ack bytes are on the socket but not consumed yet.
                assert!(membership.alive());
                let refused = core.admit_guest(&pending_token, mpsc::channel(1).0, oneshot::channel().0).err().unwrap();
                let miss = core.admit_guest("unknown", mpsc::channel(1).0, oneshot::channel().0).err().unwrap();
                assert_eq!((refused.code, refused.message), (miss.code, miss.message));
                write
            }).await;
            execute.send(write).unwrap();
            let result = delivery.await.unwrap();
            // A command after delivery is a barrier for the ordered acknowledgement reader.
            query.call(Cmd::new("whoami")).await.unwrap();
            assert!(receipt.confirmed(), "the success receipt survives membership cancellation");
            if departure_before_ack {
                assert!(links.membership(&token_hash(&token)).is_none());
                let refused = guest(&core, &token).await.unwrap_err();
                let miss = guest(&core, "unknown-token").await.unwrap_err();
                assert_eq!((refused.code, refused.message), (miss.code, miss.message));
                assert_eq!(notifications.recv().await.unwrap().name, "notifyclientleftview");
                let arrival = notifications.recv().await.unwrap();
                assert_eq!(arrival.records[0]["client_unique_identifier"], "wrong-user");
            } else {
                result.unwrap();
                assert!(guest(&core, &token).await.is_ok(), "acknowledged delivery authorizes the normal path");
            }
            drop(query);
            peer.await.unwrap();
        }
    }

    #[tokio::test]
    async fn guest_grants_stop_rtp_before_async_revocation_teardown() {
        for private in [false, true] {
            for authority in ["membership", "link", "share"] {
                if !private && authority == "membership" {
                    continue;
                }
                let (core, _notes) = core_with_bridge().await;
                let shared = linked_share(&core, 0).await;
                let membership = Lease::new();
                let token = if private {
                    let links = private_links(&core).await;
                    let (url, receipt) = links.mint("recipient", membership.clone()).unwrap();
                    receipt.acknowledge();
                    url.0.rsplit_once('#').unwrap().1.to_owned()
                } else {
                    shared
                };
                let guest = guest(&core, &token).await.unwrap();
                on_core(&core, move |core| {
                    let (screen, _commands) = mpsc::channel(8);
                    core.screen = screen;
                    let lease = core.guests[&guest].lease.clone();
                    core.guests.get_mut(&guest).unwrap().watch = Some(Watch {
                        run: GUEST_RUN,
                        sdp: String::new(),
                        browser: false,
                        h264: vec![],
                        answer: None,
                        connected: false,
                        slots: Default::default(),
                        lease: lease.clone(),
                    });
                    core.guest_watching(guest, GUEST_RUN, true);
                    let grant = core.guests[&guest].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
                    let share = &core.shares["S"];
                    crate::media::screen::tests::assert_guest_revocation_stops_rtp(grant, || {
                        match authority {
                            "membership" => membership.end(),
                            "link" => share.link_lease.as_ref().unwrap().end(),
                            "share" => share.origin.end(),
                            _ => unreachable!(),
                        }
                        assert!(lease.alive(), "the asynchronous teardown has not run");
                        assert!(core.guests.contains_key(&guest), "admission cleanup has not run");
                    });
                })
                .await;
            }
        }
    }

    #[tokio::test]
    async fn membership_revocation_fences_offers_and_delayed_connection_signals() {
        for connecting in [false, true] {
            let (core, _notes) = core_with_bridge().await;
            linked_share(&core, 0).await;
            let links = private_links(&core).await;
            let membership = Lease::new();
            let (url, receipt) = links.mint("recipient", membership.clone()).unwrap();
            receipt.acknowledge();
            let guest = guest(&core, url.0.rsplit_once('#').unwrap().1).await.unwrap();
            on_core(&core, move |core| {
                let lease = core.guests[&guest].lease.clone();
                if connecting {
                    core.guests.get_mut(&guest).unwrap().watch = Some(Watch {
                        run: GUEST_RUN,
                        sdp: String::new(),
                        browser: false,
                        h264: vec![],
                        answer: None,
                        connected: false,
                        slots: Default::default(),
                        lease: lease.clone(),
                    });
                }
                membership.end();
                // Do not yield to the cancellation waiter before either queued operation.
                if connecting {
                    core.guest_watching(guest, GUEST_RUN, true);
                } else {
                    core.guest_offer(guest, 2, String::new(), false, vec![]);
                }
                assert!(!core.guests.contains_key(&guest));
                assert!(core.shares["S"].guests.is_empty(), "the admission is free");
                assert!(!lease.alive(), "the watch transport cannot survive departure");
            })
            .await;
        }
    }

    #[tokio::test]
    async fn guests_and_members_share_the_viewer_limit() {
        // 16 members leave room for 9 guests: the tenth finds the share full, not the guest limit.
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 16).await;
        for _ in 0..9 {
            guest(&core, &token).await.unwrap();
        }
        let full = guest(&core, &token).await.unwrap_err();
        assert_eq!((full.code, full.message.as_str()), (ErrorCode::Busy, "this share has as many viewers as allowed"));

        // 15 members and 10 guests: a member reclaims one guest's viewer slot.
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 15).await;
        for _ in 0..GUESTS_PER_SHARE {
            guest(&core, &token).await.unwrap();
        }
        let refused = guest(&core, &token).await.unwrap_err();
        assert_eq!(refused.message, "this share has as many guests as allowed");
        let member = session(&core, 2, "bob").await;
        let watched = on_core(&core, move |core| {
            core.sessions.get_mut(&member).unwrap().watch = Some(Watch {
                run: 1,
                sdp: String::new(),
                browser: false,
                h264: vec![],
                answer: None,
                connected: true,
                slots: Default::default(),
                lease: Lease::new(),
            });
            let (screen, _commands) = mpsc::channel(8);
            core.screen = screen;
            let result = core.screen_watch(member, "S");
            assert_eq!(core.guests.values().filter(|g| g.paused.is_some()).count(), 1);
            result
        })
        .await;
        assert!(watched.is_ok(), "a member reclaims a guest's viewer slot");
    }

    #[tokio::test]
    async fn guest_admission_uses_configured_viewer_limit_and_screen_budget() {
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 0).await;
        on_core(&core, |core| core.screen_limits.viewers_per_share = 1).await;
        let admitted = guest(&core, &token).await.unwrap();
        let reserved = on_core(&core, |core| core.screen_reserved()).await;
        assert_eq!(reserved, 8_000_000 + 10_400_000);
        let refused = guest(&core, &token).await.unwrap_err();
        assert_eq!(refused.message, "this share has as many viewers as allowed");
        on_core(&core, move |core| {
            core.drop_guest(admitted);
            core.screen_limits.viewers_per_share = 25;
            core.screen_limits.budget_mbps = 18;
        })
        .await;
        let refused = guest(&core, &token).await.unwrap_err();
        assert_eq!(refused.message, "the server's screen-sharing budget is full");
        on_core(&core, |core| core.screen_limits.budget_mbps = 19).await;
        guest(&core, &token).await.unwrap();
        let refused = guest(&core, &token).await.unwrap_err();
        assert_eq!(refused.message, "the server's screen-sharing budget is full");
        let member = session(&core, 2, "bob").await;
        let watched = on_core(&core, move |core| {
            core.sessions.get_mut(&member).unwrap().watch = Some(Watch {
                run: 1,
                sdp: String::new(),
                browser: false,
                h264: vec![],
                answer: None,
                connected: true,
                slots: Default::default(),
                lease: Lease::new(),
            });
            let (screen, _commands) = mpsc::channel(8);
            core.screen = screen;
            let result = core.screen_watch(member, "S");
            assert_eq!(core.guests.values().filter(|g| g.paused.is_some()).count(), 1);
            result
        })
        .await;
        assert!(watched.is_ok(), "a member reclaims a guest's reservation");
    }

    fn connected_watch(lease: Arc<Lease>) -> Watch {
        Watch {
            run: GUEST_RUN,
            sdp: String::new(),
            browser: false,
            h264: vec![],
            answer: None,
            connected: true,
            slots: Default::default(),
            lease,
        }
    }

    fn admitted_guest(core: &mut Core, token: &str, connect: bool) -> (SessionId, mpsc::Receiver<Outbound>) {
        let (out, frames) = mpsc::channel(OUTBOUND_QUEUE);
        let (held, _gone) = oneshot::channel();
        let (id, _) = core.admit_guest(token, out, held).unwrap();
        if connect {
            let g = core.guests.get_mut(&id).unwrap();
            g.watch = Some(connected_watch(g.lease.clone()));
            core.guest_watching(id, GUEST_RUN, true);
        }
        (id, frames)
    }

    fn event(frames: &mut mpsc::Receiver<Outbound>) -> Event {
        let Outbound::Frame(frame) = frames.try_recv().expect("guest event") else { panic!("a frame") };
        let ServerFrame::Event(event) = serde_json::from_str(&frame).unwrap() else { panic!("an event") };
        event
    }

    #[tokio::test]
    async fn failed_unconnected_watch_releases_guest_and_member_reservations() {
        let (handle, _notes) = core_with_bridge().await;
        let token = linked_share(&handle, 0).await;
        let member = session(&handle, 2, "bob").await;
        on_core(&handle, move |core| {
            let (screen, _commands) = mpsc::channel(32);
            core.screen = screen;
            let (guest, _frames) = admitted_guest(core, &token, false);
            let guest_lease = core.guests[&guest].lease.clone();
            core.guests.get_mut(&guest).unwrap().watch = Some(connected_watch(guest_lease));
            core.guests.get_mut(&guest).unwrap().watch.as_mut().unwrap().connected = false;
            assert_eq!(core.reserved_guests(&core.shares["S"]), 1);
            let lease = core.guests[&guest].lease.clone();
            core.screen_watching(guest, GUEST_RUN, false);
            assert!(!core.guests.contains_key(&guest));
            assert!(!lease.alive());
            assert_eq!(core.reserved_guests(&core.shares["S"]), 0);
            let mut watch = connected_watch(Lease::new());
            let grant = WatchGrant::new(core.shares["S"].origin.clone());
            watch.slots[0] = Some(("S".into(), grant.clone()));
            core.shares.get_mut("S").unwrap().viewers.insert(member);
            core.sessions.get_mut(&member).unwrap().watch = Some(watch);
            core.screen_watching(member, GUEST_RUN, false);
            assert!(core.sessions[&member].watch.is_none());
            assert!(!grant.alive());
            assert!(!core.shares["S"].viewers.contains(&member));
        })
        .await;
    }

    #[tokio::test]
    async fn negotiations_reply_when_share_ends_or_watch_run_is_superseded() {
        let (handle, _notes) = core_with_bridge().await;
        linked_share(&handle, 0).await;
        let (publish, first_watch, mut frames, _commands) = on_core(&handle, |core| {
            let session = core.shares["S"].sharer;
            let (out, frames) = mpsc::channel(32);
            core.sessions.get_mut(&session).unwrap().out = Some(out);
            let (screen, mut commands) = mpsc::channel(32);
            core.screen = screen;
            assert!(core.screen_publish_offer(session, 71, gwar_proto::ScreenPublishOffer {
                share: "S".into(), ssrc: 1, rtx_ssrc: 2,
                sdp: "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 98 99\r\na=sendonly\r\na=ssrc:1 cname:p\r\na=ssrc:2 cname:p\r\na=ssrc-group:FID 1 2\r\n".into(),
            }).is_none());
            let ScreenCmd::Publish { reply: publish, .. } = commands.try_recv().unwrap() else { panic!("publish") };
            core.end_share("S", ScreenStopReason::Stopped, None);
            for run in [1, 2] {
                assert!(core.screen_watch_offer(session, 71 + run as u32, gwar_proto::ScreenWatchOffer {
                    run, sdp: "pending".into(), browser: false, h264: vec![],
                }).is_none());
            }
            let first = loop {
                if let ScreenCmd::Watch { reply, .. } = commands.try_recv().unwrap() { break reply; }
            };
            // Keep the newer negotiation pending while the older one completes.
            (publish, first, frames, commands)
        }).await;
        publish.send(Ok("late publish answer".into())).unwrap();
        first_watch.send(Ok("old watch answer".into())).unwrap();
        let mut replies = HashMap::new();
        while replies.len() < 2 {
            let frame = tokio::time::timeout(Duration::from_secs(2), frames.recv()).await.unwrap().unwrap();
            if let Outbound::Frame(frame) = frame
                && let ServerFrame::Err { re, err } = serde_json::from_str(&frame).unwrap()
            {
                replies.insert(re, err.code);
            }
        }
        assert_eq!(replies[&71], ErrorCode::NotFound);
        assert_eq!(replies[&72], ErrorCode::Stale);
    }

    #[tokio::test]
    async fn members_reclaim_newest_guests_across_shares_and_resume_oldest_pause_first() {
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 0).await;
        let member = session(&core, 2, "bob").await;
        on_core(&core, move |core| {
            let (screen, mut commands) = mpsc::channel(128);
            core.screen = screen;
            // The exact four-publisher/25-guest review repro: 292 Mbps reserved.
            let mut tokens = vec![token];
            for index in 1..4 {
                let id = format!("S{index}");
                let token = index.to_string().repeat(43);
                let hash = token_hash(&token);
                let s = &core.shares["S"];
                core.shares.insert(
                    id.clone(),
                    Share {
                        sharer: s.sharer,
                        channel: s.channel,
                        call: s.call.clone(),
                        preset: s.preset,
                        spec: s.spec,
                        surface: s.surface,
                        guest_link: true,
                        link_revoked: false,
                        link: Some(hash),
                        link_lease: Some(Lease::new()),
                        private_links: None,
                        viewers: BTreeSet::new(),
                        guests: BTreeSet::new(),
                        origin: ScreenOrigin::new(s.sharer, id.clone(), s.channel, false, false, 8_000_000),
                        live: true,
                        publish: None,
                    },
                );
                core.watch_links.insert(hash, id);
                tokens.push(token);
            }
            let mut guests: Vec<_> = (0..25).map(|i| admitted_guest(core, &tokens[i % 4], true)).collect();
            assert_eq!(core.screen_reserved(), 292_000_000);
            let newest = guests[24].0;
            let next = guests[23].0;
            let old = core.guests[&newest].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
            let lease = core.guests[&newest].lease.clone();
            core.sessions.get_mut(&member).unwrap().watch = Some(connected_watch(Lease::new()));
            // Leave room for the member's Assign, but none for the guest's Release.
            while core.screen.capacity() > 1 {
                core.screen.try_send(ScreenCmd::EndWatch { session: 999 }).unwrap();
            }
            crate::media::screen::tests::assert_guest_revocation_stops_rtp(old.clone(), || {
                assert!(core.screen_watch(member, "S").is_ok());
            });
            while commands.try_recv().is_ok() {}
            assert!(!old.alive(), "reclamation fences RTP even when Release could not be queued");
            assert!(lease.alive(), "the guest's transport survives the pause");
            assert!(core.guests[&newest].paused.is_some());
            assert!(matches!(event(&mut guests[24].1), Event::ScreenPaused { reason: ScreenPauseReason::Budget, .. }));
            core.screen_paused(newest, GUEST_RUN, &old, None);
            assert!(guests[24].1.try_recv().is_err(), "a delayed SFU resume cannot undo admission pause");
            assert!(core.screen_watch(member, "S1").is_ok());
            assert!(core.guests[&next].paused.is_some());
            core.resume_guests();
            assert!(core.guests[&newest].paused.is_some(), "the budget is still full");
            core.screen_unwatch(member, "S").unwrap();
            core.resume_guests();
            assert!(core.guests[&newest].paused.is_none(), "oldest pause resumes first");
            assert!(core.guests[&next].paused.is_some());
            let fresh = &core.guests[&newest].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1;
            assert!(fresh.alive() && !Arc::ptr_eq(&old, fresh));
            assert!(matches!(event(&mut guests[24].1), Event::ScreenResumed { .. }));
            core.screen_unwatch(member, "S1").unwrap();
            core.screen_limits.viewers_per_share = 5;
            core.resume_guests();
            assert!(core.guests[&next].paused.is_some(), "re-admission enforces current viewer limit");
            core.screen_limits.viewers_per_share = 25;
            core.screen_limits.monthly_egress_tb = 1;
            core.screen_limits.monthly_egress_stop = true;
            let reading = |transmitted| {
                Some(super::super::super::egress::Reading {
                    interface: "test0".into(),
                    transmitted,
                    scope: gwar_proto::EgressScope::Host,
                })
            };
            core.egress_tick(reading(0), (0, 0), now_ms()).unwrap();
            core.egress_tick(reading(1_000_000_000_000), (0, 0), now_ms()).unwrap();
            core.resume_guests();
            assert!(core.guests[&next].paused.is_some(), "monthly stop also applies to re-admission");
            core.screen_limits.monthly_egress_stop = false;
            core.resume_guests();
            assert!(core.guests[&next].paused.is_none());
            assert_eq!(core.screen_reserved(), 292_000_000);
            while commands.try_recv().is_ok() {}
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn the_five_second_timer_readmits_guests_after_capacity_is_available() {
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 0).await;
        let (guest, mut frames) = on_core(&core, move |core| {
            let (guest, frames) = admitted_guest(core, &token, false);
            core.screen_limits.budget_mbps = 19;
            core.reclaim_guest_reservations(8_000_000, None).unwrap();
            (guest, frames)
        })
        .await;
        assert!(matches!(event(&mut frames), Event::ScreenPaused { .. }));
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(on_core(&core, move |core| core.guests[&guest].paused.is_some()).await);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(on_core(&core, move |core| core.guests[&guest].paused.is_none()).await);
        assert!(matches!(event(&mut frames), Event::ScreenResumed { .. }));
    }

    #[tokio::test]
    async fn member_start_reclaims_guests_but_impossible_admissions_leave_them_alone() {
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 0).await;
        let member = session(&core, 2, "bob").await;
        on_core(&core, move |core| {
            let (guest, mut frames) = admitted_guest(core, &token, false);
            let channel = core.info.default_channel;
            core.sessions.get_mut(&member).unwrap().channel = Some(channel);
            let admission = core.voice_admission(member, channel).unwrap();
            core.voice_allocate(member, admission);
            let v = core.voice_state(member).unwrap();
            let start = || ScreenStart {
                call: v.call.clone(),
                participation: v.participation.clone(),
                preset: ScreenPreset::Smooth,
                spec: None,
                surface: ScreenSurface::Screen,
                guest_link: false,
            };
            core.screen_limits.budget_mbps = 7;
            assert_eq!(core.screen_start(member, start()).unwrap_err().code, ErrorCode::Busy);
            assert!(core.guests[&guest].paused.is_none());
            assert!(frames.try_recv().is_err(), "failed admissions do not pause anyone");
            core.screen_limits.budget_mbps = 20;
            assert!(core.screen_start(member, start()).is_ok());
            assert!(core.guests[&guest].paused.is_some());
            assert_eq!(
                core.screen_reserved(),
                20_000_000,
                "8 Mbps existing share plus the new 12 Mbps default; pending guests release reservations too"
            );
            assert!(matches!(event(&mut frames), Event::ScreenPaused { reason: ScreenPauseReason::Budget, .. }));
        })
        .await;
    }

    #[tokio::test]
    async fn paused_negotiation_waits_for_readmission_and_a_full_engine_retries_later() {
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 0).await;
        on_core(&core, move |core| {
            let (screen, mut commands) = mpsc::channel(1);
            core.screen = screen;
            let (guest, mut frames) = admitted_guest(core, &token, false);
            core.screen_limits.budget_mbps = 19;
            core.reclaim_guest_reservations(8_000_000, None).unwrap();
            let g = core.guests.get_mut(&guest).unwrap();
            g.watch = Some(connected_watch(g.lease.clone()));
            core.guest_watching(guest, GUEST_RUN, true);
            assert!(core.guests[&guest].watch.as_ref().unwrap().slots[0].is_none());
            core.screen.try_send(ScreenCmd::EndWatch { session: 999 }).unwrap();
            core.resume_guests();
            assert!(core.guests[&guest].paused.is_some(), "no assignment capacity yet");
            commands.try_recv().unwrap();
            core.resume_guests();
            assert!(core.guests[&guest].paused.is_none());
            assert!(core.guests[&guest].watch.as_ref().unwrap().slots[0].is_some());
            assert!(matches!(event(&mut frames), Event::ScreenPaused { .. }));
            assert!(matches!(event(&mut frames), Event::ScreenResumed { .. }));
        })
        .await;
    }

    #[tokio::test]
    async fn readmitted_guests_ignore_pause_and_resume_from_the_replaced_assignment() {
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 0).await;
        on_core(&core, move |core| {
            let (screen, _commands) = mpsc::channel(16);
            core.screen = screen;
            let (guest, mut frames) = admitted_guest(core, &token, true);
            let old = core.guests[&guest].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
            core.screen_limits.budget_mbps = 19;
            core.reclaim_guest_reservations(8_000_000, None).unwrap();
            assert!(!old.alive());
            assert!(matches!(event(&mut frames), Event::ScreenPaused { reason: ScreenPauseReason::Budget, .. }));
            core.resume_guests();
            let fresh = core.guests[&guest].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
            assert!(fresh.alive() && !Arc::ptr_eq(&old, &fresh));
            assert!(matches!(event(&mut frames), Event::ScreenResumed { .. }));
            for signal in [
                ScreenSignal::Paused {
                    session: guest,
                    run: GUEST_RUN,
                    grant: old.clone(),
                    reason: ScreenPauseReason::Bandwidth,
                },
                ScreenSignal::Resumed { session: guest, run: GUEST_RUN, grant: old.clone() },
                ScreenSignal::Incompatible { session: guest, run: GUEST_RUN, grant: old.clone() },
            ] {
                core.handle(CoreMsg::Screen(signal));
                assert!(frames.try_recv().is_err(), "a replaced assignment cannot overwrite re-admission status");
            }
            let unrelated = WatchGrant::new(core.shares["S"].origin.clone());
            core.screen_paused(guest, GUEST_RUN, &unrelated, Some(ScreenPauseReason::Bandwidth));
            assert!(frames.try_recv().is_err(), "a live grant for the same share is not the current assignment");
            core.screen_paused(guest, GUEST_RUN, &fresh, Some(ScreenPauseReason::Bandwidth));
            assert!(matches!(event(&mut frames), Event::ScreenPaused { reason: ScreenPauseReason::Bandwidth, .. }));
            core.screen_paused(guest, GUEST_RUN, &fresh, None);
            assert!(matches!(event(&mut frames), Event::ScreenResumed { .. }));
        })
        .await;
    }

    #[tokio::test]
    async fn rewatching_members_ignore_pause_and_resume_from_the_replaced_assignment() {
        let (core, _notes) = core_with_bridge().await;
        linked_share(&core, 0).await;
        let member = session(&core, 2, "bob").await;
        on_core(&core, move |core| {
            let (screen, _commands) = mpsc::channel(16);
            core.screen = screen;
            let (out, mut frames) = mpsc::channel(OUTBOUND_QUEUE);
            let s = core.sessions.get_mut(&member).unwrap();
            s.out = Some(out);
            s.watch = Some(connected_watch(Lease::new()));
            core.screen_watch(member, "S").unwrap();
            let old = core.sessions[&member].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
            core.screen_unwatch(member, "S").unwrap();
            core.screen_watch(member, "S").unwrap();
            let fresh = core.sessions[&member].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
            assert!(!old.alive() && fresh.alive() && !Arc::ptr_eq(&old, &fresh));
            while frames.try_recv().is_ok() {}
            for signal in [
                ScreenSignal::Paused {
                    session: member,
                    run: GUEST_RUN,
                    grant: old.clone(),
                    reason: ScreenPauseReason::Bandwidth,
                },
                ScreenSignal::Resumed { session: member, run: GUEST_RUN, grant: old.clone() },
                ScreenSignal::Incompatible { session: member, run: GUEST_RUN, grant: old.clone() },
            ] {
                core.handle(CoreMsg::Screen(signal));
                assert!(frames.try_recv().is_err(), "a replaced assignment cannot overwrite re-watch status");
            }
            let unrelated = WatchGrant::new(core.shares["S"].origin.clone());
            core.screen_paused(member, GUEST_RUN, &unrelated, Some(ScreenPauseReason::Bandwidth));
            assert!(frames.try_recv().is_err(), "a live grant for the same share is not the current assignment");
            core.screen_paused(member, GUEST_RUN, &fresh, Some(ScreenPauseReason::Bandwidth));
            assert!(matches!(event(&mut frames), Event::ScreenPaused { reason: ScreenPauseReason::Bandwidth, .. }));
            core.screen_paused(member, GUEST_RUN, &fresh, None);
            assert!(matches!(event(&mut frames), Event::ScreenResumed { .. }));
        })
        .await;
    }

    #[tokio::test]
    async fn guest_pause_events_validate_the_assignment_and_close_on_backpressure() {
        for closed in [false, true] {
            let (core, _notes) = core_with_bridge().await;
            let token = linked_share(&core, 0).await;
            on_core(&core, move |core| {
                let (screen, _commands) = mpsc::channel(8);
                core.screen = screen;
                let (guest, mut frames) = admitted_guest(core, &token, true);
                let grant = core.guests[&guest].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
                let other = WatchGrant::new(ScreenOrigin::new(1, "another-share".into(), 1, false, false, 8_000_000));
                core.screen_paused(guest, GUEST_RUN + 1, &grant, Some(ScreenPauseReason::Bandwidth));
                core.screen_paused(guest, GUEST_RUN, &other, None);
                assert!(frames.try_recv().is_err());
                core.screen_paused(guest, GUEST_RUN, &grant, Some(ScreenPauseReason::Bandwidth));
                core.screen_paused(guest, GUEST_RUN, &grant, None);
                assert!(matches!(event(&mut frames), Event::ScreenPaused { reason: ScreenPauseReason::Bandwidth, .. }));
                assert!(matches!(event(&mut frames), Event::ScreenResumed { .. }));
                assert!(frames.try_recv().is_err(), "exactly two events");
                let lease = core.guests[&guest].lease.clone();
                if closed {
                    drop(frames);
                } else {
                    for _ in 0..OUTBOUND_QUEUE {
                        core.send_guest_event(guest, Event::ScreenResumed { share: "S".into() });
                    }
                }
                core.screen_paused(guest, GUEST_RUN, &grant, Some(ScreenPauseReason::Budget));
                assert!(!core.guests.contains_key(&guest));
                assert!(!grant.alive() && !lease.alive());
                assert_eq!(core.screen_reserved(), 8_000_000);
            })
            .await;
        }
    }

    #[tokio::test]
    async fn revoked_private_guests_cannot_resume_or_emit_delayed_pause_events() {
        let (core, _notes) = core_with_bridge().await;
        linked_share(&core, 0).await;
        let links = private_links(&core).await;
        let membership = Lease::new();
        let (url, receipt) = links.mint("recipient", membership.clone()).unwrap();
        receipt.acknowledge();
        on_core(&core, move |core| {
            let (screen, _commands) = mpsc::channel(8);
            core.screen = screen;
            let (guest, mut frames) = admitted_guest(core, url.0.rsplit_once('#').unwrap().1, true);
            let grant = core.guests[&guest].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
            core.screen_limits.budget_mbps = 19;
            core.reclaim_guest_reservations(8_000_000, None).unwrap();
            assert!(matches!(event(&mut frames), Event::ScreenPaused { .. }));
            membership.end();
            core.resume_guests();
            core.screen_paused(guest, GUEST_RUN, &grant, None);
            assert!(!core.guests.contains_key(&guest));
            assert!(frames.try_recv().is_err());
            assert_eq!(core.screen_reserved(), 8_000_000);
        })
        .await;
    }

    #[test]
    fn a_guests_token_never_shows_in_debug_output() {
        let secret = "s3cr3t-Watch-Token_0123456789abcdefghijklmno";
        let (out, _frames) = mpsc::channel(1);
        let (held, _gone) = oneshot::channel();
        let (reply, _rx) = oneshot::channel();
        let msg = GuestMsg::Watch { id: 1, token: WatchToken(secret.into()), out, held, reply };
        let shown = format!("{msg:?} {msg:#?}");
        assert!(!shown.contains(secret), "{shown}");
    }

    #[tokio::test]
    async fn dropping_a_guest_tells_its_connection_at_once() {
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 0).await;
        let (out, _frames) = mpsc::channel(OUTBOUND_QUEUE);
        let (held, mut gone) = oneshot::channel();
        let guest = core.guest_watch(1, WatchToken(token.clone()), out, held).await.unwrap();
        assert!(gone.try_recv().is_err(), "held while the guest is");
        on_core(&core, move |core| core.turn_off_link("S")).await;
        assert!(core.guest_watch(2, WatchToken(token), mpsc::channel(1).0, oneshot::channel().0).await.is_none());
        assert!(matches!(gone.try_recv(), Err(oneshot::error::TryRecvError::Closed)), "guest {guest} let go");
        // A refused guest's signal goes with the refusal.
        let (held, gone) = oneshot::channel();
        assert!(core.guest_watch(3, WatchToken("unknown".into()), mpsc::channel(1).0, held).await.is_none());
        assert!(gone.await.is_err());
    }

    #[tokio::test]
    async fn guest_ids_never_collide_with_sessions() {
        let (core, _notes) = core_with_bridge().await;
        let token = linked_share(&core, 0).await;
        let guest = guest(&core, &token).await.unwrap();
        let later = session(&core, 3, "carol").await;
        assert_ne!(guest, later);
        let (guests, sessions) = on_core(&core, |core| (core.guests.len(), core.sessions.len())).await;
        assert_eq!((guests, sessions), (1, 2));
        // A guest that leaves is taken off its share.
        core.guest(GuestMsg::Gone { guest }).await;
        let viewers = on_core(&core, |core| core.shares["S"].guests.len()).await;
        assert_eq!(viewers, 0);
    }

    #[tokio::test]
    async fn ending_or_revoking_a_link_cancels_its_queued_posts_before_the_bridge_reads_its_note() {
        for end in [LinkEnd::Ended(ScreenStopReason::Stopped), LinkEnd::TurnedOff] {
            let (core, mut notes) = core_with_bridge().await;
            linked_share(&core, 0).await;
            let lease = on_core(&core, |core| {
                core.close_watch_link("S", LinkEnd::TurnedOff);
                core.open_watch_link("S");
                core.shares["S"].link_lease.as_ref().unwrap().clone()
            })
            .await;
            assert!(lease.alive());
            on_core(&core, move |core| core.close_watch_link("S", end)).await;
            assert!(!lease.alive(), "the bridge need not process the queued note to cancel its post");
            assert!(notes.try_recv().is_ok());
        }
    }

    #[tokio::test]
    async fn no_link_where_one_is_not_possible() {
        let (core, mut notes) = core_with_bridge().await;
        linked_share(&core, 0).await;
        type Case = (&'static str, fn(&mut Core));
        let cases: Vec<Case> = vec![
            ("no public URL", |core| core.watch_base = None),
            ("watch links off", |core| core.info.screen_guest_links = false),
            ("members-only files", |core| core.info.file_access = FileAccess::Members),
            ("no bridge", |core| core.bridge = None),
            ("an encrypted channel", |core| {
                let channel = core.shares["S"].channel;
                core.channels.get_mut(&channel).unwrap().encrypted = true;
            }),
            ("a private channel", |core| {
                let channel = core.shares["S"].channel;
                core.channels.get_mut(&channel).unwrap().private = true;
            }),
        ];
        for (case, change) in cases {
            let opened = on_core(&core, move |core| {
                let saved = (core.watch_base.clone(), core.info.clone(), core.bridge.clone(), core.channels.clone());
                change(core);
                let share = core.shares.get_mut("S").unwrap();
                share.link = None;
                share.link_revoked = false;
                core.open_watch_link("S");
                let opened = core.shares["S"].link.is_some();
                (core.watch_base, core.info, core.bridge, core.channels) = saved;
                opened
            })
            .await;
            assert!(!opened, "{case}: no link");
        }
        while let Ok(note) = notes.try_recv() {
            assert!(!matches!(note, BridgeNote::WatchLink { .. }), "nothing for the bridge to post");
        }
        // And with everything in place, one.
        let opened = on_core(&core, |core| {
            core.open_watch_link("S");
            core.shares["S"].link.is_some()
        })
        .await;
        assert!(opened);
    }

    #[tokio::test]
    async fn codec_refusal_ends_only_the_current_watch_for_members_and_guests() {
        for is_guest in [false, true] {
            let (core, _notes) = core_with_bridge().await;
            let token = linked_share(&core, 0).await;
            let member = session(&core, 2, "bob").await;
            on_core(&core, move |core| {
                let (screen, _commands) = mpsc::channel(16);
                core.screen = screen;
                let (viewer, mut frames, grant) = if is_guest {
                    let (guest, frames) = admitted_guest(core, &token, true);
                    let grant = core.guests[&guest].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
                    (guest, frames, grant)
                } else {
                    let (out, mut frames) = mpsc::channel(OUTBOUND_QUEUE);
                    let s = core.sessions.get_mut(&member).unwrap();
                    s.out = Some(out);
                    s.watch = Some(connected_watch(Lease::new()));
                    core.screen_watch(member, "S").unwrap();
                    while frames.try_recv().is_ok() {}
                    let grant = core.sessions[&member].watch.as_ref().unwrap().slots[0].as_ref().unwrap().1.clone();
                    (member, frames, grant)
                };
                core.screen_incompatible(viewer, GUEST_RUN + 1, &grant);
                assert!(grant.alive() && frames.try_recv().is_err());
                core.screen_incompatible(viewer, GUEST_RUN, &grant);
                assert!(!grant.alive());
                if !is_guest {
                    assert!(matches!(event(&mut frames), Event::ScreenSlot { share: None, .. }));
                }
                assert!(matches!(event(&mut frames), Event::ScreenStopped { reason: ScreenStopReason::Codec, .. }));
                assert!(core.shares["S"].origin.alive());
                assert!(!core.shares["S"].viewers.contains(&viewer));
                assert!(!core.shares["S"].guests.contains(&viewer));
            })
            .await;
        }
    }
}
