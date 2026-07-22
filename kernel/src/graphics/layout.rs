//! フレームバッファの形状の検証と座標計算（M3-a）。
//!
//! bootloader から渡される [`FramebufferInfo`] は、GOP がそう言っていると
//! いうだけの値であり、kernel 側で裏を取っていない。ここを素通しすると、
//! 描画時に確保範囲外へ書き込んで無言でメモリを壊す。そのため
//! [`FramebufferLayout::from_info`] で全項目を検証し、通らなければ
//! 描画そのものを行わせない。
//!
//! 生ポインタを使わない純粋ロジックであり、ホスト上の `cargo test` で
//! 検証する。実際の書き込みは [`super::framebuffer`] の責務。

use common::addr::{DirectMap, VirtAddr};
use common::boot_info::{FramebufferInfo, PixelFormat};

/// 1 ピクセルあたりのバイト数。`Rgb`/`Bgr` は UEFI 仕様上 32bpp 固定。
pub const BYTES_PER_PIXEL: u64 = 4;

/// [`FramebufferLayout::from_info`] が拒否した理由。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LayoutError {
    /// `Rgb`/`Bgr` 以外。`Bitmask` はビット幅が可変で 32bpp を仮定できず、
    /// `BltOnly` は ExitBootServices 後に使えない。
    UnsupportedPixelFormat(PixelFormat),
    /// 物理アドレスが 0（bootloader が `BltOnly` 等で無効と記録した場合）。
    NullBaseAddress,
    /// 物理アドレスが 4 バイト境界にない。
    MisalignedBaseAddress { base: u64 },
    /// 幅または高さが 0。行数計算のゼロ除算などを未然に防ぐ。
    ZeroDimension { width: u32, height: u32 },
    /// 1 行あたりのピクセル数が幅より小さい。行同士が重なってしまう。
    StrideLessThanWidth { stride: u32, width: u32 },
    /// `height * stride * 4` が `size_bytes` を超える。GOP の申告が
    /// 内部矛盾している状態であり、そのまま描くと確保範囲外へ書く。
    SizeTooSmall { required: u64, size_bytes: u64 },
    /// アドレス計算が u64 で溢れる。
    AddressOverflow,
}

/// 検証済みのフレームバッファ形状。
///
/// この型が存在すること自体が「全項目の検証を通った」ことの証明になる。
/// フィールドを直接構築できないよう非公開にし、[`Self::from_info`] だけを
/// 入口にしている。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FramebufferLayout {
    base: VirtAddr,
    size_bytes: u64,
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
}

impl FramebufferLayout {
    /// bootloader から受け取った情報を検証する。
    /// # 保持した窓は古くなりうる
    ///
    /// ここで受け取った `direct_map` の変換結果を `base` として保持する。
    /// higher-half 移行で `common::addr::replace_direct_map` を呼ぶと、
    /// **この `base` は古い窓で計算された値のまま残る。**
    /// 移行後は必ずこの型を作り直すこと。作り直さないと、正規形ではあるが
    /// 誤った仮想アドレスを指したまま描画を続ける。
    /// 詳細と対処案は `docs/deferred-decisions.md` を参照。
    pub fn from_info(info: &FramebufferInfo, direct_map: DirectMap) -> Result<Self, LayoutError> {
        match info.pixel_format {
            PixelFormat::Rgb | PixelFormat::Bgr => {}
            other => return Err(LayoutError::UnsupportedPixelFormat(other)),
        }

        if info.physical_address.as_u64() == 0 {
            return Err(LayoutError::NullBaseAddress);
        }
        if !info
            .physical_address
            .as_u64()
            .is_multiple_of(BYTES_PER_PIXEL)
        {
            return Err(LayoutError::MisalignedBaseAddress {
                base: info.physical_address.as_u64(),
            });
        }
        if info.width == 0 || info.height == 0 {
            return Err(LayoutError::ZeroDimension {
                width: info.width,
                height: info.height,
            });
        }
        if info.stride < info.width {
            return Err(LayoutError::StrideLessThanWidth {
                stride: info.stride,
                width: info.width,
            });
        }

        // 最終行の末尾までが size_bytes に収まることを確認する。この検証が
        // 無いまま全画面を描くと、申告が食い違っていた場合に範囲外へ書く。
        let required = (info.height as u64)
            .checked_mul(info.stride as u64)
            .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL))
            .ok_or(LayoutError::AddressOverflow)?;
        if required > info.size_bytes {
            return Err(LayoutError::SizeTooSmall {
                required,
                size_bytes: info.size_bytes,
            });
        }

        // 物理アドレスを、direct physical map を通して仮想アドレスへ写す。
        // **ここが型分離の効いている箇所である。** 生の値をポインタにする
        // 経路が無いので、変換を通さずに描画へ進むことができない。
        let base = direct_map.phys_to_virt(info.physical_address);
        base.checked_add(info.size_bytes)
            .ok_or(LayoutError::AddressOverflow)?;

        Ok(Self {
            base,
            size_bytes: info.size_bytes,
            width: info.width,
            height: info.height,
            stride: info.stride,
            format: info.pixel_format,
        })
    }

    /// 同じ形状のまま、別の先頭アドレスを指す形状を作る。
    ///
    /// バックバッファ（通常 RAM 上の、フレームバッファと同じ形状の面）を
    /// 作るために使う。形状が同一なので、フラッシュは行ごとの単純なコピーに
    /// なる（ADR-0017）。
    ///
    /// 元の形状は検証済みだが、先頭アドレスが変われば境界とアドレス計算の
    /// 前提も変わるため、改めて検証し直す。
    pub fn with_base(&self, base: VirtAddr) -> Result<Self, LayoutError> {
        if base.as_u64() == 0 {
            return Err(LayoutError::NullBaseAddress);
        }
        if !base.is_aligned(BYTES_PER_PIXEL) {
            return Err(LayoutError::MisalignedBaseAddress {
                base: base.as_u64(),
            });
        }
        base.checked_add(self.size_bytes)
            .ok_or(LayoutError::AddressOverflow)?;
        Ok(Self { base, ..*self })
    }

    pub fn base(&self) -> VirtAddr {
        self.base
    }

    /// 描画対象範囲の終端（排他）。`base + size_bytes`。
    ///
    /// 構築時に `base.checked_add(size_bytes)` が通っているので、ここで
    /// 桁溢れも非正規化も起きない。
    pub fn end(&self) -> VirtAddr {
        self.base
            .checked_add(self.size_bytes)
            .expect("the layout was validated at construction")
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn stride(&self) -> u32 {
        self.stride
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// `(x, y)` のバイトオフセット。画面外なら `None`。
    ///
    /// `from_info` の検証により、`Some` を返す場合は
    /// `offset + BYTES_PER_PIXEL <= size_bytes` が必ず成り立つ。
    pub fn pixel_offset_bytes(&self, x: u32, y: u32) -> Option<u64> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(((y as u64) * (self.stride as u64) + (x as u64)) * BYTES_PER_PIXEL)
    }

    /// 矩形を画面内へ切り詰める。完全に画面外なら `None`。
    ///
    /// 描画側で範囲外を弾かずに済ませるための前処理。範囲外を panic に
    /// しないのは、描画経路が将来パニック処理から呼ばれた場合に再帰する
    /// のを避けるため（ADR-0013）。
    pub fn clip_rect(&self, x: u32, y: u32, width: u32, height: u32) -> Option<ClippedRect> {
        if width == 0 || height == 0 || x >= self.width || y >= self.height {
            return None;
        }
        // x < width かつ y < height なので、以下の減算は負にならない。
        let clipped_width = width.min(self.width - x);
        let clipped_height = height.min(self.height - y);
        Some(ClippedRect {
            x,
            y,
            width: clipped_width,
            height: clipped_height,
        })
    }
}

/// 画面内へ切り詰め済みの矩形。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClippedRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::addr::PhysAddr;

    /// テスト用の恒等窓。static を直接参照せずに済むよう、`from_info` は
    /// 窓を引数で受け取る形にしてある。
    fn test_map() -> DirectMap {
        DirectMap::identity(DirectMap::IDENTITY_MAX_LENGTH).unwrap()
    }

    fn v(raw: u64) -> VirtAddr {
        VirtAddr::new(raw).unwrap()
    }

    /// 実機（QEMU + OVMF）で実測した値をそのまま使う。
    fn valid_info() -> FramebufferInfo {
        FramebufferInfo {
            physical_address: PhysAddr::new_const(0x8000_0000),
            size_bytes: 4_096_000,
            width: 1280,
            height: 800,
            stride: 1280,
            pixel_format: PixelFormat::Bgr,
            red_mask: 0,
            green_mask: 0,
            blue_mask: 0,
        }
    }

    #[test]
    fn the_real_hardware_values_are_accepted() {
        let layout =
            FramebufferLayout::from_info(&valid_info(), test_map()).expect("should be accepted");
        assert_eq!(layout.width(), 1280);
        assert_eq!(layout.height(), 800);
        assert_eq!(layout.stride(), 1280);
        assert_eq!(layout.end(), v(0x8000_0000 + 4_096_000));
    }

    #[test]
    fn bitmask_and_blt_only_are_rejected() {
        for format in [PixelFormat::Bitmask, PixelFormat::BltOnly] {
            let mut info = valid_info();
            info.pixel_format = format;
            assert_eq!(
                FramebufferLayout::from_info(&info, test_map()),
                Err(LayoutError::UnsupportedPixelFormat(format))
            );
        }
    }

    #[test]
    fn a_null_base_address_is_rejected() {
        let mut info = valid_info();
        info.physical_address = PhysAddr::new_const(0);
        assert_eq!(
            FramebufferLayout::from_info(&info, test_map()),
            Err(LayoutError::NullBaseAddress)
        );
    }

    #[test]
    fn a_misaligned_base_address_is_rejected() {
        let mut info = valid_info();
        info.physical_address = PhysAddr::new_const(0x8000_0001);
        assert_eq!(
            FramebufferLayout::from_info(&info, test_map()),
            Err(LayoutError::MisalignedBaseAddress { base: 0x8000_0001 })
        );
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        for (width, height) in [(0, 800), (1280, 0), (0, 0)] {
            let mut info = valid_info();
            info.width = width;
            info.height = height;
            assert_eq!(
                FramebufferLayout::from_info(&info, test_map()),
                Err(LayoutError::ZeroDimension { width, height })
            );
        }
    }

    #[test]
    fn a_stride_smaller_than_the_width_is_rejected() {
        let mut info = valid_info();
        info.stride = 1279;
        assert_eq!(
            FramebufferLayout::from_info(&info, test_map()),
            Err(LayoutError::StrideLessThanWidth {
                stride: 1279,
                width: 1280
            })
        );
    }

    #[test]
    fn a_size_that_cannot_hold_every_row_is_rejected() {
        // 実機の値から 1 バイトだけ減らす。この 1 バイトを見逃すと最終行の
        // 末尾が確保範囲を踏み出す。
        let mut info = valid_info();
        info.size_bytes = 4_096_000 - 1;
        assert_eq!(
            FramebufferLayout::from_info(&info, test_map()),
            Err(LayoutError::SizeTooSmall {
                required: 4_096_000,
                size_bytes: 4_095_999
            })
        );
    }

    #[test]
    fn a_size_larger_than_required_is_accepted() {
        // パディングが余分にある分には安全側。
        let mut info = valid_info();
        info.size_bytes = 4_096_000 + 4096;
        assert!(FramebufferLayout::from_info(&info, test_map()).is_ok());
    }

    #[test]
    fn an_overflowing_geometry_is_rejected() {
        let mut info = valid_info();
        info.height = u32::MAX;
        info.stride = u32::MAX;
        info.width = u32::MAX;
        info.size_bytes = u64::MAX;
        assert_eq!(
            FramebufferLayout::from_info(&info, test_map()),
            Err(LayoutError::AddressOverflow)
        );
    }

    #[test]
    fn an_overflowing_end_address_is_rejected() {
        let mut info = valid_info();
        // PhysAddr は 52 ビットを超える値を作れないので、以前のように
        // `u64::MAX - 3` を先頭アドレスにはできない。代わりに、表現できる
        // 上限に近い先頭と巨大なサイズを組み合わせて `base + size_bytes` を
        // 桁溢れさせる。
        info.physical_address = PhysAddr::new_const(0x000F_FFFF_FFFF_F000);
        info.width = 1;
        info.height = 1;
        info.stride = 1;
        info.size_bytes = u64::MAX;
        assert_eq!(
            FramebufferLayout::from_info(&info, test_map()),
            Err(LayoutError::AddressOverflow)
        );
    }

    #[test]
    fn every_in_bounds_pixel_stays_inside_the_buffer() {
        let layout = FramebufferLayout::from_info(&valid_info(), test_map()).unwrap();
        // 四隅と、stride の効き方が分かる点を確認する。
        assert_eq!(layout.pixel_offset_bytes(0, 0), Some(0));
        assert_eq!(layout.pixel_offset_bytes(1, 0), Some(4));
        assert_eq!(layout.pixel_offset_bytes(0, 1), Some(1280 * 4));
        let last = layout.pixel_offset_bytes(1279, 799).unwrap();
        assert_eq!(last, (799 * 1280 + 1279) * 4);
        assert!(last + BYTES_PER_PIXEL <= layout.size_bytes());
    }

    #[test]
    fn out_of_bounds_pixels_return_none() {
        let layout = FramebufferLayout::from_info(&valid_info(), test_map()).unwrap();
        assert_eq!(layout.pixel_offset_bytes(1280, 0), None);
        assert_eq!(layout.pixel_offset_bytes(0, 800), None);
        assert_eq!(layout.pixel_offset_bytes(u32::MAX, u32::MAX), None);
    }

    #[test]
    fn padding_between_rows_is_accounted_for() {
        // stride > width の環境（行末にパディングがある）を模す。
        let mut info = valid_info();
        info.width = 1000;
        info.stride = 1024;
        info.height = 100;
        info.size_bytes = 1024 * 100 * 4;
        let layout = FramebufferLayout::from_info(&info, test_map()).unwrap();
        // 2 行目の先頭は width ではなく stride だけ進んだ位置。
        assert_eq!(layout.pixel_offset_bytes(0, 1), Some(1024 * 4));
        // 幅の外は、行内にパディングとして存在していても書かせない。
        assert_eq!(layout.pixel_offset_bytes(1000, 0), None);
    }

    #[test]
    fn a_layout_can_be_rebased_onto_a_back_buffer() {
        let layout = FramebufferLayout::from_info(&valid_info(), test_map()).unwrap();
        let rebased = layout.with_base(v(0x22_8000)).unwrap();
        assert_eq!(rebased.base(), v(0x22_8000));
        assert_eq!(rebased.end(), v(0x22_8000 + 4_096_000));
        // 形状は変わらない。フラッシュが単純コピーになる前提。
        assert_eq!(rebased.width(), layout.width());
        assert_eq!(rebased.height(), layout.height());
        assert_eq!(rebased.stride(), layout.stride());
        assert_eq!(rebased.format(), layout.format());
        assert_eq!(rebased.size_bytes(), layout.size_bytes());
    }

    #[test]
    fn rebasing_revalidates_the_new_base_address() {
        let layout = FramebufferLayout::from_info(&valid_info(), test_map()).unwrap();
        assert_eq!(layout.with_base(v(0)), Err(LayoutError::NullBaseAddress));
        assert_eq!(
            layout.with_base(v(0x22_8001)),
            Err(LayoutError::MisalignedBaseAddress { base: 0x22_8001 })
        );
        assert_eq!(
            layout.with_base(v(u64::MAX - 3)),
            Err(LayoutError::AddressOverflow)
        );
    }

    #[test]
    fn a_rebased_layout_computes_the_same_offsets() {
        let layout = FramebufferLayout::from_info(&valid_info(), test_map()).unwrap();
        let rebased = layout.with_base(v(0x22_8000)).unwrap();
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1279, 799), (1280, 0)] {
            assert_eq!(
                layout.pixel_offset_bytes(x, y),
                rebased.pixel_offset_bytes(x, y)
            );
        }
    }

    #[test]
    fn a_rect_that_hangs_off_the_edge_is_clipped() {
        let layout = FramebufferLayout::from_info(&valid_info(), test_map()).unwrap();
        let rect = layout.clip_rect(1200, 700, 200, 200).unwrap();
        assert_eq!(
            rect,
            ClippedRect {
                x: 1200,
                y: 700,
                width: 80,
                height: 100
            }
        );
    }

    #[test]
    fn a_fully_offscreen_or_empty_rect_is_dropped() {
        let layout = FramebufferLayout::from_info(&valid_info(), test_map()).unwrap();
        assert_eq!(layout.clip_rect(1280, 0, 10, 10), None);
        assert_eq!(layout.clip_rect(0, 800, 10, 10), None);
        assert_eq!(layout.clip_rect(0, 0, 0, 10), None);
        assert_eq!(layout.clip_rect(0, 0, 10, 0), None);
    }

    #[test]
    fn a_clipped_rect_never_leaves_the_buffer() {
        let layout = FramebufferLayout::from_info(&valid_info(), test_map()).unwrap();
        let rect = layout.clip_rect(1279, 799, u32::MAX, u32::MAX).unwrap();
        assert_eq!(rect.width, 1);
        assert_eq!(rect.height, 1);
        let offset = layout
            .pixel_offset_bytes(rect.x + rect.width - 1, rect.y + rect.height - 1)
            .unwrap();
        assert!(offset + BYTES_PER_PIXEL <= layout.size_bytes());
    }
}
