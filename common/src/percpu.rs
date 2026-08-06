//! CPUごとに1つずつ値を持つ器と、自コアの番号。
//!
//! per-CPUデータは [`PerCpu`] に入れ、[`cpu_id`] で自分のスロットを引く。
//! 直接 `static` を触らずここを通すことで、番号の求め方を差し替えるときに
//! 触る面が [`cpu_id`] の1箇所で済む。
//!
//! 番号の実装は起動時に [`install_cpu_id_reader`] で据える。据える前は `0` を返す。
//!
//! 設計の経緯（seamを先に作った理由、GS化が何を与えるか）は ADR-0023 §3。

use core::sync::atomic::{AtomicUsize, Ordering};

/// サポートするCPU数の上限。
///
/// # 上げるときに壊れるもの
///
/// - `acpi-smp-test smp4-more-cpus-than-slots` が `MAX_CPUS < 4` に依存する。
///   覆いの報告の「覆えていない」側を評価する唯一の構成で、`4` 以上へ上げると
///   その枝が通らなくなる。上げるなら `-smp 8` の構成を同時に用意する。
/// - イメージへの影響は、S4-c-1 の再測の結論が S4-c-2 で失効している。
///   再測せずに上げないこと（`docs/deferred-decisions.md`）。
/// - `[T; MAX_CPUS]` は隣接要素が同一キャッシュラインに載りうる。 false sharing は
///   [`cpu_id`] をGS読みへ変えても残る（索引が残るため）。解消するには
///   コアごとの別領域へ置く形へ移る（`docs/deferred-decisions.md`）。
pub const MAX_CPUS: usize = 2;

/// 自コアのCPU番号を読む実装。`0` は未設定を表す。
///
/// 関数ポインタの値が `0` になることはないので、有効な実装と衝突しない。
static CPU_ID_READER: AtomicUsize = AtomicUsize::new(READER_NOT_INSTALLED);

/// [`CPU_ID_READER`] が未設定であることを表す値。
const READER_NOT_INSTALLED: usize = 0;

/// 自コアのCPU番号を読む実装を据える。
///
/// 実IDの出所は Local APIC で、そのアドレスとビット位置は `kernel` の知識である。
/// `common` は bootloader からも使うので、器だけをここに置き中身は `kernel` が据える。
///
/// # Safety
///
/// - `reader` は常に `0..`[`MAX_CPUS`] の値を返すこと。範囲外を返すと
///   [`PerCpu::this_cpu_ptr`] が配列外を指す。
/// - 割り込み文脈から呼ばれうる。 `reader` は再入可能で、ロックを取らず、
///   パニックしないこと。
/// - 起動時に1回だけ呼ぶこと。
pub unsafe fn install_cpu_id_reader(reader: fn() -> usize) {
    CPU_ID_READER.store(reader as usize, Ordering::Relaxed);
}

/// 実装が据えられているか。
///
/// 据える前と後で [`cpu_id`] の戻り値が変わらない構成があるので、
/// 値だけでは区別できない。それを区別するために公開している。
pub fn cpu_id_reader_installed() -> bool {
    CPU_ID_READER.load(Ordering::Relaxed) != READER_NOT_INSTALLED
}

/// 現在実行中のCPUの番号（`0..MAX_CPUS`）。
///
/// # 据える前は `0` を返す
///
/// GDT/TSS の構築は Local APIC を写像するより前に [`PerCpu::this_cpu_ptr`] を通る。
/// その時点で走っているのは bootstrap processor だけなので `0` が正しい。
///
/// # `0` を返すことに依存しない
///
/// 「どうせ `0` だから」で境界検査を省いたり先頭要素を直接触ったりすると、
/// `MAX_CPUS > 1` にした瞬間に静かに壊れる。索引は必ずこの関数の戻り値を使う。
#[inline]
pub fn cpu_id() -> usize {
    cpu_id_inner()
}

/// [`cpu_id`] の本体。
#[inline]
fn cpu_id_inner() -> usize {
    let raw = CPU_ID_READER.load(Ordering::Relaxed);
    if raw == READER_NOT_INSTALLED {
        return 0;
    }
    // SAFETY: `install_cpu_id_reader` は `fn() -> usize` の値だけを格納し、
    // 未設定を表す 0 は関数ポインタとして現れない。呼び出し側契約により
    // reader は再入可能で範囲内の値を返す。
    let reader: fn() -> usize = unsafe { core::mem::transmute::<usize, fn() -> usize>(raw) };
    reader()
}

/// このコアが bootstrap processor か。
///
/// スロットの割り当ては `kernel::smp` が行い、BSP を除いた AP へ `1` から順に配る。
/// これは Local APIC ID が `0` であることとは別の事実である。
///
/// `cpu_id() == 0` を直接書かない。 「`0` は BSP」という知識が呼び出し側へ散ると、
/// 割り当ての規則を変えたときに追随しそこねる箇所が出る。
#[inline]
pub fn is_bootstrap_processor() -> bool {
    cpu_id() == BOOTSTRAP_PROCESSOR_SLOT
}

/// bootstrap processor に割り当てる per-CPU スロット。
pub const BOOTSTRAP_PROCESSOR_SLOT: usize = 0;

/// CPUごとに1つずつ値を持つ器。
///
/// 実データは `[T; MAX_CPUS]` で、[`this_cpu`][Self::this_cpu] が [`cpu_id`] で
/// 現在のCPUの要素を引く。`AtomicUsize` のような内部可変な `T` を入れれば
/// `&T` 経由で更新できる。`T: Sync` なら `PerCpu<T>` も `Sync` になり `static` に置ける。
///
/// # `#[repr(transparent)]` にする理由
///
/// `PerCpu<T>` のアドレスがそのまま先頭スロットのアドレスであることに依存する
/// 箇所がある（GDT/TSS を `sgdt`/`str` で読み戻して照合する）。`#[repr(Rust)]` では
/// フィールドオフセット 0 は言語仕様上の保証ではない。非ZSTフィールドが1つなので
/// `transparent` を付けられ、保証へ格上げできる。
#[repr(transparent)]
pub struct PerCpu<T> {
    slots: [T; MAX_CPUS],
}

impl<T> PerCpu<T> {
    /// 各CPUの初期値を与えて器を作る。`static` の初期化子に使える。
    pub const fn new(slots: [T; MAX_CPUS]) -> Self {
        Self { slots }
    }

    /// 現在のCPU（[`cpu_id`]）に対応する要素。
    #[inline]
    pub fn this_cpu(&self) -> &T {
        &self.slots[cpu_id()]
    }

    /// 指定したCPUのスロット。範囲外なら `None`。
    ///
    /// # per-CPU の原則の例外である
    ///
    /// per-CPUデータは各コアが自分のスロットだけを触るものだが、会計と
    /// ハートビートは他コアのスロットを読む。 成立の条件は読みだけであることと、
    /// 読む側が数の正しさしか要求しないことである。順序に依存した判断へ使わない。
    #[inline]
    pub fn slot(&self, index: usize) -> Option<&T> {
        self.slots.get(index)
    }

    /// 現在のCPU（[`cpu_id`]）のスロットへの生ポインタ。
    ///
    /// [`this_cpu`][Self::this_cpu] が `&T` を返すのに対し、こちらは可変な書き込み
    /// （GDT/TSS の構築、RSP0 の更新）向けに生ポインタを返す。参照を作らずに
    /// スロットを書き換える既存の規律に合わせる。
    ///
    /// # 関数自身の不変条件
    ///
    /// `cpu_id() < MAX_CPUS` が成り立たないと配列外へのポインタ演算になる。
    /// [`cpu_id`] はこの関数の内部で呼ぶので、これは呼び出し側の契約ではなく
    /// この関数自身の不変条件である。 誰が保証するかは
    /// `docs/deferred-decisions.md` の per-CPU seam の項。
    ///
    /// # Safety
    ///
    /// - `this` は有効で整列済みの、生きている `PerCpu<T>` を指すこと。典型的には
    ///   `core::ptr::addr_of_mut!(STATIC)` から得る。
    /// - 返した生ポインタを通じた書き込みは、そのスロットへ他の実行文脈が同時に
    ///   アクセスしない区間（起動時の単一文脈、または `IF=0` の区間）で行うこと。
    ///   同一コアの割り込みハンドラは同じスロットへ触れるので、per-CPU でも
    ///   競合しないとは言えない。
    /// - アドレス計算のみ（deref せず `as u64` 等）はこの区間要件を要しない。
    ///   deref して中身を読む場合は書き込みと同じ区間要件が要る。
    #[inline]
    pub unsafe fn this_cpu_ptr(this: *mut Self) -> *mut T {
        // SAFETY: 呼び出し側契約により `this` は有効。addr_of_mut! で参照を作らずに
        // 先頭要素を取り、cpu_id() 分ずらす。上の不変条件により配列内に収まる。
        unsafe {
            let base = core::ptr::addr_of_mut!((*this).slots) as *mut T;
            base.add(cpu_id())
        }
    }

    /// 指定したスロットへの生ポインタ。
    ///
    /// [`this_cpu_ptr`][Self::this_cpu_ptr] は [`cpu_id`] を呼ぶが、`cpu_id()` が
    /// まだ正しくない文脈がある——AP は自分の GDT をロードするまで [`cpu_id`] を
    /// 使えないのに、その GDT を書くのにスロットを指す必要がある。
    /// 索引を外から与える経路をここで開ける。
    ///
    /// # Safety
    ///
    /// [`this_cpu_ptr`][Self::this_cpu_ptr] の契約に加えて、`index < MAX_CPUS`。
    /// [`cpu_id`] を通さないので、範囲を保証するのは呼び出し側である。
    #[inline]
    pub unsafe fn slot_ptr(this: *mut Self, index: usize) -> *mut T {
        debug_assert!(index < MAX_CPUS, "per-CPU slot index out of range");
        // SAFETY: 呼び出し側契約により `this` は有効で `index < MAX_CPUS`。
        unsafe {
            let base = core::ptr::addr_of_mut!((*this).slots) as *mut T;
            base.add(index)
        }
    }
}
