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

/// 4KiB ページのアドレス部分（ビット 12-51）。
pub const ADDR_MASK_4K: u64 = 0x000F_FFFF_FFFF_F000;

/// 2MiB ページのアドレス部分（ビット 21-51）。
///
/// **ビット 12-20 を含めてはならない。** ビット 12 は PAT、13-20 は予約である。
pub const ADDR_MASK_2M: u64 = 0x000F_FFFF_FFE0_0000;

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
pub const fn is_huge(entry: u64) -> bool {
    entry & PDE_PAGE_SIZE != 0
}

/// 中間テーブルの物理アドレス。
pub const fn table_address(entry: u64) -> u64 {
    entry & ADDR_MASK_TABLE
}

/// 4KiB ページの物理アドレス。
pub const fn page_address_4k(entry: u64) -> u64 {
    entry & ADDR_MASK_4K
}

/// 2MiB ページの物理アドレス。
pub const fn page_address_2m(entry: u64) -> u64 {
    entry & ADDR_MASK_2M
}

// 仮想アドレスから各階層の添字を取り出す。
pub const fn pml4_index(addr: u64) -> usize {
    ((addr >> 39) & 0x1FF) as usize
}
pub const fn pdpt_index(addr: u64) -> usize {
    ((addr >> 30) & 0x1FF) as usize
}
pub const fn pd_index(addr: u64) -> usize {
    ((addr >> 21) & 0x1FF) as usize
}
pub const fn pt_index(addr: u64) -> usize {
    ((addr >> 12) & 0x1FF) as usize
}

/// x86_64 の仮想アドレスが正規形（canonical）か。
///
/// **ビット 47 が 48-63 へ符号拡張されていなければならない。** 非正規の
/// アドレスは、そもそも CPU が拒否する（`mov` で #GP になる）。添字計算は
/// 非正規でも「それらしい」値を返してしまうので、入口で弾く。
///
/// 「マップされていない」と「アドレスが不正」は**別の状態**である。
pub const fn is_canonical(addr: u64) -> bool {
    let sign_extended = ((addr as i64) << 16 >> 16) as u64;
    sign_extended == addr
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
    let base = page_address_2m(huge_entry);
    let address = base + (index as u64) * PAGE_SIZE_4K;

    // PS・PAT(bit12)・Accessed・Dirty・アドレスを除いたフラグ。
    let mut flags = huge_entry
        & !(PDE_PAGE_SIZE | PDE_HUGE_PAT | PTE_ACCESSED | PTE_DIRTY | ADDR_MASK_2M);
    // 2MiB では予約だったビット 13-20 も落としておく（本来 0 のはずだが、
    // 万一立っていたら 4KiB ではアドレスの一部として解釈されてしまう）。
    flags &= !0x1F_F000;

    // **PAT をビット 12 からビット 7 へ移す。**
    if huge_entry & PDE_HUGE_PAT != 0 {
        flags |= PTE_PAT;
    }

    address | flags
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分割の基本。アドレスが 4KiB 刻みで並び、両端が正しいこと。
    #[test]
    fn split_produces_four_kib_pages_covering_the_same_range() {
        let base = 0x0000_0000_4020_0000; // 2MiB アライン
        let huge = base | PTE_PRESENT | PTE_WRITABLE | PDE_PAGE_SIZE;

        assert_eq!(page_address_4k(split_child_entry(huge, 0)), base);
        assert_eq!(
            page_address_4k(split_child_entry(huge, 1)),
            base + PAGE_SIZE_4K
        );
        assert_eq!(
            page_address_4k(split_child_entry(huge, ENTRIES_PER_TABLE - 1)),
            base + PAGE_SIZE_2M - PAGE_SIZE_4K
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
                0x4020_0000 + index as u64 * PAGE_SIZE_4K,
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
        let huge = 0x4020_0000
            | PTE_PRESENT
            | PTE_WRITABLE
            | PDE_PAGE_SIZE
            | PTE_ACCESSED
            | PTE_DIRTY;
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
        assert_eq!(page_address_2m(huge), 0x4020_0000, "正しいマスク");
        assert_eq!(
            page_address_4k(huge),
            0x4020_0000 | PDE_HUGE_PAT,
            "誤ったマスクだと PAT がアドレスに混ざる"
        );
    }

    /// 添字の取り出し。既知のアドレスで各階層を固定する。
    #[test]
    fn the_indices_decompose_a_known_address() {
        // PML4=1, PDPT=2, PD=3, PT=4 になるアドレスを組み立てる。
        let addr = (1u64 << 39) | (2u64 << 30) | (3u64 << 21) | (4u64 << 12);
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

    /// 非正規アドレスでも添字計算は「それらしい」値を返してしまう。
    ///
    /// だからこそ入口で弾く必要がある、ということを固定しておく。
    #[test]
    fn index_extraction_silently_succeeds_on_non_canonical_addresses() {
        let bogus = 0x0000_8000_0000_0000;
        assert!(!is_canonical(bogus));
        // エラーにならず、もっともらしい添字が出る。
        assert_eq!(pml4_index(bogus), 256);
    }
}
