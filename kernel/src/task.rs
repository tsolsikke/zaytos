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
mod scheduler;

use core::ptr::addr_of;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use common::addr::VirtAddr;
use common::critical::critical_nesting_depth;
use common::percpu::{PerCpu, MAX_CPUS};
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

/// M5-d のワーカーが「窓」を広げる遅延ループの回数。**プリエンプトが set と
/// store の間に落ちる確率を上げ、統計的レジスタ検証の窓カウントを N > 0 に
/// 保つため**（条件1）。widen feature で長くして、窓カウントが増えることで
/// 判定が正しく働くことを確かめる。
///
/// NOP そりではなくメモリカウンタの遅延ループにしている。NOP そりだと巨大な
/// そりが .text を膨らませ、カーネルイメージが 2MiB 境界をまたいで RIP/RSP が
/// 2MiB ページに載り、H-2 やガードページ（4KiB 前提）を壊す（実際に踏んだ）。
/// 遅延ループは数命令で、そりの長さがコード量に効かない。カウンタはメモリなので
/// pattern レジスタも壊さない。
#[cfg(feature = "task-widen-preempt-window")]
const PREEMPT_WINDOW_SLED: usize = 4_000_000;
#[cfg(not(feature = "task-widen-preempt-window"))]
const PREEMPT_WINDOW_SLED: usize = 200_000;

/// 窓を広げる遅延ループのカウンタ（メモリ上。レジスタを使わずに回すため）。
static mut PREEMPT_DELAY: u64 = 0;

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
/// 一度に 1 タスクしか走らないので共有でよい。M5-c は yield の往復から照合まで、
/// M5-d は set から store までが straight-line で、別タスクが割り込んでも
/// スイッチが保存・復元するのが検査の対象である。
static mut GPR_BUF: [u64; 15] = [0; 15];

/// M5-d のワーカーが「15 GPR を保持している窓」に入っているかのフラグ。
///
/// ワーカー本体が rip 相対で、15 本を load した後 1、store した後 0 にする。
/// [`on_timer_tick`] は、これが 1 のときにプリエンプトした回数を数える
/// （条件1）。この回数が 0 なら統計的レジスタ検証は何も検証していない。
static mut IN_GPR_WINDOW: u8 = 0;

/// set と store の窓でプリエンプトが起きた回数（条件1）。デモ後に報告し、
/// 0 でないことを確かめる。
static PREEMPT_IN_WINDOW: AtomicU64 = AtomicU64::new(0);

/// preempt-in-critical の破壊確認で、ワーカーが競合する共有ロック。
///
/// 破壊ビルドでは InterruptGuard が cli を落とす（IF=1 のまま）ので、ワーカー A が
/// これを保持したままスピンする間に timer がプリエンプトし、ワーカー B が同じ
/// ロックを取ろうとして二重取得検出が発火する。正常ビルドでは cli により保持中は
/// IF=0 で timer が来ないため、この競合は起きない。
#[cfg(feature = "task-preempt-in-critical")]
static DEMO_LOCK: common::critical::Locked<u64> = common::critical::Locked::new(0);

/// タスクの状態（S3-a）。
///
/// # なぜ `Running` を持たないのか
///
/// **「どのCPUがどのタスクを走らせているか」は [`CURRENT`] が既に持っている。**
/// `Running(cpu)` はその逆写像なので、置くと同じ事実が 2 箇所に出て片方が必ず
/// 古くなる。**走っているかは [`CURRENT`] から導く。**
///
/// 走行中のタスクは [`Self::Ready`] のままである。`pick_next` が現タスクを
/// 返しうる契約（ホストテストで固定）がそれを要求する。**`Ready` は「走行可能」で
/// あって「走っていない」ではない。**
///
/// # なぜ `cpu_id` をペイロードに持たないのか
///
/// `MAX_CPUS = 1` の現在はどの状態でも `cpu_id` が常に `0` で、**値が分かれない
/// 間は分類の誤りが観測できない**（`verification-coverage.md`の一般則）。
/// 値が分かれるのは `cpu_id()` が実 ID を返す S3-b なので、**そこで必要性を
/// 判断する。** 先回りして置かない。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TaskState {
    /// スロットがまだ作られていない（`static` の初期値）。
    ///
    /// **`Finished` と区別する。** 「まだ作られていない」を「終了済み」と
    /// 書くのは嘘であり、会計でこの 2 つを足し合わせると意味を持たない。
    ///
    /// **この値は `pick_next` から観測されない。** 全 3 スロットは
    /// `run_cooperative_demo`（`main.rs` で `irq::unmask(0)` より前に呼ばれる）
    /// が `init_task` で埋めるので、最初のティックが来る時点では残っていない。
    /// 観測されないことに依存はしていない（`pick_next` は `Ready` 以外を
    /// 選ばないので、残っていても安全側に倒れる）。
    Uninitialized,
    /// 走行可能。走行中のタスクもこの状態である（上記）。
    Ready,
    /// 走行不可だが終了はしていない。メイン（ワーカーが尽きたときだけ戻る）と、
    /// 締切でデモを止められたワーカーがこれである。
    ///
    /// **メイン（タスク 0）が候補にならないのは `pick_next` のループ範囲による。
    /// 状態が `Blocked` であることは除外の理由ではない。** `pick_next` は
    /// `1..=WORKER_COUNT` しか候補にせず、タスク 0 は「他に誰もいないとき」の
    /// 帰り先としてしか返らない（ホストテスト
    /// `main_is_never_picked_as_a_rotation_candidate` が固定している）。
    /// **したがってメインを `Ready` にしても走るようにはならない。**
    /// 動く理由を取り違えないよう書いておく。
    Blocked,
    /// 全ラウンドを終えた。以後スケジューラはこのタスクを選ばない。
    Finished,
}

impl TaskState {
    /// `pick_next` が選んでよい状態か。
    ///
    /// **`runnable: bool` からの置き換えで、この 1 関数が旧フィールドの
    /// 役割を担う。** 判定を 1 箇所に集めてあるので、状態を増やしたときに
    /// 選択可否を決め忘れることがない。
    const fn is_runnable(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// タスク 1 本ぶんの状態。
#[derive(Clone, Copy)]
struct Task {
    /// 保存された RSP（この値が指す先が `IrqContext`）。走行中は無効。
    saved_rsp: u64,
    /// このタスクのカーネルスタック頂点（RSP0 用。§2.2、およびスタック範囲の
    /// 上端）。
    // no-swap の破壊ビルドではスイッチしないので RSP0 更新へ進まず未読になる。
    #[cfg_attr(feature = "task-switch-no-swap", allow(dead_code))]
    stack_top: u64,
    /// このタスクのカーネルスタック下端（スタック混在検査に使う）。
    #[cfg_attr(feature = "task-switch-no-swap", allow(dead_code))]
    stack_bottom: u64,
    /// このタスクの状態（S3-a）。
    ///
    /// **以前は `runnable: bool` だった。** `false` が「メイン（ワーカー終了時のみ
    /// 戻る）」と「終了済み」の 2 つの意味を畳んでいたので、状態機械へ広げて
    /// 分けた。選択可否は [`TaskState::is_runnable`] が決める。
    state: TaskState,
    /// GPR 照合の基準値（タスク固有）。ワーカーのみ使う。
    base: u64,
    /// 残りラウンド数（M5-c の協調デモ用）。0 になったら終了する。
    rounds_left: u64,
    /// このタスクが照合を回した回数（進捗の会計用）。
    iterations: u64,
    /// このタスクが再開された回数（会計用）。
    resumes: u64,
}

const EMPTY_TASK: Task = Task {
    saved_rsp: 0,
    stack_top: 0,
    stack_bottom: 0,
    state: TaskState::Uninitialized,
    base: 0,
    rounds_left: 0,
    iterations: 0,
    resumes: 0,
};

// スケジューラのグローバル状態は [`scheduler`] モジュールが持つ。
//
// # 保護の契約（S0-bで別名違反を解消した）
//
// かつてここには `static mut SCHEDULER` があり、次の 3 つの文脈から
// **構造体全体への `&mut`** を作っていた。
//
// 1. **起動時の単一文脈**: [`setup_tasks`] と [`setup_preemptive_tasks`]
//    （後者は `InterruptGuard` で IF=0 にしてから触る）。
// 2. **IF=0 の割り込みハンドラ**: [`on_yield`] / [`on_timer_tick`] は割り込み
//    ゲート経由で入るので IF=0。そこから [`schedule_switch`] が触る。
// 3. **IF=1 のワーカーコールバック**: [`current_task_base`] /
//    [`verify_preemptive_gprs`] などがワーカー本体（`global_asm!`）から
//    呼ばれる。偽 `IrqContext` の RFLAGS は `0x202`（IF=1）なので、
//    **ここは割り込み許可のまま走る。**
//
// **文脈 2 と 3 は同一コア上で本当に並行する。** プリエンプティブデモは
// タイマ稼働後に始まるので、IF=1 のワーカーがスケジューラを触っている最中に
// タイマが入る。触るフィールドが別でも、2 つの `&mut` が同時に生きること
// 自体が Rust の別名規則違反であり、`MAX_CPUS > 1` を待たずに**現在も
// 未定義動作**だった。
//
// S0-b で実体を [`scheduler`] モジュールへ移し、外へはフィールド単位の操作
// だけを出した。**構造体全体への参照は、モジュールの外からは書こうとしても
// 書けない。** フィールドごとにどの文脈が触るか、どれが volatile を要するかは
// [`scheduler`] のモジュールコメントの表にある。
//
// 例外・NMI・パニックの各経路はスケジューラを触らない（`idt` から
// `crate::task` を呼ぶのは `on_yield` と `on_timer_tick` の 2 箇所だけで、
// どちらも IRQ 経路である。実測で確認した）。

/// 現在走行中のタスクのインデックス（コアごと。seam整備3d、ADR-0023）。
///
/// M5-c の当初は `Scheduler` の `current` フィールドだった。「現在のタスク」は
/// コアローカルな概念（各コアが別のタスクを走らせる）なので、per-CPU が正しい
/// 単位である。`tasks` 配列は BKL 下で共有しうる（全コアが同じタスク表を見る）
/// が、「そのうちどれを今走らせているか」はコアごとに異なる。
///
/// # `AtomicUsize` にする理由（`static mut usize` ではなく）
///
/// 読み手にはプリエンプティブデモのワーカー（[`preemptive_loop_top`] /
/// [`verify_preemptive_gprs`] 経由）が含まれ、そこは **IF=1**（プリエンプト可）で
/// 走る。その読みと、timer 割り込み（[`on_timer_tick`] → [`schedule_switch`] →
/// [`set_current_index`]）の書きは、同一コアでも Rust のメモリモデル上「並行」で
/// あり、非アトミックだとデータ競合＝未定義動作になる。x86 で整列 `usize` の読みが
/// 分割されないのは事実だが、それは Rust の規則を満たす根拠にはならない。よって
/// `AtomicUsize` にし、`Relaxed` で読み書きする（GDT/TSS の 3c と違い、`usize` は
/// アトミックにできる。3b の `CRITICAL_NESTING_DEPTH` と同じ形）。x86 では
/// `Relaxed` の load/store は素の `mov` にコンパイルされるので実行時コストは無い。
/// 非mut static になるので `static mut` も不要になる。
///
/// # 型検査が証明すること / 人間が確認すること（分けて書く）
///
/// - **型検査が証明した**: `Scheduler` に `current` フィールドは存在せず、それを
///   参照するコードも存在しない（フィールドごと削除したので、旧 `sched.current` が
///   1 つでも残ればコンパイルが通らない。構造で二重化を禁じている）。
/// - **grep と構造レビューが確認した（コンパイラは証明していない）**: 「現在の
///   タスク」に相当する別の状態が他に無いこと。仮に別の `static` が「最後に走った
///   タスク」等を持っていてもコンパイルは通るので、これは人間の確認である
///   （`docs/verification-coverage.md` の「二重の真実」）。
///
/// # `MAX_CPUS > 1` で顕在化する前提
///
/// 初期値 `[0; MAX_CPUS]` は「全コアがタスク 0 を current として始まる」を意味する。
/// `MAX_CPUS = 1` では正しいが、`MAX_CPUS > 1` では各 AP の起動時に別途 current を
/// 設定するか sentinel を置く必要がある。この前提は `cpu_id() < MAX_CPUS` の境界
/// （`common::percpu`）と同じクラスタで、`docs/deferred-decisions.md` の
/// 「per-CPU seam が MAX_CPUS > 1 で顕在化する前提」に一覧化してある。
static CURRENT: PerCpu<AtomicUsize> = PerCpu::new([const { AtomicUsize::new(0) }; MAX_CPUS]);

/// 自コアの現在タスクインデックスを読む（旧 `sched.current` の読みと同じ意味）。
///
/// IF=1 のワーカーからも呼ばれるので `Relaxed` のアトミック読みにする（上の
/// [`CURRENT`] のドキュメント参照）。
fn current_index() -> usize {
    CURRENT.this_cpu().load(Ordering::Relaxed)
}

/// 自コアの現在タスクインデックスを書く（旧 `sched.current = ...` と同じ意味）。
///
/// アトミックなので `unsafe` は要らない。書きは論理的には [`schedule_switch`] の
/// IF=0 区間か起動時に限るが、それはメモリ安全性の契約ではなくスケジューリングの
/// 都合である。
fn set_current_index(next: usize) {
    CURRENT.this_cpu().store(next, Ordering::Relaxed);
}

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

/// GPR 照合デモを走らせてよいコアか確かめる（S3-a）。走れないなら停止する。
///
/// # なぜ要るのか。[`GPR_BUF`] が per-CPU ではない
///
/// [`GPR_BUF`] はワーカー A / B で共有され、排他はワーカー本体の `global_asm!`
/// 内の生 `cli`…`sti` である。**`cli` が止められるのは同一コアの割り込みだけ
/// なので、別のコアで走るタスクからの並行アクセスは防げない。**
///
/// per-CPU 化はできない。**あの区間では 15 本の GPR 全部が検査対象のパターンを
/// 保持しており、アドレス計算に使えるレジスタが 1 本も無い**（だから rip 相対で
/// 触っている）。自コアのスロットを選ぶには GS 相対か集約ブロック形式が必要で、
/// どちらも現時点では無い（`deferred-decisions.md` の `GPR_BUF` の項目）。
///
/// **配列にして `MAX_CPUS` 本持たせるだけでは解決しない。** rip 相対のままだと
/// 全コアがスロット 0 を叩くので、per-CPU 化が済んだように見えて共有のままに
/// なる。そこで**形を変える代わりに、前提が破れたら落ちる形にしてある。**
///
/// # この検査の性格
///
/// **現在は常に成立する。** `MAX_CPUS = 1` で [`common::percpu::cpu_id`] が
/// 常に `0` を返すためである。**目的は、AP がタスクを実行し始めた段で落ちること**
/// であって、今なにかを捕まえることではない。
///
/// **破壊確認は現時点では構成できない。** `cpu_id()` に非 `0` を返させる手段が
/// まだ無い。**S3-b で `cpu_id()` が実 ID を返すようになった時点で構成可能に
/// なるので、S3-b の到達条件に入れてある**（`roadmap.md`）。
/// `smp::trampoline_frame()` や `irq::mask_all()` と同じ扱いである。
fn require_bootstrap_processor(what: &str) {
    let cpu = common::percpu::cpu_id();
    if cpu != 0 {
        serial_line(format_args!(
            "task: {what} may only run on the bootstrap processor (cpu 0), but cpu_id()={cpu}; \
             GPR_BUF is shared and its asm exclusion is a bare cli, which cannot keep another \
             core out; halting"
        ));
        common::cpu::halt_forever();
    }
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
    require_bootstrap_processor("the cooperative demo");
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
    // ワーカーは終了済みで、走行中はメインだけ。フィールド単位で読む
    // （配列全体への参照を作らない。S0-b）。
    let switches = scheduler::switches();
    let resume_sum: u64 = (0..TASK_COUNT).map(scheduler::resumes).sum();
    // ワーカーは rounds_left が 0 になっているはず。走った回数は
    // ROUNDS_PER_WORKER。
    let a_rounds = scheduler::rounds_left(1) == 0;
    let b_rounds = scheduler::rounds_left(2) == 0;
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

    set_current_index(0);
    scheduler::set_switches(0);
    scheduler::init_task(
        0,
        Task {
            stack_top: main_top,
            stack_bottom: crate::stack::kernel_stack_range().bottom.as_u64(),
            // メインはワーカーが尽きたときだけ戻る。終了済みではない。
            state: TaskState::Blocked,
            ..EMPTY_TASK
        },
    );

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
        scheduler::init_task(
            1 + w,
            Task {
                saved_rsp,
                stack_top: top.as_u64(),
                // 使えるスタックの下端はガードページの直上。
                stack_bottom: guard.as_u64() + GUARD_SIZE as u64,
                state: TaskState::Ready,
                base,
                rounds_left: ROUNDS_PER_WORKER,
                iterations: 0,
                resumes: 0,
            },
        );
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

    schedule_switch(current_rsp)
}

/// timer（IRQ0）のティックで `irq_entry` から呼ばれ、プリエンプティブに切り替える
/// （M5-d）。yield と同じ [`schedule_switch`] 中核へ合流する。
///
/// **明示 yield と違い、critical 区間中なら fail-fast せずスキップする。** timer
/// が割り込むのは呼び出し側のバグではない。ただし今は譲るべきでないので現 RSP を
/// 返してプリエンプトしない。もっとも、`InterruptGuard` は cli してから深さを
/// 増やすので `depth>0 ⟹ IF=0 ⟹ timer は配送されない`（ADR-0019 §5）。この
/// スキップは、その構造的保証が崩れたときの防御である（`task-preempt-in-critical`
/// で実際に崩して発火させる）。
pub fn on_timer_tick(current_rsp: u64) -> u64 {
    // 防御的スキップ。critical 区間中はプリエンプトせず現タスクを続行する。
    // 既定ビルド（と feature 下で arm されていないとき）はここで守る。
    #[cfg(not(feature = "task-preempt-in-critical"))]
    if critical_nesting_depth() != 0 {
        return current_rsp;
    }
    // preempt-in-critical の破壊確認では、**サボタージュが arm されている間だけ**この
    // 防御を bypass して、cli 落とし（IF=1 のまま）と併せてプリエンプトをクリティカル
    // 区間へ食い込ませる。arm 窓の外（デモ開始など）は通常どおり守るので startup
    // レースが起きない（かつては大域的に外していた。verification-coverage 参照）。
    #[cfg(feature = "task-preempt-in-critical")]
    if critical_nesting_depth() != 0 && !common::critical::sabotage_armed() {
        return current_rsp;
    }

    // set と store の窓（ワーカーが 15 GPR を保持している区間）でプリエンプト
    // したかを数える（条件1）。この回数が 0 なら統計的レジスタ検証は何も
    // 検証していない。
    // SAFETY: 読み取りのみ。ワーカー本体が rip 相対で書くフラグ。
    if unsafe { core::ptr::read_volatile(addr_of!(IN_GPR_WINDOW)) } != 0 {
        PREEMPT_IN_WINDOW.fetch_add(1, Ordering::Relaxed);
    }

    // 締切に達したらワーカーを走行不可にする。次の pick_next がメインを選ぶ。
    if scheduler::demo_active() && crate::idt::timer_ticks() >= scheduler::demo_deadline() {
        for w in 0..WORKER_COUNT {
            // 締切で止めるだけで、ラウンドを終えたわけではない。
            scheduler::set_state(1 + w, TaskState::Blocked);
        }
        scheduler::set_demo_active(false);
    }

    schedule_switch(current_rsp)
}

/// スイッチの中核（yield と timer が共有）。現タスクの RSP を保存し、次タスクを
/// 選び、RSP0 を更新して次タスクの RSP を返す。次が現タスクと同じなら何もしない。
fn schedule_switch(current_rsp: u64) -> u64 {
    let current = current_index();
    scheduler::set_saved_rsp(current, current_rsp);

    // 破壊確認 (ii): RSP の差し替えを省く。現タスクの RSP を返すのでスイッチが
    // 起きず、同じタスクが回り続ける。デモの会計・進捗で検出する。
    #[cfg(feature = "task-switch-no-swap")]
    {
        return current_rsp;
    }

    #[cfg(not(feature = "task-switch-no-swap"))]
    {
        let next = pick_next(scheduler::states(), current);
        // 走らせるべき相手がいない（=現タスクのまま）なら何もしない。デモ後の
        // ハートビート区間（走行可能なワーカーが無い）ではここに来て no-op になる。
        if next == current {
            return current_rsp;
        }

        // スタックが混ざっていないこと。次タスクの保存 RSP がそのタスクの
        // スタック範囲内にあること（範囲外なら別タスクのスタックを指している）。
        let next_rsp = scheduler::saved_rsp(next);
        let next_bottom = scheduler::stack_bottom(next);
        let next_top = scheduler::stack_top(next);
        if next_rsp < next_bottom || next_rsp >= next_top {
            serial_line(format_args!(
                "[ERROR] task: task {next} saved_rsp {next_rsp:#x} is outside its stack \
                 [{:#x}, {:#x}); stacks are mixed; halting",
                next_bottom, next_top
            ));
            common::cpu::halt_forever();
        }

        set_current_index(next);
        scheduler::add_switch();
        scheduler::add_resume(next);

        // RSP0 を次タスクのスタック頂点へ更新する（§2.2、効くのは M5-e）。
        // 破壊確認: drop-rsp0 では更新を落とす。読み戻し検査で捕まる。
        let expected_rsp0 = next_top;
        #[cfg(not(feature = "task-switch-drop-rsp0"))]
        // SAFETY: stack_top は次タスクの有効なスタック頂点。切り替えの割り込み
        // 禁止区間から呼んでいる。
        unsafe {
            gdt::set_rsp0(expected_rsp0);
        }
        // **実際の状態を読む。** RSP0 は M5-e まで挙動に現れないので、間違った
        // 値が書かれても誰も気づかない。TSS から読み戻して期待値と一致する
        // ことをその場で確かめる（A-1 / M5-b と同じく実状態を見る）。drop-rsp0
        // では更新を落としているのでここで食い違い、halt する。
        let readback = gdt::privilege_stack_top();
        if readback != expected_rsp0 {
            serial_line(format_args!(
                "[ERROR] task: TSS.RSP0 readback {readback:#x} != expected {expected_rsp0:#x} \
                 after switch to task {next}; halting",
            ));
            common::cpu::halt_forever();
        }

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
fn pick_next(states: [TaskState; TASK_COUNT], current: usize) -> usize {
    for offset in 1..=WORKER_COUNT {
        let cand = if current == 0 {
            ((offset - 1) % WORKER_COUNT) + 1
        } else {
            ((current - 1 + offset) % WORKER_COUNT) + 1
        };
        if states[cand].is_runnable() {
            return cand;
        }
    }
    0
}

/// ワーカー本体（`global_asm!`）から呼ばれる。現タスクの GPR 基準値を返す。
extern "sysv64" fn current_task_base() -> u64 {
    scheduler::base(current_index())
}

/// ワーカー本体から呼ばれる。往復後の 15 本の GPR（`GPR_BUF`）を基準値と照合し、
/// 結果を出す。残りラウンドがあれば 1、無ければ 0 を返す。
extern "sysv64" fn verify_gprs_and_advance() -> u64 {
    let current = current_index();
    let base = scheduler::base(current);

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

    let remaining = scheduler::rounds_left(current).saturating_sub(1);
    scheduler::set_rounds_left(current, remaining);
    let round = ROUNDS_PER_WORKER - remaining;
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

    if remaining == 0 {
        0
    } else {
        1
    }
}

/// ワーカー本体から、全ラウンドを終えたときに呼ばれる。現タスクを終了扱いに
/// して yield する。以後スケジューラはこのタスクを選ばない。
extern "sysv64" fn worker_done_and_yield() {
    let current = current_index();
    scheduler::set_state(current, TaskState::Finished);
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

// ============================================================================
// M5-d: プリエンプティブ化（タイマからのスケジューリング）
// ============================================================================

/// プリエンプティブデモを回すティック数（100Hz なので 200 ≒ 2 秒）。
const PREEMPTIVE_DEMO_TICKS: u64 = 200;

extern "C" {
    /// プリエンプティブなワーカー本体（`global_asm!`）。yield を呼ばず、GPR に
    /// pattern を保持しながらビジーループする。timer が切り替える。
    static zaytos_preemptive_body: u8;
}

/// プリエンプティブデモを実行し、検証する（M5-d）。
///
/// timer が動いている状態（sti 済み）で呼ぶこと。2 本のビジーループワーカーを
/// 起こし、初回スイッチ（yield）でワーカーへ入る。以後 timer がワーカー間を
/// プリエンプトで回す。締切に達すると [`on_timer_tick`] がワーカーを走行不可に
/// してメインへ戻し、この関数が会計・進捗・レジスタ照合・窓カウントを検査して
/// 戻る。
pub fn run_preemptive_demo() {
    require_bootstrap_processor("the preemptive demo");
    // SAFETY: run_timer_loop の sti 直後、起動時の単一実行文脈から 1 回だけ
    // 呼ばれる。スケジューラは M5-c のデモが終わった状態。
    unsafe {
        setup_preemptive_tasks();
    }

    serial_line(format_args!(
        "task: starting preemptive demo with {WORKER_COUNT} busy-loop workers for \
         {PREEMPTIVE_DEMO_TICKS} ticks"
    ));

    // 初回スイッチ。メインがワーカー A へ入る。以後 timer がプリエンプトする。
    // 締切で on_timer_tick がここへ戻す。
    yield_now();

    // --- 会計・進捗・窓カウントを閉じる ---
    // ワーカーは走行不可だが**タイマは動き続けている**ので、`switches` と
    // `resumes` は IF=0 の経路が加算しうる。フィールド単位の volatile な
    // 読みで取る（S0-b。`scheduler` の表を参照）。
    let switches = scheduler::switches();
    let resume_sum: u64 = (0..TASK_COUNT).map(scheduler::resumes).sum();
    let a_iters = scheduler::iterations(1);
    let b_iters = scheduler::iterations(2);
    let window_preempts = PREEMPT_IN_WINDOW.load(Ordering::Relaxed);

    serial_line(format_args!(
        "task: preemptive demo finished. switches={switches}, sum(resumes)={resume_sum}, \
         A iterations={a_iters}, B iterations={b_iters}, \
         preempts in the GPR window={window_preempts}"
    ));

    // 進捗: 両ワーカーが何度も回った。
    let progress = a_iters > 0 && b_iters > 0;
    // 会計: 各スイッチが 1 タスクを再開したので合計が一致する（非決定的順序でも）。
    let accounting = switches == resume_sum;
    // 統計的レジスタ検証が実際に窓を捉えたこと（条件1）。捉えていなければ、
    // レジスタ照合は何も検証していない。
    let window_meaningful = window_preempts > 0;

    if !progress {
        serial_line(format_args!(
            "[ERROR] task: a worker made no progress (A={a_iters}, B={b_iters}); the timer did \
             not preempt fairly; halting"
        ));
        common::cpu::halt_forever();
    }
    if !accounting {
        serial_line(format_args!(
            "[ERROR] task: preemptive accounting did not balance (switches != sum(resumes)); halting"
        ));
        common::cpu::halt_forever();
    }
    if !window_meaningful {
        serial_line(format_args!(
            "[ERROR] task: no preemption landed in the GPR window; the register check verified \
             nothing (widen the window or run longer); halting"
        ));
        common::cpu::halt_forever();
    }

    serial_line(format_args!(
        "task: preemptive switch verified (progress, accounting, and {window_preempts} \
         register round-trips through preemption all held)"
    ));
}

/// プリエンプティブデモ用にスケジューラを組み直し、2 本のビジーループワーカーを
/// 起こす。
///
/// # Safety
///
/// timer が動いている状態で、起動時の単一実行文脈から 1 回だけ呼ぶこと。M5-c の
/// デモが終わっていること（ワーカースタックのガードページは M5-c で設置済み。
/// ここでは再設置しない）。
unsafe fn setup_preemptive_tasks() {
    let entry = addr_of!(zaytos_preemptive_body) as u64;
    let main_top = crate::stack::kernel_stack_range().top.as_u64();

    // SAFETY: 単一実行文脈。timer は IF=1 だが、この関数は yield する前に
    // 走り、スケジューラの current はメイン（0）のままである。ここでの更新中に
    // プリエンプトが起きても、current=メインで走行可能なワーカーがまだ無い間は
    // pick_next がメインを返すので no-op になる（順序の安全性は最初のワーカーを
    // 走行可能にした後に yield で入ることに依存する）。
    let _guard = common::critical::InterruptGuard::enter();
    set_current_index(0);
    scheduler::set_switches(0);
    scheduler::init_task(
        0,
        Task {
            stack_top: main_top,
            stack_bottom: crate::stack::kernel_stack_range().bottom.as_u64(),
            state: TaskState::Blocked,
            ..EMPTY_TASK
        },
    );
    for w in 0..WORKER_COUNT {
        let (guard, top) = worker_stack_bounds(w);
        // ガードページは M5-c で設置済み。ここでは偽コンテキストだけ作り直す。
        // SAFETY: top はガードページ済みのワーカースタックの頂点。M5-c のデモは
        // 終わっており、このスタックは今は誰も使っていない。
        let saved_rsp = unsafe { build_initial_context(top, entry) };
        let base = 0xA1A1_0000u64 + (w as u64) * 0x1111_0000;
        scheduler::init_task(
            1 + w,
            Task {
                saved_rsp,
                stack_top: top.as_u64(),
                stack_bottom: guard.as_u64() + GUARD_SIZE as u64,
                state: TaskState::Ready,
                base,
                rounds_left: 0,
                iterations: 0,
                resumes: 0,
            },
        );
    }
    scheduler::set_demo_active(true);
    scheduler::set_demo_deadline(crate::idt::timer_ticks() + PREEMPTIVE_DEMO_TICKS);
    PREEMPT_IN_WINDOW.store(0, Ordering::Relaxed);
    // _guard の drop でここを抜けると割り込みが復元される（元が IF=1 なら sti）。
}

/// プリエンプティブなワーカー本体から呼ばれる。往復（プリエンプト）後の 15 本の
/// GPR（`GPR_BUF`）を基準値と照合し、進捗カウンタを増やす。
extern "sysv64" fn verify_preemptive_gprs() {
    let current = current_index();
    let base = scheduler::base(current);

    // SAFETY: ワーカー本体が直前に 15 本を書き込んだ共有バッファ。
    let buf = unsafe { *addr_of!(GPR_BUF) };
    for (i, &tag) in GPR_TAGS.iter().enumerate() {
        if buf[i] != base.wrapping_add(tag) {
            let name = if current == 1 { 'A' } else { 'B' };
            serial_line(format_args!(
                "[ERROR] task: {name} GPR tag {tag} corrupted across a preemptive switch; \
                 got {:#x} expected {:#x}; halting",
                buf[i],
                base.wrapping_add(tag)
            ));
            common::cpu::halt_forever();
        }
    }
    scheduler::add_iteration(current);
}

/// プリエンプティブなワーカー本体のループ先頭から呼ばれる。現タスクの base を
/// 返す。**IF=1 の地点である。**
///
/// preempt-in-critical の破壊確認では、ここで共有ロックを保持したまま少し
/// スピンする。破壊ビルドでは InterruptGuard が cli を落とすので、保持中も IF=1 の
/// ままになり、timer がプリエンプトして別ワーカーが同じロックを取ろうとし、
/// 二重取得検出が発火する。正常ビルドではこの経路は cfg で消える。
extern "sysv64" fn preemptive_loop_top() -> u64 {
    #[cfg(feature = "task-preempt-in-critical")]
    {
        // サボタージュをこの保持窓の間だけ arm する（Drop で disarm）。arm 中だけ
        // Locked の cli が省かれ、on_timer_tick の防御スキップが bypass される。arm 窓の
        // 外＝デモ開始は正常な cli の下で走るので startup レースが起きない（かつては
        // 大域的に壊していた。verification-coverage 参照）。
        let _armed = common::critical::arm_sabotage();
        let mut held = DEMO_LOCK.lock();
        let current = current_index();
        *held = current as u64;
        // timer ティックが 1 つ跨ぐ程度スピンして、保持中のプリエンプトを誘う。arm 中
        // なので IF=1 のままで、この窓で timer が食い込み、別ワーカーが同じ DEMO_LOCK を
        // 取って二重取得検出が発火する。
        for _ in 0..2_000_000u64 {
            core::hint::spin_loop();
        }
        // 明示的にロックを解放してから、_armed が block 末で drop されて disarm する
        // （宣言の逆順なので必ずロック解放の後に disarm）。
        drop(held);
    }
    current_task_base()
}

// プリエンプティブなワーカー本体（アセンブリ）。yield を呼ばない。
//
// 各周回:
//   1. current_task_base() で自分の base を得る（rax）
//   2. 15 本の GPR へ base + tag を入れる
//   3. IN_GPR_WINDOW を 1 にする（rip 相対、レジスタを使わない）
//   4. NOP そりを挟んで窓を広げる（条件1: プリエンプトが窓に落ちる確率を上げ、
//      N > 0 を保証する。widen feature でそりを長くして N が増えることを確かめる）
//   5. 15 本を GPR_BUF へ書き出す
//   6. IN_GPR_WINDOW を 0 にする
//   7. verify_preemptive_gprs() で照合し進捗を数える
//   8. 無限に繰り返す（締切で on_timer_tick がこのワーカーを走行不可にして
//      スケジューラが選ばなくなることで止まる。自分では抜けない）
// timer がこのビジーループを任意の瞬間にプリエンプトし、切り替えが 15 本を
// 保存・復元する。set と store の間（IN_GPR_WINDOW=1）でプリエンプトした回が、
// 保存・復元の検査として意味を持つ。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zaytos_preemptive_body",
    "zaytos_preemptive_body:",
    "2:",
    // ループ先頭（IF=1、プリエンプト可）。base を得る。preempt-in-critical の
    // 破壊確認では、ここで DEMO_LOCK を保持したままスピンする（IF=1 なので
    // timer が食い込む。正常ビルドでは何もしない）。
    "  call {loop_top}",
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
    // 窓に入る。ここから cli までの間にプリエンプトすると、保存・復元の検査に
    // なる（15 本を保持したまま切り替わる）。
    "  mov byte ptr [rip + {window}], 1",
    // メモリカウンタの遅延ループで窓を広げる。dec/jnz はフラグしか使わず
    // （フラグは IrqContext の rflags で保存・復元される）、pattern の 15 本は
    // 触らない。カウンタはメモリなのでレジスタも使わない。コードは数命令で、
    // そりの長さが .text を膨らませない。
    "  mov qword ptr [rip + {delay}], {sled}",
    "4:",
    "  dec qword ptr [rip + {delay}]",
    "  jnz 4b",
    // **cli で store と照合を保護する。** GPR_BUF は A/B 共有なので、store の後
    // 照合の前にプリエンプトされると別ワーカーが上書きし、他タスクの値を読んで
    // しまう。cli してから store・照合すれば、その区間は別タスクが割り込めない。
    // 検査対象の窓（set から cli まで）は cli の前なのでプリエンプト可のまま。
    "  cli",
    "  mov byte ptr [rip + {window}], 0",
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
    "  sti",
    "  jmp 2b",
    loop_top = sym preemptive_loop_top,
    verify = sym verify_preemptive_gprs,
    buf = sym GPR_BUF,
    window = sym IN_GPR_WINDOW,
    delay = sym PREEMPT_DELAY,
    sled = const PREEMPT_WINDOW_SLED,
);

#[cfg(test)]
mod tests {
    use super::{pick_next, TaskState, TASK_COUNT, WORKER_COUNT};

    /// 旧 `runnable: bool` に対応する短縮。`true` = 走行可能。
    fn states(flags: [bool; TASK_COUNT]) -> [TaskState; TASK_COUNT] {
        let mut out = [TaskState::Uninitialized; TASK_COUNT];
        for (slot, flag) in out.iter_mut().zip(flags) {
            *slot = if flag {
                TaskState::Ready
            } else {
                TaskState::Blocked
            };
        }
        out
    }

    /// この表が前提にしている形。崩れたら下の期待値を引き直すこと。
    #[test]
    fn the_demo_has_two_workers_and_one_main() {
        assert_eq!(WORKER_COUNT, 2);
        assert_eq!(TASK_COUNT, 3);
    }

    #[test]
    fn main_is_chosen_when_no_worker_can_run() {
        assert_eq!(pick_next(states([false, false, false]), 0), 0);
        assert_eq!(pick_next(states([false, false, false]), 1), 0);
        assert_eq!(pick_next(states([false, false, false]), 2), 0);
    }

    /// **タスク 0（メイン）は候補として巡回されない。** 走行可能と印を付けても
    /// 選ばれるのは「他に誰もいないとき」の帰り先としてだけである。
    #[test]
    fn main_is_never_picked_as_a_rotation_candidate() {
        // メインだけが走行可能でも、返るのは 0（フォールバック経路）。
        assert_eq!(pick_next(states([true, false, false]), 1), 0);
    }

    #[test]
    fn from_main_the_first_runnable_worker_is_chosen() {
        assert_eq!(pick_next(states([false, true, true]), 0), 1);
        assert_eq!(pick_next(states([false, false, true]), 0), 2);
        assert_eq!(pick_next(states([false, true, false]), 0), 1);
    }

    /// ワーカーの間は巡回する（round-robin）。
    #[test]
    fn workers_rotate() {
        assert_eq!(pick_next(states([false, true, true]), 1), 2);
        assert_eq!(pick_next(states([false, true, true]), 2), 1);
    }

    /// **現タスクが再選択されうる。** 他に走れるワーカーがおらず自分だけが
    /// 走行可能なら、`pick_next` は現タスクを返す。呼び出し側
    /// （`schedule_switch`）が `next == current` を no-op として扱うことで
    /// 成立している契約なので、**状態機械化でもこの性質を保つこと。**
    #[test]
    fn the_current_worker_is_returned_when_it_is_the_only_runnable_one() {
        assert_eq!(pick_next(states([false, true, false]), 1), 1);
        assert_eq!(pick_next(states([false, false, true]), 2), 2);
    }

    /// 走行不可のワーカーは飛ばされる。
    #[test]
    fn an_unrunnable_worker_is_skipped() {
        assert_eq!(pick_next(states([false, false, true]), 1), 2);
        assert_eq!(pick_next(states([false, true, false]), 2), 1);
    }

    /// **`Ready` 以外はすべて選ばれない。** 状態を増やしたときに
    /// `is_runnable` の更新を忘れると、ここが落ちる。
    #[test]
    fn only_ready_is_runnable() {
        assert!(TaskState::Ready.is_runnable());
        assert!(!TaskState::Uninitialized.is_runnable());
        assert!(!TaskState::Blocked.is_runnable());
        assert!(!TaskState::Finished.is_runnable());
    }

    /// **`Uninitialized` が残っていても安全側に倒れる。** `static` の初期値が
    /// `pick_next` から観測されないことに依存していないことの確認である。
    #[test]
    fn uninitialized_slots_are_never_chosen() {
        let all_empty = [TaskState::Uninitialized; TASK_COUNT];
        assert_eq!(pick_next(all_empty, 0), 0);
        assert_eq!(pick_next(all_empty, 1), 0);
    }

    /// **`Blocked` と `Finished` は選択可否では区別されない。** 区別が要るのは
    /// 会計と記録であって、選択ではない（畳んでいた `false` を分けた目的）。
    #[test]
    fn blocked_and_finished_are_both_unselectable_but_distinct() {
        let mut with_blocked = [TaskState::Blocked; TASK_COUNT];
        with_blocked[1] = TaskState::Finished;
        assert_eq!(pick_next(with_blocked, 0), 0);
        assert_ne!(TaskState::Blocked, TaskState::Finished);
    }
}
