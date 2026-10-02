//! ページテーブルエントリのビット操作（M5-a-1）。
//!
//! **純粋ロジック。** ポインタに一切触らないため、ホスト `cargo test` で
//! 検証できる。実際の読み書きは [`super::active`] と [`super::table`] が行う。
//!
//! `table.rs` をホストテストの対象にできないのは、実在しない物理アドレスへの
//! 生ポインタアクセスになるためである。**ビットの計算だけはそこから切り離せる**
//! ので、こちらへ寄せた。M5-a で加わる「分割」は、まさにビットの組み替えが
//! 本体である。
//!
//! ## 階層ごとにビットの意味が違う（最大の落とし穴）
//!
//! 同じ位置のビットが、階層とページサイズによって別の意味を持つ。
//!
//! | ビット | 4KiB PTE | 2MiB PDE（PS=1） |
//! |---|---|---|
//! | 7 | **PAT** | **PS** |
//! | 12 | アドレスの最下位ビット | **PAT** |
//! | 13-20 | アドレス | 予約（0 でなければならない） |
//! | 21-51 | アドレス | アドレス |
//!
//! **2MiB を 4KiB へ分割するとき、PAT はビット 12 からビット 7 へ移さなければ
//! ならない。** 「PS を落とす」だけで済ませると、4KiB 側では PAT を 0 にする
//! 操作になり、**キャッシュ属性が静かに変わる**。
//!
//! 現在 PAT は使っていない（Write-Combining は保留項目）ので実害は無いが、
//! WC を導入した瞬間に壊れる。しかもフレームバッファの 2MiB を分割した場合、
//! 症状は「描画がおかしいが原因が分からない」になる。移送は数行で書けて
//! ホストテストで完全に固定できるので、**先に正しく実装しておく**。
//!
//! アドレスマスクも階層で違う。2MiB エントリに 4KiB 用のマスク
//! （ビット 12 から）を当てると、**PAT ビットをアドレスの一部として読む**。

use common::addr::{PhysAddr, VirtAddr};

use crate::paging::permissions::{Cache, PagePermissions};

/// Present。
pub const PTE_PRESENT: u64 = 1 << 0;
/// 書き込み可能。
pub const PTE_WRITABLE: u64 = 1 << 1;
/// ユーザーモードからアクセス可能（M5-e で使う）。
pub const PTE_USER: u64 = 1 << 2;
/// Page-level Write Through。
pub const PTE_PWT: u64 = 1 << 3;
/// Page-level Cache Disable。
pub const PTE_PCD: u64 = 1 << 4;
/// Accessed。CPU が立てる。
pub const PTE_ACCESSED: u64 = 1 << 5;
/// Dirty。CPU が立てる。
pub const PTE_DIRTY: u64 = 1 << 6;

/// PD / PDPT レベルでの「このエントリはページそのものを指す」ビット。
///
/// **4KiB PTE では同じ位置が PAT である。** モジュールの説明を参照。
pub const PDE_PAGE_SIZE: u64 = 1 << 7;

/// 4KiB PTE における PAT ビット。
pub const PTE_PAT: u64 = 1 << 7;

/// 2MiB PDE における PAT ビット。
pub const PDE_HUGE_PAT: u64 = 1 << 12;

/// Global。CR4.PGE が有効なとき、CR3 リロードでも TLB から追い出されない。
pub const PTE_GLOBAL: u64 = 1 << 8;

/// 共有メモリの目印（ソフトウェア用の空きビット 9。`ADR-0065`）。**CPU は無視する。**
/// **`AddressSpace::detach` が「この葉はアロケータのものではない（`crate::shm` が
/// 参照数で返す）」を見分けるのに使う。** **Linux も `struct page` 相当の管理に空きビットを
/// 使う思想である**（`docs/architecture.md` の「ABIの形は合わせる」）。
pub const PTE_SHARED: u64 = 1 << 9;

/// 実行の禁止（ビット 63。XD）。**`EFER.NXE` が 0 の CPU では予約のビットで、立てた項目を引くと `#PF` になる。**
///
/// **立てるのは、権限の変換（[`leaf_flags`]）である**——カーネルの側の、実行しない権限の葉に付く（2026-10-02）。
/// **途中の項目には立てない**（CPU は段ごとの実行禁止を OR で合成するので、途中に立てると、その下の全部が
/// 実行できなくなる）。どの CPU でもこのビットを持つ項目を引けることは、試し専用のページ
/// （`crate::arch::x86_64::execute_disable_probe`）で確かめている。
pub const PTE_NO_EXECUTE: u64 = 1 << 63;

/// 権限の変換が、実行しない権限の葉に実行禁止のビットを立てるか。**既定のビルドでは立てる。**
///
/// 立てないのは、破壊テストのビルドだけである。
///
/// - `leaf-ignores-execute-test`: 実行の欄を無視する（実行禁止を入れる前の形）。試しのページ
///   （`crate::arch::x86_64::execute_disable_probe`）を足した直後の読み戻しが、ビットが付いていないことを
///   見つけて止まる。
/// - `nx-probe-only-leaf-test`: 実行禁止のビットを持つ葉を、試しのページの 1 枚だけにする（そのページの葉には、
///   後から足す）。`EFER.NXE` を落とす破壊テストが、狙いの #PF の後に、例外の出力を出せるようにするためである。
pub const LEAVES_CARRY_EXECUTE_DISABLE: bool = !cfg!(any(
    feature = "leaf-ignores-execute-test",
    feature = "nx-probe-only-leaf-test"
));

/// 試しの形（`user-leaf-high-bit-test`）が、ユーザーの葉に立てるビット（52）。
///
/// **ビット 52 は、CPU が無視する、ソフトウェア用の空きビットである**（保護キーを使わない間は、52 から 62 が空いて
/// いる。カーネルは `CR4.PKE` を立てていない）。番地（ビット 12-51）より上に在るので、項目の値を番地として読む
/// 所が残っていれば、この形で表に出る。**実行禁止のビット（63）も番地より上に在る**——実行禁止を入れる前に、
/// 番地より上のビットが立った項目を、カーネルが正しく扱えることを確かめるためのものである。
pub const PTE_HIGH_BIT_FOR_TEST: u64 = 1 << 52;

/// 4KiB ページのアドレス部分（ビット 12-51）。
pub const ADDR_MASK_4K: u64 = 0x000F_FFFF_FFFF_F000;

/// 2MiB ページのアドレス部分（ビット 21-51）。
///
/// **ビット 12-20 を含めてはならない。** ビット 12 は PAT、13-20 は予約である。
pub const ADDR_MASK_2M: u64 = 0x000F_FFFF_FFE0_0000;

/// 2MiB ページの項目のうち、4KiB ならアドレスになるが、2MiB ではアドレスでないビット（12-20）。
///
/// ビット 12 は PAT、13-20 は予約である。分割するとき、これらを 4KiB の項目へ持ち込まない。
pub const PDE_HUGE_BELOW_ADDRESS: u64 = ADDR_MASK_4K & !ADDR_MASK_2M;

/// 2MiB ページの大きさ。
pub const PAGE_SIZE_2M: u64 = 2 * 1024 * 1024;
/// 4KiB ページの大きさ。
pub const PAGE_SIZE_4K: u64 = 4096;
/// 1 つのテーブルが持つエントリ数。
pub const ENTRIES_PER_TABLE: usize = 512;

/// 中間テーブルを指すエントリのアドレス部分（ビット 12-51）。
///
/// 中間テーブルは常に 4KiB なので 4KiB 用のマスクでよい。
pub const ADDR_MASK_TABLE: u64 = ADDR_MASK_4K;

pub const fn is_present(entry: u64) -> bool {
    entry & PTE_PRESENT != 0
}

/// PD / PDPT レベルで、このエントリがページそのものを指しているか。
///
/// **PT レベル（4KiB）のエントリに対して呼んではならない。** そちらでは同じ
/// ビットが PAT を意味する。
pub const fn is_shared(entry: u64) -> bool {
    entry & PTE_SHARED != 0
}

pub const fn is_huge(entry: u64) -> bool {
    entry & PDE_PAGE_SIZE != 0
}

/// 中間テーブルの物理アドレス。
pub const fn table_address(entry: u64) -> PhysAddr {
    PhysAddr::new_const(entry & ADDR_MASK_TABLE)
}

/// 4KiB ページの物理アドレス。
pub const fn page_address_4k(entry: u64) -> PhysAddr {
    PhysAddr::new_const(entry & ADDR_MASK_4K)
}

/// 2MiB ページの物理アドレス。
pub const fn page_address_2m(entry: u64) -> PhysAddr {
    PhysAddr::new_const(entry & ADDR_MASK_2M)
}

// 仮想アドレスから各階層の添字を取り出す。
//
// **実装は `VirtAddr` 側に集約してある（T-2b）。** ここは呼び出しを
// 中継するだけである。二重に持つと、片方だけ直したときに食い違う。
pub const fn pml4_index(addr: VirtAddr) -> usize {
    addr.top_index()
}
pub const fn pdpt_index(addr: VirtAddr) -> usize {
    addr.upper_index()
}
pub const fn pd_index(addr: VirtAddr) -> usize {
    addr.middle_index()
}
pub const fn pt_index(addr: VirtAddr) -> usize {
    addr.leaf_index()
}

/// x86_64 の仮想アドレスが正規形（canonical）か。
///
/// **ビット 47 が 48-63 へ符号拡張されていなければならない。** 非正規の
/// アドレスは、そもそも CPU が拒否する（`mov` で #GP になる）。添字計算は
/// 非正規でも「それらしい」値を返してしまうので、入口で弾く。
///
/// 「マップされていない」と「アドレスが不正」は**別の状態**である。
pub const fn is_canonical(addr: u64) -> bool {
    common::addr::is_canonical(addr)
}

/// 葉の大きさ。**同じ権限でも、大きさでビットが変わる**（2MiB の葉は PD の項目で、PS を立てる）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafSize {
    /// 4KiB（PT の項目）。
    Small,
    /// 2MiB（PD の項目）。
    Large,
}

/// 権限を、葉の項目のビットへ直す（2026-10-02。`ADR-0071` の手順 3）。**番地は含まない。**
///
/// **ページの権限をビットへ直すのは、この関数と [`table_flags`] だけである。** 起動の表、起動の途中に組む表、
/// 稼働中の表へ足す 1 枚、ユーザーの空間へ足す 1 枚が、どれもここを通る。
///
/// | 権限の欄 | ビット |
/// |---|---|
/// | （常に） | P |
/// | `write` | W |
/// | `user` | U |
/// | `cache` が `Uncached` | PCD |
/// | `shared` | ビット 9（[`PTE_SHARED`]） |
/// | 大きさが `Large` | PS |
/// | `execute` でなく、`user` でもない | 実行禁止（ビット 63。[`PTE_NO_EXECUTE`]） |
///
/// **`execute` を読むのは、カーネルの側の権限だけである**（2026-10-02。`ADR-0071` の手順 4）。ユーザーの側の葉には、
/// まだ実行禁止のビットを立てない（ユーザーの写像に入れるときに、`user` の条件を外す）。G・PWT・PAT は立てない。
pub const fn leaf_flags(permissions: PagePermissions, size: LeafSize) -> u64 {
    let mut flags = PTE_PRESENT;
    // 破壊テスト (S9-a, map-force-writable): 書けるかの欄を無視して、葉を常に W=1 にする。
    // 読み取り専用でマップしたユーザーのページへ Ring 3 が書けてしまい、ring3-vectors の #PF-write-ro が
    // #PF ではなく後続の ud2 で終わる。**以前は、葉を書く 2 つの関数がそれぞれ持っていた。**
    if permissions.write() || cfg!(feature = "map-force-writable") {
        flags |= PTE_WRITABLE;
    }
    if permissions.user() {
        flags |= PTE_USER;
    }
    if matches!(permissions.cache(), Cache::Uncached) {
        flags |= PTE_PCD;
    }
    if permissions.shared() {
        flags |= PTE_SHARED;
    }
    if matches!(size, LeafSize::Large) {
        flags |= PDE_PAGE_SIZE;
    }
    if !permissions.execute() && !permissions.user() && LEAVES_CARRY_EXECUTE_DISABLE {
        flags |= PTE_NO_EXECUTE;
    }
    // 試しの形 (user-leaf-high-bit-test): ユーザーから届く葉に、番地より上の位置のビットを立てる。
    // **破壊ではない**——CPU は無視するので、正しいカーネルは今までどおり動く。項目の値を番地として読む所が
    // 在ると、フレームの会計が合わなくなって止まる。
    if cfg!(feature = "user-leaf-high-bit-test") && permissions.user() {
        flags |= PTE_HIGH_BIT_FOR_TEST;
    }
    flags
}

/// 権限を、葉へ降りる途中の項目（下の段の表を指す項目）のビットへ直す。**番地は含まない。**
///
/// **P と W は常に立てる。U は、葉がユーザーから届くときだけ立てる。実行禁止のビットは立てない。**
/// CPU は段ごとの W と U を AND で、実行禁止を OR で合成するので、途中の段は葉より緩くしておき、実際の権限は葉で決める。
/// PCD と共有の印は葉だけのもので、途中の項目には立てない（途中の項目の PCD は、表そのものを読むときの
/// キャッシュの扱いを意味する）。
pub const fn table_flags(permissions: PagePermissions) -> u64 {
    table_flags_for(permissions.user())
}

/// 途中の項目のビット。**決めるのは「下の葉がユーザーから届くか」だけである。**
///
/// 権限から作るとき（[`table_flags`]）と、2MiB の葉を分割して途中の項目に置き換えるとき
/// （[`table_entry_for_split`]。元の葉の U を引き継ぐ）の両方が、ここを通る。
const fn table_flags_for(user: bool) -> u64 {
    let mut flags = PTE_PRESENT | PTE_WRITABLE;
    if user {
        flags |= PTE_USER;
    }
    flags
}

/// 2MiB の葉を、キャッシュしない葉にするときに足すビット。**試し専用の
/// `ActivePageTable::set_huge_page_uncached` だけが使う**（既に在る葉の権限を変える、ただ 1 つの所である）。
#[cfg(feature = "paging-test")]
pub const UNCACHED_LEAF_FLAG: u64 = PTE_PCD;

/// 葉の項目を作る。**番地は、大きさに合ったマスクを通す**（4KiB はビット 12 から、2MiB はビット 21 から）。
pub const fn leaf_entry(frame: PhysAddr, permissions: PagePermissions, size: LeafSize) -> u64 {
    let address = match size {
        LeafSize::Small => frame.as_u64() & ADDR_MASK_4K,
        LeafSize::Large => frame.as_u64() & ADDR_MASK_2M,
    };
    address | leaf_flags(permissions, size)
}

/// 下の段の表を指す項目を作る。
pub const fn table_entry(table: PhysAddr, permissions: PagePermissions) -> u64 {
    (table.as_u64() & ADDR_MASK_TABLE) | table_flags(permissions)
}

/// 2MiB ページのエントリから、分割後の `index` 番目の 4KiB エントリを作る。
///
/// # PAT の移送
///
/// **ビット 12（2MiB の PAT）をビット 7（4KiB の PAT）へ移す。**
/// 単に PS を落とすだけでは、4KiB 側の PAT が 0 になりキャッシュ属性が変わる
/// （モジュールの説明を参照）。
///
/// # 移送しないもの
///
/// - **PS ビットは落とす。** 4KiB PTE には存在しない意味である
/// - **Accessed / Dirty は落とす。** CPU が立てるものであり、分割後の各ページに
///   一律で引き継ぐと「触っていないのに触ったことになっている」状態を作る
pub const fn split_child_entry(huge_entry: u64, index: usize) -> u64 {
    let base = page_address_2m(huge_entry).as_u64();
    let address = base + (index as u64) * PAGE_SIZE_4K;

    // PS・PAT(bit12)・Accessed・Dirty・アドレスを除いたフラグ。
    let mut flags =
        huge_entry & !(PDE_PAGE_SIZE | PDE_HUGE_PAT | PTE_ACCESSED | PTE_DIRTY | ADDR_MASK_2M);
    // 2MiB では予約だったビット 13-20 も落としておく（本来 0 のはずだが、
    // 万一立っていたら 4KiB ではアドレスの一部として解釈されてしまう）。
    flags &= !PDE_HUGE_BELOW_ADDRESS;

    // **PAT をビット 12 からビット 7 へ移す。**
    if huge_entry & PDE_HUGE_PAT != 0 {
        flags |= PTE_PAT;
    }

    // 検証用に、わざと PCD を落とす（`paging-test-drop-pcd`）。
    // 読み戻し照合が実際にキャッシュ属性の変化を検出するかを確かめるための
    // もので、通常ビルドには入らない。
    #[cfg(feature = "paging-test-drop-pcd")]
    let flags = flags & !PTE_PCD;

    address | flags
}

/// 分割後に PD へ書き戻す、PT を指すエントリを作る。
///
/// # 何を引き継ぎ、何を引き継がないか
///
/// **PRESENT と WRITABLE は立てる。USER は元エントリから引き継ぐ。**
/// CPU は階層ごとの R/W・U/S・NX を **AND** で合成する。親が子より厳しいと、
/// 子で許可したものが効かなくなる。2MiB ページがユーザーからアクセス可能
/// だったなら、分割後の PT を指すエントリも USER でなければならない。
///
/// **PCD / PWT は引き継がない。** 中間エントリのこれらは「PT フレーム自身を
/// 読むときのキャッシュ属性」を意味し、ページの属性ではない。ページ側の
/// PCD / PWT は [`split_child_entry`] が各 PTE へ保存している。ここで一緒に
/// 立てると、ページテーブルのウォークまでキャッシュ無効になる。
///
/// **PS は立てない。** ここが指すのは PT であってページではない。
///
/// **Accessed / Dirty も引き継がない。** CPU が立てるものである。
pub const fn table_entry_for_split(huge_entry: u64, table_phys: PhysAddr) -> u64 {
    (table_phys.as_u64() & ADDR_MASK_TABLE) | table_flags_for(huge_entry & PTE_USER != 0)
}

/// 分割後の PT に書き込む 512 エントリを組み立てる。
///
/// # なぜ配列を返す関数にするのか
///
/// 実際の書き込み（[`super::active`]）は生ポインタを触るのでホストテストに
/// できない。**ビットの計算だけを切り離せば、そこはホストで固定できる。**
/// PAT を使い始めるのは Write-Combining を導入するときで、それまで実機の
/// 2MiB エントリにビット 12 が立つことは無い。つまり **PAT の移送が正しい
/// ことは実機では確かめられない**（`deferred-decisions.md`）。移送する経路が
/// 呼ばれていることまでは実機で言えるが、移送の中身はここで固定するしかない。
pub fn split_children(huge_entry: u64) -> [u64; ENTRIES_PER_TABLE] {
    core::array::from_fn(|index| split_child_entry(huge_entry, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト内で期待値を組み立てるための補助。
    fn p(raw: u64) -> PhysAddr {
        PhysAddr::new(raw).unwrap()
    }

    /// **今ある組み合わせの全部が、決めたとおりのビットになる**（書く側が作りうる組み合わせ。2026-10-02 に数え、
    /// 同じ日に、カーネルの側の実行しない権限へ実行禁止のビットを足した）。
    /// 期待は、定数の名前ではなく数で書く——定数を取り違えても、ここで落ちるようにする。
    #[test]
    fn every_combination_in_use_converts_to_the_bits_it_has_today() {
        use LeafSize::{Large, Small};
        const P: u64 = 0x001;
        const W: u64 = 0x002;
        const U: u64 = 0x004;
        const PCD: u64 = 0x010;
        const PS: u64 = 0x080;
        const SHARED: u64 = 0x200;
        const XD: u64 = 0x8000_0000_0000_0000;
        let leaves: [(PagePermissions, LeafSize, u64, &str); 15] = [
            (
                PagePermissions::boot_table_unrestricted(),
                Large,
                P | W | PS,
                "起動の表",
            ),
            (
                PagePermissions::kernel_code(),
                Small,
                P,
                "カーネルの像のコード",
            ),
            (
                PagePermissions::kernel_data(),
                Large,
                P | W | PS | XD,
                "恒等と直接マッピングの 2MiB",
            ),
            (
                PagePermissions::kernel_device(),
                Large,
                P | W | PCD | PS | XD,
                "組む表の 2MiB の MMIO",
            ),
            (
                PagePermissions::kernel_data(),
                Small,
                P | W | XD,
                "像の .data と .bss、恒等と直接マッピングの 4KiB、CPU ごとのスタック",
            ),
            (
                PagePermissions::kernel_device(),
                Small,
                P | W | PCD | XD,
                "組む表の 4KiB の MMIO、APIC",
            ),
            (
                PagePermissions::kernel_read_only(),
                Small,
                P | XD,
                "像の読むだけの区画、実行禁止を確かめる試しのページ",
            ),
            (
                PagePermissions::kernel_read_only(),
                Large,
                P | PS | XD,
                "読むだけの区画が 2MiB を越えたときの葉",
            ),
            (
                PagePermissions::kernel_code(),
                Large,
                P | PS,
                "コードが 2MiB を越えたときの葉",
            ),
            (
                PagePermissions::user_data(),
                Small,
                P | W | U,
                "スタック、brk",
            ),
            (
                PagePermissions::user_program(true, false),
                Small,
                P | W | U,
                "書ける区画",
            ),
            (
                PagePermissions::user_program(false, true),
                Small,
                P | U,
                "実行する区画",
            ),
            (
                PagePermissions::user_program(false, false),
                Small,
                P | U,
                "読むだけの区画",
            ),
            (
                PagePermissions::user_shared(true),
                Small,
                P | W | U | SHARED,
                "書ける共有",
            ),
            (
                PagePermissions::user_shared(false),
                Small,
                P | U | SHARED,
                "読むだけの共有",
            ),
        ];
        for (permissions, size, expected, what) in leaves {
            assert_eq!(
                leaf_flags(permissions, size),
                expected,
                "{what}: {permissions:?}"
            );
        }
        // 途中の項目は、ユーザーから届くかどうかだけで決まる。
        // **途中の項目には、葉が実行しない権限でも、実行禁止のビットを立てない**（CPU は段ごとの実行禁止を OR で
        // 合成するので、立てると、その下の全部が実行できなくなる）。
        for permissions in [
            PagePermissions::boot_table_unrestricted(),
            PagePermissions::kernel_code(),
            PagePermissions::kernel_data(),
            PagePermissions::kernel_read_only(),
            PagePermissions::kernel_device(),
        ] {
            assert_eq!(table_flags(permissions), P | W, "{permissions:?}");
        }
        for permissions in [
            PagePermissions::user_data(),
            PagePermissions::user_program(false, true),
            PagePermissions::user_shared(false),
        ] {
            assert_eq!(table_flags(permissions), P | W | U, "{permissions:?}");
        }
    }

    /// **名前を付けたマスクは、以前の直書きの値と同じである**（`0x1F_F000`。ビット 12 から 20）。
    #[test]
    fn the_bits_below_a_huge_page_address_are_twelve_to_twenty() {
        assert_eq!(PDE_HUGE_BELOW_ADDRESS, 0x1F_F000);
    }

    /// **起動の表が直書きしている 2 つの値と同じである**（`kernel/src/main.rs` の `global_asm!` の `0x83` と `0x03`）。
    #[test]
    fn the_boot_table_literals_are_what_the_conversion_gives() {
        let everything = PagePermissions::boot_table_unrestricted();
        assert_eq!(leaf_flags(everything, LeafSize::Large), 0x83);
        assert_eq!(table_flags(everything), 0x03);
    }

    /// **実行の欄がビットになるのは、カーネルの側の権限だけである**（2026-10-02）。カーネルの側は、実行しない権限の
    /// 葉に実行禁止のビット（63）が立ち、実行する権限の葉には立たない。**ユーザーの側は、実行の欄に依らず、
    /// まだ立たない。** 途中の項目には、どの権限でも立たない。
    #[test]
    fn the_execute_field_reaches_the_bits_on_the_kernel_side_only() {
        for writable in [false, true] {
            let executable = leaf_flags(
                PagePermissions::user_program(writable, true),
                LeafSize::Small,
            );
            let not_executable = leaf_flags(
                PagePermissions::user_program(writable, false),
                LeafSize::Small,
            );
            assert_eq!(executable, not_executable);
        }
        for permissions in [
            PagePermissions::boot_table_unrestricted(),
            PagePermissions::kernel_code(),
            PagePermissions::user_data(),
            PagePermissions::user_program(true, false),
            PagePermissions::user_shared(true),
        ] {
            for size in [LeafSize::Small, LeafSize::Large] {
                assert_eq!(leaf_flags(permissions, size) >> 52, 0, "{permissions:?}");
            }
        }
        for permissions in [
            PagePermissions::kernel_data(),
            PagePermissions::kernel_read_only(),
            PagePermissions::kernel_device(),
        ] {
            for size in [LeafSize::Small, LeafSize::Large] {
                assert_eq!(
                    leaf_flags(permissions, size) >> 52,
                    0x800,
                    "{permissions:?}"
                );
            }
        }
        for permissions in [
            PagePermissions::boot_table_unrestricted(),
            PagePermissions::kernel_code(),
            PagePermissions::kernel_data(),
            PagePermissions::kernel_read_only(),
            PagePermissions::kernel_device(),
            PagePermissions::user_data(),
            PagePermissions::user_shared(true),
        ] {
            assert_eq!(table_flags(permissions) >> 52, 0, "{permissions:?}");
        }
    }

    /// **2MiB の葉を分割するとき、実行禁止のビットは 4KiB の葉へそのまま引き継ぐ。** 途中の項目へは持ち込まない。
    #[test]
    fn splitting_keeps_the_execute_disable_bit_on_the_leaves_only() {
        let huge = leaf_entry(
            p(0x0000_0000_4020_0000),
            PagePermissions::kernel_data(),
            LeafSize::Large,
        );
        assert_ne!(huge & PTE_NO_EXECUTE, 0);
        for index in [0, 1, 511] {
            let child = split_child_entry(huge, index);
            assert_ne!(child & PTE_NO_EXECUTE, 0, "child {index}");
            assert_eq!(
                page_address_4k(child).as_u64(),
                0x0000_0000_4020_0000 + (index as u64) * 4096
            );
        }
        let table = table_entry_for_split(huge, p(0x0000_0000_0030_0000));
        assert_eq!(table & PTE_NO_EXECUTE, 0);
        // 実行できる葉を分割しても、実行禁止のビットは付かない。
        let code = leaf_entry(
            p(0x0000_0000_4020_0000),
            PagePermissions::kernel_code(),
            LeafSize::Large,
        );
        assert_eq!(split_child_entry(code, 3) & PTE_NO_EXECUTE, 0);
    }

    /// **項目を作る関数は、番地に、大きさに合ったマスクを通してからビットを足す。** 揃った番地では、番地と
    /// ビットの OR と同じになる（今の書く側は、マスクする所としない所が在るが、番地が揃っているので同じ値である）。
    #[test]
    fn entries_carry_the_masked_address_and_the_flags() {
        let user = PagePermissions::user_data();
        let small = p(0x0000_0012_3456_7000);
        assert_eq!(
            leaf_entry(small, user, LeafSize::Small),
            0x0000_0012_3456_7000 | 0x007
        );
        let kernel = PagePermissions::boot_table_unrestricted();
        let large = p(0x0000_0000_4020_0000);
        assert_eq!(
            leaf_entry(large, kernel, LeafSize::Large),
            0x0000_0000_4020_0000 | 0x083
        );
        // 2MiB の葉では、ビット 12 から 20 を番地として残さない（ビット 12 は PAT、13 から 20 は予約）。
        let unaligned = p(0x0000_0000_4021_F000);
        assert_eq!(
            leaf_entry(unaligned, kernel, LeafSize::Large),
            0x0000_0000_4020_0000 | 0x083
        );
        assert_eq!(table_entry(small, user), 0x0000_0012_3456_7000 | 0x007);
        assert_eq!(table_entry(small, kernel), 0x0000_0012_3456_7000 | 0x003);
    }

    /// 分割の基本。アドレスが 4KiB 刻みで並び、両端が正しいこと。
    #[test]
    fn split_produces_four_kib_pages_covering_the_same_range() {
        let base = 0x0000_0000_4020_0000; // 2MiB アライン
        let huge = base | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE;

        assert_eq!(page_address_4k(split_child_entry(huge, 0)), p(base));
        assert_eq!(
            page_address_4k(split_child_entry(huge, 1)),
            p(base + PAGE_SIZE_4K)
        );
        assert_eq!(
            page_address_4k(split_child_entry(huge, ENTRIES_PER_TABLE - 1)),
            p(base + PAGE_SIZE_2M - PAGE_SIZE_4K)
        );
    }

    /// **PS ビットは落とす。** 残すと 4KiB PTE では PAT の意味になる。
    #[test]
    fn split_clears_the_page_size_bit() {
        let huge = 0x4020_0000 | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE;
        for index in [0, 1, 255, ENTRIES_PER_TABLE - 1] {
            let child = split_child_entry(huge, index);
            assert_eq!(child & PDE_PAGE_SIZE, 0, "index={index}");
        }
    }

    /// キャッシュ属性が保存されること。
    ///
    /// フレームバッファは PCD でマップしている（ADR-0015）。分割で PCD が
    /// 落ちるとキャッシュ無効が解け、**描画がおかしいのに原因が分からない**
    /// という形で出る。
    #[test]
    fn split_preserves_the_cache_attributes() {
        let huge = 0x8000_0000 | PTE_PRESENT | PTE_WRITABLE | PTE_PCD | PDE_PAGE_SIZE;
        let child = split_child_entry(huge, 7);
        assert_eq!(child & PTE_PCD, PTE_PCD, "PCD が保存されない");
        assert_eq!(child & PTE_PRESENT, PTE_PRESENT);
        assert_eq!(child & PTE_WRITABLE, PTE_WRITABLE);
    }

    /// **PAT はビット 12 からビット 7 へ移す。**
    ///
    /// 階層でビットの意味が違うことによる、このモジュール最大の落とし穴。
    /// 2MiB では PS があった位置（ビット 7）が、4KiB では PAT になる。
    #[test]
    fn split_moves_the_pat_bit_from_twelve_to_seven() {
        let huge = 0x4020_0000 | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE | PDE_HUGE_PAT;

        for index in [0, 1, 3, ENTRIES_PER_TABLE - 1] {
            let child = split_child_entry(huge, index);
            assert_eq!(child & PTE_PAT, PTE_PAT, "index={index}: 4KiB 側の PAT");
            assert_eq!(
                page_address_4k(child),
                p(0x4020_0000 + index as u64 * PAGE_SIZE_4K),
                "index={index}: アドレスが PAT に汚染されていない"
            );
        }

        // **ビット 12 の意味が変わることを明示する。** 2MiB では PAT フラグ
        // だったが、4KiB ではアドレスの最下位ビットである。したがって
        // 立つかどうかは添字だけで決まり、元の PAT とは無関係になる。
        assert_eq!(
            split_child_entry(huge, 0) & PDE_HUGE_PAT,
            0,
            "index=0 はアドレスのビット 12 が 0"
        );
        assert_eq!(
            split_child_entry(huge, 1) & PDE_HUGE_PAT,
            PDE_HUGE_PAT,
            "index=1 はアドレスのビット 12 が 1（PAT だからではない）"
        );
        // PAT を持たない元エントリでも、添字が同じならビット 12 は同じ。
        let huge_no_pat = 0x4020_0000 | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE;
        assert_eq!(
            split_child_entry(huge_no_pat, 1) & PDE_HUGE_PAT,
            PDE_HUGE_PAT
        );
    }

    /// PAT が立っていなければ、4KiB 側でも立たないこと。
    #[test]
    fn split_without_pat_leaves_bit_seven_clear() {
        let huge = 0x4020_0000 | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE;
        let child = split_child_entry(huge, 3);
        assert_eq!(child & PTE_PAT, 0);
    }

    /// Accessed / Dirty は引き継がない。
    ///
    /// CPU が立てるものであり、512 ページすべてに一律で引き継ぐと
    /// 「触っていないのに触ったことになっている」状態を作る。
    #[test]
    fn split_does_not_inherit_accessed_or_dirty() {
        let huge =
            0x4020_0000 | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE | PTE_ACCESSED | PTE_DIRTY;
        let child = split_child_entry(huge, 0);
        assert_eq!(child & PTE_ACCESSED, 0);
        assert_eq!(child & PTE_DIRTY, 0);
    }

    /// **2MiB エントリに 4KiB 用のマスクを当ててはならない。**
    ///
    /// ビット 12 は PAT であって、アドレスの一部ではない。
    #[test]
    fn the_two_address_masks_differ_at_the_pat_bit() {
        let huge = 0x4020_0000 | PDE_HUGE_PAT | PTE_PRESENT | PDE_PAGE_SIZE;
        assert_eq!(page_address_2m(huge), p(0x4020_0000), "正しいマスク");
        assert_eq!(
            page_address_4k(huge),
            p(0x4020_0000 | PDE_HUGE_PAT),
            "誤ったマスクだと PAT がアドレスに混ざる"
        );
    }

    /// 添字の取り出し。既知のアドレスで各階層を固定する。
    ///
    /// **実装は `VirtAddr` 側にあり、ここは中継である（T-2b）。**
    /// それでもこのテストを残すのは、中継の対応（pml4 が pml4 を呼ぶ、
    /// 等）を取り違えていないことを見るためである。実装が同じでも、
    /// 繋ぎ間違いは起こる。
    #[test]
    fn the_indices_decompose_a_known_address() {
        // PML4=1, PDPT=2, PD=3, PT=4 になるアドレスを組み立てる。
        let addr =
            VirtAddr::new((1u64 << 39) | (2u64 << 30) | (3u64 << 21) | (4u64 << 12)).unwrap();
        assert_eq!(pml4_index(addr), 1);
        assert_eq!(pdpt_index(addr), 2);
        assert_eq!(pd_index(addr), 3);
        assert_eq!(pt_index(addr), 4);
    }

    /// 正規形の境界。
    ///
    /// ビット 47 が 48-63 へ符号拡張されていなければならない。境界の
    /// すぐ内側と外側を固定する。
    #[test]
    fn canonical_addresses_are_recognised_at_the_boundary() {
        // 下半分の上端。
        assert!(is_canonical(0x0000_7FFF_FFFF_FFFF));
        // その 1 つ上は非正規（穴の始まり）。
        assert!(!is_canonical(0x0000_8000_0000_0000));
        // 上半分の下端。
        assert!(is_canonical(0xFFFF_8000_0000_0000));
        // その 1 つ下は非正規（穴の終わり）。
        assert!(!is_canonical(0xFFFF_7FFF_FFFF_FFFF));
        // ありふれた値。
        assert!(is_canonical(0));
        assert!(is_canonical(0x10_0000));
        assert!(is_canonical(u64::MAX));
    }

    /// PT を指す親エントリは PS を立てず、PRESENT と WRITABLE を持つ。
    #[test]
    fn the_parent_entry_points_at_a_table_and_is_not_a_page() {
        let huge = 0x4020_0000 | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE;
        let parent = table_entry_for_split(huge, p(0x9000));

        assert_eq!(parent & PDE_PAGE_SIZE, 0, "PS を立ててはならない");
        assert_eq!(parent & PTE_PRESENT, PTE_PRESENT);
        assert_eq!(parent & PTE_WRITABLE, PTE_WRITABLE);
        assert_eq!(table_address(parent), p(0x9000));
    }

    /// USER は引き継ぐ。CPU は階層ごとの U/S を AND で合成するため、
    /// 親が引き継がないと子で許可しても効かない。
    #[test]
    fn the_parent_entry_inherits_the_user_bit() {
        let kernel_only = 0x4020_0000 | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE;
        assert_eq!(table_entry_for_split(kernel_only, p(0x9000)) & PTE_USER, 0);

        let user = kernel_only | PTE_USER;
        assert_eq!(table_entry_for_split(user, p(0x9000)) & PTE_USER, PTE_USER);
    }

    /// PCD / PWT は引き継がない。中間エントリのそれは PT フレーム自身の
    /// キャッシュ属性で、ページの属性ではない。ページ側は各 PTE が持つ。
    #[test]
    fn the_parent_entry_does_not_inherit_the_cache_attributes() {
        let huge = 0x8000_0000 | PTE_PRESENT | PTE_WRITABLE | PTE_PCD | PTE_PWT | PDE_PAGE_SIZE;
        let parent = table_entry_for_split(huge, p(0x9000));

        assert_eq!(
            parent & PTE_PCD,
            0,
            "親に PCD を立てるとウォークまで無効になる"
        );
        assert_eq!(parent & PTE_PWT, 0);

        // ページ側では保存されていること（役割の分担を 1 つのテストで固定する）。
        let child = split_child_entry(huge, 0);
        assert_eq!(child & PTE_PCD, PTE_PCD);
        assert_eq!(child & PTE_PWT, PTE_PWT);
    }

    /// 512 エントリが元の 2MiB と同じ物理範囲を、隙間なく覆うこと。
    #[test]
    fn the_children_cover_the_same_physical_range_without_gaps() {
        let base = 0x0000_0000_4020_0000;
        let huge = base | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE;
        let children = split_children(huge);

        assert_eq!(children.len(), ENTRIES_PER_TABLE);
        for (index, child) in children.iter().enumerate() {
            assert_eq!(
                page_address_4k(*child),
                p(base + index as u64 * PAGE_SIZE_4K),
                "index={index}"
            );
            assert!(is_present(*child), "index={index}");
        }
        // 末尾が元の範囲の最後の 4KiB であること（覆いすぎていない）。
        let last = page_address_4k(children[ENTRIES_PER_TABLE - 1]);
        assert_eq!(last.checked_add(PAGE_SIZE_4K), Some(p(base + PAGE_SIZE_2M)));
    }

    /// **PAT の移送は実機で確かめられないので、ここで厳密に固定する。**
    ///
    /// 現在 PAT を使っていないため、実機の 2MiB エントリにビット 12 が
    /// 立つことは無い。Write-Combining を導入した時点で初めて効き始める
    /// （`deferred-decisions.md`）。M5-a-1 で一度アサーションを誤った箇所
    /// でもあるので、境界を全エントリについて見る。
    #[test]
    fn every_child_moves_the_pat_bit_and_keeps_the_address_intact() {
        let base = 0x0000_0000_4020_0000;
        let huge = base | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE | PDE_HUGE_PAT;
        let children = split_children(huge);

        for (index, child) in children.iter().enumerate() {
            // ビット 7 は 4KiB 側の PAT。元が立っていたので全エントリで立つ。
            assert_eq!(child & PTE_PAT, PTE_PAT, "index={index}: ビット 7");

            // ビット 12 は 4KiB 側ではアドレスの一部。立つかどうかは添字だけで
            // 決まり、元の PAT とは無関係になる。
            let expected_bit_twelve = if index % 2 == 1 { PDE_HUGE_PAT } else { 0 };
            assert_eq!(
                child & PDE_HUGE_PAT,
                expected_bit_twelve,
                "index={index}: ビット 12 はアドレスの最下位ビット"
            );

            // アドレスが PAT に汚染されていないこと。
            assert_eq!(
                page_address_4k(*child),
                p(base + index as u64 * PAGE_SIZE_4K),
                "index={index}: アドレス"
            );
        }

        // PAT を持たない元エントリでは、全エントリでビット 7 が落ちていること。
        let without_pat = base | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE;
        for (index, child) in split_children(without_pat).iter().enumerate() {
            assert_eq!(child & PTE_PAT, 0, "index={index}");
        }
    }

    /// 非正規アドレスは、そもそも `VirtAddr` として構築できない。
    ///
    /// **T-2b で、この保護は実行時の検査から型へ移った。** 以前は
    /// 「非正規でも添字計算は それらしい 値を返してしまうので入口で弾く
    /// 必要がある」ことを固定していたが、今は添字を取る関数へ渡すこと自体が
    /// できない。
    #[test]
    fn a_non_canonical_address_cannot_be_built() {
        let bogus = 0x0000_8000_0000_0000;
        assert!(!is_canonical(bogus));
        assert!(VirtAddr::new(bogus).is_none());
    }
}
