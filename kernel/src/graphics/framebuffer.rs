//! フレームバッファへの実際の書き込み（M3-a）。
//!
//! 生ポインタでフレームバッファへ直接書き込むため、意図的にホスト
//! `cargo test` の対象にしていない。形状の検証・座標計算・色変換は
//! [`super::layout`] と [`super::color`] に分離済みで、そちらはテスト
//! してある。ここに残しているのは「検証済みのオフセットへ書く」ことだけ。
//!
//! 書き込みには `write_volatile` を使う。フレームバッファは PCD
//! （キャッシュ無効）でマップしているためキャッシュには載らないが、
//! コンパイラから見ると「読み返さない書き込み」であり、通常の代入だと
//! 最適化で消えたり並べ替えられたりしうる。それを防ぐのは volatile の
//! 役割であってページ属性の役割ではない。

use super::color::Color;
use super::layout::{ClippedRect, FramebufferLayout, BYTES_PER_PIXEL};

/// 検証済みのフレームバッファへの排他的な書き込みハンドル。
pub struct Framebuffer {
    layout: FramebufferLayout,
}

impl Framebuffer {
    /// # Safety
    ///
    /// 呼び出し側は以下を保証すること。
    ///
    /// - `layout.base()..layout.end()` の全体が、現在のページテーブルで
    ///   書き込み可能にマップされていること。M2-d の必須領域検証
    ///   （`MappedRanges::contains_range`）で確認したうえで渡すこと。
    /// - この範囲を他の誰も使っていないこと。`Framebuffer` は排他所有を
    ///   前提とし、同じ領域に対して複数個作らないこと。
    /// - `layout` が [`FramebufferLayout::from_info`] を通ったものであること
    ///   （型がそれを保証しているが、`base` が実在のフレームバッファを
    ///   指しているかどうかまでは型では保証できない）。
    pub const unsafe fn new(layout: FramebufferLayout) -> Self {
        Self { layout }
    }

    pub fn layout(&self) -> &FramebufferLayout {
        &self.layout
    }

    /// 1 ピクセル書き込む。画面外の座標は無視する。
    ///
    /// 範囲外を panic にしないのは、描画経路から panic を起こさないため
    /// （ADR-0013）。
    pub fn write_pixel(&mut self, x: u32, y: u32, color: Color) {
        let Some(offset) = self.layout.pixel_offset_bytes(x, y) else {
            return;
        };
        let pixel = color.to_pixel(self.layout.format());
        // SAFETY: offset は pixel_offset_bytes が返した検証済みの値であり、
        // FramebufferLayout の構築時検証により
        // offset + BYTES_PER_PIXEL <= size_bytes が成り立つ。base..end は
        // new の安全性要件によりマップ済み・排他所有。4 バイト境界であることも
        // 構築時に検証済みで、offset は 4 の倍数なので u32 のアラインメントを
        // 満たす。
        unsafe {
            core::ptr::write_volatile(
                self.layout
                    .base()
                    .as_mut_ptr::<u32>()
                    .byte_add(offset as usize),
                pixel,
            );
        }
    }

    /// 1 ピクセルを生の値で読む。画面外は `None`（zi-b の判定用）。
    ///
    /// **判定のために置いた**——ANSI の消去がセルを背景へ戻したことを、
    /// バックバッファを読んで確かめる（`ansi-test`）。描画経路は使わない。
    pub fn read_pixel_raw(&self, x: u32, y: u32) -> Option<u32> {
        let offset = self.layout.pixel_offset_bytes(x, y)?;
        // SAFETY: offset は pixel_offset_bytes が返した検証済みの値であり、
        // 構築時検証により offset + BYTES_PER_PIXEL <= size_bytes が成り立つ。
        // base..end は new の安全性要件によりマップ済み・排他所有で、読みは
        // 書きと同じ範囲・同じ整列（4 バイト境界）である。volatile なのは
        // write_pixel と同じ理由（読み返しの最適化を許さない）。
        Some(unsafe {
            core::ptr::read_volatile(
                self.layout
                    .base()
                    .as_mut_ptr::<u32>()
                    .byte_add(offset as usize),
            )
        })
    }

    /// 矩形を塗る。画面外へはみ出す分は切り詰める。
    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: Color) {
        let Some(rect) = self.layout.clip_rect(x, y, width, height) else {
            return;
        };
        self.fill_clipped(rect, color);
    }

    /// 画面全体を塗る。
    pub fn clear(&mut self, color: Color) {
        self.fill_rect(0, 0, self.layout.width(), self.layout.height(), color);
    }

    /// 切り詰め済み矩形を塗る。行の先頭オフセットを 1 回だけ求め、
    /// 行内はポインタを進めるだけにする。
    fn fill_clipped(&mut self, rect: ClippedRect, color: Color) {
        let pixel = color.to_pixel(self.layout.format());
        let base = self.layout.base();

        for row in 0..rect.height {
            // clip_rect が保証する範囲内なので None にはならないが、
            // 万一崩れても範囲外へ書かないよう Option のまま扱う。
            let Some(row_offset) = self.layout.pixel_offset_bytes(rect.x, rect.y + row) else {
                debug_assert!(false, "clip_rect returned a row outside the framebuffer");
                return;
            };
            debug_assert!(
                row_offset + (rect.width as u64) * BYTES_PER_PIXEL <= self.layout.size_bytes(),
                "clipped row would run past the end of the framebuffer"
            );
            // SAFETY: row_offset は検証済みオフセットで、base..end はマップ済み。
            let row_ptr = unsafe { base.as_mut_ptr::<u32>().byte_add(row_offset as usize) };
            for column in 0..rect.width {
                // SAFETY: row_offset は検証済みオフセット。column < rect.width
                // であり、clip_rect により rect.x + rect.width <= layout.width()
                // かつ layout.width() <= layout.stride() なので、この行内の
                // 書き込みは同じ行のパディングも超えない。したがって
                // row_offset + column * 4 + 4 <= size_bytes。
                unsafe {
                    core::ptr::write_volatile(row_ptr.add(column as usize), pixel);
                }
            }
        }
    }
}
