//! SMP の下ごしらえ（S1）。この段では情報を集めるだけで、AP は起こさない。
//!
//! ここが持つのは、S3（AP 起こし）で要るがS1 の時点でしか確保できないものである。
//! 現在はトランポリン用フレームだけが該当する。

use core::sync::atomic::{AtomicU64, Ordering};

use core::fmt::Write as _;

use crate::paging::active::{ActivePageTable, PageAttributes};
#[allow(unused_imports)]
use crate::paging::verify;
use common::addr::{PhysAddr, VirtAddr};
use common::cpu;
use common::log::Logger;
use common::serial::SerialPort;

use crate::frame_allocator::{FrameAllocator, FRAME_SIZE};

/// AP のトランポリンを置ける物理アドレスの上限（この値未満）。
///
/// # なぜ 1MiB 未満なのか
///
/// AP は SIPI（Startup IPI）で起こす。SIPI が運べるのは8 ビットのベクタだけで、
/// AP はリアルモードで `vector << 12` から実行を始める。したがって開始アドレスは
/// 物理 `0x00000`〜`0xFF000` に限られる。ZaytOS のカーネルイメージは物理
/// `0x100000`（ちょうど 1MiB）から始まるので、トランポリンはイメージの外、
/// 1MiB 未満に別途確保するしかない。
///
/// # なぜ `0xA0000` ではなく `0x9F000` なのか
///
/// 実測では、1MiB 未満の空きは `0x1000..0xA0000` の 159 フレームだけである
/// （`0xA0000` 以降はレガシー領域で、UEFI メモリマップに `EfiConventionalMemory`
/// として現れない）。上限を `0xA0000` にしても届く範囲としては足りるが、
/// 1 ページぶんの余裕を残すために `0x9F000` にしてある。トランポリンのコードが
/// 1 ページに収まらなかった場合、次のページへ跨ぐ余地が要るためである。
///
/// 収まらない場合の隣接ページの確保は、この段では扱わない。S1 は 1 枚しか
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
    /// 取れたが 1MiB 未満ではない。取れたフレームはアロケータへ返してある
    /// ので、この失敗で空きが減ることはない。
    TooHigh { got: PhysAddr },
}

/// トランポリン用フレームを 1 枚予約する。
///
/// # 呼ぶ位置
///
/// フレームアロケータのスモークテストの直後、本流ページテーブルの構築より前。
/// `allocate_frame` は常に最小のフレーム番号から配るので、ここが「1MiB 未満が
/// まだ誰にも取られていない」唯一の地点である。これより後ろへ移すと、ページ
/// テーブルが低位から食っていくため、取れなくなる。
///
/// # 失敗しても停止しない
///
/// S1 は情報を集める段であり、AP はまだ起こさない。ここで停止すると、現在
/// 単一コアで動いているカーネルが「トランポリン用の 1 枚が取れない」だけで
/// 起動しなくなり、機能的な後退になる。失敗は大きく報告して継続する。
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
/// S1 の時点では誰も呼ばない。それでも `dead_code` にならないのは `pub` だから
/// であって、使われているからではない（公開範囲が広いと未使用が見えない、という
/// 一般則をここでは意図的に使っている）。S3 で実際に読まれることを、その段の
/// 到達条件にしてある（`roadmap.md`）。そうしないと、使い忘れても誰も気づかない。
pub fn trampoline_frame() -> Option<PhysAddr> {
    match TRAMPOLINE_FRAME.load(Ordering::Relaxed) {
        NO_FRAME => None,
        value => PhysAddr::new(value),
    }
}

/// 予約したフレームが SIPI のベクタとして表せるか（4KiB 境界にあるか）。
///
/// `allocate_frame` はフレーム単位で配るので、境界から外れることは通常起きない。
/// 検査というより、SIPI のベクタ計算（`vector << 12`）が成立する前提を
/// コードの形で残すためのものである。
pub fn is_sipi_addressable(frame: PhysAddr) -> bool {
    frame.as_u64().is_multiple_of(FRAME_SIZE) && frame.as_u64() < TRAMPOLINE_MAX_START
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 高位のフレームしか持たないアロケータ。失敗経路を判定側だけ閉じるための
    /// もので、起動が継続することまでは確かめられない（それは S3 で致命へ格上げ
    /// するときに見る）。
    fn allocator_with_only_high_frames() -> FrameAllocator<4> {
        let mut allocator = FrameAllocator::<4>::new();
        // 物理 1MiB（フレーム番号 0x100）から 16 枚。
        allocator.insert_free_range(0x100, 16).unwrap();
        allocator
    }

    /// `map_ap_stacks` が実際に張る並びから、通常スタックの頂点を導く。
    ///
    /// 本番のコードではなくテスト側に置いてある。production 側に同じ式を
    /// 2 本持つと片方だけが古くなるので、照合する側にだけ独立に書く。
    /// 並び = ガード + kernel + ガード + IST1 + ガード + IST2（`AP_STACK_STRIDE`）。
    fn kernel_top_from_the_layout(slot: usize) -> u64 {
        AP_STACK_REGION_BASE
            + (slot as u64) * AP_STACK_STRIDE
            + crate::stack::GUARD_SIZE as u64
            + crate::stack::KERNEL_STACK_SIZE as u64
    }

    /// AP 用アイドルタスクへ記述する範囲が、実際に張った通常スタックと一致する
    /// （S4-c-3-2a）。
    ///
    /// # なぜホストテストで守るのか
    ///
    /// `schedule_switch` の範囲検査は切り替えが起きたときにしか走らない。
    /// AP 用アイドルタスクでは切り替えが起きないので、この記述が嘘でも実行時
    /// には誰も気づかない（気づくのは将来ここで切り替えが起きたときで、
    /// そのとき初めて落ちる）。実行時に照合されない記述を守れるのは、ここだけ
    /// である。
    #[test]
    fn the_recorded_ap_kernel_stack_range_matches_the_mapped_layout() {
        let slot = 1usize;
        let top = kernel_top_from_the_layout(slot);
        let (bottom, recorded_top) = kernel_stack_bounds_from_top(top);

        assert_eq!(recorded_top, top);
        // 幅はちょうど通常スタック 1 本ぶんで、IST を含んでいない。
        assert_eq!(
            recorded_top - bottom,
            crate::stack::KERNEL_STACK_SIZE as u64
        );

        // 下端はガードの穴より上にある。ガードは張らない穴なので、
        // 範囲がそこへ食い込むと「ガードの上で走ってよい」と記述したことになる。
        let slot_base = AP_STACK_REGION_BASE + (slot as u64) * AP_STACK_STRIDE;
        assert_eq!(bottom, slot_base + crate::stack::GUARD_SIZE as u64);

        // IST1 の下端より下にある（範囲が IST へ食い込んでいない）。
        let ist1_bottom = recorded_top + crate::stack::GUARD_SIZE as u64;
        assert!(recorded_top <= ist1_bottom);

        // 次のスロットの領域へはみ出していない。
        assert!(recorded_top <= AP_STACK_REGION_BASE + ((slot + 1) as u64) * AP_STACK_STRIDE);

        // 起動ログで実測した値に釘付けする（S4-c-3-2a、`-smp 2`、スロット 1）。
        // 算術が合っていても定数がずれれば動くので、実測値を 1 点持っておく。
        assert_eq!(bottom, 0xffff_8100_0001_c000);
        assert_eq!(recorded_top, 0xffff_8100_0002_c000);
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
// VA（物理 == 仮想）でなければならない。direct map 窓の VA を渡すと、
// 64 ビットへ入った直後の最初の push で落ちる。
// ===========================================================================

/// トランポリン内のレイアウト（フレーム先頭からのオフセット）。
///
/// BSP がここを直接書き換えるので、asm 側と数値を共有する。
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
    // 起きたことを BSP へ知らせる。BSP はこれをポーリングして次の AP へ進む。
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

/// AP 用スタックとして予約したフレーム（S3-b-2b-1）。`0` は未予約。
///
/// # なぜ起動最初期に予約するのか
///
/// AP を起こすのは `run_timer_loop` の中（`sti` より後）だが、そこには
/// フレームアロケータが無い。トランポリン用フレームと同じ理由で、
/// 取れる位置で取っておく。
///
/// # 恒等 VA で使う。だから低位でなければならない
///
/// AP の CR3 は静的初期テーブルで、そこには `PML4[256]`（direct map）が無い。
/// したがって AP はこのフレームを恒等 VA（物理 == 仮想）で触る。
/// 恒等が覆うのは低位 1GiB なので、予約したフレームがそこに入ることを確かめる。
static AP_STACK_FRAMES: [AtomicU64; MAX_APS] = [const { AtomicU64::new(NO_FRAME) }; MAX_APS];

/// 起こしうる AP の本数（bootstrap processor を除く）。
const MAX_APS: usize = common::percpu::MAX_CPUS - 1;

/// 恒等写像が覆う上限。静的初期テーブルの `PML4[0]` は 2MiB ページ 512 本で
/// 低位 1GiB を覆う（`kernel/src/main.rs` の `zaytos_boot_pd_shared`）。
const IDENTITY_LIMIT: u64 = 1024 * 1024 * 1024;

/// AP 用スタックのフレームを予約する（S3-b-2b-1）。
///
/// # 呼ぶ位置
///
/// トランポリン用フレームの予約の直後。フレームアロケータが最小のフレーム
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

/// AP が最初に入る Rust の関数（S3-b-2b-1）。戻らない。
///
/// # ここで触れるものは限られている
///
/// CR3 は静的初期テーブルで、`PML4[256]`（direct map）が無い。したがって
/// direct map 越しに触るものは一切使えない。使えるのは
/// ポート I/O（シリアル）と、高位 VA の静的データである。
///
/// `cpu_id()` を呼ばない。`sgdt` 由来の実装は自コアの GDT がロードされた後
/// でなければ正しくないが、この段の AP は per-CPU GDT を持たない
/// （`kernel/src/gdt/mod.rs` の載荷条件）。身元は引数で受け取る。
///
/// ロックを取らない。`Logger` と `SerialPort` にロックは無いので、
/// BSP が 1 つずつ起こすことで混線を避けている（同時に書くとバイトが混ざる）。
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

    // === S3-b-2b-2: 本番の世界へ移る ===
    //
    // ここまでが b-2b-1 の範囲である（恒等 VA の 1 枚のスタック、共有の一時
    // GDT、IDT 無し）。引き継ぎ表があれば、自分の per-CPU 資産を載せて本番 CR3 へ移る。
    if let Some(info) = load_bringup(index as usize) {
        // SAFETY: トランポリンで入った直後で、割り込みは禁止のままである。
        // このコアにつき 1 回だけ呼ぶ。
        unsafe { bring_up_application_processor(info) }
    }

    let _ = writeln!(
        serial,
        "[WARN] smp: ap {index} has no bring-up information, so it stays on the static boot \
         page table and halts here"
    );
    // 割り込みは有効化しない。IDT を持たないので、来ても行き先が無い。
    cpu::halt_forever()
}

/// 起動署名を出した AP の本数。BSP が会計に使う。
static AP_STARTED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 起こした AP の APIC ID（S5-a）。スロット 1 以降ぶん。`u16` の番兵で
/// 「未設定」を表す（APIC ID は `u8` なので `0` も有効な値である）。
static STARTED_AP_APIC_ID: [core::sync::atomic::AtomicU16; MAX_APS] =
    [const { core::sync::atomic::AtomicU16::new(NO_APIC_ID) }; MAX_APS];

/// 「まだ起こしていない」を表す番兵（S5-a）。
const NO_APIC_ID: u16 = u16::MAX;

/// 探り用ページの仮想アドレス（S5-c）。AP スタックの領域とは別の PML4 の穴に
/// 置く（`PML4[258]` の遥か上）。本番の写像と重ならない場所を選ぶ。
#[cfg(feature = "smp-tlb-shootdown-probe")]
const SHOOTDOWN_PROBE_VIRT: u64 = 0xffff_8180_0000_0000;

/// 探り用ページを 1 枚張る（S5-c）。BSP が起動時、アロケータのある場所で呼ぶ。
///
/// 定常ループにはアロケータが無いので、張るのはここでしかできない。
/// 外すのは定常ループ側である（`unmap_4kib` はアロケータを要らない）。
///
/// # Safety
///
/// 本番テーブルへ切り替え済みで、direct map 窓が使えること。
#[cfg(feature = "smp-tlb-shootdown-probe")]
pub unsafe fn prepare_shootdown_probe<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut FrameAllocator<CAP>,
) {
    // SAFETY: 呼び出し側の契約。
    let mut table = unsafe { ActivePageTable::current(common::addr::direct_map()) };
    let Some(frame) = allocator.allocate_frame() else {
        logger.error(format_args!("smp: no frame for the shootdown probe page"));
        return;
    };
    let Some(virt) = VirtAddr::new(SHOOTDOWN_PROBE_VIRT) else {
        logger.error(format_args!("smp: the shootdown probe VA is not canonical"));
        return;
    };
    let attributes = PageAttributes {
        user: false,
        writable: true,
        cacheable: true,
    };
    // SAFETY: 稼働中のテーブルへ、まだ誰も使っていない VA を張る。
    if let Err(error) = unsafe { table.map_4kib(virt, frame, attributes, allocator) } {
        logger.error(format_args!(
            "smp: could not map the shootdown probe page: {error:?}"
        ));
        return;
    }
    shootdown_probe::set_virt(SHOOTDOWN_PROBE_VIRT);
    logger.info(format_args!(
        "smp: mapped the shootdown probe page at {SHOOTDOWN_PROBE_VIRT:#x} -> {:#x}",
        frame.as_u64()
    ));
}

/// TLB シュートダウンの実証で使う探り用ページ（S5-c）。
///
/// # なぜ 4 段の手順が要るか
///
/// 「AP が触って #PF になる」だけでは差が出ない。AP の TLB にその翻訳が
/// 載っていなければ、シュートダウンを送らない構成でもページテーブルを歩いて
/// #PF になる。両構成が同じ結果になり、比較が消える。
///
/// 手順は次の 4 段である。
///
/// 1. AP がそのアドレスを触る（翻訳を TLB へ載せる）
/// 2. 触れたことを確かめる（載せられなかったら以降の比較は無意味である）
/// 3. bootstrap processor が BKL を保持したまま写像を外し、世代を上げる
///    （破壊構成では世代を上げない）
/// 4. AP がもう一度触る——世代が上がっていれば次の取得でフラッシュ済みなので
///    #PF、上がっていなければ古い翻訳で成功する
///
/// どちらの側にも「触ったことの積極的な証拠」が要る。「落ちなかった」は
/// 「触っていない」でも満たされる（S5-a で「0 と 0 が一致する」を踏んだのと
/// 同じ形である）。そのため触った回数を数える。
#[cfg(feature = "smp-tlb-shootdown-probe")]
pub mod shootdown_probe {
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    /// 何もしない。
    pub const IDLE: u32 = 0;
    /// 触れ（1 回目。翻訳を TLB へ載せる）。
    pub const TOUCH_FIRST: u32 = 1;
    /// 触れ（2 回目。写像を外した後）。
    pub const TOUCH_AGAIN: u32 = 2;

    static COMMAND: AtomicU32 = AtomicU32::new(IDLE);
    static SERVED: AtomicU32 = AtomicU32::new(IDLE);
    static TOUCHES: AtomicU64 = AtomicU64::new(0);
    static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static PROBE_VIRT: AtomicU64 = AtomicU64::new(0);

    /// 探り用ページの仮想アドレスを覚える（BSP が起動時に呼ぶ）。
    pub fn set_virt(virt: u64) {
        PROBE_VIRT.store(virt, Ordering::SeqCst);
    }

    /// 探り用ページの仮想アドレス。未設定なら 0。
    pub fn virt() -> u64 {
        PROBE_VIRT.load(Ordering::SeqCst)
    }

    /// AP へ指示を出す。
    pub fn command(next: u32) {
        COMMAND.store(next, Ordering::SeqCst);
    }

    /// AP が触った回数。これが「触れたことの積極的な証拠」である。
    pub fn touches() -> u64 {
        TOUCHES.load(Ordering::SeqCst)
    }

    /// AP が触ろうとした回数。アクセスの直前に増える。
    ///
    /// # なぜ「触れた回数」だけでは足りないか
    ///
    /// 「2 回目で数が増えなかった」は「触ろうとして触れなかった」と
    /// 「そもそも 2 回目を試みなかった」の両方で成り立つ。AP が段 4 へ
    /// 到達する前に別の理由で死んでいても、触れた回数は 1 のままである。
    /// 試みた側にも積極的な証拠が要る。
    pub fn attempts() -> u64 {
        ATTEMPTS.load(Ordering::SeqCst)
    }

    /// AP 側。指示があれば触って数える。戻り値は触ったかどうか。
    ///
    /// # Safety
    ///
    /// `PROBE_VIRT` が写像済みであること（外された後に呼ぶと #PF になる。
    /// それがこの探りの目的である）。
    pub unsafe fn service() {
        let cmd = COMMAND.load(Ordering::SeqCst);
        if cmd == IDLE || SERVED.load(Ordering::SeqCst) == cmd {
            return;
        }
        let virt = PROBE_VIRT.load(Ordering::SeqCst);
        if virt == 0 {
            return;
        }
        // 触る「前」に試行を数える。ここで #PF になると以降は実行されないので、
        // 試行と成功の差が「触ろうとして触れなかった」の証拠になる。
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: 呼び出し側の契約。読み取りのみ。外された後はここで #PF になる。
        let _ = unsafe { core::ptr::read_volatile(virt as *const u64) };
        TOUCHES.fetch_add(1, Ordering::SeqCst);
        SERVED.store(cmd, Ordering::SeqCst);
    }
}

/// 起こした AP の APIC ID を返す（S5-a）。起こしていなければ `None`。
pub fn started_ap_apic_id(slot: usize) -> Option<u8> {
    let raw = STARTED_AP_APIC_ID
        .get(slot.checked_sub(1)?)?
        .load(Ordering::SeqCst);
    (raw != NO_APIC_ID).then_some(raw as u8)
}

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
    /// `MAX_CPUS` を超えるので起こさなかった AP の本数。
    pub skipped_no_slot: usize,
}

/// AP を起こす（S3-b-2b-1）。1 本ずつ起こし、次へ進む前に完了を待つ。
///
/// # なぜ 1 本ずつなのか
///
/// `Logger` と `SerialPort` にロックが無いので、2 つ以上の AP が同時に書くと
/// バイトが混ざる。そしてどの AP が失敗したかを切り分けられなくなる。
/// 直列にする費用はコア数 × 10ms 程度で、実害が無い。
///
/// # 起こす本数は `MAX_CPUS` で制限する
///
/// `MAX_CPUS` を超えるコアは起こさない（`roadmap.md` の S3-b-2b-1）。
/// 超えた分を起こすと、per-CPU スロットを持てない AP が生まれる。
/// 起こさなかった本数を返して、検査がそれを主張できるようにする。
///
/// # 待ち時間
///
/// INIT の後に 10ms、SIPI の後に 10ms 待つ。規格は SIPI の後 200µs だが、
/// タイマのティックが 10ms 粒度なのでそれで代用する（下限より長いだけなので
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

    // トランポリンを予約フレームへコピーし、絶対値を書き込む。
    // SAFETY: frame は S1 が予約した 4KiB 境界の物理フレームで、他の誰も使わない。
    // BSP は本番 CR3 で走るので direct map 越しに触る（AP は恒等で触る）。
    let installed = unsafe { install_trampoline(logger, frame) };

    // この値は BSP の ID とは限らない。MADT の最初の使用可能な Local APIC
    // エントリであって、エントリ順が BSP を先頭にする保証は仕様に無い
    // （[`crate::acpi::ApicMmio::bsp_candidate_apic_id`] の doc）。BSP が先頭で
    // ない実装では、下の `continue` が BSP を素通りさせ、BSP 自身へ INIT-SIPI を
    // 送ることになる。
    //
    // 権威のある出所は 2 つあり、どちらも既に読んでいる——`IA32_APIC_BASE` の
    // bit 8（`common::cpu::ApicBase::bootstrap_processor`）と、自コアの Local APIC
    // ID レジスタ（[`crate::apic`] が読んでいる）である。どちらも今はログへ出す
    // だけで、判定には使っていない。
    //
    // 直さない判断と解禁条件は `docs/deferred-decisions.md` にある。要点は、
    // QEMU で MADT の並びを変える手段が無く、破壊確認を構成できないことである。
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

        // スタック頂点は恒等 VA である。静的初期テーブルに direct map が
        // 無いので、direct map の VA を渡すと最初の push で落ちる。
        let stack_top_identity = stack.as_u64() + FRAME_SIZE;
        // SAFETY: installed はコピー済みのトランポリンで、data ブロックの位置は
        // レイアウト定数で決まっている。AP はまだ走っていない。
        unsafe { installed.set_ap_parameters(stack_top_identity, slot as u64) };

        report.attempted += 1;
        STARTED_AP_APIC_ID[slot - 1].store(u16::from(apic_id), Ordering::SeqCst);
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

        // INIT → 待つ → SIPI → 待つ → まだ起きていなければもう 1 回 SIPI。
        //
        // 2 回目を無条件に送ってはならない。既に走り出した AP へ SIPI を
        // 送ると、long mode で走っている最中に開始ベクタから再実行させる
        // ことになり、16 ビットのバイト列を 64 ビットとして解釈して #GP →
        // トリプルフォルトする。実際に踏んだ（CPU 1 が CS64・GDTR=0 で
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
                // SAFETY: 同上。まだ起きていないときだけ送る。
                unsafe { crate::apic::send_startup_ipi(lapic_virt, apic_id, installed.sipi_vector) }
            });
        if !ok {
            logger.error(format_args!(
                "smp: an IPI to apic id {apic_id} never left the local APIC (delivery status \
                 stayed set); halting"
            ));
            cpu::halt_forever();
        }

        // 起動署名を待つ。上限つきで待つ（CLAUDE.md の「シェルコマンドの制約」）。
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
/// 上限のない待ちにならない。ティックが止まっていれば進まないが、
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
    /// スタック頂点は恒等 VA で渡すこと（AP の CR3 に direct map が無い）。
    ///
    /// # Safety
    ///
    /// 対象の AP がまだ走っていないこと。走っている AP のデータブロックを
    /// 書き換えてはならない。
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
    // 定数で持たず、シンボルからの相対で求める。
    let farjmp_offset = core::ptr::addr_of!(zaytos_ap_tramp_farjmp) as u64 - src;
    let end = core::ptr::addr_of!(zaytos_ap_tramp_end) as u64;
    let len = end - src;

    // 1 ページに収まることを確かめる。収まらない場合の隣接ページの確保は
    // S1 から送った申し送りで、この段で致命として扱う。
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
            zaytos_ap_entry as *const () as u64,
        );
    }

    // 破壊 (S3-b-2b-1, smp-tramp-corrupt-copy): 設置済みのコピーを 1 バイト壊す。
    // パッチされる 3 領域の外を狙うので、雛形との比較が捕まえるはずである。
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
/// この段の実装では、16 ビット / 64 ビットの符号化の取り違えを 4 件踏んだ
/// （`.org` の詰め物が `mov cr0` の直後に入る、`lgdtw` になる、整列を仮定した
/// 書き込み、64 ビットの `[disp32]` が RIP 相対になる）。いずれも AP 側でしか
/// 落ちず、BSP 側は正常に見える。コードを触ったときに静かに戻るのを、
/// 設置後のバイト比較で捕まえる。
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

// ===========================================================================
// S3-b-2b-2: AP の per-CPU スタックを PML4[258] へ張る
// ===========================================================================

/// AP の per-CPU スタックを置く仮想アドレス空間の先頭（`PML4[258]`）。
///
/// # なぜ `PML4[257]` ではないのか
///
/// `[257..510]` は SMP の per-CPU 用に温存してきた範囲で、ここがその目的どおりの
/// 初使用である。しかし `PML4[257]`（`0xffff808000000000`）は使えない。
/// あれは破壊 feature `highhalf-remove-verify-fail` のサボタージュ VA そのもので、
/// あの破壊は「そこが空であること」に依存している。使うと破壊が静かに意味を失う
/// （`docs/verification-coverage.md` と `docs/deferred-decisions.md` の 2 箇所に
/// 警告がある）。サボタージュ VA を移さずに済むほうを選んだ。
///
/// # なぜ静的配列にしないのか
///
/// `StackBlock` は 108.0 KiB で、静的に二重化すると `MAX_CPUS = 4` で 2MiB 境界を
/// 越える（`common/src/percpu.rs` の `MAX_CPUS` の doc）。フレームアロケータから
/// 取って写像すればイメージが増えない。
const AP_STACK_REGION_BASE: u64 = 0xffff_8100_0000_0000;

/// 1 コアぶんのスタック領域の大きさ。BSP の `StackBlock` と同じ構成にする。
///
/// ガード（4KiB）+ kernel（64KiB）+ ガード + IST1（16KiB）+ ガード + IST2（16KiB）。
/// ガードは各スタックの下に置く（スタックは下へ伸びるので、溢れると下のガードに
/// 当たる）。BSP の `StackBlock` と同じ並びである。
const AP_STACK_STRIDE: u64 = (crate::stack::GUARD_SIZE
    + crate::stack::KERNEL_STACK_SIZE
    + crate::stack::GUARD_SIZE
    + crate::stack::IST_STACK_SIZE
    + crate::stack::GUARD_SIZE
    + crate::stack::IST_STACK_SIZE) as u64;

/// AP 1 本ぶんのスタックの所在（S3-b-2b-2）。
///
/// 仮想アドレスは本番テーブルにしか存在しない。AP は本番 CR3 へ移った後に
/// しか使えない（それより前は b-2b-1 の恒等 VA の 1 枚で走る）。
#[derive(Clone, Copy)]
pub struct ApStacks {
    /// 通常スタックの頂点。
    pub kernel_top: u64,
    /// IST1（ダブルフォルト）の頂点。
    pub double_fault_top: u64,
    /// IST2（ページフォルト）の頂点。
    pub page_fault_top: u64,
}

/// AP 用スタックを写像する（S3-b-2b-2）。
///
/// # ガードページは張らずに「開けておく」
///
/// 3 本のスタックの下に 1 ページずつ、写像しない穴を残す。BSP 側は静的配置の
/// 上で `unmap_4kib` して穴を開けているが、こちらは最初から張らないので
/// 分割も解除も要らない。direct map（2MiB ページ）に手を入れずに済むのが、
/// この置き方を選んだ理由の 1 つである。
///
/// # Safety
///
/// 起動時の単一文脈から、AP を起こす前に呼ぶこと。
pub unsafe fn map_ap_stacks<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    slot: usize,
    allocator: &mut FrameAllocator<CAP>,
) -> Option<ApStacks> {
    let base = AP_STACK_REGION_BASE + (slot as u64) * AP_STACK_STRIDE;
    // SAFETY: CR3 は本番テーブルを指しており、その配下は direct map 窓から
    // 読み書きできる。起動時の単一文脈で、AP はまだ走っていない。
    let mut table = unsafe { ActivePageTable::current(common::addr::direct_map()) };

    // (ガードのページ数, 本体のバイト数) を下から順に。
    let layout = [
        crate::stack::KERNEL_STACK_SIZE as u64,
        crate::stack::IST_STACK_SIZE as u64,
        crate::stack::IST_STACK_SIZE as u64,
    ];

    let mut cursor = base;
    let mut tops = [0u64; 3];
    let free_before = allocator.free_frame_count();
    for (index, size) in layout.iter().enumerate() {
        // ガードぶんを空けたまま進める（張らないので穴になる）。
        cursor += crate::stack::GUARD_SIZE as u64;
        let bottom = cursor;
        let mut offset = 0;
        while offset < *size {
            let Some(frame) = allocator.allocate_frame() else {
                logger.error(format_args!(
                    "smp: ran out of frames while mapping the per-CPU stacks for slot {slot}"
                ));
                return None;
            };
            let virt = VirtAddr::new(bottom + offset)?;
            // user=false, writable=true, cacheable=true（通常のカーネルメモリ）。
            let attributes = PageAttributes {
                user: false,
                writable: true,
                cacheable: true,
            };
            // SAFETY: 稼働中のテーブルへ、まだ誰も使っていない VA を張る。
            if let Err(error) = unsafe { table.map_4kib(virt, frame, attributes, allocator) } {
                logger.error(format_args!(
                    "smp: could not map the per-CPU stack page at {:#x} for slot {slot}: \
                     {error:?}",
                    virt.as_u64()
                ));
                return None;
            }
            offset += crate::frame_allocator::FRAME_SIZE;
        }
        cursor = bottom + *size;
        tops[index] = cursor;
    }
    let free_after = allocator.free_frame_count();

    logger.info(format_args!(
        "smp: mapped per-CPU stacks for slot {slot} at {base:#x} (PML4[258], not [257] which a \
         sabotage VA depends on): kernel top {:#x}, IST1 top {:#x}, IST2 top {:#x}; \
         {} frame(s) consumed (pages plus page tables), guards left unmapped",
        tops[0],
        tops[1],
        tops[2],
        free_before - free_after
    ));

    Some(ApStacks {
        kernel_top: tops[0],
        double_fault_top: tops[1],
        page_fault_top: tops[2],
    })
}

/// AP が本番の世界へ移るときに BSP から受け取るもの（S3-b-2b-2）。
///
/// 恒等 VA と本番 VA が混在する。どちらの空間の値かを名前で区別する
/// （取り違えると BSP 側では正常に見え、AP 側でだけ落ちる）。
#[derive(Clone, Copy)]
struct ApBringUp {
    /// 本番テーブルの物理（`mov cr3` に載せる）。
    production_cr3: u64,
    /// 本番テーブルにしか存在しない per-CPU スタック。
    stacks: ApStacks,
    /// このコアのスロット。
    slot: usize,
}

/// BSP が各スロットぶん用意する引き継ぎ表。AP が自分のスロットを読む。
static AP_BRINGUP: [AtomicU64; MAX_APS * 4] = [const { AtomicU64::new(0) }; MAX_APS * 4];

/// 引き継ぎ表へ書く（BSP 側）。
fn store_bringup(slot: usize, info: &ApBringUp) {
    let base = (slot - 1) * 4;
    AP_BRINGUP[base].store(info.production_cr3, Ordering::SeqCst);
    AP_BRINGUP[base + 1].store(info.stacks.kernel_top, Ordering::SeqCst);
    AP_BRINGUP[base + 2].store(info.stacks.double_fault_top, Ordering::SeqCst);
    AP_BRINGUP[base + 3].store(info.stacks.page_fault_top, Ordering::SeqCst);
}

/// AP（`slot`）の通常カーネルスタックの範囲を返す（S4-c-3-2a）。
///
/// `prepare_ap_per_cpu` の後でだけ意味を持つ。それ以前は引き継ぎ表が
/// 空なので `None` を返す。
///
/// 用途は `task::init_ap_idle_task` で、AP 用アイドルタスクが実際に走る
/// スタックを `Task` に記述するためである。IST は含めない——
/// `schedule_switch` の範囲検査が見るのは通常スタックだけである。
///
/// # IST を含めなくてよい根拠
///
/// タイマのベクタは IST を使わない。`idt::init` が IST を割り当てるのは
/// ベクタ 8（#DF）と 14（#PF）だけで、他はすべて `None` である。
/// Ring 0 から Ring 0 への割り込みではスタックが切り替わらないので、
/// AP がタイマで入ったときの `rsp` は、この通常スタックの内側にある。
/// したがって `schedule_switch` が保存する値も範囲の内側に入る。
/// IST を使うベクタが増えたら、この根拠は失効する。
pub fn ap_kernel_stack_range(slot: usize) -> Option<(u64, u64)> {
    Some(kernel_stack_bounds_from_top(
        load_bringup(slot)?.stacks.kernel_top,
    ))
}

/// 通常カーネルスタックの頂点から `[下端, 頂点)` を導く（S4-c-3-2a）。
///
/// 純粋な算術として切り出してある。この範囲は
/// `schedule_switch` の範囲検査が使うが、AP 用アイドルタスクでは
/// 切り替えが起きないので実行時には照合されない（`task::init_ap_idle_task`）。
/// 実行時に照合されない記述なので、誤りを捕まえられるのはホストテストだけである。
const fn kernel_stack_bounds_from_top(kernel_top: u64) -> (u64, u64) {
    (
        kernel_top - crate::stack::KERNEL_STACK_SIZE as u64,
        kernel_top,
    )
}

/// 引き継ぎ表から読む（AP 側）。
fn load_bringup(slot: usize) -> Option<ApBringUp> {
    let base = (slot - 1) * 4;
    let cr3 = AP_BRINGUP.get(base)?.load(Ordering::SeqCst);
    if cr3 == 0 {
        return None;
    }
    Some(ApBringUp {
        production_cr3: cr3,
        stacks: ApStacks {
            kernel_top: AP_BRINGUP[base + 1].load(Ordering::SeqCst),
            double_fault_top: AP_BRINGUP[base + 2].load(Ordering::SeqCst),
            page_fault_top: AP_BRINGUP[base + 3].load(Ordering::SeqCst),
        },
        slot,
    })
}

/// AP を本番 CR3 と per-CPU スタックへ移す（S3-b-2b-2）。戻らない。
///
/// # 順序。`mov cr3` と `mov rsp` の間に 1 命令も挟まない
///
/// 本番テーブルには恒等（`PML4[0]`）が無いので、`mov cr3` の瞬間に今のスタック
/// （b-2b-1 の恒等 VA の 1 枚）が消える。その状態で push・呼び出し・割り込みが
/// 起きると落ちる。b-2b-1 で `.org` の詰め物が `mov cr0` の直後に入って落ちたのと
/// 同じ型である。
///
/// したがって切り替えは asm で連続して行い、新しい RSP を先にレジスタへ載せて
/// おく。割り込みは禁止のままである（AP はまだ `sti` しない）。
///
/// # GDT / TSS / IDTR を先に載せる
///
/// 高位 VA（`.bss`）にあり、静的初期テーブルでも本番テーブルでも見える
/// （どちらも `PML4[511]` を持つ）。切り替えの前に載せれば、`cpu_id()` が
/// 早く正しくなる。
///
/// # IDTR を載せてから CR3 を切り替えるまでの窓（受け入れて記録する）
///
/// IDTR を載せた後、CR3 を切り替えるまでの数命令の間、IST の VA（本番テーブルに
/// しか無い）はまだ見えない。そこで IST 経由の例外（`#DF` / `#PF`）が起きると
/// ハンドラのスタックへ飛べない。割り込みは禁止だが、例外は禁止できない。
///
/// 受け入れる。理由は 3 つである。
///
/// - この区間に例外を起こす操作を置いていない（`mov cr3` / `mov rsp` / `jmp` だけ）
/// - 順序を入れ替える案（IST なしの IDT を先に載せ、CR3 の後で差し替える）は
///   IDT を 2 回載せることになり、「IDT は 1 本を共有する」という単純さを壊す
/// - 窓は数命令で、b-2b-1 の教訓どおり間に何も置かない形にしてある
///
/// この区間に命令を足すときは、この判断を再評価すること。上の 1 つ目の理由は
/// 「今は mov が 3 つだけ」に依存している。足した瞬間に前提が崩れる。
///
/// # Safety
///
/// AP 自身から、b-2b-1 のトランポリンで入った直後に 1 回だけ呼ぶこと。
unsafe fn bring_up_application_processor(info: ApBringUp) -> ! {
    // 1. 自分の GDT / TSS を載せる。索引は引数で受け取ったものである
    //    （`cpu_id()` はまだ使えない。GDT が載って初めて正しくなる）。
    // SAFETY: slot は BSP が割り当てた 0..MAX_CPUS の値。IST の頂点は本番
    // テーブルの VA なので、CR3 を移した後にしか実際には触れないが、
    // TSS へ書くだけならここで問題ない。割り込みは禁止のままである。
    unsafe {
        crate::gdt::init_for_cpu(
            info.slot,
            info.stacks.double_fault_top,
            info.stacks.page_fault_top,
        );
    }

    // ここから `cpu_id()` が正しい。GDTR が自分のスロットを指している。
    let derived = common::percpu::cpu_id();

    // 2. IDT を載せる。BSP が作った静的な IDT を共有する（高位 VA）。
    // SAFETY: 同上。IST の番号は BSP と同じ割り当てである。
    unsafe {
        crate::idt::load_shared();
    }

    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[INFO] smp: ap {} loaded its own GDT/TSS/IDT; cpu_id() now reads {} from GDTR \
         (the index handed over in the trampoline data block was {}, match={})",
        info.slot,
        derived,
        info.slot,
        derived == info.slot
    );
    if derived != info.slot {
        let _ = writeln!(
            serial,
            "[ERROR] smp: ap {} derived cpu_id {} from GDTR but was handed {}; the two \
             independent sources disagree; halting",
            info.slot, derived, info.slot
        );
        cpu::halt_forever();
    }

    // 3. CR3 と RSP を隣接して切り替える。
    // SAFETY: `production_cr3` は BSP が動いている本番テーブルの物理で、
    // `kernel_top` はそのテーブルに存在する VA である。間に何も置かない。
    // `noreturn` なので戻り先は要らない。
    unsafe {
        core::arch::asm!(
            "mov cr3, {cr3}",
            "mov rsp, {rsp}",
            "jmp {entry}",
            cr3 = in(reg) info.production_cr3,
            rsp = in(reg) info.stacks.kernel_top,
            entry = sym ap_after_switch,
            in("rdi") info.slot,
            options(noreturn),
        )
    }
}

/// 本番 CR3 と per-CPU スタックへ移った後の AP（S3-b-2b-2）。戻らない。
extern "C" fn ap_after_switch(slot: usize) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();

    // 恒等が無いことを AP 側で読み戻す（b-2b-1 から移した到達条件）。
    // SAFETY: 稼働中のテーブルを読むだけ。
    let cr3_phys = crate::paging::switch::read_cr3();
    let cr3 = cr3_phys.as_u64();
    // SAFETY: 稼働中のテーブルを direct map 越しに読むだけ（本番テーブルには
    // direct map がある）。読み取りのみ。
    let pml4_0 =
        unsafe { crate::paging::verify::read_pml4_entry(cr3_phys, common::addr::direct_map(), 0) };
    let identity_gone = pml4_0 & 1 == 0;

    let _ = writeln!(
        serial,
        "[INFO] smp: ap {slot} switched to the production page table (cr3={cr3:#x}) and its own \
         per-CPU stack; PML4[0] read back from this core = empty:{identity_gone} (the identity \
         mapping is gone here too, so the trampoline's identity VA stack is no longer usable)"
    );
    if !identity_gone {
        let _ = writeln!(
            serial,
            "[ERROR] smp: ap {slot} still sees an identity mapping in the production table; \
             halting"
        );
        cpu::halt_forever();
    }

    // 破壊 (S3-b-2b-2, smp-ap-touch-scheduler): AP からスケジューラの現在タスクを
    // 読む。この段は AP でタスクを実行しないので、sentinel を読んで落ちるのが
    // 正しい。丸めていたら「タスク 0 が走っている」と静かに答えていた。
    #[cfg(feature = "smp-ap-touch-scheduler-test")]
    {
        let _ = writeln!(
            serial,
            "[INFO] smp: ap {slot} is about to read the scheduler's current task (sabotage)"
        );
        crate::task::debug_read_current_index();
    }

    AP_BROUGHT_UP.fetch_add(1, Ordering::SeqCst);

    // === S4-c-3-2b: このコアの `CURRENT` を sentinel から解く ===
    //
    // `start_local_timer` より前でなければならない。あちらは戻らず、その先で
    // `sti` する。そこを過ぎると、このコアはいつでもタイマを受ける。sentinel
    // のまま受けると `current_index()` が sentinel を読んで停止する。
    //
    // BKL をここで取る。`CURRENT` は共有物で、書く時点で bootstrap processor
    // が走っている。これが `KernelEntry::ApBringUp` の唯一の取得箇所であり、
    // 列挙にあって取得箇所が無い状態がここで解消する。
    //
    // 取る区間は書き込みだけに絞る。この関数はシリアルを直に使っており、
    // そこは BKL の外のままである（S6 のログ規約の許可リストの対象であって、
    // この段の対象ではない）。
    //
    // 破壊 (S4-c-3-2b, smp-ap-no-sentinel-clear): この解除を落とす。AP は最初の
    // ティックで sentinel を読んで停止する。S4-a の `smp-ap-enter-scheduler`
    // から役目を引き継いだ破壊である。
    #[cfg(not(feature = "smp-ap-no-sentinel-clear"))]
    {
        let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::ApBringUp);
        crate::task::adopt_idle_task_on_this_cpu();
    }

    let _ = writeln!(
        serial,
        "[INFO] smp: ap {slot} is up with its own per-CPU state (own GDT/TSS/IDT, own stacks \
         in PML4[258], production CR3); it now takes part in scheduling on its own idle task"
    );

    // 破壊 (S4-c-4-1, smp-ap-runs-preemptive-demo): AP にデモを呼ばせる。
    //
    // tripwire の機序の直接観測である。`require_bootstrap_processor` は
    // `run_preemptive_demo` の入口にあり、ワーカーの継続では鳴らない。開始で
    // だけ鳴る。呼び出しは 2 箇所しか無く（協調デモとプリエンプティブデモの
    // 開始）、本番経路では bootstrap processor しか通らないので、この tripwire
    // は一度も踏まれていない。踏ませて、実際に停止することを見る。
    //
    // 止まるのは入口である。`require_bootstrap_processor` が
    // `halt_forever` するので、`setup_preemptive_tasks` へは到達しない。
    // したがってワーカーは `Ready` にならず、二重選択の窓も生まれない。
    // 窓が要るのは S4-c-4-2 で、あちらはこの tripwire を外した構成である
    // （`docs/verification-coverage.md`。同じ起動では両立しない——
    // 一方は tripwire が在ることを、他方は無いことを要求する）。
    //
    // タイマを開ける前に置く。`start_local_timer` は戻らない。
    #[cfg(feature = "smp-ap-runs-preemptive-demo")]
    {
        let _ = writeln!(
            serial,
            "[INFO] smp: ap {slot} is about to call the preemptive demo (sabotage); the \
             bootstrap-processor tripwire at its entry must stop this core"
        );
        crate::task::run_preemptive_demo();
        let _ = writeln!(
            serial,
            "[ERROR] smp: ap {slot} returned from the preemptive demo; the tripwire did not \
             fire; halting"
        );
        cpu::halt_forever();
    }

    // === S4-a: 自分の Local APIC とタイマを開ける ===
    // SAFETY: 自コアの単一文脈で、割り込みはまだ禁止されている。
    unsafe { start_local_timer(&mut serial, slot) }
}

/// AP が自分の Local APIC タイマを開けて定常ループへ入る（S4-a）。戻らない。
///
/// # SVR は BSP の設定を引き継がない
///
/// `apic::set_spurious_vector` は BSP の Local APIC にしか効いていない。
/// SVR はコアごとにあるので、AP は自分で書く。bit 8（ソフトウェア有効化）が
/// 落ちていると LVT が 1 本も届かないので、書いた後に読み戻して確かめる。
///
/// # 較正はやり直さない。それは仮定である
///
/// BSP が測った分周と初期カウントをそのまま自分の LVT へ書く。これは
/// 「Local APIC タイマの周波数がコア間で同じ」という仮定である。
/// 仮定なので、AP 側のティックのレートをホストの実時間と突き合わせて実測検証
/// する（`lapic-timer-test` と同型の独立基準）。仮定が崩れる環境ではそこで捕まる。
///
/// # Safety
///
/// 自コアの GDT / TSS / IDT が載っており、本番 CR3 と per-CPU スタックへ
/// 移った後であること。割り込みが禁止されていること。各コアにつき 1 回だけ。
unsafe fn start_local_timer(serial: &mut SerialPort, slot: usize) -> ! {
    // 1. 自分の Local APIC を有効にする。
    //
    // 破壊 (S4-a, smp-ap-timer-no-svr): ここを飛ばす。BSP が書いた SVR は
    // このコアには効いていないので、ティックが 1 本も来ない。
    #[cfg(not(feature = "smp-ap-timer-no-svr-test"))]
    {
        // SAFETY: 自コアの単一文脈で、割り込みは禁止されている。
        match unsafe { crate::irq::enable_local_apic_for_this_cpu() } {
            Some(enable) => {
                let _ = writeln!(
                    serial,
                    "[INFO] smp: ap {slot} wrote its own SVR: spurious vector {:#04x}, \
                     software_enabled={} (the BSP's write only reached the BSP's local APIC)",
                    enable.spurious_vector(),
                    enable.software_enabled()
                );
                if !enable.software_enabled() {
                    let _ = writeln!(
                        serial,
                        "[ERROR] smp: ap {slot} has its local APIC software-disabled, so no LVT \
                         interrupt can be delivered; halting"
                    );
                    cpu::halt_forever();
                }
            }
            None => {
                let _ = writeln!(
                    serial,
                    "[ERROR] smp: ap {slot} could not reach its local APIC to write the SVR; \
                     halting"
                );
                cpu::halt_forever();
            }
        }
    }
    #[cfg(feature = "smp-ap-timer-no-svr-test")]
    let _ = writeln!(
        serial,
        "[WARN] smp: ap {slot} is skipping its own SVR write (sabotage)"
    );

    // 2. 自分の LVT Timer を、BSP と同じ設定で開ける。
    // SAFETY: 自コアの IDT は載っており、LAPIC_TIMER_VECTOR には戻れるハンドラが
    // ある。割り込みはまだ禁止されているので、`sti` するまでは届かない。
    match unsafe { crate::irq::arm_lapic_timer_for_this_cpu() } {
        Some((divide, initial_count)) => {
            let _ = writeln!(
                serial,
                "[INFO] smp: ap {slot} armed its own LAPIC timer with the BSP's calibration \
                 (divide configuration {divide:#x}, initial count {initial_count}); sharing the \
                 calibration ASSUMES the LAPIC timer frequency is the same on every core, and \
                 that assumption is checked against host wall-clock time, not from inside"
            );
        }
        None => {
            let _ = writeln!(
                serial,
                "[ERROR] smp: ap {slot} could not arm its LAPIC timer (the BSP has not moved the \
                 timer to the local APIC yet); halting"
            );
            cpu::halt_forever();
        }
    }

    // 3. 割り込みを有効にして定常ループへ入る。
    //
    // S3 ではここが `cli; hlt` だった。BKL が無いので AP は待つだけで、
    // 割り込みを有効化しなかった。S4-a で前提が変わる。
    //
    // BKL はまだ無い。この段の安全は「AP のハンドラが触るものが per-CPU か
    // アトミックだけである」ことに依存する条件つきのものである（`roadmap.md` の
    // S4-a に一覧がある）。S4-b で BKL が入れば、この一覧は不要になる。
    // 破壊 (S4-b-4, bkl-hold-forever): AP が BKL を取ったまま二度と離さない。
    // BSP がタイムアウトして原因を出す。再帰検出ではなく待ちの上限を通す
    // 唯一の形である（既存の 2 破壊はどちらも同じコアが取り直すので再帰が先に鳴る）。
    #[cfg(feature = "bkl-hold-forever-test")]
    crate::bkl::sabotage_hold_forever();

    #[cfg(not(feature = "bkl-hold-forever-test"))]
    ap_heartbeat_loop(serial, slot)
}

/// AP の定常ループ（S4-a）。戻らない。
///
/// # `sti; hlt` の隣接
///
/// BSP の `run_timer_loop` と同じく `cpu::enable_interrupts_and_halt` を使う。
/// このループは眠るかどうかを条件で決めないので、条件確認と `hlt` の間で
/// 仕事を取りこぼす形にならない（あちらの doc と同じ理由である）。
///
/// # ログの規約
///
/// BKL の外からシリアルへ書く。シリアルにもロガーにもロックが無いので、
/// BSP の出力と混線しうる。行頭にコア番号を必ず置くことで、混ざっても
/// どのコアの行かが分かるようにしてある。行の途中で混ざることは防げない。
#[cfg_attr(feature = "bkl-hold-forever-test", allow(dead_code))]
fn ap_heartbeat_loop(serial: &mut SerialPort, slot: usize) -> ! {
    let mut next_heartbeat = crate::interrupts::HEARTBEAT_TICKS;
    loop {
        let ticks = crate::idt::timer_ticks_for(slot);
        // **観測が締まっていれば出さない（S11-11）。** BSP がシェルへ渡した後も
        // 出し続けると、**起動ログの長さが実時間に依存する。**
        if ticks >= next_heartbeat && !crate::interrupts::steady_observation_is_closed() {
            next_heartbeat = ticks + crate::interrupts::HEARTBEAT_TICKS;
            let _ = writeln!(
                serial,
                "[INFO] smp: ap heartbeat: cpu={slot} ticks={ticks} tsc={}",
                cpu::read_timestamp_counter()
            );
        }
        // TLB シュートダウンの探り（S5-c）。指示があるときだけ触る。
        // SAFETY: 探り用ページは BSP が起動時に写像している。外された後に触ると
        // #PF になるが、それがこの探りの目的である。
        #[cfg(feature = "smp-tlb-shootdown-probe")]
        unsafe {
            shootdown_probe::service()
        };

        // SAFETY: 自コアの IDT は載っており、タイマのハンドラは EOI を送って戻る。
        // `sti; hlt` が隣接しているので、有効化と停止の間に窓が開かない。
        unsafe {
            cpu::enable_interrupts_and_halt();
        }
    }
}

/// 本番の世界へ移った AP の本数。
static AP_BROUGHT_UP: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 本番の世界へ移った AP の本数。
pub fn brought_up_ap_count() -> usize {
    AP_BROUGHT_UP.load(Ordering::SeqCst)
}

/// AP の per-CPU 資産を用意する（S3-b-2b-2）。BSP が起動最初期に呼ぶ。
///
/// # なぜここで用意するのか
///
/// フレームアロケータと本番テーブルの両方が要る。AP を起こすのは
/// `run_timer_loop` の中だが、そこにはアロケータが無い（トランポリン用フレームと
/// AP スタック用フレームを最初期に予約したのと同じ理由）。
///
/// # Safety
///
/// 起動時の単一文脈から、本番テーブルへ切り替えた後・AP を起こす前に 1 回だけ呼ぶこと。
pub unsafe fn prepare_ap_per_cpu<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut FrameAllocator<CAP>,
) {
    let production_cr3 = crate::paging::switch::read_cr3().as_u64();
    for slot in 1..common::percpu::MAX_CPUS {
        // SAFETY: 呼び出し元契約。まだ AP は走っていない。
        let Some(stacks) = (unsafe { map_ap_stacks(logger, slot, allocator) }) else {
            logger.error(format_args!(
                "smp: could not map the per-CPU stacks for slot {slot}; that AP will stay on \
                 the static boot page table"
            ));
            continue;
        };
        store_bringup(
            slot,
            &ApBringUp {
                production_cr3,
                stacks,
                slot,
            },
        );
    }
}
