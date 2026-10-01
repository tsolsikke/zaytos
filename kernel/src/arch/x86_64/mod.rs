//! x86_64 に固有のコード（`ADR-0071` の決定 1 の 2 で、共通の側から移す）。

pub mod ap_bring_up;
pub mod ap_stacks;
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
pub use ap_bring_up::{bring_up_application_processor, ApBringUp, ApStacks};
pub use ap_stacks::{kernel_stack_bounds_from_top, map_ap_stacks};
pub use ap_trampoline::{ap_stack_frame, install_trampoline, trampoline_frame};
pub use cpu_state::{check_aps_match_bsp, record_this_ap};
#[cfg(feature = "fp-clobber-on-kernel-entry-test")]
pub use fp::clobber_fp_state_on_kernel_entry;
pub use fp::{restore_fp_state, save_fp_state, FpArea};
pub use gdt::{active_kernel_entry_stack_top, set_active_kernel_entry_stack_top};
pub use idt::context::IrqContext;
pub use idt::{
    advance_monotonic_ticks, count_timer_tick, entries_from_direction_flag_set, first_tick_arrival,
    ipi_probe_received_for, ipi_probe_sent, max_kernel_entry_depth, monotonic_ticks,
    record_ipi_probe_sent, timer_accounting_balances, timer_delivery, timer_delivery_count,
    timer_ticks, timer_ticks_for, timer_ticks_total, EntryPath, ExitAction, Interrupted,
    KernelEntryGuard,
};
pub use paging::active::{ActivePageTable, PageAttributes};
pub use paging::address_space::{AddressSpace, AddressSpaceError};
pub use paging::switch::{active_page_table_root, set_active_page_table_root};
pub use paging::verify::{
    read_top_level_entry, walk_page_table, walk_page_table_user_accessible, UserAccess,
};
pub(crate) use ring3::current_excursion_slot;
pub use ring3::{
    current_excursion_recovery, excursion_depth, excursion_fault_number, excursion_interrupted,
    excursion_recovery_belongs_to_slot, excursion_stack_canary_intact, excursion_stack_capacity,
    excursion_stack_high_water, excursion_stack_range, excursion_stack_range_at,
    excursion_stack_range_of, excursion_stack_within_budget, leave_user_mode,
    note_kernel_entry_from_user, note_return_to_user, restore_fold_record, run_excursion,
    save_fold_record, set_current_excursion_recovery, MAX_EXCURSION_DEPTH, USER_TASK_SLOTS,
};
pub use stack::{
    check_entry_stack_alignment, install_guard_page, kernel_stack_capacity,
    kernel_stack_high_water, kernel_stack_range, KERNEL_STACK_FILL,
};
pub use task_frame::{build_initial_context, raise_yield_interrupt};
