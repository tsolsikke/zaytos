//! スケジューラの実体（`static mut SCHEDULER`）と、そのフィールド単位のアクセサ。
//!
//! # なぜ別モジュールなのか（S0-b）
//!
//! かつて `task.rs` は `&mut *(addr_of!(SCHEDULER) as *mut Scheduler)` で
//! **構造体全体への `&mut`** を作っていた。触る文脈が 3 つあり、そのうち
//! **IF=0 の割り込みハンドラと IF=1 のワーカーコールバックは同一コア上で
//! 本当に並行する**（プリエンプティブデモはタイマ稼働後に走る）。2 つの
//! `&mut` が同時に生きるのは、触るフィールドが別でも Rust の別名規則違反で
//! あり、`MAX_CPUS > 1` を待たずに**現在も未定義動作**である。
//!
//! ここへ実体を移し、外へはフィールド単位の操作だけを出すことで、
//! **構造体全体への参照は書こうとしても書けない**（`SCHEDULER` という名前が
//! モジュールの外から見えない）。規律ではなくコンパイラが保証する。
//!
//! `static_mut_refs` lint では解けない。実測したが、
//! `&mut *(addr_of!(SCHEDULER) as *mut Scheduler)` は生ポインタを経由するため
//! `#![deny(static_mut_refs)]` を付けてもエラーにならない（このlintが見るのは
//! `&mut SCHEDULER` のような直接の参照生成で、`addr_of!` はその回避手段として
//! 使われる書き方である。`main.rs` の `.bss` カナリア検査に同じ理由のコメントが
//! ある）。
//!
//! # このモジュールへ足してはならないもの
//!
//! **`&mut Scheduler` や `&Scheduler` を返す関数を足さないこと。** 足した瞬間に
//! 上の保証が消え、しかもビルドは通り続ける。フィールドを増やすときは、
//! そのフィールド専用のアクセサを足す。
//!
//! # フィールドごとの文脈（触る側の一覧）
//!
//! 増やすときはこの表を更新すること。**協調デモはタイマ解禁前（`irq::unmask(0)`
//! より前）に完結する**ので、協調のワーカーは割り込みと並行しない。並行するのは
//! プリエンプティブのワーカーだけである。
//!
//! | フィールド | IF=0（割り込み） | IF=1 協調 | IF=1 プリエンプティブ | メイン文脈 | 起動時 |
//! |---|---|---|---|---|---|
//! | `saved_rsp` | 読み書き | - | - | - | 書き |
//! | `stack_top` / `stack_bottom` | 読み | - | - | - | 書き |
//! | `runnable` | 書き / 読み | 書き | - | - | 書き |
//! | `base` | - | 読み | 読み | - | 書き |
//! | `rounds_left` | - | 読み書き | - | 読み | 書き |
//! | `iterations` | - | - | **加算** | **読み** | 書き |
//! | `resumes` | **加算** | - | - | **読み** | 書き |
//! | `switches` | **加算** | - | - | **読み** | 書き |
//! | `demo_active` / `demo_deadline` | 読み書き | - | - | - | 書き（`InterruptGuard`下） |
//!
//! **太字の 3 つ（`iterations` / `resumes` / `switches`）だけが、書き手と読み手が
//! 並行する。** 会計を締めるのはデモ後のメイン文脈だが、そこは IF=1 であり、
//! タイマは動き続けている（`switches` と `resumes` は割り込み側が加算しうる）。
//! この 3 つは `read_volatile` / `write_volatile` で触る。
//!
//! **volatile は値の可視性しか与えない。複数フィールドの一貫したスナップショットは
//! 与えない。** `switches` と `sum(resumes)` を突き合わせる会計が成立しているのは、
//! **読み取りの時点で書き手が止まっているから**である。締切で `demo_active` が落ちて
//! 全ワーカーが走行不可になると、`pick_next` がメインを返し `schedule_switch` は
//! `next == current` で早期に戻るので、`add_switch` / `add_resume` へ到達しない。
//! 協調デモはそもそもタイマ解禁前に完結する。**デモが動いている最中にカウンタを
//! 読む検査を足すなら、volatile だけでは一貫して読めない**（読みの間に割り込みが
//! 入り、`switches` と `resumes` が別々の時点の値になりうる）。
//!
//! **volatile は同期プリミティブではない。** ここで足りるのは、単一コアで
//! 割り込みハンドラとスカラを共有しているからである（読み書きが 1 命令で完結し、
//! 割り込みは命令境界でしか入らない）。**複数コアでは、この理由づけは成立しない。**
//! S3 以降でこの箇所を読むときは、volatile を見て「マルチコアでも安全」と
//! 読まないこと。
//!
//! 残りのフィールドは、単一文脈からしか触られないか、触る文脈どうしが時間的に
//! 分離している（協調デモとタイマ、起動時と定常）。素の読み書きでよい。

use core::ptr::addr_of_mut;

use super::{Task, EMPTY_TASK, TASK_COUNT};

/// スケジューラのグローバル状態。**このモジュールの外からは名前が見えない。**
static mut SCHEDULER: Scheduler = Scheduler {
    tasks: [EMPTY_TASK; TASK_COUNT],
    switches: 0,
    demo_active: false,
    demo_deadline: 0,
};

/// フィールドの器。構造体そのものは外へ出さない。
struct Scheduler {
    tasks: [Task; TASK_COUNT],
    /// スイッチした総回数（会計用）。各スイッチで再開されたタスクの `resumes`
    /// も 1 増えるので、`switches == 全タスクの resumes の合計`。
    switches: u64,
    /// M5-d のプリエンプティブデモが進行中か。
    demo_active: bool,
    /// プリエンプティブデモを打ち切る TIMER_TICKS の閾値。
    demo_deadline: u64,
}

/// タスク 1 個への生ポインタ。**参照は作らない。**
///
/// 添字の妥当性はここで閉じる。範囲外でポインタを作ると、境界の内側で
/// 未定義動作を作ることになる。呼び出し側は `current_index()` や
/// `0..TASK_COUNT` の走査から添字を得ており、範囲外は論理の壊れを意味するので
/// fail-fast で止める（分岐 1 つの費用は、IF=0 の経路でも問題にならない）。
fn slot(index: usize) -> *mut Task {
    if index >= TASK_COUNT {
        panic!("scheduler: task index {index} is out of range (TASK_COUNT = {TASK_COUNT})");
    }
    // SAFETY: 添字は今検査した。`addr_of_mut!` は参照を作らずに要素の
    // アドレスを取る。
    unsafe { addr_of_mut!(SCHEDULER.tasks).cast::<Task>().add(index) }
}

/// タスク 1 個を丸ごと書く（起動時のみ）。
pub(super) fn init_task(index: usize, task: Task) {
    // SAFETY: `slot` が範囲を検査した有効なポインタ。起動時の単一文脈から
    // しか呼ばれない。
    unsafe { slot(index).write(task) }
}

pub(super) fn saved_rsp(index: usize) -> u64 {
    // SAFETY: 有効なポインタ。IF=0 の切り替え経路からのみ読む。
    unsafe { addr_of_mut!((*slot(index)).saved_rsp).read() }
}

pub(super) fn set_saved_rsp(index: usize, rsp: u64) {
    // SAFETY: 有効なポインタ。IF=0 の切り替え経路からのみ書く。
    unsafe { addr_of_mut!((*slot(index)).saved_rsp).write(rsp) }
}

pub(super) fn stack_top(index: usize) -> u64 {
    // SAFETY: 有効なポインタ。起動時に書いた後は読むだけ。
    unsafe { addr_of_mut!((*slot(index)).stack_top).read() }
}

pub(super) fn stack_bottom(index: usize) -> u64 {
    // SAFETY: 有効なポインタ。起動時に書いた後は読むだけ。
    unsafe { addr_of_mut!((*slot(index)).stack_bottom).read() }
}

pub(super) fn runnable(index: usize) -> bool {
    // SAFETY: 有効なポインタ。書き手と時間的に分離している（表参照）。
    unsafe { addr_of_mut!((*slot(index)).runnable).read() }
}

pub(super) fn set_runnable(index: usize, value: bool) {
    // SAFETY: 有効なポインタ。書き手と時間的に分離している（表参照）。
    unsafe { addr_of_mut!((*slot(index)).runnable).write(value) }
}

/// 走行可能フラグをまとめて読む。`pick_next` を純粋関数のまま保つための値。
pub(super) fn runnable_flags() -> [bool; TASK_COUNT] {
    let mut flags = [false; TASK_COUNT];
    for (index, flag) in flags.iter_mut().enumerate() {
        *flag = runnable(index);
    }
    flags
}

pub(super) fn base(index: usize) -> u64 {
    // SAFETY: 有効なポインタ。起動時に書いた後は読むだけ。
    unsafe { addr_of_mut!((*slot(index)).base).read() }
}

pub(super) fn rounds_left(index: usize) -> u64 {
    // SAFETY: 有効なポインタ。協調デモ（タイマ解禁前）とメインしか触らない。
    unsafe { addr_of_mut!((*slot(index)).rounds_left).read() }
}

pub(super) fn set_rounds_left(index: usize, value: u64) {
    // SAFETY: 有効なポインタ。協調デモ（タイマ解禁前）しか書かない。
    unsafe { addr_of_mut!((*slot(index)).rounds_left).write(value) }
}

/// 進捗カウンタ。**書き手（IF=1 のワーカー）と読み手（メイン）が並行する。**
pub(super) fn iterations(index: usize) -> u64 {
    // SAFETY: 有効なポインタ。並行する書き手がいるので volatile で読む。
    unsafe { addr_of_mut!((*slot(index)).iterations).read_volatile() }
}

pub(super) fn add_iteration(index: usize) {
    // SAFETY: `slot` が範囲を検査した有効なポインタ。参照は作らない。
    let pointer = unsafe { addr_of_mut!((*slot(index)).iterations) };
    // SAFETY: 有効なポインタ。加算するのはこのワーカー文脈だけで、割り込み側は
    // このフィールドを触らない（表参照）。読み手はメインなので volatile で書く。
    unsafe { pointer.write_volatile(pointer.read_volatile().wrapping_add(1)) }
}

/// 再開回数。**加算するのは IF=0 の切り替え経路で、読み手（メイン）と並行する。**
pub(super) fn resumes(index: usize) -> u64 {
    // SAFETY: 有効なポインタ。並行する書き手がいるので volatile で読む。
    unsafe { addr_of_mut!((*slot(index)).resumes).read_volatile() }
}

pub(super) fn add_resume(index: usize) {
    // SAFETY: `slot` が範囲を検査した有効なポインタ。参照は作らない。
    let pointer = unsafe { addr_of_mut!((*slot(index)).resumes) };
    // SAFETY: 有効なポインタ。加算するのは IF=0 の経路だけなので、加算自体が
    // 割り込みで分断されることはない。読み手と並行するので volatile で書く。
    unsafe { pointer.write_volatile(pointer.read_volatile().wrapping_add(1)) }
}

/// スイッチ総数。**加算するのは IF=0 の切り替え経路で、読み手（メイン）と並行する。**
pub(super) fn switches() -> u64 {
    // SAFETY: 静的変数への有効なポインタ。並行する書き手がいるので volatile。
    unsafe { addr_of_mut!(SCHEDULER.switches).read_volatile() }
}

pub(super) fn set_switches(value: u64) {
    // SAFETY: 静的変数への有効なポインタ。起動時のリセットのみ。
    unsafe { addr_of_mut!(SCHEDULER.switches).write_volatile(value) }
}

pub(super) fn add_switch() {
    // SAFETY: 静的変数のフィールドのアドレスを取るだけで、参照は作らない。
    let pointer = unsafe { addr_of_mut!(SCHEDULER.switches) };
    // SAFETY: 静的変数への有効なポインタ。加算は IF=0 の経路だけ。
    unsafe { pointer.write_volatile(pointer.read_volatile().wrapping_add(1)) }
}

pub(super) fn demo_active() -> bool {
    // SAFETY: 静的変数への有効なポインタ。IF=0 と起動時（IF=0）しか触らない。
    unsafe { addr_of_mut!(SCHEDULER.demo_active).read() }
}

pub(super) fn set_demo_active(value: bool) {
    // SAFETY: 静的変数への有効なポインタ。IF=0 と起動時（IF=0）しか触らない。
    unsafe { addr_of_mut!(SCHEDULER.demo_active).write(value) }
}

pub(super) fn demo_deadline() -> u64 {
    // SAFETY: 静的変数への有効なポインタ。IF=0 と起動時（IF=0）しか触らない。
    unsafe { addr_of_mut!(SCHEDULER.demo_deadline).read() }
}

pub(super) fn set_demo_deadline(value: u64) {
    // SAFETY: 静的変数への有効なポインタ。IF=0 と起動時（IF=0）しか触らない。
    unsafe { addr_of_mut!(SCHEDULER.demo_deadline).write(value) }
}
