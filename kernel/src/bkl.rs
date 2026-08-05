//! ビッグカーネルロック（S4-b-2、ADR-0023 とその Addendum）。
//!
//! # 何を守るか
//!
//! **カーネル内へ同時に入れるのは 1 コアだけにする。** 各サブシステム
//! （スケジューラ・アロケータ・ページング・ドライバ）を並行対応へ書き換えずに、
//! 正しいマルチコアを成立させるための単一の大域ロックである。
//!
//! # 取得と解放の点
//!
//! ADR-0023 §1 の定義は「カーネル入口で取り、ユーザー空間へ戻るときに離す」だが、
//! **このカーネルには定常的な Ring 3 が無い。** 等価物は「入口で取り、その入口から
//! 戻るときに離す」である（Addendum §1）。分類は [`KernelEntry`] に持つ。
//!
//! **定常ループは入口ではない。** Ring 0 のまま `hlt` で待つループなので、
//! 1 周の中の共有物を触る区間だけを保持する（Addendum §2）。
//! **ガードのスコープに `hlt` を含めないので、保持したまま `hlt` することが
//! 構造的に起きない。**
//!
//! # 保持区間は IF=0 である
//!
//! 取得は `cli` してからフラグを立てる。**逆にすると、フラグを立ててから `cli`
//! するまでの窓に割り込みが入り、同じコアが再帰する**（`Locked::lock` が同じ順序を
//! 同じ理由で採っている）。
//!
//! **この不変条件が成り立つ限り、保持中に割り込みが入らないので再帰は起きない。**
//! したがって再帰を許す形にせず、起きたら停止する。
//!
//! # 数えるのは深さではない
//!
//! `common::critical::EntryInterruptGuard` を使う。`InterruptGuard` を使うと
//! `CRITICAL_NESTING_DEPTH` が増え、`task::on_timer_tick` の防御スキップと
//! `task::on_yield` の判定が壊れる（あちらの doc）。
//!
//! # ロックの順序
//!
//! **BKL → `Locked<T>` の一方向だけである。** `Locked<T>` を保持したまま BKL を
//! 取る経路は作らない。単一の BKL と葉の `Locked<T>` しか無いので、これだけで
//! 順序は閉じる。

use core::fmt::Write as _;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use common::critical::EntryInterruptGuard;
use common::percpu::{cpu_id, PerCpu, MAX_CPUS};
use common::serial::SerialPort;

/// カーネル入口の分類（S4-b-2）。
///
/// # 取らない入口も知っている必要がある
///
/// **例外・ダブルフォルト・パニックは BKL を取らない。** これは
/// [`NON_ACQUIRING_ENTRIES`] に理由つきで並べてある。**書かなければ、後から
/// 「取り忘れ」として足されうる。**
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KernelEntry {
    /// `irq_entry`。タイマ・キーボード・テストベクタ・スプリアス・yield。
    Irq,
    /// `syscall_entry`（`int 0x80`）。
    Syscall,
    /// AP がカーネルの共有物へ触り始める直前の一度きり。
    ApBringUp,
    /// Ring 0 の定常ループのうち、共有物を触る区間。
    SteadyLoop,
}

impl KernelEntry {
    /// ログへ出す名前。
    pub const fn name(self) -> &'static str {
        match self {
            KernelEntry::Irq => "Irq",
            KernelEntry::Syscall => "Syscall",
            KernelEntry::ApBringUp => "ApBringUp",
            KernelEntry::SteadyLoop => "SteadyLoop",
        }
    }

    /// 診断フィールドへ詰める値。
    const fn as_u64(self) -> u64 {
        match self {
            KernelEntry::Irq => 0,
            KernelEntry::Syscall => 1,
            KernelEntry::ApBringUp => 2,
            KernelEntry::SteadyLoop => 3,
        }
    }

    /// [`Self::as_u64`] の逆。範囲外は `None`。
    const fn from_u64(raw: u64) -> Option<Self> {
        match raw {
            0 => Some(KernelEntry::Irq),
            1 => Some(KernelEntry::Syscall),
            2 => Some(KernelEntry::ApBringUp),
            3 => Some(KernelEntry::SteadyLoop),
            _ => None,
        }
    }
}

/// **BKL を取らない入口と、その理由。**
///
/// # 不在を記録する
///
/// 取る入口だけを列挙すると、ここに挙がっているものが「まだ実装していないだけ」に
/// 見える。**取らないのは判断であって未実装ではない**ので、理由と対で残す。
/// `Apic::is_spurious` が「常に false である理由」を 2 つ残しているのと同じ形である。
pub const NON_ACQUIRING_ENTRIES: &[(&str, &str)] = &[
    (
        "exception",
        "exception_entry は -> ! で戻らない。取れば、保持者が例外で死んだときに \
         他コアが永久に待つ",
    ),
    (
        "double fault",
        "同上（例外の一種として同じ扱い）。IST 上で走るので、なおさら戻らない",
    ),
    (
        "panic",
        "同上。加えて確保もロックもコンソールも使わずにシリアルへ直接書く経路である \
         （ADR-0004 / ADR-0012）",
    ),
];

/// 単一の大域ロック。
struct BigKernelLock {
    /// 保持されているか。
    held: AtomicBool,
    /// 保持しているコア。**診断専用**で、[`NO_HOLDER`] は未保持。
    holder_cpu: AtomicUsize,
    /// 保持者が入った入口。**診断専用。**
    holder_entry: AtomicU64,
    /// 取得した時刻（TSC）。**診断専用。**
    acquired_tsc: AtomicU64,
}

/// [`BigKernelLock::holder_cpu`] の「誰も保持していない」。
const NO_HOLDER: usize = usize::MAX;

/// 唯一の実体。
static BKL: BigKernelLock = BigKernelLock {
    held: AtomicBool::new(false),
    holder_cpu: AtomicUsize::new(NO_HOLDER),
    holder_entry: AtomicU64::new(0),
    acquired_tsc: AtomicU64::new(0),
};

/// 待ちの上限（TSC サイクル）。
///
/// # 時間源は TSC しかない
///
/// **ティックは BKL の中で数えるので、待っている側は使えない。**
/// 前例は `interrupts::FIRST_TICK_TIMEOUT_CYCLES` で、同じ桁にしてある。
///
/// **上限のない待機ループを書かない**（`CLAUDE.md` §14 と同じ規律が、
/// カーネルの中でも同じ理由で要る）。デッドロックしたときに静かに止まらないよう、
/// 上限に達したら原因を出して停止する。
const WAIT_TIMEOUT_CYCLES: u64 = 20_000_000_000;

/// 破壊 `bkl-widen-entry-window` が入口へ入れるスピン回数（S4-b-4）。
///
/// # **これは増幅器であって、素の重なりの頻度ではない**
///
/// 広げた窓での重なりを見ているのであって、**既定ビルドで重なる頻度とは別である。**
/// **「BKL が無ければ常に 2 になる」を意味しない。**
///
/// 素の重なりの実測は次のとおりである。
///
/// - KVM: 4 回のうち 3 回で観測、1 回は 116 秒（約 23,000 回の入口通過）で観測できず
/// - TCG: 86 秒（17,060 ティック）で 0 回。**3,000 スピンまで広げても 0 回**
///
/// # 幅の根拠
///
/// **既知の両端**——TCG で 3,000 では 0 回、60,000 では起動が定常状態へ到達しない。
/// **中間は未測定である。** 60,000 で起動しない機序も**未特定**である
/// （窓が 100Hz の周期を食い潰している可能性があるが、確かめていない）。
/// **KVM での幅は着手時に測って決める。**
#[cfg(feature = "bkl-widen-entry-window-test")]
pub const WIDENED_ENTRY_WINDOW_SPINS: u64 = 20_000;

/// BKL を保持している間だけ生きるガード。
///
/// # なぜ RAII なのか
///
/// `irq_entry` には早期 return が複数ある（スプリアス・LAPIC タイマ・yield）。
/// **解放を各 return の手前へ書く形にすると、1 つ落としたときに保持したまま
/// 戻る。** そのコアは次に入れず、他コアも入れないので系全体が止まる。
/// 規律ではなく構造で対にする。
///
/// # `hlt` をスコープに含めないこと
///
/// 定常ループはこのガードのスコープの外で `hlt` する。**保持したまま `hlt` すると、
/// もう一方のコアが IF=0 で永久に待つ。** ガードの寿命がブロックで決まるので、
/// `hlt` をブロックの外に置けば構造的に起きない（Addendum §2）。
pub struct BklGuard {
    /// 同時進入の計数（S4-b-3）。**`held` を落とす前に、明示的に落とす。**
    ///
    /// # `Option` にしている理由
    ///
    /// **`Drop for BklGuard` はフィールドの drop より先に走る。** したがって
    /// フィールドとして持つだけでは「`held` を落としてから数を抜ける」順序になり、
    /// **その窓で別のコアが取得して数を増やすと、排他が効いているのに 2 と
    /// 読めてしまう。** 同時進入の観測そのものが壊れる。
    ///
    /// `Option` にして [`Drop`] の先頭で `take()` すれば、**数から抜けてから
    /// フラグを落とす**順序を明示できる。
    entered: Option<crate::idt::KernelEntryGuard>,
    /// 読み出さないが、保持していること自体に意味がある（Drop で割り込みを復元する）。
    ///
    /// **最後のフィールドであることが drop 順の要件である。** BKL のフラグを
    /// 落としてから割り込みを復元する。逆にすると、まだ保持者として記録されている
    /// 区間で割り込みが入りうる（`LockGuard` と同じ形）。
    #[allow(dead_code)]
    interrupts: EntryInterruptGuard,
    _not_send_sync: PhantomData<*const ()>,
}

/// BKL を取る。**戻り値のガードが生きている間だけ保持される。**
///
/// # Safety
///
/// 安全な関数である。**ただし呼び出し側は、このガードを `hlt` を含む区間へ
/// 持ち込まないこと。** それは型では防げない（doc とレビューで守る）。
/// **写像が変わった世代（S5-b）。** 写像を変えたコアが、**BKL を保持したまま**
/// 上げる。
///
/// # 何のためにあるか
///
/// **ack を待たずに TLB の整合を取るためである。** 送信側が待機中のコアへ ack を
/// 求めると、**待機側は IF=0 でスピンしているので応答できず、デッドロックする**
/// （`docs/deferred-decisions.md` の「BKL取得待ちのIF=0とIPIのデッドロック」）。
///
/// **代わりに、取得する側が自分でフラッシュする。** コアの状態は
/// **(i) タスク実行中で IF=1（IPI が届く）**、
/// **(ii) BKL 待ちで IF=0（取得するまで写像を使わない）**、
/// **(iii) BKL 保持中（変更した本人）** のいずれかで、
/// **(ii) は取得時のフラッシュで閉じる。** したがって
/// **待機中のコアから ack を待つ必要が消える。**
///
/// # **この 3 つが尽きている根拠は篩にある**
///
/// **自明ではない。** BKL を保持も待機もしない IF=0 の区間は他にもある
/// （`Locked<T>` のクリティカル区間など）。**それらが問題にならないのは、
/// 「AP が起きた後に到達しうるか」「BKL を保持せずに到達しうるか」の 2 問で
/// 篩うと既定ビルドでは 0 件になるからである**
/// （`docs/verification-coverage.md` の「S5の機構は、現時点では発火条件の無い
/// 備えである」）。**篩の結果に依存しているので、篩が変われば列挙も変わる。**
///
/// 篩に現れなかった 2 つについても書いておく。
///
/// - **AP 起こしの途中**（`ap_after_switch`）。CR3 を積み直した直後で、
///   **`KernelEntry::ApBringUp` の取得を通ってから定常の仕事に入る。**
///   その取得でフラッシュを通るので、**古い翻訳を持ち越さない。**
/// - **例外・ダブルフォルト・パニックの経路**（ADR-0023 により BKL を取らない）。
///   **これらは戻らない経路である**（ダンプして停止する）。**戻って写像を使い続ける
///   ことが無いので、古い翻訳が問題にならない。** **戻る経路を作ったら失効する。**
///
/// # 世代が正しさの土台であり、IPI は早めるための手段である
///
/// **入口はティックごとに BKL を取る**ので、走っている全コアは 100Hz で必ず
/// [`acquire`] を通る。**したがって世代方式だけで「遅くとも 1 ティックで整合する」
/// が保証される。** IPI はそれを早めるだけなので、**IPI が 1 つ落ちても正しさは
/// 保たれる。**
///
/// # 追加のバリアが要らない理由
///
/// **順序は BKL 自身が与える。** 変更側は**保持したまま**写像を外して世代を上げ、
/// 解放は `held.store(false, Ordering::Release)` である。取得側は
/// `held.swap(true, Ordering::Acquire)` を通るので、**解放を観測してからでないと
/// 取得できない。** Acquire/Release の対がそのまま happens-before を作るので、
/// **取得した側は必ず上がった世代を読む。**
/// **`acquire` の順序を触るときは、この依存を壊していないか見ること。**
///
/// # 失効条件
///
/// **ack 無しが健全なのは「解放も再利用もしない」間だけである。** 写像を外すだけなら、
/// 古い翻訳が残っていても指す先は同じフレームで差は観測されない。**外したフレームを
/// アロケータへ返して再利用すると、その窓で古い翻訳が生きたページを別用途に
/// 使われる。** **解放・再利用を伴う操作を導入したら、この設計は失効する。**
static TLB_GENERATION: AtomicU64 = AtomicU64::new(0);

/// このコアがフラッシュ済みの世代（S5-b）。
static SEEN_GENERATION: PerCpu<AtomicU64> = PerCpu::new([const { AtomicU64::new(0) }; MAX_CPUS]);

/// このコアが世代の食い違いで行ったフラッシュの回数（S5-b）。**観測用。**
static GENERATION_FLUSHES: PerCpu<AtomicU64> = PerCpu::new([const { AtomicU64::new(0) }; MAX_CPUS]);

/// **写像を変えたことを知らせる（S5-b）。BKL を保持したまま呼ぶこと。**
///
/// **現時点で本番の呼び出し元は無い。** 写像を変える操作はすべて AP が起きる前の
/// 単一コアの区間にあるためである（`verification-coverage.md`）。
/// **呼び出し元ができるのは、実行中に写像を変える段（S5-c 以降）である。**
pub fn note_mapping_changed() {
    TLB_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// 現在の世代（S5-b）。**観測用。**
pub fn tlb_generation() -> u64 {
    TLB_GENERATION.load(Ordering::Relaxed)
}

/// 指定コアのフラッシュ回数（S5-b）。**範囲外は 0。**
pub fn generation_flushes_for(cpu: usize) -> u64 {
    GENERATION_FLUSHES
        .slot(cpu)
        .map_or(0, |slot| slot.load(Ordering::Relaxed))
}

/// 指定コアが見た世代（S7-b）。**範囲外は 0。**
///
/// **`generation_flushes_for` と同じ形にしてある。** あちらは観測用だが、
/// **こちらは判定に使う**——[`generation_is_retired`] が「誰の TLB にも古い翻訳が
/// 残っていない」を導く入力である。
pub fn seen_generation_for(cpu: usize) -> u64 {
    SEEN_GENERATION
        .slot(cpu)
        .map_or(0, |slot| slot.load(Ordering::Relaxed))
}

/// **世代 `generation` の時点の翻訳が、どのコアにも残っていないか（S7-b）。**
///
/// ADR-0027 の Addendum の不変条件——**フレームは、全コアの `SEEN_GENERATION` が
/// 解放時の世代を追い越すまで配られない**——の判定である。
///
/// # 対象は「参加しているコア」である
///
/// **起きていないコアの `SEEN_GENERATION` は 0 のままである。** 素直に `MAX_CPUS`
/// まで見ると、**`-smp 1` では永久に真にならない。** 対象は bootstrap processor と
/// **起こした AP** である（`smp::started_ap_count`）。
///
/// # 後から起きた AP を待つのは、安全側の空振りである
///
/// AP が起きるのは解放より後でも、そのコアの `SEEN_GENERATION` は 0 から始まる。
/// **したがって判定は「まだ追い越していない」と読み、余計に待つ。**
/// **待ちすぎるのは安全側である**——**早すぎることだけが危険で、遅いのは遅いだけ。**
/// **正しさは、後から起きたコアが古い翻訳を持たないことに依存していない。**
///
/// # BKL を保持したまま呼ぶこと
///
/// 判定と再利用の間に他コアが割り込むと、追い越しの判定が古くなる
/// （Addendum の失効条件）。
pub fn generation_is_retired(generation: u64) -> bool {
    let participating = 1 + crate::smp::started_ap_count();
    (0..participating).all(|cpu| seen_generation_for(cpu) > generation)
}

/// **自コアの見た世代が古ければ TLB を落とす（S5-b）。**
///
/// [`acquire`] が勝った直後に呼ぶ。**取得してから写像を使い始めるまでの間に置く**
/// ので、**古い翻訳のまま走り出す経路が無い。**
///
/// # 既定ビルドに在ることは何が主張しているか
///
/// **主たる論拠は構造の側である**——ここにも [`acquire`] からの呼び出しにも
/// **`cfg` が付かない**ので、構成によらず在る。
/// **`cargo xtask check` がその裏取りをする**（既定ビルドのバイナリにこの関数の
/// シンボルが在ることを見る）。**`#[inline(never)]` はそのために付けてある。**
#[inline(never)]
fn flush_if_generation_is_stale() {
    let current = TLB_GENERATION.load(Ordering::Relaxed);
    let seen = SEEN_GENERATION.this_cpu();
    if seen.load(Ordering::Relaxed) == current {
        return;
    }
    // **CR3 の載せ替えで全部落とす。変わった範囲が分からないので `invlpg` は使えない。**
    //
    // **載せ替えでグローバルページは落ちない。** G ビットが立っているエントリは
    // CR3 の書き換えでは無効化されない（CR4.PGE のトグルか `invlpg` が要る）。
    // **本カーネルは G ビットを立てない**ので載せ替えで足りるが、
    // **それは仮定ではなく起動時に検査している**——`CR4` と PGE を読み出して出し
    // （実測で `CR4 = 0x668, PGE(bit 7) = false`）、**写像に G ビットが 1 つでも
    // 立っていたら停止する**（`main.rs`）。
    // **失効条件——CR4.PGE を使い始めるか、G ビットを立て始めたら、
    // ここは載せ替えでは足りなくなる。**
    // **読み→フラッシュ→書き戻しが不可分でなくてよい。**
    // **世代を上げられるのは BKL を保持しているコアだけ**で、この列は BKL を
    // 取った後に走る。**したがってこの間に誰も上げられない。**
    // **これは「順序は BKL 自身が与える」（[`TLB_GENERATION`] の doc）とは
    // 別の命題である**——あちらは可視性、こちらは排他の話である。
    let cr3 = crate::paging::switch::read_cr3();
    // SAFETY: 今読んだ値をそのまま書き戻すだけで、写像は変えない。
    unsafe { crate::paging::switch::switch_to(cr3) };
    seen.store(current, Ordering::Relaxed);
    GENERATION_FLUSHES
        .this_cpu()
        .fetch_add(1, Ordering::Relaxed);
}

#[must_use = "ガードを保持している間だけ BKL を保持する。すぐ drop すると即解放される"]
pub fn acquire(entry: KernelEntry) -> BklGuard {
    // **1. cli が先である。** 逆にすると、フラグを立ててから cli するまでの窓に
    // 割り込みが入り、同じコアが再帰する。
    let interrupts = EntryInterruptGuard::enter();

    // 2. 取れるまで待つ。**競合は最初から常時ある**（両コアが 100Hz で入る）。
    let started = common::cpu::read_timestamp_counter();
    while BKL.held.swap(true, Ordering::Acquire) {
        // **自分が保持者なら再帰である。**
        //
        // 保持区間は IF=0 なので、保持中に割り込みが入って同じコアが再入する
        // ことはない。**したがってここへ来るのはバグである。**
        if BKL.holder_cpu.load(Ordering::Relaxed) == cpu_id() {
            report_recursive_acquire_and_halt(entry);
        }
        if common::cpu::read_timestamp_counter().wrapping_sub(started) > WAIT_TIMEOUT_CYCLES {
            report_timeout_and_halt(entry, started);
        }
        core::hint::spin_loop();
    }

    // 3. 勝った側だけがここへ来る。診断フィールドを埋める。
    BKL.holder_cpu.store(cpu_id(), Ordering::Relaxed);
    BKL.holder_entry.store(entry.as_u64(), Ordering::Relaxed);
    BKL.acquired_tsc
        .store(common::cpu::read_timestamp_counter(), Ordering::Relaxed);

    // **写像が変わっていれば、ここで落とす（S5-b）。**
    // **取得してから写像を使い始めるまでの間である。**
    flush_if_generation_is_stale();

    // **同時進入をここで数える（S4-b-3）。** 定義は「取得してから解放するまでの
    // 区間にいるコアの数」なので、**待っている間は入らない。**
    let entered = Some(crate::idt::KernelEntryGuard::enter());

    BklGuard {
        entered,
        interrupts,
        _not_send_sync: PhantomData,
    }
}

impl Drop for BklGuard {
    fn drop(&mut self) {
        // **1. 数から抜ける（S4-b-3）。フラグを落とす前である。**
        //
        // 逆にすると、フラグが落ちてから数が減るまでの窓で別のコアが取得し、
        // **排他が効いているのに同時進入数が 2 と読める。** 観測が壊れる。
        drop(self.entered.take());
        // 2. 診断を消す。**フラグより先である**（フラグが落ちた後も保持者が
        // 残る窓ができると、次に取ったコアが上書きするまで診断が古い値を指す）。
        BKL.holder_cpu.store(NO_HOLDER, Ordering::Relaxed);
        // 3. フラグを落とす。ここから他コアが取れる。
        BKL.held.store(false, Ordering::Release);
        // ここで暗黙に `interrupts` が落ち、保存状態に応じて復元する。
    }
}

/// 保持者の情報を読む。**診断専用で、読んだ瞬間に変わっていることがある。**
fn holder_snapshot() -> (usize, &'static str, u64) {
    let cpu = BKL.holder_cpu.load(Ordering::Relaxed);
    let entry = KernelEntry::from_u64(BKL.holder_entry.load(Ordering::Relaxed))
        .map_or("<unknown>", KernelEntry::name);
    (cpu, entry, BKL.acquired_tsc.load(Ordering::Relaxed))
}

/// 同じコアが保持したまま再取得したことを報告して停止する。
fn report_recursive_acquire_and_halt(entry: KernelEntry) -> ! {
    let (holder_cpu, holder_entry, since) = holder_snapshot();
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[ERROR] bkl: recursive acquisition on cpu {} at entry {} (already held by cpu \
         {holder_cpu} at entry {holder_entry} since tsc {since})",
        cpu_id(),
        entry.name()
    );
    let _ = writeln!(
        serial,
        "[ERROR]   the BKL is held with IF=0, so no interrupt can re-enter on the same core; \
         reaching here means that invariant is broken"
    );
    let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");
    common::cpu::halt_forever();
}

/// 待ちが上限に達したことを報告して停止する。
///
/// # 「静かに止まる」の反対を出す
///
/// **どのコアが・どの入口で・何サイクル待っているか**と、
/// **誰が・どの入口で・いつから保持しているか**を出す。
/// 保持者側は診断なので、古い値でありうることも書く。
fn report_timeout_and_halt(entry: KernelEntry, started: u64) -> ! {
    let waited = common::cpu::read_timestamp_counter().wrapping_sub(started);
    let (holder_cpu, holder_entry, since) = holder_snapshot();
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[ERROR] bkl: cpu {} has waited {waited} cycles at entry {} without acquiring the lock",
        cpu_id(),
        entry.name()
    );
    let _ = writeln!(
        serial,
        "[ERROR]   the lock reads as held by cpu {holder_cpu} at entry {holder_entry} since \
         tsc {since} (holder fields are diagnostic and may be stale)"
    );
    let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");
    common::cpu::halt_forever();
}

/// 破壊 (S4-b-4, bkl-skip-timer-entry): **ロックは取らず、計数だけ行う。**
///
/// # 数えているものが本番と違う
///
/// **S4-b-3 の定義は「BKL を取得してから解放するまでの区間」である。**
/// 取得を飛ばせば、その定義では区間そのものが存在しない。**それでは
/// 「取らなければ 2 になる」を観測できない。**
///
/// そこでこの破壊は、**ロックだけを飛ばして計数は残す。** したがって
/// **破壊ビルドのカウンタが数えているのは「守られるはずだった区間」であり、
/// 本番の定義とは別物である。**
///
/// **増分の位置**——`irq_entry` の先頭（本番で `acquire` を呼ぶのと同じ位置）で
/// 増え、同じガードの drop で減る。**区間の始まりと終わりは本番と同じで、
/// 違うのは「その区間が排他されているかどうか」だけである。**
/// だから「取らなければ 2 になる」の比較が成立する。
///
/// **IF=0 は保つ。** 割り込みゲート経由で入るので元から IF=0 だが、
/// ガードを取ることで本番と同じ状態にしてある。**変える軸を 1 つに絞る。**
#[cfg(feature = "bkl-skip-timer-entry-test")]
#[must_use = "ガードを保持している間だけ数えられる"]
pub fn acquire_counting_only(_entry: KernelEntry) -> BklGuard {
    let interrupts = EntryInterruptGuard::enter();
    let entered = Some(crate::idt::KernelEntryGuard::enter());
    BklGuard {
        entered,
        interrupts,
        _not_send_sync: PhantomData,
    }
}

/// 破壊 (S4-b-4, bkl-hold-forever): BKL を取ったまま二度と離さない。
///
/// # タイムアウトの経路を通す唯一の形
///
/// 再帰検出は**同じコアが**取ろうとしたときに発火する。既存の 2 破壊はどちらも
/// そちらが先に鳴るので、**待ちの上限に達する経路は一度も通っていない。**
/// **別のコアが解放しないまま保持し続ける**形が要る。
///
/// **同時実行を要さない。** 待っている側の TSC が進めばよい。
///
/// # Safety
///
/// **戻らない。** 呼んだコアはそこで止まる。既定ビルドには存在しない。
#[cfg(feature = "bkl-hold-forever-test")]
pub fn sabotage_hold_forever() -> ! {
    let guard = acquire(KernelEntry::ApBringUp);
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[WARN] bkl: cpu {} took the lock and will never release it (sabotage); another core \
         should time out",
        cpu_id()
    );
    // **離さない。** ガードを忘れることで解放を起こさない。
    core::mem::forget(guard);
    // **IF=0 のまま止まる。** 割り込みで抜けると解放が走りうる形にしない。
    common::cpu::halt_forever()
}

/// 破壊 (S4-b-2, bkl-hold-with-if-set): 保持したまま IF=1 にする。
///
/// **不変条件「保持区間 = IF=0」そのものの破壊確認である。** IF=1 で保持すると
/// タイマが入り、同じコアが `irq_entry` から BKL を取ろうとして再帰検出が発火する。
///
/// # Safety
///
/// BKL を保持している区間から呼ぶこと。**既定ビルドには存在しない。**
#[cfg(feature = "bkl-hold-with-if-set-test")]
pub unsafe fn sabotage_enable_interrupts_while_held() {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[WARN] bkl: enabling interrupts while holding the lock (sabotage); the recursion \
         check should fire on the next tick"
    );
    // SAFETY: 破壊 feature 専用。呼び出し側が BKL を保持している。
    unsafe { common::cpu::enable_interrupts() }
}
