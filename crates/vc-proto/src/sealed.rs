//! Stored encrypted direct messages. Binary fields use canonical unpadded base64url.

use super::*;

wire! {
    #[serde(tag = "op", content = "d")]
    pub enum SealedRequest {
        #[serde(rename = "chat.sealed.send")]
        Send { conversation: String, group: String, #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64, message_id: String, ciphertext: String },
        #[serde(rename = "chat.sealed.edit")]
        Edit { conversation: String, group: String, #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64, message_id: String, revision: u32, ciphertext: String },
        #[serde(rename = "chat.delete_own")]
        DeleteOwn { conversation: String, group: String, #[cfg_attr(feature = "ts", ts(type = "number"))] epoch: u64, message_id: String, revision: u32, ciphertext: String },
        #[serde(rename = "chat.sealed.history")]
        History { conversation: String, #[serde(default)] before: Option<MessageId>, #[serde(default)] limit: Option<u32> },
        /// A moderator's unsigned removal of a channel or server chat message.
        #[serde(rename = "chat.sealed.remove")]
        Remove { conversation: String, id: MessageId },
    }

    /// Exactly one read target, shared by requests and events.
    #[serde(untagged, deny_unknown_fields)]
    pub enum ChatReadTarget {
        Channel { channel: ChannelId },
        Conversation { conversation: String },
    }

    pub struct SealedMessage {
        pub id: MessageId,
        pub conversation: String,
        pub group: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub epoch: u64,
        pub author_uid: Uid,
        pub author_name: String,
        pub message_id: String,
        pub revision: u32,
        pub ciphertext: String,
        #[cfg_attr(feature = "ts", ts(type = "number"))]
        pub sent_at: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(type = "number | null"))]
        pub edited_at: Option<i64>,
        pub deleted: bool,
    }

    pub struct ConversationUnread {
        pub conversation: String,
        pub last_read: MessageId,
        /// Messages after last_read, capped at UNREAD_CAP, as for channels.
        pub count: u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_targets_have_exactly_one_address() {
        for d in [json!({"channel":1,"message":2}), json!({"conversation":"dm:a:b","message":2})] {
            let frame = json!({"id":7,"op":"chat.read","d":d});
            let request: ClientFrame = serde_json::from_value(frame.clone()).unwrap();
            assert_eq!(serde_json::to_value(request).unwrap(), frame);
            let frame = json!({"ev":"chat.read","d":d});
            let event: ServerFrame = serde_json::from_value(frame.clone()).unwrap();
            assert!(matches!(event, ServerFrame::Event(Event::ChatRead { .. })));
            assert_eq!(serde_json::to_value(&event).unwrap(), frame);
            let wire = serde_json::to_vec(&event).unwrap();
            assert_eq!(serde_json::from_slice::<ServerFrame>(&wire).unwrap(), event);
        }
        for d in [json!({"message":2}), json!({"channel":1,"conversation":"dm:a:b","message":2})] {
            assert!(serde_json::from_value::<ClientFrame>(json!({"id":7,"op":"chat.read","d":d})).is_err());
        }
    }

    fn sealed_message() -> SealedMessage {
        SealedMessage {
            id: 1,
            conversation: "dm:a:b".into(),
            group: "g".into(),
            epoch: 3,
            author_uid: "a".into(),
            author_name: "A".into(),
            message_id: "m".into(),
            revision: 2,
            ciphertext: "AA".into(),
            sent_at: 1,
            edited_at: Some(2),
            deleted: true,
        }
    }

    #[test]
    fn sealed_rows_are_not_plaintext_messages() {
        for edited_at in [None, Some(2)] {
            let m = SealedMessage { edited_at, ..sealed_message() };
            for frame in [
                ServerFrame::Ok { re: 7, ok: Response::SealedMessage(Box::new(m.clone())) },
                ServerFrame::Event(Event::ChatSealed(m)),
            ] {
                let wire = serde_json::to_vec(&frame).unwrap();
                assert_eq!(serde_json::from_slice::<ServerFrame>(&wire).unwrap(), frame);
            }
        }
    }

    #[test]
    fn sealed_history_round_trips_empty_and_populated_through_server_frame() {
        for sealed in [vec![], vec![sealed_message()]] {
            let frame = ServerFrame::Ok { re: 7, ok: Response::SealedHistory { sealed: sealed.clone() } };
            let wire = serde_json::to_vec(&frame).unwrap();
            assert_eq!(serde_json::from_slice::<ServerFrame>(&wire).unwrap(), frame);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&wire).unwrap(),
                json!({"re":7,"ok":{"sealed":sealed}})
            );
        }
        let plaintext = ServerFrame::Ok { re: 7, ok: Response::History { messages: vec![] } };
        let wire = serde_json::to_vec(&plaintext).unwrap();
        assert_eq!(serde_json::from_slice::<ServerFrame>(&wire).unwrap(), plaintext);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&wire).unwrap(), json!({"re":7,"ok":{"messages":[]}}));
    }

    #[test]
    fn sealed_removal_events_round_trip_empty_and_populated_through_server_frame() {
        for ids in [vec![], vec![1, 2]] {
            let frame =
                ServerFrame::Event(Event::ChatSealedRemoved { conversation: "dm:a:b".into(), ids: ids.clone() });
            let wire = serde_json::to_vec(&frame).unwrap();
            assert_eq!(serde_json::from_slice::<ServerFrame>(&wire).unwrap(), frame);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&wire).unwrap(),
                json!({"ev":"chat.sealed.removed","d":{"conversation":"dm:a:b","ids":ids}})
            );
        }
    }
}
