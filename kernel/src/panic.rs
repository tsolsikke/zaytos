//! パニックハンドラ（ADR-0004: fail-fast — 即停止 + レジスタダンプ）。
//!
//! `#[panic_handler]` はビルド全体で唯一つしか定義できないため、
//! bootloader（`bootloader/src/panic.rs`）とは独立して、この kernel
//! バイナリ用に別途定義する。内容は bootloader 側と同じ方針
//! （RSP + `PanicInfo` のダンプ。GPR フルダンプは M4 に先送り、
//! ADR-0004 Addendum 参照）。

use core::fmt::Write;
use core::panic::PanicInfo;

use common::cpu;
use common::serial::SerialPort;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();

    let rsp = cpu::read_rsp();

    let _ = writeln!(serial, "[ERROR] panic: {info}");
    let _ = writeln!(serial, "[ERROR]   rsp = {rsp:#018x}");
    let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");

    cpu::halt_forever();
}
