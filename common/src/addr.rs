//! 物理アドレスと仮想アドレスの型（T-1）。
//!
//! **純粋ロジック。** ホスト `cargo test` で検証できる。
//!
//! # なぜ分けるのか
//!
//! 現在のカーネルは恒等マッピングで動いており、物理アドレスと仮想アドレスが
//! 同じ値である。そのため両者を `u64` のまま扱っても動いてしまう。
//! higher-half へ移行した瞬間に、この「たまたま同じ」が成立しなくなる。
//!
//! 取り違えは静かに壊れる形で出る。ページテーブルへ仮想アドレスを書けば
//! 見当違いの物理ページを指し、生ポインタとして物理アドレスを参照すれば
//! 別の場所を読む。どちらもその場では落ちず、しばらく動いた後に壊れる。
//!
//! **恒等マッピングのうちに型だけ入れておけば、higher-half 移行で触るべき
//! 箇所をコンパイラが列挙してくれる。** 移行そのものは変えずに、見積もりの
//! 精度だけが上がる。
//!
//! # ポインタ化は [`VirtAddr`] からしかできない
//!
//! [`PhysAddr`] に `as_ptr` を用意していないのが、この分離の核心である。
//! 物理アドレスを生ポインタとして使いたければ [`DirectMap`] を通すしかなく、
//! そこが higher-half 移行で書き換わる 1 箇所になる。
//!
//! # 不変条件を破る入口を作らない
//!
//! `new_unchecked` のような検査を迂回する入口は用意しない。定数の組み立てには
//! [`PhysAddr::new_const`] / [`VirtAddr::new_const`] を使う。これらは不変条件を
//! 破ると panic するので、`const` 文脈で評価されればコンパイルエラーになる。
//! 実行時に呼ばれた場合も panic で、ADR-0004 の fail-fast と一貫する。

use core::fmt;

/// 物理アドレスがアドレスとして使えるビット数。
///
/// x86_64 のページテーブルエントリはビット 51 までしかアドレスを持てない
/// （ビット 52-62 は予約、63 は NX）。**実際の CPU の MAXPHYADDR とは別物
/// である。** MAXPHYADDR は 52 以下の任意の値を取りうるが、それを型の
/// 判定に使うと、実行時の値でコンパイル時の不変条件を決めることになり、
/// 同じバイナリが環境によって別の挙動をする。ここは 52 で固定し、
/// MAXPHYADDR は観測値としてログに出すだけにする
/// （`common::cpu::max_physical_address_bits`）。
pub const PHYS_ADDR_BITS: u32 = 52;

/// 物理アドレス。
///
/// ビット 52 以上が立った値は構築できない。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PhysAddr(u64);

/// 仮想アドレス。
///
/// 正規形（canonical、ビット 47 が 48-63 へ符号拡張）でない値は構築できない。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct VirtAddr(u64);

// **16 進で出す。** アドレスを 10 進で読める人はいない。ログの読みやすさが
// そのまま診断のしやすさになる。
impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#018x})", self.0)
    }
}

impl fmt::Debug for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtAddr({:#018x})", self.0)
    }
}

/// 値が物理アドレスとして表現できるか。
const fn is_valid_phys(raw: u64) -> bool {
    raw >> PHYS_ADDR_BITS == 0
}

/// 値が正規形（canonical）か。
///
/// ビット 47 が 48-63 へ符号拡張されていなければならない。非正規のアドレスは
/// CPU が拒否する（`mov` で #GP）。
pub const fn is_canonical(raw: u64) -> bool {
    ((raw as i64) << 16 >> 16) as u64 == raw
}

impl PhysAddr {
    /// 検査つきで作る。ビット 52 以上が立っていれば `None`。
    pub const fn new(raw: u64) -> Option<Self> {
        if is_valid_phys(raw) {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// 定数の組み立て用。不変条件を破ると panic する。
    ///
    /// `const` 文脈で評価されれば、panic はコンパイルエラーになる。
    /// 実行時に呼ばれた場合も panic で止まる（ADR-0004 の fail-fast）。
    pub const fn new_const(raw: u64) -> Self {
        assert!(
            is_valid_phys(raw),
            "physical address has bits set above bit 51"
        );
        Self(raw)
    }

    /// 生の値。
    ///
    /// **使ってよいのはログ出力と、ページテーブルエントリの符号化だけである。**
    /// それ以外の場所でこれを呼んでいたら、型を分けた意味が失われている。
    /// `grep as_u64` で追える状態を保つため、名前は変えない。
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// フレーム番号（4KiB 単位）。
    pub const fn frame_number(self) -> u64 {
        self.0 / 4096
    }

    /// フレーム番号から作る。桁溢れまたは範囲外なら `None`。
    pub const fn from_frame_number(frame: u64) -> Option<Self> {
        match frame.checked_mul(4096) {
            Some(raw) => Self::new(raw),
            None => None,
        }
    }

    pub const fn is_aligned(self, align: u64) -> bool {
        debug_assert!(align.is_power_of_two());
        self.0 & (align - 1) == 0
    }

    /// 切り上げ。**結果が物理アドレスの範囲を出るなら `None`。**
    ///
    /// ビット 51 付近の値を切り上げるとビット 52 へ繰り上がりうる。
    /// `Self` を返す形にすると、そこで不変条件が静かに壊れる。
    pub const fn align_up(self, align: u64) -> Option<Self> {
        debug_assert!(align.is_power_of_two());
        match self.0.checked_add(align - 1) {
            Some(sum) => Self::new(sum & !(align - 1)),
            None => None,
        }
    }

    /// 切り捨て。下へ丸めるだけなので範囲を出ることは無いが、
    /// [`Self::align_up`] と形を揃えて `Option` を返す。
    pub const fn align_down(self, align: u64) -> Option<Self> {
        debug_assert!(align.is_power_of_two());
        Self::new(self.0 & !(align - 1))
    }

    /// 加算。**桁溢れだけでなく、結果が物理アドレスの範囲に収まるかも見る。**
    pub const fn checked_add(self, offset: u64) -> Option<Self> {
        match self.0.checked_add(offset) {
            Some(sum) => Self::new(sum),
            None => None,
        }
    }

    pub const fn checked_sub(self, offset: u64) -> Option<Self> {
        match self.0.checked_sub(offset) {
            Some(difference) => Self::new(difference),
            None => None,
        }
    }
}

impl VirtAddr {
    /// 検査つきで作る。正規形でなければ `None`。
    pub const fn new(raw: u64) -> Option<Self> {
        if is_canonical(raw) {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// 定数の組み立て用。正規形でなければ panic する。
    ///
    /// [`PhysAddr::new_const`] と同じ考え方。
    pub const fn new_const(raw: u64) -> Self {
        assert!(is_canonical(raw), "virtual address is not canonical");
        Self(raw)
    }

    /// 生の値。制約は [`PhysAddr::as_u64`] と同じ。
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// 生ポインタにする。
    ///
    /// **[`PhysAddr`] には用意していない。** 物理アドレスを生ポインタとして
    /// 使いたければ [`DirectMap`] を通すしかない。そこが higher-half 移行で
    /// 書き換わる 1 箇所になる。
    pub const fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }

    /// ページ内オフセット（4KiB 単位）。
    pub const fn page_offset(self) -> u64 {
        self.0 & 0xFFF
    }

    pub const fn is_aligned(self, align: u64) -> bool {
        debug_assert!(align.is_power_of_two());
        self.0 & (align - 1) == 0
    }

    /// 切り上げ。**結果が正規形でなくなるなら `None`。**
    ///
    /// 下位半分の上端（`0x0000_7FFF_FFFF_FFFF`）付近を切り上げると、
    /// 桁溢れせずに正規形の穴へ入る。`Self` を返す形にすると、そこで
    /// 不変条件が静かに壊れる。
    pub const fn align_up(self, align: u64) -> Option<Self> {
        debug_assert!(align.is_power_of_two());
        match self.0.checked_add(align - 1) {
            Some(sum) => Self::new(sum & !(align - 1)),
            None => None,
        }
    }

    pub const fn align_down(self, align: u64) -> Option<Self> {
        debug_assert!(align.is_power_of_two());
        Self::new(self.0 & !(align - 1))
    }

    /// 加算。**桁溢れだけでなく、結果が正規形かも見る。**
    ///
    /// 下位半分の上端に 1 を足すと、桁溢れせずに非正規になる。
    /// u64 の桁溢れだけを見る `checked_add` では足りない。
    pub const fn checked_add(self, offset: u64) -> Option<Self> {
        match self.0.checked_add(offset) {
            Some(sum) => Self::new(sum),
            None => None,
        }
    }

    pub const fn checked_sub(self, offset: u64) -> Option<Self> {
        match self.0.checked_sub(offset) {
            Some(difference) => Self::new(difference),
            None => None,
        }
    }

    // ページテーブルの各階層の添字。
    //
    // **T-2 で `kernel::paging::entry` 側の同等の実装をこれに統合する。**
    // 二重に持つと、片方だけ直したときに食い違う。統合の際は、既存の実装と
    // ここが同じ結果を返すことを確かめること。
    pub const fn pml4_index(self) -> usize {
        ((self.0 >> 39) & 0x1FF) as usize
    }
    pub const fn pdpt_index(self) -> usize {
        ((self.0 >> 30) & 0x1FF) as usize
    }
    pub const fn pd_index(self) -> usize {
        ((self.0 >> 21) & 0x1FF) as usize
    }
    pub const fn pt_index(self) -> usize {
        ((self.0 >> 12) & 0x1FF) as usize
    }
}

/// 物理メモリ全体を仮想アドレス空間へ線形に写した窓（ADR-0021）。
///
/// # 範囲を定数にしない
///
/// 覆う長さは**実行時に決まる**。`kernel::memory_map::classify()` が返す
/// マップ対象の最大物理アドレスから導出する。コンパイル時定数で決めると
/// 別構成で静かに破綻する。実際に `phys_start=0xfd00000000` の PCI MMIO
/// 予約領域が観測されており、実装 RAM の量とは無関係に上端が動く。
///
/// そのため `phys_to_virt` を自由関数ではなくこの型のメソッドにしてある。
/// 範囲を持つ値を経由させることで、「範囲は実行時に決まる」ことが
/// 呼び出し側の形に現れる。
///
/// # T-1 の時点
///
/// 恒等マッピングなので `base` は 0 で、変換は値をそのまま移すだけである。
/// higher-half 移行ではここの `base` だけが変わる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DirectMap {
    base: VirtAddr,
    length: u64,
}

impl DirectMap {
    /// 窓を作る。上端が正規形に収まらなければ `None`。
    ///
    /// **恒等窓（`base==0`）は恒等除去（B-2b）の後に作ってはならない。** 除去後の
    /// 恒等窓構築は「低位ポインタを除去後に使おうとしている」ことの兆候なので、
    /// [`mark_identity_removed`] が呼ばれていれば fail-fast する。これが恒等の
    /// 利用を守る単一の関門である（`identity()` もここを通る。`new(base==0)` の
    /// 直接呼び出しも捕まる）。高位窓（`base!=0`、direct map など）は対象外。
    ///
    /// **この関門のため `const fn` にできない**（実行時にフラグを読む）。const 文脈
    /// での利用が無いことを確認して外した。**const へ戻すには関門を外すことになり、
    /// 恒等除去後の誤用を捕まえる安全網が静かに消える。** 戻さないこと。
    pub fn new(base: VirtAddr, length: u64) -> Option<Self> {
        if base.as_u64() == 0 {
            use core::sync::atomic::Ordering;
            assert!(
                !direct_map_slot::IDENTITY_REMOVED.load(Ordering::SeqCst),
                "identity DirectMap (base==0) constructed after the identity mapping was \
                 removed (B-2b): a low pointer is being used past the removal point"
            );
        }
        base.checked_add(length).map(|_| Self { base, length })
    }

    /// 恒等マッピング用（T-1 から higher-half 移行まで）。恒等除去後は
    /// [`Self::new`] の関門で fail-fast する。
    pub fn identity(length: u64) -> Option<Self> {
        Self::new(VirtAddr::new_const(0), length)
    }

    /// 恒等窓が覆える最大の長さ。
    ///
    /// **物理空間全体（52 ビット）は恒等では覆えない。** base が 0 なので
    /// 上端は長さそのものになり、`0x0000_8000_0000_0000` 以上は正規形の穴に
    /// 入る。したがって恒等で覆えるのは下位半分（47 ビット、128 TiB）までで
    /// ある。実装物理メモリより桁違いに広いので実害は無い。
    ///
    /// 4KiB 境界に丸めてあるのは、ページ単位で扱う値と揃えるためである。
    ///
    /// # 終端は排他である
    ///
    /// `length` は覆う長さであり、窓は `base .. base + length` を覆う。
    /// **終端は含まない。** 下位半分で正規形として使える最大のアドレスは
    /// `0x0000_7FFF_FFFF_FFFF` なので、排他の終端としては
    /// `0x0000_8000_0000_0000` まで取れるはずだが、[`DirectMap::new`] は
    /// `base.checked_add(length)` が正規形であることを要求する。
    /// 終端そのものを正規形として表せる形にしてあるので、1 ページ分
    /// 下げて `0x0000_7FFF_FFFF_F000` としている。
    ///
    /// 終端も正規形で表せることを要求するのは、`end()` のような
    /// 「排他の終端」を値として持ち回れるようにするためである。
    /// 表せないと、境界の計算のたびに特別扱いが要る。
    pub const IDENTITY_MAX_LENGTH: u64 = 0x0000_7FFF_FFFF_F000;

    /// direct physical map の高位窓の起点（ADR-0021）。
    ///
    /// 正規形の上半分の先頭で、512GiB 境界（当然 2MiB 境界）に載っている。
    /// higher-half 移行の A で、`phys_to_virt(p) = DIRECT_MAP_BASE + p` の
    /// 窓をページテーブルへ張り、A-2 で [`replace_direct_map`] により登録
    /// 窓をこの base へ差し替える。base が 2MiB 境界にあるため、
    /// `DIRECT_MAP_BASE + phys` のアラインメントは `phys` のそれと一致する。
    pub const DIRECT_MAP_BASE: u64 = 0xFFFF_8000_0000_0000;

    pub const fn base(self) -> VirtAddr {
        self.base
    }

    pub const fn length(self) -> u64 {
        self.length
    }

    /// この窓が覆っている物理アドレスか。
    ///
    /// **窓の範囲内であることだけを意味する。そのアドレスが実際にマップ
    /// されているかについては何も言わない。** マッピングの有無は
    /// `kernel::paging::plan::MappedRanges::contains_range`（計画の側）か
    /// `kernel::paging::active::translate`（実テーブルの側）で見る。
    ///
    /// 混同すると「covers が真だから触れるはず」という誤った推論が入り込む。
    /// 恒等マッピングの間は窓が下位半分全体を覆っているため、この誤りは
    /// 顕在化しない。higher-half 移行で窓が狭くなった瞬間、あるいは窓の外を
    /// 触った瞬間に初めて出る。
    pub const fn covers(self, phys: PhysAddr) -> bool {
        phys.as_u64() < self.length
    }

    /// 物理アドレスを仮想アドレスへ写す。**失敗しない。**
    ///
    /// direct physical map を採る決定（ADR-0021）により、写像は単なる加算で
    /// あり、窓の構築時に上端が正規形に収まることを確かめてある。
    ///
    /// **窓が覆っていない物理アドレスを渡した場合も値は返る。** 返るのは
    /// 正規形ではあるがマップされていないアドレスで、参照すれば #PF になる。
    /// 気にする呼び出し側は [`Self::covers`] を先に見ること。
    pub const fn phys_to_virt(self, phys: PhysAddr) -> VirtAddr {
        VirtAddr(self.base.as_u64().wrapping_add(phys.as_u64()))
    }

    /// 仮想アドレスを物理アドレスへ戻す。**窓の中のときだけ `Some`。**
    ///
    /// 窓の外の仮想アドレス（カーネルイメージ、MMIO の別窓など）は
    /// この写像では物理アドレスを決められない。ページテーブルを辿る必要が
    /// あり、それは `kernel::paging::active::translate` の仕事である。
    pub const fn virt_to_phys(self, virt: VirtAddr) -> Option<PhysAddr> {
        let base = self.base.as_u64();
        if virt.as_u64() < base {
            return None;
        }
        let offset = virt.as_u64() - base;
        if offset >= self.length {
            return None;
        }
        PhysAddr::new(offset)
    }
}

/// 唯一の [`DirectMap`] の出所。
///
/// # なぜ static なのか
///
/// 窓は起動時に一度決まり、以後変わらない。ページテーブル操作・フレーム
/// アロケータ・グラフィックス層のいずれもが変換を必要とするので、各所へ
/// 引き回すと呼び出し経路すべてに引数が増える。値が変わらない以上、
/// 出所を 1 つに固定して、必要な型が構築時に受け取る形が素直である。
///
/// # 使い方
///
/// **static を直接参照して回らない。** [`ActivePageTable`] や
/// `PageTableBuilder` のように変換を必要とする型は、構築時に
/// [`direct_map`] で 1 度受け取り、以後はその値を持ち回る。
/// そうすることで、
///
/// - その型がアドレス変換を必要とすることがシグネチャに現れる
/// - ホストテストで偽の窓を渡せる余地が残る（static 直参照だと
///   差し替えられない）
///
/// # パニック経路と例外ハンドラから呼んではならない
///
/// [`direct_map`] は未初期化なら panic する。パニックハンドラがこれを
/// 呼ぶと無限再帰になる。現在のパニックハンドラと例外ハンドラはシリアルへ
/// 直接書くだけでアドレス変換を必要としない。**その性質を保つこと。**
mod direct_map_slot {
    use core::sync::atomic::{AtomicBool, AtomicU64};

    pub(super) static BASE: AtomicU64 = AtomicU64::new(0);
    pub(super) static LENGTH: AtomicU64 = AtomicU64::new(0);
    pub(super) static READY: AtomicBool = AtomicBool::new(false);

    /// 恒等マッピング（`PML4[0]`）を除去した後 `true` になる（B-2b）。
    /// 除去後に恒等窓（`DirectMap::new(base==0)`、`identity()`を含む）を作ろうと
    /// する試みを [`super::DirectMap::new`] が fail-fast で捕まえる（反転設計:
    /// 恒等の利用者を列挙して守るのではなく、除去済みかを構築の関門で見る）。
    pub(super) static IDENTITY_REMOVED: AtomicBool = AtomicBool::new(false);
}

/// 恒等マッピングを除去したことを記録する（B-2b-4）。以降、恒等窓
/// （`DirectMap::new(base==0)` / `DirectMap::identity`）の構築は fail-fast する。
///
/// **除去（`PML4[0]`を落とし CR3 リロードで TLB を流す）が完了した後に呼ぶこと。**
/// これより前に呼ぶと、まだ恒等が生きているのに恒等窓の構築が止まる。
pub fn mark_identity_removed() {
    use core::sync::atomic::Ordering;
    direct_map_slot::IDENTITY_REMOVED.store(true, Ordering::SeqCst);
}

/// [`init_direct_map`] が失敗した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectMapInitError {
    /// 既に初期化されている。差し替えは [`replace_direct_map`] で行う。
    AlreadyInitialised,
    /// まだ初期化されていない。
    NotInitialised,
}

/// 窓を最初に設定する。**未初期化のときだけ成功する。**
pub fn init_direct_map(map: DirectMap) -> Result<(), DirectMapInitError> {
    use core::sync::atomic::Ordering;

    // 値を先に書き、最後に READY を立てる（公開パターン）。
    direct_map_slot::BASE.store(map.base().as_u64(), Ordering::Relaxed);
    direct_map_slot::LENGTH.store(map.length(), Ordering::Relaxed);
    // **Release はコンパイラの並べ替えを防ぐために今必要である。**
    // シングルコアであっても、上の 2 つのストアをこの store より後ろへ
    // 動かされると、READY が立った後に古い値を読む経路ができる。
    // SMP を見越した先回りではない（`docs/vision.md` の規律による）。
    direct_map_slot::READY
        .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
        .map(|_| ())
        .map_err(|_| DirectMapInitError::AlreadyInitialised)
}

/// 窓を差し替える。**higher-half 移行専用。**
///
/// # なぜ全体を `InterruptGuard` で囲むのか
///
/// base と length は別々の `AtomicU64` である。初期化は「値を書いてから
/// READY を立てる」公開パターンで守られているが、**差し替えは既に
/// READY が立った状態から 2 つの値を書き換えるので、その保護が効かない。**
/// 2 つのストアの間に割り込みが入れば、「新しい base と古い length」という
/// 裂けた値を観測する。
///
/// シングルコアなので、区間全体で割り込みを禁止すれば、読み手は差し替えの
/// 前か後のどちらかしか観測しない。M4-c-2 の `Locked<T>` と同じ構造である。
///
/// # Safety
///
/// **CR3 を新しい窓に対応するページテーブルへ切り替えた後で呼ぶこと。**
/// 切り替える前に呼ぶと、以後の変換がすべて誤った値を返し、それが生ポインタ
/// として使われる。間違った時点で呼ぶと未定義動作につながるため `unsafe`
/// にしてある（`new_const` と違い、こちらは実際に危険である）。
pub unsafe fn replace_direct_map(map: DirectMap) -> Result<(), DirectMapInitError> {
    use core::sync::atomic::Ordering;

    let _guard = crate::critical::InterruptGuard::enter();
    if !direct_map_slot::READY.load(Ordering::Acquire) {
        return Err(DirectMapInitError::NotInitialised);
    }
    direct_map_slot::BASE.store(map.base().as_u64(), Ordering::Relaxed);
    direct_map_slot::LENGTH.store(map.length(), Ordering::Relaxed);
    Ok(())
}

/// 窓を取り出す。**未初期化なら panic する。**
///
/// 未初期化の窓で変換すると、恒等でもない誤った値が返り、それが静かに
/// 伝播する。`Option` を返して呼び出し側に判断させると `unwrap` が
/// 散らばるだけで実質同じなので、ここで止める（ADR-0004 の fail-fast）。
///
/// **パニック経路と例外ハンドラから呼んではならない。** 無限再帰になる。
///
/// この関数は登録窓をモジュール内の構造体リテラルで再構築するので、
/// [`DirectMap::new`] の恒等除去の関門を通らない。恒等除去（B-2b）の後に
/// `direct_map()` が恒等窓（`base==0`）を返すことは起きない。登録窓は A-2
/// （[`replace_direct_map`]）が高位窓（`base!=0`）へ差し替え済みで、除去より
/// はるか前だからである。A-2 の順序を変えるとこの前提が崩れる。
pub fn direct_map() -> DirectMap {
    use core::sync::atomic::Ordering;

    // Acquire は `init_direct_map` の Release と対になる。
    assert!(
        direct_map_slot::READY.load(Ordering::Acquire),
        "the direct physical map was read before it was initialised"
    );
    let base = VirtAddr::new_const(direct_map_slot::BASE.load(Ordering::Relaxed));
    DirectMap {
        base,
        length: direct_map_slot::LENGTH.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正規形の穴のすぐ内側と外側。
    const LOWER_TOP: u64 = 0x0000_7FFF_FFFF_FFFF;
    const HOLE_START: u64 = 0x0000_8000_0000_0000;
    const HOLE_END: u64 = 0xFFFF_7FFF_FFFF_FFFF;
    const UPPER_BOTTOM: u64 = 0xFFFF_8000_0000_0000;

    #[test]
    fn phys_addr_rejects_values_above_bit_51() {
        assert!(PhysAddr::new(0).is_some());
        assert!(
            PhysAddr::new(0x000F_FFFF_FFFF_FFFF).is_some(),
            "ビット 51 まで"
        );
        assert!(PhysAddr::new(0x0010_0000_0000_0000).is_none(), "ビット 52");
        assert!(PhysAddr::new(u64::MAX).is_none());
    }

    #[test]
    fn virt_addr_accepts_only_canonical_values() {
        assert!(VirtAddr::new(LOWER_TOP).is_some(), "下位半分の上端");
        assert!(VirtAddr::new(HOLE_START).is_none(), "穴の始まり");
        assert!(VirtAddr::new(HOLE_END).is_none(), "穴の終わり");
        assert!(VirtAddr::new(UPPER_BOTTOM).is_some(), "上位半分の下端");
        assert!(VirtAddr::new(0).is_some());
        assert!(VirtAddr::new(u64::MAX).is_some());
    }

    /// **切り上げが正規形の穴へ入る場合を `None` にすること。**
    ///
    /// 桁溢れは起きないので、u64 の `checked_add` だけでは捕まえられない。
    #[test]
    fn virt_align_up_refuses_to_land_in_the_non_canonical_hole() {
        // 0x0000_7FFF_FFFF_F001 を 4KiB へ切り上げると
        // 0x0000_8000_0000_0000 になり、穴に入る。
        let near_top = VirtAddr::new(0x0000_7FFF_FFFF_F001).unwrap();
        assert_eq!(near_top.align_up(0x1000), None);

        // すぐ下（既に揃っている値）は動かない。境界の内側と外側を対にする。
        let aligned_top = VirtAddr::new(0x0000_7FFF_FFFF_F000).unwrap();
        assert_eq!(aligned_top.align_up(0x1000), Some(aligned_top));

        // 穴に入らない切り上げは通る。
        let safe = VirtAddr::new(0x1_0000_0001).unwrap();
        assert_eq!(safe.align_up(0x1000).unwrap().as_u64(), 0x1_0000_1000);

        // 境界ちょうどは動かない。
        let aligned = VirtAddr::new(0x1_0000_0000).unwrap();
        assert_eq!(aligned.align_up(0x1000).unwrap(), aligned);
        assert_eq!(aligned.align_down(0x1000).unwrap(), aligned);
    }

    /// **加算が穴をまたぐ場合を `None` にすること。**
    #[test]
    fn virt_checked_add_refuses_to_cross_the_hole() {
        let top = VirtAddr::new(LOWER_TOP).unwrap();
        assert_eq!(top.checked_add(1), None, "1 足すだけで非正規になる");
        assert_eq!(top.checked_add(0), Some(top));

        // 上位半分の中では普通に足せる。
        let upper = VirtAddr::new(UPPER_BOTTOM).unwrap();
        assert_eq!(
            upper.checked_add(0x1000).unwrap().as_u64(),
            UPPER_BOTTOM + 0x1000
        );

        // u64 の桁溢れも None。
        assert_eq!(VirtAddr::new(u64::MAX).unwrap().checked_add(1), None);
    }

    /// 物理側も同様に、範囲を出る切り上げ・加算を弾くこと。
    #[test]
    fn phys_arithmetic_stays_within_the_representable_range() {
        let near_top = PhysAddr::new(0x000F_FFFF_FFFF_F001).unwrap();
        assert_eq!(near_top.align_up(0x1000), None, "ビット 52 へ繰り上がる");

        assert_eq!(
            PhysAddr::new(0x000F_FFFF_FFFF_FFFF).unwrap().checked_add(1),
            None
        );
        assert_eq!(
            PhysAddr::new(0x1000)
                .unwrap()
                .checked_add(0x1000)
                .unwrap()
                .as_u64(),
            0x2000
        );
        assert_eq!(PhysAddr::new(0x1000).unwrap().checked_sub(0x2000), None);
    }

    #[test]
    fn frame_numbers_round_trip() {
        for raw in [0u64, 0x1000, 0x10_0000, 0x000F_FFFF_FFFF_F000] {
            let addr = PhysAddr::new(raw).unwrap();
            assert_eq!(
                PhysAddr::from_frame_number(addr.frame_number()).unwrap(),
                addr
            );
        }
        // 範囲外のフレーム番号は作れない。
        assert_eq!(PhysAddr::from_frame_number(u64::MAX), None);
        assert_eq!(
            PhysAddr::from_frame_number(0x0010_0000_0000_0000 / 4096),
            None
        );
    }

    #[test]
    fn the_indices_decompose_a_known_address() {
        // PML4=1, PDPT=2, PD=3, PT=4 になるアドレス。
        let raw = (1u64 << 39) | (2u64 << 30) | (3u64 << 21) | (4u64 << 12);
        let addr = VirtAddr::new(raw).unwrap();
        assert_eq!(addr.pml4_index(), 1);
        assert_eq!(addr.pdpt_index(), 2);
        assert_eq!(addr.pd_index(), 3);
        assert_eq!(addr.pt_index(), 4);
        assert_eq!(addr.page_offset(), 0);
        assert_eq!(VirtAddr::new(raw | 0xABC).unwrap().page_offset(), 0xABC);
    }

    #[test]
    fn the_identity_direct_map_round_trips() {
        let map = DirectMap::identity(0x1_0000_0000).unwrap();
        let phys = PhysAddr::new(0x10_0000).unwrap();

        assert_eq!(map.phys_to_virt(phys).as_u64(), 0x10_0000);
        assert_eq!(map.virt_to_phys(map.phys_to_virt(phys)), Some(phys));
        assert!(map.covers(phys));
    }

    /// 窓の外は `virt_to_phys` が `None`。
    #[test]
    fn the_direct_map_rejects_addresses_outside_the_window() {
        let map = DirectMap::new(VirtAddr::new_const(0xFFFF_8000_0000_0000), 0x1000).unwrap();

        let inside = VirtAddr::new(0xFFFF_8000_0000_0800).unwrap();
        assert_eq!(map.virt_to_phys(inside).unwrap().as_u64(), 0x800);

        let just_past = VirtAddr::new(0xFFFF_8000_0000_1000).unwrap();
        assert_eq!(map.virt_to_phys(just_past), None);

        let below = VirtAddr::new(0x1000).unwrap();
        assert_eq!(map.virt_to_phys(below), None);
    }

    /// 上端が正規形に収まらない窓は作れない。
    #[test]
    fn a_direct_map_that_would_leave_the_canonical_range_is_rejected() {
        assert_eq!(DirectMap::new(VirtAddr::new_const(LOWER_TOP), 2), None);
        assert!(DirectMap::new(VirtAddr::new_const(LOWER_TOP - 1), 1).is_some());
    }

    /// `new_const` は定数として使える。
    ///
    /// 不変条件を破る値を書けばコンパイルエラーになるので、ここでは
    /// 通る側だけを固定する。
    #[test]
    fn new_const_works_in_a_const_context() {
        const PHYS: PhysAddr = PhysAddr::new_const(0x10_0000);
        const VIRT: VirtAddr = VirtAddr::new_const(0xFFFF_8000_0000_0000);
        assert_eq!(PHYS.as_u64(), 0x10_0000);
        assert_eq!(VIRT.as_u64(), 0xFFFF_8000_0000_0000);
    }

    /// `#[repr(transparent)]` により、レイアウトは `u64` と同一である。
    ///
    /// T-2 で `BootInfo` のフィールドを置き換えるため、ここが崩れると
    /// bootloader と kernel の境界がずれる。
    #[test]
    fn the_types_have_the_same_layout_as_u64() {
        assert_eq!(
            core::mem::size_of::<PhysAddr>(),
            core::mem::size_of::<u64>()
        );
        assert_eq!(
            core::mem::align_of::<PhysAddr>(),
            core::mem::align_of::<u64>()
        );
        assert_eq!(
            core::mem::size_of::<VirtAddr>(),
            core::mem::size_of::<u64>()
        );
        assert_eq!(
            core::mem::align_of::<VirtAddr>(),
            core::mem::align_of::<u64>()
        );
    }
}
