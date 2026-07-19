//! ZaytOS kernel（M2-0c: bootloader からの引き渡しを受けて起動）。

#![no_std]
#![no_main]

use common::boot_info::BootInfo;
use common::cpu;
use common::log::{LogLevel, Logger};
use common::serial::SerialPort;

mod panic;

/// .bss ゼロ埋めが実際に機能しているかを実地検証するための、意図的に
/// 非ゼロサイズの `.bss` を作る静的変数（M2-0c 固有の要求）。全要素 0
/// 初期化のため通常 `.bss`（NOBITS）に配置される。
static mut BSS_CANARY: [u8; 256] = [0; 256];

/// bootloader の ELF ローダーから制御を受け取るエントリポイント
/// （`link.ld` の `ENTRY(_start)` に対応）。
///
/// `extern "sysv64"` を明示しているのは、既定の `extern "C"` が
/// コンパイル対象ごとに異なる呼び出し規約を意味しうるため
/// （`x86_64-unknown-uefi` は既定で Microsoft x64、`x86_64-unknown-none`
/// は既定で SysV）。bootloader 側の関数ポインタ型
/// （`common::boot_info::KernelEntryFn`）と一致させる必要がある。
///
/// # Safety
/// 呼び出し元（bootloader の ELF ローダー）は、`boot_info` が
/// `common::boot_info::BootInfo` として書き込み済みの有効なメモリを
/// 指しており、この呼び出しの間有効であり続けることを保証しなければ
/// ならない。
#[no_mangle]
pub unsafe extern "sysv64" fn _start(boot_info: *const BootInfo) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let mut logger = Logger::new(serial, LogLevel::Trace);

    logger.info(format_args!("ZaytOS kernel: entered _start"));

    // SAFETY: 呼び出し元契約（上記 # Safety）により、boot_info は有効な
    // BootInfo を指す。ここでは読み取り専用の参照を作るのみ。
    let boot_info = unsafe { &*boot_info };

    if let Err(e) = boot_info.validate() {
        logger.error(format_args!("BootInfo validation failed: {e}"));
        logger.error(format_args!(
            "bootloader と kernel のビルドが食い違っている可能性があります"
        ));
        cpu::halt_forever();
    }
    logger.info(format_args!("BootInfo validated (magic/version OK)"));

    logger.info(format_args!(
        "memory map: descriptors_len={} descriptor_size={} descriptor_version={}",
        boot_info.memory_map.descriptors_len,
        boot_info.memory_map.descriptor_size,
        boot_info.memory_map.descriptor_version
    ));
    logger.info(format_args!(
        "framebuffer: {}x{} stride={} format={:?} phys={:#x} size={}",
        boot_info.framebuffer.width,
        boot_info.framebuffer.height,
        boot_info.framebuffer.stride,
        boot_info.framebuffer.pixel_format,
        boot_info.framebuffer.physical_address,
        boot_info.framebuffer.size_bytes,
    ));

    // .bss ゼロ埋めの実地検証。`addr_of!` で生ポインタを取り、
    // `static_mut_refs` の警告（可変 static への参照作成）を避ける。
    let canary_ptr = core::ptr::addr_of!(BSS_CANARY);
    // SAFETY: 起動直後の単一実行文脈であり、他に誰もこの static に
    // 触れていない。読み取り専用アクセスのみ。
    let all_zero = unsafe { (*canary_ptr).iter().all(|&b| b == 0) };
    if all_zero {
        logger.info(format_args!(".bss zero-fill check: PASS (all zero)"));
    } else {
        logger.error(format_args!(".bss zero-fill check: FAIL (not all zero)"));
    }

    logger.info(format_args!("kernel: halting"));
    cpu::halt_forever();
}
