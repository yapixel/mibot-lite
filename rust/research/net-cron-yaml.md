# Rust HTTP/SSRF, Regex, Cron, YAML, HTML, IDNA & Terminal Research Report (`mibot-lite`)

Date: 2026-11-27  
Target Go Packages Replaced:
- `internal/httpx/httpx.go` (`net/http`, `net/http/httptrace`, `ProxyFromEnvironment`)
- `internal/botapi/client.go` (rate-limited Bot API HTTP POST, `NoRedirect`, redacted errors)
- `internal/commands/subinfo/fetch.go` & `parse.go` (`Dialer.Control` SSRF guard, `github.com/go-faster/yaml`, `html.UnescapeString`)
- `internal/commands/keyword/task.go` & `trigger.go` (`github.com/dlclark/regexp2` ECMAScript + timeout)
- `internal/commands/acron/acron.go`, `data.go`, `run.go` & `internal/commands/sum/sum.go`, `tasks.go` (`github.com/robfig/cron/v3`, `Asia/Shanghai`, `regexp`)
- `internal/commands/ddg/ddg.go` (`golang.org/x/net/html`)
- `internal/commands/dig/dig.go` (`golang.org/x/net/idna`)
- `internal/login/login.go` (`golang.org/x/term`)

Target Constraints (`rust/DECISIONS.md` ADR-001/004/006/007, `rust/RULES.md` §1–§3, §6–§7):
- **Pure Rust + `rustls`**, targeting static cross-compilation (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`) without OpenSSL or C `libyaml` dependencies.
- **Fail-closed security invariants**: SSRF IP validation on DNS resolution (before TCP connect), credential-redirect protection (`Authorization`, `Cookie`, `X-Api-Key`, `Api-Key`, `X-Goog-Api-Key`, `?key=`), YAML anchor/recursion budgets, and bounded regex backtracking.
- **`MemoryDenyWriteExecute=yes` compatible**: no runtime JIT regex compilation.

---

## 1. Recommended Crate Stack & Verified Versions

Verified against `crates.io` and `docs.rs`:

| Domain | Crate | Verified Version | Feature Flags / Notes | Replaces Go Package |
| :--- | :--- | :--- | :--- | :--- |
| **HTTP / Proxy / TLS** | **`reqwest`** | `0.13.5` | `default-features = false, features = ["rustls-no-provider", "socks", "http2", "charset", "json"]` (+ `rustls` `ring` provider) — see §2.2 for `0.13` feature renames & `aws-lc-rs` cross-compile caveat | `net/http`, `ProxyFromEnvironment` |
| **Backtracking Regex** | **`fancy-regex`** | `0.20.2` | default (`unicode` enabled) — for `keyword` lookaround/backreferences; use linear-time `regex` (`1`) for `acron` `del_re` | `dlclark/regexp2` |
| **6-Field Cron** | **`croner`** + **`chrono`** / **`chrono-tz`** | `4.0.3` / `0.10.4` | `croner` default (`chrono` backend); `Seconds::Required` (`acron`) & `Seconds::Optional` (`sum`) | `robfig/cron/v3` |
| **YAML (Pure Rust)** | **`serde-saphyr`** | `2.2.0` | 100% safe pure Rust (`saphyr-parser`), `budget!` DoS limits, parses YAML + JSON | `go-faster/yaml` |
| **HTML Parser** | **`scraper`** | `0.29.0` | wraps `html5ever` `0.40.1` + `selectors` + `ego-tree` (WHATWG HTML5) | `golang.org/x/net/html`, `html` |
| **IDNA** | **`idna`** | `1.1.0` | UTS #46 non-transitional (`Uts46::to_ascii`, `domain_to_ascii`) | `golang.org/x/net/idna` |
| **Terminal Secret Input** | **`rpassword`** + `std::io::IsTerminal` | `7.5.5` + stdlib | `std::io::stdin().is_terminal()` (Rust ≥1.70) + `rpassword::read_password()` | `golang.org/x/term` |

### Recommended `Cargo.toml` Dependency Block

```toml
[dependencies]
reqwest = { version = "0.13.5", default-features = false, features = ["rustls-no-provider", "socks", "http2", "charset", "json"] }
rustls = { version = "0.23", default-features = false, features = ["ring", "logging", "std", "tls12"] }
fancy-regex = "0.20.2"
regex = "1"
croner = "4.0.3"
chrono = "0.4"
chrono-tz = "0.10.4"
serde-saphyr = "2.2.0"
scraper = "0.29.0"
idna = "1.1.0"
rpassword = "7.5.5"
```

---

## 2. HTTP Client, Pooling, Redirect Credential Guard, Proxy & SSRF (`reqwest` `0.13.5`)

### 2.1 Go Baseline
- **`internal/httpx/httpx.go`**:
  - Two shared pools: `shared` (`MaxIdleConns: 32`, `MaxIdleConnsPerHost: 8`, `MaxConnsPerHost: 16`, `IdleConnTimeout: 90s`, `ResponseHeaderTimeout: 30s`, `TLSHandshakeTimeout: 10s`, `Dialer.Timeout: 10s`, `Proxy: http.ProxyFromEnvironment`) and `slowHeaders` (`ResponseHeaderTimeout: 0`, for AI streaming/generation with `WaitForHeaders`).
  - `checkRedirect`: max 3 hops; if the initial request carries credentials (`?key=` query parameter or any of `Authorization`, `Cookie`, `X-Api-Key`, `Api-Key`, `X-Goog-Api-Key` headers), refuses any redirect whose `scheme` or `host` differs (`ErrCredentialRedirect`).
  - `NoRedirect`: skips redirects via a shallow `http.Client` copy sharing the `Transport` pool (`botapi/client.go:238` uses `NoRedirect: true` because Bot API URLs embed the bot token).
  - Default total `Timeout = 30s`, `MaxBytes = 2 MiB` (`io.LimitReader(..., limit+1)`), default `User-Agent: MiBot-Lite/1`.
  - `RequestError` distinguishes stages (`"request"`, `"response_headers"`, `"response_body"`) via `httptrace` and `httpx.Reason` hides raw URLs/hostnames.
- **`internal/commands/subinfo/fetch.go`**:
  - `guardedTransport(true)`: disables proxy environment variables (`Proxy` unset — routing via proxy would defeat local IP checks) and installs `net.Dialer.Control` to validate the **actual resolved peer IP** before TCP connect (preventing DNS rebinding).
  - `publicAddress`: unmaps IPv4-mapped IPv6, requires `IsValid && IsGlobalUnicast && !IsPrivate`, and blocks `0.0.0.0/8`, `100.64.0.0/10` (CGNAT), `192.0.0.0/24`, `192.0.2.0/24`, `198.18.0.0/15`, `198.51.100.0/24`, `203.0.113.0/24`, `240.0.0.0/4`, `64:ff9b::/96`, `64:ff9b:1::/48` (NAT64), `100::/64`, `2001:db8::/32`, `2002::/16`.
  - Redirect check (`subinfo/fetch.go:114-120`): max 5 redirects, only `http`/`https`, and `Hostname()` must equal `allowedHost` (case-insensitive).

### 2.2 `reqwest` `0.13` Feature & Crypto Provider Details
- **Breaking change vs `0.12`**: In `reqwest 0.13`, `rustls` replaced `native-tls` as the default TLS backend, and the feature flag was renamed from `rustls-tls` to `rustls` (or `rustls-no-provider`). `query` and `form` are also separate opt-in features in `0.13`, and `trust-dns` was removed in favor of `hickory-dns`. Requires Rust MSRV `1.85.0+` (workspace uses `edition = "2024"`).
- **Cross-compile caveat (`aws-lc-rs` vs `ring`)**: Default `reqwest 0.13` enables `aws-lc-rs` (`__rustls-aws-lc-rs`), which invokes CMake/C compilers when cross-compiling for `aarch64-unknown-linux-musl` (`ADR-007`). Using `rustls-no-provider` + `rustls` (`features = ["ring", ...]`) avoids CMake and requires calling `rustls::crypto::ring::default_provider().install_default()` once at startup before building any `reqwest::Client` (otherwise `Client::builder().build()` panics).
- **Proxy support**: With the `socks` feature enabled, `reqwest::Client::builder()` automatically honors `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` (including `socks5://` / `socks5h://` / `socks4://`), and `NO_PROXY` from the environment—matching `http.ProxyFromEnvironment`. For SSRF-guarded clients (`subinfo`), call `.no_proxy()`.

### 2.3 Traps & Required Design Patterns

1. **`redirect::Policy::custom` cannot inspect request headers or per-request `NoRedirect`**:
   - `reqwest::redirect::Attempt` only exposes `status()`, `url()`, and `previous() -> &[Url]`. It has **no** access to the initial request's headers (`X-Api-Key`, `Api-Key`, `X-Goog-Api-Key`) or a per-request `NoRedirect` flag.
   - Furthermore, `reqwest` only strips `Authorization`/`Cookie`/`Proxy-Authorization` on cross-host redirects (it does **not** strip `X-Api-Key`/`Api-Key`/`X-Goog-Api-Key`, and Go's `httpx` errors with `ErrCredentialRedirect` instead of silently stripping).
   - **Recommended pattern**: Build `shared` and `slow_headers` `reqwest::Client` instances with **`redirect::Policy::none()`** and implement the `<= 3` (or `<= 5` for `subinfo`) redirect loop explicitly in `httpx::do_request` / `subinfo::fetch`:
     - Gives exact `NoRedirect` per-request support without cloning clients.
     - Enforces exact `ErrCredentialRedirect` on cross-origin (`scheme` + `host`[:port]) redirects when `?key=` or any credential header is present.
     - Enforces `subinfo`'s `allowed_host` + `http`/`https` redirect check cleanly.
     - Lets `httpx` separate stage errors (`request` vs `response_headers` vs `response_body`) around `req.send().await` and streaming body reads!
2. **`ResponseHeaderTimeout` vs `read_timeout`**:
   - `reqwest` provides `.connect_timeout(10s)`, `.timeout(total)`, and `.read_timeout(30s)` (per socket read, resets on each chunk), but no separate `ResponseHeaderTimeout`.
   - For `shared`: use `connect_timeout(10s)` + `read_timeout(30s)` + per-request `.timeout(req_timeout)`. For `slow_headers`: omit `read_timeout` (keep `connect_timeout(10s)` + per-request `.timeout(req_timeout)`).
3. **Streaming `MaxBytes` cap**:
   - Do **not** call `resp.bytes().await` on untrusted URLs (buffers the entire body before checking length). Stream chunks with `resp.chunk().await` in a loop, aborting as soon as `total > max_bytes` (`ErrTooLarge`).
4. **SSRF DNS-Rebinding Guard (`reqwest::dns::Resolve`)**:
   - **Trap**: `reqwest` bypasses `dns_resolver` entirely when the URL host is an IP literal (`http://127.0.0.1/`, `http://[::ffff:127.0.0.1]/`).
   - **Fix (Fail-Closed)**:
     1. **Pre-flight URL check**: Parse URL, require `http`/`https`, and if `url.host()` is `Host::Ipv4` or `Host::Ipv6`, validate `is_public_ip(ip)` before dispatching.
     2. **Custom `reqwest::dns::Resolve`**: Implement `Resolve::resolve(&self, name: Name) -> Resolving` using `tokio::net::lookup_host((name.as_str(), 0))`, canonicalize IPv4-mapped IPv6 (`v6.to_ipv4_mapped()`), and **fail closed** (return an error if resolution fails or if **any** / **all** resolved IPs fail `is_public_ip`; specifically, rejecting if any non-public IP is returned or filtering strictly before returning `Addrs` guarantees `hyper` only dials verified `SocketAddr`s with zero TOCTOU window).
     3. **`is_public_ip` in stable Rust**: `IpAddr::is_global` is nightly-only; implement `is_public_ip` explicitly on `Ipv4Addr`/`Ipv6Addr` matching all Go `netip` + `blockedPrefixes` ranges in `subinfo/fetch.go:34-56` (including IPv4-mapped `::ffff:0:0/96`, RFC 1918, loopback, link-local, `0.0.0.0/8`, `100.64.0.0/10`, `192.0.0.0/24`, `192.0.2.0/24`, `198.18.0.0/15`, `198.51.100.0/24`, `203.0.113.0/24`, `240.0.0.0/4`, `fc00::/7`, `fe80::/10`, `ff00::/8`, `64:ff9b::/96`, `64:ff9b:1::/48`, `100::/64`, `2001:db8::/32`, `2002::/16`).

---

## 3. Backtracking Regex (`fancy-regex` `0.20.2` vs `regex` `1`)

### 3.1 Go Baseline
- **`internal/commands/keyword/task.go` & `trigger.go`**:
  - Uses `dlclark/regexp2` with `RegexOptions.ECMAScript` and optional `IgnoreCase`.
  - Requires lookaround (`^(?!.*白名单).*广告` in `keyword_test.go:96`) and backreferences.
  - Enforces `maxPatternLength = 512` and `maxInputLength = 4096` measured in **UTF-16 code units**, plus `MatchTimeout = 100ms`. Timeout/match errors log a warning and evaluate as non-matching.
- **`internal/commands/acron/run.go`**:
  - `compileRegex` (`del_re`) uses Go stdlib `regexp` (RE2), accepts `/pattern/flags` (`i`, `m`, `s`; ignores `g`, `u`), converts `\uXXXX` / `\u{XXXXX}` to `\x{...}`, and **explicitly rejects** lookaround and backreferences (`acron_test.go:79`).

### 3.2 Rust Replacement & Semantic Traps

- Use **`fancy-regex` `0.20.2`** for `keyword` (supports lookahead/lookbehind and backreferences, pure Rust, no JIT).
- Use **`regex` `1.x`** for `acron` `del_re` (guarantees linear-time matching and naturally rejects `(?=a)` and `(a)\1` at compile time, matching `acron_test.go:79`).

**Critical Semantic Traps (`fancy-regex` vs `dlclark/regexp2` `ECMAScript`)**:
1. **No wall-clock timeout in `fancy-regex`**:
   - `fancy-regex` uses deterministic step counting (`RegexBuilder::backtrack_limit`, default `1_000_000`) rather than a wall-clock timer, returning `Err(fancy_regex::Error::RuntimeError(RuntimeError::BacktrackLimitExceeded))`.
   - Set an explicit conservative cap (e.g., `.backtrack_limit(100_000)`) alongside the `4096` UTF-16 code unit input cap (`s.encode_utf16().count() <= 4096`), and treat `Err(_)` from `is_match` as `false` + warning log.
2. **`\w` and `\b` on CJK / non-ASCII text (`RULES.md` §6)**:
   - In `dlclark/regexp2` `ECMAScript` mode, `\w` is strictly `[a-zA-Z0-9_]` and `\b` only checks transitions between `[a-zA-Z0-9_]` and everything else.
   - In `fancy-regex` with default `unicode_mode(true)`, `\w` matches **all Unicode letters** (including CJK ideographs like `中`, `文`), and `\b` is a **Unicode** word boundary—meaning `\bhi\b` fails to match `"你好hi世界"` in Unicode mode because `好` and `h` are both `\w`!
   - **Caveat on `(?-u)` / `unicode_mode(false)`**: Disabling Unicode globally (`unicode_mode(false)`) puts `regex-syntax` into **byte mode**, where `\u{...}` and Unicode properties fail and `.` matches a single byte instead of a Unicode scalar value! Instead, keep `unicode_mode(true)` and verify/rewrite unescaped `\w`/`\W`/`\b`/`\B` (e.g., rewrite `\w` → `[0-9A-Za-z_]` outside character classes / `0-9A-Za-z_` inside `[...]`, or scope `(?-u:\w)` only for `\w`; for `\b`, emulate ECMAScript boundary or test against Go `regexp2` outputs on CJK inputs).
3. **`(?i)` Unicode case folding**:
   - In Unicode mode, `(?i)` folds `k` ↔ `K` (U+212A) and `s` ↔ `ſ` (U+017F); ECMAScript without `/u` only folds ASCII `A-Z` ↔ `a-z`. Document/test edge cases if strict ASCII folding is ever required.
4. **JS `\uXXXX` / `\u{XXXXX}` escapes**:
   - Both `regex` and `fancy-regex` in Unicode mode support `\uXXXX` (4 hex digits) and `\u{1..6 hex}`, so Go `acron`'s `\x{...}` rewrite is unnecessary in Rust (unless validating 1–6 hex digit bounds). Note that `fancy-regex::RegexBuilder::oniguruma_mode(true)` changes escape/backref rules and is **not** needed for JS patterns (keep default mode).

---

## 4. 6-Field Seconds-Capable Cron (`croner` `4.0.3` + `chrono-tz` `0.10.4`)

### 4.1 Go Baseline
- **`internal/commands/acron`**: `robfig/cron/v3` with `SecondOptional | Minute | Hour | Dom | Month | Dow | Descriptor`, wrapped in `parseCron` which strictly enforces **6 fields** (`sec min hour dom month dow`), rejects 5-field expressions and `@daily` descriptors (`acron_test.go:35`), maps Sunday `7` / `1-7` via `sundaySeven`, supports `JAN`/`SUN` names, evaluates in **`Asia/Shanghai`**, guards against same-task re-entrancy (`running[id]`), serializes executions (`serial` mutex), and supports dynamic `schedule`/`unschedule` + panic isolation.
- **`internal/commands/sum`**: `sumCronParser` accepts **both 5-field and 6-field** Cron (first field is seconds when 6 fields) plus descriptors, evaluating in **local time** (`time.Local`).

### 4.2 Crate Comparison

| Crate | Version | 5-field & 6-field | Sunday `7` | Timezone Support | Verdict |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **`croner`** | `4.0.3` | Yes (`Seconds::Required` for `acron`, `Seconds::Optional` for `sum`) | Native (`0` and `7` both = Sunday in default mode) | `chrono` + `chrono-tz` `0.10.4` (or `jiff` `0.2.46`) via `CronDateTime` | **Recommended** |
| **`cron`** (`zslayton/cron`) | `0.15.0` | 7-field model (`sec min hour dom month dow year`), no strict 5-vs-6 toggle | Partial / 0-6 historically | `chrono::TimeZone` | Less faithful to `SecondOptional` |
| **`tokio-cron-scheduler`** | `0.15.1` | Wraps `croner` internally | Same as `croner` | Defaults to UTC (`_tz` methods needed) | Overweight for `mibot-lite`'s `serial` + `running` + `Registry.Track` lifecycle |

### 4.3 Traps & Integration Notes
1. **Do NOT enable `.alternative_weekdays(true)`**:
   - In `croner` `4.0.3`, default POSIX mode already accepts both `0` and `7` as Sunday (so `0 0 9 * * 7` and `0 0 9 * * 1-7` work natively). `.alternative_weekdays(true)` switches to **Quartz** numbering (`1 = Sunday, 7 = Saturday`), which breaks all `1-5` weekday schedules!
2. **Descriptors (`@daily`, etc.)**:
   - `acron` explicitly rejects `@daily` (`acron_test.go:35`). Check `fields.len() != 6 || expr.trim_start().starts_with('@')` before calling `croner`'s `CronParser::builder().seconds(Seconds::Required).build()`.
   - For `sum`, if `@daily`/`@hourly`/`@weekly`/`@monthly`/`@yearly` descriptors are passed, map them to 5-field equivalents (or enable `croner`'s descriptor/nickname handling if matching) and enforce `fields.len() == 5 || fields.len() == 6`.
3. **`croner` extended tokens (`?`, `L`, `W`, `#`)**:
   - `croner` accepts `?`, `L`, `W`, `#` in DOM/DOW, whereas `robfig/cron` only treats `?` as `*` and rejects `L`/`W`/`#`. If strict parity is required, validate allowed token characters (`sumCronToken`/`sumDowToken`) or reject `L`/`W` at parse time.
4. **Timezone binding**:
   - `acron` must evaluate `cron.find_next_occurrence(&now_shanghai, false)` / `cron.iter_after(now_shanghai)` using `chrono::Utc::now().with_timezone(&chrono_tz::Asia::Shanghai)`.
   - `sum` evaluates using `chrono::Local::now()`.
   - Build a tiny ~60-line Tokio scheduler task (next-wake `tokio::time::sleep_until` loop or `JoinSet`) so `acron` keeps exact `deliveryPrepared` crash-skip, `running` non-reentrancy, `serial` mutex, and `Registry.Track` shutdown quiescence.

---

## 5. Pure-Rust YAML (`serde-saphyr` `2.2.0`)

### 5.1 Go Baseline & Crate Status
- **`internal/commands/subinfo/parse.go`**: Uses `github.com/go-faster/yaml` to parse untrusted Clash subscription bodies (YAML or JSON, up to 2 MiB) and extract `document["proxies"]` → `[]` of `{ type, name }`, ignoring server/password fields and distinguishing 4 failure messages (`"订阅不是有效的 Clash 配置"`, `"无效的 Clash 节点列表"`, `"无效的 Clash 节点"`, `"无效的 Clash 协议"`).
- **Crate ecosystem status (verified on `crates.io`)**:
  - `serde_yaml` (`0.9.34+deprecated`): officially archived/deprecated (March 2024).
  - `serde_yml` (`0.0.13`): **deprecated** (now a transition shim) — *Note: update `rust/DECISIONS.md` ADR-006 which mentioned `serde_yml` as a candidate.*
  - `serde_yaml_ng` (`0.10.0`) & `yaml_serde` (`0.10.7`): maintained forks of `serde_yaml`, still backed by `unsafe-libyaml` (C2Rust transpiled C with extensive `unsafe`).
  - **`serde-saphyr` `2.2.0`** (**Recommended**): built on `saphyr-parser`, **100% safe pure Rust**, panic-free on malformed input, supports `serde_json::Value` as well as typed `Deserialize` structs, and accepts JSON (since JSON is valid YAML 1.2).

### 5.2 Traps & Security Budgets
1. **YAML Alias / Recursion Bomb Protection (`budget!`)**:
   - Because Clash subscriptions come from arbitrary untrusted URLs, configure `serde_saphyr::options!` with `serde_saphyr::budget!` (capping `max_anchors` / recursion depth) to prevent Billion Laughs memory blowup.
2. **Duplicate Keys & Non-Finite Floats**:
   - Configure `DuplicateKeyPolicy::LastWins` (matches Go `map[string]any` / JS `js-yaml` overwrite behavior) and `NonFiniteFloatPolicy::AsString` when deserializing into `serde_json::Value` (otherwise `.nan`/`.inf` in untrusted YAML fails JSON `Value` conversion).
3. **YAML 1.1 Booleans (`yes`/`no`/`on`/`off`)**:
   - `saphyr` follows **YAML 1.2**, so unquoted `NO` or `ON` stay strings rather than turning into `false`/`true`.
4. **Error-branch Parity**:
   - Deserializing into `serde_json::Value` lets `parse_clash` inspect `Value::Object` → `"proxies"` (`Value::Array`) → each item (`Value::Object` with string `"type"` and optional `"name"`), matching all 4 user-facing error strings in `subinfo_test.go:69-71`.

---

## 6. HTML Parsing (`scraper` `0.29.0`) & IDNA (`idna` `1.1.0`)

### 6.1 HTML (`scraper` `0.29.0` vs `tl` `0.7.8`)
- **Go Baseline**: `internal/commands/ddg/ddg.go` (`golang.org/x/net/html`) walks the DOM for `.result` (skipping `.result--ad`), `a.result__a`, and `.result__snippet`, unescaping entities and normalizing whitespace. `internal/commands/subinfo/fetch.go` extracts `<title>` and calls `html.UnescapeString`.
- **Recommendation**: **`scraper` `0.29.0`** (pure Rust: `html5ever` `0.40.1` + `selectors` + `ego-tree`).
  - Unlike `tl` (`0.7.8`), which targets fast zero-copy parsing of clean HTML and lacks full HTML5 error recovery, `scraper` uses Servo's WHATWG HTML5 parser (`html5ever`), handling malformed tags and HTML entities identically to `golang.org/x/net/html`.
  - Reuse `scraper` in `subinfo/fetch.go` (`Selector::parse("title")`) instead of adding a separate `html-escape` dependency or regex-matching `<title>`.
- **Traps (`ddg.go`)**:
  - **Whitespace between inline elements**: Go's `text(node)` (`ddg.go:207-221`) appends `' '` after every `TextNode`, then collapses runs via `strings.Fields`. In `scraper`, `el.text().collect::<String>()` joins adjacent text nodes with **no** space (`<span>a</span><span>b</span>` → `"ab"`). Join `el.text()` chunks with `" "` and normalize with `split_whitespace().collect::<Vec<_>>().join(" ")` to match Go.
  - **Nested `.result` skipping**: `ddg.go:109-117` stops descending into a `.result` block once processed (`return` instead of recursing into children). Replicate with a manual `ego_tree::NodeRef` preorder traversal (skipping children of `.result` elements) or filtering out nested `.result` matches.

### 6.2 IDNA (`idna` `1.1.0`)
- **Go Baseline**: `internal/commands/dig/dig.go:127` calls `idna.Lookup.ToASCII(name)` (non-transitional UTS #46, `CheckHyphens(false)`, `VerifyDNSLength(false)` — `dig.go:133` separately validates `len(name) <= 253` and `domainPattern`).
- **Recommendation**: **`idna` `1.1.0`** (pure Rust, UTS #46, non-transitional by default).
- **API & Trap**:
  - `idna::domain_to_ascii(&name)` uses `AsciiDenyList::URL`, `Hyphens::Allow`, `DnsLength::Ignore`, non-transitional UTS #46 — matching `idna.Lookup.ToASCII` closely.
  - Note: `idna::domain_to_ascii_strict` enables `UseSTD3ASCIIRules`, strict hyphen checks, and DNS label length checks (`1..=63`, total `<= 253`), which is stricter than `idna.Lookup` (it rejects underscores `_` used in SRV/DKIM/TXT record lookups!). Use `idna::domain_to_ascii` (or `Uts46::new().to_ascii(..., Hyphens::Allow, DnsLength::Ignore)`) and keep `dig.go`'s explicit `domainPattern` + `<= 253` check.

---

## 7. Terminal Password Input (`rpassword` `7.5.5` + `std::io::IsTerminal`)

- **Go Baseline**: `internal/login/login.go:220-233` checks `term.IsTerminal(fd)`. If false, prints `"(input is not a terminal; the next value will not be hidden)"` and reads a line; if true, calls `term.ReadPassword(fd)` without echo.
- **Recommendation**:
  - **`std::io::IsTerminal`** (built into Rust stdlib since `1.70`): `std::io::stdin().is_terminal()` replaces `term.IsTerminal(fd)`.
  - **`rpassword` `7.5.5`**: pure Rust (`rpassword::read_password()`), disables terminal echo on Unix/Windows without pulling in `dialoguer`/`console` UI frameworks.
  - When `!stdin().is_terminal()`, print the exact warning message and fall back to `BufRead::read_line` so piped/non-interactive `--login` tests behave identically to Go.
