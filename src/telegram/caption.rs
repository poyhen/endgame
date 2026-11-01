use grammers_client::message::Message;
use grammers_client::session::types::PeerRef;
use grammers_client::{Client, InvocationError, tl};

pub(crate) async fn clear(client: &Client, message: &Message) -> anyhow::Result<()> {
    let peer = message
        .peer_ref()
        .await
        .map_err(anyhow::Error::from_boxed)?
        .ok_or_else(|| anyhow::anyhow!("media message has no peer reference"))?;
    match client.invoke(&clear_request(peer, message.id())).await {
        Ok(_) => Ok(()),
        // Clearing a caption again is successful even if an earlier attempt's
        // acknowledgement was lost.
        Err(InvocationError::Rpc(error)) if error.name == "MESSAGE_NOT_MODIFIED" => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn clear_request(peer: PeerRef, message_id: i32) -> tl::functions::messages::EditMessage {
    tl::functions::messages::EditMessage {
        no_webpage: true,
        invert_media: false,
        peer: peer.into(),
        id: message_id,
        // Grammers' high-level edit API filters an empty string to None, which
        // preserves the existing text. Send an explicitly empty caption instead.
        message: Some(String::new()),
        media: None,
        reply_markup: None,
        entities: Some(Vec::new()),
        schedule_date: None,
        schedule_repeat_period: None,
        quick_reply_shortcut_id: None,
        rich_message: None,
    }
}

#[cfg(test)]
mod tests {
    use grammers_client::session::types::PeerId;
    use grammers_client::tl::{Deserializable, Serializable};

    use super::*;

    #[test]
    fn encodes_caption_removal_without_replacing_media() {
        let peer = PeerId::self_user().to_ambient_ref();
        let request = clear_request(peer, 42);
        let decoded =
            tl::functions::messages::EditMessage::from_boxed_bytes(&request.to_bytes()).unwrap();

        assert_eq!(decoded.message.as_deref(), Some(""));
        assert_eq!(decoded.entities, Some(Vec::new()));
        assert!(decoded.media.is_none());
        assert_eq!(decoded.peer, peer.into());
        assert_eq!(decoded.id, 42);
    }
}
