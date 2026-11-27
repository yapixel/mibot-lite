# MiBot Lite Domain Glossary (Go -> Rust)

| Original (Go) | Meaning | Rust Target |
| --- | --- | --- |
| `config.Config` / `config.json` | Root deployment config containing Telegram `api_id`, `api_hash`, and GramJS `session` string | `mibot_config::Config` |
| `session.StringSession` | Base64 versioned (`'1'`) binary payload holding DC ID, IP/port, and 256-byte MTProto auth key (Teleproto / Telethon / IPv6 layouts) | `mibot_session::StringSession` |
| `tgstate.State` / `PeerCache` | Persisted MTProto updates cursor (`pts`, `qts`, `seq`, `date`, channel `pts`) + user/channel `access_hash` cache (never guesses min-entity hashes) | `mibot_tgstate::TgState` / `PeerCache` |
| `bot.Client` | High-level MTProto account wrapper with `Retrier`, `SentRecorder`, `PeerCache`, multi-DC download pool, and IP privacy masking | `mibot_bot::Client` |
| `bot.Retrier` | MTProto RPC retry middleware sharing per-method `FLOOD_WAIT` cooldowns without holding locks during sleep; preserves `RandomID` | `mibot_bot::Retrier` |
| `bot.SentRecorder` | Tracks outgoing message IDs sent by the bot so updates gap-filler does not re-trigger own messages as commands | `mibot_bot::SentRecorder` |
| `bot.ipprivacy` | Connection-layer middleware masking IPv4/IPv6 addresses in outgoing text, links, and filenames (`.privacy`), bypassed for `.sudo`/`.sure` | `mibot_bot::privacy` |
| `botapi.Client` | Bounded HTTP Bot API client (`.checkin` summary push) with SHA-256 token buckets, 429 `retry_after`, no-redirect, and no 5xx/network replay | `mibot_botapi::Client` |
| `store.Store` | Atomic JSON state store under `data/` using deep-copied read/update transactions (`WriteFileAtomic`) | `mibot_store::Store` |
| `command.Registry` | Command dispatcher + three bounded worker pools (16 command workers / 64 queue; 4 short-bg / 16 queue; 4 long-task) | `mibot_command::Registry` |
| `app.liveHandler` | Pre-gap-fill message dispatcher so instant commands run immediately without waiting for slow channel `pts` gap recovery | `mibot_app::LiveHandler` |
| `delivery` | Message output routing (edit/reply/split into <=4096 UTF-16 code-unit pages) | `mibot_delivery` |
| `mibox.sqlite` | Pure-byte SQLite v3 table B-Tree reader used by `--import-mibox` without linking `libsqlite3` | `mibot_mibox::sqlite` |
| `statuscard` | 1600x900 PNG system card renderer using embedded 73 KB Noto Sans SC subset font | `mibot_statuscard` |
| `sudo` / `sure` | Allowlisted command delegation (`sudo` direct, `sure` pattern+rewrite) with alias-expanded permission checks | `mibot_cmd_sudo` |
