//! AP のトランポリン（雛形・設置・照合）。
//!
//! **`kernel/src/smp.rs` から移した**（2026-09-28。境界の段階の手順 2）。**SIPI で起きた AP が、リアルモードから
//! 長モードへ入るまでの 16 ビットと 64 ビットのコードと、その設置と照合なので、CPU 固有の置き場に置く。**
//! **どの AP をいつ起こすかと、長モードへ入った AP が跳ぶ Rust の入口は `smp` に残る**（入口は呼ぶ側が渡す）。

use common::addr::PhysAddr;
use common::arch::x86_64::cpu;
use common::log::Logger;
use common::machine::pc::serial::SerialPort;

use crate::frame_allocator::FRAME_SIZE;

// ===========================================================================
// S3-b-2b-1: AP トランポリン
//
// AP は SIPI で リアルモード、物理 `vector << 12` から実行を始める。
// このコードは予約フレームへコピーして使うので、リンクされた高位 VA では
// 実行されない。したがって次の 2 つを守る。
//
// - 16 ビット部は `DS = CS` にしてフレーム内オフセットだけで参照する
//   （フレームの物理アドレスは実行時に決まるので、絶対アドレスを焼けない）。
// - 絶対値が要る箇所（GDTR のベース、CR3、RSP、64 ビットの飛び先）は
//   BSP がコピー後に書き込む。自己再配置はしない。
//
// CR3 に載せるのは `zaytos_boot_pml4`（静的初期テーブル）である。
// 本番テーブルは恒等（`PML4[0]`）を除去済みなので、載せると次の命令フェッチが
// 解決できない。この静的テーブルは `PML4[0]`（低位 1GiB の恒等）と
// `PML4[511]`（高位）を持つ。
//
// `PML4[256]`（direct map）は持たない。したがって AP のスタックは恒等側の
// VA（物理 == 仮想）でなければならない。direct map ウィンドウの VA を渡すと、
// 64 ビットへ入った直後の最初の push で落ちる。
// ===========================================================================

/// トランポリン内のレイアウト（フレーム先頭からのオフセット）。
///
/// BSP がここを直接書き換えるので、asm 側と数値を共有する。
/// 片方だけ動かすと静かに壊れるため、`const` を asm のテンプレート引数へ渡す。
mod layout {
    /// 64 ビットコードの入口。
    pub const CODE64: u64 = 0x100;
    /// 一時 GDT（null / 64bit code / data）。
    pub const GDT: u64 = 0xF00;
    /// `lgdt` に渡す擬似記述子（limit u16 + base u32）。
    pub const GDTR: u64 = 0xF20;
    /// BSP が値を書き込むデータブロック。
    pub const DATA: u64 = 0xF40;
    /// `DATA` 内のオフセット。
    pub const DATA_CR3: u64 = 0x00;
    pub const DATA_RSP: u64 = 0x08;
    pub const DATA_ENTRY64: u64 = 0x10;
    pub const DATA_INDEX: u64 = 0x18;
    pub const DATA_STARTED: u64 = 0x20;
    /// 16 ビット部の中の、GDTR のベース（u32）を書く位置。
    ///
    /// 4 バイト境界に載らない。擬似記述子は `limit: u16` に `base: u32` が
    /// 続く形なので、ベースは必ず 2 mod 4 の位置に来る。整列を仮定した書き込みを
    /// してはならない（`write_volatile` は整列を要求する。実際に踏んだ）。
    pub const PATCH_GDTR_BASE: u64 = GDTR + 2;
    /// far jump のオペコード（`0x66 0xEA`）の長さ。飛び先の u32 はこの後ろに来る。
    ///
    /// 位置を定数で持たない。far jump は `mov cr0` の直後でなければならず、
    /// そこまでのコード長は書き換えるたびに変わる。シンボル
    /// `zaytos_ap_tramp_farjmp` からの相対で求める。
    pub const FARJMP_OPCODE_LEN: u64 = 2;
}

core::arch::global_asm!(
    ".section .rodata.aptramp,\"a\",@progbits",
    ".p2align 12",
    ".globl zaytos_ap_tramp_start",
    "zaytos_ap_tramp_start:",
    ".code16",
    // --- リアルモード。DS = CS にしてフレーム内を参照できるようにする ---
    "  cli",
    "  cld",
    "  mov ax, cs",
    "  mov ds, ax",
    // 一時 GDT をロードする。ベースは BSP が書き込んだ線形アドレスである。
    // 32 ビットオペランド形式で読ませる（`0x66` 前置）。前置が無いと
    // `lgdtw` になり、ベースの上位 8 ビットが捨てられる。予約フレームは
    // 1MiB 未満なので今は 24 ビットに収まるが、収まることに依存しない。
    "  .byte 0x66",
    "  lgdt [{gdtr}]",
    // CR4.PAE を立てる。
    "  mov eax, cr4",
    "  or eax, {cr4_pae}",
    "  mov cr4, eax",
    // CR3 に静的初期テーブルの物理を載せる（BSP が書き込んだ値）。
    "  mov eax, [{data} + {d_cr3}]",
    "  mov cr3, eax",
    // IA32_EFER.LME を立てる。
    "  mov ecx, {efer}",
    "  rdmsr",
    "  or eax, {efer_lme}",
    "  wrmsr",
    // CR0.PG と PE を同時に立てて long mode へ入る。
    "  mov eax, cr0",
    "  or eax, {cr0_pg_pe}",
    "  mov cr0, eax",
    // 64 ビットコードセグメントへ far jump する。
    //
    // `mov cr0` の直後に置く。間に 1 バイトも挟まない。ページングを有効に
    // した次の命令フェッチはもう保護モードの世界で起きるので、`.org` の
    // 詰め物（ゼロバイト）がここに入ると、それが命令として実行される。
    // 実際に踏んだ（AP が署名を出さず、原因が見えなかった）。
    //
    // バイトで書く。`0x66 0xEA` は ptr16:32 の直接 far jump で、
    // アセンブラの構文に依存せずオペコードとオフセットの位置を固定できる
    // （`push imm8` の符号拡張で採ったのと同じ理由）。
    // 飛び先の u32 は BSP が書き込む。位置は下のシンボルで受け渡す。
    ".globl zaytos_ap_tramp_farjmp",
    "zaytos_ap_tramp_farjmp:",
    "  .byte 0x66, 0xea",
    "  .long 0",            // 飛び先（オペコード 2 バイトの後ろ）
    "  .word 0x08",         // 一時 GDT の 64bit コードセレクタ
    // --- 64 ビット ---
    ".org 0x100",
    ".code64",
    // データセグメントを null にしておく（64 ビットでは無視されるが、
    // 残留値で紛れないようにする）。
    "  xor eax, eax",
    "  mov ds, ax",
    "  mov es, ax",
    "  mov ss, ax",
    // RSP は恒等 VA である（direct map はこのテーブルに無い）。
    // 64 ビットでは絶対アドレスの `[disp32]` を書かない。GAS は
    // それを RIP 相対として符号化するので、フレーム先頭からのオフセットの
    // つもりが「今の RIP からの相対」になり、まったく別の場所を読む。
    // 実際に踏んだ（RSP に garbage が入り、次のアクセスで落ちた）。
    //
    // RIP 相対であることを明示し、同じセクション内のラベルを指す。
    // トランポリンはコピーされて走るが、コピー先でも RIP とデータの相対距離は
    // 変わらないので、これは位置独立である。
    "  mov rsp, [rip + zaytos_ap_tramp_data_rsp]",
    // 自分の索引を第 1 引数へ。GDT に依存しない身元の出所である。
    "  mov rdi, [rip + zaytos_ap_tramp_data_index]",
    // 起動したことを BSP へ知らせる。BSP はこれをポーリングして次の AP へ進む。
    "  mov qword ptr [rip + zaytos_ap_tramp_data_started], 1",
    // 高位 VA の Rust の入口へ。戻らない。
    "  mov rax, [rip + zaytos_ap_tramp_data_entry]",
    "  jmp rax",
    // --- 一時 GDT と GDTR ---
    ".org 0xF00",
    "  .quad 0",                       // null
    "  .quad 0x00AF9A000000FFFF",      // 0x08: 64bit code, DPL0, L=1
    "  .quad 0x00CF92000000FFFF",      // 0x10: data
    ".org 0xF20",
    ".globl zaytos_ap_tramp_gdtr",
    "zaytos_ap_tramp_gdtr:",
    "  .word 23",                      // limit = 3*8 - 1
    "  .long 0",                       // base（BSP が書き込む）
    ".org 0xF40",
    // データブロック。64 ビット側は RIP 相対でここを指すので、
    // 各フィールドにラベルを置く。オフセット定数（BSP が書き込む側）と
    // 同じ位置を指していることが、レイアウトの唯一の接点である。
    "zaytos_ap_tramp_data_cr3:     .quad 0",
    "zaytos_ap_tramp_data_rsp:     .quad 0",
    "zaytos_ap_tramp_data_entry:   .quad 0",
    "zaytos_ap_tramp_data_index:   .quad 0",
    "zaytos_ap_tramp_data_started: .quad 0",
    ".org 0x1000",
    ".globl zaytos_ap_tramp_end",
    "zaytos_ap_tramp_end:",
    ".code64",
    gdtr = const layout::GDTR,
    data = const layout::DATA,
    d_cr3 = const layout::DATA_CR3,
    cr4_pae = const 1u32 << 5,
    efer = const 0xC000_0080u32,
    efer_lme = const 1u32 << 8,
    cr0_pg_pe = const (1u32 << 31) | 1,
);

/// 予約フレームへ書き込んだトランポリンの実体（S3-b-2b-1）。
pub struct InstalledTrampoline {
    /// BSP から見た書き込み先（direct map の VA）。
    direct_map_base: u64,
    /// AP に渡す CR3（静的初期テーブルの物理）。
    pub cr3: u64,
    /// SIPI に載せるベクタ。`vector << 12` が開始物理アドレスになる。
    pub sipi_vector: u8,
}

impl InstalledTrampoline {
    /// この AP に渡すスタック頂点と索引を書き込む。
    ///
    /// スタック頂点は恒等 VA で渡すこと（AP の CR3 に direct map が無い）。
    ///
    /// # Safety
    ///
    /// 対象の AP がまだ走っていないこと。走っている AP のデータブロックを
    /// 書き換えてはならない。
    pub unsafe fn set_ap_parameters(&self, stack_top_identity: u64, index: u64) {
        // SAFETY: direct_map_base はマップ済みの予約フレームの先頭で、
        // オフセットはレイアウト定数の範囲内である。
        unsafe {
            self.write_u64(layout::DATA + layout::DATA_RSP, stack_top_identity);
            self.write_u64(layout::DATA + layout::DATA_INDEX, index);
            self.write_u64(layout::DATA + layout::DATA_STARTED, 0);
        }
    }

    /// # Safety
    ///
    /// `offset` がフレーム内であること。
    unsafe fn write_u64(&self, offset: u64, value: u64) {
        // SAFETY: 呼び出し元契約。MMIO ではないが、AP が読む先なので
        // 最適化で消えないよう volatile で書く。
        unsafe { ((self.direct_map_base + offset) as *mut u64).write_volatile(value) }
    }

    /// 整列を仮定せずに `u32` を書く。
    ///
    /// 書き込み先は 4 バイト境界に載らない（GDTR のベースも far jump の
    /// 飛び先も、オペコードや `u16` の直後に来る）。`write_volatile` は整列を
    /// 要求するので使えない。バイトごとに書く。
    ///
    /// # Safety
    ///
    /// `offset` から 4 バイトがフレーム内であること。
    unsafe fn write_u32_unaligned(&self, offset: u64, value: u32) {
        // SAFETY: 呼び出し元契約。1 バイトずつなので整列の要求が無い。
        unsafe {
            let dst = (self.direct_map_base + offset) as *mut u8;
            for (i, byte) in value.to_le_bytes().iter().enumerate() {
                dst.add(i).write_volatile(*byte);
            }
        }
    }
}

/// トランポリンの雛形を予約フレームへコピーし、絶対値を書き込む（S3-b-2b-1）。
///
/// # なぜコピーするのか
///
/// 雛形はカーネルイメージ内の高位 VA にリンクされている。AP は物理
/// `vector << 12` から実行を始めるので、そのままでは走らせられない。
///
/// # 何を書き込むのか。自己再配置はしない
///
/// 16 ビット部は `DS = CS` でフレーム内オフセットだけを使うので位置独立だが、
/// 線形アドレスが要る 2 箇所だけは実行時の値でなければ成立しない。
///
/// - `lgdt` が読む擬似記述子のベース（一時 GDT の線形アドレス）
/// - long mode へ入った直後の far jump の飛び先
///
/// BSP がフレームの物理アドレスを知っているので、コピー後に書き込む。
/// 恒等マッピングの下では物理 == 線形なので、そのまま使える。
///
/// `entry` は、長モードへ入った AP が跳ぶ Rust の入口である（`smp` が渡す）。
///
/// # Safety
///
/// `frame` が予約済みで 4KiB 境界にあり、誰も使っていないこと。
pub unsafe fn install_trampoline(
    logger: &mut Logger<SerialPort>,
    frame: PhysAddr,
    entry: extern "C" fn(u64) -> !,
) -> InstalledTrampoline {
    extern "C" {
        static zaytos_ap_tramp_start: u8;
        static zaytos_ap_tramp_end: u8;
        static zaytos_ap_tramp_farjmp: u8;
    }

    let src = core::ptr::addr_of!(zaytos_ap_tramp_start) as u64;
    // far jump の位置は、`mov cr0` の直後に置く制約からコード長で決まる。
    // 定数で持たず、シンボルからの相対で求める。
    let farjmp_offset = core::ptr::addr_of!(zaytos_ap_tramp_farjmp) as u64 - src;
    let end = core::ptr::addr_of!(zaytos_ap_tramp_end) as u64;
    let len = end - src;

    // 1 ページに収まることを確かめる。収まらない場合の隣接ページの確保は
    // S1 から送った申し送りで、この段階で致命として扱う。
    if len > FRAME_SIZE {
        logger.error(format_args!(
            "smp: the AP trampoline is {len} bytes, which does not fit in the reserved {FRAME_SIZE} \
             byte frame; the adjacent page is not reserved (S1 reserved exactly one); halting"
        ));
        cpu::halt_forever();
    }

    let direct_map_base = common::addr::direct_map().phys_to_virt(frame).as_u64();

    // SAFETY: 雛形は KEEP された 1 ページのセクションで、書き込み先は予約済みの
    // フレームである。長さは上で確かめた。重なりは無い（別の物理）。
    unsafe {
        core::ptr::copy_nonoverlapping(src as *const u8, direct_map_base as *mut u8, len as usize);
    }

    // AP が使う CR3。静的初期テーブルの物理である。
    extern "C" {
        static zaytos_boot_pml4: u8;
    }
    let cr3 = crate::kernel_phys_from_virt(
        common::addr::VirtAddr::new(core::ptr::addr_of!(zaytos_boot_pml4) as u64)
            .expect("the static boot page table has a canonical address"),
    )
    .as_u64();

    let installed = InstalledTrampoline {
        direct_map_base,
        cr3,
        // `vector << 12` が開始物理アドレスになる。
        sipi_vector: (frame.as_u64() >> 12) as u8,
    };

    // 恒等の下では物理がそのまま線形アドレスである。AP はその世界で走る。
    let identity_base = frame.as_u64();
    // SAFETY: 直上でコピーしたフレームで、オフセットはレイアウト定数である。
    unsafe {
        installed.write_u32_unaligned(
            layout::PATCH_GDTR_BASE,
            (identity_base + layout::GDT) as u32,
        );
        installed.write_u32_unaligned(
            farjmp_offset + layout::FARJMP_OPCODE_LEN,
            (identity_base + layout::CODE64) as u32,
        );
        installed.write_u64(layout::DATA + layout::DATA_CR3, cr3);
        installed.write_u64(
            layout::DATA + layout::DATA_ENTRY64,
            entry as *const () as u64,
        );
    }

    // 破壊テスト (S3-b-2b-1, smp-tramp-corrupt-copy): 設置済みのコピーを 1 バイト壊す。
    // パッチされる 3 領域の外を狙うので、雛形との比較が検出するはずである。
    #[cfg(feature = "smp-tramp-corrupt-copy-test")]
    // SAFETY: コピー済みのフレーム内。オフセット 0 は `cli` のバイトである。
    unsafe {
        (direct_map_base as *mut u8).write_volatile(0x90);
    }

    if !verify_installed_trampoline(logger, &installed, src, len, farjmp_offset) {
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "smp: AP trampoline installed at {:#x} ({len} of {FRAME_SIZE} bytes used, fits in one \
         page={}), gdt at {:#x}, 64-bit entry at {:#x}, cr3 {:#x}, rust entry {:#x}",
        identity_base,
        len <= FRAME_SIZE,
        identity_base + layout::GDT,
        identity_base + layout::CODE64,
        cr3,
        entry as *const () as u64
    ));

    installed
}

/// 設置したトランポリンが雛形と一致することを確かめる（S3-b-2b-1）。
///
/// # なぜ要るのか
///
/// この段階の実装では、16 ビット / 64 ビットの符号化の取り違えを 4 件踏んだ
/// （`.org` の詰め物が `mov cr0` の直後に入る、`lgdtw` になる、整列を仮定した
/// 書き込み、64 ビットの `[disp32]` が RIP 相対になる）。いずれも AP 側でしか
/// 落ちず、BSP 側は正常に見える。コードを触ったときに静かに戻るのを、
/// 設置後のバイト比較で検出する。
///
/// # 比較の形。パッチされる箇所は除外し、位置はシンボルから導く
///
/// 比較するのは「設置済みのコピー」と「`.rodata.aptramp` の雛形」である。
/// BSP が書き込む 3 領域だけを除外する。
///
/// 除外位置を定数で持たない。far jump の位置は `mov cr0` の直後という制約
/// から決まり、コードを 1 バイト変えるたびに動く。実際に `0x40` と置いた
/// 定数が実は `0x42` だった。シンボルから導けば、動いても追随する。
fn verify_installed_trampoline(
    logger: &mut Logger<SerialPort>,
    installed: &InstalledTrampoline,
    src: u64,
    len: u64,
    farjmp_offset: u64,
) -> bool {
    extern "C" {
        static zaytos_ap_tramp_gdtr: u8;
        static zaytos_ap_tramp_data_cr3: u8;
        static zaytos_ap_tramp_data_started: u8;
    }
    let gdtr_off = core::ptr::addr_of!(zaytos_ap_tramp_gdtr) as u64 - src;
    let data_off = core::ptr::addr_of!(zaytos_ap_tramp_data_cr3) as u64 - src;
    let data_end = core::ptr::addr_of!(zaytos_ap_tramp_data_started) as u64 - src + 8;

    // BSP が書き込む領域。ここだけを除外する。
    let patched: [(u64, u64); 3] = [
        (gdtr_off + 2, gdtr_off + 6),
        (
            farjmp_offset + layout::FARJMP_OPCODE_LEN,
            farjmp_offset + layout::FARJMP_OPCODE_LEN + 4,
        ),
        (data_off, data_end),
    ];

    let mut mismatches = 0usize;
    let mut first_mismatch = 0u64;
    for offset in 0..len {
        if patched.iter().any(|(lo, hi)| offset >= *lo && offset < *hi) {
            continue;
        }
        // SAFETY: どちらも長さ `len` の有効な領域である（雛形はセクション、
        // コピー先は予約フレーム）。読み取りのみ。
        let (a, b) = unsafe {
            (
                ((src + offset) as *const u8).read_volatile(),
                ((installed.direct_map_base + offset) as *const u8).read_volatile(),
            )
        };
        if a != b {
            if mismatches == 0 {
                first_mismatch = offset;
            }
            mismatches += 1;
        }
    }

    let excluded: u64 = patched.iter().map(|(lo, hi)| hi - lo).sum();
    logger.info(format_args!(
        "smp: the installed AP trampoline matches the template outside the patched fields: \
         {} byte(s) compared, {excluded} excluded (gdtr base at {:#x}, far jump target at {:#x}, \
         data block {:#x}..{:#x}), mismatches={mismatches}",
        len - excluded,
        gdtr_off + 2,
        farjmp_offset + layout::FARJMP_OPCODE_LEN,
        data_off,
        data_end
    ));
    if mismatches != 0 {
        logger.error(format_args!(
            "smp: the installed AP trampoline diverges from the template at offset \
             {first_mismatch:#x} ({mismatches} byte(s) differ); the copy or a patch is wrong; \
             halting"
        ));
    }
    mismatches == 0
}
