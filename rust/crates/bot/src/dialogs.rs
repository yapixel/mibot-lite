use crate::peers::peer_id;
use crate::{BotError, Client};
use grammers_tl_types as tl;

#[derive(Debug, Clone, Copy, Default)]
pub struct DialogPages {
    /// 0 = main list, 1 = archive.
    pub folder: i32,
    /// Maximum number of 100-dialog pages; 0 defaults to 10.
    pub max_pages: usize,
}

impl Client {
    /// Iterates dialogs page-by-page (100 per page), remembering users/chats in `PeerCache`
    /// and sleeping through arbitrarily long `FLOOD_WAIT` responses.
    pub async fn each_dialog<F>(&self, pages: DialogPages, mut visit: F) -> Result<(), BotError>
    where
        F: FnMut(&tl::types::Dialog),
    {
        let limit = if pages.max_pages == 0 {
            10
        } else {
            pages.max_pages
        };
        let mut offset_date = 0i32;
        let mut offset_id = 0i32;
        let mut offset_peer = tl::enums::InputPeer::Empty;

        let mut page = 0usize;
        while page < limit {
            let req = tl::functions::messages::GetDialogs {
                exclude_pinned: false,
                folder_id: (pages.folder != 0).then_some(pages.folder),
                offset_date,
                offset_id,
                offset_peer: offset_peer.clone(),
                limit: 100,
                hash: 0,
            };
            let res = match self.invoke(&req).await {
                Ok(r) => r,
                Err(err) => {
                    if let Some(wait) = err.flood_wait() {
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(err);
                }
            };

            let (dialogs, messages, last) = match res {
                tl::enums::messages::Dialogs::Dialogs(d) => (d.dialogs, d.messages, true),
                tl::enums::messages::Dialogs::Slice(d) => (d.dialogs, d.messages, false),
                other => {
                    return Err(BotError::Message(format!(
                        "unexpected dialog response: {other:?}"
                    )));
                }
            };

            for entry in &dialogs {
                if let tl::enums::Dialog::Dialog(d) = entry {
                    visit(d);
                }
            }

            if last || dialogs.is_empty() {
                return Ok(());
            }

            let Some(tl::enums::Dialog::Dialog(tail)) = dialogs.last() else {
                return Err(BotError::Message(
                    "dialog cursor unavailable for last dialog".into(),
                ));
            };
            let next_peer = self.input_peer(&tail.peer)?;
            offset_peer = next_peer;
            offset_id = tail.top_message;

            let tail_peer_id = peer_id(&tail.peer);
            let mut found_date = None;
            for item in &messages {
                if let tl::enums::Message::Message(m) = item
                    && m.id == tail.top_message
                    && peer_id(&m.peer_id) == tail_peer_id
                {
                    found_date = Some(m.date);
                    break;
                }
            }
            let Some(date) = found_date else {
                return Err(BotError::Message(format!(
                    "dialog cursor message {tail_peer_id}/{} missing",
                    tail.top_message
                )));
            };
            offset_date = date;
            page += 1;
        }
        Ok(())
    }
}
