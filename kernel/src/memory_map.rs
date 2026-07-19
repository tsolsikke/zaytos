//! UEFI メモリマップ（`BootInfo.memory_map` が指す生バイト列）の解析。
//!
//! バイトスライスの読み取りのみで完結する純粋ロジックであり、unsafe を
//! 一切使わない。ホスト上の `cargo test` で検証する。
//! 実際のメモリからバイトスライスを取り出す部分（生ポインタの
//! dereference）はハードウェア依存側（`main.rs`）の責務とし、ここには
//! 含めない。

/// `EFI_MEMORY_TYPE` の値（UEFI 仕様）。
pub mod memory_type {
    pub const RESERVED: u32 = 0;
    pub const LOADER_CODE: u32 = 1;
    pub const LOADER_DATA: u32 = 2;
    pub const BOOT_SERVICES_CODE: u32 = 3;
    pub const BOOT_SERVICES_DATA: u32 = 4;
    pub const RUNTIME_SERVICES_CODE: u32 = 5;
    pub const RUNTIME_SERVICES_DATA: u32 = 6;
    pub const CONVENTIONAL: u32 = 7;
    pub const UNUSABLE: u32 = 8;
    pub const ACPI_RECLAIM: u32 = 9;
    pub const ACPI_NVS: u32 = 10;
    pub const MMIO: u32 = 11;
    pub const MMIO_PORT_SPACE: u32 = 12;
    pub const PAL_CODE: u32 = 13;
    pub const PERSISTENT: u32 = 14;
    pub const UNACCEPTED: u32 = 15;
    /// この値以上は OEM (`0x7000_0000`〜) / OS ベンダー (`0x8000_0000`〜)
    /// 予約領域。**メモリ「型の値」がこの範囲という意味であり、物理
    /// アドレスの範囲ではない**（フレームバッファの物理アドレスが
    /// たまたま `0x8000_0000` 付近になることがあるため、混同しないよう
    /// 注意）。
    pub const VENDOR_RESERVED_START: u32 = 0x7000_0000;
}

/// `EFI_MEMORY_DESCRIPTOR` のうち、空き判定に必要な最小限のフィールド。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryMapEntry {
    pub memory_type: u32,
    pub phys_start: u64,
    pub page_count: u64,
}

// `EFI_MEMORY_DESCRIPTOR` 内でのフィールドオフセット（バイト）。
// Type(4) + 4バイトパディング + PhysicalStart(8) + VirtualStart(8) +
// NumberOfPages(8) + Attribute(8) という構造上、少なくとも
// NumberOfPages の終端（オフセット32）までを読めれば良い。
const OFFSET_TYPE: usize = 0;
const OFFSET_PHYS_START: usize = 8;
const OFFSET_PAGE_COUNT: usize = 24;
const MIN_DESCRIPTOR_SIZE: usize = OFFSET_PAGE_COUNT + 8;

/// UEFI メモリマップの生バイト列を、`descriptor_size` を歩幅として解釈し、
/// 各エントリを返すイテレータを作る。
///
/// **`size_of::<EFI_MEMORY_DESCRIPTOR>()` や固定値（例: 48）を歩幅に
/// 使ってはならない。** ファームウェアは将来の拡張フィールドのために、
/// これより大きいサイズを報告することがあり（実機の QEMU + OVMF でも
/// 48 バイトだった一方、仕様上の最小構造体は 40 バイト）、固定サイズを
/// 仮定すると 2 エントリ目以降の解釈が全てずれる。
pub fn parse_entries(
    raw: &[u8],
    descriptor_size: u64,
) -> Result<impl Iterator<Item = MemoryMapEntry> + '_, &'static str> {
    let stride =
        usize::try_from(descriptor_size).map_err(|_| "descriptor_size does not fit in usize")?;
    if stride < MIN_DESCRIPTOR_SIZE {
        return Err("descriptor_size is smaller than the fields we need to read");
    }

    Ok(raw.chunks_exact(stride).map(|entry| MemoryMapEntry {
        memory_type: u32::from_le_bytes(entry[OFFSET_TYPE..OFFSET_TYPE + 4].try_into().unwrap()),
        phys_start: u64::from_le_bytes(
            entry[OFFSET_PHYS_START..OFFSET_PHYS_START + 8]
                .try_into()
                .unwrap(),
        ),
        page_count: u64::from_le_bytes(
            entry[OFFSET_PAGE_COUNT..OFFSET_PAGE_COUNT + 8]
                .try_into()
                .unwrap(),
        ),
    }))
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// テスト用に、指定した `descriptor_size` で 1 エントリ分のバイト列を
    /// 組み立てる（末尾のパディングは 0 埋め）。
    pub fn build_entry_bytes(
        memory_type: u32,
        phys_start: u64,
        page_count: u64,
        descriptor_size: usize,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; descriptor_size];
        buf[OFFSET_TYPE..OFFSET_TYPE + 4].copy_from_slice(&memory_type.to_le_bytes());
        buf[OFFSET_PHYS_START..OFFSET_PHYS_START + 8].copy_from_slice(&phys_start.to_le_bytes());
        buf[OFFSET_PAGE_COUNT..OFFSET_PAGE_COUNT + 8].copy_from_slice(&page_count.to_le_bytes());
        buf
    }

    pub fn build_map_bytes(entries: &[(u32, u64, u64)], descriptor_size: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(entries.len() * descriptor_size);
        for &(ty, phys_start, page_count) in entries {
            buf.extend(build_entry_bytes(
                ty,
                phys_start,
                page_count,
                descriptor_size,
            ));
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn parses_single_entry_at_real_observed_descriptor_size() {
        // 実機 (QEMU + OVMF) で実測した descriptor_size=48（仕様上の最小
        // 構造体サイズ 40 より大きい）でも正しくパースできることを確認する。
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0x100000, 10)], 48);
        let entries: Vec<_> = parse_entries(&bytes, 48).unwrap().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].memory_type, memory_type::CONVENTIONAL);
        assert_eq!(entries[0].phys_start, 0x100000);
        assert_eq!(entries[0].page_count, 10);
    }

    #[test]
    fn parses_multiple_entries_with_nonstandard_descriptor_size() {
        // 48 とも 40 (size_of 相当) とも異なる値でも歩幅として機能すること。
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x1000, 4),
                (memory_type::BOOT_SERVICES_DATA, 0x5000, 2),
                (memory_type::LOADER_DATA, 0x7000, 1),
            ],
            56,
        );
        let entries: Vec<_> = parse_entries(&bytes, 56).unwrap().collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].memory_type, memory_type::BOOT_SERVICES_DATA);
        assert_eq!(entries[1].phys_start, 0x5000);
        assert_eq!(entries[1].page_count, 2);
        assert_eq!(entries[2].phys_start, 0x7000);
    }

    #[test]
    fn rejects_descriptor_size_too_small_to_hold_required_fields() {
        let bytes = vec![0u8; 16];
        assert!(parse_entries(&bytes, 16).is_err());
    }

    #[test]
    fn zero_page_count_entry_is_parsed_but_carries_no_pages() {
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0x9000, 0)], 48);
        let entries: Vec<_> = parse_entries(&bytes, 48).unwrap().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].page_count, 0);
    }
}
