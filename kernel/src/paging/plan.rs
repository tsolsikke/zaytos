//! ページテーブル構築計画（M2-d, d-1）— 純粋ロジック。
//!
//! 「どの物理アドレス範囲を、どのページサイズ（2MiB/4KiB）で、どの
//! キャッシュ属性でマップすべきか」を計算する部分。生ポインタや unsafe
//! を一切使わず、ホスト上の `cargo test` で検証する。
//! 実際にページテーブルへ書き込む部分は `super::table` に分離している。
//!
//! **マップ対象の判定は、必ず [`crate::memory_map::classify`] を経由する。**
//! これは [`crate::frame_allocator::build`] が「空き」を判定するのと
//! 同じ関数であり、判定基準が二重に実装されてズレることを構造的に防ぐ
//! （空きフレームなのにマップされていない、という致命的な不整合の防止）。

use common::addr::PhysAddr;

use crate::frame_allocator::FRAME_SIZE;
use crate::memory_map::{self, RegionPolicy};

pub const PAGE_SIZE_2M: u64 = 2 * 1024 * 1024;

/// 実運用で使う既定の容量。当初 64 で見積もっていたが、実機（QEMU +
/// OVMF, 256MiB 割り当て）で実際に構築したところ超過しエラーになった
/// （`docs/troubleshooting.md` 参照）。UEFI メモリマップは、同じ型でも
/// 連続しない小さな断片が多数現れることがあり、「型ごとに1エントリ」
/// という見積もりは誤りだった。マージ前の最悪ケース（メモリマップの
/// 総ディスクリプタ数）まで余裕を持たせ、フレームアロケータと同じ
/// 256 にしている。
pub const DEFAULT_CAPACITY: usize = 256;

/// 同一キャッシュ属性で連続する物理アドレス範囲。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributedRange {
    pub start: PhysAddr,
    /// 排他的な終端。
    pub end: PhysAddr,
    pub cacheable: bool,
}

/// マップすべき物理アドレス範囲の、ソート済み・隣接結合済みの集合。
pub struct MappedRanges<const CAP: usize = DEFAULT_CAPACITY> {
    ranges: [AttributedRange; CAP],
    count: usize,
}

impl<const CAP: usize> MappedRanges<CAP> {
    /// UEFI メモリマップの生バイト列から構築する。
    ///
    /// - `RegionPolicy::Unmapped` の範囲（`EfiReservedMemoryType` 等。
    ///   PCI 64bit MMIO 窓のような、実測で 1TB 規模になりうる予約領域を
    ///   含む）はマップ対象に含めない。
    /// - 物理アドレス 0 を含むページは、型に関わらずマップしない
    ///   （フレームアロケータ側のヌルページ除外と対応させ、実際に
    ///   null ポインタ参照がページフォルトになるようにする）。
    ///
    /// `extra` には、UEFI メモリマップに現れない可能性がある、しかし
    /// マップが必須の領域を `(start, end, cacheable)` として明示的に
    /// 渡す。**GOP フレームバッファは実機検証の結果、UEFI メモリマップに
    /// 含まれないことが判明した**（PCI BAR はシステムメモリマップとは
    /// 別に扱われるため）。フレームバッファのように「メモリマップから
    /// 自動的には出てこないが、必ずマップが必要な領域」はここで渡す。
    pub fn build(
        raw: &[u8],
        descriptor_size: u64,
        extra: &[(u64, u64, bool)],
    ) -> Result<Self, &'static str> {
        let mut ranges = [AttributedRange {
            start: PhysAddr::new_const(0),
            end: PhysAddr::new_const(0),
            cacheable: true,
        }; CAP];
        let mut count = 0usize;

        for entry in memory_map::parse_entries(raw, descriptor_size)? {
            if entry.page_count == 0 {
                continue;
            }
            let cacheable = match memory_map::classify(entry.memory_type) {
                RegionPolicy::Free => true,
                RegionPolicy::ReservedButMapped { cacheable } => cacheable,
                RegionPolicy::Unmapped => continue,
            };

            let mut start = entry.phys_start;
            let end = entry.phys_start + entry.page_count * FRAME_SIZE;
            if start == 0 {
                start = FRAME_SIZE;
            }
            if start >= end {
                continue;
            }
            let (Some(start), Some(end)) = (PhysAddr::new(start), PhysAddr::new(end)) else {
                return Err("a memory map entry does not fit in a physical address");
            };

            if count >= CAP {
                return Err("mapped-range capacity exceeded while building the paging plan");
            }
            ranges[count] = AttributedRange {
                start,
                end,
                cacheable,
            };
            count += 1;
        }

        for &(start, end, cacheable) in extra {
            if start >= end {
                continue;
            }
            let (Some(start), Some(end)) = (PhysAddr::new(start), PhysAddr::new(end)) else {
                return Err("an extra range does not fit in a physical address");
            };
            if count >= CAP {
                return Err("mapped-range capacity exceeded while adding extra ranges");
            }
            ranges[count] = AttributedRange {
                start,
                end,
                cacheable,
            };
            count += 1;
        }

        ranges[..count].sort_unstable_by_key(|r| r.start);

        // 隣接/重複し、かつキャッシュ属性が同じ範囲を結合する。
        let mut merged = 0usize;
        for i in 0..count {
            let r = ranges[i];
            if merged > 0 {
                let prev = ranges[merged - 1];
                if prev.cacheable == r.cacheable && r.start <= prev.end {
                    ranges[merged - 1].end = prev.end.max(r.end);
                    continue;
                }
            }
            ranges[merged] = r;
            merged += 1;
        }

        Ok(Self {
            ranges,
            count: merged,
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = &AttributedRange> {
        self.ranges[..self.count].iter()
    }

    pub fn range_count(&self) -> usize {
        self.count
    }

    /// 単一のアドレスがマップ対象範囲に含まれるか。
    pub fn contains(&self, addr: PhysAddr) -> bool {
        self.ranges[..self.count]
            .iter()
            .any(|r| r.start <= addr && addr < r.end)
    }

    /// `[start, end)` が、マップ対象範囲によって（複数範囲にまたがって
    /// いても良いので）隙間なく覆われているか。
    pub fn contains_range(&self, start: PhysAddr, end: PhysAddr) -> bool {
        if start >= end {
            return true;
        }
        let mut cursor = start;
        loop {
            let Some(r) = self.ranges[..self.count]
                .iter()
                .find(|r| r.start <= cursor && cursor < r.end)
            else {
                return false;
            };
            if r.end >= end {
                return true;
            }
            cursor = r.end;
        }
    }
}

/// 1 ページ分のマッピング指示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageMapping {
    pub phys_addr: PhysAddr,
    /// `true` なら 2MiB ページ、`false` なら 4KiB ページ。
    pub huge: bool,
    pub cacheable: bool,
}

/// [`MappedRanges`] を、実際にどのページサイズで割り付けるかまで解決し、
/// `visit` へ 1 ページずつ渡す。
///
/// 各範囲について、2MiB 境界に揃った「核」の部分は 2MiB ページで、
/// 境界に揃わない両端の「端数」は 4KiB ページで埋める。これにより、
///
/// - 元のディスクリプタが 2MiB 境界に揃っていない
/// - 隣接する範囲どうしでキャッシュ属性が異なり、その境界が 2MiB
///   境界に揃っていない
///
/// の両方のケースを、特別な「衝突検出」を書かずに統一的に処理できる
/// （揃っていなければ端数として 4KiB 化されるだけなので、そもそも
/// 2MiB ページの範囲に異なる属性が混在することがない）。
pub fn resolve_pages<const CAP: usize>(
    ranges: &MappedRanges<CAP>,
    mut visit: impl FnMut(PageMapping),
) {
    for r in ranges.iter() {
        // 端数の切り上げが物理アドレスの範囲を出たら、その範囲に 2MiB の
        // 核は無い。`PhysAddr::align_up` は範囲外を `None` にするので、
        // そのまま「核なし」として扱う。
        let core_start = r.start.align_up(PAGE_SIZE_2M);
        let core_end = r.end.align_down(PAGE_SIZE_2M);

        match (core_start, core_end) {
            (Some(core_start), Some(core_end)) if core_start < core_end => {
                emit_4k(r.start, core_start, r.cacheable, &mut visit);
                let mut addr = core_start;
                while addr < core_end {
                    visit(PageMapping {
                        phys_addr: addr,
                        huge: true,
                        cacheable: r.cacheable,
                    });
                    let Some(next) = addr.checked_add(PAGE_SIZE_2M) else {
                        break;
                    };
                    addr = next;
                }
                emit_4k(core_end, r.end, r.cacheable, &mut visit);
            }
            _ => emit_4k(r.start, r.end, r.cacheable, &mut visit),
        }
    }
}

fn emit_4k(start: PhysAddr, end: PhysAddr, cacheable: bool, visit: &mut impl FnMut(PageMapping)) {
    let mut addr = start;
    while addr < end {
        visit(PageMapping {
            phys_addr: addr,
            huge: false,
            cacheable,
        });
        let Some(next) = addr.checked_add(FRAME_SIZE) else {
            break;
        };
        addr = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト内で期待値を組み立てるための補助。
    fn p(raw: u64) -> PhysAddr {
        PhysAddr::new(raw).unwrap()
    }
    use crate::memory_map::{memory_type, test_support::build_map_bytes};

    #[test]
    fn unmapped_types_are_excluded() {
        let bytes = build_map_bytes(
            &[
                (memory_type::RESERVED, 0x1000_0000_0000, 1_000_000), // 巨大な予約領域
                (memory_type::CONVENTIONAL, 0x200000, 4),
            ],
            48,
        );
        let ranges = MappedRanges::<DEFAULT_CAPACITY>::build(&bytes, 48, &[]).unwrap();
        assert_eq!(ranges.range_count(), 1);
        assert!(ranges.contains(p(0x200000)));
        assert!(!ranges.contains(p(0x1000_0000_0000)));
    }

    #[test]
    fn null_page_is_excluded_even_when_conventional() {
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0, 4)], 48);
        let ranges = MappedRanges::<DEFAULT_CAPACITY>::build(&bytes, 48, &[]).unwrap();
        assert!(!ranges.contains(p(0)));
        assert!(ranges.contains(p(FRAME_SIZE)));
    }

    #[test]
    fn adjacent_same_attribute_entries_are_merged() {
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x100000, 4),
                (memory_type::CONVENTIONAL, 0x104000, 4),
            ],
            48,
        );
        let ranges = MappedRanges::<DEFAULT_CAPACITY>::build(&bytes, 48, &[]).unwrap();
        assert_eq!(ranges.range_count(), 1);
        assert!(ranges.contains_range(p(0x100000), p(0x108000)));
    }

    #[test]
    fn differing_attribute_boundary_is_not_merged() {
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x100000, 4), // cacheable
                (memory_type::MMIO, 0x104000, 4),         // uncacheable, 隣接
            ],
            48,
        );
        let ranges = MappedRanges::<DEFAULT_CAPACITY>::build(&bytes, 48, &[]).unwrap();
        assert_eq!(ranges.range_count(), 2);
    }

    #[test]
    fn capacity_exceeded_is_reported() {
        let many: Vec<(u32, u64, u64)> = (0..10)
            .map(|i| (memory_type::CONVENTIONAL, 0x0010_0000 + i * 0x0100_0000, 1))
            .collect();
        let bytes = build_map_bytes(&many, 48);
        // 隣接しない小さな範囲を10個作り、容量2に対して超過させる。
        let result = MappedRanges::<2>::build(&bytes, 48, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_pages_uses_2mib_core_and_4kib_fringe() {
        // [0x1000, 0x600000): 開始は 2MiB に揃っていないが、終了 (0x600000)
        // はちょうど 3 * 2MiB に揃っている。核は [0x200000, 0x600000)
        // (2ページ分)、前方端数は [0x1000, 0x200000)、後方端数はなし。
        let page_count = (0x600000 - 0x1000) / 4096; // 1535
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0x1000, page_count)], 48);
        let ranges = MappedRanges::<DEFAULT_CAPACITY>::build(&bytes, 48, &[]).unwrap();

        let mut huge_pages = Vec::new();
        let mut small_pages = Vec::new();
        resolve_pages(&ranges, |m| {
            if m.huge {
                huge_pages.push(m.phys_addr);
            } else {
                small_pages.push(m.phys_addr);
            }
        });

        assert_eq!(huge_pages, vec![p(0x200000), p(0x400000)]);
        // 前方の端数 [0x1000, 0x200000) は 4KiB ページで埋められる。
        assert!(!small_pages.is_empty());
        assert!(small_pages
            .iter()
            .all(|&a| (p(0x1000)..p(0x200000)).contains(&a)));
        // 全ページが cacheable であることも確認。
        let mut all_cacheable = true;
        resolve_pages(&ranges, |m| {
            if !m.cacheable {
                all_cacheable = false;
            }
        });
        assert!(all_cacheable);
    }

    #[test]
    fn resolve_pages_marks_mmio_as_uncacheable() {
        let bytes = build_map_bytes(&[(memory_type::MMIO, 0x80000000, 1024)], 48);
        let ranges = MappedRanges::<DEFAULT_CAPACITY>::build(&bytes, 48, &[]).unwrap();

        let mut all_uncacheable = true;
        let mut count = 0;
        resolve_pages(&ranges, |m| {
            count += 1;
            if m.cacheable {
                all_uncacheable = false;
            }
        });
        assert!(count > 0);
        assert!(all_uncacheable);
    }

    #[test]
    fn resolve_pages_never_produces_zero_size_gaps_between_core_and_fringe() {
        // ぴったり2MiB境界の範囲: 端数なし、核のみ。
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0x200000, 512)], 48); // 512*4096=2MiB
        let ranges = MappedRanges::<DEFAULT_CAPACITY>::build(&bytes, 48, &[]).unwrap();

        let mut pages = Vec::new();
        resolve_pages(&ranges, |m| pages.push(m));
        assert_eq!(pages.len(), 1);
        assert!(pages[0].huge);
        assert_eq!(pages[0].phys_addr, p(0x200000));
    }

    #[test]
    fn contains_range_detects_gaps() {
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x100000, 4),
                (memory_type::CONVENTIONAL, 0x200000, 4), // 隙間あり (0x104000..0x200000)
            ],
            48,
        );
        let ranges = MappedRanges::<DEFAULT_CAPACITY>::build(&bytes, 48, &[]).unwrap();
        assert!(ranges.contains_range(p(0x100000), p(0x104000)));
        assert!(!ranges.contains_range(p(0x100000), p(0x108000)));
    }

    #[test]
    fn extra_ranges_are_included_even_when_absent_from_the_memory_map() {
        // 実機検証で判明した通り、GOP フレームバッファは UEFI メモリマップに
        // 現れないことがある（PCI BAR はシステムメモリマップとは別扱い）。
        // `extra` で明示的に渡した範囲が正しく取り込まれることを確認する。
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0x100000, 4)], 48);
        let framebuffer = (0x8000_0000u64, 0x8040_0000u64, false);
        let ranges = MappedRanges::<DEFAULT_CAPACITY>::build(&bytes, 48, &[framebuffer]).unwrap();

        assert!(ranges.contains_range(p(0x100000), p(0x104000)));
        assert!(ranges.contains_range(p(0x8000_0000), p(0x8040_0000)));
        assert!(!ranges.contains(p(0x7fff_ffff)));
        assert!(!ranges.contains(p(0x8040_0000)));
    }

    #[test]
    fn extra_range_capacity_exceeded_is_reported() {
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0x100000, 1)], 48);
        let extra = [(0x8000_0000u64, 0x8000_1000u64, false)];
        let result = MappedRanges::<1>::build(&bytes, 48, &extra);
        assert!(result.is_err());
    }
}
