# ZaytOS

x86_64向けの、学習目的の自作OS。
Rust製・モノリシック・保護機構あり。

## 名前について
ZaytOS（ゼイトス）の "Zayt" は、ドイツ語の **Zeit（時間）** に由来する。
細く長く、時間をかけて育てるプロジェクトである、という姿勢を込めている。

## 現在地
M4（割り込み）まで完了。
M5（マルチタスク）へ向けたページング拡張が進行中。
進捗の詳細は[docs/roadmap.md](docs/roadmap.md)を参照。

現在動くもの:

- UEFIブート（bootloaderがkernel.elfをロードして制御を渡す）
- 物理フレームアロケータ・自前ページテーブル・カーネルヒープ
- 画面コンソール（文字描画・折り返し・スクロール）とシリアルログ
- 自前のGDT/TSS/IDTと例外ハンドラ（例外時はGPRフルダンプを出して停止）
- PITによる100Hzのタイマ割り込み。カーネルは停止せず動き続け、約1秒ごとにハートビートを出す
- PS/2キーボード入力。打った文字が画面にエコーされる

## 方針（要約）
- モノリシックカーネル / メモリ保護あり / x86_64のみ / シングルコア前提
- Rust + `uefi`クレート（EDK2は使わない）
- QEMU + OVMFでテスト、WSL2上で開発
- シリアルログを最優先の観測手段とする / 異常はfail-fast（即停止 + ダンプ）
- ハードウェア依存部と純粋ロジックを分離し、後者はホストの`cargo test`で検証する
- 検証は「設定したつもりの値」ではなく実際の状態（レジスタやIMRの読み戻し）で行い、検査そのものが機能することを、わざと壊して確かめる

設計の全体像は[docs/architecture.md](docs/architecture.md)、個々の設計判断は[docs/adr/](docs/adr/)、保留した判断は解禁条件つきで[docs/deferred-decisions.md](docs/deferred-decisions.md)、長期の構想は[docs/vision.md](docs/vision.md)、実装で詰まった記録は[docs/troubleshooting.md](docs/troubleshooting.md)、「常に維持する」と書いた性質に検査があるかの一覧は[docs/verification-coverage.md](docs/verification-coverage.md)にある。

## 前提環境

WSL2（Ubuntu系）+ WSLgで開発している。
必要なもの:

- Rust（rustup経由。ツールチェインの版と`x86_64-unknown-uefi` / `x86_64-unknown-none`ターゲットは`rust-toolchain.toml`により初回ビルド時に自動導入される）
- QEMUとOVMF: `sudo apt install qemu-system-x86 ovmf`

## ビルド・実行

```
cargo xtask run
```

QEMUを`-display none`で起動し、シリアル出力を端末へ流す。
カーネルは停止せず動き続けるため、`run`は既定で120秒後にQEMUを自動停止する（`--no-limit`で解除）。

- `cargo xtask run --gui` : QEMUのウィンドウを表示する。ウィンドウにフォーカスを当ててキーを打つと、文字が画面にエコーされる
- `cargo xtask screenshot [out.png]` : 起動後の画面をキャプチャする
- `cargo test` : ホスト上で純粋ロジックの単体テストを実行する

### 静的検査

```
cargo xtask check
```

全構成のビルド、ホストテスト、clippy、`cargo fmt --check` に加え、`unsafe`ブロックが`// SAFETY:`コメントを伴っているか、コミットメッセージの文体が揃っているかを検査する。
bootloaderとkernelはターゲットが違うため`--workspace`ではまとめられず、構成ごとに実行している。
1つ落ちても途中で止めず、最後に失敗した項目をまとめて報告する。

```
cargo xtask check --full
```

上記に加えて、下記の回帰チェック14種をQEMUで順に実行する。
1種類ごとにカーネルをビルドし直して起動するため数分かかる。

### 回帰チェック

わざと異常を起こし、検出が実際に働くことを確かめるテスト群。
シリアルログとQEMUの割り込みログ（`-d int`）を突き合わせて判定する。

- `cargo xtask run --panic-test` : パニックハンドラ
- `cargo xtask run --exception-test <kind>` : 例外ハンドラ（divide-by-zero / invalid-opcode / page-fault / double-fault）
- `cargo xtask run --critical-test <kind>` : クリティカルセクション（double-lock / restore-enabled）
- `cargo xtask run --interrupt-test <kind>` : 割り込み・タイマ・キーボード（enable-only / irq-path / misaligned / timer / no-eoi / alt-offset / keyboard）

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
