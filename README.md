# MiBot Lite

[![CI](https://github.com/Yapixel/mibot-lite/actions/workflows/ci.yml/badge.svg?branch=rust)](https://github.com/Yapixel/mibot-lite/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Yapixel/mibot-lite?label=release)](https://github.com/Yapixel/mibot-lite/releases)
[![Rust 2024](https://img.shields.io/badge/Rust-2024%20Pure%20Rust-orange?logo=rust)](rust/Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-linux--amd64%20%7C%20linux--arm64-blue)](.github/workflows/release.yml)
[![License: LGPL-2.1](https://img.shields.io/badge/License-LGPL--2.1-green)](LICENSE)

**English** | [日本語](README.ja.md)

A memory-light, pure Rust Telegram userbot rewritten from upstream Go `mibot-lite`: compiled into a single static native binary (no GC, no C system library dependencies), with commands compiled in and state kept in JSON files.

Drop-in compatible with both the Go edition and [MiBox](https://github.com/MiCat-S/Mi-Box): accounts, `config.json`, session files, and command names stay identical for seamless migration, with no JavaScript runtime, no SQLite, and no plugin sandbox.

| | MiBot Lite (Go / `main`) | MiBot Lite (Rust / `rust`) |
|---|---|---|
| Language & runtime | Go 1.26 (with GC runtime) | **Pure Rust 2024 (Tokio async, no GC, zero C system deps)** |
| Binary size | ~21 MB | **~34 MB stripped on Linux `musl` (static single binary)** |
| MTProto stack | `gotd/td` (Layer 229) | **Pure Rust `grammers =0.12.0` + `rustls` (`ring`)** |
| Concurrency & dispatch | Goroutines + 3-pool channels | **Tokio + 3-pool bounded semaphore dispatcher (panic-isolated & drainable)** |
| Imaging & status card | `image/draw` + `golang.org/x/image` | **`image` + `fast_image_resize` + `tiny-skia` + `ab_glyph`** |
| Session & state files | `gotd-session.json` + `updates.json` | **Drop-in compatible + GramJS / Telethon adapters + `.cursor` persistence** |
| Command & safety parity | 45 command groups, `.sudo` allowlist, IP masking | **Full 45-group parity + fail-closed guards + UTF-16 entity alignment** |

The `Cargo.lock` dependency audit verifies zero banned C system libraries (`openssl-sys`, `libsqlite3-sys`, etc.);
the brief `ffmpeg` child process spawned while making video stickers does not stay resident (see [Design notes](#design-notes)).

## Install

On the server (Linux amd64 / arm64, systemd, root):

```sh
bash <(curl -fsSL https://raw.githubusercontent.com/Yapixel/mibot-lite/rust/scripts/install.sh)
```

It downloads, verifies, logs you in and installs the service in one go. Running it again upgrades; after that you can
also send `.update run` in Telegram. The step-by-step procedure, settings and memory limits are in
[INSTALL.md](INSTALL.md) (Chinese).

- **ffmpeg**: needed by `.eatgif`, `.eat`, `.eat2` and `.t`, and by `.yvlu` when it quotes a video. Without it the
  other commands work as usual. `apt install -y ffmpeg`.
- **Moving from MiBox**: settings for ai, sum, whois, aliases, prefixes and more can be carried over directly:

  ```sh
  ./mibot-lite --import-mibox /root/mibot --root /root/mibot-lite
  ```

  The same account can switch back and forth between the two programs, but **they must not run at the same time**:
  Telegram makes two connections of one account knock each other off.
- **Reinstalling or moving to a new machine**: send `.bf` in Telegram to back up the configuration to Saved Messages.
  On the new machine, pass `--restore <backup file>` to the installer to restore everything as it was, without logging
  in again. See [INSTALL.md section 10](INSTALL.md#10-备份与恢复重装系统换机器).

## Commands

The default prefixes are `.`, `。` and `$`; change them with `.prefix`. For a command's full usage send
`.help <command>`, or add `--help` after the command.

The "Lend" column says whether the command can be lent to other people through
[`.sudo` / `.sure`](#lending-the-account-sudo-and-sure): ✓ means the whole command can be lent; **partly** means
only the parts for looking things up and using it can be lent, while subcommands that change settings stay with the
owner; blank means owner only.

**Running and maintenance**

| Command | What it does | Lend |
|---|---|---|
| `.ping [domain\|IP]` | Latency to Telegram or a website | ✓ |
| `.status` | Status card (CPU, memory, disk, swap) | ✓ |
| `.memory` | Process memory | ✓ |
| `.sysinfo` | Detailed system information | |
| `.service [name]` | State, uptime, memory and CPU of a systemd service | |
| `.version` `.ver` | Version information | ✓ |
| `.help` `.h` | Command list, or the help for one command | ✓ |
| `.update [check\|run\|rollback\|auto]` | Check for, install or roll back an update; updates itself daily by default | |
| `.restart` | Restart the service | |
| `.log` | Export the run log (sensitive values removed) | |
| `.bf` | Back up the configuration to Saved Messages | |
| `.prefix` `.alias` | Change the command prefixes, give commands aliases | |
| `.privacy` | How IP addresses in the output are masked | |

**Lookups and tools**

| Command | What it does | Lend |
|---|---|---|
| `.calc <expression>` | Arithmetic | ✓ |
| `.rate <currency> [target] [amount]` | Exchange rates and conversion | ✓ |
| `.tr [language] <text>` | Google Translate, no setup needed | partly |
| `.gt [en] <text>` | AI translation | ✓ |
| `.whois <domain>` | Domain registration data, in batches too | partly |
| `.ip [IP\|domain]` | Location and network of an IP | ✓ |
| `.dig <domain> [type] [@server]` | DNS records, with IP location and ASN | ✓ |
| `.bgp [dns] [IP\|prefix\|AS]` | BGP route of an IP, prefix or AS: origin, upstreams, reverse DNS | ✓ |
| `.ddg <keywords>` `.duckduckgo` | Web search (DuckDuckGo, falling back to Firecrawl when blocked) | ✓ |
| `.b64encode` `.b64decode` `.urlencode` `.urldecode` `.encode` | Base64 and URL encoding and decoding | ✓ |
| `.subinfo [txt] <link>` `.cha` | Nodes, protocols, regions, traffic and expiry of a proxy subscription; the latter is the short form | |
| `.bin <first 6–8 card digits>` | Issuing bank of a card BIN | ✓ |
| `.ids` `.dc` | Profile of a user or chat, and its data center | ✓ |
| `.speedtest` `.st` | Server speed test (official Ookla CLI) | partly |

**AI**

| Command | What it does | Lend |
|---|---|---|
| `.ai [search] <question>` | AI chat and web search | partly |
| `.sum [count]` | Group chat summaries, also on a schedule | partly |
| `.checkapi [check\|speed\|compare] <tag>` | Test the APIs configured for the AI command: models, test prompt, speed, comparison | |

Configure OpenAI or a compatible gateway in Telegram Saved Messages. Replace the URL, key and model ID with your provider’s values and send each line separately.

```text
.ai config add main https://api.openai.com/v1 YOUR_API_KEY openai-compatible
.checkapi models main
.ai model chat main MODEL_ID
.ai Hello
```

New configurations use Chat Completions by default. Send `.ai config set main responses on` to use Responses. Adding a full `/responses` URL enables it automatically. Gateway URLs can include path prefixes and query parameters. Full Anthropic `/messages` and Gemini `:generateContent` endpoints are recognized automatically. For a gateway base URL, you can set the type to `anthropic` or `gemini`. With `.ai config set main stream on`, the bot receives a stream and posts the completed answer to Telegram. Web search requires support from the selected model and gateway.

**Messages and stickers**

| Command | What it does | Lend |
|---|---|---|
| `.yvlu` | Turn messages into a quote sticker, image or story | partly |
| `.eatgif <name>` | Animated sticker made from both people's avatars | partly |
| `.eat` `.eat2` | Meme sticker made from an avatar or a picture | partly |
| `.sticker` | Save a sticker into your own sticker pack | |
| `.t <text>` `.ts` `.tk` | Text to speech (`.ts` picks a voice, `.tk` sets the API key in Saved Messages) | |
| `.re [messages] [times]` | Repeat the replied-to message | ✓ |
| `.so <keyword>` `.search` | Search videos in your channel sources and send one here | |
| `.aff [number]` | Save affiliate texts and send them with one command | |
| `.save <link>` | Save or forward messages, even where forwarding is restricted | |
| `.dme <count>` | Delete your own messages, or set trigger words that do it when someone else sends them | |
| `.da` | Delete group messages in bulk | |
| `.acron <type> <cron> <chat>` | Scheduled (6-field cron with seconds) sending, copying, forwarding, deleting or pinning of messages, or running a command | |
| `.trace [emoji…]` | Auto-react to messages from chosen users or containing keywords | |

**Group administration**

| Command | What it does | Lend |
|---|---|---|
| `.ban` `.unban` `.kick` `.mute` `.unmute` | Ban, kick or mute in the current group | ✓ |
| `.sb` `.unsb` | Ban or unban in every group you administer | |
| `.refresh` | Refresh the list of administered groups (cached for a day, rescanned on next use after that) | |
| `.aban` | Help for the commands above | |
| `.keyword` | Keyword auto-replies, optionally deleting, banning or muting | |
| `.clean_member <mode>` | Find or remove group members by last seen, messages or deleted status | |
| `.guestban [on\|off]` | Delete ads posted by guest bots and ban the bot in this group | |

**Account**

| Command | What it does | Lend |
|---|---|---|
| `.acn` `.autochangename` | Change your display name automatically by time and weather | |
| `.checkin` | Daily scheduled check-ins with bots, optionally pressing a callback button, with a summary | |
| `.sudo` `.sure` | Lend commands to people on a list, see below | |

### Lending the account: .sudo and .sure

Both let people on a list run commands through your account: they post a message in a group, and the account sends
the command as you, in the same chat, replying to the same target. With `.sudo` they send commands directly; `.sure`
is narrower: their message has to match a rule you set, and it can be rewritten, for example turning a member's
`/sb` into `.ban`.

Unlike MiBox, **what can be lent is an allowlist**, the "Lend" column in the tables above. MiBox lets people on the
list run any command, `.sudo add` included, so anyone you authorize can authorize others. Here the check is made on
the real command after aliases are expanded, so an alias cannot get around it, and commands added later cannot be
lent by default. Among the "partly" commands, `.sum` and `.speedtest` allow only the listed uses, so subcommands
added to them later stay with the owner too; the others keep only their settings subcommands for the owner, and any
other subcommand (including new ones) can be lent. When someone on the list sends a command that cannot be lent,
the account only replies that they lack permission.

The lists are kept in `data/sudo.json` and `data/sure.json`, and `.bf` backs them up too.

### IP masking: .privacy

As in MiBox v2, IP addresses are masked in every message the account sends or edits: by default the last 2 parts of
an IPv4 address and the last 4 groups of an IPv6 address. Links pointing at an IP are removed, and IPs in file names
are masked too. `.privacy ip mask 2 4` changes how many parts are masked; `.privacy ip hide` replaces the whole
address with 「[IP已隐藏]」 ("IP hidden").

Masking happens in the connection layer, so it covers the output of every command. Commands sent on someone else's
behalf (`.sudo`, `.sure`) are not masked: those addresses were typed by that person in the first place.

## Design notes

### Why no plugin system

The MiBox host loads arbitrary TypeScript plugins, so it has to keep a JS engine, SQLite and a whole plugin lifecycle
resident, and that is where most of its memory goes. MiBot Lite drops that premise: commands are no longer artifacts
you can install at runtime but Rust modules compiled into the binary, scheduled on Tokio with a bounded 3-pool
dispatcher. The cost is a rebuild to add a command; in return you get a self-contained static binary with zero runtime
overhead and no sandbox required.

### Images and video

`.yvlu` and `.eatgif` composite images. MiBox uses sharp, which binds libvips: a shared library, an image cache and a
thread pool, all resident in the process for a feature used a few times a day. Here they are replaced by pure Rust
`image` + `fast_image_resize` (CatmullRom / Bicubic) + `tiny-skia` + `ab_glyph`, with no libvips or system font
libraries, keeping idle memory clean.

The only external dependency is ffmpeg: Telegram video stickers must be VP9, and software encoding inside the bot
process is avoided by delegating to the `ffmpeg` subprocess. ffmpeg uses no memory while idle, but **needs over
100 MB while it runs**. That is why the systemd unit sets `MemoryMax` to 512M instead of just above the resident size,
and **does not set** `MemoryHigh`. Going over `MemoryHigh` does not fail; it forces reclaim again and again and slows
the process to a crawl. In one encode it fired more than 14,000 times, the encode hit its own timeout before finishing,
and nothing was left to say why.

Two differences from MiBox:

- **eatgif no longer encodes a GIF first.** The original encoded the frames as a GIF and then converted that to VP9;
  the middle step cuts every frame down to 256 colors. Here the PNG frames go straight to ffmpeg, skipping one lossy
  hop.
- **yvlu does not convert tgs animated stickers.** That path needs Python's `rlottie-python`, which was never
  installed on the production machine, so porting it would mean porting a feature that does not work today. The rest
  was matched against the original item by item: multiple messages come from the real history rather than
  consecutive IDs, forwards are credited to the original author, partial quotes, admin titles, emoji status and the
  large-avatar fallback.

## Development & Test Results

```sh
cd rust
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
bash ../scripts/build.sh  # build with MIBOT_VERSION baked in
```

Self-check against a real account (sends a set of read-only commands to Saved Messages, checks the replies, then
deletes them). One account cannot be connected twice at once, so stop the service first:

```sh
./mibot-lite --verify --root /deployment/dir
```

### Verification summary (v0.4.0)

- **Workspace**: **22 crates** under `rust/` covering all 45 command groups and runtime subsystems.
- **Lint**: `cargo clippy --locked --all-targets -- -D warnings` — **0 warnings**.
- **Unit & parity tests**: `cargo test --locked` — **245 / 245 passed** (`mibot-commands`: 144, `mibot-bot`: 17, `mibot-imaging`: 9, `mibot-tgstate`: 8, `mibot-command`: 8, `mibot-app`: 7, `mibot-backup`: 5, `mibot-config`: 5, `mibot-logtail`: 5, `mibot-session`: 5, `mibot-botapi`: 5, `mibot-httpx`: 4, `mibot-mibox`: 4, `mibot-store`: 4, `mibot-media`: 3, `mibot-statuscard`: 3, `mibot-lite`: 2, `mibot-login`: 2, `mibot-verify`: 2, `mibot-delivery`: 1, `mibot-fsutil`: 1, `mibot-sysinfo`: 1).
- **Pure-Rust dependency audit**: 0 occurrences of `openssl-sys`, `libsqlite3-sys`, `rusqlite`, `sqlite`, `libsql`, `aws-lc-sys`, `aws-lc-fips-sys`, `libyaml`, `webp-sys`, or `libwebp-sys` in `rust/Cargo.lock`.
- **CLI & installer checks**: `python3 -m unittest scripts/install_test.py` (5/5 OK); offline CLI smoke checks (`--version`, no-args exit `2`, missing-root `--check` exit `1`) pass; GitHub Actions CI Run `45984816612` green.
- **Scope note**: live Telegram DC modes (`--serve`, `--verify`, interactive `--login`) require real account credentials and network access, and are verified in CI via offline mock harnesses.

### Code layout

| Path | Contents |
|---|---|
| `rust/crates/mibot-lite` | CLI entry point (`--serve`, `--check`, `--verify`, `--login`, `--backup`, `--restore`, `--import-mibox`) |
| `rust/crates/app` | Lifecycle management, `flock` single-instance root lock, and update dispatch |
| `rust/crates/bot` | `grammers` MTProto wrapper, flood-wait retrier, `SentRecorder`, IP masking, multi-DC downloads |
| `rust/crates/session` / `tgstate` | Session adapters (GramJS / Telethon / `gotd-session.json`) and debounced state persistence |
| `rust/crates/command` | Command registry, bounded 3-pool dispatcher, and UTF-16 pagination |
| `rust/crates/commands/src/` | All built-in commands and shared `kit` helpers |
| `rust/crates/imaging` / `statuscard` | Pure Rust image compositing and status card rasterization |

Modules holding more than one command: `aban.rs` holds all the group administration commands, `ids.rs` includes `.dc`,
`sudo.rs` includes `.sure`, `eatgif.rs` includes `.eat` and `.eat2`, `tts.rs` is `.t`, `.ts` and `.tk`, `encode.rs` holds
the four Base64 and URL commands, `subinfo.rs` includes `.cha`, and basic commands such as `.status` are in `core.rs`.
Short forms (`.st`, `.h` and so on) are aliases registered alongside the full name in the same module.

The text commands send back to the chat (titles, failure messages, usage, wording, punctuation) follows
[STYLE.md](STYLE.md) (Chinese); `test_command_text_style` and `test_readme_matches_registry` enforce style rules and
English/Japanese README table parity.

To add a command: create `rust/crates/commands/src/xxx.rs` with `pub fn register(a: &Arc<App>)`, export and hook it into
`register_all` in `rust/crates/commands/src/lib.rs`. For it to be lendable, add it to the allowlist in
`rust/crates/commands/src/sudo.rs` and update both `README.md` and `README.ja.md` command tables.

All repository unit tests run offline using pure logic and local mock harnesses; live Telegram verification is done
with `./mibot-lite --verify`.

## License

LGPL-2.1, the same as MiBox.

The status card font is a subset of Noto Sans SC (`rust/crates/statuscard/src/NotoSansSC-status-subset.ttf`, embedded at compile
time), distributed under the SIL Open Font License 1.1; the license text is `rust/crates/statuscard/src/NotoSansSC-OFL.txt`.
