//! PC に固有のコード（ACPI・APIC・8259・PIT・PCI・i8042。`ADR-0071` の決定 1 の 2 で、共通の側から移す）。

pub mod acpi;
pub mod ap_start;
pub mod apic;
pub mod i8042;
pub mod irq;
pub mod lapic_timer;
pub mod pci;
pub mod pmtimer;

// 共通の側から呼ぶ境界の関数と型（`ADR-0071` の決定 1 の 2。2026-09-28）。共通の側（`main.rs` を除く）は、
// ここに並べた名前で呼ぶ。並べる名前は機械に依らない名前にし、`machine` の中でだけ使うものは並べない。
//
// **`IsaIrq` と `PciIntx` はバスの番号の名前である**（2026-09-29。9e。`ADR-0072` の 3）。装置のドライバが「この装置の
// 割り込み」を解決させるのに名指しする（キーボードは ISA の IRQ 1、virtio-blk は PCI の INTx）。CPU の名前ではない。
pub use acpi::MadtSurvey;
pub use ap_start::{
    record_started_processor, start_processor, started_processor, ProcessorId, StartAddress,
};
pub use apic::{
    calibrate_local_timer, CalibrationClock, CalibrationReference, MappedInterruptController,
};
pub use i8042::{keyboard_data_ready, read_keyboard_data};
pub use irq::{
    claim, complete, delivered_count, disable_and_complete,
    enable_interrupt_controller_for_this_cpu, enable_local_timer_for_this_cpu, first_arrival,
    probe_ipi, send_ipi_probe, service_snapshot, source_for_isa_irq, source_for_pci_intx,
    spurious_counts, survey_interrupt_masks, timer_frequency_hz, Arrival, Claim, FirstArrival,
    IsaIrq, PciIntx,
};
pub use lapic_timer::switch_to_local_timer;
pub use pci::{RegisterCell, RegisterWindow, VirtioBlkLocation};
