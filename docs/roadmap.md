# ZaytOS ロードマップ

マイルストーンの現在地を管理する。細く長く進める前提のため、
各段階は「ここで中断しても死なない」区切りになるよう意識する。

状態: [ ] 未着手 / [~] 進行中 / [x] 完了

---

## M0. 環境構築 [x]
- [x] WSL2 + Rust ツールチェイン（`x86_64-unknown-uefi` ターゲット追加）
- [x] QEMU + OVMF 導入
- [x] `cargo xtask run` で QEMU が起動する骨組み
- [x] `rust-toolchain.toml` / `Cargo.lock` 固定

## M1. UEFI Hello World [x]
- [x] **シリアルログ出力（COM1）を最優先で確立**（`common/src/serial.rs`, `common/src/log.rs`）
- [x] パニックハンドラ雛形（`bootloader/src/panic.rs`; RSP + `PanicInfo` ダンプ +
  halt。GPR フルダンプは ADR-0004 Addendum の通り M4 に先送り）
- [x] UEFI アプリとして起動し、画面に文字列を出力（`bootloader/src/main.rs`）
- [x] パニックハンドラの回帰チェックを常設化（`cargo xtask run --panic-test`）

## M2. メモリ管理の基礎 [x]

M2-0a〜M2-0c で bootloader/kernel 分離（ADR-0008）と ELF ローダーを実装した
結果、当初の「UEFI メモリマップ取得」「ExitBootServices」は M2-0c の中で
実質的に完了している。以降は M2-c（自前ページテーブル構築）・M2-d
（物理フレームアロケータ）の**実装順序を入れ替えている**: ページテーブルを
構築するにはページテーブル自身を置く物理フレームが必要であり、それを
供給するのがフレームアロケータであるため、フレームアロケータを先に
実装しなければ成立しない。

- [x] M2-0a: crate 再編（`kernel` → `bootloader` へ改名、`serial`/`log`/`cpu`
  を `common` クレートへ共有化、ADR-0008）
- [x] M2-0b: kernel クレート新規作成（`x86_64-unknown-none` + リンカスクリプト、
  最小カーネル。リンクアドレスは ADR-0009）
- [x] M2-0c: ELF ローダー実装（bootloader が kernel.elf をロードし、GOP
  フレームバッファ情報取得 → ExitBootServices → BootInfo 経由で kernel へ
  ジャンプ。UEFI メモリマップ取得・ExitBootServices はここに含まれる）
- [x] M2-a: UEFI メモリマップ取得（M2-0c で完了。`BootInfo.memory_map` 経由で
  kernel へ引き渡し、内容をシリアルへログ出力済み）
- [x] M2-b: ExitBootServices 実行（M2-0c で完了。以降もシリアルログが生存
  することを確認済み）
- [x] M2-c: 物理フレームアロケータ（`BootInfo` のメモリマップを解析し、
  空き物理フレームを管理。範囲リスト方式、ADR-0011）。「空き」の判定は
  `EfiConventionalMemory` のみとし、`EfiBootServicesCode`/`Data` は
  ADR-0010 により当面除外。kernel 本体・`BootInfo`・メモリマップバッファ
  は予約済みとして除外する。ロジックはホスト `cargo test` で検証済み
- [x] M2-d: 自前ページテーブル構築（M2-c のフレームアロケータから物理
  フレームを受け取って構築する）。d-1 でCR3を切り替えずに新テーブルを
  構築・検証し、d-2 で実際にCR3を切り替えて実機確認済み（マッピングの
  穴に関する申し送りは §6.4 参照）
- [x] M2-e: カーネルヒープ（`alloc` クレート有効化）。連結リスト
  アロケータ（ADR-0012）。`Vec`/`Box`/`String` の実機動作、二重解放
  検出、ヒープ枯渇時のパニック合流を確認済み

### 将来の検討項目（今は着手しない）
- **BootServices 領域の回収**（ADR-0010）: `EfiBootServicesCode`/`Data` を
  空き物理メモリとして回収することは、自前のページテーブル・スタック・
  GDT/IDT がすべて揃うまで（M4 以降が目安）保留する。

## M3. 画面描画
- [ ] GOP（Graphics Output Protocol）でフレームバッファ取得
  （情報自体は M2-0c で `BootInfo.framebuffer` として取得・kernel へ
  引き渡し済み。ここでの残作業は kernel 側でそれを使った実際の描画）
- [ ] ピクセル描画・フォント描画
- [ ] コンソール抽象（文字出力を画面にも出す）

## M4. 割り込み
- [ ] GDT / TSS 設定
- [ ] IDT 設定・例外ハンドラ
- [ ] タイマ割り込み（PIT / APIC タイマ）。APIC 構成のため ACPI（MADT）を
  読む際は `docs/architecture.md` §6.4 の申し送り（マップされていない
  領域を踏む可能性）を必ず確認する
- [ ] キーボード割り込み
- [ ] クリティカルセクション（`cli`/`sti`）の実装

## M5. マルチタスク（プリエンプティブ）
- [ ] タスク構造・コンテキストスイッチ
- [ ] スケジューラ（シングルコア・実行キュー 1 本）
- [ ] ユーザー/カーネル権限分離の実運用

## M6. ファイルシステム / ユーザーランド
- [ ] FAT など既存の単純な形式の読み取り
- [ ] 簡易的なユーザープログラムのロードと実行

---

## 中断可能な区切り
- **M1 到達**で「何か画面に映る + ログが取れる」状態になり、
  他プロジェクトにリソースが移っても死なないプロジェクトになる。
- 以降は各 M 単位で区切り、到達ごとに ADR とロードマップを更新する。

## スコープ外（ロードマップに載せない）
SMP / 他アーキテクチャ移植 / 動的モジュール / 独自 FS / ネットワーク。
着手する場合はまず ADR で方針を決めてから。
