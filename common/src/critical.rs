//! クリティカルセクション（M4-c-1）。
//!
//! シングルコア前提では、共有データの排他は「割り込み禁止」で行う（architecture.md
//! の同期・並行性方針）。[`InterruptGuard`] は RAII で `cli`/`sti` を対にし、**入れ子でも
//! 正しく動く**ようにする。
//!
//! 復元の判断（保存時に IF=1 だった場合のみ `sti`）は純粋ロジックとして
//! [`crate::cpu::should_restore_interrupts`] に切り出し、ホストテストで固定
//! してある。ここはそれを使ってハードウェアを操作するだけ。

use core::cell::UnsafeCell;
use core::fmt::Write as _;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::cpu;
use crate::serial::SerialPort;

/// 現在保持している [`InterruptGuard`] の数（クリティカルセクションの入れ子
/// 深さ）。
///
/// **入れ子の正しさ自体はこのカウンタでは決めない**（それは各ガードが保存する
/// RFLAGS で成立している）。このカウンタは「今クリティカルセクションの中に
/// いるか」を、`IF` の状態とは独立に知るためだけのものである。M5-c の協調的
/// `yield` が、`Locked` / `InterruptGuard` を保持したまま呼ばれていないかを
/// 判定するのに使う（保持したまま `yield` すると、別タスクがクリティカル
/// セクションの途中で走る）。
///
/// [`Locked`] は `lock()` の中で `InterruptGuard` を 1 つ保持するので、
/// `InterruptGuard` の数を数えれば `Locked` の保持も覆う。
///
/// 増減は必ず割り込み禁止（`cli` 済み）の区間で行うため、シングルコアでは
/// `Relaxed` で十分（メモリ順序の問題は SMP 特有。ADR-0002 のスコープ外）。
static CRITICAL_NESTING_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// 現在のクリティカルセクションの入れ子深さ（保持中の `InterruptGuard` の数）。
///
/// `0` なら、どの `InterruptGuard` / `Locked` も保持していない。`yield` は
/// これが `0` でなければ fail-fast する（`IF` は見ない）。
pub fn critical_nesting_depth() -> usize {
    CRITICAL_NESTING_DEPTH.load(Ordering::Relaxed)
}

/// 割り込みを禁止するクリティカルセクションのガード。
///
/// [`InterruptGuard::enter`] で現在の RFLAGS を保存して `cli` し、Drop で
/// **保存時に IF=1 だった場合のみ** `sti` する。無条件に `sti` しないのが
/// 入れ子の正しさの要である。
///
/// - 外側が IF=1 で `enter` → `cli`（保存値 IF=1）
/// - 内側が `enter` → 既に IF=0 なので保存値 IF=0、Drop でも `sti` しない
/// - 外側の Drop で初めて `sti`
///
/// # スレッド安全性
///
/// このガードは「今この CPU が張っている割り込み禁止区間」を表す。別の
/// コンテキスト（M5 以降のタスク）へ移動すると、cli した文脈と sti する
/// 文脈がずれて意味が壊れる。そのため `PhantomData<*const ()>` を持たせて
/// **`!Send` かつ `!Sync`** にしてある。生ポインタは `Send`/`Sync` を
/// 自動導出しないため、これだけでガード全体が移動・共有不可になる。
/// 現状シングルコアでスレッドも無いため実害は無いが、M5 で意味を持つ。
pub struct InterruptGuard {
    saved_rflags: u64,
    /// `!Send` + `!Sync` にするためのマーカー。値としては使わない。
    _not_send_sync: PhantomData<*const ()>,
}

impl InterruptGuard {
    /// 現在の割り込み状態を保存して割り込みを禁止する。
    ///
    /// 戻り値のガードが生きている間、割り込みは禁止される。ガードを早く
    /// 落とせば、その時点で（保存状態に応じて）復元される。
    #[must_use = "the guard must be held for the critical section; dropping it immediately \
                  ends the section right away"]
    pub fn enter() -> Self {
        let saved_rflags = cpu::read_rflags();
        // SAFETY: これはまさにクリティカルセクションへ入る操作であり、割り込みを
        // 禁止してよい文脈。保存した状態は Drop で復元する。
        unsafe {
            cpu::disable_interrupts();
        }
        // 入れ子深さを 1 増やす。**cli の後に触る**ので、この増分の最中に
        // 割り込みは入らない。
        CRITICAL_NESTING_DEPTH.fetch_add(1, Ordering::Relaxed);
        Self {
            saved_rflags,
            _not_send_sync: PhantomData,
        }
    }

    /// 保存した RFLAGS（診断用）。
    pub fn saved_rflags(&self) -> u64 {
        self.saved_rflags
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        // 入れ子深さを 1 減らす。**復元（sti）より前に**減らすことで、
        // 「まだこのガードを数えているのに IF=1」という窓を作らない。この
        // 時点ではまだ割り込み禁止なので、減算の最中に割り込みは入らない。
        CRITICAL_NESTING_DEPTH.fetch_sub(1, Ordering::Relaxed);
        if cpu::should_restore_interrupts(self.saved_rflags) {
            // SAFETY: enter した時点で IF=1 だった、つまり呼び出し元は割り込みが
            // 有効な文脈にいた。その状態へ戻すだけなので有効化してよい。
            // IF=0 だった場合はこの分岐に入らないため、勝手に有効化しない。
            unsafe {
                cpu::enable_interrupts();
            }
        }
    }
}

/// 割り込み禁止で保護する内部可変ラッパー。
///
/// シングルコア前提では、[`lock`][Self::lock] が返すガードが生きている間だけ
/// 割り込みを禁止すれば、その区間に割り込みハンドラが割って入って同じデータへ
/// 触れることは無く、排他が保証される（M4-c-2）。SMP では別コアが同時に
/// アクセスしうるのでこの論法は崩れるが、それは ADR-0002 のスコープ外。
///
/// M2-e 時点では「割り込み常時禁止」を前提にしていたが、M4-d で `sti` すると
/// その前提が崩れる。前提を「取得中は割り込み禁止」へ変えたのが M4-c-2 の
/// 差し替えである（ADR-0012 の Addendum）。
pub struct Locked<T> {
    inner: UnsafeCell<T>,
    /// デバッグ用の二重取得検出フラグ。取得中は `true`。
    ///
    /// シングルコアかつ取得中は割り込み禁止なので、通常の並行アクセスでは
    /// 二重取得は起きない。起きるのは「同じロックを保持したまま同じスレッドが
    /// 再度 `lock` を呼ぶ」というコードのバグのときで、これは無言のデータ競合に
    /// なるため検出して停止する。
    acquired: AtomicBool,
}

// SAFETY: 複数の実行文脈からの同時アクセスは、シングルコア前提かつ「取得中は
// 割り込みを禁止する」ことによって防がれている。lock() がガードを返す前に
// InterruptGuard で割り込みを禁止し、ガードが生きている間は禁止が続くため、
// 保持区間中に割り込みハンドラが走って同じ値へ触ることはない。SMP へ進む
// 場合は別コアの同時アクセスを防げないため、この実装ごと見直す（ADR-0002）。
unsafe impl<T> Sync for Locked<T> {}

impl<T> Locked<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: UnsafeCell::new(value),
            acquired: AtomicBool::new(false),
        }
    }

    /// 割り込みを禁止し、二重取得でないことを確かめてから排他アクセスの
    /// ガードを返す。
    ///
    /// **ホスト（`cargo test`）からは呼べない。** `cli`/`sti` は特権命令
    /// （ring 0）であり、ユーザー空間で実行すると #GP になる。したがって
    /// ロックの実動作は実機で検証する（`cargo xtask run --critical-test`）。
    /// ホストで検証できるのは、フィールドの drop 順と
    /// [`cpu::should_restore_interrupts`] の判断だけ。
    ///
    /// 二重取得（同じロックを保持したまま再度呼ぶ）を検出した場合は、
    /// **panic ではなく**シリアルへ直接エラーを出して停止する。ヒープの
    /// ロックで起きた場合、panic 経路が確保を試みると事態が悪化するため
    /// （ヒープ枯渇時の無限再帰と同じ懸念）。
    pub fn lock(&self) -> LockGuard<'_, T> {
        // **この 2 行の順序に意味がある。** 先に割り込みを禁止し、その後で
        // フラグを立てる。逆にすると「フラグは立っているが割り込みはまだ
        // 有効」という窓ができ、そこへ割り込みが入るとハンドラからは保持中に
        // 見える。ハンドラが同じロックを取ろうとすれば、実際には誰も保持して
        // いないのに二重取得として停止する。現状は割り込みが常時禁止なので
        // この窓は開かないが、M4-d で sti した後は実際に踏みうる。
        // 禁止してからフラグを立てれば、シングルコアではその間に横取りされず、
        // 検査と更新が不可分に行える。
        let interrupts = InterruptGuard::enter();

        // 取得中は割り込み禁止なので、Relaxed で十分（メモリ順序の問題は
        // SMP 特有）。既に true なら二重取得。
        if self.acquired.swap(true, Ordering::Relaxed) {
            report_double_lock_and_halt();
        }

        // SAFETY: swap で false→true にできたのはこのガードだけであり、
        // かつ取得中は割り込み禁止。したがってこの &mut T を使っている間、
        // 他の実行文脈が同じ値へ触ることはない。
        let data = unsafe { &mut *self.inner.get() };

        LockGuard {
            data,
            lock: self,
            interrupts,
        }
    }
}

/// [`Locked::lock`] が返す排他アクセスのガード。
///
/// **フィールドの宣言順に意味がある（宣言順 = drop 順）。** Drop では
/// [`Locked::acquired`] のフラグをクリアしてから、`interrupts` を最後に
/// 落として割り込みを復元する。順序が逆になり、まだ `data`（`&mut T`）が
/// 生きているうちに割り込みが有効化されると、そこへ割り込みハンドラが入って
/// 同じデータへ触れば競合する。`interrupts` を最後のフィールドにすることで、
/// 復元が必ずガードの終わりに行われる。フィールドを並べ替えないこと。この
/// drop 順はホストテスト（`the_interrupt_guard_drops_last`）で固定してある。
pub struct LockGuard<'a, T> {
    data: &'a mut T,
    lock: &'a Locked<T>,
    /// 読み出さないが、**保持していること自体に意味がある**（Drop で割り込みを
    /// 復元する）。最後のフィールドであることが drop 順の要件。
    #[allow(dead_code)]
    interrupts: InterruptGuard,
}

impl<T> Deref for LockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.data
    }
}

impl<T> DerefMut for LockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.data
    }
}

impl<T> Drop for LockGuard<'_, T> {
    fn drop(&mut self) {
        // フラグのクリアは、まだ割り込みが禁止されているうちに行う
        // （`interrupts` フィールドはこの後に落ちる）。
        self.lock.acquired.store(false, Ordering::Relaxed);
        // ここで暗黙に `interrupts` が落ち、保存状態に応じて sti する。
    }
}

/// 二重取得をシリアルへ報告して停止する。
///
/// パニックハンドラや例外ハンドラと同じく、確保もロックもコンソールも
/// 使わずにシリアルへ直接書く。ヒープのロックで二重取得が起きた場合、
/// panic 経路が確保を試みるとさらに壊れるため（ADR-0004、ADR-0012）。
fn report_double_lock_and_halt() -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[ERROR] lock: double acquisition detected (a Locked<T> was locked while already held)"
    );
    let _ = writeln!(
        serial,
        "[ERROR]   this is a bug: some code path holds the lock and tries to lock it again"
    );
    let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");
    cpu::halt_forever();
}

/// `InterruptGuard` が `Send` でも `Sync` でもないことをコンパイル時に固定する。
///
/// 手法（`static_assertions` クレートの `assert_not_impl_any!` と同じ）:
/// 「すべての型」に実装した版（型引数 `()`）と「`Send` な型」に実装した版
/// （型引数 `u8`）の 2 つを用意する。呼び出し側は型引数を推論穴 `_` にして
/// メソッドを参照する。対象が `Send` だと両方の impl が該当し `_` を一意に
/// 決められずコンパイルに失敗する。`Send` でなければ `()` 版だけが該当して
/// 通る。**この `const _` が通ること自体が `!Send` の証明**であり、将来
/// `PhantomData` を外して誤って `Send` になると、ここでビルドが落ちる。
///
/// 型引数を `_` にするのが要点。`<()>` と明示すると常に第 1 impl へ解決して
/// しまい、負の検査にならない。
const _: fn() = || {
    trait AmbiguousIfSend<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

    // 推論穴 `_`。InterruptGuard が Send だと曖昧になりビルドが落ちる。
    let _ = <InterruptGuard as AmbiguousIfSend<_>>::some_item;
};

const _: fn() = || {
    trait AmbiguousIfSync<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}

    let _ = <InterruptGuard as AmbiguousIfSync<_>>::some_item;
};

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;
    use std::{cell::RefCell, rc::Rc};

    /// `LockGuard` のフィールド drop 順（宣言順）を機械的に固定する。
    ///
    /// `LockGuard` 自体は `InterruptGuard`（実ハードウェアの `cli`/`sti`）を
    /// 含むためホストでは構築できない。そこで**同じフィールド並び**を
    /// drop 記録型で作り、宣言順どおりに落ちること、とくに「割り込みガードに
    /// 相当する最後のフィールドが最後に落ちる」ことを確かめる。
    ///
    /// `LockGuard` のフィールドを並べ替えたら、こちらの並びも合わせて
    /// 変える必要があり、そのとき期待順序も見直すことになる。宣言順に依存して
    /// いるという事実をテストとして残すのが目的。
    #[test]
    fn the_interrupt_guard_drops_last() {
        struct DropRecorder {
            name: &'static str,
            log: Rc<RefCell<Vec<&'static str>>>,
        }
        impl Drop for DropRecorder {
            fn drop(&mut self) {
                self.log.borrow_mut().push(self.name);
            }
        }

        // LockGuard と同じフィールド順を模す:
        //   data 相当 → lock 相当 → interrupts 相当（最後）
        // 読み出さず、drop されることだけが目的。
        #[allow(dead_code)]
        struct MirroredGuard {
            data: DropRecorder,
            lock: DropRecorder,
            interrupts: DropRecorder,
        }

        let log = Rc::new(RefCell::new(Vec::new()));
        {
            let _guard = MirroredGuard {
                data: DropRecorder {
                    name: "data",
                    log: log.clone(),
                },
                lock: DropRecorder {
                    name: "lock",
                    log: log.clone(),
                },
                interrupts: DropRecorder {
                    name: "interrupts",
                    log: log.clone(),
                },
            };
        }

        // 宣言順に落ちる。interrupts（= 割り込み復元）が最後。
        assert_eq!(*log.borrow(), ["data", "lock", "interrupts"]);
        assert_eq!(
            *log.borrow().last().unwrap(),
            "interrupts",
            "割り込みの復元は必ずガードの最後で行われなければならない"
        );
    }
}
