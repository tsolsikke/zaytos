# ZaytOS

x86_64 向けの、学習目的の自作 OS。Rust 製・モノリシック・保護機構あり。

## 名前について
ZaytOS（ゼイトス）の "Zayt" は、ドイツ語の **Zeit（時間）** に由来する。
細く長く、時間をかけて育てるプロジェクトである、という姿勢を込めている。

## 現在地
M2（メモリ管理の基礎）まで完了。kernel は自前のページテーブル上で動作し、
物理フレームアロケータとカーネルヒープが使える。次は M3（画面描画）。
進捗は [docs/roadmap.md](docs/roadmap.md) を参照。

## 方針（要約）
- モノリシックカーネル / メモリ保護あり / x86_64 のみ / シングルコア前提
- Rust + `uefi` クレート（EDK2 は使わない）
- QEMU + OVMF でテスト、WSL2 上で開発
- シリアルログを最優先の観測手段とする / パニック時は即停止 + レジスタダンプ

詳細は [docs/architecture.md](docs/architecture.md) と
[docs/adr/](docs/adr/)（設計判断の記録）を参照。

## ビルド・実行

```
cargo xtask run
```

`rust-toolchain.toml` によりツールチェイン（バージョン固定）と
`x86_64-unknown-uefi` ターゲットは自動的に導入される。QEMU と OVMF は
別途 `apt install qemu-system-x86 ovmf` で導入しておくこと。

`cargo xtask run` は QEMU を `-display none` で起動し、シリアル出力を
端末へ流す。画面を出す場合は `--gui`、パニック経路の回帰確認は
`--panic-test`、画面のキャプチャは `cargo xtask screenshot`。

## 同梱している第三者のデータ
コンソールフォントとして GNU Unifont のビットマップグリフを使っている
（SIL Open Font License 1.1）。出典・ライセンス全文・収録範囲の広げ方は
[third_party/unifont/](third_party/unifont/) を参照。

## ライセンス
（未定。決める際は、OFL のフォントデータを同梱している点を前提にすること）
