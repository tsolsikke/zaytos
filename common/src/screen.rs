//! 端末の状態——セルの配列と属性とカーソル（ES-a。ADR-0040）。
//!
//! # 画面を知らない
//!
//! **ここは「次の文字をどのセルに置くか」「折り返すか」「スクロールするか」と、
//! **各セルが何の字で何色か**だけを持つ。** フレームバッファにもフォントにも
//! 依存しない純粋ロジックで、ホスト上の `cargo test` で検証する。
//! **実際に描くのは呼び出し側の責務である。**
//!
//! **`kernel/src/console/grid.rs` から移した**（ES-a）。あちらは桁送りだけを
//! 持ち、**文字は描いた先のピクセルにしか残らなかった**——**その形では
//! 「端末の状態を別の場所へ写す」ことができない**（ADR-0040 の Context）。
//!
//! # 置き場は呼び出し側が渡す
//!
//! **セルの配列を自分で持たない。** `&'a mut [Cell]` を借りる形にしてある。
//!
//! **理由は 2 つある。** **(1) 大きさをフレームバッファの実寸から導くため**
//! （ADR-0040 の Decision 3。**解像度は変わる**——GUI では FHD へ移り、
//! **ウィンドウの中の端末は「小さな画面」にすぎない**）。
//! **(2) `common` に大きな静的を持たせないため**——置き場を選ぶのは
//! カーネル側の判断で、ホストのテストは `Vec` を貸せる。
//!
//! グリフの幅（セル数）は呼び出し側が渡す。ここでフォントを引かないのは、
//! 幅の決め方（未収録文字のフォールバック等）と桁送りの規則を独立に
//! テストできるようにするため。
//!
//! 全角は 2 セルを占める。行末に 1 セルしか残っていない状態で全角が来たら、
//! その 1 セルを空白のまま残して次行の先頭から描く。半端に描かない
//! （ADR-0017）。この規則により、カーソルが全角セルの途中を指すことは
//! 構造的に起こらない。

/// セルの色（ES-a。ADR-0040 が truecolor で統一すると決めた）。
///
/// **`kernel::graphics::Color` とは別に置く。** あちらはピクセル形式への
/// 変換（`to_pixel`）を持つ描画側の型で、**こちらは端末の状態である。**
/// **変換は写す側が行う。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl Rgb {
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

/// 1 つのセル。
///
/// **属性は e-3 で使い始めた**（[`Cell::CONTINUATION`]）。太字・下線などは
/// まだ持たない。**持たせてあったのは「端末の状態とは何か」を ADR-0040 が
/// 定めたからである。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub attrs: u8,
}

impl Cell {
    /// このセルは全角の右半分である（`attrs` のビット 0。e-3）。
    ///
    /// # なぜ印が要るのか
    ///
    /// **全角の右半分は `' '` として記録される**（[`Screen::put`]）。
    /// **字だけを見ると、空白と区別が付かない。**
    ///
    /// **セルから画面を描き直す経路（代替画面バッファの復帰。ADR-0040 の
    /// Addendum）が、区別を要求する**——**印が無いと、右半分に空白を描いて
    /// 全角の右半分を消してしまう。**
    ///
    /// **描き直す側は、印の在るセルを飛ばす**（左のセルが 2 桁ぶん描く）。
    pub const CONTINUATION: u8 = 0b0000_0001;

    /// 面を `cleared_to` で丸ごと塗った後、このセルを描き直す必要があるか
    /// （PERF-h）。
    ///
    /// # なぜ飛ばせるのか
    ///
    /// **代替画面から戻るとき、[`crate::screen`] の呼び手は裏面を背景色で
    /// 丸ごと塗ってから、セルを1つずつ描き直している。** **空白のセルを
    /// 描いても、塗ったのと同じ色で同じ矩形を塗り直すだけである**——
    /// **出る絵は変わらない。**
    ///
    /// **字が空白でも、背景色が塗った色と違えば描く。** 選択や色付きの
    /// 空白がそれである。
    ///
    /// **前景色は見ない。** **空白のグリフは何も描かないので、
    /// 前景色は絵に出ない。**
    ///
    /// # なぜここに置くのか
    ///
    /// **判断は純粋である**（ハード依存が無い）。**ホストで固定する**
    /// ——`CLAUDE.md` の絶対規則の 5 つ目。
    pub fn needs_repaint_after_clear(&self, cleared_to: Rgb) -> bool {
        self.ch != ' ' || self.bg != cleared_to
    }

    /// 全角の右半分か（e-3）。
    pub const fn is_continuation(&self) -> bool {
        self.attrs & Self::CONTINUATION != 0
    }

    /// 空のセル。**色は呼び出し側の既定で塗り直される**ので、黒で作る。
    pub const fn blank() -> Self {
        Self {
            ch: ' ',
            fg: Rgb::new(0, 0, 0),
            bg: Rgb::new(0, 0, 0),
            attrs: 0,
        }
    }
}

/// タブ位置の間隔（セル数）。
pub const TAB_WIDTH: u32 = 8;

/// 1 つのグリフが占めうるセル数の上限。全角が 2 セルなので 2。
///
/// 格子はこれ以上の桁数を持つことを構築時に要求する（[`Screen::new`]）。
/// 桁数がこれを下回ると、折り返しても収まらないグリフが生じ、「収まらない
/// ので折り返す」を繰り返して前へ進めなくなるため。
///
/// フォント側の最も広いグリフがこの値を超えないことは、
/// `console` のテストで検証している。フォントに 3 セル以上のグリフを
/// 収録する場合は、この値も合わせて引き上げること。
pub const MAX_GLYPH_WIDTH_CELLS: u32 = 2;

/// [`Screen::new`] が拒否した理由。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScreenError {
    /// 桁数または行数が 0。文字を 1 つも置けない。
    EmptyGrid { columns: u32, rows: u32 },
    /// セルの幅または高さが 0。桁数・行数を計算できない。
    ZeroCellSize { cell_width: u32, cell_height: u32 },
    /// 桁数が最も広いグリフを収めるに足りない。
    /// このまま使うと折り返しても置けないグリフが生じる。
    TooNarrow { columns: u32, required: u32 },
    /// 借りたセルの置き場が `columns * rows` に足りない（ES-a）。
    ///
    /// **切り詰めない。** 足りないまま使うと、**書いたはずのセルが黙って
    /// 消える**——`zi` の上限と同じ判断である。
    CellsTooSmall { have: usize, needed: usize },
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
/// **`Copy` にしない。** 借りた置き場を持つので、写しを作ると
/// **同じセルを 2 つの `Screen` が書くことになる**（`Grid` は `Copy` だった）。
#[derive(PartialEq, Eq, Debug)]
pub struct Screen<'a> {
    columns: u32,
    rows: u32,
    cursor_column: u32,
    cursor_row: u32,
    /// セルの置き場。**呼び出し側が貸す**（モジュール doc）。
    /// 長さは `columns * rows` 以上であること（[`Screen::new`] が確かめる）。
    cells: &'a mut [Cell],
}

impl<'a> Screen<'a> {
    /// 桁数・行数を直接指定して作る。
    ///
    /// 桁数が [`MAX_GLYPH_WIDTH_CELLS`] 未満の格子は拒否する。そのような
    /// 格子では、折り返しても置けないグリフが生じてしまうため。この検証に
    /// より、[`Self::advance`] が「収まらないので折り返す」を繰り返して
    /// 前へ進めなくなる状態には到達しない。
    pub fn new(columns: u32, rows: u32, cells: &'a mut [Cell]) -> Result<Self, ScreenError> {
        if columns == 0 || rows == 0 {
            return Err(ScreenError::EmptyGrid { columns, rows });
        }
        if columns < MAX_GLYPH_WIDTH_CELLS {
            return Err(ScreenError::TooNarrow {
                columns,
                required: MAX_GLYPH_WIDTH_CELLS,
            });
        }
        // **置き場が足りるか確かめる。** 足りなければ拒む——
        // **切り詰めると、書いたはずのセルが黙って消える。**
        let needed = (columns as usize).saturating_mul(rows as usize);
        if cells.len() < needed {
            return Err(ScreenError::CellsTooSmall {
                have: cells.len(),
                needed,
            });
        }
        for cell in cells[..needed].iter_mut() {
            *cell = Cell::blank();
        }
        Ok(Self {
            columns,
            rows,
            cursor_column: 0,
            cursor_row: 0,
            cells,
        })
    }

    /// 画面の大きさ（ピクセル）とセルの大きさから作る。
    pub fn from_screen(
        screen_width: u32,
        screen_height: u32,
        cell_width: u32,
        cell_height: u32,
        cells: &'a mut [Cell],
    ) -> Result<Self, ScreenError> {
        if cell_width == 0 || cell_height == 0 {
            return Err(ScreenError::ZeroCellSize {
                cell_width,
                cell_height,
            });
        }
        Self::new(
            screen_width / cell_width,
            screen_height / cell_height,
            cells,
        )
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

    /// カーソルを指定のセルへ動かす（zi-b。ANSIのCUPが使う）。
    ///
    /// **端で切り詰める。** 画面より大きい値は右端・下端に丸まる——
    /// カーソルが画面外を指さないことの保証は`break_line`と同じで、
    /// この型が持つ。**0起点である**（1起点からの変換は呼び出し側）。
    pub fn set_cursor(&mut self, column: u32, row: u32) {
        self.cursor_column = column.min(self.columns - 1);
        self.cursor_row = row.min(self.rows - 1);
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
    /// | `\x08` | カーソルを 1 つ戻す。**行頭では戻らない** |
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
            // **後退（S12 前の手当て）。カーソルを 1 つ戻すだけである。**
            //
            // **消すのは呼び出し側である。** シェルは `\x08` のあとに空白と
            // `\x08` を送るので、**戻る・空白で塗る・また戻る**で消える。
            // ここで消去まで行うと、その 3 バイトが 2 回消すことになる。
            //
            // **行頭では戻らない。** 端末の既定と同じで、前の行の末尾へは
            // 回らない。**`\r` が行を消去する形（ADR-0017）と噛み合わせる必要が
            // 無いのが理由である**——行をまたぐと、戻った先の行が
            // 「消された行」かどうかで振る舞いが変わる。
            '\u{8}' => {
                self.cursor_column = self.cursor_column.saturating_sub(1);
                Step::default()
            }
            _ => self.place(width_cells),
        }
    }

    /// 画面を空へ戻し、カーソルを左上へ置く（ES-a）。
    ///
    /// **大きさは変えない。** 呼ぶのは `Console::clear` で、**画面を塗り直した
    /// 後に状態を合わせるためである**——片方だけを戻すと食い違う。
    pub fn reset(&mut self, bg: Rgb) {
        self.cursor_column = 0;
        self.cursor_row = 0;
        for row in 0..self.rows {
            self.clear_row(row, bg);
        }
    }

    /// セルを読む。**範囲外は `None`。**
    pub fn cell(&self, column: u32, row: u32) -> Option<&Cell> {
        self.cells.get(self.index(column, row)?)
    }

    /// セルへ字と色を置く（ES-a）。**範囲外は何もしない。**
    ///
    /// **全角は 2 セルを占める。** 後続のセルには同じ色で空白を置く——
    /// **`Grid` が「カーソルが全角の途中を指さない」を構造で保っている**ので、
    /// **後続のセルを別の字が上書きすることはない。**
    /// 描き直す側が、このセルを描くべきか（e-3）。
    ///
    /// # 純粋な述語として切り出してある
    ///
    /// **判断は「継続セル（全角の右半分）でないこと」だけである。**
    /// **描き直す側（`kernel/src/console/screen.rs` の `repaint_from_cells`）は
    /// これを呼ぶだけにしてある**——**実機では 2 桁のグリフが在るフォントが
    /// 入るまでこの分岐が通らない**ので、**判断の側だけでもホストで固定する。**
    ///
    /// **範囲外は「描かない」である。**
    pub fn should_draw(&self, column: u32, row: u32) -> bool {
        self.cell(column, row)
            .is_some_and(|cell| !cell.is_continuation())
    }

    pub fn put(&mut self, column: u32, row: u32, ch: char, fg: Rgb, bg: Rgb, width_cells: u32) {
        let Some(at) = self.index(column, row) else {
            return;
        };
        if let Some(cell) = self.cells.get_mut(at) {
            *cell = Cell {
                ch,
                fg,
                bg,
                attrs: 0,
            };
        }
        for extra in 1..width_cells {
            let Some(at) = self.index(column + extra, row) else {
                break;
            };
            if let Some(cell) = self.cells.get_mut(at) {
                // **右半分だと印を付ける（e-3）。** **字は `' '` のままである**
                // ——**印が無いと、描き直す側が空白を描いて右半分を消す**
                // （[`Cell::CONTINUATION`]）。
                *cell = Cell {
                    ch: ' ',
                    fg,
                    bg,
                    attrs: Cell::CONTINUATION,
                };
            }
        }
    }

    /// 1 行を空白で埋める（ES-a）。`\r` の行消去とスクロール後の最終行が使う。
    pub fn clear_row(&mut self, row: u32, bg: Rgb) {
        for column in 0..self.columns {
            self.put(column, row, ' ', bg, bg, 1);
        }
    }

    /// 1 行ぶん上へずらし、最終行を空白で埋める（ES-a）。
    ///
    /// **`Console` の `scroll_up` と対である**——あちらはピクセルを動かし、
    /// こちらはセルを動かす。**両方が同じ規則で動くことが振る舞い不変の
    /// 条件である。**
    pub fn scroll_up(&mut self, bg: Rgb) {
        // **`DL` の 1 行と同じ規則である（PERF-e）。** **道を 2 本持たない**
        // ——**片方だけが直る形を作らない**（この体制の規律）。
        self.delete_lines(0, 1, bg);
    }

    /// `row` から下の行を `count` 行ぶん上へ詰める（`DL`。PERF-d）。
    ///
    /// # 何のために在るのか
    ///
    /// **`less`のような全画面のアプリが、1行ずらすためである。**
    /// **いままでは全画面を描き直していた**——**窓が1行動くと49行すべての
    /// 中身が変わるので、「変わった行だけ描く」では減らない**（実測。PERF-c）。
    /// **画面をずらせば、描き直すのは新しく現れた1行だけになる。**
    ///
    /// **空いた下端は背景で埋める。** **`row` が画面の外なら何もしない。**
    pub fn delete_lines(&mut self, row: u32, count: u32, bg: Rgb) {
        if row >= self.rows || count == 0 {
            return;
        }
        let width = self.columns as usize;
        let count = count.min(self.rows - row);
        let first = (row as usize) * width;
        let from = ((row + count) as usize) * width;
        let end = (self.rows as usize) * width;
        if from < end {
            self.cells.copy_within(from..end, first);
        }
        for blank in (self.rows - count)..self.rows {
            self.clear_row(blank, bg);
        }
    }

    /// `row` から下の行を `count` 行ぶん下へずらす（`IL`。PERF-d）。
    ///
    /// **空いた `row` から `count` 行を背景で埋める。**
    /// **押し出された下端は消える**（端末の慣行どおりである）。
    pub fn insert_lines(&mut self, row: u32, count: u32, bg: Rgb) {
        if row >= self.rows || count == 0 {
            return;
        }
        let width = self.columns as usize;
        let count = count.min(self.rows - row);
        let first = (row as usize) * width;
        let to = ((row + count) as usize) * width;
        let end = (self.rows as usize) * width;
        if to < end {
            self.cells.copy_within(first..end - (to - first), to);
        }
        for blank in row..(row + count) {
            self.clear_row(blank, bg);
        }
    }

    /// セルの添字。**範囲外は `None`。**
    fn index(&self, column: u32, row: u32) -> Option<usize> {
        if column >= self.columns || row >= self.rows {
            return None;
        }
        Some((row as usize) * (self.columns as usize) + column as usize)
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

            // 折り返しても収まらないグリフ。防御的な措置であり、通常は
            // 到達しない: 構築時に桁数 >= MAX_GLYPH_WIDTH_CELLS を検証して
            // いるため、契約どおりの width_cells なら必ず収まる。
            // 契約を破る値が渡された場合にのみここへ来る。
            //
            // 破棄を選ぶのは、ここで停止すると「収まらないので折り返す」を
            // 繰り返して前へ進めなくなるため。文字は失われるが、コンソールが
            // 進行不能になるよりはよい。
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
mod line_shift_tests {
    use super::{Cell, Rgb, Screen};

    fn screen(cells: &mut [Cell], columns: u32, rows: u32) -> Screen<'_> {
        Screen::new(columns, rows, cells).expect("形が正しいこと")
    }

    fn put_marks(screen: &mut Screen<'_>, rows: u32) {
        let ink = Rgb::new(200, 200, 200);
        for row in 0..rows {
            screen.put(0, row, (b'a' + row as u8) as char, ink, ink, 1);
        }
    }

    fn row_char(screen: &Screen<'_>, row: u32) -> char {
        screen.cell(0, row).map(|cell| cell.ch).unwrap_or('?')
    }

    #[test]
    fn deleting_a_line_pulls_the_rest_up_and_blanks_the_bottom() {
        let mut cells = [Cell::blank(); 4 * 4];
        let mut screen = screen(&mut cells, 4, 4);
        put_marks(&mut screen, 4);
        screen.delete_lines(0, 1, Rgb::new(0, 0, 0));
        assert_eq!(row_char(&screen, 0), 'b');
        assert_eq!(row_char(&screen, 2), 'd');
        assert_eq!(row_char(&screen, 3), ' ', "下端は空になる");
    }

    #[test]
    fn inserting_a_line_pushes_the_rest_down_and_blanks_it() {
        let mut cells = [Cell::blank(); 4 * 4];
        let mut screen = screen(&mut cells, 4, 4);
        put_marks(&mut screen, 4);
        screen.insert_lines(0, 1, Rgb::new(0, 0, 0));
        assert_eq!(row_char(&screen, 0), ' ', "空いた行は空になる");
        assert_eq!(row_char(&screen, 1), 'a');
        assert_eq!(row_char(&screen, 3), 'c', "下端は押し出されて消える");
    }

    #[test]
    fn shifting_from_the_middle_leaves_the_rows_above_alone() {
        let mut cells = [Cell::blank(); 4 * 4];
        let mut screen = screen(&mut cells, 4, 4);
        put_marks(&mut screen, 4);
        screen.delete_lines(2, 1, Rgb::new(0, 0, 0));
        assert_eq!(row_char(&screen, 0), 'a');
        assert_eq!(row_char(&screen, 1), 'b');
        assert_eq!(row_char(&screen, 2), 'd');
        assert_eq!(row_char(&screen, 3), ' ');
    }

    #[test]
    fn a_count_past_the_bottom_clears_the_rest() {
        let mut cells = [Cell::blank(); 4 * 4];
        let mut screen = screen(&mut cells, 4, 4);
        put_marks(&mut screen, 4);
        screen.delete_lines(1, 99, Rgb::new(0, 0, 0));
        assert_eq!(row_char(&screen, 0), 'a');
        for row in 1..4 {
            assert_eq!(row_char(&screen, row), ' ');
        }
    }

    #[test]
    fn a_row_past_the_bottom_does_nothing() {
        let mut cells = [Cell::blank(); 4 * 4];
        let mut screen = screen(&mut cells, 4, 4);
        put_marks(&mut screen, 4);
        screen.delete_lines(9, 1, Rgb::new(0, 0, 0));
        assert_eq!(row_char(&screen, 0), 'a');
        assert_eq!(row_char(&screen, 3), 'd');
    }
}

#[cfg(test)]
mod tests {

    /// **塗った後に描き直すセルの見分け（PERF-h）。**
    ///
    /// **主張は 2 つあり、飛ばさない側が主である。**
    ///
    /// - **空白で、背景が塗った色と同じなら飛ばす**（絵が変わらない）
    /// - **それ以外は飛ばさない**——**字が在る / 背景が違う。**
    ///   **こちらが要る**——**飛ばしすぎると絵が欠けるが、
    ///   欠けたことは費用の計器には出ない。**
    #[test]
    fn only_blank_cells_on_the_cleared_colour_are_skipped() {
        let cleared = Rgb::new(0, 0, 0);
        let other = Rgb::new(9, 9, 9);
        let fg = Rgb::new(1, 2, 3);
        let cell = |ch, bg| Cell {
            ch,
            fg,
            bg,
            attrs: 0,
        };

        assert!(!cell(' ', cleared).needs_repaint_after_clear(cleared));
        assert!(cell('a', cleared).needs_repaint_after_clear(cleared));
        assert!(cell(' ', other).needs_repaint_after_clear(cleared));
        assert!(cell('a', other).needs_repaint_after_clear(cleared));

        // **前景色は見ない。** **空白のグリフは何も描かない。**
        let mut bright = cell(' ', cleared);
        bright.fg = Rgb::new(255, 255, 255);
        assert!(!bright.needs_repaint_after_clear(cleared));
    }

    /// **全角の右半分に印が付く（e-3）。**
    ///
    /// **左のセルには付かない。** 描き直す側は、印の在るセルを飛ばして
    /// 左のセルに 2 桁ぶんを描く（`Cell::CONTINUATION` の doc）。
    #[test]
    fn a_wide_glyph_marks_its_right_half() {
        let mut cells = [Cell::blank(); 8];
        let mut screen = Screen::new(4, 2, &mut cells).expect("4x2 fits in 8 cells");
        let fg = Rgb::new(1, 2, 3);
        let bg = Rgb::new(4, 5, 6);
        screen.put(0, 0, '日', fg, bg, 2);
        let left = screen.cell(0, 0).expect("in range");
        let right = screen.cell(1, 0).expect("in range");
        assert_eq!(left.ch, '日');
        assert!(!left.is_continuation(), "左のセルは右半分ではない");
        assert_eq!(right.ch, ' ');
        assert!(right.is_continuation(), "右半分には印が付く");
        // **半角には付かない。**
        screen.put(2, 0, 'a', fg, bg, 1);
        assert!(!screen.cell(2, 0).expect("in range").is_continuation());
        assert!(!screen.cell(3, 0).expect("in range").is_continuation());
    }

    /// **描き直す側が飛ばすセルを、述語で固定する（e-3）。**
    ///
    /// **実機ではこの分岐が通らない**（フォントに 2 桁のグリフが無い）ので、
    /// **判断の側だけでもここで固定する。**
    #[test]
    fn the_repaint_predicate_skips_only_the_right_half() {
        let mut cells = [Cell::blank(); 8];
        let mut screen = Screen::new(4, 2, &mut cells).expect("4x2 fits in 8 cells");
        let fg = Rgb::new(1, 2, 3);
        let bg = Rgb::new(4, 5, 6);
        screen.put(0, 0, '日', fg, bg, 2);
        screen.put(2, 0, 'a', fg, bg, 1);
        assert!(screen.should_draw(0, 0), "全角の左は描く");
        assert!(!screen.should_draw(1, 0), "全角の右半分は飛ばす");
        assert!(screen.should_draw(2, 0), "半角は描く");
        assert!(screen.should_draw(3, 0), "空のセルも描く（背景で塗る）");
        assert!(!screen.should_draw(4, 0), "範囲外は描かない");
        assert!(!screen.should_draw(0, 9), "範囲外は描かない");
    }
    /// 試験用のセルの置き場。**`Vec` を貸す**（モジュール doc の「置き場は
    /// 呼び出し側が渡す」）。**ホストのテストだけがこれを使う。**
    fn cells() -> std::vec::Vec<Cell> {
        std::vec![Cell::blank(); 256 * 128]
    }

    /// **CUPの切り詰め（zi-b）。** 画面内はそのまま、画面外は端に丸まる。
    #[test]
    fn set_cursor_clamps_to_the_grid() {
        let mut c = cells();
        let mut grid = super::Screen::new(10, 5, &mut c).unwrap();
        grid.set_cursor(3, 2);
        assert_eq!(grid.cursor(), (3, 2));
        grid.set_cursor(99, 99);
        assert_eq!(grid.cursor(), (9, 4), "右端・下端に丸まる");
        grid.set_cursor(0, 0);
        assert_eq!(grid.cursor(), (0, 0));
    }

    use super::*;

    const HALF: u32 = 1;
    const FULL: u32 = 2;

    /// 実機（1280x800、8x16 セル）に合わせた格子。
    ///
    /// **置き場は呼び出し側が貸す**ので、借りる形で受け取る。
    fn real_grid(cells: &mut [Cell]) -> Screen<'_> {
        Screen::from_screen(1280, 800, 8, 16, cells).unwrap()
    }

    #[test]
    fn the_real_screen_gives_160_by_50_cells() {
        let mut c = cells();
        let grid = real_grid(&mut c);
        assert_eq!(grid.columns(), 160);
        assert_eq!(grid.rows(), 50);
    }

    #[test]
    fn a_grid_without_cells_is_rejected() {
        let mut c = cells();
        assert_eq!(
            Screen::new(0, 50, &mut c),
            Err(ScreenError::EmptyGrid {
                columns: 0,
                rows: 50
            })
        );
        assert_eq!(
            Screen::new(160, 0, &mut c),
            Err(ScreenError::EmptyGrid {
                columns: 160,
                rows: 0
            })
        );
    }

    #[test]
    fn a_zero_cell_size_is_rejected_instead_of_dividing_by_zero() {
        let mut c = cells();
        assert_eq!(
            Screen::from_screen(1280, 800, 0, 16, &mut c),
            Err(ScreenError::ZeroCellSize {
                cell_width: 0,
                cell_height: 16
            })
        );
        assert_eq!(
            Screen::from_screen(1280, 800, 8, 0, &mut c),
            Err(ScreenError::ZeroCellSize {
                cell_width: 8,
                cell_height: 0
            })
        );
    }

    #[test]
    fn a_screen_smaller_than_one_cell_is_rejected() {
        let mut c = cells();
        assert!(Screen::from_screen(4, 800, 8, 16, &mut c).is_err());
        assert!(Screen::from_screen(1280, 8, 8, 16, &mut c).is_err());
    }

    // --- 通常の桁送り ---

    #[test]
    fn half_width_characters_advance_one_cell_each() {
        let mut c = cells();
        let mut grid = real_grid(&mut c);
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
        let mut c = cells();
        let mut grid = real_grid(&mut c);
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
        let mut c = cells();
        let mut grid = Screen::new(10, 5, &mut c).unwrap();
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
        let mut c = cells();
        let mut grid = Screen::new(10, 5, &mut c).unwrap();
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
        let mut c = cells();
        let mut grid = Screen::new(10, 5, &mut c).unwrap();
        for _ in 0..9 {
            grid.advance('a', HALF);
        }
        let step = grid.advance('b', HALF);
        assert_eq!(step.draw_at.unwrap().column, 9);
        assert_eq!(grid.cursor(), (10, 0));
    }

    #[test]
    fn writing_past_a_full_line_wraps_to_the_next_row() {
        let mut c = cells();
        let mut grid = Screen::new(10, 5, &mut c).unwrap();
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

    /// 最も広いグリフを収められない格子は、そもそも作らせない。これにより
    /// 「折り返しても置けない」状態には到達しなくなる。
    #[test]
    fn a_grid_too_narrow_for_the_widest_glyph_is_rejected() {
        let mut c = cells();
        assert_eq!(
            Screen::new(1, 5, &mut c),
            Err(ScreenError::TooNarrow {
                columns: 1,
                required: MAX_GLYPH_WIDTH_CELLS
            })
        );
        // 最も広いグリフがちょうど収まる幅なら通る。
        assert!(Screen::new(MAX_GLYPH_WIDTH_CELLS, 5, &mut c).is_ok());
    }

    #[test]
    fn a_screen_too_narrow_for_the_widest_glyph_is_rejected() {
        let mut c = cells();
        // 8px セルで 15px 幅 = 1 桁。全角が置けないので拒否される。
        assert_eq!(
            Screen::from_screen(15, 800, 8, 16, &mut c),
            Err(ScreenError::TooNarrow {
                columns: 1,
                required: MAX_GLYPH_WIDTH_CELLS
            })
        );
    }

    /// 構築時の検証を通っていれば到達しないが、契約を破る `width_cells` が
    /// 渡された場合の防御的な破棄。停止させず、カーソルを進めて先へ進む。
    #[test]
    fn a_width_breaking_the_contract_is_dropped_instead_of_looping() {
        let mut c = cells();
        let mut grid = Screen::new(2, 5, &mut c).unwrap();
        let step = grid.advance('x', 5);
        assert_eq!(step.draw_at, None, "収まらない文字は描かない");
        assert_eq!(grid.cursor(), (0, 1), "カーソルは進み、停滞しない");

        // 何度繰り返しても停滞せず、いずれスクロールに到達する。
        let mut scrolls = 0;
        for _ in 0..10 {
            if grid.advance('x', 5).scrolled {
                scrolls += 1;
            }
        }
        assert!(scrolls > 0, "行を消費し続けて最終的にスクロールするはず");
    }

    #[test]
    fn a_zero_width_glyph_does_not_move_the_cursor() {
        let mut c = cells();
        let mut grid = real_grid(&mut c);
        let step = grid.advance('x', 0);
        assert_eq!(step, Step::default());
        assert_eq!(grid.cursor(), (0, 0));
    }

    // --- 改行とスクロール ---

    #[test]
    fn newline_moves_to_the_start_of_the_next_row() {
        let mut c = cells();
        let mut grid = Screen::new(10, 5, &mut c).unwrap();
        grid.advance('a', HALF);
        let step = grid.advance('\n', 0);
        assert!(!step.scrolled);
        assert_eq!(step.draw_at, None);
        assert_eq!(grid.cursor(), (0, 1));
    }

    #[test]
    fn newline_on_the_last_row_requests_a_scroll_and_stays_put() {
        let mut c = cells();
        let mut grid = Screen::new(10, 3, &mut c).unwrap();
        grid.advance('\n', 0); // row 1
        grid.advance('\n', 0); // row 2 (最終行)
        assert_eq!(grid.cursor(), (0, 2));

        let step = grid.advance('\n', 0);
        assert!(step.scrolled);
        assert_eq!(grid.cursor(), (0, 2), "カーソルは最終行に留まる");
    }

    #[test]
    fn wrapping_on_the_last_row_also_requests_a_scroll() {
        let mut c = cells();
        let mut grid = Screen::new(2, 1, &mut c).unwrap();
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
        let mut c = cells();
        let mut grid = Screen::new(4, 3, &mut c).unwrap();
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
        let mut c = cells();
        let mut grid = Screen::new(10, 5, &mut c).unwrap();
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
        let mut c = cells();
        let mut grid = Screen::new(10, 5, &mut c).unwrap();
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
        let mut c = cells();
        let mut grid = real_grid(&mut c);
        assert_eq!(grid.advance('a', HALF).clear_row, None);
        assert_eq!(grid.advance('\n', 0).clear_row, None);
        assert_eq!(grid.advance('\t', 0).clear_row, None);
        assert_eq!(grid.advance('\r', 0).clear_row, Some(1));
    }

    // --- タブ ---

    #[test]
    fn tab_moves_to_the_next_multiple_of_the_tab_width() {
        let mut c = cells();
        let mut grid = real_grid(&mut c);
        grid.advance('\t', 0);
        assert_eq!(grid.cursor(), (TAB_WIDTH, 0));

        grid.advance('a', HALF);
        grid.advance('\t', 0);
        assert_eq!(grid.cursor(), (TAB_WIDTH * 2, 0));
    }

    #[test]
    fn tab_from_a_tab_stop_moves_a_full_tab_width() {
        let mut c = cells();
        let mut grid = real_grid(&mut c);
        for _ in 0..TAB_WIDTH {
            grid.advance('a', HALF);
        }
        assert_eq!(grid.cursor(), (TAB_WIDTH, 0));
        grid.advance('\t', 0);
        assert_eq!(grid.cursor(), (TAB_WIDTH * 2, 0));
    }

    #[test]
    fn tab_never_leaves_the_cursor_at_or_past_the_line_end() {
        let mut c = cells();
        // 桁数がタブ幅の倍数ちょうど。最後のタブは折り返す。
        let mut grid = Screen::new(TAB_WIDTH * 2, 5, &mut c).unwrap();
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
        let mut c = cells();
        let mut grid = Screen::new(TAB_WIDTH, 1, &mut c).unwrap();
        let step = grid.advance('\t', 0);
        assert!(step.scrolled);
        assert_eq!(grid.cursor(), (0, 0));
    }

    #[test]
    fn tab_draws_nothing() {
        let mut c = cells();
        let mut grid = real_grid(&mut c);
        assert_eq!(grid.advance('\t', 0).draw_at, None);
    }

    // --- 不変条件 ---

    /// 置かれた文字は必ず行内に収まり、同じ行で重ならない。全角セルの
    /// 途中にカーソルが入らないことは、この 2 つから導かれる。
    #[test]
    fn placements_stay_within_the_row_and_never_overlap() {
        let mut c = cells();
        let mut grid = Screen::new(11, 4, &mut c).unwrap();
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
        let mut c = cells();
        let mut grid = Screen::new(9, 3, &mut c).unwrap();
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
