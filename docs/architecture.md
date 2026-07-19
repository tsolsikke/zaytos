# ZaytOS アーキテクチャ方針

本ファイルは、現時点で確定しているアーキテクチャ・設計方針の
**「現在の結論」**を 1 箇所に集約したものである。
個別の判断に至った経緯・却下案の詳細は `adr/` 以下の各 ADR を参照。

- 詳細な経緯・却下案 → `adr/`（1 決定 = 1 ファイル）
- 要点の集約（このファイル）→ 現在確定している方針の一覧

---

## 1. プロジェクトの性格

- **主目的**: 学習。OS の基本機能を、理解しながら自作すること自体が目的。
  したがって基本的な機能（保護機構、割り込み、マルチタスク等）は省略しない。
- **開発スタイル**: 細く長く。堅牢性・理解を、機能量・速度より優先する。

---

## 2. 確定済みアーキテクチャ

| 項目 | 決定 | 対応 ADR |
|---|---|---|
| カーネル方式 | モノリシックカーネル | ADR-0001 |
| メモリ保護 | あり（ページング、ユーザー/カーネル権限分離） | ADR-0001 |
| 対象アーキテクチャ | x86_64 のみ | ADR-0006 |
| ファームウェア境界 | UEFI（`uefi` クレート） | ADR-0005 |
| コア数 | シングルコア前提（SMP は後回し） | ADR-0002 |
| デバッグ観測 | シリアルログ最優先 | ADR-0003 |
| パニック方針 | 即停止 + レジスタダンプ（fail-fast） | ADR-0004 |
| 開発言語 | Rust | ADR-0005 |
| kernel リンクアドレス | 低位固定 `0x100000`（非 PIE） | ADR-0009 |
| 物理フレーム管理 | 範囲リスト（固定長 256） | ADR-0011 |
| カーネルヒープ | 侵入型連結リスト（隣接結合あり） | ADR-0012 |
| 画面描画 | 検証済み形状に対する直接描画、`Rgb`/`Bgr` のみ | ADR-0013 |
| 割り込み | 自前 IDT（M4）まで禁止し続ける | ADR-0014 |
| MMIO のキャッシュ属性 | PCD（キャッシュ無効）。WC は保留 | ADR-0015 |
| コンソールフォント | GNU Unifont（半角 8x16 / 全角 16x16） | ADR-0016 |

---

## 3. 「堅牢さ」の定義（本プロジェクトでの立場）

堅牢さには 2 つの方向がある。

1. **防御的な堅牢さ**: 保護機構を厚くし、バグの被害を局所化する
   （Linux / Redox 型）。
2. **単純さによる堅牢さ**: 保護機構を削り、コード量と複雑度を減らす
   （TempleOS 型）。

本プロジェクトは学習目的のため **1（防御的な堅牢さ）** を採る。
保護機構は学習対象そのものであり、省略しない。
ただし規模の暴走を避けるため、モノリシック構成で全体の複雑度は抑える
（マイクロカーネルの強い分離は初期段階では採らない）。

---

## 4. 技術スタック

```
[QEMU]                      … フルシステムエミュレータ（テスト環境）
  └─ [OVMF]                 … UEFI ファームウェア（既製ビルドを導入して使用）
       └─ [bootloader crate (.efi)]  … x86_64-unknown-uefi, uefi クレートで実装
            └─ [kernel crate (ELF)]  … x86_64-unknown-none, モノリシックカーネル本体
                 （bootloader が ELF ローダー経由でロードし制御を渡す。ADR-0008）

[common crate] … bootloader / kernel の両方から使う共有ロジック
                 （serial, log, cpu。#[panic_handler] は含まない）
```

- 言語: Rust。bootloader は `x86_64-unknown-uefi`、kernel は
  `x86_64-unknown-none`（ADR-0008）。
- UEFI: `uefi` クレート（uefi-rs）。EDK2 のビルドシステムは使わない。
- ファームウェア: OVMF（`apt install ovmf` 等でパッケージ導入）
- エミュレータ: QEMU（`-serial stdio -no-reboot -no-shutdown -d int,cpu_reset`）。
  後半 3 つは常時付与する。既定の QEMU はトリプルフォルト等の致命的例外で
  無言のまま再起動を繰り返し、外から観測できなくなる。これを止めて例外と
  CPU リセット要因をログに残すためのもの。「ハングした」と判断する前に、
  まずこのログで例外の有無を確認する。
- 開発環境: WSL2（Ubuntu 系）+ WSLg
- ビルド自動化: `cargo xtask` パターン

---

## 5. `x86_64-unknown-none` ターゲットの既定値（M2-0b で確認）

kernel クレートが使う `x86_64-unknown-none` は、ビルトインターゲットとして
以下の既定値を持つ（`rustc -Z unstable-options --print target-spec-json`
で確認、Rust 1.99.0-nightly 時点）。M4（割り込みハンドラ）・M5（コンテキスト
スイッチ）で影響しうるため記録する。

| 項目 | 既定値 | 意味・影響 |
|---|---|---|
| `disable-redzone` | `true` | レッドゾーン無効。割り込みハンドラが現在のスタックをそのまま使っても、呼び出し元のレッドゾーン領域を破壊する心配がない（追加の対応不要）。 |
| `features` | `-sse,-sse2,...,-avx2,+soft-float` | SSE/AVX 全無効・ソフトウェア浮動小数点。通常のコード生成が XMM/YMM レジスタを一切使わないため、M4 の割り込みハンドラで FPU/SSE レジスタの退避・復帰は不要（今後 SSE を明示的に有効化する場合を除く）。 |
| `panic-strategy` | `abort` | unwind 情報不要。bootloader 側の判断（M1）と一致。 |
| `code-model` | `kernel`（高位負アドレス前提） | kernel を higher-half（例: `0xffffffff80000000` 付近）にリンクする前提の設定。ADR-0009 で低位アドレスを採用したため、`small` へ明示的に上書きしている（`.cargo/config.toml`）。 |
| `position-independent-executables` | `true`（PIE既定） | ADR-0009 の低位固定アドレスリンクと相性が悪いため、`relocation-model=static` で無効化している（`.cargo/config.toml`）。 |

上記の `code-model` / `relocation-model` の上書きは `.cargo/config.toml` の
`[target.x86_64-unknown-none]` セクションに集約している。

---

## 6. bootloader→kernel 引き渡し（M2-0c）に関する申し送り事項

### 6.1 ExitBootServices の呼び出し方針

`bootloader/src/loader.rs` は `unsafe { uefi::boot::exit_boot_services(...) }`
（uefi-rs 組み込みの実装）をそのまま使い、自前でリトライループを実装して
いない。理由:

- `uefi::boot::exit_boot_services` は「メモリマップ取得 →
  `ExitBootServices()` 呼び出し」を、間に他の Boot Services 呼び出しを
  一切挟まず一つの関数内で行う。マップキー不整合で失敗した場合は
  最大 2 回まで再試行し（Linux カーネルの実装と同じ方針）、それでも
  失敗した場合はコールドリセットする。
- **前提**: この安全性は「メモリマップ取得と `ExitBootServices` 呼び出しの
  間に何もアロケーションを挟まない」ことに依存している。そのため
  `BootInfo` に埋め込むメモリマップは、`exit_boot_services()` が返す
  `MemoryMapOwned` をそのまま使い、別途 `uefi::boot::memory_map()` を
  自前で呼び直してはいけない（呼び直すと、そのタイミングでアロケーション
  が発生し、それより前に取得したマップキーが古くなる可能性がある）。
  この関数は「呼ぶ側が独自にリトライや二重取得をしない」ことを前提に
  安全性を担保しているため、uefi-rs をアップデートする際は、この
  呼び出しパターン（取得と exit を分離しない）が変わっていないかを
  確認すること。
- `MemoryMapOwned` はスコープを抜けると `free_pool`（Boot Services）を
  呼ぼうとする Drop 実装を持つ。ExitBootServices 成功後にこれが走ると
  未定義動作になるため、`bootloader/src/loader.rs` では必要な値を
  `BootInfo` へコピーした直後に `core::mem::forget` で明示的にリークして
  いる（uefi-rs 側にも `are_boot_services_active()` によるガードは
  あるが、それに依存しない）。

### 6.2 メモリ種別に関する M2-c への申し送り（重要）

kernel 本体・`BootInfo`・メモリマップバッファは、いずれも
`MemoryType::LOADER_DATA` としてページ確保される（`bootloader/src/loader.rs`
の該当する `allocate_pages`/`exit_boot_services` 呼び出しで実際に指定して
いることをコードで確認済み）。**`LOADER_DATA` は一般に「OS が後で回収して
よい領域」として扱われるが、M2-c の物理フレームアロケータが素朴に
「`LOADER_DATA` は空き」と判断すると、実行中の kernel 自身・受け取った
`BootInfo`・メモリマップを空きメモリとして配ってしまう。** 症状は即座には
現れず、「しばらく動いた後に突然壊れる」という最も診断しにくい形になる。

M2-c のフレームアロケータ実装時、以下の 3 範囲は必ず予約済み扱いにする
こと。kernel 側からそれぞれ次の方法で特定できる。

| 範囲 | 特定方法 |
|---|---|
| kernel イメージ本体 | リンカスクリプト（`kernel/link.ld`）が定義する `__kernel_start`/`__kernel_end` シンボル |
| `BootInfo` 自身 | `_start` が受け取るポインタ + `common::boot_info::BOOT_INFO_PAGE_COUNT * 4096` バイト |
| メモリマップバッファ | `BootInfo.memory_map.descriptors_ptr` + `descriptors_len` |

bootloader 自身が使っていたコード・スタック領域（`EfiLoaderCode`/
`EfiLoaderData` の一部）も同様に ExitBootServices 後はメモリマップ上
「使用中」として残る。kernel がこれを再利用したい場合（bootloader の
メモリ回収）は M2-c 以降、必要になった時点で別途設計する。

**`EfiBootServicesCode`/`EfiBootServicesData` についても、当面は空きとして
扱わない（ADR-0010）。** UEFI 仕様上は ExitBootServices 後に回収してよい
とされるが、現時点の kernel はページテーブル・スタック・GDT/IDT を
いずれも UEFI 由来のまま使い続けており、これらの型を空き扱いすると
実行基盤そのものを上書きしうる。回収を解禁できるのは、自前のページ
テーブル・自前スタック・自前 GDT/IDT がすべて揃った後（M4 以降が目安）。

### 6.3 kernel のスタックについて（対応不要、認識の共有のみ）

kernel へジャンプした直後、kernel は bootloader が実行していた UEFI 由来の
スタックをそのまま使い続けている。kernel 自身専用のスタックは用意して
いない。対応時期（M2-c のページング整備時か、M4 の割り込み対応時か）は
その時点で判断する。

### 6.4 M2-d のマッピング範囲の穴について（M4 の ACPI 参照への申し送り）

M2-d (d-1) で構築したマッピング計画には、`EfiConventionalMemory` の
内部に見える領域であっても `EfiReservedMemoryType`（type=0）に該当する
部分は意図的にマップしていない（`memory_map::classify()` が `Unmapped`
と判定するため）。実機（QEMU + OVMF, RAM 256MiB）では
`0xf6ed000..0xf76d000`（512KiB）がこれに該当することを確認済み。

一方、`EfiACPIReclaimMemory` / `EfiACPIMemoryNVS` / `EfiRuntimeServicesCode`
/ `EfiRuntimeServicesData` は `classify()` により `ReservedButMapped` と
判定されるため、現時点では**すべてマップ済み**であることを実機ログで
確認している（ACPI テーブル自体を読みに行くコードはまだ存在しないが、
仮に読みに行ってもページフォルトにはならない）。

**M4 で APIC 構成のため MADT（`EfiACPIReclaimMemory` にあることが多い）を
読む場合の注意点:**

- 上記の通り ACPI Reclaim/NVS 領域自体は現状マップ済みなので、MADT の
  在り処がこれらの型の範囲内である限り、そのままではページフォルトは
  起きないはずである。
- ただし、ACPI テーブル（RSDP → XSDT/RSDT → MADT）を辿る過程で参照する
  物理アドレスが、`EfiReservedMemoryType` や UEFI メモリマップに一切
  現れない領域（ファームウェア実装依存）を指す可能性がある。その場合は
  本節の穴と同じ理由（`Unmapped` 判定）でページフォルト（`v=0e`）になる。
- 「ACPI を読もうとしたらページフォルト」となった場合は、まず
  `qemu-debug.log` の `CR2`（フォルトしたアドレス）を確認し、それが
  UEFI メモリマップ上どの型（またはマップ範囲外）に該当するかを
  `docs/troubleshooting.md` の要領で切り分けること。対応としては、
  M2-d のフレームバッファと同様に、判明した範囲を明示的な `extra` 範囲
  としてマッピング計画に追加する形になる見込み。

### 6.5 M4 まで割り込みを禁止する（M2-d 完了時点の判断）

CR3 を自前のページテーブルへ切り替えた（d-2）時点でも、IDT（割り込み
記述子テーブル）はまだ UEFI が ExitBootServices 前に設定したものを
そのまま使っている。割り込みが発生すれば、恒等マッピングにより
「たまたま到達可能なだけの、自前で検証していない」UEFI 由来のハンドラに
制御が渡りうる。

実機確認（`kernel::_start` 冒頭で RFLAGS.IF を測定）の結果、実際には
ExitBootServices 後の時点で `IF` はすでに `false`（割り込み無効）
だった。想定していた「有効なまま」という状況ではなかったが、以降も
無効であることを構造的に保証しておく価値は変わらない。ページテーブルの
回収予定（ADR-0010 で保留した `EfiBootServicesCode`/`Data` の回収）や、
今後のメモリレイアウト変更によって、この領域はいつ壊れてもおかしくない
ため。

**対応**: `kernel::_start` の冒頭（BootInfo 検証より前）で `cli` により
割り込みを禁止し、M4 で自前の IDT・例外ハンドラを導入するまでこの状態を
維持する。切り替え前後の `RFLAGS.IF` はシリアルログに残す
（`common::cpu::read_rflags`/`disable_interrupts`）。M4 で自前 IDT を
用意した後、初めて割り込みを再度有効化する。

**M4 で割り込みを有効化する際に見直すべき実装（一覧）**:
- 上記の `cli`（`kernel::_start` 冒頭）自体の要否・タイミング。
- `kernel::heap::allocator::Locked<T>` の `unsafe impl Sync`
  （ADR-0012）。「シングルコア前提かつ割り込みが常時禁止されている」
  ことに依存しており、この前提が崩れると排他性が失われる。cli/sti の
  保存・復元、またはスピンロックへの差し替えが必要。
- 画面コンソール（M3-c）をグローバル状態で持つ場合、同じ前提への依存が
  もう 1 つ増える（ADR-0013）。あわせて、パニック時の画面出力を解禁する
  かどうかもこの時点で再検討する。

### 6.6 メモリ管理の確定構成（M2 完了時点）

M2 で実装したメモリ管理の現在の姿。経緯は ADR-0010〜0012 を参照。

**物理フレームアロケータ**（`kernel/src/frame_allocator.rs`, ADR-0011）
- データ構造は範囲リスト（`(start_frame, frame_count)` のソート済み・
  隣接結合済み固定長配列、容量 256）。`dealloc` と隣接結合に対応。
- 「空き」と判定するのは `EfiConventionalMemory` のみ。物理アドレス 0 を
  含むページは型に関わらず除外する。
- 容量超過は握りつぶさず `CapacityExceeded` として fail-fast。
- 連続フレームの確保は `allocate_contiguous`。ヒープ本体やフレーム
  バッファ用の大きなバッファはこれを直接使い、ヒープを経由しない。

**ページテーブル**（`kernel/src/paging/`）
- 恒等マッピング。2MiB ページを核とし、境界の端数のみ 4KiB に分解する。
- フレームバッファと MMIO は PCD（キャッシュ無効）でマップする。
  Write-Combining は採用していない。
- 「空き」判定とマッピング対象判定は `memory_map::classify()` の一箇所に
  集約し、アロケータとページング計画が同じ基準を見るようにしている。
  アロケータの空き範囲が全てマップ済みであることは実行時に検証する。
- 新規に確保したページテーブル用フレームは必ずゼロ埋めする。NX は
  立てない。PS ビットは PD レベルのみ。
- CR3 は M2-d (d-2) で自前のテーブルへ切り替え済み。UEFI 由来の旧
  ページテーブルは回収していない。

**カーネルヒープ**（`kernel/src/heap/`, ADR-0012）
- 起動時に `allocate_contiguous` で 1MiB（256 フレーム）を一度だけ確保
  する固定サイズ。拡張機構は持たない。
- 侵入型連結リスト + 隣接結合。配置・分割・結合の判断ロジックは
  `heap::plan` に分離してホスト `cargo test` で検証し、実ポインタを
  触る部分だけ `heap::allocator` に閉じ込めている。
- ヘッダにマジック値と要求サイズを持たせ、`dealloc` 時に検証する。
  不一致は二重解放・破壊としてパニックする。解放ブロックは毒値
  `0xDE` で埋める（`heap-poison` feature、既定有効）。
- 確保失敗時は診断情報をログに出してから null を返し、既定の
  alloc error handler 経由で既存のパニックハンドラに合流する。

---

## 7. 同期・並行性方針（シングルコア前提）

- 共有データ保護は割り込み禁止（`cli`/`sti`）によるクリティカルセクション。
- 割り込みハンドラが触る共有データは、シングルコアでもクリティカルセクションで守る。
- SMP 対応時に中身をスピンロック/アトミックへ差し替えられるよう、
  呼び出し側インターフェースだけ安定させ、中身は最小限にとどめる。
- SMP 用の抽象化を現段階で先回りして作らない。

---

## 8. テスト戦略

- ハードウェア依存部（IDT、ページテーブル、MMIO）と純粋ロジック
  （スケジューラ判断、アロケータのビット演算、データ構造）を分離する。
- 純粋ロジックはホスト上で `cargo test` により検証する。
- QEMU 起動を伴う統合的な確認と、ホスト単体テストを併用し、
  検証を「QEMU 目視」だけに依存させない。

---

## 9. スコープ外（現段階では着手しない）

- マルチコア（SMP）
- x86_64 以外への移植・抽象化
- ローダブルカーネルモジュール
- 独自ファイルシステム（当面は FAT 等の読み取りから）
- ネットワークスタック
