//! x86_64 に固有のコード（`ADR-0071` の決定 1 の 2 で、共通の側から移す）。

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
pub use gdt::{active_kernel_entry_stack_top, set_active_kernel_entry_stack_top};
pub use idt::timer_ticks;
pub use task_frame::{build_initial_context, raise_yield_interrupt};
