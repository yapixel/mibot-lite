# Porting Rules (`mibot-lite`)

1. **Fail-Closed Security Invariants**: Permission gates (`.sudo`/`.sure` allowlist after alias expansion), `.privacy` IP masking, SSRF checks (`httpx`), and credential redaction (`logtail`, `botapi`) must fail closed; malformed/unreadable configs never drop protection.
2. **UTF-16 Code Units for Telegram Offsets**: Telegram message entities (`offset`, `length`) and 4096-char message limits count UTF-16 code units, not UTF-8 bytes or Unicode scalar values; keep an explicit UTF-16 helper and test with emoji/CJK.
3. **No Unchecked Panics on Fallible Paths**: Never use `unwrap()`, `expect()`, or `panic!()` where the Go original returns an `error`; propagate `Result` and document truly unreachable invariants with `// INFALLIBLE: <why>`.
4. **Atomic File Persistence**: Config, state, and backup writes must use temp-file + `fsync` + atomic rename with `0600`/`0700` permissions, preserving `config.json` / `data/*.json` / `tgstate` compatibility.
5. **Chat Text & Style Contract**: User-facing command replies must conform to `STYLE.md` (punctuation, headers, error messages, truncation).
6. **Non-ASCII & Backtracking Regex**: Use `fancy-regex` for `dlclark/regexp2` patterns (lookaround/backreferences) and verify `\w`/`\b`/`(?i)` semantics on non-ASCII/CJK inputs against Go tests.
7. **Systemd & Resource Hygiene**: Keep `MemoryDenyWriteExecute=yes` compatibility (no JIT), bound worker queues/channels, never hold async mutexes across `.await` sleeps (especially `Retrier` `FLOOD_WAIT`), and clean up `/tmp` ffmpeg workdirs on drop/cancel.
