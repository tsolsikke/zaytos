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
use common::percpu::cpu_id;
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
