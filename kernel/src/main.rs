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
use kernel::console::Console;
use core::ptr::addr_of;

use kernel::frame_allocator;
use kernel::gdt;
use kernel::stack;
use kernel::graphics::{Color, Framebuffer, FramebufferLayout};
use kernel::heap;
use kernel::idt;
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
#[cfg_attr(feature = "exception-test", allow(unreachable_code))]
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

    log_both(
        &mut logger,
        console.as_mut(),
        LogLevel::Info,
        format_args!("kernel: halting"),
    );
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
        logger.info(format_args!(
            "console: serial logging continues unaffected"
        ));
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
        logger.info(format_args!(
            "console: serial logging continues unaffected"
        ));
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
            logger.info(format_args!(
                "console: serial logging continues unaffected"
            ));
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
    // SAFETY: scratch は現在の RSP より 256 バイト下で、カーネルスタックの
    // 範囲内。まだ誰も使っていない未使用領域であり、赤ゾーン（128 バイト）
    // より外側でもある。読み書きするのはこの 8 バイトのみ。
    let scratch_ok = if kernel_stack.contains(scratch as u64) {
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
    logger.info(format_args!(
        "critical: IF before any guard = {before}"
    ));

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
