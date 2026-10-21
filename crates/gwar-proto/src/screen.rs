//! Screen sharing (docs/screen-share.md, "Protocol and permissions"). A share is announced to
//! everyone who can see its channel; watching is bound to the session, sharing to the voice
//! participation.
use super::*;

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

    /// Why a share ended. `access`: its sharer lost `share_screen`, or, told to one viewer, that
    /// viewer may no longer watch it.
    #[derive(Copy, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum ScreenStopReason { Stopped, Moderator, Left, Limit, Transport, Budget, Access }
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
    fn sharing_needs_connect_and_is_a_member_default() {
        assert!(Permission::is_channel(Permission::ShareScreen));
        assert!(Permission::MEMBER_DEFAULT.contains(&Permission::ShareScreen));
        assert!(Permission::NEEDS_CONNECT.contains(&Permission::ShareScreen));
    }
}
