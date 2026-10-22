//! TeamSpeak watch links and their guests (docs/screen-share.md, "TeamSpeak watch links").
//!
//! A share in a bridged plaintext channel whose sharer asked for it gets one link once it is
//! announced: 32 random bytes from the OS, in base64url, after `#` in the URL. The core keeps
//! only SHA-256 of the token, mapped to the share; the bridge keeps the URL to post it and to
//! tell late joiners, and nothing else ever holds it. The link dies with the share, when it is
//! turned off, when a server setting rules it out, and when the bridge detaches.
//!
//! A guest is one `/ws/watch` connection that presented a working token: no identity, no
//! session, one share. Its id comes from the session id space, so the screen SFU serves its
//! one-slot watch transport like a member's.
use super::*;
use crate::media::screen::GUEST_SLOTS;
use rand::{TryRngCore, rngs::OsRng};

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

/// A guest connection's requests, after its `guest.watch` (docs/screen-share.md, "Guest
/// sessions"). The gateway answers `ping` itself and closes a connection that sends anything
/// else, or a second offer.
#[derive(Debug)]
pub enum GuestMsg {
    /// The first frame. The core queues the reply on `out` and answers the guest's id, or `None`
    /// when it refused (the error is queued too).
    Watch {
        id: u32,
        token: String,
        out: OutboundTx,
        reply: oneshot::Sender<Option<SessionId>>,
    },
    Offer {
        guest: SessionId,
        id: u32,
        sdp: String,
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
    watch: Option<Watch>,
    /// Admission order: when 3b's budget reclaims guest reservations for a member, the newest
    /// guest goes first.
    #[allow(dead_code)]
    since: u64,
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
        self.shares.get_mut(id).expect("found above").link = Some(hash);
        self.watch_links.insert(hash, id.to_owned());
        let url = WatchUrl(format!("{base}{token}"));
        let share = id.to_owned();
        self.notify_bridge(|| BridgeNote::WatchLink { share, session, channel, url });
        let audience = self.link_audience(id);
        self.broadcast(Event::ScreenLink { share: id.to_owned(), active: true }, |s| audience.contains(&s.id));
    }

    /// Stops share `id`'s watch link: the token stops working, the bridge forgets the link and
    /// every guest is told why and let go, all before this returns.
    pub(super) fn close_watch_link(&mut self, id: &str, end: LinkEnd) {
        let Some(share) = self.shares.get_mut(id) else { return };
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
            GuestMsg::Watch { id, token, out, reply } => {
                let admitted = match self.admit_guest(&token, out.clone()) {
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
            GuestMsg::Offer { guest, id, sdp } => self.guest_offer(guest, id, sdp),
            GuestMsg::Keyframe { guest, id, share } => {
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
    fn admit_guest(&mut self, token: &str, out: OutboundTx) -> Result<(SessionId, Response), ErrorBody> {
        let not_found = || err(ErrorCode::NotFound, "this link does not work");
        if token.len() > 64 {
            return Err(not_found());
        }
        let hash = token_hash(token);
        let id = self.watch_links.get(&hash).ok_or_else(not_found)?.clone();
        let share = self.shares.get(&id).ok_or_else(not_found)?;
        if share.link != Some(hash) || share.link_revoked || !share.live || !self.link_possible(share.channel) {
            return Err(not_found());
        }
        if share.guests.len() >= GUESTS_PER_SHARE {
            return Err(err(ErrorCode::Busy, "this share has as many guests as allowed"));
        }
        if share.viewers.len() + share.guests.len() >= VIEWERS_PER_SHARE {
            return Err(err(ErrorCode::Busy, "this share has as many viewers as allowed"));
        }
        let (sharer, preset) = (share.sharer, share.preset);
        let guest = self.allocate_session();
        let since = self.next_connection();
        self.guests.insert(guest, Guest { share: id.clone(), out, watch: None, since });
        self.shares.get_mut(&id).expect("found above").guests.insert(guest);
        self.send_viewers(&id);
        let sharer = self.sessions.get(&sharer).map(|s| s.nickname.clone()).unwrap_or_default();
        Ok((guest, Response::Watching { share: id, sharer, preset, ice_servers: self.ice_servers.clone() }))
    }

    /// `guest.offer`: the guest's one-slot watch transport, negotiated once.
    fn guest_offer(&mut self, guest: SessionId, id: u32, sdp: String) {
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
        let lease = Lease::new();
        let g = self.guests.get_mut(&guest).expect("found above");
        g.watch = Some(Watch {
            run: GUEST_RUN,
            sdp: sdp.clone(),
            answer: None,
            connected: false,
            slots: Default::default(),
            lease: lease.clone(),
        });
        let (reply, rx) = oneshot::channel();
        permit.send(ScreenCmd::Watch { session: guest, run: GUEST_RUN, lease, slots: GUEST_SLOTS, sdp, reply });
        let Some(tx) = self.me.upgrade() else { return };
        tokio::spawn(async move {
            let answer = negotiated(rx).await;
            let resume = move |core: &mut Core| {
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
        let Some(g) = self.guests.get(&guest) else { return };
        let Some(watch) = g.watch.as_ref().filter(|w| w.run == run) else { return };
        let assigned = watch.slots[0].is_some();
        if !connected {
            self.drop_guest(guest);
            return;
        }
        if assigned {
            return;
        }
        let Some(share) = self.shares.get(&self.guests[&guest].share) else { return };
        let grant = WatchGrant::new(share.origin.clone());
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

    /// Lets a guest go: its slot and transport end now, and dropping its queue closes the
    /// connection once what is queued is sent.
    pub(in crate::core) fn drop_guest(&mut self, guest: SessionId) {
        let Some(g) = self.guests.remove(&guest) else { return };
        if let Some(watch) = g.watch {
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
                    surface: ScreenSurface::Screen,
                    guest_link: true,
                    link_revoked: false,
                    link: Some(hash),
                    viewers: (10_000..10_000 + viewers).collect(),
                    guests: BTreeSet::new(),
                    origin: ScreenOrigin::new(sharer, "S".into(), channel, false, false),
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
        let admitted = core.guest_watch(1, token.into(), out).await;
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

        // 15 members and 10 guests: a member's watch finds the share full too.
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
                answer: None,
                connected: true,
                slots: Default::default(),
                lease: Lease::new(),
            });
            core.screen_watch(member, "S")
        })
        .await;
        let refused = watched.unwrap_err();
        assert_eq!(
            (refused.code, refused.message.as_str()),
            (ErrorCode::Busy, "this share has as many viewers as allowed")
        );
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
}
