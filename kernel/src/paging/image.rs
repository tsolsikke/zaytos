//! カーネルの像の区画と、区画ごとの権限（2026-10-02。`ADR-0071` の手順 4）。**純粋な論理。**
//!
//! 像を高い番地へ写すとき、区画ごとに権限を変える——コードは読んで実行する（書けない）、読むだけの区画は
//! 読むだけ、データは書けて実行しない。**区画の境（`kernel/link.ld` の記号）から、写す範囲と権限の組を作るのが
//! ここである。** ポインタにもレジスタにも触らない。ホストの `cargo test` で確かめる。
//!
//! **区画はどれも 4KiB の境から始まる**（`link.ld` が揃えている）。区画ごとに範囲を分けて写すと、2MiB の境を
//! またぐ区画も 4KiB の葉になり、1 枚の 2MiB の葉に、権限の違う区画が同居することが無くなる。

use crate::frame_allocator::FRAME_SIZE;
use crate::paging::permissions::PagePermissions;

/// 像の区画の境（物理の番地）。`kernel/link.ld` の記号から作る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBounds {
    /// 像の先頭（`__kernel_start`）。コードの区画（起動のトランポリンと `.text`）の先頭である。
    pub start: u64,
    /// 読むだけの区画の先頭（`__rodata_start`）。
    pub read_only_start: u64,
    /// 書ける区画の先頭（`__data_start`）。
    pub data_start: u64,
    /// 像の終わり（`__kernel_end`。4KiB の境でなくてよい。写すときに切り上げる）。
    pub end: u64,
}

/// 像の 1 つの区画を写す範囲と権限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSection {
    /// 起動ログとページの権限の確かめに出す名前。
    pub name: &'static str,
    /// 範囲の先頭（物理の番地。4KiB の境）。
    pub start: u64,
    /// 範囲の終わり（物理の番地。4KiB の境。この番地は含まない）。
    pub end: u64,
    /// 写すときの権限。
    pub permissions: PagePermissions,
}

impl ImageSection {
    /// 範囲の長さ（バイト。4KiB の倍数）。
    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    /// 範囲が空か（[`image_sections`] は、空の区画を断るので、返す区画はどれも空でない）。
    pub const fn is_empty(&self) -> bool {
        self.end == self.start
    }

    /// 範囲のページ数（4KiB）。
    pub const fn pages(&self) -> u64 {
        self.len() / FRAME_SIZE
    }
}

/// 区画の境が、写せる形になっていない理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageLayoutError {
    /// 境が 4KiB に揃っていない（像の終わりを除く）。**揃っていないと、1 ページに権限の違う区画が載る。**
    NotPageAligned,
    /// 境の順序が違うか、空の区画が在る（コード、読むだけ、書ける、の順で、どれも 1 ページ以上）。
    OutOfOrder,
}

/// 区画の境から、写す範囲と権限の 3 組を作る。**コード、読むだけ、書ける、の順である。**
///
/// - コード: [`PagePermissions::kernel_code`]（読んで実行する。書けない）
/// - 読むだけ: [`PagePermissions::kernel_read_only`]
/// - 書ける（`.data` と `.bss`）: [`PagePermissions::kernel_data`]（実行しない）
///
/// **3 つの範囲は、隙間なく、重なりなく、像の全部を覆う**（像の終わりは 4KiB へ切り上げる）。
pub fn image_sections(bounds: ImageBounds) -> Result<[ImageSection; 3], ImageLayoutError> {
    let aligned = |address: u64| address.is_multiple_of(FRAME_SIZE);
    if !aligned(bounds.start) || !aligned(bounds.read_only_start) || !aligned(bounds.data_start) {
        return Err(ImageLayoutError::NotPageAligned);
    }
    let end = bounds.end.next_multiple_of(FRAME_SIZE);
    if !(bounds.start < bounds.read_only_start
        && bounds.read_only_start < bounds.data_start
        && bounds.data_start < end)
    {
        return Err(ImageLayoutError::OutOfOrder);
    }
    Ok([
        ImageSection {
            name: "code",
            start: bounds.start,
            end: bounds.read_only_start,
            permissions: PagePermissions::kernel_code(),
        },
        ImageSection {
            name: "read-only data",
            start: bounds.read_only_start,
            end: bounds.data_start,
            permissions: PagePermissions::kernel_read_only(),
        },
        ImageSection {
            name: "data and bss",
            start: bounds.data_start,
            end,
            permissions: PagePermissions::kernel_data(),
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-02 に測った、既定のビルドの境（物理の番地）。
    const MEASURED: ImageBounds = ImageBounds {
        start: 0x10_0000,
        read_only_start: 0x21_9000,
        data_start: 0x27_1000,
        end: 0x4d_29b0,
    };

    #[test]
    fn the_measured_image_splits_into_code_read_only_and_data() {
        let sections = image_sections(MEASURED).unwrap();
        assert_eq!(
            sections.map(|section| (section.name, section.start, section.end, section.pages())),
            [
                ("code", 0x10_0000, 0x21_9000, 281),
                ("read-only data", 0x21_9000, 0x27_1000, 88),
                ("data and bss", 0x27_1000, 0x4d_3000, 610),
            ]
        );
    }

    /// **コードは書けず、ほかは実行できない**（写像ごとの W^X）。
    #[test]
    fn no_section_is_both_writable_and_executable() {
        let [code, read_only, data] = image_sections(MEASURED).unwrap();
        assert!(code.permissions.execute() && !code.permissions.write());
        assert!(!read_only.permissions.execute() && !read_only.permissions.write());
        assert!(!data.permissions.execute() && data.permissions.write());
        for section in [code, read_only, data] {
            assert!(!section.permissions.user(), "{}", section.name);
        }
    }

    /// **3 つの範囲は、隙間も重なりも無く、像の全部を覆う。** 像の終わりは 4KiB へ切り上げる。
    #[test]
    fn the_sections_cover_the_image_without_a_gap_or_an_overlap() {
        let sections = image_sections(MEASURED).unwrap();
        assert_eq!(sections[0].start, MEASURED.start);
        assert_eq!(sections[0].end, sections[1].start);
        assert_eq!(sections[1].end, sections[2].start);
        assert_eq!(sections[2].end, 0x4d_3000);
        assert!(sections.iter().all(|section| !section.is_empty()));
        // 像の終わりが、ちょうど 4KiB の境のときは、切り上げても変わらない。
        let exact = image_sections(ImageBounds {
            end: 0x4d_3000,
            ..MEASURED
        })
        .unwrap();
        assert_eq!(exact[2].end, 0x4d_3000);
    }

    /// **境が 4KiB に揃っていなければ断る**——1 ページに、権限の違う区画が載るからである。
    #[test]
    fn a_boundary_inside_a_page_is_refused() {
        for bounds in [
            ImageBounds {
                start: 0x10_0800,
                ..MEASURED
            },
            ImageBounds {
                read_only_start: 0x21_9010,
                ..MEASURED
            },
            ImageBounds {
                data_start: 0x27_0bb8,
                ..MEASURED
            },
        ] {
            assert_eq!(
                image_sections(bounds),
                Err(ImageLayoutError::NotPageAligned),
                "{bounds:?}"
            );
        }
    }

    /// **順序が違う境と、空の区画は断る。**
    #[test]
    fn boundaries_out_of_order_and_empty_sections_are_refused() {
        for bounds in [
            // 読むだけの区画が、コードより前。
            ImageBounds {
                read_only_start: 0x0f_f000,
                ..MEASURED
            },
            // 読むだけの区画が空。
            ImageBounds {
                data_start: MEASURED.read_only_start,
                ..MEASURED
            },
            // 書ける区画が空。
            ImageBounds {
                end: MEASURED.data_start,
                ..MEASURED
            },
            // コードが空。
            ImageBounds {
                read_only_start: MEASURED.start,
                ..MEASURED
            },
        ] {
            assert_eq!(
                image_sections(bounds),
                Err(ImageLayoutError::OutOfOrder),
                "{bounds:?}"
            );
        }
    }
}
