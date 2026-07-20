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

use common::cpu;
use common::serial::SerialPort;

use crate::gdt::KERNEL_CODE_SELECTOR;
use context::ExceptionContext;
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
    // SysV ABI は call の直前に RSP が 16 バイト境界であることを要求する。
    // 例外入場時に CPU が RSP を 16 バイト境界へ揃えたうえで 5 個
    // （エラーコードありなら 6 個）を積み、スタブが合計 16 バイト
    // （ありなら 8 バイト）積むため、退避前は RSP % 16 == 8。
    // 上の push は 16 個 = 128 バイトで 16 の倍数なので剰余は変わらない。
    // 8 引いて境界へ合わせる（ADR-0018 の罠 12）。
    "  sub rsp, 8",
    "  call {handler}",
    // handler は戻らない契約。万一戻ってきたら未定義命令で止める。
    "  ud2",
    handler = sym exception_entry,
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
        if offset % STUB_SIZE as u64 != 0 || offset / STUB_SIZE as u64 != vector as u64 {
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
            let ist = if vector == 8 { double_fault_ist_index } else { None };
            (*idt)[vector] = IdtEntry::new(
                stub_address(vector),
                KERNEL_CODE_SELECTOR,
                GateType::Interrupt,
                0,
                ist,
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
extern "sysv64" fn exception_entry(context: *const ExceptionContext) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();

    use core::fmt::Write;

    // SAFETY: スタブが直前に積んだ有効な ExceptionContext を指す。
    // 読み取りのみで、この関数は戻らない。
    let context = unsafe { &*context };

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
                let _ = writeln!(
                    serial,
                    "[ERROR]     not caused by a specific descriptor"
                );
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
        assert_eq!(
            IDT_ENTRY_COUNT * core::mem::size_of::<IdtEntry>(),
            4096
        );
        assert_eq!(expected_limit(), 4095);
    }

    /// スタブの間隔が 16 バイトであること。アセンブリ側の `.p2align 4` と
    /// この定数がずれると、テーブルが全く別のアドレスを指す。
    #[test]
    fn the_stub_stride_matches_the_alignment_used_in_assembly() {
        assert_eq!(STUB_SIZE, 16);
    }
}
