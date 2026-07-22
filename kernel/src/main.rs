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
use core::ptr::addr_of;
use kernel::console::Console;

use kernel::frame_allocator;
use kernel::gdt;
use kernel::graphics::{Color, Framebuffer, FramebufferLayout};
use kernel::heap;
use kernel::idt;
use kernel::interrupts;
use kernel::keyboard;
use kernel::paging;
use kernel::paging::plan::{resolve_pages, MappedRanges};
use kernel::paging::table::PageTableBuilder;
use kernel::pic;
use kernel::pit;
use kernel::stack;

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
    // ここはまだ UEFI 由来のスタックの上で動いている。ログを出さずに
    // 最小限の処理だけを行い、自前のスタックへ移ってから本番の起動
    // シーケンスに入る（M4-a）。旧スタックの上に状態を積むほど、
    // 切り替え後に「参照してはいけない領域」が増えるため。
    //
    // 引き継ぎたい値は BOOT_HANDOFF（静的領域）に置く。スタック上に
    // 置いて渡すと、切り替え後にその参照が旧スタックを指してしまう。

    // 自前の IDT・例外ハンドラ（M4-b）を導入するまで、割り込みは常に禁止
    // しておく。UEFI が ExitBootServices 前に設定した IDT が現在も生きて
    // おり、割り込みが発生すればそのハンドラ（恒等マッピングにより
    // "たまたま" 到達可能なだけの、自前で検証していないコード）に制御が
    // 渡ってしまうため（ADR-0014）。
    //
    // ここは InterruptGuard ではなく直接 cli する。M4-d で sti するまで
    // 恒久的に禁止し続けたいのであって、スコープを抜けたら復元する
    // クリティカルセクションとは意味が違うため（cpu::disable_interrupts
    // の doc が言う「以降割り込みを一切戻さない」場面）。
    let rflags_before = cpu::read_rflags();
    // SAFETY: M4-d まで割り込みを恒久的に禁止する意図的な操作（ADR-0014）。
    // この時点で張っているクリティカルセクションは存在しない。
    unsafe {
        cpu::disable_interrupts();
    }
    let rflags_after = cpu::read_rflags();

    let old_rsp = cpu::read_rsp();

    // スタックのカナリアを先に敷く。ここから下で自前スタックを使い始める。
    // SAFETY: 起動時の単一実行文脈であり、まだ誰もこれらのスタックを
    // 使っていない。呼ぶのはこの 1 回だけ。
    unsafe {
        stack::init_guards();
    }

    // GDT と TSS を自前のものへ切り替える。どちらも .bss の静的領域なので
    // アロケータを必要とせず、この時点で実行できる。
    // SAFETY: 直前に cli 済み。起動時に 1 回だけ呼ぶ。ダブルフォルト用の
    // スタックは通常のカーネルスタックとは別の静的領域である。
    unsafe {
        gdt::init(stack::double_fault_stack_range().top);
    }

    // IDT をロードする。ここも .bss の静的領域だけで完結する。
    // 例外（フォルト）は RFLAGS.IF に関係なく発生するため、割り込みを
    // 有効化しないままでもハンドラは働く。M4-d で sti するまでの間、
    // 例外だけが自前のハンドラへ届く状態になる（ADR-0018）。
    // SAFETY: 直前に cli 済みで、GDT も直前にロードした。起動時に 1 回だけ
    // 呼ぶ。ダブルフォルト用 IST は gdt::init が TSS へ設定済み。
    unsafe {
        idt::init(Some(gdt::DOUBLE_FAULT_IST_INDEX as u8));
    }

    // SAFETY: 起動時の単一実行文脈であり、他に誰もこの static に触れていない。
    unsafe {
        let handoff = addr_of!(BOOT_HANDOFF) as *mut BootHandoff;
        (*handoff) = BootHandoff {
            boot_info,
            rflags_before,
            rflags_after,
            old_rsp,
        };
    }

    // 自前のカーネルスタックへ移り、以降は kernel_main で動く。戻らない。
    // SAFETY: 切り替え後に旧スタックの値は一切参照しない（引き継ぎは
    // BOOT_HANDOFF 経由）。kernel_main は戻らない。呼ぶのはこの 1 回だけ。
    unsafe { stack::switch_to_kernel_stack_and_run(kernel_main) }
}

/// `_start` から `kernel_main` へ引き継ぐ値。
///
/// スタック切り替えを跨ぐため、スタックではなく静的領域に置く。
#[derive(Clone, Copy)]
struct BootHandoff {
    boot_info: *const BootInfo,
    rflags_before: u64,
    rflags_after: u64,
    old_rsp: u64,
}

static mut BOOT_HANDOFF: BootHandoff = BootHandoff {
    boot_info: core::ptr::null(),
    rflags_before: 0,
    rflags_after: 0,
    old_rsp: 0,
};

/// 自前のカーネルスタックの上で動く、本体の起動シーケンス（M4-a 以降）。
///
/// `exception-test` を有効にしたビルドでは、例外を発生させた時点で戻らない
/// ため、それ以降が到達不能になる。回帰チェック専用のビルドなので許容する。
#[cfg_attr(
    any(
        feature = "exception-test",
        feature = "critical-test",
        feature = "interrupt-test"
    ),
    allow(unreachable_code)
)]
extern "sysv64" fn kernel_main() -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let mut logger = Logger::new(serial, LogLevel::Trace);

    logger.info(format_args!("ZaytOS kernel: entered _start"));

    // SAFETY: _start が switch_to_kernel_stack_and_run より前に書き込み済みで、
    // 以降は誰も書き換えない。読み取りのみ。
    let handoff = unsafe { core::ptr::read(addr_of!(BOOT_HANDOFF)) };

    logger.info(format_args!(
        "interrupts: RFLAGS.IF before cli = {} (raw RFLAGS={:#x})",
        handoff.rflags_before & cpu::RFLAGS_INTERRUPT_FLAG != 0,
        handoff.rflags_before
    ));
    logger.info(format_args!(
        "interrupts: RFLAGS.IF after cli = {} (raw RFLAGS={:#x})",
        handoff.rflags_after & cpu::RFLAGS_INTERRUPT_FLAG != 0,
        handoff.rflags_after
    ));

    report_gdt_and_stack(&mut logger, handoff.old_rsp);
    report_idt(&mut logger);
    verify_critical_sections(&mut logger);
    configure_pic(&mut logger);

    // SAFETY: 呼び出し元契約（`_start` の # Safety）により、boot_info は
    // 有効な BootInfo を指す。ここでは読み取り専用の参照を作るのみ。
    let boot_info = unsafe { &*handoff.boot_info };

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

    // 稼働中のページテーブルを読み戻して、`plan` が意図した内容と一致するかを
    // 確かめる（M5-a-1）。**これは M5-a-2 の分割・アンマップを検証するための
    // 道具でもある。** 検証手段を先に用意しておくと、後から入れる操作の結果を
    // 「それを行ったコードとは独立に」確かめられる。
    verify_page_tables(&mut logger, &mapped_ranges);

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

    // 起動時テストパターンは M3-a の検証手段であり、通常起動では描かない。
    // コンソールは変更範囲しか転送しないため、描いたままにするとコンソール
    // 領域の外に残骸が残り続ける（ADR-0017）。検証したいときは
    // `cargo xtask run --gfx-test` で有効にする。通常起動では、この後の
    // コンソール初期化による全面クリアが、フレームバッファへ到達できて
    // いることの目視確認を兼ねる。
    #[cfg(feature = "gfx-test-pattern")]
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

    // === M3-c-3: 画面コンソール ===
    //
    // ここより前のログはシリアルにしか出ない。コンソールはバックバッファの
    // 確保にフレームアロケータを必要とし、フレームバッファへ書くには CR3
    // 切り替え後である必要があるため、この時点より前には作れない。
    // 実機の Linux も同じ構造で、起動初期のログは printk のバッファに溜まり、
    // コンソールドライバが登録されるまで画面には出ない（ADR-0017）。

    #[cfg(feature = "gfx-test-pattern")]
    logger.info(format_args!(
        "console: not started (gfx-test-pattern feature is enabled)"
    ));

    #[cfg(not(feature = "gfx-test-pattern"))]
    let mut console = framebuffer
        .take()
        .and_then(|fb| init_console(&mut logger, fb, &mut allocator, &mapped_ranges));
    #[cfg(feature = "gfx-test-pattern")]
    let mut console: Option<Console> = None;

    #[cfg(not(feature = "gfx-test-pattern"))]
    if let Some(console) = console.as_mut() {
        announce_console_start(&mut logger, console);
    }

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
    log_both(
        &mut logger,
        console.as_mut(),
        LogLevel::Info,
        format_args!(
            "heap: arena {heap_start:#x}..{heap_end:#x} ({heap_frame_count} frames, \
             {} MiB), mapped={heap_mapped}",
            heap_size / (1024 * 1024)
        ),
    );
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
    log_both(
        &mut logger,
        console.as_mut(),
        LogLevel::Info,
        format_args!(
            "heap: initialized ({} bytes free, {} block(s))",
            ALLOCATOR.free_bytes(),
            ALLOCATOR.free_block_count()
        ),
    );

    // ヒープが確保できた時点で、カーネルが使う主要領域が出そろう。
    // M5-a-2 の分割対象を選ぶ材料として、ここで粒度を測る。
    report_mapping_granularity(&mut logger, heap_start, fb_start);

    // 有効な仕込み feature を報告する。通常ビルドでは none と出る。
    report_test_hooks(&mut logger);

    // MAXPHYADDR は観測値として出すだけで、判定には使わない（T-1）。
    match cpu::max_physical_address_bits() {
        Some(bits) => logger.info(format_args!(
            "cpu: MAXPHYADDR = {bits} bits (observed only; PhysAddr rejects anything above 52)"
        )),
        None => logger.info(format_args!(
            "cpu: MAXPHYADDR could not be read (CPUID leaf 0x80000008 is not supported)"
        )),
    }

    // 2MiB ページの分割とアンマップ（M5-a-2-1）。
    verify_split_and_unmap(&mut logger, &mut allocator);

    // ページテーブル操作の回帰チェック（M5-a-2-2）。通常ビルドには入らない。
    #[cfg(feature = "paging-test")]
    run_paging_test(&mut logger, &mut allocator, heap_start);

    // ロック保持中は割り込みが禁止され、解放後に元へ戻ることを確認する
    // （M4-c-2）。ヒープのロックそのものではなく同じ Locked<T> を使う。
    // ヒープのロックを保持したままログを出すと二重取得になるため。
    report_lock_interrupt_state(&mut logger);

    // 実地スモークテスト: Vec/Box/String を実際に確保・追記・解放する。
    let mut v: Vec<u32> = Vec::new();
    for i in 0..10u32 {
        v.push(i * i);
    }
    let v_ptr = v.as_ptr() as u64;
    let v_len_bytes = (v.len() * core::mem::size_of::<u32>()) as u64;
    log_both(
        &mut logger,
        console.as_mut(),
        LogLevel::Info,
        format_args!(
            "heap smoke test: Vec<u32> len={} ptr={v_ptr:#x} mapped={}",
            v.len(),
            mapped_ranges.contains_range(v_ptr, v_ptr + v_len_bytes)
        ),
    );
    drop(v);

    let b = Box::new(0x1234_5678u32);
    let b_ptr = &*b as *const u32 as u64;
    log_both(
        &mut logger,
        console.as_mut(),
        LogLevel::Info,
        format_args!(
            "heap smoke test: Box<u32> value={:#x} ptr={b_ptr:#x} mapped={}",
            *b,
            mapped_ranges.contains_range(b_ptr, b_ptr + 4)
        ),
    );
    drop(b);

    let mut s = String::from("ZaytOS heap");
    s.push_str(" is alive");
    let s_ptr = s.as_ptr() as u64;
    let s_len = s.len() as u64;
    log_both(
        &mut logger,
        console.as_mut(),
        LogLevel::Info,
        format_args!(
            "heap smoke test: String={:?} ptr={s_ptr:#x} mapped={}",
            s.as_str(),
            mapped_ranges.contains_range(s_ptr, s_ptr + s_len)
        ),
    );
    drop(s);

    log_both(
        &mut logger,
        console.as_mut(),
        LogLevel::Info,
        format_args!(
            "heap: after smoke test, {} bytes free, {} block(s)",
            ALLOCATOR.free_bytes(),
            ALLOCATOR.free_block_count()
        ),
    );

    // ダーティ矩形が実際に効いているかを数字で残す。毎回のフラッシュで
    // ログを出すと、ログ自体が次のフラッシュを誘発するため、起動
    // シーケンスの最後に 1 回だけ出す。
    if let Some(stats) = console.as_ref().map(Console::stats) {
        log_both(
            &mut logger,
            console.as_mut(),
            LogLevel::Info,
            format_args!(
                "console: {} flush(es), {} bytes transferred",
                stats.flush_count, stats.transferred_bytes
            ),
        );
        log_both(
            &mut logger,
            console.as_mut(),
            LogLevel::Info,
            format_args!(
                "console: full-screen equivalent would be {} bytes ({}% actually sent)",
                stats.full_screen_equivalent_bytes(),
                stats.transferred_percent()
            ),
        );
    }

    // 例外ハンドラの回帰チェック。起動シーケンスを最後まで通してから
    // 発火させる（mapped_ranges を使ってプローブアドレスの妥当性を
    // 確認するため、ページング構築後である必要がある）。
    #[cfg(feature = "exception-test")]
    trigger_exception_under_test(&mut logger, &mapped_ranges);

    // クリティカルセクション/ロックの回帰チェック。
    #[cfg(feature = "critical-test")]
    trigger_critical_test(&mut logger);

    // 割り込みを有効化する経路の回帰チェック（M4-d-1 / M4-d-2）。
    #[cfg(feature = "interrupt-test")]
    trigger_interrupt_test(&mut logger, console.as_mut());

    // === M4-d-2: タイマを動かす ===
    //
    // ここから先は戻らない。ZaytOS で初めて「時間が流れる」状態に入り、
    // メインループがハートビートを出し続ける。`stop_after_ticks` に 0 を
    // 渡すと止まらない（回帰チェックのときだけ有限で打ち切る）。
    start_timer(&mut logger, console.as_mut(), 0);
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

#[cfg(feature = "gfx-test-pattern")]
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

#[cfg(feature = "gfx-test-pattern")]
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
    let fallback_x = TEXT_LEFT + kernel::graphics::text_width_pixels("fallback check: ");
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

#[cfg(not(feature = "gfx-test-pattern"))]
/// バックバッファを確保して画面コンソールを作る（M3-c-2）。
///
/// 確保に失敗した場合は `None` を返し、カーネルは停止せずに続行する。
/// 画面が出なくなるだけで、シリアルログという観測手段は失われないため。
/// 失敗の内訳（要求フレーム数・空きフレーム総数・最大連続空き範囲）を
/// ログに出し、「空き自体が不足」なのか「空きはあるが連続領域が足りない
/// （断片化）」なのかを判別できるようにする。
fn init_console(
    logger: &mut Logger<SerialPort>,
    framebuffer: Framebuffer,
    allocator: &mut frame_allocator::FrameAllocator,
    mapped_ranges: &MappedRanges,
) -> Option<Console> {
    const FOREGROUND: Color = Color::rgb(0xD0, 0xD8, 0xE0);
    const BACKGROUND: Color = Color::rgb(0x10, 0x10, 0x18);

    let layout = *framebuffer.layout();
    let frames_needed = layout.size_bytes().div_ceil(frame_allocator::FRAME_SIZE);

    let Some(start_frame) = allocator.allocate_contiguous(frames_needed) else {
        logger.error(format_args!(
            "console: back buffer allocation failed; screen output is disabled"
        ));
        logger.error(format_args!(
            "console:   requested {} frames ({} KiB)",
            frames_needed,
            frames_needed * frame_allocator::FRAME_SIZE / 1024
        ));
        logger.error(format_args!(
            "console:   free total {} frames, largest contiguous run {} frames",
            allocator.free_frame_count(),
            allocator.largest_contiguous_free_frames()
        ));
        logger.info(format_args!("console: serial logging continues unaffected"));
        return None;
    };

    let base = start_frame * frame_allocator::FRAME_SIZE;
    let end = base + frames_needed * frame_allocator::FRAME_SIZE;

    // M2 以来の不変条件: 使う領域は必ずマップ済みであることを確かめてから触る。
    if !mapped_ranges.contains_range(base, end) {
        logger.error(format_args!(
            "console: back buffer {base:#x}..{end:#x} is not fully mapped; \
             screen output is disabled"
        ));
        logger.info(format_args!("console: serial logging continues unaffected"));
        return None;
    }

    // SAFETY: base..end は今確保したばかりで他の誰も使っておらず、直前に
    // contains_range でマップ済みであることを確認した。framebuffer は
    // init_framebuffer が検証済みの形状で作ったもので、所有権をここへ
    // 移している（同じ領域に対する Framebuffer は他に存在しない）。
    match unsafe { Console::new(framebuffer, base, FOREGROUND, BACKGROUND) } {
        Ok(console) => {
            let (columns, rows) = console.size();
            logger.info(format_args!(
                "console: ready ({columns}x{rows} cells), back buffer {base:#x}..{end:#x} \
                 ({frames_needed} frames)"
            ));
            Some(console)
        }
        Err(e) => {
            logger.error(format_args!(
                "console: initialization failed ({e:?}); screen output is disabled"
            ));
            logger.info(format_args!("console: serial logging continues unaffected"));
            None
        }
    }
}

/// シリアルへ書き、コンソールがあれば画面にも同じ内容を書く（M3-c-3）。
///
/// **必ずシリアルを先に書く。** 画面側で何が起きてもシリアルログだけは
/// 残るようにするため。順序を入れ替えると、コンソールの不具合がシリアル
/// ログを道連れにできる構造になり、シリアルを唯一の信頼できる観測手段と
/// する方針（ADR-0003、ADR-0017 の決定 9）が崩れる。
///
/// `Logger` 自体には複数の出力先を持たせない。マルチシンクにすると
/// シリアル出力の経路が画面出力の経路に依存してしまうため（ADR-0017）。
fn log_both(
    logger: &mut Logger<SerialPort>,
    console: Option<&mut Console>,
    level: LogLevel,
    args: core::fmt::Arguments<'_>,
) {
    use core::fmt::Write;

    // シリアルが先。ここを入れ替えないこと。
    logger.log(level, args);

    if let Some(console) = console {
        let _ = writeln!(console, "[{}] {args}", level.label());
    }
}

/// コンソールが使えるようになったことを画面の先頭に示す（M3-c-3）。
///
/// 画面だけを見た人が「ログが途中から始まっている」ことを誤解しないよう、
/// ここより前のログはシリアルにしか出ていないことと、その行数を明示する。
#[cfg(not(feature = "gfx-test-pattern"))]
fn announce_console_start(logger: &mut Logger<SerialPort>, console: &mut Console) {
    use core::fmt::Write;

    // コンソール構築までに出力した行数。これが画面に出ていない分。
    let skipped = logger.emitted_line_count();

    let _ = writeln!(console, "=== ZaytOS console started ===");
    let _ = writeln!(
        console,
        "the {skipped} log line(s) above this point went to the serial port only"
    );
    let _ = writeln!(
        console,
        "(the console needs the frame allocator and the new page tables, so it"
    );
    let _ = writeln!(console, " cannot exist before this point)");
    let _ = writeln!(console);

    logger.info(format_args!(
        "console: {skipped} log line(s) were emitted before the console existed \
         (serial only)"
    ));
}

/// GDT / TSS / スタック切り替えの結果をログに残す（M4-a）。
///
/// M2-d の CR3 切り替えと同じ作法で、切り替え後に「実際に読み戻した値」を
/// 出す。設定したつもりの値ではなく、CPU が今参照している値を確認する。
fn report_gdt_and_stack(logger: &mut Logger<SerialPort>, old_rsp: u64) {
    let (gdt_base, gdt_limit) = gdt::current_gdt();
    let code_selector = gdt::current_code_selector();
    let task_register = gdt::current_task_register();

    logger.info(format_args!(
        "gdt: base={gdt_base:#x} limit={gdt_limit} (expected base={:#x})",
        gdt::gdt_base()
    ));
    logger.info(format_args!(
        "gdt: CS={code_selector:#x} (expected {:#x}), TR={task_register:#x} (expected {:#x})",
        gdt::KERNEL_CODE_SELECTOR.bits(),
        gdt::TSS_SELECTOR.bits()
    ));

    let gdt_ok = gdt_base == gdt::gdt_base()
        && code_selector == gdt::KERNEL_CODE_SELECTOR.bits()
        && task_register == gdt::TSS_SELECTOR.bits();
    if !gdt_ok {
        logger.error(format_args!(
            "gdt: read-back does not match what we loaded; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "tss: base={:#x} IST{}={:#x} RSP0={:#x}",
        gdt::tss_base(),
        gdt::DOUBLE_FAULT_IST_INDEX,
        gdt::double_fault_stack_top(),
        gdt::privilege_stack_top()
    ));
    logger.info(format_args!(
        "tss: RSP0 は特権レベル遷移（ユーザー -> カーネル）用で、M5 まで実際には使われない"
    ));

    // --- スタック切り替えの検証 ---
    let kernel_stack = stack::kernel_stack_range();
    let double_fault_stack = stack::double_fault_stack_range();
    let current_rsp = cpu::read_rsp();

    logger.info(format_args!(
        "stack: old RSP={old_rsp:#x} (UEFI-derived), new RSP={current_rsp:#x}"
    ));
    logger.info(format_args!(
        "stack: kernel stack {:#x}..{:#x} ({} KiB)",
        kernel_stack.bottom,
        kernel_stack.top,
        kernel_stack.size() / 1024
    ));
    logger.info(format_args!(
        "stack: double fault (IST{}) stack {:#x}..{:#x} ({} KiB)",
        gdt::DOUBLE_FAULT_IST_INDEX,
        double_fault_stack.bottom,
        double_fault_stack.top,
        double_fault_stack.size() / 1024
    ));

    // 現在のスタックポインタが自前の領域にあること。
    let on_own_stack = kernel_stack.contains(current_rsp);
    logger.info(format_args!(
        "stack: RSP is inside the kernel stack: {on_own_stack}"
    ));

    // ローカル変数の置き場所も自前スタック上にあること。RSP だけでなく、
    // 実際にコンパイラが使う退避先も移っていることの確認になる。
    let probe = 0xA5A5_5A5Au32;
    let probe_address = core::ptr::addr_of!(probe) as u64;
    let locals_on_own_stack = kernel_stack.contains(probe_address);
    logger.info(format_args!(
        "stack: locals live at {probe_address:#x}, inside the kernel stack: {locals_on_own_stack}"
    ));

    // 旧スタックを参照していないこと。
    let left_old_stack = !kernel_stack.contains(old_rsp) && current_rsp != old_rsp;
    logger.info(format_args!(
        "stack: no longer using the UEFI-derived stack: {left_old_stack}"
    ));

    // 実際に書き込めること（M2-d のスタック検証と同じ考え方）。
    // 現在の RSP より下（未使用側）へ直接読み書きしてみる。
    let scratch = (current_rsp - 256) as *mut u64;
    let scratch_ok = if kernel_stack.contains(scratch as u64) {
        // SAFETY: scratch は現在の RSP より 256 バイト下で、直前の
        // `kernel_stack.contains` によりカーネルスタックの範囲内であることを
        // 確認済み。まだ誰も使っていない未使用領域であり、赤ゾーン
        // （128 バイト）より外側でもある。読み書きするのはこの 8 バイトのみ。
        unsafe {
            core::ptr::write_volatile(scratch, 0x5A5A_A5A5_5A5A_A5A5);
            core::ptr::read_volatile(scratch) == 0x5A5A_A5A5_5A5A_A5A5
        }
    } else {
        false
    };
    logger.info(format_args!(
        "stack: write/read-back at {:#x}: {}",
        scratch as u64,
        if scratch_ok { "OK" } else { "NG" }
    ));

    let guards_ok = stack::guards_intact();
    logger.info(format_args!(
        "stack: guards intact (kernel={}, double-fault={})",
        stack::kernel_guard_intact(),
        stack::double_fault_guard_intact()
    ));

    if !(on_own_stack && locals_on_own_stack && left_old_stack && scratch_ok && guards_ok) {
        logger.error(format_args!(
            "stack: one or more checks failed (see above); halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "stack: switched to the kernel's own stack (GDT/TSS loaded)"
    ));
}

/// IDT のロード結果をログに残す（M4-b-1）。
///
/// GDT と同じ作法で、設定したつもりの値ではなく `sidt` で読み戻した値を
/// 確認する。
/// クリティカルセクション（`InterruptGuard`）の入れ子を実機で検証する
/// （M4-c-1）。
///
/// 各時点の IF は「設定したつもりの値」ではなく、実際の RFLAGS から読む。
/// M4-c の時点ではまだ `sti` していない（起動時から IF=0）ため、この
/// 検証で観測できるのは「入れ子で余計に有効化されないこと」と「Drop 後に
/// 元の状態へ戻ること」である。IF=1 で `enter` する経路の検証は、PIC を
/// 全マスクした M4-c-3 の後で `--critical-test` により行う（それ以前に
/// `sti` すると未検証のハンドラへ割り込みが飛ぶため危険）。
fn verify_critical_sections(logger: &mut Logger<SerialPort>) {
    use common::critical::InterruptGuard;

    fn if_set() -> bool {
        cpu::read_rflags() & cpu::RFLAGS_INTERRUPT_FLAG != 0
    }

    let before = if_set();
    logger.info(format_args!("critical: IF before any guard = {before}"));

    let mut all_ok = true;

    {
        let _outer = InterruptGuard::enter();
        let after_outer = if_set();
        // enter で必ず IF=0 になる。
        all_ok &= !after_outer;

        {
            let _middle = InterruptGuard::enter();
            let after_middle = if_set();
            all_ok &= !after_middle;

            {
                let _inner = InterruptGuard::enter();
                let after_inner = if_set();
                all_ok &= !after_inner;
                logger.info(format_args!(
                    "critical: IF after 3 nested guards = {after_inner} (expected false)"
                ));
            }
            // 内側の Drop 後。保存値が IF=0 だったので復元しない = まだ IF=0。
            let after_inner_drop = if_set();
            all_ok &= !after_inner_drop;
        }
        let after_middle_drop = if_set();
        all_ok &= !after_middle_drop;
        logger.info(format_args!(
            "critical: IF after inner guards dropped = {after_middle_drop} (still disabled)"
        ));
    }

    // 一番外側の Drop 後。起動時から IF=0 なので、保存値も IF=0 で復元しない。
    // つまり元の状態（IF=0）へ正しく戻っている。
    let after_all = if_set();
    all_ok &= after_all == before;
    logger.info(format_args!(
        "critical: IF after all guards dropped = {after_all} (expected {before}, back to start)"
    ));

    if !all_ok {
        logger.error(format_args!(
            "critical: nesting behaviour is wrong (see above); halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "critical: nested InterruptGuard behaves correctly (no spurious enable, restored on exit)"
    ));
}

fn report_idt(logger: &mut Logger<SerialPort>) {
    let (base, limit) = idt::current_idt();
    logger.info(format_args!(
        "idt: base={base:#x} limit={limit} (expected base={:#x} limit={})",
        idt::idt_base(),
        idt::expected_limit()
    ));

    if base != idt::idt_base() || limit != idt::expected_limit() {
        logger.error(format_args!(
            "idt: read-back does not match what we loaded; halting"
        ));
        cpu::halt_forever();
    }

    // 全ベクタが present で、割り込みゲート（0xE）・DPL 0 であること。
    // 1 つでも欠けると、そのベクタが発生したときに #NP になり、しかも
    // #NP のハンドラも無ければ落ちる。
    let mut all_present = true;
    let mut all_interrupt_gates = true;
    let mut all_dpl_zero = true;
    for vector in 0..idt::IDT_ENTRY_COUNT {
        let Some(entry) = idt::entry(vector) else {
            all_present = false;
            break;
        };
        all_present &= entry.is_present();
        all_interrupt_gates &= entry.gate_type() == 0xE;
        all_dpl_zero &= entry.descriptor_privilege_level() == 0;
    }
    logger.info(format_args!(
        "idt: {} entries, all present={all_present}, all interrupt gates={all_interrupt_gates}, \
         all DPL 0={all_dpl_zero}",
        idt::IDT_ENTRY_COUNT
    ));

    // ダブルフォルトだけが IST を使うこと。
    let double_fault_ist = idt::entry(8).and_then(|e| e.ist_index());
    let divide_error_ist = idt::entry(0).and_then(|e| e.ist_index());
    logger.info(format_args!(
        "idt: #DF (vector 8) IST index={double_fault_ist:?}, #DE (vector 0) IST index={divide_error_ist:?}"
    ));

    if !(all_present && all_interrupt_gates && all_dpl_zero)
        || double_fault_ist != Some(gdt::DOUBLE_FAULT_IST_INDEX as u8)
        || divide_error_ist.is_some()
    {
        logger.error(format_args!("idt: entry checks failed; halting"));
        cpu::halt_forever();
    }

    // スタブ表の刻み幅と、IDT エントリがそれを正しく指していることを検証する。
    // IDT は base + n * STUB_SIZE でエントリを作っているため、この前提が
    // 崩れると全エントリが誤ったアドレスを指す。同じ式で検算しても循環
    // するので、アセンブラが付けた独立のラベルと突き合わせる。
    let check = idt::check_stub_table();
    logger.info(format_args!(
        "idt: stub table {:#x}..{:#x} size={} (expected {}), stride={}, entries={}",
        check.base,
        check.end,
        check.actual_size,
        check.expected_size,
        if check.stride_ok { "OK" } else { "NG" },
        if check.entries_ok { "OK" } else { "NG" }
    ));
    if !check.is_ok() {
        logger.error(format_args!(
            "idt: stub table layout is broken; every IDT entry would point at the \
             wrong address; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "idt: loaded (exceptions now reach our handlers; interrupts stay disabled until M4-d)"
    ));
}

/// 8259A PIC を 0x20-0x2F へ再マップし、全 IRQ をマスクする（M4-c-3）。
///
/// 再マップ**前**の IMR も記録する。UEFI が何を開けたまま制御を渡してきたかは
/// 実際に読まないと分からず、後で「誰も設定していないはずの IRQ が来る」と
/// 悩んだときの手掛かりになる（OVMF はアイドル中もタイマ割り込みを処理して
/// いる。`docs/troubleshooting.md` の起動ログのベースライン）。
fn configure_pic(logger: &mut Logger<SerialPort>) {
    let (master_before, slave_before) = pic::read_masks();
    logger.info(format_args!(
        "pic: IMR before remap master={master_before:#04x} ({master_before:#010b}) \
         slave={slave_before:#04x} ({slave_before:#010b}) [0 = unmasked]"
    ));

    // 再マップ先が IDT のカバー範囲に入っており、present なハンドラを持つ
    // ことを**再マップより先に**確かめる。順序が逆だと、検査に落ちた場合
    // でも PIC は既に新しいベクタを向いており、halt するまでの間に IRQ が
    // 届けば行き先の無いベクタへ飛ぶ。
    let first = pic::MASTER_VECTOR_OFFSET as usize;
    let last = pic::SLAVE_VECTOR_OFFSET as usize + pic::IRQS_PER_PIC as usize - 1;
    let mut covered = true;
    for vector in first..=last {
        covered &= idt::entry(vector).is_some_and(|entry| entry.is_present());
    }
    logger.info(format_args!(
        "pic: target vectors {first:#04x}..={last:#04x} are covered by present IDT entries={covered} \
         (IDT has {} entries)",
        idt::IDT_ENTRY_COUNT
    ));
    if !covered {
        logger.error(format_args!(
            "pic: refusing to remap onto vectors without a present handler; halting"
        ));
        cpu::halt_forever();
    }

    // SAFETY: 起動時の単一実行文脈で、呼び出しはこの 1 回だけ。`_start` 冒頭の
    // `cli` により割り込みは禁止されたままである。行き先のベクタに present な
    // ハンドラがあることは直前に確認した。
    let result = unsafe { pic::remap(pic::MASTER_VECTOR_OFFSET, pic::SLAVE_VECTOR_OFFSET) };
    if let Err(error) = result {
        logger.error(format_args!(
            "pic: rejected the vector offsets ({error:?}); halting"
        ));
        cpu::halt_forever();
    }

    // ベクタオフセットは**書いた値であって、検証した値ではない**。ICW2 は
    // 書き込み専用で、データポートから読めるのは IMR だけである。したがって
    // ここは「こう書いた」以上のことを主張できない。断定形で書くと、下の
    // マスク検証が通ったことをもって再マップ全体が正しいと読めてしまう。
    logger.info(format_args!(
        "pic: programmed master={:#04x}-{:#04x} slave={:#04x}-{:#04x} \
         (ICW2 is write-only; the offset cannot be read back)",
        pic::MASTER_VECTOR_OFFSET,
        pic::MASTER_VECTOR_OFFSET + pic::IRQS_PER_PIC - 1,
        pic::SLAVE_VECTOR_OFFSET,
        pic::SLAVE_VECTOR_OFFSET + pic::IRQS_PER_PIC - 1
    ));

    // 一方 IMR は読める。「設定したつもり」ではなく実際の値を読み戻す。
    // ICW シーケンスが途中で崩れていると、最後の OCW1 が ICW として
    // 解釈されてマスクが掛からない。そのまま M4-d で `sti` すると、
    // ハンドラの無い IRQ がいきなり飛んでくる。
    let (master_after, slave_after) = pic::read_masks();
    logger.info(format_args!(
        "pic: IMR after remap master={master_after:#04x} slave={slave_after:#04x} \
         (expected {:#04x}/{:#04x}) [read back from hardware]",
        pic::MASK_ALL,
        pic::MASK_ALL
    ));
    if master_after != pic::MASK_ALL || slave_after != pic::MASK_ALL {
        logger.error(format_args!(
            "pic: the mask read-back does not match; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "pic: all IRQs masked (nothing can fire until M4-d unmasks the timer explicitly); \
         the vector offset stays unverified until the first timer IRQ arrives as vector \
         {:#04x} in M4-d",
        pic::MASTER_VECTOR_OFFSET
    ));
}

/// `--exception-test` で各 GPR に入れる既知の値。
///
/// レジスタごとに異なる値にしてあるので、ダンプで名前と値の対応が入れ替わって
/// いれば一目で分かる。`.bss` のゼロ埋め検証で毒値を使ったのと同じ考え方。
/// この値は xtask 側の突き合わせ表と一致していなければならない。
#[cfg(feature = "exception-test-invalid-opcode")]
mod known_register_values {
    pub const RAX: u64 = 0x1111_1111_1111_1111;
    pub const RBX: u64 = 0x2222_2222_2222_2222;
    pub const RCX: u64 = 0x3333_3333_3333_3333;
    pub const RDX: u64 = 0x4444_4444_4444_4444;
    pub const RSI: u64 = 0x5555_5555_5555_5555;
    pub const RDI: u64 = 0x6666_6666_6666_6666;
    pub const RBP: u64 = 0x7777_7777_7777_7777;
    pub const R8: u64 = 0x8888_8888_8888_8888;
    pub const R9: u64 = 0x9999_9999_9999_9999;
    pub const R10: u64 = 0xAAAA_AAAA_AAAA_AAAA;
    pub const R11: u64 = 0xBBBB_BBBB_BBBB_BBBB;
    pub const R12: u64 = 0xCCCC_CCCC_CCCC_CCCC;
    pub const R13: u64 = 0xDDDD_DDDD_DDDD_DDDD;
    pub const R14: u64 = 0xEEEE_EEEE_EEEE_EEEE;
    pub const R15: u64 = 0xFFFF_FFFF_FFFF_FFFF;
}

/// 全 GPR に既知の値を入れてから `ud2` を実行する。
///
/// naked にしているのは、コンパイラに一切レジスタを触らせないため。通常の
/// `asm!` では callee-saved レジスタ（rbx, rbp, r12-r15）を自由に書き換え
/// られず、書き換えると呼び出し規約を壊す。ここは戻らないので問題ない。
///
/// **RSP には触れない。** 触ると例外配送そのものが失敗する。
///
/// # Safety
///
/// `ud2` により必ず #UD が発生し、ハンドラが停止するため戻らない。
#[cfg(feature = "exception-test-invalid-opcode")]
#[unsafe(naked)]
unsafe extern "sysv64" fn trigger_invalid_opcode_with_known_registers() -> ! {
    use known_register_values as v;
    core::arch::naked_asm!(
        "movabs rax, {rax}",
        "movabs rbx, {rbx}",
        "movabs rcx, {rcx}",
        "movabs rdx, {rdx}",
        "movabs rsi, {rsi}",
        "movabs rdi, {rdi}",
        "movabs rbp, {rbp}",
        "movabs r8, {r8}",
        "movabs r9, {r9}",
        "movabs r10, {r10}",
        "movabs r11, {r11}",
        "movabs r12, {r12}",
        "movabs r13, {r13}",
        "movabs r14, {r14}",
        "movabs r15, {r15}",
        "ud2",
        rax = const v::RAX,
        rbx = const v::RBX,
        rcx = const v::RCX,
        rdx = const v::RDX,
        rsi = const v::RSI,
        rdi = const v::RDI,
        rbp = const v::RBP,
        r8 = const v::R8,
        r9 = const v::R9,
        r10 = const v::R10,
        r11 = const v::R11,
        r12 = const v::R12,
        r13 = const v::R13,
        r14 = const v::R14,
        r15 = const v::R15,
    );
}

/// ページフォルトを起こすために読みに行くアドレス。
///
/// 実装している物理メモリ（256MiB）からも、フレームバッファや MMIO の窓からも
/// 遠い、明らかにマップされていない値を選ぶ。上位ビットが符号拡張された
/// 正準アドレスなので、#GP ではなく #PF になる。
///
/// 使う前に `MappedRanges::contains_range` で本当にマップされていないことを
/// 確認する。偶然マップされている領域を選ぶと、フォルトが起きずテストが
/// 成功したように見える。
#[cfg(any(
    feature = "exception-test-page-fault",
    feature = "exception-test-double-fault"
))]
const UNMAPPED_PROBE_ADDRESS: u64 = 0x0000_4000_0000_0000;

/// `--exception-test` 用に、意図した例外をわざと発生させる。
///
/// 通常ビルドには含まれない。`cargo xtask run --exception-test <kind>` が
/// 対応する feature を有効にしてビルドする。
#[cfg(feature = "exception-test")]
#[cfg_attr(
    not(any(
        feature = "exception-test-page-fault",
        feature = "exception-test-double-fault"
    )),
    allow(unused_variables)
)]
#[cfg_attr(feature = "exception-test-invalid-opcode", allow(unreachable_code))]
fn trigger_exception_under_test(
    logger: &mut Logger<SerialPort>,
    mapped_ranges: &MappedRanges,
) -> ! {
    #[cfg(feature = "exception-test-divide-by-zero")]
    {
        logger.info(format_args!(
            "exception-test: about to trigger #DE (divide by zero)"
        ));
        // Rust の `/` はゼロ除算を検査してパニックするため #DE にならない。
        // div 命令を直接実行する。
        // SAFETY: 意図的に #DE を起こすためのテスト経路。ハンドラが停止する。
        unsafe {
            core::arch::asm!(
                "xor rdx, rdx",
                "mov rax, 1",
                "xor rcx, rcx",
                "div rcx",
                out("rax") _,
                out("rdx") _,
                out("rcx") _,
                options(nostack),
            );
        }
    }

    #[cfg(feature = "exception-test-invalid-opcode")]
    {
        logger.info(format_args!(
            "exception-test: about to trigger #UD (invalid opcode) with known registers"
        ));
        // SAFETY: 意図的に #UD を起こすためのテスト経路。戻らない。
        unsafe {
            trigger_invalid_opcode_with_known_registers();
        }
    }

    #[cfg(any(
        feature = "exception-test-page-fault",
        feature = "exception-test-double-fault"
    ))]
    {
        let probe = UNMAPPED_PROBE_ADDRESS;

        // 本当にマップされていないことを確認してから使う。マップされて
        // いればフォルトが起きず、テストが通ったように見えてしまう。
        let unmapped = !mapped_ranges.contains_range(probe, probe + 8);
        logger.info(format_args!(
            "exception-test: probe address {probe:#x} is unmapped: {unmapped}"
        ));
        if !unmapped {
            logger.error(format_args!(
                "exception-test: the probe address is mapped; the test would silently \
                 pass without faulting. halting"
            ));
            cpu::halt_forever();
        }

        #[cfg(feature = "exception-test-double-fault")]
        {
            // #PF のゲートを不在にしてからページフォルトを起こす。例外の
            // 配送そのものが #NP（Contributory 分類）を引き起こすため、
            // 「Page Fault の配送中に Contributory」の組み合わせが成立して
            // #DF へ昇格する（ADR-0018）。
            //
            // スタックを溢れさせる方法は使えない。犠牲領域は .bss 内の
            // マップ済みメモリであり、溢れてもページフォルトが起きないため。
            logger.info(format_args!(
                "exception-test: clearing the present bit of the #PF gate (vector 14)"
            ));
            // SAFETY: このあと意図的にページフォルトを起こし、#DF へ昇格
            // させるためのテスト経路。ハンドラが停止するので復元は不要。
            unsafe {
                idt::clear_present(14);
            }
            logger.info(format_args!(
                "exception-test: about to trigger #DF (via a page fault with no #PF handler)"
            ));
        }

        #[cfg(all(
            feature = "exception-test-page-fault",
            not(feature = "exception-test-double-fault")
        ))]
        logger.info(format_args!(
            "exception-test: about to trigger #PF (read from an unmapped address)"
        ));

        // SAFETY: 意図的にページフォルトを起こすためのテスト経路。直前に
        // マップされていないことを確認済みで、ハンドラが停止する。
        unsafe {
            let value = core::ptr::read_volatile(probe as *const u64);
            // 到達しないが、最適化で読み取りごと消えないよう値を使う。
            logger.error(format_args!(
                "exception-test: the read unexpectedly succeeded ({value:#x}); halting"
            ));
        }
    }

    logger.error(format_args!(
        "exception-test: the expected exception did not fire; halting"
    ));
    cpu::halt_forever();
}

/// `--critical-test` 用に、ロックとクリティカルセクションの異常経路を
/// わざと踏む。
///
/// 通常ビルドには含まれない。`cargo xtask run --critical-test <kind>` が
/// 対応する feature を有効にしてビルドする。
#[cfg(feature = "critical-test")]
#[allow(unreachable_code)]
fn trigger_critical_test(logger: &mut Logger<SerialPort>) -> ! {
    #[cfg(feature = "critical-test-double-lock")]
    {
        use common::critical::Locked;

        logger.info(format_args!(
            "critical-test: about to acquire the same lock twice (double-lock detection)"
        ));

        // ヒープのロックではなく専用の Locked を使う。ヒープを壊すと
        // 以降のログ出力そのものが巻き添えになるため。検出の仕組みは
        // 同じ Locked<T> の実装なので、これで十分に検証できる。
        static PROBE: Locked<u64> = Locked::new(0);

        let _held = PROBE.lock();
        logger.info(format_args!(
            "critical-test: first lock acquired; the next lock() must be detected and halt"
        ));
        // 保持したまま再取得する。検出されればここから戻らない。
        let _second = PROBE.lock();

        logger.error(format_args!(
            "critical-test: the second lock() returned; double-lock detection FAILED"
        ));
        cpu::halt_forever();
    }

    #[cfg(feature = "critical-test-restore-enabled")]
    {
        // IF=1 で enter した場合の復元経路。**PIC を全マスクしてからでないと
        // 危険**（未検証のハンドラへ割り込みが飛ぶ）。M4-c-3 で PIC の
        // マスクを確認したうえで実行する。
        logger.info(format_args!(
            "critical-test: enabling interrupts temporarily to exercise the restore path"
        ));
        // SAFETY: PIC は全 IRQ マスク済みで、IDT の全 256 ベクタに
        // ハンドラが入っている（M4-b-1）。この区間で割り込みが届いても
        // 「予期しないベクタ」として報告されるだけで、無言では落ちない。
        unsafe {
            cpu::enable_interrupts();
        }
        let enabled = cpu::read_rflags() & cpu::RFLAGS_INTERRUPT_FLAG != 0;
        logger.info(format_args!(
            "critical-test: IF after sti = {enabled} (expected true)"
        ));

        let restored = {
            let _guard = common::critical::InterruptGuard::enter();
            let inside = cpu::read_rflags() & cpu::RFLAGS_INTERRUPT_FLAG != 0;
            logger.info(format_args!(
                "critical-test: IF inside the guard = {inside} (expected false)"
            ));
            // ガードを抜けると、保存値が IF=1 なので復元されるはず。
            !inside
        };
        let after = cpu::read_rflags() & cpu::RFLAGS_INTERRUPT_FLAG != 0;
        logger.info(format_args!(
            "critical-test: IF after the guard dropped = {after} (expected true, restored)"
        ));

        // 後片付け。以降は割り込みを禁止したままにする。
        // SAFETY: 検証が終わったので、M4-d まで再び禁止しておく。
        unsafe {
            cpu::disable_interrupts();
        }

        if enabled && restored && after {
            logger.info(format_args!(
                "critical-test: restore path OK (IF=1 on enter is restored on drop)"
            ));
        } else {
            logger.error(format_args!(
                "critical-test: restore path FAILED (enabled={enabled} inside_disabled={restored} \
                 after={after})"
            ));
        }
        cpu::halt_forever();
    }

    logger.error(format_args!(
        "critical-test: no test kind was selected; halting"
    ));
    cpu::halt_forever();
}

/// ロックの保持中に割り込みが禁止され、解放後に元へ戻ることを確認する
/// （M4-c-2）。
///
/// `Locked<T>` は取得中に `InterruptGuard` を保持する。その効果を実 RFLAGS で
/// 観測する。現状は起動時から IF=0 なので「保持中も IF=0、解放後も IF=0
/// （元の状態）」になる。IF=1 から入る経路は M4-c-3 の後に
/// `--critical-test restore-enabled` で確認する。
fn report_lock_interrupt_state(logger: &mut Logger<SerialPort>) {
    use common::critical::Locked;

    fn if_set() -> bool {
        cpu::read_rflags() & cpu::RFLAGS_INTERRUPT_FLAG != 0
    }

    static PROBE: Locked<u64> = Locked::new(0);

    let before = if_set();
    let (inside, value) = {
        let mut guard = PROBE.lock();
        *guard = 0xABCD;
        (if_set(), *guard)
    };
    let after = if_set();

    logger.info(format_args!(
        "lock: IF before={before} while held={inside} after={after} (value read back = {value:#x})"
    ));

    // 保持中は必ず IF=0。解放後は元の状態へ戻る。
    if inside || after != before || value != 0xABCD {
        logger.error(format_args!(
            "lock: interrupt state around the guard is wrong; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "lock: interrupts are disabled while the guard is held and restored afterwards"
    ));
}

/// `--interrupt-test` 用に、割り込みを有効化する経路を踏む（M4-d-1）。
///
/// 通常ビルドには含まれない。`cargo xtask run --interrupt-test <kind>` が
/// 対応する feature を有効にしてビルドする。
#[cfg(feature = "interrupt-test")]
#[allow(unreachable_code)]
fn trigger_interrupt_test(
    logger: &mut Logger<SerialPort>,
    #[allow(unused)] console: Option<&mut Console>,
) -> ! {
    /// スピンする長さと、ハートビートの間隔（TSC サイクル）。
    ///
    /// TSC の周波数は環境依存で、時刻源として信用できない（`cpu` モジュール
    /// 参照）。ここでは「だいたいこのくらい回れば十分」という目安として
    /// 使うだけなので、絶対時間の正確さは要らない。
    const SPIN_CYCLES: u64 = 2_000_000_000;
    const HEARTBEAT_CYCLES: u64 = 400_000_000;

    let report = interrupts::verify_ready_for_sti(logger);

    if !report.may_enable_interrupts() {
        logger.error(format_args!(
            "interrupt-test: the pre-sti checks did not pass; refusing to sti (ADR-0018 §2)"
        ));
        cpu::halt_forever();
    }

    #[cfg(feature = "interrupt-test-irq-path")]
    {
        // IRQ 経路が GPR を復元することを、ソフトウェア割り込みで確かめる。
        verify_irq_path_restores_registers(logger);
    }

    #[cfg(feature = "interrupt-test-timer")]
    {
        /// 何ティックで止めるか。100Hz なので 500 ティック = 約 5 秒。
        ///
        /// TCG で `-d int` を有効にすると 1 ティックあたり 20 行強が
        /// 記録される。500 ティックで約 1 万行・800KB 程度に収まる
        /// （M4-b-1 のログが 14,400 行だったので同程度）。通常起動では
        /// 止めずに回し続ける。
        const STOP_AFTER_TICKS: u64 = 500;
        start_timer(logger, console, STOP_AFTER_TICKS);
    }

    logger.info(format_args!(
        "interrupt-test: all pre-sti checks passed (or are explicitly unverifiable); enabling interrupts"
    ));

    // SAFETY: 直前に 7 項目を検証し、blocks_sti() な項目が無いことを
    // 確認した。全 IRQ はマスク済みで、IDT の全 256 ベクタに present な
    // ハンドラが入っている。
    unsafe {
        interrupts::spin_with_interrupts_enabled(logger, SPIN_CYCLES, HEARTBEAT_CYCLES);
    }

    // **増加分で判定する。** テスト用ベクタを PIC の範囲外へ移しても、これは
    // 必要なままである。カウンタは全 256 ベクタを対象に合計するので、
    // irq-path の `int 0x40` で計上された 1 件は絶対値に残る。0x20 が
    // 汚れなくなっただけで、絶対値では「スピン中に届いた」と誤判定する
    // 構造は変わらない。
    //
    // 合計の対象を PIC の範囲だけに絞る案は採らない。ここで見たいのは
    // 「何も届かないこと」であって、`cli` でマスクできない NMI（ベクタ 2）を
    // 含む全ベクタが対象である。
    let delta = interrupts::spin_interrupt_delta();
    let (absolute_total, _) = idt::interrupt_total_and_first_nonzero();
    let iterations = interrupts::loop_iterations();
    logger.info(format_args!(
        "interrupt-test: spin finished; loop iterations={iterations}, \
         interrupts during the spin={delta} (absolute total since boot={absolute_total})"
    ));

    // 周回回数も判定に含める。0 回なら「割り込みが来なかった」のではなく
    // 「そもそもループが回っていない」ので、意味がまるで違う。
    if iterations == 0 {
        logger.error(format_args!(
            "interrupt-test: the loop never iterated; sti-then-idle FAILED (not an interrupt problem)"
        ));
    } else if delta != 0 {
        logger.error(format_args!(
            "interrupt-test: something was delivered while every IRQ is masked; \
             sti-then-idle FAILED"
        ));
    } else {
        logger.info(format_args!(
            "interrupt-test: sti-then-idle OK (interrupts enabled, loop ran {iterations} times, nothing arrived)"
        ));
    }

    cpu::halt_forever();
}

/// IRQ 経路が GPR を復元することを、ソフトウェア割り込みで確かめる。
///
/// 使うのは **PIC の範囲外**のベクタ 0x40（`idt::TEST_VECTOR`）である。
/// `int 0x40` は 8259A を経由せず CPU が直接 IDT を引くので、マスク状態と
/// 無関係にハンドラ経路だけを試せるうえ、**EOI の論理が一切絡まない**。
/// PIC 経由で配送されないベクタなので、ハンドラが EOI を送らないことが
/// そのまま正しい実装になる。
///
/// 各 GPR にレジスタごとに異なる既知値を入れ、`int` の前後で一致することを
/// 見る。1 本でも復元を落とすと、そのレジスタだけ値が変わる。
///
/// # なぜ 0x20 ではなく PIC の範囲外を使うのか
///
/// M4-d-1 では `int 0x20` を使っていたが、M4-d-2 で EOI を実装すると衝突する。
/// ソフトウェア割り込みは実在の IRQ ではないため、タイマハンドラが無条件に
/// EOI を送る作りだと**起きてもいない割り込みに応答する**ことになり、PIC の
/// 優先度スタックを壊しうる。
///
/// 検討した代替案:
///
/// - **実タイマでの検証に置き換える**: 却下。「GPR が壊れた」ことは分かるが、
///   壊れたのがスタブか PIT 設定か EOI かを切り分けられない。ハンドラ経路
///   だけを単独で試せるという、この検証の価値そのものが失われる。
/// - **ハンドラ側でソフトウェア割り込み由来かを判別して EOI を抑制する**:
///   却下。本番経路にテスト専用の分岐が入るうえ、判別を誤れば本物の割り込みへ
///   EOI を送らない側へ倒れ、以降の割り込みが全部止まる。テストのために
///   本番経路の信頼性を下げることになる。
///
/// PIC の範囲外へ移すのが、本番経路に一切手を入れずに済む唯一の案だった
/// （ADR-0018 Addendum 3）。
#[cfg(feature = "interrupt-test-irq-path")]
fn verify_irq_path_restores_registers(logger: &mut Logger<SerialPort>) {
    // レジスタごとに異なる既知値。値が入れ替わっても気づけるようにする
    // （M4-b-2 の GPR ダンプ検証と同じ考え方）。
    //
    // **rbx と rbp は検査できない。** LLVM がこの 2 本を内部的に予約して
    // おり、`asm!` のオペランドに指定できない（フレームポインタ等に使う）。
    // 検査できるのは残る 13 本である。順序の取り違えは 13 本の相異なる値で
    // 十分に捕まり、本数の過不足は RSP がずれて `iretq` の時点で即座に
    // 壊れるため、この 2 本が抜けても検査の意味は保たれる。
    let mut regs: [u64; 13] = [
        0x0101_0101_0101_0101, // rax
        0x0202_0202_0202_0202, // rcx
        0x0303_0303_0303_0303, // rdx
        0x0404_0404_0404_0404, // rsi
        0x0505_0505_0505_0505, // rdi
        0x0606_0606_0606_0606, // r8
        0x0707_0707_0707_0707, // r9
        0x0808_0808_0808_0808, // r10
        0x0909_0909_0909_0909, // r11
        0x0a0a_0a0a_0a0a_0a0a, // r12
        0x0b0b_0b0b_0b0b_0b0b, // r13
        0x0c0c_0c0c_0c0c_0c0c, // r14
        0x0d0d_0d0d_0d0d_0d0d, // r15
    ];
    let before = regs;

    // SAFETY: ベクタ 0x20 の IDT エントリは IRQ スタブを指しており（起動時に
    // check_irq_stub_table で検証済み）、そのスタブは GPR を退避・復元して
    // `iretq` で戻る。割り込みゲートなので入場時に IF はクリアされ、戻る
    // ときに復元される。`nostack` は付けない（ハンドラがスタックを使う）。
    unsafe {
        core::arch::asm!(
            // idt::TEST_VECTOR と同じ値。`int` のオペランドは即値でなければ
            // ならず、定数を差し込めないため、ここだけ数値が重複する。
            // 食い違いは下の const アサーションで防いでいる。
            "int 0x40",
            inout("rax") regs[0],
            inout("rcx") regs[1],
            inout("rdx") regs[2],
            inout("rsi") regs[3],
            inout("rdi") regs[4],
            inout("r8") regs[5],
            inout("r9") regs[6],
            inout("r10") regs[7],
            inout("r11") regs[8],
            inout("r12") regs[9],
            inout("r13") regs[10],
            inout("r14") regs[11],
            inout("r15") regs[12],
        );
    }

    let after = regs;
    // `int 0x40` の 0x40 と idt::TEST_VECTOR が食い違わないことを固定する。
    const _: () = assert!(idt::TEST_VECTOR == 0x40);

    let count = idt::interrupt_count(idt::TEST_VECTOR);

    logger.info(format_args!(
        "irq-path: int 0x40 handled (handler count for vector 0x40 = {count})"
    ));
    logger.info(format_args!(
        "irq-path: rax={:#x} rcx={:#x} r15={:#x} (13 registers checked; rbx and rbp cannot be)",
        after[0], after[1], after[12]
    ));

    if count != 1 {
        logger.error(format_args!(
            "irq-path: the handler ran {count} time(s), expected exactly 1; FAILED"
        ));
        cpu::halt_forever();
    }

    if before != after {
        logger.error(format_args!(
            "irq-path: a general purpose register was not restored across the IRQ; FAILED"
        ));
        for (index, (expected, actual)) in before.iter().zip(after.iter()).enumerate() {
            if expected != actual {
                logger.error(format_args!(
                    "irq-path:   register slot {index}: expected {expected:#x}, got {actual:#x}"
                ));
            }
        }
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "irq-path: OK (the IRQ stub returned via iretq and every checked register survived)"
    ));
}

/// PIT を設定し、IRQ0 を解禁してタイマを動かす（M4-d-2）。
///
/// # 割り込みを有効化するまでの順序
///
/// 設定中に割り込みが飛び込む余地を作らないため、順序を固定している。
///
/// 1. PIT を設定する（この時点で IRQ0 はマスクされたまま）
/// 2. IMR を読み戻し、**まだ全マスクのまま**であることを確認する。
///    PIT の設定が誤って IMR を触っていないことの確認。ポート 0x21（IMR）と
///    0x40/0x43（PIT）は番号が近く、定数の書き間違いが起こりうる
/// 3. IRQ0 のマスクを解除する（**解禁はこの 1 箇所のみ**）
/// 4. IMR を読み戻し、master=0xFE / slave=0xFF を照合する
/// 5. `sti` 前 7 項目を再検証する（項目 5 の期待値が 0xFF から 0xFE へ変わる）
/// 6. `sti`（`run_timer_loop` の中で行う）
fn start_timer(
    logger: &mut Logger<SerialPort>,
    console: Option<&mut Console>,
    stop_after_ticks: u64,
) -> ! {
    // --- 1. PIT を設定する ---
    // SAFETY: 起動時に 1 回だけ。この時点で IRQ0 はマスクされている
    // （M4-c-3 の remap が全マスクで終わり、以降解除していない）。
    let divisor = match unsafe { pit::configure_channel0(pit::TARGET_FREQUENCY_HZ) } {
        Ok(divisor) => divisor,
        Err(error) => {
            logger.error(format_args!(
                "pit: refused the requested frequency ({error:?}); halting"
            ));
            cpu::halt_forever();
        }
    };
    let actual = pit::actual_frequency_millihertz(divisor);
    logger.info(format_args!(
        "pit: channel 0 set to divisor={divisor} for a requested {} Hz; actual is {}.{:03} Hz \
         (the divisor is an integer, so the period never matches exactly)",
        pit::TARGET_FREQUENCY_HZ,
        actual / 1000,
        actual % 1000
    ));

    // --- 2. PIT の設定が IMR を壊していないことを確認する ---
    let (master_before, slave_before) = pic::read_masks();
    logger.info(format_args!(
        "pit: IMR after configuring the PIT master={master_before:#04x} slave={slave_before:#04x} \
         (must still be {:#04x}/{:#04x}; the PIT ports must not touch the IMR)",
        pic::MASK_ALL,
        pic::MASK_ALL
    ));
    if master_before != pic::MASK_ALL || slave_before != pic::MASK_ALL {
        logger.error(format_args!(
            "pit: configuring the PIT changed the interrupt mask; halting"
        ));
        cpu::halt_forever();
    }

    // --- 3. IRQ0 を解禁する（ここが唯一の解禁箇所）---
    // SAFETY: ベクタ 0x20 には IRQ スタイルのスタブが入っており（起動時に
    // check_irq_stub_table で検証済み）、ハンドラは EOI を発行する。
    unsafe {
        pic::unmask_irq(0);
    }

    // --- 4. 解禁の結果を読み戻す ---
    let expected_master = pic::MASK_ALL & !(1 << 0);
    let (master_after, slave_after) = pic::read_masks();
    logger.info(format_args!(
        "pic: IMR after unmasking IRQ0 master={master_after:#04x} slave={slave_after:#04x} \
         (expected {expected_master:#04x}/{:#04x}) [read back from hardware]",
        pic::MASK_ALL
    ));
    if master_after != expected_master || slave_after != pic::MASK_ALL {
        logger.error(format_args!(
            "pic: the mask read-back after unmasking IRQ0 does not match; halting"
        ));
        cpu::halt_forever();
    }

    // --- 4.5 キーボード（IRQ1）を用意する ---
    setup_keyboard(logger);

    // --- 5. sti 前 7 項目を再検証する ---
    let report = interrupts::verify_ready_for_sti_with_timer(logger);
    if !report.may_enable_interrupts() {
        logger.error(format_args!(
            "interrupt-test: the pre-sti checks did not pass; refusing to sti (ADR-0018 §2)"
        ));
        cpu::halt_forever();
    }

    // --- 6. sti してループへ入る ---
    // SAFETY: 7 項目を検証し、PIT を設定し、IRQ0 のマスクを外した。
    // ベクタ 0x20 のハンドラはティックを数えて EOI を送る。
    unsafe {
        interrupts::run_timer_loop(logger, console, stop_after_ticks);
    }

    // stop_after_ticks == 0 なら run_timer_loop は戻らないので、ここから先は
    // 回帰チェック（`--interrupt-test timer`）でしか実行されない。
    let ticks = idt::timer_ticks();
    logger.info(format_args!(
        "interrupt-test: timer stopped at {ticks} tick(s), max tick jump per wakeup={}",
        interrupts::max_tick_jump()
    ));

    // EOI が出ていなければ 1 回で止まる。2 以上増えたこと自体が EOI の
    // 動作証明である（ADR-0018 §2 の項目 7）。
    if ticks >= 2 {
        logger.info(format_args!(
            "interrupt-test: timer OK (ticks kept coming, which proves the handler issues EOI)"
        ));
    } else {
        logger.error(format_args!(
            "interrupt-test: timer FAILED - only {ticks} tick(s); the handler is not issuing EOI"
        ));
    }

    cpu::halt_forever();
}

/// i8042 を検証してから IRQ1 を解禁する（M4-e）。
///
/// 順序に意味がある。
///
/// 1. コンフィグバイトを**読んで**、翻訳（セット 1）と割り込みが有効かを見る
/// 2. 落ちていれば立てて書き戻し、**読み直して一致を確認**する
/// 3. 出力バッファの残留データを読み捨てる
/// 4. IRQ1 のマスクを解除する
/// 5. IMR を読み戻して `master=0xFC` を照合する
///
/// 3 を 4 より前に置くのが要点。ファームウェアが残したバイトが最初のキー
/// 入力として現れる事故を防ぐ。OVMF はブートメニューでキーを扱っているので、
/// 何か残っていてもおかしくない。
fn setup_keyboard(logger: &mut Logger<SerialPort>) {
    use keyboard::controller;

    // --- 1. コンフィグバイトを読む ---
    // SAFETY: 起動シーケンス中で IRQ1 はマスクされており、他の実行文脈が
    // i8042 を触っていない。
    let config = match unsafe { controller::read_config() } {
        Ok(config) => config,
        Err(error) => {
            logger.error(format_args!(
                "i8042: failed to read the configuration byte ({error:?}); halting"
            ));
            cpu::halt_forever();
        }
    };
    logger.info(format_args!(
        "i8042: configuration byte = {config:#010b} (keyboard interrupt={}, translation to set 1={})",
        config & controller::CONFIG_KEYBOARD_INTERRUPT != 0,
        config & controller::CONFIG_TRANSLATION != 0
    ));

    // --- 2. 必要なら立てて、読み直して確認する ---
    if !controller::config_is_ready(config) {
        let updated = controller::config_with_keyboard_enabled(config);
        logger.info(format_args!(
            "i8042: enabling the missing bits ({config:#04x} -> {updated:#04x})"
        ));
        // SAFETY: 同上。書いた後に読み直して照合する。
        if let Err(error) = unsafe { controller::write_config(updated) } {
            logger.error(format_args!(
                "i8042: the configuration byte did not stick ({error:?}); halting"
            ));
            cpu::halt_forever();
        }
        logger.info(format_args!(
            "i8042: configuration byte verified by reading it back"
        ));
    } else {
        logger.info(format_args!(
            "i8042: the configuration byte is already what we need; leaving it alone"
        ));
    }

    // --- 3. 残留データを読み捨てる ---
    // SAFETY: IRQ1 はまだマスクされている。
    let discarded = unsafe { controller::drain_output_buffer() };
    if discarded > 0 {
        logger.info(format_args!(
            "i8042: discarded {discarded} stale byte(s) left in the output buffer by the firmware \
             (they would otherwise look like the first keypress)"
        ));
    } else {
        logger.info(format_args!("i8042: the output buffer was already empty"));
    }

    // --- 4. IRQ1 を解禁する ---
    // SAFETY: ベクタ 0x21 には IRQ スタイルのスタブが入っており、ハンドラは
    // データポートを読み切ってから EOI を送る。
    unsafe {
        pic::unmask_irq(keyboard::KEYBOARD_IRQ);
    }

    // --- 5. IMR を読み戻す ---
    let expected_master = pic::MASK_ALL & !(1 << 0) & !(1 << keyboard::KEYBOARD_IRQ);
    let (master, slave) = pic::read_masks();
    logger.info(format_args!(
        "pic: IMR after unmasking IRQ1 master={master:#04x} slave={slave:#04x} \
         (expected {expected_master:#04x}/{:#04x}) [read back from hardware]",
        pic::MASK_ALL
    ));
    if master != expected_master || slave != pic::MASK_ALL {
        logger.error(format_args!(
            "pic: the mask read-back after unmasking IRQ1 does not match; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "keyboard: IRQ1 is unmasked; press a key (the first one must arrive as vector {:#04x})",
        keyboard::KEYBOARD_VECTOR
    ));
}

/// 2MiB ページの分割とアンマップを、影響のない領域で一巡させる（M5-a-2-1）。
///
/// # なぜ通常起動で毎回行うのか
///
/// M4 までの検証（GDT / IDT / IMR の読み戻し、`sti` 前 7 項目）と同じ扱いに
/// する。回帰として常に効くほうが、feature で囲って忘れるより価値がある。
///
/// # なぜ影響のない領域を使うのか
///
/// 失敗したときにログを最後まで出せるようにするためである。実行中のコードや
/// スタックが載るページを対象にすると、失敗した瞬間に何も観測できないまま
/// 落ちる。**実測ではコードもスタックも 4KiB ページに載っており**
/// （`report_mapping_granularity`）、そもそも 2MiB の分割対象にならない。
/// ヒープとフレームバッファは 2MiB に載っているが、どちらも稼働中なので
/// 通常起動では触らない。
///
/// そこでフレームアロケータから 2MiB 境界に揃った 512 フレームを確保し、
/// それを対象にする。アロケータが確保済みとして扱うので他の誰も使わない。
/// 確保したまま解放しないので、その分のメモリは失われる（量はログに出す）。
///
/// # 照合は独立した経路で行う
///
/// 分割後の 512 エントリを `split_child_entry` と同じ式で検算しても、
/// 同じ間違いを 2 回するだけで何も確かめられない。`translate()` は実際の
/// テーブルを辿るので、分割を行ったコードとは独立している。こちらで見る。
fn verify_split_and_unmap<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut frame_allocator::FrameAllocator<CAP>,
) {
    use kernel::paging::active::{ActivePageTable, MapUpdateError, PageSize};
    use kernel::paging::entry;

    const FRAMES_PER_2M: u64 = entry::PAGE_SIZE_2M / frame_allocator::FRAME_SIZE;

    /// 検証用に恒久的に予約する領域の大きさ。
    ///
    /// **2MiB なのは、分割の対象として 2MiB ページがちょうど 1 枚必要
    /// だからである。** それ以上でも以下でもない。境界も 2MiB に揃える
    /// 必要がある（`plan::resolve_pages` が 2MiB ページを作るのは 2MiB
    /// 境界に揃った範囲だけなので、揃っていないと 4KiB に分解されていて
    /// 分割対象にならない）。
    ///
    /// 確保したまま解放しないので、起動のたびにこの分だけ空きが減る。
    /// 実測 205MiB に対して約 1 パーセントで、現状は無視できる。
    /// メモリ需要が増えたときの見直しは `deferred-decisions.md` に挙げてある。
    const SCRATCH_BYTES: u64 = entry::PAGE_SIZE_2M;

    let Some(start_frame) = allocator
        .allocate_contiguous_aligned(SCRATCH_BYTES / frame_allocator::FRAME_SIZE, FRAMES_PER_2M)
    else {
        logger.error(format_args!(
            "split-test: could not reserve a 2MiB-aligned scratch region; halting"
        ));
        cpu::halt_forever();
    };
    let base = start_frame * frame_allocator::FRAME_SIZE;
    logger.info(format_args!(
        "split-test: reserved {base:#x}..{:#x} as scratch ({} KiB permanently withheld from the \
         allocator)",
        base + SCRATCH_BYTES,
        SCRATCH_BYTES / 1024
    ));

    // SAFETY: CR3 は自前のテーブルへ切り替え済みで、テーブル自体は恒等
    // マッピングで読み書きできる。
    let mut table = unsafe { ActivePageTable::current() };

    // --- 分割前の状態を記録する ---
    let probes = [
        base,
        base + entry::PAGE_SIZE_2M / 2,
        base + entry::PAGE_SIZE_2M - 1,
    ];
    let mut before = [0u64; 3];
    for (slot, probe) in probes.iter().enumerate() {
        match table.translate(*probe) {
            Ok(Some(translation)) if translation.page_size == PageSize::Size2MiB => {
                before[slot] = translation.phys;
            }
            other => {
                logger.error(format_args!(
                    "split-test: {probe:#x} is not mapped by a 2MiB page ({other:?}); halting"
                ));
                cpu::halt_forever();
            }
        }
    }
    let huge_flags = match table.translate(base) {
        Ok(Some(translation)) => translation.entry,
        _ => unreachable!("直前に 2MiB として翻訳できている"),
    };

    // --- 分割する ---
    // SAFETY: `allocator` の空き範囲はすべて恒等マッピング済みであることを
    // 起動時に検証している。テーブルは CR3 に載っているものである。
    let outcome = match unsafe { table.split_huge_page(base, allocator) } {
        Ok(outcome) => outcome,
        Err(error) => {
            logger.error(format_args!("split-test: split failed: {error:?}; halting"));
            cpu::halt_forever();
        }
    };
    logger.info(format_args!(
        "split-test: split {:#x} into 512 x 4KiB via a new page table at {:#x}",
        outcome.base_virt, outcome.table_phys
    ));

    // --- 512 エントリを読み戻して照合する（translate 経由の独立した経路）---
    let mut mismatches = 0u32;
    for index in 0..entry::ENTRIES_PER_TABLE {
        let virt = base + index as u64 * entry::PAGE_SIZE_4K;
        match table.translate(virt) {
            Ok(Some(translation)) => {
                if translation.page_size != PageSize::Size4KiB {
                    mismatches += 1;
                } else if translation.phys != virt {
                    // 恒等マッピングなので物理 == 仮想。
                    mismatches += 1;
                } else {
                    // 属性が分割前と一致すること。PS は 4KiB では PAT の意味に
                    // なるため、ここでは Present / Writable / PCD / PWT を見る。
                    const KEPT: u64 =
                        entry::PTE_PRESENT | entry::PTE_WRITABLE | entry::PTE_PCD | entry::PTE_PWT;
                    if translation.entry & KEPT != huge_flags & KEPT {
                        mismatches += 1;
                    }
                }
            }
            _ => mismatches += 1,
        }
    }
    logger.info(format_args!(
        "split-test: read back 512 entries through translate(), mismatches={mismatches}"
    ));

    // --- 粒度だけが変わったこと ---
    for (slot, probe) in probes.iter().enumerate() {
        match table.translate(*probe) {
            Ok(Some(t)) if t.phys == before[slot] && t.page_size == PageSize::Size4KiB => {}
            other => {
                mismatches += 1;
                logger.error(format_args!(
                    "split-test: {probe:#x} changed more than its granularity: {other:?}"
                ));
            }
        }
    }

    // --- 分割した領域を読み書きできること ---
    let mut io_ok = true;
    for probe in [
        base,
        base + entry::PAGE_SIZE_2M / 2,
        base + entry::PAGE_SIZE_2M - 8,
    ] {
        // SAFETY: 直前に translate() で 4KiB としてマップ済みと確認した、
        // アロケータから確保した誰も使っていない領域である。8 バイトだけ触る。
        let read_back = unsafe {
            core::ptr::write_volatile(probe as *mut u64, 0xA5A5_5A5A_A5A5_5A5A);
            core::ptr::read_volatile(probe as *const u64)
        };
        io_ok &= read_back == 0xA5A5_5A5A_A5A5_5A5A;
    }
    logger.info(format_args!(
        "split-test: the split region is readable and writable = {}",
        if io_ok { "OK" } else { "NG" }
    ));

    // --- アンマップする ---
    let target = base + entry::PAGE_SIZE_4K; // 先頭ではなく 2 本目を消す
                                             // SAFETY: 上記と同じ領域で、以後この 4KiB へはアクセスしない。
    let old_pte = match unsafe { table.unmap_4kib(target) } {
        Ok(pte) => pte,
        Err(error) => {
            logger.error(format_args!("split-test: unmap failed: {error:?}; halting"));
            cpu::halt_forever();
        }
    };
    let unmapped_ok = matches!(table.translate(target), Ok(None));
    // **隣が生きていること。** これを見ないと、添字を間違えて領域全体を
    // 消していても気づけない。
    let neighbours_ok = matches!(
        table.translate(target - entry::PAGE_SIZE_4K),
        Ok(Some(t)) if t.page_size == PageSize::Size4KiB
    ) && matches!(
        table.translate(target + entry::PAGE_SIZE_4K),
        Ok(Some(t)) if t.page_size == PageSize::Size4KiB
    );
    logger.info(format_args!(
        "split-test: unmapped {target:#x} (old pte={old_pte:#x}); translate returns none={unmapped_ok}, \
         both neighbours still mapped={neighbours_ok}"
    ));

    // --- API が誤用を弾くこと ---
    // SAFETY: 状態を変えない呼び出し。いずれもエラーで戻ることを期待する。
    let already_small = unsafe { table.split_huge_page(base, allocator) };
    let rejects_double_split = already_small == Err(MapUpdateError::AlreadySmall);
    logger.info(format_args!(
        "split-test: splitting an already-split page is rejected = {rejects_double_split}"
    ));

    if mismatches > 0 || !io_ok || !unmapped_ok || !neighbours_ok || !rejects_double_split {
        logger.error(format_args!(
            "split-test: the split/unmap round trip did not behave as planned; halting"
        ));
        cpu::halt_forever();
    }
    logger.info(format_args!(
        "split-test: split and unmap behave as planned (verified through translate())"
    ));
}

/// 意図的に壊した経路を有効にする feature の一覧。
///
/// **どれか 1 つでも有効なら、そのビルドの観測結果を正常な結果として
/// 扱ってはならない。** 名前と「何を壊すか」を対にして並べる。
///
/// 仕込みが `paging::active` や `paging::entry` のようなレビュー必須の
/// ファイルにも住むようになったため、実行時に一覧を出す。7 種類まで増えると、
/// どれが有効か分からないまま実行する余地が生まれる。
const TEST_HOOKS: &[(&str, bool, &str)] = &[
    (
        "misalign-test",
        cfg!(feature = "misalign-test"),
        "IRQ スタブのスタック 16 バイト調整を外す",
    ),
    (
        "no-eoi-test",
        cfg!(feature = "no-eoi-test"),
        "タイマハンドラの EOI 発行を落とす",
    ),
    (
        "alt-offset-test",
        cfg!(feature = "alt-offset-test"),
        "PIC を 0x30-0x3F へ再マップする",
    ),
    (
        "tiny-key-buffer",
        cfg!(feature = "tiny-key-buffer"),
        "キーバッファを極小にする",
    ),
    (
        "paging-test",
        cfg!(feature = "paging-test"),
        "ページテーブルの追加検証を走らせる（それ自体は壊さない）",
    ),
    (
        "paging-test-drop-pcd",
        cfg!(feature = "paging-test-drop-pcd"),
        "分割時に PCD を落とす",
    ),
    (
        "paging-test-wrong-order",
        cfg!(feature = "paging-test-wrong-order"),
        "分割の順序を逆にし、中間状態を意図的に踏む",
    ),
    (
        "paging-test-bad-index",
        cfg!(feature = "paging-test-bad-index"),
        "アンマップの添字を間違える",
    ),
    (
        "paging-test-no-invlpg",
        cfg!(feature = "paging-test-no-invlpg"),
        "アンマップ後の invlpg を落とす",
    ),
    (
        "paging-test-unmap-fault",
        cfg!(feature = "paging-test-unmap-fault"),
        "アンマップしたページを読む",
    ),
    (
        "paging-test-split-heap",
        cfg!(feature = "paging-test-split-heap"),
        "稼働中のヒープが載るページを分割する",
    ),
    (
        "exception-test",
        cfg!(feature = "exception-test"),
        "起動完了後に意図的な例外を起こす",
    ),
    (
        "critical-test",
        cfg!(feature = "critical-test"),
        "クリティカルセクションの回帰チェックを走らせる",
    ),
    (
        "interrupt-test",
        cfg!(feature = "interrupt-test"),
        "割り込み経路の回帰チェックを走らせる",
    ),
    (
        "gfx-test-pattern",
        cfg!(feature = "gfx-test-pattern"),
        "描画テストパターンを描き、コンソールを起動しない",
    ),
];

/// 有効な仕込み feature を起動時に報告する。
///
/// **1 つでも有効なら WARN を出す。** 仕込みが有効なビルドで測った結果を
/// 正常な結果として報告する事故を防ぐためのものである。何も有効でない
/// 場合も 1 行出す。「出ていない」と「そもそも報告していない」を
/// 区別できるようにするため。
fn report_test_hooks(logger: &mut Logger<SerialPort>) {
    let enabled: usize = TEST_HOOKS.iter().filter(|(_, on, _)| *on).count();
    if enabled == 0 {
        logger.info(format_args!(
            "test hooks: none enabled (this is a normal build)"
        ));
        return;
    }
    logger.warn(format_args!(
        "test hooks: {enabled} deliberately-modified feature(s) are ENABLED; \
         do not treat this run as a normal result"
    ));
    for (name, _, effect) in TEST_HOOKS.iter().filter(|(_, on, _)| *on) {
        logger.warn(format_args!("test hooks:   {name} - {effect}"));
    }
}

/// ページテーブル操作の回帰チェック（M5-a-2-2、`paging-test` feature）。
///
/// **通常起動には入らない。** ここで行うのは、意図的にフォルトを起こす、
/// 意図的に壊した状態を作る、稼働中の領域を触る、といった操作である。
/// 通常の起動シーケンスに混ぜると、起動時の他の異常と区別しにくくなる。
///
/// 判定はシリアルのマーカー行で行い、xtask が突き合わせる。
#[cfg(feature = "paging-test")]
fn run_paging_test<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut frame_allocator::FrameAllocator<CAP>,
    heap_start: u64,
) {
    use kernel::paging::active::{ActivePageTable, PageSize};
    use kernel::paging::entry;

    const FRAMES_PER_2M: u64 = entry::PAGE_SIZE_2M / frame_allocator::FRAME_SIZE;

    // SAFETY: CR3 は自前のテーブルへ切り替え済み。
    let mut table = unsafe { ActivePageTable::current() };

    // --- PCD 付きの 2MiB ページを分割する ---
    //
    // 実機で PCD 付きの 2MiB ページはフレームバッファだけだが、そこを
    // 分割対象にすると失敗時に画面が壊れ、観測手段の一部を失う。誰も
    // 使っていないスクラッチ領域に PCD を立ててから分割すれば、同じ性質を
    // 安全に試せる。
    let Some(frame) = allocator.allocate_contiguous_aligned(FRAMES_PER_2M, FRAMES_PER_2M) else {
        logger.error(format_args!("paging-test: no scratch region available"));
        cpu::halt_forever();
    };
    let pcd_base = frame * frame_allocator::FRAME_SIZE;
    // SAFETY: 今確保したばかりの、誰も使っていない領域である。PCD を立てても
    // アクセスがキャッシュされなくなるだけで、内容も配置も変わらない。
    let before = unsafe { table.add_huge_page_flags(pcd_base, entry::PTE_PCD) };
    let pcd_set = matches!(
        table.translate(pcd_base),
        Ok(Some(t)) if t.entry & entry::PTE_PCD != 0 && t.page_size == PageSize::Size2MiB
    );
    logger.info(format_args!(
        "paging-test: scratch {pcd_base:#x} now has PCD as a 2MiB page = {pcd_set} (was {before:?})"
    ));

    // SAFETY: 上記のスクラッチ領域。
    match unsafe { table.split_huge_page(pcd_base, allocator) } {
        Ok(_) => {}
        Err(error) => {
            logger.error(format_args!("paging-test: split failed: {error:?}"));
            cpu::halt_forever();
        }
    }
    let mut pcd_lost = 0u32;
    for index in 0..entry::ENTRIES_PER_TABLE {
        let virt = pcd_base + index as u64 * entry::PAGE_SIZE_4K;
        match table.translate(virt) {
            Ok(Some(t)) if t.entry & entry::PTE_PCD != 0 => {}
            _ => pcd_lost += 1,
        }
    }
    if pcd_lost == 0 {
        logger.info(format_args!(
            "paging-test: PCD survived the split on all 512 entries = OK"
        ));
    } else {
        logger.error(format_args!(
            "paging-test: PCD was lost on {pcd_lost} of 512 entries after the split = NG"
        ));
    }

    // --- 稼働中のヒープが載る 2MiB ページを、使いながら分割する ---
    #[cfg(feature = "paging-test-split-heap")]
    {
        let heap_page = heap_start & !(entry::PAGE_SIZE_2M - 1);
        let phys_before = table.translate(heap_start);
        // ヒープを実際に使ってから分割し、分割後も使えることを見る。
        let mut live: Vec<u64> = (0..64).collect();
        // SAFETY: 稼働中のヒープが載るページだが、分割は物理アドレスも属性も
        // 変えない。手順の途中でも古い 2MiB エントリが有効なままである
        // （`split_huge_page` の説明を参照）。
        let result = unsafe { table.split_huge_page(heap_page, allocator) };
        live.push(0xDEAD);
        let phys_after = table.translate(heap_start);
        let same = match (phys_before, phys_after) {
            (Ok(Some(a)), Ok(Some(b))) => {
                a.phys == b.phys
                    && a.page_size == PageSize::Size2MiB
                    && b.page_size == PageSize::Size4KiB
            }
            _ => false,
        };
        logger.info(format_args!(
            "paging-test: split the live heap page {heap_page:#x}: {result:?}, translation \
             unchanged apart from granularity = {same}, heap still usable = {} ({} items)",
            live.last() == Some(&0xDEAD),
            live.len()
        ));
    }
    #[cfg(not(feature = "paging-test-split-heap"))]
    let _ = heap_start;

    // --- アンマップ後のアクセス ---
    //
    // `paging-test-no-invlpg` では invlpg を落としてある。古い翻訳が TLB に
    // 残っていればフォルトせずに読めてしまう。残らなければ #PF になる。
    // QEMU の TCG が TLB をどう扱うかに依存するため、**どちらになるかは
    // 事前に決めつけない。** 観測した結果をそのまま出す。
    let Some(frame) = allocator.allocate_contiguous_aligned(FRAMES_PER_2M, FRAMES_PER_2M) else {
        logger.error(format_args!("paging-test: no second scratch region"));
        cpu::halt_forever();
    };
    let unmap_base = frame * frame_allocator::FRAME_SIZE;
    // SAFETY: 誰も使っていないスクラッチ領域。
    if let Err(error) = unsafe { table.split_huge_page(unmap_base, allocator) } {
        logger.error(format_args!("paging-test: second split failed: {error:?}"));
        cpu::halt_forever();
    }
    let target = unmap_base + 4 * entry::PAGE_SIZE_4K;

    // **アンマップする前に必ず 1 度触る。** 触っていないページには TLB
    // エントリが存在せず、`invlpg` を落としても「古い翻訳が残る」状態を
    // 作れない。それに気づかずに書いたところ、`paging-test-no-invlpg` でも
    // #PF になり、invlpg の有無が結果に現れなかった。検査に見えて何も
    // 検査していない状態である。
    // SAFETY: 直前に分割した、誰も使っていないスクラッチ領域である。
    unsafe {
        core::ptr::write_volatile(target as *mut u64, 0x1234_5678_9ABC_DEF0);
    }

    // SAFETY: 上記の領域。以後この 4KiB へアクセスするのは、この検証の
    // 目的そのものである。
    let old = unsafe { table.unmap_4kib(target) };
    let target_none = matches!(table.translate(target), Ok(None));
    // 添字を間違えていれば、別のページが消えているはずである。
    let neighbour_none = matches!(table.translate(unmap_base), Ok(None));
    logger.info(format_args!(
        "paging-test: unmap {target:#x} -> {old:?}; target none={target_none}, \
         region head none={neighbour_none}"
    ));

    // アンマップしたページを実際に読むのは、専用のビルドだけである。
    //
    // 正しい実装（invlpg を発行する）では #PF になり、そこで停止する。
    // `paging-test-no-invlpg` では古い翻訳が TLB に残っていればフォルト
    // しない。**この 2 つを対にして初めて「invlpg が効いている」と言える。**
    // 片方だけでは「常にフォルトする経路」と区別できない。
    //
    // QEMU の TCG が TLB をどう扱うかに依存するため、no-invlpg 側が
    // 本当にフォルトしないかは事前に決めつけない。観測した結果をそのまま出す。
    #[cfg(any(feature = "paging-test-unmap-fault", feature = "paging-test-no-invlpg"))]
    {
        logger.info(format_args!(
            "paging-test: about to read the unmapped page {target:#x}"
        ));
        // SAFETY: この読み取りがフォルトするかどうかを観測することが、
        // この検証の目的そのものである。
        let value = unsafe { core::ptr::read_volatile(target as *const u64) };
        logger.info(format_args!(
            "paging-test: the read did NOT fault; value={value:#x} (a stale TLB entry was used)"
        ));
    }

    logger.info(format_args!("paging-test: done"));
}

/// カーネルが実際に使っている領域が、どの粒度でマップされているかを測る。
///
/// M5-a-2 で 2MiB ページを分割するにあたり、**どこが 2MiB ページに載って
/// いるのかを推測で決めない**ために測る。`plan::resolve_pages` は 2MiB 境界に
/// 揃った核だけを 2MiB ページにし、前後の端数を 4KiB へ分解する。どの領域が
/// 核に入り、どれが端数になるかは実際のメモリマップ次第で、コードを読んだ
/// だけでは決まらない。
///
/// 分割対象の選定材料であると同時に、恒等マッピングの現状把握そのものでも
/// ある。ここに挙げた 4 つはいずれもカーネルが動き続けるために必要な領域で、
/// 翻訳できないことがあってはならない。できなければ fail-fast する。
fn report_mapping_granularity(
    logger: &mut Logger<SerialPort>,
    heap_start: u64,
    framebuffer_phys: u64,
) {
    use kernel::paging::active::{ActivePageTable, PageSize};
    use kernel::paging::entry;

    // SAFETY: CR3 は自前のテーブルへ切り替えて読み戻し済みであり、テーブル
    // 自体は恒等マッピングで読める（`verify_page_tables` と同じ前提）。
    let table = unsafe { ActivePageTable::current() };

    // RIP と RSP は**測定時点の実値**を読む。リンカスクリプトのシンボルや
    // スタックの静的配列の番地から計算すると、「そう配置したはず」の値を
    // 見ることになり、実際に実行しているアドレスの確認にならない。
    let probes: [(&str, u64); 4] = [
        ("executing code (RIP)", cpu::read_rip()),
        ("kernel stack (RSP)", cpu::read_rsp()),
        ("heap arena", heap_start),
        ("framebuffer", framebuffer_phys),
    ];

    let mut failures = 0u32;
    for (name, addr) in probes {
        if addr == 0 {
            // フレームバッファが無い構成ではここに来る。存在しないものを
            // 「マップされていない」として数えない。
            logger.info(format_args!("granularity: {name} is absent (address 0)"));
            continue;
        }
        match table.translate(addr) {
            Ok(Some(translation)) => {
                let (size_name, base) = match translation.page_size {
                    PageSize::Size2MiB => ("2MiB", addr & !(entry::PAGE_SIZE_2M - 1)),
                    PageSize::Size4KiB => ("4KiB", addr & !(entry::PAGE_SIZE_4K - 1)),
                };
                logger.info(format_args!(
                    "granularity: {name} {addr:#x} -> phys {:#x}, mapped by a {size_name} page \
                     at {base:#x} (entry={:#x})",
                    translation.phys, translation.entry
                ));
            }
            Ok(None) => {
                failures += 1;
                logger.error(format_args!(
                    "granularity: {name} {addr:#x} has no translation"
                ));
            }
            Err(error) => {
                failures += 1;
                logger.error(format_args!(
                    "granularity: {name} {addr:#x} translate failed: {error:?}"
                ));
            }
        }
    }

    if failures > 0 {
        logger.error(format_args!(
            "granularity: {failures} region(s) in use are not translatable; halting"
        ));
        cpu::halt_forever();
    }
}

/// 稼働中のページテーブルを読み戻し、`plan` の意図と突き合わせる（M5-a-1）。
///
/// M4 で `sgdt` / `sidt` / PIC の IMR に対して行ってきたのと同じことを、
/// ページテーブルに対して行う。これまでページテーブルだけは**書きっぱなしで
/// 読み戻す手段が無かった**。
///
/// あわせて、TLB の全フラッシュ（CR3 リロード）が成立する条件も実測する。
fn verify_page_tables(
    logger: &mut Logger<SerialPort>,
    mapped: &MappedRanges<{ kernel::paging::plan::DEFAULT_CAPACITY }>,
) {
    use kernel::paging::active::{ActivePageTable, PageSize, TranslateError};
    use kernel::paging::entry;

    // SAFETY: CR3 は直前に自前のテーブルへ切り替えて読み戻し済みで、
    // 恒等マッピングによりテーブル自体を読める。
    let table = unsafe { ActivePageTable::current() };
    logger.info(format_args!(
        "paging: walking the live tables from PML4 {:#x}",
        table.pml4_phys()
    ));

    // --- TLB フラッシュの前提を実測する ---
    let precondition = kernel::paging::active::tlb_flush_precondition();
    logger.info(format_args!(
        "paging: CR4 = {:#x}, PGE(bit 7) = {} - a CR3 reload flushes everything only when \
         PGE is off or no entry has the global bit",
        precondition.cr4, precondition.page_global_enabled
    ));

    // --- 各マップ済み範囲の先頭・中間・末尾を翻訳して照合する ---
    let mut checked = 0u32;
    let mut mismatches = 0u32;
    let mut global_entries = 0u32;

    for range in mapped.iter() {
        let probes = [
            range.start,
            range.start + (range.end - range.start) / 2,
            range.end - 1,
        ];
        for probe in probes {
            match table.translate(probe) {
                Ok(Some(translation)) => {
                    checked += 1;
                    // **恒等マッピングなので、物理 == 仮想でなければならない。**
                    if translation.phys != probe {
                        mismatches += 1;
                        logger.error(format_args!(
                            "paging: {probe:#x} translates to {:#x} (identity mapping broken)",
                            translation.phys
                        ));
                    }
                    // G ビットが立っていると CR3 リロードで消えない。
                    if translation.entry & entry::PTE_GLOBAL != 0 {
                        global_entries += 1;
                    }
                    // キャッシュ属性が `plan` の意図と一致すること。
                    let expects_pcd = !range.cacheable;
                    let has_pcd = translation.entry & entry::PTE_PCD != 0;
                    if expects_pcd != has_pcd {
                        mismatches += 1;
                        logger.error(format_args!(
                            "paging: {probe:#x} PCD={has_pcd} but the plan wanted {expects_pcd}"
                        ));
                    }
                    let _ = translation.page_size;
                }
                Ok(None) => {
                    mismatches += 1;
                    logger.error(format_args!(
                        "paging: {probe:#x} is in a mapped range but has no translation"
                    ));
                }
                Err(error) => {
                    mismatches += 1;
                    logger.error(format_args!(
                        "paging: {probe:#x} translate failed: {error:?}"
                    ));
                }
            }
        }
    }

    logger.info(format_args!(
        "paging: walked {checked} probe(s) across {} range(s), mismatches={mismatches}, \
         entries with the global bit={global_entries}",
        mapped.range_count()
    ));

    // --- 翻訳の粒度を 1 つ実測して出す（分割の前後で変わることの基準）---
    if let Ok(Some(translation)) = table.translate(0x10_0000) {
        logger.info(format_args!(
            "paging: 0x100000 is mapped by a {} page (entry={:#x})",
            match translation.page_size {
                PageSize::Size2MiB => "2MiB",
                PageSize::Size4KiB => "4KiB",
            },
            translation.entry
        ));
    }

    // --- 「マップされていない」と「アドレスが不正」を区別できること ---
    // 非正規アドレス。CPU が受け付けない形なので、None ではなくエラー。
    let non_canonical = table.translate(0x0000_8000_0000_0000);
    let non_canonical_ok = non_canonical == Err(TranslateError::NonCanonicalAddress);
    logger.info(format_args!(
        "paging: a non-canonical address is rejected as an error, not as \"unmapped\" = {}",
        if non_canonical_ok { "OK" } else { "NG" }
    ));

    // G ビットが 1 つでも立っていたら停止する。
    //
    // 「G ビットを一切立てていない」ことは、architecture.md と ADR-0018 が
    // **維持していると主張している性質**であり、M5-a-1 が「CR3 リロードで
    // TLB を全部追い出せる」と結論した根拠でもある。M5-a-2 の 2MiB ページ
    // 分割は、その結論の上に手順を組んでいる。
    //
    // 数えて WARN を出すだけでは、主張の強さと検査の強さが釣り合わない。
    // 立っていたら前提が崩れているということなので、そこで止める方が正しい。
    // `plan` には G ビットを立てる経路が無いため、通常はここに掛からない。
    if mismatches > 0 || !non_canonical_ok || global_entries > 0 {
        if global_entries > 0 {
            logger.error(format_args!(
                "paging: {global_entries} entry(ies) have the global bit; a CR3 reload would NOT \
                 evict them from the TLB, which breaks the assumption M5-a relies on"
            ));
        }
        logger.error(format_args!(
            "paging: the live tables do not match the plan; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "paging: the live tables match the plan (read back from the tables themselves)"
    ));
}
