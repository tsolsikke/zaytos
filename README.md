# ZaytOS

x86_64 向けの、学習目的の自作 OS。Rust 製・モノリシック・保護機構あり。

## 名前について
ZaytOS（ゼイトス）の "Zayt" は、ドイツ語の **Zeit（時間）** に由来する。
細く長く、時間をかけて育てるプロジェクトである、という姿勢を込めている。

## 現在地
M4（割り込み）まで完了。M5（マルチタスク）へ向けたページング拡張が進行中。
進捗の詳細は [docs/roadmap.md](docs/roadmap.md) を参照。

現在動くもの:

- UEFI ブート（bootloader が kernel.elf をロードして制御を渡す）
- 物理フレームアロケータ・自前ページテーブル・カーネルヒープ
- 画面コンソール（文字描画・折り返し・スクロール）とシリアルログ
- 自前の GDT/TSS/IDT と例外ハンドラ（例外時は GPR フルダンプを出して停止）
- PIT による 100Hz のタイマ割り込み。カーネルは停止せず動き続け、
  約 1 秒ごとにハートビートを出す
- PS/2 キーボード入力。打った文字が画面にエコーされる

## 方針（要約）
- モノリシックカーネル / メモリ保護あり / x86_64 のみ / シングルコア前提
- Rust + `uefi` クレート（EDK2 は使わない）
- QEMU + OVMF でテスト、WSL2 上で開発
- シリアルログを最優先の観測手段とする / 異常は fail-fast（即停止 + ダンプ）
- ハードウェア依存部と純粋ロジックを分離し、後者はホストの `cargo test` で
  検証する
- 検証は「設定したつもりの値」ではなく実際の状態（レジスタや IMR の
  読み戻し）で行い、検査そのものが機能することを、わざと壊して確かめる

設計の全体像は [docs/architecture.md](docs/architecture.md)、個々の設計判断は
[docs/adr/](docs/adr/)、保留した判断は解禁条件つきで
[docs/deferred-decisions.md](docs/deferred-decisions.md)、長期の構想は
[docs/vision.md](docs/vision.md)、実装で詰まった記録は
[docs/troubleshooting.md](docs/troubleshooting.md) にある。

## 前提環境

WSL2（Ubuntu 系）+ WSLg で開発している。必要なもの:

- Rust（rustup 経由。ツールチェインの版と `x86_64-unknown-uefi` /
  `x86_64-unknown-none` ターゲットは `rust-toolchain.toml` により初回
  ビルド時に自動導入される）
- QEMU と OVMF: `sudo apt install qemu-system-x86 ovmf`

## ビルド・実行

```
cargo xtask run
```

QEMU を `-display none` で起動し、シリアル出力を端末へ流す。カーネルは
停止せず動き続けるため、`run` は既定で 120 秒後に QEMU を自動停止する
（`--no-limit` で解除）。

- `cargo xtask run --gui` : QEMU のウィンドウを表示する。ウィンドウに
  フォーカスを当ててキーを打つと、文字が画面にエコーされる
- `cargo xtask screenshot [out.png]` : 起動後の画面をキャプチャする
- `cargo test` : ホスト上で純粋ロジックの単体テストを実行する

### 回帰チェック

わざと異常を起こし、検出が実際に働くことを確かめるテスト群。シリアルログと
QEMU の割り込みログ（`-d int`）を突き合わせて判定する。

- `cargo xtask run --panic-test` : パニックハンドラ
- `cargo xtask run --exception-test <kind>` : 例外ハンドラ
  （divide-by-zero / invalid-opcode / page-fault / double-fault）
- `cargo xtask run --critical-test <kind>` : クリティカルセクション
  （double-lock / restore-enabled）
- `cargo xtask run --interrupt-test <kind>` : 割り込み・タイマ・キーボード
  （enable-only / irq-path / misaligned / timer / no-eoi / alt-offset /
  keyboard）

## 同梱している第三者のコンポーネント

| コンポーネント | 用途 | ライセンス |
|---|---|---|
| [GNU Unifont](third_party/unifont/) | コンソールフォント | SIL OFL 1.1 |

GNU Unifont のグリフデータは SIL Open Font License 1.1 と GNU GPL v2 以降
（フォント埋め込み例外つき）のデュアルライセンスで、ZaytOS は OFL 1.1 の
条件で利用している。日本語漢字グリフの元になっている jiskan16 由来の部分は
パブリックドメイン。出典・ライセンス全文・収録範囲の広げ方は
[third_party/unifont/](third_party/unifont/) を参照。

## ライセンス

MIT License（[LICENSE](LICENSE) を参照）。ただし `third_party/` 以下は
上記のとおり各コンポーネント自身のライセンスに従う。
