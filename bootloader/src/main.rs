#![no_std]
#![no_main]

extern crate alloc;

use common::log::{LogLevel, Logger};
use common::serial::SerialPort;
use uefi::prelude::*;
use uefi::println;

#[cfg(not(feature = "panic-test"))]
mod loader;
mod panic;

#[entry]
fn efi_main() -> Status {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let mut logger = Logger::new(serial, LogLevel::Trace);

    logger.info(format_args!(
        "ZaytOS bootloader: serial log established (COM1)"
    ));

    println!("Hello, ZaytOS!");
    logger.info(format_args!("printed \"Hello, ZaytOS!\" to UEFI console"));

    // `cargo xtask run --panic-test` によるパニックハンドラの回帰チェック用。
    // 通常ビルドではこの分岐は含まれず、fail-fast 方針 (ADR-0004) に影響しない。
    // ELF ローダー一式を経由せず、素早くパニック経路だけを検証する。
    #[cfg(feature = "panic-test")]
    panic!("xtask panic-test: deliberate panic to exercise the panic handler");

    #[cfg(not(feature = "panic-test"))]
    loader::run(logger);
}
