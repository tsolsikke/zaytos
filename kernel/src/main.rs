//! ZaytOS kernel（M2-0c: bootloader からの引き渡しを受けて起動。
//! M2-c: 物理フレームアロケータ。M2-d: 新規ページテーブル構築 (d-1) と
//! CR3 切り替え (d-2)）。

#![no_std]
#![no_main]

use common::boot_info::{BootInfo, BOOT_INFO_PAGE_COUNT};
use common::cpu;
use common::log::{LogLevel, Logger};
use common::serial::SerialPort;
use kernel::frame_allocator;
use kernel::paging;
use kernel::paging::plan::{resolve_pages, MappedRanges};
use kernel::paging::table::PageTableBuilder;

mod panic;

// `kernel/link.ld` が定義するシンボル。kernel イメージ自身の占有範囲を
// 実行時に把握するために使う（M2-d の必須マッピング検証）。
extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

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

    // 自前の IDT・例外ハンドラ（M4）を導入するまで、割り込みは常に禁止
    // しておく。UEFI が ExitBootServices 前に設定した IDT が現在も生きて
    // おり、割り込みが発生すればそのハンドラ（恒等マッピングにより
    // "たまたま" 到達可能なだけの、自前で検証していないコード）に制御が
    // 渡ってしまうため（docs/architecture.md §6.5 参照）。
    let rflags_before = cpu::read_rflags();
    logger.info(format_args!(
        "interrupts: RFLAGS.IF before cli = {} (raw RFLAGS={:#x})",
        rflags_before & cpu::RFLAGS_INTERRUPT_FLAG != 0,
        rflags_before
    ));
    cpu::disable_interrupts();
    let rflags_after = cpu::read_rflags();
    logger.info(format_args!(
        "interrupts: RFLAGS.IF after cli = {} (raw RFLAGS={:#x})",
        rflags_after & cpu::RFLAGS_INTERRUPT_FLAG != 0,
        rflags_after
    ));

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

    // 物理フレームアロケータの構築（M2-c）。
    // SAFETY: bootloader 契約により、descriptors_ptr は descriptors_len
    // バイトの有効なメモリマップ（恒等マッピング済み、ADR-0009）を指す。
    let raw_map = unsafe {
        core::slice::from_raw_parts(
            boot_info.memory_map.descriptors_ptr as *const u8,
            boot_info.memory_map.descriptors_len as usize,
        )
    };

    let (mut allocator, stats) =
        frame_allocator::build(raw_map, boot_info.memory_map.descriptor_size).unwrap_or_else(|e| {
            logger.error(format_args!("frame allocator init failed: {e}"));
            cpu::halt_forever();
        });

    // QEMU に割り当てているメモリ量と突き合わせて実装ミスに気づけるよう、
    // 空きフレーム数・除外量（型別内訳つき）を必ずログへ残す。
    logger.info(format_args!(
        "frame allocator: {} free frames ({} MiB)",
        stats.free_frame_count,
        stats.free_mib()
    ));
    logger.info(format_args!(
        "frame allocator: {} pages excluded ({} MiB total)",
        stats.exclusions.total_pages(),
        stats.excluded_mib()
    ));
    let e = &stats.exclusions;
    logger.info(format_args!(
        "frame allocator: excluded breakdown (pages) - reserved={} loader_code={} \
         loader_data={} boot_services_code={} boot_services_data={} \
         runtime_services_code={} runtime_services_data={} unusable={} acpi_reclaim={} \
         acpi_nvs={} mmio={} mmio_port_space={} pal_code={} persistent={} \
         unaccepted={} vendor_reserved={} null_page={} unknown={}",
        e.reserved_pages,
        e.loader_code_pages,
        e.loader_data_pages,
        e.boot_services_code_pages,
        e.boot_services_data_pages,
        e.runtime_services_code_pages,
        e.runtime_services_data_pages,
        e.unusable_pages,
        e.acpi_reclaim_pages,
        e.acpi_nvs_pages,
        e.mmio_pages,
        e.mmio_port_space_pages,
        e.pal_code_pages,
        e.persistent_pages,
        e.unaccepted_pages,
        e.vendor_reserved_pages,
        e.null_page_excluded_pages,
        e.unknown_type_pages,
    ));
    // 容量に対してどれだけ余裕があるかを実測で把握するため、使用した
    // 範囲数と容量を必ずログへ残す（ADR-0011）。容量超過時は build() が
    // Err を返し、上の unwrap_or_else で既にエラー出力 + halt している。
    logger.info(format_args!(
        "frame allocator: {} / {} free ranges used",
        allocator.free_range_count(),
        frame_allocator::DEFAULT_CAPACITY
    ));

    // 実地スモークテスト: 1フレーム確保して解放し、空きフレーム数が
    // 元通りになることを確認する（.bss ゼロ埋め検証と同じ考え方）。
    let before = allocator.free_frame_count();
    match allocator.allocate_frame() {
        Some(frame) => {
            logger.info(format_args!(
                "frame allocator smoke test: allocated frame {:#x}",
                frame * frame_allocator::FRAME_SIZE
            ));
            let _ = allocator.deallocate_frame(frame);
            if allocator.free_frame_count() == before {
                logger.info(format_args!("frame allocator smoke test: PASS"));
            } else {
                logger.error(format_args!(
                    "frame allocator smoke test: FAIL (count mismatch after deallocate)"
                ));
            }
        }
        None => {
            logger.error(format_args!(
                "frame allocator smoke test: FAIL (no free frames available)"
            ));
        }
    }

    // === M2-d (d-1): 新しいページテーブルを構築する（CR3 は切り替えない） ===

    // フレームバッファは実機検証の結果、UEFI メモリマップに現れないことが
    // 判明した（PCI BAR はシステムメモリマップとは別扱いのため）。
    // メモリマップ由来の判定だけに頼らず、BootInfo から得た範囲を明示的に
    // 追加する（`physical_address == 0` は BltOnly 等で無効なため除く）。
    let fb_start = boot_info.framebuffer.physical_address;
    let fb_end = fb_start + boot_info.framebuffer.size_bytes;
    let extra_ranges: &[(u64, u64, bool)] = if fb_start != 0 {
        &[(fb_start, fb_end, false)]
    } else {
        &[]
    };

    // マップ対象範囲の計画（純粋ロジック、frame_allocator::build と同じ
    // classify() を経由するため、判定基準が独自にずれることはない）。
    let mapped_ranges = MappedRanges::<{ kernel::paging::plan::DEFAULT_CAPACITY }>::build(
        raw_map,
        boot_info.memory_map.descriptor_size,
        extra_ranges,
    )
    .unwrap_or_else(|e| {
        logger.error(format_args!("paging plan build failed: {e}"));
        cpu::halt_forever();
    });

    // 不変条件: アロケータが配りうる全フレームは、必ずこの計画に
    // 含まれている（さもないと、後で配られたフレームが未マップのまま
    // 使われ、無言で壊れる）。classify() を共有しているため理屈の上では
    // 常に成立するはずだが、実装が今後ズレても検出できるよう実行時にも
    // 確認する。
    let mut allocator_ranges_covered = true;
    for (start_frame, frame_count) in allocator.free_ranges() {
        let start = start_frame * frame_allocator::FRAME_SIZE;
        let end = (start_frame + frame_count) * frame_allocator::FRAME_SIZE;
        if !mapped_ranges.contains_range(start, end) {
            allocator_ranges_covered = false;
            logger.error(format_args!(
                "paging: allocator free range {start:#x}..{end:#x} is NOT fully mapped"
            ));
        }
    }
    if !allocator_ranges_covered {
        logger.error(format_args!(
            "paging: invariant violated (allocator free frame not mapped); halting"
        ));
        cpu::halt_forever();
    }

    // マップ範囲一覧をダンプする（範囲・キャッシュ属性）。
    logger.info(format_args!(
        "paging: {} mapped range(s) planned:",
        mapped_ranges.range_count()
    ));
    for r in mapped_ranges.iter() {
        logger.info(format_args!(
            "paging:   {:#x}..{:#x} cacheable={}",
            r.start, r.end, r.cacheable
        ));
    }

    // 実際にページテーブルへ書き込む（kernel/src/paging/table.rs 参照）。
    let mut builder = PageTableBuilder::new(&mut allocator).unwrap_or_else(|e| {
        logger.error(format_args!(
            "paging: failed to start page table build: {e:?}"
        ));
        cpu::halt_forever();
    });

    let mut huge_page_count: u64 = 0;
    let mut small_page_count: u64 = 0;
    let mut map_error = None;
    resolve_pages(&mapped_ranges, |m| {
        if map_error.is_some() {
            return;
        }
        if let Err(e) = builder.map_page(m.phys_addr, m.huge, m.cacheable) {
            map_error = Some((m, e));
            return;
        }
        if m.huge {
            huge_page_count += 1;
        } else {
            small_page_count += 1;
        }
    });
    if let Some((m, e)) = map_error {
        logger.error(format_args!(
            "paging: map_page({:#x}, huge={}) failed: {:?}",
            m.phys_addr, m.huge, e
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "paging: {huge_page_count} huge (2MiB) page(s), {small_page_count} small (4KiB) page(s), \
         {} frame(s) consumed for page tables",
        builder.frames_used()
    ));

    // 必須領域の充足検証。ここに挙げる領域は、CR3 切り替え後も
    // アクセスできる必要がある（切り替え直後のトリプルフォルトの
    // 主要因になるため）。
    let kernel_start = core::ptr::addr_of!(__kernel_start) as u64;
    let kernel_end = core::ptr::addr_of!(__kernel_end) as u64;
    let boot_info_start = boot_info as *const BootInfo as u64;
    let boot_info_end =
        boot_info_start + (BOOT_INFO_PAGE_COUNT as u64) * frame_allocator::FRAME_SIZE;
    let mmap_start = boot_info.memory_map.descriptors_ptr;
    let mmap_end = mmap_start + boot_info.memory_map.descriptors_len;
    // fb_start/fb_end は上（extra_ranges 構築時）で計算済みのものを使う。
    let current_rsp = cpu::read_rsp();
    let current_rip = cpu::read_rip();
    let pml4_phys = builder.pml4_phys();

    let mut all_required_ok = true;
    let mut check_range = |name: &str, start: u64, end: u64| {
        let ok = mapped_ranges.contains_range(start, end);
        logger.info(format_args!(
            "paging: required range [{name}] {start:#x}..{end:#x}: {}",
            if ok { "OK" } else { "NG" }
        ));
        if !ok {
            all_required_ok = false;
        }
    };
    check_range("kernel image", kernel_start, kernel_end);
    check_range("BootInfo", boot_info_start, boot_info_end);
    check_range("memory map buffer", mmap_start, mmap_end);
    if fb_start != 0 {
        check_range("framebuffer", fb_start, fb_end);
    }
    check_range(
        "new page tables (PML4)",
        pml4_phys,
        pml4_phys + frame_allocator::FRAME_SIZE,
    );
    check_range("current RSP", current_rsp, current_rsp + 1);
    check_range("current RIP", current_rip, current_rip + 1);

    if !all_required_ok {
        logger.error(format_args!(
            "paging: one or more required ranges are NOT mapped; halting (see NG above)"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "paging: all required ranges mapped. proceeding to CR3 switch (d-2)."
    ));

    // === M2-d (d-2): CR3 を新しいページテーブルへ切り替える ===

    let old_cr3 = paging::switch::read_cr3();
    logger.info(format_args!("paging: current CR3 = {old_cr3:#x}"));

    // pml4_phys は alloc_zeroed_table がフレームアロケータから確保した
    // フレームの先頭アドレス（frame * FRAME_SIZE）であるため下位12ビットは
    // 常に 0 のはずだが、CR3 に書き込む値の PWT/PCD ビット（bit 3, 4）を
    // 含む下位ビットが確実に 0 であることを実行時にも検証する。
    if pml4_phys & 0xFFF != 0 {
        logger.error(format_args!(
            "paging: new PML4 {pml4_phys:#x} is not 4KiB aligned; refusing to switch CR3"
        ));
        cpu::halt_forever();
    }
    let cr3_value = pml4_phys;

    // 切り替え前スナップショット(切り替え後の整合性確認に使う)。
    // SAFETY: kernel_start は必須領域検証により読み取り可能であることを
    // 確認済み。
    let kernel_first_byte_before = unsafe { core::ptr::read_volatile(kernel_start as *const u8) };

    // SAFETY: 直前の必須領域検証(all_required_ok)により、現在実行中の
    // コード・現在のスタック・pml4_phys 自身のフレームがすべて新しい
    // ページテーブルでも恒等マッピングされていることを確認済み。
    unsafe {
        paging::switch::switch_to(cr3_value);
    }

    // ここが出れば CR3 切り替え命令自体は実行できた(トリプルフォルト
    // していない)ことが分かる。
    logger.info(format_args!("paging: CR3 switch instruction executed"));

    let new_cr3 = paging::switch::read_cr3();
    let cr3_ok = new_cr3 == cr3_value;
    logger.info(format_args!(
        "paging: CR3 readback {new_cr3:#x} (expected {cr3_value:#x}): {}",
        if cr3_ok { "OK" } else { "NG" }
    ));
    if !cr3_ok {
        logger.error(format_args!("paging: CR3 readback mismatch; halting"));
        cpu::halt_forever();
    }

    let mut post_switch_ok = true;

    // (a) kernel イメージの読み取り検証。
    logger.info(format_args!("paging: about to test: kernel image read"));
    // SAFETY: kernel_start は必須領域検証により読み取り可能であることを
    // 確認済み。
    let kernel_first_byte_after = unsafe { core::ptr::read_volatile(kernel_start as *const u8) };
    let kernel_read_ok = kernel_first_byte_after == kernel_first_byte_before;
    logger.info(format_args!(
        "paging: kernel image read: {}",
        if kernel_read_ok { "OK" } else { "NG" }
    ));
    post_switch_ok &= kernel_read_ok;

    // (b) スタックの読み書き検証。ローカル変数は volatile アクセスでも
    // レジスタに割り当てられうるため、それでは実際にスタックへ触った
    // ことにならない。現在の RSP を実レジスタ値として読み、その少し下
    // (未使用側、生きているスタックフレームより低いアドレス)へ生
    // ポインタで直接書き書き・読み戻しする。
    logger.info(format_args!("paging: about to test: stack read/write"));
    let stack_probe_addr = cpu::read_rsp().wrapping_sub(256);
    const STACK_PROBE_PATTERN: u64 = 0xDEAD_BEEF_CAFE_0000;
    // SAFETY: stack_probe_addr は現在の RSP より低いアドレス(スタックが
    // まだ使っていない未使用領域)であり、かつ必須領域検証で確認した
    // "current RSP" と同じスタック領域内(同じマップ済み範囲)にある。
    // 使用中のスタックフレームには重ならない。
    let stack_read_back = unsafe {
        core::ptr::write_volatile(stack_probe_addr as *mut u64, STACK_PROBE_PATTERN);
        core::ptr::read_volatile(stack_probe_addr as *const u64)
    };
    let stack_ok = stack_read_back == STACK_PROBE_PATTERN;
    logger.info(format_args!(
        "paging: stack read/write: {}",
        if stack_ok { "OK" } else { "NG" }
    ));
    post_switch_ok &= stack_ok;

    // (c) BootInfo の再検証(マジック値の再チェックを流用)。
    logger.info(format_args!("paging: about to test: BootInfo read"));
    let boot_info_ok = boot_info.validate().is_ok();
    logger.info(format_args!(
        "paging: BootInfo read: {}",
        if boot_info_ok { "OK" } else { "NG" }
    ));
    post_switch_ok &= boot_info_ok;

    // (d) フレームバッファへの実描画。読み戻しだけでは、読めた値が実際に
    // フレームバッファのものかキャッシュ上の値かを区別できないため、
    // 左上に目視可能な色付きブロックを描画する(PCD が機能していれば
    // screenshot で実際に見えるはず)。
    if fb_start != 0 {
        logger.info(format_args!(
            "paging: about to test: framebuffer 64x64 block draw"
        ));
        match boot_info.framebuffer.pixel_format {
            common::boot_info::PixelFormat::Rgb | common::boot_info::PixelFormat::Bgr => {
                let stride = boot_info.framebuffer.stride as u64;
                let block_w = 64u64.min(boot_info.framebuffer.width as u64);
                let block_h = 64u64.min(boot_info.framebuffer.height as u64);
                const TEST_COLOR: u32 = 0x00FF_3366;
                for y in 0..block_h {
                    let row_ptr = (fb_start + y * stride * 4) as *mut u32;
                    for x in 0..block_w {
                        // SAFETY: (x, y) は framebuffer.width/height 以内、
                        // row_ptr は framebuffer の必須領域検証で確認済みの
                        // 範囲内(size_bytes = height * stride * 4 に収まる)。
                        unsafe {
                            core::ptr::write_volatile(row_ptr.add(x as usize), TEST_COLOR);
                        }
                    }
                }
                logger.info(format_args!(
                    "paging: framebuffer 64x64 block draw: OK (see screenshot)"
                ));
            }
            other => {
                logger.info(format_args!(
                    "paging: framebuffer 64x64 block draw: SKIPPED (pixel_format={other:?} \
                     is not directly writable)"
                ));
            }
        }
    }

    if !post_switch_ok {
        logger.error(format_args!(
            "paging: one or more post-switch checks failed (see NG above); halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "paging: CR3 switch verified. now running under self-built page tables."
    ));

    logger.info(format_args!("kernel: halting"));
    cpu::halt_forever();
}
