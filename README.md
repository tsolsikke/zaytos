# ZaytOS

[![check](https://github.com/tsolsikke/zaytos/actions/workflows/check.yml/badge.svg)](https://github.com/tsolsikke/zaytos/actions/workflows/check.yml)

ZaytOS is an educational x86_64 operating system written in Rust, featuring a
custom UEFI bootloader, SMP, Ring 3 user space, persistent ext2 storage, a shell,
and a text editor.

x86_64向けの、学習目的の自作OS。Rust製のモノリシックカーネルである。
UEFIで起動し、マルチコアで動き、Ring 3のユーザープログラムを走らせ、
ext2のディスクへ書いたものが再起動をまたいで残る。

Linuxディストリビューションや実用品の代替を目指すものではない。
対象はx86_64のみで、動かす先はQEMU + OVMFである。
重視しているのは、堅牢性・設計判断を残すこと・検査できることの3つで、
実装済みのものと予定しているものを分けて書くようにしている。

## 開発状況

一人で開発している学習用のプロジェクトである。
下に挙げたものは実際に動いているが、OSとして必要なものの多くはまだ無い
（「現在の制約・未実装機能」を参照）。
インタフェースは予告なく変わる。

## 主な実装済み機能

- UEFIブート。自前のbootloaderが`kernel.elf`を読み、higher-halfへ写して制御を渡す
- 物理フレームアロケータ、自前のページテーブル、カーネルヒープ
- マルチコア（SMP）。BKLで守り、プリエンプティブに切り替える。IPIとTLB shootdown
- プロセスごとのアドレス空間と、Ring 3のユーザープログラム
- `int 0x80`のシステムコール（番号はLinux x86-64から採るが、互換は目標にしない）
- virtio-blkの上のext2。読みと書きの両方で、**書いたものは再起動をまたいで残る**
- 画面コンソール（文字描画・折り返し・スクロール・代替画面）とシリアルログ
- ANSIエスケープの解釈（truecolorを含む）
- PS/2キーボード。JIS配列とUS配列を起動時に選べる
- 環境変数、`/etc/profile`と`~/.profile`、`/root/.zash_history`に残るコマンド履歴

## 技術的な特徴

- **higher-half kernel**（`0xFFFFFFFF80000000`、非PIE）。恒等マッピングは起動の
  途中で外し、以降は高位の窓と直接写像だけで動く（[ADR-0024](docs/adr/0024-higher-half-kernel.md)）
- **SMPとBKL。** 先に大きなロックを1つ置き、細粒度化は測ってから判断する方針である
  （[ADR-0023](docs/adr/0023-smp-bkl-first.md)）。TLBの無効化は世代番号で追い、
  応答を待たない（[ADR-0027](docs/adr/0027-tlb-shootdown-without-ack.md)）
- **Ring 3とプロセス別アドレス空間。** ユーザーポインタは実PTEを読んで検証する
- **端末の状態を画面から切り離してある。** セルの格子・桁送り・折り返しは
  ハードウェアに依らない純粋ロジックで、ホストの`cargo test`で検証している
  （[ADR-0040](docs/adr/0040-terminal-state-apart-from-the-screen.md)）
- **ユーザーランドはUTF-8を壊さない。** 挿入点は常に文字の境界に置き、
  桁は幅で数える（全角は2セル）。境界の計算は`common`に置き、カーネルと
  ユーザープログラムが同じ実装を使う
  （[ADR-0054](docs/adr/0054-the-screen-counts-characters-not-bytes.md)、
  [ADR-0045](docs/adr/0045-userland-borrows-pure-logic-from-common.md)）
- **外の道具で結果を確かめる。** 自分で書いた機構だけで成功と判定しない（下記）

## ユーザーランド

`/bin`に置いているもの。すべてRustで書いた独立の実行ファイルである。

| プログラム | 説明 |
|---|---|
| `zash` | シェル。行編集、履歴、`$NAME`の展開、`export`と`set`、Tabの補完 |
| `zi` | vi風のエディタ。ノーマル／インサート／コマンドの3モード、代替画面 |
| `less` / `more` | ページャ |
| `ls` `cat` `echo` `rm` `mkdir` `rmdir` `touch` `tail` | 基本のコマンド |

`zash`はTabでコマンド名とパスを補完し（共通接頭辞まで伸ばし、もう一度打つと
一覧を出す）、`Ctrl+A`/`Ctrl+E`/`Ctrl+K`/`Ctrl+U`/`Ctrl+W`などの行編集を持つ。
`zi`のバッファは固定長配列ではなく、ユーザーヒープの上で必要なだけ伸びる
（物理メモリとヒープの上限までである）。

## ビルドと起動

### 前提

WSL2（Ubuntu系）+ WSLg、またはLinuxで開発している。

- Rust（rustup経由。ツールチェインの版と`x86_64-unknown-uefi` /
  `x86_64-unknown-none`ターゲットは`rust-toolchain.toml`により初回ビルド時に自動導入される）
- QEMUとOVMF: `sudo apt install qemu-system-x86 ovmf`
- e2fsprogs: `sudo apt install e2fsprogs`（`mke2fs`と`e2fsck`）

`mke2fs`はビルド時に使う。カーネルが読むext2の像を`build.rs`が建てるためで、
外の道具が作った像を読めることが目的である（自作の書き手が作った像を読めても、
自分の理解どうしの一致しか言えない）。`e2fsck`は、書いた像を独立に検証するために使う。

`rustup`と違い、この要求は`rust-toolchain.toml`では固定できない。版が変わると
`mke2fs`の既定値（ブロックサイズ、inodeサイズ）が動きうるので、実際に使った版は
起動ログの判定行に出る。

### 起動

```
cargo xtask run
```

QEMUを`-display none`で起動し、シリアル出力を端末へ流す。
カーネルは停止せず動き続けるため、`run`は既定で120秒後にQEMUを自動停止する
（`--no-limit`で解除）。

手で触るときは窓を開ける。

```
cargo xtask run --gui --manual
```

`--manual`を付けると上限が外れ、**`disk0.img`が起動間で持ち越される**
（`zi`で保存したものが次の起動に在る）。`--keep-disk`でも持ち越せる。

**ディスク像の扱いに注意。** 旗を付けない実行と`--rebuild-disk`は、
`target/disk0.img`を建てたばかりの像で**上書きする**（前の起動で書いたものは消える）。
検査は毎回同じ像から始めたいので、これが既定である。
残したいものが在るときは`--manual`か`--keep-disk`を使うこと。

## 検査

ZaytOSは「検査そのものが働いていること」を確かめる形を取っている。
考え方は5つである。

- **外の道具で結果を確かめる。** `e2fsck`・`dumpe2fs`・`debugfs`・QEMUのmonitorを使い、
  ZaytOSが書いた像をLinuxで実際にmountする手順も残してある。
  自分で書いた読み手だけで成功を判定すると、自分の理解どうしの一致しか言えない
- **わざと壊して、検査が落ちることを確かめる。** 意図的に振る舞いを変える構成
  （破壊feature）を用意し、「その構成でだけ落ちる判定が在る」ことを確かめてから置く
- **自動判定だけに頼らない。** 画面の見え方など、判定が見ていない範囲は
  運用者の目視で埋め、確かめていないものは「確かめていない」と書き残す
- **通ったかだけでなく、通る理由が変わっていないかを見る。** 能力を足したときに、
  既存の判定が落ちないまま意味だけ変わることが実際に起きた
- **数えたものを記録に残す。** 実測値には測った時点を添える

```
cargo xtask check
```

全構成のビルド、ホストテスト、clippy、`cargo fmt --check`に加えて、
`unsafe`が`// SAFETY:`コメントを伴っているか、コミットメッセージの文体が
揃っているかなどの静的検査を行う。数秒で終わる。

```
cargo xtask check --full
```

上記に加えて、QEMUを繰り返し起動する回帰チェックを順に実行する。
1種類ごとにカーネルをビルドし直して起動するため、1時間強かかる。

個別の回帰チェックは`cargo xtask run --<名前>-test`の形で単体でも走らせられる
（`cargo xtask`を引数なしで実行すると、使い方の一覧が出る）。

## 設計資料

- [docs/architecture.md](docs/architecture.md) — 全体像
- [docs/adr/](docs/adr/) — 個々の設計判断（採用理由と却下した案）
- [docs/roadmap.md](docs/roadmap.md) — 段ごとの進み方と、各段の締め
- [docs/deferred-decisions.md](docs/deferred-decisions.md) — 保留した判断と、その解禁条件
- [docs/verification-coverage.md](docs/verification-coverage.md) — 何を検査していて、何をしていないか
- [docs/troubleshooting.md](docs/troubleshooting.md) — 実装で詰まった記録
- [docs/coding-standards.md](docs/coding-standards.md) — 書き方の規約
- [docs/vision.md](docs/vision.md) — 長期の構想

## ロードマップ

現在地と、次に何をするかは[docs/roadmap.md](docs/roadmap.md)にある。
次はC移植で、その先にGUIを置いている。

## 現在の制約・未実装機能

使ってすぐ当たるものを挙げる。設計上の限界と、検査の穴の一覧は
[docs/roadmap.md](docs/roadmap.md)の各段の締めと
[docs/deferred-decisions.md](docs/deferred-decisions.md)にある。

- **日本語は表示できない。** フォントが収録しているのは可読ASCIIと置換文字だけで、
  それ以外は箱になる。IMEも無い
- **GUIは無い。** 画面はテキストのコンソールである
- **C言語のライブラリもコンパイラも無い。** 動くのはRustで書いた実行ファイルだけである
- **権限も認証も無い。** 利用者の概念が無く、すべてが同じ権限で走る
- `less`の長い行は画面の桁数で切れる（横へ動く手段が無い）
- 引用（`"`と`'`）と語の分割が無いので、空白を含む語を書けない
- `zash`に`Ctrl+Y`（貼り付け）と`Ctrl+R`（逆向き検索）が無い
- ディレクトリは1ブロックに収まる範囲だけで、越えると作成が断られる
- 時刻の源が無いので、`touch`は既存のファイルに何もしない
- ファイルへの書き込みは、像の全体（2MiB）を装置へ書き戻す形である

## 同梱している第三者のコンポーネント

| コンポーネント | 用途 | ライセンス |
|---|---|---|
| [GNU Unifont](third_party/unifont/) | コンソールフォント | SIL OFL 1.1 |

GNU UnifontのグリフデータはSIL Open Font License 1.1とGNU GPL v2以降（フォント埋め込み例外つき）のデュアルライセンスで、ZaytOSはOFL 1.1の条件で利用している。
日本語漢字グリフの元になっているjiskan16由来の部分はパブリックドメイン。
出典・ライセンス全文・収録範囲の広げ方は[third_party/unifont/](third_party/unifont/)を参照。

## ライセンス

MIT License（[LICENSE](LICENSE)を参照）。
ただし`third_party/`以下は上記のとおり各コンポーネント自身のライセンスに従う。
