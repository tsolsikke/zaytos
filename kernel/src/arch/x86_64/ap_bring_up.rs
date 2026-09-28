//! AP 自身が本番の世界へ移る所（BSP の CR0・CR4・EFER の写し、GDT/TSS と IDT、SSE、CR3 と RSP の切り替え）。
//!
//! **`kernel/src/smp.rs` から移した**（2026-09-28。境界の段階の手順 2）。**x86 の記述子の表と制御レジスタを扱い、
//! CR3 と RSP を隣接して切り替える asm を持つので、CPU 固有の置き場に置く。** **切り替えた後の共通の部分
//! （`smp` の `ap_after_switch`）と引き継ぎ表は `smp` に残る**（入口は呼ぶ側が渡す）。

use core::fmt::Write as _;

use common::arch::x86_64::cpu;
use common::machine::pc::serial::SerialPort;

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

/// AP が本番の世界へ移るときに BSP から受け取るもの（S3-b-2b-2）。
///
/// 恒等 VA と本番 VA が混在する。どちらの空間の値かを名前で区別する
/// （取り違えると BSP 側では正常に見え、AP 側でだけ落ちる）。
#[derive(Clone, Copy)]
pub struct ApBringUp {
    /// 本番テーブルの物理（`mov cr3` に載せる）。
    pub production_cr3: u64,
    /// 本番テーブルにしか存在しない per-CPU スタック。
    pub stacks: ApStacks,
    /// このコアのスロット。
    pub slot: usize,
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
/// # IDTR を載せてから CR3 を切り替えるまでのウィンドウ（受け入れて記録する）
///
/// IDTR を載せた後、CR3 を切り替えるまでの数命令の間、IST の VA（本番テーブルに
/// しか無い）はまだ見えない。そこで IST 経由の例外（`#DF` / `#PF`）が起きると
/// ハンドラのスタックへ飛べない。割り込みは禁止だが、例外は禁止できない。
///
/// 受け入れる。理由は 3 つである。
///
/// - この区間に例外を起こす操作を置いていない（`mov cr3` / `mov rsp` / `call` だけ。`call` が戻り先を積む先は
///   切り替えた後の自分の通常スタックで、本番のテーブルにある）
/// - 順序を入れ替える案（IST なしの IDT を先に載せ、CR3 の後で差し替える）は
///   IDT を 2 回載せることになり、「IDT は 1 本を共有する」という単純さを壊す
/// - ウィンドウは数命令で、b-2b-1 の教訓どおり間に何も置かない形にしてある
///
/// この区間に命令を足すときは、この判断を再評価すること。上の 1 つ目の理由は
/// 「今は mov が 2 つと call だけ」に依存している。足した瞬間に前提が崩れる。
///
/// `entry` は、切り替えた後に跳ぶ共通の側の入口である（`smp` が渡す）。
///
/// # 契約（境界の関数。2026-09-28）
///
/// - `info.production_cr3` は本番のページテーブルの根の**物理アドレス**、`info.stacks` の 3 つの頂点は本番の
///   ページテーブルにだけある**仮想アドレス**、`info.slot` はこの AP のスロットの番号（1 から）である。
/// - 呼んでよいのは AP 自身だけで、トランポリンから入った直後（起動の表の上、割り込みは止まったまま、自分の GDT は
///   まだ載っていない）に 1 回だけである。BKL は要らない（共有するものにまだ触らない）。
/// - 戻らない。`entry` へ入る時点で、この CPU の CR0・CR4・EFER は BSP と同じで、自分の GDT/TSS と共有の IDT が
///   載っていて（`cpu_id()` が正しい）、SSE が使え、CR3 は本番のページテーブル、RSP は自分の通常スタックの頂点である。
/// - 変えるのはこの CPU の状態だけで、ほかの CPU との同期は含まない。
///
/// # 入口（`entry`）の契約
///
/// - 呼び出し規約は `extern "C"`（System V）で、戻らない（`-> !`）。`call` で入る（戻り先を積む。戻ったら `ud2` で
///   落とす）。第 1 引数（`rdi`）はスロットの番号である。
/// - スタックは自分の通常スタックの頂点である（4 KiB 境界）。`call` が戻り先を積むので、入口の RSP は System V の
///   決まりどおり 16 で割ると 8 余る（2026-09-28 までは `jmp` で入っていて、8 ずれていた）。入口の先頭で
///   [`crate::arch::x86_64::check_entry_stack_alignment`] を呼ぶこと。
/// - 割り込みは止まったままである。IDT は載っているので、例外は自分の IST へ入る。NMI は `cli` では止まらない。
/// - `entry` はカーネルのイメージの中の関数であること。関数なので、ずっと有効である。
///
/// # Safety
///
/// AP 自身から、b-2b-1 のトランポリンで入った直後に 1 回だけ呼ぶこと。
pub unsafe fn bring_up_application_processor(
    info: ApBringUp,
    entry: extern "C" fn(usize) -> !,
) -> ! {
    // 0. **BSP の CR0・CR4・EFER をコピーする**（2026-09-24。`kernel::arch::x86_64::cpu_state`）。**トランポリンは INIT の直後の
    //    値に PAE・LME・PG と PE しか足さない**——**CD と NW が 1（キャッシュが効かない）で、WP と NE が 0 の
    //    まま走っていた**（実測）。**何より先にコピーする**——この先のコードをキャッシュと WP の下で走らせる。
    // SAFETY: AP の起動の途中で、長モードに居て、割り込みは禁止のままである。
    unsafe {
        crate::arch::x86_64::cpu_state::adopt_bsp_state_on_this_ap();
    }

    // 1. 自分の GDT / TSS を載せる。索引は引数で受け取ったものである
    //    （`cpu_id()` はまだ使えない。GDT が載って初めて正しくなる）。
    // SAFETY: slot は BSP が割り当てた 0..MAX_CPUS の値。IST の頂点は本番
    // テーブルの VA なので、CR3 を移した後にしか実際には触れないが、
    // TSS へ書くだけならここで問題ない。割り込みは禁止のままである。
    unsafe {
        crate::arch::x86_64::gdt::init_for_cpu(
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
        crate::arch::x86_64::idt::load_shared();
    }

    // 3. このコアで SSE を有効にする（`ADR-0058` の Decision 3）。
    //
    // **CR0 と CR4 はコアごとのレジスタなので、BSP で立てても AP には効かない。**
    // **忘れると、このコアの上で SSE 命令が `#UD` で落ちる**——**いま Ring 3 は
    // BSP の上でしか走らないので、落とす判定が無い**（`ADR-0058` の
    // 「決定 3 に判定が無い理由」）。**だから忘れやすい。ここに置く理由でもある。**
    // SAFETY: このコアにつき 1 回だけで、まだ FP を使うコードは走っていない。
    unsafe {
        crate::arch::x86_64::fp::enable_on_this_cpu();
    }
    {
        // **読み戻して出力する。** **BSP の行は AP について何も示さない**ので、
        // **コアごとに 1 行ずつ出す。**
        let state = crate::arch::x86_64::fp::enabled_state();
        let mut port = SerialPort::new(SerialPort::COM1_BASE);
        port.init();
        let _ = writeln!(
            port,
            "[INFO] fp: SSE is enabled on ap {}: CR0={:#x} CR4={:#x}, as intended = {} \
             [read back from the registers]",
            info.slot,
            state.cr0,
            state.cr4,
            state.as_intended()
        );
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

    // 3. CR3 と RSP を隣接して切り替え、入口へ `call` で入る（System V の入口の決まりに合わせる。2026-09-28）。
    // SAFETY: `production_cr3` は BSP が動いている本番テーブルの物理で、
    // `kernel_top` はそのテーブルに存在する VA である。間に何も置かない。
    // `call` が戻り先を積む先は切り替えた後のスタック（本番テーブルにある）で、入口は戻らない（戻ったら `ud2`）。
    unsafe {
        core::arch::asm!(
            "mov cr3, {cr3}",
            "mov rsp, {rsp}",
            "call {entry}",
            "ud2",
            cr3 = in(reg) info.production_cr3,
            rsp = in(reg) info.stacks.kernel_top,
            entry = in(reg) entry,
            in("rdi") info.slot,
            options(noreturn),
        )
    }
}
