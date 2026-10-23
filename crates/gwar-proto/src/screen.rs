//! Screen sharing (docs/screen-share.md, "Protocol and permissions"). A share is announced to
//! everyone who can see its channel; watching is bound to the session, sharing to the voice
//! participation.
use super::*;

/// A watch link's token (docs/screen-share.md, "The link and its token"): a bearer secret, so
/// its `Debug` shows none of it, and neither does any frame or message that carries it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WatchToken(pub String);

impl std::fmt::Debug for WatchToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WatchToken(..)")
    }
}

/// Bound the capability list before it reaches the media task. Only the three probed
/// profiles and target tuples are meaningful; extensions cannot manufacture a decoder.
fn h264_capabilities<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<H264Capability>, D::Error> {
    struct Bounded;
    impl<'de> serde::de::Visitor<'de> for Bounded {
        type Value = Vec<H264Capability>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most three H.264 decoder profiles")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut caps = Vec::<H264Capability>::with_capacity(3);
            while let Some(cap) = seq.next_element::<H264Capability>()? {
                if caps.len() == 3
                    || !cap.valid()
                    || caps.iter().any(|c| c.profile_level_id >> 8 == cap.profile_level_id >> 8)
                {
                    return Err(serde::de::Error::custom("invalid H.264 decoder capabilities"));
                }
                caps.push(cap);
            }
            Ok(caps)
        }
    }
    d.deserialize_seq(Bounded)
}

wire! {
    /// Highest successful MediaCapabilities WebRTC decode probe for one H.264 profile.
    #[derive(Eq)]
    #[serde(deny_unknown_fields)]
    pub struct H264Capability {
        pub profile_level_id: u32,
        pub max_fs: u32,
        pub max_mbps: u32,
    }

    /// A server-requested replacement of the publisher, retaining the share and watches.
    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ScreenRecoveryCodec { H264, Vp9 }

    #[derive(Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ScreenRecovery {
        pub share: String,
        pub codec: ScreenRecoveryCodec,
        pub width: u32,
        pub height: u32,
        pub fps: u32,
    }
}

impl H264Capability {
    pub fn valid(&self) -> bool {
        matches!(self.profile_level_id >> 8, 0x42e0 | 0x640c | 0x6400)
            && matches!(
                (self.profile_level_id as u8, self.max_fs, self.max_mbps),
                (31, 3600, 108000) | (42, 8160, 489600) | (51, 14400, 864000)
            )
    }
}

wire! {
    /// What the sharer picked, for display only.
    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ScreenSurface { Screen, Window, Tab, Unknown }

    /// The encoding preset a share starts with (docs/screen-share.md, "Presets").
    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ScreenPreset { Text, Smooth, Game, Saver }

    /// Starts sharing into one's voice channel. Every field is required: a forgotten
    /// `guest_link` must never post a watch link.
    #[serde(deny_unknown_fields)]
    pub struct ScreenStart {
        pub call: String,
        pub participation: String,
        pub preset: ScreenPreset,
        pub surface: ScreenSurface,
        /// Post a TeamSpeak watch link where one is possible.
        pub guest_link: bool,
    }

    /// A running share, as `Welcome.shares` and `screen.started` describe it.
    #[derive(Eq)]
    pub struct ScreenShare {
        /// 16 random bytes, base64url.
        pub share: String,
        pub client: SessionId,
        pub channel: ChannelId,
        pub preset: ScreenPreset,
        pub surface: ScreenSurface,
    }

    /// A share's publish transport: one `sendonly` video m-line whose main and RTX SSRCs are
    /// named here and must be the only ones in the SDP.
    #[serde(deny_unknown_fields)]
    pub struct ScreenPublishOffer {
        pub share: String,
        pub ssrc: u32,
        pub rtx_ssrc: u32,
        pub sdp: String,
    }

    /// The session's watch transport. A greater `run` replaces it, like voice's run.
    #[serde(deny_unknown_fields)]
    pub struct ScreenWatchOffer {
        #[serde(deserialize_with = "voice::run")]
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub run: u64,
        pub sdp: String,
        /// Legacy browser marker, retained for wire compatibility; grants no decode capability.
        /// Omitted by native viewers, whose offered levels remain authoritative.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        #[cfg_attr(feature = "ts", ts(optional, as = "Option<_>"))]
        pub browser: bool,
        /// Verified browser decode limits. Omission retains the SDP-negotiated limits.
        #[serde(default, deserialize_with = "h264_capabilities", skip_serializing_if = "Vec::is_empty")]
        #[cfg_attr(feature = "ts", ts(optional, as = "Option<_>"))]
        pub h264: Vec<H264Capability>,
    }

    /// Why a share ended. `access`: its sharer lost `share_screen`, or, told to one viewer, that
    /// viewer may no longer watch it.
    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ScreenStopReason { Stopped, Moderator, Left, Limit, Transport, Budget, Access, Codec }

    /// A TeamSpeak guest's frame on `/ws/watch` (docs/screen-share.md, "TeamSpeak watch links"):
    /// `{"id": 1, "op": "guest.watch", "d": {...}}`. Guests speak nothing else.
    pub struct GuestFrame {
        pub id: u32,
        #[serde(flatten)]
        pub request: GuestRequest,
    }

    #[serde(tag = "op", content = "d")]
    pub enum GuestRequest {
        /// The first frame: the watch link's token. Answers [`Response::Watching`], or `not_found`
        /// alike for an unknown, ended or revoked token.
        #[serde(rename = "guest.watch")]
        Watch {
            #[cfg_attr(feature = "ts", ts(type = "string"))]
            token: WatchToken,
        },
        /// The guest's watch transport: one `recvonly` video slot. Once per connection; a second
        /// one closes it.
        #[serde(rename = "guest.offer")]
        Offer {
            sdp: String,
            /// Same browser decoder policy as ScreenWatchOffer; native viewers omit it.
            #[serde(default, skip_serializing_if = "std::ops::Not::not")]
            #[cfg_attr(feature = "ts", ts(optional, as = "Option<_>"))]
            browser: bool,
            #[serde(default, deserialize_with = "h264_capabilities", skip_serializing_if = "Vec::is_empty")]
            #[cfg_attr(feature = "ts", ts(optional, as = "Option<_>"))]
            h264: Vec<H264Capability>,
        },
        #[serde(rename = "screen.keyframe")]
        Keyframe { share: String },
        #[serde(rename = "screen.decode_failed")]
        DecodeFailed { share: String, slot: u32, #[serde(deserialize_with = "voice::run")] #[cfg_attr(feature = "ts", ts(type = "number"))] run: u64, generation: u16 },
        /// The heartbeat, every 20 s: WebSocket pings do not keep a connection open.
        #[serde(rename = "ping")]
        Ping {},
    }
}

wire! {
    /// Why a viewer's slot stopped carrying its share for now (`screen.paused`). `bandwidth`:
    /// the viewer's link does not fit it; `budget`: the server's screen egress is over budget.
    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ScreenPauseReason { Bandwidth, Budget }

    /// Screen-sharing limits (docs/screen-share.md, "Limits"): server settings, replaced as a
    /// whole by `server.update`.
    #[derive(Copy, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ScreenLimits {
        pub shares_per_channel: u32,
        pub shares_per_server: u32,
        pub viewers_per_share: u32,
        /// Bitrate cap of each new share, kbps.
        pub share_cap_kbps: u32,
        /// What all slot reservations and publish caps together may reach, Mbps.
        pub budget_mbps: u32,
        /// Admins are warned when the month's outbound transfer reaches 80 % and 100 % of it, TB.
        pub monthly_egress_tb: u32,
        /// Past 100 % of `monthly_egress_tb`, new watches are refused.
        pub monthly_egress_stop: bool,
    }

    /// What the month's outbound transfer counts: the host's default-route interface, a
    /// container's own interface, or nothing (not Linux).
    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum EgressScope { Host, Container, Unavailable }

    /// `server.egress`: this calendar month's outbound transfer (UTC), for the server settings.
    #[derive(Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ServerEgress {
        /// `YYYY-MM`, UTC.
        pub month: String,
        pub scope: EgressScope,
        /// True when some traffic could not be attributed to this month or observed.
        pub incomplete: bool,
        /// Bytes the interface sent this month, every service on it included; `null` where
        /// unavailable.
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub total: Option<u64>,
        /// Of those, the screen SFU's to members and to guests.
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub screen_members: u64,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub screen_guests: u64,
        pub limits: ScreenLimits,
    }
}

impl Default for ScreenLimits {
    /// The owner's defaults of 2026-10-21 (docs/screen-share.md, "Limits").
    fn default() -> Self {
        Self {
            shares_per_channel: 4,
            shares_per_server: 32,
            viewers_per_share: 25,
            share_cap_kbps: 8_000,
            budget_mbps: 300,
            monthly_egress_tb: 10,
            monthly_egress_stop: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_start_names_every_field() {
        let start =
            json!({"call": "c", "participation": "p", "preset": "text", "surface": "window", "guest_link": false});
        let frame: ClientFrame = serde_json::from_value(json!({"id": 1, "op": "screen.start", "d": start})).unwrap();
        assert!(matches!(frame.request, Request::ScreenStart(ScreenStart { guest_link: false, .. })));
        for field in ["guest_link", "preset", "surface", "call"] {
            let mut partial = start.clone();
            partial.as_object_mut().unwrap().remove(field);
            let frame = json!({"id": 1, "op": "screen.start", "d": partial});
            assert!(serde_json::from_value::<ClientFrame>(frame).is_err(), "without {field}");
        }
        let mut extra = start;
        extra["audio"] = json!(true);
        assert!(serde_json::from_value::<ClientFrame>(json!({"id": 1, "op": "screen.start", "d": extra})).is_err());
    }

    #[test]
    fn screen_frames_round_trip() {
        let reply = ServerFrame::Ok { re: 2, ok: Response::ScreenShare { share: "s".into() } };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(text, r#"{"re":2,"ok":{"share":"s"}}"#);
        assert_eq!(serde_json::from_str::<ServerFrame>(&text).unwrap(), reply);
        for event in [
            Event::ScreenStarted(ScreenShare {
                share: "s".into(),
                client: 3,
                channel: 4,
                preset: ScreenPreset::Smooth,
                surface: ScreenSurface::Screen,
            }),
            Event::ScreenStopped { share: "s".into(), reason: ScreenStopReason::Moderator, by: Some("mod".into()) },
            Event::ScreenViewers { share: "s".into(), members: 2, guests: 0 },
            Event::ScreenLink { share: "s".into(), active: false },
            Event::ScreenSlot { run: 1, slot: 3, share: Some("s".into()) },
            Event::ScreenSlot { run: 2, slot: 0, share: None },
            Event::ScreenPaused { share: "s".into(), reason: ScreenPauseReason::Bandwidth },
            Event::ScreenResumed { share: "s".into() },
            Event::ServerEgressWarning { month: "2026-10".into(), percent: 80, total: 8_000_000_000_000, limit: 10 },
        ] {
            let frame = ServerFrame::Event(event);
            let text = serde_json::to_string(&frame).unwrap();
            assert_eq!(serde_json::from_str::<ServerFrame>(&text).unwrap(), frame, "{text}");
        }
        let stopped = Event::ScreenStopped { share: "s".into(), reason: ScreenStopReason::Access, by: None };
        assert_eq!(
            serde_json::to_value(ServerFrame::Event(stopped)).unwrap(),
            json!({"ev": "screen.stopped", "d": {"share": "s", "reason": "access"}})
        );
    }

    #[test]
    fn guests_speak_only_the_watch_ops() {
        let frame: GuestFrame =
            serde_json::from_value(json!({"id": 1, "op": "guest.watch", "d": {"token": "t"}})).unwrap();
        assert_eq!(frame.request, GuestRequest::Watch { token: WatchToken("t".into()) });
        let ping: GuestFrame = serde_json::from_value(json!({"id": 2, "op": "ping", "d": {}})).unwrap();
        assert_eq!(ping.request, GuestRequest::Ping {});
        for op in ["hello", "join", "screen.watch", "screen.watch.offer", "chat.send", "channel.join"] {
            let frame = json!({"id": 3, "op": op, "d": {}});
            assert!(serde_json::from_value::<GuestFrame>(frame).is_err(), "{op}");
        }
        let watching = Response::Watching {
            screen_h264: true,
            share: "s".into(),
            sharer: "alice".into(),
            preset: ScreenPreset::Text,
            ice_servers: vec![],
        };
        let reply = ServerFrame::Ok { re: 1, ok: watching };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(
            text,
            r#"{"re":1,"ok":{"share":"s","sharer":"alice","preset":"text","ice_servers":[],"screen_h264":true}}"#
        );
        assert_eq!(serde_json::from_str::<ServerFrame>(&text).unwrap(), reply);
    }

    #[test]
    fn a_watch_token_never_shows_in_debug_output() {
        let secret = "s3cr3t-Watch-Token_0123456789abcdefghijklmno";
        let raw = json!({"id": 1, "op": "guest.watch", "d": {"token": secret}});
        let frame: GuestFrame = serde_json::from_value(raw.clone()).unwrap();
        for shown in [format!("{frame:?}"), format!("{frame:#?}"), format!("{:?}", frame.request)] {
            assert!(!shown.contains(secret), "{shown}");
            assert!(shown.contains("WatchToken(..)"), "{shown}");
        }
        // The wire form is the token itself.
        assert_eq!(serde_json::to_value(&frame).unwrap(), raw);
    }

    #[test]
    fn egress_reports_and_limits_round_trip() {
        let egress = ServerEgress {
            month: "2026-10".into(),
            scope: EgressScope::Container,
            incomplete: true,
            total: None,
            screen_members: 5,
            screen_guests: 0,
            limits: ScreenLimits::default(),
        };
        let reply = ServerFrame::Ok { re: 3, ok: Response::ServerEgress(egress.clone()) };
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<ServerFrame>(&text).unwrap(), reply, "{text}");
        assert_eq!(serde_json::to_value(&egress).unwrap()["total"], json!(null));
        let update = json!({"id": 1, "op": "server.update", "d": {"screen": ScreenLimits::default()}});
        let frame: ClientFrame = serde_json::from_value(update).unwrap();
        assert!(matches!(frame.request, Request::ServerUpdate(ServerUpdate { screen: Some(_), .. })));
        let mut partial = serde_json::to_value(ScreenLimits::default()).unwrap();
        partial.as_object_mut().unwrap().remove("budget_mbps");
        let update = json!({"id": 1, "op": "server.update", "d": {"screen": partial}});
        assert!(serde_json::from_value::<ClientFrame>(update).is_err(), "limits are replaced as a whole");
    }

    #[test]
    fn transport_frames_are_strict() {
        let offer = json!({"share": "s", "ssrc": 1, "rtx_ssrc": 2, "sdp": "v=0"});
        let frame: ClientFrame =
            serde_json::from_value(json!({"id": 1, "op": "screen.publish.offer", "d": offer})).unwrap();
        assert!(matches!(frame.request, Request::ScreenPublishOffer(ScreenPublishOffer { ssrc: 1, rtx_ssrc: 2, .. })));
        let watch = |run: serde_json::Value| json!({"id": 1, "op": "screen.watch.offer", "d": {"run": run, "sdp": ""}});
        let native: ClientFrame = serde_json::from_value(watch(json!(1))).unwrap();
        assert!(matches!(native.request, Request::ScreenWatchOffer(ScreenWatchOffer { browser: false, .. })));
        assert!(serde_json::to_value(native).unwrap()["d"].get("browser").is_none());
        let mut browser = watch(json!(1));
        browser["d"]["browser"] = json!(true);
        let browser: ClientFrame = serde_json::from_value(browser).unwrap();
        assert!(matches!(browser.request, Request::ScreenWatchOffer(ScreenWatchOffer { browser: true, .. })));
        for browser in [false, true] {
            let guest: GuestFrame =
                serde_json::from_value(json!({"id": 1, "op": "guest.offer", "d": {"sdp": "", "browser": browser}}))
                    .unwrap();
            assert!(matches!(guest.request, GuestRequest::Offer { browser: value, .. } if value == browser));
        }
        for run in [json!(0), json!(9007199254740992u64), json!(-1)] {
            assert!(serde_json::from_value::<ClientFrame>(watch(run.clone())).is_err(), "run {run}");
        }
        for (ok, text) in [
            (Response::ScreenAnswer { sdp: "a".into() }, r#"{"sdp":"a"}"#),
            (Response::ScreenSlot { slot: 2 }, r#"{"slot":2}"#),
        ] {
            let reply = ServerFrame::Ok { re: 1, ok };
            assert_eq!(serde_json::to_string(&reply).unwrap(), format!(r#"{{"re":1,"ok":{text}}}"#));
            assert_eq!(serde_json::from_str::<ServerFrame>(&serde_json::to_string(&reply).unwrap()).unwrap(), reply);
        }
        let slot = json!({"ev": "screen.slot", "d": {"run": 1, "slot": 0}});
        assert!(serde_json::from_value::<ServerFrame>(slot).is_err(), "share must be present, even as null");
    }

    #[test]
    fn legacy_watch_schemas_remain_accepted_in_both_directions() {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldWatch {
            run: u64,
            sdp: String,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldGuestOffer {
            sdp: String,
        }
        let old = json!({"run": 1, "sdp": "old"});
        let current: ScreenWatchOffer = serde_json::from_value(old.clone()).unwrap();
        assert!(!current.browser && current.h264.is_empty());
        let back: OldWatch = serde_json::from_value(serde_json::to_value(&current).unwrap()).unwrap();
        assert_eq!((back.run, back.sdp), (1, "old".into()));
        for field in ["browser", "h264"] {
            let mut extended = old.clone();
            extended[field] = if field == "browser" { json!(true) } else { json!([]) };
            assert!(
                serde_json::from_value::<OldWatch>(extended).is_err(),
                "negative control: old servers reject {field}, including an empty capability list"
            );
        }
        let current: GuestFrame = serde_json::from_value(json!({"id":1,"op":"guest.offer","d":{"sdp":"old"}})).unwrap();
        let body = serde_json::to_value(current).unwrap()["d"].clone();
        assert_eq!(serde_json::from_value::<OldGuestOffer>(body).unwrap().sdp, "old");
        let reply: Response =
            serde_json::from_value(json!({"share":"s","sharer":"alice","preset":"text","ice_servers":[]})).unwrap();
        assert!(matches!(reply, Response::Watching { screen_h264: false, .. }));
    }

    #[test]
    fn verified_capabilities_are_bounded_and_validated_for_members_and_guests() {
        let valid = json!({"profile_level_id": 0x42e02a, "max_fs": 8160, "max_mbps": 489600});
        for caps in [json!([valid.clone()]), json!([])] {
            let watch = json!({"run": 1, "sdp": "", "h264": caps});
            assert!(serde_json::from_value::<ScreenWatchOffer>(watch).is_ok());
        }
        let mut wrong_size = valid.clone();
        wrong_size["max_fs"] = json!(u32::MAX);
        let mut wrong_profile = valid.clone();
        wrong_profile["profile_level_id"] = json!(0x4d002a);
        for caps in [
            json!([valid.clone(), valid.clone()]),
            json!([valid.clone(), valid.clone(), valid.clone(), valid.clone()]),
            json!([wrong_size]),
            json!([wrong_profile]),
            json!([{}]),
            json!(null),
        ] {
            assert!(serde_json::from_value::<ScreenWatchOffer>(json!({"run": 1, "sdp": "", "h264": caps})).is_err());
            assert!(
                serde_json::from_value::<GuestFrame>(
                    json!({"id": 1, "op": "guest.offer", "d": {"sdp": "", "h264": caps}})
                )
                .is_err()
            );
        }
    }

    #[test]
    fn sharing_needs_connect_and_is_a_member_default() {
        assert!(Permission::is_channel(Permission::ShareScreen));
        assert!(Permission::MEMBER_DEFAULT.contains(&Permission::ShareScreen));
        assert!(Permission::NEEDS_CONNECT.contains(&Permission::ShareScreen));
    }
}
