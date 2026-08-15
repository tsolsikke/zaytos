//! ZaytOS kernel の起動シーケンス。
//!
//! bootloader から [`BootInfo`] を受け取り、GDT/IDT・ページテーブル・
//! ヒープ・割り込みを順に立ち上げてタイマループへ入る。

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
use kernel::irq;
use kernel::keyboard;
use kernel::paging;
use kernel::paging::plan::{resolve_pages, MappedRanges};
use kernel::paging::table::PageTableBuilder;
use kernel::stack;

mod panic;

use kernel::heap::ALLOCATOR;

// `kernel/link.ld` が定義するシンボル。kernel イメージ自身の占有範囲を
// 実行時に把握するために使う（M2-d の必須マッピング検証）。
extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

// === higher-half のトランポリンと静的初期ページテーブルとブートスタック ===
//
// トランポリンは RIP 相対のみで書く。絶対アドレスはコードに現れず、隣接する
// `.quad` にだけ入る。低位で実行している間、RIP 相対の結果は物理アドレスになる。
// 3 つの `.quad` は CR3 に載せる PML4 の物理、ブートスタック頂点の高位 VA、
// `_start` の高位 VA を保持する。
//
// mov cr3 の直後は、次の命令フェッチが新テーブルで RIP はまだ低位なので、
// 静的テーブルの PML4[0] 恒等（1GiB）がトランポリンの低位 VA を覆う。
// 切り替えた後に高位 `_start` へ jmp し、以降を高位で実行する。
// === higher-half の破壊 feature ===
//
// 静的初期テーブルとトランポリンはリンク時計算の global_asm なので、破壊は
// const オペランド（(a)(b)）と、同一 asm へ 1 行挿入するマクロ（(d)）で行う。
// 検査対象の asm を複製せずに済む。

/// (a) highhalf-no-identity-in-boot-pt: 静的 PML4[0] の存在ビットを落とす。
/// 既定 0x03（P|RW）/ feature 0x02（P クリア）。恒等 PML4[0] が not-present に
/// なり、mov cr3 直後の低位命令フェッチ（次命令）が解決できずトリプルフォルトする。
const BOOT_PML4_0_FLAGS: u64 = if cfg!(feature = "highhalf-no-identity-in-boot-pt") {
    0x02
} else {
    0x03
};

/// (b) highhalf-bad-high-slot: PDPT_high のエントリを 510→509 へずらす。前後の
/// fill 数（合計 512 を保つ）を切り替える。PML4[511]→PDPT_high[510] が空になり、
/// 高位 _start への jmp 先が未マップでトリプルフォルトする。
const BOOT_PDPT_HIGH_BEFORE: u64 = if cfg!(feature = "highhalf-bad-high-slot") {
    509
} else {
    510
};
const BOOT_PDPT_HIGH_AFTER: u64 = if cfg!(feature = "highhalf-bad-high-slot") {
    2
} else {
    1
};

/// (d) highhalf-trampoline-absolute-ref: トランポリンに絶対メモリ参照命令を 1 行
/// 挿入する。**実行時ではなく静的なバイト単位一致検査で捕まえる。** 既定は空文字列
/// で、トランポリンのバイト列は不変。feature 時のみ `mov rbx, [0x2000]`（disp32 の
/// 絶対参照）が入り、コード先頭 24 バイトが期待リテラルと食い違う。同一 asm 内の
/// 1 行なので、トランポリン本体を変えたときに sabotage 側が置き去りにならない。
#[cfg(feature = "highhalf-trampoline-absolute-ref")]
macro_rules! tramp_sabotage {
    () => {
        "  mov rbx, [0x2000]\n"
    };
}
#[cfg(not(feature = "highhalf-trampoline-absolute-ref"))]
macro_rules! tramp_sabotage {
    () => {
        ""
    };
}

core::arch::global_asm!(
    ".section .text.trampoline,\"ax\",@progbits",
    ".p2align 12",
    ".globl zaytos_trampoline",
    "zaytos_trampoline:",
    "  mov rax, [rip + zaytos_tramp_pml4]",   // 48 8B 05 <rel32>: PML4 物理をロード
    "  mov cr3, rax",                          // 0F 22 D8: 切り替え
    tramp_sabotage!(),                         // (d) 既定は空。feature 時のみ絶対参照を 1 行挿入
    "  mov rsp, [rip + zaytos_tramp_stack]",   // 48 8B 25 <rel32>: 高位ブートスタック頂点を RSP へ
    "  jmp [rip + zaytos_tramp_entry]",        // FF 25 <rel32>: 高位 _start へ間接 jmp
    ".p2align 3",
    "zaytos_tramp_pml4:  .quad zaytos_boot_pml4 - {kvb}",   // PML4 の LMA（= 物理）
    "zaytos_tramp_stack: .quad zaytos_boot_stack_top",       // 高位 VA
    "zaytos_tramp_entry: .quad _start",                      // 高位 VA
    kvb = const kernel::link_symbols::KERNEL_VIRT_BASE,
);

// 静的初期ページテーブル。リンク時計算で全エントリを確定する（フレーム
// アロケータを使わない）。2MiB ページのみ、4 フレーム = 16KiB。
//   PML4[0]    -> PDPT_low     恒等（松葉杖。切り替えの瞬間と M2-d 切り替えまで）
//   PML4[511]  -> PDPT_high    高位カーネル（0xFFFFFFFF80000000 起点）
//   PDPT_low[0]   -> PD_shared
//   PDPT_high[510]-> PD_shared （**同一 PD を共有**）
//   PD_shared[i]  = (i << 21) | 0x83   物理 i*2MiB、P|RW|PS
// PD エントリは「物理ターゲット + フラグ」だけを符号化するので、恒等窓と高位窓が
// 1 枚の PD を共有できる。両窓ともフラグが同一（cacheable RW、NX なし）で成立する。
// フレームバッファ（物理 2GiB）は [0,1GiB) の外なので、キャッシュ属性の別名は
// 生じない（ADR-0021）。
// 高位窓が [0,1GiB) を覆うのはイメージに要る範囲を超えるが、この表を使うのは
// 本流テーブルへの切り替えまでで、その間に触れる高位 VA はすべてイメージ内である。
core::arch::global_asm!(
    ".section .data.bootpt,\"aw\",@progbits",
    ".p2align 12",
    ".globl zaytos_boot_pml4",
    "zaytos_boot_pml4:",
    "  .quad zaytos_boot_pdpt_low - {kvb} + {pml4_0}",   // (a) 既定 0x03 / feature 0x02（P クリア）
    "  .fill 510, 8, 0",
    "  .quad zaytos_boot_pdpt_high - {kvb} + 0x03",
    ".p2align 12",
    "zaytos_boot_pdpt_low:",
    "  .quad zaytos_boot_pd_shared - {kvb} + 0x03",
    "  .fill 511, 8, 0",
    ".p2align 12",
    "zaytos_boot_pdpt_high:",
    "  .fill {hi_before}, 8, 0",                          // (b) 既定 510 / feature 509
    "  .quad zaytos_boot_pd_shared - {kvb} + 0x03",
    "  .fill {hi_after}, 8, 0",                           // (b) 既定 1 / feature 2（合計 512 を保つ）
    ".p2align 12",
    "zaytos_boot_pd_shared:",
    "  .set idx, 0",
    "  .rept 512",
    "    .quad (idx << 21) | 0x83",
    "    .set idx, idx + 1",
    "  .endr",
    kvb = const kernel::link_symbols::KERNEL_VIRT_BASE,
    pml4_0 = const BOOT_PML4_0_FLAGS,
    hi_before = const BOOT_PDPT_HIGH_BEFORE,
    hi_after = const BOOT_PDPT_HIGH_AFTER,
);

/// ブートスタックの大きさ。トランポリンが CR3 切り替え後に RSP をここへ移す。
///
/// `_start` の序盤だけで使う浅いスタックで、深さは [`report_boot_stack_usage`]
/// が実測する。実測は 632 バイトだが 16KiB のまま据え置いている（余剰は 4 フレーム）。
const BOOT_STACK_SIZE: usize = 16 * 1024;

/// ブートスタックの毒値。既存のカーネルスタックのカナリア（`stack::CANARY_BYTE`）
/// と同じ 0xC5 にして、ログに出たときに同種の会計だと分かるようにする。
const BOOT_STACK_POISON: u8 = 0xC5;

// 0xC5 で初期化する（.bss ではなく初期化データ）。未使用部分を数えて最大深さを
// 実測するためで、ガードページが無いのでこの会計が唯一の観測手段である。
core::arch::global_asm!(
    ".section .data.bootstack,\"aw\",@progbits",
    ".p2align 12",
    ".globl zaytos_boot_stack",
    "zaytos_boot_stack:",
    "  .fill 16384, 1, 0xC5",
    ".globl zaytos_boot_stack_top",
    "zaytos_boot_stack_top:",
);

// `zaytos_trampoline` は Rust から触らないので extern 宣言を置かない
// （静的検査は nm/objdump で拾う）。下の 2 つは Rust から読むので宣言する。
extern "C" {
    /// ブートスタックの下端（低位側）。深さ実測の走査起点。
    static zaytos_boot_stack: u8;
    /// 静的初期 PML4 の先頭。CR3 との突き合わせに使う。
    static zaytos_boot_pml4: u8;
}

/// ブートスタックの最大深さを実測してログに出す。
///
/// 下端から連続する毒値（未使用バイト）を数えて使用量を出す。
/// ガードページを持たないブートスタックの、オーバーフロー検出を兼ねた会計である。
fn report_boot_stack_usage(logger: &mut Logger<SerialPort>) {
    let base = addr_of!(zaytos_boot_stack);
    let mut unused = 0usize;
    while unused < BOOT_STACK_SIZE {
        // SAFETY: base..base+BOOT_STACK_SIZE は静的なブートスタックの範囲内。
        // 読み取りのみ。
        let byte = unsafe { core::ptr::read_volatile(base.add(unused)) };
        if byte != BOOT_STACK_POISON {
            break;
        }
        unused += 1;
    }
    let used = BOOT_STACK_SIZE - unused;
    logger.info(format_args!(
        "boot stack: {used} of {BOOT_STACK_SIZE} bytes used (high-water; {unused} bytes poison intact)"
    ));
}

/// 高位到達の実証。`kernel_main` が高位 VA で走っていることを読み戻しで確かめる。
///
/// 到達できていること自体が証拠だが、RIP/RSP/CR3 と恒等の生存を数字で残す。
/// 本流テーブルへの CR3 切り替えより前に呼ぶこと（CR3 が静的初期 PML4 を指す間）。
fn report_high_half_arrival(logger: &mut Logger<SerialPort>) {
    use common::addr::VirtAddr;

    let rip = cpu::read_rip();
    let rsp = cpu::read_rsp();
    let cr3 = paging::switch::read_cr3();

    // 期待する高位範囲（イメージの VMA）。
    let image_lo = kernel::link_symbols::KERNEL_VIRT_BASE + kernel::link_symbols::KERNEL_LOAD_ADDR;
    let image_hi = addr_of!(__kernel_end) as u64;
    let rip_high = rip >= image_lo && rip < image_hi;
    let rsp_high = rsp >= kernel::link_symbols::KERNEL_VIRT_BASE;

    // CR3 は静的初期 PML4 の物理を指しているはず（まだ M2-d へ切り替える前）。
    let boot_pml4_phys = kernel::kernel_phys_from_virt(
        VirtAddr::new(addr_of!(zaytos_boot_pml4) as u64)
            .expect("the boot PML4 symbol is canonical"),
    );
    let cr3_is_bootstrap = cr3 == boot_pml4_phys;

    // 恒等がまだ生きていること（別名の直接証明）: 同じ物理を低位 VA（恒等）と
    // 高位 VA（カーネル高位）の両方から読んで一致するか。
    let (image_start, _) = kernel_image_phys_range();
    let phys = image_start.as_u64();
    let high_va = kernel::kernel_virt_from_phys(image_start).as_u64();
    // SAFETY: phys（低位、恒等）と high_va（高位）はともに静的初期テーブルで
    // present（PML4[0] と PML4[511] が同じ PD を共有）。読み取りのみ。
    let (via_low, via_high) = unsafe {
        (
            core::ptr::read_volatile(phys as *const u8),
            core::ptr::read_volatile(high_va as *const u8),
        )
    };
    let identity_alive = via_low == via_high;

    logger.info(format_args!(
        "higher-half: arrived at high VA. RIP={rip:#x} in [{image_lo:#x}, {image_hi:#x})={rip_high}, \
         RSP={rsp:#x} high={rsp_high}, CR3={:#x} == bootstrap PML4 {:#x} = {cr3_is_bootstrap}, \
         identity alive (phys {phys:#x} read via low==high) = {identity_alive}",
        cr3.as_u64(),
        boot_pml4_phys.as_u64()
    ));
    if !(rip_high && rsp_high && cr3_is_bootstrap && identity_alive) {
        logger.error(format_args!(
            "higher-half: high-arrival invariants failed; halting"
        ));
        cpu::halt_forever();
    }
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
    // ここはまだ UEFI 由来のスタックの上である。最小限だけ行い、自前のスタックへ
    // 移ってから本番の起動シーケンスに入る。旧スタックの上に状態を積むほど、
    // 切り替えた後に「参照してはいけない領域」が増える。
    // 引き継ぐ値は BOOT_HANDOFF（静的領域）へ置く。スタック上だと切り替え後に
    // 旧スタックを指す。

    // 自前の IDT を入れるまで割り込みを禁止する（ADR-0014）。UEFI が設定した
    // IDT がまだ生きており、割り込みが起きれば検証していないハンドラへ制御が渡る。
    //
    // InterruptGuard ではなく直接 cli する。sti するまで恒久的に禁止したいので、
    // スコープを抜けたら復元するクリティカルセクションとは意味が違う。
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
    // SAFETY: 直前に cli 済み。起動時に 1 回だけ呼ぶ。ダブルフォルト用と
    // ページフォルト用の IST スタックは通常のカーネルスタックとも互いとも
    // 別の静的領域である。
    unsafe {
        gdt::init(
            stack::double_fault_stack_range().top.as_u64(),
            stack::page_fault_stack_range().top.as_u64(),
        );
    }

    // IDT をロードする。ここも .bss の静的領域だけで完結する。
    // 例外は RFLAGS.IF に関係なく発生するので、sti する前でもハンドラは働く。
    //
    // 破壊 (stack-overflow-df-test): #PF に IST を与えない。ガードページに触れた
    // #PF が壊れたスタックの上で動こうとし、そこでさらに #PF が起きて #DF へ
    // 昇格する経路を、本来のスタックオーバーフローで出すためである。
    #[cfg(feature = "stack-overflow-df-test")]
    let page_fault_ist = None;
    #[cfg(not(feature = "stack-overflow-df-test"))]
    let page_fault_ist = Some(gdt::PAGE_FAULT_IST_INDEX as u8);

    // SAFETY: 直前に cli 済みで、GDT も直前にロードした。起動時に 1 回だけ
    // 呼ぶ。ダブルフォルト用と #PF 用の IST は gdt::init が TSS へ設定済み。
    unsafe {
        idt::init(Some(gdt::DOUBLE_FAULT_IST_INDEX as u8), page_fault_ist);
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
        feature = "interrupt-test",
        feature = "stack-guard-test"
    ),
    allow(unreachable_code)
)]
extern "sysv64" fn kernel_main() -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let mut logger = Logger::new(serial, LogLevel::Trace);

    logger.info(format_args!("ZaytOS kernel: entered _start"));

    // **どの像が走ったかを、起動ログの先頭で言う（S12 前の手当て）。**
    //
    // **破壊 feature の項目が落ちたとき、判定行だけでは 2 つを分けられない**
    // ——「破壊が効かなかった」のか「破壊の無い像が走った」のか
    // （`docs/verification-coverage.md` の該当節）。この行があれば分かれる。
    //
    // **一覧は `build.rs` が `CARGO_FEATURE_*` から生成する。**
    // **手で並べないので、feature を足したときの書き足し忘れが起きない。**
    if kernel::enabled_features::ENABLED_FEATURES.is_empty() {
        logger.info(format_args!("build: no cargo feature is enabled"));
    } else {
        logger.info(format_args!(
            "build: {} cargo feature(s) enabled: {}",
            kernel::enabled_features::ENABLED_FEATURES.len(),
            FeatureList(kernel::enabled_features::ENABLED_FEATURES)
        ));
    }

    // ブートスタックの深さ会計（B-2a）。B-2a-3 で実際の深さが出る。
    report_boot_stack_usage(&mut logger);

    // 高位到達の実証（B-2a-3）。M2-d の CR3 切り替えより前に呼ぶ（CR3 が
    // 静的初期 PML4 を指している間に確かめる）。到達できなければここより前で
    // トリプルフォルトしている。
    report_high_half_arrival(&mut logger);

    // === cpu_id() を GDTR 由来へ差し替える ===
    //
    // gdt::init（`lgdt`）より後でなければならない（`gdt::cpu_id_from_gdtr` の doc）。
    // ここまで遅らせても正しい——それより前の `cpu_id()` は定数 0 を返し、
    // その間走っているのは bootstrap processor だけである。
    //
    // AP では窓が再び開く。フォールバックの 0 は AP では別コアのスロットを指す
    // ので誤りである（`docs/roadmap.md`）。
    // SAFETY: `gdt::init` は既に戻っており、自コアの GDT はロード済みである。
    unsafe {
        gdt::install_cpu_id_from_gdtr(&mut logger);
    }

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

    // **未解決の恒等前提。** handoff.boot_info は UEFI が低位に置いた物理ポインタで、
    // 恒等マッピングの間だけ低位 VA として参照できる。B-2b で恒等を外す前に
    // direct map 高位窓経由へ移す（恒等前提の網羅列挙は
    // docs/verification-coverage.md の「higher-half B-2b」を参照）。
    // SAFETY: 呼び出し元契約（`_start` の # Safety）により、boot_info は
    // 有効な BootInfo を指す。ここでは読み取り専用の参照を作るのみ。
    let boot_info = unsafe { &*handoff.boot_info };

    // direct physical map を登録する（T-2a）。
    //
    // **恒等マッピングの間は、窓が物理空間全体を覆う。** 覆う長さを
    // `classify()` の返す最大物理アドレスから決めるのは higher-half 移行の
    // 時点である。ここでそれを求めようとしても、メモリマップを読むために
    // まず `descriptors_ptr` を変換する必要があり、順序が循環する。
    //
    // 長さを定数として型やコードに埋め込んではいない。窓は値として持ち回り、
    // 移行時に新しい base と実測した長さで作り直す（`replace_direct_map`）。
    let direct_map =
        common::addr::DirectMap::identity(common::addr::DirectMap::IDENTITY_MAX_LENGTH)
            .expect("an identity window over the representable physical space is canonical");
    if common::addr::init_direct_map(direct_map).is_err() {
        logger.error(format_args!("addr: the direct map was already initialised"));
        cpu::halt_forever();
    }

    if let Err(e) = boot_info.validate() {
        logger.error(format_args!("BootInfo validation failed: {e}"));
        logger.error(format_args!(
            "bootloader と kernel のビルドが食い違っている可能性があります"
        ));
        cpu::halt_forever();
    }
    logger.info(format_args!("BootInfo validated (magic/version OK)"));

    // S1-a: bootloader が引いた RSDP の物理アドレス。**この時点では検証も走査も
    // していない。** 署名・チェックサム・revision の検査と辿る先の決定は、
    // 起動シーケンスの後方（direct map 窓の高位化より後）で `acpi::survey` が行う。
    // ここへ持ってこられないのは、物理を読むのに窓と稼働中のページテーブルが要るためである。
    if boot_info.acpi_rsdp.as_u64() == 0 {
        logger.error(format_args!(
            "acpi: the bootloader reported no RSDP; S2 (APIC) will need it"
        ));
    } else {
        logger.info(format_args!(
            "acpi: RSDP physical address from the bootloader = {:#x} (validated later in this \
             boot; see the acpi: lines below)",
            boot_info.acpi_rsdp.as_u64()
        ));
    }

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
        boot_info.framebuffer.physical_address.as_u64(),
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
            direct_map
                .phys_to_virt(boot_info.memory_map.descriptors_ptr)
                .as_ptr::<u8>(),
            boot_info.memory_map.descriptors_len as usize,
        )
    };

    // ACPI の走査（S1-b）が使う値をここで抜き出しておく。**boot_info の最終利用は
    // A-2 の rehome であり、それより後ろで boot_info を参照してはならない**（低位 VA で、
    // 恒等除去後は無効になる）。走査は除去より前だが、rehome より後ろに置くため、
    // 抽出済みの値だけで完結させる。`raw_map` も同じ理由で既に抽出済みである。
    let acpi_rsdp = boot_info.acpi_rsdp;
    let memory_map_descriptor_size = boot_info.memory_map.descriptor_size;

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
                frame.as_u64()
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

    // === S1-d: AP トランポリン用フレームの予約 ===
    //
    // **ここでしか取れない。** `allocate_frame` は最小のフレーム番号から配るので、
    // この直後に始まるページテーブル構築が低位から食っていく。SIPI のベクタは
    // 8 ビットで、AP は `vector << 12` から走り始めるため、トランポリンは物理
    // 1MiB 未満に要る（`smp::TRAMPOLINE_MAX_START` の doc を参照）。
    //
    // **失敗しても停止しない。** S1 は情報を集める段で、AP はまだ起こさない。
    // 致命として扱うのは S3（AP 起こし）である。
    match kernel::smp::reserve_trampoline_frame(&mut allocator) {
        Ok(frame) => logger.info(format_args!(
            "smp: reserved the AP trampoline frame at {:#x} (below {:#x}, SIPI-addressable={})",
            frame.as_u64(),
            kernel::smp::TRAMPOLINE_MAX_START,
            kernel::smp::is_sipi_addressable(frame)
        )),
        Err(error) => logger.error(format_args!(
            "smp: could not reserve an AP trampoline frame ({error:?}); AP startup (S3) needs a \
             frame below {:#x} because the SIPI vector is 8 bits and the AP starts at vector << 12. \
             continuing: S1 only collects information and does not start APs",
            kernel::smp::TRAMPOLINE_MAX_START
        )),
    }

    // === S3-b-2b-1: AP 用スタックのフレームを予約する ===
    //
    // **ここで取るのは、`run_timer_loop` にフレームアロケータが無いからである**
    // （トランポリン用フレームと同じ理由）。**恒等 VA で使うので低位でなければ
    // ならず**、アロケータが最小のフレーム番号から配るうちに取る。
    match kernel::smp::reserve_ap_stacks(&mut allocator) {
        Ok(count) => logger.info(format_args!(
            "smp: reserved {count} AP stack frame(s), all below the identity limit"
        )),
        Err(error) => logger.error(format_args!(
            "smp: could not reserve AP stack frames ({error:?}); AP startup will halt"
        )),
    }

    // === M2-d (d-1): 新しいページテーブルを構築する（CR3 は切り替えない） ===

    // フレームバッファは UEFI メモリマップに現れない（PCI BAR は別扱い。実機で確認）。
    // メモリマップだけに頼らず、BootInfo の範囲を明示的に足す
    // （`physical_address == 0` は BltOnly 等で無効なので除く）。
    // 生の値へ落とすのは一時的な措置で、T-2b / T-2c で `paging::plan` と
    // `frame_allocator` に型を入れるまでの橋渡しである。
    let fb_start = boot_info.framebuffer.physical_address.as_u64();
    let fb_end = fb_start + boot_info.framebuffer.size_bytes;
    let extra_ranges: &[(u64, u64, bool)] = if fb_start != 0 {
        &[(fb_start, fb_end, false)]
    } else {
        &[]
    };

    // マップ対象範囲の計画。frame_allocator::build と同じ classify() を通るので、
    // 判定基準がずれることはない。
    let mapped_ranges = MappedRanges::<{ kernel::paging::plan::DEFAULT_CAPACITY }>::build(
        raw_map,
        boot_info.memory_map.descriptor_size,
        extra_ranges,
    )
    .unwrap_or_else(|e| {
        logger.error(format_args!("paging plan build failed: {e}"));
        cpu::halt_forever();
    });

    // 不変条件: アロケータが配りうる全フレームがこの計画に含まれる。破れると、後で
    // 配られたフレームが未マップのまま使われて無言で壊れる。classify() を共有して
    // いるので理屈の上では常に成立するが、実装がずれても気づけるよう実行時にも見る。
    let mut allocator_ranges_covered = true;
    for (start_frame, frame_count) in allocator.free_ranges() {
        let start = start_frame * frame_allocator::FRAME_SIZE;
        let end = (start_frame + frame_count) * frame_allocator::FRAME_SIZE;
        let (Some(start_phys), Some(end_phys)) = (
            common::addr::PhysAddr::new(start),
            common::addr::PhysAddr::new(end),
        ) else {
            allocator_ranges_covered = false;
            logger.error(format_args!(
                "paging: allocator free range {start:#x}..{end:#x} does not fit in a physical address"
            ));
            continue;
        };
        if !mapped_ranges.contains_range(start_phys, end_phys) {
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

    logger.info(format_args!(
        "paging: {} mapped range(s) planned:",
        mapped_ranges.range_count()
    ));
    for r in mapped_ranges.iter() {
        logger.info(format_args!(
            "paging:   {:#x}..{:#x} cacheable={}",
            r.start.as_u64(),
            r.end.as_u64(),
            r.cacheable
        ));
    }

    // ここから実際にページテーブルへ書き込む（`paging::table`）。
    let mut builder = PageTableBuilder::new(&mut allocator, common::addr::direct_map())
        .unwrap_or_else(|e| {
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
            m.phys_addr.as_u64(),
            m.huge,
            e
        ));
        cpu::halt_forever();
    }

    // higher-half（B-2a）: kernel イメージを高位（KERNEL_VIRT_BASE + phys）にも張る。
    // base=0 では高位 VA == 恒等 VA で、既にこのビルダーが恒等で張った 4KiB PT を
    // 同一物理・同一フラグで上書きするだけ（冪等、新規フレーム 0）。イメージは
    // [0x100000, 0x200000) の 4KiB 領域に収まるので、2MiB huge との衝突
    // （ensure_child の UnexpectedHugePageEntry）は起きない。再リンク（B-2a-3）後は
    // PML4[511] 配下に実マッピングを作る。これがないと、再リンク後にこのテーブルへ
    // CR3 を切り替えた瞬間、高位で走るコードが見えなくなって即死する。
    // (c) highhalf-no-kernel-high-in-live-table: この呼び出しを外すと本流テーブルに
    // 高位マッピングが無くなり、下の CR3 切り替えで死ぬ（期待署名は実測で確定）。
    #[cfg(not(feature = "highhalf-no-kernel-high-in-live-table"))]
    map_kernel_high_half(&mut builder, &mut logger);

    logger.info(format_args!(
        "paging: {huge_page_count} huge (2MiB) page(s), {small_page_count} small (4KiB) page(s), \
         {} frame(s) consumed for page tables",
        builder.frames_used()
    ));

    // 必須領域の充足検証。ここに挙げる領域は CR3 切り替え後も引けなければならず、
    // 欠けると切り替え直後にトリプルフォルトする。
    let (kernel_start_phys, kernel_end_phys) = kernel_image_phys_range();
    let kernel_start = kernel_start_phys.as_u64();

    // **未解決の恒等前提。** BootInfo とフレームバッファは仮想アドレスとして得た値を、
    // 物理アドレスの範囲を見る `check_range` へ渡している。恒等マッピングだから通って
    // いるだけである。どちらも kernel イメージ外なので、下の image_phys（リンク差で
    // 物理へ戻す）は使えない。解消するには変換を挟むか検証を分ける。
    // RSP と RIP はここを通らない。B-2a-2 で image_phys へ移した（下を参照）。
    let identity = |virt: u64| {
        common::addr::PhysAddr::new(virt)
            .expect("an identity-mapped address fits in a physical address")
    };

    let boot_info_start = boot_info as *const BootInfo as u64;
    let boot_info_end =
        boot_info_start + (BOOT_INFO_PAGE_COUNT as u64) * frame_allocator::FRAME_SIZE;
    let mmap_start_phys = boot_info.memory_map.descriptors_ptr;
    let mmap_end_phys = mmap_start_phys
        .checked_add(boot_info.memory_map.descriptors_len)
        .expect("the memory map buffer stays within the physical address range");
    // fb_start/fb_end は上（extra_ranges 構築時）で計算済みのものを使う。
    let current_rsp = cpu::read_rsp();
    let current_rip = cpu::read_rip();
    let pml4_phys = builder.pml4_phys();

    let mut all_required_ok = true;
    // 必須領域はいずれも物理アドレスの範囲である。マップ計画が物理で書かれているため。
    // 恒等の間は仮想と値が一致するので `u64` のままでも通ってしまうが、`PhysAddr` を
    // 要求すれば呼び出し側が何のアドレスかを意識せざるを得ない。
    let mut check_range =
        |name: &str, start: common::addr::PhysAddr, end: common::addr::PhysAddr| {
            let ok = mapped_ranges.contains_range(start, end);
            logger.info(format_args!(
                "paging: required range [{name}] {:#x}..{:#x}: {}",
                start.as_u64(),
                end.as_u64(),
                if ok { "OK" } else { "NG" }
            ));
            if !ok {
                all_required_ok = false;
            }
        };
    check_range("kernel image", kernel_start_phys, kernel_end_phys);
    check_range(
        "BootInfo",
        identity(boot_info_start),
        identity(boot_info_end),
    );
    check_range("memory map buffer", mmap_start_phys, mmap_end_phys);
    if fb_start != 0 {
        check_range("framebuffer", identity(fb_start), identity(fb_end));
    }
    check_range(
        "new page tables (PML4)",
        pml4_phys,
        pml4_phys
            .checked_add(frame_allocator::FRAME_SIZE)
            .expect("a page table frame stays within the physical address range"),
    );
    // **解消済みの恒等前提（B-2a-2で解消）。** RSP と RIP は kernel イメージ内
    // （スタックは .bss、コードは .text）を指すので、恒等ではなくリンク差
    // （KERNEL_VIRT_BASE）で物理へ変換する。再リンク（B-2a-3）で高位になっても
    // 物理へ戻せる。base=0 では素通し。
    // BootInfo・メモリマップ・フレームバッファは kernel イメージ外なので恒等のまま
    // にする（上の check_range と、その手前の「未解決の恒等前提」マーカー）。
    let image_phys = |virt: u64| {
        kernel::kernel_phys_from_virt(
            common::addr::VirtAddr::new(virt).expect("an rsp/rip value is canonical"),
        )
    };
    check_range(
        "current RSP",
        image_phys(current_rsp),
        image_phys(current_rsp + 1),
    );
    check_range(
        "current RIP",
        image_phys(current_rip),
        image_phys(current_rip + 1),
    );

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
    logger.info(format_args!(
        "paging: current CR3 = {:#x}",
        old_cr3.as_u64()
    ));

    // pml4_phys はフレームの先頭（frame * FRAME_SIZE）なので下位12ビットは常に 0 の
    // はずだが、実行時にも見る。CR3 の下位には PWT/PCD（bit 3, 4）が載るため。
    if !pml4_phys.is_aligned(0x1000) {
        logger.error(format_args!(
            "paging: new PML4 {:#x} is not 4KiB aligned; refusing to switch CR3",
            pml4_phys.as_u64()
        ));
        cpu::halt_forever();
    }
    let cr3_value = pml4_phys;

    // 切り替え前スナップショット（切り替え後の整合性確認に使う）。
    // **未解決の恒等前提（順序依存。記録でしか守れない）。** kernel_start は物理値を
    // 低位 VA として read する。ここは A-2（activate_direct_map_window）より前で
    // direct_map() が base=0 を返すので、phys_to_virt しても同じ低位 VA になる
    // （handoff.boot_info の「未解決の恒等前提」が高位化できなかったのと同じ構造）。
    // したがって高位化できず、恒等除去（B-2b-4）より前に走ることに依存する。
    // 反転関門（DirectMap::new の IDENTITY_REMOVED）は DirectMap の構築を捕まえるが、
    // この生の低位 read は経由しないので捕まえない。
    // 順序要件: kernel_first_byte_before と kernel_first_byte_after が恒等除去点より
    // 後ろへ来ないこと。read を除去点の後ろへ動かすのも、除去点をこの read の前へ
    // 動かすのも違反である（除去点をさらに後ろへ動かすのは安全）。
    // 一覧は docs/verification-coverage.md の「higher-half B-2b」。
    // SAFETY: kernel_start は必須領域検証で読み取り可能を確認済み。
    let kernel_first_byte_before = unsafe { core::ptr::read_volatile(kernel_start as *const u8) };

    // SAFETY: switch_to が要求する3つを、別々の機構が満たす。
    // - 実行中のコードと現在のスタック: 再リンク（B-2a-3）後の RIP と RSP は kernel
    //   イメージ内の高位 VA（.text と .bss のカーネルスタック）で、引けるように
    //   しているのは上の map_kernel_high_half である。恒等ではない。必須領域検証の
    //   "current RIP" / "current RSP" が見たのは image_phys で落とした物理の側で、
    //   高位が引けることは見ていない。
    // - pml4_phys 自身のフレーム: こちらは恒等である。必須領域検証の "new page
    //   tables (PML4)" が計画にあることを確認済みで、恒等を外す B-2b-4 はずっと
    //   後にある。直後の verify_page_tables も A-2 より前で direct_map() の base が
    //   0 なので、同じ恒等で読む。
    unsafe {
        paging::switch::switch_to(cr3_value);
    }

    // ここが出れば CR3 切り替え命令は実行できた（トリプルフォルトしていない）。
    logger.info(format_args!("paging: CR3 switch instruction executed"));

    let new_cr3 = paging::switch::read_cr3();
    let cr3_ok = new_cr3 == cr3_value;
    logger.info(format_args!(
        "paging: CR3 readback {:#x} (expected {:#x}): {}",
        new_cr3.as_u64(),
        cr3_value.as_u64(),
        if cr3_ok { "OK" } else { "NG" }
    ));
    if !cr3_ok {
        logger.error(format_args!("paging: CR3 readback mismatch; halting"));
        cpu::halt_forever();
    }

    // 稼働中のページテーブルを読み戻し、`plan` が意図した内容と一致するかを確かめる
    // （M5-a-1）。M5-a-2 の分割・アンマップを検証する道具でもある。先に用意して
    // おけば、後から入れる操作の結果をそれを行ったコードとは独立に確かめられる。
    verify_page_tables(&mut logger, &mapped_ranges);

    let mut post_switch_ok = true;

    // (a) kernel イメージの読み取り検証。
    logger.info(format_args!("paging: about to test: kernel image read"));
    // SAFETY: kernel_start は必須領域検証で読み取り可能を確認済み。
    let kernel_first_byte_after = unsafe { core::ptr::read_volatile(kernel_start as *const u8) };
    let kernel_read_ok = kernel_first_byte_after == kernel_first_byte_before;
    logger.info(format_args!(
        "paging: kernel image read: {}",
        if kernel_read_ok { "OK" } else { "NG" }
    ));
    post_switch_ok &= kernel_read_ok;

    // (b) スタックの読み書き検証。ローカル変数は volatile でもレジスタに置かれうる
    // ので、それではスタックへ触ったことにならない。RSP を実レジスタから読み、その
    // 少し下（生きているスタックフレームより低い未使用側）へ生ポインタで書いて
    // 読み戻す。
    logger.info(format_args!("paging: about to test: stack read/write"));
    let stack_probe_addr = cpu::read_rsp().wrapping_sub(256);
    const STACK_PROBE_PATTERN: u64 = 0xDEAD_BEEF_CAFE_0000;
    // SAFETY: stack_probe_addr は現在の RSP より低い未使用側で、使用中のスタック
    // フレームには重ならない。必須領域検証の "current RSP" と同じマップ済み範囲にある。
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

    // (c) BootInfo の再検証（マジック値の再チェックを流用）。
    logger.info(format_args!("paging: about to test: BootInfo read"));
    let boot_info_ok = boot_info.validate().is_ok();
    logger.info(format_args!(
        "paging: BootInfo read: {}",
        if boot_info_ok { "OK" } else { "NG" }
    ));
    post_switch_ok &= boot_info_ok;

    // (d) フレームバッファへの実描画（M3-a）。読み戻しだけでは、読めた値がフレーム
    // バッファのものかキャッシュ上の値かを区別できない。目視できるテストパターンを
    // 描くことが、CR3 切り替え後も到達できている証明を兼ねる。
    let mut framebuffer = init_framebuffer(&mut logger, boot_info, &mapped_ranges);

    // 起動時テストパターンは M3-a の検証手段で、通常起動では描かない。コンソールは
    // 変更範囲しか転送しないので、描いたままだとコンソール領域の外に残骸が残る
    // （ADR-0017）。検証は `cargo xtask run --gfx-test`。通常起動では、この後の
    // コンソール初期化による全面クリアが到達の目視確認を兼ねる。
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

    // === higher-half A-1: direct physical map の導入 ===
    //
    // 恒等と direct map 窓（DIRECT_MAP_BASE + phys）の両方を持つ新テーブルを構築し、
    // CR3 を切り替える。恒等は外さない（B の領域）。登録 DirectMap も恒等のまま
    // （差し替えは A-2 の replace_direct_map）。ヒープ初期化より前なので、載せ替える
    // べき低位ポインタはまだ無い（ADR-0021 の Addendum、判断2）。
    build_and_switch_direct_map(&mut logger, &mut allocator, &mapped_ranges);

    // === higher-half A-2: 登録 DirectMap を高位窓へ差し替える ===
    //
    // これ以降 direct_map().phys_to_virt は高位を返す（恒等は残す）。差し替え後に
    // phys_to_virt を呼ぶ経路（init_console の base_virt など）は自動的に高位になる。
    // 差し替え前に値を計算して持っているフレームバッファだけは追従しないので、
    // 明示的に高位 base へ載せ替える。
    activate_direct_map_window(&mut logger);
    rehome_framebuffer_to_window(&mut logger, &mut framebuffer, boot_info);
    // boot_info の最終利用はここ（rehome）で、以降は触らない。低位 VA
    // （handoff.boot_info、恒等前提）なので恒等除去（B-2b-4）後は無効になる。有用な
    // データは抽出済み（memory_map はフレームアロケータへ、framebuffer は高位窓へ）。
    // 除去点より後ろで boot_info を参照するコードを足さないこと。反転関門は生の低位
    // 参照を捕まえない（docs/verification-coverage.md の「higher-half B-2b」）。

    // === M3-c-3: 画面コンソール ===
    //
    // ここより前のログはシリアルにしか出ない。コンソールはバックバッファの確保に
    // フレームアロケータを使い、フレームバッファへ書くのは CR3 切り替え後なので、
    // ここより前には作れない。Linux も同じ構造で、起動初期のログは printk の
    // バッファに溜まり、コンソールドライバの登録まで画面に出ない（ADR-0017）。

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
    let heap_start = heap_start_frame.as_u64();
    let heap_size = heap_frame_count * frame_allocator::FRAME_SIZE;
    let heap_end = heap_start + heap_size;
    let heap_mapped = match (
        common::addr::PhysAddr::new(heap_start),
        common::addr::PhysAddr::new(heap_end),
    ) {
        (Some(s), Some(e)) => mapped_ranges.contains_range(s, e),
        _ => false,
    };
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

    // **解消済みの恒等前提（B-2b-2で解消）。** かつては heap_start（物理値）をその
    // ままヒープ基底 VA として渡していた。恒等の間だけ通り、恒等除去（B-2b-4）後は
    // デレフでフォルトする。ヒープは除去後もタイマループ・タスク・コンソールの全
    // アロケーションで使われるので、direct map の高位窓へ載せる。以降アロケーションは
    // 高位 VA を返し、除去を跨いで生き残る。
    // 網羅列挙は docs/verification-coverage.md の「higher-half B-2b」。
    let heap_virt_base = common::addr::direct_map()
        .phys_to_virt(
            common::addr::PhysAddr::new(heap_start)
                .expect("heap arena is a valid physical address"),
        )
        .as_u64();
    // 破壊 (highhalf-remove-before-highify): ヒープ基底を低位（heap_start=物理）へ戻し、
    // 除去より前に高位化する順序の必要性を実証する（B-2b-4(e)）。heap_virt_base は下の
    // 恒等除去の必須領域チェックでも使うので、このビルドでも計算だけ残す。したがって
    // step4 は高位 VA で通り除去は完了し、死ぬのは除去後に低位ヒープを触った瞬間である。
    #[cfg(not(feature = "highhalf-remove-before-highify"))]
    let heap_init_base = heap_virt_base;
    #[cfg(feature = "highhalf-remove-before-highify")]
    let heap_init_base = heap_start;
    // SAFETY: [heap_start, heap_end) は今アロケータから切り出した、他の誰も使って
    // いない領域で、直前に mapped_ranges でマップ済みを確認した。既定の heap_init_base
    // はその物理を direct map 高位窓へ写した VA で、窓は RW・マップ済み。`init` の
    // 呼び出しはこれが最初で最後である。
    unsafe {
        ALLOCATOR.init(heap_init_base, heap_size);
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

    // kernel イメージの高位マッピングを持つ新テーブルの構築と検証（H-2）。
    // CR3 は切り替えない。稼働中のテーブルにも手を加えない。
    build_and_verify_high_half(&mut logger, &mut allocator, &mapped_ranges, direct_map);

    // 2MiB ページの分割とアンマップ（M5-a-2-1）。
    verify_split_and_unmap(&mut logger, &mut allocator);

    // ページテーブル操作の回帰チェック（M5-a-2-2）。通常ビルドには入らない。
    #[cfg(feature = "paging-test")]
    run_paging_test(&mut logger, &mut allocator, heap_start);

    // === M5-b: カーネルスタックのガードページ化 ===
    //
    // 自前のページテーブルへ切り替え済み（M2-d / A）で、自前のカーネルスタックの上で
    // 動いている（_start で切り替え済み）ので、直下のガードページを unmap できる。
    // IST2 は _start の gdt::init / idt::init で配線済みなので、以降に溢れが起きても
    // #PF は IST2 上で動く。
    //
    // 分割・アンマップの回帰チェック（M5-a-2）より後に置く。`paging-test-bad-index`
    // などが `unmap_4kib` を全体的に壊すビルドを持ち、ガードページ化も同じ
    // `unmap_4kib` を使う。先に置くと壊れた unmap でガードが作れず fail-fast し、
    // 回帰チェックの判定行より前に止まる。通常運転（タイマループ以降）はこの後に
    // 始まるので、steady state は保護される。
    install_kernel_stack_guard_page(&mut logger, &mut allocator);

    // ユーザーページのマッピング能力の検証（M5-e-2）。専用サブツリー
    // PML4[USER_PML4_INDEX] へ U=1 ページを張り、両側 U/S 監査で権限分離を実状態で
    // 確かめ、葉だけ落として中間は M5-e-3 のために残す。paging-test ビルドでは
    // unmap を全体破壊するので載せない（関数側で cfg 済み）。
    #[cfg(not(feature = "paging-test"))]
    verify_user_page_mapping(&mut logger, &mut allocator);

    // Ring 3 への単発遠征の検証（M5-e-3）。M5-e-2 が残した PML4 サブツリーへ
    // ユーザーコード/スタックを張り、iretq で Ring 3 へ落ち、cli の #GP を
    // 予期の畳みでカーネルへ戻す。RSP0 が実挙動で初めて効く。
    #[cfg(not(feature = "paging-test"))]
    verify_ring3_excursion(&mut logger, &mut allocator);

    // int 0x80 システムコールの往復の検証（M5-f-1）。verify_ring3_excursion が残した
    // ユーザーページを再利用する。Ring 3 から int 0x80 を発行し、syscall_entry
    // （空ディスパッチャ）が RSP0 スタックで走り、iretq で Ring 3 へ戻り、続く cli の
    // #GP を予期の畳みでカーネルへ戻すまでを確かめる。
    #[cfg(not(feature = "paging-test"))]
    verify_syscall_roundtrip(&mut logger);

    // ユーザーポインタ検証の検証（M5-f-2-1）。Ring 3 が (buf, len) を渡す syscall で、
    // カーネルが読み書きに踏み込む前に範囲を実 PTE で検証する。正常系と異常系5ケースを
    // 回す。
    #[cfg(not(feature = "paging-test"))]
    verify_syscall_pointer(&mut logger, &mut allocator);

    // ユーザーバッファの内容往復の検証（M5-f-2-2）。カーネルが既知内容をユーザー
    // バッファへ書き、SYS_CHECKSUM を発行し、検証 → copy_from_user → 総和が期待値と
    // 一致することを確かめる。
    #[cfg(not(feature = "paging-test"))]
    verify_syscall_checksum(&mut logger);

    // Ring 3 の4ベクタが中断され、カーネルが継続することの検証（S8-d-2）。
    // verify_ring3_excursion が残したユーザーページを使い回す。アロケータも恒等窓も
    // 要らない（既にあるページへ命令列を書くだけで、#PF の対象は未マップのまま使う）。
    #[cfg(not(feature = "paging-test"))]
    verify_ring3_fault_vectors(&mut logger);

    // === S1-b-1: ACPI テーブルの検証つき走査 ===
    //
    // 置ける区間が上下から挟まれている。
    //   - 下限は A-2（direct map 窓の高位化）。物理を読むのに窓を使う。
    //   - 上限は恒等除去（すぐ下）。`raw_map` は低位 VA のスライスで、除去後は無効に
    //     なる。ACPI の物理アドレスがどのメモリ型に載るかを見るのに使う。
    // 検査そのものは高位窓の翻訳を見るので、除去を跨いでも結論は変わらない（除去が
    // 落とすのは `PML4[0]` だけ）。呼び出し位置を動かすときは両方の境界を確かめること。
    // どちらを踏み外しても、症状は「ACPI が読めない」ではなく低位 VA のデレフによる
    // #PF になる。
    //
    // 異常があっても停止しない。S1 は情報を集める段で、ACPI が読めないだけで単一コアの
    // カーネルが起動しなくなるのは機能的な後退である。致命へ格上げするのは S2 である。
    //
    // ログはシリアルのみ（`log_both` を使わない）。近傍の検証サイトはいずれもシリアル
    // のみで、`log_both` は人が読む要約に使っている。ACPI の走査は検証の材料なので
    // 前者へ揃える。
    let apic_mmio =
        kernel::acpi::survey(&mut logger, acpi_rsdp, raw_map, memory_map_descriptor_size);

    // === S1-c: APIC MMIO を direct map 窓へ 4KiB 粒度で写像する ===
    //
    // survey の直後に置く。写像に使う所在は survey が返した値そのもので、産地と利用点を
    // 離さないため。survey と違って恒等除去より前である必要は無いが（UEFI メモリマップの
    // スライスを使わない）、離す理由も無い。
    //
    // APIC へは移行しない。割り込みは PIC のままである（移行は S2）。ここでやるのは
    // 写像と、Local APIC を読めることの確認だけで、レジスタへは書き込まない。
    let mapped_apic = kernel::apic::map_and_probe(&mut logger, &mut allocator, &apic_mmio);

    // === S3-b-2b-2: AP の per-CPU 資産を用意する ===
    //
    // 位置が正しさの条件である。要るのは2つ。
    //   - フレームアロケータ（`run_timer_loop` には無い。AP を起こすのはそこである）
    //   - 本番テーブルが CR3 に載っていること
    //
    // 早すぎると壊れる。実際に踏んだ。最初はトランポリン用フレームの予約の直後
    // （M2-d の CR3 切り替えより前）に置いたので、`read_cr3()` が bootstrap PML4 を
    // 返し、AP をそちらへ移してしまった。AP は自分のスタック（PML4[258]）までは
    // 動いたが、direct map（PML4[256]）が無いので最初の参照で #PF になった。
    //
    // PML4[258] へ張る（`PML4[257]` は破壊 feature のサボタージュ VA）。
    // SAFETY: A-1 の切り替えが済んで本番テーブルが CR3 に載っており、起動時の
    // 単一文脈で AP はまだ走っていない。
    unsafe {
        kernel::smp::prepare_ap_per_cpu(&mut logger, &mut allocator);
        // 探り用ページは、アロケータのあるここで張る（S5-c）。
        // SAFETY: 本番テーブルへ切り替え済みで、direct map 窓が使える。
        #[cfg(feature = "smp-tlb-shootdown-probe")]
        unsafe {
            kernel::smp::prepare_shootdown_probe(&mut logger, &mut allocator)
        };
    }

    // === S2-a: APIC のレジスタを読んで現在値を記録する ===
    //
    // 読むだけの段で、割り込みの経路は変えない（PIC / PIT のまま）。I/O APIC の
    // IOREGSEL への書き込みだけは伴う。読みたいレジスタを選ぶセレクタで割り込みの
    // 設定ではないが、S2 で最初の書き込みはここである。
    if let Some(mapped_apic) = mapped_apic.as_ref() {
        kernel::apic::survey_registers(&mut logger, mapped_apic);

        // === S2-b: スプリアスベクタを予約例外ベクタの外へ移す ===
        //
        // ここが LAPIC への最初の書き込みである。書くのは SVR のベクタ欄だけで、
        // bit 8（有効化）は読んだ値から保つ。bit 8 を落とすと LINT0 経由で届いて
        // いる 8259 の IRQ0 が止まる。危険なのは bit 8 であってベクタ欄ではない。
        // 振る舞いは変わらない（スプリアスは現在発生しない）。
        kernel::apic::set_spurious_vector(&mut logger, mapped_apic);

        // === S3-b-1: cpu_id() を Local APIC ID 由来へ差し替える ===
        //
        // ここより前は cpu_id() が定数 0 を返す経路である。gdt::init が this_cpu_ptr を
        // 通るのは Local APIC を写像するより前なので、据わる前に呼ばれるのは避けられ
        // ない。MAX_CPUS = 1 では据える前も後も 0 で振る舞いは変わらない。GS ベースは
        // 使わない（apic.rs の該当節）。
    }

    // === S3-b-1: per-CPU スロットが起動しうるコア数を覆っているかを報告する ===
    //
    // 停止はしない。この段では cpu_id() が定数 0 で、走るのは bootstrap processor
    // だけなので、列挙が MAX_CPUS を超えても配列外の索引は起きない。停止が正しく
    // なるのは cpu_id() が非 0 を返しうる S3-b-2a である（当初ここで停止させたら
    // -smp 2 の起動が止まった。apic.rs の doc）。
    // `mapped_apic` の有無に依らず行う。覆えているかを問うのはコア数と MAX_CPUS の
    // 関係で、APIC の写像が成功したかとは別である。
    kernel::apic::report_per_cpu_slot_coverage(&mut logger, apic_mmio.usable_local_apics());

    // === higher-half B-2b-4（恒等除去） ===
    //
    // ここが恒等（PML4[0]）を外す点。恒等窓を握る4検証サイト（verify_user_page_mapping /
    // verify_ring3_excursion / verify_syscall_roundtrip / verify_syscall_pointer）と
    // verify_syscall_checksum が全て走り終えた後、start_timer より前。この時点で
    // boot_info は無効である（低位 VA、最終利用は上の rehome）。順序依存の詳細と除去
    // 手順は docs/verification-coverage.md の「higher-half B-2b」。
    //
    // (d) で既定有効化した。paging-test は split/unmap の破壊検査が目的で恒等除去まで
    // 通さないので除外する（そのビルドでは恒等除去が検査されない＝カバレッジ穴。
    // verification-coverage に記録）。highhalf-remove-verify-fail のときは下でヒープの
    // 高位 VA を解決不能値へ差し替え、step4 失敗から 5a 復帰を実証する（除去は既定と
    // 同じく走る）。
    #[cfg(not(feature = "paging-test"))]
    {
        use common::addr::{PhysAddr, VirtAddr};
        use kernel::paging::remove::{remove_identity, RequiredRegion};

        let direct_map = common::addr::direct_map();

        // lib が導ける領域（RIP/RSP/direct map 窓/カーネルイメージ）は remove_identity が
        // 内部で足す。ここで渡すのは lib が知り得ない高位 VA だけである。
        //   - ヒープの高位 VA（heap_virt_base、B-2b-2）
        //   - フレームバッファの高位 VA。`framebuffer` 変数は console へ move 済みなので、
        //     rehome と同じ式で fb_start（物理）から `phys_to_virt` で再計算する
        // 新しく低位ポインタを高位化したら、この列にも足すこと
        // （verification-coverage の「解消済み」群と同期）。
        //
        // 破壊 (highhalf-remove-verify-fail): 解決不能な高位 VA（空の PML4[257]）へ
        // 差し替え、step4 を失敗させて 5a の復帰経路を実証する。既定は heap_virt_base。
        #[cfg(not(feature = "highhalf-remove-verify-fail"))]
        let heap_high_va = VirtAddr::new(heap_virt_base).expect("the heap high VA is canonical");
        #[cfg(feature = "highhalf-remove-verify-fail")]
        let heap_high_va = VirtAddr::new(0xFFFF_8080_0000_0000)
            .expect("the sabotaged heap VA is canonical (empty PML4[257])");

        // 不可逆な除去操作はヒープに依存しない。フラッシュ後にヒープが使えない可能性が
        // あるので、high_mapped は Vec でなくスタックの固定配列で持つ。Vec にしていた
        // とき、remove-before-highify（ヒープを低位のまま除去）が「除去自身の step6 が、
        // 低位ヒープ上の Vec をフラッシュ後に辿って #PF で復帰不能になる」経路を露呈した。
        // step4 はヒープの高位 VA が解決するかを見るが、除去自身のデータ構造が高位に
        // あるかは見ない。生き残ることが構造的に保証されたもの（スタック = .bss、再リンクで
        // 高位化済み）だけに依存する。「ここで Vec を使えば楽」と戻さないための不変条件。
        //
        // 除去の実体側は構造的に確保できない。`remove_identity`（`paging::remove`）と、それが
        // 呼ぶ `paging::verify` / `paging::table` / `paging::switch` はいずれも `alloc` を
        // import しないので、Vec/Box/String の確保が不可能である（grep より強い保証）。残る
        // 確保の可能性はこの呼び出し側だけで、それを high_mapped のスタック配列化で断つ。
        // 除去経路全体でヒープ確保はゼロである。
        //
        // 配列サイズは必須領域リストと同期する。現在は 2（ヒープ・フレームバッファ）。
        // 高位化した低位ポインタを増やすときは、この配列サイズ・下の `n`・`remove_identity`
        // 内部の `always`・docs/verification-coverage.md の「解消済み」群を同時に増やす。
        // 配列とリストの対応をコンパイル時に縛る手段は、リストが呼び出し側と lib 内部に
        // またがるため単純には作れない。
        let regions: [RequiredRegion; 2] = [
            RequiredRegion {
                name: "heap high VA",
                va: heap_high_va,
            },
            if fb_start != 0 {
                RequiredRegion {
                    name: "framebuffer high VA",
                    va: direct_map.phys_to_virt(
                        PhysAddr::new(fb_start).expect("the framebuffer phys is valid"),
                    ),
                }
            } else {
                // フレームバッファ無し。この要素はスライス（`..n`）で落とすので、
                // 配列を埋めるためだけにヒープの高位 VA を置く。
                RequiredRegion {
                    name: "heap high VA",
                    va: heap_high_va,
                }
            },
        ];
        let n = if fb_start != 0 { 2 } else { 1 };
        let high_mapped: &[RequiredRegion] = &regions[..n];

        // SAFETY: 恒等窓を握る全検証サイトと boot_info の消費、除去点より前に走るべき
        // 生の低位 read（kernel_start）はいずれも終えている。direct_map は登録高位窓で、
        // 稼働 PML4 配下を読み書きできる。順序前提は上のコメントと
        // docs/verification-coverage.md の「higher-half B-2b」に従う。
        unsafe {
            remove_identity(&mut logger, direct_map, high_mapped);
        }
    }

    // 破壊 (highhalf-panic-after-remove): 恒等除去の直後に意図的 panic する。パニック経路
    // （シリアル I/O・レジスタ値のみ・walk なし。ADR-0003）が恒等非依存であることを、
    // 恒等を外した実状態で確認する（B-2b-4(e)）。
    #[cfg(feature = "highhalf-panic-after-remove")]
    panic!("intentional panic right after identity removal (highhalf-panic-after-remove)");

    // ロック保持中は割り込みが禁止され、解放後に元へ戻ることを確認する（M4-c-2）。
    // ヒープのロックそのものではなく同じ Locked<T> を使う。ヒープのロックを保持した
    // ままログを出すと二重取得になるため。
    report_lock_interrupt_state(&mut logger);

    // 実地スモークテスト: Vec/Box/String を確保・追記・解放する。
    // ヒープは direct map 高位窓上にある（B-2b-2）ので、確保したポインタは高位 VA。
    // `range_is_mapped` は物理範囲を見るので、高位窓経由で物理へ戻してから照合する
    // （恒等の間は高位 VA と低位 VA が同じ物理を指すので結果は不変）。
    let heap_mapped = |va: u64, len: u64| -> bool {
        match common::addr::VirtAddr::new(va)
            .and_then(|v| common::addr::direct_map().virt_to_phys(v))
        {
            Some(p) => range_is_mapped(&mapped_ranges, p.as_u64(), p.as_u64() + len),
            None => false,
        }
    };
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
            heap_mapped(v_ptr, v_len_bytes)
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
            heap_mapped(b_ptr, 4)
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
            heap_mapped(s_ptr, s_len)
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

    // ダーティ矩形が効いているかを数字で残す。毎回のフラッシュでログを出すと、ログ
    // 自体が次のフラッシュを誘発するので、起動シーケンスの最後に 1 回だけ出す。
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
        // 転送の所要（S12 前の手当て）。**画面へ書く経路を BKL の内側へ入れてよいかの
        // 判断材料である。** サイクルは時刻ではなく回る量の目安として読む。
        log_both(
            &mut logger,
            console.as_mut(),
            LogLevel::Info,
            format_args!(
                "console: flush cycles total={} max={}, full-screen flush(es)={} max={}, \
                 write path (draw + flush) count={} total={} max={}",
                stats.flush_cycles_total,
                stats.flush_cycles_max,
                stats.full_screen_flush_count,
                stats.full_screen_cycles_max,
                stats.write_count,
                stats.write_cycles_total,
                stats.write_cycles_max
            ),
        );
    }

    // === S4-c-2: AP 用アイドルタスクを登録する ===
    //
    // まだ誰も走らせない。`pick_next` はワーカーしか候補にしないので、足しただけでは
    // 選ばれない。選ばれるようにするのは S4-c-3 である。
    //
    // 協調デモより前に置く。デモはスケジューラを触るので、登録を後ろへ置くと「デモ中に
    // タスクが増える」形になる。増えるのは起動時の 1 回だけにしておく。
    //
    // `smp::prepare_ap_per_cpu` より後でなければならない（per-CPU スタックの範囲を
    // 読む）。守れていなければ停止するので、順序は実行時に見える。
    kernel::task::init_ap_idle_task();

    // 協調的マルチタスクのデモと検証（M5-c）。2 本のワーカーが決定的に往復し、
    // 各タスクの全 GPR が切り替えを跨いで保たれることを確認する。戻ってくると
    // 起動シーケンスは続行する。
    // **アロケータを渡す（S12 前の手当ての C の途中）。** ワーカーのガードページが
    // 2MiB ページに載っていたら、張る前に分割するために要る
    // （`kernel::stack::install_guard_page`）。
    kernel::task::run_cooperative_demo(&mut allocator);

    // 例外ハンドラの回帰チェック。起動シーケンスを最後まで通してから発火させる。
    // mapped_ranges でプローブアドレスの妥当性を見るので、ページング構築後に置く。
    #[cfg(feature = "exception-test")]
    trigger_exception_under_test(&mut logger, &mapped_ranges);

    // カーネルスタックのガードページの回帰チェック（M5-b）。意図的に溢れさせ、
    // #PF（CR2 = ガードページ）が IST2 上で報告されること、または #PF に IST を
    // 与えない構成では #DF へ昇格することを確認する。戻らない。
    #[cfg(feature = "stack-guard-test")]
    trigger_stack_guard_test(&mut logger);

    // クリティカルセクション/ロックの回帰チェック。
    #[cfg(feature = "critical-test")]
    trigger_critical_test(&mut logger);

    // 割り込みを有効化する経路の回帰チェック（M4-d-1 / M4-d-2）。
    #[cfg(feature = "interrupt-test")]
    trigger_interrupt_test(&mut logger, console.as_mut());

    // === S7-c: プロセス別アドレス空間の切り替えを1往復する ===
    //
    // 到達条件4（CR3切り替え後もカーネルが動くこと）の観測である。新しい PML4 を作り、
    // カーネルの上位だけを写し、切り替え、戻す。
    //
    // ここに置くのは、下位を失っても困らない位置だからである。ring3 と syscall のデモは
    // 終わっており、AP はまだ起きていない。新しい空間の下位は空なので、切り替えている
    // 間にユーザー空間へ触ると #PF になる。触らない。
    demo_address_space_switch(&mut logger, &mut allocator);

    // **ここでフレームアロケータを預ける（S11-3。`ADR-0030`）。**
    // 起動の組み立てはここまでで終わり、**以降はユーザープログラムの
    // 写像だけがフレームを要る。** あちらは借りて、**Ring 3 へ落ちる前に返す。**
    //
    // **`deposit` は「返却」ではない**——ここは借りずに預ける唯一の場所である。
    // SAFETY: 起動シーケンスの単一実行文脈で一度だけ。まだ誰も借りていない。
    unsafe { kernel::frame_allocator::deposit(allocator) };

    // === S9-b-1: 埋め込んだユーザープログラムの ELF を読み、写像する ===
    //
    // **まだ走らせない**（Ring 3 への遷移は次の刻み）。ここまでで、像が読めること、
    // 新しいアドレス空間へ区画が張れること、**張った葉の W が区画の権限どおりで
    // あること**を見る。
    verify_embedded_fs_image(&mut logger);
    copy_fs_image_to_frames(&mut logger);
    verify_corrupt_fs_image_is_rejected(&mut logger);
    verify_embedded_user_elf(&mut logger);
    verify_corrupt_user_elf_is_rejected(&mut logger);
    verify_corrupt_user_program_is_not_loaded(&mut logger);
    // **貸し借りの回数を出す（S11-3。`ADR-0030`）。** 取り出した回数と返した
    // 回数が一致し、いまこの場に在れば、**その時点で返し忘れは無い。**
    let (taken, returned, present) = kernel::frame_allocator::lending_counts();
    logger.info(format_args!(
        "frame-allocator: lent out {taken} time(s), given back {returned} time(s), \
         balanced={}, present={present}",
        taken == returned
    ));

    if let Err(error) = load_embedded_user_program(&mut logger) {
        // **この段ではまだ止める。** 既定の `hello` は成功するので、ここへは来ない。
        // 壊した像を渡してプロセスだけを失敗させるのは S9-b-2 の 2 つ目である。
        logger.error(format_args!(
            "user-load: loading the embedded hello failed: {error:?}; halting"
        ));
        cpu::halt_forever();
    }

    // **ユーザープログラムを走らせた後にも、貸し借りを突き合わせる（S11-5）。**
    //
    // **上の行はプログラムを走らせる前の状態しか主張していない。**
    // `spawn` はシステムコールの中で借りて返す経路で、**そこが 1 度でも
    // 返し忘れれば、以後の確保がすべて `None` になる。**
    let (taken_after, returned_after, present_after) = kernel::frame_allocator::lending_counts();
    logger.info(format_args!(
        "frame-allocator: after the user programs, lent out {taken_after} time(s), given back \
         {returned_after} time(s), balanced={}, present={present_after}",
        taken_after == returned_after
    ));
    if taken_after != returned_after || !present_after {
        logger.error(format_args!(
            "frame-allocator: the lending is not balanced after the user programs; halting"
        ));
        cpu::halt_forever();
    }

    // === M4-d-2: タイマを動かす ===
    //
    // ここから先は戻らない。ZaytOS で初めて「時間が流れる」状態に入り、
    // メインループがハートビートを出し続ける。`stop_after_ticks` に 0 を
    // 渡すと止まらない（回帰チェックのときだけ有限で打ち切る）。
    start_timer(
        &mut logger,
        console.as_mut(),
        0,
        SHELL_AFTER_HEARTBEATS,
        mapped_apic.as_ref(),
    );

    // === S11-11: init がシェルを起こす ===
    //
    // **ここから戻らない。**
    //
    // **`console` を渡す（S12 前の手当て）。** メインループが抜けて画面の書き手が
    // 居なくなったので、**`init` が引き取る**（`run_init` の doc）。
    run_init(&mut logger, console.as_mut());
}

/// ハートビートを何本出してからシェルへ渡すか（S11-11）。
///
/// # なぜ本数で決めるのか
///
/// **ティック数の閾値だと、越えた時点で何本出ているかが揺れる**——
/// `hlt` から起きた時点で数えるので、**`-smp 1` と `-smp 2` で起動ログの行数が
/// 1 本ずれた**（実測）。**本数で決めれば、どの構成でも同じ本数だけ出る。**
///
/// # なぜ 0 ではないのか
///
/// **定常ループの観測を先に済ませる。** **2 本出ていれば「ループが起き続けている」
/// を示すには足りる**（`--full` の `interrupt-test` が 4 本以上を要求しているのは、
/// **500 ティックで止める構成でループが最後まで起きていること**を見るためで、
/// あちらはシェルを走らせない）。
///
/// # なぜ長くしないのか
///
/// **待っているあいだ、シェルは何もできない。** 手で触る人にとっては
/// ただの待ち時間である。**観測に足りる最短にする。**
///
/// # 測定の構成では渡さない
///
/// **タイマの速さを実時間と比べる項目**（`lapic-timer-test rate` と
/// `smp-ap-test ap-timer-rate`）は**20 秒ぶんのティックを要求する。**
/// **2 本目のハートビートで渡すと、カーネル側の時間が 2 秒で止まって測れない。**
/// **`keep-steady-loop` を立てた構成では渡さない**——**シェルはタイマ経路に
/// 触らないので、この差は測定の対象に影響しない。**
const SHELL_AFTER_HEARTBEATS: u64 = if cfg!(feature = "keep-steady-loop") {
    0
} else {
    2
};

/// `init`（S11-11）。**シェルを起こし、終わったら起こし直す。**
///
/// # なぜカーネル側に置くのか
///
/// **`init` がやることは「シェルを起こし直す」だけで、それはカーネルの直線上でも
/// 書ける。** **Ring 3 に置く値打ちは、いまのところ「Linux と同じ形」だけである。**
///
/// **そして遠征の深さが 1 つ浮く。** `init` を Ring 3 に置くと、
/// `init`（深さ 1）→ シェル（深さ 2）→ `ls`（深さ 3）となり、
/// **`MAX_EXCURSION_DEPTH` を 3 へ上げることになる**——**遠征スタックが
/// 64 KiB 増える。** **得るものが「形が同じ」だけなら、`docs/vision.md` の
/// 「先回りの抽象化を入れない」に当たる。**
///
/// **`docs/roadmap.md` の到達条件は「init と簡易シェル」である。**
/// **`init` の役目（子の始末）は果たしており、置き場所がカーネル側である。**
///
/// # 移す条件
///
/// **`init` が子の始末以外の仕事を持つようになったとき。**
/// そのとき `MAX_EXCURSION_DEPTH` を上げることになる。
///
/// # PID 1 が死んだら
///
/// **起こし直す。** `docs/vision.md` は「再起動・rescue・停止のいずれか」と
/// 書いている。**起こし直しを選ぶのは、シェルが落ちても触り続けられるからである。**
/// **起こせなくなったら止まる**——**同じ失敗を無限に繰り返さない。**
///
/// # 画面への書き手は、ここから `init` 1 つである（S12 前の手当て）
///
/// **`Console` を受け取り、判定行を [`log_both`] で画面にも出す。**
///
/// **それまでの書き手はメインループだった**（`interrupts::run_timer_loop`）。
/// **S11-11 でそのループがシェルへ渡すために抜け、書き手が居なくなった**——
/// `Console` は `kernel_main` の局所として生きていたが、**参照を持つ者が
/// 誰も居なくなり、画面は起動ログで止まった。**
///
/// **`deferred-decisions.md` の「コンソール / シリアルへの出力の多重化」は、
/// 「出力するのはメインループだけ」という制約で運用すると決めていた。**
/// **その制約は一度も破られていない。前提のほうが消えた**
/// （`verification-coverage.md` の「条件が前提にしている状態が消えると、
/// 条件は破られないまま意味を失う」）。**書き手を `init` へ付け替えて、
/// 「書き手は 1 つ」という形のほうを保つ。**
///
/// **多重化はここでも要らない。** **Ring 3 の `write` は画面へ出さない**
/// （`crate::syscall` の `sys_write`。シリアルへ出す）。**したがって
/// この関数が画面へ書いている間、他に書く者は居ない。**
///
/// **この「1 つであること」は判定行にしない。理由は 2 つある。**
///
/// **1 つ目——偽になりうる道が今は無い。** 2 人目の書き手が現れるときは、
/// その時点でコードが変わっている。**主張が偽になる道の無いものを判定行にすると、
/// 落ちない検査になる**（`verification-coverage.md` の
/// 「検査があるように見えて何も検査していない」）。
///
/// **2 つ目——`&mut` で渡している以上、「同時に 2 人が書けない」は
/// 借用検査が既に保証している。** [`Console`] は静的ではなく `kernel_main` の
/// 局所で、ここへは `Option<&mut Console>` として渡る。
/// **判定行を置いても、型が保証しているものを実行時に測り直すだけになる。**
///
/// **2 つの理由は、次に触る人にとって別のことを言っている。**
/// **1 つ目だけを読むと「実行時に測れないから諦めた」に見えるが、
/// 実際は「型が既に保証しているから要らない」である。**
/// **したがって [`Console`] を静的へ移すなら、そのとき型の保証が消える**
/// ——静的にした瞬間、書き手が 1 つであることを言うものが何も無くなる。
/// **そこが、この判断をやり直す場所である。**
///
/// **`deferred-decisions.md` の行が、条件としてこの doc を指している。**
///
/// # やり直した（S12 前の手当て）。**条件が発火した場所である**
///
/// **[`Console`] を静的の側へ移した**——シェルの出力を画面へ出すためである
/// （`crate::console::install_foreground`）。**上が名指しした場所に来た。**
///
/// **型の保証は消えていない。** 消えると書いたのは「静的にすれば参照が誰でも
/// 取れる」形を想定していたからで、**実際に採ったのは借用を預けるガードである。**
/// **据えている間、この関数は画面へ書けない**——`&mut Console` をガードが持つので、
/// 借用検査がそれを見る。**据える区間が `spawn` の呼び出しだけなのも、そのためである。**
///
/// **構造の側の根拠は 3 つある**——AP は画面へ書かない、`init` は `spawn` の間
/// ブロックしている、遠征は入れ子でも親が待つ。**`ADR-0023` の Addendum に
/// 6 つ目として書いた。**
///
/// # 画面に出すのは `init` の行だけ**ではなくなった**
///
/// **かつては子（シェル）の出力が画面へ出なかった。** シェルは `write` で
/// シリアルへ書き、**画面に出るのは「起こした / 終わった / 起こし直す」の 3 種類だけ**
/// だった。**いまは `write` が画面へも届く**（上の節）。
/// **`init` の 3 種類は変わらずこの関数が書く**——据えるのは `spawn` の間だけである。
fn run_init(logger: &mut Logger<SerialPort>, console: Option<&mut Console>) -> ! {
    /// 起こし直す上限。**同じ失敗を無限に繰り返さない。**
    const MAX_RESTARTS: usize = 3;

    // **借り直しながら回す。** `Option<&mut _>` は `Copy` ではないので、
    // 各周で `as_deref_mut` を取る（`interrupts::drain_keyboard` と同じ形）。
    let mut console = console;

    let mut restarts = 0usize;
    loop {
        log_both(
            logger,
            console.as_deref_mut(),
            LogLevel::Info,
            format_args!("init: starting {SHELL_PATH_TEXT} (restart {restarts} of {MAX_RESTARTS})"),
        );
        // **シェルが走っている間だけ、画面をシェルへ渡す（S12 前の手当て）。**
        //
        // **据えている間、この関数は画面へ書けない**——`&mut Console` をガードへ
        // 預けるので、借用検査がそれを見る。**「書き手は 1 つ」はそこが保証する。**
        // **外す時機は `spawn` が戻った直後である**（この束の終わり）。
        // その後の `init` の行は、また `init` が書く。
        let outcome = {
            let _foreground = console
                .as_deref_mut()
                .map(kernel::console::install_foreground);
            kernel::userland::spawn(SHELL_PATH, SHELL_ARGV, 1)
        };
        match outcome {
            Ok(outcome) => {
                // **止まった場所が分かる形で出す（S11-11）。** 打鍵が届かない
                // ときに、**どこまで来ていたかを 1 行で切り分ける。**
                //
                // - スキャンコードが 0 なら、**リングまで来ていない**
                //   （IRQ1 の配送か i8042 の側）
                // - 0 でなく届けたバイトが 0 なら、**前景か `read(0)` の側**
                // - 届けたバイトが 0 でなければ、**Ring 3 まで来ている**
                log_both(
                    logger,
                    console.as_deref_mut(),
                    LogLevel::Info,
                    format_args!(
                        "init: the shell ended ({outcome:?}); the keyboard ring received {} \
                         scancode(s) and the foreground handed {} byte(s) to Ring 3",
                        kernel::keyboard::buffer::received_count(),
                        kernel::input::delivered_count()
                    ),
                );
            }
            Err(error) => {
                log_both(
                    logger,
                    console.as_deref_mut(),
                    LogLevel::Error,
                    format_args!("init: could not start {SHELL_PATH_TEXT}: {error:?}"),
                );
                cpu::halt_forever();
            }
        }
        restarts += 1;
        if restarts > MAX_RESTARTS {
            log_both(
                logger,
                console.as_deref_mut(),
                LogLevel::Error,
                format_args!("init: the shell ended {MAX_RESTARTS} time(s); not starting it again"),
            );
            cpu::halt_forever();
        }
    }
}

/// シェルの像のパス。**NUL は付けない**（`spawn` はスライスを取る）。
const SHELL_PATH: &[u8] = b"/bin/zash";
/// 判定行に出すためのパス。
const SHELL_PATH_TEXT: &str = "/bin/zash";
/// シェルへ渡す `argv`。**NUL 区切りで並べる**（`spawn` の受け取る形）。
const SHELL_ARGV: &[u8] = b"zash\0";

/// フレームバッファを検証し、描画ハンドルを作る（M3-a）。
///
/// bootloader から渡された形状をそのまま信じず、[`FramebufferLayout`] の検証を通す。
/// 通らなければ理由を ERROR で残して `None` を返し、描画せずに続ける。halt しないのは、
/// フレームバッファが使えない環境でも起動シーケンスの診断ログを最後まで取りたいため
/// （ADR-0013）。画面が主たる出力手段になる M3-c では方針を見直す。
fn init_framebuffer(
    logger: &mut Logger<SerialPort>,
    boot_info: &BootInfo,
    mapped_ranges: &MappedRanges,
) -> Option<Framebuffer> {
    let info = &boot_info.framebuffer;
    let layout = match FramebufferLayout::from_info(info, common::addr::direct_map()) {
        Ok(layout) => layout,
        Err(e) => {
            logger.error(format_args!(
                "framebuffer: validation failed ({e:?}); drawing is disabled"
            ));
            return None;
        }
    };

    // 検証は「GOP の申告に内部矛盾が無いこと」しか見ていない。その範囲が現在の
    // ページテーブルでマップされているかは別問題なので、ここで確認する
    // （`Framebuffer::new` の安全性要件）。
    if !range_is_mapped(mapped_ranges, layout.base().as_u64(), layout.end().as_u64()) {
        logger.error(format_args!(
            "framebuffer: {:#x}..{:#x} is not fully mapped; drawing is disabled",
            layout.base().as_u64(),
            layout.end().as_u64()
        ));
        return None;
    }

    logger.info(format_args!(
        "framebuffer: validated {}x{} stride={} format={:?} {:#x}..{:#x}",
        layout.width(),
        layout.height(),
        layout.stride(),
        layout.format(),
        layout.base().as_u64(),
        layout.end().as_u64()
    ));

    // SAFETY: layout は FramebufferLayout の検証を通っており、最終行の末尾まで
    // size_bytes に収まる。base..end がマップ済みであることは直前に contains_range で
    // 確認した。フレームバッファは他の誰も使っておらず、この Framebuffer が唯一の
    // 書き込み手段になる（作るのはこの 1 箇所のみ）。
    Some(unsafe { Framebuffer::new(layout) })
}

#[cfg(feature = "gfx-test-pattern")]
/// 起動時のテストパターンを描く（M3-a）。
///
/// 目視で次を確認できるように選んである。
/// - 画面全体が塗られる: 形状の検証（`height * stride * 4 <= size_bytes`）が正しく、
///   全画面を描いても範囲外へ出ない
/// - 外周 1px の枠が四辺すべてに出る: stride の扱いが正しい。width と取り違えていると
///   枠が斜めにずれる
/// - 赤・緑・青の順に並ぶ: ピクセルフォーマット変換が正しい。Rgb/Bgr を取り違えて
///   いると赤と青が入れ替わる
/// - 右下からはみ出した矩形が画面内の分だけ描かれる: 切り詰めが効いている
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

    // 右下からわざとはみ出させる。切り詰めが効いていれば画面内に収まる部分だけが
    // 描かれ、効いていなければ範囲外へ書き込んで #PF になるか、無関係なメモリを壊す。
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
/// - 印字可能な ASCII が 3 行すべて欠けずに並ぶ: グリフテーブルの検索とビットの並びが
///   正しい。左右が反転していれば字形が鏡像になる
/// - 日本語が代替グリフ（U+FFFD）として描かれる: 未収録文字のフォールバックが効いて
///   いる。日本語を収録した時点でここが本来の字形に変わる
/// - 右端から始まる行が画面内に収まる分だけ描かれる: 文字単位の切り詰めが効いている
///
/// ヒープ初期化より前に呼ばれるので、動的な文字列は組み立てられない。静的な文字列
/// だけで確認できる内容にしてある。
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
/// 確保に失敗したら `None` を返し、停止せずに続行する。画面が出なくなるだけで、
/// シリアルログという観測手段は失われないため。失敗の内訳（要求フレーム数・空き
/// フレーム総数・最大連続空き範囲）をログに出し、「空き自体が不足」と「空きはあるが
/// 連続領域が足りない（断片化）」を判別できるようにする。
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

    let base = start_frame.as_u64();
    let end = base + frames_needed * frame_allocator::FRAME_SIZE;

    // M2 以来の不変条件: 使う領域は必ずマップ済みであることを確かめてから触る。
    if !range_is_mapped(mapped_ranges, base, end) {
        logger.error(format_args!(
            "console: back buffer {base:#x}..{end:#x} is not fully mapped; \
             screen output is disabled"
        ));
        logger.info(format_args!("console: serial logging continues unaffected"));
        return None;
    }

    // バックバッファは物理フレームから切り出したもので、変換は direct map を通す
    // （T-2c で frame_allocator が PhysAddr を返すようになれば、この分岐は消える）。
    let base_virt = common::addr::direct_map().phys_to_virt(start_frame);
    // SAFETY: base..end は今確保したばかりで他の誰も使っておらず、直前に
    // contains_range でマップ済みを確認した。framebuffer は init_framebuffer が検証済み
    // の形状で作ったもので、所有権をここへ移している（同じ領域に対する Framebuffer は
    // 他に存在しない）。
    match unsafe { Console::new(framebuffer, base_virt, FOREGROUND, BACKGROUND) } {
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
/// 必ずシリアルを先に書く。画面側で何が起きてもシリアルログだけは残るようにするため。
/// 順序を入れ替えると、コンソールの不具合がシリアルログを道連れにできる構造になり、
/// シリアルを唯一の信頼できる観測手段とする方針（ADR-0003、ADR-0017 の決定 9）が崩れる。
///
/// `Logger` 自体には複数の出力先を持たせない。マルチシンクにするとシリアル出力の経路が
/// 画面出力の経路に依存する（ADR-0017）。
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
/// 画面だけを見た人が「ログが途中から始まっている」と誤解しないよう、ここより前の
/// ログはシリアルにしか出ていないことと、その行数を明示する。
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

/// 稼働中の GDT のディスクリプタを 1 本ずつ読み戻し、期待値と照合する（M5-e-1）。
///
/// カーネルコード/データに加え、M5-e で足したユーザー用（ucode32 / udata / ucode64、
/// DPL=3、STAR 互換順）と TSS を確認する。Ring 3 遷移はまだ行わない（M5-e-3）。
/// 読むのは GDT が今持っている値で、設計上の仮定ではなく実状態を見る。
fn verify_gdt_descriptors(logger: &mut Logger<SerialPort>, gdt_limit: u16) {
    use gdt::layout::{
        user_segment_descriptor, KERNEL_CODE_ACCESS, KERNEL_CODE_FLAGS, KERNEL_DATA_ACCESS,
        KERNEL_DATA_FLAGS, USER_CODE32_FLAGS, USER_CODE64_FLAGS, USER_CODE_ACCESS,
        USER_DATA_ACCESS, USER_DATA_FLAGS,
    };

    // limit は「サイズ - 1」。並びを変えて枠が増えた分、値も変わる。
    let expected_limit = gdt::expected_gdt_limit();
    logger.info(format_args!(
        "gdt: limit={gdt_limit} (expected {expected_limit})"
    ));

    // 8 バイトのコード/データディスクリプタ 5 本。期待値は稼働中の GDT を組んだのと
    // 同じ layout 関数から作る。
    let entries: [(&str, usize, u64, u8); 5] = [
        (
            "kcode",
            gdt::KERNEL_CODE_INDEX as usize,
            user_segment_descriptor(KERNEL_CODE_ACCESS, KERNEL_CODE_FLAGS),
            0,
        ),
        (
            "kdata",
            gdt::KERNEL_DATA_INDEX as usize,
            user_segment_descriptor(KERNEL_DATA_ACCESS, KERNEL_DATA_FLAGS),
            0,
        ),
        (
            "ucode32",
            gdt::USER_CODE32_INDEX as usize,
            user_segment_descriptor(USER_CODE_ACCESS, USER_CODE32_FLAGS),
            3,
        ),
        (
            "udata",
            gdt::USER_DATA_INDEX as usize,
            user_segment_descriptor(USER_DATA_ACCESS, USER_DATA_FLAGS),
            3,
        ),
        (
            "ucode64",
            gdt::USER_CODE64_INDEX as usize,
            user_segment_descriptor(USER_CODE_ACCESS, USER_CODE64_FLAGS),
            3,
        ),
    ];

    // CPU はセグメントセレクタをロードすると、そのディスクリプタの Accessed ビット
    // （アクセスバイトの bit 0 = ディスクリプタの bit 40）を 1 にする。kdata は起動時に
    // DS/ES/SS/FS/GS へロード済みなので、実状態では Accessed が立ち、書き込んだ値
    // （0x92）と食い違う（0x93）。DPL/type/並びとは別の CPU 管理のビットなので照合から
    // 除外する。ユーザー用はまだどのレジスタにもロードしていない（Ring 3 は M5-e-3）
    // ので Accessed は 0 のままで、書き込んだ値と一致する。
    const ACCESSED: u64 = 1 << 40;

    let mut all_ok = expected_limit == gdt_limit;
    for (name, index, expected, expected_dpl) in entries {
        let loaded = gdt::loaded_descriptor(index);
        let dpl = ((loaded >> 45) & 0b11) as u8;
        let present = (loaded >> 47) & 1;
        let accessed = (loaded >> 40) & 1;
        // S（bit 44）が 1 ならコード/データ。Executable（bit 43）でどちらかが決まる。
        let user_segment = (loaded >> 44) & 1;
        let executable = (loaded >> 43) & 1;
        let kind = if user_segment == 0 {
            "system"
        } else if executable == 1 {
            "code"
        } else {
            "data"
        };
        logger.info(format_args!(
            "gdt[{index}] {name}: {loaded:#018x} DPL={dpl} present={present} kind={kind} \
             accessed={accessed} (expected {expected:#018x}, accessed bit is CPU-managed)"
        ));
        let matches = (loaded & !ACCESSED) == (expected & !ACCESSED) && dpl == expected_dpl;
        // 破壊 (ring3-test-user-desc-dpl0): ucode64 の DPL を 0 に落とすので、この
        // 読み戻しで先に halt させない。遠征の runtime で iretq の #GP として捕まえる
        // （M5-e-1 の申し送り）。この feature のときだけ ucode64 を素通しにする。
        #[cfg(feature = "ring3-test-user-desc-dpl0")]
        let matches = matches || name == "ucode64";
        all_ok &= matches;
    }

    // udata の D/B を、設計の仮定ではなく稼働中の kdata の実バイトへ揃えたことの確認。
    // ロングモードでデータの D/B は無視されうるが、既知値照合が「kdata と同じ」を
    // 前提にしているので、両者の D/B（ディスクリプタの bit 54）の一致を実状態で見る。
    let loaded_kdata = gdt::loaded_descriptor(gdt::KERNEL_DATA_INDEX as usize);
    let loaded_udata = gdt::loaded_descriptor(gdt::USER_DATA_INDEX as usize);
    let kdata_db = (loaded_kdata >> 54) & 1;
    let udata_db = (loaded_udata >> 54) & 1;
    logger.info(format_args!(
        "gdt: kdata D/B={kdata_db}, udata D/B={udata_db} (must match; udata follows kdata)"
    ));
    all_ok &= kdata_db == udata_db;

    // TSS は 16 バイトのシステムディスクリプタ（index 6-7）。base が実体を指し、
    // S=0（システム）・present であることを確かめる。type は available TSS（0x9）と
    // して書くが、ltr がロード時に busy ビットを立てて 0xB にする。Accessed と同じく
    // CPU 管理のビットなので両方を許容する（違いは bit 1 のみ）。
    let tss_low = gdt::loaded_descriptor(gdt::TSS_SELECTOR.index() as usize);
    let tss_present = (tss_low >> 47) & 1;
    let tss_system = (tss_low >> 44) & 1; // S ビット。TSS は 0。
    let tss_type = ((tss_low >> 40) & 0xF) as u8;
    let tss_base_lo = ((tss_low >> 16) & 0xFF_FFFF) | (((tss_low >> 56) & 0xFF) << 24);
    let tss_high = gdt::loaded_descriptor(gdt::TSS_SELECTOR.index() as usize + 1);
    let tss_base = tss_base_lo | ((tss_high & 0xFFFF_FFFF) << 32);
    logger.info(format_args!(
        "gdt[{}] tss: base={tss_base:#x} (expected {:#x}) present={tss_present} S={tss_system} \
         type={tss_type:#x} (0xb = busy, set by ltr)",
        gdt::TSS_SELECTOR.index(),
        gdt::tss_base()
    ));
    let tss_type_ok = tss_type == 0x9 || tss_type == 0xB;
    let tss_ok = tss_present == 1 && tss_system == 0 && tss_type_ok && tss_base == gdt::tss_base();
    all_ok &= tss_ok;

    if !all_ok {
        logger.error(format_args!(
            "gdt: a descriptor read-back did not match the expected value (DPL/type/limit/order); \
             halting"
        ));
        cpu::halt_forever();
    }
    logger.info(format_args!(
        "gdt: all descriptors match (kernel + user DPL=3 in STAR-compatible order, TSS at new index)"
    ));
}

/// GDT / TSS / スタック切り替えの結果をログに残す（M4-a）。
///
/// M2-d の CR3 切り替えと同じ作法で、切り替え後に読み戻した値を出す。設定したつもりの
/// 値ではなく、CPU が今参照している値を確認する。
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

    verify_gdt_descriptors(logger, gdt_limit);

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
        kernel_stack.bottom.as_u64(),
        kernel_stack.top.as_u64(),
        kernel_stack.size() / 1024
    ));
    logger.info(format_args!(
        "stack: double fault (IST{}) stack {:#x}..{:#x} ({} KiB)",
        gdt::DOUBLE_FAULT_IST_INDEX,
        double_fault_stack.bottom.as_u64(),
        double_fault_stack.top.as_u64(),
        double_fault_stack.size() / 1024
    ));

    // 現在のスタックポインタが自前の領域にあること。
    let on_own_stack = kernel_stack
        .contains(common::addr::VirtAddr::new(current_rsp).expect("a stack address is canonical"));
    logger.info(format_args!(
        "stack: RSP is inside the kernel stack: {on_own_stack}"
    ));

    // ローカル変数の置き場所も自前スタック上にあること。RSP だけでなく、コンパイラが
    // 使う退避先も移っていることの確認になる。
    let probe = 0xA5A5_5A5Au32;
    let probe_address = core::ptr::addr_of!(probe) as u64;
    let locals_on_own_stack = kernel_stack.contains(
        common::addr::VirtAddr::new(probe_address).expect("a stack address is canonical"),
    );
    logger.info(format_args!(
        "stack: locals live at {probe_address:#x}, inside the kernel stack: {locals_on_own_stack}"
    ));

    // 旧スタックを参照していないこと。
    let left_old_stack = !kernel_stack
        .contains(common::addr::VirtAddr::new(old_rsp).expect("a stack address is canonical"))
        && current_rsp != old_rsp;
    logger.info(format_args!(
        "stack: no longer using the UEFI-derived stack: {left_old_stack}"
    ));

    // 書き込めること（M2-d のスタック検証と同じ考え方）。現在の RSP より下
    // （未使用側）へ直接読み書きする。
    let scratch = (current_rsp - 256) as *mut u64;
    let scratch_ok = if kernel_stack.contains(
        common::addr::VirtAddr::new(scratch as u64).expect("a stack address is canonical"),
    ) {
        // SAFETY: scratch は現在の RSP より 256 バイト下で、直前の
        // `kernel_stack.contains` でカーネルスタックの範囲内を確認済み。まだ誰も
        // 使っておらず、赤ゾーン（128 バイト）より外側でもある。触るのは 8 バイトのみ。
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

    // カーネルスタックの直下はガードページ（起動シーケンスの中で unmap する）なので
    // カナリアを読まない。IST スタックの犠牲領域だけを見る。
    let guards_ok = stack::guards_intact();
    logger.info(format_args!(
        "stack: IST guards intact (double-fault={}, page-fault={})",
        stack::double_fault_guard_intact(),
        stack::page_fault_guard_intact()
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

/// クリティカルセクション（`InterruptGuard`）の入れ子を実機で検証する
/// （M4-c-1）。
///
/// 各時点の IF は、設定したつもりの値ではなく実際の RFLAGS から読む。M4-c の時点では
/// まだ `sti` していない（起動時から IF=0）ので、ここで観測できるのは「入れ子で余計に
/// 有効化されないこと」と「Drop 後に元の状態へ戻ること」である。IF=1 で `enter` する
/// 経路は、PIC を全マスクした M4-c-3 の後に `--critical-test` で確かめる。それ以前に
/// `sti` すると未検証のハンドラへ割り込みが飛ぶ。
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

    // 一番外側の Drop 後。起動時から IF=0 なので保存値も IF=0 で、復元しないことが
    // そのまま元の状態へ戻っていることになる。
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

/// IDT のロード結果をログに残す（M4-b-1）。
///
/// GDT と同じ作法で、設定したつもりの値ではなく `sidt` で読み戻した値を
/// 確認する。
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

    // 全ベクタが present で割り込みゲート（0xE）であること。1 つでも欠けると、その
    // ベクタが発生したときに #NP になり、#NP のハンドラも無ければ落ちる。
    //
    // DPL は「syscall ベクタ（0x80）だけ DPL 3、他は全て DPL 0」であること。DPL=3 は
    // Ring 3 から int 0x80 を呼べる唯一の条件なので名指しで確かめる。他のゲートに
    // DPL 3 が紛れると、そのベクタを Ring 3 から任意に発火できてしまう。
    let mut all_present = true;
    let mut all_interrupt_gates = true;
    let mut dpl_layout_ok = true;
    for vector in 0..idt::IDT_ENTRY_COUNT {
        let Some(entry) = idt::entry(vector) else {
            all_present = false;
            break;
        };
        all_present &= entry.is_present();
        all_interrupt_gates &= entry.gate_type() == 0xE;
        // syscall ベクタの期待 DPL はゲート登録と同じ定数から出す（gate-dpl0 の
        // 破壊では両方が 0 になり、この検査は通って runtime で #GP になる）。
        let expected_dpl = if vector == idt::SYSCALL_VECTOR {
            idt::SYSCALL_GATE_DPL
        } else {
            0
        };
        dpl_layout_ok &= entry.descriptor_privilege_level() == expected_dpl;
    }
    let syscall_dpl = idt::entry(idt::SYSCALL_VECTOR).map(|e| e.descriptor_privilege_level());
    logger.info(format_args!(
        "idt: {} entries, all present={all_present}, all interrupt gates={all_interrupt_gates}, \
         DPL layout ok={dpl_layout_ok} (syscall vector {:#x} DPL={syscall_dpl:?}, others DPL 0)",
        idt::IDT_ENTRY_COUNT,
        idt::SYSCALL_VECTOR,
    ));

    // ダブルフォルトだけが IST を使うこと。
    let double_fault_ist = idt::entry(8).and_then(|e| e.ist_index());
    let divide_error_ist = idt::entry(0).and_then(|e| e.ist_index());
    logger.info(format_args!(
        "idt: #DF (vector 8) IST index={double_fault_ist:?}, #DE (vector 0) IST index={divide_error_ist:?}"
    ));

    if !(all_present && all_interrupt_gates && dpl_layout_ok)
        || double_fault_ist != Some(gdt::DOUBLE_FAULT_IST_INDEX as u8)
        || divide_error_ist.is_some()
    {
        logger.error(format_args!("idt: entry checks failed; halting"));
        cpu::halt_forever();
    }

    // スタブ表の刻み幅と、IDT エントリがそれを指していることを検証する。IDT は
    // base + n * STUB_SIZE でエントリを作るので、この前提が崩れると全エントリが誤った
    // アドレスを指す。同じ式で検算すると循環するので、アセンブラが付けた独立のラベルと
    // 突き合わせる。
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

    // 例外スタブ表の外のベクタが、それぞれの専用スタブを指すこと。
    //
    // 上の検査はこれらのベクタを除外しており、除外したものについては何も見ない。
    // ゲートの代入を落としても既定の例外スタブを指したまま静かに通るので、肯定的な
    // 主張を別に置く。yield と syscall は「戻らない」経路へ落ち、スプリアスは S2-b
    // 以前の「起きたら止まる」状態へ戻る。
    let dedicated = idt::check_dedicated_stubs();
    let dedicated_ok = dedicated.iter().all(idt::DedicatedStubCheck::is_ok);
    logger.info(format_args!(
        "idt: dedicated stubs outside the exception table: {} checked, \
         every gate points at its own stub={dedicated_ok}",
        dedicated.len()
    ));
    if !dedicated_ok {
        for entry in dedicated.iter().filter(|entry| !entry.is_ok()) {
            logger.error(format_args!(
                "idt: vector {:#04x} points at {:#x}, expected its dedicated stub at {:#x}",
                entry.vector, entry.actual_handler, entry.expected_handler
            ));
        }
        logger.error(format_args!(
            "idt: a vector outside the exception stub table does not reach its own stub; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "idt: loaded (exceptions now reach our handlers; interrupts stay disabled until M4-d)"
    ));
}

/// 8259A PIC を 0x20-0x2F へ再マップし、全 IRQ をマスクする（M4-c-3）。
///
/// 再マップ前の IMR も記録する。UEFI が何を開けたまま制御を渡してきたかは読まないと
/// 分からず、後で「誰も設定していないはずの IRQ が来る」と悩んだときの手掛かりになる
/// （OVMF はアイドル中もタイマ割り込みを処理している。`docs/troubleshooting.md` の
/// 起動ログのベースライン）。
fn configure_pic(logger: &mut Logger<SerialPort>) {
    logger.info(format_args!(
        "pic: IMR before remap {} [0 = unmasked]",
        irq::check_masks(&[]).observed_with_bits()
    ));

    // 再マップ先が IDT のカバー範囲にあり、present なハンドラを持つことを、再マップ
    // より先に確かめる。順序が逆だと、検査に落ちても PIC は既に新しいベクタを向いて
    // おり、halt するまでの間に IRQ が届けば行き先の無いベクタへ飛ぶ。
    let (first_vector, last_vector) = irq::managed_vectors();
    let first = first_vector as usize;
    let last = last_vector as usize;
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
    let programming = match unsafe { irq::init() } {
        Ok(programming) => programming,
        Err(error) => {
            logger.error(format_args!(
                "pic: rejected the vector offsets ({error:?}); halting"
            ));
            cpu::halt_forever();
        }
    };

    // ベクタオフセットは書いた値であって、検証した値ではない。ICW2 は書き込み専用で、
    // データポートから読めるのは IMR だけである。断定形で書くと、下のマスク検証が
    // 通ったことをもって再マップ全体が正しいと読めてしまう。
    logger.info(format_args!("pic: programmed {programming}"));

    // 一方 IMR は読める。設定したつもりではなく実際の値を読み戻す。ICW シーケンスが
    // 途中で崩れていると、最後の OCW1 が ICW として解釈されてマスクが掛からない。
    // そのまま M4-d で `sti` すると、ハンドラの無い IRQ がいきなり飛んでくる。
    let after_remap = irq::check_masks(&[]);
    logger.info(format_args!("pic: IMR after remap {after_remap}"));
    if !after_remap.matches() {
        logger.error(format_args!(
            "pic: the mask read-back does not match; halting"
        ));
        cpu::halt_forever();
    }

    // ここは 8259 の採番を問うている。ICW2 に書いたオフセットが効いているかという話
    // なので、現在の配送先ではなく `PIC_TIMER_VECTOR` が正しい。S2-d-2 でタイマが
    // Local APIC へ移ると、この行の前提そのものが変わる。
    logger.info(format_args!(
        "pic: all IRQs masked (nothing can fire until M4-d unmasks the timer explicitly); \
         the vector offset stays unverified until the first timer IRQ arrives as vector \
         {:#04x} in M4-d",
        idt::PIC_TIMER_VECTOR
    ));
}

/// `--exception-test` で各 GPR に入れる既知の値。
///
/// レジスタごとに異なる値にしてあるので、ダンプで名前と値の対応が入れ替わっていれば
/// 一目で分かる。`.bss` のゼロ埋め検証で毒値を使ったのと同じ考え方である。
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
/// naked にしているのは、コンパイラにレジスタを触らせないため。通常の `asm!` では
/// callee-saved レジスタ（rbx, rbp, r12-r15）を書き換えると呼び出し規約を壊すが、
/// ここは戻らないので問題ない。
///
/// RSP には触れない。触ると例外配送そのものが失敗する。
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
/// 実装している物理メモリ（256MiB）からも、フレームバッファや MMIO の窓からも遠い値を
/// 選ぶ。上位ビットが符号拡張された正準アドレスなので、#GP ではなく #PF になる。
///
/// 使う前に `MappedRanges::contains_range` でマップされていないことを確認する。偶然
/// マップされている領域を選ぶと、フォルトが起きずテストが成功したように見える。
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

        // マップされていないことを確認してから使う。マップされていればフォルトが
        // 起きず、テストが通ったように見えてしまう。
        let to_phys = |raw: u64| {
            common::addr::PhysAddr::new(raw).expect("the probe address fits in a physical address")
        };
        let unmapped = !mapped_ranges.contains_range(to_phys(probe), to_phys(probe + 8));
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
            // #PF のゲートを不在にしてからページフォルトを起こす。配送そのものが
            // #NP（Contributory 分類）を引き起こし、「Page Fault の配送中に
            // Contributory」が成立して #DF へ昇格する（ADR-0018）。
            //
            // スタックを溢れさせる方法は使えない。犠牲領域は .bss 内のマップ済み
            // メモリなので、溢れてもページフォルトが起きない。
            logger.info(format_args!(
                "exception-test: clearing the present bit of the #PF gate (vector 14)"
            ));
            // SAFETY: このあと意図的にページフォルトを起こして #DF へ昇格させる
            // テスト経路。ハンドラが停止するので復元は要らない。
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

        // SAFETY: 意図的にページフォルトを起こすテスト経路。直前にマップされて
        // いないことを確認済みで、ハンドラが停止する。
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

/// カーネルスタックを意図的に溢れさせて、ガードページの回帰チェックを行う
/// （M5-b、`stack-guard-test` / `stack-overflow-df-test`）。
///
/// 無限再帰で RSP を下げ続け、ガードページ（unmap 済み）に触れる。正常な構成
/// （#PF に IST2）では #PF、CR2 = ガードページ、on IST2=true として報告される。
/// `stack-overflow-df-test`（#PF に IST 無し）では、溢れたスタックの上で #PF を
/// 配送しようとしてさらに #PF が起き、#DF へ昇格する。
///
/// 通常ビルドには含まれない。
#[cfg(feature = "stack-guard-test")]
#[allow(unreachable_code)]
fn trigger_stack_guard_test(logger: &mut Logger<SerialPort>) -> ! {
    let guard = stack::kernel_guard_page();
    logger.info(format_args!(
        "stack-guard-test: kernel stack guard page {:#x}..{:#x}; about to overflow the stack on purpose",
        guard.bottom.as_u64(),
        guard.top.as_u64()
    ));
    #[cfg(feature = "stack-overflow-df-test")]
    logger.info(format_args!(
        "stack-guard-test: #PF has no IST in this build; expecting escalation to #DF"
    ));
    #[cfg(not(feature = "stack-overflow-df-test"))]
    logger.info(format_args!(
        "stack-guard-test: expecting #PF (vector 14) on IST2 with CR2 in the guard page"
    ));

    // 溢れさせる。戻り値と volatile で末尾呼び出し最適化を潰し、各段が実際に
    // スタックフレームを積むようにする。
    let sink = overflow_the_stack(0);
    // 到達しない。最適化で溢れごと消えないよう、結果を使う。
    logger.error(format_args!(
        "stack-guard-test: the recursion returned ({sink:#x}); the guard did not fire; halting"
    ));
    cpu::halt_forever();
}

/// スタックを溢れさせるための無限再帰。各段が 64 バイトのローカルを積み、volatile で
/// 最適化に消されないようにする。`#[inline(never)]` で確実にフレームを作る。
#[cfg(feature = "stack-guard-test")]
#[inline(never)]
// 意図的な無限再帰。ガードページに触れて #PF/#DF になるまで戻らない。
#[allow(unconditional_recursion)]
fn overflow_the_stack(depth: u64) -> u64 {
    let mut frame = [depth; 8];
    // SAFETY: frame はこの関数の局所配列。volatile で読み書きするのは、末尾呼び出し
    // 最適化とデッドコード除去を防いで実フレームを積むため。
    unsafe {
        core::ptr::write_volatile(&mut frame[0], depth);
    }
    // SAFETY: frame[0] は今書き込んだ局所配列の要素。volatile 読みは最適化に消されない。
    let next = unsafe { core::ptr::read_volatile(&frame[0]) }.wrapping_add(1);
    let deeper = overflow_the_stack(next);
    // SAFETY: 同上。戻り値とローカルの両方を使い、再帰を末尾化させない。
    deeper.wrapping_add(unsafe { core::ptr::read_volatile(&frame[7]) })
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

        // ヒープのロックではなく専用の Locked を使う。ヒープを壊すと以降のログ出力
        // まで巻き添えになるため。検出の仕組みは同じ Locked<T> の実装である。
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
        // IF=1 で enter した場合の復元経路。PIC を全マスクしてからでないと危険で
        // ある（未検証のハンドラへ割り込みが飛ぶ）。M4-c-3 で PIC のマスクを
        // 確認したうえで実行する。
        logger.info(format_args!(
            "critical-test: enabling interrupts temporarily to exercise the restore path"
        ));
        // SAFETY: PIC は全 IRQ マスク済みで、IDT の全 256 ベクタにハンドラが入って
        // いる（M4-b-1）。この区間で割り込みが届いても「予期しないベクタ」として
        // 報告されるだけで、無言では落ちない。
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
/// `Locked<T>` は取得中に `InterruptGuard` を保持する。その効果を実 RFLAGS で観測する。
/// 現状は起動時から IF=0 なので、保持中も解放後も IF=0（元の状態）になる。IF=1 から
/// 入る経路は `--critical-test restore-enabled` で確認する。
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
    /// TSC の周波数は環境依存で、時刻源として信用できない（`cpu` モジュール）。
    /// ここでは回る量の目安として使うだけなので、絶対時間の正確さは要らない。
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
        /// TCG で `-d int` を有効にすると 1 ティックあたり 20 行強が記録される。
        /// 500 ティックで約 1 万行・800KB 程度に収まる（M4-b-1 のログが 14,400 行
        /// だったので同程度）。通常起動では止めずに回し続ける。
        const STOP_AFTER_TICKS: u64 = 500;
        start_timer(logger, console, STOP_AFTER_TICKS, 0, None);
    }

    logger.info(format_args!(
        "interrupt-test: all pre-sti checks passed (or are explicitly unverifiable); enabling interrupts"
    ));

    // SAFETY: 直前に 7 項目を検証し、blocks_sti() な項目が無いことを確認した。
    // 全 IRQ はマスク済みで、IDT の全 256 ベクタに present なハンドラが入っている。
    unsafe {
        interrupts::spin_with_interrupts_enabled(logger, SPIN_CYCLES, HEARTBEAT_CYCLES);
    }

    // 増加分で判定する。テスト用ベクタを PIC の範囲外へ移しても同じである。カウンタは
    // 全 256 ベクタの合計なので、irq-path の `int 0x40` で計上された 1 件が絶対値に
    // 残る。0x20 が汚れなくなっただけで、絶対値では「スピン中に届いた」と誤判定する
    // 構造は変わらない。
    //
    // 合計の対象を PIC の範囲だけに絞る案は採らない。ここで見たいのは「何も届かない
    // こと」で、`cli` でマスクできない NMI（ベクタ 2）を含む全ベクタが対象である。
    let delta = interrupts::spin_interrupt_delta();
    let (absolute_total, _) = idt::interrupt_total_and_first_nonzero();
    let iterations = interrupts::loop_iterations();
    logger.info(format_args!(
        "interrupt-test: spin finished; loop iterations={iterations}, \
         interrupts during the spin={delta} (absolute total since boot={absolute_total})"
    ));

    // 周回回数も判定に含める。0 回なら「割り込みが来なかった」ではなく「ループが
    // 回っていない」で、意味がまるで違う。
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
/// 使うのは PIC の範囲外のベクタ 0x40（`idt::TEST_VECTOR`）である。`int 0x40` は 8259A を
/// 経由せず CPU が直接 IDT を引くので、マスク状態と無関係にハンドラ経路だけを試せ、
/// EOI の論理が絡まない。PIC 経由で配送されないベクタなので、ハンドラが EOI を送らない
/// ことがそのまま正しい実装になる。
///
/// 各 GPR にレジスタごとに異なる既知値を入れ、`int` の前後で一致することを見る。
/// 1 本でも復元を落とすと、そのレジスタだけ値が変わる。
///
/// # なぜ 0x20 ではなく PIC の範囲外を使うのか
///
/// M4-d-1 では `int 0x20` を使っていたが、M4-d-2 で EOI を実装すると衝突する。
/// ソフトウェア割り込みは実在の IRQ ではないので、タイマハンドラが無条件に EOI を送る
/// 作りだと起きてもいない割り込みに応答することになり、PIC の優先度スタックを壊しうる。
///
/// 検討した代替案:
///
/// - 実タイマでの検証に置き換える: 却下。「GPR が壊れた」ことは分かるが、壊れたのが
///   スタブか PIT 設定か EOI かを切り分けられない。ハンドラ経路だけを単独で試せると
///   いう、この検証の価値そのものが失われる。
/// - ハンドラ側でソフトウェア割り込み由来かを判別して EOI を抑制する: 却下。本番経路に
///   テスト専用の分岐が入るうえ、判別を誤れば本物の割り込みへ EOI を送らない側へ倒れ、
///   以降の割り込みが全部止まる。テストのために本番経路の信頼性を下げることになる。
///
/// PIC の範囲外へ移すのが、本番経路に一切手を入れずに済む唯一の案だった
/// （ADR-0018 Addendum 3）。
#[cfg(feature = "interrupt-test-irq-path")]
fn verify_irq_path_restores_registers(logger: &mut Logger<SerialPort>) {
    // レジスタごとに異なる既知値。値が入れ替わっても気づけるようにする
    // （M4-b-2 の GPR ダンプ検証と同じ考え方）。
    //
    // rbx と rbp は検査できない。LLVM がこの 2 本を内部的に予約しており、`asm!` の
    // オペランドに指定できない。検査できるのは残る 13 本である。順序の取り違えは
    // 13 本の相異なる値で捕まり、本数の過不足は RSP がずれて `iretq` の時点で壊れる
    // ので、この 2 本が抜けても検査の意味は保たれる。
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
    // check_irq_stub_table で検証済み）、そのスタブは GPR を退避・復元して `iretq` で
    // 戻る。割り込みゲートなので入場時に IF はクリアされ、戻るときに復元される。
    // `nostack` は付けない（ハンドラがスタックを使う）。
    unsafe {
        core::arch::asm!(
            // idt::TEST_VECTOR と同じ値。`int` のオペランドは即値でなければならず
            // 定数を差し込めないので、ここだけ数値が重複する。食い違いは下の
            // const アサーションで防ぐ。
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

/// デモのアドレス空間が使うユーザーサブツリーの添字（S7-e）。
///
/// `DEMO_VIRT` の添字と一致していなければならない。一致しないと `map_user_4kib` が
/// `NotPrivate` で弾く。弾くのが正しい。別の添字へ張れば、その空間の監査の主張が破れる。
///
/// 本番の `USER_PML4_INDEX` とは別でよい。プロセスごとにアドレス空間が分かれる以上、
/// ユーザーサブツリーの添字も空間ごとの性質である（S7-e）。
const DEMO_USER_PML4_INDEX: usize = 0;

/// アドレス空間を1つ作り、切り替えて、戻す（S7-c）。
///
/// 主張は「上位を共有していれば、CR3 を差し替えてもカーネルは動き続ける」である。
/// 切り替えた後にこの関数がログを出せること自体が証拠になる。命令フェッチもスタックも
/// direct map も、新しい CR3 の下で引き続き翻訳できているということである。
///
/// 破壊 (addrspace-no-kernel-share): 上位を写さない。切り替えた瞬間に死ぬので、
/// 「切り替えた後」の行が出ない（S7-c）。
fn demo_address_space_switch(
    logger: &mut Logger<SerialPort>,
    allocator: &mut kernel::frame_allocator::FrameAllocator,
) {
    let direct_map = common::addr::direct_map();
    let production = kernel::paging::switch::read_cr3();

    // SAFETY: 稼働中の PML4 を読み、direct map が覆っている新しいフレームへ写すだけ。
    // AP はまだ起きておらず、他コアが写像を変えることはない。
    let space = match unsafe {
        kernel::address_space::AddressSpace::new(
            allocator,
            direct_map,
            production,
            DEMO_USER_PML4_INDEX,
        )
    } {
        Ok(space) => space,
        Err(error) => {
            logger.error(format_args!(
                "address-space: could not build a second address space ({error:?}); halting"
            ));
            cpu::halt_forever();
        }
    };

    logger.info(format_args!(
        "address-space: built a second address space (pml4={:#x}); the production one is {:#x}; \
         about to switch",
        space.pml4().as_u64(),
        production.as_u64()
    ));

    // SAFETY: 上位 256 本を写してあるので、実行中のコード・スタック・direct map は
    // 同じ物理を指し続ける。下位は空だが、この区間ではユーザー空間へ触らない。
    unsafe { space.activate() };

    // この行が出ること自体が到達条件4の観測である。
    let after = kernel::paging::switch::read_cr3();
    logger.info(format_args!(
        "address-space: still running after the switch (cr3 read back = {:#x}, expected {:#x}, \
         matches={})",
        after.as_u64(),
        space.pml4().as_u64(),
        after.as_u64() == space.pml4().as_u64()
    ));

    // SAFETY: 本番のテーブルへ戻すだけ。こちらは起動以来使っているものである。
    unsafe { kernel::paging::switch::switch_to(production) };

    let restored = kernel::paging::switch::read_cr3();
    logger.info(format_args!(
        "address-space: switched back to the production table (cr3 read back = {:#x}, \
         matches={})",
        restored.as_u64(),
        restored.as_u64() == production.as_u64()
    ));

    // === S7-d: 2 つの空間で同じ VA が別の物理を指すこと（到達条件 1・2・6）===
    demo_two_address_spaces(logger, allocator, direct_map, production, space);
}

/// 2 つのアドレス空間を作り、同じ VA を別の物理へ張って読み分ける（S7-d）。
///
/// 到達条件 1（同じ VA が別の物理を指す）・2（A の書き込みが B から見えない）・
/// 6（共有カーネル部分が一致する）・5（破棄後に古い翻訳で触れない）の観測である。
///
/// ユーザーモードへは行かない。張るのはユーザーページだが、読むのは Ring 0 からである。
/// 権限の検査は S8 以降の仕事で、ここで見たいのは翻訳が別であることだけである。
fn demo_two_address_spaces(
    logger: &mut Logger<SerialPort>,
    allocator: &mut kernel::frame_allocator::FrameAllocator,
    direct_map: common::addr::DirectMap,
    production: common::addr::PhysAddr,
    mut space_a: kernel::address_space::AddressSpace,
) {
    use kernel::address_space::{is_shared_kernel_index, AddressSpace, PML4_ENTRY_COUNT};
    use kernel::paging::active::PageAttributes;

    // 下位の、どのデモとも重ならない VA。
    //
    // 添字は 0 である（`0x1_0000_0000 >> 39 == 0`）。S7-d の時点では「PML4[2] は
    // 誰も使っていない」と書いていたが、算が誤っていた。通ったのは恒等除去（B-2b）で
    // PML4[0] が空いていたからであって、書いてあった理由によるのではない。
    // S7-e で訂正した。
    const DEMO_VIRT: u64 = 0x1_0000_0000;
    const VALUE_A: u64 = 0xAAAA_AAAA_AAAA_AAAA;
    const VALUE_B: u64 = 0xBBBB_BBBB_BBBB_BBBB;

    let Some(virt) = common::addr::VirtAddr::new(DEMO_VIRT) else {
        logger.error(format_args!(
            "address-space: the demo VA is not canonical; halting"
        ));
        cpu::halt_forever();
    };

    // SAFETY: 稼働中の PML4 を読み、direct map が覆う新しいフレームへ写すだけ。
    let mut space_b =
        match unsafe { AddressSpace::new(allocator, direct_map, production, DEMO_USER_PML4_INDEX) }
        {
            Ok(space) => space,
            Err(error) => {
                logger.error(format_args!(
                    "address-space: second space failed ({error:?}); halting"
                ));
                cpu::halt_forever();
            }
        };

    let (Some(frame_a), Some(frame_b)) = (allocator.allocate_frame(), allocator.allocate_frame())
    else {
        logger.error(format_args!(
            "address-space: no frames for the demo; halting"
        ));
        cpu::halt_forever();
    };

    // direct map 越しに既知の値を置く。ユーザー VA からではなく物理から書く。
    for (frame, value) in [(frame_a, VALUE_A), (frame_b, VALUE_B)] {
        let ptr = direct_map.phys_to_virt(frame).as_u64() as *mut u64;
        // SAFETY: いま取ったフレームで、direct map が覆っている。誰も使っていない。
        unsafe { ptr.write_volatile(value) };
    }

    for (space, frame) in [(&mut space_a, frame_a), (&mut space_b, frame_b)] {
        // S9-b-1: 渡す値は従来と同じ writable=true なので振る舞いは変わらない。
        let attributes = PageAttributes {
            user: true,
            writable: true,
            cacheable: true,
        };
        // SAFETY: どちらもまだ稼働していない。direct map は覆っている。
        if let Err(error) =
            unsafe { space.map_user_4kib(allocator, direct_map, virt, frame, attributes) }
        {
            logger.error(format_args!(
                "address-space: mapping failed ({error:?}); halting"
            ));
            cpu::halt_forever();
        }
    }

    logger.info(format_args!(
        "address-space: the same VA {DEMO_VIRT:#x} is backed by {:#x} in A and {:#x} in B \
         (different={})",
        frame_a.as_u64(),
        frame_b.as_u64(),
        frame_a.as_u64() != frame_b.as_u64()
    ));

    // 到達条件 1・2: 切り替えて読み分ける。
    let ptr = DEMO_VIRT as *const u64;
    // SAFETY: 上位を共有しているので切り替えてもカーネルは動く。読むのは張った VA。
    let read_a = unsafe {
        space_a.activate();
        ptr.read_volatile()
    };
    // SAFETY: 同上。
    let read_b = unsafe {
        space_b.activate();
        ptr.read_volatile()
    };
    // SAFETY: 本番のテーブルへ戻す。
    unsafe { kernel::paging::switch::switch_to(production) };

    logger.info(format_args!(
        "address-space: read {read_a:#x} in A and {read_b:#x} in B (A sees only its own={}, \
         B sees only its own={})",
        read_a == VALUE_A,
        read_b == VALUE_B
    ));

    // 到達条件 6: 共有カーネル部分が 3 つのテーブルで一致すること。
    let mut shared_mismatches = 0usize;
    for index in 0..PML4_ENTRY_COUNT {
        if !is_shared_kernel_index(index) {
            continue;
        }
        // SAFETY: いずれも direct map が覆う稼働可能な PML4。添字は 512 未満。
        let (p, a, b) = unsafe {
            (
                kernel::paging::verify::read_pml4_entry(production, direct_map, index),
                kernel::paging::verify::read_pml4_entry(space_a.pml4(), direct_map, index),
                kernel::paging::verify::read_pml4_entry(space_b.pml4(), direct_map, index),
            )
        };
        if p != a || p != b {
            shared_mismatches += 1;
        }
    }
    logger.info(format_args!(
        "address-space: shared kernel PML4 entries compared across 3 tables: mismatches={shared_mismatches} \
         (expected 0)"
    ));

    // U/S の監査を、それぞれの空間について行う（S7-e）。
    //
    // 主張が言い換わっている。単一アドレス空間のときは「U=1 はユーザーサブツリーの外に
    // 一切存在しない」という大域の主張だった。プロセスごとになると、どの空間について
    // 言っているかが付いて回る。
    //
    // 添字は空間が持っているので、ここから渡していない。
    for (label, space) in [("A", &space_a), ("B", &space_b)] {
        // SAFETY: どちらも direct map が覆う、稼働可能な PML4 である。読み取りのみ。
        let audit = unsafe { space.audit_user_supervisor(direct_map) };
        logger.info(format_args!(
            "address-space: U/S audit of {label} (user subtree PML4[{}]): user entries={} \
             violations(U=0)={}, kernel entries={} violations(U=1)={}",
            space.user_pml4_index(),
            audit.user_entries,
            audit.user_violations,
            audit.kernel_entries,
            audit.kernel_violations
        ));
        if audit.user_violations != 0 || audit.kernel_violations != 0 {
            logger.error(format_args!(
                "address-space: the U/S audit of {label} found violations; halting"
            ));
            cpu::halt_forever();
        }
    }

    // 到達条件 5 の機構: 破棄して隔離へ入れ、退くまで配られないこと。
    let mut quarantine = kernel::quarantine::Quarantine::new();
    let generation_before = kernel::bkl::tlb_generation();
    let free_before = allocator.free_frame_count();
    let (held, leaked) = {
        let guard = kernel::bkl::acquire(kernel::bkl::KernelEntry::SteadyLoop);
        // SAFETY: A はいま稼働していない（本番へ戻してある）。BKL を保持している。
        unsafe { space_a.destroy(direct_map, &mut quarantine, &guard) }
    };
    let free_after_destroy = allocator.free_frame_count();
    logger.info(format_args!(
        "address-space: destroyed A. generation {generation_before} -> {}, quarantined={held} \
         leaked={leaked}, allocator free {free_before} -> {free_after_destroy} (unchanged={})",
        kernel::bkl::tlb_generation(),
        free_before == free_after_destroy
    ));

    // まだ退いていない。このコアの `SEEN_GENERATION` は次の `acquire` で進む。
    let released_now = quarantine.release_retired(allocator, kernel::bkl::generation_is_retired);
    // 1 度 BKL を取れば、このコアは新しい世代を見る。単一コアなのでこれで退く。
    drop(kernel::bkl::acquire(kernel::bkl::KernelEntry::SteadyLoop));
    let released_after = quarantine.release_retired(allocator, kernel::bkl::generation_is_retired);
    logger.info(format_args!(
        "address-space: quarantine released {released_now} before the cores caught up and \
         {released_after} after; still held={}, allocator free {free_after_destroy} -> {}",
        quarantine.held_count(),
        allocator.free_frame_count()
    ));

    // B は生かしたままにしない。同じ経路で片付ける。
    let (held_b, leaked_b) = {
        let guard = kernel::bkl::acquire(kernel::bkl::KernelEntry::SteadyLoop);
        // SAFETY: B も稼働していない。BKL を保持している。
        unsafe { space_b.destroy(direct_map, &mut quarantine, &guard) }
    };
    drop(kernel::bkl::acquire(kernel::bkl::KernelEntry::SteadyLoop));
    let released_b = quarantine.release_retired(allocator, kernel::bkl::generation_is_retired);
    logger.info(format_args!(
        "address-space: destroyed B too (quarantined={held_b} leaked={leaked_b} released={released_b}); \
         quarantine now holds {} with {} overflow(s); allocator free {}",
        quarantine.held_count(),
        quarantine.overflow_count(),
        allocator.free_frame_count()
    ));
}

/// PIT を設定し、IRQ0 を解禁してタイマを動かす（M4-d-2）。
///
/// # 割り込みを有効化するまでの順序
///
/// 設定中に割り込みが飛び込む余地を作らないため、順序を固定している。
///
/// 1. PIT を設定する（この時点で IRQ0 はマスクされたまま）
/// 2. IMR を読み戻し、まだ全マスクのままであることを確認する。PIT の設定が誤って IMR を
///    触っていないことの確認。ポート 0x21（IMR）と 0x40/0x43（PIT）は番号が近く、
///    定数の書き間違いが起こりうる
/// 3. IRQ0 のマスクを解除する（解禁はこの 1 箇所のみ）
/// 4. IMR を読み戻し、master=0xFE / slave=0xFF を照合する
/// 5. `sti` 前 7 項目を再検証する（項目 5 の期待値が 0xFF から 0xFE へ変わる）
/// 6. `sti`（`run_timer_loop` の中で行う）
fn start_timer(
    logger: &mut Logger<SerialPort>,
    console: Option<&mut Console>,
    stop_after_ticks: u64,
    shell_after_heartbeats: u64,
    apic: Option<&kernel::apic::MappedApic>,
) {
    // --- 1. PIT を設定する ---
    // SAFETY: 起動時に 1 回だけ。この時点で IRQ0 はマスクされている
    // （M4-c-3 の remap が全マスクで終わり、以降解除していない）。
    let timer = match unsafe { irq::configure_timer(irq::timer_frequency_hz()) } {
        Ok(timer) => timer,
        Err(error) => {
            logger.error(format_args!(
                "pit: refused the requested frequency ({error:?}); halting"
            ));
            cpu::halt_forever();
        }
    };
    logger.info(format_args!("pit: channel 0 set to {timer}"));

    // --- 2. PIT の設定が IMR を壊していないことを確認する ---
    let before_unmask = irq::check_masks(&[]);
    logger.info(format_args!(
        "pit: IMR after configuring the PIT {} \
         (must still be 0xff/0xff; the PIT ports must not touch the IMR)",
        before_unmask.observed()
    ));
    if !before_unmask.matches() {
        logger.error(format_args!(
            "pit: configuring the PIT changed the interrupt mask; halting"
        ));
        cpu::halt_forever();
    }

    // --- 3. IRQ0 を解禁する（ここが唯一の解禁箇所）---
    // SAFETY: ベクタ 0x20 には IRQ スタイルのスタブが入っており（起動時に
    // check_irq_stub_table で検証済み）、ハンドラは EOI を発行する。
    unsafe {
        irq::unmask(0);
    }

    // --- 4. 解禁の結果を読み戻す ---
    let after_unmask = irq::check_masks(&[0]);
    logger.info(format_args!("pic: IMR after unmasking IRQ0 {after_unmask}"));
    if !after_unmask.matches() {
        logger.error(format_args!(
            "pic: the mask read-back after unmasking IRQ0 does not match; halting"
        ));
        cpu::halt_forever();
    }

    // --- 4.5 キーボード（IRQ1）を用意する ---
    setup_keyboard(logger);

    // --- 4.6 キーボードの配送を I/O APIC 経由へ移す（S2-d-1c）---
    //
    // `sti` より前に切り替え終える。割り込みが有効な状態で切り替えると、4 手の途中で
    // IRQ1 が届く形になりうる。
    switch_keyboard_to_io_apic(logger, apic);

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
        interrupts::run_timer_loop(
            logger,
            console,
            stop_after_ticks,
            shell_after_heartbeats,
            apic,
        );
    }

    // **シェルへ渡すために戻ってきた（S11-11）。** 割り込みは動いたままである。
    if stop_after_ticks == 0 && shell_after_heartbeats != 0 {
        return;
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
/// 1. コンフィグバイトを読んで、翻訳（セット 1）と割り込みが有効かを見る
/// 2. 落ちていれば立てて書き戻し、読み直して一致を確認する
/// 3. 出力バッファの残留データを読み捨てる
/// 4. IRQ1 のマスクを解除する
/// 5. IMR を読み戻して `master=0xFC` を照合する
///
/// 3 を 4 より前に置くのが要点。ファームウェアが残したバイトが最初のキー入力として
/// 現れる事故を防ぐ。OVMF はブートメニューでキーを扱っているので、何か残っていても
/// おかしくない。
fn setup_keyboard(logger: &mut Logger<SerialPort>) {
    use keyboard::controller;

    // --- 1. コンフィグバイトを読む ---
    // SAFETY: 起動シーケンス中で IRQ1 はマスクされており、他の実行文脈が i8042 を
    // 触っていない。
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
    // SAFETY: ベクタ 0x21 には IRQ スタイルのスタブが入っており、ハンドラはデータ
    // ポートを読み切ってから EOI を送る。
    unsafe {
        irq::unmask(keyboard::KEYBOARD_IRQ);
    }

    // --- 5. IMR を読み戻す ---
    let after_unmask = irq::check_masks(&[0, keyboard::KEYBOARD_IRQ]);
    logger.info(format_args!("pic: IMR after unmasking IRQ1 {after_unmask}"));
    if !after_unmask.matches() {
        logger.error(format_args!(
            "pic: the mask read-back after unmasking IRQ1 does not match; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "keyboard: IRQ1 is unmasked on the 8259; the delivery vector is {:#04x} for now \
         (S2-d-1c re-routes it through the I/O APIC before sti, which changes the vector)",
        keyboard::delivery_vector()
    ));
}

/// キーボード（IRQ1）の配送を I/O APIC 経由へ切り替える（S2-d-1c）。
///
/// 配送が変わる。この段で最も危険な一手である。
///
/// # 呼ぶ位置
///
/// IRQ1 を 8259 で解禁した後、`sti` より前である。割り込みが有効になる前に切り替え
/// 終えるので、切り替えの途中で IRQ1 が届く形にならない。
///
/// # 順序
///
/// 実際の 4 手（経路の設定 → PIC でマスク → 状態 → I/O APIC で解禁）は
/// [`irq::route_to_apic`] が持つ。ここはその前後の観測に徹する。
fn switch_keyboard_to_io_apic(
    logger: &mut Logger<SerialPort>,
    mapped: Option<&kernel::apic::MappedApic>,
) {
    let Some(mapped) = mapped else {
        logger.warn(format_args!(
            "ioapic: no I/O APIC was mapped, so IRQ1 stays on the 8259"
        ));
        return;
    };

    // 破壊 (ioapic-wrong-vector-test): ゲートの無いベクタへ向ける。読み戻しの主張と
    // 到達の主張が両方落ちる（S2-d-1c）。
    #[cfg(not(feature = "ioapic-wrong-vector-test"))]
    let vector = idt::IOAPIC_KEYBOARD_VECTOR as u8;
    #[cfg(feature = "ioapic-wrong-vector-test")]
    let vector = idt::IOAPIC_KEYBOARD_VECTOR as u8 + 1;

    // SAFETY: ベクタ IOAPIC_KEYBOARD_VECTOR には専用スタブのゲートが入っており
    // （`idt::init`）、ハンドラはデータポートを読み切ってから EOI を送る。
    // 割り込みはまだ禁止されている。
    if let Err(error) = unsafe { irq::route_to_apic(mapped, keyboard::KEYBOARD_IRQ, vector) } {
        logger.error(format_args!(
            "ioapic: could not route IRQ1 to the I/O APIC ({error:?}); halting"
        ));
        cpu::halt_forever();
    }

    // 設定の読み戻し。書いた値が entry に載っていることを、到達とは独立に確かめる。
    // 読み戻しだけだと配送されない形（マスクの外し忘れ、宛先の誤り）を通し、到達だけ
    // だと保持されているかを見ていない。
    let readback = irq::routed_entry_readback(mapped, keyboard::KEYBOARD_IRQ);
    match readback {
        Some(entry) => {
            let vector_ok = entry.vector() == vector;
            // 宛先を主張にする（S4-a）。「キーボードは bootstrap processor にしか
            // 届かない」は、AP が割り込みを受けられるようになった段の安全の根拠で
            // ある。それまでは起動時の棚卸しのログに `destination=0x00` が出ている
            // だけで、実測の記憶であって主張ではなかった。
            //
            // physical モードなら high dword の宛先は Local APIC ID そのものである。
            // BSP の APIC ID は MADT の最初の使用可能なエントリから取る（その値が
            // BSP とは限らないという制約は `smp.rs` の該当箇所）。
            let expected_destination = mapped.mmio().bsp_candidate_apic_id().unwrap_or(0);
            let destination_ok =
                entry.physical_destination_mode() && entry.destination() == expected_destination;
            logger.info(format_args!(
                "ioapic: IRQ1 redirection entry read back: {entry}, vector matches what we wrote \
                 ({vector:#04x}) = {vector_ok}, destination is physical mode and equals the \
                 bootstrap processor ({expected_destination:#04x}) = {destination_ok}"
            ));
            if !vector_ok {
                logger.error(format_args!(
                    "ioapic: the redirection entry does not carry the vector we wrote; halting"
                ));
                cpu::halt_forever();
            }
            if !destination_ok {
                logger.error(format_args!(
                    "ioapic: IRQ1 is not aimed at the bootstrap processor in physical mode, so \
                     an application processor could receive it; the S4-a safety argument (the \
                     AP handler touches only per-CPU and atomic state) depends on this; halting"
                ));
                cpu::halt_forever();
            }
        }
        None => {
            logger.error(format_args!(
                "ioapic: the redirection entry for IRQ1 could not be read back; halting"
            ));
            cpu::halt_forever();
        }
    }

    logger.info(format_args!(
        "ioapic: IRQ1 now goes through the I/O APIC as vector {:#04x}; the 8259 line is \
         masked (the first key must arrive as that vector, which the 8259 cannot produce)",
        keyboard::delivery_vector()
    ));
}

/// 埋め込んだユーザープログラム `hello` の ELF（S9-b-1）。
///
/// `kernel/build.rs` が `kernel/userland/hello.rs` を `rustc` で単独にリンクし、
/// `OUT_DIR` へ置いたものを抱える。**ファイルシステムを経由しない**
/// （`docs/roadmap.md` の S9 が「ファイルシステムに依存せず」を範囲としている）。
static HELLO_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf"));

/// 埋め込んだユーザープログラム `fault-test` の ELF（S9-b-3-2a）。
///
/// 出所は [`HELLO_ELF`] と同じで、`kernel/userland/fault-test.rs` を建てたものである。
static FAULT_TEST_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fault-test.elf"));

/// 埋め込んだユーザープログラム `syscall-test` の ELF（S9-b-3-2a）。
static SYSCALL_TEST_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/syscall-test.elf"));

use kernel::userland::{load_user_program, UserLoadError};
use kernel::vfs::FS_IMAGE;

/// `build.rs` が生成した、像を建てた道具の版と大きさ（S10-a）。
mod fsimage_info {
    include!(concat!(env!("OUT_DIR"), "/fsimage_info.rs"));
}

/// 埋め込んだユーザープログラムの ELF を読み、会計を出す（S9-b-1）。
///
/// **写像もしないし走らせもしない。** 像が在って、`common::elf` が受理し、
/// 中身が期待どおりであることまでを見る。
///
/// # entry がセグメントの先頭と一致しないことを主張する
///
/// `hello` は `.text.prepad` を entry の手前へ置いてある。**リンカスクリプトの
/// `KEEP` を外すと、詰め物は到達不能なのでセクション回収に落ち、entry が
/// セグメントの先頭に戻る。実際に一度落ちた。** そうなると「entry ではなく
/// セグメントの先頭へ飛ぶ」破壊が破壊にならなくなるので、ここで主張しておく。
fn verify_embedded_user_elf(logger: &mut Logger<SerialPort>) {
    use common::elf::Elf;

    let elf = match Elf::parse(HELLO_ELF) {
        Ok(elf) => elf,
        Err(e) => {
            logger.error(format_args!(
                "user-elf: the embedded hello image did not parse: {e:?}; halting"
            ));
            cpu::halt_forever();
        }
    };

    logger.info(format_args!(
        "user-elf: embedded hello is {} byte(s), entry {:#x}, {} program header(s)",
        HELLO_ELF.len(),
        elf.entry_point,
        elf.program_headers().count()
    ));

    let mut load_count = 0usize;
    let mut entry_segment: Option<common::elf::ProgramHeader> = None;
    for ph in elf.load_segments() {
        logger.info(format_args!(
            "user-elf: PT_LOAD vaddr={:#x} filesz={:#x} memsz={:#x} flags={:#x} (r={} w={} x={})",
            ph.p_vaddr,
            ph.p_filesz,
            ph.p_memsz,
            ph.p_flags,
            ph.p_flags & 0x4 != 0,
            ph.p_flags & 0x2 != 0,
            ph.p_flags & 0x1 != 0
        ));
        load_count += 1;
        if elf.entry_point >= ph.p_vaddr && elf.entry_point < ph.p_vaddr + ph.p_memsz {
            entry_segment = Some(ph);
        }
    }

    if load_count == 0 {
        logger.error(format_args!(
            "user-elf: the embedded hello image has no PT_LOAD segment; halting"
        ));
        cpu::halt_forever();
    }

    let Some(entry_segment) = entry_segment else {
        logger.error(format_args!(
            "user-elf: the entry point {:#x} is not inside any PT_LOAD segment; halting",
            elf.entry_point
        ));
        cpu::halt_forever();
    };

    // 詰め物が生きていること。**破壊のためだけではない**——`.text` の前に別の節が
    // 来るほうが普通で、entry とセグメントの先頭が一致するのは極小の像だけである。
    if elf.entry_point == entry_segment.p_vaddr {
        logger.error(format_args!(
            "user-elf: the entry point {:#x} equals the start of its PT_LOAD; the .text.prepad \
             padding was dropped (check KEEP in userland/user.ld); halting",
            elf.entry_point
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "user-elf: embedded hello verified ({load_count} PT_LOAD segment(s), entry sits {:#x} \
         byte(s) into its segment, every segment is read-only={})",
        elf.entry_point - entry_segment.p_vaddr,
        elf.load_segments().all(|ph| ph.p_flags & 0x2 == 0)
    ));
}

/// 埋め込んだ ext2 の像を読み、superblock と group descriptor を主張する（S10-a）。
///
/// # 何を主張しているか
///
/// **外の道具（`mke2fs`）が作った像を、こちらのパーサが同じに読めることである。**
/// 自作の書き手が作った像を読めても、**自分の理解どうしの一致しか言わない。**
///
/// # 版を出す
///
/// **`mke2fs` の版が変わると既定値が動きうる**（ブロックサイズ、inode サイズ）。
/// 判定行に載せておくと、**将来ここが落ちたときに「像の作り手が変わった」を
/// 最初に疑える。**
/// 埋め込んだ ext2 の像を、書ける場所（フレーム）へ複製する（S12-a）。
///
/// # なぜ複製するのか。**`.rodata` だからではない**
///
/// **保護の話ではない。** 像を含むカーネル像の写像は読み書き可で、`W^X` は
/// 未実装である（棚卸しで実測した）。**書けないのは [`FS_IMAGE`] が
/// `&'static [u8]` だからで、型の話である。**
///
/// **それでも複製する。** `static mut` にしてその場で書く案は
/// **「不可能」ではなく「採らない」である**——**埋め込んだ像は
/// `build.rs` が建てたものと同一であることが検査の前提**で
/// （`--full` の `fs image e2fsck`）、**その前提を実行時に壊すと、
/// 「建てた像」と「動かした像」が同じものを指さなくなる。**
///
/// # S12-a では書き換えない
///
/// **作るのは経路だけである。** 複製して、物理の位置を判定行に出し、
/// **ホスト側が取り出して元の像と突き合わせる**ところまでを見る。
///
/// **書き換えを入れると、経路の誤りと書き込みの誤りが混ざる。**
/// **S12-a が主張するのは「複製と取り出しが 1 バイトも落とさないこと」だけである。**
///
/// # 返さないフレームである
///
/// **取ったきり返さない。** 像は起動中ずっと生きる。
/// **`spawn` の会計（`leaked`）には出ない**——あちらはプロセスの
/// アドレス空間の畳みを、`spawn` の前後で測っている。**ここは spawn より前で、
/// 窓の外である**（実測で確かめた）。
fn copy_fs_image_to_frames(logger: &mut Logger<SerialPort>) {
    use kernel::frame_allocator::FRAME_SIZE;

    // **預けた後なので借りる**（`ADR-0030`）。**取ったフレームは返さないが、
    // アロケータ自身は返す**——貸し借りの回数は判定行で突き合わされている。
    let Some(allocator) = kernel::frame_allocator::take() else {
        logger.error(format_args!(
            "fs-image-copy: the frame allocator is not available (someone did not give it back); \
             halting"
        ));
        cpu::halt_forever();
    };

    let bytes = FS_IMAGE.len() as u64;
    let frames = bytes.div_ceil(FRAME_SIZE);
    // **2 MiB 境界へ揃える。** いま要るのは取り出しだけで、揃える必要は無い。
    // **引数 1 つで済むので揃えておく**——後で 2MiB ページ 1 枚で張り直す道が残る。
    const HUGE_PAGE_FRAMES: u64 = (2 * 1024 * 1024) / FRAME_SIZE;

    let Some(base) = allocator.allocate_contiguous_aligned(frames, HUGE_PAGE_FRAMES) else {
        logger.error(format_args!(
            "fs-image-copy: could not allocate {frames} contiguous frame(s) for the {bytes}-byte              image; halting"
        ));
        cpu::halt_forever();
    };

    let direct_map = common::addr::direct_map();
    // **覆いを先に見る。** `phys_to_virt` は覆いを検査せずに加算するだけなので、
    // **覆いの外を渡すと黙って別のアドレスを返す**（`address_space` と同じ作法）。
    let Some(last) = common::addr::PhysAddr::new(base.as_u64() + frames * FRAME_SIZE - 1) else {
        logger.error(format_args!(
            "fs-image-copy: the end of the copy is not a valid physical address; halting"
        ));
        cpu::halt_forever();
    };
    if !direct_map.covers(base) || !direct_map.covers(last) {
        logger.error(format_args!(
            "fs-image-copy: the direct map does not cover {:#x}..{:#x}; halting",
            base.as_u64(),
            last.as_u64()
        ));
        cpu::halt_forever();
    }

    let destination = direct_map.phys_to_virt(base).as_u64() as *mut u8;
    // SAFETY: いま確保した連続フレームで、direct map が覆っていることを上で確かめた。
    // 誰も使っていない。`bytes` は像の長さで、確保した範囲に収まる。
    unsafe {
        core::ptr::copy_nonoverlapping(FS_IMAGE.as_ptr(), destination, bytes as usize);
    }

    // **書いたものを読み戻して突き合わせる。** 複製したことを主張の根拠にしない
    // （`install_guard_page` が split の後に粒度を読み直すのと同じ形）。
    //
    // 破壊 (S12-a, fs-copy-corrupt-tail): 末尾の 1 バイトを 0xFF で潰す。
    // **読み戻しがここで落ちる。** 落とさなければホスト側の突き合わせが落ちる。
    //
    // **0 で潰す形は破壊にならない。** 像の末尾は既に 0 なので、書いても何も
    // 変わらない（**実測でそうなった**——破壊を立てたのに項目が通った）。
    // **「壊したつもりで壊れていない」を、破壊を走らせて捕まえた例である。**
    #[cfg(feature = "fs-copy-corrupt-tail-test")]
    // SAFETY: 上と同じ範囲。破壊のために末尾を 1 バイトだけ変える。
    unsafe {
        destination.add(bytes as usize - 1).write(0xFF);
    }

    // SAFETY: いま書いた範囲を読むだけである。
    let copied = unsafe { core::slice::from_raw_parts(destination as *const u8, bytes as usize) };
    let identical = copied == FS_IMAGE;

    // **アロケータを返す。** 取ったフレームは返さないが、**借りたものは返す。**
    kernel::frame_allocator::give_back(allocator);

    // **カーネル像の物理範囲も一緒に出す。** ホスト側が「複製先がカーネル像の
    // 外にあること」を見る——**出さないと「複製した」が反証できない**
    // （複製せずに `.rodata` の番地を出す形が通ってしまう）。
    let (image_start, image_end) = kernel_image_phys_range();
    logger.info(format_args!(
        "fs-image-copy: copied {bytes} byte(s) to phys {:#x}..{:#x} ({frames} frame(s), \
         2MiB-aligned={}), read-back identical={identical}; the kernel image is {:#x}..{:#x}",
        base.as_u64(),
        base.as_u64() + bytes,
        base.as_u64() % (2 * 1024 * 1024) == 0,
        image_start.as_u64(),
        image_end.as_u64()
    ));

    if !identical {
        logger.error(format_args!(
            "fs-image-copy: the copy does not match the embedded image; halting"
        ));
        cpu::halt_forever();
    }
}

fn verify_embedded_fs_image(logger: &mut Logger<SerialPort>) {
    use common::ext2::Ext2;

    let fs = match Ext2::parse(FS_IMAGE) {
        Ok(fs) => fs,
        Err(e) => {
            logger.error(format_args!(
                "ext2: the embedded image did not parse: {e:?}; halting"
            ));
            cpu::halt_forever();
        }
    };

    logger.info(format_args!(
        "ext2: image {} byte(s) built by {:?}",
        FS_IMAGE.len(),
        fsimage_info::MKE2FS_VERSION
    ));
    logger.info(format_args!(
        "ext2: superblock rev=1 block_size={} inode_size={} inodes={} blocks={} \
         blocks_per_group={} inodes_per_group={} first_ino={} groups={}",
        fs.block_size(),
        fs.inode_size(),
        fs.inodes_count(),
        fs.blocks_count(),
        fs.blocks_per_group(),
        fs.inodes_per_group(),
        fs.first_inode(),
        fs.group_count()
    ));
    logger.info(format_args!(
        "ext2: features compat={:#x} incompat={:#x} ro_compat={:#x} (only unknown incompat \
         bits are refused; unknown ro_compat and compat are accepted for reading, as Linux does)",
        fs.feature_compat(),
        fs.feature_incompat(),
        fs.feature_ro_compat()
    ));

    // **像の大きさは build.rs が知っている値と一致するはず。** 食い違えば、
    // 抱えた像と建てた像が別物である。
    if FS_IMAGE.len() as u64 != fsimage_info::IMAGE_BYTES {
        logger.error(format_args!(
            "ext2: the embedded image is {} byte(s) but build.rs made {}; halting",
            FS_IMAGE.len(),
            fsimage_info::IMAGE_BYTES
        ));
        cpu::halt_forever();
    }

    // group descriptor を全部読む。**3 つのブロック番号が像の外を指していない
    // ことは `group_descriptor` が見ている**（線3）。
    for group in 0..fs.group_count() {
        match fs.group_descriptor(group) {
            Ok(descriptor) => logger.info(format_args!(
                "ext2: group {group}: block bitmap at {} inode bitmap at {} inode table at {}",
                descriptor.block_bitmap, descriptor.inode_bitmap, descriptor.inode_table
            )),
            Err(e) => {
                logger.error(format_args!(
                    "ext2: group {group} descriptor is not usable: {e:?}; halting"
                ));
                cpu::halt_forever();
            }
        }
    }

    verify_root_inode(logger, &fs);
    verify_path_lookup(logger, &fs);
    verify_single_indirect_boundary(logger, &fs);
}

/// 壊した ext2 の像を組み立てる作業領域（S10-a）。
///
/// # 像全体（2 MiB）を抱えない
///
/// **`.bss` が 2 MiB 増えると、bootloader が `0x100000` へ確保する量がそのぶん
/// 増える。** 起動する上限は 6 MiB と 7 MiB のあいだにあると実測してあり
/// （`kernel/build.rs` の `IMAGE_BYTES`）、**像を 2 MiB に決めたときの余裕を
/// ここで食い潰しては、決めた意味が無くなる。**
///
/// # 先頭 80 ブロックだけで、読み切れる像になる
///
/// **`s_blocks_count` を 80 に直せば、切り出した先頭がそれ自体で完結する。**
/// 実際に参照されている最大のブロックは 74 だからである（実測。`/etc/motd` の
/// データブロック）。**80 に余裕を取ってあるので、種が少し増えても収まる。**
/// **収まらなくなったら健全な対照（下）が最初に落ちる。**
///
/// **S11-9 で 64 から 80 へ上げた。** 像へ `/bin/ls` と `/bin/cat` を足したので、
/// **後ろのブロック番号がすべてずれた**（58 → 69）。**「収まらなくなったら対照が
/// 落ちる」が実際に働く前に、測って直した。**
static mut CORRUPT_FS_IMAGE: [u8; CORRUPT_FS_LEN] = [0; CORRUPT_FS_LEN];

/// 切り出すブロック数。**参照されている最大のブロック（74）より大きいこと。**
const CORRUPT_FS_BLOCKS: usize = 80;

/// 切り出した像のバイト数。
const CORRUPT_FS_LEN: usize = CORRUPT_FS_BLOCKS * FS_BLOCK_SIZE;

/// 像のブロックサイズ（`mke2fs` の既定。判定行で毎起動確かめている）。
const FS_BLOCK_SIZE: usize = 4096;

/// superblock の像内オフセット。**ブロックサイズに依らず固定である。**
const FS_SUPERBLOCK: usize = 1024;

/// group descriptor テーブルの先頭（`s_first_data_block` が 0 なのでブロック 1）。
const FS_GROUP_DESCRIPTORS: usize = FS_BLOCK_SIZE;

/// inode テーブルの先頭（group 0 の `bg_inode_table` は 4。判定行に出ている）。
const FS_INODE_TABLE: usize = 4 * FS_BLOCK_SIZE;

/// inode 1 つのバイト数（`s_inode_size`。判定行に出ている）。
const FS_INODE_SIZE: usize = 256;

/// inode `ino` の像内オフセット。
const fn fs_inode_at(ino: usize) -> usize {
    FS_INODE_TABLE + (ino - 1) * FS_INODE_SIZE
}

/// ルート inode の像内オフセット。
const FS_ROOT_INODE_AT: usize = fs_inode_at(2);

/// `/etc/motd` の inode（判定行に出ている）の像内オフセット。
///
/// **S11-5 で 18 から 19 へ、S11-9 で 19 から 21 へ、S11-11 で 21 から 22 へ、
/// S12 前の手当ての C で 22 から 23 へ動いた。** 像へ `/bin/spawn-test`、
/// 続いて `/bin/ls` と `/bin/cat`、そして `/bin/spin` を足したので、**後ろの
/// inode 番号がそのぶんずれた**（`docs/coding-standards.md` の
/// 「実測値は、測った条件が変わると古くなる」）。**そのつど測り直している。**
///
/// **doc の見出しから番号を落とした。** かつて「21 番」と書いてあったが、
/// **本体が 22 になっても直されていなかった**——**同じ数を 2 か所に書くと、
/// 片方だけが古くなる。** 番号は下の式が持つ。
const FS_MOTD_INODE_AT: usize = fs_inode_at(23);

/// ルートディレクトリのデータブロック（実測。判定行の `i_block[0]` に出ている）。
const FS_ROOT_DIR_BLOCK: usize = 20 * FS_BLOCK_SIZE;

/// ルートディレクトリの `etc` エントリの位置（実測。`. .. lost+found bin data` の後）。
const FS_ROOT_ETC_ENTRY: usize = FS_ROOT_DIR_BLOCK + 68;

/// `/data/indirect-first` の単一間接ブロック（実測。判定行の `single indirect` に出ている）。
///
/// **S11-5 で 55 から 58 へ、S11-9 で 58 から 66 へ、S11-10 で 67 へ、S11-11 で 71 へ、
/// S12 前の手当ての 3 本目で 72 へ動いた**（像へプログラムを足し、受け皿の位置を
/// 上げて像が育った。[`FS_MOTD_INODE_AT`] と同じ理由である）。
///
/// **5 度目はプログラムを足していない。`sh` が太っただけである**——
/// 固定の既定（`/` を含まない語を `/bin/` の下で探す）で `.text` が伸び、
/// **`/bin/zash`（当時の名前は `/bin/sh`）が 1 ブロック増えて、後ろのブロックがそのぶんずれた。**
/// **像に載るのは本数だけでなく、1 本あたりの大きさでもある。**
///
/// **6 度目は S12 前の手当ての C で、72 から 75 へ動いた**（`/bin/spin` を足した）。
/// **今回は `debugfs` で測った**——判定行にも出ているが、
/// **像を読む側と壊す側が同じ数を別々に持つので、外の道具で突き合わせた。**
const FS_INDIRECT_TABLE_BLOCK: usize = 75 * FS_BLOCK_SIZE;

/// `/etc/motd` のデータブロック（実測）。
///
/// **S11-5 で 58 から 61 へ、S11-9 で 61 から 69 へ、S11-10 で 70 へ、S11-11 で 74 へ、
/// S12 前の手当ての 3 本目で 75 へ、同じ手当ての C で 78 へ動いた**
/// （[`FS_MOTD_INODE_AT`] と同じ理由）。
const FS_MOTD_DATA_BLOCK: usize = 78 * FS_BLOCK_SIZE;

/// 種のファイルと同じ木にある `/etc/motd` の中身（S10-a）。
///
/// **像の中の `/etc/motd` は、このファイルを `mke2fs -d` が写したものである。**
/// **写しを 2 つ持たない**——期待値をカーネルへ書き写すと、種を変えたときに
/// 片方だけが古くなる（**単一間接の期待値を `build.rs` から出したのと同じ理由**）。
/// `kernel/build.rs` が種の木に `rerun-if-changed` を張っているので、
/// **この定数と像は同じ 1 本のファイルから来る。**
static MOTD_SEED: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fsimage/seed/etc/motd"
));

/// 名前でファイルへ届き、中身が種と一致することを主張する（S10-a）。
///
/// # `hello` の `write` と同じ形の主張である
///
/// **既知のバイト列と一致することを言う。** ここまでの判定行は「読めた」「形が
/// 合っている」を言ってきたが、**中身そのものを突き合わせるのはこれが最初である。**
///
/// # 毎回ルートから辿る
///
/// **`dentry` を置かない**（`docs/roadmap.md` の S10）。引く回数が問題になって
/// いない段では、**キャッシュを持つ理由が無い。**
fn verify_path_lookup(logger: &mut Logger<SerialPort>, fs: &common::ext2::Ext2<'_>) {
    const MOTD_PATH: &str = "/etc/motd";

    let motd = match fs.lookup(MOTD_PATH.as_bytes()) {
        Ok(inode) => inode,
        Err(e) => {
            logger.error(format_args!(
                "ext2: {MOTD_PATH} does not resolve: {e:?}; halting"
            ));
            cpu::halt_forever();
        }
    };

    // **1 ブロックに収まる大きさである。** 収まらなくなったら、ここが最初に気づく。
    if motd.size != MOTD_SEED.len() as u64 || fs.block_span(&motd) != 1 {
        logger.error(format_args!(
            "ext2: {MOTD_PATH} is {} byte(s) in {} block(s) but the seed file is {} byte(s); \
             halting",
            motd.size,
            fs.block_span(&motd),
            MOTD_SEED.len()
        ));
        cpu::halt_forever();
    }

    let contents = match fs.file_block(&motd, 0) {
        Ok(bytes) => bytes,
        Err(e) => {
            logger.error(format_args!(
                "ext2: {MOTD_PATH} is not readable: {e:?}; halting"
            ));
            cpu::halt_forever();
        }
    };

    // **改行を判定行に出さない。** 1 行の判定行が 2 行に割れると、
    // 起動ログの参照との突き合わせが読みにくくなる。
    let shown = contents.strip_suffix(b"\n").unwrap_or(contents);
    logger.info(format_args!(
        "ext2: resolved {MOTD_PATH} to inode {}: {} byte(s) = {:?} (trailing newline trimmed \
         for this line only); matches the seed file byte for byte: {}",
        motd.number,
        contents.len(),
        core::str::from_utf8(shown).unwrap_or("<not valid UTF-8>"),
        contents == MOTD_SEED
    ));

    if contents != MOTD_SEED {
        logger.error(format_args!(
            "ext2: {MOTD_PATH} does not match the seed file that mke2fs copied into the image; \
             halting"
        ));
        cpu::halt_forever();
    }
}

/// ルート inode を読み、直接ブロックで中身へ届くことを主張する（S10-a）。
///
/// # なぜルートだけか
///
/// **番号で辿れるのがルートだけだからである。** ext2 のルートは 2 番で固定
/// （`common::ext2::ROOT_INODE`）で、**それ以外の inode へは名前からしか届かない。**
/// ディレクトリの走査とパス解決は次の 2 刻みなので、ここではまだ名前を引けない。
///
/// # ここが主張するのは「届いた」ところまでである
///
/// **「4096 バイト読めた」だけでは、読めたブロックがルートの中身だとは言えない。**
/// **正しいものが読めたことは [`verify_root_directory_walk`] が言う**——先頭の
/// エントリが `.` で、その inode 番号が自分自身であることを、走査の結果として
/// 見る。**以前はここで先頭 4 バイトだけを覗いていたが、走査を書いたので
/// そちらへ寄せた。**
fn verify_root_inode(logger: &mut Logger<SerialPort>, fs: &common::ext2::Ext2<'_>) {
    use common::ext2::ROOT_INODE;

    let root = match fs.inode(ROOT_INODE) {
        Ok(inode) => inode,
        Err(e) => {
            logger.error(format_args!(
                "ext2: the root inode ({ROOT_INODE}) is not readable: {e:?}; halting"
            ));
            cpu::halt_forever();
        }
    };
    logger.info(format_args!(
        "ext2: root inode {}: mode={:06o} size={} links={} i_block[0]={} directory={}",
        root.number,
        root.mode,
        root.size,
        root.links_count,
        root.blocks[0],
        root.is_directory()
    ));

    if !root.is_directory() {
        logger.error(format_args!(
            "ext2: the root inode is not a directory (mode={:06o}); halting",
            root.mode
        ));
        cpu::halt_forever();
    }

    let first = match fs.file_block(&root, 0) {
        Ok(bytes) => bytes,
        Err(e) => {
            logger.error(format_args!(
                "ext2: the root directory's first block is not readable: {e:?}; halting"
            ));
            cpu::halt_forever();
        }
    };
    logger.info(format_args!(
        "ext2: root directory block 0 of {}: {} byte(s) via the direct blocks",
        fs.block_span(&root),
        first.len()
    ));

    verify_root_directory_walk(logger, fs, &root);
}

/// ルートディレクトリを走査し、名前を判定行に出す（S10-a）。
///
/// # 何を主張しているか
///
/// **`mke2fs` が並べたエントリを、こちらの走査が同じ順で同じ名前として読めること。**
/// **先頭 2 つは形が決まっている**——`.` は自分自身を指し、`..` はルートでは
/// 自分自身を指す。**ここだけをカーネルで確かめる。**
///
/// # 残りの名前をここで固定しない
///
/// **`lost+found` は `mke2fs` が作り、`bin`・`data`・`etc` は種の木が決めている。**
/// カーネルへ書き写すと、種を変えたときに片方だけが古くなる（**単一間接の
/// 期待値を `build.rs` から出したのと同じ理由**）。**一覧を固定しているのは
/// 起動ログの参照である**——`xtask/reference/boot-log-smp2.txt` が行単位で
/// 突き合わせるので、並びが変われば `--boot-log-diff` が落ちる。
///
/// # 走査が止まることは、ここでは主張しない
///
/// **停止性はホストテストが見ている**（`common::ext2` の
/// `an_all_zero_directory_block_ends_the_walk`）。**QEMU では「返ってこない」が
/// タイムアウトとしてしか観測できないので、falsify できる場所へ寄せてある。**
fn verify_root_directory_walk(
    logger: &mut Logger<SerialPort>,
    fs: &common::ext2::Ext2<'_>,
    root: &common::ext2::Inode,
) {
    use common::ext2::ROOT_INODE;

    /// 名前を並べる作業領域。**足りなければ切り詰めたことを判定行に出す。**
    const NAME_BUFFER_LEN: usize = 192;

    let entries = match fs.directory_entries(root) {
        Ok(entries) => entries,
        Err(e) => {
            logger.error(format_args!(
                "ext2: the root inode cannot be walked as a directory: {e:?}; halting"
            ));
            cpu::halt_forever();
        }
    };

    let mut names = [0u8; NAME_BUFFER_LEN];
    let mut used = 0usize;
    let mut truncated = false;
    let mut count = 0usize;
    let mut first_two = [0u32; 2];

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                logger.error(format_args!(
                    "ext2: root directory entry {count} is not usable: {e:?}; halting"
                ));
                cpu::halt_forever();
            }
        };
        if count < first_two.len() {
            first_two[count] = entry.inode;
        }
        count += 1;

        // 区切りの空白と名前を詰める。**入らなければ詰めるのをやめる。**
        let separator = usize::from(used != 0);
        if used + separator + entry.name.len() <= NAME_BUFFER_LEN {
            if separator != 0 {
                names[used] = b' ';
                used += 1;
            }
            names[used..used + entry.name.len()].copy_from_slice(entry.name);
            used += entry.name.len();
        } else {
            truncated = true;
        }
    }

    // **名前は生のバイト列で、UTF-8 とは限らない。** 読めない並びが来たら、
    // 判定行にそう出す（**黙って落とさない**）。
    let listing = core::str::from_utf8(&names[..used]).unwrap_or("<not valid UTF-8>");
    logger.info(format_args!(
        "ext2: root directory: {count} entr(y/ies): {listing}{}",
        if truncated { " ..." } else { "" }
    ));

    // **先頭 2 つだけを固定する。** `.` と `..` はどちらもルート自身を指す。
    if count < 2 || first_two != [ROOT_INODE, ROOT_INODE] {
        logger.error(format_args!(
            "ext2: the root directory should start with \".\" and \"..\" both pointing at \
             inode {ROOT_INODE}, but the first entries point at {first_two:?} ({count} entries \
             in total); halting"
        ));
        cpu::halt_forever();
    }
}

/// 単一間接の境界を挟む 2 本を読み、**両側**を主張する（S10-a）。
///
/// # なぜ対で見るのか
///
/// `/data/direct-max` は 12 ブロックちょうど（49152 バイト）で、**単一間接を
/// 使わない。** `/data/indirect-first` はそれより 1 バイト大きく、**13 ブロック目が
/// 単一間接の先にある。** 片側だけでは「間接を踏んだ」ことも「踏まずに済んだ」ことも
/// 言えない。**境界の両側を並べて初めて、越えたことが主張になる。**
///
/// # 最後の 1 バイトを見る理由
///
/// **「読めた」と「正しいものが読めた」は違う**（ルート inode の `.` と同じ形）。
/// 最後の 1 バイトは**間接の表を経由しないと届かない位置**にあり、期待値は
/// `kernel/build.rs` が模様を決めている側から出している。**カーネルへ書き写すと、
/// 模様を変えたときに片方だけが古くなる。**
///
/// # 名前で引く
///
/// **`mke2fs -d` が割り当てた inode 番号を直に書いていたが、パス解決を書いたので
/// 外した**（S10-a の 6 本目）。**大きさの突き合わせは残してある**——番号ではなく
/// 名前で届くようになっても、**届いた先が期待どおりのものかは別の主張である。**
fn verify_single_indirect_boundary(logger: &mut Logger<SerialPort>, fs: &common::ext2::Ext2<'_>) {
    use common::ext2::SINGLE_INDIRECT_SLOT;

    let cases = [
        (
            "/data/direct-max",
            fsimage_info::DIRECT_MAX_BYTES,
            fsimage_info::DIRECT_MAX_LAST_BYTE,
            false,
        ),
        (
            "/data/indirect-first",
            fsimage_info::INDIRECT_FIRST_BYTES,
            fsimage_info::INDIRECT_FIRST_LAST_BYTE,
            true,
        ),
    ];

    for (path, expected_size, expected_last, expects_indirect) in cases {
        let inode = match fs.lookup(path.as_bytes()) {
            Ok(inode) => inode,
            Err(e) => {
                logger.error(format_args!(
                    "ext2: {path} does not resolve: {e:?}; halting"
                ));
                cpu::halt_forever();
            }
        };
        let ino = inode.number;
        if inode.size != expected_size || !inode.is_regular_file() {
            logger.error(format_args!(
                "ext2: {path} should be {expected_size} byte(s) and regular but is \
                 {} byte(s) mode={:06o}; halting",
                inode.size, inode.mode
            ));
            cpu::halt_forever();
        }

        let uses_indirect = inode.blocks[SINGLE_INDIRECT_SLOT] != 0;
        if uses_indirect != expects_indirect {
            logger.error(format_args!(
                "ext2: {path} has i_block[12]={} but the single indirect block is expected to be \
                 {}; halting",
                inode.blocks[SINGLE_INDIRECT_SLOT],
                if expects_indirect { "in use" } else { "unused" }
            ));
            cpu::halt_forever();
        }

        // 最後の 1 バイトは最後のブロックの末尾にある。**返るのは `i_size` で
        // 切られたバイト列なので、末尾がそのまま最後の 1 バイトである。**
        let last_index = (fs.block_span(&inode) - 1) as u32;
        let last_block = match fs.file_block(&inode, last_index) {
            Ok(bytes) => bytes,
            Err(e) => {
                logger.error(format_args!(
                    "ext2: {path} block {last_index} is not readable: {e:?}; halting"
                ));
                cpu::halt_forever();
            }
        };
        let Some(&last_byte) = last_block.last() else {
            logger.error(format_args!(
                "ext2: {path} block {last_index} came back empty; halting"
            ));
            cpu::halt_forever();
        };

        logger.info(format_args!(
            "ext2: {path} inode={ino} size={} blocks={} single indirect={} last byte={last_byte:#04x} \
             (expected {expected_last:#04x})",
            inode.size,
            fs.block_span(&inode),
            inode.blocks[SINGLE_INDIRECT_SLOT]
        ));
        if last_byte != expected_last {
            logger.error(format_args!(
                "ext2: {path} last byte is {last_byte:#04x}, expected {expected_last:#04x}; \
                 halting"
            ));
            cpu::halt_forever();
        }
    }
}

/// 壊した像に対して走らせる観測（S10-a）。
///
/// **すべて `Result<(), Ext2Error>` へ畳む。** 成功したら反証が失敗である。
type FsProbe = fn(&common::ext2::Ext2<'_>) -> Result<(), common::ext2::Ext2Error>;

/// 書き換え 1 か所。
struct FsPatch {
    offset: usize,
    value: u64,
    width: usize,
}

/// 壊し方 1 つ分の記述（S10-a）。
///
/// **原則として 1 か所だけを壊す**（S9-b-2 と同じ。2 か所壊すと、どちらで拒まれた
/// のかが分からない）。**例外は「1 つの論理的な変更が 2 つの欄にまたがる」場合だけ**
/// で、inode テーブルを像の外へ伸ばす case がそれに当たる（`s_inodes_count` と
/// `s_inodes_per_group` を揃えて動かさないと、別の検査に先に当たる）。
struct CorruptFsCase {
    /// 判定行に出す壊し方の説明。**「何をしたか」を書く。**
    what: &'static str,
    /// 書き換え（空なら [`CorruptFsCase::truncate_to`] だけを使う）。
    patches: &'static [FsPatch],
    /// 像をこの長さへ切り詰める（0 なら切り詰めない）。
    truncate_to: usize,
    /// `parse` が通った後に走らせる観測。
    probe: FsProbe,
    /// 期待する拒否理由。
    expected: common::ext2::Ext2Error,
}

/// `parse` で拒まれるはずの case に付ける観測。**通ってしまったことが分かればよい。**
fn fs_probe_nothing(_: &common::ext2::Ext2<'_>) -> Result<(), common::ext2::Ext2Error> {
    Ok(())
}

/// ルート inode を読む。
fn fs_probe_root_inode(fs: &common::ext2::Ext2<'_>) -> Result<(), common::ext2::Ext2Error> {
    fs.inode(common::ext2::ROOT_INODE).map(|_| ())
}

/// 名乗った最大の inode を読む。**inode テーブルの端の算術に当たる。**
fn fs_probe_highest_inode(fs: &common::ext2::Ext2<'_>) -> Result<(), common::ext2::Ext2Error> {
    fs.inode(fs.inodes_count()).map(|_| ())
}

/// ルートディレクトリを最後まで走査する。
fn fs_probe_root_walk(fs: &common::ext2::Ext2<'_>) -> Result<(), common::ext2::Ext2Error> {
    let root = fs.inode(common::ext2::ROOT_INODE)?;
    for entry in fs.directory_entries(&root)? {
        entry?;
    }
    Ok(())
}

/// `/etc/motd` を名前で引く。
fn fs_probe_lookup_motd(fs: &common::ext2::Ext2<'_>) -> Result<(), common::ext2::Ext2Error> {
    fs.lookup(b"/etc/motd").map(|_| ())
}

/// `/etc/motd` を最後のブロックまで読む。
fn fs_probe_read_motd(fs: &common::ext2::Ext2<'_>) -> Result<(), common::ext2::Ext2Error> {
    fs_read_whole_file(fs, b"/etc/motd")
}

/// `/data/indirect-first` を最後のブロックまで読む。**単一間接の先まで届く。**
fn fs_probe_read_indirect_first(
    fs: &common::ext2::Ext2<'_>,
) -> Result<(), common::ext2::Ext2Error> {
    fs_read_whole_file(fs, b"/data/indirect-first")
}

/// パスで引いたファイルを、最初のブロックから最後まで読む。
fn fs_read_whole_file(
    fs: &common::ext2::Ext2<'_>,
    path: &[u8],
) -> Result<(), common::ext2::Ext2Error> {
    let inode = fs.lookup(path)?;
    for index in 0..fs.block_span(&inode) {
        fs.file_block(&inode, index as u32)?;
    }
    Ok(())
}

/// 壊した ext2 の像が拒まれ、**カーネルが止まらない**ことを確かめる（S10-a）。
///
/// # 既定ビルドで毎起動走らせる
///
/// **壊す対象がデータなので、破壊 feature ではなく像を壊す**（S9-b-2 と同じ形。
/// `docs/roadmap.md` の S10）。**破壊 feature の中だけで壊すと、「壊す処理そのものが
/// 壊れている」ことに気づけない。**
///
/// # 4 つの線に対する反証である
///
/// - **線1**: 切り詰めた像。**切り出しが範囲外へ出ない**
/// - **線2**: ブロックサイズの桁あふれ、inode テーブルの位置の算術
/// - **線3**: group descriptor・`i_block`・dirent・間接ブロックの、4 種類の参照
/// - **線4**: `rec_len` が 0。**「止まらないこと」は「エラーが返ること」で観測する**
///   ——この case が返ってきた時点で、走査が止まったことが示されている
///
/// # 置いていない壊し方と、その理由
///
/// - **ビットマップと使用状況の矛盾**: `e2fsck` の仕事である（`common::ext2` の
///   「扱わないもの」）。**読み取りには要らない**
/// - **チェックサム**: ext2 に無い（ext4 の機能）
/// - **バックアップ superblock との食い違い**: 突き合わせていないので、壊しても
///   何も起きない。**検査していないものを壊しても反証にならない**
/// - **`s_state` が clean でない**: **読み取りは拒まない**（Linux も読み取り専用
///   マウントは許す）。**拒まないと決めたものを、拒むことの反証にはできない**
/// - **二重・三重間接と穴**: **この像には作れない。** 二重間接が要るのは 4 MiB 超の
///   ファイルからで 2 MiB の像に入らず、`mke2fs -d` は穴を作らない。
///   **ホストテストが見ている**（`common::ext2` の
///   `refuses_a_file_that_uses_the_double_or_triple_indirect_slots` ほか）
fn verify_corrupt_fs_image_is_rejected(logger: &mut Logger<SerialPort>) {
    use common::ext2::{Ext2, Ext2Error};

    /// `s_inodes_count` と `s_inodes_per_group` を揃えて動かす値。
    const HUGE_INODE_COUNT: u64 = 600_000;
    /// `etc` を `xtc` にする 1 バイト。**名前が変われば、パスは解決しない。**
    const MOTD_PATH_BREAKING_BYTE: u64 = b'x' as u64;
    /// そのときに inode テーブルの端が要求するバイト位置。
    const HUGE_INODE_END: u64 =
        FS_INODE_TABLE as u64 + (HUGE_INODE_COUNT - 1) * FS_INODE_SIZE as u64 + 128;

    let cases: &[CorruptFsCase] = &[
        CorruptFsCase {
            what: "truncated to 512 bytes (the superblock does not fit)",
            patches: &[],
            truncate_to: 512,
            probe: fs_probe_nothing,
            expected: Ext2Error::TooShort,
        },
        CorruptFsCase {
            what: "s_magic zeroed",
            patches: &[FsPatch {
                offset: FS_SUPERBLOCK + 56,
                value: 0,
                width: 2,
            }],
            truncate_to: 0,
            probe: fs_probe_nothing,
            expected: Ext2Error::BadMagic,
        },
        CorruptFsCase {
            what: "s_rev_level set to 0 (no s_inode_size, no s_first_ino)",
            patches: &[FsPatch {
                offset: FS_SUPERBLOCK + 76,
                value: 0,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_nothing,
            expected: Ext2Error::UnsupportedRevision(0),
        },
        CorruptFsCase {
            what: "an unknown INCOMPAT bit set alongside FILETYPE",
            patches: &[FsPatch {
                offset: FS_SUPERBLOCK + 96,
                value: 0x42,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_nothing,
            expected: Ext2Error::UnsupportedIncompatFeatures(0x40),
        },
        CorruptFsCase {
            what: "s_log_block_size set to 31 (1024 << 31 overflows)",
            patches: &[FsPatch {
                offset: FS_SUPERBLOCK + 24,
                value: 31,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_nothing,
            expected: Ext2Error::BadBlockSizeShift(31),
        },
        CorruptFsCase {
            what: "s_blocks_count claiming 65536 blocks (256 MiB)",
            patches: &[FsPatch {
                offset: FS_SUPERBLOCK + 4,
                value: 65_536,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_nothing,
            expected: Ext2Error::ImageTooSmall {
                needed: 65_536 * FS_BLOCK_SIZE as u64,
                actual: CORRUPT_FS_LEN as u64,
            },
        },
        CorruptFsCase {
            what: "s_inodes_per_group zeroed (a divisor of zero)",
            patches: &[FsPatch {
                offset: FS_SUPERBLOCK + 40,
                value: 0,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_nothing,
            expected: Ext2Error::ZeroPerGroup,
        },
        CorruptFsCase {
            what: "group 0's bg_inode_table pointing past the filesystem",
            patches: &[FsPatch {
                offset: FS_GROUP_DESCRIPTORS + 8,
                value: 65_535,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_root_inode,
            expected: Ext2Error::BlockOutOfRange(65_535),
        },
        CorruptFsCase {
            what: "the inode table stretched past the end of the image",
            patches: &[
                FsPatch {
                    offset: FS_SUPERBLOCK,
                    value: HUGE_INODE_COUNT,
                    width: 4,
                },
                FsPatch {
                    offset: FS_SUPERBLOCK + 40,
                    value: HUGE_INODE_COUNT,
                    width: 4,
                },
            ],
            truncate_to: 0,
            probe: fs_probe_highest_inode,
            expected: Ext2Error::InodeTableOutOfRange {
                inode: HUGE_INODE_COUNT as u32,
                needed: HUGE_INODE_END,
            },
        },
        CorruptFsCase {
            what: "the root inode's i_block[0] pointing past the filesystem",
            patches: &[FsPatch {
                offset: FS_ROOT_INODE_AT + 40,
                value: 65_535,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_root_inode,
            expected: Ext2Error::BlockOutOfRange(65_535),
        },
        CorruptFsCase {
            what: "the root inode's i_mode changed to a regular file",
            patches: &[FsPatch {
                offset: FS_ROOT_INODE_AT,
                value: 0o100_644,
                width: 2,
            }],
            truncate_to: 0,
            probe: fs_probe_root_walk,
            expected: Ext2Error::NotADirectory(2),
        },
        CorruptFsCase {
            what: "the root inode's i_size grown past its blocks",
            patches: &[FsPatch {
                offset: FS_ROOT_INODE_AT + 4,
                value: 100_000,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_root_walk,
            expected: Ext2Error::SparseBlock(1),
        },
        CorruptFsCase {
            what: "the \"etc\" entry's rec_len zeroed (the walk would not advance)",
            patches: &[FsPatch {
                offset: FS_ROOT_ETC_ENTRY + 4,
                value: 0,
                width: 2,
            }],
            truncate_to: 0,
            probe: fs_probe_root_walk,
            expected: Ext2Error::DirEntryRecordTooSmall {
                rec_len: 0,
                name_len: 3,
            },
        },
        CorruptFsCase {
            what: "the \"etc\" entry's rec_len made odd",
            patches: &[FsPatch {
                offset: FS_ROOT_ETC_ENTRY + 4,
                value: 13,
                width: 2,
            }],
            truncate_to: 0,
            probe: fs_probe_root_walk,
            expected: Ext2Error::DirEntryMisaligned(13),
        },
        CorruptFsCase {
            what: "the \"etc\" entry's rec_len reaching past the block",
            patches: &[FsPatch {
                offset: FS_ROOT_ETC_ENTRY + 4,
                value: 5_000,
                width: 2,
            }],
            truncate_to: 0,
            probe: fs_probe_root_walk,
            expected: Ext2Error::DirEntryRecordPastBlock {
                rec_len: 5_000,
                remaining: 4_028,
            },
        },
        CorruptFsCase {
            what: "the \"etc\" entry pointing at an inode outside the table",
            patches: &[FsPatch {
                offset: FS_ROOT_ETC_ENTRY,
                value: 9_999,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_root_walk,
            expected: Ext2Error::InodeOutOfRange(9_999),
        },
        CorruptFsCase {
            what: "the \"etc\" entry renamed to \"xtc\" (the path stops resolving)",
            patches: &[FsPatch {
                offset: FS_ROOT_ETC_ENTRY + 8,
                value: MOTD_PATH_BREAKING_BYTE,
                width: 1,
            }],
            truncate_to: 0,
            probe: fs_probe_lookup_motd,
            expected: Ext2Error::NotFound,
        },
        CorruptFsCase {
            what: "the single indirect table's first entry pointing past the filesystem",
            patches: &[FsPatch {
                offset: FS_INDIRECT_TABLE_BLOCK,
                value: 65_535,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_read_indirect_first,
            expected: Ext2Error::BlockOutOfRange(65_535),
        },
        CorruptFsCase {
            what: "/etc/motd's i_size grown past its blocks",
            patches: &[FsPatch {
                offset: FS_MOTD_INODE_AT + 4,
                value: 100_000,
                width: 4,
            }],
            truncate_to: 0,
            probe: fs_probe_read_motd,
            expected: Ext2Error::SparseBlock(1),
        },
    ];

    // **健全な対照を先に走らせる。** 切り出した先頭が、それ自体で読み切れる像で
    // あることを確かめる。**ここが落ちたら、壊す側ではなく切り出す長さが足りない。**
    // SAFETY: 起動時の単一実行文脈で、この静的領域を触るのはこの関数だけである。
    let control = unsafe {
        let buf = &mut *core::ptr::addr_of_mut!(CORRUPT_FS_IMAGE);
        build_truncated_fs_image(buf);
        &buf[..]
    };
    match Ext2::parse(control) {
        Ok(fs) => {
            let probes: [(&str, FsProbe); 5] = [
                ("root inode", fs_probe_root_inode),
                ("root walk", fs_probe_root_walk),
                ("lookup /etc/motd", fs_probe_lookup_motd),
                ("read /etc/motd", fs_probe_read_motd),
                ("read /data/indirect-first", fs_probe_read_indirect_first),
            ];
            for (name, probe) in probes {
                if let Err(e) = probe(&fs) {
                    logger.error(format_args!(
                        "ext2-corrupt: the untouched {CORRUPT_FS_BLOCKS}-block prefix failed \
                         the \"{name}\" probe with {e:?}. The prefix is too short to hold \
                         everything the image references; raise CORRUPT_FS_BLOCKS. halting"
                    ));
                    cpu::halt_forever();
                }
            }
        }
        Err(e) => {
            logger.error(format_args!(
                "ext2-corrupt: the untouched {CORRUPT_FS_BLOCKS}-block prefix did not parse \
                 ({e:?}); halting"
            ));
            cpu::halt_forever();
        }
    }

    let mut rejected = 0usize;
    for case in cases {
        // 毎回、健全な像から作り直す。**前の壊し方が残らないようにする。**
        // SAFETY: 起動時の単一実行文脈で、この静的領域を触るのはこの関数だけである。
        let image = unsafe {
            let buf = &mut *core::ptr::addr_of_mut!(CORRUPT_FS_IMAGE);
            build_truncated_fs_image(buf);
            for patch in case.patches {
                for i in 0..patch.width {
                    buf[patch.offset + i] = ((patch.value >> (i * 8)) & 0xFF) as u8;
                }
            }
            if case.truncate_to != 0 {
                &buf[..case.truncate_to]
            } else {
                &buf[..]
            }
        };

        let outcome = match Ext2::parse(image) {
            Ok(fs) => (case.probe)(&fs),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => {
                logger.error(format_args!(
                    "ext2-corrupt: {} was accepted; expected {:?}; halting",
                    case.what, case.expected
                ));
                cpu::halt_forever();
            }
            Err(e) if e != case.expected => {
                logger.error(format_args!(
                    "ext2-corrupt: {} was rejected as {e:?}, expected {:?}; halting",
                    case.what, case.expected
                ));
                cpu::halt_forever();
            }
            Err(e) => {
                logger.info(format_args!("ext2-corrupt: {} -> {e:?}", case.what));
                rejected += 1;
            }
        }
    }

    verify_fs_content_mismatch_is_noticed(logger, &mut rejected);

    logger.info(format_args!(
        "ext2-corrupt: all {rejected} corrupted image(s) were refused with the expected reason, \
         and the kernel continued (the embedded image is untouched; each case patches a fresh \
         copy of its first {CORRUPT_FS_BLOCKS} blocks)"
    ));

    // 壊した後も、抱えている像が読めること。**壊す処理が元を汚していないことの主張。**
    if Ext2::parse(FS_IMAGE).is_err() {
        logger.error(format_args!(
            "ext2-corrupt: the embedded image no longer parses after the corruption pass; halting"
        ));
        cpu::halt_forever();
    }
}

/// **パーサは通るが、カーネル自身の突き合わせが食い違いに気づく**壊し方（S10-a）。
///
/// # 上の表とは観測が違う
///
/// あちらは**パーサがエラーを返す**ことを見る。**こちらは読めてしまう**——
/// 形はすべて妥当なままで、**違うのは中身だけである。** 気づくのはパーサではなく
/// **既知のバイト列と突き合わせている側**である（`verify_path_lookup`）。
///
/// **この 2 つを表に混ぜない。** 混ぜると「エラーが返る」と「値が違う」が
/// 同じ主張に見える。
fn verify_fs_content_mismatch_is_noticed(logger: &mut Logger<SerialPort>, rejected: &mut usize) {
    use common::ext2::Ext2;

    let cases: [(&str, FsPatch); 2] = [
        (
            "/etc/motd's i_size shrunk to 4 bytes",
            FsPatch {
                offset: FS_MOTD_INODE_AT + 4,
                value: 4,
                width: 4,
            },
        ),
        (
            "the first byte of /etc/motd's contents zeroed",
            FsPatch {
                offset: FS_MOTD_DATA_BLOCK,
                value: 0,
                width: 1,
            },
        ),
    ];

    for (what, patch) in cases {
        // SAFETY: 起動時の単一実行文脈で、この静的領域を触るのはこの pass だけである。
        let image = unsafe {
            let buf = &mut *core::ptr::addr_of_mut!(CORRUPT_FS_IMAGE);
            build_truncated_fs_image(buf);
            for i in 0..patch.width {
                buf[patch.offset + i] = ((patch.value >> (i * 8)) & 0xFF) as u8;
            }
            &buf[..]
        };

        let Ok(fs) = Ext2::parse(image) else {
            logger.error(format_args!(
                "ext2-corrupt: {what} made the image unparseable, but this case is about a \
                 mismatch that the parser cannot see; halting"
            ));
            cpu::halt_forever();
        };
        let contents = match fs
            .lookup(b"/etc/motd")
            .and_then(|inode| fs.file_block(&inode, 0))
        {
            Ok(bytes) => bytes,
            Err(e) => {
                logger.error(format_args!(
                    "ext2-corrupt: {what} made /etc/motd unreadable ({e:?}), but this case is \
                     about a mismatch in what is read; halting"
                ));
                cpu::halt_forever();
            }
        };
        if contents == MOTD_SEED {
            logger.error(format_args!(
                "ext2-corrupt: {what} still read back as the seed file; the comparison in \
                 verify_path_lookup would not have noticed; halting"
            ));
            cpu::halt_forever();
        }
        logger.info(format_args!(
            "ext2-corrupt: {what} -> read {} byte(s) that differ from the seed",
            contents.len()
        ));
        *rejected += 1;
    }
}

/// 抱えている像の先頭 [`CORRUPT_FS_BLOCKS`] ブロックを写し、それ自体で読み切れる
/// 像に直す（S10-a）。
///
/// **`s_blocks_count` を切り出した長さへ合わせる。** 直さないと `parse` が
/// [`common::ext2::Ext2Error::ImageTooSmall`] で拒み、**壊し方に関係なく
/// すべての case が同じ理由で落ちる。**
fn build_truncated_fs_image(buf: &mut [u8; CORRUPT_FS_LEN]) {
    buf.copy_from_slice(&FS_IMAGE[..CORRUPT_FS_LEN]);
    buf[FS_SUPERBLOCK + 4..FS_SUPERBLOCK + 8]
        .copy_from_slice(&(CORRUPT_FS_BLOCKS as u32).to_le_bytes());
}

/// 壊した像を組み立てる作業領域（S9-b-2）。
///
/// **スタックへ置かない。** 8 KiB を超える単一のローカル配列は
/// `docs/deferred-decisions.md` の「大きなスタック配列とガード幅」の解禁条件に
/// 当たる。**静的領域なら当たらない。**
static mut CORRUPT_IMAGE: [u8; HELLO_ELF.len()] = [0; HELLO_ELF.len()];

/// 壊し方 1 つ分の記述（S9-b-2）。
///
/// **1 か所だけを壊す。** 2 か所壊すと、どちらで拒まれたのかが分からない。
struct CorruptCase {
    /// 判定行に出す壊し方の説明。**「何をしたか」を書く。**
    what: &'static str,
    /// 書き換える位置（0 なら [`CorruptCase::truncate_to`] を使う）。
    offset: usize,
    /// 書き込む値（リトルエンディアン）。
    value: u64,
    /// 書き込む幅（バイト）。0 なら書き換えない。
    width: usize,
    /// 像をこの長さへ切り詰める（0 なら切り詰めない）。
    truncate_to: usize,
    /// 期待する拒否理由。
    expected: common::elf::ElfError,
}

/// 壊した像がパーサに拒まれることを確かめる（S9-b-2）。
///
/// # 既定ビルドで常に走らせる
///
/// **破壊 feature の中だけで壊すと、「壊す処理そのものが壊れている」ことに
/// 気づけない。** 既定で毎回壊し、毎回拒まれることを主張すれば、
/// **壊す側と拒む側の両方が守られる。**
///
/// # 像は 1 つ。壊すのは実行時である
///
/// 壊した像を埋め込みで増やす案は採らなかった。**`ElfError` は 11 種あり、
/// 種類ごとに像を持つと本数がそのまま費用になる**（`rustc` の呼び出しも
/// イメージの大きさも）。実行時に 1 バイトから 8 バイト書き換えれば全種を作れる。
///
/// **「壊した像がどこから来たか読めない」という欠点は、判定行に壊し方を
/// 書いて消してある**（`what` の欄）。
fn verify_corrupt_user_elf_is_rejected(logger: &mut Logger<SerialPort>) {
    use common::elf::{Elf, ElfError};

    const EI_CLASS: usize = 4;
    const EI_DATA: usize = 5;
    const E_TYPE: usize = 16;
    const E_MACHINE: usize = 18;
    const E_PHOFF: usize = 32;
    const E_PHENTSIZE: usize = 54;
    /// 先頭のプログラムヘッダの位置。`hello` の `e_phoff` は 64 である。
    const PHDR0: usize = 64;
    const P_OFFSET: usize = 8;
    const P_VADDR: usize = 16;
    const P_MEMSZ: usize = 40;

    let len = HELLO_ELF.len();
    let cases: [CorruptCase; 11] = [
        CorruptCase {
            what: "truncated to 32 bytes",
            offset: 0,
            value: 0,
            width: 0,
            truncate_to: 32,
            expected: ElfError::TooShort,
        },
        CorruptCase {
            what: "magic byte 0 set to 0",
            offset: 0,
            value: 0,
            width: 1,
            truncate_to: 0,
            expected: ElfError::BadMagic,
        },
        CorruptCase {
            what: "EI_CLASS set to ELFCLASS32",
            offset: EI_CLASS,
            value: 1,
            width: 1,
            truncate_to: 0,
            expected: ElfError::NotElf64,
        },
        CorruptCase {
            what: "EI_DATA set to big endian",
            offset: EI_DATA,
            value: 2,
            width: 1,
            truncate_to: 0,
            expected: ElfError::NotLittleEndian,
        },
        CorruptCase {
            what: "e_type set to ET_REL",
            offset: E_TYPE,
            value: 1,
            width: 2,
            truncate_to: 0,
            expected: ElfError::NotExecutable,
        },
        CorruptCase {
            what: "e_machine set to EM_386",
            offset: E_MACHINE,
            value: 3,
            width: 2,
            truncate_to: 0,
            expected: ElfError::NotX86_64,
        },
        CorruptCase {
            what: "e_phoff pushed past the end",
            offset: E_PHOFF,
            value: len as u64 + 0x1000,
            width: 8,
            truncate_to: 0,
            expected: ElfError::ProgramHeaderOutOfBounds,
        },
        CorruptCase {
            what: "e_phentsize set to 8",
            offset: E_PHENTSIZE,
            value: 8,
            width: 2,
            truncate_to: 0,
            expected: ElfError::BadProgramHeaderEntrySize,
        },
        CorruptCase {
            what: "p_offset of the first segment pushed past the end",
            offset: PHDR0 + P_OFFSET,
            value: len as u64 + 1,
            width: 8,
            truncate_to: 0,
            expected: ElfError::SegmentFileRangeOutOfBounds,
        },
        CorruptCase {
            what: "p_memsz of the first segment made smaller than p_filesz",
            offset: PHDR0 + P_MEMSZ,
            value: 1,
            width: 8,
            truncate_to: 0,
            expected: ElfError::SegmentMemorySmallerThanFile,
        },
        CorruptCase {
            what: "p_vaddr of the first segment set to u64::MAX",
            offset: PHDR0 + P_VADDR,
            value: u64::MAX,
            width: 8,
            truncate_to: 0,
            expected: ElfError::SegmentAddressOverflow,
        },
    ];

    let mut rejected = 0usize;
    for case in cases {
        // 毎回、健全な像から作り直す。**前の壊し方が残らないようにする。**
        // SAFETY: 起動時の単一実行文脈で、この静的領域を触るのはここだけである。
        let image = unsafe {
            let buf = &mut *core::ptr::addr_of_mut!(CORRUPT_IMAGE);
            buf.copy_from_slice(HELLO_ELF);
            for i in 0..case.width {
                buf[case.offset + i] = ((case.value >> (i * 8)) & 0xFF) as u8;
            }
            if case.truncate_to != 0 {
                &buf[..case.truncate_to]
            } else {
                &buf[..]
            }
        };

        match Elf::parse(image) {
            Ok(_) => {
                logger.error(format_args!(
                    "user-elf-corrupt: {} was accepted; expected {:?}; halting",
                    case.what, case.expected
                ));
                cpu::halt_forever();
            }
            Err(e) if e != case.expected => {
                logger.error(format_args!(
                    "user-elf-corrupt: {} was rejected as {e:?}, expected {:?}; halting",
                    case.what, case.expected
                ));
                cpu::halt_forever();
            }
            Err(e) => {
                logger.info(format_args!("user-elf-corrupt: {} -> {e:?}", case.what));
                rejected += 1;
            }
        }
    }

    logger.info(format_args!(
        "user-elf-corrupt: all {rejected} corrupted image(s) were rejected with the expected \
         reason, and the kernel continued (the good image is untouched; each case patches a \
         fresh copy)"
    ));

    // 壊した後も健全な像が読めること。**壊す処理が元を汚していないことの主張である。**
    if Elf::parse(HELLO_ELF).is_err() {
        logger.error(format_args!(
            "user-elf-corrupt: the good image no longer parses after the corruption pass; halting"
        ));
        cpu::halt_forever();
    }
}

/// `hello` の `ud2` が entry から何バイト目にあるか（S9-b-1、S9-b-3-1 で移った）。
///
/// **`kernel/userland/hello.rs` の `.org 0x30` と対になっている。** 値を 2 か所で
/// 持つが、**食い違えば下の判定行が落ちる**ので静かには残らない。
///
/// **役割が変わった。** S9-b-1 では `hello` の出口そのもの（畳んで戻る）だったが、
/// S9-b-3-1 で出口は `exit` になった。**いまは `exit` が効かなかったときの受け皿で
/// ある。** 既定ビルドでここへは来ない。来たら `user-run` の判定行が止める。
const HELLO_UD2_OFFSET: u64 = 0x30;

/// `hello` が `write` で送るはずのバイト列（S9-b-1）。
const HELLO_MESSAGE: &str = "hello from ring 3\n";

/// `fault-test` が書きに行く番地（S9-b-3-2a）。**自分の `.text` の先頭である。**
///
/// **`kernel/userland/user.ld` のリンク先と、`fault-test.rs` の即値と対になって
/// いる。** 3 か所で同じ値を持つが、**食い違えば CR2 の突き合わせが落ちる。**
const FAULT_TEST_TARGET: u64 = 0x0040_0000;

/// `fault-test` の書き込み命令が entry から何バイト目にあるか（S9-b-3-2a）。
///
/// **`kernel/userland/fault-test.rs` の `.org 0x20` と対になっている。**
const FAULT_TEST_STORE_OFFSET: u64 = 0x20;

/// `fault-test` の受け皿の `ud2` が entry から何バイト目にあるか（S9-b-3-2a）。
///
/// **書けてしまったときの行き先である。** 既定ビルドでここへは来ない。
/// 来たらベクタが 6 になり、判定行が食い違いとして止める。
const FAULT_TEST_UD2_OFFSET: u64 = 0x30;

/// `syscall-test` が `write` で送るはずのバイト列（S9-b-3-2a）。
const SYSCALL_TEST_MESSAGE: &str = "syscall-test wrote this\n";

/// `syscall-test` の受け皿の `ud2` が entry から何バイト目にあるか（S9-b-3-2a）。
///
/// # 出所は `user.ld` 1 つである（S10-b の締めで直した）
///
/// **以前は `.org` の即値と、この定数の 2 か所に同じ値があった。**
/// 検算を足してコードが伸びるたびに両方を直すことになり、**S10-b で 3 度起きた**
/// （0x100 → 0x200 → 0x400 → 0x800。実測で 310 / 743 / 1198 バイト）。
///
/// **いまはリンカが `.userland.receiver` を置き、`build.rs` が `user.ld` から
/// 読んだ値をここへ生成する。** 直す場所は `user.ld` の 1 行だけである。
///
/// **収まらなくなったときに静かには通らない性質は残る**——`.text` が受け皿の
/// 位置を越えると、**リンカが「位置カウンタを戻せない」で落ちる。**
const SYSCALL_TEST_UD2_OFFSET: u64 = userland_layout::USER_RECEIVER_OFFSET;

/// `build.rs` が `userland/user.ld` から生成した配置の定数（S10-b）。
mod userland_layout {
    include!(concat!(env!("OUT_DIR"), "/userland_layout.rs"));
}

/// `syscall-test` の終了状態の意味（S9-b-3-2a）。
///
/// # 値に意味がある
///
/// **どの検算が落ちたかは、この値でしか分からない。** `user-exit-wrong-status` の
/// 期待マーカーに値を入れなかったのとは逆で、**こちらは値そのものが情報である。**
/// 判定行が 0 以外の終了状態を出すときは、この対応を引いて意味も出す。
///
/// **`kernel/userland/syscall-test.rs` の doc と対になっている。**
const SYSCALL_TEST_STATUS: &[(u64, &str)] = &[
    (1, "the probe return value was not PROBE_RETURN"),
    (2, "write did not return the number of bytes it was given"),
    (3, "the unimplemented number did not return -ENOSYS"),
    (
        4,
        "open(\"/etc/motd\", O_RDONLY) did not return descriptor 3",
    ),
    (5, "close(3) did not return 0"),
    (6, "the open right after close did not reuse descriptor 3"),
    (7, "open(\"/nope\") did not return -ENOENT"),
    (8, "open(\"/etc/motd\", O_WRONLY) did not return -EROFS"),
    (
        9,
        "the second close of the same descriptor did not return -EBADF",
    ),
    (10, "open(NULL) did not return -EFAULT"),
    (11, "reading all of /etc/motd did not return 18 bytes"),
    (12, "the bytes read back did not match the known contents"),
    (13, "reading at the end of the file did not return 0"),
    (14, "the short read did not return 5 matching bytes"),
    (
        15,
        "the follow-up read did not return the remaining 13 matching bytes",
    ),
    (16, "reading a directory did not return -EISDIR"),
    (17, "reading a closed descriptor did not return -EBADF"),
    (18, "stat(\"/etc/motd\") did not return 0"),
    (19, "st_size was not 18"),
    (20, "st_mode did not say regular file"),
    (21, "st_blocks was not 8 (512-byte units)"),
    (22, "st_mode for /etc did not say directory"),
    (23, "stat(\"/nope\") did not return -ENOENT"),
    (24, "getdents64 on / did not fill the buffer"),
    (25, "the root listing did not have 6 entries"),
    (26, "a d_reclen was not a multiple of 8"),
    (
        27,
        "d_type did not separate the regular file from the directories",
    ),
    (28, "getdents64 at the end did not return 0"),
    (
        29,
        "getdents64 with a buffer too small for one record did not return -EINVAL",
    ),
    (30, "argc was not 2"),
    (31, "argv[0] was not \"syscall-test\""),
    (32, "argv[1] was not \"alpha\""),
    (33, "the argv terminator was not NULL"),
    (34, "the envp terminator was not NULL"),
    (35, "the auxv terminator (AT_NULL) was missing"),
    (36, "spawn(\"/bin/hello\") did not return 0"),
    (37, "spawn(\"/nope\") did not return -ENOENT"),
    (38, "spawn(\"/etc\") did not return -EISDIR"),
    (39, "spawn(NULL) did not return -EFAULT"),
    (
        40,
        "spawn(\"/bin/spawn-test\") did not return 0; the grandchild was not refused",
    ),
    (41, "spawn(path, NULL) did not return -EFAULT"),
    (
        42,
        "an argv with more entries than the limit did not return -E2BIG",
    ),
    (
        43,
        "an argv whose total length is too big did not return -E2BIG",
    ),
    (
        44,
        "write(0, ...) did not return the number of bytes it was given; 0 is the same terminal",
    ),
    (45, "write(3, ...) did not return -EBADF"),
    (
        46,
        "write(2, ...) did not return the number of bytes it was given",
    ),
    (
        47,
        "a write longer than the 64-byte record did not return the number of bytes it was given",
    ),
    (51, "read(0) did not return -EAGAIN with no keys pending"),
    (52, "read(1) did not return -EAGAIN; 1 is the same terminal"),
    (53, "read(3) did not return -EBADF; nothing is open there"),
    (48, "spawn(\"/bin/ls\", [\"ls\"]) did not return 0"),
    (
        49,
        "spawn(\"/bin/cat\", [\"cat\", \"/etc/motd\"]) did not return 0",
    ),
    (
        50,
        "spawn(\"/bin/cat\", [\"cat\"]) did not return 2; cat did not refuse the missing argument",
    ),
];

/// `fault-test` が起こす #PF のエラーコード（S9-b-3-2a）。
///
/// `P`（bit 0）| `W`（bit 1）| `U`（bit 2）= 7。**「不在」ではなく「権限違反」で
/// あることを、この値で言っている**——ページは在る（`P=1`）が、書けない
/// （`W=1` は書きでの違反を指す）。**S7 の到達条件 3 の観測が使っているのと
/// 同じ区別である。**
const FAULT_TEST_ERROR_CODE: u64 = 0b111;

/// プロセスの終わり方（S9-b-3-2a）。**期待する側の記述である。**
///
/// # なぜ表に持たせるか
///
/// **プログラムごとに正しい終わり方が違う。** `hello` は `exit(0)` で終わるのが
/// 正しく、`fault-test` は畳まれて終わるのが正しい。**「畳んで戻った」を一律に
/// 失敗として扱うと、後者を正しく終わらせられない。**
///
/// **終わり方は観測される量であって、判定はここが持つ。** 観測は
/// [`kernel::ring3`] と [`kernel::syscall`] の記録から読む。
enum UserProgramOutcome {
    /// `exit(status)` で終わる。
    Exit {
        /// 期待する終了状態。
        status: u64,
    },
    /// Ring 3 の違反が畳まれて終わる。
    Fold {
        /// 期待するベクタ。
        vector: u64,
        /// フォルトした命令の位置（entry からの相対）。
        rip_offset: u64,
        /// 期待する CR2。**ベクタが 14 のときだけ突き合わせる。**
        cr2: u64,
        /// 期待するエラーコード。
        error_code: u64,
    },
}

/// 走らせるプログラム 1 本分の記述（S9-b-3-2a）。
///
/// **静的な記述であって、管理構造ではない**（[`UserProcess`] の doc）。
struct UserProgram {
    /// 判定行に出す名前。
    name: &'static str,
    /// 埋め込んだ像。
    image: &'static [u8],
    /// 期待する終わり方。
    outcome: UserProgramOutcome,
    /// 期待した終わり方が起きなかったときに実行が落ちる先（entry からの相対）。
    ///
    /// **破壊が成功した後の行き先を、破壊と一緒に用意する**（`coding-standards.md`）。
    /// どのプログラムも、そこに `ud2` を置いてある。**判定行はこの位置を出す**
    /// ので、「期待した終わり方が起きず、受け皿へ落ちた」が RIP で分かる。
    receiver_offset: u64,
    /// `write` で届くはずのバイト列。**発行しないなら `None`。**
    expected_write: Option<&'static str>,
    /// **`ADR-0020` の ABI の契約を probe で確かめるプログラムか**（S9-b-3-2a）。
    ///
    /// 真なら、カーネル側が受け取った 6 引数を [`kernel::syscall::PROBE_ARGS`] と
    /// 突き合わせる。**確かめているのは `dispatch` ではなく、ユーザーが asm で
    /// 組み立てた引数が規約どおりのレジスタで届くことである。**
    probes_abi: bool,
    /// 0 以外の終了状態の意味（S9-b-3-2a）。**空なら値に意味を持たせていない。**
    status_meanings: &'static [(u64, &'static str)],
    /// 初期スタックへ積む `argv`（S11-1）。
    ///
    /// **`argv[0]` はプログラム名である**——Unix の慣行であって、
    /// **カーネルが強制するものではない**（`execve` は呼び出し側に決めさせる）。
    argv: &'static [&'static [u8]],
}

/// 走らせるプログラムの一覧（S9-b-3-1、S9-b-3-2a で期待を持たせた）。
///
/// **順に走らせる。1 本が終わってから次の 1 本が始まる。**
///
/// # 順序に意味がある
///
/// **畳まれて終わるプログラムの後ろに、正常に終わるプログラムを置く。**
/// そうすると「1 本が畳まれて終わり、**次の 1 本が始まって**正常に終わる」が
/// 1 回の起動で観測できる。**逆順では、畳んだ後に何かが始まるところを見せられない。**
/// これは S8 が言い換えた到達条件（中断ではなく終了であること）の観測にあたる。
///
/// `syscall-test` は S9-b-3-2a の 2 本目で足し、`fault-test` の後ろに置く。
const USER_PROGRAMS: &[UserProgram] = &[
    UserProgram {
        name: "hello",
        image: HELLO_ELF,
        outcome: UserProgramOutcome::Exit { status: 0 },
        receiver_offset: HELLO_UD2_OFFSET,
        expected_write: Some(HELLO_MESSAGE),
        probes_abi: false,
        status_meanings: &[],
        argv: &[b"hello"],
    },
    UserProgram {
        name: "fault-test",
        image: FAULT_TEST_ELF,
        outcome: UserProgramOutcome::Fold {
            vector: 14,
            rip_offset: FAULT_TEST_STORE_OFFSET,
            cr2: FAULT_TEST_TARGET,
            error_code: FAULT_TEST_ERROR_CODE,
        },
        receiver_offset: FAULT_TEST_UD2_OFFSET,
        expected_write: None,
        probes_abi: false,
        status_meanings: &[],
        argv: &[b"fault-test"],
    },
    UserProgram {
        name: "syscall-test",
        image: SYSCALL_TEST_ELF,
        outcome: UserProgramOutcome::Exit { status: 0 },
        receiver_offset: SYSCALL_TEST_UD2_OFFSET,
        expected_write: Some(SYSCALL_TEST_MESSAGE),
        probes_abi: true,
        status_meanings: SYSCALL_TEST_STATUS,
        // **2 要素にしてある。** `argc` が 1 のままだと、
        // **「積んでいない」と「1 つ積んだ」が区別できない。**
        argv: &[b"syscall-test", b"alpha"],
    },
];

/// 今の空きフレーム数（S11-3）。**会計のために短く借りて、すぐ返す。**
///
/// **借りられないのは異常である**（`ADR-0030`）。起動シーケンスは単一コアの
/// 直線なので、**ここで `None` が返るなら誰かが返し忘れている。**
fn frame_count_now(logger: &mut Logger<SerialPort>) -> u64 {
    let Some(allocator) = kernel::frame_allocator::take() else {
        logger.error(format_args!(
            "frame-allocator: the allocator is on loan while the boot sequence needs it; \
             someone did not give it back. halting"
        ));
        cpu::halt_forever();
    };
    let count = allocator.free_frame_count();
    kernel::frame_allocator::give_back(allocator);
    count
}

/// 今の空き範囲の数（S11-3）。**同じく短く借りて返す。**
fn free_range_count_now(logger: &mut Logger<SerialPort>) -> usize {
    let Some(allocator) = kernel::frame_allocator::take() else {
        logger.error(format_args!(
            "frame-allocator: the allocator is on loan while the boot sequence needs it; \
             someone did not give it back. halting"
        ));
        cpu::halt_forever();
    };
    let count = allocator.free_range_count();
    kernel::frame_allocator::give_back(allocator);
    count
}

/// 埋め込んだユーザープログラムを順に走らせる（S9-b-1、S9-b-3-2a で複数になった）。
///
/// **1 本ずつ、生成から破棄まで閉じてから次へ行く。** 期待どおりに終わった
/// プログラムは失敗ではない——`fault-test` は畳まれて終わるのが正しい
/// （[`USER_PROGRAMS`]）。**期待と違う終わり方をしたときだけ止まる。**
fn load_embedded_user_program(logger: &mut Logger<SerialPort>) -> Result<(), UserLoadError> {
    for program in USER_PROGRAMS {
        let name = program.name;
        // **会計のために短く借りる（S11-3）。** 読むだけなので、すぐ返す。
        let free_before = frame_count_now(logger);
        // **子の会計を 0 に戻す（S11-5）。** このプログラムが `spawn` で起こした
        // 子の隔離は、下の突き合わせで足す。
        kernel::userland::reset_spawn_accounting();
        let (outcome, held, leaked) =
            load_user_program(logger, program.image, true, name, program.argv);
        let (child_held, child_leaked) = kernel::userland::spawn_accounting();
        let entry = outcome?;

        // **終わり方を判定する。** ここで止まっても空間は既に畳まれている
        // （`load_user_program` が成否によらず畳む）ので、会計はこの後で見られる。
        check_user_program_outcome(logger, program, entry)?;

        // **畳んだ会計。** 消えた枚数と隔離へ入れた枚数が一致すること。
        // **空きフレームの絶対値は出さない**（コア数で変わる。
        // `verify_corrupt_user_program_is_not_loaded` が同じ理由で差だけを出している）。
        // **主張の前に確かめる。** 先に「畳んだ」と書くと、会計が合わない場合に
        // **その行が偽のまま残る。**
        // **子が隔離へ入れたぶんを足す（S11-5）。** 隔離のフレームは世代が退くまで
        // アロケータへ戻らないので、**親から見ると消えたままである。**
        // **実測で踏んだ**——`syscall-test` が子を 2 本起こしたところ、24 枚消えて
        // 自分の隔離は 8 枚だった。差の 16 枚が子 2 本のぶんである。
        let consumed = (free_before - frame_count_now(logger)) as usize;
        let quarantined = held + child_held;
        let all_leaked = leaked + child_leaked;
        if consumed != quarantined || all_leaked != 0 {
            logger.error(format_args!(
                "user-load: {name} left the allocator short: {consumed} frame(s) consumed but \
                 {quarantined} quarantined ({held} its own + {child_held} from the process(es) \
                 it spawned, {all_leaked} leaked)"
            ));
            return Err(UserLoadError::DestroyAccounting {
                consumed,
                quarantined,
                leaked: all_leaked,
            });
        }
        logger.info(format_args!(
            "user-load: {name} ran as a process in its own address space, ended as expected, \
             and the kernel continued after the process was gone; the space was destroyed \
             ({consumed} frame(s) left the allocator and {quarantined} reached quarantine \
             ({held} its own + {child_held} spawned), match={} leaked={all_leaked})",
            consumed == quarantined
        ));

        // **空き範囲の数を別の行で出す。** フレームアロケータの容量（256）の
        // 見直しは「プロセスが任意の順で終了する形になるとき」が条件で、
        // **この段ではまだ足りている**（順に 1 本ずつなので同時生存は 1）。
        // **増え方が見えていなければ、足りなくなる時期も見えない。**
        //
        // **行を分けてあるのは、この値が起動ごとに揺れるからである**（UEFI の
        // メモリマップ由来。実測で 10 と 11 の両方が出た）。**上の会計と同じ行に
        // すると、起動ログの参照からその会計ごと落ちる。**
        let ranges = free_range_count_now(logger);
        logger.info(format_args!(
            "user-load: the allocator holds {ranges} free range(s) of {} after {name}",
            kernel::frame_allocator::DEFAULT_CAPACITY
        ));
    }
    Ok(())
}

/// **壊した像がローダーの中で拒まれ、後始末まで済むことを確かめる（S9-b-2）。**
///
/// # パーサで止まる種類とは別の経路である
///
/// `verify_corrupt_user_elf_is_rejected` の 11 種はすべて `Elf::parse` が拒む。
/// **あれらはローダーへ入らないので、`UserLoadError` の経路を 1 度も通らない。**
/// **`Result` にした意味はここで初めて出る。**
///
/// 3 つ置く。**落ちる場所が違う。**
///
/// - 入口で落ちる（`Parse`）。写像は 1 枚も張られていない
/// - **途中で落ちる（`Mapping { NotPrivate }`）。** 1 本目の区画は張り終わって
///   おり、**2 本目で拒まれる。そこまでに張ったものの後始末が要る**
/// - **途中で落ちる（`Mapping { AlreadyMapped }`、S9-b-3-2b）。** 区画が同じ
///   4KiB ページを共有する像である。**以前はこれが拒まれず、上書きして
///   1 枚漏らしていた**（`docs/verification-coverage.md` の「ELF の検査を
///   3 つに分ける」）
///
/// 途中で落ちる像は、**2 本目の `p_vaddr` を動かして作る。** パーサは `p_vaddr`
/// の範囲も区画の重なりも見ない（配置の方針を知らないため。`common::elf` の
/// モジュール doc）ので、**パースは通り、写像で拒まれる。** 行き先は
/// ユーザーサブツリーの外（`PML4[1]`）と、1 本目の区画のページの中である。
fn verify_corrupt_user_program_is_not_loaded(logger: &mut Logger<SerialPort>) {
    /// 先頭のプログラムヘッダの位置（`hello` の `e_phoff` は 64）。
    const PHDR0: usize = 64;
    /// `Elf64_Phdr` の大きさ。
    const PHDR_SIZE: usize = 56;
    /// `p_vaddr` のオフセット。
    const P_VADDR: usize = 16;

    let free_before = frame_count_now(logger);
    let mut quarantined_total = 0usize;

    for (what, offset, value, expect_mapping) in [
        ("magic byte 0 set to 0", 0usize, 0u64, false),
        (
            "p_vaddr of the second segment moved out of the user subtree",
            PHDR0 + PHDR_SIZE + P_VADDR,
            1u64 << 39,
            true,
        ),
        (
            "p_vaddr of the second segment moved into the first segment's page",
            PHDR0 + PHDR_SIZE + P_VADDR,
            0x0040_0030u64,
            true,
        ),
    ] {
        // SAFETY: 起動時の単一実行文脈で、この静的領域を触るのはここだけである。
        let image = unsafe {
            let buf = &mut *core::ptr::addr_of_mut!(CORRUPT_IMAGE);
            buf.copy_from_slice(HELLO_ELF);
            let width = if offset == 0 { 1 } else { 8 };
            for i in 0..width {
                buf[offset + i] = ((value >> (i * 8)) & 0xFF) as u8;
            }
            &buf[..]
        };

        let (outcome, held, leaked) =
            load_user_program(logger, image, false, "corrupt", &[b"corrupt"]);

        let Err(error) = outcome else {
            logger.error(format_args!(
                "user-load-corrupt: {what} was loaded; expected a failure; halting"
            ));
            cpu::halt_forever();
        };

        let matches = match error {
            UserLoadError::Parse(_) => !expect_mapping,
            UserLoadError::Mapping { .. } => expect_mapping,
            _ => false,
        };
        logger.info(format_args!(
            "user-load-corrupt: {what} -> {error:?} (the space was destroyed: quarantined={held} \
             leaked={leaked})"
        ));
        if !matches {
            logger.error(format_args!(
                "user-load-corrupt: {what} failed in the wrong place; halting"
            ));
            cpu::halt_forever();
        }
        quarantined_total += held;
        if leaked != 0 {
            logger.error(format_args!(
                "user-load-corrupt: {what} leaked {leaked} frame(s) on the failure path; halting"
            ));
            cpu::halt_forever();
        }
    }

    // **消えた枚数と隔離へ入れた枚数が一致すること。**
    //
    // 絶対値は出さない。**空きフレーム数はコア数で変わる**（AP ごとに per-CPU
    // スタックを取る）ので、出すと `-smp 1/2/4` で起動ログが一致しなくなる。
    // **実測で踏んだ。差だけならコア数に依らない。**
    let consumed = (free_before - frame_count_now(logger)) as usize;
    logger.info(format_args!(
        "user-load-corrupt: all 3 corrupted images were refused by the loader (one at the \
         entrance, one after the first segment was already mapped, one whose segments share a \
         page) and the kernel continued; {consumed} frame(s) left the allocator and \
         {quarantined_total} reached quarantine (match={})",
        consumed == quarantined_total
    ));
    if consumed != quarantined_total {
        logger.error(format_args!(
            "user-load-corrupt: {consumed} frame(s) left the allocator but only \
             {quarantined_total} reached quarantine; the failure path lost the rest; halting"
        ));
        cpu::halt_forever();
    }
}

/// 走り終えたプロセスが、期待どおりに終わったかを判定する（S9-b-3-2a）。
///
/// # 戻ってきた理由は 2 つに 1 つである
///
/// **終了**（`exit` が `ring3::leave_ring3` を呼んだ）か、**畳み**（Ring 3 由来の
/// 違反を S8 の機構が受けた）である。`ring3::enter` はこの 2 つの longjmp でしか
/// 戻らない。どちらであるべきかは [`UserProgram::outcome`] が持つ。
///
/// # 観測と判定を分けてある
///
/// 観測は [`kernel::ring3`] と [`kernel::syscall`] の記録から読む。**走らせる側
/// （`load_user_program_into`）は判定しない**——プログラムごとに正しい終わり方が
/// 違い、それは一覧を持つ側の知識である。`ring3::enter` が畳んだ位置を主張せず
/// 呼び出し側に委ねているのと同じ形である。
fn check_user_program_outcome(
    logger: &mut Logger<SerialPort>,
    program: &UserProgram,
    entry: u64,
) -> Result<(), UserLoadError> {
    let name = program.name;
    let exited = kernel::syscall::process_exited();
    let status = kernel::syscall::process_exit_status();
    let folded = kernel::ring3::folded();
    let vector = kernel::ring3::fault_vector();
    let rip = kernel::ring3::fault_rip();
    let cs = kernel::ring3::fault_cs();
    let cr2 = kernel::ring3::fault_cr2();
    let error_code = kernel::ring3::fault_error_code();
    let mut bytes = [0u8; kernel::syscall::WRITE_BUF_LEN];
    let written = kernel::syscall::last_write_bytes(&mut bytes);
    let message = core::str::from_utf8(&bytes[..written]).unwrap_or("<not utf-8>");

    logger.info(format_args!(
        "user-run: {name} left Ring 3 (exited={exited} status={status} folded={folded} \
         vector={vector} rip={rip:#x} cs={cs:#x} cr2={cr2:#x} err={error_code:#x}), it made {} \
         syscall(s) and the kernel was entered from Ring 3 ({}), write(fd={}, {written} byte(s)) \
         said {:?}",
        kernel::syscall::invocation_count(),
        kernel::syscall::in_ring3_at_entry(),
        kernel::syscall::last_write_fd(),
        message.trim_end()
    ));

    if !exited && !folded {
        logger.error(format_args!(
            "user-run: {name} came back from Ring 3 without exiting and without folding"
        ));
        return Err(UserLoadError::NoExitNoFold);
    }

    match program.outcome {
        UserProgramOutcome::Exit {
            status: expected_status,
        } => {
            // **畳んで戻ったなら、`exit` が効かなかったということである。** 受け皿の
            // `ud2` は entry + [`HELLO_UD2_OFFSET`] にあり、そこまで出して原因を絞る。
            if folded {
                let receiver_rip = entry + program.receiver_offset;
                logger.error(format_args!(
                    "user-run: {name} folded instead of exiting (vector={vector} rip={rip:#x} \
                     cs={cs:#x}); the trailing ud2 sits at {receiver_rip:#x} (entry + {:#x}), so \
                     exit did not take effect",
                    program.receiver_offset
                ));
                return Err(UserLoadError::DidNotExit);
            }
            if status != expected_status {
                // **意味を持たせてある終了状態なら、意味も出す。** 値だけでは
                // どの検算が落ちたか分からない（[`SYSCALL_TEST_STATUS`]）。
                let meaning = program
                    .status_meanings
                    .iter()
                    .find(|(value, _)| *value == status)
                    .map_or("no meaning is recorded for this value", |(_, text)| *text);
                logger.error(format_args!(
                    "user-run: {name} exited with status {status}, expected \
                     {expected_status} ({meaning})"
                ));
                return Err(UserLoadError::ExitStatus(status));
            }
        }
        UserProgramOutcome::Fold {
            vector: expected_vector,
            rip_offset,
            cr2: expected_cr2,
            error_code: expected_error_code,
        } => {
            // **終了して戻ったなら、起こすはずの違反が起きなかったということである。**
            if !folded {
                logger.error(format_args!(
                    "user-run: {name} exited with status {status} instead of faulting; the \
                     violation it is supposed to raise did not happen"
                ));
                return Err(UserLoadError::DidNotFold);
            }
            let expected_rip = entry + rip_offset;
            // **CR2 とエラーコードは #PF のときだけ意味を持つ**（`ring3.rs` の
            // `FAULT_CR2` の doc）。ベクタが食い違っている時点で、残りの
            // 突き合わせは意味を失うので、まとめて 1 つの食い違いとして出す。
            let matched = vector == expected_vector
                && rip == expected_rip
                && (cs & 0b11) == 3
                && (vector != 14 || (cr2 == expected_cr2 && error_code == expected_error_code));
            if !matched {
                logger.error(format_args!(
                    "user-run: {name} folded somewhere else (vector={vector} rip={rip:#x} \
                     cs={cs:#x} cr2={cr2:#x} err={error_code:#x}), expected vector \
                     {expected_vector} at {expected_rip:#x} (entry + {rip_offset:#x}) from Ring 3 \
                     with cr2={expected_cr2:#x} err={expected_error_code:#x}; the receiver ud2 \
                     sits at {receiver_rip:#x}, so landing there means the violation never \
                     happened",
                    receiver_rip = entry + program.receiver_offset
                ));
                return Err(UserLoadError::FoldMismatch);
            }
        }
    }

    // **ABI の契約を確かめるプログラムなら、届いた引数を突き合わせる。**
    if program.probes_abi {
        let seen = kernel::syscall::probe_seen_args();
        if !kernel::syscall::probe_invoked() {
            logger.error(format_args!(
                "user-run: {name} was supposed to issue the probe, but the kernel never saw it"
            ));
            return Err(UserLoadError::AbiMismatch);
        }
        for (index, (&got, &expected)) in seen
            .iter()
            .zip(kernel::syscall::PROBE_ARGS.iter())
            .enumerate()
        {
            if got != expected {
                logger.error(format_args!(
                    "user-run: {name} argument register mismatch (arg{index} seen {got:#x}, \
                     expected {expected:#x}); the ABI contract of ADR-0020 does not hold for \
                     arguments assembled in user code"
                ));
                return Err(UserLoadError::AbiMismatch);
            }
        }
        logger.info(format_args!(
            "user-abi: {name} issued the probe from user assembly and all 6 arguments arrived \
             per ADR-0020 (arg4 came from R10, not RCX, which carried a sentinel); this checks \
             the ABI contract itself, not the dispatcher — the boot-time probe assembles its \
             arguments in Rust inside the kernel, this one assembles them in assembly, links, \
             loads and enters Ring 3 like any program"
        ));
    }

    // **`write` の中身は、送るはずのプログラムについてだけ見る。**
    // 送らないプログラムには「送っていないこと」を要求する（会計は
    // `syscall::reset_counters` が走るたびに戻るので、前のプロセスの記録は残らない）。
    match program.expected_write {
        Some(expected) => {
            if kernel::syscall::last_write_fd() != 1 || message != expected {
                logger.error(format_args!(
                    "user-run: {name} write did not deliver the expected bytes (fd={}, got \
                     {message:?}, expected {expected:?})",
                    kernel::syscall::last_write_fd()
                ));
                return Err(UserLoadError::WriteMismatch);
            }
        }
        None => {
            if written != 0 {
                logger.error(format_args!(
                    "user-run: {name} is not supposed to write, but {written} byte(s) arrived \
                     ({message:?})"
                ));
                return Err(UserLoadError::WriteMismatch);
            }
        }
    }

    Ok(())
}

/// 本番のアドレス空間で使うユーザー空間の PML4 インデックス。空きの下位半分の先頭。
///
/// # 値は暫定のままである。理由は S7-e で変わった
///
/// **かつての理由は「カーネルが PML4[0] に恒等で居るので PML4[0] を空けられない」
/// だった。これは B-2b で成立しなくなっている**（カーネルは上位半分へ移り、恒等は
/// 落ちている。[`kernel::address_space`] のモジュール doc）。**PML4[0] は空いており、
/// ユーザー空間を通常の低位へ広げること自体は今できる。**
///
/// **それでも動かしていないのは、動かした先が正しいかを確かめる相手がいないため
/// である**（S7-e で見送った理由）。値を変えても、その値でプログラムが走るところを
/// 誰も見ていなければ、検査の無い変更になる。**解禁条件は
/// 「ユーザープログラムを実際に置き、そのアドレスが正しいかを確かめる相手が
/// できたとき」である**（`docs/deferred-decisions.md`）。
///
/// # これは「本番の空間の」添字であって、「すべての空間の」ではない
///
/// S7-e 以降、[`kernel::address_space::AddressSpace`] は自分のユーザーサブツリーの
/// 添字を持つ。**プロセスごとに違ってよい。** ここにあるのは本番の空間の値である。
pub const USER_PML4_INDEX: usize = 1;

/// ユーザーページのマッピング能力を検証する（M5-e-2）。
///
/// 専用サブツリー（空き PML4[[`USER_PML4_INDEX`]]、仮想ベース 512 GiB）へ
/// [`ActivePageTable::map_4kib`] で U=1 のテストページを 1 枚張り、独立 walker で
/// 「ユーザーサブツリー全階層 U=1・カーネル側全 U=0」を実走査で確かめ、Ring 0 から
/// 既知値を書いて読み戻し、葉だけをアンマップする。中間テーブルは残す（M5-e-3 が
/// 同じサブツリーを再利用する。判断 b-i）。Ring 3 からのアクセスはまだ試さない。
///
/// `paging-test` ビルドは `unmap_4kib` を全体的にわざと壊すので、切り分け軸を一つに
/// 保つためこの検証は載せない。ユーザーマッピングの検証は分割/アンマップの回帰とは
/// 独立の関心事である。
#[cfg(not(feature = "paging-test"))]
fn verify_user_page_mapping<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut frame_allocator::FrameAllocator<CAP>,
) {
    use kernel::paging::active::{ActivePageTable, PageAttributes};
    use kernel::paging::verify;

    /// テストページの仮想アドレス（PML4[USER_PML4_INDEX] の先頭 = 512 GiB）。
    const USER_TEST_VIRT: u64 = 0x0000_0080_0000_0000;
    /// Ring 0 から書いて読み戻す既知値。
    const KNOWN: u64 = 0x00E2_C0DE_1234_5678;

    // テーブルの読み書きは恒等窓で行う（フレームはすべて恒等マッピング済み。
    // verify_split_and_unmap と同じ理由。テスト対象の U/S 照合は恒等に依存しない
    // ので B で恒等を外しても、窓を高位へ差し替えるだけでよい）。
    let identity = common::addr::DirectMap::identity(common::addr::DirectMap::IDENTITY_MAX_LENGTH)
        .expect("the identity window is canonical");

    let Some(leaf_phys) = allocator.allocate_frame() else {
        logger.error(format_args!(
            "user-map: could not reserve a leaf frame for the test user page; halting"
        ));
        cpu::halt_forever();
    };

    let virt = common::addr::VirtAddr::new(USER_TEST_VIRT)
        .expect("the user test virtual address is canonical");

    // SAFETY: CR3 は自前テーブルへ切り替え済みで、配下は恒等窓で読み書きできる。
    let mut table = unsafe { ActivePageTable::current(identity) };
    let pml4_phys = table.pml4_phys();

    // --- 張る（専用サブツリー、user=true で全階層 U=1） ---
    let attributes = PageAttributes {
        user: true,
        writable: true,
        cacheable: true,
    };
    // SAFETY: virt はまだマップされていない空き PML4 スロット配下。leaf_phys は
    // 今確保した未使用フレーム。allocator は中間テーブルの確保に使う。
    if let Err(e) = unsafe { table.map_4kib(virt, leaf_phys, attributes, allocator) } {
        logger.error(format_args!(
            "user-map: map_4kib({USER_TEST_VIRT:#x}) failed: {e:?}; halting"
        ));
        cpu::halt_forever();
    }

    // --- 独立 walker で物理対応を照合（構築側とは別のループ） ---
    // SAFETY: pml4_phys は稼働中 PML4、identity 窓でテーブルを読める。
    match unsafe { verify::walk(pml4_phys, identity, virt) } {
        Ok(r) if r.phys.as_u64() == leaf_phys.as_u64() && !r.huge => {}
        other => {
            logger.error(format_args!(
                "user-map: independent walk of {USER_TEST_VIRT:#x} did not resolve to the leaf \
                 {:#x} ({other:?}); halting",
                leaf_phys.as_u64()
            ));
            cpu::halt_forever();
        }
    }

    // --- U/S 監査（両側）。ユーザーサブツリー全 U=1、それ以外全 U=0 ---
    // SAFETY: 同上。テーブル全体を独立に歩くだけ。
    let audit = unsafe { verify::audit_user_supervisor(pml4_phys, identity, USER_PML4_INDEX) };
    logger.info(format_args!(
        "user-map: U/S audit: user subtree PML4[{USER_PML4_INDEX}] entries={} violations(U=0)={}, \
         kernel entries={} violations(U=1)={}",
        audit.user_entries, audit.user_violations, audit.kernel_entries, audit.kernel_violations
    ));
    if audit.user_violations != 0 || audit.kernel_violations != 0 {
        logger.error(format_args!(
            "user-map: U/S audit failed (user page not fully U=1, or a kernel entry leaked U=1); \
             halting"
        ));
        cpu::halt_forever();
    }

    // --- Ring 0 から既知値を書いて読み戻す（present・writable・到達可能） ---
    // SAFETY: virt は今張ったばかりの writable なページ。SMAP は未有効なので Ring 0 から
    // ユーザーページへアクセスできる。
    unsafe {
        core::ptr::write_volatile(virt.as_mut_ptr::<u64>(), KNOWN);
    }
    // SAFETY: 同上。直前に書いた値を読み戻す。
    let got = unsafe { core::ptr::read_volatile(virt.as_ptr::<u64>()) };
    if got != KNOWN {
        logger.error(format_args!(
            "user-map: read-back of the test user page mismatched (got {got:#x}, expected \
             {KNOWN:#x}); halting"
        ));
        cpu::halt_forever();
    }

    // --- 葉だけアンマップ。中間テーブルは残す（M5-e-3 が再利用） ---
    // SAFETY: virt は今張った 4KiB ページ。以後この仮想アドレスへはアクセスしない
    // （葉を落とした後の walk は TLB ではなくテーブルを読む）。
    if let Err(e) = unsafe { table.unmap_4kib(virt) } {
        logger.error(format_args!("user-map: unmap_4kib failed: {e:?}; halting"));
        cpu::halt_forever();
    }
    // 葉のフレームは解放してよい（中間は残す）。
    let _ = allocator.deallocate_frame(leaf_phys);

    // 葉が消えたこと（独立 walk が NotPresent）。
    // SAFETY: 同上。
    match unsafe { verify::walk(pml4_phys, identity, virt) } {
        Err(verify::WalkError::NotPresent) => {}
        other => {
            logger.error(format_args!(
                "user-map: the test user page is still resolvable after unmap ({other:?}); halting"
            ));
            cpu::halt_forever();
        }
    }

    // アンマップ後の再監査。中間 residue が PML4[USER_PML4_INDEX] に閉じ、カーネル側に
    // U=1 が漏れていないことを再確認する。
    // SAFETY: 同上。
    let after = unsafe { verify::audit_user_supervisor(pml4_phys, identity, USER_PML4_INDEX) };
    logger.info(format_args!(
        "user-map: after unmap: user subtree residue entries={} (intermediates kept for M5-e-3), \
         kernel violations(U=1)={}",
        after.user_entries, after.kernel_violations
    ));
    if after.kernel_violations != 0 {
        logger.error(format_args!(
            "user-map: a kernel entry leaked U=1 after unmap; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "user-map: user-page mapping verified (all levels U=1 for the user page, all kernel \
         entries U=0, Ring 0 read-back held; leaf unmapped, subtree kept for M5-e-3)"
    ));
}

/// 畳みが予期した位置で起きたかを主張する（S8-a）。
///
/// 畳みの判定はベクタと CS.RPL と遠征フラグだけを見る。**どこで畳まれたかを知って
/// いるのは遠征を組み立てた側なので、突き合わせはここで行う。**
/// 食い違ったら、記録した3つ（ベクタ・RIP・CS）を出して停止する。ハンドラの dump は
/// 畳んだ時点で通っていないため、この行が唯一の手がかりになる。
///
/// `what` はログの接頭辞（遠征ごとに `ring3` / `syscall` と使い分ける）。
///
/// **`paging-test` でも載せる。** 呼び出し元3つのうち [`verify_ring3_excursion`] だけが
/// cfg で落ち、`verify_syscall_roundtrip` と [`issue_ptr_len_syscall`] は関数自体が
/// 残る（落ちるのは呼び出し側）。ここを落とすとその2つがコンパイルできない。
fn assert_folded_at(
    logger: &mut Logger<SerialPort>,
    what: &str,
    expected_vector: u64,
    expected_rip: u64,
) {
    use kernel::ring3;

    let vector = ring3::fault_vector();
    let rip = ring3::fault_rip();
    if vector == expected_vector && rip == expected_rip {
        return;
    }
    logger.error(format_args!(
        "{what}: folded at an unexpected place (vector={vector} rip={rip:#018x} \
         cs={:#x}), expected vector={expected_vector} rip={expected_rip:#018x}; halting",
        ring3::fault_cs()
    ));
    cpu::halt_forever();
}

/// Ring 3 への単発遠征を検証する（M5-e-3）。
///
/// M5-e-2 が残した PML4[[`USER_PML4_INDEX`]] サブツリーへ、ユーザーコード（`cli` 1 命令）
/// とユーザースタックの 2 ページを U=1 で張る。iretq で Ring 3 へ落ち、`cli` が #GP を
/// 起こし、`exception_entry` が予期と判定して畳んでここへ戻る。確かめるのは、RSP0 が
/// 実挙動で効くこと（#GP が遠征専用スタックへ切り替わったこと）、Ring 3 に落ちたこと、
/// 両側 U/S 監査が成立し続けることの3つである。
///
/// `paging-test` ビルドでは載せない（[`verify_user_page_mapping`] と同じ理由）。
#[cfg(not(feature = "paging-test"))]
fn verify_ring3_excursion<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut frame_allocator::FrameAllocator<CAP>,
) {
    use kernel::paging::active::{ActivePageTable, PageAttributes};
    use kernel::paging::verify;
    use kernel::ring3;

    let identity = common::addr::DirectMap::identity(common::addr::DirectMap::IDENTITY_MAX_LENGTH)
        .expect("the identity window is canonical");

    // ユーザーコード・ユーザースタック・読み取り専用ページの葉フレームを確保する。
    let (Some(code_phys), Some(stack_phys), Some(readonly_phys)) = (
        allocator.allocate_frame(),
        allocator.allocate_frame(),
        allocator.allocate_frame(),
    ) else {
        logger.error(format_args!(
            "ring3: could not reserve frames for the user code/stack/read-only pages; halting"
        ));
        cpu::halt_forever();
    };

    let code_virt = common::addr::VirtAddr::new(ring3::USER_CODE_VIRT)
        .expect("the user code virtual address is canonical");
    let stack_virt = common::addr::VirtAddr::new(ring3::USER_STACK_VIRT)
        .expect("the user stack virtual address is canonical");
    let readonly_virt = common::addr::VirtAddr::new(ring3::USER_READONLY_VIRT)
        .expect("the read-only user virtual address is canonical");

    // SAFETY: CR3 は自前テーブル。配下は恒等窓で読み書きできる。
    let mut table = unsafe { ActivePageTable::current(identity) };
    let pml4_phys = table.pml4_phys();

    // 2 ページを U=1 で張る（M5-e-2 残置の中間テーブルを再利用）。
    // 破壊 (ring3-test-user-page-supervisor): USER を落とす（U=0）。遠征前の両側監査が
    // user violation として捕まえる。
    #[cfg(not(feature = "ring3-test-user-page-supervisor"))]
    let user_flag = true;
    #[cfg(feature = "ring3-test-user-page-supervisor")]
    let user_flag = false;
    // 3 枚目は writable=false で張る（S9-a）。**Ring 3 からの書き込みが #PF に
    // なることを ring3-vectors の 6 本目が確かめる的である。**
    for (virt, phys, writable, what) in [
        (code_virt, code_phys, true, "code"),
        (stack_virt, stack_phys, true, "stack"),
        (readonly_virt, readonly_phys, false, "read-only"),
    ] {
        let attributes = PageAttributes {
            user: user_flag,
            writable,
            cacheable: true,
        };
        // SAFETY: いずれも未マップのユーザーサブツリー内アドレス。frame は未使用。
        if let Err(e) = unsafe { table.map_4kib(virt, phys, attributes, allocator) } {
            logger.error(format_args!(
                "ring3: map_4kib for the user {what} page failed: {e:?}; halting"
            ));
            cpu::halt_forever();
        }
    }

    // ユーザーコードへ cli(0xFA) を書き込む。NX を立てていないので実行可能。
    // SAFETY: code_virt は今張った writable なユーザーページ。SMAP は未有効。
    unsafe {
        core::ptr::write_volatile(code_virt.as_mut_ptr::<u8>(), 0xFA);
    }

    // 張った直後の両側 U/S 監査（遠征前）。
    // SAFETY: pml4_phys は稼働中 PML4、identity 窓で読める。
    let before = unsafe { verify::audit_user_supervisor(pml4_phys, identity, USER_PML4_INDEX) };
    if before.user_violations != 0 || before.kernel_violations != 0 {
        logger.error(format_args!(
            "ring3: U/S audit before the excursion failed (user violations={}, kernel \
             violations={}); halting",
            before.user_violations, before.kernel_violations
        ));
        cpu::halt_forever();
    }

    // 遠征前の RSP0（メインのカーネルスタック上端）。遠征後にここへ戻す。
    let main_rsp0_top = gdt::privilege_stack_top();
    let (exc_bottom, exc_top) = ring3::excursion_stack_range();

    logger.info(format_args!(
        "ring3: entering Ring 3 (user code {:#x} with cli, user stack top {:#x}, RSP0 -> \
         excursion stack [{exc_bottom:#x}, {exc_top:#x}))",
        ring3::USER_CODE_VIRT,
        ring3::USER_STACK_TOP
    ));

    // --- 遠征。iretq -> Ring 3 -> cli -> #GP -> 畳み -> ここへ戻る ---
    // cli はユーザーコード入口（USER_CODE_VIRT）に置いてあるので、予期する #GP の
    // フォルト RIP はそこである。
    // SAFETY: ユーザーページは張り済み。main_rsp0_top はメインの上端なので、遠征後に
    // RSP0 をそこへ戻せる。起動時の単一実行文脈から 1 回だけ呼ぶ。
    unsafe {
        ring3::enter(
            main_rsp0_top,
            ring3::USER_CODE_VIRT,
            ring3::USER_STACK_TOP,
            kernel::syscall::window_for_subtree(USER_PML4_INDEX),
        );
    }

    // --- 会計と検証 ---
    if !ring3::folded() {
        logger.error(format_args!(
            "ring3: returned from the excursion without folding an expected #GP; halting"
        ));
        cpu::halt_forever();
    }

    // 畳んだ位置の主張（S8-a）。判定側は位置を見ないので、予期と突き合わせるのは
    // ここである。cli はユーザーコード入口に置いたので、そこで #GP になるはず。
    assert_folded_at(logger, "ring3", 13, ring3::USER_CODE_VIRT);

    let fault_cs = ring3::fault_cs();
    let fault_rsp = ring3::fault_rsp();
    let handler_rsp = ring3::handler_rsp();

    // Ring 3 に落ちたこと: フォルト CS の RPL==3。
    let cs_rpl = fault_cs & 0b11;
    // フォルト時 RSP がユーザースタック範囲。
    let fault_in_user = fault_rsp > ring3::USER_STACK_VIRT && fault_rsp <= ring3::USER_STACK_TOP;
    // #GP ハンドラの RSP が遠征専用スタック範囲（RSP0 の実利用）。
    let handler_in_excursion = handler_rsp >= exc_bottom && handler_rsp < exc_top;
    // RSP0 がメインの上端へ戻っていること。
    let rsp0_restored = gdt::privilege_stack_top() == main_rsp0_top;

    logger.info(format_args!(
        "ring3: folded expected #GP. fault CS={fault_cs:#x} (RPL={cs_rpl}), fault RSP={fault_rsp:#x} \
         (in user stack={fault_in_user}), handler RSP={handler_rsp:#x} (in excursion \
         stack={handler_in_excursion}), RSP0 restored={rsp0_restored}"
    ));

    if cs_rpl != 3 {
        logger.error(format_args!(
            "ring3: the fault did not come from Ring 3 (CS RPL={cs_rpl}); halting"
        ));
        cpu::halt_forever();
    }
    if !fault_in_user {
        logger.error(format_args!(
            "ring3: fault RSP {fault_rsp:#x} is not in the user stack; halting"
        ));
        cpu::halt_forever();
    }
    if !handler_in_excursion {
        logger.error(format_args!(
            "ring3: #GP handler did not run on the RSP0 excursion stack (handler RSP \
             {handler_rsp:#x}); RSP0 did not take effect; halting"
        ));
        cpu::halt_forever();
    }
    if !rsp0_restored {
        logger.error(format_args!(
            "ring3: RSP0 was not restored to main; halting"
        ));
        cpu::halt_forever();
    }

    // 遠征後も両側 U/S 監査が成立すること（ユーザー 2 ページと中間が U=1、カーネルが U=0）。
    // SAFETY: 同上。
    let after = unsafe { verify::audit_user_supervisor(pml4_phys, identity, USER_PML4_INDEX) };
    logger.info(format_args!(
        "ring3: U/S audit after the excursion: user subtree entries={} violations={}, kernel \
         entries={} violations={}",
        after.user_entries, after.user_violations, after.kernel_entries, after.kernel_violations
    ));
    if after.user_violations != 0 || after.kernel_violations != 0 {
        logger.error(format_args!(
            "ring3: U/S audit after the excursion failed; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "ring3: Ring 3 excursion verified (fell to Ring 3, cli faulted as #GP, RSP0 switched to \
         the excursion stack, folded back to the kernel, permission split intact)"
    ));
}

/// int 0x80 システムコールの往復を検証する（M5-f-1-2）。
///
/// [`verify_ring3_excursion`] が残したユーザーコード/スタックページ
/// （`PML4[USER_PML4_INDEX]` サブツリー）を再利用する。ユーザーコードを
/// 「6 引数を既知値でセット → int 0x80 → 戻り値をユーザースタックへ store → cli」に
/// 書き換え、Ring 3 から probe システムコールを 1 回発行する。syscall_entry は
/// 番号と 6 引数を記録し、既知の戻り値 [`syscall::PROBE_RETURN`] を返す。iretq で
/// Ring 3 へ戻ると、その戻り値がユーザー RAX に入り、ユーザーがスタックへ store する。
/// 続く cli の #GP を予期の畳みでカーネルへ戻す。
///
/// 確かめること。
///
/// - 6 引数（RDI/RSI/RDX/R10/R8/R9）が規約どおり syscall_entry に届いたこと。記録した
///   6 値が発行側の既知値と一致すればよい。同じ式の自己検算ではなく、発行側の既知値と
///   ハンドラの独立読み戻しの突き合わせである
/// - 戻り値が RAX で Ring 3 へ返ったこと（ユーザースタックへ store された値が
///   [`syscall::PROBE_RETURN`] と一致）
/// - syscall_entry が RSP0（遠征）スタックで走ったこと
/// - 畳みで戻り RSP0 が復帰したこと
fn verify_syscall_roundtrip(logger: &mut Logger<SerialPort>) {
    use kernel::paging::active::{ActivePageTable, PageSize};
    use kernel::ring3;
    use kernel::syscall;

    let identity = common::addr::DirectMap::identity(common::addr::DirectMap::IDENTITY_MAX_LENGTH)
        .expect("the identity window is canonical");

    let code_virt = common::addr::VirtAddr::new(ring3::USER_CODE_VIRT)
        .expect("the user code virtual address is canonical");

    // verify_ring3_excursion が張ったユーザーコードページを再利用する。前段の副作用に
    // 暗黙依存しないよう、実状態を読んで 4KiB でマップされていることを確かめてから
    // 書き換える。外れていれば静かに壊れる代わりに止まる。
    // SAFETY: CR3 は自前テーブル。配下は恒等窓で読める。
    let table = unsafe { ActivePageTable::current(identity) };
    match table.translate(code_virt) {
        Ok(Some(t)) if t.page_size == PageSize::Size4KiB => {}
        other => {
            logger.error(format_args!(
                "syscall: user code page {:#x} is not a 4KiB mapping ({other:?}); \
                 verify_ring3_excursion must run first; halting",
                code_virt.as_u64()
            ));
            cpu::halt_forever();
        }
    }

    // ユーザールーチンの機械語を組み立てる。imm32 は手書きせず to_le_bytes で埋める。
    //   mov edi, ARGS[0]     BF id            RDI = 第 1 引数
    //   mov esi, ARGS[1]     BE id            RSI = 第 2 引数
    //   mov edx, ARGS[2]     BA id            RDX = 第 3 引数
    //   mov r10d, ARGS[3]    41 BA id         R10 = 第 4 引数（RCX ではない）
    //   mov r8d, ARGS[4]     41 B8 id         R8  = 第 5 引数
    //   mov r9d, ARGS[5]     41 B9 id         R9  = 第 6 引数
    //   mov ecx, SENTINEL    B9 id            RCX = 番兵（引数ではない。クロバー扱い）
    //   mov eax, NUMBER      B8 id            RAX = 番号
    //   int 0x80             CD 80
    //   mov [rsp-8], rax     48 89 44 24 F8   戻り値をユーザースタックへ store
    //   cli                  FA               予期の #GP（畳み出口）
    // mov r32, imm32 は 64bit で上位ゼロ拡張されるので、32bit に収まる既知値をそのまま使う。
    let mut code = [0u8; 64];
    let mut n = 0usize;
    let emit = |bytes: &[u8], code: &mut [u8; 64], n: &mut usize| {
        code[*n..*n + bytes.len()].copy_from_slice(bytes);
        *n += bytes.len();
    };
    let a: [u32; 6] = core::array::from_fn(|i| syscall::PROBE_ARGS[i] as u32);
    emit(&[0xBF], &mut code, &mut n);
    emit(&a[0].to_le_bytes(), &mut code, &mut n);
    emit(&[0xBE], &mut code, &mut n);
    emit(&a[1].to_le_bytes(), &mut code, &mut n);
    emit(&[0xBA], &mut code, &mut n);
    emit(&a[2].to_le_bytes(), &mut code, &mut n);
    emit(&[0x41, 0xBA], &mut code, &mut n);
    emit(&a[3].to_le_bytes(), &mut code, &mut n);
    emit(&[0x41, 0xB8], &mut code, &mut n);
    emit(&a[4].to_le_bytes(), &mut code, &mut n);
    emit(&[0x41, 0xB9], &mut code, &mut n);
    emit(&a[5].to_le_bytes(), &mut code, &mut n);
    emit(&[0xB9], &mut code, &mut n);
    emit(
        &(syscall::SENTINEL_RCX as u32).to_le_bytes(),
        &mut code,
        &mut n,
    );
    emit(&[0xB8], &mut code, &mut n);
    emit(
        &(syscall::PROBE_NUMBER as u32).to_le_bytes(),
        &mut code,
        &mut n,
    );
    let int_offset = n;
    emit(&[0xCD, 0x80], &mut code, &mut n);
    emit(&[0x48, 0x89, 0x44, 0x24, 0xF8], &mut code, &mut n);
    let cli_offset = n;
    emit(&[0xFA], &mut code, &mut n);
    let code_len = n;

    // SAFETY: code_virt は今マップを確認したユーザーページ。NX 未設定で実行可能、
    // SMAP 未有効で書き込み可能。code_len <= 64 <= 4096。
    unsafe {
        let p = code_virt.as_mut_ptr::<u8>();
        for (i, byte) in code[..code_len].iter().enumerate() {
            core::ptr::write_volatile(p.add(i), *byte);
        }
    }
    let cli_rip = ring3::USER_CODE_VIRT + cli_offset as u64;
    let int_rip = ring3::USER_CODE_VIRT + int_offset as u64;

    // ユーザーが戻り値を store する先（ユーザースタック頂点の直下）。事前に毒値を入れて
    // おき、畳み後に読み戻す。毒値のままなら store が起きていない。
    let store_slot = ring3::USER_STACK_TOP - 8;
    const STORE_POISON: u64 = 0x0BAD_0BAD_0BAD_0BAD;
    // SAFETY: store_slot はマップ済みのユーザースタックページ内。SMAP 未有効。
    unsafe {
        core::ptr::write_volatile(store_slot as *mut u64, STORE_POISON);
    }

    syscall::reset_counters();

    let main_rsp0_top = gdt::privilege_stack_top();
    let (exc_bottom, exc_top) = ring3::excursion_stack_range();

    logger.info(format_args!(
        "syscall: entering Ring 3 to issue probe int 0x80 (number={:#x}, int at {int_rip:#x}, \
         cli fold at {cli_rip:#x}, RSP0 -> excursion stack [{exc_bottom:#x}, {exc_top:#x}))",
        syscall::PROBE_NUMBER
    ));

    // --- 遠征。iretq -> Ring 3 -> 6 引数セット -> int 0x80 -> syscall_entry -> iretq ->
    //     戻り値 store -> cli -> #GP -> 畳み -> ここへ戻る ---
    // int 0x80 は 1 回だけ発行する。probe の記録はこの単一の呼び出しのものである
    // （invocations=1 と整合）。
    // SAFETY: ユーザーページは張り済み。Ring 3 は cli_rip で cli を実行して #GP を起こす。
    // main_rsp0_top はメインの上端。起動時の単一実行文脈から 1 回だけ呼ぶ。
    unsafe {
        ring3::enter(
            main_rsp0_top,
            ring3::USER_CODE_VIRT,
            ring3::USER_STACK_TOP,
            kernel::syscall::window_for_subtree(USER_PML4_INDEX),
        );
    }

    // --- 会計と検証 ---
    if !ring3::folded() {
        logger.error(format_args!(
            "syscall: returned without folding the expected #GP after int 0x80; halting"
        ));
        cpu::halt_forever();
    }

    // 畳んだ位置の主張（S8-a）。int 0x80 の直後に置いた cli で #GP になるはず。
    // ここが int_rip なら、往復せずに int の時点で落ちている。
    assert_folded_at(logger, "syscall", 13, cli_rip);

    let count = syscall::invocation_count();
    let seen_number = syscall::last_number();
    let seen_args = syscall::last_args();
    let handler_rsp = syscall::handler_rsp();
    let handler_in_rsp0 = handler_rsp >= exc_bottom && handler_rsp < exc_top;
    let rsp0_restored = gdt::privilege_stack_top() == main_rsp0_top;
    // SAFETY: store_slot はマップ済みのユーザースタックページ内。読み取りのみ。
    let stored = unsafe { core::ptr::read_volatile(store_slot as *const u64) };

    logger.info(format_args!(
        "syscall: probe int 0x80 returned. invocations={count} (issued exactly 1), number \
         seen={seen_number:#x} (expected {:#x}), handler RSP={handler_rsp:#x} (on RSP0 \
         excursion stack={handler_in_rsp0}), in-Ring-3 flag at entry={}, RSP0 \
         restored={rsp0_restored}",
        syscall::PROBE_NUMBER,
        syscall::in_ring3_at_entry()
    ));
    logger.info(format_args!(
        "syscall: args seen=[{:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}], user stored={stored:#x} \
         (expected return {:#x})",
        seen_args[0],
        seen_args[1],
        seen_args[2],
        seen_args[3],
        seen_args[4],
        seen_args[5],
        syscall::PROBE_RETURN
    ));

    if count != 1 {
        logger.error(format_args!(
            "syscall: syscall_entry ran {count} times, expected exactly 1; halting"
        ));
        cpu::halt_forever();
    }
    if seen_number != syscall::PROBE_NUMBER {
        logger.error(format_args!(
            "syscall: number seen {seen_number:#x} != expected {:#x}; RAX did not carry the \
             number; halting",
            syscall::PROBE_NUMBER
        ));
        cpu::halt_forever();
    }
    // 6 引数を規約どおり受け取ったか。発行側の既知値 PROBE_ARGS とハンドラの独立読み戻し
    // seen_args を突き合わせる（同じ式での自己検算ではない）。
    for (i, (&got, &expected)) in seen_args.iter().zip(syscall::PROBE_ARGS.iter()).enumerate() {
        if got != expected {
            logger.error(format_args!(
                "syscall: argument register mismatch (arg{i} seen {got:#x}, expected \
                 {expected:#x}); the register convention is wrong; halting"
            ));
            cpu::halt_forever();
        }
    }
    if !handler_in_rsp0 {
        logger.error(format_args!(
            "syscall: syscall_entry did not run on the RSP0 excursion stack (handler RSP \
             {handler_rsp:#x}); halting"
        ));
        cpu::halt_forever();
    }
    // syscall_entry は Ring 3 から呼ばれたのだから、入場時点で「今 Ring 3 にいる」が
    // 立っていたはずである（S8-b）。立っていなければ、Ring 3 へ落ちる経路か
    // 上げ下げの位置が壊れている。
    //
    // この主張の反証で示した範囲を書いておく。note_kernel_entry が常に false を返す
    // 形へ壊して、ここが止まることを確かめた。**示したのは「主張が真の値に固定されて
    // おらず、偽の値が来れば止まる」ことである。「enter が立て損ねたときに止まる」ことは、
    // この破壊では示していない**——その道は ring3-test no-fold-flag が塞いでおり、
    // あちらは最初の遠征で止まるのでここまで到達しない。同じ性質を 2 つの検査が
    // 別々の場所で見ている。
    if !syscall::in_ring3_at_entry() {
        logger.error(format_args!(
            "syscall: syscall_entry was reached while the in-Ring-3 flag was down; the flag \
             does not track the privilege boundary; halting"
        ));
        cpu::halt_forever();
    }
    if !rsp0_restored {
        logger.error(format_args!(
            "syscall: RSP0 was not restored to main; halting"
        ));
        cpu::halt_forever();
    }
    // 戻り値が RAX 経由で Ring 3 へ返り、ユーザーが store したか。
    if stored != syscall::PROBE_RETURN {
        logger.error(format_args!(
            "syscall: return value mismatch (user stored {stored:#x}, expected {:#x}); the \
             return value did not reach the user RAX; halting",
            syscall::PROBE_RETURN
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "syscall: probe int 0x80 round-trip verified (6 args reached syscall_entry per the R10 \
         convention, the return value came back through RAX to Ring 3 and was stored, syscall_entry \
         ran on the RSP0 stack, and the cli #GP folded back to the kernel)"
    ));
}

/// ポインタ系 syscall（`number`）を (buf, len) で 1 回発行し、ユーザーが store した
/// 戻り値を返す（M5-f-2-1 / M5-f-2-2）。
///
/// 遠征機構（enter → int 0x80 → 戻り値 store → cli 畳み）は verify_syscall_roundtrip と
/// 同じものを使う。ユーザーコード/スタックページは verify_ring3_excursion が張ったものを
/// 再利用する（呼び出し側が確認済みであることが前提）。
fn issue_ptr_len_syscall(logger: &mut Logger<SerialPort>, number: u64, buf: u64, len: u64) -> u64 {
    use kernel::ring3;
    use kernel::syscall;

    let code_virt = common::addr::VirtAddr::new(ring3::USER_CODE_VIRT)
        .expect("the user code virtual address is canonical");

    // ユーザールーチン: movabs rdi, buf; movabs rsi, len; mov eax, number;
    //   int 0x80; mov [rsp-8], rax; cli
    // buf は 512 GiB 付近で 32bit に収まらないので movabs（imm64）で積む。
    let mut code = [0u8; 64];
    let mut n = 0usize;
    let emit = |bytes: &[u8], code: &mut [u8; 64], n: &mut usize| {
        code[*n..*n + bytes.len()].copy_from_slice(bytes);
        *n += bytes.len();
    };
    emit(&[0x48, 0xBF], &mut code, &mut n); // movabs rdi, imm64
    emit(&buf.to_le_bytes(), &mut code, &mut n);
    emit(&[0x48, 0xBE], &mut code, &mut n); // movabs rsi, imm64
    emit(&len.to_le_bytes(), &mut code, &mut n);
    emit(&[0xB8], &mut code, &mut n); // mov eax, imm32
    emit(&(number as u32).to_le_bytes(), &mut code, &mut n);
    emit(&[0xCD, 0x80], &mut code, &mut n); // int 0x80
    emit(&[0x48, 0x89, 0x44, 0x24, 0xF8], &mut code, &mut n); // mov [rsp-8], rax
    let cli_offset = n;
    emit(&[0xFA], &mut code, &mut n); // cli
    let code_len = n;

    // SAFETY: code_virt は verify_ring3_excursion が張ったユーザーコードページ。NX 未設定で
    // 実行可能、SMAP 未有効で書き込み可能。code_len <= 64 <= 4096。
    unsafe {
        let p = code_virt.as_mut_ptr::<u8>();
        for (i, byte) in code[..code_len].iter().enumerate() {
            core::ptr::write_volatile(p.add(i), *byte);
        }
    }
    let cli_rip = ring3::USER_CODE_VIRT + cli_offset as u64;

    let store_slot = ring3::USER_STACK_TOP - 8;
    const STORE_POISON: u64 = 0x0BAD_0BAD_0BAD_0BAD;
    // SAFETY: store_slot はマップ済みのユーザースタックページ内。SMAP 未有効。
    unsafe {
        core::ptr::write_volatile(store_slot as *mut u64, STORE_POISON);
    }

    syscall::reset_counters();
    let main_rsp0_top = gdt::privilege_stack_top();

    // SAFETY: ユーザーページは張り済み。Ring 3 は cli_rip で cli を実行して #GP を起こす。
    // main_rsp0_top はメインの上端。起動時の単一実行文脈から呼ぶ。
    unsafe {
        ring3::enter(
            main_rsp0_top,
            ring3::USER_CODE_VIRT,
            ring3::USER_STACK_TOP,
            kernel::syscall::window_for_subtree(USER_PML4_INDEX),
        );
    }

    if !ring3::folded() {
        logger.error(format_args!(
            "syscall: pointer syscall (buf={buf:#x}, len={len:#x}) did not fold; halting"
        ));
        cpu::halt_forever();
    }

    // 畳んだ位置の主張（S8-a）。
    assert_folded_at(logger, "syscall", 13, cli_rip);
    if syscall::invocation_count() != 1 {
        logger.error(format_args!(
            "syscall: pointer syscall issued but syscall_entry ran {} times (expected 1); halting",
            syscall::invocation_count()
        ));
        cpu::halt_forever();
    }
    // SAFETY: store_slot はマップ済みのユーザースタックページ内。読み取りのみ。
    unsafe { core::ptr::read_volatile(store_slot as *const u64) }
}

/// ユーザーポインタ検証の検証（M5-f-2-1）。正常系と異常系5ケースを回す。
///
/// verify_ring3_excursion が残したユーザーページを再利用し、無効3（supervisor in user
/// range）のために U=0 ページを1枚張る。各ケースで SYS_CHECK_PTR を発行し、有効ポインタは
/// 受理（戻り値 0）、無効ポインタは拒否（-EFAULT）されることを確かめる。この段は copy が
/// 未実装なので、拒否は「踏み込む前に弾いた」ことそのものである。バイトを読む経路が無い。
///
/// 異常系は多層防御のどのチェックが弾いても拒否は成立する。単独チェックの隔離破壊は
/// skip-us / skip-laststep / skip-all（verification-coverage）。
fn verify_syscall_pointer<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut frame_allocator::FrameAllocator<CAP>,
) {
    use kernel::paging::active::{ActivePageTable, PageAttributes, PageSize};
    use kernel::ring3;
    use kernel::syscall;

    let identity = common::addr::DirectMap::identity(common::addr::DirectMap::IDENTITY_MAX_LENGTH)
        .expect("the identity window is canonical");
    let code_virt = common::addr::VirtAddr::new(ring3::USER_CODE_VIRT)
        .expect("the user code virtual address is canonical");

    // ユーザーコードページが再利用できることを実状態で確認する。
    // SAFETY: CR3 は自前テーブル。配下は恒等窓で読める。
    let mut table = unsafe { ActivePageTable::current(identity) };
    match table.translate(code_virt) {
        Ok(Some(t)) if t.page_size == PageSize::Size4KiB => {}
        other => {
            logger.error(format_args!(
                "syscall: user code page {:#x} is not a 4KiB mapping ({other:?}); halting",
                code_virt.as_u64()
            ));
            cpu::halt_forever();
        }
    }

    // 無効3の setup: ユーザー範囲内に U=0（supervisor）のページを1枚張る。code/stack が
    // 載る PD[0]（オフセット 0〜2MiB）とは別の 2MiB 領域、PD[1]（オフセット 2MiB）の
    // 0x8000200000 に置く。map_4kib(user=false) なので中間 PD[1] も葉も U=0 になる。
    let sup = common::addr::VirtAddr::new(0x8000200000).expect("SUP_VIRT is canonical");
    let Some(sup_phys) = allocator.allocate_frame() else {
        logger.error(format_args!(
            "syscall: could not reserve a frame for the supervisor test page; halting"
        ));
        cpu::halt_forever();
    };
    let sup_attributes = PageAttributes {
        user: false,
        writable: true,
        cacheable: true,
    };
    // SAFETY: sup はユーザーサブツリー内の未マップ VA。user=false で張るので Ring 3 から
    // 到達不可（walk_user_accessible が SupervisorOnly で弾く）。frame は未使用。
    if let Err(e) = unsafe { table.map_4kib(sup, sup_phys, sup_attributes, allocator) } {
        logger.error(format_args!(
            "syscall: map_4kib for the supervisor test page failed: {e:?}; halting"
        ));
        cpu::halt_forever();
    }

    let efault = (-syscall::EFAULT) as u64;
    let kernel_ptr: u64 = 0x10_0000; // カーネルイメージ領域（PML4[0]、U=0、範囲下限外）
    let unmapped: u64 = 0x8000400000; // PML4[1]、PD[2]、未マップ
                                      // **窓の上端を超える長さ。** 窓は 1 つになったので、上端は今の遠征の窓から
                                      // 導く（S9-b-3-2b）。
    let (_, window_end) = kernel::syscall::window_for_subtree(USER_PML4_INDEX);
    let over_long_len: u64 = window_end - ring3::USER_CODE_VIRT + 0x1000;

    // (buf, len, 受理を期待するか, 名前)
    let cases: [(u64, u64, bool, &str); 8] = [
        (ring3::USER_CODE_VIRT, 1, true, "valid page"),
        (kernel_ptr, 1, false, "kernel pointer"),
        (unmapped, 1, false, "unmapped user-range"),
        (0x8000200000, 1, false, "supervisor in user range"),
        (
            ring3::USER_CODE_VIRT + 0xFFF,
            2,
            false,
            "straddle last page",
        ),
        (ring3::USER_CODE_VIRT, over_long_len, false, "over-long"),
        (ring3::USER_CODE_VIRT, 0, true, "len=0 valid buf"),
        (kernel_ptr, 0, true, "len=0 invalid buf"),
    ];

    for (buf, len, expect_accept, name) in cases {
        let stored = issue_ptr_len_syscall(logger, kernel::syscall::SYS_CHECK_PTR, buf, len);
        let accepted = stored == 0;
        let rejected = stored == efault;
        let ok = if expect_accept { accepted } else { rejected };
        logger.info(format_args!(
            "syscall: ptr case '{name}' buf={buf:#x} len={len:#x} -> stored={stored:#x} \
             (expect {}, ok={ok})",
            if expect_accept {
                "accept(0)"
            } else {
                "reject(-EFAULT)"
            }
        ));
        if !ok {
            logger.error(format_args!(
                "syscall: pointer validation battery failed: case '{name}' buf={buf:#x} \
                 len={len:#x} expected {} but syscall returned {stored:#x}; halting",
                if expect_accept {
                    "accept(0)"
                } else {
                    "reject(-EFAULT)"
                }
            ));
            cpu::halt_forever();
        }
    }

    // 無効3の teardown: 葉を落とす（中間は残す）。
    // SAFETY: sup は今張ったユーザーページ。以後アクセスしない。
    if let Err(e) = unsafe { table.unmap_4kib(sup) } {
        logger.error(format_args!(
            "syscall: failed to unmap the supervisor test page: {e:?}; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "syscall: pointer validation battery verified (valid pointer accepted; kernel pointer, \
         unmapped, supervisor, straddle, and over-long all rejected before touching; len=0 \
         accepted regardless of buf)"
    ));
}

/// ユーザーバッファの内容往復の検証（M5-f-2-2）。
///
/// カーネルが既知内容をユーザーバッファへ書き、SYS_CHECKSUM を発行する。バッファは
/// ユーザースタックページの下部に置く（Ring 3 の RSP は頂点付近しか使わないので空き）。
/// カーネルは検証 → copy_from_user → バイト総和を返す。ユーザーが store し、カーネルが
/// 畳み後に読み戻して、発行側の既知内容から計算した期待総和と一致することを確かめる。
/// 自己検算ではなく、発行側の既知値とカーネルの独立読みの突き合わせである。
/// あわせて、カーネルポインタを渡すと copy 前の検証で -EFAULT が返る（読みに踏み込まない）
/// ことを確かめる。
fn verify_syscall_checksum(logger: &mut Logger<SerialPort>) {
    use kernel::ring3;
    use kernel::syscall;

    // 内容バッファはユーザースタックページの下部に置く。余分バイトは copy-overrun の
    // 検出用に、len の直後（同じ有効ページ内）へ置く。
    let buf_va = ring3::USER_STACK_VIRT;
    const N: usize = 8;
    let content: [u8; N] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
    const OVERRUN_MARK: u8 = 0xEE;

    // カーネルが既知内容と余分バイトをユーザーバッファへ書く（マップ済み、SMAP 未有効）。
    // SAFETY: buf_va は verify_ring3_excursion が張ったユーザースタックページ内で、
    // N+1 はページに収まる。
    unsafe {
        for (i, b) in content.iter().enumerate() {
            core::ptr::write_volatile((buf_va + i as u64) as *mut u8, *b);
        }
        core::ptr::write_volatile((buf_va + N as u64) as *mut u8, OVERRUN_MARK);
    }
    let expected_sum: u64 = content.iter().map(|b| *b as u64).sum();

    // 正常系: 検証を通し、total を返す。
    let stored = issue_ptr_len_syscall(logger, syscall::SYS_CHECKSUM, buf_va, N as u64);
    logger.info(format_args!(
        "syscall: checksum case 'valid buffer' buf={buf_va:#x} len={N} -> stored={stored:#x} \
         (expected sum {expected_sum:#x})"
    ));
    if stored != expected_sum {
        logger.error(format_args!(
            "syscall: checksum mismatch (stored {stored:#x} != expected {expected_sum:#x}); the \
             kernel read the wrong bytes (e.g. an overrun); halting"
        ));
        cpu::halt_forever();
    }

    // 異常系: カーネルポインタは copy 前の検証で -EFAULT。読みに踏み込まない。
    let efault = (-syscall::EFAULT) as u64;
    let kernel_ptr: u64 = 0x10_0000;
    let bad = issue_ptr_len_syscall(logger, syscall::SYS_CHECKSUM, kernel_ptr, N as u64);
    logger.info(format_args!(
        "syscall: checksum case 'kernel pointer' buf={kernel_ptr:#x} len={N} -> stored={bad:#x} \
         (expect -EFAULT {efault:#x})"
    ));
    if bad != efault {
        logger.error(format_args!(
            "syscall: checksum case 'kernel pointer' expected reject (-EFAULT) but got {bad:#x}; \
             copy_from_user read without validating; halting"
        ));
        cpu::halt_forever();
    }

    // 異常系: 長さがカーネルバッファを超える。**アドレスは正しいので -EINVAL であって
    // -EFAULT ではない**（S9-a で分けた）。
    //
    // **S9-a より前、この経路は一度も通っていなかった。** 検証はどちらの場合も
    // len=8 しか渡しておらず、容量超過の分岐は書かれているだけだった。errno を
    // 分けるなら、分けた側が実際に返ることを見る必要がある。
    let einval = (-syscall::EINVAL) as u64;
    let too_long = (syscall::CHECKSUM_BUF_LEN + 1) as u64;
    let over = issue_ptr_len_syscall(logger, syscall::SYS_CHECKSUM, buf_va, too_long);
    logger.info(format_args!(
        "syscall: checksum case 'over-long' buf={buf_va:#x} len={too_long} -> stored={over:#x} \
         (expect -EINVAL {einval:#x}, not -EFAULT {efault:#x}; the address is fine, the length \
         is not)"
    ));
    if over != einval {
        logger.error(format_args!(
            "syscall: checksum case 'over-long' expected -EINVAL {einval:#x} but got {over:#x}; \
             halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "syscall: checksum round-trip verified (the kernel read the user buffer through a \
         validated UserSlice and returned the correct byte sum; a kernel pointer was rejected \
         before reading; an over-long length was rejected as -EINVAL, distinct from -EFAULT)"
    ));
}

/// Ring 3 の 4 ベクタが中断され、カーネルが続くことを確かめる（S8-d-2）。
///
/// #DE・#UD・#GP・#PF を 1 つずつ Ring 3 で起こし、それぞれ畳んでここへ戻る。
/// **4 本を 4 つの判定行に分ける。** 1 つにまとめると、どのベクタで落ちたかが
/// 判定行から読めない。
///
/// # 4 本とも同じユーザーコードページを使い回す
///
/// この 6 本はいずれも [`ring3::USER_CODE_VIRT`] を飛び先として [`ring3::enter`] へ
/// 渡すので、遠征のたびにそのページの先頭へ別の命令列を書く。**ページが書き込み可能なのは、
/// `verify_ring3_excursion` がコードページを `writable: true` で張っているから
/// である。** S9-a より前は `map_4kib` が葉を常に W=1 で作っており、選ぶ余地が
/// 無かった。**6 本目（`#PF-write-ro`）だけは `writable: false` で張った別の
/// ページを的にする。**
///
/// # カーネルが継続したことの観測
///
/// **判定行が 4 本出ること自体がそれである。** 2 本目が出た時点で、1 本目の中断から
/// カーネルが戻って次の仕事へ進んだことが確定する。4 本目の後は起動シーケンスが
/// そのまま続く。
///
/// `paging-test` ビルドでは載せない（[`verify_ring3_excursion`] と同じ理由で、
/// ユーザーページがそちらで張られるため）。
#[cfg(not(feature = "paging-test"))]
fn verify_ring3_fault_vectors(logger: &mut Logger<SerialPort>) {
    use kernel::ring3;

    // ユーザーサブツリー内の未マップ VA。#PF の対象にする。
    // verify_ring3_excursion が張ったのはコード（+0）とスタック（+1 MiB）だけなので、
    // その間のこの位置は空いている。
    const UNMAPPED_USER_VIRT: u64 = ring3::USER_CODE_VIRT + 0x2000;

    // カーネル像の先頭。張られていて（present）、U=0 である。Ring 3 から読むと
    // 権限違反の #PF になる（S8-e。S7 の到達条件 3 の観測対象）。
    const KERNEL_TARGET_VIRT: u64 = 0xffff_ffff_8010_0000;

    // 各遠征の命令列と、**フォルトする命令の位置**（ページ先頭からのオフセット）。
    //
    // **フォルトするのは必ずしも先頭の命令ではない。** #DE は被除数と除数を 0 に
    // 置いてから割るので、落ちるのは 3 命令目である。#PF はアドレスを積んでから
    // 読むので 2 命令目になる。**このオフセットは実測で合わせた**（最初 #DE を 0 と
    // 書いて主張が落ち、4 が正しいと分かった）。
    // 5 本目（S8-e）は S7 の到達条件 3（ユーザーからカーネル領域へアクセス
    // できない）の観測である。S7 は Ring 3 へ一度も行かず、構造の監査（U=1 が
    // ユーザーサブツリーの外に無い）までで閉じた。**「Ring 3 から触って #PF に
    // なる」はここが初めての直接の観測である。** 対象はカーネル像の先頭
    // （張られていて U=0）。エラーコードの期待が 4 本目と違う——未マップは
    // 0x4（不在・ユーザー・読み）、こちらは **0x5（存在・ユーザー・読み）**で、
    // **「穴に落ちた」ではなく「権限で拒まれた」ことをエラーコードが区別する。**
    // 読み取り専用で張ったユーザーページ（S9-a）。Ring 3 が書くと #PF になる。
    // エラーコードは 0x7（存在・ユーザー・**書き**）で、4 本目（0x4）とも
    // 5 本目（0x5）とも違う。**3 本の #PF が、不在・権限（読み）・権限（書き）を
    // エラーコードで撃ち分けている。**
    const READONLY_USER_VIRT: u64 = ring3::USER_READONLY_VIRT;

    /// 1 本の遠征の記述。名前 / 期待ベクタ / 命令列（#PF は空でアクセス列を生成） /
    /// フォルトする命令のオフセット / #PF のアクセス先（0 = #PF でない） /
    /// 期待するエラーコード（#PF のみ意味を持つ） / 読みでなく書きか。
    type FaultCase = (&'static str, u8, &'static [u8], u64, u64, u64, bool);
    let cases: [FaultCase; 6] = [
        // #DE: xor edx,edx / xor ecx,ecx / div ecx。0 除算。落ちるのは div（+4）。
        (
            "#DE",
            0,
            &[0x31, 0xD2, 0x31, 0xC9, 0xF7, 0xF1],
            4,
            0,
            0,
            false,
        ),
        // #UD: ud2。落ちるのは先頭。
        ("#UD", 6, &[0x0F, 0x0B], 0, 0, 0, false),
        // #GP: cli。Ring 3 では特権命令。落ちるのは先頭。
        ("#GP", 13, &[0xFA], 0, 0, 0, false),
        // #PF: movabs rax, <読み先>（10 バイト）/ mov al,[rax]。落ちるのは
        // 読み（+10）。読み先は未マップのユーザー VA。エラーコード 0x4 =
        // 不在・ユーザー・読み。
        ("#PF", 14, &[], 10, UNMAPPED_USER_VIRT, 0x4, false),
        // #PF-kernel: 同じ命令列で、読み先だけカーネル VA。エラーコード 0x5 =
        // 存在・ユーザー・読み（権限違反）。
        ("#PF-kernel", 14, &[], 10, KERNEL_TARGET_VIRT, 0x5, false),
        // #PF-write-ro: movabs rax, <書き先>/ mov [rax],al / ud2。書き先は
        // writable=false で張ったユーザーページ。エラーコード 0x7 =
        // 存在・ユーザー・書き。
        //
        // **末尾の ud2 が破壊の受け皿である。** W=0 が効いていれば書きが落ちる
        // （+10、ベクタ 14）。効いていなければ書きが通り、+12 の ud2 で
        // ベクタ 6 が畳まれる。**どちらでも遠征は戻るので、判定行が
        // 「ベクタが違う」と言える。** 受け皿を置かないと、書きが通った後に
        // ページ上のゼロを命令として実行し始め、落ち方が決まらない。
        ("#PF-write-ro", 14, &[], 10, READONLY_USER_VIRT, 0x7, true),
    ];

    let main_rsp0_top = gdt::privilege_stack_top();
    let code_ptr = ring3::USER_CODE_VIRT as *mut u8;

    for (name, expected_vector, bytes, fault_offset, load_target, expected_error, is_store) in cases
    {
        // ユーザーコードページの先頭を、この遠征の命令列で埋める。
        // SAFETY: verify_ring3_excursion が張った U=1 / W=1 のユーザーページ。
        // 書くのは先頭の数バイトだけで、4KiB に収まる。SMAP は未有効。
        unsafe {
            if expected_vector == 14 {
                // movabs rax, imm64
                core::ptr::write_volatile(code_ptr, 0x48);
                core::ptr::write_volatile(code_ptr.add(1), 0xB8);
                for i in 0..8 {
                    let byte = ((load_target >> (i * 8)) & 0xFF) as u8;
                    core::ptr::write_volatile(code_ptr.add(2 + i as usize), byte);
                }
                // mov al,[rax]（読み）か mov [rax],al（書き）。1 バイトしか違わない。
                core::ptr::write_volatile(code_ptr.add(10), if is_store { 0x88 } else { 0x8A });
                core::ptr::write_volatile(code_ptr.add(11), 0x00);
                if is_store {
                    // 書きが通ってしまった場合の受け皿（ud2）。
                    core::ptr::write_volatile(code_ptr.add(12), 0x0F);
                    core::ptr::write_volatile(code_ptr.add(13), 0x0B);
                }
            } else {
                for (i, byte) in bytes.iter().enumerate() {
                    core::ptr::write_volatile(code_ptr.add(i), *byte);
                }
            }
        }

        // SAFETY: ユーザーページは張り済みで、今書いた命令列が必ずフォルトする。
        // main_rsp0_top はメインの上端。起動時の単一実行文脈から呼ぶ。
        unsafe {
            ring3::enter(
                main_rsp0_top,
                ring3::USER_CODE_VIRT,
                ring3::USER_STACK_TOP,
                kernel::syscall::window_for_subtree(USER_PML4_INDEX),
            );
        }

        if !ring3::folded() {
            logger.error(format_args!("ring3-vectors: {name} did not fold; halting"));
            cpu::halt_forever();
        }

        let vector = ring3::fault_vector();
        let rip = ring3::fault_rip();
        let cs = ring3::fault_cs();
        let expected_rip = ring3::USER_CODE_VIRT + fault_offset;

        // 判定行。**ベクタごとに 1 行**にする。
        if expected_vector == 14 {
            logger.info(format_args!(
                "ring3-vectors: {name} interrupted the Ring 3 run and the kernel continued \
                 (vector={vector} rip={rip:#018x} cs={cs:#x} cr2={:#018x} err={:#x})",
                ring3::fault_cr2(),
                ring3::fault_error_code()
            ));
        } else {
            logger.info(format_args!(
                "ring3-vectors: {name} interrupted the Ring 3 run and the kernel continued \
                 (vector={vector} rip={rip:#018x} cs={cs:#x})"
            ));
        }

        if vector != expected_vector as u64 {
            logger.error(format_args!(
                "ring3-vectors: {name} folded with vector={vector}, expected \
                 {expected_vector}; halting"
            ));
            cpu::halt_forever();
        }
        if rip != expected_rip {
            logger.error(format_args!(
                "ring3-vectors: {name} folded at rip={rip:#018x}, expected \
                 {expected_rip:#018x}; halting"
            ));
            cpu::halt_forever();
        }
        if (cs & 0b11) != 3 {
            logger.error(format_args!(
                "ring3-vectors: {name} did not come from Ring 3 (cs={cs:#x}); halting"
            ));
            cpu::halt_forever();
        }
        // #PF だけ CR2 とエラーコードを主張する。他のベクタでは意味を持たない値で
        // ある。エラーコードは「穴（不在）」と「権限違反（存在するが U=0）」を
        // 区別する——後者が S7 の到達条件 3 の中身である。
        if expected_vector == 14 {
            if ring3::fault_cr2() != load_target {
                logger.error(format_args!(
                    "ring3-vectors: {name} faulted on {:#018x}, expected \
                     {load_target:#018x}; halting",
                    ring3::fault_cr2()
                ));
                cpu::halt_forever();
            }
            if ring3::fault_error_code() != expected_error {
                logger.error(format_args!(
                    "ring3-vectors: {name} faulted with error code {:#x}, expected \
                     {expected_error:#x}; halting",
                    ring3::fault_error_code()
                ));
                cpu::halt_forever();
            }
        }
    }

    // S7 から預かった 1 件目の決着。ここまで来たなら 5 本目が通っている。
    logger.info(format_args!(
        "ring3-vectors: S7 condition 3 observed: a Ring 3 read of the kernel address \
         {KERNEL_TARGET_VIRT:#x} faulted as a protection violation (CR2 matched, error \
         code 0x5 = present+user+read), not as a hole; the kernel region is mapped but \
         unreachable from Ring 3"
    ));

    // S9-a。ここまで来たなら 6 本目が通っている。
    logger.info(format_args!(
        "ring3-vectors: map_4kib(writable=false) observed: a Ring 3 store to \
         {READONLY_USER_VIRT:#x} faulted with error code 0x7 (present+user+write), so the \
         leaf really carries W=0 (this says nothing about a Ring 0 store, which would need \
         CR0.WP on every core)"
    ));

    logger.info(format_args!(
        "ring3-vectors: all six Ring 3 faults (#DE, #UD, #GP, #PF unmapped, #PF kernel, \
         #PF write to a read-only page) interrupted only the Ring 3 run; the kernel ran on \
         after each one"
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
/// 失敗したときにログを最後まで出せるようにするため。実行中のコードやスタックが載る
/// ページを対象にすると、失敗した瞬間に何も観測できないまま落ちる。実測ではコードも
/// スタックも 4KiB ページに載っており（`report_mapping_granularity`）、そもそも 2MiB の
/// 分割対象にならない。ヒープとフレームバッファは 2MiB に載っているが、どちらも稼働中
/// なので通常起動では触らない。
///
/// そこでフレームアロケータから 2MiB 境界に揃った 512 フレームを確保し、それを対象に
/// する。アロケータが確保済みとして扱うので他の誰も使わない。確保したまま解放しないので、
/// その分のメモリは失われる（量はログに出す）。
///
/// # 照合は独立した経路で行う
///
/// 分割後の 512 エントリを `split_child_entry` と同じ式で検算しても、同じ間違いを
/// 2 回するだけである。`translate()` は実際のテーブルを辿るので、分割を行ったコードとは
/// 独立している。こちらで見る。
fn verify_split_and_unmap<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut frame_allocator::FrameAllocator<CAP>,
) {
    use kernel::paging::active::{ActivePageTable, MapUpdateError, PageSize};
    use kernel::paging::entry;

    const FRAMES_PER_2M: u64 = entry::PAGE_SIZE_2M / frame_allocator::FRAME_SIZE;

    /// 検証用に恒久的に予約する領域の大きさ。
    ///
    /// 2MiB なのは、分割の対象として 2MiB ページがちょうど 1 枚要るからである。境界も
    /// 2MiB に揃える。`plan::resolve_pages` が 2MiB ページを作るのは 2MiB 境界に揃った
    /// 範囲だけなので、揃っていないと 4KiB に分解されていて分割対象にならない。
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
    let base_phys = start_frame;
    let base = base_phys.as_u64();
    logger.info(format_args!(
        "split-test: reserved {base:#x}..{:#x} as scratch ({} KiB permanently withheld from the \
         allocator)",
        base + SCRATCH_BYTES,
        SCRATCH_BYTES / 1024
    ));

    // 登録 direct map の高位窓を使う（B-0）。A-2 以降 direct_map() は高位窓
    // （DIRECT_MAP_BASE + phys、PML4[256]、B でも残る）を返す。分割/アンマップの対象と
    // 読み戻しを高位窓へ移し、照合を「phys == 高位窓の virt_to_phys(virt)」へ一般化した
    // （恒等窓を base=0 の特殊ケースとして含む形）。B で恒等（PML4[0]）を外しても
    // このテストは生き残る。
    let table_map = common::addr::direct_map();
    // SAFETY: CR3 は自前のテーブルへ切り替え済みで、テーブルフレームは高位窓で読み書き
    // できる（窓は全マップ範囲を覆い、テーブルフレームは空き RAM 上にある）。
    let mut table = unsafe { ActivePageTable::current(table_map) };

    // --- 分割前の状態を記録する ---
    let base_virt = table_map.phys_to_virt(base_phys);
    let probes = [
        base_virt,
        base_virt.checked_add(entry::PAGE_SIZE_2M / 2).unwrap(),
        base_virt.checked_add(entry::PAGE_SIZE_2M - 1).unwrap(),
    ];
    let mut before = [common::addr::PhysAddr::new_const(0); 3];
    for (slot, probe) in probes.iter().enumerate() {
        match table.translate(*probe) {
            Ok(Some(translation)) if translation.page_size == PageSize::Size2MiB => {
                before[slot] = translation.phys;
            }
            other => {
                logger.error(format_args!(
                    "split-test: {:#x} is not mapped by a 2MiB page ({other:?}); halting",
                    probe.as_u64()
                ));
                cpu::halt_forever();
            }
        }
    }
    let huge_flags = match table.translate(base_virt) {
        Ok(Some(translation)) => translation.entry,
        _ => unreachable!("直前に 2MiB として翻訳できている"),
    };

    // --- 分割する ---
    // SAFETY: `allocator` の空き範囲がすべてマップ済みであることを起動時に検証している。
    // テーブルは CR3 に載っているものである。
    let outcome = match unsafe { table.split_huge_page(base_virt, allocator) } {
        Ok(outcome) => outcome,
        Err(error) => {
            logger.error(format_args!("split-test: split failed: {error:?}; halting"));
            cpu::halt_forever();
        }
    };
    logger.info(format_args!(
        "split-test: split {:#x} into 512 x 4KiB via a new page table at {:#x}",
        outcome.base_virt.as_u64(),
        outcome.table_phys.as_u64()
    ));

    // --- 512 エントリを読み戻して照合する（translate 経由の独立した経路）---
    let mut mismatches = 0u32;
    for index in 0..entry::ENTRIES_PER_TABLE {
        let virt = base_virt
            .checked_add(index as u64 * entry::PAGE_SIZE_4K)
            .expect("the scratch region stays canonical");
        match table.translate(virt) {
            Ok(Some(translation)) => {
                if translation.page_size != PageSize::Size4KiB {
                    mismatches += 1;
                } else if table_map.virt_to_phys(virt) != Some(translation.phys) {
                    // 高位窓なので phys == 窓の virt_to_phys(virt)（恒等窓 base=0 を含む一般形）。
                    mismatches += 1;
                } else {
                    // 属性が分割前と一致すること。PS は 4KiB では PAT の意味になるので、
                    // ここでは Present / Writable / PCD / PWT を見る。
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
                    "split-test: {:#x} changed more than its granularity: {other:?}",
                    probe.as_u64()
                ));
            }
        }
    }

    // --- 分割した領域を読み書きできること ---
    let mut io_ok = true;
    for probe in [
        base_virt,
        base_virt.checked_add(entry::PAGE_SIZE_2M / 2).unwrap(),
        base_virt.checked_add(entry::PAGE_SIZE_2M - 8).unwrap(),
    ] {
        // SAFETY: 直前に translate() で 4KiB としてマップ済みを確認した、アロケータから
        // 確保した誰も使っていない領域である。触るのは 8 バイトだけ。
        let read_back = unsafe {
            core::ptr::write_volatile(probe.as_mut_ptr::<u64>(), 0xA5A5_5A5A_A5A5_5A5A);
            core::ptr::read_volatile(probe.as_ptr::<u64>())
        };
        io_ok &= read_back == 0xA5A5_5A5A_A5A5_5A5A;
    }
    logger.info(format_args!(
        "split-test: the split region is readable and writable = {}",
        if io_ok { "OK" } else { "NG" }
    ));

    // --- アンマップする ---
    // 先頭ではなく 2 本目を消す。
    let target = base_virt.checked_add(entry::PAGE_SIZE_4K).unwrap();
    // SAFETY: 上記と同じ領域で、以後この 4KiB へはアクセスしない。
    let old_pte = match unsafe { table.unmap_4kib(target) } {
        Ok(pte) => pte,
        Err(error) => {
            logger.error(format_args!("split-test: unmap failed: {error:?}; halting"));
            cpu::halt_forever();
        }
    };
    let unmapped_ok = matches!(table.translate(target), Ok(None));
    // 隣が生きていること。これを見ないと、添字を間違えて領域全体を消していても
    // 気づけない。
    let neighbours_ok = matches!(
        table.translate(target.checked_sub(entry::PAGE_SIZE_4K).unwrap()),
        Ok(Some(t)) if t.page_size == PageSize::Size4KiB
    ) && matches!(
        table.translate(target.checked_add(entry::PAGE_SIZE_4K).unwrap()),
        Ok(Some(t)) if t.page_size == PageSize::Size4KiB
    );
    logger.info(format_args!(
        "split-test: unmapped {:#x} (old pte={old_pte:#x}); translate returns none={unmapped_ok}, \
         both neighbours still mapped={neighbours_ok}",
        target.as_u64()
    ));

    // --- API が誤用を弾くこと ---
    // SAFETY: 状態を変えない呼び出し。いずれもエラーで戻ることを期待する。
    let already_small = unsafe { table.split_huge_page(base_virt, allocator) };
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
/// どれか 1 つでも有効なら、そのビルドの観測結果を正常な結果として扱ってはならない。
/// 名前と「何を壊すか」を対にして並べる。
///
/// 仕込みが `paging::active` や `paging::entry` のようなレビュー必須のファイルにも
/// 住むようになったので、実行時に一覧を出す。7 種類まで増えると、どれが有効か分からない
/// まま実行する余地が生まれる。
const TEST_HOOKS: &[(&str, bool, &str)] = &[
    (
        "misalign-test",
        cfg!(feature = "misalign-test"),
        "IRQ スタブのスタック 16 バイト調整を外す",
    ),
    (
        "idt-irq-stub-offset-test",
        cfg!(feature = "idt-irq-stub-offset-test"),
        "IRQ スタブ表の索引を 1 本ずらす",
    ),
    (
        "addrspace-no-kernel-share",
        cfg!(feature = "addrspace-no-kernel-share"),
        "新しいアドレス空間へカーネルの上位を写さない",
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
        "keyboard-drop-arrows-test",
        cfg!(feature = "keyboard-drop-arrows-test"),
        "左右の矢印を未対応へ戻し、シェルの挿入点が動かないようにする",
    ),
    (
        "kill-ignore-interrupt-test",
        cfg!(feature = "kill-ignore-interrupt-test"),
        "中断の旗を立てず、Ctrl+C で子が止まらないようにする",
    ),
    (
        "kill-fold-at-depth-one-test",
        cfg!(feature = "kill-fold-at-depth-one-test"),
        "深さ 1 でも畳み、シェル自身が Ctrl+C で死ぬようにする",
    ),
    (
        "kill-keep-stale-interrupt-test",
        cfg!(feature = "kill-keep-stale-interrupt-test"),
        "子を起こす前に中断の旗を降ろさず、次の子へ持ち越す",
    ),
    (
        "kill-fold-keep-bkl-test",
        cfg!(feature = "kill-fold-keep-bkl-test"),
        "BKL を解かずに畳み、次に取る者が再取得として捕まえるようにする",
    ),
    (
        "fs-copy-corrupt-tail-test",
        cfg!(feature = "fs-copy-corrupt-tail-test"),
        "像の複製の末尾 1 バイトを 0xFF で潰す",
    ),
    (
        "kill-keep-typed-input-test",
        cfg!(feature = "kill-keep-typed-input-test"),
        "止めた後の入力を捨てず、次のプロンプトに ^C を余分に出す",
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
        "paging-test-directmap-low-window",
        cfg!(feature = "paging-test-directmap-low-window"),
        "direct map の窓を高位ではなく低位（恒等と同じ）で張る",
    ),
    (
        "paging-test-directmap-wrong-base",
        cfg!(feature = "paging-test-directmap-wrong-base"),
        "A-2 の登録窓の base を 1 ページずらす",
    ),
    (
        "stack-guard-test",
        cfg!(feature = "stack-guard-test"),
        "カーネルスタックを溢れさせてガードページを踏む",
    ),
    (
        "stack-overflow-df-test",
        cfg!(feature = "stack-overflow-df-test"),
        "溢れさせ、#PF に IST を与えず #DF へ昇格させる",
    ),
    (
        "task-switch-drop-reg",
        cfg!(feature = "task-switch-drop-reg"),
        "協調的スイッチで次タスクの rbx を壊す",
    ),
    (
        "task-switch-no-swap",
        cfg!(feature = "task-switch-no-swap"),
        "協調的スイッチで RSP の差し替えを省く",
    ),
    (
        "task-switch-yield-in-critical",
        cfg!(feature = "task-switch-yield-in-critical"),
        "InterruptGuard 保持中に yield を呼ぶ",
    ),
    (
        "task-switch-drop-rsp0",
        cfg!(feature = "task-switch-drop-rsp0"),
        "スイッチで RSP0 の更新を落とす",
    ),
    (
        "task-widen-preempt-window",
        cfg!(feature = "task-widen-preempt-window"),
        "プリエンプト窓の NOP そりを広げる",
    ),
    (
        "task-preempt-in-critical",
        cfg!(feature = "task-preempt-in-critical"),
        "InterruptGuard の cli を落とし、クリティカル区間へプリエンプトを食い込ませる",
    ),
    (
        "ring3-test-user-desc-dpl0",
        cfg!(feature = "ring3-test-user-desc-dpl0"),
        "ucode64 の DPL を 0 にして Ring 3 に落ちなくする",
    ),
    (
        "ring3-test-user-page-supervisor",
        cfg!(feature = "ring3-test-user-page-supervisor"),
        "ユーザーページの USER を落とす",
    ),
    (
        "ring3-test-drop-rsp0",
        cfg!(feature = "ring3-test-drop-rsp0"),
        "遠征の RSP0 据え付けを落とす",
    ),
    (
        "ring3-test-no-fold-flag",
        cfg!(feature = "ring3-test-no-fold-flag"),
        "遠征フラグを立てず予期 #GP を畳ませない",
    ),
    (
        "ring3-test-corrupt-frame-cs",
        cfg!(feature = "ring3-test-corrupt-frame-cs"),
        "例外フレームの CS を既知でない値へ差し替える",
    ),
    (
        "map-force-writable",
        cfg!(feature = "map-force-writable"),
        "map_4kib の書き込み可否の引数を無視して葉を常に W=1 にする",
    ),
    (
        "user-run-skip-load",
        cfg!(feature = "user-run-skip-load"),
        "PT_LOAD のコピーを落とす",
    ),
    (
        "user-run-writable-text",
        cfg!(feature = "user-run-writable-text"),
        "PT_LOAD を writable: true で張る",
    ),
    (
        "user-run-wrong-entry",
        cfg!(feature = "user-run-wrong-entry"),
        "entry ではなく PT_LOAD の先頭へ飛ぶ",
    ),
    (
        "user-exit-ignored",
        cfg!(feature = "user-exit-ignored"),
        "exit を受けても終了させず Ring 3 へ返す",
    ),
    (
        "user-exit-keep-bkl",
        cfg!(feature = "user-exit-keep-bkl"),
        "exit の分岐で BKL を解かずに longjmp する",
    ),
    (
        "user-exit-keep-space",
        cfg!(feature = "user-exit-keep-space"),
        "終了しても空間を畳まない",
    ),
    (
        "user-exit-wrong-status",
        cfg!(feature = "user-exit-wrong-status"),
        "終了状態を RDI でなく RSI から読む",
    ),
    (
        "syscall-test-einval-as-efault",
        cfg!(feature = "syscall-test-einval-as-efault"),
        "SYS_CHECKSUM の容量超過を -EINVAL でなく -EFAULT で返す",
    ),
    (
        "syscall-test-eisdir-as-enotdir",
        cfg!(feature = "syscall-test-eisdir-as-enotdir"),
        "ディレクトリの read を -EISDIR でなく -ENOTDIR で返す",
    ),
    (
        "syscall-test-read-no-advance",
        cfg!(feature = "syscall-test-read-no-advance"),
        "read がファイルの位置を進めない",
    ),
    (
        "syscall-test-stat-blocks-in-bytes",
        cfg!(feature = "syscall-test-stat-blocks-in-bytes"),
        "stat の st_blocks を 512 バイト単位でなくバイト数で書く",
    ),
    (
        "syscall-test-dirent-no-align",
        cfg!(feature = "syscall-test-dirent-no-align"),
        "getdents64 の d_reclen を 8 バイト境界へ切り上げない",
    ),
    (
        "syscall-test-no-auxv-terminator",
        cfg!(feature = "syscall-test-no-auxv-terminator"),
        "初期スタックの auxv に項目を足し、終端を書かない",
    ),
    (
        "spawn-eagain-as-enosys",
        cfg!(feature = "spawn-eagain-as-enosys"),
        "深さの上限で断ったことを -EAGAIN でなく -ENOSYS で返す",
    ),
    (
        "spawn-keep-child-records",
        cfg!(feature = "spawn-keep-child-records"),
        "入れ子の遠征から戻ったとき、親の記録を戻さない",
    ),
    (
        "spawn-child-rsp0",
        cfg!(feature = "spawn-child-rsp0"),
        "入れ子の遠征から戻す RSP0 を、親ではなく子自身の上端にする",
    ),
    (
        "spawn-e2big-as-einval",
        cfg!(feature = "spawn-e2big-as-einval"),
        "argv の量の問題を -E2BIG でなく -EINVAL で返す",
    ),
    (
        "spawn-argv-drop-last",
        cfg!(feature = "spawn-argv-drop-last"),
        "写した argv の最後の 1 本を落とす",
    ),
    (
        "write-ignores-fd",
        cfg!(feature = "write-ignores-fd"),
        "write が fd を見ずに、何番でも出力する",
    ),
    (
        "keep-steady-loop",
        cfg!(feature = "keep-steady-loop"),
        "シェルへ渡さず、定常ループを回し続ける（測定のための構成）",
    ),
    (
        "write-half-only",
        cfg!(feature = "write-half-only"),
        "write が要求された長さの半分だけ書いて返す",
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
    (
        "acpi-test-bad-signature",
        cfg!(feature = "acpi-test-bad-signature"),
        "MADT の署名を壊す",
    ),
    (
        "acpi-test-bad-checksum",
        cfg!(feature = "acpi-test-bad-checksum"),
        "MADT のチェックサムを壊す",
    ),
    (
        "acpi-test-bad-length",
        cfg!(feature = "acpi-test-bad-length"),
        "MADT の length を最小長未満にする",
    ),
    (
        "acpi-test-zero-entry-length",
        cfg!(feature = "acpi-test-zero-entry-length"),
        "MADT の最初のエントリの length を 0 にする",
    ),
    (
        "acpi-test-unmapped-rsdp",
        cfg!(feature = "acpi-test-unmapped-rsdp"),
        "RSDP を未マップの物理アドレスへ差し替える",
    ),
    (
        "acpi-test-rsdp-outside-window",
        cfg!(feature = "acpi-test-rsdp-outside-window"),
        "RSDP を direct map 窓の外へ差し替える",
    ),
    (
        "smp-ap-timer-no-svr-test",
        cfg!(feature = "smp-ap-timer-no-svr-test"),
        "AP が自分の SVR を書かない",
    ),
    (
        "smp-ap-timer-share-ticks-test",
        cfg!(feature = "smp-ap-timer-share-ticks-test"),
        "TIMER_TICKS を per-CPU にせず 1 つを共有する",
    ),
    (
        "smp-ap-no-sentinel-clear",
        cfg!(feature = "smp-ap-no-sentinel-clear"),
        "AP 起動時に CURRENT の sentinel を解かない",
    ),
    (
        "sched-ignore-owner",
        cfg!(feature = "sched-ignore-owner"),
        "pick_next の第 1 層（担当コア）を無効にする",
    ),
    (
        "sched-ignore-current",
        cfg!(feature = "sched-ignore-current"),
        "pick_next の第 2 層（CURRENT 条件）のフィルタでの参照だけを無効にする",
    ),
    (
        "smp-ap-runs-preemptive-demo",
        cfg!(feature = "smp-ap-runs-preemptive-demo"),
        "AP にプリエンプティブデモを呼ばせ、入口の tripwire を踏ませる",
    ),
    (
        "sched-ignore-bootstrap-tripwire",
        cfg!(feature = "sched-ignore-bootstrap-tripwire"),
        "デモ入口の bootstrap processor 見張りを外す",
    ),
    (
        "smp-tlb-shootdown-probe",
        cfg!(feature = "smp-tlb-shootdown-probe"),
        "APに探り用ページを触らせ、写像を外した後の見え方を比べる（S5-c）",
    ),
    (
        "smp-tlb-no-generation-bump",
        cfg!(feature = "smp-tlb-no-generation-bump"),
        "写像を外しても世代を上げない（S5-c の破壊。APは古い翻訳で成功する）",
    ),
    (
        "smp-tlb-generation-probe",
        cfg!(feature = "smp-tlb-generation-probe"),
        "TLB の世代を 1 つ上げ、各コアが次の取得でフラッシュすることを見る（S5-b）",
    ),
    (
        "smp-ipi-probe",
        cfg!(feature = "smp-ipi-probe"),
        "測定用 IPI を AP へ送る（S5-a。既定では送らない）",
    ),
    (
        "sched-keep-workers-runnable",
        cfg!(feature = "sched-keep-workers-runnable"),
        "AP 起動後にデモのワーカーを走行可能へ戻す（増幅器）",
    ),
    (
        "ioapic-keyboard-broadcast-test",
        cfg!(feature = "ioapic-keyboard-broadcast-test"),
        "キーボードの redirection entry の宛先を logical broadcast にする",
    ),
    (
        "bkl-hold-with-if-set-test",
        cfg!(feature = "bkl-hold-with-if-set-test"),
        "BKL を保持したまま IF=1 にする",
    ),
    (
        "bkl-hold-across-hlt-test",
        cfg!(feature = "bkl-hold-across-hlt-test"),
        "定常ループが hlt の前に BKL を離さない",
    ),
    (
        "bkl-hold-forever-test",
        cfg!(feature = "bkl-hold-forever-test"),
        "AP が BKL を取ったまま二度と離さない",
    ),
    (
        "bkl-skip-timer-entry-test",
        cfg!(feature = "bkl-skip-timer-entry-test"),
        "タイマ入口で BKL を取らない（計数は残す）",
    ),
    (
        "bkl-widen-entry-window-test",
        cfg!(feature = "bkl-widen-entry-window-test"),
        "入口の保持区間を広げて重なりを増幅する",
    ),
    (
        "acpi-test",
        cfg!(feature = "acpi-test"),
        "傘。ACPI の破壊一式を有効にする",
    ),
    (
        "apic-test",
        cfg!(feature = "apic-test"),
        "傘。APIC 写像の破壊一式を有効にする",
    ),
    (
        "apic-test-skip-map",
        cfg!(feature = "apic-test-skip-map"),
        "Local APIC MMIO の写像を省く",
    ),
    (
        "apic-test-wrong-target",
        cfg!(feature = "apic-test-wrong-target"),
        "写像先の物理をずらす",
    ),
    (
        "apic-test-base-mismatch",
        cfg!(feature = "apic-test-base-mismatch"),
        "MADT の Local APIC アドレスを 1 ページずらす",
    ),
    (
        "critical-test-double-lock",
        cfg!(feature = "critical-test-double-lock"),
        "同じロックを保持したまま再取得する",
    ),
    (
        "critical-test-restore-enabled",
        cfg!(feature = "critical-test-restore-enabled"),
        "IF=1 から InterruptGuard へ入る",
    ),
    (
        "exception-test-divide-by-zero",
        cfg!(feature = "exception-test-divide-by-zero"),
        "除算例外を起こす",
    ),
    (
        "exception-test-invalid-opcode",
        cfg!(feature = "exception-test-invalid-opcode"),
        "不正命令例外を起こす",
    ),
    (
        "exception-test-page-fault",
        cfg!(feature = "exception-test-page-fault"),
        "ページフォルトを起こす",
    ),
    (
        "exception-test-double-fault",
        cfg!(feature = "exception-test-double-fault"),
        "ダブルフォルトを起こす",
    ),
    (
        "highhalf-no-identity-in-boot-pt",
        cfg!(feature = "highhalf-no-identity-in-boot-pt"),
        "静的初期テーブルの PML4[0] の存在ビットを落とす",
    ),
    (
        "highhalf-bad-high-slot",
        cfg!(feature = "highhalf-bad-high-slot"),
        "PDPT_high のエントリを 510 から 509 へずらす",
    ),
    (
        "highhalf-no-kernel-high-in-live-table",
        cfg!(feature = "highhalf-no-kernel-high-in-live-table"),
        "本流テーブルへカーネル高位マッピングを張らない",
    ),
    (
        "highhalf-trampoline-absolute-ref",
        cfg!(feature = "highhalf-trampoline-absolute-ref"),
        "トランポリンへ絶対メモリ参照命令を 1 つ入れる",
    ),
    (
        "highhalf-remove-verify-fail",
        cfg!(feature = "highhalf-remove-verify-fail"),
        "恒等除去の必須領域の検証を失敗させる",
    ),
    (
        "highhalf-remove-before-highify",
        cfg!(feature = "highhalf-remove-before-highify"),
        "ヒープを高位化せずに恒等を除去する",
    ),
    (
        "highhalf-panic-after-remove",
        cfg!(feature = "highhalf-panic-after-remove"),
        "恒等除去の直後に panic する",
    ),
    (
        "interrupt-test-enable-only",
        cfg!(feature = "interrupt-test-enable-only"),
        "全 IRQ をマスクしたまま sti する",
    ),
    (
        "interrupt-test-irq-path",
        cfg!(feature = "interrupt-test-irq-path"),
        "int 0x40 を発行する",
    ),
    (
        "interrupt-test-timer",
        cfg!(feature = "interrupt-test-timer"),
        "タイマを動かす",
    ),
    (
        "ioapic-wrong-vector-test",
        cfg!(feature = "ioapic-wrong-vector-test"),
        "redirection entry へゲートの無いベクタを書く",
    ),
    (
        "ioapic-skip-unmask-test",
        cfg!(feature = "ioapic-skip-unmask-test"),
        "I/O APIC 側のマスクを外さない",
    ),
    (
        "ioapic-keep-pic-irq1-test",
        cfg!(feature = "ioapic-keep-pic-irq1-test"),
        "PIC 側の IRQ1 をマスクしない",
    ),
    (
        "lapic-timer-scale-calibration-test",
        cfg!(feature = "lapic-timer-scale-calibration-test"),
        "較正の戻り値を 2 倍にする",
    ),
    (
        "lapic-timer-wrong-divide-test",
        cfg!(feature = "lapic-timer-wrong-divide-test"),
        "較正時と運用時の分周を食い違わせる",
    ),
    (
        "lapic-timer-no-mask-all-test",
        cfg!(feature = "lapic-timer-no-mask-all-test"),
        "8259 を全マスクせずに LVT を開ける",
    ),
    (
        "percpu-fake-nonzero-cpu-id",
        cfg!(feature = "percpu-fake-nonzero-cpu-id"),
        "tripwire が見る cpu_id を非 0 に偽る",
    ),
    (
        "smp-tramp-corrupt-copy-test",
        cfg!(feature = "smp-tramp-corrupt-copy-test"),
        "設置した AP トランポリンのコピーを 1 バイト壊す",
    ),
    (
        "smp-ap-touch-scheduler-test",
        cfg!(feature = "smp-ap-touch-scheduler-test"),
        "AP からスケジューラの現在タスクを読む",
    ),
    (
        "syscall-test-arg4-rcx",
        cfg!(feature = "syscall-test-arg4-rcx"),
        "第 4 引数を R10 ではなく RCX から読む",
    ),
    (
        "syscall-test-gate-dpl0",
        cfg!(feature = "syscall-test-gate-dpl0"),
        "syscall ゲートの DPL を 0 にする",
    ),
    (
        "syscall-test-drop-retval",
        cfg!(feature = "syscall-test-drop-retval"),
        "戻り値の RAX 書き戻しを落とす",
    ),
    (
        "syscall-test-validate-skip-us",
        cfg!(feature = "syscall-test-validate-skip-us"),
        "ユーザーポインタ検証の U=1 判定を外す",
    ),
    (
        "syscall-test-validate-skip-laststep",
        cfg!(feature = "syscall-test-validate-skip-laststep"),
        "ページ走査を先頭ページで打ち切る",
    ),
    (
        "syscall-test-validate-skip-all",
        cfg!(feature = "syscall-test-validate-skip-all"),
        "検証器を常に受理にする",
    ),
    (
        "syscall-test-copy-skip-validate",
        cfg!(feature = "syscall-test-copy-skip-validate"),
        "copy_from_user が検証を経ずに読む",
    ),
    (
        "syscall-test-copy-overrun",
        cfg!(feature = "syscall-test-copy-overrun"),
        "copy_from_user が len を 1 バイト超えて読む",
    ),
];

/// 有効な仕込み feature を起動時に報告する。
///
/// 1 つでも有効なら WARN を出す。仕込みが有効なビルドで測った結果を正常な結果として
/// 報告する事故を防ぐためである。何も有効でない場合も 1 行出す。「出ていない」と
/// 「そもそも報告していない」を区別できるようにするため。
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
/// 通常起動には入らない。ここで行うのは、意図的にフォルトを起こす、意図的に壊した状態を
/// 作る、稼働中の領域を触る、といった操作である。通常の起動シーケンスに混ぜると、起動時の
/// 他の異常と区別しにくくなる。
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

    // 登録 direct map の高位窓を使う（B-0、verify_split_and_unmap と同じ）。この経路の
    // スクラッチ VA は既に窓相対（test_map.phys_to_virt）なので、窓を高位へ替えるだけで
    // 恒等前提が外れ、対象が高位窓の split/unmap になる。B で恒等（PML4[0]）を外しても
    // この経路は生き残る。
    let test_map = common::addr::direct_map();
    // SAFETY: CR3 は自前のテーブルへ切り替え済み。テーブルフレームは高位窓で読める。
    let mut table = unsafe { ActivePageTable::current(test_map) };

    // --- PCD 付きの 2MiB ページを分割する ---
    //
    // 実機で PCD 付きの 2MiB ページはフレームバッファだけだが、そこを分割対象にすると
    // 失敗時に画面が壊れ、観測手段の一部を失う。誰も使っていないスクラッチ領域に PCD を
    // 立ててから分割すれば、同じ性質を安全に試せる。
    let Some(frame) = allocator.allocate_contiguous_aligned(FRAMES_PER_2M, FRAMES_PER_2M) else {
        logger.error(format_args!("paging-test: no scratch region available"));
        cpu::halt_forever();
    };
    let pcd_phys = frame;
    let pcd_base_virt = test_map.phys_to_virt(pcd_phys);
    let pcd_base = pcd_phys.as_u64();
    // SAFETY: 今確保したばかりの、誰も使っていない領域である。PCD を立ててもアクセスが
    // キャッシュされなくなるだけで、内容も配置も変わらない。
    let before = unsafe { table.add_huge_page_flags(pcd_base_virt, entry::PTE_PCD) };
    let pcd_set = matches!(
        table.translate(pcd_base_virt),
        Ok(Some(t)) if t.entry & entry::PTE_PCD != 0 && t.page_size == PageSize::Size2MiB
    );
    logger.info(format_args!(
        "paging-test: scratch {pcd_base:#x} now has PCD as a 2MiB page = {pcd_set} (was {before:?})"
    ));

    // SAFETY: 上記のスクラッチ領域。
    match unsafe { table.split_huge_page(pcd_base_virt, allocator) } {
        Ok(_) => {}
        Err(error) => {
            logger.error(format_args!("paging-test: split failed: {error:?}"));
            cpu::halt_forever();
        }
    }
    let mut pcd_lost = 0u32;
    for index in 0..entry::ENTRIES_PER_TABLE {
        let virt = pcd_base_virt
            .checked_add(index as u64 * entry::PAGE_SIZE_4K)
            .expect("the scratch region stays canonical");
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
        let heap_page = common::addr::VirtAddr::new(heap_start & !(entry::PAGE_SIZE_2M - 1))
            .expect("the heap address is canonical");
        let heap_virt = common::addr::VirtAddr::new(heap_start).expect("canonical");
        let phys_before = table.translate(heap_virt);
        // ヒープを実際に使ってから分割し、分割後も使えることを見る。
        let mut live: Vec<u64> = (0..64).collect();
        // SAFETY: 稼働中のヒープが載るページだが、分割は物理アドレスも属性も変えない。
        // 手順の途中でも古い 2MiB エントリが有効なままである（`split_huge_page`）。
        let result = unsafe { table.split_huge_page(heap_page, allocator) };
        live.push(0xDEAD);
        let phys_after = table.translate(heap_virt);
        let same = match (phys_before, phys_after) {
            (Ok(Some(a)), Ok(Some(b))) => {
                a.phys == b.phys
                    && a.page_size == PageSize::Size2MiB
                    && b.page_size == PageSize::Size4KiB
            }
            _ => false,
        };
        logger.info(format_args!(
            "paging-test: split the live heap page {:#x}: {result:?}, translation \
             unchanged apart from granularity = {same}, heap still usable = {} ({} items)",
            heap_page.as_u64(),
            live.last() == Some(&0xDEAD),
            live.len()
        ));
    }
    #[cfg(not(feature = "paging-test-split-heap"))]
    let _ = heap_start;

    // --- アンマップ後のアクセス ---
    //
    // `paging-test-no-invlpg` では invlpg を落としてある。古い翻訳が TLB に残っていれば
    // フォルトせずに読めてしまい、残らなければ #PF になる。QEMU の TCG が TLB をどう扱うか
    // に依存するので、どちらになるかは事前に決めつけない。観測した結果をそのまま出す。
    let Some(frame) = allocator.allocate_contiguous_aligned(FRAMES_PER_2M, FRAMES_PER_2M) else {
        logger.error(format_args!("paging-test: no second scratch region"));
        cpu::halt_forever();
    };
    let unmap_phys = frame;
    let unmap_base = test_map.phys_to_virt(unmap_phys);
    // SAFETY: 誰も使っていないスクラッチ領域。
    if let Err(error) = unsafe { table.split_huge_page(unmap_base, allocator) } {
        logger.error(format_args!("paging-test: second split failed: {error:?}"));
        cpu::halt_forever();
    }
    let target = unmap_base.checked_add(4 * entry::PAGE_SIZE_4K).unwrap();

    // アンマップする前に必ず 1 度触る。触っていないページには TLB エントリが存在せず、
    // `invlpg` を落としても「古い翻訳が残る」状態を作れない。それに気づかずに書いた
    // ところ、`paging-test-no-invlpg` でも #PF になり、invlpg の有無が結果に現れな
    // かった。検査に見えて何も検査していない状態である。
    // SAFETY: 直前に分割した、誰も使っていないスクラッチ領域である。
    unsafe {
        core::ptr::write_volatile(target.as_mut_ptr::<u64>(), 0x1234_5678_9ABC_DEF0);
    }

    // SAFETY: 上記の領域。以後この 4KiB へアクセスするのが、この検証の目的である。
    let old = unsafe { table.unmap_4kib(target) };
    let target_none = matches!(table.translate(target), Ok(None));
    // 添字を間違えていれば、別のページが消えているはずである。
    let neighbour_none = matches!(table.translate(unmap_base), Ok(None));
    logger.info(format_args!(
        "paging-test: unmap {:#x} -> {old:?}; target none={target_none}, \
         region head none={neighbour_none}",
        target.as_u64()
    ));

    // アンマップしたページを実際に読むのは、専用のビルドだけである。
    //
    // 正しい実装（invlpg を発行する）では #PF になり、そこで停止する。
    // `paging-test-no-invlpg` では古い翻訳が TLB に残っていればフォルトしない。
    // この 2 つを対にして初めて「invlpg が効いている」と言える。片方だけでは
    // 「常にフォルトする経路」と区別できない。
    //
    // QEMU の TCG が TLB をどう扱うかに依存するので、no-invlpg 側が本当にフォルト
    // しないかは事前に決めつけない。観測した結果をそのまま出す。
    #[cfg(any(feature = "paging-test-unmap-fault", feature = "paging-test-no-invlpg"))]
    {
        logger.info(format_args!(
            "paging-test: about to read the unmapped page {:#x}",
            target.as_u64()
        ));
        // SAFETY: この読み取りがフォルトするかどうかの観測が、この検証の目的である。
        let value = unsafe { core::ptr::read_volatile(target.as_ptr::<u64>()) };
        logger.info(format_args!(
            "paging-test: the read did NOT fault; value={value:#x} (a stale TLB entry was used)"
        ));
    }

    logger.info(format_args!("paging-test: done"));
}

/// カーネルが実際に使っている領域が、どの粒度でマップされているかを測る。
///
/// M5-a-2 で 2MiB ページを分割するにあたり、どこが 2MiB ページに載っているのかを推測で
/// 決めないために測る。`plan::resolve_pages` は 2MiB 境界に揃った核だけを 2MiB ページに
/// し、前後の端数を 4KiB へ分解する。どの領域が核に入り、どれが端数になるかは実際の
/// メモリマップ次第で、コードを読んだだけでは決まらない。
///
/// 分割対象の選定材料であると同時に、恒等マッピングの現状把握でもある。ここに挙げた
/// 4 つはカーネルが動き続けるために要る領域で、翻訳できなければ fail-fast する。
fn report_mapping_granularity(
    logger: &mut Logger<SerialPort>,
    heap_start: u64,
    framebuffer_phys: u64,
) {
    use kernel::paging::active::{ActivePageTable, PageSize};
    use kernel::paging::entry;

    // SAFETY: CR3 は自前のテーブルへ切り替えて読み戻し済みで、テーブル自体は
    // 恒等マッピングで読める（`verify_page_tables` と同じ前提）。
    let table = unsafe { ActivePageTable::current(common::addr::direct_map()) };

    // RIP と RSP は測定時点の実値を読む。リンカスクリプトのシンボルやスタックの静的配列の
    // 番地から計算すると、「そう配置したはず」の値を見ることになり、実際に実行している
    // アドレスの確認にならない。
    let probes: [(&str, u64); 4] = [
        ("executing code (RIP)", cpu::read_rip()),
        ("kernel stack (RSP)", cpu::read_rsp()),
        ("heap arena", heap_start),
        ("framebuffer", framebuffer_phys),
    ];

    let mut failures = 0u32;
    for (name, addr) in probes {
        if addr == 0 {
            // フレームバッファが無い構成ではここに来る。存在しないものを「マップされて
            // いない」として数えない。
            logger.info(format_args!("granularity: {name} is absent (address 0)"));
            continue;
        }
        let Some(virt) = common::addr::VirtAddr::new(addr) else {
            failures += 1;
            logger.error(format_args!(
                "granularity: {name} {addr:#x} is not a canonical address"
            ));
            continue;
        };
        match table.translate(virt) {
            Ok(Some(translation)) => {
                let (size_name, base) = match translation.page_size {
                    PageSize::Size2MiB => ("2MiB", addr & !(entry::PAGE_SIZE_2M - 1)),
                    PageSize::Size4KiB => ("4KiB", addr & !(entry::PAGE_SIZE_4K - 1)),
                };
                logger.info(format_args!(
                    "granularity: {name} {addr:#x} -> phys {:#x}, mapped by a {size_name} page \
                     at {base:#x} (entry={:#x})",
                    translation.phys.as_u64(),
                    translation.entry
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

/// higher-half A-1: 恒等と direct map 窓の両方を持つ新テーブルを構築し、
/// CR3 を切り替える（ADR-0021 の Addendum）。
///
/// # やること・やらないこと
///
/// 恒等マッピングは外さない。direct map 窓（`DIRECT_MAP_BASE + phys`）を足すだけで、
/// RSP・ヒープ・boot_info はすべて低位のまま動き続ける。恒等の除去はカーネルイメージの
/// 高位化（B）と不可分なので、A では行わない。
///
/// 登録 DirectMap は恒等（base=0）のまま保つ。差し替えは A-2 の `replace_direct_map` で
/// 行う。`direct_map()` を通す既存経路は恒等アドレスを返し続け、新テーブルの恒等側で
/// 到達可能なままである。
///
/// kernel イメージの高位マッピングは含めない。それは H-2 が別テーブルで扱う。ここが
/// 張るのは恒等と direct map 窓の 2 つだけである。
///
/// # 検証の独立性
///
/// 構築は `map_page` / `map_range`（`table` の式）で行い、検証は `verify::walk`
/// （別に書き直した式）で辿る。恒等部分の照合は、入力の `mapped` とではなく、稼働中
/// テーブル（M2-d が切り替えた実体）を `active::translate` で読み戻した実状態と突き
/// 合わせる。同じ入力から同じ式で作ったものを検算しないためである。
///
/// # 移設の余地
///
/// B でこの窓構築を bootloader 側へ移す可能性があるので、kernel 専用のグローバル状態に
/// 依存させず、引数（テーブルアクセス用の窓・アロケータ・マップ範囲）だけで完結させて
/// ある。登録 `direct_map()` はテーブルフレームのアクセスにのみ引く。
fn build_and_switch_direct_map(
    logger: &mut Logger<SerialPort>,
    allocator: &mut frame_allocator::FrameAllocator<{ kernel::paging::plan::DEFAULT_CAPACITY }>,
    mapped: &MappedRanges<{ kernel::paging::plan::DEFAULT_CAPACITY }>,
) {
    use common::addr::{DirectMap, VirtAddr};
    use kernel::paging::{active::ActivePageTable, verify};

    // 構築中のテーブルフレームは、登録済みの窓（現在は恒等）を通して読み書きする。
    // M2-d のビルダーと同じ経路である。
    let access = common::addr::direct_map();

    let mut builder = match PageTableBuilder::new(allocator, access) {
        Ok(builder) => builder,
        Err(error) => {
            logger.error(format_args!(
                "direct-map: failed to start the build: {error:?}"
            ));
            cpu::halt_forever();
        }
    };

    // --- 恒等部分（現在の plan と同一） ---
    let mut identity_error = None;
    resolve_pages(mapped, |m| {
        if identity_error.is_some() {
            return;
        }
        if let Err(e) = builder.map_page(m.phys_addr, m.huge, m.cacheable) {
            identity_error = Some(e);
        }
    });
    if let Some(error) = identity_error {
        logger.error(format_args!("direct-map: identity build failed: {error:?}"));
        cpu::halt_forever();
    }

    // --- direct map 窓（DIRECT_MAP_BASE + phys） ---
    //
    // cacheable は classify 由来をそのまま引き継ぐ（フレームバッファ・MMIO は PCD）。
    // `map_range` が仮想・物理の両方のアラインメントで 2MiB 昇格を判定する。G ビットと
    // NX（bit 63）は立てない（EFER.NXE 未有効。ADR-0021）。
    //
    // 破壊 (paging-test-directmap-low-window): 窓を高位ではなく低位（phys、恒等と同じ）で
    // 張る。恒等が既に phys->phys を張っているので起動は検証手前まで進むが、切り替え前の
    // walker 検証が DIRECT_MAP_BASE + phys を辿って NotPresent で捕まえる。高位窓が
    // 存在すること自体を検査していることの証明である。
    let window_base = if cfg!(feature = "paging-test-directmap-low-window") {
        0
    } else {
        DirectMap::DIRECT_MAP_BASE
    };
    for r in mapped.iter() {
        let Some(virt) = VirtAddr::new(window_base.wrapping_add(r.start.as_u64())) else {
            logger.error(format_args!(
                "direct-map: window virtual base for phys {:#x} is not canonical; halting",
                r.start.as_u64()
            ));
            cpu::halt_forever();
        };
        let len = r.end.as_u64() - r.start.as_u64();
        if let Err(e) = builder.map_range(virt, r.start, len, r.cacheable) {
            logger.error(format_args!(
                "direct-map: window map_range at {:#x} (phys {:#x}, len {:#x}) failed: {e:?}",
                virt.as_u64(),
                r.start.as_u64(),
                len
            ));
            cpu::halt_forever();
        }
    }

    // higher-half（B-2a）: この本流テーブルにも kernel イメージの高位マッピングを張る。
    // base=0 では冪等（新規フレーム 0）。base=高位（B-2a-3）では再リンク後にこのテーブルへ
    // CR3 を切り替えても高位コードが見え続けるようにする。
    // 破壊 (highhalf-no-kernel-high-in-live-table): A-1 の本流テーブルからも外す。
    #[cfg(not(feature = "highhalf-no-kernel-high-in-live-table"))]
    map_kernel_high_half(&mut builder, logger);

    let new_pml4 = builder.pml4_phys();
    let frames_used = builder.frames_used();
    logger.info(format_args!(
        "direct-map: built a new table at PML4 {:#x} using {frames_used} frame(s) ({} KiB), \
         window base {:#x}",
        new_pml4.as_u64(),
        frames_used * frame_allocator::FRAME_SIZE / 1024,
        window_base
    ));

    // --- 切り替え前の独立検証（新テーブルはまだ稼働していない） ---
    //
    // 新テーブルのフレームは、現在稼働中の恒等マッピングで読める。ここで壊れた窓
    // （low-window）を捕まえ、壊れていれば切り替えずに停止する。
    // SAFETY: 現在の CR3 は M2-d の恒等テーブルを指しており、その配下は恒等で読める
    // （`current` の契約）。
    let live = unsafe { ActivePageTable::current(access) };

    let mut identity_checked = 0u32;
    let mut identity_mismatches = 0u32;
    let mut window_checked = 0u32;
    let mut window_mismatches = 0u32;
    let mut huge_seen = false;

    for r in mapped.iter() {
        // 各範囲について、先頭・末尾直前・内部の 2MiB 境界を標本にする。
        // 2MiB 境界は昇格した窓ページ（huge=true）を踏むための位置である。
        let last = r.end.checked_sub(1).unwrap_or(r.start);
        let mut probes = [Some(r.start), Some(last), None];
        if let Some(boundary) = r.start.align_up(kernel::paging::plan::PAGE_SIZE_2M) {
            if boundary < r.end {
                probes[2] = Some(boundary);
            }
        }

        for probe in probes.into_iter().flatten() {
            // 恒等側: 稼働中テーブルの実状態（active::translate、`entry` の式）と新テーブル
            // （verify::walk、別の式）が、同じ物理へ解決すること。
            if let Some(virt) = VirtAddr::new(probe.as_u64()) {
                identity_checked += 1;
                let live_phys = match live.translate(virt) {
                    Ok(Some(t)) => Some(t.phys),
                    _ => None,
                };
                // SAFETY: new_pml4 は今構築したテーブルで、恒等で読める。
                let new_phys = match unsafe { verify::walk(new_pml4, access, virt) } {
                    Ok(res) => Some(res.phys),
                    Err(_) => None,
                };
                if live_phys != Some(probe) || new_phys != Some(probe) {
                    identity_mismatches += 1;
                    logger.error(format_args!(
                        "direct-map: identity probe {:#x}: live={:?} new={:?} (expected {:#x})",
                        virt.as_u64(),
                        live_phys.map(|p| p.as_u64()),
                        new_phys.map(|p| p.as_u64()),
                        probe.as_u64()
                    ));
                }
            }

            // 窓側: DIRECT_MAP_BASE + phys が phys へ解決すること。窓の base は常に高位で
            // 辿る。構築が低位で張られていれば、ここで NotPresent になって捕まる。
            if let Some(virt) =
                VirtAddr::new(DirectMap::DIRECT_MAP_BASE.wrapping_add(probe.as_u64()))
            {
                window_checked += 1;
                // SAFETY: 上と同じ。
                match unsafe { verify::walk(new_pml4, access, virt) } {
                    Ok(res) if res.phys == probe => {
                        if res.huge {
                            huge_seen = true;
                        }
                    }
                    Ok(res) => {
                        window_mismatches += 1;
                        logger.error(format_args!(
                            "direct-map: window probe {:#x} resolves to {:#x}, expected {:#x}",
                            virt.as_u64(),
                            res.phys.as_u64(),
                            probe.as_u64()
                        ));
                    }
                    Err(e) => {
                        window_mismatches += 1;
                        logger.error(format_args!(
                            "direct-map: window probe {:#x} does not resolve: {e:?} (expected {:#x})",
                            virt.as_u64(),
                            probe.as_u64()
                        ));
                    }
                }
            }
        }
    }

    logger.info(format_args!(
        "direct-map: pre-switch check: identity {identity_checked} probe(s) mismatches={identity_mismatches}, \
         window {window_checked} probe(s) mismatches={window_mismatches}, saw a 2MiB window page={huge_seen}"
    ));
    if identity_mismatches > 0 || window_mismatches > 0 {
        logger.error(format_args!(
            "direct-map: the new table does not match (see mismatches above); refusing to switch CR3; halting"
        ));
        cpu::halt_forever();
    }

    // --- CR3 を新テーブルへ切り替える（M2-d と同じ作法） ---
    if !new_pml4.is_aligned(0x1000) {
        logger.error(format_args!(
            "direct-map: new PML4 {:#x} is not 4KiB aligned; refusing to switch CR3",
            new_pml4.as_u64()
        ));
        cpu::halt_forever();
    }

    // 切り替え後の低位到達性を確かめる材料。恒等側の既知バイトを控える。
    let (kernel_start_phys, _) = kernel_image_phys_range();
    let kernel_start = kernel_start_phys.as_u64();
    // SAFETY: kernel_start は恒等でマップ済み（M2-d の必須領域検証を通過している）。
    // 読み取りのみ。
    let kernel_byte_before = unsafe { core::ptr::read_volatile(kernel_start as *const u8) };

    // SAFETY: 直前の切り替え前検証で、恒等側に現在の RIP・RSP・pml4 のフレームが
    // すべて含まれていることを確認済み（恒等は M2-d と解決が一致）。
    unsafe {
        paging::switch::switch_to(new_pml4);
    }
    logger.info(format_args!("direct-map: CR3 switch instruction executed"));

    let new_cr3 = paging::switch::read_cr3();
    if new_cr3 != new_pml4 {
        logger.error(format_args!(
            "direct-map: CR3 readback {:#x} (expected {:#x}); halting",
            new_cr3.as_u64(),
            new_pml4.as_u64()
        ));
        cpu::halt_forever();
    }
    logger.info(format_args!(
        "direct-map: CR3 readback OK ({:#x})",
        new_cr3.as_u64()
    ));

    let mut post_ok = true;

    // (c) 低位（恒等）が切り替え後も生きていること。
    // SAFETY: kernel_start は新テーブルの恒等側にも含まれる。読み取りのみ。
    let kernel_byte_after = unsafe { core::ptr::read_volatile(kernel_start as *const u8) };
    let identity_alive = kernel_byte_after == kernel_byte_before;
    logger.info(format_args!(
        "direct-map: identity still reachable after switch = {identity_alive}"
    ));
    post_ok &= identity_alive;

    // (d) 窓経由の読み取りが、恒等経由と同じ物理を指すこと。
    let Some(kernel_window_virt) =
        VirtAddr::new(DirectMap::DIRECT_MAP_BASE.wrapping_add(kernel_start))
    else {
        logger.error(format_args!(
            "direct-map: kernel window address is not canonical; halting"
        ));
        cpu::halt_forever();
    };
    // SAFETY: kernel_start は kernel image の範囲内で、その範囲は窓の構築対象である
    // （map_range で全ページを張る）。切り替え前の窓プローブがその範囲の境界で解決を
    // 確認している。当該アドレス自体をプローブしているのではなく、範囲を全張りした構築と
    // 境界での独立検証に依る。読み取りのみ。
    let kernel_byte_via_window =
        unsafe { core::ptr::read_volatile(kernel_window_virt.as_ptr::<u8>()) };
    let window_read_ok = kernel_byte_via_window == kernel_byte_before;
    logger.info(format_args!(
        "direct-map: read via the window {:#x} matches identity = {window_read_ok}",
        kernel_window_virt.as_u64()
    ));
    post_ok &= window_read_ok;

    // (e) 別名の直接証明。窓経由で書いて、恒等経由で読む。同じ物理が 2 つの仮想から
    // 見えることの直接の証明で、ADR-0021 の移行が機能する核心である。
    if let Some(scratch) = allocator.allocate_frame() {
        let scratch_phys = scratch.as_u64();
        let Some(scratch_window_virt) =
            VirtAddr::new(DirectMap::DIRECT_MAP_BASE.wrapping_add(scratch_phys))
        else {
            logger.error(format_args!(
                "direct-map: scratch window address is not canonical; halting"
            ));
            cpu::halt_forever();
        };
        const ALIAS_PATTERN: u64 = 0xA11A_5000_D1EC_7000;
        // SAFETY: scratch は今確保した空きフレームで、恒等側にも窓側にもマップ済み
        // （切り替え前検証で恒等を、窓の probe で高位を確認した範囲に属する空き RAM）。
        // 他の誰も参照していない。書いて読むだけで、このあと解放する。
        let (via_identity, via_window) = unsafe {
            core::ptr::write_volatile(scratch_window_virt.as_mut_ptr::<u64>(), ALIAS_PATTERN);
            let via_identity = core::ptr::read_volatile(scratch_phys as *const u64);
            let via_window = core::ptr::read_volatile(scratch_window_virt.as_ptr::<u64>());
            (via_identity, via_window)
        };
        let alias_ok = via_identity == ALIAS_PATTERN && via_window == ALIAS_PATTERN;
        logger.info(format_args!(
            "direct-map: wrote {ALIAS_PATTERN:#x} via window {:#x}, read {:#x} via identity {:#x} = {alias_ok}",
            scratch_window_virt.as_u64(),
            via_identity,
            scratch_phys
        ));
        post_ok &= alias_ok;
        let _ = allocator.deallocate_frame(scratch);
    } else {
        logger.error(format_args!(
            "direct-map: could not allocate a scratch frame for the alias proof"
        ));
        post_ok = false;
    }

    if !post_ok {
        logger.error(format_args!(
            "direct-map: one or more post-switch checks failed (see above); halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "direct-map: verified. identity kept, direct map window live at base {:#x}. \
         the registered window stays identity until A-2.",
        DirectMap::DIRECT_MAP_BASE
    ));
}

/// higher-half A-2: 登録 DirectMap を恒等から高位窓へ差し替える。
///
/// A-1 で高位窓を張り CR3 も切り替えてあるので、差し替えた瞬間から
/// `direct_map().phys_to_virt` が高位を返し、その高位アドレスは有効である。恒等は残す
/// （A では外さない）。`replace_direct_map` の # Safety が要求する「CR3 を新窓のテーブルへ
/// 切り替えた後で呼ぶこと」を満たしている。
///
/// 破壊 (paging-test-directmap-wrong-base): 差し替える窓の base をわざと 1 ページずらす。
/// 直後の phys_to_virt 検証が期待値と食い違うのを捕まえ、以降の経路（フレームバッファ・
/// コンソール）が誤った高位を触る前に停止する。A-1 の low-window（窓の構築を壊す）とは
/// 層が違い、こちらは登録値を壊す。
fn activate_direct_map_window(logger: &mut Logger<SerialPort>) {
    use common::addr::{DirectMap, PhysAddr, VirtAddr};

    // 差し替え前は恒等（base=0）であることを確かめる。
    let before = common::addr::direct_map();
    if before.base().as_u64() != 0 {
        logger.error(format_args!(
            "direct-map A-2: the registered window is not identity before the swap \
             (base={:#x}); halting",
            before.base().as_u64()
        ));
        cpu::halt_forever();
    }

    let (base_raw, length) = if cfg!(feature = "paging-test-directmap-wrong-base") {
        (
            DirectMap::DIRECT_MAP_BASE + frame_allocator::FRAME_SIZE,
            DirectMap::IDENTITY_MAX_LENGTH - frame_allocator::FRAME_SIZE,
        )
    } else {
        (DirectMap::DIRECT_MAP_BASE, DirectMap::IDENTITY_MAX_LENGTH)
    };
    let Some(base) = VirtAddr::new(base_raw) else {
        logger.error(format_args!(
            "direct-map A-2: the window base {base_raw:#x} is not canonical; halting"
        ));
        cpu::halt_forever();
    };
    let Some(high) = DirectMap::new(base, length) else {
        logger.error(format_args!(
            "direct-map A-2: the window does not fit the canonical space; halting"
        ));
        cpu::halt_forever();
    };

    // SAFETY: A-1 が高位窓を張り CR3 を新テーブルへ切り替え済みで、恒等も残っている。
    // replace_direct_map の # Safety（新窓のテーブルへ切り替えた後で呼ぶこと）を満たす。
    // この区間に他の実行文脈は無い。AP を起こすのは `run_timer_loop` の中
    // （`interrupts.rs`）で、ここより後である。**この根拠は起動順に依っている。
    // AP を起こす位置がこの区間より前へ動くなら、書き直すこと。**
    if let Err(e) = unsafe { common::addr::replace_direct_map(high) } {
        logger.error(format_args!(
            "direct-map A-2: replace_direct_map failed: {e:?}; halting"
        ));
        cpu::halt_forever();
    }

    // 差し替え後、phys_to_virt が DIRECT_MAP_BASE + phys を返すことを数点で確かめる。
    // 期待値は常に正しい base（DIRECT_MAP_BASE）で計算するので、wrong-base の版はここで
    // 食い違って捕まり、以降の高位アクセスへ進まない。
    let now = common::addr::direct_map();
    let mut mismatches = 0u32;
    for raw in [0x1000u64, 0x20_0000, 0x8000_0000] {
        let Some(p) = PhysAddr::new(raw) else {
            continue;
        };
        let got = now.phys_to_virt(p).as_u64();
        let expected = DirectMap::DIRECT_MAP_BASE + raw;
        if got != expected {
            mismatches += 1;
            logger.error(format_args!(
                "direct-map A-2: phys_to_virt({raw:#x}) = {got:#x}, expected {expected:#x} = NG"
            ));
        }
    }
    if mismatches > 0 {
        logger.error(format_args!(
            "direct-map A-2: the registered window is wrong (see NG above); \
             halting before any high access"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "direct-map A-2: registered window active at base {:#x}; phys_to_virt now returns \
         high addresses. identity kept.",
        now.base().as_u64()
    ));
}

/// A-2: フレームバッファのハンドルを高位 base へ載せ替える。
///
/// `FramebufferLayout.base` は init_framebuffer の時点（差し替え前、恒等）で計算した値を
/// 保持しており、差し替えに追従しない（layout.rs の doc）。ここで高位 base で作り直す。
/// 恒等は残っているので旧ハンドル（恒等 base）でも描けるが、B で恒等を外すことに備えて
/// A-2 の時点で高位へ寄せておく。
///
/// バックバッファ側の base_virt は、init_console が差し替え後に `direct_map()` を引くので
/// 自動的に高位になる。ヒープ・RSP・boot_info は恒等を直接使うので A では触らない
/// （B で高位化する）。
fn rehome_framebuffer_to_window(
    logger: &mut Logger<SerialPort>,
    framebuffer: &mut Option<Framebuffer>,
    boot_info: &BootInfo,
) {
    use kernel::paging::active::ActivePageTable;

    let old_layout = match framebuffer.as_ref() {
        Some(fb) => *fb.layout(),
        None => return,
    };
    let fb_phys = boot_info.framebuffer.physical_address;
    let high_base = common::addr::direct_map().phys_to_virt(fb_phys);

    // 高位 base が実際にフレームバッファの物理を指すことを、稼働中テーブルを
    // 独立に辿って確かめる（arithmetic だけでなくマッピングの存在を見る）。
    // SAFETY: CR3 は A-1 のテーブルを指し、その配下は高位窓でも読める（窓は
    // 全マップ範囲を覆い、テーブルフレームは空き RAM 上にある）。
    let live = unsafe { ActivePageTable::current(common::addr::direct_map()) };
    match live.translate(high_base) {
        Ok(Some(t)) if t.phys == fb_phys => {}
        other => {
            logger.error(format_args!(
                "direct-map A-2: the high framebuffer base {:#x} does not map to {:#x} \
                 (got {other:?}); keeping the identity-based framebuffer",
                high_base.as_u64(),
                fb_phys.as_u64()
            ));
            return;
        }
    }

    let new_layout = match old_layout.with_base(high_base) {
        Ok(layout) => layout,
        Err(e) => {
            logger.error(format_args!(
                "direct-map A-2: could not rebase the framebuffer to {:#x}: {e:?}; keeping identity",
                high_base.as_u64()
            ));
            return;
        }
    };

    // SAFETY: new_layout は with_base の再検証を通り、high_base..end が高位窓でマップ済み
    // であることを直前に translate で確認した。フレームバッファは排他所有で、ここで旧
    // ハンドル（恒等 base）を捨てて新ハンドルへ差し替える。同じ物理を指す仮想が恒等と
    // 高位の 2 つあるが、書き込み手段はこの 1 個に統一する。
    let mut high_fb = unsafe { Framebuffer::new(new_layout) };

    // 高位 base 経由で書き、高位と恒等の両方から読み戻して、同じ物理が両窓から見える
    // ことを直接確かめる（A-1 の別名証明のフレームバッファ版）。この画素は直後の
    // コンソール全面クリアで消える。
    const PROOF: Color = Color::rgb(0xC0, 0x40, 0x80);
    high_fb.write_pixel(0, 0, PROOF);
    let proof_pixel = PROOF.to_pixel(new_layout.format());
    // SAFETY: high_base と fb_phys は同じ物理フレームバッファの先頭を指し、どちらも現在の
    // テーブルでマップ済みである（高位は直前の translate で、恒等は A-1 で確認済み）。
    // 読み取りのみ。
    let (via_high, via_identity) = unsafe {
        (
            core::ptr::read_volatile(high_base.as_ptr::<u32>()),
            core::ptr::read_volatile(fb_phys.as_u64() as *const u32),
        )
    };
    let readback_ok = via_high == proof_pixel && via_identity == proof_pixel;
    logger.info(format_args!(
        "direct-map A-2: framebuffer rehomed to {:#x}; wrote a proof pixel, read {via_high:#x}/{via_identity:#x} \
         via high/identity = {readback_ok}",
        high_base.as_u64()
    ));

    *framebuffer = Some(high_fb);
}

/// M5-b: カーネルスタックの直下の 1 ページを unmap してガードページにする。
///
/// これ以降、通常スタックが溢れて `kernel_guard` ページに触れると即座に #PF（CR2 = その
/// ページ）になる。#PF は IST2 上で動くので、溢れたスタックの上でハンドラを走らせずに
/// 済み、#DF へ昇格しない（ADR-0019 §3.1）。
///
/// # ガード幅を 1 ページにした根拠
///
/// 単一のスタックフレームがガード幅（4KiB）を一撃で飛び越えないことを前提にしている。
/// 現在コード全体でスタック上の単一配列の最大は `[u64; 512] = 4096` バイト（H-2 の PML4
/// スナップショット）で、`from_fn` が低位から要素ごとに書くのでガードに入れば必ず触れる。
/// 他はいずれも小さい。通常のフレーム伸長も暴走再帰も 1 段が 4KiB 未満なので、ガード
/// ページへ 1 段ずつ踏み込んで #PF になる。4KiB を超えるローカル配列を導入するなら、
/// ガード幅を再検討すること（`deferred-decisions.md` の「大きなスタック配列とガード幅」）。
///
/// # 2MiB ページに載っていたら、先に split する（S11-5 で配線した）
///
/// `StackBlock` は長らく `0x100000`〜`0x200000` の 4KiB フリンジにあり、`kernel_guard` は
/// 4KiB ページで張られていた。だから split せずに `unmap_4kib` だけで落とせた。
/// **M5-b では「使わない分岐を今書かない」ためにここで fail-fast させ、
/// `deferred-decisions.md` の「ガードページの split 化」へ条件を登録してあった。**
///
/// **S11-5 でその条件が発火した。** `spawn` のために遠征スタックを 16KiB から 64KiB へ
/// 広げ、像の緩衝（32KiB）を足したところ、**`interrupt-test` の構成で像が `0x400000` を
/// 越え、`0x200000..0x400000` が丸ごと 2MiB ページで張られるようになった**——
/// `StackBlock` はその中に居る。**登録しておいた条件が、まったく別の変更で現実になった**
/// （M5-d の NOP そりで同じことが起きている。`docs/troubleshooting.md`）。
///
/// **分岐は登録のとおりに書いた**——2MiB なら `split_huge_page` を通してから `unmap_4kib`
/// する。**機構は M5-a から在ったので、配線するだけである。**
fn install_kernel_stack_guard_page(
    logger: &mut Logger<SerialPort>,
    allocator: &mut kernel::frame_allocator::FrameAllocator,
) {
    let guard_virt = stack::kernel_guard_page().bottom;
    // **張る手順は `kernel::stack::install_guard_page` が持つ**（S12 前の手当ての C で
    // 寄せた）。**ワーカースタック側と同じ 1 本を通る**——あちらの doc に、
    // 2 つに分かれていたときに対処が片側にしか入らなかった経緯がある。
    //
    // SAFETY: 自前のページテーブルへ切り替え済みで、guard_virt はカーネルスタックの
    // 直下の 1 ページ。今後このページへ正規のアクセスは無い。
    unsafe {
        kernel::stack::install_guard_page(
            guard_virt,
            allocator,
            "stack-guard",
            "the kernel stack guard page",
            &mut |args| logger.info(args),
        );
    }
}

/// kernel イメージを高位（`KERNEL_VIRT_BASE + phys`）へ張る（B-2a）。
///
/// M2-d・A-1 の両テーブルで共通に使う。base=0 では、ビルダーが既に恒等で張った 4KiB PT を
/// 同一物理・同一フラグで上書きするだけで冪等になる（新規フレーム 0）。イメージは
/// `[0x100000, 0x200000)` の 4KiB 領域に収まるので、2MiB huge との衝突（`ensure_child` の
/// `UnexpectedHugePageEntry`）は起きない。base=高位（B-2a-3）では `PML4[511]` 配下に実
/// マッピングを作り、再リンク後にこのテーブルへ CR3 を切り替えても高位で走るコードが
/// 見え続けるようにする。丸めは 4KiB（H-2 と同一。要確認1）。
fn map_kernel_high_half<const CAP: usize>(
    builder: &mut PageTableBuilder<'_, CAP>,
    logger: &mut Logger<SerialPort>,
) {
    let (image_start, image_end) = kernel_image_phys_range();
    let image_len =
        (image_end.as_u64() - image_start.as_u64()).next_multiple_of(frame_allocator::FRAME_SIZE);
    let high_start = kernel::kernel_virt_from_phys(image_start);
    // 高位マッピングが消費した中間テーブルのフレーム数を会計する。base=0 では恒等が
    // 既に張った PT を上書きするだけなので 0 のはずで、それをログで確かめる。base=高位
    // （B-2a-3）では PML4[511] 配下の新規部分木の分だけ増える。
    let frames_before = builder.frames_used();
    if let Err(e) = builder.map_range(high_start, image_start, image_len, true) {
        logger.error(format_args!(
            "higher-half: kernel high mapping ({:#x} -> phys {:#x}, len {:#x}) failed: {e:?}",
            high_start.as_u64(),
            image_start.as_u64(),
            image_len
        ));
        cpu::halt_forever();
    }
    let high_frames = builder.frames_used() - frames_before;
    logger.info(format_args!(
        "higher-half: kernel image {:#x}..{:#x} mapped at {:#x} (len {:#x}), \
         {high_frames} new page-table frame(s)",
        image_start.as_u64(),
        image_end.as_u64(),
        high_start.as_u64(),
        image_len
    ));
}

/// kernel イメージの高位マッピングを持つ新しいテーブルを構築し、検証する（H-2）。
///
/// # この段階でやること・やらないこと
///
/// CR3 は切り替えない。恒等マッピングで動いたまま、新しいテーブルを組み立てて読み戻す
/// ところまでである。切り替えは H-3 で行う。
///
/// 稼働中のテーブルには一切手を加えない。新しいテーブルを別に作る。構築の前後で稼働中
/// テーブルの PML4 を読み戻し、変わっていないことを確かめる。
///
/// direct map の高位窓はこの段階の対象外である。作るのは kernel イメージの高位マッピング
/// （`KERNEL_VIRT_BASE + (phys - LMA)`）だけで、これは direct map とは別の対応である。
/// ログでもそう明示する。
fn build_and_verify_high_half(
    logger: &mut Logger<SerialPort>,
    allocator: &mut frame_allocator::FrameAllocator<{ kernel::paging::plan::DEFAULT_CAPACITY }>,
    mapped: &MappedRanges<{ kernel::paging::plan::DEFAULT_CAPACITY }>,
    direct_map: common::addr::DirectMap,
) {
    use kernel::paging::verify;

    // 稼働中テーブルの PML4 を、構築の前に控える。
    // SAFETY: CR3 は自前のテーブルを指しており、恒等マッピングで読める。
    let live_pml4 =
        unsafe { kernel::paging::active::ActivePageTable::current(direct_map) }.pml4_phys();
    let live_before: [u64; entry_count()] = core::array::from_fn(|index| {
        // SAFETY: 稼働中の PML4 は有効なテーブルで、添字は 512 未満。
        unsafe { verify::read_pml4_entry(live_pml4, direct_map, index) }
    });

    let frames_before = allocator.free_frame_count();

    // --- 恒等部分を作る（現在の plan と同じ結果になる）---
    let mut builder = match PageTableBuilder::new(allocator, direct_map) {
        Ok(builder) => builder,
        Err(error) => {
            logger.error(format_args!(
                "high-half: failed to start the build: {error:?}"
            ));
            cpu::halt_forever();
        }
    };
    let mut map_error = None;
    resolve_pages(mapped, |m| {
        if map_error.is_some() {
            return;
        }
        if let Err(e) = builder.map_page(m.phys_addr, m.huge, m.cacheable) {
            map_error = Some(e);
        }
    });
    if let Some(error) = map_error {
        logger.error(format_args!("high-half: identity build failed: {error:?}"));
        cpu::halt_forever();
    }

    // --- kernel イメージの高位マッピングを張る ---
    //
    // 対応は virt = phys + KERNEL_VIRT_BASE。direct map の窓とは別の対応である。
    let (image_start, image_end) = kernel_image_phys_range();
    let image_len = image_end.as_u64() - image_start.as_u64();
    let image_len = image_len.next_multiple_of(frame_allocator::FRAME_SIZE);
    let high_start = kernel::kernel_virt_from_phys(image_start);
    if let Err(error) = builder.map_range(high_start, image_start, image_len, true) {
        logger.error(format_args!(
            "high-half: kernel high mapping failed: {error:?}"
        ));
        cpu::halt_forever();
    }

    let new_pml4 = builder.pml4_phys();
    let frames_used = frames_before - allocator.free_frame_count();
    logger.info(format_args!(
        "high-half: built a new table at PML4 {:#x} using {frames_used} frame(s) ({} KiB)",
        new_pml4.as_u64(),
        frames_used * frame_allocator::FRAME_SIZE / 1024
    ));
    logger.info(format_args!(
        "high-half: kernel image {:#x}..{:#x} is also mapped at {:#x} (kernel high mapping, \
         NOT the direct map window)",
        image_start.as_u64(),
        image_start.as_u64() + image_len,
        high_start.as_u64()
    ));

    // --- 独立 walker で読み戻す ---
    //
    // 構築に使った関数は呼ばない。`verify::walk` は階層の降り方もビットの解釈も
    // 別に書いてある。
    let mut checked = 0u32;
    let mut mismatches = 0u32;
    let probe_count = 8u64;
    for index in 0..probe_count {
        let offset = image_len / probe_count * index;
        let Some(phys) = image_start.checked_add(offset) else {
            mismatches += 1;
            continue;
        };
        let virt = kernel::kernel_virt_from_phys(phys);
        // SAFETY: `new_pml4` は今構築したテーブルで、恒等マッピングで読める。
        match unsafe { verify::walk(new_pml4, direct_map, virt) } {
            Ok(resolved) => {
                checked += 1;
                if resolved.phys != phys {
                    mismatches += 1;
                    logger.error(format_args!(
                        "high-half: {:#x} resolves to {:#x}, expected {:#x}",
                        virt.as_u64(),
                        resolved.phys.as_u64(),
                        phys.as_u64()
                    ));
                }
            }
            Err(error) => {
                mismatches += 1;
                logger.error(format_args!(
                    "high-half: {:#x} does not resolve: {error:?}",
                    virt.as_u64()
                ));
            }
        }
    }
    logger.info(format_args!(
        "high-half: walked {checked} probe(s) through the kernel high mapping with an \
         independent walker, mismatches={mismatches}"
    ));

    // --- 恒等部分が新テーブルでも同じ対応であること ---
    let mut identity_mismatches = 0u32;
    for range in mapped.iter() {
        for probe in [range.start, range.end.checked_sub(1).unwrap_or(range.start)] {
            let Some(virt) = common::addr::VirtAddr::new(probe.as_u64()) else {
                identity_mismatches += 1;
                continue;
            };
            // SAFETY: 上記と同じ。
            match unsafe { verify::walk(new_pml4, direct_map, virt) } {
                Ok(resolved) if resolved.phys == probe => {}
                _ => identity_mismatches += 1,
            }
        }
    }
    logger.info(format_args!(
        "high-half: the identity part of the new table matches the plan, mismatches={identity_mismatches}"
    ));

    // --- 稼働中テーブルが無傷であること ---
    let live_after: [u64; entry_count()] = core::array::from_fn(|index| {
        // SAFETY: 上記と同じ。
        unsafe { verify::read_pml4_entry(live_pml4, direct_map, index) }
    });
    let live_untouched = live_before == live_after;
    logger.info(format_args!(
        "high-half: the live table's PML4 is untouched by the build = {live_untouched}"
    ));

    if mismatches > 0 || identity_mismatches > 0 || !live_untouched {
        logger.error(format_args!(
            "high-half: the new table does not match the plan; halting"
        ));
        cpu::halt_forever();
    }
}

/// PML4 のエントリ数。`core::array::from_fn` の型引数に使う。
const fn entry_count() -> usize {
    512
}

/// リンカが定義する kernel イメージの範囲を、物理アドレスとして得る。
///
/// # なぜ変換を関数にするのか
///
/// リンカシンボルは**仮想アドレス**である。かつてはリンクアドレスが
/// `0x100000` の恒等マッピングで、値がそのまま物理アドレスとしても
/// 通っていた。そのため `as u64` で済ませても動いていた。
///
/// **higher-half 移行（B-2a-3）でここが変わった。** 現在リンカが返すのは
/// `0xFFFFFFFF80100000` 付近の仮想アドレスで、物理としては使えない。
/// 一方、フレームアロケータやマップ計画が必要とするのは物理アドレスである。
///
/// # この変換は direct map ではない
///
/// **direct physical map 経由の変換とは別の関係である。** kernel イメージの
/// 物理位置は「リンクアドレスとロードアドレスの差」で決まる。bootloader が
/// ELF をどこへ置いたかで決まるものであって、direct map の窓とは無関係で
/// ある。その差を引くのは `kernel::kernel_phys_from_virt` で、値は `link.ld` の
/// `KERNEL_VIRT_BASE` から生成される。
/// 詳細は `docs/deferred-decisions.md` を参照。
fn kernel_image_phys_range() -> (common::addr::PhysAddr, common::addr::PhysAddr) {
    use common::addr::VirtAddr;

    // リンカシンボルは仮想アドレスとして受け取る。
    let start_virt = VirtAddr::new(core::ptr::addr_of!(__kernel_start) as u64)
        .expect("the linker places the kernel at a canonical address");
    let end_virt = VirtAddr::new(core::ptr::addr_of!(__kernel_end) as u64)
        .expect("the linker places the kernel at a canonical address");

    (
        kernel::kernel_phys_from_virt(start_virt),
        kernel::kernel_phys_from_virt(end_virt),
    )
}

/// 物理アドレスの範囲がマップ計画に含まれるか。
///
/// 恒等マッピングの間の橋渡しである。呼び出し側はまだ `u64` で範囲を持っており、
/// `MappedRanges` は `PhysAddr` を扱う。物理として表せない値は「含まれない」とする。
fn range_is_mapped<const CAP: usize>(mapped: &MappedRanges<CAP>, start: u64, end: u64) -> bool {
    match (
        common::addr::PhysAddr::new(start),
        common::addr::PhysAddr::new(end),
    ) {
        (Some(s), Some(e)) => mapped.contains_range(s, e),
        _ => false,
    }
}

/// 稼働中のページテーブルを読み戻し、`plan` の意図と突き合わせる（M5-a-1）。
///
/// M4 で `sgdt` / `sidt` / PIC の IMR に対して行ってきたのと同じことを、ページテーブルに
/// 対して行う。これまでページテーブルだけは書きっぱなしで、読み戻す手段が無かった。
///
/// あわせて、TLB の全フラッシュ（CR3 リロード）が成立する条件も実測する。
fn verify_page_tables(
    logger: &mut Logger<SerialPort>,
    mapped: &MappedRanges<{ kernel::paging::plan::DEFAULT_CAPACITY }>,
) {
    use kernel::paging::active::{ActivePageTable, PageSize};
    use kernel::paging::entry;

    // SAFETY: CR3 は直前に自前のテーブルへ切り替えて読み戻し済みで、
    // 恒等マッピングによりテーブル自体を読める。
    let table = unsafe { ActivePageTable::current(common::addr::direct_map()) };
    logger.info(format_args!(
        "paging: walking the live tables from PML4 {:#x}",
        table.pml4_phys().as_u64()
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
            range.start.as_u64(),
            range.start.as_u64() + (range.end.as_u64() - range.start.as_u64()) / 2,
            range.end.as_u64() - 1,
        ];
        for probe in probes {
            let Some(probe_virt) = common::addr::VirtAddr::new(probe) else {
                mismatches += 1;
                logger.error(format_args!("paging: {probe:#x} is not canonical"));
                continue;
            };
            match table.translate(probe_virt) {
                Ok(Some(translation)) => {
                    checked += 1;
                    // 恒等マッピングなので、物理 == 仮想でなければならない。
                    if translation.phys.as_u64() != probe {
                        mismatches += 1;
                        logger.error(format_args!(
                            "paging: {probe:#x} translates to {:#x} (identity mapping broken)",
                            translation.phys.as_u64()
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
    if let Ok(Some(translation)) = table.translate(common::addr::VirtAddr::new_const(0x10_0000)) {
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
    //
    // T-2b で、この区別は実行時の検査から型へ移った。非正規アドレスは `VirtAddr` を
    // 構築できないので、`translate` へ渡すことがそもそもできない。確認するのは
    // 「翻訳が拒否するか」ではなく「型が構築を拒否するか」になる。
    let non_canonical_ok = common::addr::VirtAddr::new(0x0000_8000_0000_0000).is_none();
    logger.info(format_args!(
        "paging: a non-canonical address cannot even be built as a VirtAddr = {}",
        if non_canonical_ok { "OK" } else { "NG" }
    ));

    // G ビットが 1 つでも立っていたら停止する。
    //
    // 「G ビットを一切立てていない」は、architecture.md と ADR-0018 が維持していると
    // 主張している性質で、M5-a-1 が「CR3 リロードで TLB を全部追い出せる」と結論した
    // 根拠でもある。M5-a-2 の 2MiB ページ分割は、その結論の上に手順を組んでいる。
    //
    // 数えて WARN を出すだけでは、主張の強さと検査の強さが釣り合わない。立っていたら
    // 前提が崩れているので、そこで止める方が正しい。`plan` には G ビットを立てる経路が
    // 無いので、通常はここに掛からない。
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

/// 立っている feature を 1 行へ並べる（S12 前の手当て）。
///
/// **区切りはカンマである。** `xtask` が渡す形（`--features a,b`）と同じにしておくと、
/// ログから構成をそのまま貼り直せる。
struct FeatureList(&'static [&'static str]);

impl core::fmt::Display for FeatureList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (index, name) in self.0.iter().enumerate() {
            if index > 0 {
                write!(f, ",")?;
            }
            write!(f, "{name}")?;
        }
        Ok(())
    }
}
