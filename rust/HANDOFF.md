# MiBot Lite Rust Port Handoff (Checkpoint 4 — Complete)

- **Plan & Decisions**: [MAP.md](MAP.md), [DECISIONS.md](DECISIONS.md), [GLOSSARY.md](GLOSSARY.md), [RULES.md](RULES.md).
- **Research Reports**: [mtproto-grammers.md](research/mtproto-grammers.md), [imaging-fonts.md](research/imaging-fonts.md), [net-cron-yaml.md](research/net-cron-yaml.md).
- **CLI Baseline**: `rust/app-cases/` (offline Go CLI cases recorded against `/tmp/mibot-lite-ref`).

## Current Progress
- **Completed (68/68 units across Checkpoints 2–4, all 22 workspace crates)**:
  - **Checkpoints 2 & 3 (20 crates)**: `delivery`, `fsutil`, `httpx` (incl. `do_request_with_headers`), `logtail`, `mibox`, `sysinfo`, `imaging`, `config`, `store`, `botapi`, `statuscard`, `backup`, `media`, `session`, `tgstate`, `bot` (incl. `make_reply_to` forum topic routing + `flush` before `metadata`), `command` (incl. `WorkGuard` / `track_work`), `verify`, `app` (incl. `checkpoint_state`), `login`.
  - **Checkpoint 4 (`crates/commands` + `crates/mibot-lite`)**:
    - `crates/commands`: `kit` + all 45 command modules (Wave 5: 39 units including `ai/{mod,markdown,check}`; Wave 6: `gt`, `re`, `checkapi`, `update`, `sum`, `yvlu`), plus `register_all`, `import_mibox` (including `TB_PREFIX` -> `MIBOT_PREFIX` and root lock refusal), and style / 2-language README (EN/JA) / delegable / standalone-help / read-only tests in `crates/commands/src/lib.rs`.
    - `crates/mibot-lite`: CLI entrypoint binary (`--serve`, `--check`, `--verify`, `--login`, `--force`, `--backup`, `--restore`, `--import-mibox`, `--api-id`, `--api-hash`, `--root`, `--verbose`, `--version`) with `rustls` `ring` provider initialization and Go-aligned exit codes (`0` / `1` / `2`).
- **Verified**:
  - `cargo clippy --all-targets -- -D warnings`: passed (`0` warnings across all 22 crates).
  - `cargo test`: `245/245` tests passed (`app` 7, `backup` 5, `bot` 17, `botapi` 5, `command` 8, `commands` 144, `config` 5, `delivery` 1, `fsutil` 1, `httpx` 4, `imaging` 9, `lite` 2, `login` 2, `logtail` 5, `media` 3, `mibox` 4, `session` 5, `statuscard` 3, `store` 4, `sysinfo` 1, `tgstate` 8, `verify` 2).
  - `cargo build -p mibot-lite` + offline binary smoke checks (`--version` -> `v0.3.11`, no-args -> `exit=2`, missing-root `--check` -> `exit=1`).
  - `Cargo.lock` pure-Rust audit: zero banned C system dependencies (`openssl-sys`, `libsqlite3-sys`, `rusqlite`, `sqlite`, `libsql`, `aws-lc-sys`, `aws-lc-fips-sys`, `libyaml`, `webp-sys`, `libwebp-sys` all absent).

## Notes for Next Operator
- Live Telegram integration modes (`--serve`, `--verify`, interactive `--login`) require valid account credentials / network access and were verified only via unit/mock harnesses (`mibot-verify` 2 tests, `mibot-app` 7 tests, `mibot-login` 2 tests) rather than against a live Telegram DC session in this sandbox.
