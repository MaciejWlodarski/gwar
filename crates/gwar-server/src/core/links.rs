//! TeamSpeak identities linked to members (docs/teamspeak-import.md, "Linked TeamSpeak
//! identities"): someone joining Gwar's TeamSpeak server with a linked identity keeps it
//! (`Session::uid`) but acts as the member (`Session::member`).

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use rand::RngCore;
use tsproto_types::crypto::EccKeyPubP256;

use super::*;
use crate::store::Link;

/// How long a claim nonce lasts.
const CLAIM_NONCE_MS: i64 = 5 * 60 * 1000;
/// At most this many identities answer `teamspeak.claimable`.
const MAX_CLAIMABLE: u32 = 5000;

/// The statement a claim signs (docs/teamspeak-import.md, "Claim").
pub(crate) fn claim_statement(server_key: &str, uid: &str, nonce: &str) -> String {
    format!("gwar teamspeak claim v1\n{server_key}\n{uid}\n{nonce}")
}

impl Core {
    /// Links `teamspeak` (`ts:<uid>`) to the member `into` and brings everyone online up to date:
    /// the member's sessions gain the roles a member `teamspeak` had, and sessions of that identity
    /// act as the member from now on.
    pub(super) fn link_teamspeak(&mut self, teamspeak: &str, into: &str) -> Result<Link, ErrorBody> {
        let before = self.admissions();
        let link = self.store.link_teamspeak(teamspeak, into, now_ms()).map_err(storage_error)?;
        let Link::Linked { member: had_member, ref overrides, .. } = link else { return Ok(link) };
        // The channels' overrides follow what the store did: moved where `into` had none, dropped otherwise.
        for channel in overrides {
            let Some(row) = self.channels.get_mut(channel) else { continue };
            let taken = row.overrides.iter().any(|o| o.user.as_deref() == Some(into));
            if taken {
                row.overrides.retain(|o| o.user.as_deref() != Some(teamspeak));
            } else {
                for o in row.overrides.iter_mut().filter(|o| o.user.as_deref() == Some(teamspeak)) {
                    o.user = Some(into.to_owned());
                }
            }
            let view = channel_view(row);
            self.broadcast_channel(Event::ChannelUpdated(view));
        }
        if had_member {
            self.broadcast(Event::MemberRemoved { uid: teamspeak.to_owned() }, |_| true);
        }
        let Some((user_id, member)) = self.store.user_by_uid(into).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such member"));
        };
        let online: Vec<_> =
            self.sessions.values().filter(|s| s.user_id == user_id || s.uid == teamspeak).map(|s| s.id).collect();
        for id in online {
            let Some(s) = self.sessions.get_mut(&id) else { continue };
            s.groups = member.groups.clone();
            if s.uid == teamspeak {
                s.user_id = user_id;
                s.member = member.uid.clone();
                s.nickname = member.nickname.clone();
            }
            self.client_updated(id);
        }
        self.broadcast(Event::MemberUpdated(member), |_| true);
        self.reconcile_admissions(before);
        self.enforce_connect_everywhere();
        self.publish_routing();
        Ok(link)
    }

    /// `member.teamspeak.unlink`: by the member, or by someone with `manage_members` ranked above
    /// them. The roles stay with the member; a session of that identity goes on as an unknown
    /// TeamSpeak user.
    pub(super) fn member_teamspeak_unlink(&mut self, session: SessionId, uid: Uid, teamspeak: Uid) -> Reply {
        let Some((_, member)) = self.store.user_by_uid(&uid).map_err(storage_error)? else {
            return Err(err(ErrorCode::NotFound, "no such member"));
        };
        if !member.teamspeak.contains(&teamspeak) {
            return Err(err(ErrorCode::NotFound, "this TeamSpeak identity is not linked to this member"));
        }
        if self.sessions[&session].uid != uid {
            self.require(session, Permission::ManageMembers)?;
            if !self.outranks(session, &uid, &member.groups) {
                return Err(err(ErrorCode::Forbidden, "this member is not below your rank"));
            }
        }
        self.unlink_teamspeak(&teamspeak)?;
        Ok(Response::Empty {})
    }

    /// Unlinks `teamspeak` and brings everyone online up to date.
    pub(super) fn unlink_teamspeak(&mut self, teamspeak: &str) -> Result<(), ErrorBody> {
        let before = self.admissions();
        let Some(member) = self.store.unlink_teamspeak(teamspeak).map_err(storage_error)? else { return Ok(()) };
        let online: Vec<_> = self.sessions.values().filter(|s| s.uid == teamspeak).map(|s| s.id).collect();
        for id in online {
            let Some(s) = self.sessions.get_mut(&id) else { continue };
            s.user_id = 0;
            s.member = s.uid.clone();
            s.groups = Vec::new();
            self.client_updated(id);
        }
        if let Some((_, member)) = self.store.user_by_uid(&member).map_err(storage_error)? {
            self.broadcast(Event::MemberUpdated(member), |_| true);
        }
        self.reconcile_admissions(before);
        self.enforce_connect_everywhere();
        self.publish_routing();
        Ok(())
    }

    /// `teamspeak.challenge`: a fresh nonce for this session, replacing any earlier one.
    pub(super) fn teamspeak_challenge(&mut self, session: SessionId) -> Reply {
        let s = self.sessions.get_mut(&session).expect("requesting session");
        if s.out.is_none() {
            return Err(err(ErrorCode::Forbidden, "TeamSpeak sessions do not claim"));
        }
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let nonce = URL_SAFE_NO_PAD.encode(bytes);
        s.claim_nonce = Some((nonce.clone(), now_ms() + CLAIM_NONCE_MS));
        Ok(Response::TeamspeakNonce { nonce })
    }

    /// `teamspeak.claim`: proves the caller holds a TeamSpeak identity and links it to them.
    pub(super) fn teamspeak_claim(&mut self, session: SessionId, key: &str, signature: &str) -> Reply {
        let s = self.sessions.get_mut(&session).expect("requesting session");
        if s.out.is_none() {
            return Err(err(ErrorCode::Forbidden, "TeamSpeak sessions do not claim"));
        }
        // One attempt per nonce, right or wrong.
        let Some((nonce, expires_at)) = s.claim_nonce.take() else {
            return Err(err(ErrorCode::Forbidden, "ask for a challenge first"));
        };
        if expires_at <= now_ms() {
            return Err(err(ErrorCode::Forbidden, "the challenge expired"));
        }
        let uid = s.uid.clone();
        if key.len() > 512 || signature.len() > 256 {
            return Err(err(ErrorCode::BadRequest, "malformed key or signature"));
        }
        let public =
            EccKeyPubP256::from_ts(key).map_err(|_| err(ErrorCode::BadRequest, "not a TeamSpeak public key"))?;
        let signature =
            STANDARD.decode(signature).map_err(|_| err(ErrorCode::BadRequest, "the signature is not base64"))?;
        // The uid as TeamSpeak derives it: from the key re-encoded in its own form.
        let teamspeak = format!("ts:{}", public.get_uid());
        let statement = claim_statement(&self.server_key, &uid, &nonce);
        if public.verify(statement.as_bytes(), &signature).is_err() {
            return Err(err(ErrorCode::Forbidden, "the signature does not match"));
        }
        if self.ban_for(Some(&teamspeak), None).is_some() {
            return Err(err(ErrorCode::Banned, "this TeamSpeak identity is banned here"));
        }
        match self.link_teamspeak(&teamspeak, &uid)? {
            Link::Elsewhere => Err(err(ErrorCode::Conflict, "this TeamSpeak identity is linked to another member")),
            Link::TooMany => {
                Err(err(ErrorCode::QuotaExceeded, "you have as many TeamSpeak identities linked as allowed"))
            }
            Link::Linked { .. } | Link::Already => {
                info!(session, %uid, %teamspeak, "TeamSpeak identity claimed");
                Ok(Response::TeamspeakClaimed { teamspeak })
            }
        }
    }

    /// `teamspeak.claimable`: what a claim could bring here.
    pub(super) fn teamspeak_claimable(&mut self, session: SessionId) -> Reply {
        if self.sessions[&session].out.is_none() {
            return Err(err(ErrorCode::Forbidden, "TeamSpeak sessions do not claim"));
        }
        Ok(Response::TeamspeakClaimable {
            claimable: self.store.teamspeak_claimable(MAX_CLAIMABLE).map_err(storage_error)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::bridge::{BridgeMsg, RemoteClient};
    use super::super::expiry_tests::{on_core, test_core_teamspeak};
    use super::*;
    use tsproto_types::crypto::EccKeyPrivP256;

    async fn gwar_session(core: &CoreHandle, seed: u8, nickname: &str) -> SessionId {
        let (out, _frames) = mpsc::channel(OUTBOUND_QUEUE);
        // Keep the receiver alive for the test: a closed queue would end the session.
        std::mem::forget(_frames);
        core.connect(ConnectRequest {
            request_id: 1,
            hello_key: String::new(),
            certificate: None,
            uid: crate::identity::uid_for_key(&[seed; 32]),
            public_key: URL_SAFE_NO_PAD.encode([seed; 32]),
            nickname: nickname.into(),
            platform: Platform::Web,
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

    async fn remote(core: &CoreHandle, uid: &str, channel: ChannelId) -> Option<SessionId> {
        let (joined, session) = oneshot::channel();
        let client = RemoteClient {
            uid: uid.into(),
            nickname: "from TeamSpeak".into(),
            channel,
            platform: Platform::Ts3,
            muted: false,
            deafened: false,
            away: None,
        };
        core.bridge(BridgeMsg::Join(client, joined)).await;
        session.await.unwrap()
    }

    /// A signed claim of `identity` for `session` (asking for a fresh challenge).
    async fn claim(core: &CoreHandle, session: SessionId, identity: &EccKeyPrivP256) -> Reply {
        let identity = identity.clone();
        on_core(core, move |core| {
            let Response::TeamspeakNonce { nonce } = core.teamspeak_challenge(session).unwrap() else { panic!() };
            let statement = claim_statement(&core.server_key, &core.sessions[&session].uid, &nonce);
            let signature = STANDARD.encode(identity.clone().sign(statement.as_bytes()));
            core.teamspeak_claim(session, &identity.to_pub().to_ts(), &signature)
        })
        .await
    }

    #[tokio::test]
    async fn a_claimed_identity_brings_its_roles_and_its_teamspeak_sessions_act_as_the_member() {
        let store = Store::in_memory().unwrap();
        let mods = store.insert_group("Moderators", &[Permission::KickMembers], None, false, false).unwrap();
        let identity = EccKeyPrivP256::create();
        let teamspeak = format!("ts:{}", identity.to_pub().get_uid());
        let row = store.touch_user(&teamspeak, &teamspeak, "TS name", 1).unwrap();
        store.set_user_groups(row.id, &[MEMBER_GROUP, mods]).unwrap();
        // A stage only the imported member may join (an override of theirs).
        let overrides = vec![
            gwar_proto::ChannelOverride {
                role: Some(MEMBER_GROUP),
                deny: vec![Permission::Connect],
                ..Default::default()
            },
            gwar_proto::ChannelOverride {
                user: Some(teamspeak.clone()),
                allow: vec![Permission::Connect],
                ..Default::default()
            },
        ];
        let stage =
            store.create_channel(&ChannelRow { name: "Stage".into(), overrides, ..Default::default() }).unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let alice = gwar_session(&core, 1, "alice").await;
        let channel = core.info().await.unwrap().default_channel;

        // Before: the TeamSpeak session is the imported member, the app session only a Member.
        let ts = remote(&core, &teamspeak, channel).await.unwrap();
        let (claimable, groups) = on_core(&core, move |core| {
            (core.teamspeak_claimable(alice).unwrap(), core.sessions[&alice].groups.clone())
        })
        .await;
        assert!(
            matches!(claimable, Response::TeamspeakClaimable { claimable } if claimable == vec![teamspeak.clone()])
        );
        assert!(!groups.contains(&mods));

        let claimed = claim(&core, alice, &identity).await.unwrap();
        assert!(matches!(claimed, Response::TeamspeakClaimed { teamspeak: t } if t == teamspeak));
        let (alice_uid, alice_groups, ts_view) = on_core(&core, move |core| {
            let s = &core.sessions[&ts];
            (
                core.sessions[&alice].uid.clone(),
                core.sessions[&alice].groups.clone(),
                (s.uid.clone(), s.member.clone(), s.groups.clone(), s.nickname.clone()),
            )
        })
        .await;
        assert!(alice_groups.contains(&mods));
        // The online TeamSpeak session keeps its identity but now acts as Alice.
        assert_eq!(ts_view, (teamspeak.clone(), alice_uid.clone(), alice_groups.clone(), "alice".to_string()));

        // The override moved to Alice and holds for her TeamSpeak session too.
        let joins = on_core(&core, move |core| {
            (core.can_in(alice, stage, Permission::Connect), core.can_in(ts, stage, Permission::Connect))
        })
        .await;
        assert_eq!(joins, (true, true));
        // A new TeamSpeak session of that identity arrives as Alice too.
        let again = remote(&core, &teamspeak, channel).await.unwrap();
        let view =
            on_core(&core, move |core| (core.sessions[&again].member.clone(), core.sessions[&again].groups.clone()))
                .await;
        assert_eq!(view, (alice_uid.clone(), alice_groups));

        // Unlinked: the TeamSpeak sessions go on as unknown TeamSpeak users; Alice keeps the role.
        let unlinked = on_core(&core, {
            let teamspeak = teamspeak.clone();
            let alice_uid = alice_uid.clone();
            move |core| core.member_teamspeak_unlink(alice, alice_uid, teamspeak)
        })
        .await;
        assert!(unlinked.is_ok());
        let after = on_core(&core, move |core| {
            let s = &core.sessions[&again];
            (s.member == s.uid, s.groups.clone(), core.sessions[&alice].groups.clone())
        })
        .await;
        assert!(after.0);
        assert!(after.1.is_empty());
        assert!(after.2.contains(&mods));
    }

    #[tokio::test]
    async fn claims_need_a_fresh_challenge_and_the_identitys_own_signature() {
        let store = Store::in_memory().unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let alice = gwar_session(&core, 1, "alice").await;
        let bob = gwar_session(&core, 2, "bob").await;
        let identity = EccKeyPrivP256::create();
        let key = identity.to_pub().to_ts();

        // No challenge yet.
        let none = on_core(&core, {
            let key = key.clone();
            move |core| core.teamspeak_claim(alice, &key, "AAAA")
        })
        .await;
        assert_eq!(none.unwrap_err().code, ErrorCode::Forbidden);
        // Signed by another key, or for another member: refused, and the nonce is used up.
        let (wrong_key, for_bob, reused) = on_core(&core, {
            let identity = identity.clone();
            let key = key.clone();
            move |core| {
                let Response::TeamspeakNonce { nonce } = core.teamspeak_challenge(alice).unwrap() else { panic!() };
                let alice_uid = core.sessions[&alice].uid.clone();
                let statement = claim_statement(&core.server_key, &alice_uid, &nonce);
                let other = EccKeyPrivP256::create().sign(statement.as_bytes());
                let wrong_key = core.teamspeak_claim(alice, &key, &STANDARD.encode(&other));
                let right = STANDARD.encode(identity.clone().sign(statement.as_bytes()));
                let reused = core.teamspeak_claim(alice, &key, &right);
                let Response::TeamspeakNonce { nonce } = core.teamspeak_challenge(bob).unwrap() else { panic!() };
                let for_alice = claim_statement(&core.server_key, &alice_uid, &nonce);
                let for_bob = core.teamspeak_claim(bob, &key, &STANDARD.encode(identity.sign(for_alice.as_bytes())));
                (wrong_key, for_bob, reused)
            }
        })
        .await;
        assert_eq!(wrong_key.unwrap_err().code, ErrorCode::Forbidden);
        assert_eq!(reused.unwrap_err().code, ErrorCode::Forbidden);
        assert_eq!(for_bob.unwrap_err().code, ErrorCode::Forbidden);
        // Malformed input is a bad request.
        let malformed = on_core(&core, move |core| {
            core.teamspeak_challenge(alice).unwrap();
            core.teamspeak_claim(alice, "not a key", "AAAA")
        })
        .await;
        assert_eq!(malformed.unwrap_err().code, ErrorCode::BadRequest);

        // A good claim; the same identity claimed by Bob is a conflict, by Alice again fine.
        assert!(claim(&core, alice, &identity).await.is_ok());
        assert_eq!(claim(&core, bob, &identity).await.unwrap_err().code, ErrorCode::Conflict);
        assert!(claim(&core, alice, &identity).await.is_ok());
    }

    #[tokio::test]
    async fn an_expired_challenge_and_a_banned_identity_are_refused() {
        let store = Store::in_memory().unwrap();
        let banned = EccKeyPrivP256::create();
        let uid = format!("ts:{}", banned.to_pub().get_uid());
        let ban = gwar_proto::Ban {
            id: 0,
            uid: Some(uid),
            ip: None,
            nickname: "x".into(),
            reason: None,
            by: "y".into(),
            created_at: 1,
            expires_at: None,
        };
        store.insert_ban(&ban, None).unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let alice = gwar_session(&core, 1, "alice").await;
        assert_eq!(claim(&core, alice, &banned).await.unwrap_err().code, ErrorCode::Banned);
        let expired = on_core(&core, move |core| {
            core.teamspeak_challenge(alice).unwrap();
            core.sessions.get_mut(&alice).unwrap().claim_nonce.as_mut().unwrap().1 = now_ms() - 1;
            core.teamspeak_claim(alice, &EccKeyPrivP256::create().to_pub().to_ts(), "AAAA")
        })
        .await;
        assert_eq!(expired.unwrap_err().code, ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn a_linked_teamspeak_user_has_the_members_rank_and_their_ban() {
        let store = Store::in_memory().unwrap();
        let mods = store
            .insert_group("Moderators", &[Permission::KickMembers, Permission::BanMembers], None, false, false)
            .unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let admin = gwar_session(&core, 1, "admin").await;
        let moderator = gwar_session(&core, 2, "moderator").await;
        let channel = core.info().await.unwrap().default_channel;
        let identity = EccKeyPrivP256::create();
        let teamspeak = format!("ts:{}", identity.to_pub().get_uid());
        on_core(&core, move |core| {
            core.sessions.get_mut(&admin).unwrap().groups = vec![crate::store::ADMIN_GROUP];
            core.sessions.get_mut(&moderator).unwrap().groups = vec![MEMBER_GROUP, mods];
            let admin_id = core.sessions[&admin].user_id;
            core.store.set_user_groups(admin_id, &[crate::store::ADMIN_GROUP]).unwrap();
        })
        .await;
        assert!(claim(&core, admin, &identity).await.is_ok());
        let ts = remote(&core, &teamspeak, channel).await.unwrap();
        // The moderator ranks below the admin, so not above the admin's TeamSpeak session either.
        let kick = on_core(&core, move |core| core.client_kick(moderator, ts, None, None)).await;
        assert_eq!(kick.unwrap_err().code, ErrorCode::Forbidden);
        // A ban of the member keeps their TeamSpeak identity out.
        let admin_uid = on_core(&core, move |core| core.sessions[&admin].uid.clone()).await;
        on_core(&core, move |core| {
            let ban = gwar_proto::Ban {
                id: 0,
                uid: Some(admin_uid),
                ip: None,
                nickname: "a".into(),
                reason: None,
                by: "x".into(),
                created_at: 1,
                expires_at: None,
            };
            core.store.insert_ban(&ban, None).unwrap();
        })
        .await;
        assert_eq!(remote(&core, &teamspeak, channel).await, None);
    }

    #[tokio::test]
    async fn a_linked_user_moving_on_teamspeak_is_held_to_the_members_own_denies() {
        let store = Store::in_memory().unwrap();
        let alice_uid = crate::identity::uid_for_key(&[1; 32]);
        let overrides = vec![gwar_proto::ChannelOverride {
            user: Some(alice_uid),
            deny: vec![Permission::Connect],
            ..Default::default()
        }];
        let closed =
            store.create_channel(&ChannelRow { name: "Closed".into(), overrides, ..Default::default() }).unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let alice = gwar_session(&core, 1, "alice").await;
        let channel = core.info().await.unwrap().default_channel;
        let identity = EccKeyPrivP256::create();
        assert!(claim(&core, alice, &identity).await.is_ok());
        let ts = remote(&core, &format!("ts:{}", identity.to_pub().get_uid()), channel).await.unwrap();
        core.bridge(BridgeMsg::Update(
            ts,
            super::super::bridge::RemoteUpdate { channel: Some(closed), ..Default::default() },
        ))
        .await;
        let now = on_core(&core, move |core| core.sessions[&ts].channel).await;
        assert_eq!(now, Some(channel));
    }

    #[tokio::test]
    async fn a_claim_past_the_limit_is_refused_and_teamspeak_use_counts_as_seen() {
        let store = Store::in_memory().unwrap();
        let alice_uid = crate::identity::uid_for_key(&[1; 32]);
        let core = test_core_teamspeak(store).unwrap();
        let alice = gwar_session(&core, 1, "alice").await;
        let channel = core.info().await.unwrap().default_channel;
        let identity = EccKeyPrivP256::create();
        assert!(claim(&core, alice, &identity).await.is_ok());
        // Seen long ago; using the linked identity on TeamSpeak counts as being here.
        let user_id = on_core(&core, move |core| core.sessions[&alice].user_id).await;
        on_core(&core, move |core| core.store.set_last_seen(user_id, 1).unwrap()).await;
        let ts = remote(&core, &format!("ts:{}", identity.to_pub().get_uid()), channel).await.unwrap();
        async fn seen(core: &CoreHandle, uid: String) -> i64 {
            on_core(core, move |core| core.store.user_by_uid(&uid).unwrap().unwrap().1.last_seen).await
        }
        assert!(seen(&core, alice_uid.clone()).await > 1);
        on_core(&core, move |core| core.store.set_last_seen(user_id, 1).unwrap()).await;
        core.bridge(BridgeMsg::Leave(ts, LeaveReason::Quit)).await;
        assert!(seen(&core, alice_uid.clone()).await > 1);
        // Fill the member up to the limit; one more claim is refused.
        on_core(&core, move |core| {
            for i in 1..crate::store::MAX_TEAMSPEAK_LINKS {
                core.store.link_teamspeak(&format!("ts:filler{i}="), &core.sessions[&alice].uid.clone(), 1).unwrap();
            }
        })
        .await;
        let refused = claim(&core, alice, &EccKeyPrivP256::create()).await;
        assert_eq!(refused.unwrap_err().code, ErrorCode::QuotaExceeded);
    }

    #[tokio::test]
    async fn lifting_a_members_ban_lifts_the_teamspeak_bans_it_caused_even_after_an_unlink() {
        let store = Store::in_memory().unwrap();
        let core = test_core_teamspeak(store).unwrap();
        let (notes, mut bridge) = mpsc::unbounded_channel();
        core.bridge(BridgeMsg::Attach(notes)).await;
        let admin = gwar_session(&core, 1, "admin").await;
        let alice = gwar_session(&core, 2, "alice").await;
        let channel = core.info().await.unwrap().default_channel;
        // One identity online on TeamSpeak when the ban comes (the bridge bans it there), one not.
        let (online, offline) = (EccKeyPrivP256::create(), EccKeyPrivP256::create());
        let uid_of = |k: &EccKeyPrivP256| format!("ts:{}", k.to_pub().get_uid());
        assert!(claim(&core, alice, &online).await.is_ok());
        assert!(claim(&core, alice, &offline).await.is_ok());
        remote(&core, &uid_of(&online), channel).await.unwrap();
        let alice_uid = on_core(&core, move |core| {
            core.sessions.get_mut(&admin).unwrap().groups = vec![crate::store::ADMIN_GROUP];
            core.sessions[&alice].uid.clone()
        })
        .await;
        let ban = on_core(&core, {
            let alice_uid = alice_uid.clone();
            move |core| core.ban_create(admin, gwar_proto::BanCreate { uid: Some(alice_uid), ..Default::default() })
        })
        .await;
        let Ok(Response::Ban { ban }) = ban else { panic!("{ban:?}") };
        // Unlinked meanwhile: its TeamSpeak ban still goes with this one.
        let online_uid = uid_of(&online);
        on_core(&core, move |core| core.unlink_teamspeak(&online_uid).unwrap()).await;
        while bridge.try_recv().is_ok() {}
        let deleted = on_core(&core, move |core| core.ban_delete(admin, ban.id)).await;
        assert!(deleted.is_ok());
        let mut unbanned = Vec::new();
        while let Ok(note) = bridge.try_recv() {
            if let BridgeNote::Unban { uid } = note {
                unbanned.push(uid);
            }
        }
        assert_eq!(unbanned, vec![uid_of(&online)]);
    }
}
