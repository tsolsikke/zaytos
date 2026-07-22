//! ELF ローダー（M2-0c）。
//!
//! `\zaytos\kernel.elf` を ESP から読み込み、ELF64 の `PT_LOAD` セグメントを
//! パースして物理メモリへ配置し、GOP フレームバッファ情報を取得したうえで
//! ExitBootServices を実行し、kernel へ制御を渡す。

use core::mem;

use common::boot_info::{
    BootInfo, FramebufferInfo, KernelEntryFn, MemoryMapInfo, PixelFormat as BiPixelFormat,
    BOOT_INFO_MAGIC, BOOT_INFO_PAGE_COUNT, BOOT_INFO_VERSION,
};
use common::elf::Elf;
use common::log::Logger;
use common::serial::SerialPort;
use uefi::boot::{AllocateType, MemoryType};
use uefi::cstr16;
use uefi::fs::{FileSystem, Path};
use uefi::mem::memory_map::MemoryMap;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat as GopPixelFormat};

const PAGE_SIZE: u64 = 4096;
const KERNEL_ELF_PATH: &uefi::CStr16 = cstr16!("\\zaytos\\kernel.elf");

fn align_down(addr: u64, align: u64) -> u64 {
    addr & !(align - 1)
}

fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

/// kernel.elf をロードし、ExitBootServices を経て kernel へジャンプする。
/// 正常経路では戻らない。
pub fn run(mut logger: Logger<SerialPort>) -> ! {
    // --- 1. kernel.elf の読み込み・パース・セグメント配置 ---
    // fs/elf_bytes/elf はこのブロックの終わりでスコープを抜けて破棄される。
    // elf_bytes は Boot Services のプールアロケータ（Vec）に由来するため、
    // ExitBootServices より前に破棄しておく必要がある。
    let entry_point = {
        let mut fs = FileSystem::new(
            uefi::boot::get_image_file_system(uefi::boot::image_handle())
                .expect("failed to open the boot volume's file system"),
        );
        let elf_bytes = fs
            .read(Path::new(KERNEL_ELF_PATH))
            .expect("failed to read \\zaytos\\kernel.elf from the ESP");

        let elf = Elf::parse(&elf_bytes).expect("failed to parse kernel.elf as ELF64");
        logger.info(format_args!(
            "ELF parsed: entry point = {:#x}",
            elf.entry_point
        ));

        let mut region_start = u64::MAX;
        let mut region_end = 0u64;
        for seg in elf.load_segments() {
            region_start = region_start.min(seg.p_vaddr);
            region_end = region_end.max(seg.p_vaddr + seg.p_memsz);
        }
        assert!(
            region_start < region_end,
            "kernel.elf has no PT_LOAD segments"
        );
        region_start = align_down(region_start, PAGE_SIZE);
        region_end = align_up(region_end, PAGE_SIZE);
        let page_count = ((region_end - region_start) / PAGE_SIZE) as usize;

        // ADR-0009 Consequences (物理アドレス確保の失敗リスク) 参照:
        // AllocatePages(Address) は既にその領域が使用中の場合に失敗しうる。
        // 握りつぶさずエラーとして報告する。
        uefi::boot::allocate_pages(
            AllocateType::Address(region_start),
            MemoryType::LOADER_DATA,
            page_count,
        )
        .unwrap_or_else(|e| {
            logger.error(format_args!(
                "AllocatePages(Address({region_start:#x}), count={page_count}) failed: {e:?}"
            ));
            panic!(
                "failed to allocate physical pages for the kernel image at the fixed \
                 address required by ADR-0009"
            );
        });

        // .bss ゼロ埋めの検証を意味のあるものにするため、セグメントコピー・
        // ゼロ埋めの前に領域全体を非ゼロ値（毒値）で埋めておく。QEMU の
        // 新規確保メモリはしばしば「たまたま」ゼロなので、これをしないと
        // ゼロ埋め処理自体にバグがあっても検出できない。
        // SAFETY: region_start..region_end は直前に排他的に確保した領域。
        unsafe {
            core::ptr::write_bytes(
                region_start as *mut u8,
                0xAA,
                (region_end - region_start) as usize,
            );
        }

        for seg in elf.load_segments() {
            let file_data = elf.segment_data(&seg);
            let dst = seg.p_vaddr as *mut u8;
            // SAFETY: `dst..dst + p_memsz` lies within `region_start..region_end`,
            // which we just exclusively allocated above via AllocatePages(Address).
            // `file_data.len() == p_filesz <= p_memsz` is guaranteed by the ELF
            // program header contract that `common::elf::Elf` parses.
            unsafe {
                core::ptr::copy_nonoverlapping(file_data.as_ptr(), dst, file_data.len());
                let zero_start = dst.add(file_data.len());
                let zero_len = (seg.p_memsz - seg.p_filesz) as usize;
                core::ptr::write_bytes(zero_start, 0, zero_len);
            }
        }
        logger.info(format_args!(
            "kernel segments placed and .bss zeroed ({region_start:#x}..{region_end:#x})"
        ));

        elf.entry_point
    };

    // --- 2. GOP フレームバッファ情報の取得（ExitBootServices 前のみ可能） ---
    // gop はこのブロックの終わりでスコープを抜け、ExitBootServices より前に
    // プロトコルが閉じられる。
    let framebuffer = {
        let gop_handle = uefi::boot::get_handle_for_protocol::<GraphicsOutput>()
            .expect("no Graphics Output Protocol handle found");
        let mut gop = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle)
            .expect("failed to open the Graphics Output Protocol");

        let mode = gop.current_mode_info();
        let (width, height) = mode.resolution();
        let stride = mode.stride();
        let pixel_format = match mode.pixel_format() {
            GopPixelFormat::Rgb => BiPixelFormat::Rgb,
            GopPixelFormat::Bgr => BiPixelFormat::Bgr,
            GopPixelFormat::Bitmask => BiPixelFormat::Bitmask,
            GopPixelFormat::BltOnly => BiPixelFormat::BltOnly,
        };
        let (red_mask, green_mask, blue_mask) = match mode.pixel_bitmask() {
            Some(mask) => (mask.red, mask.green, mask.blue),
            None => (0, 0, 0),
        };

        if pixel_format == BiPixelFormat::BltOnly {
            // Blt() はプロトコル関数呼び出しであり ExitBootServices 後は使えない。
            // M3 はこの場合まだ扱えないため、フレームバッファ無しとして
            // 記録するにとどめ、ここでは致命的エラーにしない（M2 の主目的
            // であるメモリマップの引き渡しはこれに依存しないため）。
            logger.warn(format_args!(
                "GOP reports BltOnly (no direct framebuffer access); M3 cannot draw \
                 until this is handled"
            ));
            FramebufferInfo {
                physical_address: 0,
                size_bytes: 0,
                width: width as u32,
                height: height as u32,
                stride: stride as u32,
                pixel_format,
                red_mask,
                green_mask,
                blue_mask,
            }
        } else {
            let mut fb = gop.frame_buffer();
            FramebufferInfo {
                physical_address: fb.as_mut_ptr() as u64,
                size_bytes: fb.size() as u64,
                width: width as u32,
                height: height as u32,
                stride: stride as u32,
                pixel_format,
                red_mask,
                green_mask,
                blue_mask,
            }
        }
    };
    logger.info(format_args!(
        "GOP framebuffer acquired: {}x{} stride={} format={:?} phys={:#x} size={}",
        framebuffer.width,
        framebuffer.height,
        framebuffer.stride,
        framebuffer.pixel_format,
        framebuffer.physical_address,
        framebuffer.size_bytes
    ));

    // --- 3. BootInfo 用ページの確保 ---
    let boot_info_ptr = uefi::boot::allocate_pages(
        AllocateType::AnyPages,
        MemoryType::LOADER_DATA,
        BOOT_INFO_PAGE_COUNT,
    )
    .unwrap_or_else(|e| {
        logger.error(format_args!("AllocatePages for BootInfo failed: {e:?}"));
        panic!("failed to allocate the BootInfo page");
    })
    .as_ptr()
    .cast::<BootInfo>();

    // --- 4. ExitBootServices ---
    // uefi-rs の `exit_boot_services` 自身が「メモリマップ取得 →
    // ExitBootServices 呼び出し」をアロケーションを挟まず一体で行い、
    // マップキー不整合時のリトライ（最大2回、失敗時はコールドリセット）を
    // 内部で実装している（docs/architecture.md 参照）。自前でリトライを
    // 書く必要はない。
    logger.info(format_args!(
        "ExitBootServices: about to exit boot services"
    ));
    // SAFETY: この時点までに取得した UEFI プロトコル参照（GOP 等）は
    // いずれも対応するブロックの終わりで既にスコープを抜けている。
    // kernel.elf 読み込みに使ったプールアロケータ由来のバッファ
    // （Vec<u8>）も同様にスコープを抜けて破棄済みである。
    let memory_map = unsafe { uefi::boot::exit_boot_services(Some(MemoryType::LOADER_DATA)) };
    logger.info(format_args!("ExitBootServices: done"));

    let meta = memory_map.meta();
    let descriptors_ptr = memory_map.buffer().as_ptr() as u64;

    // SAFETY: boot_info_ptr は直前に AllocatePages(AnyPages, ..,
    // BOOT_INFO_PAGE_COUNT) で確保した、他に誰も参照していない領域を指す。
    // BootInfo 一つ分の書き込みはそのページ内に収まる
    // (size_of::<BootInfo>() < BOOT_INFO_PAGE_COUNT * 4096)。
    unsafe {
        boot_info_ptr.write(BootInfo {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            memory_map: MemoryMapInfo {
                descriptors_ptr,
                descriptors_len: meta.map_size as u64,
                descriptor_size: meta.desc_size as u64,
                descriptor_version: meta.desc_version,
            },
            framebuffer,
        });
    }

    // SAFETY: `MemoryMapOwned::drop` は Boot Services の `free_pool` を
    // 呼び出すが、ExitBootServices が成功した今はもう存在しない。
    // 必要な値（descriptors_ptr/meta）は上で BootInfo へコピー済みのため、
    // このまま drop させず意図的にリークする。（uefi-rs 自身の
    // backing-memory Drop 実装にも `are_boot_services_active()` による
    // ガードがあるが、その内部実装に依存せず、ここで明示的に保証する。）
    mem::forget(memory_map);

    logger.info(format_args!(
        "jumping to kernel entry point {entry_point:#x}"
    ));

    // SAFETY: `entry_point` はロード済みの kernel イメージ内の実行可能な
    // PT_LOAD セグメント内を指しており、link.ld の `ENTRY(_start)` に対応
    // する。呼び出し規約 `extern "sysv64"` は
    // `common::boot_info::KernelEntryFn` で定義され、kernel 側の `_start`
    // のシグネチャと一致させている（`extern "C"` はコンパイル対象ごとに
    // 既定の呼び出し規約が異なるため使わない）。`boot_info_ptr` は
    // 直前に書き込み済みの有効な `BootInfo` を指す。
    let entry: KernelEntryFn = unsafe { mem::transmute(entry_point as usize) };
    // SAFETY: 直前の `transmute` が満たした契約のもとで呼ぶ。この呼び出しは
    // 戻らない（kernel 側の `_start` は `-> !`）。ExitBootServices は既に
    // 済んでおり、以降 Boot Services には触れない。
    unsafe { entry(boot_info_ptr) }
}
