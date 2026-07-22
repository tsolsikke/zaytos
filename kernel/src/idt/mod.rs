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

use core::ptr::addr_of;
use core::sync::atomic::{AtomicU64, Ordering};

use common::cpu;
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
    // 積んだものを、積んだ順序の逆に、同じ量だけ正確に取り除く。
    // iretq は RSP が CPU の積んだフレームの先頭（RIP）を指している状態で
    // 実行されなければならない。1 バイトでもずれると制御が飛ぶ。
    "  add rsp, {adjust}",
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
    handler = sym irq_entry,
    adjust = const STACK_ALIGN_ADJUST,
);

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
/// 0x30 にもなりうる（`pic::MASTER_VECTOR_OFFSET`）。ここはスタブ表が
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

/// ベクタ別の割り込み回数。
///
/// **通常の `static` にしてはならない。** メインループがこれを読む形になる
/// ため、通常の変数だとコンパイラが読み出しをループの外へ巻き上げ、
/// 値が永久に変わらないように見える。「割り込みは来ているのにメインループが
/// 気づかない」という診断しにくい症状になる（ADR-0018 のチェックリスト 9）。
/// `Relaxed` で十分なのは、シングルコアで順序に依存した判断をしないため。
static INTERRUPT_COUNTS: [AtomicU64; IDT_ENTRY_COUNT] =
    [const { AtomicU64::new(0) }; IDT_ENTRY_COUNT];

/// タイマ（IRQ0 = ベクタ 0x20）のティック数。
///
/// [`INTERRUPT_COUNTS`] とは別に持つ。ティックは「時間の流れ」として
/// 頻繁に読む値であり、ベクタ番号での添字を経由せず直接読めるほうが
/// メインループの意図が読み取りやすい。
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

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

/// タイマのティック数を読む。
pub fn timer_ticks() -> u64 {
    TIMER_TICKS.load(Ordering::Relaxed)
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
fn check_stack_alignment(rsp_at_call: u64, path: &str, vector: u64) {
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
extern "sysv64" fn irq_entry(context: *const IrqContext, rsp_at_call: u64) {
    // SAFETY: スタブが直前に積んだ有効な IrqContext を指す。読み取りのみ。
    let context = unsafe { &*context };

    check_stack_alignment(rsp_at_call, "irq", context.vector);

    let vector = context.vector as usize;
    if vector < IDT_ENTRY_COUNT {
        INTERRUPT_COUNTS[vector].fetch_add(1, Ordering::Relaxed);
    }

    // PIC の範囲で最初に届いたベクタを 1 度だけ記録する。ICW2 の検証に使う。
    if pic_irq_for(vector).is_some() {
        let _ = FIRST_PIC_VECTOR.compare_exchange(
            NO_VECTOR_YET,
            context.vector,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    // PIC 由来の IRQ かどうか。テスト専用ベクタ（0x30、PIC の範囲外）は
    // ここに入らないので、EOI の論理が一切絡まない。
    let pic_irq = pic_irq_for(vector);

    if let Some(irq) = pic_irq {
        if vector == TIMER_VECTOR {
            TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
        }

        // キーボード（IRQ1）。**EOI より先に呼ぶ。** この中でデータポートを
        // 読み切らないと、コントローラの出力バッファが空かず次の IRQ1 が
        // 来なくなる。
        if vector == crate::keyboard::KEYBOARD_VECTOR {
            crate::keyboard::handle_irq(context.vector);
        }

        // スプリアス（偽）割り込みの判定。IRQ7 / IRQ15 でしか起きない。
        // 本物なら ISR の該当ビットが立っている。
        //
        // SAFETY: 割り込みハンドラの中であり、割り込みゲート経由で入場した
        // ため IF=0。他の実行文脈が同時に PIC を触ることはない。
        let isr = unsafe { crate::pic::read_isr() };
        let spurious = crate::pic::is_spurious(irq, isr);
        if spurious {
            SPURIOUS_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        // **処理を終えてから EOI を送る。** 送った時点で PIC は次の同じ
        // 割り込みを上げられるようになる。宛先は純粋ロジックが決める
        // （スプリアスの扱いはマスタ側とスレーブ側で非対称）。
        //
        // SAFETY: action は eoi_action_for が返した値そのものである。
        #[cfg(not(feature = "no-eoi-test"))]
        unsafe {
            crate::pic::send_eoi_for(crate::pic::eoi_action_for(irq, spurious));
        }
    }

    // ここで出力してはならない（ADR-0018 §5）。100Hz で毎回ログを出すと
    // 出力自体がハンドラの処理時間を支配し、ティックを取りこぼす。観測は
    // メインループがカウンタ越しに行う。
}

/// タイマ（IRQ0）のベクタ。
///
/// PIC のベクタオフセットに追随する。`alt-offset-test` では 0x30 になる。
pub const TIMER_VECTOR: usize = crate::pic::MASTER_VECTOR_OFFSET as usize;

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

/// ベクタ番号から PIC の IRQ 番号を求める。PIC 由来でなければ `None`。
///
/// テスト専用ベクタ（[`TEST_VECTOR`]）は PIC の範囲外なので `None` になり、
/// EOI の経路へ入らない。
fn pic_irq_for(vector: usize) -> Option<u8> {
    let base = crate::pic::MASTER_VECTOR_OFFSET as usize;
    if (base..base + PIC_IRQ_COUNT).contains(&vector) {
        Some((vector - base) as u8)
    } else {
        None
    }
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
    let mut entries_ok = true;
    for vector in 0..IDT_ENTRY_COUNT {
        // 0x20-0x40 は IRQ スタイルのスタブへ差し替えてあるので、こちらの
        // 範囲には入らない。別系統の check_irq_stub_table が担当する。
        if (IRQ_VECTOR_BASE..IRQ_VECTOR_BASE + IRQ_STYLE_STUB_COUNT).contains(&vector) {
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
/// を指定すると、ダブルフォルト（ベクタ 8）だけがその IST スタックへ
/// 切り替わる。
///
/// # Safety
///
/// - 起動時に 1 回だけ呼ぶこと。
/// - 呼び出し時点で割り込みが禁止されていること。
/// - 自前の GDT がロード済みで、[`KERNEL_CODE_SELECTOR`] が有効な 64bit
///   コードセグメントを指していること。
/// - `double_fault_ist_index` を指定する場合、TSS の当該 IST エントリに
///   有効でマップ済みのスタック上端が設定済みであること。
pub unsafe fn init(double_fault_ist_index: Option<u8>) {
    // SAFETY: 起動時の単一実行文脈であり、他に誰もこの static に触れていない。
    unsafe {
        let idt = addr_of!(IDT) as *mut [IdtEntry; IDT_ENTRY_COUNT];
        for vector in 0..IDT_ENTRY_COUNT {
            // ダブルフォルトだけ IST を使う。通常のスタックが壊れている
            // 可能性がある例外なので、無条件で別スタックへ移る。
            let ist = if vector == 8 {
                double_fault_ist_index
            } else {
                None
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
    } else {
        let _ = writeln!(
            serial,
            "[ERROR]   cr2={:#018x} (not meaningful for this exception)",
            context.cr2
        );
    }

    // ダブルフォルトは IST で別スタックへ切り替わっているはず。実際に
    // 切り替わったかを、このフレーム自身の位置で確かめる。切り替わって
    // いなければ、壊れた可能性のあるスタックの上でハンドラが動いている。
    if vector == 8 {
        let handler_rsp = context as *const ExceptionContext as u64;
        let ist = crate::stack::double_fault_stack_range();
        let on_ist = ist.contains(handler_rsp);
        let _ = writeln!(
            serial,
            "[ERROR]   handler frame at {handler_rsp:#018x}, IST1 stack {:#x}..{:#x}, on IST1={on_ist}",
            ist.bottom, ist.top
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
