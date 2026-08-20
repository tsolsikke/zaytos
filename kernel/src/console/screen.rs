//! 画面コンソール本体（M3-c-2）。
//!
//! [`super::grid`] の桁送り、[`super::dirty`] の未転送範囲、
//! [`super::backbuffer`] のバックバッファを束ね、`core::fmt::Write` として
//! 使えるようにする。
//!
//! 転送はキャッシュ無効なフレームバッファへの書き込みで最も高い工程なので、
//! 変更範囲だけを送る。フラッシュ点は `write_fmt` 1 回ごと、つまり
//! `write!` / `writeln!` の呼び出し 1 回ごと（ADR-0017）。改行ごとに
//! しないのは、改行を含まない `write!` が画面に出ないと、ハングしたときに
//! 書きかけの行が見えなくなるため。
//!
//! グローバルには置かず、ロックも持たない。割り込みハンドラから出力する
//! 要求は M4 で生じるため、そのロック設計は割り込み安全性のモデルと一体で
//! 決める。

use common::addr::VirtAddr;
use core::fmt;

use common::cpu;
use common::serial::SerialPort;

use crate::graphics::font;
use crate::graphics::{Color, Framebuffer};

use super::backbuffer::{BackBuffer, BackBufferError, FlushRangeError};
use super::dirty::DirtyRegion;
use common::screen::{Cell, Rgb, Screen, ScreenError};

/// [`Console::new`] が拒否した理由。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConsoleError {
    BackBuffer(BackBufferError),
    Screen(ScreenError),
}

/// 転送量の記録。ダーティ矩形が実際に効いているかを数字で確かめるためのもの。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FlushStats {
    /// 実際に転送を行った回数（送るものが無かった場合は数えない）。
    pub flush_count: u64,
    /// 転送した総バイト数。
    pub transferred_bytes: u64,
    /// 全面転送 1 回分のバイト数。
    pub full_screen_bytes: u64,
    /// 転送に費やした TSC サイクルの合計（S12 前の手当て）。
    ///
    /// **時刻ではなく、回る量の目安である**（`common::cpu::read_timestamp_counter`）。
    /// **測る理由は、転送が BKL を保持している区間へ入るかを判断するためである**
    /// ——`sys_write` は BKL の内側で走る（`kernel/src/syscall.rs`）。
    pub flush_cycles_total: u64,
    /// 1 回の転送に費やした TSC サイクルの最大。
    pub flush_cycles_max: u64,
    /// そのうち全面転送だった回数。**スクロールで `mark_all` が呼ばれた回である。**
    pub full_screen_flush_count: u64,
    /// 全面転送 1 回の TSC サイクルの最大。**部分転送と桁が違うはずなので分けて持つ。**
    pub full_screen_cycles_max: u64,
    /// 1 行を書く経路（描画 + 転送）の TSC サイクルの合計。
    ///
    /// **転送だけでは足りない。** `write!` 1 回は、字を描いてから転送する
    /// （[`Console::put_char`] がバックバッファへ書き、[`Console::flush`] が送る）。
    /// **BKL の内側へ入るのはこの経路の全体である。**
    pub write_cycles_total: u64,
    /// 1 行を書く経路の TSC サイクルの最大。
    pub write_cycles_max: u64,
    /// その経路を通った回数（`write!` / `writeln!` の回数）。
    pub write_count: u64,
}

impl FlushStats {
    /// 毎回全面転送していた場合に転送されたはずのバイト数。
    pub fn full_screen_equivalent_bytes(&self) -> u64 {
        self.flush_count * self.full_screen_bytes
    }

    /// 全面転送に対して実際に送った割合（百分率）。
    ///
    /// 100 に近ければダーティ矩形が効いておらず、毎回ほぼ全面を送っている。
    pub fn transferred_percent(&self) -> u64 {
        let baseline = self.full_screen_equivalent_bytes();
        if baseline == 0 {
            return 0;
        }
        self.transferred_bytes * 100 / baseline
    }
}

/// 画面コンソール。
pub struct Console {
    front: Framebuffer,
    back: BackBuffer,
    grid: Screen<'static>,
    dirty: DirtyRegion,
    foreground: Color,
    background: Color,
    stats: FlushStats,
}

impl Console {
    /// コンソールを作り、画面全体を背景色で塗る。
    ///
    /// フレームアロケータが返すフレームには前の内容が残っており、ヒープの
    /// 毒値が書かれた領域が回ってくることもある。ここで塗り潰してから
    /// 最初の転送を行うため、起動直後にノイズが出ることはない。
    ///
    /// # Safety
    ///
    /// 呼び出し側は以下を保証すること。
    ///
    /// - `back_buffer_base` から `framebuffer` の `size_bytes` バイトが、
    ///   現在のページテーブルで読み書き可能にマップされていること
    ///   （`MappedRanges::contains_range` で確認してから渡すこと）。
    /// - その領域を他の誰も使っていないこと。
    /// - `framebuffer` が指す領域も同様にマップ済みで、他の誰も使って
    ///   いないこと（`Framebuffer::new` の契約）。
    pub unsafe fn new(
        framebuffer: Framebuffer,
        back_buffer_base: VirtAddr,
        foreground: Color,
        background: Color,
        cells: &'static mut [Cell],
    ) -> Result<Self, ConsoleError> {
        let layout = *framebuffer.layout();

        // SAFETY: 呼び出し側の契約をそのまま引き継ぐ。
        let mut back = unsafe { BackBuffer::new(back_buffer_base, &layout) }
            .map_err(ConsoleError::BackBuffer)?;

        // **端末の状態は `Screen` が持つ（ES-a。ADR-0040）。**
        // **置き場は呼び出し側が渡す**——大きさをフレームバッファの実寸から
        // 導くためで、**`Screen` 自身は画面の大きさを知らない。**
        let grid = Screen::from_screen(
            layout.width(),
            layout.height(),
            font::CELL_WIDTH,
            font::GLYPH_HEIGHT,
            cells,
        )
        .map_err(ConsoleError::Screen)?;

        // 最初の転送より前に、確保したばかりの領域を必ず塗り潰す。
        back.clear_all(background);

        let mut dirty = DirtyRegion::new(layout.width(), layout.height());
        dirty.mark_all();

        let stats = FlushStats {
            full_screen_bytes: (layout.width() as u64)
                * (layout.height() as u64)
                * (crate::graphics::layout::BYTES_PER_PIXEL),
            ..FlushStats::default()
        };

        let mut console = Self {
            front: framebuffer,
            back,
            grid,
            dirty,
            foreground,
            background,
            stats,
        };
        // 画面に残っている前の内容（ファームウェアの表示など）を消しておく。
        console.flush();
        Ok(console)
    }

    /// 描画側の [`Color`] を端末の状態の [`Rgb`] へ写す（ES-a）。
    ///
    /// **型を分けてあるのは、片方が描画側でもう片方が状態だからである**
    /// （`common::screen::Rgb` の doc）。
    const fn rgb(color: Color) -> Rgb {
        Rgb::new(color.red, color.green, color.blue)
    }

    /// フレームバッファの形状。計測や診断で参照する。
    pub fn framebuffer_layout(&self) -> &crate::graphics::FramebufferLayout {
        self.front.layout()
    }

    pub fn stats(&self) -> FlushStats {
        self.stats
    }

    /// 桁数と行数。
    pub fn size(&self) -> (u32, u32) {
        (self.grid.columns(), self.grid.rows())
    }

    pub fn set_foreground(&mut self, color: Color) {
        self.foreground = color;
    }

    /// カーソルのセル位置 `(column, row)`（zi-b の判定行が読む）。
    pub fn cursor_cell(&self) -> (u32, u32) {
        self.grid.cursor()
    }

    /// カーソルをセルへ動かす（zi-b。ANSI の CUP が使う）。
    ///
    /// **0 起点である。** 1 起点からの変換は呼び出し側（`console::mod` の
    /// 前景経路）が行う。端の切り詰めは [`Grid::set_cursor`] が持つ。
    pub fn cursor_to_cell(&mut self, column: u32, row: u32) {
        self.grid.set_cursor(column, row);
    }

    /// 行のセル範囲 `[from, to)` を背景で塗る（zi-b の消去の部品）。
    fn erase_cells(&mut self, row: u32, from: u32, to: u32) {
        if from >= to {
            return;
        }
        let x = from * font::CELL_WIDTH;
        let y = row * font::GLYPH_HEIGHT;
        let width = (to - from) * font::CELL_WIDTH;
        self.back
            .surface_mut()
            .fill_rect(x, y, width, font::GLYPH_HEIGHT, self.background);
        self.dirty.mark(x, y, width, font::GLYPH_HEIGHT);
        // **セルも消す（ES-a）。**
        let background = Self::rgb(self.background);
        for column in from..to {
            self.grid.put(column, row, ' ', background, background, 1);
        }
    }

    /// 行消去（EL。zi-b）。**カーソルは動かさない。**
    ///
    /// 範囲は ANSI の規約どおり——`After` と `Before` はどちらもカーソルの
    /// セルを含む。
    pub fn erase_in_line(&mut self, scope: common::ansi::EraseScope) {
        use common::ansi::EraseScope;
        let (column, row) = self.grid.cursor();
        let columns = self.grid.columns();
        match scope {
            EraseScope::After => self.erase_cells(row, column, columns),
            EraseScope::Before => self.erase_cells(row, 0, column + 1),
            EraseScope::All => self.erase_cells(row, 0, columns),
        }
    }

    /// 画面消去（ED。zi-b）。**カーソルは動かさない**——ここが [`Self::clear`]
    /// との違いである（ANSI の ED はカーソルを移さない。`zi` は ED の後に
    /// CUP を送る）。
    pub fn erase_in_display(&mut self, scope: common::ansi::EraseScope) {
        use common::ansi::EraseScope;
        let (column, row) = self.grid.cursor();
        let (columns, rows) = (self.grid.columns(), self.grid.rows());
        match scope {
            EraseScope::After => {
                self.erase_cells(row, column, columns);
                for below in row + 1..rows {
                    self.erase_cells(below, 0, columns);
                }
            }
            EraseScope::Before => {
                for above in 0..row {
                    self.erase_cells(above, 0, columns);
                }
                self.erase_cells(row, 0, column + 1);
            }
            EraseScope::All => {
                self.back.clear_all(self.background);
                self.dirty.mark_all();
            }
        }
    }

    /// セルの中に背景色でないピクセルが在るか（zi-b の判定用）。
    ///
    /// **バックバッファを読む**ので、転送（flush）前でも判定できる。
    /// 画面外のセルは「無い」。
    pub fn cell_has_ink(&mut self, column: u32, row: u32) -> bool {
        let expected = self.background.to_pixel(self.back.layout().format());
        let x = column * font::CELL_WIDTH;
        let y = row * font::GLYPH_HEIGHT;
        for dy in 0..font::GLYPH_HEIGHT {
            for dx in 0..font::CELL_WIDTH {
                let pixel = self.back.surface_mut().read_pixel_raw(x + dx, y + dy);
                if pixel.is_some_and(|raw| raw != expected) {
                    return true;
                }
            }
        }
        false
    }

    /// 画面全体を背景色に戻し、カーソルを左上へ移す。
    pub fn clear(&mut self) {
        self.back.clear_all(self.background);
        self.dirty.mark_all();
        // **セルも空へ戻す（ES-a）。** 画面を塗り直したので、
        // **状態とピクセルを食い違わせない。**
        self.grid.reset(Self::rgb(self.background));
    }

    /// 未転送の範囲をフレームバッファへ送る。
    ///
    /// 送るものが無ければフレームバッファには一切触れない。
    ///
    /// 転送直前の範囲検証に失敗した場合は、内訳をシリアルへ出して停止する。
    /// **到達しないはずの経路だが、到達した場合は必ず観測できるようにして
    /// ある**（[`BackBuffer::flush_rect`] のコメント参照）。無言でその行を
    /// 飛ばすと画面の一部が欠けるだけで原因に気づけないため、握りつぶさない
    /// （ADR-0004）。フラッシュはパニック経路から呼ばれないので、ここで
    /// 停止しても再帰の懸念はない。
    pub fn flush(&mut self) {
        let Some(rect) = self.dirty.take() else {
            return;
        };
        let started = common::cpu::read_timestamp_counter();
        match self.back.flush_rect(&mut self.front, rect) {
            Ok(transferred) => {
                let elapsed = common::cpu::read_timestamp_counter().wrapping_sub(started);
                self.stats.flush_count += 1;
                self.stats.transferred_bytes += transferred;
                self.stats.flush_cycles_total += elapsed;
                self.stats.flush_cycles_max = self.stats.flush_cycles_max.max(elapsed);
                // **全面かどうかは転送量で分かる。** `mark_all` が呼ばれた回である。
                if transferred == self.stats.full_screen_bytes {
                    self.stats.full_screen_flush_count += 1;
                    self.stats.full_screen_cycles_max =
                        self.stats.full_screen_cycles_max.max(elapsed);
                }
            }
            Err(error) => report_flush_failure_and_halt(&error),
        }
    }

    /// 1 文字書く。転送はしない。
    pub fn put_char(&mut self, c: char) {
        let glyph = font::glyph(c);
        let step = self.grid.advance(c, glyph.width_cells());

        // Step の順序どおりに処理する: スクロール → 行消去 → 描画。
        if step.scrolled {
            self.back.scroll_up(font::GLYPH_HEIGHT, self.background);
            self.dirty.mark_all();
            // **セルも同じ規則で動かす（ES-a）。** 片方だけを動かすと、
            // **状態と画面が食い違う。**
            self.grid.scroll_up(Self::rgb(self.background));
        }

        if let Some(row) = step.clear_row {
            let top = row * font::GLYPH_HEIGHT;
            self.back
                .clear_rows(top, font::GLYPH_HEIGHT, self.background);
            self.dirty
                .mark(0, top, self.back.layout().width(), font::GLYPH_HEIGHT);
            self.grid.clear_row(row, Self::rgb(self.background));
        }

        if let Some(placement) = step.draw_at {
            // **セルへ記録してから描く（ES-a。ADR-0040）。**
            // **状態が先で、描画はその写しである**——ES-b 以降で
            // 「`Screen` を矩形へ写す」形へ寄せるための順序である。
            let (foreground, background) = (self.foreground, self.background);
            self.grid.put(
                placement.column,
                placement.row,
                c,
                Self::rgb(foreground),
                Self::rgb(background),
                placement.width_cells,
            );
            let x = placement.column * font::CELL_WIDTH;
            let y = placement.row * font::GLYPH_HEIGHT;
            self.back
                .surface_mut()
                .draw_glyph(x, y, glyph, foreground, Some(background));
            self.dirty
                .mark(x, y, glyph.width_pixels(), glyph.height_pixels());
        }
    }

    /// 文字列を書く。転送はしない。
    pub fn put_str(&mut self, text: &str) {
        for c in text.chars() {
            self.put_char(c);
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.put_str(s);
        Ok(())
    }

    /// `write!` / `writeln!` の呼び出し 1 回につき 1 回だけ転送する。
    ///
    /// `write_str` は 1 回の `write!` の中で複数回呼ばれるため、そちらで
    /// 転送すると転送回数が書式の断片の数だけ増える。`common::log::Logger`
    /// はログ 1 行を `writeln!` 1 回で書くので、この形でログ 1 行 =
    /// 転送 1 回になる（ADR-0017）。
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        let started = common::cpu::read_timestamp_counter();
        let result = fmt::write(self, args);
        self.flush();
        let elapsed = common::cpu::read_timestamp_counter().wrapping_sub(started);
        self.stats.write_cycles_total += elapsed;
        self.stats.write_cycles_max = self.stats.write_cycles_max.max(elapsed);
        self.stats.write_count += 1;
        result
    }
}

/// 転送範囲の検証に失敗したことをシリアルへ出して停止する。
///
/// `Logger` を持ち回らずにシリアルへ直接書くのは、パニックハンドラと同じ
/// 考え方による。ここへ来る時点で描画系の不変条件が壊れており、報告経路が
/// 増えるほど「報告そのものが失敗する」余地が増えるため、最短の経路で書く。
fn report_flush_failure_and_halt(error: &FlushRangeError) -> ! {
    use core::fmt::Write;

    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();

    let _ = writeln!(
        serial,
        "[ERROR] console: flush range check failed; the drawing invariants are broken"
    );
    let _ = writeln!(
        serial,
        "[ERROR]   row={} bytes={} rect=({}, {}) {}x{}",
        error.row, error.bytes, error.rect.x, error.rect.y, error.rect.width, error.rect.height
    );
    let _ = writeln!(
        serial,
        "[ERROR]   back  offset={:?} size_bytes={}",
        error.back_offset, error.back_size_bytes
    );
    let _ = writeln!(
        serial,
        "[ERROR]   front offset={:?} size_bytes={}",
        error.front_offset, error.front_size_bytes
    );
    let _ = writeln!(serial, "[ERROR] halting (cli + hlt loop)");

    cpu::halt_forever();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_baseline_is_zero_before_any_flush() {
        let stats = FlushStats {
            full_screen_bytes: 4_096_000,
            ..FlushStats::default()
        };
        assert_eq!(stats.full_screen_equivalent_bytes(), 0);
        assert_eq!(stats.transferred_percent(), 0);
    }

    #[test]
    fn the_percentage_compares_against_full_screen_transfers() {
        let stats = FlushStats {
            flush_count: 10,
            transferred_bytes: 4_096_000,
            full_screen_bytes: 4_096_000,
            ..FlushStats::default()
        };
        // 10 回全面転送していれば 40,960,000 バイト。実際は 1 回分だけ。
        assert_eq!(stats.full_screen_equivalent_bytes(), 40_960_000);
        assert_eq!(stats.transferred_percent(), 10);
    }

    #[test]
    fn transferring_everything_every_time_shows_as_one_hundred_percent() {
        let stats = FlushStats {
            flush_count: 5,
            transferred_bytes: 5 * 4_096_000,
            full_screen_bytes: 4_096_000,
            ..FlushStats::default()
        };
        assert_eq!(stats.transferred_percent(), 100);
    }
}
