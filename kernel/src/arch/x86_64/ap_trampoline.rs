//! AP のトランポリン（置く枠と恒等のスタックの予約・雛形・設置・照合）。
//!
//! **`kernel/src/smp.rs` から移した**（2026-09-28。境界の段階の手順 2）。**SIPI で起きた AP が、リアルモードから
//! 長モードへ入るまでの 16 ビットと 64 ビットのコードと、その設置と照合なので、CPU 固有の置き場に置く。**
//! **どの AP をいつ起こすかと、長モードへ入った AP が跳ぶ Rust の入口は `smp` に残る**（入口は呼ぶ側が渡す）。
//! **置く枠（1 MiB 未満）と、AP が最初に使う恒等のスタック（低位 1 GiB）の予約も、同じ日に移した。** どちらも
//! SIPI と起動の表の都合で決まる。

use core::sync::atomic::{AtomicU64, Ordering};

use common::addr::PhysAddr;
use common::arch::x86_64::cpu;
use common::log::Logger;
use common::machine::pc::serial::Serial;

use crate::frame_allocator::{FrameAllocator, FRAME_SIZE};

/// AP のトランポリンを置ける物理アドレスの上限（この値未満）。
///
/// # なぜ 1MiB 未満なのか
///
/// AP は SIPI（Startup IPI）で起動する。SIPI が運べるのは8 ビットのベクタだけで、
/// AP はリアルモードで `vector << 12` から実行を始める。したがって開始アドレスは
/// 物理 `0x00000`〜`0xFF000` に限られる。ZeikOS のカーネルイメージは物理
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
/// 収まらない場合の隣接ページの確保は、この段階では扱わない。S1 は 1 枚しか
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
/// S1 は情報を集める段階であり、AP はまだ起動しない。ここで停止すると、現在
/// 単一コアで動いているカーネルが「トランポリン用の 1 枚が取れない」だけで
/// 起動しなくなり、機能的な後退になる。失敗は大きく報告して継続する。
/// 致命として扱うのは S3（AP の起動）である。
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
/// 一般則をここでは意図的に使っている）。S3 で実際に読まれることを、その段階の
/// 到達条件にしてある（`roadmap.md`）。そうしないと、使い忘れても誰も気づかない。
///
/// # 契約（境界の関数。2026-09-28）
///
/// - 戻り値は**物理アドレス**である（1 MiB 未満、4 KiB 境界）。BSP は direct map を通して触り、AP は恒等で触る。
/// - いつ呼んでもよい。値は起動の最初期に [`reserve_trampoline_frame`] が 1 回だけ書き、その後は変わらない。
/// - 読むだけで、どの CPU の状態も変えない。ほかの CPU との同期は要らない（書くのは、AP がまだ居ない起動の
///   最初期だけである）。
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

/// AP 用スタックとして予約したフレーム（S3-b-2b-1）。`0` は未予約。
///
/// # なぜ起動最初期に予約するのか
///
/// AP を起動するのは `run_timer_loop` の中（`sti` より後）だが、そこには
/// フレームアロケータが無い。トランポリン用フレームと同じ理由で、
/// 取れる位置で取っておく。
///
/// # 恒等 VA で使う。だから低位でなければならない
///
/// AP の CR3 は静的初期テーブルで、そこには `PML4[256]`（direct map）が無い。
/// したがって AP はこのフレームを恒等 VA（物理 == 仮想）で触る。
/// 恒等が覆うのは低位 1GiB なので、予約したフレームがそこに入ることを確かめる。
static AP_STACK_FRAMES: [AtomicU64; MAX_APS] = [const { AtomicU64::new(NO_FRAME) }; MAX_APS];

/// 起動しうる AP の本数（bootstrap processor を除く）。`smp` の同じ名前の定数と同じく、`MAX_CPUS` から導く。
const MAX_APS: usize = common::percpu::MAX_CPUS - 1;

/// 恒等マッピングが覆う上限。静的初期テーブルの `PML4[0]` は 2MiB ページ 512 本で
/// 低位 1GiB を覆う（`kernel/src/main.rs` の `zeikos_boot_pd_shared`）。
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
///
/// # 契約（境界の関数。2026-09-28）
///
/// - `index` は AP のスロットの番号から 1 を引いたもの（スロット 1 の AP が 0）。範囲の外か、まだ予約していなければ
///   `None`。
/// - 戻り値は 1 フレーム（4 KiB）の**物理アドレス**で、低位 1 GiB の中にある。AP は起動の表の恒等（物理 == 仮想）で
///   このフレームをスタックに使うので、スタックの頂点には「物理アドレス + フレームの大きさ」をそのまま渡す。
/// - いつ呼んでもよい。値は起動の最初期に [`reserve_ap_stacks`] が 1 回だけ書き、その後は変わらない。
/// - 読むだけで、どの CPU の状態も変えない。ほかの CPU との同期は要らない。
pub fn ap_stack_frame(index: usize) -> Option<PhysAddr> {
    match AP_STACK_FRAMES.get(index)?.load(Ordering::Relaxed) {
        NO_FRAME => None,
        value => PhysAddr::new(value),
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
// CR3 に載せるのは `zeikos_boot_pml4`（静的初期テーブル）である。
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
    /// `zeikos_ap_tramp_farjmp` からの相対で求める。
    pub const FARJMP_OPCODE_LEN: u64 = 2;
}

/// 破壊テスト（`ap-entry-misalign-test`。2026-09-28）: Rust の入口へ `call` する前に RSP を 8 ずらす。入口の確かめ
/// （[`crate::arch::x86_64::check_entry_stack_alignment`]）が `zeikos_ap_entry` の名前つきで止めることを見る。
/// 既定は空の文字列で、トランポリンのバイト列は変わらない。**雛形そのものを変えるので、設置の照合（コピーと雛形の
/// 突き合わせ）は通る**——止めるのは入口の確かめである。
#[cfg(feature = "ap-entry-misalign-test")]
macro_rules! ap_entry_sabotage {
    () => {
        "  sub rsp, 8"
    };
}
#[cfg(not(feature = "ap-entry-misalign-test"))]
macro_rules! ap_entry_sabotage {
    () => {
        ""
    };
}

core::arch::global_asm!(
    ".section .rodata.aptramp,\"a\",@progbits",
    ".p2align 12",
    ".globl zeikos_ap_tramp_start",
    "zeikos_ap_tramp_start:",
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
    ".globl zeikos_ap_tramp_farjmp",
    "zeikos_ap_tramp_farjmp:",
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
    "  mov rsp, [rip + zeikos_ap_tramp_data_rsp]",
    // 自分の索引を第 1 引数へ。GDT に依存しない身元の出所である。
    "  mov rdi, [rip + zeikos_ap_tramp_data_index]",
    // 起動したことを BSP へ知らせる。BSP はこれをポーリングして次の AP へ進む。
    "  mov qword ptr [rip + zeikos_ap_tramp_data_started], 1",
    // 高位 VA の Rust の入口へ `call` で入る。戻らない。**`jmp` で入ると、入口の RSP が System V の決まり
    // （16 で割ると 8 余る）から 8 ずれる**（スタックの頂点は 4 KiB 境界。2026-09-28 に直した）。入口の先頭で
    // 確かめる（`check_entry_stack_alignment`）。戻ったら `ud2` で落とす。
    "  mov rax, [rip + zeikos_ap_tramp_data_entry]",
    ap_entry_sabotage!(), // 既定は空。feature のときだけ RSP を 8 ずらす
    "  call rax",
    "  ud2",
    // --- 一時 GDT と GDTR ---
    ".org 0xF00",
    "  .quad 0",                       // null
    "  .quad 0x00AF9A000000FFFF",      // 0x08: 64bit code, DPL0, L=1
    "  .quad 0x00CF92000000FFFF",      // 0x10: data
    ".org 0xF20",
    ".globl zeikos_ap_tramp_gdtr",
    "zeikos_ap_tramp_gdtr:",
    "  .word 23",                      // limit = 3*8 - 1
    "  .long 0",                       // base（BSP が書き込む）
    ".org 0xF40",
    // データブロック。64 ビット側は RIP 相対でここを指すので、
    // 各フィールドにラベルを置く。オフセット定数（BSP が書き込む側）と
    // 同じ位置を指していることが、レイアウトの唯一の接点である。
    "zeikos_ap_tramp_data_cr3:     .quad 0",
    "zeikos_ap_tramp_data_rsp:     .quad 0",
    "zeikos_ap_tramp_data_entry:   .quad 0",
    "zeikos_ap_tramp_data_index:   .quad 0",
    "zeikos_ap_tramp_data_started: .quad 0",
    ".org 0x1000",
    ".globl zeikos_ap_tramp_end",
    "zeikos_ap_tramp_end:",
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
    /// AP に渡すページテーブルの根（CR3 に載せる。静的初期テーブルの物理）。
    pub page_table_root: u64,
    /// AP が最初に実行する場所（トランポリンを置いたページ。SIPI に載せる。2026-09-29 の 9f で、SIPI のベクタの値から
    /// `machine` の型にした）。
    pub start: crate::machine::pc::StartAddress,
}

impl InstalledTrampoline {
    /// この AP に渡すスタック頂点と索引を書き込む。
    ///
    /// スタック頂点は恒等 VA で渡すこと（AP の CR3 に direct map が無い）。
    ///
    /// # 契約（境界の関数。2026-09-28）
    ///
    /// - `stack_top_identity` は恒等の仮想アドレス（物理 == 仮想。低位 1 GiB の中）で、4 KiB 境界にあること
    ///   （[`ap_stack_frame`] の値にフレームの大きさを足したもの）。`index` は AP のスロットの番号（1 から）で、
    ///   入口の第 1 引数になる。
    /// - 呼んでよいのは BSP だけで、その AP へ SIPI を送る前である。
    /// - 変えるのはメモリ（コピーのデータブロック）だけで、どの CPU の状態も変えない。AP がこの値を読むのは SIPI を
    ///   受けた後なので、ほかの CPU との同期は、呼ぶ側が SIPI を送る順番で成り立つ。
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
/// # 契約（境界の関数。2026-09-28）
///
/// - `frame` は予約済みのトランポリン用フレームの**物理アドレス**である（[`trampoline_frame`] の値）。
/// - 戻り値の [`InstalledTrampoline`] は、BSP から見たコピーの置き場（direct map の仮想アドレス）と、AP に渡す
///   起動の表の根（物理アドレス）と、SIPI のベクタを持つ。AP ごとの引数は
///   [`InstalledTrampoline::set_ap_parameters`] で書く。
/// - 呼んでよいのは BSP だけで、起動の途中に 1 回だけである。本番のページテーブルへ移った後（direct map が使える）で、
///   どの AP もまだトランポリンを走らせていないこと。割り込みの状態は問わない。BKL は要らない（AP がまだ走って
///   いないので、共有するものが無い）。
/// - 呼んだ後は、フレームに雛形がコピーされ、GDTR のベース・far jump の飛び先・起動の表の根・`entry` が書き込まれて
///   いて、雛形との照合が通っている（通らなければ停止する）。
/// - 変えるのはメモリ（予約フレーム）だけで、どの CPU の状態も変えない。AP がフレームを読むのは SIPI を受けた後な
///   ので、ほかの CPU との同期は、呼ぶ側が SIPI を送る順番で成り立つ。
///
/// # 入口（`entry`）の契約
///
/// - 呼び出し規約は `extern "C"`（System V）で、戻らない（`-> !`）。トランポリンの 64 ビットのコードから `call` で
///   入る（戻り先を積む。戻ったら `ud2` で落とす）。第 1 引数（`rdi`）は `set_ap_parameters` で書いた番号である。
/// - スタックは `set_ap_parameters` で渡した恒等のスタックの頂点である（1 フレーム、4 KiB 境界）。`call` が戻り先を
///   積むので、入口の RSP は System V の決まりどおり 16 で割ると 8 余る（2026-09-28 までは `jmp` で入っていて、
///   8 ずれていた）。入口の先頭で [`crate::arch::x86_64::check_entry_stack_alignment`] を呼ぶこと。
/// - 割り込みは止まっている（トランポリンの先頭の `cli`）。IDT は載っていないので、例外が起きても行き先が無い。
///   NMI は `cli` では止まらない。
/// - ページテーブルは起動の表（低位 1 GiB の恒等と、高位のカーネル。direct map は無い）で、自分の GDT も載っていない
///   （`cpu_id()` は使えない）。
/// - `entry` はカーネルのイメージの中の関数であること（起動の表の高位で見える）。値はコピーへ書き込まれ、AP が
///   入るまで使われる。関数なので、その間ずっと有効である。
///
/// # Safety
///
/// `frame` が予約済みで 4KiB 境界にあり、誰も使っていないこと。
pub unsafe fn install_trampoline(
    logger: &mut Logger<Serial>,
    frame: PhysAddr,
    entry: extern "C" fn(u64) -> !,
) -> InstalledTrampoline {
    extern "C" {
        static zeikos_ap_tramp_start: u8;
        static zeikos_ap_tramp_end: u8;
        static zeikos_ap_tramp_farjmp: u8;
    }

    let src = core::ptr::addr_of!(zeikos_ap_tramp_start) as u64;
    // far jump の位置は、`mov cr0` の直後に置く制約からコード長で決まる。
    // 定数で持たず、シンボルからの相対で求める。
    let farjmp_offset = core::ptr::addr_of!(zeikos_ap_tramp_farjmp) as u64 - src;
    let end = core::ptr::addr_of!(zeikos_ap_tramp_end) as u64;
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
        static zeikos_boot_pml4: u8;
    }
    let cr3 = crate::kernel_phys_from_virt(
        common::addr::VirtAddr::new(core::ptr::addr_of!(zeikos_boot_pml4) as u64)
            .expect("the static boot page table has a canonical address"),
    )
    .as_u64();

    let installed = InstalledTrampoline {
        direct_map_base,
        page_table_root: cr3,
        // SIPI のベクタ（ページの番号）にするのは `machine` である（4 KiB の境界で 1 MiB より下かも確かめる）。
        start: crate::machine::pc::StartAddress::of_page(frame),
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
    logger: &mut Logger<Serial>,
    installed: &InstalledTrampoline,
    src: u64,
    len: u64,
    farjmp_offset: u64,
) -> bool {
    extern "C" {
        static zeikos_ap_tramp_gdtr: u8;
        static zeikos_ap_tramp_data_cr3: u8;
        static zeikos_ap_tramp_data_started: u8;
    }
    let gdtr_off = core::ptr::addr_of!(zeikos_ap_tramp_gdtr) as u64 - src;
    let data_off = core::ptr::addr_of!(zeikos_ap_tramp_data_cr3) as u64 - src;
    let data_end = core::ptr::addr_of!(zeikos_ap_tramp_data_started) as u64 - src + 8;

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
