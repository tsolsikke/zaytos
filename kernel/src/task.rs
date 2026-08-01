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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use common::addr::VirtAddr;
use common::critical::critical_nesting_depth;
use common::percpu::{PerCpu, MAX_CPUS};
use common::serial::SerialPort;

use crate::gdt;
use crate::idt::YIELD_VECTOR;
use crate::paging::active::{ActivePageTable, PageSize};

/// ワーカータスクの本数（M5-c は 2 本）。
pub const WORKER_COUNT: usize = 2;

/// タスクの総数。メイン（0）+ ワーカー（`1..=WORKER_COUNT`）+ AP 用アイドル（末尾）。
///
/// **S4-c-2 で 1 つ増えた。** 担当コアを入れると、**AP にとっての「タスク 0」に
/// 相当するものが要る**（`pick_next` は走行可能な担当が無いときタスク 0 を返すが、
/// タスク 0 は bootstrap processor の担当である）。
const TASK_COUNT: usize = WORKER_COUNT + 2;

/// メインタスクの添字。**bootstrap processor の既定タスクでもある**（S4-c-3-1）。
const MAIN_TASK: usize = 0;

/// AP 用アイドルタスクの添字（S4-c-2）。**AP の既定タスクである**（S4-c-3-1）。
///
/// `pick_next` はワーカー（`1..=WORKER_COUNT`）しか巡回の候補にしないので、
/// **これが巡回で選ばれることはない。** [`MAIN_TASK`] と同じ扱いで、
/// **落ち先としてだけ選ばれる**（[`default_task_for`]）。
const AP_IDLE_TASK: usize = WORKER_COUNT + 1;

/// そのコアの既定タスク（アイドル）を返す（S4-c-3-1）。
///
/// # なぜ「候補ゼロなら 0」ではいけないのか
///
/// **固定の `0` は第 1 層も第 2 層も素通りする。** `0` は bootstrap processor の
/// 担当なので、AP がここへ落ちると**他コアのタスクを走らせる。** しかも
/// 落ち先は候補のフィルタを通らないので、**担当（第 1 層）も `CURRENT` 条件
/// （第 2 層）も参照されない。** さらに BSP がワーカーを走らせている間、
/// タスク 0 は誰の `CURRENT` にも入っていないので、**二重選択の検出器も鳴らない。**
/// **静かに壊れる形である。**
///
/// # 検査ではなく、選べない形にした
///
/// 落ち先を**コアごとに持たせる**ことで、「落ち先が自コアの担当であること」を
/// 定義から成り立たせる。**他コアのタスクへ落ちる経路が存在しない。**
/// 対応は下の表明（`the_fallback_of_every_core_is_a_task_that_core_owns`）が
/// 固定する。
///
/// **メインが bootstrap processor のアイドルである。** 専用のアイドルタスクを
/// もう 1 本足さないのは、タスク 0 が既にその役（走行可能な担当が無いときの
/// 落ち先）を果たしているためである。
///
/// # **これは `MAX_CPUS = 2` でしか成り立たない**
///
/// `else` が AP をコアで区別していないので、**`MAX_CPUS` が 3 以上になると
/// 複数の AP が同じアイドルタスクへ落ちる。** タスクが同じならスタックも同じ
/// なので、**「2 つ以上のコアが同一スタックを走る」が復活する。** しかも
/// **その経路は第 1 層も第 2 層も通らない**（上のとおり落ち先は候補ではない）
/// ので、**検出器も鳴らない。**
///
/// **`MAX_CPUS` を上げるなら、コアごとにアイドルタスクを持たせる作業が要る。**
/// [`AP_IDLE_TASK_OWNER`] の doc にも同じことが書いてある。
const fn default_task_for(cpu: usize) -> usize {
    if cpu == common::percpu::BOOTSTRAP_PROCESSOR_SLOT {
        MAIN_TASK
    } else {
        AP_IDLE_TASK
    }
}

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
    /// このタスクを走らせてよいコア（S4-c-1）。
    ///
    /// # なぜ静的な担当なのか
    ///
    /// **タスクのコア間移動を実装しないと決めてある**（ADR-0023 Addendum §5）。
    /// `GPR_BUF` の安全がそれに依存するためで、負荷分散を実装しないこととは
    /// 理由が別である。**動的な affinity は負荷分散へ踏み込むので採らない。**
    ///
    /// # これが第 1 層である
    ///
    /// 同じタスクが 2 コアから選ばれる危険に対する守りは 2 層あり、
    /// **こちらが先に防ぐ。** 第 2 層（候補が他コアの `CURRENT` に入っていない
    /// こと）は S4-c-3 で入れる予定で、**本番では発火条件が無い構造的な
    /// ガードになる。**
    ///
    /// **この段（S4-c-1）では全タスクが bootstrap processor の担当である。**
    /// したがって候補集合は今までと同じで、**振る舞いは変わらない。**
    owner: usize,
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
    // **既定は bootstrap processor。** S4-c-2 で新設する AP 用タスクだけが
    // これを上書きする。
    owner: common::percpu::BOOTSTRAP_PROCESSOR_SLOT,
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
static CURRENT: PerCpu<AtomicUsize> =
    PerCpu::new([const { AtomicUsize::new(NO_CURRENT_TASK) }; MAX_CPUS]);

/// 破壊確認から現在タスクを読む（S3-b-2b-2、`smp-ap-touch-scheduler-test`）。
///
/// **AP から呼ぶと sentinel を読んで停止するのが正しい。**
#[cfg(feature = "smp-ap-touch-scheduler-test")]
pub fn debug_read_current_index() -> usize {
    current_index()
}

/// [`CURRENT`] の「まだ誰も走らせていない」を表す値（S3-b-2b-2）。
///
/// # なぜ `0` を初期値にしないのか
///
/// `0` はメイン（[`TaskState::Blocked`]）である。**初期値を `0` にすると、
/// 「メインを走らせている」と「まだ何も決めていない」が同じ値になる。**
/// `MAX_CPUS > 1` では AP のスロットが `0` のまま残るので、**誤って読めば
/// 「タスク 0 が走っている」と静かに答える。**
///
/// **bootstrap processor も起動時に明示的に `0` を書く**（`setup_tasks`）。
/// これで **`CURRENT[0] == 0` が「既定値の 0」ではなく「メインを走らせている
/// という宣言」になる。読まれる値はすべて誰かが書いた値である。**
///
/// S3-a で `TaskState::Uninitialized` を足したのと同じ判断である。
const NO_CURRENT_TASK: usize = usize::MAX;

/// 自コアの現在タスクインデックスを読む（旧 `sched.current` の読みと同じ意味）。
///
/// IF=1 のワーカーからも呼ばれるので `Relaxed` のアトミック読みにする（上の
/// [`CURRENT`] のドキュメント参照）。
fn current_index() -> usize {
    let value = CURRENT.this_cpu().load(Ordering::Relaxed);
    if value == NO_CURRENT_TASK {
        // **このコアはまだタスクを割り当てられていない。** S3-b-2b-2 の段では
        // AP はタスクを実行しないので、ここへ来るのは「AP がスケジューラへ
        // 入った」ことを意味する。**丸めず、落とす。**
        serial_line(format_args!(
            "[ERROR] task: current_index() was read on a CPU with no current task              (CURRENT is still the sentinel); this stage does not run tasks on application              processors; halting"
        ));
        common::cpu::halt_forever();
    }
    value
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

/// コアごとの、スケジューラを通った回数（S4-c-3-2a）。
///
/// # なぜ「アイドルタスクが回った回数」を捨てたのか
///
/// S4-c-2 は AP 用アイドルタスクの専用ループ（`ap_idle_entry`）が回した回数を
/// 数えていた。**S4-c-3-2a でそのループを廃したので、数える対象が無くなった。**
///
/// **それ以上に、あの観測量では「参加した」を示せなかった。** 枝 1 では AP は
/// 文脈切り替えを 1 度も行わず、既存のハートビートのループがそのまま
/// アイドルタスクの本体になる。**そのループは早期リターンを残した構成でも
/// 同じように回る**ので、**「スケジューラへ参加した」と「従来どおりループして
/// いる」を区別できない。** 「同値である間は分類の誤りが観測できない」の形である。
///
/// **区別できる量に置き換えた。** これは [`schedule_switch`] を通った回数で、
/// **早期リターンが残っている間、AP のスロットは 0 のままである**（AP は
/// `irq_entry` で手前に戻るので `schedule_switch` へ到達しない）。
/// **0 であることを実測してから、次段で外す。**
static SCHEDULE_PASSES: PerCpu<AtomicU64> = PerCpu::new([const { AtomicU64::new(0) }; MAX_CPUS]);

/// AP（スロット 1）がスケジューラを通った回数（S4-c-3-2a）。**ハートビートが読む。**
///
/// # 「参加」の観測の定義
///
/// **2 回のハートビートの差が正であること**を「参加している」とする。
/// **「0 でないこと」では足りない**——一度だけ通って止まった形を通してしまう。
///
/// 折り返しは実用上起きない（`u64`）。**それでも差は `wrapping_sub` で取る。**
///
/// **この段では 0 でなければならない。** 早期リターンがあるので AP は
/// `schedule_switch` へ到達しない。**0 でなければ、外したつもりのない経路から
/// 入っている。**
pub fn ap_schedule_passes() -> u64 {
    SCHEDULE_PASSES
        .slot(AP_IDLE_TASK_OWNER)
        .map_or(0, |slot| slot.load(Ordering::Relaxed))
}

/// AP（スロット 1）が今どのタスクを走らせているか（S4-c-2）。
///
/// **「割り当てられた」の観測である。** 参加は [`ap_schedule_passes`] が示す。
/// sentinel のままなら「まだ何も割り当てられていない」。
///
/// **こちらは早期リターンの有無で変わる**（sentinel から添字へ動くのは
/// 次段で sentinel を解いたときである）ので、到達条件として有効である。
///
/// **これは表現を返す。** ログへ出すのは [`ap_current_display`] のほうである。
pub fn ap_current_index() -> usize {
    CURRENT
        .slot(AP_IDLE_TASK_OWNER)
        .map_or(NO_CURRENT_TASK, |slot| slot.load(Ordering::Relaxed))
}

/// sentinel をログへ出すときの綴り（S4-c-2）。
///
/// **表示と表現は別である。** 表現は [`NO_CURRENT_TASK`]（`usize::MAX`）のまま
/// 変えない。変えるのは出し方だけで、`18446744073709551615` という 20 桁が
/// ハートビートの 1 行を押し広げて目視で追いにくいことへの対処である。
///
/// **綴りを 1 箇所に置くのは、sentinel を出す箇所が増えたときに揃えるためである。**
/// 現時点で sentinel を**値として**出すのはハートビートだけで、
/// [`current_index`] は語（`the sentinel`）で書いていて値を出さない。
const NO_CURRENT_TASK_DISPLAY: &str = "none";

/// [`ap_current_index`] をログ向けに整形する（S4-c-2）。
///
/// sentinel なら [`NO_CURRENT_TASK_DISPLAY`]、それ以外は添字をそのまま出す。
pub struct ApCurrent(usize);

impl core::fmt::Display for ApCurrent {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == NO_CURRENT_TASK {
            formatter.write_str(NO_CURRENT_TASK_DISPLAY)
        } else {
            write!(formatter, "{}", self.0)
        }
    }
}

/// AP の現在タスクを、ハートビートへ出す形で返す（S4-c-2）。
pub fn ap_current_display() -> ApCurrent {
    ApCurrent(ap_current_index())
}

/// AP 用アイドルタスクを登録する（S4-c-2、形は S4-c-3-2a で作り替えた）。
///
/// # なぜ専用の本体とスタックを廃したのか
///
/// **S4-c-2 は専用のループ（`ap_idle_entry`）と専用のスタック（`AP_IDLE_STACK`、
/// 20,480 バイト）を用意し、初期コンテキストを組んで登録していた。**
/// 「登録するが誰も走らせない」段だったので、**走らせ方が決まる前に形を決めていた。**
///
/// **走らせ方を決めた時点で、その形では走らないと分かった。** AP の `CURRENT` へ
/// この添字を書くと、AP の最初のティックで [`schedule_switch`] は
/// 「現タスク = 次タスク」になり**切り替えを行わない。** したがって
/// **組んだ初期コンテキストは `set_saved_rsp` に上書きされ、`ap_idle_entry` へは
/// 永久に入らない。** 専用スタックも使われない。
///
/// **bootstrap processor と同じ形に揃えた。** あちらは起動コンテキストが
/// そのままタスク 0（メイン）であり、専用の本体もスタックも持たない。
/// **AP も同じで、`smp` の per-CPU スタックの上を走っているループが、
/// そのままこのタスクの本体である。**
///
/// # ガードページはどこへ行ったか
///
/// **失われていない。出所が変わった。** S4-c-2 は `AP_IDLE_STACK` の直下へ
/// `install_worker_guard_page` で穴を開けていた。per-CPU スタックには
/// **最初から張らない穴**が下にある（`smp::map_ap_stacks`）。**写像の不在で
/// 作ったガードなので、こちらのほうが解除の手数が少ない。**
///
/// # Safety
///
/// # 呼び出しの前提（**メモリ安全性の契約ではない**）
///
/// **S4-c-3-2a まで `unsafe fn` だった。外した。** 当時の `unsafe` は
/// `install_worker_guard_page` と `build_initial_context` を呼ぶためのもので、
/// **どちらも本関数から消えたので、守るべき義務が 1 つも残っていない。**
/// **義務の無い `unsafe fn` は「呼ぶ側に守るものがある」と誤って伝える。**
/// 積もると `unsafe` の印そのものが読み飛ばされるので、外す。
///
/// 前提は 2 つあるが、いずれも**正しさの前提であって、メモリ安全性の前提では
/// ない。**
///
/// - **`smp::prepare_ap_per_cpu` より後で呼ぶこと。** per-CPU スタックの範囲を
///   読むためである。**これは実行時に守られている**——まだなら
///   `ap_kernel_stack_range` が `None` を返し、本関数は理由を出して停止する。
/// - **起動時の単一実行文脈から 1 回だけ呼ぶこと。** スケジューラの静的領域へ
///   書くためである。**この書き込み自体は [`scheduler::init_task`] が安全な
///   関数として提供している**（添字を検査し、参照を作らずに書く）ので、
///   義務は本関数の呼び出し側ではなくあちらのモジュールにある。
pub fn init_ap_idle_task() {
    // **本当に走るスタックを記述する。** ここを嘘にすると、`schedule_switch` の
    // 「保存 RSP がそのタスクのスタック範囲内か」の検査が、**切り替えが起きた
    // ときにだけ**誤って落ちる（この段では切り替えが起きないので鳴らない）。
    //
    // **実際に保存される値がこの範囲へ入ることも確かめてある。** 保存されるのは
    // 割り込み入口の `rsp` なので、**タイマのベクタが IST を使うなら範囲の外へ
    // 出る。** `idt::init` が IST を割り当てるのは**ベクタ 8（#DF）と 14（#PF）
    // だけ**で、タイマは `None` である。Ring 0 から Ring 0 への割り込みでは
    // スタックが切り替わらないので、**入口の `rsp` はこの通常スタックの内側に
    // ある。** **IST を使うベクタが増えたら、この根拠は失効する。**
    let Some((bottom, top)) = crate::smp::ap_kernel_stack_range(AP_IDLE_TASK_OWNER) else {
        serial_line(format_args!(
            "[ERROR] task: the per-CPU stack for cpu {AP_IDLE_TASK_OWNER} is not mapped yet; \
             init_ap_idle_task must run after smp::prepare_ap_per_cpu; halting"
        ));
        common::cpu::halt_forever();
    };
    scheduler::init_task(
        AP_IDLE_TASK,
        Task {
            // **`saved_rsp` は 0 のままにしてある。** メイン（タスク 0）と同じ形で、
            // **このタスクは登録された時点で既に走っている。**
            //
            // **「読まれる前に必ず書かれる」は条件つきである（S4-c-4-2 で判明）。**
            // 成り立つのは**この AP の `CURRENT` がこのタスクのままである間**だけ
            // である。`schedule_switch` は `set_saved_rsp(current, ...)` を
            // `pick_next` より前に行うので、`current` がこのタスクなら確かに
            // 先に埋まる。**ところが `CURRENT` を外から別のタスクへ移されると、
            // このタスクは「切り替え先」になり、0 のままの `saved_rsp` が
            // 読まれる。** 実際に踏んだ——`smp-ap-runs-preemptive-demo` +
            // `sched-ignore-bootstrap-tripwire` では `setup_preemptive_tasks` が
            // `set_current_index(0)` を呼ぶので AP の `CURRENT` が 0 になり、
            // **次の `pick_next` がこのタスクを選んだ時点で範囲検査が
            // 「saved_rsp 0x0 は範囲外」で停止する。**
            //
            // **停止するのは正しい。** 0 は本当にこのタスクのスタックの外である。
            // **ここで書いておくのは、「必ず書かれる」を無条件と読まないためである。**
            stack_top: top,
            stack_bottom: bottom,
            // **`Ready` にしておく。** `pick_next` が巡回の候補にしないので
            // 選ばれないが、落ち先としては選ばれる（`default_task_for`）。
            state: TaskState::Ready,
            // **担当は AP である。** これが第 1 層の実体で、
            // **AP がワーカーを選べない理由でもある。**
            owner: AP_IDLE_TASK_OWNER,
            ..EMPTY_TASK
        },
    );
    serial_line(format_args!(
        "task: registered the AP idle task as index {AP_IDLE_TASK} owned by cpu \
         {AP_IDLE_TASK_OWNER} on its per-CPU stack [{bottom:#x}, {top:#x}); the application \
         processor adopts it at bring-up and schedules on it from then on"
    ));
}

/// **このコアの `CURRENT` を、自分の既定タスクにする**（S4-c-3-2b）。
///
/// AP が本番の世界へ移った直後に 1 回だけ呼ぶ。**sentinel を解く唯一の箇所である。**
///
/// # 呼び出しの前提
///
/// **BKL を保持した状態で呼ぶこと**（`KernelEntry::ApBringUp`）。`CURRENT` は
/// 共有物で、書く時点で bootstrap processor が走っている。
///
/// **自コアのタイマを開ける前に呼ぶこと。** 開けた後だと、解く前にティックが
/// 来て [`current_index`] が sentinel を読んで停止しうる。
pub fn adopt_idle_task_on_this_cpu() {
    let cpu = common::percpu::cpu_id();
    set_current_index(default_task_for(cpu));
}

/// AP 用アイドルタスクの担当コア。**`MAX_CPUS = 2` の前提でスロット 1 である。**
///
/// **`MAX_CPUS` を上げるなら、AP ごとにアイドルタスクが要る。** そのときは
/// この定数では足りない。**上げる前にここを見ること。**
const AP_IDLE_TASK_OWNER: usize = 1;

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
    // 破壊 (S3-a, percpu-fake-nonzero-cpu-id): この tripwire が見る値だけを偽る。
    //
    // **`cpu_id()` そのものを偽る形は S3-b-2a で使えなくなった。** `cpu_id()` が
    // GDTR 由来になったので、「`cpu_id()` は 1 と言うが GDTR はスロット 0 を
    // 指している」は**本物の不整合**であり、`gdt::init` の読み戻しが**この
    // tripwire より前に**捕まえて停止する。**より基本的な検査が先に働く。**
    //
    // **したがって破壊は tripwire が読む値に限定する。** そうしないと、
    // 「tripwire の分岐が働くこと」ではなく「GDT の読み戻しが働くこと」を
    // 確かめてしまう。**何を確かめたいかで破壊の位置が決まる。**
    #[cfg(feature = "percpu-fake-nonzero-cpu-id")]
    let cpu = 1usize;
    #[cfg(not(feature = "percpu-fake-nonzero-cpu-id"))]
    let cpu = common::percpu::cpu_id();
    // 破壊 (S4-c-4-2, sched-ignore-bootstrap-tripwire): **この見張りを外す。**
    //
    // **単独では意味を持たない。** `smp-ap-runs-preemptive-demo` と組んで初めて
    // 「AP がデモを実際に走らせる」形になり、そこで**二重選択の窓が生まれる。**
    // S4-c-4-1 は逆に**この見張りが在ること**を要求するので、
    // **同じ起動では両立しない。**
    #[cfg(feature = "sched-ignore-bootstrap-tripwire")]
    let _ = cpu;
    #[cfg(not(feature = "sched-ignore-bootstrap-tripwire"))]
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
                // **BSP のワーカーである。** `GPR_BUF` に触るので AP へ渡さない
                // （ADR-0023 Addendum §5。タスクのコア間移動を実装しない）。
                owner: common::percpu::BOOTSTRAP_PROCESSOR_SLOT,
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
    // **このコアがスケジューラを通った回数**（S4-c-3-2a）。`current_index()` より
    // 前で数える。あちらは sentinel を読むと停止するので、後ろに置くと
    // **「入ったが数えられていない」**が生じる（`smp-ap-no-sentinel-clear` の
    // 破壊はまさにその形で止まる）。
    SCHEDULE_PASSES.this_cpu().fetch_add(1, Ordering::Relaxed);
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
        let cpu = common::percpu::cpu_id();
        // **`CURRENT` を読むのはここ 1 回だけである。** 同じスナップショットを
        // フィルタ（[`pick_next`]）と検出器（[`report_double_selection`]）の
        // 両方へ渡す。
        //
        // **読み直さない理由は、検出器の根拠を明確にするためである。** 別々に
        // 読むと、フィルタが見た状態と検出器が見た状態が違いうる。そうなると
        // 「フィルタが通したのに検出器が鳴った」が**守りの破れなのか読んだ時点の
        // ずれなのか**を区別できない。**1 回の読みから両方を導けば、その曖昧さが
        // 構造的に無くなる。**
        //
        // BKL の内側なので実害は無いはずだが、**「無いはず」に依らない形にして
        // ある**（借りている保証を減らす）。
        let currents = current_indices();
        // `owners` も 1 回だけ読み、`pick_next` と下の観測の両方へ渡す
        // （`currents` と同じ理由。上のコメント）。
        let owners = scheduler::owners();
        let next = pick_next(scheduler::states(), owners, currents, cpu, current);
        // **第 1 層の実証（S4-c-4-2）。** 自コアが担当していないタスクを選んだら
        // 1 度だけ出す。**本番では鳴らない**——第 1 層が候補から外し、落ち先も
        // 定義上自コアの担当だからである（`default_task_for`）。
        //
        // **検出器とは別の事象を見ている。** あちらは「他コアが今走らせている
        // タスクを選んだ」、こちらは「自分のものでないタスクを選んだ」である。
        // **前者は後者を含むが、逆は含まない**——他コアがまだ走らせていない
        // よそのタスクを選ぶ形は、こちらだけが捉える。
        report_foreign_task_adoption(next, cpu, &owners);
        // **検出器（S4-c-3-2b）。** 2 層とも迂回されたときだけ鳴る。
        //
        // **BKL の内側である**——`irq_entry` が入口で取っており、ここはその中
        // である。**BKL の外の行は判定に使えない**（S4-b-4 でバイト混線の実物を
        // 観測している）。
        //
        // **フィルタより後に置く。** フィルタが効いていればここは通らないので、
        // 鳴ったこと自体が「フィルタが通さなかったはずのものが通った」を意味する。
        report_double_selection(next, cpu, &currents);
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
/// **あるタスクが、自分以外のコアの `CURRENT` に入っているか**（S4-c-3-2b）。
///
/// 第 2 層のフィルタと、二重選択の検出器が**共有する述語である。**
///
/// # **この述語自体には `cfg` を付けない**
///
/// 破壊 `sched-ignore-current` が無効にするのは**フィルタでの参照だけ**で、
/// **検出器の参照は生かす。** 述語ごと `cfg` で消すと**検出器も一緒に死に、
/// 2 層とも壊した構成で主マーカーが出なくなる。** 壊したい対象は「フィルタが
/// 見ること」であって「見る手段が在ること」ではない。
///
/// # 限界
///
/// **フィルタと検出器は同じ出所（`CURRENT`）から両辺を導く。** したがって
/// **この述語自体の誤りは検出できない**（`currents` の読み方が間違っていれば、
/// フィルタも検出器も同じように間違う）。**検出できるのは「2 層とも迂回された
/// こと」だけである。** `docs/verification-coverage.md` の「同一の出所から
/// 両辺を導く検査はその出所の誤りを検出できない」と同じ型である。
fn is_running_on_another_cpu(task: usize, cpu: usize, currents: &[usize; MAX_CPUS]) -> bool {
    currents
        .iter()
        .enumerate()
        .any(|(other, &running)| other != cpu && running == task)
}

/// 全コアの `CURRENT` を読む（S4-c-3-2b）。**第 2 層と検出器の入力である。**
fn current_indices() -> [usize; MAX_CPUS] {
    let mut out = [NO_CURRENT_TASK; MAX_CPUS];
    for (cpu, slot) in out.iter_mut().enumerate() {
        if let Some(current) = CURRENT.slot(cpu) {
            *slot = current.load(Ordering::Relaxed);
        }
    }
    out
}

/// 担当外のタスクを選んだことを既に報告したか。**系全体で 1 度だけ出す。**
static FOREIGN_ADOPTION_REPORTED: AtomicBool = AtomicBool::new(false);

/// **自コアが担当していないタスクを選んだことを、1 度だけ報告する**（S4-c-4-2）。
///
/// # なぜ検出器と別に要るのか
///
/// **「窓があること」を運に依らず観測するためである。** 二重選択の検出器は
/// 「他コアが**今**走らせているタスクを選んだ」ときにしか鳴らないので、
/// **第 1 層だけを外した構成（第 2 層が防ぐ）では鳴らない。** そこで
/// 「AP がワーカーを走らせられたのか、そもそも走らせていないのか」を
/// 区別する手段が無くなる。**区別できないと、対照が
/// 「窓が無いから鳴らない」に戻る**（S4-c-3-2b で実際に踏んだ形）。
///
/// # **ハートビートの標本抽出に依存しない**
///
/// `ap_current` は 1 秒ごとのスナップショットなので、**短時間だけワーカーを
/// 走らせた場合に取り逃す。** こちらは**起きた瞬間に 1 度だけ行を出す**ので、
/// **観測が運に依存しない。**
///
/// **本番では鳴らない。** 第 1 層が候補から外し、落ち先も定義上自コアの担当で
/// ある。**発火条件が無いことに意味があるので、本番ビルドに置く。**
#[inline(never)]
fn report_foreign_task_adoption(next: usize, cpu: usize, owners: &[usize; TASK_COUNT]) {
    let owner = owners[next];
    if owner == cpu {
        return;
    }
    if FOREIGN_ADOPTION_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    serial_line(format_args!(
        "[ERROR] task: cpu {cpu} selected task {next}, which is owned by cpu {owner}; the \
         first guard layer did not keep it out"
    ));
}

/// 二重選択の検出器が既に鳴ったか。**系全体で 1 度だけ出す。**
static DOUBLE_SELECTION_REPORTED: AtomicBool = AtomicBool::new(false);

/// **同じタスクが 2 つのコアから選ばれたことを、1 度だけ報告する**（S4-c-3-2b）。
///
/// # 本番ビルドにも在る
///
/// **発火条件が無いことに意味がある。** 守りが 2 層とも効いている限りここは
/// 鳴らないので、**「鳴らないこと」が主張になる。** そのためには**コードが
/// 在ること**が要る。「本番で出ない」と「コードが無い」を区別できるように、
/// `cargo xtask check` が**既定ビルドのバイナリにこのシンボルが在ること**を
/// 見る（`detector-symbol-present`）。
///
/// **主たる論拠は構造の側にある。** 破壊 `sched-ignore-current` が触るのは
/// [`pick_next`] のフィルタでの参照だけで、**ここの呼び出しに `cfg` は付かない。**
/// よって検出器は構成によらず全ビルドに在る。**シンボル検査はその裏取りである。**
///
/// **シンボル検査の限界も書いておく。** 見えるのは「呼ばれうる位置に在る」まで
/// で、**「正しい位置で呼ばれる」ことは示さない。** それを示すのは
/// 2 層とも壊した構成（`sched-ignore-owner` + `sched-ignore-current`）で
/// 実際に鳴ることのほうである。
#[inline(never)]
fn report_double_selection(next: usize, cpu: usize, currents: &[usize; MAX_CPUS]) {
    if !is_running_on_another_cpu(next, cpu, currents) {
        return;
    }
    if DOUBLE_SELECTION_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    serial_line(format_args!(
        "[ERROR] task: double selection detected: cpu {cpu} picked task {next}, which is \
         already the current task of another cpu (currents={currents:?}); both guard layers \
         were bypassed"
    ));
}

fn pick_next(
    states: [TaskState; TASK_COUNT],
    owners: [usize; TASK_COUNT],
    currents: [usize; MAX_CPUS],
    cpu: usize,
    current: usize,
) -> usize {
    for offset in 1..=WORKER_COUNT {
        let cand = if current == 0 {
            ((offset - 1) % WORKER_COUNT) + 1
        } else {
            ((current - 1 + offset) % WORKER_COUNT) + 1
        };
        // **第 1 層: 自コアが担当のタスクだけを候補にする（S4-c-1）。**
        //
        // 破壊 (S4-c-3-2b, sched-ignore-owner): この層だけを外す。**それだけでは
        // 二重選択は起きない**——第 2 層が防ぐ。**第 2 層が働いていることの
        // 実証がこの破壊の役目である。**
        #[cfg(not(feature = "sched-ignore-owner"))]
        if owners[cand] != cpu {
            continue;
        }
        // **第 2 層: 他コアが今走らせているタスクは選ばない（S4-c-3-2b）。**
        //
        // **本番では発火条件が無い。** 担当が互いに素なので、自コア担当のタスクが
        // 他コアの `CURRENT` に入ることがない。**発火条件が無いことに意味がある**
        // ので、本番ビルドにも置く。
        //
        // 破壊 (S4-c-3-2b, sched-ignore-current): **ここの参照だけを外す。**
        // 述語も検出器もそのまま残る（[`is_running_on_another_cpu`] の doc）。
        #[cfg(not(feature = "sched-ignore-current"))]
        if is_running_on_another_cpu(cand, cpu, &currents) {
            continue;
        }
        if states[cand].is_runnable() {
            return cand;
        }
    }
    // 走行可能な担当ワーカーが無いので、**自コアの既定タスクへ落ちる**（S4-c-3-1）。
    //
    // **固定の `0` から変えた。** `0` は bootstrap processor の担当なので、
    // AP がここへ落ちると他コアのタスクを走らせる。**しかもこの経路は候補の
    // フィルタを通らないので、第 1 層も第 2 層も参照されず、検出器も鳴らない**
    // （[`default_task_for`] の doc）。**落ち先をコアごとに持たせて、他コアの
    // タスクへ落ちる経路そのものを無くしてある。**
    //
    // **bootstrap processor から見た振る舞いは変わらない**（`default_task_for(0)`
    // は `MAIN_TASK` = 0）。既存の表明の期待値は 1 つも動いていない。
    default_task_for(cpu)
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
                // **BSP のワーカーである。** `GPR_BUF` に触るので AP へ渡さない
                // （ADR-0023 Addendum §5。タスクのコア間移動を実装しない）。
                owner: common::percpu::BOOTSTRAP_PROCESSOR_SLOT,
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
    use super::{TaskState, AP_IDLE_TASK, TASK_COUNT, WORKER_COUNT};

    /// 全タスクを bootstrap processor 担当に置いた構成。
    ///
    /// # **S4-c-2 以降、これは本番の実態ではない**
    ///
    /// S4-c-1 の時点では全タスクが bootstrap processor 担当だったので、
    /// これは実態そのものだった。**S4-c-2 で AP 用アイドルタスク（添字
    /// [`AP_IDLE_TASK`]）の担当が AP になったので、その一致は失われている。**
    /// **本番では起こらない構成になった**ということである。
    ///
    /// **それでも残す。** 下の [`pick_next`] を通る既存の表明は「担当が全部
    /// 自コアなら、担当コアを入れる前と同じに振る舞う」ことを言っており、
    /// **その主張自体は本番と一致するかどうかに依らない。** ただし
    /// **一致していると読まれると困る**ので、一致が切れたことを書いておく。
    const ALL_BSP: [usize; TASK_COUNT] = [common::percpu::BOOTSTRAP_PROCESSOR_SLOT; TASK_COUNT];

    /// どのコアも何も走らせていない `CURRENT`（S4-c-3-2b）。
    ///
    /// **第 2 層が何も弾かない入力である。** 既存の表明はすべてこれを通すので、
    /// **第 2 層を足しても期待値が 1 つも動かない。**
    const NOBODY_RUNNING: [usize; common::percpu::MAX_CPUS] =
        [super::NO_CURRENT_TASK; common::percpu::MAX_CPUS];

    /// **第 2 層の述語は、自コアを数えない（S4-c-3-2b）。**
    ///
    /// 自分の `CURRENT` に入っているのは当たり前なので、そこで弾くと
    /// **現タスクを選び直せなくなる**（`pick_next` が現タスクを返しうるという
    /// 既存の契約が壊れる）。
    #[test]
    fn the_layer_two_predicate_ignores_the_calling_cpu() {
        let mut currents = NOBODY_RUNNING;
        currents[0] = 1;
        // bootstrap processor 自身がタスク 1 を走らせている。自分は数えない。
        assert!(!super::is_running_on_another_cpu(1, 0, &currents));
        // AP から見ると、タスク 1 は他コアが走らせている。
        assert!(super::is_running_on_another_cpu(1, 1, &currents));
        // 誰も走らせていないタスクは、どちらから見ても弾かれない。
        assert!(!super::is_running_on_another_cpu(2, 0, &currents));
        assert!(!super::is_running_on_another_cpu(2, 1, &currents));
    }

    /// **sentinel は「走っている」に数えない（S4-c-3-2b）。**
    ///
    /// sentinel は `usize::MAX` で、**どのタスクの添字とも一致しない。**
    /// 一致してしまうと、まだ何も割り当てていないコアが、**全タスクを
    /// 「他コアが走らせている」ことにしてしまう。**
    #[test]
    fn the_sentinel_is_not_treated_as_a_running_task() {
        for task in 0..TASK_COUNT {
            assert!(!super::is_running_on_another_cpu(task, 0, &NOBODY_RUNNING));
        }
    }

    /// **第 2 層は、他コアが走らせているタスクを候補から外す（S4-c-3-2b）。**
    ///
    /// **本番では発火条件が無い**（担当が互いに素）ので、担当を意図的に
    /// そろえて第 2 層だけを働かせる。**`sched-ignore-owner` を入れた構成が
    /// これにあたる。**
    #[test]
    fn layer_two_skips_a_task_that_another_cpu_is_running() {
        let all_ready = states([true, true, true, true]);
        // 全部 bootstrap processor 担当（= 第 1 層が何も弾かない構成）。
        let mut currents = NOBODY_RUNNING;

        // AP がワーカー 1 を走らせている。bootstrap processor はワーカー 2 を選ぶ。
        currents[1] = 1;
        assert_eq!(super::pick_next(all_ready, ALL_BSP, currents, 0, 0), 2);

        // AP がワーカー 2 を走らせている。bootstrap processor はワーカー 1 を選ぶ。
        currents[1] = 2;
        assert_eq!(super::pick_next(all_ready, ALL_BSP, currents, 0, 0), 1);

        // **`MAX_CPUS = 2` では、塞がるワーカーは同時に 1 本までである。**
        // 他コアは 1 つで、1 コアは 1 タスクしか走らせないためで、
        // **「2 本とも塞がって落ち先へ行く」は現在の構成では作れない。**
        // `MAX_CPUS` を上げたらここに 1 件足せる。
    }

    /// 担当コアを既定（全部 BSP）にして bootstrap processor から呼ぶ短縮。
    ///
    /// **既存の契約を書き換えないための薄い包みである。** S4-c-1 は振る舞い
    /// 不変の段なので、**既存の表明はそのまま残し、担当コアつきの表明を足す。**
    ///
    /// # **この包みを通る表明が拘束する範囲は狭い**
    ///
    /// 包みは担当を全部 bootstrap processor に、呼び出しコアを bootstrap
    /// processor に固定する。**したがってこれらの表明が拘束するのは
    /// 「全タスクが BSP 担当で、BSP から呼んだとき」の契約だけである。**
    ///
    /// **S4-c-2 以降、この固定は本番の担当割りとも一致しない**（[`ALL_BSP`] の
    /// doc）。**「包みを通る表明が緑」から本番について言えることは、さらに狭まった。**
    ///
    /// **担当が混ざる場合や AP から呼ぶ場合は覆っていない。** そちらは
    /// `super::pick_next` を直に呼ぶ表明（`a_task_owned_by_another_cpu_is_not_a_candidate`
    /// と `only_the_tasks_owned_by_this_cpu_are_rotated`）が別に持つ。
    /// **包みを通る表明が全部緑でも、担当コアの振る舞いは何も言えない。**
    fn pick_next(states: [TaskState; TASK_COUNT], current: usize) -> usize {
        super::pick_next(
            states,
            ALL_BSP,
            NOBODY_RUNNING,
            common::percpu::BOOTSTRAP_PROCESSOR_SLOT,
            current,
        )
    }

    /// 本番の担当割り（S4-c-2 以降）。メイン + ワーカーが BSP、AP 用アイドルが AP。
    ///
    /// **[`ALL_BSP`] と違い、これは本番と一致する。** 下の 2 本はこちらを使う。
    const PRODUCTION_OWNERS: [usize; TASK_COUNT] = {
        let mut owners = [common::percpu::BOOTSTRAP_PROCESSOR_SLOT; TASK_COUNT];
        owners[AP_IDLE_TASK] = super::AP_IDLE_TASK_OWNER;
        owners
    };

    /// **どのコアの落ち先も、そのコアが担当しているタスクである（S4-c-3-1）。**
    ///
    /// これが `default_task_for` の存在理由そのものである。**固定の `0` だと
    /// AP の落ち先が他コアの担当になり、しかもその経路は第 1 層も第 2 層も
    /// 通らないので、検出器も鳴らずに静かに壊れる。**
    ///
    /// # **`MAX_CPUS` を上げるとこの表明は落ちる。それが正しい**
    ///
    /// `0..MAX_CPUS` を回しているので、**`MAX_CPUS` を 3 以上にした瞬間に
    /// ここが落ちる。** `default_task_for` は AP をコアで区別しておらず、
    /// 2 つ目以降の AP も `AP_IDLE_TASK` へ落ちるためである（同じタスク =
    /// 同じスタックなので、**複数コアが同一スタックを走る**）。
    ///
    /// **落ちるのは退行ではなく、この表明が仕事をしたということである。**
    /// **通すために表明のほうを弱めないこと**——`0..MAX_CPUS` を
    /// `0..2` に狭めたり、AP 側を除外したりすると、**危険がそのまま残って
    /// 検査だけが緑になる。** 正しい直し方は
    /// **コアごとにアイドルタスクを持たせること**で、それは `MAX_CPUS` を
    /// 上げる作業に含まれる（`docs/deferred-decisions.md` の当該項目）。
    #[test]
    fn the_fallback_of_every_core_is_a_task_that_core_owns() {
        for cpu in 0..common::percpu::MAX_CPUS {
            let fallback = super::default_task_for(cpu);
            assert_eq!(
                PRODUCTION_OWNERS[fallback], cpu,
                "cpu {cpu} falls back to task {fallback}, which it does not own"
            );
        }
    }

    /// **AP は自分のアイドルタスクへ落ちる。メインへは落ちない（S4-c-3-1）。**
    ///
    /// 走行可能な担当ワーカーが無い AP を `pick_next` に通す。**`MAIN_TASK` が
    /// 返ったら、AP が bootstrap processor のタスクを走らせることになる。**
    #[test]
    fn an_application_processor_falls_back_to_its_own_idle_task() {
        let all_ready = states([true, true, true, true]);
        let ap = super::AP_IDLE_TASK_OWNER;
        // ワーカーは 2 本とも BSP 担当なので、AP から見た候補は 0 本である。
        assert_eq!(
            super::pick_next(all_ready, PRODUCTION_OWNERS, NOBODY_RUNNING, ap, 0),
            AP_IDLE_TASK
        );
        assert_eq!(
            super::pick_next(
                all_ready,
                PRODUCTION_OWNERS,
                NOBODY_RUNNING,
                ap,
                AP_IDLE_TASK
            ),
            AP_IDLE_TASK
        );
        // ワーカーが全部走行不可でも同じ落ち先である。
        let none_ready = states([false, false, false, true]);
        assert_eq!(
            super::pick_next(none_ready, PRODUCTION_OWNERS, NOBODY_RUNNING, ap, 0),
            AP_IDLE_TASK
        );
        // **bootstrap processor 側は従来どおりメインへ落ちる。**
        assert_eq!(
            super::pick_next(none_ready, PRODUCTION_OWNERS, NOBODY_RUNNING, 0, 0),
            super::MAIN_TASK
        );
    }

    /// **表示を変えても表現は変わらない（S4-c-2）。**
    ///
    /// sentinel は `usize::MAX` のままで、出し方だけが `none` になる。
    /// **この 2 つが一緒に動いてしまうと、`CURRENT` を読む側の契約が変わる。**
    #[test]
    fn the_sentinel_is_displayed_as_a_word_but_still_stored_as_usize_max() {
        use super::{ApCurrent, NO_CURRENT_TASK, NO_CURRENT_TASK_DISPLAY};

        assert_eq!(NO_CURRENT_TASK, usize::MAX);
        assert_eq!(
            format!("{}", ApCurrent(NO_CURRENT_TASK)),
            NO_CURRENT_TASK_DISPLAY
        );
        // sentinel 以外は添字がそのまま出る。**`none` に丸めない。**
        assert_eq!(format!("{}", ApCurrent(AP_IDLE_TASK)), "3");
        assert_eq!(format!("{}", ApCurrent(0)), "0");
    }

    /// **担当コアが違うタスクは候補にならない（S4-c-1）。**
    ///
    /// 第 1 層そのものの表明である。全員走行可能でも、担当が別コアなら
    /// 選ばれず、**走行可能な担当が無いときの既存の経路（タスク 0）へ落ちる。**
    #[test]
    fn a_task_owned_by_another_cpu_is_not_a_candidate() {
        let all_ready = states([true, true, true, true]);
        // 全部 AP 担当にすると、bootstrap processor から見て候補が無い。
        let all_ap = [1usize; TASK_COUNT];
        assert_eq!(super::pick_next(all_ready, all_ap, NOBODY_RUNNING, 0, 0), 0);
        assert_eq!(super::pick_next(all_ready, all_ap, NOBODY_RUNNING, 0, 1), 0);
        // 逆に、AP から見れば選べる。
        assert_eq!(super::pick_next(all_ready, all_ap, NOBODY_RUNNING, 1, 0), 1);
    }

    /// **担当が混ざっていても、自コアのぶんだけを回す（S4-c-1）。**
    #[test]
    fn only_the_tasks_owned_by_this_cpu_are_rotated() {
        let all_ready = states([true, true, true, true]);
        // ワーカー 1 = BSP、ワーカー 2 = AP。
        let mixed = [0usize, 0, 1, 1];
        // BSP はワーカー 1 しか選べない。**現タスクが 1 でも 1 を返す**
        // （`pick_next` は現タスクを返しうるという既存の契約）。
        assert_eq!(super::pick_next(all_ready, mixed, NOBODY_RUNNING, 0, 0), 1);
        assert_eq!(super::pick_next(all_ready, mixed, NOBODY_RUNNING, 0, 1), 1);
        // AP はワーカー 2 しか選べない。
        assert_eq!(super::pick_next(all_ready, mixed, NOBODY_RUNNING, 1, 0), 2);
    }

    /// 旧 `runnable: bool` に対応する短縮。`true` = 走行可能。
    ///
    /// # **S4-c-2 で入力が 1 要素増えた**
    ///
    /// `TASK_COUNT` が 3 から 4 へ増えたので、既存の表明の入力も 1 つ伸びた。
    /// **足した要素は AP 用アイドルタスクで、値は本番と同じ `Ready` にしてある。**
    /// `pick_next` はワーカー（`1..=WORKER_COUNT`）しか候補にしないので結果は
    /// 変わらないが、**既存の表明はすべて「AP 用アイドルタスクが選ばれないこと」も
    /// 同時に主張するようになった。** 入力が変わったことを書いておく。
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
    ///
    /// # **S4-c-2 で実際に崩れ、この表明が止めた**
    ///
    /// `TASK_COUNT` を 3 から 4 へ増やしたとき（AP 用アイドルタスクの新設）、
    /// **この表明が落ちて期待値の引き直しを促した。** 引き直した内容は
    /// [`states`] の doc に書いてある（入力に 1 要素増え、値は本番と同じ `Ready`）。
    /// **「崩れたら引き直せ」と書いておいた表明が、実際にその役目を果たした。**
    #[test]
    fn the_demo_has_two_workers_and_one_main() {
        assert_eq!(WORKER_COUNT, 2);
        // メイン（0）+ ワーカー 2 + AP 用アイドル 1。
        assert_eq!(TASK_COUNT, 4);
        // **AP 用アイドルはワーカーの後ろに置く。** `pick_next` の走査範囲
        // （`1..=WORKER_COUNT`）の外であることが、選ばれない理由である。
        assert_eq!(AP_IDLE_TASK, WORKER_COUNT + 1);
        assert!(AP_IDLE_TASK > WORKER_COUNT);
    }

    #[test]
    fn main_is_chosen_when_no_worker_can_run() {
        assert_eq!(pick_next(states([false, false, false, true]), 0), 0);
        assert_eq!(pick_next(states([false, false, false, true]), 1), 0);
        assert_eq!(pick_next(states([false, false, false, true]), 2), 0);
    }

    /// **タスク 0（メイン）は候補として巡回されない。** 走行可能と印を付けても
    /// 選ばれるのは「他に誰もいないとき」の帰り先としてだけである。
    #[test]
    fn main_is_never_picked_as_a_rotation_candidate() {
        // メインだけが走行可能でも、返るのは 0（フォールバック経路）。
        assert_eq!(pick_next(states([true, false, false, true]), 1), 0);
    }

    #[test]
    fn from_main_the_first_runnable_worker_is_chosen() {
        assert_eq!(pick_next(states([false, true, true, true]), 0), 1);
        assert_eq!(pick_next(states([false, false, true, true]), 0), 2);
        assert_eq!(pick_next(states([false, true, false, true]), 0), 1);
    }

    /// ワーカーの間は巡回する（round-robin）。
    #[test]
    fn workers_rotate() {
        assert_eq!(pick_next(states([false, true, true, true]), 1), 2);
        assert_eq!(pick_next(states([false, true, true, true]), 2), 1);
    }

    /// **現タスクが再選択されうる。** 他に走れるワーカーがおらず自分だけが
    /// 走行可能なら、`pick_next` は現タスクを返す。呼び出し側
    /// （`schedule_switch`）が `next == current` を no-op として扱うことで
    /// 成立している契約なので、**状態機械化でもこの性質を保つこと。**
    #[test]
    fn the_current_worker_is_returned_when_it_is_the_only_runnable_one() {
        assert_eq!(pick_next(states([false, true, false, true]), 1), 1);
        assert_eq!(pick_next(states([false, false, true, true]), 2), 2);
    }

    /// 走行不可のワーカーは飛ばされる。
    #[test]
    fn an_unrunnable_worker_is_skipped() {
        assert_eq!(pick_next(states([false, false, true, true]), 1), 2);
        assert_eq!(pick_next(states([false, true, false, true]), 2), 1);
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
