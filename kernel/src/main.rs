//! ZaytOS kernel（M2-0c: bootloader からの引き渡しを受けて起動。
//! M2-c: 物理フレームアロケータ。M2-d: 新規ページテーブル構築 (d-1) と
//! CR3 切り替え (d-2)。M2-e: カーネルヒープ）。

#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use common::boot_info::{BootInfo, BOOT_INFO_PAGE_COUNT};
use common::cpu;
use common::log::{LogLevel, Logger};
use common::serial::SerialPort;
use kernel::frame_allocator;
use kernel::graphics::{self, Color, Framebuffer, FramebufferLayout};
use kernel::heap;
use kernel::paging;
use kernel::paging::plan::{resolve_pages, MappedRanges};
use kernel::paging::table::PageTableBuilder;

mod panic;

#[global_allocator]
static ALLOCATOR: heap::allocator::LockedHeap = heap::allocator::LockedHeap::empty();

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

    // (d) フレームバッファへの実描画（M3-a）。読み戻しだけでは、読めた値が
    // 実際にフレームバッファのものかキャッシュ上の値かを区別できないため、
    // 目視できるテストパターンを実際に描く。ここで描けることが、CR3 切り替え
    // 後もフレームバッファへ到達できていることの証明も兼ねる。
    let mut framebuffer = init_framebuffer(&mut logger, boot_info, &mapped_ranges);
    if let Some(fb) = framebuffer.as_mut() {
        draw_startup_test_pattern(&mut logger, fb);
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

    // === M2-e: カーネルヒープ ===

    let heap_frame_count = heap::DEFAULT_HEAP_FRAME_COUNT;
    let heap_start_frame = allocator
        .allocate_contiguous(heap_frame_count)
        .unwrap_or_else(|| {
            logger.error(format_args!(
                "heap: failed to reserve a contiguous arena of {heap_frame_count} frames"
            ));
            cpu::halt_forever();
        });
    let heap_start = heap_start_frame * frame_allocator::FRAME_SIZE;
    let heap_size = heap_frame_count * frame_allocator::FRAME_SIZE;
    let heap_end = heap_start + heap_size;
    let heap_mapped = mapped_ranges.contains_range(heap_start, heap_end);
    logger.info(format_args!(
        "heap: arena {heap_start:#x}..{heap_end:#x} ({heap_frame_count} frames, \
         {} MiB), mapped={heap_mapped}",
        heap_size / (1024 * 1024)
    ));
    if !heap_mapped {
        logger.error(format_args!("heap: arena is not fully mapped; halting"));
        cpu::halt_forever();
    }

    // SAFETY: heap_start..heap_end はフレームアロケータから今切り出した
    // ばかりの、他の誰も使っていない領域であり、直前に mapped_ranges で
    // マップ済みであることも確認済み。このヒープに対する `init` 呼び出しは
    // これが最初で最後(1回のみ)。
    unsafe {
        ALLOCATOR.init(heap_start, heap_size);
    }
    logger.info(format_args!(
        "heap: initialized ({} bytes free, {} block(s))",
        ALLOCATOR.free_bytes(),
        ALLOCATOR.free_block_count()
    ));

    // 実地スモークテスト: Vec/Box/String を実際に確保・追記・解放する。
    let mut v: Vec<u32> = Vec::new();
    for i in 0..10u32 {
        v.push(i * i);
    }
    let v_ptr = v.as_ptr() as u64;
    let v_len_bytes = (v.len() * core::mem::size_of::<u32>()) as u64;
    logger.info(format_args!(
        "heap smoke test: Vec<u32> len={} ptr={v_ptr:#x} mapped={}",
        v.len(),
        mapped_ranges.contains_range(v_ptr, v_ptr + v_len_bytes)
    ));
    drop(v);

    let b = Box::new(0x1234_5678u32);
    let b_ptr = &*b as *const u32 as u64;
    logger.info(format_args!(
        "heap smoke test: Box<u32> value={:#x} ptr={b_ptr:#x} mapped={}",
        *b,
        mapped_ranges.contains_range(b_ptr, b_ptr + 4)
    ));
    drop(b);

    let mut s = String::from("ZaytOS heap");
    s.push_str(" is alive");
    let s_ptr = s.as_ptr() as u64;
    let s_len = s.len() as u64;
    logger.info(format_args!(
        "heap smoke test: String={:?} ptr={s_ptr:#x} mapped={}",
        s.as_str(),
        mapped_ranges.contains_range(s_ptr, s_ptr + s_len)
    ));
    drop(s);

    logger.info(format_args!(
        "heap: after smoke test, {} bytes free, {} block(s)",
        ALLOCATOR.free_bytes(),
        ALLOCATOR.free_block_count()
    ));

    logger.info(format_args!("kernel: halting"));
    cpu::halt_forever();
}

/// フレームバッファを検証し、描画ハンドルを作る（M3-a）。
///
/// bootloader から渡された形状をそのまま信じず、[`FramebufferLayout`] の
/// 検証を通す。通らなかった場合は理由を ERROR で残して `None` を返し、
/// 描画せずに以降の処理を続ける。ここで halt しないのは、フレームバッファが
/// 使えない環境でも、それ以外の起動シーケンスの診断ログは最後まで取りたい
/// ため（ADR-0013）。画面が主たる出力手段になる M3-c では方針を見直す。
fn init_framebuffer(
    logger: &mut Logger<SerialPort>,
    boot_info: &BootInfo,
    mapped_ranges: &MappedRanges,
) -> Option<Framebuffer> {
    let info = &boot_info.framebuffer;
    let layout = match FramebufferLayout::from_info(info) {
        Ok(layout) => layout,
        Err(e) => {
            logger.error(format_args!(
                "framebuffer: validation failed ({e:?}); drawing is disabled"
            ));
            return None;
        }
    };

    // 検証は「GOP の申告に内部矛盾が無いこと」しか見ていない。その範囲が
    // 実際に現在のページテーブルでマップされているかは別問題なので、ここで
    // 確認する（`Framebuffer::new` の安全性要件）。
    if !mapped_ranges.contains_range(layout.base(), layout.end()) {
        logger.error(format_args!(
            "framebuffer: {:#x}..{:#x} is not fully mapped; drawing is disabled",
            layout.base(),
            layout.end()
        ));
        return None;
    }

    logger.info(format_args!(
        "framebuffer: validated {}x{} stride={} format={:?} {:#x}..{:#x}",
        layout.width(),
        layout.height(),
        layout.stride(),
        layout.format(),
        layout.base(),
        layout.end()
    ));

    // SAFETY: layout は FramebufferLayout の検証を通っており、最終行の末尾まで
    // size_bytes に収まることが保証されている。base..end が現在のページ
    // テーブルでマップ済みであることは直前に contains_range で確認した。
    // フレームバッファは他の誰も使っておらず、この Framebuffer が唯一の
    // 書き込み手段になる（作るのはこの 1 箇所のみ）。
    Some(unsafe { Framebuffer::new(layout) })
}

/// 起動時のテストパターンを描く（M3-a）。
///
/// 目視で次を確認できるように選んである。
/// - 画面全体が塗られる: 形状の検証（`height * stride * 4 <= size_bytes`）が
///   正しく、全画面を描いても範囲外へ出ない。
/// - 外周 1px の枠が四辺すべてに出る: stride の扱いが正しい。stride を width と
///   取り違えていると枠が斜めにずれる。
/// - 赤・緑・青の順に正しい色で並ぶ: ピクセルフォーマット変換が正しい。
///   Rgb/Bgr を取り違えていると赤と青が入れ替わる。
/// - 右下からはみ出した矩形が、画面内の分だけ描かれて落ちない: 切り詰めが
///   効いており、範囲外へ書いていない。
fn draw_startup_test_pattern(logger: &mut Logger<SerialPort>, framebuffer: &mut Framebuffer) {
    const BACKGROUND: Color = Color::rgb(0x10, 0x10, 0x18);
    const BORDER: Color = Color::WHITE;
    const SWATCH_SIZE: u32 = 64;
    const SWATCH_MARGIN: u32 = 16;

    let width = framebuffer.layout().width();
    let height = framebuffer.layout().height();

    framebuffer.clear(BACKGROUND);

    // 外周 1px の枠。四辺を個別に塗る。
    framebuffer.fill_rect(0, 0, width, 1, BORDER);
    framebuffer.fill_rect(0, height - 1, width, 1, BORDER);
    framebuffer.fill_rect(0, 0, 1, height, BORDER);
    framebuffer.fill_rect(width - 1, 0, 1, height, BORDER);

    // 原色の並び。左から赤・緑・青。
    for (index, color) in [Color::RED, Color::GREEN, Color::BLUE].iter().enumerate() {
        let x = SWATCH_MARGIN + (index as u32) * (SWATCH_SIZE + SWATCH_MARGIN);
        framebuffer.fill_rect(x, SWATCH_MARGIN, SWATCH_SIZE, SWATCH_SIZE, *color);
    }

    // 右下からわざとはみ出させる。切り詰めが効いていれば、画面内に収まる
    // 部分だけが描かれる。効いていなければ範囲外へ書き込んでページ
    // フォルトするか、無関係なメモリを壊す。
    framebuffer.fill_rect(
        width - SWATCH_SIZE / 2,
        height - SWATCH_SIZE / 2,
        SWATCH_SIZE * 4,
        SWATCH_SIZE * 4,
        Color::rgb(0xFF, 0xC0, 0x00),
    );

    draw_startup_text(framebuffer, BACKGROUND);

    logger.info(format_args!(
        "framebuffer: startup test pattern drawn ({width}x{height}); verify with \
         cargo xtask screenshot"
    ));
}

/// 起動時のテストパターンに文字を描く（M3-b）。
///
/// 目視で次を確認できるように選んである。
/// - 印字可能な ASCII が 3 行すべて欠けずに並ぶ: グリフテーブルの検索と
///   ビットの並びが正しい。左右が反転していれば字形が鏡像になる。
/// - 日本語が代替グリフ（U+FFFD）として描かれる: 未収録文字のフォールバックが
///   効いており、落ちない。日本語を収録した時点でここが本来の字形に変わる。
/// - 右端から始まる行が、画面内に収まる分だけ描かれて落ちない: 文字単位の
///   切り詰めが効いている。
///
/// ヒープ初期化より前に呼ばれるため、動的な文字列は組み立てられない。
/// 静的な文字列だけで確認できる内容にしてある。
fn draw_startup_text(framebuffer: &mut Framebuffer, background: Color) {
    const TEXT_LEFT: u32 = 16;
    const TEXT_TOP: u32 = 112;
    const LINE_HEIGHT: u32 = 20;
    const FOREGROUND: Color = Color::rgb(0xE0, 0xE0, 0xE0);
    const ACCENT: Color = Color::rgb(0x66, 0xD0, 0xFF);

    let lines: [(&str, Color); 6] = [
        ("ZaytOS", ACCENT),
        (" !\"#$%&'()*+,-./0123456789:;<=>?", FOREGROUND),
        ("@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_", FOREGROUND),
        ("`abcdefghijklmnopqrstuvwxyz{|}~", FOREGROUND),
        ("not yet embedded: (japanese)", FOREGROUND),
        ("fallback check: ", FOREGROUND),
    ];

    for (index, (text, color)) in lines.iter().enumerate() {
        let y = TEXT_TOP + (index as u32) * LINE_HEIGHT;
        framebuffer.draw_str(TEXT_LEFT, y, text, *color, Some(background));
    }

    // 未収録文字が代替グリフになることの確認。上の最終行の続きに描く。
    let fallback_x = TEXT_LEFT + graphics::text_width_pixels("fallback check: ");
    let fallback_y = TEXT_TOP + 5 * LINE_HEIGHT;
    framebuffer.draw_str(
        fallback_x,
        fallback_y,
        "あア漢",
        Color::rgb(0xFF, 0xA0, 0xA0),
        Some(background),
    );

    // 右端をまたぐ位置から描き、画面内の分だけが出ることを確認する。
    let clipped_y = TEXT_TOP + 7 * LINE_HEIGHT;
    framebuffer.draw_str(
        framebuffer.layout().width() - 24,
        clipped_y,
        "CLIPPED",
        Color::rgb(0xFF, 0xC0, 0x00),
        Some(background),
    );
}
