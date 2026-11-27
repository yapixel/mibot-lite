# MiBot Lite Rust Port Map

Status legend: `open` | `claimed (<session> <date>)` | `done` | `blocked`

- **Goal**: Pure Rust standalone `mibot-lite` binary under `rust/` matching Go behavior (`ADR-001`..`ADR-007`).
- **ADRs**: [DECISIONS.md](DECISIONS.md) | **Glossary**: [GLOSSARY.md](GLOSSARY.md) | **Rules**: [RULES.md](RULES.md)

## Research Topics (`rust/research/`)
| Topic | State | Output |
| --- | --- | --- |
| Telegram MTProto (`grammers` vs `gotd/td`) | `done` | [mtproto-grammers.md](research/mtproto-grammers.md) |
| Imaging & font rasterization (`golang.org/x/image`) | `done` | [imaging-fonts.md](research/imaging-fonts.md) |
| HTTP, Proxy, HTML, Cron & YAML crates | `done` | [net-cron-yaml.md](research/net-cron-yaml.md) |

## Wave 0 — Leaf Offline Foundation (7 units)
| Unit | Lines | State | Notes |
| --- | ---: | --- | --- |
| `internal/delivery/` | 15 | `done` | `crates/delivery` (1 test) |
| `internal/fsutil/` | 38 | `done` | `crates/fsutil` (1 test) |
| `internal/httpx/` | 235 | `done` | `crates/httpx` (4 tests; explicit redirect & SSRF resolver) |
| `internal/logtail/` | 176 | `done` | `crates/logtail` (5 tests) |
| `internal/mibox/` | 234 | `done` | `crates/mibox` (4 tests incl. real `alias.db`) |
| `internal/sysinfo/` | 323 | `done` | `crates/sysinfo` (1 test) |
| `internal/imaging/` | 179 | `done` | `crates/imaging` (9 tests; premul + CatmullRom + Bicubic) |

## Wave 1 — Core State, Config, Cards & MTProto Boundary (7 units)
| Unit | Lines | State | Notes |
| --- | ---: | --- | --- |
| `internal/config/` | 191 | `done` | `crates/config` (5 tests) |
| `internal/store/` | 92 | `done` | `crates/store` (4 tests) |
| `internal/botapi/` | 207 | `done` | `crates/botapi` (5 tests; 32 slots + token buckets + uncertain) |
| `internal/statuscard/` | 338 | `done` | `crates/statuscard` (3 tests; embedded subset font + `tiny-skia`) |
| `internal/session/` | 234 | `done` | `crates/session` (5 tests; GramJS/Telethon `StringSession`, `gotd-session.json` & `MibotSession`) |
| `internal/tgstate/` | 390 | `done` | `crates/tgstate` (8 tests; `updates.json` + `.cursor` 1s debounced state & peer access-hash persistence) |
| `internal/bot/` | 2276 | `done` | `crates/bot` (17 tests; `NoRetries` + `Retrier`, `PeerCache`, `SentRecorder`, `ipprivacy`, UTF-16 `html`, multi-DC download, `InputRichMessage`) |

## Wave 2 — Runtime Subsystems (4 units)
| Unit | Lines | State | Notes |
| --- | ---: | --- | --- |
| `internal/backup/` | 306 | `done` | `crates/backup` (5 tests) |
| `internal/media/` | 423 | `done` | `crates/media` (3 tests; EBML WebM parser + ffmpeg wrapper) |
| `internal/verify/` | 193 | `done` | `crates/verify` (2 tests; 35 `--verify` cases, `observe_sent` & reply cleanup) |
| `internal/command/` | 998 | `done` | `crates/command` (8 tests; `Registry`, bounded 3-pool + long scheduler, `WorkGuard`, UTF-16 `html_pages`/`escaped_pages`, redacted `brief`) |

## Wave 3 & 4 — App Wiring, Login & Command Kit (3 units)
| Unit | Lines | State | Notes |
| --- | ---: | --- | --- |
| `internal/app/` | 585 | `done` | `crates/app` (7 tests; `flock` root lock, `LiveHandler`, `subscribe_guard`, `SeenMessages`, `checkpoint_state`, `App::handle`/`run`) |
| `internal/login/` | 243 | `done` | `crates/login` (2 tests; `precheck` lock/`--force`/proxy validation, interactive login, atomic `write_account`) |
| `internal/commands/kit/` | 188 | `done` | `crates/commands/src/kit.rs` (2 tests; shared command helpers & UTF-16 utilities) |

## Wave 5 — Commands Batch 1 (39 units — `crates/commands`)
- Utility & Config (24): `privacy`, `bf`, `prefix`, `log`, `ip`, `alias`, `bin`, `encode`, `tr`, `calc`, `service`, `ddg`, `core`, `restart`, `whois`, `sticker`, `tts`, `dig`, `ids`, `aff`, `rate`, `bgp`, `subinfo`, `speedtest` — all `done`
- Group Admin & Automation (13): `sudo`, `guestban`, `trace`, `da`, `aban`, `acn`, `acron`, `checkin`, `cleanmember`, `dme`, `keyword`, `save`, `search` — all `done`
- Media & AI (2): `eatgif`, `ai` (`mod.rs`, `markdown.rs`, `check.rs`) — all `done`

## Wave 6–8 — Dependent Commands & Entrypoint (8 units — `crates/commands` & `crates/mibot-lite`)
- Wave 6 (6 units): `gt`, `re`, `checkapi`, `update`, `sum`, `yvlu` — all `done`
- Wave 7 (1 unit): `internal/commands/` (`register_all`, `import_mibox`, style/README/delegable/read-only parity tests in `crates/commands/src/lib.rs`) — `done` (144 tests in `mibot-commands`)
- Wave 8 (1 unit): `cmd/mibot-lite/` (`crates/mibot-lite` CLI entrypoint binary & integration unit tests) — `done` (2 tests)
