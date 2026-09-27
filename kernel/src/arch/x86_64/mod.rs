//! x86_64 に固有のコード（`ADR-0071` の決定 1 の 2 で、共通の側から移す）。

pub mod cpu_state;
pub mod fp;
pub mod gdt;
pub mod idt;
pub mod interrupt_readiness;
pub mod paging;
pub mod ring3;
pub mod stack;
