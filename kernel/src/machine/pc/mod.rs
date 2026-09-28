//! PC に固有のコード（ACPI・APIC・8259・PIT・PCI・i8042。`ADR-0071` の決定 1 の 2 で、共通の側から移す）。

pub mod ap_start;
pub mod apic;
pub mod irq;
pub mod lapic_timer;

// 共通の側から呼ぶ境界の関数と型（`ADR-0071` の決定 1 の 2。2026-09-28）。共通の側（`main.rs` を除く）は、
// ここに並べた名前で呼ぶ。並べる名前は機械に依らない名前にし、`machine` の中でだけ使うものは並べない。
pub use ap_start::start_application_processor;
pub use irq::{enable_interrupt_controller_for_this_cpu, enable_local_timer_for_this_cpu};
pub use lapic_timer::switch_to_local_timer;
