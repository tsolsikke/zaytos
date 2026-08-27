//! 文字列の描画（M3-b）。
//!
//! 1 行分をそのまま描くだけの層で、改行や折り返し・スクロールは扱わない。
//! それらは M3-c のコンソールの責務とする。
//!
//! 実際の書き込みは [`Framebuffer`] に委ねており、画面外は切り詰められる。
//! ここに置いてある純粋な計算（文字列の描画幅）はホスト `cargo test` で
//! 検証する。

use super::color::Color;
use super::font::{self, Glyph};
use super::framebuffer::Framebuffer;

/// `text` を 1 行に描いたときの幅（ピクセル）。
///
/// 全角は半角 2 つ分として数える。収録されていない文字は、実際に描かれる
/// 代替グリフの幅で数えるため、この値は描画結果と一致する。
///
/// 桁溢れしないよう飽和加算する。u32 を超える幅の文字列は現実には存在
/// しないが、溢れた値で座標計算をすると描画位置が巻き戻る。
pub fn text_width_pixels(text: &str) -> u32 {
    text.chars().fold(0u32, |width, c| {
        width.saturating_add(font::glyph(c).width_pixels())
    })
}

impl Framebuffer {
    /// グリフ 1 文字を描く。
    ///
    /// `background` が `Some` なら、立っていないピクセルをその色で塗る。
    /// `None` なら背景をそのまま残す（重ね描き）。コンソールで文字を
    /// 書き換える場合、背景を塗らないと前の字形が残るため `Some` を使う。
    pub fn draw_glyph(
        &mut self,
        x: u32,
        y: u32,
        glyph: Glyph,
        foreground: Color,
        background: Option<Color>,
    ) {
        // **RAM の面は 1 行ぶんをまとめて書く（PERF-c）。**
        //
        // **1 画素ずつ `write_volatile` で書いていた**——**MMIO の規則を
        // RAM の面へも当てていた**（`super::framebuffer::SurfaceKind` の doc）。
        // **背景を塗る形（コンソールが使う側）では、1 行が連続した 8 画素に
        // なるので、組み立てて 1 回で写せる。**
        //
        // **重ね描き（`background` が `None`）は 1 画素ずつのままである**
        // ——**穴が開くので連続にならない。** **利用者はデモの経路だけで、
        // コンソールは必ず背景を塗る。**
        //
        // 破壊 (PERF-c, draw-pixel-by-pixel-test): まとめずに 1 画素ずつ書く。
        if let (Some(background), false) = (
            background,
            cfg!(feature = "draw-pixel-by-pixel-test")
                || self.kind() != super::framebuffer::SurfaceKind::Ram,
        ) {
            self.draw_glyph_rows(x, y, glyph, foreground, background);
            return;
        }
        for row in 0..glyph.height_pixels() {
            for column in 0..glyph.width_pixels() {
                let color = if glyph.is_set(column, row) {
                    foreground
                } else {
                    match background {
                        Some(background) => background,
                        None => continue,
                    }
                };
                // 画面外の座標は write_pixel 側で無視される。
                self.write_pixel(x.saturating_add(column), y.saturating_add(row), color);
            }
        }
    }

    /// グリフを 1 行ずつまとめて書く（PERF-c）。**RAM の面だけが通る。**
    ///
    /// **`MAX_GLYPH_WIDTH` 画素までを組み立てて、行ごとに 1 回で写す。**
    fn draw_glyph_rows(
        &mut self,
        x: u32,
        y: u32,
        glyph: Glyph,
        foreground: Color,
        background: Color,
    ) {
        /// 組み立てる行の上限（画素）。**全角でも 16 である**
        /// （`common::screen::MAX_GLYPH_WIDTH_CELLS` × セル幅）。
        const MAX_GLYPH_WIDTH: usize = 32;
        let width = (glyph.width_pixels() as usize).min(MAX_GLYPH_WIDTH);
        let format = self.layout().format();
        let front = foreground.to_pixel(format);
        let back = background.to_pixel(format);
        let mut line = [0u32; MAX_GLYPH_WIDTH];
        for row in 0..glyph.height_pixels() {
            for (column, slot) in line[..width].iter_mut().enumerate() {
                *slot = if glyph.is_set(column as u32, row) {
                    front
                } else {
                    back
                };
            }
            self.write_pixel_run(x, y.saturating_add(row), &line[..width]);
        }
    }

    /// 文字列を 1 行に描き、描画後のペン位置（x 座標）を返す。
    ///
    /// 改行などの制御文字は解釈せず、収録されていない文字と同じく代替
    /// グリフとして描く。制御文字の扱いはコンソール（M3-c）の責務。
    pub fn draw_str(
        &mut self,
        x: u32,
        y: u32,
        text: &str,
        foreground: Color,
        background: Option<Color>,
    ) -> u32 {
        let screen_width = self.layout().width();
        let mut pen_x = x;

        for c in text.chars() {
            // 右端を越えたら、以降は 1 ピクセルも見えないので打ち切る。
            if pen_x >= screen_width {
                break;
            }
            let glyph = font::glyph(c);
            self.draw_glyph(pen_x, y, glyph, foreground, background);
            pen_x = pen_x.saturating_add(glyph.width_pixels());
        }

        pen_x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_width_of_ascii_is_eight_pixels_per_character() {
        assert_eq!(text_width_pixels("A"), 8);
        assert_eq!(text_width_pixels("ZaytOS"), 6 * 8);
    }

    #[test]
    fn an_empty_string_has_no_width() {
        assert_eq!(text_width_pixels(""), 0);
    }

    #[test]
    fn unlisted_characters_are_counted_as_the_replacement_glyph() {
        // 代替グリフは半角なので、日本語 1 文字も現時点では 8 ピクセル。
        // 日本語を収録した時点で 16 ピクセルへ変わる。
        let replacement_width = font::glyph(font::REPLACEMENT_CHARACTER).width_pixels();
        assert_eq!(text_width_pixels("あ"), replacement_width);
    }

    #[test]
    fn the_measured_width_matches_the_sum_of_the_glyph_widths() {
        let text = "ZaytOS 0.1";
        let expected: u32 = text.chars().map(|c| font::glyph(c).width_pixels()).sum();
        assert_eq!(text_width_pixels(text), expected);
    }
}
