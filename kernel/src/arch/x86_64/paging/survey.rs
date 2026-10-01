//! ページテーブルを歩いて、写っている範囲と権限を並べる（2026-10-01）。**読むだけで、何も変えない。**
//!
//! 権限の設定を 1 か所へ寄せる前と後で、どのページの権限も変わっていないことを比べるための道具である
//! （`ADR-0071` の手順 3）。使う側は共通の側の `crate::page_survey` で、ここは表を歩いて、
//! 同じ権限で続くページを 1 つの範囲にまとめて渡すところまでを受け持つ。
//!
//! # 作る側とコードを共有しない
//!
//! [`super::verify`] と同じ理由である。階層を降りるループと、項目のビットの読み方を、このファイルに
//! 独立に書いてある。`entry` の定数も `verify` の定数も参照しない。**作る側と同じ式で読むと、
//! 作る側の誤りを一緒に読み違える。**
//!
//! # 何を読むか
//!
//! - **葉の権限**——書き込み可・ユーザーから触れる・実行できる・キャッシュの属性・グローバル・共有の印
//! - **途中の階層の権限**——書き込み可とユーザーから触れるかは全部の階層の AND で決まり、実行の禁止は
//!   どこか 1 つの階層に立てば効く。**葉だけを読むと、途中の階層の違いを見落とす**ので、分けて持つ
//! - **ページの大きさ**（4KiB・2MiB・1GiB）
//!
//! **読まないもの**——物理の番地（ビルドごとに動く）と、CPU が立てる印（アクセス済み・書き込み済み）。
//!
//! # 表の読み方を差し替えられる
//!
//! 歩くループ（[`walk`]）は「表の物理の番地と添字から項目を読む」関数を引数で受ける。カーネルでは
//! 直接写像を通して読み（[`for_each_mapped_range`]）、ホストのテストでは作った表を読む。**歩く論理は
//! ホストのテストで確かめられる。**

use core::ops::Range;

use common::addr::{DirectMap, PhysAddr};

/// 独立に書き直したビット定義。**`entry` と `verify` の定数を参照しない。**
mod bits {
    /// Present。
    pub const PRESENT: u64 = 1 << 0;
    /// Read/Write。
    pub const WRITABLE: u64 = 1 << 1;
    /// User/Supervisor。
    pub const USER: u64 = 1 << 2;
    /// Page-level Write-Through。
    pub const WRITE_THROUGH: u64 = 1 << 3;
    /// Page-level Cache Disable。
    pub const CACHE_DISABLE: u64 = 1 << 4;
    /// PDPT と PD の項目では「ページそのもの」、PT の項目では PAT。
    pub const PAGE_SIZE_OR_SMALL_PAT: u64 = 1 << 7;
    /// Global。
    pub const GLOBAL: u64 = 1 << 8;
    /// ソフトウェアが使える印のうち、共有の葉に立てているもの（ビット 9）。
    pub const SHARED: u64 = 1 << 9;
    /// 2MiB と 1GiB の葉の PAT（ビット 12）。
    pub const LARGE_PAT: u64 = 1 << 12;
    /// 実行の禁止（ビット 63）。`EFER.NXE` が立っていないと予約ビットである。
    pub const NO_EXECUTE: u64 = 1 << 63;
    /// 次の表の物理の番地（ビット 12-51）。
    pub const TABLE_ADDRESS: u64 = 0x000F_FFFF_FFFF_F000;
}

/// 1 つの表の項目の数。
const ENTRIES: usize = 512;

/// ページの大きさ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingSize {
    /// 4KiB。
    Small,
    /// 2MiB。
    Large,
    /// 1GiB。ZeikOS は作らないが、在れば読む。
    Giant,
}

impl MappingSize {
    /// バイト数。
    pub const fn bytes(self) -> u64 {
        match self {
            MappingSize::Small => 1 << 12,
            MappingSize::Large => 1 << 21,
            MappingSize::Giant => 1 << 30,
        }
    }

    /// 一覧に出す短い名前。
    pub const fn label(self) -> &'static str {
        match self {
            MappingSize::Small => "4K",
            MappingSize::Large => "2M",
            MappingSize::Giant => "1G",
        }
    }
}

/// 写っている範囲の権限。**葉と、途中の階層を分けて持つ。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingPermissions {
    /// 葉が書き込みを許すか。
    pub writable: bool,
    /// 葉がユーザーから触れる印を持つか。
    pub user: bool,
    /// 葉が実行を許すか（実行の禁止の印が無いか）。
    pub executable: bool,
    /// 葉のキャッシュの属性。ビット 0 が書き通し、ビット 1 がキャッシュ無効、ビット 2 が PAT。0 が通常である。
    pub cache: u8,
    /// 葉がグローバルの印を持つか。
    pub global: bool,
    /// 葉が共有の印を持つか。
    pub shared: bool,
    /// 途中の階層が全部、書き込みを許すか。
    pub tables_writable: bool,
    /// 途中の階層が全部、ユーザーから触れる印を持つか。
    pub tables_user: bool,
    /// 途中の階層のどれも、実行を禁じていないか。
    pub tables_executable: bool,
}

/// 同じ権限・同じ大きさで続くページの範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedRange {
    /// 先頭の仮想の番地。
    pub start: u64,
    /// ページの数（`size` の大きさのページで数える）。
    pub pages: u64,
    /// ページの大きさ。
    pub size: MappingSize,
    /// 権限。
    pub permissions: MappingPermissions,
}

impl MappedRange {
    /// 範囲の長さ（バイト）。
    pub const fn bytes(&self) -> u64 {
        self.pages * self.size.bytes()
    }
}

/// 途中の階層の権限を、根から順に重ねたもの。
#[derive(Clone, Copy)]
struct Path {
    writable: bool,
    user: bool,
    executable: bool,
}

impl Path {
    /// まだ階層を 1 つも通っていない状態。
    const START: Path = Path {
        writable: true,
        user: true,
        executable: true,
    };

    /// 表を指す項目を 1 つ通る。
    fn through(self, entry: u64) -> Path {
        Path {
            writable: self.writable && entry & bits::WRITABLE != 0,
            user: self.user && entry & bits::USER != 0,
            executable: self.executable && entry & bits::NO_EXECUTE == 0,
        }
    }
}

/// 葉の項目と、そこまでの道から、権限を作る。
fn permissions_of(leaf: u64, size: MappingSize, path: Path) -> MappingPermissions {
    let pat = match size {
        MappingSize::Small => bits::PAGE_SIZE_OR_SMALL_PAT,
        MappingSize::Large | MappingSize::Giant => bits::LARGE_PAT,
    };
    let cache = u8::from(leaf & bits::WRITE_THROUGH != 0)
        | u8::from(leaf & bits::CACHE_DISABLE != 0) << 1
        | u8::from(leaf & pat != 0) << 2;
    MappingPermissions {
        writable: leaf & bits::WRITABLE != 0,
        user: leaf & bits::USER != 0,
        executable: leaf & bits::NO_EXECUTE == 0,
        cache,
        global: leaf & bits::GLOBAL != 0,
        shared: leaf & bits::SHARED != 0,
        tables_writable: path.writable,
        tables_user: path.user,
        tables_executable: path.executable,
    }
}

/// 添字から仮想の番地を作る。**上位の半分（最上位の添字が 256 以上）は、上のビットを 1 で埋める。**
fn address_of(top: usize, upper: usize, middle: usize, leaf: usize) -> u64 {
    let low = ((top as u64) << 39)
        | ((upper as u64) << 30)
        | ((middle as u64) << 21)
        | ((leaf as u64) << 12);
    if top >= ENTRIES / 2 {
        low | 0xFFFF_0000_0000_0000
    } else {
        low
    }
}

/// 続いている葉を 1 つの範囲にまとめる。
struct Joiner<'a> {
    pending: Option<MappedRange>,
    visit: &'a mut dyn FnMut(MappedRange),
}

impl Joiner<'_> {
    /// 葉を 1 つ受ける。**直前の範囲のすぐ後ろで、大きさも権限も同じなら、範囲を伸ばす。**
    fn leaf(&mut self, start: u64, size: MappingSize, permissions: MappingPermissions) {
        if let Some(pending) = &mut self.pending {
            // 範囲の終わりは u64 を越えうる（最後のページが番地の上端に在るとき）ので、越えたら続きとみなさない。
            let continues = pending.start.checked_add(pending.bytes()) == Some(start);
            if continues && pending.size == size && pending.permissions == permissions {
                pending.pages += 1;
                return;
            }
        }
        self.flush();
        self.pending = Some(MappedRange {
            start,
            pages: 1,
            size,
            permissions,
        });
    }

    /// 溜めている範囲を渡す。
    fn flush(&mut self) {
        if let Some(range) = self.pending.take() {
            (self.visit)(range);
        }
    }
}

/// 表を歩き、写っている範囲を番地の順に `visit` へ渡す（純粋な論理）。
///
/// `read` は「表の物理の番地と添字から、項目を 1 つ読む」関数である。`top` は、最上位の表のうち歩く添字の
/// 範囲である（全部なら `0..512`。ユーザーの側だけなら `0..256`）。**512 を越える分は読まない。**
///
/// 在る（Present）項目だけを辿る。PDPT と PD の項目が「ページそのもの」なら葉として扱い、そこで降りない。
fn walk(
    read: &dyn Fn(u64, usize) -> u64,
    root: u64,
    top: Range<usize>,
    visit: &mut dyn FnMut(MappedRange),
) {
    let mut joiner = Joiner {
        pending: None,
        visit,
    };
    for top_index in top.start..top.end.min(ENTRIES) {
        let top_entry = read(root, top_index);
        if top_entry & bits::PRESENT == 0 {
            continue;
        }
        let path = Path::START.through(top_entry);
        let upper_table = top_entry & bits::TABLE_ADDRESS;
        for upper_index in 0..ENTRIES {
            let upper_entry = read(upper_table, upper_index);
            if upper_entry & bits::PRESENT == 0 {
                continue;
            }
            if upper_entry & bits::PAGE_SIZE_OR_SMALL_PAT != 0 {
                joiner.leaf(
                    address_of(top_index, upper_index, 0, 0),
                    MappingSize::Giant,
                    permissions_of(upper_entry, MappingSize::Giant, path),
                );
                continue;
            }
            let path = path.through(upper_entry);
            let middle_table = upper_entry & bits::TABLE_ADDRESS;
            for middle_index in 0..ENTRIES {
                let middle_entry = read(middle_table, middle_index);
                if middle_entry & bits::PRESENT == 0 {
                    continue;
                }
                if middle_entry & bits::PAGE_SIZE_OR_SMALL_PAT != 0 {
                    joiner.leaf(
                        address_of(top_index, upper_index, middle_index, 0),
                        MappingSize::Large,
                        permissions_of(middle_entry, MappingSize::Large, path),
                    );
                    continue;
                }
                let path = path.through(middle_entry);
                let leaf_table = middle_entry & bits::TABLE_ADDRESS;
                for leaf_index in 0..ENTRIES {
                    let leaf_entry = read(leaf_table, leaf_index);
                    if leaf_entry & bits::PRESENT == 0 {
                        continue;
                    }
                    joiner.leaf(
                        address_of(top_index, upper_index, middle_index, leaf_index),
                        MappingSize::Small,
                        permissions_of(leaf_entry, MappingSize::Small, path),
                    );
                }
            }
        }
    }
    joiner.flush();
}

/// 指定した根の表を歩き、写っている範囲を番地の順に `visit` へ渡す。
///
/// # 契約（境界の関数）
///
/// - 根から全部の階層を読むだけで、何も変えない。表が稼働中かどうかは問わない（CR3 を読まない）。
/// - `top` は、最上位の表のうち歩く添字の範囲である（全部なら `0..512`）。
/// - 渡す範囲は番地の順で、同じ大きさ・同じ権限で続くページは 1 つにまとめてある。
/// - **答えは歩いた時点の表についてのものである。** 歩いている間に表が変わらないことは、呼ぶ側が保証する
///   （起動の途中の 1 本の流れか、BKL を持っている間に呼ぶ）。
/// - メモリを確保しない。
///
/// # Safety
///
/// `root` が有効なページテーブルの根のフレームを指し、`direct_map` を通して、そのフレームと配下の表の
/// フレームが全部読めること。配下の表を指す項目が、有効な表のフレームを指していること。
pub unsafe fn for_each_mapped_range(
    root: PhysAddr,
    direct_map: DirectMap,
    top: Range<usize>,
    visit: &mut dyn FnMut(MappedRange),
) {
    let read = |table: u64, index: usize| -> u64 {
        // SAFETY: 呼び出し元契約により、`table` は直接写像を通して読める表のフレームで、`index` は 512 未満。
        // 読み取りのみ。
        unsafe {
            core::ptr::read_volatile(
                direct_map
                    .phys_to_virt(PhysAddr::new_const(table))
                    .as_ptr::<u64>()
                    .add(index),
            )
        }
    };
    walk(&read, root.as_u64(), top, visit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// 作った表。物理の番地から 512 項目を引く。
    struct Tables {
        frames: RefCell<HashMap<u64, Vec<u64>>>,
        next: RefCell<u64>,
    }

    impl Tables {
        fn new() -> Self {
            Tables {
                frames: RefCell::new(HashMap::new()),
                next: RefCell::new(0x10_0000),
            }
        }

        /// 空の表を 1 枚作り、その物理の番地を返す。
        fn table(&self) -> u64 {
            let at = *self.next.borrow();
            *self.next.borrow_mut() += 0x1000;
            self.frames.borrow_mut().insert(at, vec![0; ENTRIES]);
            at
        }

        fn set(&self, table: u64, index: usize, entry: u64) {
            self.frames.borrow_mut().get_mut(&table).unwrap()[index] = entry;
        }

        fn read(&self, table: u64, index: usize) -> u64 {
            self.frames.borrow()[&table][index]
        }

        /// 歩いて、渡された範囲を集める。
        fn ranges(&self, root: u64, top: Range<usize>) -> Vec<MappedRange> {
            let mut found = Vec::new();
            walk(
                &|table, index| self.read(table, index),
                root,
                top,
                &mut |range| found.push(range),
            );
            found
        }
    }

    const P: u64 = bits::PRESENT;
    const W: u64 = bits::WRITABLE;
    const U: u64 = bits::USER;

    /// 根から PT までを 1 本つなぎ、（根, PT）を返す。途中の階層の印は `flags` で決める。
    fn chain(tables: &Tables, top: usize, upper: usize, middle: usize, flags: u64) -> (u64, u64) {
        let root = tables.table();
        let pt = extend(tables, root, top, upper, middle, flags);
        (root, pt)
    }

    /// 既に在る根へ、PT までの道を 1 本足す。
    fn extend(
        tables: &Tables,
        root: u64,
        top: usize,
        upper: usize,
        middle: usize,
        flags: u64,
    ) -> u64 {
        let pdpt = tables.table();
        let pd = tables.table();
        let pt = tables.table();
        tables.set(root, top, pdpt | flags);
        tables.set(pdpt, upper, pd | flags);
        tables.set(pd, middle, pt | flags);
        pt
    }

    /// **同じ権限で続く 4KiB のページは 1 つの範囲にまとまり、権限が違う所と、間が空いた所で分かれる。**
    /// **物理の番地と、CPU が立てる印（アクセス済み・書き込み済み）の違いでは分かれない。**
    #[test]
    fn pages_in_a_row_with_the_same_permissions_become_one_range() {
        let tables = Tables::new();
        let (root, pt) = chain(&tables, 0, 0, 2, P | W | U);
        let accessed = 1 << 5;
        let dirty = 1 << 6;
        tables.set(pt, 0, 0x20_0000 | P | U);
        tables.set(pt, 1, 0x90_0000 | P | U | accessed);
        tables.set(pt, 2, 0x30_0000 | P | U | W | accessed | dirty);
        tables.set(pt, 3, 0x40_0000 | P | U | W);
        // 4 は空き。
        tables.set(pt, 5, 0x50_0000 | P | U | W);
        let ranges = tables.ranges(root, 0..512);
        let summary: Vec<(u64, u64, bool)> = ranges
            .iter()
            .map(|range| (range.start, range.pages, range.permissions.writable))
            .collect();
        assert_eq!(
            summary,
            vec![
                (0x40_0000, 2, false),
                (0x40_2000, 2, true),
                (0x40_5000, 1, true),
            ]
        );
        for range in &ranges {
            assert_eq!(range.size, MappingSize::Small);
            assert!(range.permissions.user && range.permissions.executable);
            assert!(range.permissions.tables_writable && range.permissions.tables_user);
            assert_eq!(range.permissions.cache, 0);
        }
    }

    /// **途中の階層の権限は、葉とは別に読む。** 葉が書き込み可でも、途中の階層のどれかが書き込みを許さなければ
    /// `tables_writable` は偽になる。実行の禁止は、途中の階層のどれか 1 つに立てば `tables_executable` が偽になる。
    #[test]
    fn the_permissions_of_the_tables_on_the_way_are_read_apart_from_the_leaf() {
        let tables = Tables::new();
        let root = tables.table();
        let pdpt = tables.table();
        let pd = tables.table();
        let pt = tables.table();
        tables.set(root, 0, pdpt | P | W | U);
        tables.set(pdpt, 0, pd | P | U); // ここだけ書き込みを許さない。
        tables.set(pd, 0, pt | P | W | U | bits::NO_EXECUTE);
        tables.set(pt, 7, 0x20_0000 | P | W | U);
        let ranges = tables.ranges(root, 0..512);
        assert_eq!(ranges.len(), 1);
        let permissions = ranges[0].permissions;
        assert!(permissions.writable && permissions.user && permissions.executable);
        assert!(!permissions.tables_writable);
        assert!(permissions.tables_user);
        assert!(!permissions.tables_executable);

        // 途中の階層がユーザーから触れなければ、`tables_user` が偽になる。
        let (root, pt) = chain(&tables, 1, 0, 0, P | W);
        tables.set(pt, 0, 0x20_0000 | P | W | U);
        let permissions = tables.ranges(root, 0..512)[0].permissions;
        assert!(permissions.user && !permissions.tables_user);
    }

    /// **2MiB と 1GiB の葉は、その階層で止まる。** キャッシュの属性の PAT は、4KiB ではビット 7、
    /// 大きいページではビット 12 に在る。**大きいページのビット 7 は「ページそのもの」で、PAT ではない。**
    #[test]
    fn large_pages_stop_at_their_level_and_carry_the_cache_bits_at_their_own_place() {
        let tables = Tables::new();
        let root = tables.table();
        let pdpt = tables.table();
        let pd = tables.table();
        tables.set(root, 0, pdpt | P | W);
        tables.set(pdpt, 0, pd | P | W);
        let large = bits::PAGE_SIZE_OR_SMALL_PAT;
        tables.set(pd, 0, 0x4000_0000 | P | W | large);
        tables.set(pd, 1, 0x4020_0000 | P | W | large);
        tables.set(pd, 2, 0x8000_0000 | P | W | large | bits::CACHE_DISABLE);
        tables.set(pd, 3, 0x8020_0000 | P | W | large | bits::LARGE_PAT);
        tables.set(pdpt, 1, 0xC000_0000 | P | W | large | bits::GLOBAL);
        let ranges = tables.ranges(root, 0..512);
        let summary: Vec<(u64, u64, MappingSize, u8, bool)> = ranges
            .iter()
            .map(|range| {
                (
                    range.start,
                    range.pages,
                    range.size,
                    range.permissions.cache,
                    range.permissions.global,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (0, 2, MappingSize::Large, 0, false),
                (0x40_0000, 1, MappingSize::Large, 0b010, false),
                (0x60_0000, 1, MappingSize::Large, 0b100, false),
                (0x4000_0000, 1, MappingSize::Giant, 0, true),
            ]
        );
        assert_eq!(ranges[0].bytes(), 4 << 20);

        // 4KiB の葉のビット 7 は PAT である。書き通しはビット 3、共有の印はビット 9。
        let (root, pt) = chain(&tables, 0, 0, 0, P | W);
        tables.set(
            pt,
            0,
            0x20_0000 | P | bits::PAGE_SIZE_OR_SMALL_PAT | bits::WRITE_THROUGH | bits::SHARED,
        );
        let permissions = tables.ranges(root, 0..512)[0].permissions;
        assert_eq!(permissions.cache, 0b101);
        assert!(permissions.shared && !permissions.writable);
    }

    /// **上位の半分の番地は、上のビットを 1 で埋めた形で渡す。** 歩く添字の範囲の外は読まない。
    /// 実行の禁止の印が葉に在れば、`executable` が偽になる。
    #[test]
    fn the_upper_half_is_reported_in_its_canonical_form_and_only_the_asked_top_entries_are_walked()
    {
        let tables = Tables::new();
        let (root, low) = chain(&tables, 0, 0, 2, P | W | U);
        tables.set(low, 0, 0x20_0000 | P | U);
        let high = extend(&tables, root, 511, 510, 0, P | W);
        tables.set(high, 0x100, 0x10_0000 | P | W | bits::NO_EXECUTE);

        let all = tables.ranges(root, 0..512);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].start, 0x40_0000);
        assert_eq!(all[1].start, 0xFFFF_FFFF_8010_0000);
        assert!(!all[1].permissions.executable);
        assert!(!all[1].permissions.user && !all[1].permissions.tables_user);

        let lower = tables.ranges(root, 0..256);
        assert_eq!(lower.len(), 1);
        assert_eq!(lower[0].start, 0x40_0000);
        let upper = tables.ranges(root, 256..512);
        assert_eq!(upper.len(), 1);
        assert_eq!(upper[0].start, 0xFFFF_FFFF_8010_0000);
        // 512 を越える範囲を渡しても、512 までしか読まない。
        assert_eq!(tables.ranges(root, 256..600).len(), 1);
        // 何も写っていない表は、範囲を 1 つも渡さない。
        let empty = tables.table();
        assert!(tables.ranges(empty, 0..512).is_empty());
    }

    /// **2 つの添字が同じ下の表を指していれば、同じ中身が 2 つの番地の範囲に出る**（起動の表は、低い番地と
    /// 高い番地で同じ PD を共有している）。**番地の上端で終わる範囲も、あふれずに渡す。**
    #[test]
    fn a_table_shared_by_two_entries_shows_up_at_both_addresses() {
        let tables = Tables::new();
        let root = tables.table();
        let low = tables.table();
        let high = tables.table();
        let shared = tables.table();
        tables.set(root, 0, low | P | W);
        tables.set(root, 511, high | P | W);
        tables.set(low, 0, shared | P | W);
        tables.set(high, 510, shared | P | W);
        for index in 0..4 {
            tables.set(
                shared,
                index,
                ((index as u64) << 21) | P | W | bits::PAGE_SIZE_OR_SMALL_PAT,
            );
        }
        let ranges = tables.ranges(root, 0..512);
        let summary: Vec<(u64, u64)> = ranges
            .iter()
            .map(|range| (range.start, range.pages))
            .collect();
        assert_eq!(summary, vec![(0, 4), (0xFFFF_FFFF_8000_0000, 4)]);

        // 番地の上端の 2 ページ。
        let (root, pt) = chain(&tables, 511, 511, 511, P | W);
        tables.set(pt, 510, 0x20_0000 | P | W);
        tables.set(pt, 511, 0x20_1000 | P | W);
        let ranges = tables.ranges(root, 0..512);
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            (ranges[0].start, ranges[0].pages),
            (0xFFFF_FFFF_FFFF_E000, 2)
        );
    }
}
