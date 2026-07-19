# ZaytOS

x86_64 向けの、学習目的の自作 OS。Rust 製・モノリシック・保護機構あり。

## 名前について
ZaytOS（ゼイトス）の "Zayt" は、ドイツ語の **Zeit（時間）** に由来する。
細く長く、時間をかけて育てるプロジェクトである、という姿勢を込めている。

## 現在地
M0（環境構築）完了。次は M1（UEFI Hello World）。進捗は [docs/roadmap.md](docs/roadmap.md) を参照。

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

現段階（M0）ではまだブートローダー / カーネルが存在しないため、
`cargo xtask run` は OVMF ファームウェアのみを起動する（ブート可能な
デバイスがない状態で待機する）。実際に何かが起動するのは M1 から。

## ライセンス
（未定）
