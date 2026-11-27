# MiBot Lite

**简体中文** | [English](README.en.md) | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md)

一个省内存的纯 Rust Telegram UserBot：基于上游 Go 版 `mibot-lite` 全量重构，编译为单一静态原生二进制（无 GC、无 C 系统库依赖），命令编译进程序，状态与配置存成 JSON 文件。

保留与原版及 [MiBox](https://github.com/MiCat-S/Mi-Box) 的完整兼容性：账号、`config.json`、会话文件和命令名完全兼容，可直接平滑切换，也能一条命令迁移 MiBox 配置；无插件沙箱、无 JavaScript 运行时、无 SQLite。

| | MiBot Lite（Go 原版 / `main`） | MiBot Lite（Rust 版 / `rust`） |
|---|---|---|
| 语言与内存模型 | Go 1.26（含 GC 运行时） | **纯 Rust 2024（Tokio 异步，无 GC，零 C 系统库依赖）** |
| 单文件体积 | 约 21 MB | **Linux `musl` strip 后约 34 MB（静态单文件）** |
| MTProto 协议栈 | `gotd/td`（Layer 229） | **纯 Rust `grammers =0.12.0` + `rustls`（`ring`）** |
| 并发与调度 | Goroutine + 三池信道 | **Tokio + 三池有界信号量调度（含 Panic 隔离与排空）** |
| 图像处理与卡片 | `image/draw` + `golang.org/x/image` | **`image` + `fast_image_resize` + `tiny-skia` + `ab_glyph`** |
| 会话与状态格式 | `gotd-session.json` + `updates.json` | **原格式兼容 + GramJS / Telethon 互转 + `.cursor` 增量落盘** |
| 命令与安全基线 | 45 组命令、`.sudo` 白名单、IP 打码 | **45 组命令全量对等 + fail-closed 护栏 + UTF-16 实体精修** |

产物经 `Cargo.lock` 审计确认不含 `openssl-sys`、`libsqlite3-sys` 等禁用 C 系统依赖；生成动态贴纸时临时调用的 `ffmpeg` 子进程不常驻内存（见[设计取舍](#设计取舍)）。

## 安装

服务器上执行（Linux amd64 / arm64，systemd，root）：

```sh
bash <(curl -fsSL https://raw.githubusercontent.com/Yapixel/mibot-lite/rust/scripts/install.sh)
```

下载、校验、登录、装成服务一步完成；重跑就是升级，之后也可以在 Telegram 里发 `.update run`。
分步流程、配置项和内存限额见 [INSTALL.md](INSTALL.md)。

- **ffmpeg**：`.eatgif`、`.eat`、`.eat2`、`.t` 要用，`.yvlu` 引用视频时也要用；没装不影响其他命令。
  `apt install -y ffmpeg`。
- **从 MiBox 迁移**：ai、sum、whois、别名、前缀等配置可以直接搬过来：

  ```sh
  ./mibot-lite --import-mibox /root/mibot --root /root/mibot-lite
  ```

  同一个账号可以在两套程序之间来回切，但**不能同时跑**：Telegram 会让同账号的两个连接互相顶掉。
- **重装系统或换机器**：在 Telegram 里发 `.bf`，配置备份到收藏夹；新机器给安装脚本加
  `--restore 备份文件`，原样恢复，不用重新登录。见 [INSTALL.md 第 10 节](INSTALL.md#10-备份与恢复重装系统换机器)。

## 命令

默认前缀是 `.`、`。` 和 `$`，可以用 `.prefix` 改。每条命令的详细用法发 `.help 命令`，
或者在命令后面加 `--help`。

「可借」一栏说的是能不能通过 [`.sudo` / `.sure`](#借用账号sudo-和-sure) 借给别人用：
✓ 是整条命令都能借；**部分**是只能借用来查、用的那部分，改设置的子命令只限本人；空着的是只限本人。

**运行与维护**

| 命令 | 作用 | 可借 |
|---|---|---|
| `.ping [域名\|IP]` | Telegram 或某个网站的延迟 | ✓ |
| `.status` | 运行状态卡片（CPU、内存、磁盘、Swap） | ✓ |
| `.memory` | 进程内存 | ✓ |
| `.sysinfo` | 详细系统信息 | |
| `.service [服务名]` | systemd 服务的状态、运行时间、内存和 CPU | |
| `.version` `.ver` | 版本信息 | ✓ |
| `.help` `.h` | 命令列表或单条命令说明 | ✓ |
| `.update [check\|run\|rollback\|auto]` | 检查、更新或回滚程序，默认每天自动更新 | |
| `.restart` | 重启服务 | |
| `.log` | 导出运行日志（已脱敏） | |
| `.bf` | 备份配置到收藏夹 | |
| `.prefix` `.alias` | 改命令前缀、给命令起别名 | |
| `.privacy` | 设置输出里 IP 地址的打码方式 | |

**查询与工具**

| 命令 | 作用 | 可借 |
|---|---|---|
| `.calc 表达式` | 四则运算 | ✓ |
| `.rate 货币 [目标] [数量]` | 汇率与换算 | ✓ |
| `.tr [语言] 文本` | 谷歌翻译，不用配置 | 部分 |
| `.gt [en] 文本` | AI 翻译 | ✓ |
| `.whois 域名` | 域名注册信息，支持批量 | 部分 |
| `.ip [IP\|域名]` | IP 的位置与运营商 | ✓ |
| `.dig 域名 [类型] [@服务器]` | DNS 记录，附 IP 归属地与 AS | ✓ |
| `.bgp [dns] [IP\|前缀\|AS]` | IP、前缀或 AS 的 BGP 路由：起源、上游与反向解析 | ✓ |
| `.ddg 关键词` `.duckduckgo` | 网页搜索（DuckDuckGo，被拦时改用 Firecrawl） | ✓ |
| `.b64encode` `.b64decode` `.urlencode` `.urldecode` `.encode` | Base64、URL 编码与解码 | ✓ |
| `.subinfo [txt] 订阅链接` `.cha` | 订阅的节点、协议、地区与流量到期，后者是简要版 | |
| `.bin 卡号前 6–8 位` | 卡头对应的发卡行 | ✓ |
| `.ids` `.dc` | 用户或对话的资料、所在数据中心 | ✓ |
| `.speedtest` `.st` | 服务器测速（Ookla 官方 CLI） | 部分 |

**AI**

| 命令 | 作用 | 可借 |
|---|---|---|
| `.ai [search] 问题` | AI 对话与联网搜索 | 部分 |
| `.sum [数量]` | 群消息摘要，也能设定时任务 | 部分 |
| `.checkapi [check\|speed\|compare] 标签` | 检测 AI 命令里配置的 API：模型、测试提问、测速与对比 | |

在 Telegram 收藏夹里配置 OpenAI 或兼容网关。把下面的地址、Key 和模型 ID 换成服务商提供的值，每行单独发送。

```text
.ai config add main https://api.openai.com/v1 你的APIKey openai-compatible
.checkapi models main
.ai model chat main 模型ID
.ai 你好
```

新配置默认使用 Chat Completions。使用 Responses 时发送 `.ai config set main responses on`，新添加的完整 `/responses` 地址会自动开启此模式。网关基址可以带路径前缀和查询参数。完整的 Anthropic `/messages` 或 Gemini `:generateContent` 端点也能自动识别。只有网关基址时，可以显式选择 `anthropic` 或 `gemini`。`.ai config set main stream on` 开启接口流式传输，Telegram 在收齐回答后显示结果。联网搜索需要所选模型和网关支持相应协议的搜索功能。

**消息与贴纸**

| 命令 | 作用 | 可借 |
|---|---|---|
| `.yvlu` | 把消息做成语录贴纸、图片或故事 | 部分 |
| `.eatgif 名称` | 双方头像合成动画贴纸 | 部分 |
| `.eat` `.eat2` | 用头像或图片生成表情包 | 部分 |
| `.sticker` | 把贴纸存进自己的贴纸包 | |
| `.t 文本` `.ts` `.tk` | 文字转语音（`.ts` 选角色，`.tk` 在收藏夹设 API Key） | |
| `.re [消息数] [次数]` | 复读回复的消息 | ✓ |
| `.so 关键词` `.search` | 在添加的频道里搜视频并发到当前对话 | |
| `.aff [序号]` | 保存并一键发出机场 Aff 推广文字 | |
| `.save 链接` | 保存或转发消息，禁止转发的也行 | |
| `.dme 数量` | 删除自己的消息，也可以设触发词：别人发出时自动删 | |
| `.da` | 批量删除群消息 | |
| `.acron 类型 Cron 对话` | 按带秒的六段 Cron 定时发送、复制、转发、删除、置顶消息或执行命令 | |
| `.trace [表情…]` | 追踪指定用户或关键词，自动给他们的消息点表情 | |

**群管理**

| 命令 | 作用 | 可借 |
|---|---|---|
| `.ban` `.unban` `.kick` `.mute` `.unmute` | 在当前群封禁、踢出、禁言 | ✓ |
| `.sb` `.unsb` | 在所有管理的群里封禁、解封 | |
| `.refresh` | 刷新管理群缓存（缓存一天，过期后下次用到时重扫） | |
| `.aban` | 以上几条的帮助 | |
| `.keyword` | 关键词自动回复，可删消息、封禁或禁言 | |
| `.clean_member 模式` | 按上线时间、发言或注销状态搜索或移出群成员 | |
| `.guestban [on\|off]` | 自动删掉访客机器人的广告并在本群封禁那个机器人 | |

**账号**

| 命令 | 作用 | 可借 |
|---|---|---|
| `.acn` `.autochangename` | 按时间、天气自动改昵称 | |
| `.checkin` | 每天定时向机器人签到，可以点回调按钮，发汇总 | |
| `.sudo` `.sure` | 把命令借给名单里的人，见下文 | |

### 借用账号：.sudo 和 .sure

两者都是让名单里的人通过你的账号执行命令：对方在群里发消息，账号以你的身份在同一个对话里
发出命令、回复同一个目标。`.sudo` 让对方直接发命令；`.sure` 更窄，对方的消息要和你设的规则
对上，还能改写，比如把群友发的 `/sb` 变成 `.ban`。

和 MiBox 不同，**能借出去的是一张白名单**，也就是上面表里的「可借」一栏。MiBox 让名单里的人
执行任何命令，包括 `.sudo add` 本身，被授权的人可以再去授权别人。这里判断按别名展开之后的
真实命令来，起个别名绕不过去；以后新加的命令默认借不出去。「部分」的命令里，`.sum` 和
`.speedtest` 只放行列出的那几种用法，以后新加的子命令也借不出去；其余几条只把改设置的子命令留给
本人，别的子命令（包括以后新加的）都能借。名单里的人发了不能借的命令，账号只回一句没有权限。

名单存在 `data/sudo.json` 和 `data/sure.json`，`.bf` 备份时会一起带上。

### IP 打码：.privacy

和 MiBox v2 一样，账号发出和编辑的每条消息里，IP 地址都会打码：默认 IPv4 遮后 2 段、
IPv6 遮后 4 段，指向 IP 的链接会去掉，文件名里的 IP 也一样。`.privacy ip mask 2 4` 改遮几段，
`.privacy ip hide` 整个换成「[IP已隐藏]」。

打码在连接层做，所有命令的输出都经过它。替别人代发的命令（`.sudo`、`.sure`）不打码，
那些地址本来就是对方自己打出来的。

## 设计取舍

### 为什么不要插件系统

MiBox 的宿主要装载任意 TypeScript 插件，所以它必须常驻一个 JS 引擎、一个 SQLite 和一整套
插件生命周期，这是它内存占用的主要来源。MiBot Lite 换掉的就是这个前提：命令不再是可以热装的
构件，而是编译进二进制的 Rust 模块，跑在 Tokio 异步运行时与三池有界调度器上。代价是加命令要
重新编译；换来的是无运行时开销、无需沙箱的静态单文件。

### 图像与视频

`.yvlu` 和 `.eatgif` 要合成图片。MiBox 用的是 sharp，它绑定 libvips：一个共享库、一份图像缓存和
一个线程池，为了一天用几次的功能常驻整个进程。这里换成纯 Rust 的 `image` + `fast_image_resize`
（CatmullRom / Bicubic 缩放）+ `tiny-skia` + `ab_glyph`，不依赖 libvips 或系统字体库，闲置时不占额外内存。

唯一的外部依赖是 ffmpeg：Telegram 的视频贴纸必须是 VP9，仍由外部 `ffmpeg` 子进程完成编码。ffmpeg
不跑的时候不占内存，但**跑的时候要一百多 MB**。所以 systemd 单元把 `MemoryMax` 设在 512M，
而不是贴着常驻值设，并且**不设** `MemoryHigh`。后者超了不会失败，只会反复强制回收，把进程
拖慢：一次编码里曾触发一万四千多次，编码没跑完就撞上自己的超时，也没留下任何错误信息。

和 MiBox 的两处差异：

- **eatgif 不再先编码 GIF**。原实现把帧编码成 GIF 再转 VP9，中间那步会把每帧压到 256 色。
  这里直接把 PNG 帧序列交给 ffmpeg，省掉一次有损中转。
- **yvlu 不支持 tgs 动画贴纸转换**。那条路要 Python 的 `rlottie-python`，生产机上本来就没装，
  移植它等于移植一个当前不工作的功能。其余行为对着原版逐条对齐过：多条消息走真实历史而不是
  连续 ID、转发归属原作者、部分引用、管理员头衔、emoji 状态、头像大图兜底。

## 开发与测试

```sh
cd rust
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
bash ../scripts/build.sh  # 带版本号构建（注入 MIBOT_VERSION）
```

连上真账号自检（往收藏夹发一组只读命令、核对回复、再删掉）。同一个账号不能同时连两次，
所以要先停掉服务：

```sh
./mibot-lite --verify --root /部署目录
```

### 测试与校验结果（v0.4.0）

- **Workspace 规模**：`rust/` 共 **22 个 crate**，覆盖全部 45 组命令实现与运行时。
- **静态检查**：`cargo clippy --locked --all-targets -- -D warnings` — **0 警告**通过。
- **单元测试与行为对等测试**：`cargo test --locked` — **245 / 245 全部通过**（含 `mibot-commands` 144、`mibot-bot` 17、`mibot-imaging` 9、`mibot-tgstate` 8、`mibot-command` 8、`mibot-app` 7、`mibot-backup` 5、`mibot-config` 5、`mibot-logtail` 5、`mibot-session` 5、`mibot-botapi` 5、`mibot-httpx` 4、`mibot-mibox` 4、`mibot-store` 4、`mibot-media` 3、`mibot-statuscard` 3、`mibot-lite` 2、`mibot-login` 2、`mibot-verify` 2、`mibot-delivery` 1、`mibot-fsutil` 1、`mibot-sysinfo` 1）。
- **纯 Rust 依赖审计**：`rust/Cargo.lock` 中 `openssl-sys`、`libsqlite3-sys`、`rusqlite`、`sqlite`、`libsql`、`aws-lc-sys`、`aws-lc-fips-sys`、`libyaml`、`webp-sys`、`libwebp-sys` 均为 0 命中。
- **CLI 与安装脚本测试**：`python3 -m unittest scripts/install_test.py` 5/5 通过；离线二进制校验 `--version`、无参数退出码 `2`、缺失根目录 `--check` 退出码 `1` 全部一致；GitHub Actions CI（Run `45984816612`）通过。
- **边界说明**：连真 Telegram DC 的端到端模式（`--serve`、`--verify`、交互式 `--login`）依赖真实账号凭据与网络环境，当前仓库测试通过本地 Mock harness 覆盖验证。

### 代码结构

| 位置 | 内容 |
|---|---|
| `rust/crates/mibot-lite` | CLI 入口（`--serve` / `--check` / `--verify` / `--login` / `--backup` / `--restore` / `--import-mibox`） |
| `rust/crates/app` | 运行时生命周期、单实例锁（`flock`）与更新分发 |
| `rust/crates/bot` | `grammers` MTProto 封装、限流重试、`SentRecorder`、IP 打码、多 DC 下载 |
| `rust/crates/session` / `tgstate` | 会话兼容适配（GramJS / Telethon / `gotd-session.json`）与防抖状态持久化 |
| `rust/crates/command` | 命令注册、三池有界调度与 UTF-16 分页 |
| `rust/crates/commands/src/` | 全部内置命令实现与 `kit` 公共辅助模块 |
| `rust/crates/imaging` / `statuscard` | 纯 Rust 图像处理与状态卡片栅格化渲染 |

一个文件或子目录里有几条命令的：`aban.rs` 是全部群管理命令，`ids.rs` 含 `.dc`，`sudo.rs` 含 `.sure`，
`eatgif.rs` 含 `.eat`、`.eat2`，`tts.rs` 是 `.t`、`.ts`、`.tk`，`encode.rs` 含 Base64 和 URL 的四条，
`subinfo.rs` 含 `.cha`，`.status` 等基础命令在 `core.rs`。
简写（`.st`、`.h` 这些）是注册时的别名，和本名在同一个模块里。

命令回到聊天里的文字（标题、失败提示、用法、用词、标点）按 [STYLE.md](STYLE.md) 写，
`test_command_text_style` 与 `test_readme_matches_registry` 会检查其中能机器检查的部分与四语 README 的一致性。

新增一条命令：在 `rust/crates/commands/src/` 下新建模块并导出 `register(a: &Arc<App>)`，到 `rust/crates/commands/src/lib.rs`
导出并在 `register_all` 里挂上。要让它能借给别人，还得加进 `rust/crates/commands/src/sudo.rs` 的白名单，并同步四语 README 的命令表。

仓库里的测试覆盖不依赖外部环境的纯逻辑与本地 Mock 行为测试，无需真账号即可完整运行；真实 Telegram 端到端行为可通过 `./mibot-lite --verify` 校验。

## 许可

LGPL-2.1，与 MiBox 一致。

状态卡片用的字体是 Noto Sans SC 的子集（`rust/crates/statuscard/src/NotoSansSC-status-subset.ttf`，在编译期嵌入二进制），
按 SIL Open Font License 1.1 分发，许可证见同目录的 `rust/crates/statuscard/src/NotoSansSC-OFL.txt`。
