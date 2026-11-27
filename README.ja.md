# banto (番頭)

[English](README.md)

*この日本語版は [README.md](README.md) の翻訳です。内容が食い違う場合は英語版が正です。*

![帳場で台帳に書き込む番頭。台帳の行は端末の一覧のように光り、店員たちが箱詰めのセッションを棚の間で運んでいる。箱のひとつは受け渡しの途中で光っている](docs/assets/banto-hero.png)

**Claude Code と Codex 向けの全画面ターミナル UI です。全セッションを 1 つの検索できる一覧にまとめ、複数を並べて動かし、Director から Worker へ仕事を渡します。**

*番頭（bantō）は昔ながらの日本の商家で、店に詰めて商いを切り盛りする筆頭の奉公人です。*

![WSL 上の banto。新しいセッションに名前を付けて Director にし、Worker を 3 つ（Claude Code が 2 つ、Codex が 1 つ）付ける。Director が各 Worker に役割名を付け、Worker は banto のリレーで報告を返す。Director が Goinkyo に相談し、最後に Director が並べた選択肢からあなたが選ぶ](docs/assets/brigade-demo.gif)

*WSL 上で録画し、3〜5 倍速にしています。Goinkyo（Director が相談する助言役のセッション）が考えている間は 40 倍速です。*

## できること

左の一覧は**帳場**（chōba、「店の勘定台」の意）です。全セッションを並べたもので、Claude Code が `~/.claude` の下に書くセッションファイルと、Codex が `~/.codex` の下に持つセッション索引から、読み取り専用で作ります。検索、ピン留め、アーカイブ、Group への振り分けができます。新しいセッションは、選ぶか入力したディレクトリで始められます。セッションを選べば、id を知らなくても再開できます。再開したセッションは右のペイン（大店、*emporium*）で開き、Vim のバッファのように切り替えながら、バックグラウンドで生かしておけます。すでに開いているセッションを選ぶとそのペインに切り替わり、banto の外のどこか（別の banto や素の `claude`）で動いているセッションは、メッセージを出して拒み、二重には起動しません。動いているセッションをもう一度再開すると、履歴がフォークするからです。

### Director/Worker の Brigade

`B` は、選択したセッションを **Director** に任命し、その隣に新しい **Worker** セッションを起こします。数、モデル、プロダクトは設定でき、Worker は Director とは別に Claude Code と Codex のどちらでも動かせます。Worker のモデルと reasoning effort（推論の強さ）を渡すのは、セッションを作るときだけです。再起動しても、モデルはセッションが最後に使っていたものが保たれます。effort も Codex ではそのまま保たれます。一方、Claude Code では手元の Claude 設定から決まるため、そこに保存したレベルだけが引き継がれます（`max` と、そのセッションだけに設定したレベルは再起動までです）。

banto は MCP 上でメンバー間のメッセージを中継します。各メンバーの起動時に `banto _mcp` へつなぎます。メンバーは `send_to_peer`、`check_messages`、`brigade_status` を使えます。Director にはさらに次のツールがあります。

- `consult_goinkyo` / `dismiss_goinkyo`
- `open_planner` / `close_planner`: 作業が複数の用事にまたがり、まだ固まっていないとき、Director があなたを Planner に引き合わせます（下記）
- `request_decision`: あなたにしか下せない判断を Director が待つときのものです。banto はその Director に `▸ decision` タグを付け、そのペインで Enter を押すまで残します。Director は止めません。質問は読み取り画面（`F5`）で読めます
- `update_progress`: Director が作業の状態（ステージの 1 行、状態付きのタスク、計画外の作業）を記録します。前の記録は置き換わります。ステージは Director のタイルに、残りは読み取り画面（`F5`）に出ます
- `interrupt_worker`: Director が書いた理由をメッセージとして Worker に渡し、Claude の Worker には、リレーが `auto` のときだけ、それを読むまで次のツール呼び出しを banto が断ります（最大 3 回）。ターンを止めることも、実行中の呼び出しを切ることもできません。リレーが `manual` の場合と Codex の Worker には、通常のメッセージとして届くだけで、何も断られません
- `name_member`: Worker の常設の役割に表示名を付けます（宛先は引き続きトークンです）

Goinkyo は、Director が `consult_goinkyo` で相談する助言役のセッションです。既定では Director と同じプロダクトで動くため、Codex の Director の Goinkyo は Codex になります（`goinkyo_agent`）。`goinkyo_agent = "claude"` にすると Claude のままです。Codex の Goinkyo は読み取り専用で、承認の確認なしに動くので、ビルドやテストはできません。Codex がまだ信頼していないディレクトリでの最初の相談は、Codex 自身の信頼の質問で止まり、Goinkyo のペインで答えるまで待ちます。

Planner は複数の用事にまたがり、かつ範囲や完了条件があなたの言葉で決まっていない作業を頼まれたときに、Director が `open_planner` で開くセッションです。Planner は専用のペインであなたと Plan を詰め、下書きを `write_plan` で書きます。Claude の Planner がコードを読むのは `Read`、`Grep`、`Glob` だけです。Planner は Director と同じプロダクトで動き（`planner_agent`）、プロダクトは Planner を開いたときに決まります。`planner_agent = "claude"` にすると Claude のままです。Codex の Planner は Codex の Goinkyo と同じく読み取り専用で、承認の確認なしに動き、Codex のシェル、Web 検索、アプリはそのまま使えるので、読み取り型のアプリのツール、ほかの Codex のスレッド、取得した Web ページの中身は読めます。読み取り画面（`F5`）で下書きを読み、`A`（Shift）を押して画面に出ている版を承認するまで、何も確定しません。画面を開いた後に Planner がもっと新しい版を書いていたときは、banto は承認を断り、先にその版を読むよう伝えます。承認すると banto から Director にメッセージが届きます。メッセージには `goinkyo_consult` に従って Plan を Goinkyo のレビューに回すかどうかも書かれています。Director は承認された Plan を用事に分けます。下書きの段階で Worker を始めないのは、`read_plan` ツールの説明にある指示で、banto が確かめるわけではありません。Director は `close_planner` で Plan を終え、banto は Planner のセッションをアーカイブして、Plan の Markdown の書き出しを残します。

各メンバーには所属する Brigade、トークン、同僚を示す役割ブリーフィングも渡します。Claude Code ではシステムプロンプトへ追記し、Codex では banto 自身の `SessionStart` フックで届けます。中継されたメッセージには別の AI から来たものだと明示する枠を付けるので、操作者の入力と取り違えられることはありません。banto の自動リレーは、未読メッセージがあるアイドルのメンバーを起こします。Claude Code では起動用フックを使い、Codex では短い定型の Nudge（催促）を `codex queue` で積みます。そのため Director→Worker→Director の往復に人手の橋渡しは要りません。`relay = "auto"` のとき、この起こし方はメンバーの文脈全体に対するターンを始めるので、メッセージに依頼の中身がなくても、アイドルのメンバーは 1 ターン分の費用を使うことがあります。このリポジトリの大部分はこのループで作られました。

生きているメンバー全員が、実際の作業のあと 5 秒間アイドルだったとき、Brigade は静まったとみなされます。あなたの入力が求められる瞬間です。banto は Director に印を付け（一覧の行の待機ドット、マゼンタのペイン枠）、ターミナルのベルを鳴らします。ただし Director のペインを見ているあいだは鳴らしません。`F4` を押すと、そのときも鳴らします（ベルだけで、印は付きません）。

どの Brigade にも属さず、ペインに開いたままのセッションにも同じように知らせます。実際の作業のあと 5 秒間アイドルだったら、一覧の行に待機ドットが付いてベルが鳴ります。そのペインの中にいるあいだは鳴りません。読むためだけに開いたセッションは鳴りません。ターンは終わっていてバックグラウンドのコマンドだけが動いている Claude のセッションは、静まったものとして数えます。Codex にはこの区別ができないので、バックグラウンドのターミナルが動いている Codex のペインは作業中として数えます。

最初の Codex の Brigade ができる前に、banto は承認用のペインを 1 回だけ開きます。Codex は起動時に banto のフックを信頼してよいか確認しますが、確認は初回だけで、承認すれば以降の Brigade はすでに信頼済みだからです。

## なぜ tmux だけではだめなのか

tmux はターミナルのウィンドウマネージャです。banto は Claude Code と Codex のセッションのライフサイクルマネージャで、セッションの id、ディレクトリ、まだ動いているかどうか、あなたがどう仕分けたかを知っています。行を選ぶと、たまたま `claude` が動いているシェルではなく、特定のひとつの会話、またはその会話をすでに持っているプロセスに戻ります。banto がペインを自分でホストするのは、マルチプレクサにはペインの隣のセッション一覧も、自分の Brigade を知っているセッションも、セッション間で中継されるメッセージも載せられないからです。

## なぜ herdr だけではだめなのか

[herdr](https://github.com/herdrdev/herdr) はコーディングエージェント向けに作られたターミナルマルチプレクサです。バックグラウンドのサーバーがペインを所有するので、デタッチして後で戻ってこられます。20 を超えるエージェントのうち、どれが作業中か、ブロックされているか、アイドルかも表示します。種類を問わず多数のエージェントを、デタッチやマシン、SSH をまたいで生かし続けたいなら、それは herdr の仕事で、banto にはできません。banto の主題はセッションです。Claude Code と Codex 自身のファイルを読み取り専用で読むので、banto が存在する前や banto の外で始めたセッションも把握でき、同じ会話を二度再開することもありません。また、Director/Worker のセッション間のメッセージも banto 自身が中継します。herdr は、スクリプトやエージェントに CLI とソケット API を渡します。その代わり、banto が話せるエージェントは 2 つで、banto より長生きするサーバーは持ちません。

## 状況と注意点

- 個人用のツールで、1.0 前で、速いペースで変化しています。インターフェースとディスク上のスキーマは前方へ移行されますが、まだ何も約束していません。UI がデータベースを移行する前に、その隣へ `banto.db.before-v<N>` としてコピーを作ります。これはスキーマ `N - 1` の時点のデータベースで、ビルドのスキーマは `banto --version` で分かります。古いビルドは、新しいビルドが移行したデータベースを開けません。コピーを戻すか、新しいビルドを使ってください。
- banto にスーパーバイザーは置きません。意図的にそうしています。banto のペインにホストされたセッションは、banto が終わると終わります。それぞれにまず通常のコンソールクローズを送るので、`claude` はウィンドウを閉じたときと同じように履歴を確定します。banto はあなたが座って使う作業台であって、背後で仕事を動かし続けるデーモンではありません。
- Claude Code のセッションファイルの形式はドキュメント化されておらず、変更される可能性があります。banto は寛容にパースしますが、将来のリリースが banto を驚かせる可能性は残ります。

## 読み取り専用の保証

**banto は `~/.claude` や `~/.codex` の下には何も書き込みません**。セッションファイル、ライブ状態のファイル、履歴、Codex の `threads` と `logs` の sqlite データベースは、すべて厳密に読み取り専用です。banto 自身のファイルは次の場所にあります。

- 設定: 既定では `dirs::config_dir()/banto/config.toml`（Windows: `%APPDATA%\banto\`）。解決順序は[設定](#設定)を参照してください
- データ（sqlite と作業ファイル）: 既定では `dirs::data_local_dir()/banto/banto.db`（Windows: `%LOCALAPPDATA%\banto\`）。設定の `data_dir` か `$BANTO_DATA_DIR` で別の場所を指定できます。`~/.claude` や `~/.codex` の内側にあるデータディレクトリや `db_path` は（どんな名前で到達しても、リンク経由でも）拒否されます。UI と `banto _mcp` は起動に失敗し、エージェントのフックは黙ってストアを開きません

データベースごとに banto の UI はひとつです。実行中の banto がすでに使っているデータベースを開こうとする 2 つ目の `banto` は、起動を拒否します。そのときは、データベースと、分かる場合は保持しているプロセスの id を示します（ロックはデータベースの隣の `banto.db.lock` です）。2 つ目を動かすには、専用のデータディレクトリかデータベース（`BANTO_DATA_DIR`、`data_dir`、`db_path`）を与えてください。

例外がひとつあり、ここに名指しで書きます。Codex の sqlite データベースは、Codex が書き込み中の可能性があるときに読むので、SQLite 自身の読み取り専用の仕組みを使います。対象はセッション索引の `state_5.sqlite` と、生存確認の `logs_2.sqlite` です。`-wal` ファイルがあれば `mode=ro`、なければ `immutable=1` です。追加の書き込みが避けられないのは、クラッシュの残骸、つまり `-shm` なしに `-wal` が残っているケースだけです。そのとき、最初の `mode=ro` の open が新しい `-shm` を作り直します。約 32KB の一度きりの書き込みです。これは banto のデータではなく SQLite 自身の調整用インデックスで、Codex の次の実行でもどのみち作られます。「banto は書き込まない」という言葉がどの箇所でも文字どおりに成り立つよう、ここに明記します。

## インストールとアップグレード

`PATH` 上に `claude`（Claude Code）か `codex`、またはその両方が必要です。banto は本物の CLI をホストするもので、同梱はしません。どちらかが別の場所にある場合は、`config.toml` で場所を指定してください。

```toml
[agent_binaries]
claude = "C:/tools/claude.exe"
codex = "C:/tools/codex.exe"
```

banto は Windows x64、Linux x64（glibc 2.35 以降）、Apple silicon の macOS で動きます。リリースは Ubuntu 22.04 でビルドされ、バイナリがそれより新しい glibc を必要とするとビルドが失敗します。Alpine のような musl のシステムはソースからビルドします。Intel Mac と Windows on ARM 向けのビルド済みバイナリはありません。

macOS は日常的に使っています。2026-11-26 から、メンテナーは Apple silicon の Mac の Ghostty 1.4.0 で、main からビルドした banto を普段使いにしています。セッションの再開（banto の再起動をまたぐものも含む）、Claude と Codex の Worker と Goinkyo を含む Brigade、セッションファイルへの一覧の追従、ペインのキー（改行、単語の削除と移動、プレフィックス）、ペインからのテキストのコピー、ベルとデスクトップ通知が動いています。Ghostty でベルの音を鳴らすには `bell-features = system` が必要です。v0.1.0 と v0.3.0 は `install.sh` でインストールでき、Gatekeeper のプロンプトは出ませんでした。まだ確認していないのは、ブラウザでダウンロードしたバイナリ（Gatekeeper）、実行中の banto をインストーラーが置き換えずに拒否する動作、Ghostty 以外のターミナル、ペインの hang-up と強制終了のタイミングです。

### ワンライナーでインストール

Windows: 開いている PowerShell（Windows PowerShell 5.1 または PowerShell 7）に、次を入力します。`-ExecutionPolicy` のラッパーは不要です。

```powershell
irm https://github.com/kt81/banto/releases/latest/download/install.ps1 | iex
```

Linux と macOS では次を実行します。

```sh
curl -fsSL https://github.com/kt81/banto/releases/latest/download/install.sh | sh
```

インストーラはお使いのプラットフォーム用のアーカイブと `SHA256SUMS` をリリースからダウンロードし、一致しなければ何もインストールしません。`SHA256SUMS` はアーカイブと同じリリースから来るので、この確認で分かるのはダウンロードが破損なく届いたことで、誰がビルドしたかではありません。それは[手動インストール](#手動インストール)の attestation で確認します。置き換える前に新しいバイナリを一度実行するので、ここで起動できないビルドは、いま入っている banto に手をつけません。banto ではない `banto` は上書きを拒否し、`PATH` 上に banto が複数あれば警告します。置き換えるのは最初のひとつだけです。`PATH` やシェルのプロファイルは編集しません。ディレクトリが `PATH` にないときは、追加する一行を表示します。

banto は `PATH` 上にすでにある `banto` の場所に置かれます。なければ `BANTO_INSTALL_DIR` に置かれます。それもなければ、Windows では `%USERPROFILE%\.local\bin\banto.exe`、それ以外では `~/.local/bin/banto` です。次の 2 つの環境変数は PowerShell では同じウィンドウで先に設定し、Unix シェルではパイプの `sh` 側に付けます。

```powershell
$env:BANTO_VERSION = 'v0.1.0'            # a release tag instead of the latest
$env:BANTO_INSTALL_DIR = 'C:\tools\banto' # where, if no banto is on PATH yet
irm https://github.com/kt81/banto/releases/latest/download/install.ps1 | iex
```

```sh
curl -fsSL https://github.com/kt81/banto/releases/latest/download/install.sh | BANTO_VERSION=v0.1.0 BANTO_INSTALL_DIR="$HOME/bin" sh
```

### 手動インストール

お使いのプラットフォーム用のアーカイブと `SHA256SUMS` を、[Releases ページ](https://github.com/kt81/banto/releases)からダウンロードします。アーカイブ名は `banto-<tag>-x86_64-pc-windows-msvc.zip`、`banto-<tag>-x86_64-unknown-linux-gnu.tar.gz`、`banto-<tag>-aarch64-apple-darwin.tar.gz` です。アーカイブの sha256 を `SHA256SUMS` のその行と比べます（PowerShell では `(Get-FileHash <archive>).Hash`、Linux では `grep <archive> SHA256SUMS | sha256sum -c -`、macOS では `grep <archive> SHA256SUMS | shasum -a 256 -c -`）。そのあと、単一の `banto` / `banto.exe` を、banto の置き場にする唯一のディレクトリへ展開します。バイナリは署名されていません。ブラウザでダウンロードした `banto.exe` をエクスプローラーでダブルクリックすると、SmartScreen の "Windows protected your PC" が出ます。"More info" → "Run anyway" で起動できます。ターミナルから起動した場合や `install.ps1` でインストールした場合は、v0.1.0 ではメンテナーのマシンでプロンプトは出ませんでした。

各アーカイブには、どのコミットとワークフローがビルドしたかを示すビルド来歴の証明（attestation）も付いています。ファイルの出どころを示す、ここにある唯一の確認です。[GitHub CLI](https://cli.github.com/) で検証します。検証するのはアーカイブであって、そこから取り出した exe ではないので、展開する前に実行してください。

```sh
gh attestation verify banto-v0.1.0-x86_64-pc-windows-msvc.zip --repo kt81/banto
```

### ソースからインストール

```sh
cargo install --git https://github.com/kt81/banto banto --locked
```

最近の stable な Rust ツールチェーン（edition 2024）と C コンパイラが必要です（同梱の SQLite は C からコンパイルされます）。`~/.cargo/bin` にインストールされます。

開発用にはチェックアウトから次のようにします。

```sh
cargo build --release

# The UI: the session list on the left, banto-hosted panes on the right
cargo run --

# Plain-text session listing
cargo run -- list

# Point at a different Claude home
cargo run -- --claude-home /path/to/.claude
```

### マシンに banto は一つだけ

banto のバイナリはスペースを含まないひとつのパスに、ちょうどひとつだけ置いてください。Codex は banto の `SessionStart` フックを、フックの完全なコマンドで信頼します。そのコマンドには banto 自身のパスが含まれるので、別のパスにあるコピーは Codex にとって別のフックです。その場合に最初のコピーの信頼がどうなるかは、信頼ハッシュの作りから推測したもので、観測したものではありません。スペースを含むパスはフックにまったく書き込めないので、Codex のメンバーは役割ブリーフィングなしで起動します。インストーラが常にその場で置き換えるのはそのためです。スペースを含むパスや `PATH` 上の 2 つ目の banto は警告しますが、拒否はしません。同じパスで置き換えて信頼が保たれたのは、観測した 1 件（2026-11-18）だけです。根拠は[この注](docs/notes/codex-briefing-spike.md#addendum-2026-11-18-replacing-banto-at-the-same-path-keeps-the-trust)にあります。

### アップグレード

banto を終了し、同じインストール行を実行して、banto をまた起動します。どちらのインストーラも、置き換え先のファイルから banto が動いているあいだは拒否します（macOS では未検証です）。アップグレード後の最初の起動はデータベースの隣へ `banto.db.before-v<N>` としてコピーを作ってから、その場で移行します（[状況と注意点](#状況と注意点)を参照）。元に戻すには、古いビルドとそのコピーが必要です。

### Windows Defender

2026-11-06、Microsoft Defender がローカルでビルドしたリリース版 `banto.exe` を `Trojan:Win32/Bearfoos.A!ml`（機械学習による検出）として検知し、対処しました。banto とそれがホストしていたすべてのセッションが終了しました（[経緯](docs/notes/windows-defender.md)）。リリースからダウンロードした banto を Defender が検知した場合は、その sha256 を `SHA256SUMS` と比べてください。そのうえで、検出名と定義ファイルのバージョンを添えて[issue を開いてください](https://github.com/kt81/banto/issues)。除外の追加はやめてください。本当に改ざんされたファイルも見えなくなります。

### アンインストール

唯一の `banto` バイナリを削除します（`where.exe banto` / `command -v banto` で見つかります）。banto のデータも消すには、設定ディレクトリとデータディレクトリを削除します。データディレクトリには `banto.db` とそのロックファイル（`banto.db.lock`、`banto.db.lock.pid`）、`banto.db.before-v<N>` のコピー、作業ファイルがあります。`data_dir`、`BANTO_DATA_DIR`、`db_path` によって別の場所になっていることもあります。セッションは各エージェントが保管している場所に残ります。Codex が `<codex home>/config.toml` に持つ banto のフックの `trusted_hash` は、残しても害はありません。

## キー

ホストしているものがないとき、あるいはすべてアイドルと分かっているときは、`q`/`Esc` は即座に終了します。そうでなければ先に確認を求め、数えた各セッションとその理由をステータスバーに出します。3 秒以内に 2 回目の `q`/`Esc` を押すと終了します。終了すると、ホストしているすべてのプロセスが終わるからです。そのあと各セッションを順に閉じ、終了するまで最大 5 秒待ちます。

| キー | 動作 |
|---|---|
| `j`/`k`、矢印、`PgUp`/`PgDn`、`Home`/`End` | 選択を移動 |
| `Enter` / ダブルクリック | セッションをペインで開く（すでに開いていればそれに切り替える）。Director では、その Brigade 全体を stage する（ステージに出す） |
| `n` / `/` | 新しいセッション（cwd を選ぶか入力する）/ 検索（曖昧検索、タイトル + cwd） |
| `p` / `d` / `g` / `D` | ピン留め / アーカイブ（ソフトハイド）/ Group に入れる / 選択したアーカイブ済みセッションを復元 |
| `Tab` / `a` | Group 表示（Pinned / Group / Ungrouped）の切り替え / 一覧が見せる範囲を順に切り替える: 通常のセッション → 隠れたもの（エージェントが起動したもの）も → アーカイブ済みも |
| `B` / `b` | Director を任命し、Worker を自動起動する（Director の上では: 解散（disband））/ stage 済みの Brigade に Worker をもうひとつ起こす |
| `x` | 選択した行が動かしているものを終了する（確認あり）。Director ではその Brigade を畳む（Close Brigade）。Brigade のすべてのプロセスが終わり、何も削除されないので、Director の行で Enter を押せばすべてが戻る。ペインが開いている単独のセッションでは、そのプロセスを終了する。Worker を Brigade から外すことは決してない。それには、その Worker のタイルで `Ctrl+B x` を使う |
| `F2` / `F3` / `F4` | フォーカスを一覧とペインの間で移動する / stage 済みの Brigade の次のペインを現在のペインにする / 静まったペイン（Director か、単独のセッション）にフォーカスがあってもベルを鳴らす（切り替え） |
| `F5` | Brigade の未回答の判断、Progress、Plan を、読み取り専用の画面で読む（`A` で画面に出ている Plan の下書きを承認する）。一覧からは選択行の Brigade（なければ stage 済みのもの）、ペインからは stage 済みのものを読む。矢印、`j`/`k`、`PgUp`/`PgDn`、`Home`/`End` でスクロールし、`Esc` で閉じる。判断が開いているときは `Enter` で閉じて Director のペインへ移る。画面を開くだけでは何も起動も再開もしない |
| `Ctrl+B` … | tmux 風のプレフィックス（設定可能）。一覧からでもペインからでも使える。フォーカス、タイルの順序、折りたたみ、Solo、ペイン N への移動ができる。有効なあいだは、ステータスバーが全キーバインド表を表示する |
| … `x` | フォーカス中の Worker/Goinkyo（ご隠居）を Brigade から外す（dismiss。確認あり。二度と Brigade には戻らず、セッションは残る）。Director や単独のセッションでは、一覧の `x` と同じ |

ペインでは、どのターミナルでも改行になるのは `Ctrl+J` です。`Shift+Enter` は、ターミナルが Shift を報告する環境で使えます。Shift を付けても素の CR しか送らないターミナルでは使えません。`Alt+Enter`（macOS では `Option+Enter`）も、ターミナルが Alt 付きで送る環境では改行になります。

マウスではペインやタイルの境界をドラッグしてサイズを変え、スタックのタイルはタイトルをドラッグして動かし、タイトル行の `[S]`（Solo）や `[_]`/`[+]`（折りたたむ/開く）を使えます。その行にはエージェント名と、busy、waiting、idle のいずれかも表示されます。

## 設定

`config.toml` は省略可能で、ファイルがなければ既定値になります。ファイルがある場合はパースできる必要があり（正しい TOML で、既知のキーに、正しい型で、そのキーで使える値が入っていること。`relay = "typo"` は `relay = 5` と同じく拒否されます）、できなければ banto は行と列つきのエラーを表示して起動しません。未知のキーは無視されます。最初に見つかったものが使われます。`--config <path>` と `$BANTO_CONFIG` は、さらに存在していなければなりません。次に、存在すれば `$XDG_CONFIG_HOME/banto/config.toml`、続いて `~/.config/banto/config.toml` が使われます。どちらもなければ、既定の `dirs::config_dir()/banto/config.toml`（Windows では `%APPDATA%\banto\`）です。

```toml
claude_home = "C:/Users/you/.claude"   # 上書き（任意）
db_path = "..."                        # 上書き（任意）。空にすると起動エラー
# banto がデータベースと作業ファイルを置く場所。`$BANTO_DATA_DIR` が優先されます。
# 最初の [section] より上に書いてください。[brigade] の下に置くと brigade.data_dir
# として読まれ、黙って無視されます。変えても何も移動せず、banto は新しい場所で
# 空のデータベースを開きます。
data_dir = "D:/banto-data"

# "all"（既定）| "claude" | "codex" | "claude,codex": banto が索引するプロダクト。
agents = "all"

[activity]
today_hours = 24   # アクティビティドットの区切り
week_days = 7

[brigade]           # Director/Worker の Brigade
workers = 1         # Brigade ごとに自動起動する Worker の数（1..=8）
worker_agent = "inherit"   # "inherit"（Director と同じプロダクト）| "claude" | "codex" | "select"（選択画面: エージェント、モデル、effort）
# Worker の自動圧縮ウィンドウ。起動のたびに渡します: "auto"、
# 100000..=1000000 の整数、または ""（フラグなし）。Codex はトークン数だけを受け付けます。Director と Goinkyo には渡しません。
worker_autocompact = 250000
relay = "auto"      # "auto" | "manual" — 起こすための Nudge エンジン
# Brigade のメッセージをすべて全文のまま <data dir>/logs/brigade-<id>.log に追記します。
# このファイルには実際の会話内容が入り、ローテーションも削除もされません。コミットしないでください。
message_log = false
# 各メンバーの役割ブリーフィング。{brigade}、{token}、{peers} は置き換えられます。
# "" ならブリーフィングなしで起動します。既定値は変更してかまわない方針です。
director_prompt = "..."
worker_prompt = "..."
# Director が頼まれなくても Goinkyo に相談する度合い: "off"（頼んだときだけ）
# | "disputes" | "design" | "decisions"。後ろのものは前のものすべてに上乗せします。これは
# Director のブリーフィング内の指示にすぎず、director_prompt が空なら何も起きません。
# Claude の Director は編集後に banto の再起動が必要です。Codex の Director はフックが
# 次のセッション開始時に読み直します。動いている Director は変更を知りません。ペインで伝えてください。
goinkyo_consult = "design"
# Goinkyo が動くプロダクト: "inherit"（Director と同じ）| "claude" | "codex"。
# モデルと effort は [brigade.codex] の goinkyo_model と goinkyo_effort です。
goinkyo_agent = "inherit"
# Director が Planner を開くとき: "auto"（作業が複数の用事にまたがり、かつ範囲や
# 完了条件があなたの言葉で決まっていないとき）| "off"（頼まれたときだけ）。goinkyo_consult と同じく
# director_prompt が空なら何も起きません。
planner = "auto"
# Planner が動くプロダクト: "inherit"（Director と同じ）| "claude" | "codex"。
# Planner を開いたときに決まり、開いている Planner は編集しても動きません。
# モデルと effort は [brigade.claude] または [brigade.codex] の planner_model と planner_effort です。
planner_agent = "inherit"
# Planner の役割の文面。banto が必ず足す段落と共に Claude 自身のシステムプロンプトに
# 取って代わります（Codex の Planner には banto のフック経由で届きます）。{brigade} は置き換えられ、
# "" ならその段落だけが残ります。既定は英語で、あなたの言語で答えるよう Planner に伝えます。
planner_prompt = "..."
# Director の request_decision は、行に印を付けてベルも鳴らします。false なら印だけです。
request_attention = true

# 片方のプロダクトだけが読む設定は、そのプロダクトのテーブルに置きます。上にあるすべての
# [brigade] の通常のキーより下に書いてください。テーブルの下に書いた通常のキーはそのテーブルの
# キーとして読まれ、banto 自身のキーがそこにあると起動を拒否します。モデル、effort、パーミッション
# モードは新しい Worker や Goinkyo に、作成時にだけ渡します。"" ならフラグを渡しません。
# [brigade] 直下の古い書き方も引き続き読まれ、両方あるときはテーブルが優先されます。
[brigade.claude]
worker_model = "sonnet"          # --model
worker_effort = "high"           # --effort (low | medium | high | xhigh | max)
worker_permission_mode = "auto"  # --permission-mode
# Fable と max は既定ではなく、選んで使うものです。Fable は一部のプランで使用クレジットに
# 課金され、max は Claude が保存できないため再起動で外れます。
goinkyo_model = "opus"
goinkyo_effort = "xhigh"
goinkyo_permission_mode = "auto"
planner_model = "opus"
planner_effort = "xhigh"
planner_permission_mode = "auto"
planner_tools = "Read,Grep,Glob"   # --tools: banto 自身のツールのほかに渡す組み込みツール

[brigade.codex]
worker_model = "terra"           # slug、または最新の slug に解決されるファミリー名
worker_effort = "high"           # -c model_reasoning_effort
goinkyo_model = ""               # Codex の Goinkyo のモデル。"" は Codex の既定
goinkyo_effort = ""              # Codex の Goinkyo の -c model_reasoning_effort。"" は渡さない
planner_model = ""               # Codex の Planner のモデル。"" は Codex の既定
planner_effort = ""              # Codex の Planner の -c model_reasoning_effort。"" は渡さない

[keys]
prefix = "C-b"      # ペインのプレフィックスのキー。バックスラッシュは TOML では 'C-\' か "C-\\" と書く
                    # macOS/Linux で使えるキーは、i と m を除く a-z、スペース、\ ] ^ _ と、その数字の別名 4-7

[attention]
notify = "osc777"   # ベルのたびにデスクトップ通知（OSC 777）も出す。対応しない端末は無視する。"off": ベルだけ
```

banto が通知を出すとき、バナーにはセッションか Brigade のタイトルが入ります。端末には、OS の通知の許可が必要です。

`--claude-home` と `--codex-home`、および設定の `claude_home` と `codex_home` は、banto が一覧にして再開するホームを変えます。ペインの中のエージェントが読むホームは変えません。banto は `CLAUDE_CONFIG_DIR` と `CODEX_HOME` を設定しないので、そのエージェントは環境変数か、`~/.claude` / `~/.codex` を使います。両者が違うとき、または環境変数が相対パスのときは、banto が起動時にステータスラインで一度だけ、両方の場所を挙げて知らせます。

## 参考資料

[docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) に設計と要件、[docs/DISCIPLINE.md](docs/DISCIPLINE.md) にアーキテクチャの規律（純粋なコアと、端に寄せた I/O）、[docs/notes/](docs/notes/) に実機での検証ノートがあります。[tools/measure/](tools/measure/README.md) には、エージェントのチームがどれだけ使い、その変更がレビューでどうなるかを測る Python スクリプトがあります（ビルドには含まれません）。

## ライセンス

ライセンスの条文は法的な文言として英語のまま掲載します。英語の条文が正です。

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
