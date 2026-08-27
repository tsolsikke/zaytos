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
/// 面の実体がどこに在るか（PERF-c）。
///
/// # ここが線である
///
/// **フレームバッファは MMIO である**——**PCD（キャッシュ無効）で張っており、
/// 書き込みは `write_volatile` でなければならない**（通常の代入は最適化で
/// 消えたり並べ替えられたりしうる。このモジュールの doc）。
///
/// **バックバッファは普通の RAM である**——**フレームアロケータが返した
/// フレームで、WB（書き戻し）である。** **volatile で書く理由が1つも無い。**
///
/// # 実測（PERF-c）
///
/// **1 画素ずつ `write_volatile` で書いていたので、消す経路が支配していた**
/// ——**`less` の1回の移動で、消すのに 438M サイクル（約124ms）、
/// 転送は 4.7M サイクル（約1.3ms）だった。** **同じ画素数を一括で消す道
/// （`BackBuffer::clear_rows` の `slice::fill`）は45倍速い**（実測）。
///
/// **次に触る者へ**——**RAM の面は一括で書くこと。** **MMIO の面だけが
/// volatile である。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SurfaceKind {
    /// フレームバッファ本体（MMIO。PCD）。
    Mmio,
    /// バックバッファ（普通の RAM）。
    Ram,
}

pub struct Framebuffer {
    kind: SurfaceKind,
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
        Self {
            kind: SurfaceKind::Mmio,
            layout,
        }
    }

    /// 普通の RAM の面として作る（PERF-c）。**バックバッファがこれである。**
    ///
    /// # Safety
    ///
    /// [`Self::new`] と同じ契約に加え、**その領域が普通の RAM であること**
    /// （MMIO でないこと）。**volatile を外して書くので、MMIO へ向けると
    /// 書き込みが消えうる。**
    pub const unsafe fn new_ram(layout: FramebufferLayout) -> Self {
        Self {
            kind: SurfaceKind::Ram,
            layout,
        }
    }

    /// 面の種別（PERF-c）。
    pub fn kind(&self) -> SurfaceKind {
        self.kind
    }

    pub fn layout(&self) -> &FramebufferLayout {
        &self.layout
    }

    /// 連続した画素をまとめて書く（PERF-c）。**RAM の面だけが呼ぶこと。**
    ///
    /// **画面の外へはみ出す分は捨てる**（[`Self::write_pixel`] と同じ規則。
    /// **描画経路から panic を起こさない**。ADR-0013）。
    pub(crate) fn write_pixel_run(&mut self, x: u32, y: u32, pixels: &[u32]) {
        let Some(rect) = self.layout.clip_rect(x, y, pixels.len() as u32, 1) else {
            return;
        };
        let Some(offset) = self.layout.pixel_offset_bytes(rect.x, rect.y) else {
            return;
        };
        let take = (rect.width as usize).min(pixels.len());
        // SAFETY: `clip_rect` と `pixel_offset_bytes` が返した検証済みの位置で、
        // `take` 画素は面の中に収まる。**RAM の面なので通常の書き込みでよい**
        // （[`SurfaceKind`] の doc）。**MMIO の面はこの関数を呼ばない。**
        unsafe {
            let base = self
                .layout
                .base()
                .as_mut_ptr::<u32>()
                .byte_add(offset as usize);
            core::slice::from_raw_parts_mut(base, take).copy_from_slice(&pixels[..take]);
        }
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
            // **RAM の面は一括で埋める（PERF-c）。** **1 画素ずつ volatile で
            // 書く必要が無い**（[`SurfaceKind`] の doc）。**実測で、ここが
            // 描画の費用の6割を占めていた。**
            //
            // 破壊 (PERF-c, draw-pixel-by-pixel-test): RAM でも 1 画素ずつ書く。
            // **PERF-c の前の形そのものである**——**出る絵は同じで、費用だけが
            // 桁で増える。** **描く費用の判定が捕まえる。**
            #[cfg(not(feature = "draw-pixel-by-pixel-test"))]
            if self.kind == SurfaceKind::Ram {
                // SAFETY: row_offset は検証済みで、この行の rect.width 画素は
                // 面の中に収まる（上の debug_assert と clip_rect の保証）。
                // **RAM なので通常の書き込みでよい**（[`SurfaceKind`] の doc）。
                let row_slice = unsafe {
                    core::slice::from_raw_parts_mut(
                        base.as_mut_ptr::<u32>().byte_add(row_offset as usize),
                        rect.width as usize,
                    )
                };
                row_slice.fill(pixel);
                continue;
            }
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
