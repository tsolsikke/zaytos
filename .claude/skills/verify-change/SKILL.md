---
name: verify-change
description: 変更を検査するときの標準の手順（build・test・clippy・fmt・QEMU・serial ログ）。実装ループを回すときに使う
---

## 4. 実装ループの標準手順

Claude Code / Codex は、次のループを回す。可能な範囲で自動実行してよいが、
§5 の「レビュー必須」条件に該当する場合はループを止めて人間に渡す。

1. 変更対象と意図を明確化する（何を、なぜ）。
2. コードを実装する（§8 のコーディング規約に従う）。
3. **`cargo xtask check` を実行する。** 全構成のビルド・ホストテスト・
   clippy・`cargo fmt --check` が 1 コマンドで走る。内訳は次のとおり。
   - build: bootloader（`x86_64-unknown-uefi`）、kernel（`x86_64-unknown-none`）、
     common（ホスト）、xtask（ホスト）の 4 構成
   - test: `cargo test --workspace`
   - clippy: 上記 4 構成すべてに `-D warnings`
   - fmt: `cargo fmt --all -- --check`
4. `cargo xtask run` 等で QEMU 起動 → serial ログを取得。
5. ログを読み、期待挙動と一致するか自己診断する。
6. 失敗時: 例外ログ（§3.3）を確認し、原因を推論して修正、3 に戻る。
7. 成功時: 変更内容を要約し、必要なら ADR / troubleshooting / roadmap を更新。
8. ドキュメントを機械的に書き換えた場合は `python3 tools/docstyle.py` も
   通す（`docs/coding-standards.md` の「機械的に適用するときの例外」を
   参照）。毎回は要らない。

**`cargo xtask check --full` を走らせる時機は `docs/coding-standards.md` の
「回帰チェックの必須条件」にある。** 本体はそちらで、ここに複製しない。
毎ループには重い（QEMU を多数回起動する）が、段階の完了時と、`unsafe` /
割り込み / ページテーブル / GDT / IDT に触れた変更のコミット前は必須である。

**低レイヤのバグは実行するまで顕在化しないことが多い。**
ビルドが通ったことをもって正しさの証明としないこと。必ず QEMU 実行と
serial ログでの確認までを 1 ループとする。

**手順 3 を飛ばさない。** M3-b から M5-a-1 までの 2 日間、このループに
fmt と clippy が無かったため、整形差分が 52 箇所、clippy 警告が 10 件まで
誰にも気づかれずに積み上がった。CI は無いので、ここを飛ばすと検出機構が
どこにも無くなる。個々の見落としではなく手順の欠落が原因だったので、
手順として明記する。

---
