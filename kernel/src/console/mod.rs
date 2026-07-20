//! 画面コンソール（M3）。
//!
//! - [`grid`][mod@grid]: カーソルと桁送りの管理（純粋ロジック、ホスト
//!   `cargo test` で検証）。
//! - [`dirty`][mod@dirty]: 未転送範囲の追跡（純粋ロジック、ホスト
//!   `cargo test` で検証）。
//!
//! 設計の背景は ADR-0017 を参照。要点:
//! - バックバッファは通常 RAM に置き、フレームバッファと同じ形式・同じ
//!   stride で持つ。転送を単純なコピーにするため。
//! - 転送は未転送範囲の外接矩形だけを送る。フラッシュ点は `write_fmt`
//!   1 回ごと（`write!` / `writeln!` 1 回ごと）。
//! - コンソールはグローバルに置かず、ロックも持たない。割り込みハンドラ
//!   から出力する要求は M4 で生じるため、そのロック設計は割り込み安全性の
//!   モデルと一体で決める。
//! - シリアルログはコンソールから完全に独立している。コンソールの構築に
//!   失敗してもシリアルログは影響を受けない。
//! - パニック時に画面へは出さない（ADR-0013 Addendum）。

pub mod dirty;
pub mod grid;

pub use dirty::{DirtyRegion, Rect};
pub use grid::{Grid, GridError, Placement, Step, MAX_GLYPH_WIDTH_CELLS, TAB_WIDTH};

#[cfg(test)]
mod tests {
    use super::grid::MAX_GLYPH_WIDTH_CELLS;
    use crate::graphics::font;

    /// 格子が想定するグリフ幅の上限が、実際にフォントへ収録されている最大幅を
    /// 下回っていないことを確かめる。
    ///
    /// 下回ると、折り返しても置けないグリフが生じ、`Grid` の防御的な破棄
    /// 処理へ落ちて文字が消える。日本語（全角 2 セル）を収録した時点でも
    /// この関係が保たれていることを、ここで機械的に検出する。
    #[test]
    fn the_grid_can_hold_the_widest_glyph_in_the_font() {
        assert!(
            font::max_width_cells() <= MAX_GLYPH_WIDTH_CELLS,
            "フォントに {} セル幅のグリフがあるが、格子の想定上限は {} セル。\
             console::grid::MAX_GLYPH_WIDTH_CELLS を引き上げること",
            font::max_width_cells(),
            MAX_GLYPH_WIDTH_CELLS
        );
    }
}
