//! 色の表現と、ピクセルフォーマットに応じた 32bit 値への変換（M3-a）。
//!
//! 生の `u32` をコード中に直接書くと、`Rgb` と `Bgr` で赤と青が入れ替わる。
//! 実機（QEMU + OVMF）は `Bgr` だが、これに合わせた定数を書くと `Rgb` の
//! 環境で色が化ける。そのため色は必ず [`Color`] で持ち、書き込み直前に
//! [`Color::to_pixel`] でフォーマットへ合わせる。
//!
//! 生ポインタを使わない純粋ロジックであり、ホスト上の `cargo test` で
//! 検証する。

use common::boot_info::PixelFormat;

/// 8bit ずつの RGB 色。アルファは扱わない（UEFI の 32bpp では予約バイト）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Color {
    pub const BLACK: Color = Color::rgb(0x00, 0x00, 0x00);
    pub const WHITE: Color = Color::rgb(0xFF, 0xFF, 0xFF);
    pub const RED: Color = Color::rgb(0xFF, 0x00, 0x00);
    pub const GREEN: Color = Color::rgb(0x00, 0xFF, 0x00);
    pub const BLUE: Color = Color::rgb(0x00, 0x00, 0xFF);

    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    /// 指定フォーマットで 1 ピクセル分の 32bit 値へ変換する。
    ///
    /// x86_64 はリトルエンディアンなので、`u32` を書き込むと最下位バイトが
    /// 最も低いアドレスへ入る。UEFI の `Bgr` は先頭バイトから B, G, R, 予約の
    /// 順に並ぶため、`u32` としては `0x00RRGGBB` になる（`Rgb` はその逆）。
    ///
    /// `Bitmask` / `BltOnly` は [`super::layout::FramebufferLayout`] の構築
    /// 時点で弾いているため、ここへは到達しない。到達した場合は実装の
    /// 不整合なので、黒を返して無言で化けるより panic させる。
    pub const fn to_pixel(self, format: PixelFormat) -> u32 {
        let (red, green, blue) = (self.red as u32, self.green as u32, self.blue as u32);
        match format {
            PixelFormat::Bgr => (red << 16) | (green << 8) | blue,
            PixelFormat::Rgb => (blue << 16) | (green << 8) | red,
            PixelFormat::Bitmask | PixelFormat::BltOnly => {
                panic!("Color::to_pixel called with an unsupported pixel format")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgr_places_red_in_the_third_byte() {
        // メモリ上のバイト列は B, G, R, 予約。リトルエンディアンの u32 では
        // 0x00RRGGBB。
        let pixel = Color::rgb(0x12, 0x34, 0x56).to_pixel(PixelFormat::Bgr);
        assert_eq!(pixel, 0x0012_3456);
        assert_eq!(pixel.to_le_bytes(), [0x56, 0x34, 0x12, 0x00]);
    }

    #[test]
    fn rgb_places_blue_in_the_third_byte() {
        let pixel = Color::rgb(0x12, 0x34, 0x56).to_pixel(PixelFormat::Rgb);
        assert_eq!(pixel, 0x0056_3412);
        assert_eq!(pixel.to_le_bytes(), [0x12, 0x34, 0x56, 0x00]);
    }

    #[test]
    fn the_two_formats_swap_red_and_blue() {
        let color = Color::rgb(0xFF, 0x00, 0x00);
        let bgr = color.to_pixel(PixelFormat::Bgr).to_le_bytes();
        let rgb = color.to_pixel(PixelFormat::Rgb).to_le_bytes();
        assert_eq!(bgr[2], 0xFF);
        assert_eq!(rgb[0], 0xFF);
        assert_ne!(bgr, rgb);
    }

    #[test]
    fn the_reserved_byte_is_always_zero() {
        for format in [PixelFormat::Bgr, PixelFormat::Rgb] {
            let pixel = Color::rgb(0xFF, 0xFF, 0xFF).to_pixel(format);
            assert_eq!(pixel.to_le_bytes()[3], 0x00);
        }
    }

    #[test]
    #[should_panic]
    fn bitmask_is_rejected() {
        let _ = Color::WHITE.to_pixel(PixelFormat::Bitmask);
    }
}
