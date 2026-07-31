//! IDT と例外ハンドラ（M4-b-1）。
//!
//! **unsafe を含む。** IDT を構築して `lidt` でロードし、例外の入口となる
//! アセンブリスタブを定義する。
//!
//! - [`layout`][mod@layout]: エントリの符号化（純粋ロジック、ホスト
//!   `cargo test` で検証）。
//! - このモジュール: 実体の静的確保、スタブ、`lidt` / `sidt`。
//!
//! ## ゲート種別と DPL
//!
//! すべて**割り込みゲート（type 0xE）・DPL 0**にする。トラップゲート
//! （0xF）は入場時に `IF` をクリアしないため、「ハンドラ入場時点で IF=0」
//! という前提（ADR-0018）が崩れる。
//!
//! ## 256 ベクタすべてを埋める
//!
//! 現在は割り込み禁止中なので 0..=31 で足りるが、M4-d で `sti` した後に
//! スプリアス割り込み（ベクタ 0x27 / 0x2F）など想定外のベクタが届きうる。
//! 空のままだと #NP になり、しかも #NP のハンドラも無ければ即座に落ちる。
//! 全 256 に入れておけば「予期しないベクタが来た」と報告できる。コストは
//! テーブル 4KiB とスタブ 4KiB だけである。
//!
//! ## NMI（ベクタ 2）について
//!
//! NMI は `cli` でマスクできない。「ハンドラ入場時点で IF=0」という前提は
//! 割り込みゲートが `IF` をクリアすることによるもので、NMI の到達自体は
//! 防げない。したがって NMI ハンドラも、他の例外ハンドラと同じく
//! **ロックも確保も使わない**（シリアルへ直接書いて停止する）。
//!
//! ## スタブを 2 種類用意する理由
//!
//! CPU がエラーコードを積む例外（#DF, #TS, #NP, #SS, #GP, #PF, #AC, #CP）と
//! 積まない例外があり、スタックのレイアウトが 8 バイトずれる。積まない側は
//! ダミーのエラーコードを push して揃え、共通ハンドラからは同じレイアウトに
//! 見えるようにする。

pub mod context;
pub mod decode;
pub mod layout;

use core::fmt::Write as _;
use core::ptr::addr_of;
use core::sync::atomic::{AtomicU64, Ordering};

use common::cpu;
use common::percpu::{PerCpu, MAX_CPUS};
use common::serial::SerialPort;

use crate::gdt::KERNEL_CODE_SELECTOR;
use context::{ExceptionContext, IrqContext};
use decode::{error_code_kind, ErrorCodeKind, PageFaultErrorCode, SelectorErrorCode};
use layout::{exception_name, GateType, IdtEntry};

/// IDT のエントリ数。CPU が定義する 0..=31 と、それ以外も含めて全部埋める。
pub const IDT_ENTRY_COUNT: usize = 256;

/// スタブ 1 個あたりのバイト数。`.p2align 4` で 16 バイト境界に並べている
/// ため、`n` 番目のスタブは `表の先頭 + n * STUB_SIZE` にある。
const STUB_SIZE: usize = 16;

static mut IDT: [IdtEntry; IDT_ENTRY_COUNT] = [IdtEntry::missing(); IDT_ENTRY_COUNT];

// 例外の入口となるスタブ表を生成する。
//
// 各スタブは 16 バイト境界に置き、
//   - エラーコードを積まない例外: ダミーの 0 と ベクタ番号を push
//   - エラーコードを積む例外:     ベクタ番号だけを push
// してから共通ルーチンへ飛ぶ。どちらの場合も、共通ルーチンから見た
// スタックは [ベクタ][エラーコード][RIP][CS][RFLAGS][RSP][SS] になる。
//
// エラーコードを積む例外の一覧は Intel SDM Vol.3A の
// 「Table 6-1. Protected-Mode Exceptions and Interrupts」の Error Code 列に
// 対応する。#DF(8), #TS(10), #NP(11), #SS(12), #GP(13), #PF(14), #AC(17),
// #CP(21), #HV(29), #VC(30) の 10 個。29 / 30 は AMD 由来だが、積む側に
// 入れておくのが安全側（積まない例外を積む側に分類すると、ベクタ番号を
// エラーコードとして読み、スタックが 8 バイトずれる）。
// この一覧は layout::pushes_error_code と一致していなければならず、
// ホストテストで固定してある。
//
// `.p2align 4` は各繰り返しの**末尾**に置く。先頭に置くと最後のスタブが
// 埋められず、末尾ラベルまでの距離が 256 * 16 にならないため、下の
// 自動検証が働かなくなる。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zaytos_exception_stubs",
    "zaytos_exception_stubs:",
    ".set stub_vector, 0",
    ".rept 256",
    // 刻み幅の検証に使う独立したラベル。アセンブラが算出するので、
    // Rust 側の base + n * STUB_SIZE という計算とは独立している。
    "  .if stub_vector == 8",
    "    .globl zaytos_exception_stub_8",
    "    zaytos_exception_stub_8:",
    "  .endif",
    "  .if stub_vector == 14",
    "    .globl zaytos_exception_stub_14",
    "    zaytos_exception_stub_14:",
    "  .endif",
    "  .if stub_vector == 255",
    "    .globl zaytos_exception_stub_255",
    "    zaytos_exception_stub_255:",
    "  .endif",
    "  .if (stub_vector == 8) || (stub_vector == 10) || (stub_vector == 11) || (stub_vector == 12) || (stub_vector == 13) || (stub_vector == 14) || (stub_vector == 17) || (stub_vector == 21) || (stub_vector == 29) || (stub_vector == 30)",
    "    push stub_vector",
    "  .else",
    "    push 0",
    "    push stub_vector",
    "  .endif",
    "  jmp zaytos_exception_common",
    "  .set stub_vector, stub_vector + 1",
    "  .p2align 4",
    ".endr",
    // 表の終端。ここまでの距離が 256 * STUB_SIZE であることを実行時に検証する。
    ".globl zaytos_exception_stubs_end",
    "zaytos_exception_stubs_end:",
    ".p2align 4",
    "zaytos_exception_common:",
    // ここに来た時点のスタック:
    //   [rsp]=ベクタ, +8=エラーコード, +16=RIP, +24=CS, +32=RFLAGS, +40=RSP, +48=SS
    //
    // 汎用レジスタを退避する。**push の順序は
    // idt::context::ExceptionContext のフィールド順と一対一で対応している。**
    // 後に push したものほど低いアドレスに来るので、r15 から始めて rax で
    // 終える（構造体では rax がレジスタ群の先頭になる）。
    "  push r15",
    "  push r14",
    "  push r13",
    "  push r12",
    "  push r11",
    "  push r10",
    "  push r9",
    "  push r8",
    "  push rbp",
    "  push rdi",
    "  push rsi",
    "  push rdx",
    "  push rcx",
    "  push rbx",
    "  push rax",
    // CR2 を読んで積む。**必ずここで読む。** ハンドラ内で別のページ
    // フォルトが起きると CR2 は上書きされるため、他のメモリアクセスより
    // 前に取る必要がある。rax は既に退避済みなので、作業用に使ってよい。
    "  mov rax, cr2",
    "  push rax",
    // ここで rsp が ExceptionContext の先頭を指している。
    "  mov rdi, rsp",
    // SysV ABI は call の直前に RSP が 16 バイト境界であることを要求する
    // （ADR-0018 の罠 12）。導出は下の STACK_ALIGN_ADJUST のコメント参照。
    "  sub rsp, {adjust}",
    // **実測**: 調整後の RSP そのものを第 2 引数として渡す。手計算の再現
    // ではなくレジスタの実値を渡すので、計算が間違っていれば handler 側の
    // 検証で捕まる。
    "  mov rsi, rsp",
    "  call {handler}",
    // handler は戻らない契約。万一戻ってきたら未定義命令で止める。
    "  ud2",
    handler = sym exception_entry,
    adjust = const STACK_ALIGN_ADJUST,
);

/// `call` の直前に RSP から引いて 16 バイト境界へ合わせる量。
///
/// # 導出（両経路に共通、単位はバイト、剰余は mod 16）
///
/// 長モードでは、割り込み・例外の配送時に **CPU が RSP を 16 バイト境界へ
/// 揃えてから**スタックフレームを積む（Intel SDM Vol.3A 6.14.2）。したがって
/// 基準点は必ず `RSP ≡ 0` である。そこから積まれる量で入場時の剰余が決まる。
///
/// | 経路 | CPU が積む | 入場時 | スタブが積む | 共通ルーチンが積む | call 直前 |
/// |---|---|---|---|---|---|
/// | 例外（エラーコードなし） | 5 個 = 40 → ≡ 8 | **8** | ダミー EC + ベクタ = 16 | GPR 15 + CR2 = 128 | 8 |
/// | 例外（エラーコードあり） | 6 個 = 48 → ≡ 0 | **0** | ベクタのみ = 8 | GPR 15 + CR2 = 128 | 8 |
/// | IRQ | 5 個 = 40 → ≡ 8 | **8** | ベクタのみ = 8 | GPR 15 = 120 | 8 |
///
/// **入場時の剰余は一定ではない。** エラーコードを積む例外だけ `≡ 0` で、
/// 他は `≡ 8` である。エラーコードなしの例外でダミーを push しているのは
/// `ExceptionContext` のレイアウトを揃えるためだが、結果として**剰余も
/// 揃える**働きをしている（`8 - 16 ≡ 8`、`0 - 8 ≡ 8`）。
///
/// 3 経路とも `call` 直前が `≡ 8` になるので、8 引いて `≡ 0` にする。
/// SysV ABI が要求するのは `call` **実行時点**で `RSP ≡ 0` であることで、
/// `call` が戻りアドレスを積んだ後の関数入口では `RSP ≡ 8` になる。
///
/// **「エラーコードの有無が調整の要否を分ける」ではない。** 決めるのは
/// 「入場時の剰余 − 積んだ総量」であり、たまたま 3 経路とも同じ結論に
/// なっている。IRQ 側で GPR 15 個 + ベクタ = 128 バイト（16 の倍数）だから
/// 調整不要、と考えるのは誤りである。入場時が `≡ 0` でないためこれは成立
/// しない。
///
/// この導出が正しいことは手計算に頼らず、ハンドラ入口で実測した RSP を
/// 検証している（[`check_stack_alignment`]）。
#[cfg(not(feature = "misalign-test"))]
const STACK_ALIGN_ADJUST: usize = 8;

/// 境界検証がほんとうに働くかを確かめるための、意図的に壊した値
/// （`--interrupt-test misaligned`）。
#[cfg(feature = "misalign-test")]
const STACK_ALIGN_ADJUST: usize = 0;

// IRQ スタイル（GPR を復元して `iretq` で戻る）のスタブ表。
//
// 0x20-0x3F の 32 本が PIC の IRQ が届きうる範囲、末尾の 1 本（0x40）は
// **テスト専用**で PIC の範囲外にある。
//
// **32 本ある理由は、PIC のベクタオフセットが 1 つに固定されていないため
// である。** 通常は 0x20-0x2F だが、`alt-offset-test` は 0x30-0x3F へ
// 再マップする。スタブ表が 0x20 から 17 本しか無いと、再マップ時に
// IRQ1 以降（0x31-0x3F）が例外スタブを指したままになり、最初の
// キーボード割り込みで「unexpected vector」として停止する。実際に
// M4-e から M5-a-1 までこの状態だった（troubleshooting.md 参照）。
// 取りうるオフセットの両方を最初から覆っておけば、この穴は生じない。
//
// テスト専用ベクタを PIC の範囲外へ置いているのは、
// GPR 復元の検証（`int` によるソフトウェア割り込み）に EOI の論理を
// 一切絡ませないためである。PIC 経由で配送されないベクタなら、EOI を
// 送らないことがそのまま正しい実装になる。
//
// 本番ハンドラ側に「ソフトウェア割り込み由来か」を判別する分岐を入れる案は
// 採らなかった。判別に失敗すれば本物の割り込みへ EOI を送らない側へ倒れ、
// 以降の割り込みが全部止まる。テストのために本番経路の信頼性を下げることに
// なる（ADR-0018 Addendum 3）。
//
// **例外用スタブを流用しない。** 例外ハンドラは戻らないため GPR を退避
// するだけで済むが、IRQ は中断した処理へ**戻る**ので、退避したものを
// 必ず復元しなければならない。復元を忘れると、割り込まれた側のレジスタが
// 静かに壊れる。症状は「割り込みと無関係な場所で不定期に落ちる」形になり、
// このプロジェクトで最も診断しにくい部類である。
//
// 経路を分けているので、例外側を触っても IRQ 側の復元は壊れない。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zaytos_irq_stubs",
    "zaytos_irq_stubs:",
    ".set irq_index, 0",
    ".rept 33",
    // スタブ表の刻み幅を独立に検証するためのラベル（例外側と同じ発想）。
    "  .if irq_index == 0",
    "    .globl zaytos_irq_stub_0",
    "    zaytos_irq_stub_0:",
    "  .endif",
    "  .if irq_index == 15",
    "    .globl zaytos_irq_stub_15",
    "    zaytos_irq_stub_15:",
    "  .endif",
    "  .if irq_index == 16",
    "    .globl zaytos_irq_stub_16",
    "    zaytos_irq_stub_16:",
    "  .endif",
    "  .if irq_index == 31",
    "    .globl zaytos_irq_stub_31",
    "    zaytos_irq_stub_31:",
    "  .endif",
    "  .if irq_index == 32",
    "    .globl zaytos_irq_stub_32",
    "    zaytos_irq_stub_32:",
    "  .endif",
    // IRQ にエラーコードは無い。ベクタ番号だけを積む。
    "  push irq_index + 0x20",
    "  jmp zaytos_irq_common",
    "  .set irq_index, irq_index + 1",
    "  .p2align 4",
    ".endr",
    ".globl zaytos_irq_stubs_end",
    "zaytos_irq_stubs_end:",
    ".p2align 4",
    // Local APIC のスプリアス割り込み用スタブ（S2-d-1）。**表の外に置く。**
    //
    // 表は `0x20` から 33 本の連続範囲しか覆っておらず、スプリアスの
    // `0xFF`（`crate::apic::SPURIOUS_VECTOR`）は範囲外である。`zaytos_yield_stub` と
    // `zaytos_syscall_stub` が同じ形の前例で、非連続のベクタには専用スタブを置いて
    // `zaytos_irq_common` へ合流させる。
    //
    // **`push 0xff` の符号拡張に注意が要る。** `push imm8` は 64 ビットへ符号拡張
    // されるので、`0xff` を imm8 で積むと `-1` になる。表の中のベクタ（`0x20`-`0x40`）は
    // どれも `0x80` 未満なので、この問題は今まで現れなかった。**アセンブラが
    // imm32 を選ぶことに依存しない**よう、符号なしで安全な形を明示する。
    // 値が正しいことはビルド後に逆アセンブルで確かめる（`verification-coverage.md`）。
    ".globl zaytos_spurious_stub",
    "zaytos_spurious_stub:",
    "  .byte 0x68, 0xff, 0x00, 0x00, 0x00",
    "  jmp zaytos_irq_common",
    ".p2align 4",
    // I/O APIC 経由のキーボード用スタブ（S2-d-1c）。**表の外に置く。**
    //
    // `0x42` は表（`0x20` から 33 本）の範囲外である。スプリアスと同じ形で、
    // 専用スタブを置いて `zaytos_irq_common` へ合流させる。
    //
    // **バイトを明示するのはスプリアスと揃えるためである。** `0x42` は
    // `0x80` 未満なので `push imm8` でも符号拡張の問題は起きないが、
    // 書き方を揃えておけば「どちらの形だったか」を毎回考えずに済む。
    ".globl zaytos_ioapic_keyboard_stub",
    "zaytos_ioapic_keyboard_stub:",
    "  .byte 0x68, 0x42, 0x00, 0x00, 0x00",
    "  jmp zaytos_irq_common",
    ".p2align 4",
    // Local APIC タイマ用スタブ（S2-d-2）。**表の外に置く。**
    //
    // **`0xFE` は `0x80` 以上なので、`push imm8` だと符号拡張されて `-2` に
    // なる。** スプリアスの `0xFF` と同じ罠で、バイトを明示して imm32 を
    // 固定する。値が正しいことはビルド後に逆アセンブルで確かめる。
    ".globl zaytos_lapic_timer_stub",
    "zaytos_lapic_timer_stub:",
    "  .byte 0x68, 0xfe, 0x00, 0x00, 0x00",
    "  jmp zaytos_irq_common",
    ".p2align 4",
    "zaytos_irq_common:",
    // 入場時のスタック: [rsp]=ベクタ, +8=RIP, +16=CS, +24=RFLAGS, +32=RSP, +40=SS
    //
    // GPR を退避する。順序は IrqContext のフィールド順と一対一。
    "  push r15",
    "  push r14",
    "  push r13",
    "  push r12",
    "  push r11",
    "  push r10",
    "  push r9",
    "  push r8",
    "  push rbp",
    "  push rdi",
    "  push rsi",
    "  push rdx",
    "  push rcx",
    "  push rbx",
    "  push rax",
    // CR2 は積まない。IRQ はページフォルトではないので意味を持たない。
    "  mov rdi, rsp",
    "  sub rsp, {adjust}",
    "  mov rsi, rsp",
    "  call {handler}",
    // --- ここから復帰 ---
    // irq_entry は「次に使う RSP」を RAX で返す（ADR-0019 §2.1）。それを
    // そのまま RSP にする。**これがコンテキストスイッチの実体である。**
    // 切り替え不要なら現在の IrqContext 先頭が返るので同じ場所へ戻り、挙動は
    // 変わらない。切り替え時は次タスクの IrqContext 先頭が返り、以降の pop は
    // 次タスクのレジスタを復元し、iretq が次タスクへ入る。
    //
    // sub した分（{adjust}）を足し戻す代わりに RAX を入れているのは、返り値が
    // 既に「先頭を指す RSP」だからである。iretq は RSP が CPU の積んだフレーム
    // の先頭（RIP）を指す状態で実行されねばならず、この後の pop 15 本と
    // add rsp,8 でちょうどそこへ着く。
    "  mov rsp, rax",
    // GPR を復元する。push の逆順（rax から r15 へ）。
    "  pop rax",
    "  pop rbx",
    "  pop rcx",
    "  pop rdx",
    "  pop rsi",
    "  pop rdi",
    "  pop rbp",
    "  pop r8",
    "  pop r9",
    "  pop r10",
    "  pop r11",
    "  pop r12",
    "  pop r13",
    "  pop r14",
    "  pop r15",
    // スタブが積んだベクタ番号を捨てる。これで RSP は RIP を指す。
    "  add rsp, 8",
    "  iretq",
    // 協調的 yield 用の専用スタブ（M5-c）。自動生成のスタブ表とは別に、
    // yield ベクタ 1 本ぶんを手で置く。IRQ と同じく共通ルーチンへ jmp する
    // ので、int YIELD_VECTOR が IRQ の退避・復元・スイッチ経路にそのまま
    // 載る。エラーコードは無いのでベクタ番号だけを積む。
    ".p2align 4",
    ".globl zaytos_yield_stub",
    "zaytos_yield_stub:",
    "  push {yield_vector}",
    "  jmp zaytos_irq_common",
    handler = sym irq_entry,
    adjust = const STACK_ALIGN_ADJUST,
    yield_vector = const YIELD_VECTOR,
);

extern "C" {
    /// 協調的 yield 用スタブの先頭（M5-c）。IDT の yield ゲートが指す。
    static zaytos_yield_stub: u8;
}

// システムコール（int 0x80）用のスタブと共通経路（M5-f-1、ADR-0020）。
//
// **IRQ スタイルの復元経路（zaytos_irq_common）を写した別ブロックである。**
// 退避・整列・call・復元・iretq の骨格は同じで、違うのは call 先が
// `crate::syscall::syscall_entry` で、context を *mut で渡し、戻り値を RAX へ
// 書き戻す点だけである。本番 IRQ 経路（irq_entry）へ syscall 固有の分岐を
// 持ち込まないために経路を分ける（このモジュール先頭の説明と ADR-0018 Addendum 3
// の「テストのために本番経路の信頼性を下げない」方針と揃える）。
//
// Ring 3 からの int 0x80 は特権変化（3→0）なので、CPU が TSS.RSP0 のスタックへ
// 自動で切り替えてから 5 語（SS/RSP/RFLAGS/CS/RIP）を積む。押し込む語数は IRQ と
// 同じ（CPU 5 語 + スタブのベクタ 1 語 + GPR 15 本）なので、STACK_ALIGN_ADJUST も
// 共通で正しい。導出に頼らず syscall_entry が実測 RSP を裏取りする。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zaytos_syscall_stub",
    "zaytos_syscall_stub:",
    // int 0x80 にエラーコードは無い。ベクタ番号だけを積む。
    "  push {syscall_vector}",
    "  jmp zaytos_syscall_common",
    ".p2align 4",
    "zaytos_syscall_common:",
    // 入場時のスタック: [rsp]=ベクタ, +8=RIP, +16=CS, +24=RFLAGS, +32=RSP, +40=SS
    // GPR を退避する。順序は IrqContext のフィールド順と一対一（IRQ と同じ）。
    "  push r15",
    "  push r14",
    "  push r13",
    "  push r12",
    "  push r11",
    "  push r10",
    "  push r9",
    "  push r8",
    "  push rbp",
    "  push rdi",
    "  push rsi",
    "  push rdx",
    "  push rcx",
    "  push rbx",
    "  push rax",
    "  mov rdi, rsp",
    "  sub rsp, {adjust}",
    "  mov rsi, rsp",
    "  call {handler}",
    // syscall_entry は「復元経路が使う RSP」を RAX で返す（M5-f-1 は入場時の
    // IrqContext 先頭）。ユーザー RAX に載る戻り値は context.rax に書き戻し済みで、
    // 下の pop rax がそれを復元する。
    "  mov rsp, rax",
    "  pop rax",
    "  pop rbx",
    "  pop rcx",
    "  pop rdx",
    "  pop rsi",
    "  pop rdi",
    "  pop rbp",
    "  pop r8",
    "  pop r9",
    "  pop r10",
    "  pop r11",
    "  pop r12",
    "  pop r13",
    "  pop r14",
    "  pop r15",
    // スタブが積んだベクタ番号を捨てる。これで RSP は RIP を指す。
    "  add rsp, 8",
    "  iretq",
    handler = sym crate::syscall::syscall_entry,
    adjust = const STACK_ALIGN_ADJUST,
    syscall_vector = const SYSCALL_VECTOR,
);

extern "C" {
    /// システムコール用スタブの先頭（M5-f-1）。IDT の 0x80 ゲートが指す。
    static zaytos_syscall_stub: u8;
}

extern "C" {
    /// `global_asm!` が定義するスタブ表の先頭。
    static zaytos_exception_stubs: u8;
    /// スタブ表の終端。先頭との差が `256 * STUB_SIZE` になるはず。
    static zaytos_exception_stubs_end: u8;
    /// 刻み幅の検証用に、アセンブラが直接付けたラベル。
    static zaytos_exception_stub_8: u8;
    static zaytos_exception_stub_14: u8;
    static zaytos_exception_stub_255: u8;

    /// IRQ スタブ表の先頭・終端・刻み幅検証用ラベル。
    ///
    /// 例外用とは**別の領域**なので、範囲検証も別系統になる。
    static zaytos_irq_stubs: u8;
    static zaytos_irq_stubs_end: u8;
    static zaytos_spurious_stub: u8;
    static zaytos_ioapic_keyboard_stub: u8;
    static zaytos_lapic_timer_stub: u8;
    static zaytos_irq_stub_0: u8;
    static zaytos_irq_stub_15: u8;
    static zaytos_irq_stub_16: u8;
    static zaytos_irq_stub_31: u8;
    static zaytos_irq_stub_32: u8;
}

/// IRQ スタイルのスタブの本数。
///
/// PIC が取りうるベクタ範囲 32 本（[`PIC_VECTOR_SPAN`]）に、テスト専用の
/// 1 本（[`TEST_VECTOR`]）を加えた数。
pub const IRQ_STYLE_STUB_COUNT: usize = PIC_VECTOR_SPAN + 1;

/// IRQ スタイルのスタブが担当する最初のベクタ。
///
/// **PIC のベクタオフセットそのものではない。** オフセットは 0x20 にも
/// 0x30 にもなりうる（`irq::vector_for`）。ここはスタブ表が
/// 覆う範囲の下端であり、取りうるオフセットのうち最小のものである。
pub const IRQ_VECTOR_BASE: usize = 0x20;

/// スタブ表が PIC のために覆うベクタ数（0x20-0x3F）。
///
/// PIC 自体の IRQ は 16 本（[`PIC_IRQ_COUNT`]）だが、オフセットが
/// 0x20 と 0x30 のどちらにもなりうるため、その両方を覆う。
pub const PIC_VECTOR_SPAN: usize = 32;

/// PIC の IRQ 本数（マスタ 8 + スレーブ 8）。
pub const PIC_IRQ_COUNT: usize = 16;

/// GPR 復元の検証に使うテスト専用ベクタ。
///
/// **PIC が取りうるどのベクタ範囲の外にもある。** `int 0x40` は 8259A を
/// 経由せず CPU が直接 IDT を引くため、ここへ来た割り込みに EOI を送る
/// 必要が無い。「EOI を送らないハンドラ」がそのまま正しい実装になるので、
/// テストと EOI の論理が干渉しない。
///
/// 以前は 0x30 だったが、それは `alt-offset-test` の IRQ0 と同じ番号で、
/// 両者を排他にしなければならなかった。スタブ表を 0x3F まで広げたのに
/// 合わせて、PIC の外へ恒久的に移した。
pub const TEST_VECTOR: usize = IRQ_VECTOR_BASE + PIC_VECTOR_SPAN;

/// 協調的 yield 用のソフトウェア割り込みベクタ（M5-c）。
///
/// PIC の範囲（0x20-0x2F）とテストベクタ（0x40）の外の 0x41 を 1 本使う。
/// 専用スタブ（`zaytos_yield_stub`）が `zaytos_irq_common` へ jmp するので、
/// `int YIELD_VECTOR` を実行すると IRQ の復元経路にそのまま載り、`irq_entry`
/// が「次タスクの RSP」を返してコンテキストスイッチが起きる（ADR-0019 §2）。
/// PIC 由来ではないので EOI の論理には一切絡まない。
pub const YIELD_VECTOR: usize = 0x41;

/// I/O APIC 経由のキーボード（IRQ1）用ベクタ（S2-d-1c）。
///
/// # なぜ `0x21` のまま使わないのか
///
/// **理由は 2 つあり、どちらか片方では足りない。**
///
/// 1. **観測。** `0x21` のままだと、届いたことが配送経路の証拠にならない。
///    8259 経由でも I/O APIC 経由でも同じベクタで届くので、
///    `keyboard: first key arrived as vector 0x21` は移行の前後で同じまま通り、
///    **何も新しいことを示さない。** 8259 が出しえないベクタで届けば、
///    到達がそのまま経路の証拠になる。
/// 2. **構造。** ベクタから IRQ を引く経路が PIC の採番表
///    （`irq::irq_for`）に依存していた。同じベクタを使うとその依存が
///    残ったまま動いてしまう。**動く理由が正しい理由でなくなる。**
///
/// # ベクタ番号の選択は、優先度クラスの選択でもある
///
/// x86 では**ベクタ番号を 16 で割った値が割り込みの優先度クラス**である。
/// `0x21` はクラス 2、**`0x42` はクラス 4** なので、この移行で
/// **キーボードがタイマ（`0x20`、クラス 2）より高い優先度になる。**
///
/// **実害は無い見込みである。** ゲートは割り込みゲート（IF を落とす）なので
/// 入れ子は起きず、TPR は 0 のままでどのクラスも遮断していない。
/// ただし**自明ではない**ので書いておく。S2-d-2 でタイマを `0xFE`
/// （クラス 15）へ移すと、今度はタイマが最上位クラスになる。同じ性質の
/// 副作用である。
///
/// スタブ表（`0x20`-`0x40`）の外なので専用スタブが要る。
pub const IOAPIC_KEYBOARD_VECTOR: usize = 0x42;

/// Local APIC タイマ用ベクタ（S2-d-2）。
///
/// # なぜ `0x20` のまま使わないのか
///
/// **キーボードを `0x42` へ移したのと同じ 2 つの理由による。**
/// 同じベクタだと、届いたことが配送経路の証拠にならない（8259 経由でも
/// Local APIC 経由でも `0x20` で届く）。そして「ベクタから IRQ を引く」経路が
/// PIC の採番表に当たったままになる。**`0xFE` は 8259 が出しえない値である。**
///
/// # ベクタ番号の選択は、優先度クラスの選択でもある
///
/// `0xFE` はクラス 15 で、**タイマが最上位クラスになる。**
/// キーボード（`0x42`、クラス 4）より高い。S2-d-1c でキーボードが
/// タイマより高くなったのが、ここで逆転する。**実害は無い見込みである**
/// （割り込みゲートで入れ子は起きず、TPR は 0 のまま）が、自明ではない。
///
/// # `0xFF` の隣である
///
/// スプリアス（`0xFF`）と隣り合う。どちらもスタブ表の外で専用スタブが要り、
/// 扱いが揃う。`alt-offset` ビルドの PIC（`0x30`-`0x3F`）とも衝突しない。
pub const LAPIC_TIMER_VECTOR: usize = 0xFE;

/// システムコール用のソフトウェア割り込みベクタ（M5-f-1、ADR-0020）。
///
/// `int 0x80` の 0x80。専用スタブ（`zaytos_syscall_stub`）が
/// `zaytos_syscall_common` へ jmp する。ゲートは **DPL=3** で登録し、Ring 3 から
/// 呼べるようにする（他のゲートは DPL=0）。PIC 由来ではないので EOI の論理には
/// 一切絡まない。
pub const SYSCALL_VECTOR: usize = 0x80;

/// syscall ゲート（0x80）の DPL。**DPL=3** が Ring 3 から int 0x80 を呼べる唯一の
/// 条件である。起動時の DPL 配置検査（`main.rs`）もこの値を期待値として使うので、
/// ゲート登録と検査の期待値が単一の定数から出る。
///
/// 破壊 (M5-f-1-2, gate-dpl0): DPL=0 にする。Ring 3 からの int 0x80 がゲート
/// DPL<CPL で #GP になり、`syscall_entry` に到達しない。起動時検査は期待値も 0 に
/// なるので通り、異常は int 0x80 発行時の #GP として runtime に現れる（M5-e-1 の
/// user-desc-dpl0 と同じ作りで、検査を先に発火させず runtime で捕まえる）。
#[cfg(not(feature = "syscall-test-gate-dpl0"))]
pub const SYSCALL_GATE_DPL: u8 = 3;
#[cfg(feature = "syscall-test-gate-dpl0")]
pub const SYSCALL_GATE_DPL: u8 = 0;

/// ベクタ別の割り込み回数。
///
/// **通常の `static` にしてはならない。** メインループがこれを読む形になる
/// ため、通常の変数だとコンパイラが読み出しをループの外へ巻き上げ、
/// 値が永久に変わらないように見える。「割り込みは来ているのにメインループが
/// 気づかない」という診断しにくい症状になる（ADR-0018 のチェックリスト 9）。
/// `Relaxed` で十分なのは、シングルコアで順序に依存した判断をしないため。
static INTERRUPT_COUNTS: [AtomicU64; IDT_ENTRY_COUNT] =
    [const { AtomicU64::new(0) }; IDT_ENTRY_COUNT];

/// タイマのティック数。**コアごとに持つ（S4-a）。**
///
/// [`INTERRUPT_COUNTS`] とは別に持つ。ティックは「時間の流れ」として
/// 頻繁に読む値であり、ベクタ番号での添字を経由せず直接読めるほうが
/// メインループの意図が読み取りやすい。
///
/// # なぜ per-CPU なのか
///
/// **「このコアが何回起きたか」は、コアごとの問いである。** 大域のままだと、
/// 2 コアが 100Hz で数えたとき合計が 200Hz で増え、**どちらのコアも自分の
/// 経過時間を知らない。** S4 の到達条件「コアごとのハートビート」は、
/// この値がコアごとであることを要求している。
///
/// **`INTERRUPT_COUNTS` は大域のままである。** あちらは「ベクタごとに何本
/// 配送されたか」で、コアの帰属を持たない別の問いである。**この 2 つは
/// 合計で閉じる**（[`timer_ticks_total`] の doc）。
///
/// 増減は自コアのスロットに対してのみ行う。読み手には他コアのスロットを読む
/// 会計があるが、`Relaxed` で足りる（順序に依存した判断をせず、数を見るだけ
/// である）。
static TIMER_TICKS: PerCpu<AtomicU64> = PerCpu::new([const { AtomicU64::new(0) }; MAX_CPUS]);

/// 破壊 (S4-a, smp-ap-timer-share-ticks): per-CPU をやめて 1 つを共有する。
///
/// **per-CPU 化が「済んだように見えて共有のまま」という形を捕まえる**
/// （`GPR_BUF` で見たのと同じ形である）。共有すると 2 コアぶんが 1 つの
/// カウンタへ入るので、**コアごとの合計がベクタ別カウンタの 2 倍になる。**
#[cfg(feature = "smp-ap-timer-share-ticks-test")]
static SHARED_TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

/// このコアのティックカウンタ。
fn timer_ticks_slot() -> &'static AtomicU64 {
    #[cfg(feature = "smp-ap-timer-share-ticks-test")]
    {
        &SHARED_TIMER_TICKS
    }
    #[cfg(not(feature = "smp-ap-timer-share-ticks-test"))]
    {
        TIMER_TICKS.this_cpu()
    }
}

/// 今カーネル入口の中にいるコアの数（S4-a）。
///
/// # 数える前に、数える対象を定義する
///
/// **現在の定義は「BKL を取得してから解放するまでの区間にいるコアの数」である**
/// （S4-b-3 で移した）。
///
/// **S4-a の定義は違った**——「`irq_entry` の先頭から戻るまでの区間にいるコアの数」
/// だった。BKL がまだ無いので、そちらしか立てられなかった。
///
/// **移す前と後で、同じものを数えていない。**
///
/// | | S4-a の定義 | 現在の定義 |
/// |---|---|---|
/// | 区間の始まり | `irq_entry` の先頭 | BKL を取得した直後 |
/// | 区間の終わり | `irq_entry` から戻る直前 | BKL を解放する直前 |
/// | BKL を待っている間 | **数に入る** | **数に入らない** |
/// | `syscall_entry` | 入らない | **入る** |
/// | 定常ループの共有区間 | 入らない | **入る** |
///
/// **値が 2 から 1 へ落ちたとしても、それだけでは BKL が効いた証明にならない。**
/// 定義が変わったぶんも混ざるからである。**証明は S4-b-4 で、増幅器を固定した
/// まま `skip` の有無を比べて行う。**
static KERNEL_ENTRY_DEPTH: AtomicU64 = AtomicU64::new(0);

/// [`KERNEL_ENTRY_DEPTH`] がこれまでに取った最大値（S4-a）。
///
/// **`fetch_max` で更新する。** 「今の値を読んで比べて書く」形にすると、
/// 2 コアが同時に更新したときに片方が消える。**同時進入を数える装置そのものが
/// 競合で壊れていては本末転倒である。**
static MAX_KERNEL_ENTRY_DEPTH: AtomicU64 = AtomicU64::new(0);

/// カーネル入口にいる間だけ生きるガード（S4-a）。
///
/// # なぜ RAII なのか
///
/// 数える区間には早期 return が複数ある。**減算を各 return の手前へ書く形に
/// すると、1 つ落としたときに静かに壊れる。** カウンタが下がらないまま増え続け、
/// **同時進入数が実際より多く見える。** 規律ではなく構造で対にする。
///
/// # 今は [`crate::bkl`] だけが作る
///
/// **S4-b-3 で、作る場所を BKL の中へ移した。** 入口が増えても数える場所は
/// 1 つのままである（BKL を取る入口はすべてここを通る）。
pub struct KernelEntryGuard {
    /// `!Send` + `!Sync` にするためのマーカー。**この区間はコアに固定である。**
    _not_send_sync: core::marker::PhantomData<*const ()>,
}

impl KernelEntryGuard {
    /// カーネル入口へ入ったことを記録する。
    #[must_use = "ガードを保持している間だけ「入口の中」として数えられる"]
    pub fn enter() -> Self {
        let depth = KERNEL_ENTRY_DEPTH.fetch_add(1, Ordering::Relaxed) + 1;
        MAX_KERNEL_ENTRY_DEPTH.fetch_max(depth, Ordering::Relaxed);
        if depth > 1 {
            report_concurrent_entry_once(depth);
        }
        Self {
            _not_send_sync: core::marker::PhantomData,
        }
    }
}

impl Drop for KernelEntryGuard {
    fn drop(&mut self) {
        KERNEL_ENTRY_DEPTH.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 同時進入数のこれまでの最大値（S4-a）。
pub fn max_kernel_entry_depth() -> u64 {
    MAX_KERNEL_ENTRY_DEPTH.load(Ordering::Relaxed)
}

/// 同時進入を 1 度だけ報告したか。
static CONCURRENT_ENTRY_REPORTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 同時進入を**起きた瞬間に**1 度だけ報告する（S4-b-4）。
///
/// # なぜハートビートを待たないのか
///
/// ハートビートは 100 ティック（約 1 秒）ごとにしか出ない。**その前に別の理由で
/// 停止すると、同時進入が起きていたことが観測されないまま終わる。**
/// `bkl-skip-timer-entry` の構成では `Locked<T>` の競合による停止がありうるので、
/// **観測とその後の停止の順序がタイミング次第になる。**
///
/// **起きた瞬間に出せば、後で何が起きても順序は決まる。**
///
/// 1 度だけにするのは、2 コアが 100Hz で重なり続けるとログが埋まるためである。
fn report_concurrent_entry_once(depth: u64) {
    if CONCURRENT_ENTRY_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[WARN] bkl: kernel entry depth reached {depth}; more than one core is inside a \
         kernel entry at the same time"
    );
}

/// PIC の範囲で最初に観測した割り込みのベクタ番号。
///
/// **これが ICW2（PIC のベクタオフセット）を事後的に証明する唯一の手段
/// である。** ICW2 は書き込み専用で読み戻せないため、再マップが意図どおり
/// 効いたかは「実際にどのベクタで届いたか」でしか分からない
/// （ADR-0018 Addendum 1）。
///
/// [`NO_VECTOR_YET`] は「まだ 1 件も来ていない」ことを表す番兵。
static FIRST_PIC_VECTOR: AtomicU64 = AtomicU64::new(NO_VECTOR_YET);

/// [`FIRST_PIC_VECTOR`] の「まだ来ていない」を表す値（ベクタ番号は 0-255）。
pub const NO_VECTOR_YET: u64 = u64::MAX;

/// **このコアの**タイマのティック数を読む（S4-a）。
///
/// 較正（`apic::calibrate_timer`）も定常ループもこれを読む。どちらも BSP で
/// 走り、そのとき数えているのも BSP のスロットなので、**両辺が同じスロットで
/// あり意味は変わらない。**
pub fn timer_ticks() -> u64 {
    timer_ticks_slot().load(Ordering::Relaxed)
}

/// 指定したコアのティック数（S4-a）。**ハートビートと会計が使う。**
///
/// 範囲外は `0` を返す。
pub fn timer_ticks_for(cpu: usize) -> u64 {
    #[cfg(feature = "smp-ap-timer-share-ticks-test")]
    {
        let _ = cpu;
        SHARED_TIMER_TICKS.load(Ordering::Relaxed)
    }
    #[cfg(not(feature = "smp-ap-timer-share-ticks-test"))]
    {
        TIMER_TICKS
            .slot(cpu)
            .map_or(0, |slot| slot.load(Ordering::Relaxed))
    }
}

/// 全コアのティック数の合計（S4-a）。
///
/// # これは会計の片辺である
///
/// **もう片辺は [`timer_delivery_count`] である。** 1 本のティックは
/// 必ずどこか 1 コアのスロットを増やし、同時にベクタ別カウンタも増やすので、
/// **合計は一致する。** 一致しなければ「どこかのコアぶんが別のスロットへ入って
/// いる」ことになる。
///
/// **厳密な同時刻の一致は取れない。** 2 つの値を続けて読む間にも両コアが
/// 数えるので、**進行中のぶんだけずれる。** ずれの上限はコア数程度である。
pub fn timer_ticks_total() -> u64 {
    let mut total = 0;
    for cpu in 0..MAX_CPUS {
        total += timer_ticks_for(cpu);
    }
    total
}

/// タイマとして配送された割り込みの総数（S4-a）。**会計のもう片辺である。**
///
/// # なぜ 2 本のベクタを足すのか
///
/// **タイマは起動途中で配送経路が変わる。** PIT で起動し、較正の後に Local APIC
/// タイマへ移る（S2-d-2）。したがって**移行より前のティックは
/// [`PIC_TIMER_VECTOR`] に、後のティックは [`LAPIC_TIMER_VECTOR`] に積まれる。**
/// 片方だけを見ると、移行前のぶんが丸ごと欠けて会計が閉じない。
pub fn timer_delivery_count() -> u64 {
    interrupt_count(PIC_TIMER_VECTOR) + interrupt_count(LAPIC_TIMER_VECTOR)
}

/// 進行中のぶんとして許すずれ（S4-a）。
///
/// **統計的な許容ではない。** 2 つの値を続けて読む間に、各コアが最大 1 本ずつ
/// 数えうる、という**上限**である。標本を増やしても縮まない類の値ではなく、
/// コア数で決まる。余裕を見て 2 倍にしてある。
const TIMER_ACCOUNTING_SLACK: u64 = (MAX_CPUS as u64) * 2;

/// コアごとのティックの合計と、配送された本数が一致するか（S4-a）。
///
/// **`smp-ap-timer-share-ticks` が捕まる先はここである。** per-CPU をやめて
/// 1 つを共有すると、合計が配送数のおよそ 2 倍になり、[`TIMER_ACCOUNTING_SLACK`]
/// をはるかに超える。**「per-CPU 化が済んだように見えて共有のまま」を、
/// 名前ではなく数で捕まえる。**
pub fn timer_accounting_balances() -> bool {
    timer_ticks_total().abs_diff(timer_delivery_count()) <= TIMER_ACCOUNTING_SLACK
}

/// PIC の範囲で最初に届いた割り込みのベクタ番号。まだなら `None`。
pub fn first_pic_vector() -> Option<u64> {
    match FIRST_PIC_VECTOR.load(Ordering::Relaxed) {
        NO_VECTOR_YET => None,
        vector => Some(vector),
    }
}

/// 指定ベクタの割り込み回数を読む。
pub fn interrupt_count(vector: usize) -> u64 {
    if vector >= IDT_ENTRY_COUNT {
        return 0;
    }
    INTERRUPT_COUNTS[vector].load(Ordering::Relaxed)
}

/// 現時点の全ベクタのカウンタを写し取る。
///
/// 「この時点より後に何か届いたか」を見るための基準点。**絶対値で
/// 「全部 0 か」を見てはいけない。** 起動シーケンスの中でソフトウェア
/// 割り込みによる経路検証（`--interrupt-test irq-path`）を通ると、
/// [`TEST_VECTOR`] の分が既にカウントされており、絶対値では常に
/// 「何か来た」と判定されてしまう。
pub fn snapshot_counts() -> [u64; IDT_ENTRY_COUNT] {
    core::array::from_fn(|vector| INTERRUPT_COUNTS[vector].load(Ordering::Relaxed))
}

/// 基準点からの増加分の合計と、最初に増えたベクタを返す。
pub fn delta_since(baseline: &[u64; IDT_ENTRY_COUNT]) -> (u64, Option<usize>) {
    let mut total = 0u64;
    let mut first = None;
    for vector in 0..IDT_ENTRY_COUNT {
        let now = INTERRUPT_COUNTS[vector].load(Ordering::Relaxed);
        let delta = now.saturating_sub(baseline[vector]);
        total += delta;
        if delta != 0 && first.is_none() {
            first = Some(vector);
        }
    }
    (total, first)
}

/// 全ベクタの合計と、0 でなかった最初のベクタを返す。
///
/// 「全部 0 のはず」を確認する用途で、**0 でなかった場合にどのベクタかが
/// 分かる**形にしてある。とくに NMI（ベクタ 2）は `cli` でマスクできない
/// ため、全 IRQ をマスクした状態でも理論上は届きうる。合計だけを見ていると
/// 「何かが来た」までしか分からず、原因の見当がつかない。
pub fn interrupt_total_and_first_nonzero() -> (u64, Option<usize>) {
    let mut total = 0u64;
    let mut first = None;
    // `enumerate` の添字はベクタ番号そのものである。返り値がベクタ番号で
    // ある以上、この対応は失いたくない。
    for (vector, counter) in INTERRUPT_COUNTS.iter().enumerate() {
        let count = counter.load(Ordering::Relaxed);
        total += count;
        if count != 0 && first.is_none() {
            first = Some(vector);
        }
    }
    (total, first)
}

/// `call` 直前の実測 RSP が 16 バイト境界にあることを確認する。
///
/// **手計算の再現ではない。** スタブが `call` の直前にレジスタから読んだ
/// 実値を受け取って検査する。境界計算（[`STACK_ALIGN_ADJUST`] の導出）が
/// 間違っていれば、ここで捕まる。
///
/// SysV ABI が要求するのは `call` 実行時点で `RSP % 16 == 0` であること。
/// 関数入口では戻りアドレスの分だけずれて `RSP % 16 == 8` になるため、
/// 「入口のフレームアドレス + 8 が 16 の倍数」と言っても同じである。
///
/// 違反は fail-fast する。SSE を無効化しているため即座にクラッシュはしない
/// が ABI 違反であり、放置すると将来 SSE を有効化した瞬間や、コンパイラが
/// 境界を仮定した最適化を行った瞬間に、原因不明の形で壊れる。
// `RSP % 16 == 0` は SysV ABI と本関数の説明の書き方そのものである。
// `is_multiple_of(16)` へ言い換えると、ABI の記述との対応が読み取りにくくなる。
#[allow(clippy::manual_is_multiple_of)]
pub(crate) fn check_stack_alignment(rsp_at_call: u64, path: &str, vector: u64) {
    if rsp_at_call % 16 == 0 {
        return;
    }
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    use core::fmt::Write;
    let _ = writeln!(
        serial,
        "[ERROR] stack alignment: {path} stub violated the SysV ABI (vector={vector})"
    );
    let _ = writeln!(
        serial,
        "[ERROR]   rsp at call = {rsp_at_call:#018x} (rsp % 16 = {}, must be 0)",
        rsp_at_call % 16
    );
    let _ = writeln!(
        serial,
        "[ERROR]   the stub pushed an amount that does not match STACK_ALIGN_ADJUST"
    );
    let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");
    cpu::halt_forever();
}

/// IRQ の共通処理。**戻る。**
///
/// スタブから `extern "sysv64"` で呼ばれる（ADR-0018 のチェックリスト 11）。
///
/// **出力しない。** ADR-0018 §5 のとおり、ここでやるのは共有状態の更新だけ
/// である。観測はメインループがカウンタ越しに行う。
///
/// M4-d-1 の時点では EOI を送らない。全 IRQ をマスクしているため実際の
/// IRQ は届かず、ここへ来るのはソフトウェア割り込み（`int`）による経路
/// 検証だけである。EOI は M4-d-2 で実装する。
///
/// # Safety
///
/// `context` はスタブが積んだ [`IrqContext`] を指していること。
/// `rsp_at_call` はスタブが `call` 直前に読んだ RSP であること。
extern "sysv64" fn irq_entry(context: *const IrqContext, rsp_at_call: u64) -> u64 {
    // 切り替え不要なときに返す RSP。**入場時の IrqContext 先頭そのもの**で、
    // スタブの復帰部で `mov rsp, rax` してもこれなら現状と同じ場所へ戻る
    // （ADR-0019 §2.1）。M5-c ではここが切り替えの唯一の分岐点になり、
    // yield ベクタのときだけ別タスクの RSP を返す（下の分岐）。
    let no_switch_rsp = context as u64;

    // **BKL を取る（S4-b-2）。** ここから戻るまでカーネルへ入れるのは 1 コアだけ
    // である。**早期 return が複数あるので RAII にする**（解放を各 return の手前へ
    // 書くと、1 つ落としたときに保持したまま戻り、系全体が止まる）。
    //
    // **同時進入は BKL の中で数える（S4-b-3）。** ここで別に数えると、
    // 定義が 2 つになる。
    // 破壊 (S4-b-4, bkl-skip-timer-entry): ロックを取らず計数だけ行う。
    // **数えているものが本番と違う**（`acquire_counting_only` の doc）。
    #[cfg(feature = "bkl-skip-timer-entry-test")]
    let _bkl = crate::bkl::acquire_counting_only(crate::bkl::KernelEntry::Irq);
    #[cfg(not(feature = "bkl-skip-timer-entry-test"))]
    let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::Irq);

    // 破壊 (S4-b-4, bkl-widen-entry-window): 入口の保持区間を広げる。
    // **重なりの増幅器であって、素の重なりの頻度とは別である**（feature の doc）。
    #[cfg(feature = "bkl-widen-entry-window-test")]
    for _ in 0..crate::bkl::WIDENED_ENTRY_WINDOW_SPINS {
        core::hint::spin_loop();
    }

    // SAFETY: スタブが直前に積んだ有効な IrqContext を指す。読み取りのみ。
    let context = unsafe { &*context };

    check_stack_alignment(rsp_at_call, "irq", context.vector);

    // 協調的 yield（M5-c）。ここだけが切り替えの分岐点で、次タスクの RSP を
    // 返す。それ以外（タイマ・キーボード・テストベクタ）は切り替えない。
    if context.vector as usize == YIELD_VECTOR {
        INTERRUPT_COUNTS[YIELD_VECTOR].fetch_add(1, Ordering::Relaxed);
        return crate::task::on_yield(no_switch_rsp);
    }

    let vector = context.vector as usize;
    if vector < IDT_ENTRY_COUNT {
        INTERRUPT_COUNTS[vector].fetch_add(1, Ordering::Relaxed);
    }

    // PIC の範囲で最初に届いたベクタを 1 度だけ記録する。ICW2 の検証に使う。
    if u8::try_from(vector)
        .ok()
        .and_then(crate::irq::irq_for)
        .is_some()
    {
        let _ = FIRST_PIC_VECTOR.compare_exchange(
            NO_VECTOR_YET,
            context.vector,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    // Local APIC のスプリアス割り込み（S2-d-1）。**EOI を送らずに戻る。**
    //
    // **判定を明示にした。** 以前このベクタに EOI が送られなかったのは
    // 「PIC の担当範囲の外だから」であって、スプリアスだからではなかった。
    // S2-d で Local APIC が配送を担うと LAPIC 由来のベクタには EOI が要るので、
    // **その偶然の一致は壊れる。** ここで問いの形にしておく。
    //
    // 回数は PIC のスプリアス（IRQ7 / IRQ15）とは**別に数える。** 機序が違い、
    // 合流させるとどちらが起きたのかハートビートから分からなくなる。
    if vector == crate::apic::SPURIOUS_VECTOR as usize {
        LAPIC_SPURIOUS_COUNT.fetch_add(1, Ordering::Relaxed);
        return no_switch_rsp;
    }

    // Local APIC タイマ（S2-d-2）。**LVT 由来なので IRQ 番号を持たない。**
    //
    // **判定の順序を固定する。LVT 由来を先に見る。** 後ろに置くと、
    // このベクタが PIC の採番表に当たる構成で誤る。現行の 2 構成
    // （`0x20`-`0x2F` と `0x30`-`0x3F`）では当たらないが、**依存を残さない。**
    //
    // EOI は Local APIC へ送る。**8259 は関与しない。**
    if vector == LAPIC_TIMER_VECTOR {
        timer_ticks_slot().fetch_add(1, Ordering::Relaxed);
        // SAFETY: 割り込みハンドラの中であり、割り込みゲート経由なので IF=0。
        // 実際に配送された割り込みに対してのみ呼んでいる。
        //
        // **EOI は自コアの Local APIC へ届く。** 送り先の VA は 1 つだが、
        // その物理アドレスは実行しているコア自身の LAPIC に別名づけられている。
        // **共有 IDT で両コアが同じハンドラに入っても、EOI の宛先は分かれる。**
        #[cfg(not(feature = "no-eoi-test"))]
        unsafe {
            crate::irq::end_of_interrupt_for_lapic_timer();
        }
        // **AP はスケジューラへ入らない（S4-a）。**
        //
        // この段の AP はタスクを実行しない。入れば `CURRENT` の sentinel を
        // 読んで停止する（S3-b-2b-2 で置いた防衛線）。**手前で戻るのは、
        // 停止させないためであって、sentinel を信用していないからではない。**
        // 外し忘れても静かには壊れない。参加は S4-c である。
        //
        // 破壊 (S4-a, smp-ap-enter-scheduler): この分岐を外して AP を
        // スケジューラへ入れる。sentinel が止めることを確かめる。
        #[cfg(not(feature = "smp-ap-enter-scheduler-test"))]
        if !common::percpu::is_bootstrap_processor() {
            return no_switch_rsp;
        }
        return crate::task::on_timer_tick(no_switch_rsp);
    }

    // このベクタはどの IRQ か。**移行済みの経路も含めて引く**（S2-d-1c）。
    //
    // **ここは EOI の入口ではなく、IRQ 処理全体の入口である。** 下の
    // ブロックにはティックの加算もキーボードのハンドラも入っており、
    // **引けなければハンドラごと呼ばれない。** I/O APIC 経由のベクタは
    // PIC の採番表に載っていないので、`irq::irq_for` では引けない。
    //
    // テスト専用ベクタ（`0x40`、どちらの表にも無い）はここに入らないので、
    // EOI の論理が一切絡まない。
    let delivered_irq = irq_for_vector(vector);

    if let Some(irq) = delivered_irq {
        // **配送先を問うので、8259 の採番ではなく現在の配送先を見る。**
        // 今は同じ値だが、S2-d-2 で Local APIC タイマへ移すと変わる。
        if vector == timer_delivery_vector() {
            timer_ticks_slot().fetch_add(1, Ordering::Relaxed);
        }

        // キーボード（IRQ1）。**EOI より先に呼ぶ。** この中でデータポートを
        // 読み切らないと、コントローラの出力バッファが空かず次の IRQ1 が
        // 来なくなる。
        //
        // **ベクタではなく IRQ 番号で判定する**（S2-d-1c）。配送先ベクタは
        // 8259 経由と I/O APIC 経由で違うが、IRQ 番号は移行しても変わらない。
        if irq == crate::keyboard::KEYBOARD_IRQ {
            crate::keyboard::handle_irq(context.vector);
        }

        // スプリアス（偽）割り込みの判定。IRQ7 / IRQ15 でしか起きない。
        // 本物なら ISR の該当ビットが立っている。
        //
        // SAFETY: 割り込みハンドラの中であり、割り込みゲート経由で入場した
        // ため IF=0。他の実行文脈が同時にコントローラを触ることはない。
        let spurious = unsafe { crate::irq::is_spurious(irq) };
        if spurious {
            SPURIOUS_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        // **処理を終えてから EOI を送る。** 送った時点で PIC は次の同じ
        // 割り込みを上げられるようになる。宛先は純粋ロジックが決める
        // （スプリアスの扱いはマスタ側とスレーブ側で非対称）。
        //
        // SAFETY: 実際に発生した割り込みに対してのみ呼んでいる。宛先の決定は
        // 境界の内側の純粋ロジックが行う。
        #[cfg(not(feature = "no-eoi-test"))]
        unsafe {
            crate::irq::end_of_interrupt(irq, spurious);
        }
    }

    // ここで出力してはならない（ADR-0018 §5）。100Hz で毎回ログを出すと
    // 出力自体がハンドラの処理時間を支配し、ティックを取りこぼす。観測は
    // メインループがカウンタ越しに行う。

    // タイマ（IRQ0）はプリエンプティブに切り替える（M5-d）。**EOI はここより
    // 前で送っている**ので、次タスクは IF=1 で次ティックを受けられる。キーボード
    // やテストベクタは切り替えない（入場時の RSP を返す）。
    if vector == timer_delivery_vector() {
        return crate::task::on_timer_tick(no_switch_rsp);
    }

    no_switch_rsp
}

/// タイマ（IRQ0）の**8259 での**ベクタ。
///
/// 8259 のベクタ採番に追随する。`alt-offset-test` では `0x30` になる。
///
/// # これは現在の配送先とは限らない
///
/// **名前が事実と食い違わないよう改名した**（旧 `TIMER_VECTOR`）。
/// S2-d-2 でタイマを Local APIC タイマへ移すと、実際の配送先は LVT Timer に
/// 載せた別のベクタになる。この定数はあくまで**8259 の採番表が与える値**で
/// あって、現在どこへ届くかではない。**改名は移行より前でも正確である**
/// （8259 の採番表が与える値である、というのは移行前から真である）。
///
/// 現在の配送先を知りたい場合は [`timer_delivery_vector`] を使うこと。
/// キーボードについて [`crate::keyboard::PIC_KEYBOARD_VECTOR`] と
/// [`crate::keyboard::delivery_vector`] を分けたのと同じ形である。
///
/// `match` で剥がしているのは、失敗時のメッセージが読めるためである
/// （`unwrap()` も固定トールチェインで const 評価できることは確認済み）。
pub const PIC_TIMER_VECTOR: usize = match crate::irq::vector_for(0) {
    Some(vector) => vector as usize,
    None => panic!("the timer IRQ has no vector"),
};

/// タイマ割り込みが**現在**届くベクタ。
///
/// Local APIC タイマへ移った後は [`LAPIC_TIMER_VECTOR`]、それ以前は
/// [`PIC_TIMER_VECTOR`] である。
///
/// # なぜ今から関数にするのか
///
/// **改名だけでは、値を使う側が `const` を直接読む形のままになる。**
/// 「現在の配送先」を問う箇所と「8259 の採番」を問う箇所が同じ式で書かれて
/// いると、移行のときにどちらの意味で書かれたのかを 1 箇所ずつ読み直す
/// ことになる。**意味の違う 2 つを、今のうちに別の呼び出しに分けておく。**
///
/// # タイマは IRQ 単位の移行状態に乗らない
///
/// Local APIC タイマは I/O APIC ではなく **LVT 経由**で、**IRQ 番号を
/// 持たない。** したがって `irq` の `ROUTED_TO_APIC`（I/O APIC 経由へ移した
/// IRQ のビットマップ）では表せない。**IRQ0 のビットを立てて表現しないこと。**
/// 立てるとビットマップの意味が「I/O APIC 経由である」から「PIC でなくなった」
/// へ静かにずれる。S2-d-2 では別の器で持つ。
pub fn timer_delivery_vector() -> usize {
    if crate::irq::timer_on_lapic() {
        LAPIC_TIMER_VECTOR
    } else {
        PIC_TIMER_VECTOR
    }
}

/// スプリアス割り込みを受けた回数（ベクタ別ではなく合計）。
///
/// 8259A がノイズ等で上げる偽の割り込み。IRQ7 / IRQ15 として届く
/// （ADR-0018 のチェックリスト 8）。EOI を送ってはいけないので、通常の
/// 経路と分けて数える。
static SPURIOUS_COUNT: AtomicU64 = AtomicU64::new(0);

/// スプリアス割り込みを受けた回数。
pub fn spurious_count() -> u64 {
    SPURIOUS_COUNT.load(Ordering::Relaxed)
}

/// Local APIC のスプリアス割り込みを受けた回数（S2-d-1）。
///
/// **[`SPURIOUS_COUNT`] とは別に数える。** あちらは 8259A が IRQ7 / IRQ15 として
/// 上げる偽の割り込みで、こちらは Local APIC が SVR のベクタで上げるものである。
/// **機序が違うので合流させない。** 合流させると、ハートビートを見たときに
/// どちらが起きたのか分からなくなる。
static LAPIC_SPURIOUS_COUNT: AtomicU64 = AtomicU64::new(0);

/// Local APIC のスプリアス割り込みを受けた回数。
pub fn lapic_spurious_count() -> u64 {
    LAPIC_SPURIOUS_COUNT.load(Ordering::Relaxed)
}

/// ベクタ番号から IRQ 番号を求める。どのコントローラにも属さなければ `None`。
///
/// **`pic_irq_for` から改名した**（S2-d-1c）。I/O APIC 経由へ移した IRQ も
/// 引くようになり、「PIC 由来か」という名前が事実と合わなくなったためである。
///
/// テスト専用ベクタ（[`TEST_VECTOR`]）はどちらの表にも無いので `None` になり、
/// EOI の経路へ入らない。
fn irq_for_vector(vector: usize) -> Option<u8> {
    if vector > u8::MAX as usize {
        return None;
    }
    crate::irq::irq_for_vector(vector as u8)
}

/// IRQ スタブ表の配置検証。
///
/// 例外用（[`check_stub_table`]）と**別系統**である。表が別の領域にある
/// ため、片方の検証がもう片方を保証しない。
pub fn check_irq_stub_table() -> StubTableCheck {
    let base = addr_of!(zaytos_irq_stubs) as u64;
    let end = addr_of!(zaytos_irq_stubs_end) as u64;
    let expected_size = (IRQ_STYLE_STUB_COUNT * STUB_SIZE) as u64;

    let stride_ok = addr_of!(zaytos_irq_stub_0) as u64 == base
        && addr_of!(zaytos_irq_stub_15) as u64 == base + 15 * STUB_SIZE as u64
        && addr_of!(zaytos_irq_stub_16) as u64 == base + 16 * STUB_SIZE as u64
        && addr_of!(zaytos_irq_stub_31) as u64 == base + 31 * STUB_SIZE as u64
        && addr_of!(zaytos_irq_stub_32) as u64 == base + 32 * STUB_SIZE as u64;

    // 0x20-0x2F の IDT エントリが、IRQ スタブ表の対応する位置を指すこと。
    // 上書きに失敗して例外スタブを指したままだと、IRQ が「戻らない」経路へ
    // 入り、最初の割り込みで停止する。
    let mut entries_ok = true;
    for index in 0..IRQ_STYLE_STUB_COUNT {
        let vector = IRQ_VECTOR_BASE + index;
        let Some(entry) = entry(vector) else {
            entries_ok = false;
            break;
        };
        let handler = entry.handler_address();
        if handler < base || handler >= end {
            entries_ok = false;
            break;
        }
        let offset = handler - base;
        if !offset.is_multiple_of(STUB_SIZE as u64) || offset / STUB_SIZE as u64 != index as u64 {
            entries_ok = false;
            break;
        }
    }

    StubTableCheck {
        base,
        end,
        actual_size: end - base,
        expected_size,
        stride_ok,
        entries_ok,
    }
}

/// `n` 番目の IRQ スタブのアドレス。
fn irq_stub_address(index: usize) -> u64 {
    addr_of!(zaytos_irq_stubs) as u64 + (index * STUB_SIZE) as u64
}

/// スタブ表の配置に関する検証結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StubTableCheck {
    pub base: u64,
    pub end: u64,
    /// 実際の表の大きさ。`IDT_ENTRY_COUNT * STUB_SIZE` と一致すべき。
    pub actual_size: u64,
    pub expected_size: u64,
    /// アセンブラが付けたラベルと `base + n * STUB_SIZE` が一致したか。
    pub stride_ok: bool,
    /// 全 IDT エントリのハンドラが表の範囲内で、かつ正しい位置にあるか。
    pub entries_ok: bool,
}

impl StubTableCheck {
    pub fn is_ok(&self) -> bool {
        self.actual_size == self.expected_size && self.stride_ok && self.entries_ok
    }
}

/// 例外スタブ表の外に置いた専用スタブの本数。
pub const DEDICATED_STUB_COUNT: usize = 5;

/// 例外スタブ表の外に置いた専用スタブと、それを指すべきゲートの対応。
///
/// **この一覧が唯一の出所である。** [`check_stub_table`] は表の中に無い
/// ベクタとしてここに載っているものを飛ばし、[`check_dedicated_stubs`] は
/// 同じ一覧について「専用スタブを指していること」を確かめる。**飛ばす側と
/// 確かめる側が同じ配列を読むので、片方だけを更新して食い違わせられない。**
///
/// 分けて持つと、飛ばす側にだけ足したときに「何も見ないベクタ」が生まれる。
/// 実際に S2-d-1a でスプリアスベクタを飛ばす側にだけ足しており、その時点では
/// ゲートの指す先を誰も見ていなかった（`verification-coverage.md`）。
///
/// `addr_of!` は const ではないので、定数ではなく関数として持つ。
fn dedicated_stubs() -> [(usize, u64); DEDICATED_STUB_COUNT] {
    [
        (YIELD_VECTOR, addr_of!(zaytos_yield_stub) as u64),
        (SYSCALL_VECTOR, addr_of!(zaytos_syscall_stub) as u64),
        (
            crate::apic::SPURIOUS_VECTOR as usize,
            addr_of!(zaytos_spurious_stub) as u64,
        ),
        (
            IOAPIC_KEYBOARD_VECTOR,
            addr_of!(zaytos_ioapic_keyboard_stub) as u64,
        ),
        (LAPIC_TIMER_VECTOR, addr_of!(zaytos_lapic_timer_stub) as u64),
    ]
}

/// 専用スタブ 1 本ぶんの検証結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DedicatedStubCheck {
    pub vector: usize,
    /// アセンブラが付けたラベルのアドレス。
    pub expected_handler: u64,
    /// IDT ゲートが実際に指しているアドレス。ゲートが読めなければ 0。
    pub actual_handler: u64,
}

impl DedicatedStubCheck {
    pub fn is_ok(&self) -> bool {
        self.actual_handler == self.expected_handler
    }
}

/// 例外スタブ表の外のベクタが、それぞれの専用スタブを指していることを検証する。
///
/// **[`check_stub_table`] の除外リストが空けた穴を塞ぐための検査である。**
/// あちらは「全ベクタが例外スタブ表の対応する位置を指す」を見て、そこから
/// 外れるベクタを飛ばす。飛ばされたベクタについては**何も見ない**ので、
/// ゲートの代入を落としても、既定の例外スタイルのスタブを指したまま静かに
/// 通る。yield と syscall は「戻らない」経路へ落ち、スプリアスは S2-b 以前の
/// 「起きたら止まる」状態へ戻る。いずれも起動時には現れない。
///
/// **見るのはハンドラのアドレスだけである。** present / ゲート種別 / DPL は
/// 全 256 ベクタを対象にした別の検査が既に見ており、syscall の DPL は
/// [`SYSCALL_GATE_DPL`] という本ごとの期待値を持っている。ここで属性を
/// 一律に見ると、DPL 3 が正しい syscall で落ちる。
pub fn check_dedicated_stubs() -> [DedicatedStubCheck; DEDICATED_STUB_COUNT] {
    dedicated_stubs().map(|(vector, expected_handler)| DedicatedStubCheck {
        vector,
        expected_handler,
        actual_handler: entry(vector).map_or(0, |e| e.handler_address()),
    })
}

/// スタブ表の刻み幅と、IDT エントリがそれを正しく指していることを検証する。
///
/// IDT は `base + n * STUB_SIZE` という式でエントリを作っているため、この
/// 前提が崩れると**全エントリが誤ったアドレスを指す**。しかも、その前提を
/// 使って作ったエントリを同じ式で検算しても意味がない（循環する）。
/// そこでアセンブラが算出した独立のラベル（終端と抜き取り 3 点）と
/// 突き合わせる。
///
/// スタブに命令を 1 つ足して 16 バイトを超えると、終端までの距離が
/// `256 * STUB_SIZE` からずれるため、ここで捕まる。
pub fn check_stub_table() -> StubTableCheck {
    let base = addr_of!(zaytos_exception_stubs) as u64;
    let end = addr_of!(zaytos_exception_stubs_end) as u64;
    let expected_size = (IDT_ENTRY_COUNT * STUB_SIZE) as u64;

    let stride_ok = addr_of!(zaytos_exception_stub_8) as u64 == base + 8 * STUB_SIZE as u64
        && addr_of!(zaytos_exception_stub_14) as u64 == base + 14 * STUB_SIZE as u64
        && addr_of!(zaytos_exception_stub_255) as u64 == base + 255 * STUB_SIZE as u64;

    // 全エントリのハンドラが表の範囲内で、ベクタ番号と位置が対応すること。
    //
    // **この検査は実際に働いた。** S2-d-1a でスプリアスベクタを専用スタブへ
    // 差し替えたとき、下の除外へ追加するのを忘れたまま起動したところ、
    // `entries=NG` で停止した。**列挙で守る検査は列挙に無い形を静かに通す**のが
    // 常だが、ここは逆に「表の中にあるはず」を検査しているので、**列挙から
    // 漏れると落ちる側**である。
    //
    // **ただし除外リストのほうは、静かに通す向きである。** 除外したベクタに
    // ついてここは何も見ない。その穴は check_dedicated_stubs が塞ぐ。
    let dedicated = dedicated_stubs();
    let mut entries_ok = true;
    for vector in 0..IDT_ENTRY_COUNT {
        // 0x20-0x40 は IRQ スタイルのスタブへ差し替えてあるので、こちらの
        // 範囲には入らない。別系統の check_irq_stub_table が担当する。
        if (IRQ_VECTOR_BASE..IRQ_VECTOR_BASE + IRQ_STYLE_STUB_COUNT).contains(&vector) {
            continue;
        }
        // yield（M5-c）・syscall（M5-f-1）・スプリアス（S2-d-1a）は例外表の外の
        // 専用スタブを指す。**飛ばす根拠と、その先を確かめる検査が同じ配列を
        // 読む**ので、片方だけ更新して食い違わせられない。
        if dedicated
            .iter()
            .any(|(dedicated_vector, _)| *dedicated_vector == vector)
        {
            continue;
        }
        let Some(entry) = entry(vector) else {
            entries_ok = false;
            break;
        };
        let handler = entry.handler_address();
        if handler < base || handler >= end {
            entries_ok = false;
            break;
        }
        let offset = handler - base;
        if !offset.is_multiple_of(STUB_SIZE as u64) || offset / STUB_SIZE as u64 != vector as u64 {
            entries_ok = false;
            break;
        }
    }

    StubTableCheck {
        base,
        end,
        actual_size: end - base,
        expected_size,
        stride_ok,
        entries_ok,
    }
}

/// `n` 番目のスタブのアドレス。
fn stub_address(vector: usize) -> u64 {
    let base = addr_of!(zaytos_exception_stubs) as u64;
    base + (vector * STUB_SIZE) as u64
}

/// `lidt` / `sidt` が扱うディスクリプタテーブルレジスタの形。
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// IDT を構築して `lidt` でロードする。
///
/// 全 256 ベクタに割り込みゲート（DPL 0）を入れる。`double_fault_ist_index`
/// を指定すると、ダブルフォルト（ベクタ 8）だけがその IST スタックへ、
/// `page_fault_ist_index` を指定すると、ページフォルト（ベクタ 14）だけが
/// その IST スタックへ切り替わる（M5-b、ADR-0019 §3.1）。
///
/// # Safety
///
/// - 起動時に 1 回だけ呼ぶこと。
/// - 呼び出し時点で割り込みが禁止されていること。
/// - 自前の GDT がロード済みで、[`KERNEL_CODE_SELECTOR`] が有効な 64bit
///   コードセグメントを指していること。
/// - IST インデックスを指定する場合、TSS の当該 IST エントリに有効で
///   マップ済みのスタック上端が設定済みであること。
pub unsafe fn init(double_fault_ist_index: Option<u8>, page_fault_ist_index: Option<u8>) {
    // SAFETY: 起動時の単一実行文脈であり、他に誰もこの static に触れていない。
    unsafe {
        let idt = addr_of!(IDT) as *mut [IdtEntry; IDT_ENTRY_COUNT];
        for vector in 0..IDT_ENTRY_COUNT {
            // ダブルフォルト（8）とページフォルト（14）は IST を使う。通常の
            // スタックが壊れている可能性がある例外なので、別スタックへ移る。
            // #PF はスタックオーバーフローで発生しうるため、溢れたスタックの
            // 上でハンドラを動かすとさらに #PF が起きて #DF へ昇格し、CR2 が
            // 失われる（ADR-0019 §3.1）。
            let ist = match vector {
                8 => double_fault_ist_index,
                14 => page_fault_ist_index,
                _ => None,
            };
            (*idt)[vector] = IdtEntry::new(
                stub_address(vector),
                KERNEL_CODE_SELECTOR,
                GateType::Interrupt,
                0,
                ist,
            );
        }

        // 0x20-0x30 を IRQ スタイルのスタブへ上書きする。**例外を IRQ 化
        // してはならない。** 例外ハンドラは戻ってはいけない（たとえば #DE
        // からそのまま戻れば、同じ除算命令を再実行して無限ループになる）。
        // 戻れるのは、原因が外部にあり再実行の必要がないものだけである。
        for index in 0..IRQ_STYLE_STUB_COUNT {
            let vector = IRQ_VECTOR_BASE + index;
            (*idt)[vector] = IdtEntry::new(
                irq_stub_address(index),
                KERNEL_CODE_SELECTOR,
                GateType::Interrupt,
                0,
                None,
            );
        }

        // 協調的 yield 用のゲート（M5-c）。専用スタブが zaytos_irq_common へ
        // jmp するので、int YIELD_VECTOR が IRQ の退避・復元・スイッチ経路に
        // 載る。割り込みゲート（IF を落とす）にする。
        (*idt)[YIELD_VECTOR] = IdtEntry::new(
            addr_of!(zaytos_yield_stub) as u64,
            KERNEL_CODE_SELECTOR,
            GateType::Interrupt,
            0,
            None,
        );

        // Local APIC のスプリアス割り込み用ゲート（S2-d-1）。**IRQ スタイルの
        // スタブへ載せる。** 既定では例外スタイルのスタブが入っており、
        // 起きるとダンプして停止する。Local APIC が配送を担い始めるとスプリアスは
        // 実際に起こりうるので、戻れる経路へ移す（EOI は送らない。判定は
        // `irq_entry` にある）。
        (*idt)[crate::apic::SPURIOUS_VECTOR as usize] = IdtEntry::new(
            addr_of!(zaytos_spurious_stub) as u64,
            KERNEL_CODE_SELECTOR,
            GateType::Interrupt,
            0,
            None,
        );

        // I/O APIC 経由のキーボード用ゲート（S2-d-1c）。専用スタブへ載せる。
        // **配送を切り替える前に置く。** ゲートが無い状態で redirection entry の
        // マスクを外すと、最初のキー入力で例外スタイルのスタブへ落ちて停止する。
        (*idt)[IOAPIC_KEYBOARD_VECTOR] = IdtEntry::new(
            addr_of!(zaytos_ioapic_keyboard_stub) as u64,
            KERNEL_CODE_SELECTOR,
            GateType::Interrupt,
            0,
            None,
        );

        // Local APIC タイマ用ゲート（S2-d-2）。専用スタブへ載せる。
        // **LVT のマスクを外す前に置く。** ゲートが無い状態で解禁すると、
        // 最初のティックで例外スタイルのスタブへ落ちて停止する。
        (*idt)[LAPIC_TIMER_VECTOR] = IdtEntry::new(
            addr_of!(zaytos_lapic_timer_stub) as u64,
            KERNEL_CODE_SELECTOR,
            GateType::Interrupt,
            0,
            None,
        );

        // システムコール用ゲート（M5-f-1、ADR-0020）。ベクタ 0x80。DPL は
        // SYSCALL_GATE_DPL（通常 3）。**DPL=3** で Ring 3 から int 0x80 を呼べる
        // ようにする（他のゲートは DPL=0）。割り込みゲート（IF を落とす）で ADR-0018
        // の「入場時 IF=0」を保つ。IST は使わず、特権変化のたびに CPU が TSS.RSP0 の
        // スタックへ切り替える。
        (*idt)[SYSCALL_VECTOR] = IdtEntry::new(
            addr_of!(zaytos_syscall_stub) as u64,
            KERNEL_CODE_SELECTOR,
            GateType::Interrupt,
            SYSCALL_GATE_DPL,
            None,
        );
    }

    let pointer = DescriptorTablePointer {
        limit: (IDT_ENTRY_COUNT * core::mem::size_of::<IdtEntry>() - 1) as u16,
        base: addr_of!(IDT) as u64,
    };

    // SAFETY: pointer は今組み立てた有効な IDT を指す。呼び出し側の契約により
    // 割り込みは禁止されている。
    unsafe {
        core::arch::asm!(
            "lidt [{ptr}]",
            ptr = in(reg) &pointer,
            options(readonly, nostack, preserves_flags),
        );
    }
}

/// 既に構築済みの IDT を、このコアの IDTR へ載せる（S3-b-2b-2）。
///
/// # **IDT は 1 本を共有する。GDT / TSS と違って per-CPU ではない**
///
/// ゲートの中身はコアに依存しない（ハンドラも、IST の**番号**も同じ）。
/// **コアごとに違うのは IST が指す先で、それは TSS が持つ。**
/// したがって IDT の実体は共有し、**各コアが `lidt` でそれを指すだけでよい。**
///
/// # Safety
///
/// [`init`] が既に走って IDT が構築済みであること。割り込みは禁止されていること。
/// 各コアにつき 1 回だけ呼ぶこと。
pub unsafe fn load_shared() {
    let pointer = DescriptorTablePointer {
        limit: (IDT_ENTRY_COUNT * core::mem::size_of::<IdtEntry>() - 1) as u16,
        base: addr_of!(IDT) as u64,
    };
    // SAFETY: 呼び出し側の契約により IDT は構築済みで、割り込みは禁止されている。
    unsafe {
        core::arch::asm!(
            "lidt [{ptr}]",
            ptr = in(reg) &pointer,
            options(readonly, nostack, preserves_flags),
        );
    }
}

/// 現在ロードされている IDT の位置と limit（`sidt` の読み戻し）。
pub fn current_idt() -> (u64, u16) {
    let mut pointer = DescriptorTablePointer { limit: 0, base: 0 };
    // SAFETY: sidt は IDTR を読むだけで副作用が無い。書き込み先は
    // このスタックフレーム上の有効な領域。
    unsafe {
        core::arch::asm!(
            "sidt [{ptr}]",
            ptr = in(reg) &mut pointer,
            options(nostack, preserves_flags),
        );
    }
    (pointer.base, pointer.limit)
}

/// 自前の IDT の先頭アドレス。読み戻しの照合に使う。
pub fn idt_base() -> u64 {
    addr_of!(IDT) as u64
}

/// IDT が占めるバイト数から求めた limit（= サイズ - 1）。
pub fn expected_limit() -> u16 {
    (IDT_ENTRY_COUNT * core::mem::size_of::<IdtEntry>() - 1) as u16
}

/// 指定ベクタのエントリを読み出す（検証用）。
pub fn entry(vector: usize) -> Option<IdtEntry> {
    if vector >= IDT_ENTRY_COUNT {
        return None;
    }
    // SAFETY: 範囲内であることを直前に確認した。読み取りのみ。
    unsafe {
        let idt = addr_of!(IDT);
        Some((*idt)[vector])
    }
}

/// 指定ベクタの Present ビットを落とす。
///
/// ダブルフォルトの誘発テスト（M4-b-2）専用。
///
/// # Safety
///
/// このベクタの例外が発生すると、ハンドラ不在によりダブルフォルトへ
/// 昇格する。テスト以外で呼ばないこと。
pub unsafe fn clear_present(vector: usize) {
    if vector >= IDT_ENTRY_COUNT {
        return;
    }
    // SAFETY: 範囲内。起動時の単一実行文脈で、他に誰も触れていない。
    unsafe {
        let idt = addr_of!(IDT) as *mut [IdtEntry; IDT_ENTRY_COUNT];
        (*idt)[vector].clear_present();
    }
}

/// 例外の共通処理。レジスタ一式をシリアルへ出して停止する。
///
/// スタブから `extern "sysv64"` で呼ばれる。Rust の既定 ABI はレイアウトが
/// 安定していないため、アセンブリから呼ぶ関数には使えない（M2-0c の
/// カーネルエントリと同じ理由）。
///
/// **確保もロックもコンソールも使わない。** シリアルへ直接書く。例外
/// ハンドラ自身がフォルトするとダブルフォルトになるため、依存を最小に
/// する（ADR-0018）。エラーコードの解釈も `&'static str` を返すだけの
/// 純粋関数で行い、文字列を組み立てない。
///
/// # Safety
///
/// `context` はスタブが積んだ [`ExceptionContext`] を指していること。
extern "sysv64" fn exception_entry(context: *const ExceptionContext, rsp_at_call: u64) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();

    use core::fmt::Write;

    // SAFETY: スタブが直前に積んだ有効な ExceptionContext を指す。
    // 読み取りのみで、この関数は戻らない。
    let context = unsafe { &*context };

    // M5-e-3: Ring 3 遠征の予期した #GP だけを畳む。**二重判別（+RIP 照合）を
    // 全て満たすときのみ**畳んでカーネルへ戻る。1 つでも欠ける全ての例外は、
    // この分岐を素通りして下の dump+halt へ落ちる（従来と 1 ビットも変わらない）。
    //   (1) ベクタ==13（#GP）
    //   (2) 例外フレームの CS の RPL==3（Ring 3 由来。カーネル由来は CS.RPL=0 で
    //       ここで弾かれる）
    //   (3) 遠征フラグが立っている（遠征外の Ring 3 #GP は畳まない）
    //   (4) フォルト RIP がユーザーコード入口である（別 RIP は畳まない）
    // (3)(4) は crate::ring3::should_fold_gp が見る。
    if context.vector as u8 == 13
        && (context.cs & 0b11) == 3
        && crate::ring3::should_fold_gp(context.rip)
    {
        // SAFETY: 上の 4 条件が全て真。遠征中で RECOVERY は保存済み。longjmp で
        // 遠征の呼び出し元へ戻る（戻らない）。dump は行わない。
        unsafe {
            crate::ring3::record_and_fold(context.cs, context.rsp, rsp_at_call);
        }
    }

    // 既存の境界計算が正しいことの裏取り。IRQ 側と同じ検査を通す。
    check_stack_alignment(rsp_at_call, "exception", context.vector);

    let vector = context.vector as u8;
    let name = exception_name(vector);
    let _ = writeln!(
        serial,
        "[ERROR] exception: vector={} ({name})",
        context.vector
    );

    dump_error_code(&mut serial, vector, context.error_code);

    let _ = writeln!(
        serial,
        "[ERROR]   rip={:#018x} cs={:#06x} rflags={:#x}",
        context.rip, context.cs, context.rflags
    );
    let _ = writeln!(
        serial,
        "[ERROR]   rsp={:#018x} ss={:#06x} (at the time of the fault)",
        context.rsp, context.ss
    );

    // 汎用レジスタ。4 個ずつ並べる。
    let registers = context.general_purpose_registers();
    for chunk in registers.chunks(4) {
        let _ = write!(serial, "[ERROR]  ");
        for (name, value) in chunk {
            let _ = write!(serial, " {name}={value:#018x}");
        }
        let _ = writeln!(serial);
    }

    // CR2 は #PF のときだけ意味を持つ。それ以外では直前の #PF の残骸か
    // 未定義の値なので、そうと分かる形で出す。
    if vector == 14 {
        let _ = writeln!(
            serial,
            "[ERROR]   cr2={:#018x} (faulting address)",
            context.cr2
        );
        // スタックオーバーフローを自己識別する。CR2 がカーネルスタックの
        // ガードページ内なら、この #PF は溢れによるものである（M5-b）。
        let guard = crate::stack::kernel_guard_page();
        let in_guard =
            common::addr::VirtAddr::new(context.cr2).is_some_and(|cr2| guard.contains(cr2));
        let _ = writeln!(
            serial,
            "[ERROR]   cr2 is in the kernel stack guard page = {in_guard} (guard {:#x}..{:#x})",
            guard.bottom.as_u64(),
            guard.top.as_u64()
        );
    } else {
        let _ = writeln!(
            serial,
            "[ERROR]   cr2={:#018x} (not meaningful for this exception)",
            context.cr2
        );
    }

    // ダブルフォルト（8）とページフォルト（14）は IST で別スタックへ
    // 切り替わっているはず。実際に切り替わったかを、このフレーム自身の位置で
    // 確かめる。切り替わっていなければ、壊れた可能性のあるスタックの上で
    // ハンドラが動いている（#PF がスタックオーバーフローで起きた場合、これが
    // 効いていないと #DF へ昇格して CR2 が失われる。ADR-0019 §3.1）。
    let ist_stack = match vector {
        8 => Some((1u8, crate::stack::double_fault_stack_range())),
        14 => Some((2u8, crate::stack::page_fault_stack_range())),
        _ => None,
    };
    if let Some((ist_number, ist)) = ist_stack {
        let handler_rsp = context as *const ExceptionContext as u64;
        let on_ist = common::addr::VirtAddr::new(handler_rsp).is_some_and(|rsp| ist.contains(rsp));
        let _ = writeln!(
            serial,
            "[ERROR]   handler frame at {handler_rsp:#018x}, IST{ist_number} stack {:#x}..{:#x}, on IST{ist_number}={on_ist}",
            ist.bottom.as_u64(),
            ist.top.as_u64()
        );
    }

    let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");

    cpu::halt_forever();
}

/// エラーコードをベクタに応じて解釈して出す。
fn dump_error_code(serial: &mut SerialPort, vector: u8, error_code: u64) {
    use core::fmt::Write;

    match error_code_kind(vector) {
        ErrorCodeKind::None => {
            let _ = writeln!(serial, "[ERROR]   error code = (none for this exception)");
        }
        ErrorCodeKind::AlwaysZero => {
            // #DF のエラーコードは Intel SDM により常に 0 と決まっている。
            let _ = writeln!(
                serial,
                "[ERROR]   error code = {error_code:#x} (always zero for #DF)"
            );
        }
        ErrorCodeKind::PageFault => {
            let code = PageFaultErrorCode(error_code);
            let _ = writeln!(serial, "[ERROR]   error code = {error_code:#x}");
            let _ = writeln!(
                serial,
                "[ERROR]     cause={} access={} mode={}",
                code.cause(),
                code.access(),
                code.mode()
            );
            if code.is_reserved_bit_violation() {
                let _ = writeln!(
                    serial,
                    "[ERROR]     reserved bit set in a page table entry (page table is malformed)"
                );
            }
            if code.is_protection_key_violation() {
                let _ = writeln!(serial, "[ERROR]     protection key violation");
            }
            if code.is_shadow_stack() {
                let _ = writeln!(serial, "[ERROR]     shadow stack access");
            }
        }
        ErrorCodeKind::Selector => {
            let code = SelectorErrorCode(error_code);
            let _ = writeln!(serial, "[ERROR]   error code = {error_code:#x}");
            if code.is_null() {
                let _ = writeln!(serial, "[ERROR]     not caused by a specific descriptor");
            } else {
                let _ = writeln!(
                    serial,
                    "[ERROR]     table={} index={} external={}",
                    code.table().as_str(),
                    code.index(),
                    code.is_external()
                );
            }
        }
        ErrorCodeKind::Raw => {
            let _ = writeln!(
                serial,
                "[ERROR]   error code = {error_code:#x} (vector specific)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_is_the_size_minus_one() {
        // 256 エントリ x 16 バイト = 4096 バイト。limit はその 1 つ手前。
        assert_eq!(IDT_ENTRY_COUNT * core::mem::size_of::<IdtEntry>(), 4096);
        assert_eq!(expected_limit(), 4095);
    }

    /// スタブの間隔が 16 バイトであること。アセンブリ側の `.p2align 4` と
    /// この定数がずれると、テーブルが全く別のアドレスを指す。
    #[test]
    fn the_stub_stride_matches_the_alignment_used_in_assembly() {
        assert_eq!(STUB_SIZE, 16);
    }
}
