//! SMP の下ごしらえ（S1）。**この段では情報を集めるだけで、AP は起こさない。**
//!
//! ここが持つのは、S3（AP 起こし）で要るが**S1 の時点でしか確保できないもの**である。
//! 現在はトランポリン用フレームだけが該当する。

use core::sync::atomic::{AtomicU64, Ordering};

use core::fmt::Write as _;

use common::addr::PhysAddr;
use common::cpu;
use common::log::Logger;
use common::serial::SerialPort;

use crate::frame_allocator::{FrameAllocator, FRAME_SIZE};

/// AP のトランポリンを置ける物理アドレスの上限（この値未満）。
///
/// # なぜ 1MiB 未満なのか
///
/// AP は SIPI（Startup IPI）で起こす。SIPI が運べるのは**8 ビットのベクタ**だけで、
/// AP はリアルモードで `vector << 12` から実行を始める。したがって開始アドレスは
/// 物理 `0x00000`〜`0xFF000` に限られる。**ZaytOS のカーネルイメージは物理
/// `0x100000`（ちょうど 1MiB）から始まる**ので、トランポリンはイメージの外、
/// 1MiB 未満に別途確保するしかない。
///
/// # なぜ `0xA0000` ではなく `0x9F000` なのか
///
/// 実測では、1MiB 未満の空きは `0x1000..0xA0000` の 159 フレームだけである
/// （`0xA0000` 以降はレガシー領域で、UEFI メモリマップに `EfiConventionalMemory`
/// として現れない）。上限を `0xA0000` にしても届く範囲としては足りるが、
/// **1 ページぶんの余裕を残す**ために `0x9F000` にしてある。トランポリンのコードが
/// 1 ページに収まらなかった場合、次のページへ跨ぐ余地が要るためである。
///
/// **収まらない場合の隣接ページの確保は、この段では扱わない。** S1 は 1 枚しか
/// 予約せず、隣が空いている保証も与えない（S3 の到達条件へ送った）。
pub const TRAMPOLINE_MAX_START: u64 = 0x9F000;

/// 予約が無いことを表す値。物理アドレス 0 はフレームアロケータが必ず除外するので
/// （ヌルポインタ対策）、有効な予約と衝突しない。
const NO_FRAME: u64 = 0;

/// S1-d で予約したトランポリン用フレームの物理アドレス。
static TRAMPOLINE_FRAME: AtomicU64 = AtomicU64::new(NO_FRAME);

/// トランポリン用フレームの予約に失敗した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrampolineError {
    /// 空きフレームが 1 枚も無い。
    NoFreeFrame,
    /// 取れたが 1MiB 未満ではない。**取れたフレームはアロケータへ返してある**
    /// ので、この失敗で空きが減ることはない。
    TooHigh { got: PhysAddr },
}

/// トランポリン用フレームを 1 枚予約する。
///
/// # 呼ぶ位置
///
/// **フレームアロケータのスモークテストの直後、本流ページテーブルの構築より前。**
/// `allocate_frame` は常に最小のフレーム番号から配るので、ここが「1MiB 未満が
/// まだ誰にも取られていない」唯一の地点である。これより後ろへ移すと、ページ
/// テーブルが低位から食っていくため、取れなくなる。
///
/// # 失敗しても停止しない
///
/// S1 は情報を集める段であり、AP はまだ起こさない。ここで停止すると、現在
/// 単一コアで動いているカーネルが「トランポリン用の 1 枚が取れない」だけで
/// 起動しなくなり、機能的な後退になる。**失敗は大きく報告して継続する。**
/// 致命として扱うのは S3（AP 起こし）である。
pub fn reserve_trampoline_frame<const CAP: usize>(
    allocator: &mut FrameAllocator<CAP>,
) -> Result<PhysAddr, TrampolineError> {
    let Some(frame) = allocator.allocate_frame() else {
        return Err(TrampolineError::NoFreeFrame);
    };
    if frame.as_u64() >= TRAMPOLINE_MAX_START {
        // 取ったものを返す。失敗で空きが減らないようにする。
        let _ = allocator.deallocate_frame(frame);
        return Err(TrampolineError::TooHigh { got: frame });
    }
    TRAMPOLINE_FRAME.store(frame.as_u64(), Ordering::Relaxed);
    Ok(frame)
}

/// 予約済みのトランポリン用フレーム。まだ予約していなければ `None`。
///
/// **S1 の時点では誰も呼ばない。** それでも `dead_code` にならないのは `pub` だから
/// であって、使われているからではない（公開範囲が広いと未使用が見えない、という
/// 一般則をここでは意図的に使っている）。**S3 で実際に読まれることを、その段の
/// 到達条件にしてある**（`roadmap.md`）。そうしないと、使い忘れても誰も気づかない。
pub fn trampoline_frame() -> Option<PhysAddr> {
    match TRAMPOLINE_FRAME.load(Ordering::Relaxed) {
        NO_FRAME => None,
        value => PhysAddr::new(value),
    }
}

/// 予約したフレームが SIPI のベクタとして表せるか（4KiB 境界にあるか）。
///
/// `allocate_frame` はフレーム単位で配るので、境界から外れることは通常起きない。
/// **検査というより、SIPI のベクタ計算（`vector << 12`）が成立する前提を
/// コードの形で残すためのものである。**
pub fn is_sipi_addressable(frame: PhysAddr) -> bool {
    frame.as_u64().is_multiple_of(FRAME_SIZE) && frame.as_u64() < TRAMPOLINE_MAX_START
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 高位のフレームしか持たないアロケータ。**失敗経路を判定側だけ閉じるため**の
    /// もので、起動が継続することまでは確かめられない（それは S3 で致命へ格上げ
    /// するときに見る）。
    fn allocator_with_only_high_frames() -> FrameAllocator<4> {
        let mut allocator = FrameAllocator::<4>::new();
        // 物理 1MiB（フレーム番号 0x100）から 16 枚。
        allocator.insert_free_range(0x100, 16).unwrap();
        allocator
    }

    #[test]
    fn a_high_only_allocator_is_rejected() {
        let mut allocator = allocator_with_only_high_frames();
        let error = reserve_trampoline_frame(&mut allocator).unwrap_err();
        match error {
            TrampolineError::TooHigh { got } => assert_eq!(got.as_u64(), 0x100000),
            other => panic!("expected TooHigh, got {other:?}"),
        }
        // 予約は成立していない。
        assert_eq!(trampoline_frame(), None);
        // 取ったフレームは返してあるので、空きは減っていない。
        assert_eq!(allocator.free_frame_count(), 16);
    }

    #[test]
    fn an_empty_allocator_reports_no_free_frame() {
        let mut allocator = FrameAllocator::<4>::new();
        assert_eq!(
            reserve_trampoline_frame(&mut allocator).unwrap_err(),
            TrampolineError::NoFreeFrame
        );
    }

    #[test]
    fn a_low_frame_is_sipi_addressable() {
        assert!(is_sipi_addressable(PhysAddr::new(0x1000).unwrap()));
        assert!(is_sipi_addressable(PhysAddr::new(0x9E000).unwrap()));
    }

    #[test]
    fn the_limit_itself_is_not_addressable() {
        // 上限は「この値未満」なので、境界そのものは弾く。
        assert!(!is_sipi_addressable(
            PhysAddr::new(TRAMPOLINE_MAX_START).unwrap()
        ));
    }

    #[test]
    fn a_frame_above_one_mib_is_not_addressable() {
        assert!(!is_sipi_addressable(PhysAddr::new(0x100000).unwrap()));
    }
}

// ===========================================================================
// S3-b-2b-1: AP トランポリン
//
// AP は SIPI で **リアルモード、物理 `vector << 12`** から実行を始める。
// このコードは予約フレームへ**コピーして**使うので、**リンクされた高位 VA では
// 実行されない。** したがって次の 2 つを守る。
//
// - **16 ビット部は `DS = CS` にしてフレーム内オフセットだけで参照する**
//   （フレームの物理アドレスは実行時に決まるので、絶対アドレスを焼けない）。
// - **絶対値が要る箇所（GDTR のベース、CR3、RSP、64 ビットの飛び先）は
//   BSP がコピー後に書き込む。** 自己再配置はしない。
//
// **CR3 に載せるのは `zaytos_boot_pml4`（静的初期テーブル）である。**
// 本番テーブルは恒等（`PML4[0]`）を除去済みなので、載せると次の命令フェッチが
// 解決できない。**この静的テーブルは `PML4[0]`（低位 1GiB の恒等）と
// `PML4[511]`（高位）を持つ。**
//
// **`PML4[256]`（direct map）は持たない。** したがって **AP のスタックは恒等側の
// VA（物理 == 仮想）でなければならない。** direct map 窓の VA を渡すと、
// 64 ビットへ入った直後の最初の push で落ちる。
// ===========================================================================

/// トランポリン内のレイアウト（フレーム先頭からのオフセット）。
///
/// **BSP がここを直接書き換える**ので、asm 側と数値を共有する。
/// 片方だけ動かすと静かに壊れるため、`const` を asm のテンプレート引数へ渡す。
pub(crate) mod layout {
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
    /// **4 バイト境界に載らない。** 擬似記述子は `limit: u16` に `base: u32` が
    /// 続く形なので、ベースは必ず 2 mod 4 の位置に来る。**整列を仮定した書き込みを
    /// してはならない**（`write_volatile` は整列を要求する。実際に踏んだ）。
    pub const PATCH_GDTR_BASE: u64 = GDTR + 2;
    /// far jump のオペコード（`0x66 0xEA`）の長さ。飛び先の u32 はこの後ろに来る。
    ///
    /// **位置を定数で持たない。** far jump は `mov cr0` の直後でなければならず、
    /// そこまでのコード長は書き換えるたびに変わる。**シンボル
    /// `zaytos_ap_tramp_farjmp` からの相対で求める。**
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
    // 一時 GDT をロードする。**ベースは BSP が書き込んだ線形アドレスである。**
    // **32 ビットオペランド形式で読ませる**（`0x66` 前置）。前置が無いと
    // `lgdtw` になり、**ベースの上位 8 ビットが捨てられる。** 予約フレームは
    // 1MiB 未満なので今は 24 ビットに収まるが、**収まることに依存しない。**
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
    // **`mov cr0` の直後に置く。間に 1 バイトも挟まない。** ページングを有効に
    // した次の命令フェッチはもう保護モードの世界で起きるので、**`.org` の
    // 詰め物（ゼロバイト）がここに入ると、それが命令として実行される。**
    // 実際に踏んだ（AP が署名を出さず、原因が見えなかった）。
    //
    // **バイトで書く。** `0x66 0xEA` は ptr16:32 の直接 far jump で、
    // アセンブラの構文に依存せずオペコードとオフセットの位置を固定できる
    // （`push imm8` の符号拡張で採ったのと同じ理由）。
    // **飛び先の u32 は BSP が書き込む。** 位置は下のシンボルで受け渡す。
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
    // **RSP は恒等 VA である**（direct map はこのテーブルに無い）。
    // **64 ビットでは絶対アドレスの `[disp32]` を書かない。** GAS は
    // それを **RIP 相対**として符号化するので、フレーム先頭からのオフセットの
    // つもりが「今の RIP からの相対」になり、まったく別の場所を読む。
    // **実際に踏んだ**（RSP に garbage が入り、次のアクセスで落ちた）。
    //
    // **RIP 相対であることを明示し、同じセクション内のラベルを指す。**
    // トランポリンはコピーされて走るが、**コピー先でも RIP とデータの相対距離は
    // 変わらない**ので、これは位置独立である。
    "  mov rsp, [rip + zaytos_ap_tramp_data_rsp]",
    // 自分の索引を第 1 引数へ。**GDT に依存しない身元の出所である。**
    "  mov rdi, [rip + zaytos_ap_tramp_data_index]",
    // 起きたことを BSP へ知らせる。**BSP はこれをポーリングして次の AP へ進む。**
    "  mov qword ptr [rip + zaytos_ap_tramp_data_started], 1",
    // 高位 VA の Rust の入口へ。**戻らない。**
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
    // データブロック。**64 ビット側は RIP 相対でここを指す**ので、
    // 各フィールドにラベルを置く。オフセット定数（BSP が書き込む側）と
    // **同じ位置を指していることが、レイアウトの唯一の接点である。**
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

/// AP 用スタックとして予約したフレーム（S3-b-2b-1）。`0` は未予約。
///
/// # なぜ起動最初期に予約するのか
///
/// AP を起こすのは `run_timer_loop` の中（`sti` より後）だが、**そこには
/// フレームアロケータが無い。** トランポリン用フレームと同じ理由で、
/// **取れる位置で取っておく。**
///
/// # **恒等 VA で使う。だから低位でなければならない**
///
/// AP の CR3 は静的初期テーブルで、そこには `PML4[256]`（direct map）が無い。
/// **したがって AP はこのフレームを恒等 VA（物理 == 仮想）で触る。**
/// 恒等が覆うのは低位 1GiB なので、**予約したフレームがそこに入ることを確かめる。**
static AP_STACK_FRAMES: [AtomicU64; MAX_APS] = [const { AtomicU64::new(NO_FRAME) }; MAX_APS];

/// 起こしうる AP の本数（bootstrap processor を除く）。
const MAX_APS: usize = common::percpu::MAX_CPUS - 1;

/// 恒等写像が覆う上限。静的初期テーブルの `PML4[0]` は 2MiB ページ 512 本で
/// **低位 1GiB** を覆う（`kernel/src/main.rs` の `zaytos_boot_pd_shared`）。
const IDENTITY_LIMIT: u64 = 1024 * 1024 * 1024;

/// AP 用スタックのフレームを予約する（S3-b-2b-1）。
///
/// # 呼ぶ位置
///
/// **トランポリン用フレームの予約の直後。** フレームアロケータが最小のフレーム
/// 番号から配るうちに取る（恒等の範囲に入ることを確実にする）。
pub fn reserve_ap_stacks<const CAP: usize>(
    allocator: &mut FrameAllocator<CAP>,
) -> Result<usize, TrampolineError> {
    let mut reserved = 0;
    for slot in AP_STACK_FRAMES.iter() {
        let Some(frame) = allocator.allocate_frame() else {
            return Err(TrampolineError::NoFreeFrame);
        };
        if frame.as_u64() >= IDENTITY_LIMIT {
            let _ = allocator.deallocate_frame(frame);
            return Err(TrampolineError::TooHigh { got: frame });
        }
        slot.store(frame.as_u64(), Ordering::Relaxed);
        reserved += 1;
    }
    Ok(reserved)
}

/// 予約済みの AP 用スタックフレーム。
pub fn ap_stack_frame(index: usize) -> Option<PhysAddr> {
    match AP_STACK_FRAMES.get(index)?.load(Ordering::Relaxed) {
        NO_FRAME => None,
        value => PhysAddr::new(value),
    }
}

/// AP が最初に入る Rust の関数（S3-b-2b-1）。**戻らない。**
///
/// # ここで触れるものは限られている
///
/// CR3 は静的初期テーブルで、**`PML4[256]`（direct map）が無い。** したがって
/// **direct map 越しに触るものは一切使えない。** 使えるのは
/// **ポート I/O（シリアル）と、高位 VA の静的データ**である。
///
/// **`cpu_id()` を呼ばない。** `sgdt` 由来の実装は自コアの GDT がロードされた後
/// でなければ正しくないが、この段の AP は per-CPU GDT を持たない
/// （`kernel/src/gdt/mod.rs` の載荷条件）。**身元は引数で受け取る。**
///
/// **ロックを取らない。** `Logger` と `SerialPort` にロックは無いので、
/// **BSP が 1 つずつ起こすことで混線を避けている**（同時に書くとバイトが混ざる）。
#[no_mangle]
pub extern "C" fn zaytos_ap_entry(index: u64) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[INFO] smp: application processor {index} started (long mode reached, running on the \
         static boot page table; no per-CPU GDT/TSS/IDT yet, so cpu_id() is not used here)"
    );
    AP_STARTED.fetch_add(1, Ordering::SeqCst);
    // **割り込みは有効化しない。** IDT を持たないので、来ても行き先が無い。
    cpu::halt_forever()
}

/// 起動署名を出した AP の本数。**BSP が会計に使う。**
static AP_STARTED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 起動署名を出した AP の本数。
pub fn started_ap_count() -> usize {
    AP_STARTED.load(Ordering::SeqCst)
}

/// AP を起こした結果（S3-b-2b-1）。
pub struct WakeReport {
    /// MADT が報告した使用可能なコア数（bootstrap processor を含む）。
    pub usable: usize,
    /// 起こそうとした AP の本数。
    pub attempted: usize,
    /// 起動署名を出した AP の本数。
    pub started: usize,
    /// **`MAX_CPUS` を超えるので起こさなかった AP の本数。**
    pub skipped_no_slot: usize,
}

/// AP を起こす（S3-b-2b-1）。**1 本ずつ起こし、次へ進む前に完了を待つ。**
///
/// # なぜ 1 本ずつなのか
///
/// `Logger` と `SerialPort` にロックが無いので、**2 つ以上の AP が同時に書くと
/// バイトが混ざる。** そして**どの AP が失敗したかを切り分けられなくなる。**
/// 直列にする費用はコア数 × 10ms 程度で、実害が無い。
///
/// # 起こす本数は `MAX_CPUS` で制限する
///
/// **`MAX_CPUS` を超えるコアは起こさない**（`roadmap.md` の S3-b-2b-1）。
/// 超えた分を起こすと、per-CPU スロットを持てない AP が生まれる。
/// **起こさなかった本数を返して、検査がそれを主張できるようにする。**
///
/// # 待ち時間
///
/// INIT の後に 10ms、SIPI の後に 10ms 待つ。**規格は SIPI の後 200µs だが、
/// タイマのティックが 10ms 粒度なのでそれで代用する**（下限より長いだけなので
/// 安全側である。TSC は較正していないので新しい時間源を導入しない）。
///
/// # Safety
///
/// - `mapped` が写像済みの Local APIC を指すこと。
/// - タイマが動いていること（10ms の待ちをティックのエッジで作る）。
/// - 起動時に 1 回だけ呼ぶこと。
pub unsafe fn wake_application_processors(
    logger: &mut Logger<SerialPort>,
    mapped: &crate::apic::MappedApic,
    mmio: &crate::acpi::ApicMmio,
) -> WakeReport {
    let lapic_virt = crate::apic::lapic_virt_of(mapped);
    let usable = mmio.usable_local_apics();

    let Some(frame) = trampoline_frame() else {
        logger.error(format_args!(
            "smp: no AP trampoline frame was reserved, so no AP can be started; halting \
             (S1 reserved this frame and S3-b-2b-1 makes the failure fatal)"
        ));
        cpu::halt_forever();
    };

    // **トランポリンを予約フレームへコピーし、絶対値を書き込む。**
    // SAFETY: frame は S1 が予約した 4KiB 境界の物理フレームで、他の誰も使わない。
    // BSP は本番 CR3 で走るので direct map 越しに触る（AP は恒等で触る）。
    let installed = unsafe { install_trampoline(logger, frame) };

    let bsp = mmio.bsp_candidate_apic_id();
    let mut report = WakeReport {
        usable,
        attempted: 0,
        started: 0,
        skipped_no_slot: 0,
    };

    // bootstrap processor を除いた AP を、MADT の並び順で起こす。
    let mut slot = 1usize;
    for apic_id in mmio.local_apic_ids() {
        if Some(apic_id) == bsp {
            continue;
        }
        if slot >= common::percpu::MAX_CPUS {
            report.skipped_no_slot += 1;
            logger.warn(format_args!(
                "smp: not starting the application processor with apic id {apic_id}: there are \
                 only {} per-CPU slot(s) and slot {slot} would be out of range. This is the \
                 documented policy (do not start more CPUs than MAX_CPUS)",
                common::percpu::MAX_CPUS
            ));
            continue;
        }

        let Some(stack) = ap_stack_frame(slot - 1) else {
            logger.error(format_args!(
                "smp: no stack frame was reserved for application processor slot {slot}; halting"
            ));
            cpu::halt_forever();
        };

        // **スタック頂点は恒等 VA である。** 静的初期テーブルに direct map が
        // 無いので、direct map の VA を渡すと最初の push で落ちる。
        let stack_top_identity = stack.as_u64() + FRAME_SIZE;
        // SAFETY: installed はコピー済みのトランポリンで、data ブロックの位置は
        // レイアウト定数で決まっている。AP はまだ走っていない。
        unsafe { installed.set_ap_parameters(stack_top_identity, slot as u64) };

        report.attempted += 1;
        let before = started_ap_count();
        logger.info(format_args!(
            "smp: starting application processor apic id {apic_id} as slot {slot} \
             (trampoline at {:#x}, vector {:#04x}, stack top {:#x} identity-mapped, \
             cr3 {:#x} = the static boot page table)",
            frame.as_u64(),
            installed.sipi_vector,
            stack_top_identity,
            installed.cr3
        ));

        // INIT → 待つ → SIPI → 待つ → **まだ起きていなければ**もう 1 回 SIPI。
        //
        // **2 回目を無条件に送ってはならない。** 既に走り出した AP へ SIPI を
        // 送ると、**long mode で走っている最中に開始ベクタから再実行させる**
        // ことになり、16 ビットのバイト列を 64 ビットとして解釈して #GP →
        // トリプルフォルトする。**実際に踏んだ**（CPU 1 が CS64・GDTR=0 で
        // オフセット 0x15 に落ちた）。規格が 2 回目を許すのは「1 回目が
        // 届かなかった場合」であって、常に 2 回送れという意味ではない。
        // SAFETY: 写像済みの Local APIC。起動時の 1 回だけ。
        let ok = unsafe {
            crate::apic::send_init_ipi(lapic_virt, apic_id) && {
                wait_ticks(AP_WAKE_WAIT_TICKS);
                crate::apic::send_startup_ipi(lapic_virt, apic_id, installed.sipi_vector)
            }
        };
        wait_ticks(AP_WAKE_WAIT_TICKS);
        let ok = ok
            && (started_ap_count() > before || {
                // SAFETY: 同上。**まだ起きていないときだけ**送る。
                unsafe { crate::apic::send_startup_ipi(lapic_virt, apic_id, installed.sipi_vector) }
            });
        if !ok {
            logger.error(format_args!(
                "smp: an IPI to apic id {apic_id} never left the local APIC (delivery status \
                 stayed set); halting"
            ));
            cpu::halt_forever();
        }

        // 起動署名を待つ。**上限つきで待つ**（CLAUDE.md §14）。
        let mut started = false;
        for _ in 0..AP_START_WAIT_TICKS {
            wait_ticks(1);
            if started_ap_count() > before {
                started = true;
                break;
            }
        }
        if started {
            report.started += 1;
        } else {
            logger.error(format_args!(
                "smp: application processor apic id {apic_id} did not report its start signature \
                 within {AP_START_WAIT_TICKS} tick(s)"
            ));
        }
        slot += 1;
    }

    report
}

/// AP を起こすときの各段の待ちティック数。1 ティック = 10ms（100Hz）。
const AP_WAKE_WAIT_TICKS: u64 = 1;
/// 起動署名を待つ上限（ティック）。
const AP_START_WAIT_TICKS: u64 = 50;

/// タイマのティックが `count` 回進むまで待つ。
///
/// **上限のない待ちにならない。** ティックが止まっていれば進まないが、
/// 呼び出し側が回数で上限を持つ。
fn wait_ticks(count: u64) {
    let start = crate::idt::timer_ticks();
    while crate::idt::timer_ticks().wrapping_sub(start) < count {
        core::hint::spin_loop();
    }
}

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
    /// **スタック頂点は恒等 VA で渡すこと**（AP の CR3 に direct map が無い）。
    ///
    /// # Safety
    ///
    /// 対象の AP がまだ走っていないこと。**走っている AP のデータブロックを
    /// 書き換えてはならない。**
    pub unsafe fn set_ap_parameters(&self, stack_top_identity: u64, index: u64) {
        // SAFETY: direct_map_base は写像済みの予約フレームの先頭で、
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
    /// **書き込み先は 4 バイト境界に載らない**（GDTR のベースも far jump の
    /// 飛び先も、オペコードや `u16` の直後に来る）。`write_volatile` は整列を
    /// 要求するので使えない。**バイトごとに書く。**
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
/// 雛形はカーネルイメージ内の高位 VA にリンクされている。**AP は物理
/// `vector << 12` から実行を始めるので、そのままでは走らせられない。**
///
/// # 何を書き込むのか。**自己再配置はしない**
///
/// 16 ビット部は `DS = CS` でフレーム内オフセットだけを使うので位置独立だが、
/// **線形アドレスが要る 2 箇所だけは実行時の値でなければ成立しない。**
///
/// - `lgdt` が読む擬似記述子のベース（一時 GDT の線形アドレス）
/// - long mode へ入った直後の far jump の飛び先
///
/// **BSP がフレームの物理アドレスを知っているので、コピー後に書き込む。**
/// 恒等写像の下では物理 == 線形なので、そのまま使える。
///
/// # Safety
///
/// `frame` が予約済みで 4KiB 境界にあり、誰も使っていないこと。
unsafe fn install_trampoline(
    logger: &mut Logger<SerialPort>,
    frame: PhysAddr,
) -> InstalledTrampoline {
    extern "C" {
        static zaytos_ap_tramp_start: u8;
        static zaytos_ap_tramp_end: u8;
        static zaytos_ap_tramp_farjmp: u8;
    }

    let src = core::ptr::addr_of!(zaytos_ap_tramp_start) as u64;
    // far jump の位置は、`mov cr0` の直後に置く制約からコード長で決まる。
    // **定数で持たず、シンボルからの相対で求める。**
    let farjmp_offset = core::ptr::addr_of!(zaytos_ap_tramp_farjmp) as u64 - src;
    let end = core::ptr::addr_of!(zaytos_ap_tramp_end) as u64;
    let len = end - src;

    // **1 ページに収まることを確かめる。** 収まらない場合の隣接ページの確保は
    // S1 から送った申し送りで、**この段で致命として扱う。**
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

    // AP が使う CR3。**静的初期テーブルの物理**である。
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

    // **恒等の下では物理がそのまま線形アドレスである。** AP はその世界で走る。
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
            zaytos_ap_entry as *const () as u64,
        );
    }

    // 破壊 (S3-b-2b-1, smp-tramp-corrupt-copy): 設置済みのコピーを 1 バイト壊す。
    // **パッチされる 3 領域の外**を狙うので、雛形との比較が捕まえるはずである。
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
        zaytos_ap_entry as *const () as u64
    ));

    installed
}

/// 設置したトランポリンが雛形と一致することを確かめる（S3-b-2b-1）。
///
/// # なぜ要るのか
///
/// この段の実装では、**16 ビット / 64 ビットの符号化の取り違えを 4 件踏んだ**
/// （`.org` の詰め物が `mov cr0` の直後に入る、`lgdtw` になる、整列を仮定した
/// 書き込み、64 ビットの `[disp32]` が RIP 相対になる）。**いずれも AP 側でしか
/// 落ちず、BSP 側は正常に見える。** コードを触ったときに静かに戻るのを、
/// **設置後のバイト比較で捕まえる。**
///
/// # 比較の形。**パッチされる箇所は除外し、位置はシンボルから導く**
///
/// 比較するのは「設置済みのコピー」と「`.rodata.aptramp` の雛形」である。
/// BSP が書き込む 3 領域だけを除外する。
///
/// **除外位置を定数で持たない。** far jump の位置は `mov cr0` の直後という制約
/// から決まり、**コードを 1 バイト変えるたびに動く。** 実際に `0x40` と置いた
/// 定数が実は `0x42` だった。**シンボルから導けば、動いても追随する。**
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

    // BSP が書き込む領域。**ここだけを除外する。**
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
