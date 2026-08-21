//! 画面の実物を見る観測（ES-d。`zi-test` のときだけ在る）。
//!
//! # なぜカーネル側に在るのか
//!
//! **色を出す利用者は Ring 3 に居る**（`zash` のプロンプトと `zi` の状態行）が、
//! **あちらは画面を読めない**——`Console` はカーネルの側にあり、
//! ユーザープログラムから読み戻す道が無い（`kernel/userland/zi.rs` の
//! モジュール doc が「観測できない」と書いている当のものである）。
//! **したがって、判定は画面を持っている側に置く。**
//!
//! # いつ見るのか
//!
//! **台本の中に観測点を置く**（`crate::input` の `script`）。
//! **観測点まで来たということは、プログラムがそこまでの入力を処理し終えて
//! 次の 1 バイトを要求した**ということである。**書き込みは `write` の中で
//! 画面へ届いてフラッシュ済みなので、この時点の画面は最新である。**
//!
//! **これは「判定が見た時点が違う」を避ける形である**
//! （`docs/verification-coverage.md` の破壊が緑を出す道の 4 つ目）。

use crate::graphics::Color;
use common::serial::SerialPort;
use core::fmt::Write as _;
use core::sync::atomic::Ordering;

/// 観測の種類。**台本の中の 1 バイトが指す。**
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observation {
    /// `zash` のプロンプト。
    Prompt,
    /// `zi` の状態行。
    Status,
}

/// `zash` のプロンプトの名前の部分の色（緑）。
///
/// # ES-b の判定色の緑と同じ値である
///
/// **共有している。** **150 以上離れた「別の緑」は作れない**——`(0, 200, 0)`
/// から離すと黄かシアンに寄る（`kernel/userland/zash.rs` の `PROMPT_COLOR`）。
/// **同じ画面に並ばない**ことを実測で確かめてあるが、**それに頼らず、
/// プロンプトの判定はカーソルの居る行だけを見る**（[`observe_prompt`]）。
///
/// # 写しである
///
/// **`kernel/userland/zash.rs` の `PROMPT_COLOR` と同じ値である。**
/// **ユーザープログラムはカーネルの定数を参照できず、逆も同じ**なので、
/// **境界をまたぐ値は写すしかない**（`SYS_*` の番号や `SPAWN_INTERRUPTED` と
/// 同じ形である）。
///
/// **写しが古くなったら判定が落ちる。** 色を変えて片方だけ直せば、
/// **その色のセルが見つからず `= false` が出る**——**黙って緑にはならない。**
/// **判定が実装の値を写さない規律（`docs/verification-coverage.md`）は
/// 同じプログラムの中の話で、ここは越えられない境界である。**
const PROMPT_COLOR: Color = Color::rgb(0, 200, 0);
/// `zi` の状態行の色。**`kernel/userland/zi.rs` の `STATUS_COLOR` の写しである。**
const STATUS_COLOR: Color = Color::rgb(200, 200, 0);

/// ピクセルの値を 16 進で出すための包み（ES-d）。
///
/// **10 進で出すと、期待値（`{want:#010x}`）と見比べられない。**
struct PixelHex(u32);

impl core::fmt::Debug for PixelHex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:#010x}", self.0)
    }
}

/// 状態行から控える札の最大の長さ（セル数）。
const LABEL_MAX: usize = 16;

/// 前に見た状態行の札（ES-d）。**モードに従って変わったことを見るために要る。**
#[derive(Clone, Copy)]
struct StatusSnapshot {
    label: [u8; LABEL_MAX],
    length: usize,
    seen: bool,
}

/// 前に見た札。**`Locked` で守る**——観測はシステムコールの経路から来るので、
/// 割り込みと同一コアの再入を締める（`common::critical` の doc）。
static LAST_STATUS: common::critical::Locked<StatusSnapshot> =
    common::critical::Locked::new(StatusSnapshot {
        label: [0; LABEL_MAX],
        length: 0,
        seen: false,
    });

/// 画面を見て、判定行をシリアルへ出す。
///
/// **据えられていなければ、見ていないと書く。** 黙って何も出さないと、
/// **判定行が無いことを「まだ出ていない」と読める**（`--full` は行の有無で
/// 見ている）。
pub(crate) fn observe(kind: Observation) {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let console = super::FOREGROUND.load(Ordering::Acquire);
    if console.is_null() {
        let _ = writeln!(
            serial,
            "screen-color: nothing was observed - no foreground console was installed"
        );
        return;
    }
    // SAFETY: 非 null なら [`super::install_foreground`] のガードが生きており、
    // 据えている間は据えた側が `&mut Console` を預けたままなので書けない
    // （`super::FOREGROUND` の doc）。**読むだけだが、`&mut` が要るのは
    // バックバッファのピクセルを読む経路が可変借用を求めるためである。**
    // 同時に 2 つの遠征が走らないことも同じ doc に挙げてある。
    let console = unsafe { &mut *console };
    match kind {
        Observation::Prompt => observe_prompt(&mut serial, console),
        Observation::Status => observe_status(&mut serial, console),
    }
}

/// 色の付いた連なりを、1 行の中で探す（ES-d）。
///
/// **行を呼ぶ側が決める形である。** プロンプトの判定はカーソルの居る行を渡す
/// ——**色でだけ探すと、同じ色を使う別の判定のセルを拾いうる**（緑は ES-b と
/// 共有している）。
fn find_colored_run_in_row(
    console: &mut crate::console::Console,
    row: u32,
    want: Color,
) -> Option<(u32, u32, u32)> {
    let (columns, _) = console.size();
    let mut from = None;
    for column in 0..columns {
        let matches = console
            .cell_colors(column, row)
            .is_some_and(|(fg, _)| fg == want);
        match (matches, from) {
            (true, None) => from = Some(column),
            (false, Some(start)) => return Some((row, start, column)),
            _ => {}
        }
    }
    from.map(|start| (row, start, columns))
}

/// 色の付いた連なりを、画面の下から探す（ES-d）。
///
/// **下から探すのは、いちばん新しく描かれたものを見るためである**——
/// プロンプトは行が進むたびに増えるので、**上から探すと最初の 1 本に当たる。**
///
/// 返すのは `(row, from, to)` で、`to` は終端の 1 つ先である。
fn find_colored_run(console: &mut crate::console::Console, want: Color) -> Option<(u32, u32, u32)> {
    let (columns, rows) = console.size();
    for row in (0..rows).rev() {
        let mut from = None;
        for column in 0..columns {
            let matches = console
                .cell_colors(column, row)
                .is_some_and(|(fg, _)| fg == want);
            match (matches, from) {
                (true, None) => from = Some(column),
                (false, Some(start)) => return Some((row, start, column)),
                _ => {}
            }
        }
        if let Some(start) = from {
            return Some((row, start, columns));
        }
    }
    None
}

/// 連なりの中で、字の在るセルの実物のピクセルを読む（ES-d）。
///
/// **空白のセルにはインクが無い**ので、字の在るセルを選ぶ。
/// **返せなければ `None`**——「色は在るが画面には出ていない」がその形である。
fn ink_of_run(
    console: &mut crate::console::Console,
    row: u32,
    from: u32,
    to: u32,
) -> Option<(u32, u32)> {
    for column in from..to {
        if console.cell_char(column, row) != Some(' ') {
            return console.cell_ink_pixel(column, row).map(|ink| (column, ink));
        }
    }
    None
}

/// プロンプトの色を見る。
///
/// # 見るのはカーソルの居る行である
///
/// **プロンプトを出し終えて入力を待っている時点で観測する**ので、
/// **カーソルはプロンプトの直後に居る**（実測。8 桁目）。
/// **画面全体を色で探さない**——同じ緑を ES-b の判定が使うためである。
///
/// # 2 つを主張する
///
/// **色が名前に乗っていること**（連なりのインクが緑であること）と、
/// **色が記号へ漏れていないこと**（連なりの直後のセルが既定前景で、
/// かつ字が在ること）。**後者は ES-d の目視で「白のはず」と決めた側である。**
/// **既定前景は写さない。コンソールに訊く。**
fn observe_prompt(serial: &mut SerialPort, console: &mut crate::console::Console) {
    let format = console.framebuffer_layout().format();
    let want = PROMPT_COLOR.to_pixel(format);
    let (_, cursor_row) = console.cursor_cell();
    let found = find_colored_run_in_row(console, cursor_row, PROMPT_COLOR);
    let ink = found.and_then(|(row, from, to)| ink_of_run(console, row, from, to));
    let drawn = ink.is_some_and(|(_, pixel)| pixel == want);
    let _ = writeln!(
        serial,
        "screen-color: the zash prompt name is drawn in its own color = {drawn} \
         (row {cursor_row}, run {found:?}, ink {:?} at column {:?}, expected {want:#010x})",
        ink.map(|(_, pixel)| PixelHex(pixel)),
        ink.map(|(column, _)| column)
    );

    // **記号のセル**（連なりの直後）**は既定前景で、字が在る。**
    let default_foreground = console.default_colors().0;
    let symbol = found.map(|(row, _, to)| (to, row));
    let symbol_colors = symbol.and_then(|(column, row)| console.cell_colors(column, row));
    let symbol_char = symbol.and_then(|(column, row)| console.cell_char(column, row));
    let symbol_is_plain = symbol_colors.is_some_and(|(fg, _)| fg == default_foreground)
        && symbol_char.is_some_and(|c| c != ' ');
    let _ = writeln!(
        serial,
        "screen-color: the prompt symbol kept the default color = {symbol_is_plain} \
         (cell {symbol:?} char {symbol_char:?} fg {:?}, default {default_foreground:?})",
        symbol_colors.map(|(fg, _)| fg)
    );
}

/// 状態行の色と、モードに従って変わったことを見る。
fn observe_status(serial: &mut SerialPort, console: &mut crate::console::Console) {
    let format = console.framebuffer_layout().format();
    let want = STATUS_COLOR.to_pixel(format);
    let found = find_colored_run(console, STATUS_COLOR);
    let ink = found.and_then(|(row, from, to)| ink_of_run(console, row, from, to));
    let drawn = ink.is_some_and(|(_, pixel)| pixel == want);
    let _ = writeln!(
        serial,
        "screen-color: the zi status line is drawn in its own color = {drawn} \
         (run {found:?}, ink {:?} at column {:?}, expected {want:#010x})",
        ink.map(|(_, pixel)| PixelHex(pixel)),
        ink.map(|(column, _)| column)
    );

    // **札を控えて、前に見たものと比べる。**
    let mut label = [0u8; LABEL_MAX];
    let mut length = 0usize;
    if let Some((row, from, to)) = found {
        for column in from..to.min(from + LABEL_MAX as u32) {
            let c = console.cell_char(column, row).unwrap_or(' ');
            // **ASCII だけを控える。** 札は ASCII である（`zi` の `STATUS_*`）。
            label[length] = if c.is_ascii() { c as u8 } else { b'?' };
            length += 1;
        }
    }
    let previous = *LAST_STATUS.lock();
    *LAST_STATUS.lock() = StatusSnapshot {
        label,
        length,
        seen: true,
    };
    if !previous.seen {
        // **1 回目は比べる相手が無い。** 観測していないことは書いておく。
        let _ = writeln!(
            serial,
            "screen-color: the zi status line said {:?} (first observation, nothing to compare yet)",
            core::str::from_utf8(&label[..length]).unwrap_or("?")
        );
        return;
    }
    let changed = length > 0 && label[..length] != previous.label[..previous.length];
    let _ = writeln!(
        serial,
        "screen-color: the zi status line followed the mode = {changed} (was {:?}, now {:?})",
        core::str::from_utf8(&previous.label[..previous.length]).unwrap_or("?"),
        core::str::from_utf8(&label[..length]).unwrap_or("?")
    );
}
