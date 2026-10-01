//! PC に固有のコード（`ADR-0071` の決定 1 の 2 で、共通の側から移す）。

pub mod serial;

// 共通の側から呼ぶ境界の関数と型（`ADR-0071` の決定 1 の 2。2026-09-28）。共通の側（`main.rs` を除く）は、
// ここに並べた名前で呼ぶ。並べる名前は機械に依らない名前にし、`machine` の中でだけ使うものは並べない。
pub use serial::{
    open_direct_serial, serial_forced_write_count, serial_reentry_count,
    serial_setup_gave_up_count, serial_setup_write_count, Serial,
};
