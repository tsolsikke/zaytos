//! x86_64 に固有のコード（`ADR-0071` の決定 1 の 2 で、共通の側から移す）。

pub mod ap_trampoline;
pub mod cpu_state;
pub mod fp;
pub mod gdt;
pub mod idt;
pub mod interrupt_readiness;
pub mod paging;
pub mod ring3;
pub mod stack;
pub mod task_frame;
pub mod worker_bodies;

// 共通の側から呼ぶ境界の関数と型（`ADR-0071` の決定 1 の 2。2026-09-28）。共通の側（`main.rs` を除く）は、
// ここに並べた名前で呼ぶ。並べる名前は CPU に依らない名前にし、`arch` の中でだけ使うものは並べない。
pub use ap_trampoline::{ap_stack_frame, install_trampoline, trampoline_frame};
pub use fp::{restore_fp_state, save_fp_state, FpArea};
pub use gdt::{active_kernel_entry_stack_top, set_active_kernel_entry_stack_top};
pub use idt::timer_ticks;
pub use paging::switch::{active_page_table_root, set_active_page_table_root};
pub use ring3::{
    current_excursion_recovery, excursion_recovery_belongs_to_slot, excursion_stack_range_of,
    set_current_excursion_recovery,
};
pub use stack::{install_guard_page, kernel_stack_range, KERNEL_STACK_FILL};
pub use task_frame::{build_initial_context, raise_yield_interrupt};
