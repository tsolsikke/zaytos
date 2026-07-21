//! パニックハンドラ（ADR-0004: fail-fast — 即停止 + レジスタダンプ）。
//!
//! `#[panic_handler]` はビルド全体で唯一つしか定義できないため、
//! bootloader（`bootloader/src/panic.rs`）とは独立して、この kernel
//! バイナリ用に別途定義する。内容は bootloader 側と同じ方針
//! （RSP + `PanicInfo` のダンプのみ）。
//!
//! **GPR フルダンプは行わない。これは保留ではなく確定した方針である。**
//! ADR-0004 Addendum では「M4 で再検討する」としていたが、M4-b で結論が
//! 出た（ADR-0018 §4）。解禁されたのは例外ダンプだけである。`panic!()` の
//! 時点のレジスタは、フォーマット処理や関数呼び出しを経た後の値でしかなく、
//! 「パニックの原因となった状態」を表さない。この理由は M4 でも変わらない。
//! 例外ハンドラの側は、CPU が積んだフレームとスタブが push した GPR という
//! 正確な値を持つため事情が違う。

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
