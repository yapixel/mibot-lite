pub mod dialogs;
pub mod download;
pub mod html;
pub mod ipprivacy;
pub mod message;
pub mod peers;
pub mod retrier;
pub mod rich;
pub mod sent;

pub use dialogs::DialogPages;
pub use download::{MediaFile, MediaSource, document_of, largest_photo_size, source_of};
pub use html::{bold, code, escape, parse_html, utf16_len};
pub use ipprivacy::{
    InvalidIpPolicyError, IpPolicy, current_ip_policy, ip_link, is_ip_privacy_off, mask_ips,
    mask_ips_with, set_ip_policy, with_optional_ip_privacy_off, without_ip_privacy,
};
pub use message::{ChatType, Message, envelope};
pub use peers::{ChannelInfo, ChatInfo, HashLookup, HashStore, PeerCache, UserInfo, peer_from_id, peer_id};
pub use retrier::{Clock, Retrier, SystemClock};
pub use rich::{
    RICH_BLOCK_LIMIT, RICH_COLUMN_LIMIT, RICH_TEXT_LIMIT, input_rich_fits, input_rich_text,
    message_text, plain_copy, rich_code, rich_concat, rich_fits, rich_message_text, rich_plain,
    rich_rejected,
};
pub use sent::{
    SentRecorder, current_sent_observer, first_sent_message_id, input_peer_to_peer, observe_sent,
    with_optional_sent_observer,
};

use grammers_mtsender::{InvocationError, RpcError, SenderPoolHandle};
use grammers_session::Session;
use grammers_tl_types::{self as tl, Deserializable, Identifiable, RemoteCall, Serializable};
use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

static FALLBACK_RANDOM: AtomicU64 = AtomicU64::new(1);

/// Generates a non-zero 64-bit random ID for MTProto `random_id` fields without panicking.
pub fn random_id() -> i64 {
    let mut buf = [0u8; 8];
    let mut val = if getrandom::getrandom(&mut buf).is_ok() {
        i64::from_le_bytes(buf)
    } else {
        let seq = FALLBACK_RANDOM.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        (nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15)) as i64
    };
    if val == 0 {
        val = 1;
    }
    val
}

#[derive(Debug, Error)]
pub enum BotError {
    #[error("too large: {0}")]
    TooLarge(String),
    #[error("message has no media")]
    NoMedia,
    #[error("empty download")]
    EmptyDownload,
    #[error("unaddressable peer: {0}")]
    UnaddressablePeer(String),
    #[error("uncertain delivery: {0}")]
    Uncertain(#[source] mibot_delivery::UncertainError),
    #[error("html error: {0}")]
    Html(String),
    #[error("cancelled")]
    Cancelled,
    #[error("deserialize failed: {0}")]
    Deserialize(String),
    #[error(transparent)]
    Invocation(#[from] InvocationError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

impl BotError {
    pub fn is_uncertain(&self) -> bool {
        matches!(
            self,
            Self::Uncertain(_)
                | Self::Invocation(InvocationError::Io(_) | InvocationError::Dropped(_))
        )
    }

    pub fn flood_wait(&self) -> Option<Duration> {
        match self {
            Self::Invocation(inv) => Retrier::flood_wait(inv),
            _ => None,
        }
    }

    pub fn rpc_error(&self) -> Option<&RpcError> {
        match self {
            Self::Invocation(InvocationError::Rpc(rpc)) => Some(rpc),
            _ => None,
        }
    }

    pub fn rpc_code(&self) -> Option<String> {
        self.rpc_error().map(|rpc| rpc.name.clone())
    }

    pub fn is_rpc(&self, name: &str) -> bool {
        self.rpc_error().is_some_and(|rpc| rpc.name == name)
    }

    pub fn is_unaddressable_peer(&self) -> bool {
        matches!(self, Self::UnaddressablePeer(_))
    }
}

/// Low-level MTProto RPC invocation transport (implemented by `SenderPoolTransport` and test mocks).
pub trait RpcTransport: Send + Sync {
    fn invoke_raw<'a>(
        &'a self,
        dc_id: Option<i32>,
        constructor_id: u32,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>>;

    fn disconnect_dc<'a>(&'a self, _dc_id: i32) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn home_dc_id(&self) -> i32 {
        0
    }
}

/// Live `grammers-mtsender` `SenderPoolHandle` transport reading home DC dynamically from `Session`.
pub struct SenderPoolTransport<S> {
    handle: SenderPoolHandle,
    session: Arc<S>,
    cached_home_dc: AtomicI32,
}

impl<S: Session + Send + Sync + 'static> SenderPoolTransport<S> {
    pub fn new(handle: SenderPoolHandle, session: Arc<S>) -> Self {
        Self {
            handle,
            session,
            cached_home_dc: AtomicI32::new(0),
        }
    }
}

impl<S: Session + Send + Sync + 'static> RpcTransport for SenderPoolTransport<S> {
    fn invoke_raw<'a>(
        &'a self,
        dc_id: Option<i32>,
        _constructor_id: u32,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
        Box::pin(async move {
            let target_dc = match dc_id {
                Some(dc) => dc,
                None => {
                    let home = self
                        .session
                        .home_dc_id()
                        .map_err(|_| InvocationError::Dropped("session home_dc_id unavailable"))?;
                    self.cached_home_dc.store(home, Ordering::Relaxed);
                    home
                }
            };
            self.handle.raw_invoke_in_dc(target_dc, body).await
        })
    }

    fn disconnect_dc<'a>(&'a self, dc_id: i32) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let _ = self.handle.disconnect_from_dc(dc_id);
        })
    }

    fn home_dc_id(&self) -> i32 {
        self.cached_home_dc.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SendOptions {
    pub reply_to: Option<i32>,
    pub topic_id: Option<i32>,
    pub no_webpage: bool,
    pub entities: Option<Vec<tl::enums::MessageEntity>>,
}

#[derive(Debug, Clone, Default)]
pub struct MediaOptions {
    pub name: String,
    pub mime_type: String,
    pub caption: String,
    pub reply_to: Option<i32>,
    pub topic_id: Option<i32>,
    pub entities: Option<Vec<tl::enums::MessageEntity>>,
    pub html: bool,
    pub force_document: bool,
    pub attributes: Vec<tl::enums::DocumentAttribute>,
}

#[derive(Clone)]
pub struct Client {
    transport: Arc<dyn RpcTransport>,
    retrier: Arc<Retrier>,
    pub(crate) peers: Arc<PeerCache>,
    sent: Arc<SentRecorder>,
    self_user: Arc<RwLock<Option<tl::types::User>>>,
    auth_copied: Arc<tokio::sync::Mutex<Vec<i32>>>,
}

struct PreparedRequest {
    body: Vec<u8>,
    edit_banned_until: Option<i32>,
    request_peer: Option<tl::enums::InputPeer>,
    record_sent: bool,
}

impl Client {
    pub fn from_transport(
        transport: Arc<dyn RpcTransport>,
        peers: Arc<PeerCache>,
        sent: Arc<SentRecorder>,
    ) -> Self {
        Self::with_retrier(transport, Arc::new(Retrier::default()), peers, sent)
    }

    pub fn with_retrier(
        transport: Arc<dyn RpcTransport>,
        retrier: Arc<Retrier>,
        peers: Arc<PeerCache>,
        sent: Arc<SentRecorder>,
    ) -> Self {
        Self {
            transport,
            retrier,
            peers,
            sent,
            self_user: Arc::new(RwLock::new(None)),
            auth_copied: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn from_sender_pool<S: Session + Send + Sync + 'static>(
        handle: SenderPoolHandle,
        session: Arc<S>,
        peers: Arc<PeerCache>,
        sent: Arc<SentRecorder>,
    ) -> Self {
        let transport: Arc<dyn RpcTransport> = Arc::new(SenderPoolTransport::new(handle, session));
        Self::from_transport(transport, peers, sent)
    }

    pub fn peers(&self) -> &Arc<PeerCache> {
        &self.peers
    }

    pub fn sent(&self) -> &Arc<SentRecorder> {
        &self.sent
    }

    pub fn self_user(&self) -> Option<tl::types::User> {
        self.self_user
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_self_user(&self, user: tl::types::User) {
        self.peers.set_self(user.id);
        self.peers
            .remember_users(&[tl::enums::User::User(user.clone())]);
        *self.self_user.write().unwrap_or_else(|e| e.into_inner()) = Some(user);
    }

    pub async fn refresh_self(&self) -> Result<tl::types::User, BotError> {
        let users = self
            .invoke(&tl::functions::users::GetUsers {
                id: vec![tl::enums::InputUser::UserSelf],
            })
            .await?;
        for entry in users {
            if let tl::enums::User::User(user) = entry {
                self.set_self_user(user.clone());
                return Ok(user);
            }
        }
        Err(BotError::Message("GetUsers returned no self user".into()))
    }

    pub async fn copy_auth_to_dc(&self, target_dc_id: i32) -> Result<(), BotError> {
        if target_dc_id == 0 || target_dc_id == self.transport.home_dc_id() {
            return Ok(());
        }
        let mut guard = self.auth_copied.lock().await;
        if guard.contains(&target_dc_id) {
            return Ok(());
        }
        let tl::enums::auth::ExportedAuthorization::Authorization(exported) = self
            .invoke(&tl::functions::auth::ExportAuthorization {
                dc_id: target_dc_id,
            })
            .await?;
        let _auth = self
            .invoke_optional_dc(
                Some(target_dc_id),
                &tl::functions::auth::ImportAuthorization {
                    id: exported.id,
                    bytes: exported.bytes,
                },
            )
            .await?;
        guard.push(target_dc_id);
        Ok(())
    }

    pub async fn invoke<R>(&self, request: &R) -> Result<R::Return, BotError>
    where
        R: RemoteCall + Identifiable + Any,
        R::Return: Any,
    {
        self.invoke_optional_dc(None, request).await
    }

    pub async fn invoke_optional_dc<R>(
        &self,
        dc_id: Option<i32>,
        request: &R,
    ) -> Result<R::Return, BotError>
    where
        R: RemoteCall + Identifiable + Any,
        R::Return: Any,
    {
        let prepared = prepare_request(request);
        let raw = self
            .retrier
            .invoke(
                self.transport.as_ref(),
                dc_id,
                R::CONSTRUCTOR_ID,
                prepared.body,
                prepared.edit_banned_until,
            )
            .await?;
        let decoded = R::Return::from_bytes(&raw)
            .map_err(|e| BotError::Deserialize(format!("{}: {e}", R::CONSTRUCTOR_ID)))?;
        self.remember_tl_response(&decoded);
        if prepared.record_sent
            && let Some(updates) = (&decoded as &dyn Any).downcast_ref::<tl::enums::Updates>()
        {
            self.sent.record_updates(
                self.peers.self_id(),
                prepared.request_peer.as_ref(),
                updates,
            );
        }
        Ok(decoded)
    }

    pub fn remember_tl_response(&self, resp: &dyn Any) {
        if let Some(u) = resp.downcast_ref::<tl::enums::Updates>() {
            match u {
                tl::enums::Updates::Updates(x) => self.peers.remember_entities(&x.users, &x.chats),
                tl::enums::Updates::Combined(x) => {
                    self.peers.remember_entities(&x.users, &x.chats)
                }
                _ => {}
            }
        } else if let Some(m) = resp.downcast_ref::<tl::enums::messages::Messages>() {
            match m {
                tl::enums::messages::Messages::Messages(x) => {
                    self.peers.remember_entities(&x.users, &x.chats)
                }
                tl::enums::messages::Messages::Slice(x) => {
                    self.peers.remember_entities(&x.users, &x.chats)
                }
                tl::enums::messages::Messages::ChannelMessages(x) => {
                    self.peers.remember_entities(&x.users, &x.chats)
                }
                tl::enums::messages::Messages::NotModified(_) => {}
            }
        } else if let Some(d) = resp.downcast_ref::<tl::enums::messages::Dialogs>() {
            match d {
                tl::enums::messages::Dialogs::Dialogs(x) => {
                    self.peers.remember_entities(&x.users, &x.chats)
                }
                tl::enums::messages::Dialogs::Slice(x) => {
                    self.peers.remember_entities(&x.users, &x.chats)
                }
                tl::enums::messages::Dialogs::NotModified(_) => {}
            }
        } else if let Some(tl::enums::contacts::ResolvedPeer::Peer(p)) =
            resp.downcast_ref::<tl::enums::contacts::ResolvedPeer>()
        {
            self.peers.remember_entities(&p.users, &p.chats);
        } else if let Some(tl::enums::contacts::Found::Found(f)) =
            resp.downcast_ref::<tl::enums::contacts::Found>()
        {
            self.peers.remember_entities(&f.users, &f.chats);
        } else if let Some(c) = resp.downcast_ref::<tl::enums::messages::Chats>() {
            match c {
                tl::enums::messages::Chats::Chats(x) => self.peers.remember_chats(&x.chats),
                tl::enums::messages::Chats::Slice(x) => self.peers.remember_chats(&x.chats),
            }
        } else if let Some(tl::enums::channels::ChannelParticipants::Participants(p)) =
            resp.downcast_ref::<tl::enums::channels::ChannelParticipants>()
        {
            self.peers.remember_entities(&p.users, &p.chats);
        } else if let Some(tl::enums::messages::ChatFull::Full(f)) =
            resp.downcast_ref::<tl::enums::messages::ChatFull>()
        {
            self.peers.remember_entities(&f.users, &f.chats);
        } else if let Some(tl::enums::users::UserFull::Full(f)) =
            resp.downcast_ref::<tl::enums::users::UserFull>()
        {
            self.peers.remember_entities(&f.users, &f.chats);
        } else if let Some(users) = resp.downcast_ref::<Vec<tl::enums::User>>() {
            self.peers.remember_users(users);
        }
    }

    pub fn input_peer(&self, peer: &tl::enums::Peer) -> Result<tl::enums::InputPeer, BotError> {
        self.peers
            .input_peer(peer)
            .ok_or_else(|| BotError::UnaddressablePeer(peer_id(peer)))
    }

    pub fn html(&self, input: &str) -> Result<(String, Vec<tl::enums::MessageEntity>), BotError> {
        parse_html(input, Some(&self.peers))
    }

    pub async fn send_html(
        &self,
        peer: &tl::enums::InputPeer,
        text: &str,
        opts: SendOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        let (plain, entities) = match opts.entities {
            Some(entities) => (text.to_owned(), entities),
            None => self.html(text)?,
        };
        let req = build_send_message(peer.clone(), plain, opt_entities(entities), opts.no_webpage, opts.reply_to, opts.topic_id, None);
        self.invoke(&req).await
    }

    pub async fn edit_html(
        &self,
        peer: &tl::enums::InputPeer,
        msg_id: i32,
        text: &str,
        opts: SendOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        let (plain, entities) = match opts.entities {
            Some(entities) => (text.to_owned(), entities),
            None => self.html(text)?,
        };
        let req = build_edit_message(peer.clone(), msg_id, Some(plain), opt_entities(entities), None, opts.no_webpage, None);
        self.invoke(&req).await
    }

    pub async fn send_media(
        &self,
        peer: &tl::enums::InputPeer,
        media: tl::enums::InputMedia,
        opts: MediaOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        let (caption, entities) = match opts.entities {
            Some(entities) => (opts.caption, Some(entities)),
            None if opts.html && !opts.caption.is_empty() => {
                let (plain, ents) = self.html(&opts.caption)?;
                (plain, opt_entities(ents))
            }
            None => (opts.caption, None),
        };
        let req = tl::functions::messages::SendMedia {
            silent: false,
            background: false,
            clear_draft: false,
            noforwards: false,
            update_stickersets_order: false,
            invert_media: false,
            allow_paid_floodskip: false,
            peer: peer.clone(),
            reply_to: make_reply_to(opts.reply_to, opts.topic_id),
            media,
            message: caption,
            random_id: random_id(),
            reply_markup: None,
            entities,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            allow_paid_stars: None,
            suggested_post: None,
        };
        self.invoke(&req).await
    }

    pub async fn edit_media(
        &self,
        peer: &tl::enums::InputPeer,
        msg_id: i32,
        media: tl::enums::InputMedia,
        opts: MediaOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        let (caption, entities) = match opts.entities {
            Some(entities) => (opts.caption, Some(entities)),
            None if opts.html && !opts.caption.is_empty() => {
                let (plain, ents) = self.html(&opts.caption)?;
                (plain, opt_entities(ents))
            }
            None => (opts.caption, None),
        };
        let req = build_edit_message(
            peer.clone(),
            msg_id,
            Some(caption),
            entities,
            Some(media),
            false,
            None,
        );
        self.invoke(&req).await
    }

    /// Sends an `InputRichMessage`, falling back to `plain_copy(&message_text)` if it exceeds
    /// Telegram's rich-message bounds or Telegram explicitly rejects it with a 4xx RPC error
    /// (excluding `FLOOD_WAIT` and uncertain delivery failures).
    pub async fn send_rich(
        &self,
        peer: &tl::enums::InputPeer,
        rich: tl::enums::InputRichMessage,
        opts: SendOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        let fallback_text = plain_copy(&input_rich_text(&rich));
        if !input_rich_fits(&rich) {
            let req = build_send_message(
                peer.clone(),
                fallback_text,
                None,
                opts.no_webpage,
                opts.reply_to,
                opts.topic_id,
                None,
            );
            return self.invoke(&req).await;
        }
        let req = build_send_message(
            peer.clone(),
            fallback_text.clone(),
            None,
            opts.no_webpage,
            opts.reply_to,
            opts.topic_id,
            Some(rich),
        );
        match self.invoke(&req).await {
            Ok(updates) => Ok(updates),
            Err(err) if rich_rejected(&err) => {
                let req = build_send_message(
                    peer.clone(),
                    fallback_text,
                    None,
                    opts.no_webpage,
                    opts.reply_to,
                    opts.topic_id,
                    None,
                );
                self.invoke(&req).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn edit_rich(
        &self,
        peer: &tl::enums::InputPeer,
        msg_id: i32,
        rich: tl::enums::InputRichMessage,
        opts: SendOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        let fallback_text = plain_copy(&input_rich_text(&rich));
        if !input_rich_fits(&rich) {
            let req = build_edit_message(
                peer.clone(),
                msg_id,
                Some(fallback_text),
                None,
                None,
                opts.no_webpage,
                None,
            );
            return self.invoke(&req).await;
        }
        let req = build_edit_message(
            peer.clone(),
            msg_id,
            Some(fallback_text.clone()),
            None,
            None,
            opts.no_webpage,
            Some(rich),
        );
        match self.invoke(&req).await {
            Ok(updates) => Ok(updates),
            Err(err) if rich_rejected(&err) => {
                let req = build_edit_message(
                    peer.clone(),
                    msg_id,
                    Some(fallback_text),
                    None,
                    None,
                    opts.no_webpage,
                    None,
                );
                self.invoke(&req).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn get_messages(
        &self,
        peer: &tl::enums::InputPeer,
        ids: &[i32],
    ) -> Result<Vec<tl::types::Message>, BotError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let input_ids: Vec<tl::enums::InputMessage> = ids
            .iter()
            .map(|&id| tl::enums::InputMessage::Id(tl::types::InputMessageId { id }))
            .collect();
        let res = match peer {
            tl::enums::InputPeer::Channel(ch) => {
                self.invoke(&tl::functions::channels::GetMessages {
                    channel: tl::types::InputChannel {
                        channel_id: ch.channel_id,
                        access_hash: ch.access_hash,
                    }
                    .into(),
                    id: input_ids,
                })
                .await?
            }
            _ => {
                self.invoke(&tl::functions::messages::GetMessages { id: input_ids })
                    .await?
            }
        };

        let expected = input_peer_to_peer(self.peers.self_id(), peer);
        let raw_msgs = match res {
            tl::enums::messages::Messages::Messages(m) => m.messages,
            tl::enums::messages::Messages::Slice(m) => m.messages,
            tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
            tl::enums::messages::Messages::NotModified(_) => Vec::new(),
        };
        let mut out = Vec::with_capacity(raw_msgs.len());
        for entry in raw_msgs {
            if let tl::enums::Message::Message(msg) = entry {
                if let Some(exp) = &expected
                    && &msg.peer_id != exp
                {
                    continue;
                }
                out.push(msg);
            }
        }
        Ok(out)
    }

    pub async fn delete_messages(
        &self,
        peer: &tl::enums::InputPeer,
        ids: &[i32],
    ) -> Result<(), BotError> {
        if ids.is_empty() {
            return Ok(());
        }
        match peer {
            tl::enums::InputPeer::Channel(ch) => {
                self.invoke(&tl::functions::channels::DeleteMessages {
                    channel: tl::types::InputChannel {
                        channel_id: ch.channel_id,
                        access_hash: ch.access_hash,
                    }
                    .into(),
                    id: ids.to_vec(),
                })
                .await?;
            }
            _ => {
                self.invoke(&tl::functions::messages::DeleteMessages {
                    revoke: true,
                    id: ids.to_vec(),
                })
                .await?;
            }
        }
        Ok(())
    }

    pub fn self_id(&self) -> i64 {
        self.peers.self_id()
    }

    pub fn can_send_rich(&self) -> bool {
        self.self_user().is_some_and(|u| u.premium)
    }

    pub async fn send_text(
        &self,
        peer: &tl::enums::InputPeer,
        text: &str,
        opts: SendOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        self.send_html(peer, &escape(text), opts).await
    }

    pub async fn send_plain(
        &self,
        peer: &tl::enums::InputPeer,
        text: &str,
        entities: Option<Vec<tl::enums::MessageEntity>>,
        opts: SendOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        let req = build_send_message(
            peer.clone(),
            text.to_owned(),
            entities,
            opts.no_webpage,
            opts.reply_to,
            opts.topic_id,
            None,
        );
        self.invoke(&req).await
    }

    pub async fn edit_message_html(
        &self,
        peer: &tl::enums::InputPeer,
        msg_id: i32,
        html: &str,
        link_preview: bool,
    ) -> Result<(), BotError> {
        let opts = SendOptions {
            no_webpage: !link_preview,
            ..Default::default()
        };
        match self.edit_html(peer, msg_id, html, opts).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_rpc("MESSAGE_NOT_MODIFIED") => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn edit_text(
        &self,
        peer: &tl::enums::InputPeer,
        msg_id: i32,
        text: &str,
        link_preview: bool,
    ) -> Result<(), BotError> {
        self.edit_message_html(peer, msg_id, &escape(text), link_preview)
            .await
    }

    pub async fn delete_message(&self, msg: &Message) -> Result<(), BotError> {
        let peer = self.input_peer(&msg.peer)?;
        self.delete_messages(&peer, &[msg.id]).await
    }

    pub async fn forward_messages(
        &self,
        from: &tl::enums::InputPeer,
        to: &tl::enums::InputPeer,
        ids: &[i32],
        topic_id: Option<i32>,
    ) -> Result<tl::enums::Updates, BotError> {
        let random_ids: Vec<i64> = ids.iter().map(|_| random_id()).collect();
        let req = tl::functions::messages::ForwardMessages {
            silent: false,
            background: false,
            with_my_score: false,
            drop_author: false,
            drop_media_captions: false,
            noforwards: false,
            allow_paid_floodskip: false,
            from_ephemeral: false,
            from_peer: from.clone(),
            id: ids.to_vec(),
            random_id: random_ids,
            to_peer: to.clone(),
            top_msg_id: topic_id.filter(|&id| id > 0),
            reply_to: None,
            schedule_date: None,
            schedule_repeat_period: None,
            send_as: None,
            quick_reply_shortcut: None,
            effect: None,
            video_timestamp: None,
            allow_paid_stars: None,
            suggested_post: None,
        };
        self.invoke(&req).await
    }

    pub async fn full_rich(
        &self,
        peer: &tl::enums::InputPeer,
        message: tl::types::Message,
    ) -> Result<tl::types::Message, BotError> {
        let is_part = matches!(
            &message.rich_message,
            Some(tl::enums::RichMessage::Message(r)) if r.part
        );
        if !is_part {
            return Ok(message);
        }
        let msgs = self
            .invoke(&tl::functions::messages::GetRichMessage {
                peer: peer.clone(),
                id: message.id,
            })
            .await?;
        let items = match msgs {
            tl::enums::messages::Messages::Messages(m) => m.messages,
            tl::enums::messages::Messages::Slice(m) => m.messages,
            tl::enums::messages::Messages::ChannelMessages(m) => m.messages,
            tl::enums::messages::Messages::NotModified(_) => Vec::new(),
        };
        for item in items {
            if let tl::enums::Message::Message(full) = item
                && full.id == message.id
            {
                return Ok(full);
            }
        }
        Ok(message)
    }

    pub async fn ping(&self) -> Result<Duration, BotError> {
        let started = std::time::Instant::now();
        self.invoke(&tl::functions::Ping {
            ping_id: random_id(),
        })
        .await?;
        Ok(started.elapsed())
    }

    pub fn input_peer_from_chat_id(&self, chat_id: &str) -> Result<tl::enums::InputPeer, BotError> {
        let peer = peer_from_id(chat_id)
            .ok_or_else(|| BotError::UnaddressablePeer(chat_id.to_owned()))?;
        self.input_peer(&peer)
    }

    pub async fn resolve_username(&self, username: &str) -> Result<tl::enums::InputPeer, BotError> {
        let clean = username.trim().trim_start_matches('@');
        if clean.is_empty() {
            return Err(BotError::UnaddressablePeer(username.to_owned()));
        }
        let tl::enums::contacts::ResolvedPeer::Peer(res) = self
            .invoke(&tl::functions::contacts::ResolveUsername {
                username: clean.to_owned(),
                referer: None,
            })
            .await?;
        self.input_peer(&res.peer)
    }

    pub async fn resolve_target(&self, target: &str) -> Result<tl::enums::InputPeer, BotError> {
        let trimmed = target.trim();
        if trimmed.is_empty() || trimmed == "me" || trimmed == "self" {
            return Ok(tl::enums::InputPeer::PeerSelf);
        }
        if let Some(rest) = trimmed.strip_prefix('@') {
            return self.resolve_username(rest).await;
        }
        if let Some(peer) = peer_from_id(trimmed) {
            return self.input_peer(&peer);
        }
        self.resolve_username(trimmed).await
    }

    pub async fn send_document(
        &self,
        peer: &tl::enums::InputPeer,
        data: &[u8],
        opts: MediaOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        let file = self.upload_bytes(&opts.name, data).await?;
        let mut attrs = Vec::with_capacity(opts.attributes.len() + 1);
        if !opts.name.is_empty() {
            attrs.push(
                tl::types::DocumentAttributeFilename {
                    file_name: opts.name.clone(),
                }
                .into(),
            );
        }
        attrs.extend(opts.attributes.iter().cloned());
        let media: tl::enums::InputMedia = tl::types::InputMediaUploadedDocument {
            nosound_video: false,
            force_file: opts.force_document,
            spoiler: false,
            file,
            thumb: None,
            mime_type: if opts.mime_type.is_empty() {
                "application/octet-stream".into()
            } else {
                opts.mime_type.clone()
            },
            attributes: attrs,
            stickers: None,
            video_cover: None,
            video_timestamp: None,
            ttl_seconds: None,
        }
        .into();
        self.send_media(
            peer,
            media,
            MediaOptions {
                html: opts.html || !opts.caption.is_empty(),
                ..opts
            },
        )
        .await
    }

    pub async fn send_photo(
        &self,
        peer: &tl::enums::InputPeer,
        data: &[u8],
        opts: MediaOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        let file = self.upload_bytes(&opts.name, data).await?;
        let media: tl::enums::InputMedia = tl::types::InputMediaUploadedPhoto {
            spoiler: false,
            live_photo: false,
            file,
            stickers: None,
            ttl_seconds: None,
            video: None,
        }
        .into();
        self.send_media(
            peer,
            media,
            MediaOptions {
                html: opts.html || !opts.caption.is_empty(),
                ..opts
            },
        )
        .await
    }

    pub async fn copy_rich(
        &self,
        peer: &tl::enums::InputPeer,
        message: &tl::types::Message,
        opts: SendOptions,
    ) -> Result<tl::enums::Updates, BotError> {
        if let Some(tl::enums::RichMessage::Message(rich)) = &message.rich_message
            && self.can_send_rich()
        {
            let input_rich: tl::enums::InputRichMessage = tl::types::InputRichMessage {
                rtl: rich.rtl,
                noautolink: false,
                blocks: rich.blocks.clone(),
                photos: None,
                documents: None,
                users: None,
            }
            .into();
            match self.send_rich(peer, input_rich, opts.clone()).await {
                Ok(u) => return Ok(u),
                Err(e) if !rich_rejected(&e) => return Err(e),
                Err(_) => {}
            }
        }
        let plain = plain_copy(&rich_message_text(message));
        self.send_plain(peer, &plain, None, opts).await
    }

    pub async fn get_reply(&self, message: &Message) -> Result<Option<Message>, BotError> {
        if message.reply_to_id == 0 {
            return Ok(None);
        }
        let target = message.reply_to_peer.as_ref().unwrap_or(&message.peer);
        let peer = self.input_peer(target)?;
        let found = self.get_messages(&peer, &[message.reply_to_id]).await?;
        for candidate in found {
            if candidate.id == message.reply_to_id {
                let full = self.full_rich(&peer, candidate.clone()).await.unwrap_or(candidate);
                return Ok(envelope(&full, self.peers.self_id(), false, &self.peers));
            }
        }
        Ok(None)
    }
}

fn prepare_request<R: Serializable + Any>(request: &R) -> PreparedRequest {
    let any = request as &dyn Any;
    let privacy_on = !is_ip_privacy_off();
    let policy = if privacy_on {
        Some(current_ip_policy())
    } else {
        None
    };

    if let Some(req) = any.downcast_ref::<tl::functions::channels::EditBanned>() {
        let tl::enums::ChatBannedRights::Rights(rights) = &req.banned_rights;
        return PreparedRequest {
            body: req.to_bytes(),
            edit_banned_until: Some(rights.until_date),
            request_peer: None,
            record_sent: false,
        };
    }

    if let Some(req) = any.downcast_ref::<tl::functions::messages::SendMessage>() {
        let peer = req.peer.clone();
        let body = if let Some(policy) = &policy {
            let mut cloned = req.clone();
            ipprivacy::redact_send_message(&mut cloned, policy);
            cloned.to_bytes()
        } else {
            req.to_bytes()
        };
        return PreparedRequest {
            body,
            edit_banned_until: None,
            request_peer: Some(peer),
            record_sent: true,
        };
    }

    if let Some(req) = any.downcast_ref::<tl::functions::messages::EditMessage>() {
        let body = if let Some(policy) = &policy {
            let mut cloned = req.clone();
            ipprivacy::redact_edit_message(&mut cloned, policy);
            cloned.to_bytes()
        } else {
            req.to_bytes()
        };
        return PreparedRequest {
            body,
            edit_banned_until: None,
            request_peer: None,
            record_sent: false,
        };
    }

    if let Some(req) = any.downcast_ref::<tl::functions::messages::SendMedia>() {
        let peer = req.peer.clone();
        let body = if let Some(policy) = &policy {
            let mut cloned = req.clone();
            ipprivacy::redact_send_media(&mut cloned, policy);
            cloned.to_bytes()
        } else {
            req.to_bytes()
        };
        return PreparedRequest {
            body,
            edit_banned_until: None,
            request_peer: Some(peer),
            record_sent: true,
        };
    }

    if let Some(req) = any.downcast_ref::<tl::functions::messages::SendMultiMedia>() {
        let peer = req.peer.clone();
        let body = if let Some(policy) = &policy {
            let mut cloned = req.clone();
            ipprivacy::redact_send_multi_media(&mut cloned, policy);
            cloned.to_bytes()
        } else {
            req.to_bytes()
        };
        return PreparedRequest {
            body,
            edit_banned_until: None,
            request_peer: Some(peer),
            record_sent: true,
        };
    }

    if let Some(req) = any.downcast_ref::<tl::functions::messages::ForwardMessages>() {
        return PreparedRequest {
            body: req.to_bytes(),
            edit_banned_until: None,
            request_peer: Some(req.to_peer.clone()),
            record_sent: true,
        };
    }

    PreparedRequest {
        body: request.to_bytes(),
        edit_banned_until: None,
        request_peer: None,
        record_sent: false,
    }
}

fn opt_entities(entities: Vec<tl::enums::MessageEntity>) -> Option<Vec<tl::enums::MessageEntity>> {
    if entities.is_empty() {
        None
    } else {
        Some(entities)
    }
}

fn make_reply_to(reply_to: Option<i32>, topic_id: Option<i32>) -> Option<tl::enums::InputReplyTo> {
    let reply = reply_to.filter(|&id| id > 0);
    let topic = topic_id.filter(|&id| id > 0);
    if reply.is_none() && topic.is_none() {
        return None;
    }
    Some(
        tl::types::InputReplyToMessage {
            reply_to_msg_id: reply.or(topic).unwrap_or(0),
            top_msg_id: topic,
            reply_to_peer_id: None,
            quote_text: None,
            quote_entities: None,
            quote_offset: None,
            monoforum_peer_id: None,
            todo_item_id: None,
            poll_option: None,
            quote_rich: None,
        }
        .into(),
    )
}

fn build_send_message(
    peer: tl::enums::InputPeer,
    message: String,
    entities: Option<Vec<tl::enums::MessageEntity>>,
    no_webpage: bool,
    reply_to: Option<i32>,
    topic_id: Option<i32>,
    rich_message: Option<tl::enums::InputRichMessage>,
) -> tl::functions::messages::SendMessage {
    tl::functions::messages::SendMessage {
        no_webpage,
        silent: false,
        background: false,
        clear_draft: false,
        noforwards: false,
        update_stickersets_order: false,
        invert_media: false,
        allow_paid_floodskip: false,
        peer,
        reply_to: make_reply_to(reply_to, topic_id),
        message,
        random_id: random_id(),
        reply_markup: None,
        entities,
        schedule_date: None,
        schedule_repeat_period: None,
        send_as: None,
        quick_reply_shortcut: None,
        effect: None,
        allow_paid_stars: None,
        suggested_post: None,
        rich_message,
    }
}

fn build_edit_message(
    peer: tl::enums::InputPeer,
    id: i32,
    message: Option<String>,
    entities: Option<Vec<tl::enums::MessageEntity>>,
    media: Option<tl::enums::InputMedia>,
    no_webpage: bool,
    rich_message: Option<tl::enums::InputRichMessage>,
) -> tl::functions::messages::EditMessage {
    tl::functions::messages::EditMessage {
        no_webpage,
        invert_media: false,
        peer,
        id,
        message,
        media,
        reply_markup: None,
        entities,
        schedule_date: None,
        schedule_repeat_period: None,
        quick_reply_shortcut_id: None,
        rich_message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::sync::Mutex;

    type RecordedCall = (Option<i32>, u32, Vec<u8>);

    struct StubTransport {
        home_dc: i32,
        calls: Mutex<Vec<RecordedCall>>,
        replies: Mutex<VecDeque<Result<Vec<u8>, InvocationError>>>,
    }

    impl StubTransport {
        fn new(replies: Vec<Result<Vec<u8>, InvocationError>>) -> Self {
            Self {
                home_dc: 5,
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(VecDeque::from(replies)),
            }
        }
    }

    impl RpcTransport for StubTransport {
        fn invoke_raw<'a>(
            &'a self,
            dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                self.calls.lock().await.push((dc_id, constructor_id, body));
                self.replies
                    .lock()
                    .await
                    .pop_front()
                    .unwrap_or_else(|| Err(InvocationError::Dropped("empty stub replies")))
            })
        }

        fn home_dc_id(&self) -> i32 {
            self.home_dc
        }
    }

    fn short_sent(id: i32) -> tl::enums::Updates {
        tl::enums::Updates::UpdateShortSentMessage(tl::types::UpdateShortSentMessage {
            out: true,
            id,
            pts: 1,
            pts_count: 1,
            date: 100,
            media: None,
            entities: None,
            ttl_period: None,
        })
    }

    #[tokio::test]
    async fn client_invoke_redacts_ip_records_sent_and_filters_get_messages() {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(10);
        let seen = Arc::new(std::sync::Mutex::new(Vec::<(String, i32)>::new()));
        let seen_clone = seen.clone();
        let sent = Arc::new(SentRecorder::new(Arc::new(move |p, id| {
            seen_clone.lock().unwrap().push((peer_id(p), id));
        })));

        // Prepare: 1) SendMessage reply, 2) without_ip_privacy SendMessage reply,
        // 3) EditMessage reply, 4) GetMessages reply containing cross-dialog noise.
        let foreign_msg = message::blank_tl_message(
            77,
            tl::types::PeerUser { user_id: 999 }.into(),
        );
        let target_msg = message::blank_tl_message(
            88,
            tl::types::PeerUser { user_id: 42 }.into(),
        );
        let msgs_resp: tl::enums::messages::Messages = tl::types::messages::Messages {
            messages: vec![foreign_msg.into(), target_msg.into()],
            topics: vec![],
            chats: vec![],
            users: vec![tl::enums::User::User(tl::types::User {
                is_self: false,
                contact: false,
                mutual_contact: false,
                deleted: false,
                bot: false,
                bot_chat_history: false,
                bot_nochats: false,
                verified: false,
                restricted: false,
                min: false,
                bot_inline_geo: false,
                support: false,
                scam: false,
                apply_min_photo: false,
                fake: false,
                bot_attach_menu: false,
                premium: false,
                attach_menu_enabled: false,
                bot_can_edit: false,
                close_friend: false,
                stories_hidden: false,
                stories_unavailable: true,
                contact_require_premium: false,
                bot_business: false,
                bot_has_main_app: false,
                bot_forum_view: false,
                bot_forum_can_manage_topics: false,
                bot_can_manage_bots: false,
                bot_guestchat: false,
                bot_guard: false,
                bot_forum_sidebar: false,
                id: 42,
                access_hash: Some(4242),
                first_name: Some("Alice".into()),
                last_name: None,
                username: None,
                phone: None,
                photo: None,
                status: None,
                bot_info_version: None,
                restriction_reason: None,
                bot_inline_placeholder: None,
                lang_code: None,
                emoji_status: None,
                usernames: None,
                stories_max_id: None,
                color: None,
                profile_color: None,
                bot_active_users: None,
                bot_verification_icon: None,
                send_paid_messages_stars: None,
                linked_community_id: None,
            })],
        }
        .into();

        let stub = Arc::new(StubTransport::new(vec![
            Ok(short_sent(101).to_bytes()),
            Ok(short_sent(102).to_bytes()),
            Ok(short_sent(103).to_bytes()),
            Ok(msgs_resp.to_bytes()),
        ]));
        let client = Client::from_transport(stub.clone(), peers.clone(), sent);

        let peer = tl::enums::InputPeer::User(tl::types::InputPeerUser {
            user_id: 42,
            access_hash: 4242,
        });
        client
            .send_html(&peer, "ip 1.2.3.4", SendOptions::default())
            .await
            .unwrap();
        without_ip_privacy(|| client.send_html(&peer, "ip 1.2.3.4", SendOptions::default()))
            .await
            .unwrap();
        client
            .edit_html(&peer, 101, "edited 1.2.3.4", SendOptions::default())
            .await
            .unwrap();
        let fetched = client.get_messages(&peer, &[77, 88]).await.unwrap();

        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].id, 88);
        assert!(peers.user(42).is_some_and(|u| u.hash == 4242));

        let recorded = seen.lock().unwrap().clone();
        assert_eq!(recorded, vec![("42".into(), 101), ("42".into(), 102)]);

        let calls = stub.calls.lock().await;
        // First SendMessage must NOT contain literal 1.2.3.4; second must contain 1.2.3.4.
        assert!(!calls[0].2.windows(7).any(|w| w == b"1.2.3.4"));
        assert!(calls[1].2.windows(7).any(|w| w == b"1.2.3.4"));
        // EditMessage must NOT contain literal 1.2.3.4 either.
        assert!(!calls[2].2.windows(7).any(|w| w == b"1.2.3.4"));
    }

    #[tokio::test]
    async fn send_rich_falls_back_on_400_but_not_on_uncertain_or_flood() {
        let peers = Arc::new(PeerCache::new());
        let sent = Arc::new(SentRecorder::default());
        let rpc_400 = InvocationError::Rpc(RpcError {
            code: 400,
            name: "RICH_MESSAGE_INVALID".into(),
            value: None,
            caused_by: None,
        });
        let stub = Arc::new(StubTransport::new(vec![
            Err(rpc_400),
            Ok(short_sent(201).to_bytes()),
            Err(InvocationError::Dropped("socket lost mid-write")),
        ]));
        let client = Client::from_transport(stub.clone(), peers, sent);
        let peer = tl::enums::InputPeer::PeerSelf;
        let rich: tl::enums::InputRichMessage = tl::types::InputRichMessage {
            rtl: false,
            noautolink: false,
            blocks: vec![tl::types::PageBlockParagraph {
                text: rich_plain("hello"),
            }
            .into()],
            photos: None,
            documents: None,
            users: None,
        }
        .into();

        // 400 falls back to plain message.
        let res = client
            .send_rich(&peer, rich.clone(), SendOptions::default())
            .await;
        assert!(res.is_ok());
        assert_eq!(stub.calls.lock().await.len(), 2);

        // Uncertain failure does NOT double-send.
        let err = client
            .send_rich(&peer, rich, SendOptions::default())
            .await
            .unwrap_err();
        assert!(err.is_uncertain());
        assert_eq!(stub.calls.lock().await.len(), 3);
    }

    #[tokio::test]
    async fn download_file_migrates_and_enforces_limit() {
        let peers = Arc::new(PeerCache::new());
        peers.set_self(1);
        let sent = Arc::new(SentRecorder::default());

        let file_migrate = InvocationError::Rpc(RpcError {
            code: 303,
            name: "FILE_MIGRATE".into(),
            value: Some(2),
            caused_by: None,
        });
        let exported: tl::enums::auth::ExportedAuthorization =
            tl::types::auth::ExportedAuthorization {
                id: 9,
                bytes: vec![1, 2, 3],
            }
            .into();
        let imported: tl::enums::auth::Authorization = tl::types::auth::Authorization {
            setup_password_required: false,
            otherwise_relogin_days: None,
            tmp_sessions: None,
            future_auth_token: None,
            user: tl::enums::User::Empty(tl::types::UserEmpty { id: 1 }),
        }
        .into();
        let file_chunk: tl::enums::upload::File = tl::types::upload::File {
            r#type: tl::enums::storage::FileType::FilePng,
            mtime: 0,
            bytes: vec![7; 16],
        }
        .into();

        let stub = Arc::new(StubTransport::new(vec![
            Err(file_migrate),
            Ok(exported.to_bytes()),
            Ok(imported.to_bytes()),
            Ok(file_chunk.clone().to_bytes()),
            Ok(file_chunk.to_bytes()),
        ]));
        let client = Client::from_transport(stub.clone(), peers, sent);
        let loc: tl::enums::InputFileLocation = tl::types::InputDocumentFileLocation {
            id: 10,
            access_hash: 20,
            file_reference: vec![],
            thumb_size: String::new(),
        }
        .into();

        let data = client.download_file(loc.clone(), 2, 32).await.unwrap();
        assert_eq!(data, vec![7; 16]);
        let calls = stub.calls.lock().await;
        assert_eq!(calls[2].0, Some(2));
        assert_eq!(calls[3].0, Some(2));
        drop(calls);

        let err = client.download_file(loc, 2, 8).await.unwrap_err();
        assert!(matches!(err, BotError::TooLarge(_)));
    }
}
