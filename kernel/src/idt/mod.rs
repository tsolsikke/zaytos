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

pub mod layout;

use core::ptr::addr_of;

use common::cpu;
use common::serial::SerialPort;

use crate::gdt::KERNEL_CODE_SELECTOR;
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
    // スタック: [rsp]=ベクタ, [rsp+8]=エラーコード
    "  mov rdi, [rsp]",
    "  mov rsi, [rsp + 8]",
    // SysV ABI は call の直前に RSP が 16 バイト境界であることを要求する。
    // 例外入場時に CPU が RSP を 16 バイト境界へ揃えたうえで 5 個
    // （エラーコードありなら 6 個）を積み、スタブが合計 16 バイト
    // （ありなら 8 バイト）積むため、ここでは RSP % 16 == 8 になっている。
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

/// 例外の共通処理（M4-b-1 では報告して停止するだけ）。
///
/// スタブから `extern "sysv64"` で呼ばれる。Rust の既定 ABI はレイアウトが
/// 安定していないため、アセンブリから呼ぶ関数には使えない（M2-0c の
/// カーネルエントリと同じ理由）。
///
/// ロックも確保もコンソールも使わず、シリアルへ直接書く。例外ハンドラ自身が
/// フォルトするとダブルフォルトになるため、依存を最小にする（ADR-0018）。
extern "sysv64" fn exception_entry(vector: u64, error_code: u64) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();

    use core::fmt::Write;
    let name = exception_name(vector as u8);
    let _ = writeln!(serial, "[ERROR] exception: vector={vector} ({name})");
    if layout::pushes_error_code(vector as u8) {
        let _ = writeln!(serial, "[ERROR]   error code = {error_code:#x}");
    } else {
        let _ = writeln!(serial, "[ERROR]   error code = (none)");
    }
    let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");

    cpu::halt_forever();
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
