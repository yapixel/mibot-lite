# Research: Rust Telegram MTProto Crates (`grammers` vs `gotd/td`)

- **Date**: 2026-11-27
- **Scope**: Evaluate Rust MTProto crates against primary sources (`crates.io`, `docs.rs`, `grammers` `0.12.0` crate source tarballs from `https://codeberg.org/Lonami/grammers`) and map every MTProto capability used by `mibot-lite` (`github.com/gotd/td v0.162.0`, TL Layer `229`) onto Rust (`ADR-005`, `ADR-006`).
- **Status**: Complete — ready to guide Wave 1 (`mibot_session`, `mibot_tgstate`, `mibot_bot`) and Wave 3/4 (`mibot_app`, `mibot_login`).

---

## 1. Executive Summary & Recommended Crate Versions

**Recommendation**: Adopt the **`grammers` `0.12.0` crate family** (published 2026-11-08, `edition = "2024"`, MSRV Rust `1.85+`), pinned to exact patch parity across all workspace crates, with **`grammers-session` `default-features = false`** (to exclude `libsql` / C `sqlite3` per `ADR-006`) and a custom `MibotSession` + async middleware wrapper (`mibot_bot::Client`) replicating `mibot-lite`'s `Retrier`, `IPRedactor`, `PeerCache`, `LiveHandler`, and `subscribeGuard`.

| Crate | Version | Published | Features in `mibot-lite` | Role |
| --- | --- | --- | --- | --- |
| `grammers-client` | `=0.12.0` | 2026-11-08 | `default-features = false, features = ["proxy"]` (avoid `html`, `markdown`, `fs` optional) | High-level `Client`, auth, `upload_stream`, `iter_download`, `UpdateStream` |
| `grammers-mtsender` | `=0.12.0` | 2026-11-08 | `features = ["proxy"]` | `SenderPool`, `SenderPoolRunner`, `UpdatesReceiver`, `RpcError`, `InvocationError` |
| `grammers-session` | `=0.12.0` | 2026-11-08 | `default-features = false, features = ["serde"]` | `Session` trait, `DcOption`, `PeerId`/`PeerRef`/`PeerInfo`, `MessageBoxes`, `UpdatesLike` |
| `grammers-tl-types` | `=0.12.0` | 2026-11-08 | `tl-api` (default), `impl-debug`, `impl-from-enum`, `impl-from-type` | Generated Telegram TL Layer **`233`** types/functions & `RemoteCall` |
| `grammers-mtproto` | `=0.12.0` | 2026-11-08 | default (`[]`) | Sans-IO MTProto 2.0 framing, `Encrypted`, `BadServerSalt` handling (transitive) |
| `grammers-crypto` | `=0.12.0` | 2026-11-08 | default (`[]`) | Pure-Rust AES-IGE/CTR/SHA/DH/2FA SRP primitives (transitive) |

### Alternative Crates Considered
- **`ferogram` (`0.6.7`, published 2026-11-10, `github.com/ankit-chaubey/ferogram`)**: Pure-Rust async MTProto framework (`ferogram-connect`, `ferogram-crypto`, `ferogram-session`, `ferogram-tl-types`). Much younger ecosystem (2,927 all-time / 1,367 recent downloads on crates.io vs `grammers-client`'s 298,291 all-time / 145,782 recent downloads) with less battle-tested `MessageBox` gap-recovery and DC-migration handling. Not recommended over `grammers`.

---

## 2. TL Layer & Raw TL `invoke` Ergonomics (`grammers-tl-types` vs `gotd/td/tg`)

### 2.1 Supported TL Layer & Schema Coverage
- **`gotd/td v0.162.0`** (`go.mod`): compiled against **Layer `229`** (`tg/tl_registry_gen.go`).
- **`grammers-tl-types 0.12.0`**: compiled from `tl/api.tl` at **`LAYER 233`** (`pub const LAYER: i32 = 233`).
- Every RPC and constructor currently used across `mibot-lite` (37 distinct `*Request` types, 297 `tg.*` symbols) was verified present in `grammers-tl-types-0.12.0/tl/api.tl`, including:
  - Messaging & Rich Messages: `messages.sendMessage`, `messages.editMessage`, `messages.deleteMessages`, `channels.deleteMessages`, `messages.forwardMessages`, `messages.getMessages`, `channels.getMessages`, `messages.sendMedia`, `messages.sendMultiMedia`, `messages.uploadMedia`, `messages.getRichMessage`, `inputRichMessage`, `richMessage`, `PageBlock*`, `RichText*`.
  - Peers, Admin & Dialogs: `contacts.resolveUsername`, `messages.getDialogs`, `messages.getFullChat`, `channels.getChannels`, `channels.getFullChannel`, `channels.getParticipant`, `channels.getParticipants`, `channels.editBanned`, `channels.deleteParticipantHistory`, `channels.getSendAs`, `account.updateProfile`, `users.getUsers`.
  - Files & Stickers: `upload.getFile`, `upload.saveFilePart`, `upload.saveBigFilePart`, `stickers.createStickerSet`, `stickers.addStickerToSet`, `messages.getStickerSet`, `messages.getAllStickers`.

### 2.2 Raw `invoke` Ergonomics
| Aspect | `gotd/td` (Go) | `grammers` `0.12.0` (Rust) | Migration Impact |
| --- | --- | --- | --- |
| Raw RPC call | `c.api.MessagesSendMessage(ctx, req)` (one typed Go method per RPC) | `client.invoke(&tl::functions::messages::SendMessage { ... }).await` (single generic method `R: tl::RemoteCall`) | Cleaner single dispatch point; `R::Return` is strongly typed via `RemoteCall::Return`. |
| Cross-DC invoke | `c.tg.DC(ctx, dcID, 1)` returns separate `tg.Invoker` | `client.invoke_in_dc(dc_id, &req).await` on the same `SenderPoolHandle` | Simpler pooling, but see §6 for lack of per-connect timeout. |
| Optional TL flags | `req.SetReplyTo(...)` / `msg.GetMedia()` | Plain struct fields with `Option<T>` or `bool` | **Trap**: TL structs do **not** implement `Default` — every optional field (`None`/`false`) must be listed explicitly in raw TL literals (e.g., `messages::SendMessage` has 21 fields). Prefer `InputMessage` / helper builders in `mibot_bot` where possible. |
| Polymorphic TL types | `*Class` interfaces + type switch (`*tg.Message`, `*tg.PeerChannel`) | `tl::enums::*` + exhaustive `match` (`tl::enums::Message::Message(m)`) | Exhaustive `match` catches new TL variants at compile time; `impl-from-type` / `impl-from-enum` features give `.into()` / `.try_into()`. |
| Middleware / hooks | `telegram.Middleware` (`bin.Encoder`, `bin.Decoder` + `reflect`) | **No middleware trait** in `grammers-client` or `grammers-mtsender` | Wrap `SenderPoolHandle` in a `mibot_bot::Client` facade with typed/generic `invoke` methods — eliminates Go's `reflect` hacks in `EntityRecorder` and `redactRichField`. |

---

## 3. Session Storage & `StringSession` Compatibility

### 3.1 `grammers-session` Model vs `internal/session/gramjs.go` & `internal/tgstate/`
`grammers_session::Session` (`grammers-session-0.12.0/src/session.rs`) stores four state domains:
1. `home_dc_id() -> Result<i32, Self::Error>` / `set_home_dc_id(dc_id)`
2. `dc_option(dc_id) -> Result<Option<DcOption>, Self::Error>` / `set_dc_option(&DcOption)` where `DcOption` (`src/types.rs:22-45`) is `{ id: i32, ipv4: SocketAddrV4, ipv6: SocketAddrV6, auth_key: Option<[u8; 256]> }`.
3. `peer(PeerId)` / `cache_peer(PeerInfo)` / `cache_peers(Vec<PeerInfo>)`.
4. `updates_state() -> UpdatesState` / `set_update_state(UpdateState)` (`pts`, `qts`, `date`, `seq`, `channels: Vec<ChannelState { id, pts }>`).

### 3.2 Compatibility with `session.StringSession`
- `internal/session/gramjs.go` decodes version `'1'` GramJS/Teleproto (plus 352-char Telethon IPv4 and IPv6) `StringSession` into `{ DC, Address, Port, AuthKey [256]byte }`. It carries **no `server_salt` and no `Config`**.
- **Verified compatible with `grammers`**:
  - Mapping `StringSession` -> `DcOption { id: dc, ipv4/ipv6: addr:port, auth_key: Some(key) }` and `home_dc = dc` works directly with `grammers-mtsender`.
  - When `DcOption.auth_key` is `Some(auth_key)`, `SenderPoolRunner::connect_sender` (`sender_pool.rs:450-465`) calls `connect_with_auth(..., auth_key)` which initializes `mtp::Encrypted::build().finish(auth_key)` with `first_salt = 0` (`encrypted.rs:114-161`).
  - On the initial `InvokeWithLayer(LAYER, InitConnection(..., help::GetConfig))`, Telegram returns `bad_server_salt` if salt `0` is rejected; `mtp::Encrypted::handle_bad_notification` (`encrypted.rs:683-694`) captures `new_server_salt` and `Sender` automatically retries the request (`sender.rs:453-462`), while `update_config` populates all DCs' `ipv4`/`ipv6` endpoints.
- **Exporting back to `StringSession` (`--login` / `config.json`)**: `DcOption` stores `id`, `ipv4`, `ipv6`, and `auth_key: Option<[u8; 256]>`, matching `FromData` / `ResolveAddress` in `internal/session/gramjs.go:215-275`.

### 3.3 Required Custom `Session` Implementation & Traps
- **Avoid `SqliteSession`**: `grammers-session` enables `sqlite-storage` by default (`Cargo.toml:39`), pulling `libsql 0.10.0` (which compiles C `sqlite3`). Set **`default-features = false`** on `grammers-session` and implement `MibotSession: Session` backed by `config.json` + `tgstate` (`updates.json` + `updates.json.cursor`) per `ADR-003` and `ADR-006`.
- **Trap 1 — `PeerId::self_user()` must be seeded before `stream_updates`**: `StringSession` does not store the account's own user ID or `is_self` flag. `UpdatesReceiver::create` (`updates_receiver.rs:100-101`) checks `session.peer(PeerId::self_user()).await?.is_some()` to decide whether to call `updates.getState`. Call `client.get_me()` (and cache `PeerInfo::User { id, auth, bot, is_self: Some(true) }`) before starting `stream_updates`.
- **Trap 2 — `auto_cache_peers` bulk-write bloat**: `ClientConfiguration::default()` sets `auto_cache_peers: true`, causing `build_peer_map` (`chats.rs:924-937`) to call `session.cache_peers(...)` on every `iter_participants` / `iter_messages` / `iter_dialogs` page. For `.clean_member` on a 50k-member group (`internal/bot/peers.go:195-199`), persisting every hash to `updates.json` causes severe write amplification and memory growth. Either set `auto_cache_peers: false` and replicate `EntityRecorder` + `maxPersistBatch = 32` in `MibotSession`, or enforce the `<= 32` persistence cap inside `MibotSession::cache_peers` (always keeping all hashes in memory).
- **Trap 3 — `min` entities**: `grammers` already guards `.filter(|_| !user.min)` / `.filter(|_| !channel.min)` in `User::auth()` / `Channel::auth()`, matching `PeerCache`. Keep the same `!min` invariant in `MibotSession` and never call `PeerId::to_ambient_ref()` (`PeerAuth(0)`) for uncached peers (`ErrUnaddressablePeer` invariant in `internal/bot/client.go:27-29`).
- **Trap 4 — `PeerId` range check**: `PeerId::from_bot_api_dialog_id` (`peer.rs:186-196`) validates strict ranges (`1..=0xffffffffff` for users, `1..=997852516352` and monoforum range for channels). Ensure `PeerFromID` helpers propagate `None` -> error cleanly instead of using `*_unchecked` variants on untrusted input.

---

## 4. Updates Gap Filling & Pre-Gap-Fill `liveHandler` Bypass

### 4.1 How `grammers` Processes Updates
- `SenderPool::new` returns `{ runner, handle, updates: mpsc::Receiver<UpdatesLike> }`.
- Inside `Sender::process_own_update` (`sender.rs:330-384`), `grammers` **already** intercepts `OwnUpdate` from RPC responses (`messages.sendMessage` -> `UpdatesLike::ShortSentMessage`, `channels.deleteMessages` -> `UpdatesLike::AffectedChannelMessages`, `Updates`, etc.) and pushes them into `pool.updates` so `MessageBoxes` advances `pts` without falling behind.
- `Client::stream_updates(updates_rx, UpdatesConfiguration { catch_up })` passes `updates_rx` to `UpdatesReceiver` (`updates_receiver.rs`), which drives `grammers_session::updates::MessageBoxes`:
  - `POSSIBLE_GAP_TIMEOUT = 500ms`, `NO_UPDATES_TIMEOUT = 15m` (`defs.rs:36-49`).
  - `MessageBoxes::load(state)` (`message_box/mod.rs:114-153`) loads saved `pts`, `qts`, `date`, `seq`, and all `channels`.

### 4.2 Pre-Gap-Fill `liveHandler` Bypass Is Possible
Unlike `gotd` (which provides a single `UpdateHandler` interface), `grammers` decouples `SenderPool.updates: mpsc::Receiver<UpdatesLike>` from `UpdatesReceiver`:
- **Design**: Spawn a lightweight `LiveHandler` task that **owns `pool.updates`**:
  1. Continuously reads `UpdatesLike` from `pool.updates`.
  2. For `UpdatesLike::Updates(ref u)`, immediately extracts `UpdateNewMessage` / `UpdateNewChannelMessage` / `UpdateShortMessage` / `UpdateShortChatMessage` (plus `Users`/`Chats` for `PeerCache`) and dispatches them to `App::handle` **without waiting for `MessageBoxes`**.
  3. For `UpdatesLike::ShortSentMessage` / own `Updates`, records message IDs in `seenMessages` (`SentRecorder` equivalent) so gap-fill replays do not re-trigger commands.
  4. Forwards the owned `UpdatesLike` via `try_send` to a bounded `mpsc::Sender<UpdatesLike>` consumed by `UpdatesReceiver` (using a generous channel capacity, e.g., 512–1024).
- Because `LiveHandler` drains `pool.updates` independently, a stalled `UpdatesReceiver` **cannot block `LiveHandler`** and will not backpressure `SenderPoolRunner` (whose internal `updates_tx` defaults to capacity `100` and **drops** updates via `try_send` when full, `sender_pool.rs:542-555`).

### 4.3 Channel `pts` Stalls & Gap-Filler Traps in `grammers`
1. **Sequential startup catch-up**: `MessageBoxes::load(state)` puts **every** channel in `UpdatesState::channels` into `getting_diff_for` (`message_box/mod.rs:144`). `UpdatesReceiver::next()` (`updates_receiver.rs:150-225`) processes `getting_diff_for` **one channel at a time sequentially** (`USER_CHANNEL_DIFF_LIMIT = 100`) and **does not poll `self.updates.recv()`** while `getting_diff_for` is non-empty! For accounts in 200+ channels, startup catch-up can take many seconds/minutes; the `LiveHandler` bypass is therefore **essential** so live commands respond immediately during startup catch-up.
2. **Uncaught `GetChannelDifference` errors terminate `UpdatesReceiver::next()`**: `updates_receiver.rs:178-220` catches `PERSISTENT_TIMESTAMP_OUTDATED`, `CHANNEL_PRIVATE`, and `rpc_error.code == 500`, **but returns `Err(e)`** on `420 FLOOD_WAIT` (if `RetryPolicy` breaks), `502`/`503`/`504`, or `CHANNEL_INVALID`. Run `UpdatesReceiver` in a supervised restart loop (log, sleep/backoff, recreate `UpdatesReceiver` from `Session::updates_state()`) so a single channel error never kills update state tracking.
3. **Missing channel hash deletes channel `pts`**: If `prepare_channel_difference` (`updates_receiver.rs:50-82`) cannot find `PeerInfo::Channel { auth: Some(..), .. }` in `Session`, it logs a warning and calls `end_channel_difference(PrematureEndReason::Banned)`, which **removes** that channel from `MessageBoxes` (`pop_entry`). Ensure `tgstate` channel hashes stay populated alongside channel `pts`.
4. **No auto-reconnect without an active RPC (`subscribeGuard` equivalent)**: When the home DC socket dies, `run_sender` emits `UpdatesLike::ConnectionClosed` and exits (`sender_pool.rs:567-570`). `SenderPoolRunner` only dials a new socket when `Request::Invoke` arrives (`create_connection`). Normally `UpdatesReceiver` turns `ConnectionClosed` into `Gap` -> `GetDifference` -> triggers reconnect; if `UpdatesReceiver` is restarting or `GetDifference` fails, the account sits disconnected silently. Port `subscribeGuard` (`internal/app/subscribe.go`) as a connection watchdog that backs off (`2s..60s` or `FLOOD_WAIT`) and calls `get_me()` / `updates::GetState` after reconnects/errors.
5. **`channelDifferenceTooLong`**: `adapt_channel_difference` (`adaptor.rs:401-421`) skips to `difference.dialog.pts` (matching `gotd`), but `.expect()`s `dialog.pts` to be `Some` and panics if `dialog` is `Folder` or `Community`.

---

## 5. `FLOOD_WAIT` / `FLOOD_PREMIUM_WAIT` / `SLOWMODE_WAIT` / 5xx & `Retrier`

### 5.1 Error Representation in `grammers-mtsender`
`InvocationError::Rpc(RpcError)` (`errors.rs:69-182`) parses `tl::types::RpcError { error_code, error_message }` by splitting on non-ASCII digits and extracting the **first** integer into `value: Option<u32>`, stripping `_<digits>` from `name`:
- `FLOOD_WAIT_31` -> `code: 420, name: "FLOOD_WAIT", value: Some(31)`
- `FLOOD_PREMIUM_WAIT_31` -> `code: 420, name: "FLOOD_PREMIUM_WAIT", value: Some(31)`
- `SLOWMODE_WAIT_15` -> `code: 420, name: "SLOWMODE_WAIT", value: Some(15)`
- `INTERDC_2_CALL_ERROR` -> `code: 500, name: "INTERDC_CALL_ERROR", value: Some(2)`
- `caused_by: Option<u32>` is populated in `Sender::process_error` (`sender.rs:426`) with the 4-byte LE constructor ID (`tl::Identifiable::CONSTRUCTOR_ID`), resolvable via `tl::name_for_id(id)` (e.g. `"channels.getMessages"`).

### 5.2 Why `ConnectionParams::retry_policy` Alone Is Not Enough
`grammers`' `AutoSleep` (`configuration.rs:176-209`) matches `code: 420, value: Some(seconds)` and `InvocationError::Io(_)`, but `RetryPolicy::should_retry(&self, ctx: &RetryContext) -> ControlFlow<(), Duration>` has three structural limitations compared to `internal/bot/retrier.go`:
1. **Post-failure only & synchronous**: It cannot consult a shared per-method `cooldown` map **before** sending a concurrent request, nor can it share cooldowns cleanly without blocking/spinning.
2. **No 5xx / `RPC_CALL_FAIL` / `RPC_MCGET_FAIL` backoff**: `AutoSleep` immediately returns `Break(())` on `5xx` RPC errors.
3. **Interferes with `UpdatesReceiver` (`selfRecovering`)**: `UpdatesReceiver` invokes `updates.getChannelDifference` through `SenderPoolHandle` directly (`updates_receiver.rs:251`), so an aggressive pool-level `RetryPolicy` would stall `UpdatesReceiver` on `PERSISTENT_TIMESTAMP_OUTDATED` / 5xx just like the 2026-09-27 `gotd` incident (`retrier.go:45-50`).

### 5.3 Recommended `Retrier` Architecture
- Set `ConnectionParams.retry_policy = Box::new(NoRetries)` (or a minimal policy that only sleeps short `FLOOD_WAIT <= 10s` for `UpdatesReceiver` while explicitly excluding `updates::GetChannelDifference::CONSTRUCTOR_ID` via `rpc.caused_by`).
- Implement `mibot_bot::Retrier` as an **async wrapper around `SenderPoolHandle::invoke_in_dc`** (`mibot_bot::Client::invoke`):
  - Compute `random_id` **once** before entering the retry loop (and serialize/redact via `IPRedactor` **once** outside `Retrier`, matching `app.go:230-239`), so retries always transmit the exact same `random_id` and redacted text.
  - Maintain `blocked: tokio::sync::Mutex<HashMap<u32, Cooldown>>` keyed by `R::CONSTRUCTOR_ID`. **Never hold the mutex across `.await` / `tokio::time::sleep`** (`RULES.md` #7): lock -> read/prune `until` -> drop guard -> `sleep` -> re-check loop.
  - Handle `code == 420` (`FLOOD_WAIT` / `FLOOD_PREMIUM_WAIT` / `SLOWMODE_WAIT`) with `wait = seconds + 1s` (`MaxWait = 60s`, `Attempts = 5`).
  - Handle `serverFailure`: `code >= 500` or `name == "RPC_CALL_FAIL"` or `name == "RPC_MCGET_FAIL"` with `2s << min(server_errors, 3)` + <=25% jitter (`Attempts = 5`).
  - Preserve the `channels.editBanned` 30-second `until_date` guard (`retrier.go:119-123`) and `sendFailure` -> `delivery::Unknown` classification (`client.go:268-273`: `Rpc(r) if r.code < 500` is definitive; `code >= 500`, `Io`, `Deserialize`, `Transport`, `Dropped` are `delivery::Unknown`).

---

## 6. Multi-DC Media Download, Upload & UTF-16 Entity Offsets

### 6.1 Multi-DC Media Download (`files.rs` vs `internal/bot/download.go`)
- `Client::iter_download` and `Client::download_media` (`grammers-client-0.12.0/src/client/files.rs`):
  - Automatically handle `FILE_MIGRATE_X` (`code == 303`) and `AUTH_KEY_UNREGISTERED` by calling `copy_auth_to_dc(dc)` (`net.rs:116-145`: `auth.exportAuthorization` + `auth.importAuthorization`, serialized by `auth_copied_to_dcs` mutex).
  - For files `> 10 MiB` (`BIG_FILE_SIZE`), `download_media` uses 4 concurrent tasks (`WORKER_COUNT = 4`, `MAX_CHUNK_SIZE = 512 KiB`) with `upload.getFile`.
- **Gaps to address in `mibot_bot::download`**:
  1. **No byte `limit` in `download_media` / `iter_download`**: Wrap `iter_download` (or direct `upload::GetFile` through `mibot_bot::Client`) with a `limitedWriter`-style byte counter returning `ErrTooLarge` (`download.go:79-89`).
  2. **Home-DC-first vs `downloadable.dc_id()`-first & missing connect timeout**: `grammers` tries `downloadable.dc_id()` first (`files.rs:101-103`), and `SenderPoolRunner::connect_sender` has **no TCP/handshake timeout**. Wrap cross-DC downloads in `tokio::time::timeout(Duration::from_secs(20), ...)` for initial connection/first chunk (or invoke `upload::GetFile` on `home_dc_id` first and migrate on `303 FILE_MIGRATE` like `download.go:97-118`). Note `Peer::photo()` (`ChatPhoto` in `media.rs:925`) already returns `dc_id() -> None`, so profile photos naturally try `home_dc_id` first.
  3. **Panics in `files.rs`**: `download_media_concurrent` uses `err.value.unwrap()` on `303` and `tx.send(...).unwrap()`, and `DownloadIter` panics on `CdnRedirect`. Calling `upload::GetFile` via `mibot_bot::Client` for `DownloadTo` (or validating `err.value` on `303`) avoids `unwrap()` panics (`RULES.md` #3).

### 6.2 File Upload (`files.rs:390-512` vs `client.go:511-570`)
- `Client::upload_stream(&mut reader, size, name)` handles `upload.saveFilePart` (`<= 10 MiB`, sequential + MD5 -> `InputFile`) and `upload.saveBigFilePart` (`> 10 MiB`, 4 concurrent futures -> `InputFileBig`).
- **Trap**: `Client::upload_stream` bypasses `mibot_bot::Retrier` and wraps `InvocationError` in `io::Error::other`, losing typed `RpcError` matching. Porting the small/big part upload loop (~70 lines) onto `mibot_bot::Client::invoke` gives full `Retrier` + `IPRedactor` (`DocumentAttributeFilename`) protection.

### 6.3 UTF-16 Message Entity Offsets (`parsers/html.rs` vs `client.go` & `ipprivacy.go`)
- Telegram measures `MessageEntity.offset` and `MessageEntity.length` in **UTF-16 code units** (`RULES.md` #2).
- `grammers-client` (`parsers/common.rs:21-24`) provides `telegram_string_len(s: &str) -> i32` via `s.encode_utf16().count() as i32`, and `feature = "html"` enables `parse_html_message` / `generate_html_message` (`html5ever 0.40.1` + `jiff 0.2.40`).
- **Do NOT use `grammers-client`'s `html` feature directly** for `mibot-lite`:
  1. **Panic bug**: `html.rs:122` executes `url[MENTION_URL_PREFIX.len()..].parse::<i64>().unwrap()`, which panics on malformed `<a href="tg://user?id=...">` links (`RULES.md` #3).
  2. **Unsafe mention fallback**: `messages.rs:104-121` converts `MentionName` using `access_hash = 0` when a user is not in `Session`, whereas `mibot-lite` (`client.go:167-180`) strips the mention entity and leaves plain text if the user is missing from `PeerCache`.
  3. **Tag differences**: `grammers`' HTML parser uses `<details>` for spoilers rather than `<tg-spoiler>`, and brings in `html5ever` + `jiff` solely for HTML parsing.
- **Recommendation**: Write a small, panic-free `mibot_bot::html` parser/renderer + `utf16_len` helper (`s.encode_utf16().count()`) matching `gotd`'s `html.HTML` and `ipprivacy`'s `redactMessage` shift math (`ipprivacy.go:226-279`).

---

## 7. Semantic Traps, Unimplemented Pieces & Compatibility Risks

| Risk / Trap | Evidence in `grammers 0.12.0` | Mitigation in `mibot-lite` |
| --- | --- | --- |
| **Pure Rust / `rustls` / no OpenSSL / no C deps (`ADR-006`)** | `grammers-crypto`, `grammers-mtproto`, `grammers-mtsender`, `grammers-client`, `grammers-tl-types` have **zero** OpenSSL, `native-tls`, `ring`, `aws-lc`, or C dependencies (MTProto encrypts over raw TCP in pure Rust). **Only `grammers-session` default feature `sqlite-storage` pulls `libsql` (C `sqlite3`).** | Use `grammers-session = { version = "=0.12.0", default-features = false, features = ["serde"] }`. Use `reqwest` with `rustls` for HTTP (`httpx`, `botapi`). |
| **MSRV / Rust Edition 2024** | All 6 `grammers-*` `0.12.0` crates specify `edition = "2024"`. | Require Rust toolchain `>= 1.85.0` in CI / cross-compile setup. |
| **TL Layer breaking changes** | `grammers-client/src/lib.rs:78-80` warns raw `tl::*` breaks on minor releases and crate versions must match strictly. | Pin `=0.12.0` across all `grammers-*` dependencies in workspace `Cargo.toml`. |
| **`edit_message` drops `rich_message`** | `Client::edit_message` (`messages.rs:866`) hardcodes `rich_message: None` (`send_message` supports `InputMessage::rich_message`). | Implement `EditRich` via raw `client.invoke(&tl::functions::messages::EditMessage { ... })`. |
| **SOCKS5 proxy limitations** | `NetStream::connect_proxy_stream` (`tcp.rs:43-108`) only supports `socks5://` and constructs a fresh `hickory_resolver::Resolver::builder_tokio().unwrap().build().unwrap()` per domain connection. | Matches `mibot-lite`'s SOCKS5 config (`app.go:262`), though pre-resolving domain proxy hosts (or guarding resolver init) avoids `.unwrap()` panics on misconfigured `/etc/resolv.conf`. |
| **No `connect_timeout` in `SenderPool`** | `SenderPoolRunner::connect_sender` (`sender_pool.rs:411-470`) has no timeout on `TcpStream::connect` or DH keygen. | Wrap cross-DC downloads and health checks in `tokio::time::timeout(20s, ...)`. |
| **CDN DC options not filtered in `update_config`** | `sender_pool.rs:480` filters `!option.media_only && !option.tcpo_only && option.r#static`, omitting `!option.cdn` (compare `gramjs.go:262`). | Low risk because `cdn_supported: false` is used in `GetFile`, but filter `!option.cdn` when exporting back to `StringSession`. |
| **Panics / `unimplemented!()` in `grammers-client`** | `auth.rs:271,284` (`SentCode::PaymentRequired => unimplemented!()`), `auth.rs:435` (invalid DH prime), `chats.rs:422` (`get_me`), `html.rs:122` (`tg://user?id=`), `files.rs:319` (`303` unwrap). | Avoid `grammers-client`'s `html` module; validate inputs and prefer raw `invoke` wrappers where `RULES.md` #3 forbids panics. |
| **Compile time of `grammers-tl-types`** | `build.rs` generates all Layer 233 structs/enums from `tl/api.tl`. | Keep `grammers-tl-types` in a single workspace dependency (feature-unified) so it builds once per target. |

---

## 8. Primary Sources Cited
- **Crates.io & Codeberg**:
  - `https://crates.io/api/v1/crates/grammers-client` (`0.12.0`, 2026-11-08)
  - `https://crates.io/api/v1/crates/grammers-mtsender` (`0.12.0`, 2026-11-08)
  - `https://crates.io/api/v1/crates/grammers-session` (`0.12.0`, 2026-11-08)
  - `https://crates.io/api/v1/crates/grammers-tl-types` (`0.12.0`, 2026-11-08)
  - `https://crates.io/api/v1/crates/grammers-mtproto` (`0.12.0`, 2026-11-08)
  - `https://crates.io/api/v1/crates/grammers-crypto` (`0.12.0`, 2026-11-08)
  - `https://crates.io/api/v1/crates/ferogram` (`0.6.7`, 2026-11-10)
  - Upstream repository: `https://codeberg.org/Lonami/grammers`
- **`grammers 0.12.0` Inspected Source Modules**:
  - `grammers-tl-types-0.12.0`: `tl/api.tl`, `build.rs`, `src/lib.rs`
  - `grammers-session-0.12.0`: `Cargo.toml`, `src/{lib.rs,session.rs,session_data.rs,types.rs,dc_options.rs,peer.rs,updates.rs}`, `src/storages/{memory.rs,sqlite.rs}`, `src/message_box/{mod.rs,defs.rs,adaptor.rs}`
  - `grammers-mtsender-0.12.0`: `Cargo.toml`, `src/{errors.rs,configuration.rs,sender_pool.rs,sender.rs,updates_receiver.rs}`, `src/net/{mod.rs,tcp.rs}`
  - `grammers-mtproto-0.12.0`: `Cargo.toml`, `src/{authentication.rs,mtp/encrypted.rs}`
  - `grammers-crypto-0.12.0`: `Cargo.toml`
  - `grammers-client-0.12.0`: `Cargo.toml`, `src/lib.rs`, `src/client/{client.rs,net.rs,updates.rs,retry_policy.rs,files.rs,messages.rs,chats.rs,dialogs.rs,auth.rs}`, `src/message/{input_message.rs,input_rich_message.rs,message.rs}`, `src/media/{media.rs,downloadable.rs}`, `src/peer/{mod.rs,peer_map.rs,user.rs,channel.rs}`, `src/parsers/{common.rs,html.rs}`
- **`mibot-lite` Reference Files**:
  - [go.mod](file:///home/ezhang/mibot-lite/go.mod)
  - [client.go](file:///home/ezhang/mibot-lite/internal/bot/client.go), [retrier.go](file:///home/ezhang/mibot-lite/internal/bot/retrier.go), [peers.go](file:///home/ezhang/mibot-lite/internal/bot/peers.go), [download.go](file:///home/ezhang/mibot-lite/internal/bot/download.go), [sent.go](file:///home/ezhang/mibot-lite/internal/bot/sent.go), [entities.go](file:///home/ezhang/mibot-lite/internal/bot/entities.go), [ipprivacy.go](file:///home/ezhang/mibot-lite/internal/bot/ipprivacy.go), [rich.go](file:///home/ezhang/mibot-lite/internal/bot/rich.go), [rich_privacy.go](file:///home/ezhang/mibot-lite/internal/bot/rich_privacy.go), [dialogs.go](file:///home/ezhang/mibot-lite/internal/bot/dialogs.go), [message.go](file:///home/ezhang/mibot-lite/internal/bot/message.go)
  - [app.go](file:///home/ezhang/mibot-lite/internal/app/app.go), [live.go](file:///home/ezhang/mibot-lite/internal/app/live.go), [subscribe.go](file:///home/ezhang/mibot-lite/internal/app/subscribe.go)
  - [gramjs.go](file:///home/ezhang/mibot-lite/internal/session/gramjs.go), [state.go](file:///home/ezhang/mibot-lite/internal/tgstate/state.go), [cursor.go](file:///home/ezhang/mibot-lite/internal/tgstate/cursor.go), [login.go](file:///home/ezhang/mibot-lite/internal/login/login.go)
