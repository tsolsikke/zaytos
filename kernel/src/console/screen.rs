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
    /// Ring 3 の `write` が端末へ来た回数（PERF）。**システムコールの回数である。**
    ///
    /// # 下の [`Self::foreground_writes`] とは別である
    ///
    /// **`sys_write` は 64 バイト（`WRITE_BUF_LEN`）ずつに刻んでコンソールへ渡す**
    /// ので、**1 回のシステムコールが複数回の `write_foreground_bytes` になる。**
    /// **BKL の取り直しと `int 0x80` の回数はこちらである**（PERF-b が減らすのはここ）。
    pub terminal_writes: u64,
    /// 前景の画面へバイト列が届いた回数（PERF）。**刻んだ後の回数である。**
    ///
    /// **`sys_write` の刻み（64 バイト）ごとに 1 回である**（上の doc）。
    pub foreground_writes: u64,
    /// その `write` が運んだバイト数の合計（PERF）。
    pub foreground_bytes: u64,
    /// グリフを描く呼び出しに費やした TSC サイクル（PERF-c の測定）。
    ///
    /// **`draw_cycles` の内訳である**——**消す経路とこれを引いた残りが、
    /// パーサと格子とカーソルの費用である。**
    pub glyph_cycles: u64,
    /// 消す経路（`EL` / `ED`）に費やした TSC サイクル（PERF-c の測定）。
    ///
    /// **`draw_cycles` の内訳である**——**引いた残りが字を置く費用である。**
    pub erase_cycles: u64,
    /// 字を置く経路に費やした TSC サイクル（PERF-c の測定）。
    ///
    /// **転送とは別の層である**——**あちらは送る費用、こちらは描く費用である。**
    /// **PERF-a と b は転送の回数しか減らしていない**ので、**描く側が
    /// 支配していないかを別に測る。**
    pub draw_cycles: u64,
    /// 画面へ字を置いた回数（PERF）。**[`Console::put_char`] を通った回数である。**
    ///
    /// **描いた字とは限らない**——**スクロールや行消去だけの回も数える。**
    pub glyphs_drawn: u64,
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
    /// 既定の前景・背景（ES-b）。**SGR の `0` がここへ戻す。**
    ///
    /// **`foreground` / `background` は「いま使っている色」で、
    /// こちらは「起動時に決めた色」である。** 分けないと、
    /// **一度色を変えたら既定へ戻れない。**
    default_foreground: Color,
    default_background: Color,
    /// カーソルを描くか（ES-c。DECTCEM が切り替える）。
    ///
    /// **既定は「出す」である**——端末の慣行どおりで、`zi` は隠したいときに
    /// 明示的に `\x1b[?25l` を送る。
    cursor_visible: bool,
    /// いまカーソルを描いてあるセル（ES-c）。**消すために覚えておく。**
    ///
    /// **「描いた場所」であって「カーソルの位置」ではない。** カーソルは
    /// `Screen` が持つが、**描いた跡はピクセルなので、動かす前に元へ戻す
    /// 必要がある**——そのための記録である。
    cursor_drawn_at: Option<(u32, u32)>,
    /// 表に出ていない側の面（e-3。代替画面バッファ）。
    ///
    /// # 2 面を入れ替えて持つ
    ///
    /// **`grid` が常に「いま表に出ている面」である。** 切り替えは 2 つを
    /// 入れ替えるだけで、**既存の経路はどれも `grid` を見たままでよい。**
    ///
    /// **カーソルは面ごとに付いてくる**——`Screen` が自分で持っているので、
    /// 戻ったときの位置は入れ替えで自然に戻る。
    ///
    /// **静的に持つ**（呼び出し側が 2 面ぶんを貸す）。**ヒープは 1MiB 固定で、
    /// 1 面が 188.4KiB である**（240x67 = 16080 セル、1 セル 12 バイト）。
    inactive: Screen<'static>,
    /// いま代替画面に居るか（e-3）。
    alternate_active: bool,
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
        alternate_cells: &'static mut [Cell],
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

        // **代替画面バッファの面（e-3）。** **同じ大きさで作る**——
        // **入れ替えて使うので、形が違うと切り替えた瞬間に食い違う。**
        let inactive = Screen::from_screen(
            layout.width(),
            layout.height(),
            font::CELL_WIDTH,
            font::GLYPH_HEIGHT,
            alternate_cells,
        )
        .map_err(ConsoleError::Screen)?;

        // 最初の転送より前に、確保したばかりの領域を必ず塗り潰す。
        back.clear_all(background);

        // **セルも既定の背景で始める（e-3）。**
        //
        // **`Cell::blank()` の背景は黒である**（「色は呼び出し側の既定で
        // 塗り直される」と doc に書いてある側の話で、**塗り直すのはここである**）。
        // **触られていないセルが黒のままだと、セルから描き直したときに
        // 画面の下半分が黒くなる**——**実測で見つけた**（代替画面から戻ると、
        // 一度も字を置いていない領域だけ背景が変わって見えた）。
        //
        // **2 面とも初期化する。** 代替の面も、切り替えた瞬間に同じ形で使われる。
        let mut grid = grid;
        let mut inactive = inactive;
        grid.reset(Rgb::new(background.red, background.green, background.blue));
        inactive.reset(Rgb::new(background.red, background.green, background.blue));

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
            default_foreground: foreground,
            default_background: background,
            cursor_visible: true,
            cursor_drawn_at: None,
            inactive,
            alternate_active: false,
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

    /// カーソルの描画（ES-c）。
    ///
    /// # 形と色を判定から選んだ
    ///
    /// **形は「セルの下 2 ピクセルを塗る下線」である。** 塗りつぶしの矩形に
    /// しない——**字の上に重ねると、字が読めているかを目で確かめられなく
    /// なる**（`cargo xtask run --gui` の補助が効かなくなる）。
    ///
    /// **色は truecolor の `(0, 255, 255)`（シアン）である。**
    /// **`read_pixel_raw` で読む以上、背景と紛れない色でなければ判定が
    /// 主張を持てない**（ADR-0040 の到達条件5）。**背景
    /// （`0x10,0x10,0x18`）とも既定前景（`0xD0,0xD8,0xE0`）とも、
    /// ES-b の判定色（緑 `(0,200,0)` と赤 `(200,0,0)`）とも、
    /// RGB のどれかの軸で 150 以上離れている。**
    /// **見た目の好みで変えないこと**——近い色にすると判定は緑のまま鈍る。
    const CURSOR_COLOR: Color = Color::rgb(0, 255, 255);
    /// カーソルの下線の厚み（ピクセル）。
    const CURSOR_THICKNESS: u32 = 2;

    /// カーソルを描く / 消す（ES-c）。**描画の直前と直後に呼ぶ。**
    ///
    /// **点滅は作らない。** **時間で変わる状態は判定を揺らす**（揺れる値を
    /// 判定行に載せない）し、**そもそも時計が無い。**
    /// **いまは「在る / 無い」だけである。**
    fn paint_cursor(&mut self, show: bool) {
        let (column, row) = self.grid.cursor();
        let x = column * font::CELL_WIDTH;
        let y = row * font::GLYPH_HEIGHT + font::GLYPH_HEIGHT - Self::CURSOR_THICKNESS;
        let color = if show {
            Self::CURSOR_COLOR
        } else {
            // **消すときは、そのセルの背景色へ戻す。** 既定ではなく
            // **セルが持つ色である**——色付きの行の上でも跡が残らない。
            self.grid
                .cell(column, row)
                .map(|cell| Color::rgb(cell.bg.red, cell.bg.green, cell.bg.blue))
                .unwrap_or(self.background)
        };
        self.back
            .surface_mut()
            .fill_rect(x, y, font::CELL_WIDTH, Self::CURSOR_THICKNESS, color);
        self.dirty
            .mark(x, y, font::CELL_WIDTH, Self::CURSOR_THICKNESS);
    }

    /// 描いてあるカーソルを消す（ES-c）。**描いた場所へ戻る。**
    fn erase_drawn_cursor(&mut self) {
        let Some((column, row)) = self.cursor_drawn_at.take() else {
            return;
        };
        let (current_column, current_row) = self.grid.cursor();
        // **カーソルを一時的に描いた場所へ移して消す**——`paint_cursor` は
        // 現在位置を見るためである。**移してすぐ戻す。**
        self.grid.set_cursor(column, row);
        self.paint_cursor(false);
        self.grid.set_cursor(current_column, current_row);
    }

    /// カーソルを描き直す（ES-c）。**`flush` の直前に呼ぶ。**
    ///
    /// **前に描いた跡を消してから、いまの位置へ描く。**
    fn refresh_cursor(&mut self) {
        self.erase_drawn_cursor();
        // 破壊 (ES-c, ansi-cursor-ignore-hide): 隠す指示を無視して常に描く。
        // **DECTCEM が届いていても画面から消えない**ので、ansi-test の
        // 「隠した後にカーソルが無い」判定が落ちる。
        //
        // **逆の形（出す指示を受けたが描かない）は立てていない。**
        // **既にある 3 つの判定がそれを覆うためである**——「CUP が置いた
        // ところに描かれる」「出し直すと戻る」「動いたら跡が消えて新しい
        // 場所に在る」の 3 つは、**どれも「描かれている」ことを画面の実物で
        // 見ている。** 描かない形を作れば**その 3 つが同時に落ちる**ので、
        // **feature を足しても捕まえる先が増えない。**
        // **破壊は「その形でしか落ちない判定がある」ときに足す。**
        #[cfg(feature = "ansi-cursor-ignore-hide-test")]
        let visible = true;
        #[cfg(not(feature = "ansi-cursor-ignore-hide-test"))]
        let visible = self.cursor_visible;
        if visible {
            self.paint_cursor(true);
            self.cursor_drawn_at = Some(self.grid.cursor());
        }
    }

    /// カーソルを出す / 隠す（DECTCEM。ES-c）。
    pub fn show_cursor(&mut self, show: bool) {
        self.cursor_visible = show;
    }

    /// カーソルの下線が在るはずのピクセルを読む（ES-c の判定用）。
    ///
    /// **セルの下端の 1 点を返す。** **`pixel_at` を呼ぶ側が座標を組み立てる
    /// と、判定と実装で計算がずれる**ので、ここで組み立てる。
    pub fn cursor_pixel(&mut self, column: u32, row: u32) -> Option<u32> {
        let x = column * font::CELL_WIDTH;
        let y = row * font::GLYPH_HEIGHT + font::GLYPH_HEIGHT - 1;
        self.back.surface_mut().read_pixel_raw(x, y)
    }

    /// カーソルの色（ES-c の判定用）。**判定が値を写さずに済ませる。**
    pub fn cursor_color(&self) -> Color {
        Self::CURSOR_COLOR
    }

    /// 字を置く経路に費やしたサイクルを足す（PERF-c の測定）。
    pub fn note_draw_cycles(&mut self, cycles: u64) {
        self.stats.draw_cycles += cycles;
    }

    /// 端末への `write`（システムコール 1 回）を数える（PERF-b）。
    pub fn note_terminal_write(&mut self) {
        self.stats.terminal_writes += 1;
    }

    /// 前景の画面へバイト列が届いたことを数える（PERF）。
    pub fn note_foreground_write(&mut self, bytes: usize) {
        self.stats.foreground_writes += 1;
        self.stats.foreground_bytes += bytes as u64;
    }

    /// いま代替画面に居るか（ADR-0046）。
    ///
    /// **`sys_write`が「溜めるかどうか」を決めるために要る。**
    /// **全画面のアプリが動いていることを、カーネルはこの状態でしか知らない**
    /// ——**代替画面へ入るのは全画面のアプリだけだからである**（実測。
    /// `?1049h`を送るユーザープログラムは`zi`と`less`である）。
    pub fn alternate_screen_active(&self) -> bool {
        self.alternate_active
    }

    /// 代替画面バッファへ切り替える / 元へ戻る（`?1049`。e-3。ADR-0040 の Addendum）。
    ///
    /// # 何をするか
    ///
    /// **切り替える**（`true`）——**面を入れ替え、新しい面を空にする。**
    /// `xterm` の `?1049h` と同じで、**代替画面は毎回まっさらから始まる。**
    ///
    /// **戻る**（`false`）——**面を入れ替え、セルから画面を描き直す。**
    /// **描き直す責務はここにある**（ADR-0040 の Addendum）——
    /// **Ring 3 には画面を読み戻す手段が無いので、元の絵を持っている側が戻す。**
    ///
    /// # カーソルの跡は面をまたがない
    ///
    /// **[`Self::cursor_drawn_at`] は「いまの面のどこに下線を描いたか」である。**
    /// **面が変われば、その跡はもう無い**ので忘れる。
    ///
    /// # 色（SGR の状態）は面をまたぐ
    ///
    /// **`foreground` / `background` は入れ替えない。** **`xterm` も同じで、
    /// 代替画面へ入っても色は続く。** **戻したときに色が変わって見えないよう、
    /// 触らない。**
    pub fn set_alternate_screen(&mut self, alternate: bool) {
        if alternate == self.alternate_active {
            return;
        }
        core::mem::swap(&mut self.grid, &mut self.inactive);
        self.alternate_active = alternate;
        self.cursor_drawn_at = None;
        if alternate {
            // **代替画面はまっさらから始める。**
            self.back.clear_all(self.background);
            self.grid.reset(Self::rgb(self.background));
            self.dirty.mark_all();
        } else {
            // 破壊 (e-3, alt-screen-skip-repaint): 面は戻すが描き直さない。
            // **セルは元の画面のもの、ピクセルは代替画面のまま**になる——
            // **状態と画面が食い違う形そのものである。**
            // **判定は画面の実物を読むので落ちる**（セルだけを読む判定では
            // 落ちない。だから ES-d の判定はピクセルまで見ている）。
            #[cfg(not(feature = "alt-screen-skip-repaint-test"))]
            self.repaint_from_cells();
        }
    }

    /// いまの面のセルから、画面を丸ごと描き直す（e-3）。
    ///
    /// # 全角の右半分を飛ばす
    ///
    /// **右半分は `' '` として記録されている**ので、**素朴に描くと左の
    /// セルが描いた全角の右半分を空白で潰す。** **印（[`Cell::CONTINUATION`]）
    /// の在るセルは飛ばす**——**左のセルが 2 桁ぶんを描いている。**
    //
    // **破壊ビルドでは呼ばれない**（唯一の呼び出し側が消えるため）。
    // **未使用の警告を止めるだけで、既定ビルドの形は変えない。**
    #[cfg_attr(feature = "alt-screen-skip-repaint-test", allow(dead_code))]
    fn repaint_from_cells(&mut self) {
        let (columns, rows) = (self.grid.columns(), self.grid.rows());
        self.back.clear_all(self.background);
        for row in 0..rows {
            for column in 0..columns {
                // **飛ばすかどうかの判断は `Screen` が持つ（e-3）。**
                // **ホストで固定してある**（`Screen::should_draw`）——
                // **実機では 2 桁のグリフが在るフォントが入るまで、
                // 飛ばす側が一度も通らない。**
                if !self.grid.should_draw(column, row) {
                    continue;
                }
                let Some(cell) = self.grid.cell(column, row) else {
                    continue;
                };
                let foreground = Color::rgb(cell.fg.red, cell.fg.green, cell.fg.blue);
                let background = Color::rgb(cell.bg.red, cell.bg.green, cell.bg.blue);
                let glyph = font::glyph(cell.ch);
                let x = column * font::CELL_WIDTH;
                let y = row * font::GLYPH_HEIGHT;
                self.back
                    .surface_mut()
                    .draw_glyph(x, y, glyph, foreground, Some(background));
            }
        }
        self.dirty.mark_all();
    }

    /// SGR を適用する（ES-b。ADR-0040）。
    ///
    /// **`reset` が先に効く**——`\x1b[0;31m` は「全部戻してから赤」である。
    /// **`None` は「触らない」**（`common::ansi::Graphics` の doc）。
    ///
    /// **ここで変わるのは「これから描く色」だけである。** 既に描いたセルは
    /// 変わらない——**端末の規約どおりで、SGR は遡らない。**
    pub fn set_graphics(&mut self, graphics: common::ansi::Graphics) {
        // 破壊 (ES-b, ansi-sgr-ignore-color): 受けても色を変えない。
        // **パーサは正しく展開しており、状態も届いている**——**渡す先だけが
        // 欠けている形である**（zi-b の「接続の取り違え」と同じ族）。
        // **セルの色も画面の色も既定のままになる**ので、ansi-test の
        // 2 判定が落ちる。
        #[cfg(feature = "ansi-sgr-ignore-color-test")]
        {
            let _ = graphics;
            return;
        }
        #[cfg(not(feature = "ansi-sgr-ignore-color-test"))]
        {
            if graphics.reset {
                self.foreground = self.default_foreground;
                self.background = self.default_background;
            }
            if let Some(fg) = graphics.foreground {
                self.foreground = Color::rgb(fg.red, fg.green, fg.blue);
            }
            if let Some(bg) = graphics.background {
                self.background = Color::rgb(bg.red, bg.green, bg.blue);
            }
        }
    }

    /// 起動時に決めた既定の前景・背景（ES-b）。
    ///
    /// **判定が値を写さずに済ませるために出す。** **写すと、片方だけが
    /// 古くなる**——実際にES-bで写し間違えた（`FOREGROUND` という名の
    /// 局所定数が3つの関数にあり、コンソールが使うのはそのうち1つである）。
    /// **「期待値は定数で持たず外の道具から導く」の形をここでも採る。**
    pub fn default_colors(&self) -> (Color, Color) {
        (self.default_foreground, self.default_background)
    }

    /// セルの色を読む（ES-b の判定用）。**範囲外は `None`。**
    pub fn cell_colors(&self, column: u32, row: u32) -> Option<(Color, Color)> {
        let cell = self.grid.cell(column, row)?;
        Some((
            Color::rgb(cell.fg.red, cell.fg.green, cell.fg.blue),
            Color::rgb(cell.bg.red, cell.bg.green, cell.bg.blue),
        ))
    }

    /// セルの字を読む（ES-d の判定用）。**範囲外は `None`。**
    ///
    /// **色とは別に要る**——状態行が「モードに従って変わったか」は、
    /// **色ではなく字が変わったこと**でしか見られない。
    pub fn cell_char(&self, column: u32, row: u32) -> Option<char> {
        self.grid.cell(column, row).map(|cell| cell.ch)
    }

    /// セルの中で最初に見つかる「そのセルの背景でない」ピクセル（ES-d の判定用）。
    ///
    /// **[`Self::cell_has_ink`] は在る / 無いしか返さない。** 色を主張するには
    /// **実際に塗られた値**が要る。**セル自身の背景と比べる**ので、
    /// **色付きの行の上でも字のピクセルだけが返る。**
    ///
    /// **カーソルの下線は含まれる**（同じセルに在れば拾う）。
    /// **呼ぶ側がカーソルの居るセルを避けること。**
    pub fn cell_ink_pixel(&mut self, column: u32, row: u32) -> Option<u32> {
        let format = self.back.layout().format();
        let background = self
            .grid
            .cell(column, row)
            .map(|cell| Color::rgb(cell.bg.red, cell.bg.green, cell.bg.blue))
            .unwrap_or(self.background)
            .to_pixel(format);
        let x = column * font::CELL_WIDTH;
        let y = row * font::GLYPH_HEIGHT;
        for dy in 0..font::GLYPH_HEIGHT {
            for dx in 0..font::CELL_WIDTH {
                let pixel = self.back.surface_mut().read_pixel_raw(x + dx, y + dy)?;
                if pixel != background {
                    return Some(pixel);
                }
            }
        }
        None
    }

    /// 画面の実物のピクセルを読む（ES-b の判定用。ADR-0040 の到達条件3）。
    ///
    /// **セルの中身ではなく、バックバッファに実際に書かれた値である。**
    /// **`cell_colors` と対で使う**——状態と画面が一致することを見るために、
    /// 両方が要る。
    pub fn pixel_at(&mut self, x: u32, y: u32) -> Option<u32> {
        self.back.surface_mut().read_pixel_raw(x, y)
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

    /// 行を削除して下を詰める（`DL`。PERF-d）。
    ///
    /// # セルと画素を同じ規則で動かす
    ///
    /// **片方だけを動かすと、状態と画面が食い違う**（`set_alternate_screen`の
    /// doc と同じ立場である）。**セルは`common::screen`が、画素は
    /// [`crate::console::BackBuffer`]が持つ。**
    ///
    /// # 全面を未転送にする
    ///
    /// **ずらした範囲は画面のどこでも変わりうる。** **矩形1つで表せるので、
    /// `mark_all`ではなく動かした範囲だけを印にする。**
    pub fn delete_lines(&mut self, count: u32) {
        let (_, row) = self.grid.cursor();
        self.erase_drawn_cursor();
        let top = row * font::GLYPH_HEIGHT;
        let height = self.back.layout().height().saturating_sub(top);
        self.back
            .delete_rows(top, count * font::GLYPH_HEIGHT, self.background);
        self.grid
            .delete_lines(row, count, Self::rgb(self.background));
        self.dirty.mark(0, top, self.back.layout().width(), height);
    }

    /// 行を挿入して下へずらす（`IL`。PERF-d）。
    pub fn insert_lines(&mut self, count: u32) {
        let (_, row) = self.grid.cursor();
        self.erase_drawn_cursor();
        let top = row * font::GLYPH_HEIGHT;
        let height = self.back.layout().height().saturating_sub(top);
        self.back
            .insert_rows(top, count * font::GLYPH_HEIGHT, self.background);
        self.grid
            .insert_lines(row, count, Self::rgb(self.background));
        self.dirty.mark(0, top, self.back.layout().width(), height);
    }

    /// 行消去（EL。zi-b）。**カーソルは動かさない。**
    ///
    /// 範囲は ANSI の規約どおり——`After` と `Before` はどちらもカーソルの
    /// セルを含む。
    pub fn erase_in_line(&mut self, scope: common::ansi::EraseScope) {
        use common::ansi::EraseScope;
        let started = common::cpu::read_timestamp_counter();
        let (column, row) = self.grid.cursor();
        let columns = self.grid.columns();
        match scope {
            EraseScope::After => self.erase_cells(row, column, columns),
            EraseScope::Before => self.erase_cells(row, 0, column + 1),
            EraseScope::All => self.erase_cells(row, 0, columns),
        }
        self.stats.erase_cycles += common::cpu::read_timestamp_counter().wrapping_sub(started);
    }

    /// 画面消去（ED。zi-b）。**カーソルは動かさない**——ここが [`Self::clear`]
    /// との違いである（ANSI の ED はカーソルを移さない。`zi` は ED の後に
    /// CUP を送る）。
    pub fn erase_in_display(&mut self, scope: common::ansi::EraseScope) {
        use common::ansi::EraseScope;
        let started = common::cpu::read_timestamp_counter();
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
                // **セルも空へ戻す（zi-e 前の手当て）。** **ピクセルだけ消すと、
                // 状態と画面が食い違う**——`erase_cells`（`After` / `Before` が
                // 使う側）は最初から両方を消しており、**ここだけが片方だった。**
                //
                // **実測で見つかった。** 画面の字をセルから読み出したところ、
                // **`ED(2)` の後の行に、消えたはずの起動ログが残っていた**
                // （ピクセルは消えている）。**セルを読む判定を置くなら、
                // ここが合っていなければ嘘を読む。**
                //
                // **カーソルは動かさない。** `Screen::reset` は左上へ戻すので、
                // **前後で位置を控えて戻す**——**ANSI の `ED` はカーソルを
                // 移さない**（この関数の doc。`clear()` との違いそのものである）。
                self.grid.reset(Self::rgb(self.background));
                self.grid.set_cursor(column, row);
            }
        }
        self.stats.erase_cycles += common::cpu::read_timestamp_counter().wrapping_sub(started);
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
        // **カーソルを描き直してから送る（ES-c）。**
        //
        // **`take` より前である**——描き直すと未転送範囲が増えるので、
        // **先に描かないとカーソルだけが送られない。**
        self.refresh_cursor();
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
        // **数える（PERF）。** **速さの層を分けるために要る**——
        // **この数と、`write` の数と、転送の数が別の層である。**
        self.stats.glyphs_drawn += 1;
        // **描いてあるカーソルを先に消す（ES-c）。** **消さずに字を置くと、
        // カーソルの跡が字の下に残る**——下線とグリフが重なる位置にあるため。
        self.erase_drawn_cursor();
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
            let started = common::cpu::read_timestamp_counter();
            self.back
                .surface_mut()
                .draw_glyph(x, y, glyph, foreground, Some(background));
            self.stats.glyph_cycles += common::cpu::read_timestamp_counter().wrapping_sub(started);
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
