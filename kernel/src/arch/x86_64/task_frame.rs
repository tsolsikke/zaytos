//! 新しいタスクの最初の文脈（偽の `IrqContext`）を作る。yield のベクタへ割り込みを出す。
//!
//! **`kernel/src/task.rs` から移した**（2026-09-28。境界の段階の手順 2）。**割り込みのフレームの並び・セグメントの
//! セレクタ・RFLAGS・ベクタを積むので、CPU 固有の置き場に置く。** **どのタスクをいつ作るかは `task` に残る。**
//! **yield の `int`（[`raise_yield_interrupt`]）も、同じ日に `task::yield_now` から移した。** 偽の文脈は、
//! この `int` で入ったときと同じ形に積む。**いつ yield するかは `task` に残る。**

use common::addr::VirtAddr;

use crate::arch::x86_64::gdt;
use crate::arch::x86_64::idt::YIELD_VECTOR;

/// `IrqContext` のバイト数（21 個の `u64`）。偽コンテキストの大きさに使う。
const IRQ_CONTEXT_BYTES: u64 = 21 * 8;

/// 新規タスクの偽 `IrqContext` をスタック頂点に積み、保存 RSP を返す。
///
/// 初回スイッチで [`on_yield`](crate::task::on_yield) がこの RSP を返すと、`mov rsp, rax` → pop 15 →
/// `add rsp, 8` → `iretq` の経路が、あたかも割り込みから戻るように `entry` へ
/// IF=1 で入る。
///
/// # 契約（境界の関数。2026-09-30）
///
/// - 書くのは `top` の直下の、割り込みの文脈 1 つぶんだけである（それより下には触れない）。
/// - 呼ぶのはタスクを作る所（`crate::task`）で、返した値を、そのタスクの保存するスタックポインタにする。
///
/// # Safety
///
/// `top` が有効でマップ済みのスタック頂点（16 バイト境界）であること。
pub unsafe fn build_initial_context(top: VirtAddr, entry: u64) -> u64 {
    let saved_stack_pointer = top.as_u64() - IRQ_CONTEXT_BYTES;
    // saved_stack_pointer から上へ 21 個の u64 を並べる（IrqContext のフィールド順）。
    // 0..15: GPR（rax..r15）、15: vector、16: rip、17: cs、18: rflags、
    // 19: rsp、20: ss。
    let slot = |i: usize, value: u64| {
        // SAFETY: 呼び出し元契約により、[saved_stack_pointer, top) はマップ済みで誰も
        // 使っていないスタック領域。i < 21。
        unsafe {
            core::ptr::write_volatile((saved_stack_pointer as *mut u64).add(i), value);
        }
    };
    for i in 0..15 {
        slot(i, 0); // GPR は 0 で始める。ワーカーは自分で base を読み直す。
    }
    slot(15, YIELD_VECTOR as u64); // vector（add rsp,8 で捨てられる）
    slot(16, entry); // rip
    slot(17, gdt::KERNEL_CODE_SELECTOR.bits() as u64); // cs
    slot(18, 0x202); // rflags（IF=1、予約ビット1）
    slot(19, top.as_u64()); // rsp（iretq 後にタスクが使う RSP）
    slot(20, gdt::KERNEL_DATA_SELECTOR.bits() as u64); // ss
    saved_stack_pointer
}

/// yield のベクタへソフトウェア割り込みを出し、切り替えの経路へ入る（[`crate::task::yield_now`] の中身）。
///
/// 次に呼び出し元のタスクが選ばれると、この `int` の直後へ戻る。
///
/// # 契約（境界の関数。2026-09-30）
///
/// - 呼ぶのは [`crate::task::yield_now`] だけである。戻るのは、次にこのタスクが選ばれたときである。
#[inline(always)]
pub fn raise_yield_interrupt() {
    // SAFETY: yield_vector のゲートは IDT に登録済みで、専用スタブ経由で
    // 共通ルーチンへ入る。レジスタは呼び出し規約どおりクロバー扱いにする。
    unsafe {
        core::arch::asm!(
            "int {yv}",
            yv = const YIELD_VECTOR,
            clobber_abi("sysv64"),
        );
    }
}
