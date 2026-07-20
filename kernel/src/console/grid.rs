//! カーソルと桁送りの管理（M3-c-1）。
//!
//! 「次の文字をどのセルに置くか」「折り返すか」「スクロールするか」だけを
//! 決める。フレームバッファにもフォントにも依存しない純粋ロジックで、
//! ホスト上の `cargo test` で検証する。実際に描くのは呼び出し側の責務。
//!
//! グリフの幅（セル数）は呼び出し側が渡す。ここでフォントを引かないのは、
//! 幅の決め方（未収録文字のフォールバック等）と桁送りの規則を独立に
//! テストできるようにするため。
//!
//! 全角は 2 セルを占める。行末に 1 セルしか残っていない状態で全角が来たら、
//! その 1 セルを空白のまま残して次行の先頭から描く。半端に描かない
//! （ADR-0017）。この規則により、カーソルが全角セルの途中を指すことは
//! 構造的に起こらない。

/// タブ位置の間隔（セル数）。
pub const TAB_WIDTH: u32 = 8;

/// [`Grid::new`] が拒否した理由。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GridError {
    /// 桁数または行数が 0。文字を 1 つも置けない。
    EmptyGrid { columns: u32, rows: u32 },
    /// セルの幅または高さが 0。桁数・行数を計算できない。
    ZeroCellSize { cell_width: u32, cell_height: u32 },
}

/// 文字を置く位置。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Placement {
    pub column: u32,
    pub row: u32,
    pub width_cells: u32,
}

impl Placement {
    /// 占有するセルの右端（排他）。
    pub fn end_column(&self) -> u32 {
        self.column + self.width_cells
    }
}

/// 1 文字を処理した結果、描画側がやるべきこと。
///
/// 順序が意味を持つ。`scrolled` → `clear_row` → `draw_at` の順に処理する
/// こと。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Step {
    /// 画面を 1 行分上へずらし、最終行を背景で埋める必要があるか。
    pub scrolled: bool,
    /// 消去すべき行。`\r` のときだけ `Some`。
    pub clear_row: Option<u32>,
    /// 描画位置。制御文字や、置き場所が無い場合は `None`。
    pub draw_at: Option<Placement>,
}

/// 文字セルの格子とカーソル位置。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grid {
    columns: u32,
    rows: u32,
    cursor_column: u32,
    cursor_row: u32,
}

impl Grid {
    /// 桁数・行数を直接指定して作る。
    pub fn new(columns: u32, rows: u32) -> Result<Self, GridError> {
        if columns == 0 || rows == 0 {
            return Err(GridError::EmptyGrid { columns, rows });
        }
        Ok(Self {
            columns,
            rows,
            cursor_column: 0,
            cursor_row: 0,
        })
    }

    /// 画面の大きさ（ピクセル）とセルの大きさから作る。
    pub fn from_screen(
        screen_width: u32,
        screen_height: u32,
        cell_width: u32,
        cell_height: u32,
    ) -> Result<Self, GridError> {
        if cell_width == 0 || cell_height == 0 {
            return Err(GridError::ZeroCellSize {
                cell_width,
                cell_height,
            });
        }
        Self::new(screen_width / cell_width, screen_height / cell_height)
    }

    pub fn columns(&self) -> u32 {
        self.columns
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// 現在のカーソル位置 `(column, row)`。
    pub fn cursor(&self) -> (u32, u32) {
        (self.cursor_column, self.cursor_row)
    }

    /// 1 文字を処理し、描画側がやるべきことを返す。
    ///
    /// `width_cells` はそのグリフが占めるセル数（半角なら 1、全角なら 2）。
    /// 制御文字（`\n` / `\r` / `\t`）のときは使わない。
    ///
    /// 対応する制御文字は次の 3 つだけで、それ以外の制御文字は通常の文字と
    /// 同じように扱う（呼び出し側が未収録文字として代替グリフを渡す想定）。
    ///
    /// | 文字 | 挙動 |
    /// |---|---|
    /// | `\n` | 次行の先頭へ。最終行なら 1 行スクロールする |
    /// | `\r` | 行頭へ戻り、**その行を消去する** |
    /// | `\t` | 次のタブ位置へ。行末を越えるなら折り返す |
    pub fn advance(&mut self, c: char, width_cells: u32) -> Step {
        match c {
            '\n' => Step {
                scrolled: self.break_line(),
                ..Step::default()
            },
            '\r' => {
                self.cursor_column = 0;
                Step {
                    clear_row: Some(self.cursor_row),
                    ..Step::default()
                }
            }
            '\t' => self.tab(),
            _ => self.place(width_cells),
        }
    }

    /// 明示的な改行。`advance('\n', _)` と同じ。
    pub fn newline(&mut self) -> Step {
        self.advance('\n', 0)
    }

    /// 行頭へ移動して次の行を使う。戻り値はスクロールが必要かどうか。
    ///
    /// 最終行にいる場合はカーソルを最終行に留めたままスクロールを要求する。
    /// カーソルが画面外へ出ないことを、ここで保証している。
    fn break_line(&mut self) -> bool {
        self.cursor_column = 0;
        if self.cursor_row + 1 < self.rows {
            self.cursor_row += 1;
            false
        } else {
            true
        }
    }

    fn tab(&mut self) -> Step {
        // 次のタブ位置。桁溢れしても行末判定で折り返すだけなので飽和で足りる。
        let next = (self.cursor_column / TAB_WIDTH)
            .saturating_add(1)
            .saturating_mul(TAB_WIDTH);

        if next >= self.columns {
            // 行末に達するタブは折り返しとして扱う。cursor_column が
            // columns を指す状態を作らないため。
            Step {
                scrolled: self.break_line(),
                ..Step::default()
            }
        } else {
            self.cursor_column = next;
            Step::default()
        }
    }

    fn place(&mut self, width_cells: u32) -> Step {
        if width_cells == 0 {
            // 幅 0 のグリフは存在しない想定だが、渡された場合にカーソルを
            // 進めずに無限ループさせないよう、何もしない。
            return Step::default();
        }

        let mut step = Step::default();

        if self.cursor_column + width_cells > self.columns {
            // 行末に収まらない。残ったセルは空白のまま残し、次行へ送る。
            step.scrolled = self.break_line();

            // 折り返しても収まらない（1 行より広いグリフ）。これ以上
            // 折り返しても収まらないので、この文字は描かずに捨てる。
            // 描けないまま折り返しを繰り返して進まなくなるのを防ぐ。
            if width_cells > self.columns {
                return step;
            }
        }

        step.draw_at = Some(Placement {
            column: self.cursor_column,
            row: self.cursor_row,
            width_cells,
        });
        self.cursor_column += width_cells;
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HALF: u32 = 1;
    const FULL: u32 = 2;

    /// 実機（1280x800、8x16 セル）に合わせた格子。
    fn real_grid() -> Grid {
        Grid::from_screen(1280, 800, 8, 16).unwrap()
    }

    #[test]
    fn the_real_screen_gives_160_by_50_cells() {
        let grid = real_grid();
        assert_eq!(grid.columns(), 160);
        assert_eq!(grid.rows(), 50);
    }

    #[test]
    fn a_grid_without_cells_is_rejected() {
        assert_eq!(
            Grid::new(0, 50),
            Err(GridError::EmptyGrid {
                columns: 0,
                rows: 50
            })
        );
        assert_eq!(
            Grid::new(160, 0),
            Err(GridError::EmptyGrid {
                columns: 160,
                rows: 0
            })
        );
    }

    #[test]
    fn a_zero_cell_size_is_rejected_instead_of_dividing_by_zero() {
        assert_eq!(
            Grid::from_screen(1280, 800, 0, 16),
            Err(GridError::ZeroCellSize {
                cell_width: 0,
                cell_height: 16
            })
        );
        assert_eq!(
            Grid::from_screen(1280, 800, 8, 0),
            Err(GridError::ZeroCellSize {
                cell_width: 8,
                cell_height: 0
            })
        );
    }

    #[test]
    fn a_screen_smaller_than_one_cell_is_rejected() {
        assert!(Grid::from_screen(4, 800, 8, 16).is_err());
        assert!(Grid::from_screen(1280, 8, 8, 16).is_err());
    }

    // --- 通常の桁送り ---

    #[test]
    fn half_width_characters_advance_one_cell_each() {
        let mut grid = real_grid();
        let step = grid.advance('A', HALF);
        assert_eq!(
            step.draw_at,
            Some(Placement {
                column: 0,
                row: 0,
                width_cells: 1
            })
        );
        assert_eq!(grid.cursor(), (1, 0));

        let step = grid.advance('B', HALF);
        assert_eq!(step.draw_at.unwrap().column, 1);
        assert_eq!(grid.cursor(), (2, 0));
    }

    #[test]
    fn full_width_characters_advance_two_cells_each() {
        let mut grid = real_grid();
        let step = grid.advance('あ', FULL);
        assert_eq!(
            step.draw_at,
            Some(Placement {
                column: 0,
                row: 0,
                width_cells: 2
            })
        );
        assert_eq!(grid.cursor(), (2, 0));
    }

    // --- 全角の折り返し（最重要） ---

    /// 行末に 1 セルだけ残った状態で全角が来たら、その 1 セルは空白のまま
    /// 残し、全角は次行の先頭から描く。半分だけ描いてはいけない。
    #[test]
    fn a_full_width_character_never_splits_across_the_line_end() {
        let mut grid = Grid::new(10, 5).unwrap();
        // 9 桁を半角で埋め、残り 1 セルにする。
        for _ in 0..9 {
            grid.advance('a', HALF);
        }
        assert_eq!(grid.cursor(), (9, 0));

        let step = grid.advance('あ', FULL);
        assert!(!step.scrolled);
        assert_eq!(
            step.draw_at,
            Some(Placement {
                column: 0,
                row: 1,
                width_cells: 2
            }),
            "全角は次行の先頭から描かれるべき"
        );
        assert_eq!(grid.cursor(), (2, 1));
    }

    /// 残り 2 セルちょうどなら、折り返さずにその行へ収まる。
    #[test]
    fn a_full_width_character_fits_when_exactly_two_cells_remain() {
        let mut grid = Grid::new(10, 5).unwrap();
        for _ in 0..8 {
            grid.advance('a', HALF);
        }
        assert_eq!(grid.cursor(), (8, 0));

        let step = grid.advance('あ', FULL);
        assert_eq!(
            step.draw_at,
            Some(Placement {
                column: 8,
                row: 0,
                width_cells: 2
            })
        );
        assert_eq!(grid.cursor(), (10, 0));
    }

    #[test]
    fn a_half_width_character_fits_when_one_cell_remains() {
        let mut grid = Grid::new(10, 5).unwrap();
        for _ in 0..9 {
            grid.advance('a', HALF);
        }
        let step = grid.advance('b', HALF);
        assert_eq!(step.draw_at.unwrap().column, 9);
        assert_eq!(grid.cursor(), (10, 0));
    }

    #[test]
    fn writing_past_a_full_line_wraps_to_the_next_row() {
        let mut grid = Grid::new(10, 5).unwrap();
        for _ in 0..10 {
            grid.advance('a', HALF);
        }
        assert_eq!(grid.cursor(), (10, 0));

        let step = grid.advance('b', HALF);
        assert_eq!(
            step.draw_at,
            Some(Placement {
                column: 0,
                row: 1,
                width_cells: 1
            })
        );
    }

    /// 1 行より広いグリフは、折り返しても収まらない。折り返しを繰り返して
    /// 進まなくなるのを避けるため、描かずに捨てる。
    #[test]
    fn a_glyph_wider_than_the_line_is_dropped_instead_of_looping() {
        let mut grid = Grid::new(1, 5).unwrap();
        let step = grid.advance('あ', FULL);
        assert_eq!(step.draw_at, None, "収まらない文字は描かない");
        assert_eq!(grid.cursor(), (0, 1), "カーソルは進み、停滞しない");

        // 何度繰り返しても停滞せず、いずれスクロールに到達する。
        let mut scrolls = 0;
        for _ in 0..10 {
            if grid.advance('あ', FULL).scrolled {
                scrolls += 1;
            }
        }
        assert!(scrolls > 0, "行を消費し続けて最終的にスクロールするはず");
    }

    #[test]
    fn a_zero_width_glyph_does_not_move_the_cursor() {
        let mut grid = real_grid();
        let step = grid.advance('x', 0);
        assert_eq!(step, Step::default());
        assert_eq!(grid.cursor(), (0, 0));
    }

    // --- 改行とスクロール ---

    #[test]
    fn newline_moves_to_the_start_of_the_next_row() {
        let mut grid = Grid::new(10, 5).unwrap();
        grid.advance('a', HALF);
        let step = grid.advance('\n', 0);
        assert!(!step.scrolled);
        assert_eq!(step.draw_at, None);
        assert_eq!(grid.cursor(), (0, 1));
    }

    #[test]
    fn newline_on_the_last_row_requests_a_scroll_and_stays_put() {
        let mut grid = Grid::new(10, 3).unwrap();
        grid.advance('\n', 0); // row 1
        grid.advance('\n', 0); // row 2 (最終行)
        assert_eq!(grid.cursor(), (0, 2));

        let step = grid.advance('\n', 0);
        assert!(step.scrolled);
        assert_eq!(grid.cursor(), (0, 2), "カーソルは最終行に留まる");
    }

    #[test]
    fn wrapping_on_the_last_row_also_requests_a_scroll() {
        let mut grid = Grid::new(2, 1).unwrap();
        grid.advance('a', HALF);
        grid.advance('b', HALF);
        let step = grid.advance('c', HALF);
        assert!(step.scrolled);
        assert_eq!(
            step.draw_at,
            Some(Placement {
                column: 0,
                row: 0,
                width_cells: 1
            })
        );
    }

    /// カーソルは常に画面内に留まる。スクロールを要求するだけで、行番号が
    /// 行数を超えることはない。
    #[test]
    fn the_cursor_never_leaves_the_grid() {
        let mut grid = Grid::new(4, 3).unwrap();
        for index in 0..200u32 {
            let c = if index % 7 == 0 { '\n' } else { 'a' };
            grid.advance(c, HALF);
            let (column, row) = grid.cursor();
            assert!(column <= grid.columns(), "column {column} escaped");
            assert!(row < grid.rows(), "row {row} escaped");
        }
    }

    // --- 復帰（行消去） ---

    /// `\r` は行頭へ戻すだけでなく、その行を消去する。厳密な端末挙動からの
    /// 意図的な逸脱（ADR-0017）。短い文字列で上書きしたときに、全角文字の
    /// 後半セルだけが残る不整合を構造的に避けるため。
    #[test]
    fn carriage_return_clears_the_line_it_returns_to() {
        let mut grid = Grid::new(10, 5).unwrap();
        grid.advance('\n', 0);
        grid.advance('a', HALF);
        grid.advance('b', HALF);

        let step = grid.advance('\r', 0);
        assert_eq!(step.clear_row, Some(1));
        assert!(!step.scrolled);
        assert_eq!(step.draw_at, None);
        assert_eq!(grid.cursor(), (0, 1));
    }

    #[test]
    fn carriage_return_does_not_change_the_row() {
        let mut grid = Grid::new(10, 5).unwrap();
        grid.advance('\n', 0);
        grid.advance('\n', 0);
        let (_, row_before) = grid.cursor();
        grid.advance('\r', 0);
        let (column, row_after) = grid.cursor();
        assert_eq!(row_after, row_before);
        assert_eq!(column, 0);
    }

    #[test]
    fn only_carriage_return_asks_for_a_line_clear() {
        let mut grid = real_grid();
        assert_eq!(grid.advance('a', HALF).clear_row, None);
        assert_eq!(grid.advance('\n', 0).clear_row, None);
        assert_eq!(grid.advance('\t', 0).clear_row, None);
        assert_eq!(grid.advance('\r', 0).clear_row, Some(1));
    }

    // --- タブ ---

    #[test]
    fn tab_moves_to_the_next_multiple_of_the_tab_width() {
        let mut grid = real_grid();
        grid.advance('\t', 0);
        assert_eq!(grid.cursor(), (TAB_WIDTH, 0));

        grid.advance('a', HALF);
        grid.advance('\t', 0);
        assert_eq!(grid.cursor(), (TAB_WIDTH * 2, 0));
    }

    #[test]
    fn tab_from_a_tab_stop_moves_a_full_tab_width() {
        let mut grid = real_grid();
        for _ in 0..TAB_WIDTH {
            grid.advance('a', HALF);
        }
        assert_eq!(grid.cursor(), (TAB_WIDTH, 0));
        grid.advance('\t', 0);
        assert_eq!(grid.cursor(), (TAB_WIDTH * 2, 0));
    }

    #[test]
    fn tab_never_leaves_the_cursor_at_or_past_the_line_end() {
        // 桁数がタブ幅の倍数ちょうど。最後のタブは折り返す。
        let mut grid = Grid::new(TAB_WIDTH * 2, 5).unwrap();
        for _ in 0..(TAB_WIDTH + 1) {
            grid.advance('a', HALF);
        }
        assert_eq!(grid.cursor(), (TAB_WIDTH + 1, 0));

        let step = grid.advance('\t', 0);
        assert!(!step.scrolled);
        assert_eq!(grid.cursor(), (0, 1), "行末に達するタブは折り返す");
    }

    #[test]
    fn tab_wrapping_on_the_last_row_requests_a_scroll() {
        let mut grid = Grid::new(TAB_WIDTH, 1).unwrap();
        let step = grid.advance('\t', 0);
        assert!(step.scrolled);
        assert_eq!(grid.cursor(), (0, 0));
    }

    #[test]
    fn tab_draws_nothing() {
        let mut grid = real_grid();
        assert_eq!(grid.advance('\t', 0).draw_at, None);
    }

    // --- 不変条件 ---

    /// 置かれた文字は必ず行内に収まり、同じ行で重ならない。全角セルの
    /// 途中にカーソルが入らないことは、この 2 つから導かれる。
    #[test]
    fn placements_stay_within_the_row_and_never_overlap() {
        let mut grid = Grid::new(11, 4).unwrap();
        // 半角と全角を混ぜ、折り返しが何度も起きる並びにする。
        let script = "aあbいcうdえoかkがtきmくnけ\tzこ\rxやyゆzよ";

        let mut last: Option<Placement> = None;
        for (index, c) in script.chars().enumerate() {
            let width = if c.is_ascii() { HALF } else { FULL };
            let step = grid.advance(c, width);

            if let Some(placement) = step.draw_at {
                assert!(
                    placement.end_column() <= grid.columns(),
                    "文字 {index} ({c:?}) が行からはみ出した: {placement:?}"
                );
                assert!(placement.row < grid.rows());

                if let Some(previous) = last {
                    if previous.row == placement.row {
                        assert!(
                            placement.column >= previous.end_column(),
                            "文字 {index} ({c:?}) が直前の文字と重なった: \
                             {previous:?} then {placement:?}"
                        );
                    }
                }
                last = Some(placement);
            }

            // 行が変わったら重なり判定の基準を捨てる。
            if step.scrolled || step.clear_row.is_some() {
                last = None;
            }
        }
    }

    /// カーソルは常にグリフの境界にある。全角の途中を指すことはない。
    #[test]
    fn the_cursor_always_sits_on_a_glyph_boundary() {
        let mut grid = Grid::new(9, 3).unwrap();
        // 全角だけを並べると、カーソルの桁は常に偶数になる。行頭からの
        // 積み上げで境界が保たれていることの確認。
        for _ in 0..20 {
            let step = grid.advance('あ', FULL);
            let (column, _) = grid.cursor();
            assert_eq!(column % 2, 0, "全角だけなのに奇数桁を指している");
            if let Some(placement) = step.draw_at {
                assert_eq!(placement.column % 2, 0);
            }
        }
    }
}
