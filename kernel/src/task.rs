//! 協調的マルチタスク（M5-c）。
//!
//! コンテキストスイッチは IRQ スタブの復元経路に載せる（ADR-0019 §2）。
//! `yield` は専用ベクタへのソフトウェア割り込み（`int YIELD_VECTOR`）で、
//! CPU が積む割り込みスタックフレームと、共通スタブが積む 15 本の GPR で
//! **スタック上に完全な `IrqContext` がそろう**。[`on_yield`] が「次に使う
//! RSP」を返し、スタブが `mov rsp, rax` でそれを RSP へ入れることで、RSP の
//! 入れ替えだけで切り替わる。明示的なレジスタ復元は既存の復元経路と `iretq`
//! がそのまま担う。
//!
//! M5-c は **協調的**（自発的 yield のみ、プリエンプションなし）で、タスクは
//! 2 本。決定的に往復するので逐次的に検証できる。
//!
//! # 保存する CPU 状態は RSP ただ 1 つ
//!
//! タスクの状態は「保存された RSP」だけである。その RSP が指す先に、
//! 15 本の GPR と割り込みフレーム（RIP/CS/RFLAGS/RSP/SS）が `IrqContext` の
//! 形で並んでいる。復元は復元経路が行う。

use core::fmt::Write as _;
use core::ptr::addr_of;

use common::addr::VirtAddr;
use common::critical::critical_nesting_depth;
use common::serial::SerialPort;

use crate::gdt;
use crate::idt::YIELD_VECTOR;
use crate::paging::active::{ActivePageTable, PageSize};

/// ワーカータスクの本数（M5-c は 2 本）。
pub const WORKER_COUNT: usize = 2;

/// タスクの総数（メイン + ワーカー）。インデックス 0 がメイン。
const TASK_COUNT: usize = WORKER_COUNT + 1;

/// 各ワーカーのカーネルスタックの大きさ。デモは浅いので 16KiB で足りる。
const TASK_STACK_SIZE: usize = 16 * 1024;

/// スタックの直下に置くガードページの大きさ（1 ページ）。
const GUARD_SIZE: usize = 4096;

/// 各ワーカーが GPR 照合を回すラウンド数。
const ROUNDS_PER_WORKER: u64 = 3;

/// `IrqContext` のバイト数（21 個の `u64`）。偽コンテキストの大きさに使う。
const IRQ_CONTEXT_BYTES: u64 = 21 * 8;

/// 15 本の GPR の、`IrqContext` 先頭からのオフセット順に対応するタグ。
///
/// ワーカー本体は各レジスタへ `base + tag` を入れ、往復後に一致を照合する。
/// `rsp`（タグ 7 相当）は値レジスタではないのでこの検査には含めない。順序は
/// rax, rbx, rcx, rdx, rsi, rdi, rbp, r8..r15。
const GPR_TAGS: [u64; 15] = [0, 1, 2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15];

/// ワーカー本体が往復後に 15 本の GPR を書き出す共有バッファ。
///
/// 協調的で一度に 1 タスクしか走らないため、往復から照合までの straight-line
/// 区間を別タスクが割り込むことは無い（プリエンプションは M5-d）。
static mut GPR_BUF: [u64; 15] = [0; 15];

/// タスク 1 本ぶんの状態。
#[derive(Clone, Copy)]
struct Task {
    /// 保存された RSP（この値が指す先が `IrqContext`）。走行中は無効。
    saved_rsp: u64,
    /// このタスクのカーネルスタック頂点（RSP0 用。§2.2）。
    // no-swap の破壊ビルドではスイッチしないので RSP0 更新へ進まず未読になる。
    #[cfg_attr(feature = "task-switch-no-swap", allow(dead_code))]
    stack_top: u64,
    /// 実行可能か。`false` はメイン（ワーカー終了時のみ戻る）または終了済み。
    runnable: bool,
    /// GPR 照合の基準値（タスク固有）。ワーカーのみ使う。
    base: u64,
    /// 残りラウンド数。0 になったら終了する。
    rounds_left: u64,
    /// このタスクが再開された回数（会計用）。
    resumes: u64,
}

const EMPTY_TASK: Task = Task {
    saved_rsp: 0,
    stack_top: 0,
    runnable: false,
    base: 0,
    rounds_left: 0,
    resumes: 0,
};

struct Scheduler {
    tasks: [Task; TASK_COUNT],
    /// 現在走行中のタスク。
    current: usize,
    /// スイッチした総回数（会計用）。各スイッチで再開されたタスクの `resumes`
    /// も 1 増えるので、`switches == 全タスクの resumes の合計`。
    switches: u64,
}

/// スケジューラのグローバル状態。
///
/// **触るのは割り込み禁止の区間だけである。** [`on_yield`] は割り込みゲート
/// 経由（IF=0）で入り、セットアップ（[`run_cooperative_demo`]）は起動時の
/// 単一実行文脈から呼ぶ。シングルコアなのでこれで排他が成立する。
static mut SCHEDULER: Scheduler = Scheduler {
    tasks: [EMPTY_TASK; TASK_COUNT],
    current: 0,
    switches: 0,
};

/// 各ワーカーのスタック（ガードページ + スタック本体）。
///
/// `align(4096)` で先頭がページ境界に載り、`guard` がちょうど 1 ページになる
/// （M5-b と同じ作りで、各ワーカーのスタックにガードページを置ける）。
#[repr(C, align(4096))]
struct WorkerStack {
    guard: [u8; GUARD_SIZE],
    stack: [u8; TASK_STACK_SIZE],
}

const EMPTY_WORKER_STACK: WorkerStack = WorkerStack {
    guard: [0; GUARD_SIZE],
    stack: [0; TASK_STACK_SIZE],
};

static mut WORKER_STACKS: [WorkerStack; WORKER_COUNT] = [EMPTY_WORKER_STACK; WORKER_COUNT];

extern "C" {
    /// ワーカー本体（`global_asm!` で定義）。偽 `IrqContext` の RIP が指す。
    static zaytos_worker_body: u8;
}

/// COM1 へ 1 行書く小さな補助。デモの出力はメインループの外の複数文脈から
/// 出るので、確保もロックも介さずシリアルへ直接書く（ADR-0019 §4、パニック
/// 経路と同じ作法）。
fn serial_line(args: core::fmt::Arguments) {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(serial, "{args}");
}

/// あるワーカーのスタックのガードページを unmap する（M5-b と同じ機構）。
///
/// # Safety
///
/// 自前のページテーブルへ切り替え済みで、`guard_virt` が 4KiB でマップされた
/// ワーカースタックの直下のページであること。
unsafe fn install_worker_guard_page(guard_virt: VirtAddr) {
    // SAFETY: CR3 は自前のテーブルを指し、その配下は登録窓で読み書きできる。
    let mut table = unsafe { ActivePageTable::current(common::addr::direct_map()) };
    match table.translate(guard_virt) {
        Ok(Some(t)) if t.page_size == PageSize::Size4KiB => {}
        other => {
            serial_line(format_args!(
                "[ERROR] task: worker guard page {:#x} is not a 4KiB mapping ({other:?}); halting \
                 (deferred-decisions: ガードページの split 化)",
                guard_virt.as_u64()
            ));
            common::cpu::halt_forever();
        }
    }
    // SAFETY: guard_virt はワーカースタックの直下のガードページ。今後この
    // ページへ正規のアクセスは無く、溢れたら #PF（IST2）で捕まえる。
    if let Err(e) = unsafe { table.unmap_4kib(guard_virt) } {
        serial_line(format_args!(
            "[ERROR] task: failed to unmap worker guard page {:#x}: {e:?}; halting",
            guard_virt.as_u64()
        ));
        common::cpu::halt_forever();
    }
}

/// ワーカースタックの (ガードページ先頭, スタック頂点) を返す。
fn worker_stack_bounds(index: usize) -> (VirtAddr, VirtAddr) {
    // SAFETY: 静的配列のアドレスを取るだけ。読み書きはしない。
    let block = unsafe { addr_of!(WORKER_STACKS[index]) };
    let base = block as u64;
    let guard = VirtAddr::new(base).expect("a .bss address is canonical");
    let top = VirtAddr::new(base + GUARD_SIZE as u64 + TASK_STACK_SIZE as u64)
        .expect("the worker stack stays within the canonical range");
    (guard, top)
}

/// 新規タスクの偽 `IrqContext` をスタック頂点に積み、保存 RSP を返す。
///
/// 初回スイッチで [`on_yield`] がこの RSP を返すと、`mov rsp, rax` → pop 15 →
/// `add rsp, 8` → `iretq` の経路が、あたかも割り込みから戻るように `entry` へ
/// IF=1 で入る。
///
/// # Safety
///
/// `top` が有効でマップ済みのスタック頂点（16 バイト境界）であること。
unsafe fn build_initial_context(top: VirtAddr, entry: u64) -> u64 {
    let saved_rsp = top.as_u64() - IRQ_CONTEXT_BYTES;
    // saved_rsp から上へ 21 個の u64 を並べる（IrqContext のフィールド順）。
    // 0..15: GPR（rax..r15）、15: vector、16: rip、17: cs、18: rflags、
    // 19: rsp、20: ss。
    let slot = |i: usize, value: u64| {
        // SAFETY: 呼び出し元契約により、[saved_rsp, top) はマップ済みで誰も
        // 使っていないスタック領域。i < 21。
        unsafe {
            core::ptr::write_volatile((saved_rsp as *mut u64).add(i), value);
        }
    };
    for i in 0..15 {
        slot(i, 0); // GPR は 0 で始める。ワーカーは自分で base を読み直す。
    }
    slot(15, YIELD_VECTOR as u64); // vector（add rsp,8 で捨てられる）
    slot(16, entry); // rip
    slot(17, gdt::KERNEL_CODE_SELECTOR.bits() as u64); // cs
    slot(18, 0x202); // rflags（IF=1、予約ビット1）
    slot(19, top.as_u64()); // rsp（iretq 後にタスクが使う RSP）
    slot(20, gdt::KERNEL_DATA_SELECTOR.bits() as u64); // ss
    saved_rsp
}

/// 協調的マルチタスクのデモと検証を実行する（M5-c）。
///
/// メイン（タスク 0）が 2 本のワーカーを起こし、初回スイッチで往復を始める。
/// 両ワーカーが終了するとメインへ戻り、会計を閉じて戻る。呼び出し後、起動
/// シーケンスは続行する（タイマループへ進む）。
// yield-in-critical のビルドでは fail-fast で halt するため、その先の会計が
// 到達不能になる。回帰チェック専用のビルドなので許容する。
#[cfg_attr(feature = "task-switch-yield-in-critical", allow(unreachable_code))]
pub fn run_cooperative_demo() {
    // SAFETY: 起動時の単一実行文脈。まだ誰もスケジューラを触っていない。
    unsafe {
        setup_tasks();
    }

    serial_line(format_args!(
        "task: starting cooperative demo with {WORKER_COUNT} workers, \
         {ROUNDS_PER_WORKER} rounds each"
    ));

    // yield-in-critical の破壊確認: InterruptGuard を保持したまま yield を
    // 呼び、on_yield のガードが fail-fast することを確かめる。戻らない。
    #[cfg(feature = "task-switch-yield-in-critical")]
    {
        serial_line(format_args!(
            "task: (yield-in-critical) acquiring an InterruptGuard, then yielding on purpose"
        ));
        let _guard = common::critical::InterruptGuard::enter();
        yield_now();
        serial_line(format_args!(
            "[ERROR] task: yield returned while holding a guard; the yield guard did not fire; halting"
        ));
        common::cpu::halt_forever();
    }

    // 初回スイッチ。メインの文脈がここで保存され、ワーカー A へ入る。両ワーカー
    // が終了すると、この int から戻ってくる。
    #[cfg(not(feature = "task-switch-yield-in-critical"))]
    yield_now();

    // --- 会計を閉じる ---
    // SAFETY: ワーカーは終了済みで、走行中はメインだけ。読み取りのみ。
    let (switches, resume_sum, a_rounds, b_rounds) = unsafe {
        let s = addr_of!(SCHEDULER);
        let switches = (*s).switches;
        let resume_sum: u64 = (*s).tasks.iter().map(|t| t.resumes).sum();
        // ワーカーは rounds_left が 0 になっているはず。走った回数は
        // ROUNDS_PER_WORKER。
        let a_done = (*s).tasks[1].rounds_left == 0;
        let b_done = (*s).tasks[2].rounds_left == 0;
        (switches, resume_sum, a_done, b_done)
    };
    let accounting_ok = switches == resume_sum && a_rounds && b_rounds;
    serial_line(format_args!(
        "task: demo finished. switches={switches}, sum(resumes)={resume_sum}, \
         A done={a_rounds}, B done={b_rounds}, accounting balanced={accounting_ok}"
    ));
    if !accounting_ok {
        serial_line(format_args!(
            "[ERROR] task: accounting did not balance (a switch did not resume a task, or a \
             worker did not finish); halting"
        ));
        common::cpu::halt_forever();
    }

    // RSP0 の確認（§2.2）。スイッチのたびに on_yield が set_rsp0 → 読み戻しで
    // 一致を確かめており（不一致なら即 halt）、ここまで来た時点で全スイッチで
    // 一致していたことになる。最後のスイッチはメインへ戻ったので、現在の
    // TSS.RSP0 はメインのスタック頂点のはずである。それを読み戻して示す。
    let main_top = crate::stack::kernel_stack_range().top.as_u64();
    let rsp0 = gdt::privilege_stack_top();
    serial_line(format_args!(
        "task: TSS.RSP0 tracked every switch; now {rsp0:#x} (main stack top {main_top:#x}, \
         match={})",
        rsp0 == main_top
    ));

    serial_line(format_args!("task: cooperative switch verified"));
}

/// タスク表を初期化し、2 本のワーカーを起こす。
///
/// # Safety
///
/// 起動時の単一実行文脈から 1 回だけ呼ぶこと。自前のページテーブルへ切り替え
/// 済みであること（ガードページの unmap に使う）。
unsafe fn setup_tasks() {
    let entry = addr_of!(zaytos_worker_body) as u64;

    // タスク 0 = メイン。走行中なので saved_rsp は初回 yield で埋まる。
    // メインのスタック頂点は通常のカーネルスタック（RSP0 用）。
    let main_top = crate::stack::kernel_stack_range().top.as_u64();

    // SAFETY: 起動時の単一実行文脈。スケジューラはまだ誰も触っていない。
    let sched = unsafe { &mut *(addr_of!(SCHEDULER) as *mut Scheduler) };
    sched.current = 0;
    sched.switches = 0;
    sched.tasks[0] = Task {
        stack_top: main_top,
        runnable: false, // メインはワーカーが尽きたときだけ戻る
        ..EMPTY_TASK
    };

    for w in 0..WORKER_COUNT {
        let (guard, top) = worker_stack_bounds(w);
        // SAFETY: 起動時、自前のページテーブル上。ワーカースタックの直下 1
        // ページをガードページにする。
        unsafe {
            install_worker_guard_page(guard);
        }
        // SAFETY: top は今ガードページを張ったワーカースタックの頂点で、
        // まだ誰も使っていない。16 バイト境界（4KiB 境界）に載っている。
        let saved_rsp = unsafe { build_initial_context(top, entry) };
        // タスク固有の base。A=0xA1A1_0000、B=0xB2B2_0000 のように区別する。
        let base = 0xA1A1_0000u64 + (w as u64) * 0x1111_0000;
        sched.tasks[1 + w] = Task {
            saved_rsp,
            stack_top: top.as_u64(),
            runnable: true,
            base,
            rounds_left: ROUNDS_PER_WORKER,
            resumes: 0,
        };
    }
}

/// 協調的 yield。専用ベクタへソフトウェア割り込みを出す。
///
/// `int` が割り込みスタックフレームを積み、スタブが GPR を退避して
/// `IrqContext` を完成させる。[`on_yield`] が次タスクの RSP を返し、復元経路が
/// そこへ切り替える。次に自分が選ばれるとこの `int` の直後へ戻る。
///
/// ガードの判定は [`on_yield`] 側で行う（`int` を通る全 yield を覆うため）。
#[inline(always)]
pub fn yield_now() {
    // SAFETY: yield_vector のゲートは IDT に登録済みで、専用スタブ経由で
    // 共通ルーチンへ入る。レジスタは呼び出し規約どおりクロバー扱いにする。
    unsafe {
        core::arch::asm!(
            "int {yv}",
            yv = const YIELD_VECTOR,
            clobber_abi("sysv64"),
        );
    }
}

/// yield ベクタが届いたときに `irq_entry` から呼ばれ、次に使う RSP を返す。
///
/// `current_rsp` は現タスクの `IrqContext` 先頭（`irq_entry` に渡る `context`）
/// で、現タスクの保存 RSP として記録する。
///
/// **`Locked` / `InterruptGuard` を保持したまま yield してはならない。** 保持
/// したまま切り替えると、別タスクがクリティカルセクションの途中で走る。判定は
/// critical nesting depth で行い、IF は見ない（ADR-0019 §5、yield は IF=0 から
/// 正当に呼ばれうる）。
pub fn on_yield(current_rsp: u64) -> u64 {
    // 保持中の yield を fail-fast する。int ゲート自身が積んだぶんは
    // InterruptGuard ではないのでカウンタには乗らない。したがってここが 0 で
    // なければ、呼び出し側が Locked / InterruptGuard を保持している。
    if critical_nesting_depth() != 0 {
        serial_line(format_args!(
            "[ERROR] task: yield called while holding a Locked/InterruptGuard \
             (critical nesting depth = {}). yielding here would run another task inside a \
             critical section; halting",
            critical_nesting_depth()
        ));
        common::cpu::halt_forever();
    }

    // SAFETY: 割り込みゲート経由（IF=0）で入っており、シングルコアなので他の
    // 実行文脈が同時にスケジューラを触ることはない。
    let sched = unsafe { &mut *(addr_of!(SCHEDULER) as *mut Scheduler) };

    let current = sched.current;
    sched.tasks[current].saved_rsp = current_rsp;

    // 破壊確認 (ii): RSP の差し替えを省く。現タスクの RSP を返すのでスイッチが
    // 起きず、同じタスクが回り続ける。デモの会計・順序で検出する。
    #[cfg(feature = "task-switch-no-swap")]
    {
        let _ = &sched.tasks; // no-swap では next を選ばない。
        return current_rsp;
    }

    #[cfg(not(feature = "task-switch-no-swap"))]
    {
        let next = pick_next(sched, current);
        sched.current = next;
        sched.switches += 1;
        sched.tasks[next].resumes += 1;

        // RSP0 を次タスクのスタック頂点へ更新する（§2.2、効くのは M5-e）。
        let expected_rsp0 = sched.tasks[next].stack_top;
        // SAFETY: stack_top は次タスクの有効なスタック頂点。切り替えの割り込み
        // 禁止区間から呼んでいる。
        unsafe {
            gdt::set_rsp0(expected_rsp0);
        }
        // **実際の状態を読む。** RSP0 は M5-e まで挙動に現れないので、間違った
        // 値が書かれても誰も気づかない。TSS から読み戻して期待値と一致する
        // ことをその場で確かめる（A-1 / M5-b と同じく実状態を見る）。一致
        // しなければ静かに壊れる前に止める。
        let readback = gdt::privilege_stack_top();
        if readback != expected_rsp0 {
            serial_line(format_args!(
                "[ERROR] task: TSS.RSP0 readback {readback:#x} != expected {expected_rsp0:#x} \
                 after switch to task {next}; halting",
            ));
            common::cpu::halt_forever();
        }

        let next_rsp = sched.tasks[next].saved_rsp;

        // 破壊確認 (i): 次タスクの保存コンテキストの rbx スロットを壊す。
        // 復帰した次タスクは rbx が base+1 と食い違うのを GPR 照合で検出する。
        #[cfg(feature = "task-switch-drop-reg")]
        // SAFETY: next_rsp は次タスクの IrqContext 先頭。+8 は rbx のスロット。
        unsafe {
            core::ptr::write_volatile((next_rsp as *mut u64).add(1), 0xDEAD_BEEF);
        }

        next_rsp
    }
}

/// 次に走らせるタスクを選ぶ。ワーカーを巡回し、走行可能なものが無ければ
/// メイン（0）へ戻る。
// no-swap の破壊ビルドではスイッチしないので、次タスクを選ばず未使用になる。
#[cfg_attr(feature = "task-switch-no-swap", allow(dead_code))]
fn pick_next(sched: &Scheduler, current: usize) -> usize {
    for offset in 1..=WORKER_COUNT {
        let cand = if current == 0 {
            ((offset - 1) % WORKER_COUNT) + 1
        } else {
            ((current - 1 + offset) % WORKER_COUNT) + 1
        };
        if sched.tasks[cand].runnable {
            return cand;
        }
    }
    0
}

/// ワーカー本体（`global_asm!`）から呼ばれる。現タスクの GPR 基準値を返す。
extern "sysv64" fn current_task_base() -> u64 {
    // SAFETY: ワーカー本体（IF=0 ではないが単一走行）から呼ばれる。読み取りのみ。
    let sched = unsafe { &*addr_of!(SCHEDULER) };
    sched.tasks[sched.current].base
}

/// ワーカー本体から呼ばれる。往復後の 15 本の GPR（`GPR_BUF`）を基準値と照合し、
/// 結果を出す。残りラウンドがあれば 1、無ければ 0 を返す。
extern "sysv64" fn verify_gprs_and_advance() -> u64 {
    // SAFETY: 単一走行。現タスクの基準値とバッファを読む。
    let sched = unsafe { &mut *(addr_of!(SCHEDULER) as *mut Scheduler) };
    let current = sched.current;
    let base = sched.tasks[current].base;

    // SAFETY: ワーカー本体が直前に 15 本を書き込んだ共有バッファ。
    let buf = unsafe { *addr_of!(GPR_BUF) };
    let mut mismatches = 0u32;
    let mut first_bad = None;
    for (i, &tag) in GPR_TAGS.iter().enumerate() {
        let expected = base.wrapping_add(tag);
        if buf[i] != expected {
            mismatches += 1;
            if first_bad.is_none() {
                first_bad = Some((tag, buf[i], expected));
            }
        }
    }

    sched.tasks[current].rounds_left = sched.tasks[current].rounds_left.saturating_sub(1);
    let round = ROUNDS_PER_WORKER - sched.tasks[current].rounds_left;
    let name = if current == 1 { 'A' } else { 'B' };

    if mismatches == 0 {
        serial_line(format_args!(
            "task: {name} round {round}/{ROUNDS_PER_WORKER}: all 15 GPRs survived the switch \
             (base={base:#x})"
        ));
    } else {
        let (tag, got, exp) = first_bad.unwrap();
        serial_line(format_args!(
            "[ERROR] task: {name} round {round}: {mismatches} GPR(s) corrupted across the switch; \
             tag {tag} got {got:#x} expected {exp:#x}; halting"
        ));
        common::cpu::halt_forever();
    }

    if sched.tasks[current].rounds_left == 0 {
        0
    } else {
        1
    }
}

/// ワーカー本体から、全ラウンドを終えたときに呼ばれる。現タスクを終了扱いに
/// して yield する。以後スケジューラはこのタスクを選ばない。
extern "sysv64" fn worker_done_and_yield() {
    // SAFETY: 単一走行。現タスクを走行不可にする。
    let sched = unsafe { &mut *(addr_of!(SCHEDULER) as *mut Scheduler) };
    let current = sched.current;
    sched.tasks[current].runnable = false;
    let name = if current == 1 { 'A' } else { 'B' };
    serial_line(format_args!(
        "task: {name} finished all rounds; yielding for good"
    ));
    yield_now();
}

// ワーカー本体（アセンブリ）。偽 IrqContext の RIP がここを指す。
//
// 各ラウンド:
//   1. current_task_base() で自分の base を得る（rax）
//   2. 15 本の GPR へ base + tag を入れる（rax=base+0、rbx=base+1、…）
//   3. int YIELD_VECTOR で yield（往復でスタブが GPR を退避・復元する）
//   4. 復帰後の 15 本を GPR_BUF へ rip 相対で書き出す（レジスタを空けずに済む）
//   5. verify_gprs_and_advance() で照合。1 なら次ラウンド、0 なら終了
// 終了時は worker_done_and_yield() を呼び、戻ってこない前提で jmp ループする。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zaytos_worker_body",
    "zaytos_worker_body:",
    "2:", // ラウンドループ
    "  call {current_base}",
    "  lea rbx, [rax + 1]",
    "  lea rcx, [rax + 2]",
    "  lea rdx, [rax + 3]",
    "  lea rsi, [rax + 4]",
    "  lea rdi, [rax + 5]",
    "  lea rbp, [rax + 6]",
    "  lea r8,  [rax + 8]",
    "  lea r9,  [rax + 9]",
    "  lea r10, [rax + 10]",
    "  lea r11, [rax + 11]",
    "  lea r12, [rax + 12]",
    "  lea r13, [rax + 13]",
    "  lea r14, [rax + 14]",
    "  lea r15, [rax + 15]",
    // rax は既に base（タグ 0）。
    "  int {yv}",
    // 復帰。15 本を GPR_BUF へ rip 相対で書き出す（アドレスにレジスタを使わない）。
    "  mov qword ptr [rip + {buf} + 0],   rax",
    "  mov qword ptr [rip + {buf} + 8],   rbx",
    "  mov qword ptr [rip + {buf} + 16],  rcx",
    "  mov qword ptr [rip + {buf} + 24],  rdx",
    "  mov qword ptr [rip + {buf} + 32],  rsi",
    "  mov qword ptr [rip + {buf} + 40],  rdi",
    "  mov qword ptr [rip + {buf} + 48],  rbp",
    "  mov qword ptr [rip + {buf} + 56],  r8",
    "  mov qword ptr [rip + {buf} + 64],  r9",
    "  mov qword ptr [rip + {buf} + 72],  r10",
    "  mov qword ptr [rip + {buf} + 80],  r11",
    "  mov qword ptr [rip + {buf} + 88],  r12",
    "  mov qword ptr [rip + {buf} + 96],  r13",
    "  mov qword ptr [rip + {buf} + 104], r14",
    "  mov qword ptr [rip + {buf} + 112], r15",
    "  call {verify}",
    "  test rax, rax",
    "  jnz 2b",
    // 終了。
    "3:",
    "  call {done}",
    "  jmp 3b",
    current_base = sym current_task_base,
    verify = sym verify_gprs_and_advance,
    done = sym worker_done_and_yield,
    buf = sym GPR_BUF,
    yv = const YIELD_VECTOR,
);
