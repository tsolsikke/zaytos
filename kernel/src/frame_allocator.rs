//! 物理フレームアロケータ（M2-c）。
//!
//! データ構造として「空き物理フレームの範囲（start_frame, frame_count）を
//! 固定長配列で保持するソート済みリスト」を採用している。
//!
//! 選択理由:
//! - **ビットマップ**（1フレーム=1ビット）は、フレーム数分のビット列を
//!   格納する領域が別途必要になる。現段階ではヒープがなく（M2-e 未実装）、
//!   そのビットマップ自身をどこに置くかという別の鶏卵問題が生じる。
//! - **侵入型フリーリスト**（各空きフレームの先頭に次フレームへの
//!   ポインタを書き込む）は追加のメモリを必要としない点は良いが、
//!   実際にフレームへポインタを書き込む操作が常に unsafe になり、
//!   ロジックの大部分をホスト上でテストできなくなる。
//! - **範囲リスト**は、UEFI メモリマップが最初から「範囲」の集合として
//!   与えられることと相性が良く、範囲の個数は実測で高々 100〜200 程度
//!   であるため固定長配列に収まる。範囲の追加・結合・分割は生ポインタを
//!   一切使わない純粋なデータ操作であり、ハードウェア依存からの分離と
//!   ホスト `cargo test` での検証をそのまま満たせる。
//!
//! ADR 化するかどうかは、この説明を見た人間の判断に委ねる。

use crate::memory_map::memory_type;

pub const FRAME_SIZE: u64 = 4096;

/// 実運用で使う既定の容量。実測（QEMU + OVMF, 256MiB 割り当て）では
/// `EfiConventionalMemory` のエントリ数は数十程度であり、余裕を見て
/// この値にしている。
pub const DEFAULT_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameRange {
    start_frame: u64,
    frame_count: u64,
}

impl FrameRange {
    const fn end_frame(&self) -> u64 {
        self.start_frame + self.frame_count
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAllocatorError {
    /// 空き範囲の数が固定長配列の容量を超えた。
    CapacityExceeded,
}

/// 除外した物理ページの、メモリ型別の内訳。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExclusionBreakdown {
    pub reserved_pages: u64,
    pub loader_code_pages: u64,
    pub loader_data_pages: u64,
    pub boot_services_code_pages: u64,
    pub boot_services_data_pages: u64,
    pub runtime_services_code_pages: u64,
    pub runtime_services_data_pages: u64,
    pub unusable_pages: u64,
    pub acpi_reclaim_pages: u64,
    pub acpi_nvs_pages: u64,
    pub mmio_pages: u64,
    pub mmio_port_space_pages: u64,
    pub pal_code_pages: u64,
    pub persistent_pages: u64,
    pub unaccepted_pages: u64,
    pub vendor_reserved_pages: u64,
    /// 型は `EfiConventionalMemory` だったが、物理アドレス 0 を含むため
    /// 個別に除外したページ数（ADR-0010 とは別件、ヌルポインタ対策）。
    pub null_page_excluded_pages: u64,
    /// 既知のいずれの型にも一致しなかったページ数（通常は発生しない）。
    pub unknown_type_pages: u64,
}

impl ExclusionBreakdown {
    pub const fn total_pages(&self) -> u64 {
        self.reserved_pages
            + self.loader_code_pages
            + self.loader_data_pages
            + self.boot_services_code_pages
            + self.boot_services_data_pages
            + self.runtime_services_code_pages
            + self.runtime_services_data_pages
            + self.unusable_pages
            + self.acpi_reclaim_pages
            + self.acpi_nvs_pages
            + self.mmio_pages
            + self.mmio_port_space_pages
            + self.pal_code_pages
            + self.persistent_pages
            + self.unaccepted_pages
            + self.vendor_reserved_pages
            + self.null_page_excluded_pages
            + self.unknown_type_pages
    }

    fn add(&mut self, ty: u32, pages: u64) {
        if pages == 0 {
            return;
        }
        match ty {
            memory_type::RESERVED => self.reserved_pages += pages,
            memory_type::LOADER_CODE => self.loader_code_pages += pages,
            memory_type::LOADER_DATA => self.loader_data_pages += pages,
            memory_type::BOOT_SERVICES_CODE => self.boot_services_code_pages += pages,
            memory_type::BOOT_SERVICES_DATA => self.boot_services_data_pages += pages,
            memory_type::RUNTIME_SERVICES_CODE => self.runtime_services_code_pages += pages,
            memory_type::RUNTIME_SERVICES_DATA => self.runtime_services_data_pages += pages,
            memory_type::UNUSABLE => self.unusable_pages += pages,
            memory_type::ACPI_RECLAIM => self.acpi_reclaim_pages += pages,
            memory_type::ACPI_NVS => self.acpi_nvs_pages += pages,
            memory_type::MMIO => self.mmio_pages += pages,
            memory_type::MMIO_PORT_SPACE => self.mmio_port_space_pages += pages,
            memory_type::PAL_CODE => self.pal_code_pages += pages,
            memory_type::PERSISTENT => self.persistent_pages += pages,
            memory_type::UNACCEPTED => self.unaccepted_pages += pages,
            t if t >= memory_type::VENDOR_RESERVED_START => self.vendor_reserved_pages += pages,
            _ => self.unknown_type_pages += pages,
        }
    }
}

/// 初期化時にシリアルへ出力すべき統計情報。
#[derive(Debug, Clone, Copy)]
pub struct FrameAllocatorStats {
    pub free_frame_count: u64,
    pub exclusions: ExclusionBreakdown,
}

impl FrameAllocatorStats {
    pub const fn free_mib(&self) -> u64 {
        (self.free_frame_count * FRAME_SIZE) / (1024 * 1024)
    }

    pub const fn excluded_mib(&self) -> u64 {
        (self.exclusions.total_pages() * FRAME_SIZE) / (1024 * 1024)
    }
}

/// 空き物理フレームを、ソート済み・隣接結合済みの範囲リストとして管理する。
///
/// `CAP` は保持できる範囲（連続していない空き領域の断片）の最大数。
/// 通常の利用は [`DEFAULT_CAPACITY`] を使う。テストでは容量超過の挙動を
/// 検証するために小さい値を明示的に指定する。
pub struct FrameAllocator<const CAP: usize = DEFAULT_CAPACITY> {
    ranges: [FrameRange; CAP],
    range_count: usize,
}

impl<const CAP: usize> FrameAllocator<CAP> {
    pub const fn new() -> Self {
        Self {
            ranges: [FrameRange {
                start_frame: 0,
                frame_count: 0,
            }; CAP],
            range_count: 0,
        }
    }

    /// 現在の空きフレーム総数。個別のカウンタを持たず、範囲リストから
    /// その都度計算する（範囲の個数は高々 CAP 程度で軽量、かつ
    /// カウンタと実体がズレる不整合の可能性を構造的に排除できる）。
    pub fn free_frame_count(&self) -> u64 {
        self.ranges[..self.range_count]
            .iter()
            .map(|r| r.frame_count)
            .sum()
    }

    pub fn free_range_count(&self) -> usize {
        self.range_count
    }

    /// 空きフレーム範囲を1つ追加する。前後の既存範囲と隣接・重複していれば
    /// 結合する。呼び出し側は、追加する範囲が他の空き範囲と重複しないこと
    /// （ある物理フレームを二重に空き扱いしないこと）を保証すること。
    pub fn insert_free_range(
        &mut self,
        start_frame: u64,
        frame_count: u64,
    ) -> Result<(), FrameAllocatorError> {
        if frame_count == 0 {
            return Ok(());
        }
        let new_end = start_frame + frame_count;

        let mut insert_at = self.range_count;
        for i in 0..self.range_count {
            if self.ranges[i].start_frame > start_frame {
                insert_at = i;
                break;
            }
        }

        // 直前の範囲と隣接/重複していれば結合し、そこからさらに直後とも
        // 結合できるか確認する（例: 隙間をちょうど埋める場合）。
        if insert_at > 0 && self.ranges[insert_at - 1].end_frame() >= start_frame {
            let prev = &mut self.ranges[insert_at - 1];
            let merged_end = prev.end_frame().max(new_end);
            prev.frame_count = merged_end - prev.start_frame;
            self.try_merge_forward(insert_at - 1);
            return Ok(());
        }

        // 直後の範囲と隣接/重複していれば結合。
        if insert_at < self.range_count && new_end >= self.ranges[insert_at].start_frame {
            let next = &mut self.ranges[insert_at];
            let merged_start = start_frame.min(next.start_frame);
            let merged_end = new_end.max(next.end_frame());
            next.start_frame = merged_start;
            next.frame_count = merged_end - merged_start;
            return Ok(());
        }

        // どちらとも結合できなければ新規範囲として挿入する。
        if self.range_count >= CAP {
            return Err(FrameAllocatorError::CapacityExceeded);
        }
        for i in (insert_at..self.range_count).rev() {
            self.ranges[i + 1] = self.ranges[i];
        }
        self.ranges[insert_at] = FrameRange {
            start_frame,
            frame_count,
        };
        self.range_count += 1;
        Ok(())
    }

    /// `ranges[idx]` が結合によって拡張された結果、直後の範囲とも隣接/
    /// 重複するようになっていないかを確認し、必要なら結合する。
    fn try_merge_forward(&mut self, idx: usize) {
        if idx + 1 < self.range_count
            && self.ranges[idx].end_frame() >= self.ranges[idx + 1].start_frame
        {
            let merged_end = self.ranges[idx]
                .end_frame()
                .max(self.ranges[idx + 1].end_frame());
            self.ranges[idx].frame_count = merged_end - self.ranges[idx].start_frame;
            for i in (idx + 1)..(self.range_count - 1) {
                self.ranges[i] = self.ranges[i + 1];
            }
            self.range_count -= 1;
        }
    }

    /// 空きフレームを1つ確保する。確保順は先頭（最小のフレーム番号）から。
    pub fn allocate_frame(&mut self) -> Option<u64> {
        if self.range_count == 0 {
            return None;
        }
        let frame = self.ranges[0].start_frame;
        self.ranges[0].start_frame += 1;
        self.ranges[0].frame_count -= 1;
        if self.ranges[0].frame_count == 0 {
            for i in 0..(self.range_count - 1) {
                self.ranges[i] = self.ranges[i + 1];
            }
            self.range_count -= 1;
        }
        Some(frame)
    }

    /// フレームを解放し、空きリストへ戻す（隣接する空き範囲とは結合される）。
    pub fn deallocate_frame(&mut self, frame: u64) -> Result<(), FrameAllocatorError> {
        self.insert_free_range(frame, 1)
    }
}

impl<const CAP: usize> Default for FrameAllocator<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

/// `BootInfo.memory_map` が指す生のメモリマップから、物理フレーム
/// アロケータを構築する。
///
/// 「空き」として扱うのは `EfiConventionalMemory` のみ（ADR-0010: 承認済み
/// のとおり `EfiBootServicesCode`/`Data` はまだページテーブル・スタック・
/// GDT/IDT が UEFI 由来のため除外する）。さらに物理アドレス 0 を含む
/// ページは、型に関わらず除外する（ヌルポインタ参照がバグ検出不能に
/// なることを防ぐ）。
///
/// kernel 本体・`BootInfo`・メモリマップバッファはいずれも
/// `EfiLoaderData` として確保されている（`bootloader/src/loader.rs` で
/// 確認済み）ため、型ベースのこの判定だけで自動的に除外される。
/// 個別のアドレス範囲を特別扱いする必要はない。
pub fn build(
    raw: &[u8],
    descriptor_size: u64,
) -> Result<(FrameAllocator<DEFAULT_CAPACITY>, FrameAllocatorStats), &'static str> {
    let mut allocator = FrameAllocator::<DEFAULT_CAPACITY>::new();
    let mut exclusions = ExclusionBreakdown::default();

    for entry in crate::memory_map::parse_entries(raw, descriptor_size)? {
        if entry.page_count == 0 {
            continue;
        }
        if entry.memory_type != memory_type::CONVENTIONAL {
            exclusions.add(entry.memory_type, entry.page_count);
            continue;
        }

        let mut start_frame = entry.phys_start / FRAME_SIZE;
        let mut frame_count = entry.page_count;

        if start_frame == 0 {
            exclusions.null_page_excluded_pages += 1;
            start_frame = 1;
            frame_count -= 1;
        }
        if frame_count == 0 {
            continue;
        }

        allocator.insert_free_range(start_frame, frame_count).map_err(|_| {
            "frame allocator free-range capacity exceeded while building from the UEFI memory map"
        })?;
    }

    let stats = FrameAllocatorStats {
        free_frame_count: allocator.free_frame_count(),
        exclusions,
    };
    Ok((allocator, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_map::test_support::build_map_bytes;

    // --- FrameAllocator 単体のテスト（範囲リストの挙動） ---

    #[test]
    fn new_allocator_is_empty() {
        let allocator = FrameAllocator::<8>::new();
        assert_eq!(allocator.free_frame_count(), 0);
        assert_eq!(allocator.free_range_count(), 0);
    }

    #[test]
    fn insert_then_allocate_returns_frames_from_the_range() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(10, 3).unwrap();
        assert_eq!(allocator.free_frame_count(), 3);

        assert_eq!(allocator.allocate_frame(), Some(10));
        assert_eq!(allocator.allocate_frame(), Some(11));
        assert_eq!(allocator.free_frame_count(), 1);
        assert_eq!(allocator.allocate_frame(), Some(12));
        assert_eq!(allocator.free_frame_count(), 0);
        assert_eq!(allocator.allocate_frame(), None);
    }

    #[test]
    fn deallocate_returns_a_frame_to_the_free_set() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(10, 1).unwrap();
        let frame = allocator.allocate_frame().unwrap();
        assert_eq!(allocator.free_frame_count(), 0);

        allocator.deallocate_frame(frame).unwrap();
        assert_eq!(allocator.free_frame_count(), 1);
        assert_eq!(allocator.allocate_frame(), Some(frame));
    }

    #[test]
    fn adjacent_free_regions_are_coalesced_into_one_range() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 5).unwrap(); // [0,5)
        allocator.insert_free_range(5, 5).unwrap(); // [5,10) touches the first
        assert_eq!(allocator.free_range_count(), 1);
        assert_eq!(allocator.free_frame_count(), 10);
    }

    #[test]
    fn insert_that_exactly_fills_a_gap_merges_both_neighbors() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 5).unwrap(); // [0,5)
        allocator.insert_free_range(8, 5).unwrap(); // [8,13)
        assert_eq!(allocator.free_range_count(), 2);

        allocator.insert_free_range(5, 3).unwrap(); // [5,8) fills the gap exactly
        assert_eq!(allocator.free_range_count(), 1);
        assert_eq!(allocator.free_frame_count(), 13);
    }

    #[test]
    fn deallocate_coalesces_adjacent_singleton_frames() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 10).unwrap();
        let a = allocator.allocate_frame().unwrap();
        let b = allocator.allocate_frame().unwrap();
        assert_eq!(allocator.free_range_count(), 1); // [2,10)

        // a, b (連続した2フレーム) を解放すると、既存の空き範囲と結合される。
        allocator.deallocate_frame(a).unwrap();
        allocator.deallocate_frame(b).unwrap();
        assert_eq!(allocator.free_range_count(), 1);
        assert_eq!(allocator.free_frame_count(), 10);
    }

    #[test]
    fn non_adjacent_regions_stay_as_separate_ranges() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 2).unwrap();
        allocator.insert_free_range(10, 2).unwrap();
        assert_eq!(allocator.free_range_count(), 2);
        assert_eq!(allocator.free_frame_count(), 4);
    }

    #[test]
    fn capacity_exceeded_is_reported_as_an_error() {
        let mut allocator = FrameAllocator::<2>::new();
        allocator.insert_free_range(0, 1).unwrap();
        allocator.insert_free_range(10, 1).unwrap();
        // 3つ目の非隣接範囲は容量(2)を超える。
        let result = allocator.insert_free_range(20, 1);
        assert_eq!(result, Err(FrameAllocatorError::CapacityExceeded));
        // 失敗時も既存の状態は破壊されていない。
        assert_eq!(allocator.free_frame_count(), 2);
    }

    #[test]
    fn boundary_near_top_of_address_space_does_not_overflow() {
        let mut allocator = FrameAllocator::<4>::new();
        // 2^64 の物理アドレス空間の末尾付近（オーバーフローが起きやすい境界）。
        let near_top_frame = (u64::MAX / FRAME_SIZE) - 2;
        allocator.insert_free_range(near_top_frame, 2).unwrap();
        assert_eq!(allocator.free_frame_count(), 2);
        assert_eq!(allocator.allocate_frame(), Some(near_top_frame));
        assert_eq!(allocator.allocate_frame(), Some(near_top_frame + 1));
    }

    // --- build() のテスト（メモリ型ポリシーの適用） ---

    #[test]
    fn build_treats_only_conventional_memory_as_free() {
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x100000, 4),
                (memory_type::BOOT_SERVICES_DATA, 0x200000, 4),
                (memory_type::LOADER_DATA, 0x300000, 4),
                (memory_type::RESERVED, 0x400000, 4),
            ],
            48,
        );
        let (allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(allocator.free_frame_count(), 4);
        assert_eq!(stats.free_frame_count, 4);
    }

    #[test]
    fn build_excludes_loader_data_adjacent_to_conventional() {
        // kernel 本体・BootInfo・メモリマップバッファは LOADER_DATA として
        // 確保されている（bootloader/src/loader.rs で確認済み）。これが
        // CONVENTIONAL な空き領域のすぐ隣にあっても、空きとして誤って
        // 取り込まれない（型で自動的に守られる）ことを確認する。
        let bytes = build_map_bytes(
            &[
                (memory_type::LOADER_DATA, 0x100000, 6), // kernel/BootInfo/mmap buffer 相当
                (memory_type::CONVENTIONAL, 0x106000, 4), // すぐ隣の空き領域
            ],
            48,
        );
        let (allocator, stats) = build(&bytes, 48).unwrap();

        assert_eq!(allocator.free_frame_count(), 4);
        assert_eq!(stats.exclusions.loader_data_pages, 6);
        // LOADER_DATA の範囲にあるフレーム番号は一切割り当てられない。
        let loader_data_start_frame = 0x100000 / FRAME_SIZE;
        let loader_data_end_frame = 0x106000 / FRAME_SIZE;
        let mut allocator = allocator;
        while let Some(frame) = allocator.allocate_frame() {
            assert!(!(loader_data_start_frame..loader_data_end_frame).contains(&frame));
        }
    }

    #[test]
    fn build_excludes_boot_services_data_per_adr_0010() {
        let bytes = build_map_bytes(
            &[
                (memory_type::BOOT_SERVICES_DATA, 0x100000, 8),
                (memory_type::CONVENTIONAL, 0x108000, 2),
            ],
            48,
        );
        let (_allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(stats.exclusions.boot_services_data_pages, 8);
        assert_eq!(stats.free_frame_count, 2);
    }

    #[test]
    fn build_excludes_the_page_containing_physical_address_zero() {
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0, 4)], 48);
        let (mut allocator, stats) = build(&bytes, 48).unwrap();

        assert_eq!(stats.exclusions.null_page_excluded_pages, 1);
        assert_eq!(allocator.free_frame_count(), 3);
        // フレーム 0 (物理アドレス 0) は絶対に配られない。
        while let Some(frame) = allocator.allocate_frame() {
            assert_ne!(frame, 0);
        }
    }

    #[test]
    fn build_excludes_entire_entry_when_it_is_only_the_null_page() {
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0, 1)], 48);
        let (allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(stats.exclusions.null_page_excluded_pages, 1);
        assert_eq!(allocator.free_frame_count(), 0);
    }

    #[test]
    fn build_ignores_zero_page_count_descriptors() {
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x100000, 0),
                (memory_type::CONVENTIONAL, 0x200000, 4),
            ],
            48,
        );
        let (_allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(stats.free_frame_count, 4);
    }

    #[test]
    fn build_coalesces_adjacent_conventional_entries_from_the_map() {
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x100000, 4), // frames 256..260
                (memory_type::CONVENTIONAL, 0x104000, 4), // frames 260..264 (隣接)
            ],
            48,
        );
        let (allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(stats.free_frame_count, 8);
        assert_eq!(allocator.free_range_count(), 1);
    }

    #[test]
    fn build_works_with_descriptor_size_other_than_48() {
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0x100000, 4)], 64);
        let (_allocator, stats) = build(&bytes, 64).unwrap();
        assert_eq!(stats.free_frame_count, 4);
    }

    #[test]
    fn build_reports_exclusion_breakdown_by_type() {
        let bytes = build_map_bytes(
            &[
                (memory_type::RESERVED, 0x1000, 1),
                (memory_type::LOADER_CODE, 0x2000, 2),
                (memory_type::LOADER_DATA, 0x3000, 3),
                (memory_type::BOOT_SERVICES_CODE, 0x4000, 4),
                (memory_type::BOOT_SERVICES_DATA, 0x5000, 5),
                (memory_type::RUNTIME_SERVICES_CODE, 0x6000, 6),
                (memory_type::RUNTIME_SERVICES_DATA, 0x7000, 7),
                (memory_type::UNUSABLE, 0x8000, 8),
                (memory_type::ACPI_RECLAIM, 0x9000, 9),
                (memory_type::ACPI_NVS, 0xA000, 10),
                (memory_type::MMIO, 0xB000, 11),
                (memory_type::MMIO_PORT_SPACE, 0xC000, 12),
                (memory_type::PAL_CODE, 0xD000, 13),
                (memory_type::PERSISTENT, 0xE000, 14),
                (memory_type::UNACCEPTED, 0xF000, 15),
                (0x8000_0000, 0x1000_0000, 16), // OS ベンダー予約 (型の値)
            ],
            48,
        );
        let (_allocator, stats) = build(&bytes, 48).unwrap();
        let e = &stats.exclusions;
        assert_eq!(e.reserved_pages, 1);
        assert_eq!(e.loader_code_pages, 2);
        assert_eq!(e.loader_data_pages, 3);
        assert_eq!(e.boot_services_code_pages, 4);
        assert_eq!(e.boot_services_data_pages, 5);
        assert_eq!(e.runtime_services_code_pages, 6);
        assert_eq!(e.runtime_services_data_pages, 7);
        assert_eq!(e.unusable_pages, 8);
        assert_eq!(e.acpi_reclaim_pages, 9);
        assert_eq!(e.acpi_nvs_pages, 10);
        assert_eq!(e.mmio_pages, 11);
        assert_eq!(e.mmio_port_space_pages, 12);
        assert_eq!(e.pal_code_pages, 13);
        assert_eq!(e.persistent_pages, 14);
        assert_eq!(e.unaccepted_pages, 15);
        assert_eq!(e.vendor_reserved_pages, 16);
        assert_eq!(stats.free_frame_count, 0);
    }

    #[test]
    fn free_mib_and_excluded_mib_convert_frames_to_mib() {
        let stats = FrameAllocatorStats {
            free_frame_count: 256, // 256 * 4096 = 1 MiB
            exclusions: ExclusionBreakdown {
                reserved_pages: 512, // 2 MiB
                ..Default::default()
            },
        };
        assert_eq!(stats.free_mib(), 1);
        assert_eq!(stats.excluded_mib(), 2);
    }
}
