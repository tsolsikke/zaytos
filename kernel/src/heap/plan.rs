//! ヒープの空きブロックに対する配置・分割・結合の判断ロジック（M2-e）。
//!
//! 実ポインタを一切使わない純粋ロジックであり、ホスト上の `cargo test` で
//! 検証する。実際に侵入型連結リストへ読み書きする部分は
//! ハードウェア依存側（`super::allocator`）の責務とし、ここには含めない。

/// 1回の確保の配置計画。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocPlan {
    /// [`super::allocator::AllocatedBlockHeader`] を書き込む位置。
    pub header_addr: u64,
    /// 呼び出し元へ返す、実際に使えるアドレス（`layout.align()` を満たす）。
    pub user_addr: u64,
    /// 消費する空きブロックの元の開始アドレス（アラインメント調整で生じる
    /// 前方の隙間も含む）。ヘッダにこの値を記録しておくことで、解放時に
    /// 前方の隙間を含めた領域全体をまるごと空きリストへ戻せる（隙間を
    /// 別ブロックとして分離する必要がない）。
    pub block_start: u64,
    /// 消費する領域全体の大きさ（前方の隙間・ヘッダ・ユーザー領域・
    /// 末尾の端数のうち分離しなかった分を含む）。
    pub block_size: u64,
    /// 末尾に十分な余りがあり、新しい空きブロックとして分離した場合、
    /// その `(start, size)`。
    pub trailing_split: Option<(u64, u64)>,
}

/// `align` に切り上げる。`align` は2のべき乗であること。
const fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

/// 空き領域 `[region_start, region_end)` に、`header_size`/`header_align` の
/// ヘッダと `requested_size`/`requested_align` のユーザー領域を配置できるか
/// 計算する。配置できなければ `None`。
///
/// アラインメント処理: ヘッダの直後にユーザー領域が来るよう、
/// `region_start + header_size` を `max(requested_align, header_align)` に
/// 切り上げた位置を `user_addr` とする。2のべき乗どうしの大きい方に
/// 揃えれば、小さい方の制約も自動的に満たされる。
///
/// `region_start` と `header_addr` の間に生じる隙間は、別ブロックとして
/// 分離せず `block_start = region_start` としてヘッダへ記録する（解放時に
/// まるごと回収するため）。末尾の余りは `min_block_size` 以上あれば
/// 新しい空きブロックとして分離し、それ未満なら確保領域に含めて許容する
/// 内部断片化とする。
pub fn plan_allocation(
    region_start: u64,
    region_end: u64,
    header_size: u64,
    header_align: u64,
    requested_size: u64,
    requested_align: u64,
    min_block_size: u64,
) -> Option<AllocPlan> {
    let effective_align = requested_align.max(header_align);

    let user_addr = align_up(region_start.checked_add(header_size)?, effective_align);
    let header_addr = user_addr.checked_sub(header_size)?;
    debug_assert!(header_addr >= region_start);

    let alloc_end_unaligned = user_addr.checked_add(requested_size)?;
    let alloc_end = align_up(alloc_end_unaligned, header_align);
    if alloc_end > region_end {
        return None;
    }

    let leftover = region_end - alloc_end;
    if leftover >= min_block_size {
        Some(AllocPlan {
            header_addr,
            user_addr,
            block_start: region_start,
            block_size: alloc_end - region_start,
            trailing_split: Some((alloc_end, leftover)),
        })
    } else {
        Some(AllocPlan {
            header_addr,
            user_addr,
            block_start: region_start,
            block_size: region_end - region_start,
            trailing_split: None,
        })
    }
}

/// 2つの空きブロックが直接隣接していれば、結合後の `(start, size)` を返す。
pub fn merge_adjacent(a_start: u64, a_size: u64, b_start: u64, b_size: u64) -> Option<(u64, u64)> {
    if a_start + a_size == b_start {
        Some((a_start, a_size + b_size))
    } else if b_start + b_size == a_start {
        Some((b_start, a_size + b_size))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER_SIZE: u64 = 32;
    const HEADER_ALIGN: u64 = 8;
    const MIN_BLOCK_SIZE: u64 = 16;

    #[test]
    fn plan_allocation_aligns_user_addr_for_various_alignments() {
        for align in [1u64, 8, 16, 64, 4096] {
            let plan = plan_allocation(
                0x1000,
                0x1000 + 0x10000,
                HEADER_SIZE,
                HEADER_ALIGN,
                100,
                align,
                MIN_BLOCK_SIZE,
            )
            .unwrap_or_else(|| panic!("expected a plan for align={align}"));

            assert_eq!(
                plan.user_addr % align,
                0,
                "user_addr not aligned to {align}"
            );
            assert_eq!(plan.header_addr, plan.user_addr - HEADER_SIZE);
            assert_eq!(plan.block_start, 0x1000);
        }
    }

    #[test]
    fn plan_allocation_splits_off_trailing_free_block_when_large_enough() {
        // region が十分大きければ、末尾の余りは新しい空きブロックとして
        // 分離される。
        let plan = plan_allocation(
            0x1000,
            0x1000 + 4096,
            HEADER_SIZE,
            HEADER_ALIGN,
            64,
            8,
            MIN_BLOCK_SIZE,
        )
        .unwrap();
        let (split_start, split_size) = plan.trailing_split.expect("expected a trailing split");
        assert_eq!(split_start, plan.user_addr + 64);
        assert_eq!(split_start + split_size, 0x1000 + 4096);
        assert!(split_size >= MIN_BLOCK_SIZE);
    }

    #[test]
    fn plan_allocation_folds_in_trailing_leftover_smaller_than_min_block_size() {
        // header(32) + user(64) = 96 バイトちょうど、余りは0。
        let region_start = 0x2000u64;
        let region_end = region_start + HEADER_SIZE + 64;
        let plan = plan_allocation(
            region_start,
            region_end,
            HEADER_SIZE,
            HEADER_ALIGN,
            64,
            8,
            MIN_BLOCK_SIZE,
        )
        .unwrap();
        assert_eq!(plan.trailing_split, None);
        assert_eq!(plan.block_size, region_end - region_start);
    }

    #[test]
    fn plan_allocation_leftover_exactly_min_block_size_is_split() {
        let region_start = 0x3000u64;
        let region_end = region_start + HEADER_SIZE + 64 + MIN_BLOCK_SIZE;
        let plan = plan_allocation(
            region_start,
            region_end,
            HEADER_SIZE,
            HEADER_ALIGN,
            64,
            8,
            MIN_BLOCK_SIZE,
        )
        .unwrap();
        let (_, split_size) = plan
            .trailing_split
            .expect("exact min_block_size must split");
        assert_eq!(split_size, MIN_BLOCK_SIZE);
    }

    #[test]
    fn plan_allocation_zero_size_request_still_produces_a_valid_plan() {
        let plan = plan_allocation(
            0x1000,
            0x1000 + 4096,
            HEADER_SIZE,
            HEADER_ALIGN,
            0,
            8,
            MIN_BLOCK_SIZE,
        )
        .unwrap();
        assert_eq!(plan.user_addr - plan.header_addr, HEADER_SIZE);
        assert!(plan.block_size >= HEADER_SIZE);
    }

    #[test]
    fn plan_allocation_fails_when_request_larger_than_region() {
        let plan = plan_allocation(
            0x1000,
            0x1000 + 64,
            HEADER_SIZE,
            HEADER_ALIGN,
            1024,
            8,
            MIN_BLOCK_SIZE,
        );
        assert_eq!(plan, None);
    }

    #[test]
    fn plan_allocation_fails_when_region_too_small_for_header_and_alignment() {
        let plan = plan_allocation(
            0x1000,
            0x1000 + 4,
            HEADER_SIZE,
            HEADER_ALIGN,
            1,
            8,
            MIN_BLOCK_SIZE,
        );
        assert_eq!(plan, None);
    }

    #[test]
    fn plan_allocation_large_alignment_may_require_a_large_region() {
        // align=4096 かつ region_start が既に4096アラインでない場合、
        // user_addr は次の4096境界まで切り上がる。
        let plan = plan_allocation(
            0x1001,
            0x1001 + 8192,
            HEADER_SIZE,
            HEADER_ALIGN,
            64,
            4096,
            MIN_BLOCK_SIZE,
        )
        .unwrap();
        assert_eq!(plan.user_addr % 4096, 0);
        assert!(plan.user_addr > 0x1001);
    }

    #[test]
    fn merge_adjacent_detects_forward_adjacency() {
        assert_eq!(
            merge_adjacent(0x1000, 0x100, 0x1100, 0x50),
            Some((0x1000, 0x150))
        );
    }

    #[test]
    fn merge_adjacent_detects_backward_adjacency() {
        assert_eq!(
            merge_adjacent(0x1100, 0x50, 0x1000, 0x100),
            Some((0x1000, 0x150))
        );
    }

    #[test]
    fn merge_adjacent_returns_none_when_gap_present() {
        assert_eq!(merge_adjacent(0x1000, 0x100, 0x1200, 0x50), None);
    }

    #[test]
    fn merge_adjacent_exact_boundary_no_off_by_one() {
        // a が b のちょうど1バイト手前で終わる場合は隣接、1バイト空くと非隣接。
        assert!(merge_adjacent(0x1000, 0xFF, 0x1100, 0x10).is_none());
        assert!(merge_adjacent(0x1000, 0x100, 0x1100, 0x10).is_some());
    }
}
