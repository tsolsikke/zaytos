//! クリティカルセクション（M4-c-1）。
//!
//! [`InterruptGuard`] は RAII で `cli`/`sti` を対にし、**入れ子でも正しく動く**
//! ようにする。
//!
//! # 排他の論法
//!
//! **[`Locked<T>`] の排他は 2 つで成り立っている。片方だけでは足りない。**
//!
//! - **同一コアの再入は、取得中の割り込み禁止が止める。** ガードが生きている間は
//!   `IF=0` なので、その区間へ割り込みハンドラが入って同じ値へ触ることはない。
//! - **別コアは、[`Locked::acquired`] の `swap` が止める。** `false` から `true` へ
//!   できた側だけがガードを得る。
//! - **競合したら待たない。** 負けた側は**保持者を出して停止する**（fail-fast。
//!   ADR-0004。保持者を持つ理由は [`Locked::holder`]）。
//! - **BKL を保持したまま `Locked<T>` を取ってよく、逆は作らない**（`kernel/src/bkl.rs`）。
//!   向きを固定して循環待ちを構造から消してある。
//!
//! **かつてここには「シングルコア前提では、共有データの排他は割り込み禁止で行う」と
//! 書いてあった。** **`IF` を落とすこと自体は現在の設計である**——同一コアの再入を
//! 止めるのがその役目である。**失効したのは「それだけで排他になる」のほうで、
//! 別コアは割り込み禁止では止まらない。**
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
use crate::percpu::{cpu_id, PerCpu, MAX_CPUS};
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
/// # CPUごとに持つ（seam整備3b、ADR-0023）
///
/// 入れ子深さは [`PerCpu`] でCPUごとに持つ。各コアは自分のスロットだけを
/// 触る。「このコアが今クリティカルセクションの中にいるか」「保持したまま
/// `yield` していないか」はいずれもコアローカルな問いなので、per-CPU が正しい
/// 単位である（別コアの入れ子深さは、このコアの `yield` 判定に無関係）。
/// [`crate::percpu::MAX_CPUS`] が `1` だった間は [`PerCpu::this_cpu`] が常に唯一の
/// スロットを返し、振る舞いは従来の単一カウンタと同一だった（S3-b-1 まで）。
/// **いまは複数コアが同時に自分のスロットを増減している。**
///
/// 増減は必ず割り込み禁止（`cli` 済み）の区間で、かつ自コアのスロットに対して
/// のみ行うため、`Relaxed` で十分。他コアが同じスロットへ触ることはないので、
/// SMP でもメモリ順序の考慮は要らない（隣接スロットの false sharing は性能の
/// 問題で、この値の正しさには影響しない。[`crate::percpu::MAX_CPUS`] の注意）。
///
/// **将来の条件**: 他コアからこのスロットを読む用途（パニック時に全コアの
/// 入れ子深さをダンプする診断など）を足すなら、`Relaxed` の根拠が変わるので
/// 見直すこと。上の「他コアが同じスロットへ触ることはない」は現在の不変条件で
/// あり、クロスコア読みが入ると成り立たなくなる（BKL本体では複数コアの
/// デバッグをするので、この種の診断は実際に欲しくなりうる）。
static CRITICAL_NESTING_DEPTH: PerCpu<AtomicUsize> =
    PerCpu::new([const { AtomicUsize::new(0) }; MAX_CPUS]);

/// 現在のクリティカルセクションの入れ子深さ（保持中の `InterruptGuard` の数）。
///
/// `0` なら、どの `InterruptGuard` / `Locked` も保持していない。`yield` は
/// これが `0` でなければ fail-fast する（`IF` は見ない）。
pub fn critical_nesting_depth() -> usize {
    CRITICAL_NESTING_DEPTH.this_cpu().load(Ordering::Relaxed)
}

/// preempt-in-critical の破壊確認（M5-d）で、サボタージュ（[`InterruptGuard`] の
/// cli 省略と on_timer_tick の防御スキップ bypass）を「今だけ」有効にするフラグ。
///
/// **かつてサボタージュは大域的だった。** feature を有効にすると全区間で cli を
/// 落とし防御スキップを外していた。それだとデモ開始（setup / yield）時まで perturb
/// して、検査対象（保持窓）へ到達する前に `switches=0` の startup レースで約10%落ちた
/// （既定ビルドは20/20健全なので、カーネルではなくサボタージュが原因と実測で確定）。
/// arm 窓へ絞ることで、デモ開始は正常な cli の下で走り、二重取得を狙う保持窓だけを
/// 壊す。**再び大域化すると startup レースが再発する**（`docs/verification-coverage.md`
/// の「確率的なテストとフレークの署名」）。
///
/// arm/disarm を `Release`、読み出しを `Acquire` にする。**厳密には `Relaxed` でも
/// 正しい**: 同一スレッドが同一アトミックへアクセスする限り、arm の store と直後の
/// [`InterruptGuard`] の load は Ordering に依らず program order で順序付き、guard は
/// 必ず armed を見る。それでも、このフラグは cli を**省く**区間で割り込みコンテキスト
/// （on_timer_tick）から読まれるので、`Relaxed` で足りる理由を都度たどるより
/// **保守的に強い順序を選ぶ**（`CRITICAL_NESTING_DEPTH` は cli 済み区間で触るので
/// `Relaxed` で足りるのと対照的である。害は無く、意図が読み手に伝わる）。
#[cfg(feature = "preempt-in-critical-break")]
static SABOTAGE_ARMED: AtomicBool = AtomicBool::new(false);

/// サボタージュが今 arm されているか（on_timer_tick が防御スキップを bypass するかの
/// 判定に使う）。
#[cfg(feature = "preempt-in-critical-break")]
pub fn sabotage_armed() -> bool {
    SABOTAGE_ARMED.load(Ordering::Acquire)
}

/// サボタージュを arm する。返すガードを保持している間だけ有効で、**Drop で disarm
/// する**。手動の arm/disarm ペアはパニックや早期リターンで disarm が漏れるので、
/// [`InterruptGuard`] と同じく「規律より構造」で RAII にする。
#[cfg(feature = "preempt-in-critical-break")]
#[must_use = "arm はガードを保持している間だけ有効。すぐ drop すると即 disarm される"]
pub fn arm_sabotage() -> SabotageArmGuard {
    SABOTAGE_ARMED.store(true, Ordering::Release);
    SabotageArmGuard {
        _not_send_sync: PhantomData,
    }
}

/// [`arm_sabotage`] のガード。Drop で disarm する。
#[cfg(feature = "preempt-in-critical-break")]
pub struct SabotageArmGuard {
    _not_send_sync: PhantomData<*const ()>,
}

#[cfg(feature = "preempt-in-critical-break")]
impl Drop for SabotageArmGuard {
    fn drop(&mut self) {
        SABOTAGE_ARMED.store(false, Ordering::Release);
    }
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
/// **書いた時点（シングルコアでタスクも無かった）では実害が無かったが、
/// いまは複数コアがそれぞれ独立に禁止区間を張っている。**
/// 別コアや別タスクへ渡ると、`cli` した文脈と `sti` する文脈がずれる。
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
        // preempt-in-critical-break: サボタージュが arm されている間だけ cli を落とす。
        // Locked 保持中も IF=1 のままになり、timer プリエンプトがクリティカル区間へ
        // 食い込む（M5-d の破壊確認）。**かつては大域的に落としていたが、それだと
        // デモ開始時まで perturb して startup レースを起こした**（[`SABOTAGE_ARMED`]
        // 参照）。arm 窓の外は通常どおり cli する。既定ビルドは feature オフなのでこの
        // 判定ごと消え、常に cli する = production は不変。
        #[cfg(feature = "preempt-in-critical-break")]
        let drop_cli = SABOTAGE_ARMED.load(Ordering::Acquire);
        #[cfg(not(feature = "preempt-in-critical-break"))]
        let drop_cli = false;
        if !drop_cli {
            // SAFETY: これはまさにクリティカルセクションへ入る操作であり、割り込みを
            // 禁止してよい文脈。保存した状態は Drop で復元する。
            unsafe {
                cpu::disable_interrupts();
            }
        }
        // 入れ子深さを 1 増やす。**cli の後に触る**ので、この増分の最中に割り込みは
        // 入らない（cli を落とす破壊ビルド + arm 中を除く）。自コアのスロットだけを
        // 触る（[`PerCpu::this_cpu`]）。
        CRITICAL_NESTING_DEPTH
            .this_cpu()
            .fetch_add(1, Ordering::Relaxed);
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
        CRITICAL_NESTING_DEPTH
            .this_cpu()
            .fetch_sub(1, Ordering::Relaxed);
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

/// カーネル入口が自分で張る割り込み禁止区間のガード（S4-b-2）。
///
/// # なぜ [`InterruptGuard`] を使わないのか。**カウンタの意味が違う**
///
/// [`InterruptGuard`] は [`critical_nesting_depth`] を増やす。その値の意味は
/// 「割り込みが禁止されているか」ではなく、**「呼び出し側が自発的にクリティカル
/// セクションを保持しているか」**である。`kernel::task::on_yield` のコメントが
/// 既にその線を引いている——「int ゲート自身が積んだぶんは `InterruptGuard` では
/// ないのでカウンタには乗らない。したがってここが 0 でなければ、呼び出し側が
/// `Locked` / `InterruptGuard` を保持している」。
///
/// **BKL は入口自身が取るものなので、「int ゲート自身が積んだぶん」と同じ側に
/// 落ちる。** 数えると意味が変わり、実際に 2 つ壊れる（実装前にコードから確かめた）。
///
/// - `on_timer_tick` の防御スキップが毎回発火し、**プリエンプトが 1 度も起きなくなる**
/// - `on_yield` が「保持したまま yield した」と判定して**停止する**
///
/// # 数えないが `cli` はする
///
/// **「保持区間 = IF=0」の不変条件は保たれる。** 数えないだけである。
/// 再帰の検出はカウンタではなく保持者の CPU 番号で行う（`kernel::bkl`）。
///
/// # スレッド安全性
///
/// [`InterruptGuard`] と同じ理由で `!Send` かつ `!Sync` にしてある。
pub struct EntryInterruptGuard {
    saved_rflags: u64,
    _not_send_sync: PhantomData<*const ()>,
}

impl EntryInterruptGuard {
    /// 現在の割り込み状態を保存して割り込みを禁止する。**深さは数えない。**
    #[must_use = "the guard must be held for the entry; dropping it immediately ends the \
                  interrupt-disabled section right away"]
    pub fn enter() -> Self {
        let saved_rflags = cpu::read_rflags();
        // SAFETY: カーネル入口の排他区間へ入る操作であり、割り込みを禁止して
        // よい文脈である。保存した状態は Drop で復元する。
        unsafe {
            cpu::disable_interrupts();
        }
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

impl Drop for EntryInterruptGuard {
    fn drop(&mut self) {
        if cpu::should_restore_interrupts(self.saved_rflags) {
            // SAFETY: enter した時点で IF=1 だった文脈へ戻すだけである。
            unsafe {
                cpu::enable_interrupts();
            }
        }
    }
}

/// 割り込み禁止で保護する内部可変ラッパー。
///
/// **排他が何で成り立っているかは、モジュール doc の「排他の論法」にある。**
/// 要点だけ言えば、**同一コアの再入は取得中の割り込み禁止が止め**（M4-c-2）、
/// **別コアは [`Self::acquired`] の `swap` が止める**（S4-b-1）。
///
/// **以前ここには「SMP では別コアが同時にアクセスしうるのでこの論法は崩れるが、
/// それは ADR-0002 のスコープ外」と書いてあった。** **その SMP が来たので
/// スコープ外ではなくなり、`swap` と保持者を足して論法を作り直した**（ADR-0023）。
///
/// M2-e 時点では「割り込み常時禁止」を前提にしていたが、M4-d で `sti` すると
/// その前提が崩れる。前提を「取得中は割り込み禁止」へ変えたのが M4-c-2 の
/// 差し替えである（ADR-0012 の Addendum）。
pub struct Locked<T> {
    inner: UnsafeCell<T>,
    /// 取得中かどうか。取得中は `true`。
    ///
    /// 起きうる「既に取られている」は 2 通りある。**同じコアが保持したまま
    /// 再度 `lock` を呼んだ**（コードのバグ）か、**別のコアが保持している**
    /// （競合）かである。**このフラグだけでは区別できない**ので、
    /// [`Self::holder`] を併せて持つ（S4-b-1）。
    acquired: AtomicBool,
    /// 取得中のコアの番号。未取得なら [`NO_HOLDER`]。
    ///
    /// # なぜ足したのか。**同値である間は分類の誤りが観測できない**
    ///
    /// シングルコアの間は「既に取られている」が必ず同一コアの二重取得だった
    /// ので、両者は**同じ観測**だった。複数コアでは別の原因になるが、
    /// **観測が同じままだと、どちらが起きたのかをログから決められない。**
    /// 保持者を持てば、その場で分かれる。
    ///
    /// **この段（S4-b-1）ではまだ競合は起きない。** 本番経路で `Locked<T>` を
    /// 触るのは bootstrap processor だけである（キーボードのリングバッファも
    /// ヒープもそうである）。**起きない競合への備えを先に入れているので、
    /// そう書いておく。** 競合が実際に起きうるのは、BKL が入って AP が
    /// カーネルの共有物へ触るようになる段である。
    holder: AtomicUsize,
}

/// [`Locked::holder`] の「誰も保持していない」。**CPU 番号として現れない値である。**
const NO_HOLDER: usize = usize::MAX;

// SAFETY: 同時アクセスは 2 つで止める（モジュール doc の「排他の論法」）。
// 同一コアの再入は、lock() が返すガードが保持する InterruptGuard が止める。
// 別コアは acquired の swap（Acquire）が止め、false から true にできた側だけが
// &mut T を得る。負けた側はガードを得ずに保持者を出して停止するので、同じ値へ
// 2 つの参照が同時に存在することはない。
unsafe impl<T> Sync for Locked<T> {}

impl<T> Locked<T> {
    pub const fn new(value: T) -> Self {
        Self {
            inner: UnsafeCell::new(value),
            acquired: AtomicBool::new(false),
            holder: AtomicUsize::new(NO_HOLDER),
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
        //
        // **検査と更新が不可分なのは swap のほうである。** 割り込み禁止を先に
        // 置くのは、フラグが立っているのに IF=1 という窓を作らないためで、
        // 不可分性を担っているわけではない。**ここには「禁止してからフラグを
        // 立てれば、シングルコアではその間に横取りされない」と書いてあったが、
        // 別コアは割り込み禁止では止まらない。** 振る舞いは変えていない。
        // 根拠のほうを実態へ合わせる（下の Acquire / Release と同じ直しである）。
        let interrupts = InterruptGuard::enter();

        // **順序を `Acquire` / `Release` にした（S4-b-1）。**
        //
        // ここには「Relaxed で十分（メモリ順序の問題は SMP 特有）」と書いて
        // あったが、**その SMP になったので根拠が失効した。** x86 の TSO では
        // 生成される命令が変わらないので**振る舞いは不変**だが、
        // 根拠のほうを実態に合わせておく。
        if self.acquired.swap(true, Ordering::Acquire) {
            // **既に取られている。原因は 2 通りある（S4-b-1）。**
            //
            // 保持者の読みは競合しうる（読んだ瞬間に解放されているかもしれない）。
            // **ただしこの段ではどちらの原因でも停止する**ので、判断が変わる
            // のは出力する文言だけである。**分類は診断のためにある。**
            let holder = self.holder.load(Ordering::Relaxed);
            if holder == cpu_id() {
                report_double_lock_and_halt();
            }
            report_contended_lock_and_halt(holder);
        }
        // 勝った側だけがここへ来る。**保持者を記録するのはフラグを立てた後**で、
        // 解放では逆順に落とす。
        self.holder.store(cpu_id(), Ordering::Relaxed);

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
        //
        // **保持者を先に消し、フラグを後で落とす。** 逆にすると、フラグが
        // 落ちた後も保持者が残る窓ができ、次に取ったコアが上書きするまでの間、
        // 診断が古い値を指す。
        self.lock.holder.store(NO_HOLDER, Ordering::Relaxed);
        self.lock.acquired.store(false, Ordering::Release);
        // ここで暗黙に `interrupts` が落ち、保存状態に応じて sti する。
    }
}

/// 二重取得をシリアルへ報告して停止する。
///
/// パニックハンドラや例外ハンドラと同じく、確保もロックもコンソールも
/// 使わずにシリアルへ直接書く。ヒープのロックで二重取得が起きた場合、
/// panic 経路が確保を試みるとさらに壊れるため（ADR-0004、ADR-0012）。
/// 別のコアが保持しているロックを取ろうとしたことを報告して停止する（S4-b-1）。
///
/// # なぜ二重取得と分けるのか
///
/// **原因が違う。** 二重取得は「同じコアが保持したまま再度呼んだ」コードのバグで、
/// 競合は「別のコアが同時に触った」である。**どちらも停止するが、直し方が違う。**
/// 同じ文言で報告すると、ログを読む人が誤った方向を調べることになる。
///
/// # この段では起きない
///
/// 本番経路で `Locked<T>` を触るのは bootstrap processor だけなので、
/// **この関数はまだ呼ばれない。** 呼ばれうるのは BKL が入る段からである。
fn report_contended_lock_and_halt(holder: usize) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[ERROR] lock: contended acquisition detected (a Locked<T> is held by cpu {holder} \
         while cpu {} tried to take it)",
        cpu_id()
    );
    let _ = writeln!(
        serial,
        "[ERROR]   this is NOT the same as a double acquisition: the holder is another core, \
         so the fix is exclusion between cores, not a re-entrant call path"
    );
    let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");
    cpu::halt_forever();
}

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
