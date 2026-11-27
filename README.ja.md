# MiBot Lite

[简体中文](README.md) | [English](README.en.md) | [繁體中文](README.zh-TW.md) | **日本語**

省メモリな純 Rust 製 Telegram ユーザーボットです。上流の Go 版 `mibot-lite` をベースに全面リライトし、静的リンクされた単一のネイティブバイナリ（GC なし・C システムライブラリ依存なし）として動作し、コマンドと状態はそれぞれバイナリと JSON ファイルに収まります。

Go 原版および [MiBox](https://github.com/MiCat-S/Mi-Box) との互換性を維持しており、アカウント、`config.json`、セッションファイル、コマンド名そのままですぐに乗り換えられ、MiBox 設定もコマンド 1 つで移行できます（JavaScript ランタイム・SQLite・プラグインサンドボックスなし）。

| | MiBot Lite（Go 原版 / `main`） | MiBot Lite（Rust 版 / `rust`） |
|---|---|---|
| 言語・メモリモデル | Go 1.26（GC ランタイムあり） | **純 Rust 2024（Tokio 非同期・GC なし・C 依存ゼロ）** |
| バイナリサイズ | 約 21 MB | **Linux `musl` strip 後 約 34 MB（完全静的単一ファイル）** |
| MTProto スタック | `gotd/td`（Layer 229） | **純 Rust `grammers =0.12.0` + `rustls`（`ring`）** |
| 並行制御・ディスパッチ | Goroutine + 3 プールチャネル | **Tokio + 3 プール有界セマフォ（Panic 隔離・ドレイン対応）** |
| 画像処理・状態カード | `image/draw` + `golang.org/x/image` | **`image` + `fast_image_resize` + `tiny-skia` + `ab_glyph`** |
| セッション・状態形式 | `gotd-session.json` + `updates.json` | **上位互換 + GramJS / Telethon 変換 + `.cursor` 差分永続化** |
| コマンド・安全基準 | 45 コマンド群・`.sudo` 許可リスト・IP マスク | **45 コマンド群完全互換 + fail-closed ガード + UTF-16 エンティティ整合** |

`Cargo.lock` の監査により `openssl-sys` や `libsqlite3-sys` などの禁止された C システム依存を含まないことを検証済みです。スタンプ生成時に一時的に起動する `ffmpeg` の子プロセスは常駐しません（[設計上の判断](#設計上の判断)を参照）。

## インストール

サーバーで実行します（Linux amd64 / arm64、systemd、root）：

```sh
bash <(curl -fsSL https://raw.githubusercontent.com/Yapixel/mibot-lite/rust/scripts/install.sh)
```

ダウンロード、検証、ログイン、サービスとしての登録まで一度に行います。もう一度実行するとアップグレードになり、
その後は Telegram で `.update run` を送っても更新できます。手順の詳細、設定項目、メモリ上限は
[INSTALL.md](INSTALL.md)（中国語）にあります。

- **ffmpeg**：`.eatgif`、`.eat`、`.eat2`、`.t` で使い、`.yvlu` で動画を引用するときにも使います。入っていなくても
  他のコマンドには影響しません。`apt install -y ffmpeg`。
- **MiBox からの移行**：ai、sum、whois、エイリアス、プレフィックスなどの設定をそのまま移せます：

  ```sh
  ./mibot-lite --import-mibox /root/mibot --root /root/mibot-lite
  ```

  同じアカウントで 2 つのプログラムを行き来できますが、**同時に動かすことはできません**。同じアカウントの接続が
  2 つあると、Telegram 上で互いに切断し合います。
- **OS の再インストールやマシンの移転**：Telegram で `.bf` を送ると設定が「保存済みメッセージ」にバックアップされます。
  新しいマシンではインストールスクリプトに `--restore バックアップファイル` を付ければ、再ログインなしでそのまま
  復元できます。[INSTALL.md 第 10 節](INSTALL.md#10-备份与恢复重装系统换机器)を参照してください。

## コマンド

デフォルトのプレフィックスは `.`、`。`、`$` で、`.prefix` で変更できます。各コマンドの詳しい使い方は
`.help コマンド` を送るか、コマンドの後ろに `--help` を付けてください。

「貸出」列は、[`.sudo` / `.sure`](#アカウントの貸し出しsudo-と-sure) で他の人に使わせられるかどうかを示します。
✓ はコマンド全体を貸し出せるもの、**一部** は調べる・使う部分だけを貸し出せて設定を変えるサブコマンドは本人専用のもの、
空欄は本人専用です。

**運用とメンテナンス**

| コマンド | 内容 | 貸出 |
|---|---|---|
| `.ping [ドメイン\|IP]` | Telegram またはウェブサイトまでの遅延 | ✓ |
| `.status` | 状態カード（CPU、メモリ、ディスク、Swap） | ✓ |
| `.memory` | プロセスのメモリ | ✓ |
| `.sysinfo` | 詳しいシステム情報 | |
| `.service [サービス名]` | systemd サービスの状態、稼働時間、メモリ、CPU | |
| `.version` `.ver` | バージョン情報 | ✓ |
| `.help` `.h` | コマンド一覧、または個々のコマンドの説明 | ✓ |
| `.update [check\|run\|rollback\|auto]` | 更新の確認、適用、ロールバック（既定で毎日自動更新） | |
| `.restart` | サービスを再起動 | |
| `.log` | 実行ログを書き出す（機密情報は除去済み） | |
| `.bf` | 設定を「保存済みメッセージ」にバックアップ | |
| `.prefix` `.alias` | コマンドのプレフィックスを変更、コマンドに別名を付ける | |
| `.privacy` | 出力に含まれる IP アドレスの伏せ方を設定 | |

**検索とツール**

| コマンド | 内容 | 貸出 |
|---|---|---|
| `.calc 式` | 四則演算 | ✓ |
| `.rate 通貨 [換算先] [金額]` | 為替レートと換算 | ✓ |
| `.tr [言語] テキスト` | Google 翻訳、設定不要 | 一部 |
| `.gt [en] テキスト` | AI 翻訳 | ✓ |
| `.whois ドメイン` | ドメインの登録情報、一括検索にも対応 | 一部 |
| `.ip [IP\|ドメイン]` | IP の所在地と回線事業者 | ✓ |
| `.dig ドメイン [タイプ] [@サーバー]` | DNS レコード、IP の所在地と AS 付き | ✓ |
| `.bgp [dns] [IP\|プレフィックス\|AS]` | IP・プレフィックス・AS の BGP 経路：起源、上流、逆引き | ✓ |
| `.ddg キーワード` `.duckduckgo` | ウェブ検索（DuckDuckGo、ブロック時は Firecrawl） | ✓ |
| `.b64encode` `.b64decode` `.urlencode` `.urldecode` `.encode` | Base64・URL のエンコードとデコード | ✓ |
| `.subinfo [txt] サブスクURL` `.cha` | サブスクのノード数・プロトコル・地域・通信量と期限（後者は簡易版） | |
| `.bin カード番号の先頭 6–8 桁` | カード番号に対応する発行銀行 | ✓ |
| `.ids` `.dc` | ユーザーやチャットの情報と、所属するデータセンター | ✓ |
| `.speedtest` `.st` | サーバーの回線速度測定（Ookla 公式 CLI） | 一部 |

**AI**

| コマンド | 内容 | 貸出 |
|---|---|---|
| `.ai [search] 質問` | AI との対話とウェブ検索 | 一部 |
| `.sum [件数]` | グループのメッセージの要約、定期実行も可能 | 一部 |
| `.checkapi [check\|speed\|compare] タグ` | AI コマンドで設定した API を検査（モデル一覧・テスト質問・速度・比較） | |

Telegram の「保存したメッセージ」で OpenAI または互換ゲートウェイを設定します。URL、API キー、モデル ID をプロバイダーの値に置き換え、各行を個別に送信してください。

```text
.ai config add main https://api.openai.com/v1 APIキー openai-compatible
.checkapi models main
.ai model chat main モデルID
.ai こんにちは
```

新しい設定では Chat Completions を使用します。`.ai config set main responses on` で Responses に切り替えられます。`/responses` で終わる URL を新規登録すると自動で有効になります。ゲートウェイの URL にはパスの接頭辞やクエリパラメーターを含められます。Anthropic の `/messages` と Gemini の `:generateContent` で終わる URL は自動判別されます。ゲートウェイのベース URL を使う場合は、種別を `anthropic` または `gemini` に指定できます。`.ai config set main stream on` を有効にすると、ストリームを受信し終えた時点で回答が Telegram に届きます。ウェブ検索には、選択したモデルとゲートウェイが対応している必要があります。

**メッセージとスタンプ**

| コマンド | 内容 | 貸出 |
|---|---|---|
| `.yvlu` | メッセージを引用スタンプ、画像、ストーリーにする | 一部 |
| `.eatgif 名前` | 2 人のプロフィール画像からアニメーションスタンプを作る | 一部 |
| `.eat` `.eat2` | プロフィール画像や写真からミームスタンプを作る | 一部 |
| `.sticker` | スタンプを自分のスタンプセットに保存 | |
| `.t テキスト` `.ts` `.tk` | 音声読み上げ（`.ts` で声を選び、「保存したメッセージ」で `.tk` を使って API キーを設定） | |
| `.re [件数] [回数]` | 返信先のメッセージを繰り返し転送 | ✓ |
| `.so キーワード` `.search` | 登録したチャンネルから動画を検索して送信 | |
| `.aff [番号]` | 空港の Aff 紹介文を保存してすぐ送信 | |
| `.save リンク` | メッセージを保存・転送、転送禁止のものも可能 | |
| `.dme 件数` | 自分のメッセージを削除。相手がトリガーワードを送ったときに自動で削除する設定も可能 | |
| `.da` | グループのメッセージを一括削除 | |
| `.acron タイプ Cron チャット` | 秒付き 6 項目の Cron で、メッセージの送信・コピー・転送・削除・ピン留め、またはコマンド実行を定期的に行う | |
| `.trace [絵文字…]` | 指定したユーザーやキーワードのメッセージに自動でリアクション | |

**グループ管理**

| コマンド | 内容 | 貸出 |
|---|---|---|
| `.ban` `.unban` `.kick` `.mute` `.unmute` | 今のグループで BAN、キック、ミュート | ✓ |
| `.sb` `.unsb` | 管理しているすべてのグループで BAN、BAN 解除 | |
| `.refresh` | 管理グループの一覧を更新（1 日キャッシュし、期限切れの後は次に使うときに取り直す） | |
| `.aban` | 上記コマンドのヘルプ | |
| `.keyword` | キーワード自動返信（削除・BAN・ミュートも可） | |
| `.clean_member モード` | 最終オンライン・発言・削除済みでメンバーを検索／削除 | |
| `.guestban [on\|off]` | ゲストボットの広告を削除し、そのボットをこのグループで BAN | |

**アカウント**

| コマンド | 内容 | 貸出 |
|---|---|---|
| `.acn` `.autochangename` | 時刻や天気に合わせて表示名を自動で変更 | |
| `.checkin` | 毎日決まった時刻にボットへチェックイン（コールバックボタンも押せる）、結果をまとめて送信 | |
| `.sudo` `.sure` | リストに載せた人にコマンドを貸し出す（後述） | |

### アカウントの貸し出し：.sudo と .sure

どちらも、リストに載せた人があなたのアカウントを通じてコマンドを実行できるようにするものです。相手がグループで
メッセージを送ると、アカウントがあなたとして同じチャットにコマンドを送り、同じ相手に返信します。`.sudo` では相手が
コマンドを直接送ります。`.sure` はもっと範囲が狭く、相手のメッセージがあなたの決めたルールに合ったときだけ動き、
書き換えもできます。たとえばメンバーが送った `/sb` を `.ban` に変える、といった使い方です。

MiBox と違い、**貸し出せる範囲はホワイトリスト**、つまり上の表の「貸出」列です。MiBox ではリストに載った人が
どのコマンドでも実行でき、`.sudo add` 自体も例外ではないため、許可された人がさらに別の人に許可を出せてしまいます。
ここではエイリアスを展開した後の実際のコマンドで判定するので、別名を付けてもすり抜けられません。今後追加される
コマンドは、デフォルトでは貸し出せません。「一部」のうち `.sum` と `.speedtest` は決められた使い方だけを許可するので、
今後増えるサブコマンドも貸し出せません。それ以外は設定を変えるサブコマンドだけが本人専用で、ほかのサブコマンド
（今後増えるものを含む）は貸し出せます。リストの人が貸し出せないコマンドを送った場合、アカウントは権限がないと
一言返すだけです。

リストは `data/sudo.json` と `data/sure.json` に保存され、`.bf` のバックアップにも含まれます。

### IP アドレスを伏せる：.privacy

MiBox v2 と同じく、アカウントが送信・編集するすべてのメッセージで IP アドレスが伏せられます。デフォルトでは
IPv4 は後ろ 2 つ、IPv6 は後ろ 4 つの区切りを伏せ、IP を指すリンクは取り除き、ファイル名に含まれる IP も同様に
伏せます。`.privacy ip mask 2 4` で伏せる数を変え、`.privacy ip hide` でアドレス全体を「[IP已隐藏]」（IP 非表示）に
置き換えます。

伏せる処理は接続層で行うので、すべてのコマンドの出力が対象になります。他の人の代わりに送るコマンド（`.sudo`、
`.sure`）は伏せません。そのアドレスはもともと相手自身が入力したものだからです。

## 設計上の判断

### プラグイン機構をなくした理由

MiBox 本体は任意の TypeScript プラグインを読み込むため、JS エンジン、SQLite、プラグインのライフサイクル一式を
常駐させる必要があり、これがメモリ消費の主な原因です。MiBot Lite はこの前提を変えました。コマンドは実行中に
インストールできる部品ではなく、バイナリに組み込まれた Rust モジュールとして Tokio 非同期ランタイムと 3 プール
有界スケジューラ上で動作します。コマンドを追加するには再ビルドが必要ですが、その代わりランタイム負荷ゼロで
サンドボックス不要な単一静的バイナリを実現しています。

### 画像と動画

`.yvlu` と `.eatgif` は画像を合成します。MiBox は sharp を使っていますが、これは libvips に依存しており、共有
ライブラリ、画像キャッシュ、スレッドプールを、1 日に数回しか使わない機能のためにプロセスに常駐させます。ここでは
純 Rust の `image` + `fast_image_resize`（CatmullRom / Bicubic スケーラー）+ `tiny-skia` + `ab_glyph` に置き換えており、
libvips やシステムフォントライブラリに依存せず、待機中の余分なメモリ消費もありません。

唯一の外部依存は ffmpeg です。Telegram の動画スタンプは VP9 でなければならず、外部の `ffmpeg` 子プロセスに
委譲しています。ffmpeg は使っていないときはメモリを消費しませんが、**動いている間は 100 MB 以上必要です**。
そのため systemd ユニットでは `MemoryMax` を常駐サイズぎりぎりではなく 512M にし、`MemoryHigh` は
**設定していません**。MemoryHigh は超えても失敗せず、メモリの強制回収を繰り返してプロセスを極端に遅くします。ある
エンコードでは 14,000 回以上発生し、エンコードは終わる前に自身のタイムアウトに達し、原因を示すものは何も残りません
でした。

MiBox との違いが 2 つあります：

- **eatgif は GIF を経由しません**。元の実装はフレームを GIF にエンコードしてから VP9 に変換しており、その途中で
  各フレームが 256 色に減色されていました。ここでは PNG のフレーム列を直接 ffmpeg に渡し、非可逆な中間変換を 1 回
  省いています。
- **yvlu は tgs アニメーションスタンプの変換に対応していません**。その処理には Python の `rlottie-python` が必要ですが、
  本番マシンにはもともと入っておらず、移植しても今動いていない機能を移すだけになるためです。それ以外の挙動は元の
  実装と 1 つずつ突き合わせています：複数メッセージは連番の ID ではなく実際の履歴から取得、転送は元の投稿者の
  発言として扱う、部分引用、管理者の肩書き、絵文字ステータス、大きいプロフィール画像へのフォールバック。

## 開発とテスト結果

```sh
cd rust
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
bash ../scripts/build.sh  # バージョン番号付きでビルド
```

実際のアカウントでのセルフチェック（読み取り専用のコマンドを「保存済みメッセージ」に送り、返信を確認してから削除）。
同じアカウントで同時に 2 つ接続することはできないので、先にサービスを止めてください：

```sh
./mibot-lite --verify --root /デプロイ先ディレクトリ
```

### 検証サマリー（v0.4.0）

- **ワークスペース**：`rust/` 配下の **22 クレート**（全 45 コマンド群とランタイムを網羅）。
- **静的解析**：`cargo clippy --locked --all-targets -- -D warnings` — **警告ゼロ**で通過。
- **単体・挙動互換テスト**：`cargo test --locked` — **245 / 245 合格**（`mibot-commands`: 144、`mibot-bot`: 17、`mibot-imaging`: 9、`mibot-tgstate`: 8、`mibot-command`: 8、`mibot-app`: 7、`mibot-backup`: 5、`mibot-config`: 5、`mibot-logtail`: 5、`mibot-session`: 5、`mibot-botapi`: 5、`mibot-httpx`: 4、`mibot-mibox`: 4、`mibot-store`: 4、`mibot-media`: 3、`mibot-statuscard`: 3、`mibot-lite`: 2、`mibot-login`: 2、`mibot-verify`: 2、`mibot-delivery`: 1、`mibot-fsutil`: 1、`mibot-sysinfo`: 1）。
- **純 Rust 依存監査**：`rust/Cargo.lock` 内に `openssl-sys`、`libsqlite3-sys`、`rusqlite`、`sqlite`、`libsql`、`aws-lc-sys`、`aws-lc-fips-sys`、`libyaml`、`webp-sys`、`libwebp-sys` が含まれないことを確認済み。
- **CLI・インストーラーテスト**：`python3 -m unittest scripts/install_test.py`（5/5 合格）、オフライン CLI スモークテスト（`--version`、引数なし終了コード `2`、存在しない `--root` で `--check` 終了コード `1`）、GitHub Actions CI Run `45984816612` 合格。
- **前提条件の注記**：実アカウントを使った Telegram DC 接続モード（`--serve`、`--verify`、対話型 `--login`）は認証情報とネットワーク接続を必要とするため、CI 上ではローカルのモックハーネス経由で検証しています。

### コードの構成

| 場所 | 内容 |
|---|---|
| `rust/crates/mibot-lite` | CLI エントリーポイント（`--serve` / `--check` / `--verify` / `--login` / `--backup` / `--restore` / `--import-mibox`） |
| `rust/crates/app` | ライフサイクル管理、`flock` 単一インスタンスロック、更新の振り分け |
| `rust/crates/bot` | `grammers` MTProto ラッパー、レート制限リトライ、`SentRecorder`、IP マスク、マルチ DC ダウンロード |
| `rust/crates/session` / `tgstate` | セッション互換アダプター（GramJS / Telethon / `gotd-session.json`）およびデバウンス永続化 |
| `rust/crates/command` | コマンド登録、3 プール有界ディスパッチ、UTF-16 ページネーション |
| `rust/crates/commands/src/` | 全組み込みコマンド実装と共通 `kit` ユーティリティ |
| `rust/crates/imaging` / `statuscard` | 純 Rust 画像合成および状態カードのラスタライズ描画 |

1 つのファイルに複数のコマンドがあるもの：`aban.rs` はグループ管理コマンドすべて、`ids.rs` は `.dc` を含み、
`sudo.rs` は `.sure` を含み、`eatgif.rs` は `.eat` と `.eat2` を含みます。`tts.rs` は `.t`、`.ts`、`.tk`、`encode.rs` は
Base64 と URL の 4 つのコマンド、`subinfo.rs` は `.cha` を含み、`.status` などの
基本コマンドは `core.rs` にあります。短縮形（`.st`、`.h` など）は登録時の別名で、本来の名前と同じモジュールにあります。

コマンドがチャットに返す文言（見出し、失敗メッセージ、使い方、用語、句読点）は [STYLE.md](STYLE.md)（中国語）に従います。
機械的に確認できる部分と 4 言語 README の一致は `test_command_text_style` と `test_readme_matches_registry` がチェックします。

コマンドを追加するには、`rust/crates/commands/src/xxx.rs` を作成して `pub fn register(a: &Arc<App>)` を書き、
`rust/crates/commands/src/lib.rs` の公開モジュールと `register_all` に追加します。他の人に貸し出せるようにするには、
`rust/crates/commands/src/sudo.rs` のホワイトリストおよび 4 言語 README のコマンド表への追加も必要です。

リポジトリ内のテストは外部環境に依存しない純粋なロジックとローカルモックを検証しており、オフラインで完結します。
実際の Telegram アカウントを使った検証は `./mibot-lite --verify` で行えます。

## ライセンス

LGPL-2.1（MiBox と同じ）。

状態カードのフォントは Noto Sans SC のサブセット（`rust/crates/statuscard/src/NotoSansSC-status-subset.ttf`、ビルド時にバイナリへ埋め込み）で、
SIL Open Font License 1.1 のもとで配布しています。ライセンス文は `rust/crates/statuscard/src/NotoSansSC-OFL.txt` です。
