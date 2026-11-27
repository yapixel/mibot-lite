# Architecture Decision Records — MiBot Lite Rust Port

## ADR-001: Pure Rust standalone rewrite (no long-lived cgo bridge)
Status: accepted (2026-11-27)
Context: `mibot-lite` is a 36.7k-line Go CLI/userbot daemon across 68 units whose core bottleneck is network RTT, MTProto rate limits, and external `ffmpeg` subprocesses rather than CPU-bound hot loops. Introducing `cgo` shared libraries across 68 packages adds FFI marshalling overhead and breaks `CGO_ENABLED=0` static builds.
Decision: Build a standalone pure-Rust Cargo workspace under `rust/` (keeping the existing Go codebase untouched as the behavioral reference and test oracle), recording CLI/unit baselines for verification and porting wave-by-wave from Wave 0 up.
Consequences: No `cgo` runtime glue needed; all crates target pure Rust + `rustls`; cutover swaps the compiled binary directly.

## ADR-002: Preserve full CLI surface and all command modules
Status: accepted (2026-11-27)
Context: `cmd/mibot-lite/main.go` exposes `--serve`, `--check`, `--verify`, `--login` (`--api-id`, `--api-hash`, `--force`), `--backup`, `--restore` (`--force`), `--import-mibox`, `--version`, `--root`, `--verbose`, plus ~50 Telegram userbot command groups (`internal/commands/*`).
Decision: Port every CLI mode, flag, exit code semantics, and all command modules without dropping features.
Consequences: Existing `deploy/mibot-lite.service` systemd unit and `scripts/install.sh` / `.update` workflows remain drop-in compatible.

## ADR-003: 100% on-disk and session format compatibility
Status: accepted (2026-11-27)
Context: Production deployments hold `config.json` (with GramJS/Teleproto StringSession), `.env`, `data/*.json`, `.bf` backup archives, and `tgstate` cursor/peer state files.
Decision: Maintain exact backward-compatible JSON schemas, StringSession binary layout, SQLite B-Tree MiBox import reader (`internal/mibox`), atomic temp-file+rename writes (`0600`/`0700`), and `.bf` backup archive layout.
Consequences: Users can switch between Go and Rust binaries on the same `--root` directory without re-logging in or migrating data.

## ADR-004: Normalized behavioral equivalence bar
Status: accepted (2026-11-27)
Context: Network latencies, timestamps, random IDs, and image rasterizer anti-aliasing cannot be bit-identical across languages.
Decision: Enforce exact equivalence on CLI exit codes, JSON schemas/keys, error handling (fail-closed invariants), and `STYLE.md` chat output formatting; allow normalized equivalence for timestamps/IDs/latencies and minor pixel anti-aliasing tolerances in `.status`/`.eatgif`/`.yvlu`.
Consequences: Offline tests verify exact semantics and edge cases; image generators assert dimensions, layout, and perceptual/pixel thresholds.

## ADR-005: Telegram MTProto stack (`grammers =0.12.0`)
Status: accepted (2026-11-27)
Context: `github.com/gotd/td` (layer 229) is used across 35 units, including raw `tg.*` TL calls, `updates` gap-filling + `liveHandler` bypass, `PeerCache` access-hash resolution, multi-DC media download, UTF-16 entity offsets, and per-method `FLOOD_WAIT` `Retrier`. Primary-source research in `rust/research/mtproto-grammers.md` verified `grammers =0.12.0` targets Layer 233, supports `InputRichMessage`, and has zero C/OpenSSL/libsql dependencies when `grammers-session` uses `default-features = false, features = ["serde"]`.
Decision: Adopt `grammers-client`, `grammers-mtsender`, `grammers-session`, and `grammers-tl-types` (`=0.12.0`). Wrap `SenderPoolHandle` in `mibot_bot::Client` with `NoRetries` + custom async `Retrier`, implement a custom `MibotSession: Session` backed by `config.json` + `tgstate`, bypass `updates` gap-filling via a `LiveHandler` draining `pool.updates`, and implement a panic-free UTF-16 `mibot_bot::html` parser instead of `grammers-client`'s `html` feature.
Consequences: Pure Rust MTProto stack compatible with `MemoryDenyWriteExecute=yes` and static `musl` cross-compilation.

## ADR-006: Pure Rust + rustls foundation stack
Status: accepted (2026-11-27)
Context: Static cross-compilation (`linux/amd64`, `linux/arm64`, `darwin/arm64`) must avoid OpenSSL/libvips/libsqlite3/unsafe-libyaml C dependencies. Primary-source research in `rust/research/imaging-fonts.md` and `rust/research/net-cron-yaml.md` vetted pure-Rust crate versions and semantic traps.
Decision: Standardize on `tokio`, `clap` (derive), `reqwest 0.13.5` (`rustls-no-provider` + `rustls` `ring`, `Policy::none()` explicit redirect loop, custom `Resolve` SSRF guard), `serde`/`serde_json`/`serde-saphyr 2.2.0` (pure Rust YAML 1.2), `fancy-regex 0.20.2` + `regex 1`, `croner 4.0.3` + `chrono-tz 0.10.4` (6-field cron), `scraper 0.29.0`, `idna 1.1.0`, `rpassword 7.5.5`, and `image 0.25.10` + `fast_image_resize 6.2.0` + `tiny-skia 0.12.0` + `ab_glyph 0.2.32`.
Consequences: Zero C/OpenSSL/libyaml dependencies across the entire workspace; clean cross-builds.

## ADR-007: Target platforms and checkpoint cadence
Status: accepted (2026-11-27)
Context: `scripts/release.sh` ships `linux/amd64`, `linux/arm64`, and `darwin/arm64` with `checksums.txt` for `.update`.
Decision: Target `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, and `aarch64-apple-darwin` (using current Linux `x86_64` host for continuous dev/test). Pause at 4 checkpoints: (1) Research & ADRs locked, (2) Wave 0–2 offline foundation crates passing, (3) MTProto & App runtime (`bot`/`session`/`app`) passing, (4) Command waves + CLI entrypoint complete.
Consequences: Clear progress visibility and bounded review batches; original Go code remains intact as reference until user approves cutover.
