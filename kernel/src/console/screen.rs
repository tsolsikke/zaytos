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
use super::grid::{Grid, GridError};

/// [`Console::new`] が拒否した理由。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConsoleError {
    BackBuffer(BackBufferError),
    Grid(GridError),
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
    grid: Grid,
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
    ) -> Result<Self, ConsoleError> {
        let layout = *framebuffer.layout();

        // SAFETY: 呼び出し側の契約をそのまま引き継ぐ。
        let mut back = unsafe { BackBuffer::new(back_buffer_base, &layout) }
            .map_err(ConsoleError::BackBuffer)?;

        let grid = Grid::from_screen(
            layout.width(),
            layout.height(),
            font::CELL_WIDTH,
            font::GLYPH_HEIGHT,
        )
        .map_err(ConsoleError::Grid)?;

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

    /// 画面全体を背景色に戻し、カーソルを左上へ移す。
    pub fn clear(&mut self) {
        self.back.clear_all(self.background);
        self.dirty.mark_all();
        self.grid = Grid::from_screen(
            self.front.layout().width(),
            self.front.layout().height(),
            font::CELL_WIDTH,
            font::GLYPH_HEIGHT,
        )
        .unwrap_or(self.grid);
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
        match self.back.flush_rect(&mut self.front, rect) {
            Ok(transferred) => {
                self.stats.flush_count += 1;
                self.stats.transferred_bytes += transferred;
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
        }

        if let Some(row) = step.clear_row {
            let top = row * font::GLYPH_HEIGHT;
            self.back
                .clear_rows(top, font::GLYPH_HEIGHT, self.background);
            self.dirty
                .mark(0, top, self.back.layout().width(), font::GLYPH_HEIGHT);
        }

        if let Some(placement) = step.draw_at {
            let x = placement.column * font::CELL_WIDTH;
            let y = placement.row * font::GLYPH_HEIGHT;
            let (foreground, background) = (self.foreground, self.background);
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
        let result = fmt::write(self, args);
        self.flush();
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
        };
        assert_eq!(stats.transferred_percent(), 100);
    }
}
