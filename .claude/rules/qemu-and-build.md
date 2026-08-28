---
description: QEMU の沈黙の失敗を可視化する引数と、ビルド環境の再現性。xtask を読むときに入る
paths:
  - "xtask/**/*.rs"
  - "Cargo.toml"
  - "rust-toolchain.toml"
---

## QEMU の「沈黙の失敗」を必ず可視化する

**本体は `docs/coding-standards.md` の「QEMU の「沈黙の失敗」を必ず可視化する」にある**
（`ADR-0048`）。

---

## ビルド環境の再現性

- `rust-toolchain.toml` で Rust バージョン（チャンネル）を固定する。
- `Cargo.lock` はリポジトリにコミットする。
- 「バージョン差による謎の失敗」（EDK2 で経験した事象）を構造的に排除するのが目的。
- **ファームウェアと対象トリプルは `docs/architecture.md` の「技術スタック」にある。**
  OVMF の導入方法と EDK2 に触れないことは、そちらが本体である。
