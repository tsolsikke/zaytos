//! ZaytOS kernel（M2-0b: 最小骨組み）。
//!
//! この時点では ELF ローダー（M2-0c）が未実装のため、この kernel は
//! まだ実行されない。`readelf`/`objdump` でリンカスクリプト
//! （`link.ld`, ADR-0009）通りの配置になっているかを検証するための
//! ビルド対象として存在する。

#![no_std]
#![no_main]

use common::cpu;
use common::log::{LogLevel, Logger};
use common::serial::SerialPort;

mod panic;

/// ELF のエントリポイント（`link.ld` の `ENTRY(_start)` に対応）。
/// bootloader から BootInfo を受け取る呼び出し規約は M2-0c で設計・
/// レビューする。それまでは引数なしの最小形にとどめる。
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // bootloader 側で COM1 が初期化済みでも、再初期化は同じハードウェアを
    // 触るだけで副作用がないため、kernel 単体でも自己完結して動くように
    // ここでも初期化する（panic.rs と同じ考え方）。
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let mut logger = Logger::new(serial, LogLevel::Trace);

    logger.info(format_args!("ZaytOS kernel: entered _start"));

    cpu::halt_forever();
}
