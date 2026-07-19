#![no_std]
#![no_main]

#[cfg(not(feature = "panic-test"))]
use kernel::cpu;
use kernel::log::{LogLevel, Logger};
use kernel::serial::SerialPort;
use uefi::prelude::*;
use uefi::println;

mod panic;

#[entry]
fn efi_main() -> Status {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let mut logger = Logger::new(serial, LogLevel::Trace);

    logger.info(format_args!("ZaytOS M1: serial log established (COM1)"));

    println!("Hello, ZaytOS!");
    logger.info(format_args!("printed \"Hello, ZaytOS!\" to UEFI console"));

    logger.info(format_args!("M1 bring-up complete; halting"));

    // `cargo xtask run --panic-test` によるパニックハンドラの回帰チェック用。
    // 通常ビルドではこの分岐は含まれず、fail-fast 方針 (ADR-0004) に影響しない。
    #[cfg(feature = "panic-test")]
    panic!("xtask panic-test: deliberate panic to exercise the panic handler");

    #[cfg(not(feature = "panic-test"))]
    cpu::halt_forever();
}
