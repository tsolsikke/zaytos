//! パニックハンドラ（ADR-0004: fail-fast — 即停止 + レジスタダンプ）。
//!
//! `#[panic_handler]` はビルド全体で唯一つしか定義できず、かつ std の
//! パニック実装と衝突するため、`no_std`/`no_main` な実バイナリである
//! この `main.rs` 側にのみ置く（`kernel` ライブラリ側はホスト向け
//! `cargo test` でも使うため、ここには置けない）。
//!
//! 起動時に確立した `Logger`/`SerialPort` の状態がパニック発生時点で
//! 引き続き有効か保証できないため、ここでは COM1 用の新しい
//! `SerialPort` を都度用意する。ハードウェア的には同じ COM1 を
//! 再初期化するだけであり、実害はない。
//!
//! 注記: ここで出力できる「レジスタ状態」は、CPU 例外発生時に
//! 割り込みスタブが積む本物の例外フレームとは異なり、パニック処理系を
//! 経由した後の値でしかない。本物の汎用レジスタスナップショットが
//! 意味を持つのは、IDT・例外ハンドラ実装後（M4）に例外フレームから
//! 読み取る場合である。現段階では、意味のある値である RSP と、
//! `PanicInfo` が提供するパニック位置・メッセージのみを出力する。

use core::fmt::Write;
use core::panic::PanicInfo;

use kernel::cpu;
use kernel::serial::SerialPort;

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
