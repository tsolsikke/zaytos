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
    pub const fn new(base: VirtAddr, length: u64) -> Option<Self> {
        match base.checked_add(length) {
            Some(_) => Some(Self { base, length }),
            None => None,
        }
    }

    /// 恒等マッピング用（T-1 から higher-half 移行まで）。
    pub const fn identity(length: u64) -> Option<Self> {
        Self::new(VirtAddr::new_const(0), length)
    }

    pub const fn base(self) -> VirtAddr {
        self.base
    }

    pub const fn length(self) -> u64 {
        self.length
    }

    /// この窓が覆っている物理アドレスか。
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
