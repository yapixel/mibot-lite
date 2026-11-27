# MiBot Lite

[简体中文](README.md) | [English](README.en.md) | **繁體中文** | [日本語](README.ja.md)

一個省記憶體的純 Rust Telegram UserBot：基於上游 Go 版 `mibot-lite` 全量重寫，編譯為單一靜態原生執行檔（無 GC、無 C 系統函式庫相依），指令編譯在程式裡，狀態與設定存成 JSON 檔案。

保留與 Go 原版及 [MiBox](https://github.com/MiCat-S/Mi-Box) 的完整相容性：帳號、`config.json`、工作階段檔案與指令名稱維持一致，可直接無痛切換，也能一條指令搬移 MiBox 設定；無外掛沙盒、無 JavaScript 執行環境、無 SQLite。

| | MiBot Lite（Go 原版 / `main`） | MiBot Lite（Rust 版 / `rust`） |
|---|---|---|
| 語言與記憶體模型 | Go 1.26（含 GC 執行期） | **純 Rust 2024（Tokio 非同步，無 GC，零 C 系統相依）** |
| 單一檔案體積 | 約 21 MB | **Linux `musl` strip 後約 34 MB（全靜態單檔）** |
| MTProto 協定層 | `gotd/td`（Layer 229） | **純 Rust `grammers =0.12.0` + `rustls`（`ring`）** |
| 並行與排程 | Goroutine + 三池通道 | **Tokio + 三池有界號誌排程（含 Panic 隔離與優雅排空）** |
| 影像處理與卡片 | `image/draw` + `golang.org/x/image` | **`image` + `fast_image_resize` + `tiny-skia` + `ab_glyph`** |
| 工作階段與狀態格式 | `gotd-session.json` + `updates.json` | **原格式相容 + GramJS / Telethon 互轉 + `.cursor` 增量落盤** |
| 指令與安全基線 | 45 組指令、`.sudo` 白名單、IP 遮蔽 | **45 組指令全量對等 + fail-closed 護欄 + UTF-16 實體對齊** |

產物經 `Cargo.lock` 審計確認不含 `openssl-sys`、`libsqlite3-sys` 等禁用 C 系統相依；產生動態貼圖時臨時啟動的 `ffmpeg` 子行程不常駐記憶體（見[設計取捨](#設計取捨)）。

## 安裝

在伺服器上執行（Linux amd64 / arm64，systemd，root）：

```sh
bash <(curl -fsSL https://raw.githubusercontent.com/Yapixel/mibot-lite/rust/scripts/install.sh)
```

下載、校驗、登入、安裝成服務一步完成；重新執行就是升級，之後也可以在 Telegram 裡傳 `.update run`。
分步流程、設定項目和記憶體限額見 [INSTALL.md](INSTALL.md)（簡體中文）。

- **ffmpeg**：`.eatgif`、`.eat`、`.eat2`、`.t` 需要，`.yvlu` 引用影片時也需要；沒裝不影響其他指令。
  `apt install -y ffmpeg`。
- **從 MiBox 遷移**：ai、sum、whois、別名、前綴等設定可以直接搬過來：

  ```sh
  ./mibot-lite --import-mibox /root/mibot --root /root/mibot-lite
  ```

  同一個帳號可以在兩套程式之間來回切換，但**不能同時執行**：Telegram 會讓同一帳號的兩個連線互相擠掉。
- **重灌系統或換機器**：在 Telegram 裡傳 `.bf`，設定會備份到「儲存的訊息」；新機器在安裝腳本後面加上
  `--restore 備份檔`，原樣還原，不用重新登入。見 [INSTALL.md 第 10 節](INSTALL.md#10-备份与恢复重装系统换机器)。

## 指令

預設前綴是 `.`、`。` 和 `$`，可以用 `.prefix` 修改。每條指令的詳細用法請傳 `.help 指令`，
或在指令後面加上 `--help`。

「可借」一欄指的是能不能透過 [`.sudo` / `.sure`](#借用帳號sudo-和-sure) 借給別人使用：
✓ 是整條指令都能借；**部分**是只能借用查詢、使用的那部分，修改設定的子指令只限本人；空白是只限本人。

**執行與維護**

| 指令 | 作用 | 可借 |
|---|---|---|
| `.ping [網域\|IP]` | Telegram 或某個網站的延遲 | ✓ |
| `.status` | 執行狀態卡片（CPU、記憶體、磁碟、Swap） | ✓ |
| `.memory` | 行程記憶體 | ✓ |
| `.sysinfo` | 詳細系統資訊 | |
| `.service [服務名稱]` | systemd 服務的狀態、執行時間、記憶體與 CPU | |
| `.version` `.ver` | 版本資訊 | ✓ |
| `.help` `.h` | 指令列表或單條指令說明 | ✓ |
| `.update [check\|run\|rollback\|auto]` | 檢查、更新或回滾程式，預設每天自動更新 | |
| `.restart` | 重新啟動服務 | |
| `.log` | 匯出執行日誌（已去除敏感資訊） | |
| `.bf` | 備份設定到「儲存的訊息」 | |
| `.prefix` `.alias` | 修改指令前綴、為指令取別名 | |
| `.privacy` | 設定輸出裡 IP 位址的遮蔽方式 | |

**查詢與工具**

| 指令 | 作用 | 可借 |
|---|---|---|
| `.calc 算式` | 四則運算 | ✓ |
| `.rate 貨幣 [目標] [數量]` | 匯率與換算 | ✓ |
| `.tr [語言] 文字` | Google 翻譯，不用設定 | 部分 |
| `.gt [en] 文字` | AI 翻譯 | ✓ |
| `.whois 網域` | 網域註冊資訊，支援批次查詢 | 部分 |
| `.ip [IP\|網域]` | IP 的位置與電信業者 | ✓ |
| `.dig 網域 [類型] [@伺服器]` | DNS 記錄，附 IP 位置與 AS | ✓ |
| `.bgp [dns] [IP\|前綴\|AS]` | IP、前綴或 AS 的 BGP 路由：起源、上游與反向解析 | ✓ |
| `.ddg 關鍵字` `.duckduckgo` | 網頁搜尋（DuckDuckGo，被擋時改用 Firecrawl） | ✓ |
| `.b64encode` `.b64decode` `.urlencode` `.urldecode` `.encode` | Base64、URL 編碼與解碼 | ✓ |
| `.subinfo [txt] 訂閱連結` `.cha` | 訂閱的節點、協定、地區與流量到期，後者為精簡版 | |
| `.bin 卡號前 6–8 碼` | 卡號對應的發卡銀行 | ✓ |
| `.ids` `.dc` | 使用者或對話的資料、所在資料中心 | ✓ |
| `.speedtest` `.st` | 伺服器測速（Ookla 官方 CLI） | 部分 |

**AI**

| 指令 | 作用 | 可借 |
|---|---|---|
| `.ai [search] 問題` | AI 對話與連網搜尋 | 部分 |
| `.sum [數量]` | 群組訊息摘要，也能設定排程 | 部分 |
| `.checkapi [check\|speed\|compare] 標籤` | 檢測 AI 指令裡設定的 API：模型、測試提問、測速與比較 | |

在 Telegram 收藏夾裡設定 OpenAI 或相容閘道。把下列網址、Key 和模型 ID 換成服務商提供的值，每行分開傳送。

```text
.ai config add main https://api.openai.com/v1 你的APIKey openai-compatible
.checkapi models main
.ai model chat main 模型ID
.ai 你好
```

新設定預設使用 Chat Completions。傳送 `.ai config set main responses on` 可切換到 Responses，新增完整的 `/responses` 網址時會自動開啟。閘道網址可包含路徑前綴與查詢參數。完整的 Anthropic `/messages` 或 Gemini `:generateContent` 端點也能自動辨識。若只有閘道基底網址，可指定 `anthropic` 或 `gemini`。`.ai config set main stream on` 開啟串流傳輸，Telegram 會在收到完整回答後顯示結果。連網搜尋需要所選模型和閘道支援該協定的搜尋功能。

**訊息與貼圖**

| 指令 | 作用 | 可借 |
|---|---|---|
| `.yvlu` | 把訊息做成語錄貼圖、圖片或限時動態 | 部分 |
| `.eatgif 名稱` | 用雙方大頭貼合成動態貼圖 | 部分 |
| `.eat` `.eat2` | 用大頭貼或圖片產生梗圖貼圖 | 部分 |
| `.sticker` | 把貼圖存進自己的貼圖包 | |
| `.t 文字` `.ts` `.tk` | 文字轉語音（`.ts` 選角色，`.tk` 在收藏夾設定 API Key） | |
| `.re [訊息數] [次數]` | 複讀回覆的訊息 | ✓ |
| `.so 關鍵字` `.search` | 在已加入的頻道搜尋影片並傳到目前對話 | |
| `.aff [序號]` | 儲存並一鍵送出機場 Aff 推廣文字 | |
| `.save 連結` | 儲存或轉傳訊息，禁止轉傳的也可以 | |
| `.dme 數量` | 刪除自己的訊息，也可以設觸發詞：別人發出時自動刪 | |
| `.da` | 批次刪除群組訊息 | |
| `.acron 類型 Cron 對話` | 按含秒的六段 Cron 定時傳送、複製、轉傳、刪除、置頂訊息或執行指令 | |
| `.trace [表情…]` | 追蹤指定用戶或關鍵詞，自動給他們的訊息按表情 | |

**群組管理**

| 指令 | 作用 | 可借 |
|---|---|---|
| `.ban` `.unban` `.kick` `.mute` `.unmute` | 在目前群組封鎖、踢出、禁言 | ✓ |
| `.sb` `.unsb` | 在所有管理的群組裡封鎖、解除封鎖 | |
| `.refresh` | 重新整理管理群組快取（快取一天，過期後下次用到時重掃） | |
| `.aban` | 以上幾條的說明 | |
| `.keyword` | 關鍵詞自動回覆，可刪訊息、封鎖或禁言 | |
| `.clean_member 模式` | 依上線時間、發言或註銷狀態搜尋或移出群組成員 | |
| `.guestban [on\|off]` | 自動刪掉訪客機器人的廣告並在本群封鎖那個機器人 | |

**帳號**

| 指令 | 作用 | 可借 |
|---|---|---|
| `.acn` `.autochangename` | 依時間、天氣自動改暱稱 | |
| `.checkin` | 每天定時向機器人簽到，可以點回調按鈕，發彙總 | |
| `.sudo` `.sure` | 把指令借給名單裡的人，見下文 | |

### 借用帳號：.sudo 和 .sure

兩者都是讓名單裡的人透過你的帳號執行指令：對方在群組裡傳訊息，帳號以你的身分在同一個對話裡
送出指令、回覆同一個目標。`.sudo` 讓對方直接傳指令；`.sure` 範圍更窄，對方的訊息要符合你設定的規則，
還能改寫，例如把群友傳的 `/sb` 變成 `.ban`。

和 MiBox 不同，**能借出去的是一份白名單**，也就是上面表格裡的「可借」一欄。MiBox 讓名單裡的人
執行任何指令，包括 `.sudo add` 本身，被授權的人可以再去授權別人。這裡依照別名展開之後的
真實指令判斷，取個別名也繞不過去；日後新增的指令預設借不出去。「部分」的指令裡，`.sum` 和
`.speedtest` 只放行列出的那幾種用法，日後新增的子指令也借不出去；其餘幾條只把修改設定的子指令留給
本人，其他子指令（包括日後新增的）都能借。名單裡的人傳了不能借的指令，帳號只會回一句沒有權限。

名單存在 `data/sudo.json` 和 `data/sure.json`，`.bf` 備份時會一起帶上。

### IP 遮蔽：.privacy

和 MiBox v2 一樣，帳號送出和編輯的每則訊息裡，IP 位址都會被遮蔽：預設 IPv4 遮蔽後 2 段、
IPv6 遮蔽後 4 段，指向 IP 的連結會被移除，檔名裡的 IP 也一樣。`.privacy ip mask 2 4` 修改遮蔽幾段，
`.privacy ip hide` 整個換成「[IP已隐藏]」。

遮蔽在連線層處理，所有指令的輸出都會經過它。替別人代發的指令（`.sudo`、`.sure`）不遮蔽，
那些位址本來就是對方自己打出來的。

## 設計取捨

### 為什麼不要外掛系統

MiBox 的主程式要載入任意 TypeScript 外掛，所以它必須常駐一個 JS 引擎、一個 SQLite 和一整套
外掛生命週期，這是它記憶體占用的主要來源。MiBot Lite 換掉的就是這個前提：指令不再是可以熱安裝的
元件，而是編譯進執行檔的 Rust 模組，運行於 Tokio 非同步執行期與三池有界排程器。代價是新增指令要
重新編譯；換來的是零執行期額外開銷、不需要沙箱的靜態單一執行檔。

### 影像與影片

`.yvlu` 和 `.eatgif` 要合成圖片。MiBox 用的是 sharp，它綁定 libvips：一個共用函式庫、一份影像快取和
一個執行緒池，為了一天用幾次的功能常駐在整個行程裡。這裡換成純 Rust 的 `image` + `fast_image_resize`
（CatmullRom / Bicubic 縮放）+ `tiny-skia` + `ab_glyph`，不依賴 libvips 或系統字型庫，閒置時不占額外記憶體。

唯一的外部相依是 ffmpeg：Telegram 的影片貼圖必須是 VP9，仍交由外部 `ffmpeg` 子行程完成轉碼。ffmpeg
不執行時不占記憶體，但**執行時要一百多 MB**。所以 systemd 單元把 `MemoryMax` 設在 512M，
而不是貼著常駐值設定，並且**不設** `MemoryHigh`。後者超過時不會失敗，只會反覆強制回收，把行程
拖慢：一次編碼裡曾觸發一萬四千多次，編碼還沒跑完就撞上自己的逾時，也沒留下任何錯誤訊息。

和 MiBox 的兩處差異：

- **eatgif 不再先編碼成 GIF**。原本的實作先把影格編碼成 GIF 再轉成 VP9，中間那一步會把每一格壓到
  256 色。這裡直接把 PNG 影格序列交給 ffmpeg，省掉一次有損的中轉。
- **yvlu 不支援 tgs 動態貼圖轉換**。那條路需要 Python 的 `rlottie-python`，正式環境機器上本來就沒裝，
  移植它等於移植一個目前不能用的功能。其餘行為都對照原版逐條對齊過：多則訊息走真實的歷史紀錄而不是
  連續 ID、轉傳歸屬原作者、部分引用、管理員頭銜、emoji 狀態、大頭貼大圖備援。

## 開發與測試

```sh
cd rust
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
bash ../scripts/build.sh  # 帶版本號建置（注入 MIBOT_VERSION）
```

連上真實帳號自我檢查（往「儲存的訊息」傳一組唯讀指令、核對回覆、再刪掉）。同一個帳號不能同時連線兩次，
所以要先停掉服務：

```sh
./mibot-lite --verify --root /部署目錄
```

### 測試與校驗結果（v0.4.0）

- **Workspace 規模**：`rust/` 共 **22 個 crate**，涵蓋全部 45 組指令實作與執行環境。
- **靜態檢查**：`cargo clippy --locked --all-targets -- -D warnings` — **0 警告**通過。
- **單元與行為對等測試**：`cargo test --locked` — **245 / 245 全部通過**（含 `mibot-commands` 144、`mibot-bot` 17、`mibot-imaging` 9、`mibot-tgstate` 8、`mibot-command` 8、`mibot-app` 7、`mibot-backup` 5、`mibot-config` 5、`mibot-logtail` 5、`mibot-session` 5、`mibot-botapi` 5、`mibot-httpx` 4、`mibot-mibox` 4、`mibot-store` 4、`mibot-media` 3、`mibot-statuscard` 3、`mibot-lite` 2、`mibot-login` 2、`mibot-verify` 2、`mibot-delivery` 1、`mibot-fsutil` 1、`mibot-sysinfo` 1）。
- **純 Rust 相依審計**：`rust/Cargo.lock` 中無 `openssl-sys`、`libsqlite3-sys`、`rusqlite`、`sqlite`、`libsql`、`aws-lc-sys`、`aws-lc-fips-sys`、`libyaml`、`webp-sys`、`libwebp-sys` 等禁用 C 系統依賴。
- **CLI 與安裝腳本測試**：`python3 -m unittest scripts/install_test.py` 5/5 通過；離線 CLI `--version`、無參數退出碼 `2`、缺失根目錄 `--check` 退出碼 `1` 均與預期一致；GitHub Actions CI（Run `45984816612`）通過。
- **邊界說明**：連上真實 Telegram DC 的端到端模式（`--serve`、`--verify`、互動式 `--login`）仰賴真實帳號憑證與網路連線，目前測試透過本地 Mock harness 驗證。

### 程式碼結構

| 位置 | 內容 |
|---|---|
| `rust/crates/mibot-lite` | CLI 進入點（`--serve` / `--check` / `--verify` / `--login` / `--backup` / `--restore` / `--import-mibox`） |
| `rust/crates/app` | 執行生命週期、`flock` 單實例鎖與更新分派 |
| `rust/crates/bot` | `grammers` MTProto 封裝、限流重試、`SentRecorder`、IP 遮蔽、多 DC 下載 |
| `rust/crates/session` / `tgstate` | 工作階段相容轉接（GramJS / Telethon / `gotd-session.json`）與防抖狀態持久化 |
| `rust/crates/command` | 指令註冊、三池有界排程與 UTF-16 分頁 |
| `rust/crates/commands/src/` | 全部內建指令實作與共用 `kit` 輔助模組 |
| `rust/crates/imaging` / `statuscard` | 純 Rust 影像處理與狀態卡片點陣化算繪 |

一個檔案裡有幾條指令的：`aban.rs` 是全部群組管理指令，`ids.rs` 含 `.dc`，`sudo.rs` 含 `.sure`，
`eatgif.rs` 含 `.eat`、`.eat2`，`tts.rs` 是 `.t`、`.ts`、`.tk`，`encode.rs` 含 Base64 和 URL 的四條，
`subinfo.rs` 含 `.cha`，`.status` 等基本指令在 `core.rs`。
簡寫（`.st`、`.h` 這些）是註冊時的別名，和本名在同一個模組裡。

指令回到聊天裡的文字（標題、失敗提示、用法、用詞、標點）依照 [STYLE.md](STYLE.md)（簡體中文）撰寫，
`test_command_text_style` 與 `test_readme_matches_registry` 會檢查格式規則及四語 README 的一致性。

新增一條指令：在 `rust/crates/commands/src/` 建立模組並實作 `pub fn register(a: &Arc<App>)`，於 `rust/crates/commands/src/lib.rs`
匯出並掛進 `register_all`。要讓它能借給別人，還得加進 `rust/crates/commands/src/sudo.rs` 的白名單並同步更新四語 README 指令表。

儲存庫裡的測試可完全離線執行，涵蓋純邏輯與本地 Mock 行為驗證；真實 Telegram 帳號端到端檢查可使用 `./mibot-lite --verify`。

## 授權

LGPL-2.1，與 MiBox 相同。

狀態卡片用的字型是 Noto Sans SC 的子集（`rust/crates/statuscard/src/NotoSansSC-status-subset.ttf`，於編譯期嵌入執行檔），
依 SIL Open Font License 1.1 散布，授權條款見同目錄的 `rust/crates/statuscard/src/NotoSansSC-OFL.txt`。
