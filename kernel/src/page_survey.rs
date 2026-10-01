//! 写っているページの権限を、名前つきの領域ごとの一覧にする（2026-10-01）。
//!
//! 権限の設定を 1 か所へ寄せる前と後で、どのページの権限も変わっていないことを比べるための道具である
//! （`ADR-0071` の手順 3）。**表を歩くのは置き場の側**（`crate::arch` の `for_each_mapped_range`。作る側と
//! コードを共有しない）で、ここは渡された範囲を領域に分け、行にまとめ、出す。
//!
//! # 領域
//!
//! **領域は、名前を持つ番地の範囲である**（[`Region`]）。カーネルの像の区画、直接写像、フレームバッファ、
//! 見張りのページ、ユーザーのプログラムの区画などを、写した所が登録する（[`register`]）。
//! 範囲が領域の境をまたげば、境で分ける。領域が入れ子なら、いちばん狭いものの名前を採る。
//!
//! **見張りのページは「在ってはならない領域」として登録する**（[`register_absent`]）。そこにページが
//! 写っていれば、その行に印が付く。外したはずの写像が残っている形は、ページ数の増減だけでは見えないので、
//! 名前を持たせて見えるようにする。
//!
//! # 行
//!
//! **1 行は「領域の名前・ページの大きさ・権限」の組で、ページ数（4KiB で数える）を持つ。** 同じ組の範囲が
//! 離れた番地に在っても、1 行にまとまる。**物理の番地は持たない**（ビルドごとに動く）。
//!
//! # 出すもの
//!
//! - **既定のビルドは、時点ごとに要約を 1 行だけ出す**——行の数と、行の中身から作った要約の値（[`Survey::digest`]）。
//!   起動ログの参照に載るので、どこかの権限が変わると起動ログの突き合わせが落ちる。**どの領域にも入らない
//!   範囲と、在ってはならない領域に写っている範囲は、既定のビルドでも行を出す。**
//! - **ユーザーのプログラムの一覧は、既定のビルドでは起動時のプログラムの分だけを出す**（[`boot_programs_are_done`]）。
//!   シェルから走らせたプログラムのたびに 1 行増えると、手で使うときのログとシェルの試験の出力の見通しが悪くなる。
//!   `page-permissions-dump` を付ければ、どのプログラムの分も出す。
//! - **`page-permissions-dump` の feature を付けると、全部の行を出す。** 2 つの一覧を比べて違いを名前つきで
//!   示すのは `xtask` の側である（`xtask/src/page_permissions.rs`。`cargo xtask run --page-permissions`）。
//!   **ページ数を要約の値に入れない行は、`pages=~12` のように印を付けて出す**（比べる側も、その行のページ数を比べない）。
//!
//! # 要約の値に入れるもの
//!
//! 行ごとに、領域の名前・ページの大きさ・権限を入れる。**ページ数は、領域が「数える」と登録されたときだけ入れる**
//! ——カーネルの像の区画はコードの量で、CPU ごとのスタックは CPU の数で、ページ数が動く。そういう領域は
//! 権限だけを比べる。**行の順には依らない**（行ごとの値を足し合わせる）。
//!
//! # 限界
//!
//! - 読むのは、呼んだ時点の表だけである。時点の間に変わって戻った権限は見えない。
//! - TLB の中身は見ない。物理の番地（どのフレームを指しているか）は比べない。

use core::ops::Range;

use common::addr::{DirectMap, PhysAddr};
use common::critical::Locked;
use common::log::Logger;
use common::machine::pc::Serial;

use crate::arch::x86_64::{for_each_mapped_range, MappedRange, MappingPermissions, MappingSize};

/// 行を数える単位（4KiB）。
const UNIT: u64 = 4096;

/// 登録できる領域の数の上限。
pub const MAX_REGIONS: usize = 48;

/// カーネルの表の一覧が持てる行の数の上限。
pub const KERNEL_ROWS: usize = 64;

/// ユーザーの空間の一覧が持てる行の数の上限。**小さくしてある**——`spawn` は親の遠征のスタックの上で走る。
pub const USER_ROWS: usize = 24;

/// どの領域にも入らない範囲の行に付ける名前。
const UNNAMED: &str = "(unnamed)";

/// 領域にページが在るべきか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// 写っているはずの領域。
    Mapped,
    /// 何も写っていてはならない領域（見張りのページ、外した後の写像）。
    Absent,
}

/// 名前を持つ番地の範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// 一覧に出す名前。同じ名前を複数の範囲に付けてよい（行は名前でまとまる）。
    pub name: &'static str,
    /// 先頭の番地。
    pub start: u64,
    /// 終わりの番地（含まない）。
    pub end: u64,
    /// ページが在るべきか。
    pub expectation: Expectation,
    /// ページ数を要約の値に入れるか。
    pub counted: bool,
}

impl Region {
    /// 写っているはずの領域。
    pub const fn mapped(name: &'static str, start: u64, end: u64, counted: bool) -> Region {
        Region {
            name,
            start,
            end,
            expectation: Expectation::Mapped,
            counted,
        }
    }

    /// 何も写っていてはならない領域。
    pub const fn absent(name: &'static str, start: u64, end: u64) -> Region {
        Region {
            name,
            start,
            end,
            expectation: Expectation::Absent,
            counted: true,
        }
    }

    /// 領域の境を、ページの境へ外向きに広げたもの（先頭は切り下げ、終わりは切り上げ）。**行は 4KiB で数えるので、
    /// 境をページの途中に置かない。** 終わりは u64 を越えうるので、広い型で返す。
    fn bounds(&self) -> (u128, u128) {
        let unit = u128::from(UNIT);
        let start = u128::from(self.start) / unit * unit;
        let end = u128::from(self.end).div_ceil(unit) * unit;
        (start, end)
    }

    fn contains(&self, address: u128) -> bool {
        let (start, end) = self.bounds();
        start <= address && address < end
    }

    fn length(&self) -> u128 {
        let (start, end) = self.bounds();
        end - start
    }
}

/// 一覧の 1 行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    /// 領域の名前。どの領域にも入らなければ `(unnamed)`。
    pub name: &'static str,
    /// その領域にページが在るべきか。**`Absent` の行は、在ってはならない所に写っているということである。**
    pub expectation: Expectation,
    /// ページ数を要約の値に入れるか。
    pub counted: bool,
    /// ページの大きさ。
    pub size: MappingSize,
    /// 権限。
    pub permissions: MappingPermissions,
    /// ページ数（4KiB で数える）。
    pub pages: u64,
    /// この行に入った最初の範囲の番地。名前の無い行を出すときに使う。
    pub first: u64,
}

impl Row {
    /// どの領域にも入らない範囲の行か。
    pub fn is_unnamed(&self) -> bool {
        self.name == UNNAMED
    }

    /// 在ってはならない領域に写っている行か。
    pub fn is_unexpected(&self) -> bool {
        self.expectation == Expectation::Absent
    }
}

/// 一覧（行の集まり）。**メモリを確保しない**（行の数の上限は型で決まる）。
pub struct Survey<const N: usize> {
    rows: [Option<Row>; N],
    count: usize,
    /// 上限を越えて入らなかった範囲の数。
    dropped: usize,
}

impl<const N: usize> Default for Survey<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Survey<N> {
    /// 空の一覧。
    pub const fn new() -> Self {
        Survey {
            rows: [None; N],
            count: 0,
            dropped: 0,
        }
    }

    /// 行。
    pub fn rows(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().take(self.count).flatten()
    }

    /// 上限を越えて入らなかった範囲の数。0 でなければ、一覧は欠けている。
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// 写っている範囲を 1 つ足す（純粋な論理）。**領域の境で分け、いちばん狭い領域の名前で行へ入れる。**
    pub fn add(&mut self, regions: &[Region], range: MappedRange) {
        // 範囲の終わりは u64 を越えうる（番地の上端で終わるとき）ので、広い型で数える。
        let end = u128::from(range.start) + u128::from(range.bytes());
        let mut at = u128::from(range.start);
        while at < end {
            let region = regions
                .iter()
                .filter(|region| region.contains(at))
                .min_by_key(|region| region.length());
            // 次の境——いまの領域の終わりと、この先で始まる領域の先頭のうち、いちばん近いもの。
            let mut next = end;
            if let Some(region) = region {
                next = next.min(region.bounds().1);
            }
            for other in regions {
                let start = other.bounds().0;
                if start > at {
                    next = next.min(start);
                }
            }
            let pages = ((next - at) / u128::from(UNIT)) as u64;
            let first = at as u64;
            match region {
                Some(region) => self.put(
                    region.name,
                    region.expectation,
                    region.counted,
                    &range,
                    pages,
                    first,
                ),
                None => self.put(UNNAMED, Expectation::Mapped, true, &range, pages, first),
            }
            at = next;
        }
    }

    fn put(
        &mut self,
        name: &'static str,
        expectation: Expectation,
        counted: bool,
        range: &MappedRange,
        pages: u64,
        first: u64,
    ) {
        for row in self.rows.iter_mut().take(self.count).flatten() {
            if row.name == name
                && row.expectation == expectation
                && row.size == range.size
                && row.permissions == range.permissions
            {
                row.pages += pages;
                // 同じ名前の領域のどれかが「数える」なら、行も数える。
                row.counted |= counted;
                return;
            }
        }
        if self.count == N {
            self.dropped += 1;
            return;
        }
        self.rows[self.count] = Some(Row {
            name,
            expectation,
            counted,
            size: range.size,
            permissions: range.permissions,
            pages,
            first,
        });
        self.count += 1;
    }

    /// 行の数。
    pub fn len(&self) -> usize {
        self.count
    }

    /// 行が 1 つも無いか。
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// どの領域にも入らない範囲の行の数。
    pub fn unnamed(&self) -> usize {
        self.rows().filter(|row| row.is_unnamed()).count()
    }

    /// 在ってはならない領域に写っている行の数。
    pub fn unexpected(&self) -> usize {
        self.rows().filter(|row| row.is_unexpected()).count()
    }

    /// 要約の値（純粋な論理）。**行の順には依らない。** 領域の一覧も渡す——**在ってはならない領域が登録されている
    /// こと自体も値に入れる**（登録が消えると、見張りのページを見なくなるため）。
    pub fn digest(&self, regions: &[Region]) -> u64 {
        let mut sum = 0u64;
        for row in self.rows() {
            let mut hash = Fnv::new();
            hash.text(row.name);
            hash.byte(match row.expectation {
                Expectation::Mapped => 0,
                Expectation::Absent => 1,
            });
            hash.text(row.size.label());
            hash.permissions(&row.permissions);
            if row.counted {
                hash.number(row.pages);
            }
            sum = sum.wrapping_add(hash.finish());
        }
        // 在ってはならない領域は、名前ごとに 1 度だけ入れる。
        for (index, region) in regions.iter().enumerate() {
            if region.expectation != Expectation::Absent {
                continue;
            }
            if regions[..index].iter().any(|earlier| {
                earlier.expectation == Expectation::Absent && earlier.name == region.name
            }) {
                continue;
            }
            let mut hash = Fnv::new();
            hash.text("absent:");
            hash.text(region.name);
            sum = sum.wrapping_add(hash.finish());
        }
        sum.wrapping_add(self.dropped as u64)
    }
}

/// FNV-1a（64 ビット）。要約の値を作るためだけに使う。
struct Fnv(u64);

impl Fnv {
    const fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn byte(&mut self, byte: u8) {
        self.0 ^= u64::from(byte);
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    fn text(&mut self, text: &str) {
        for byte in text.bytes() {
            self.byte(byte);
        }
        // 区切り。隣り合う 2 つの文字列の切れ目を動かしても、同じ値にならないようにする。
        self.byte(0xff);
    }

    fn number(&mut self, number: u64) {
        for byte in number.to_le_bytes() {
            self.byte(byte);
        }
    }

    fn permissions(&mut self, permissions: &MappingPermissions) {
        for flag in [
            permissions.writable,
            permissions.user,
            permissions.executable,
            permissions.global,
            permissions.shared,
            permissions.tables_writable,
            permissions.tables_user,
            permissions.tables_executable,
        ] {
            self.byte(u8::from(flag));
        }
        self.byte(permissions.cache);
    }

    const fn finish(&self) -> u64 {
        self.0
    }
}

/// 登録された領域の集まり。
#[derive(Clone, Copy)]
pub struct Regions {
    items: [Option<Region>; MAX_REGIONS],
    count: usize,
    /// 上限を越えて登録できなかった数。
    refused: usize,
}

impl Default for Regions {
    fn default() -> Self {
        Self::new()
    }
}

impl Regions {
    /// 空の集まり。
    pub const fn new() -> Self {
        Regions {
            items: [None; MAX_REGIONS],
            count: 0,
            refused: 0,
        }
    }

    /// 領域を 1 つ足す。**空の範囲（終わりが先頭以下）は足さない。**
    pub fn add(&mut self, region: Region) {
        if region.end <= region.start {
            return;
        }
        if self.count == MAX_REGIONS {
            self.refused += 1;
            return;
        }
        self.items[self.count] = Some(region);
        self.count += 1;
    }

    /// その名前の領域を、全部「在ってはならない領域」に変える。変えた数を返す。
    pub fn retire(&mut self, name: &str) -> usize {
        let mut changed = 0;
        for region in self.items.iter_mut().take(self.count).flatten() {
            if region.name == name {
                region.expectation = Expectation::Absent;
                region.counted = true;
                changed += 1;
            }
        }
        changed
    }

    /// 上限を越えて登録できなかった数。
    pub fn refused(&self) -> usize {
        self.refused
    }

    /// 登録された領域を、配列へ写して返す（`Survey::add` に渡す形）。
    fn list(&self) -> ([Region; MAX_REGIONS], usize) {
        let mut list = [Region::mapped("", 0, 0, false); MAX_REGIONS];
        let mut count = 0;
        for region in self.items.iter().take(self.count).flatten() {
            list[count] = *region;
            count += 1;
        }
        (list, count)
    }
}

/// 起動時のプログラムを走らせ終えたか（[`boot_programs_are_done`]）。
static BOOT_PROGRAMS_DONE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 起動時のプログラムを走らせ終えたことを知らせる（`init` がシェルを起こす直前に呼ぶ）。**以後、既定のビルドは
/// ユーザーのプログラムの一覧を出さない**（[`report_user`]）。
pub fn boot_programs_are_done() {
    BOOT_PROGRAMS_DONE.store(true, core::sync::atomic::Ordering::SeqCst);
}

/// カーネルの表の領域（起動の途中で、写した所が足していく）。
static KERNEL_REGIONS: Locked<Regions> = Locked::new(Regions::new());

/// カーネルの表の領域を 1 つ登録する（写っているはずの領域）。
pub fn register(name: &'static str, start: u64, end: u64, counted: bool) {
    KERNEL_REGIONS
        .lock()
        .add(Region::mapped(name, start, end, counted));
}

/// カーネルの表の領域を 1 つ登録する（何も写っていてはならない領域）。
pub fn register_absent(name: &'static str, start: u64, end: u64) {
    KERNEL_REGIONS.lock().add(Region::absent(name, start, end));
}

/// その名前で登録した領域を、全部「在ってはならない領域」に変える（写像を外した後に呼ぶ）。
pub fn retire(name: &str) {
    KERNEL_REGIONS.lock().retire(name);
}

/// 稼働中のカーネルの表を歩き、登録された領域で一覧を作って出す。
///
/// `moment` は時点の名前である。**起動の途中の 1 本の流れか、BKL を持っている間に呼ぶこと**（歩いている間に
/// 表が変わらないこと）。
///
/// # Safety
///
/// `root` が稼働中のカーネルの表の根で、`direct_map` を通して配下の表が全部読めること。
pub unsafe fn report_kernel(
    logger: &mut Logger<Serial>,
    moment: &dyn core::fmt::Display,
    root: PhysAddr,
    direct_map: DirectMap,
) {
    // **錠の中では写すだけにする**（歩く間と出す間は持たない）。
    let (regions, refused) = {
        let regions = KERNEL_REGIONS.lock();
        (regions.list(), regions.refused())
    };
    let regions = &regions.0[..regions.1];
    let mut survey = Survey::<KERNEL_ROWS>::new();
    // SAFETY: 呼び出し元契約による。読み取りのみ。
    unsafe {
        for_each_mapped_range(root, direct_map, 0..512, &mut |range| {
            survey.add(regions, range)
        })
    };
    print(logger, moment, &survey, regions, refused);
}

/// 稼働していないユーザーの空間の表を歩き、渡された領域で一覧を作って出す。**歩くのは `top` の添字だけである**
/// （ユーザーの側だけ。カーネルと共有している側は歩かない）。
///
/// **既定のビルドでは、起動時のプログラムを走らせ終えた後は何もしない**（[`boot_programs_are_done`]）。
/// `page-permissions-dump` を付けたビルドは、いつでも出す。
///
/// # Safety
///
/// `root` が有効な表の根で、`direct_map` を通して配下の表が全部読めること。歩いている間に表が変わらないこと。
pub unsafe fn report_user(
    logger: &mut Logger<Serial>,
    moment: &dyn core::fmt::Display,
    root: PhysAddr,
    direct_map: DirectMap,
    top: Range<usize>,
    regions: &[Region],
) {
    if BOOT_PROGRAMS_DONE.load(core::sync::atomic::Ordering::SeqCst)
        && !cfg!(feature = "page-permissions-dump")
    {
        return;
    }
    let mut survey = Survey::<USER_ROWS>::new();
    // SAFETY: 呼び出し元契約による。読み取りのみ。
    unsafe {
        for_each_mapped_range(root, direct_map, top, &mut |range| {
            survey.add(regions, range)
        })
    };
    print(logger, moment, &survey, regions, 0);
}

/// 一覧を出す。**要約の 1 行は必ず出す。** 全部の行は `page-permissions-dump` のときだけ出す。
fn print<const N: usize>(
    logger: &mut Logger<Serial>,
    moment: &dyn core::fmt::Display,
    survey: &Survey<N>,
    regions: &[Region],
    refused: usize,
) {
    let all = cfg!(feature = "page-permissions-dump");
    for row in survey.rows() {
        if !(all || row.is_unnamed() || row.is_unexpected()) {
            continue;
        }
        let permissions = &row.permissions;
        logger.info(format_args!(
            "page-perms: {moment} | {}{} | {} | w={} u={} x={} cache={} g={} shared={} tables(w={} u={} x={}) | \
             pages={}{}{}",
            row.name,
            if row.is_unexpected() {
                " (MAPPED, but nothing should be here)"
            } else {
                ""
            },
            row.size.label(),
            u8::from(permissions.writable),
            u8::from(permissions.user),
            u8::from(permissions.executable),
            permissions.cache,
            u8::from(permissions.global),
            u8::from(permissions.shared),
            u8::from(permissions.tables_writable),
            u8::from(permissions.tables_user),
            u8::from(permissions.tables_executable),
            // ページ数を比べない行には印を付ける（`xtask` の側が読む）。
            if row.counted { "" } else { "~" },
            row.pages,
            Start(row.is_unnamed().then_some(row.first)),
        ));
    }
    if all {
        // 在ってはならない領域のうち、何も写っていないもの（見込みどおり）も、名前ごとに 1 行出す。
        for (index, region) in regions.iter().enumerate() {
            let first_of_its_name = !regions[..index].iter().any(|earlier| {
                earlier.expectation == Expectation::Absent && earlier.name == region.name
            });
            let mapped = survey
                .rows()
                .any(|row| row.is_unexpected() && row.name == region.name);
            if region.expectation == Expectation::Absent && first_of_its_name && !mapped {
                logger.info(format_args!(
                    "page-perms: {moment} | {} | absent | nothing is mapped here, as it should be | pages=0",
                    region.name
                ));
            }
        }
    }
    logger.info(format_args!(
        "page-perms: {moment}: {} row(s), {} unnamed, {} mapped where nothing should be, {} dropped, \
         digest={:016x}",
        survey.len(),
        survey.unnamed(),
        survey.unexpected(),
        survey.dropped() + refused,
        survey.digest(regions)
    ));
}

/// 名前の無い行の先頭の番地を出すための包み。
struct Start(Option<u64>);

impl core::fmt::Display for Start {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(address) => write!(f, " first={address:#x}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ_ONLY: MappingPermissions = MappingPermissions {
        writable: false,
        user: false,
        executable: true,
        cache: 0,
        global: false,
        shared: false,
        tables_writable: true,
        tables_user: false,
        tables_executable: true,
    };

    const WRITABLE: MappingPermissions = MappingPermissions {
        writable: true,
        ..READ_ONLY
    };

    fn range(
        start: u64,
        pages: u64,
        size: MappingSize,
        permissions: MappingPermissions,
    ) -> MappedRange {
        MappedRange {
            start,
            pages,
            size,
            permissions,
        }
    }

    fn rows<const N: usize>(survey: &Survey<N>) -> Vec<(&'static str, &'static str, bool, u64)> {
        survey
            .rows()
            .map(|row| {
                (
                    row.name,
                    row.size.label(),
                    row.permissions.writable,
                    row.pages,
                )
            })
            .collect()
    }

    /// **範囲は領域の境で分かれ、行は「名前・大きさ・権限」でまとまる。** 大きいページが領域をまたぐときも、
    /// 4KiB で数えて分ける。どの領域にも入らない分は、名前の無い行になる。
    #[test]
    fn a_range_is_split_at_region_borders_and_rows_gather_by_name_size_and_permissions() {
        let regions = [
            Region::mapped("text", 0x20_0000, 0x20_3000, false),
            Region::mapped("data", 0x20_3000, 0x20_8000, false),
        ];
        let mut survey = Survey::<8>::new();
        // 2MiB のページ 1 枚が、text・data・その先（名前なし）にまたがる。
        survey.add(&regions, range(0x20_0000, 1, MappingSize::Large, WRITABLE));
        assert_eq!(
            rows(&survey),
            vec![
                ("text", "2M", true, 3),
                ("data", "2M", true, 5),
                (UNNAMED, "2M", true, 512 - 8),
            ]
        );
        assert_eq!(survey.unnamed(), 1);
        assert_eq!(survey.rows().last().unwrap().first, 0x20_8000);

        // 同じ名前・同じ大きさ・同じ権限の範囲は、離れていても 1 行にまとまる。権限が違えば別の行になる。
        let mut survey = Survey::<8>::new();
        survey.add(&regions, range(0x20_0000, 1, MappingSize::Small, READ_ONLY));
        survey.add(&regions, range(0x20_2000, 1, MappingSize::Small, READ_ONLY));
        survey.add(&regions, range(0x20_1000, 1, MappingSize::Small, WRITABLE));
        assert_eq!(
            rows(&survey),
            vec![("text", "4K", false, 2), ("text", "4K", true, 1)]
        );
    }

    /// **領域が入れ子なら、いちばん狭い領域の名前を採る。** 見張りのページは「在ってはならない領域」で、
    /// そこに写っていれば行に印が付く。
    #[test]
    fn the_narrowest_region_wins_and_a_mapping_in_an_absent_region_is_marked() {
        let regions = [
            Region::mapped("bss", 0x30_0000, 0x34_0000, false),
            Region::absent("guard", 0x31_0000, 0x31_1000),
        ];
        // 見張りのページを外した形——その 1 ページだけ写っていない。
        let mut survey = Survey::<8>::new();
        survey.add(
            &regions,
            range(0x30_0000, 0x10, MappingSize::Small, WRITABLE),
        );
        survey.add(
            &regions,
            range(0x31_1000, 0x2f, MappingSize::Small, WRITABLE),
        );
        assert_eq!(rows(&survey), vec![("bss", "4K", true, 0x3f)]);
        assert_eq!(survey.unexpected(), 0);

        // 外し忘れた形——見張りのページも写っている。
        let mut forgotten = Survey::<8>::new();
        forgotten.add(
            &regions,
            range(0x30_0000, 0x40, MappingSize::Small, WRITABLE),
        );
        assert_eq!(
            rows(&forgotten),
            vec![("bss", "4K", true, 0x3f), ("guard", "4K", true, 1)]
        );
        assert_eq!(forgotten.unexpected(), 1);
        assert!(forgotten.rows().nth(1).unwrap().is_unexpected());
        // 像の区画はページ数を数えないが、それでも 2 つの要約の値は違う（見張りの行が増えるため）。
        assert_ne!(survey.digest(&regions), forgotten.digest(&regions));
    }

    /// **要約の値は、行の順に依らず、権限が 1 つ変われば変わる。** ページ数は、数えると登録した領域でだけ効く。
    /// 在ってはならない領域の登録が消えても変わる。
    #[test]
    fn the_digest_ignores_row_order_and_follows_permissions_and_counted_pages() {
        let regions = [
            Region::mapped("text", 0x20_0000, 0x21_0000, false),
            Region::mapped("stack", 0x40_0000, 0x41_0000, true),
            Region::absent("guard", 0x3f_f000, 0x40_0000),
        ];
        let build =
            |order: &[usize], text_pages: u64, stack_pages: u64, text: MappingPermissions| {
                let ranges = [
                    range(0x20_0000, text_pages, MappingSize::Small, text),
                    range(0x40_0000, stack_pages, MappingSize::Small, WRITABLE),
                ];
                let mut survey = Survey::<8>::new();
                for index in order {
                    survey.add(&regions, ranges[*index]);
                }
                survey.digest(&regions)
            };
        let base = build(&[0, 1], 4, 8, READ_ONLY);
        assert_eq!(base, build(&[1, 0], 4, 8, READ_ONLY), "row order");
        assert_eq!(
            base,
            build(&[0, 1], 5, 8, READ_ONLY),
            "pages of an uncounted region"
        );
        assert_ne!(
            base,
            build(&[0, 1], 4, 9, READ_ONLY),
            "pages of a counted region"
        );
        assert_ne!(base, build(&[0, 1], 4, 8, WRITABLE), "a permission");
        let not_executable = MappingPermissions {
            executable: false,
            ..READ_ONLY
        };
        assert_ne!(
            base,
            build(&[0, 1], 4, 8, not_executable),
            "another permission"
        );

        // 在ってはならない領域の登録を外すと、値が変わる。
        let mut survey = Survey::<8>::new();
        survey.add(&regions, range(0x20_0000, 4, MappingSize::Small, READ_ONLY));
        assert_ne!(survey.digest(&regions), survey.digest(&regions[..2]));
    }

    /// **行の上限を越えた分は捨て、捨てた数を持つ**（黙って欠けない）。番地の上端で終わる範囲も、あふれずに分ける。
    #[test]
    fn rows_past_the_limit_are_counted_and_a_range_ending_at_the_top_does_not_overflow() {
        let mut survey = Survey::<2>::new();
        for (index, cache) in [0u8, 1, 2, 3].into_iter().enumerate() {
            let permissions = MappingPermissions { cache, ..WRITABLE };
            survey.add(
                &[],
                range(0x1000 * index as u64, 1, MappingSize::Small, permissions),
            );
        }
        assert_eq!(survey.len(), 2);
        assert_eq!(survey.dropped(), 2);

        // 領域の終わり（含まない）は u64 で書ける上限までしか書けないが、境はページの境へ切り上げて扱うので、
        // 最後のページも領域に入る。
        let regions = [Region::mapped("top", 0xFFFF_FFFF_FFFF_E000, u64::MAX, true)];
        let mut survey = Survey::<4>::new();
        survey.add(
            &regions,
            range(0xFFFF_FFFF_FFFF_E000, 2, MappingSize::Small, WRITABLE),
        );
        assert_eq!(rows(&survey), vec![("top", "4K", true, 2)]);

        // 領域の境がページの途中に在っても、ページの境へ外向きに広げて分ける（端数の行を作らない）。
        let ragged = [Region::mapped("ragged", 0x1234, 0x2345, true)];
        let mut survey = Survey::<4>::new();
        survey.add(&ragged, range(0, 4, MappingSize::Small, WRITABLE));
        assert_eq!(
            rows(&survey),
            vec![(UNNAMED, "4K", true, 2), ("ragged", "4K", true, 2),]
        );
    }

    /// **領域の集まり**——空の範囲は足さない。名前で「在ってはならない領域」に変えられる。上限を越えた分は数える。
    #[test]
    fn regions_skip_empty_ranges_retire_by_name_and_count_what_did_not_fit() {
        let mut regions = Regions::new();
        regions.add(Region::mapped("identity", 0, 0x1000, true));
        regions.add(Region::mapped("identity", 0x2000, 0x3000, false));
        regions.add(Region::mapped("empty", 0x5000, 0x5000, true));
        let (list, count) = regions.list();
        assert_eq!(count, 2);
        assert_eq!(list[1].start, 0x2000);
        assert_eq!(regions.retire("identity"), 2);
        let (list, count) = regions.list();
        assert!(list[..count]
            .iter()
            .all(|region| region.expectation == Expectation::Absent && region.counted));
        assert_eq!(regions.retire("nothing"), 0);

        let mut full = Regions::new();
        for index in 0..MAX_REGIONS as u64 + 3 {
            full.add(Region::mapped(
                "r",
                index * 0x1000,
                (index + 1) * 0x1000,
                true,
            ));
        }
        assert_eq!(full.refused(), 3);
    }
}
