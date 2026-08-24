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
    /// 代替画面へ入る前の画面（e-3）。**控えるだけで、判定は出さない。**
    BeforeAlternate,
    /// 代替画面から戻った画面（e-3）。**控えたものと突き合わせる。**
    AfterAlternate,
    /// コマンド行（最下行。e-4）。**打っている途中が出ているか。**
    CommandLine,
    /// `zi` の窓（VIEW-a）。**本文の先頭行に何が出ているかを控えて出す。**
    ///
    /// # 画面を読む。バッファではない
    ///
    /// **窓が動いたことは、画面の先頭行が変わったことでしか言えない。**
    /// **`zi` の内部状態（`top`）は診断行に出ているが、それはバッファ側で
    /// ある**——**H-b-2 で捕まえられなかったのは、まさにバッファしか
    /// 見ていなかったからである。**
    ZiWindow,
    /// エコーエリア（最下行。ADR-0046）。**エラーがそこに出ているか。**
    ///
    /// # 「見えなくする」と区別が付く形にする
    ///
    /// **カーネルが溜めた `fd 2` を、アプリが取り出してここへ描く。**
    /// **溜めるだけで描かなければ、画面は壊れないが人にも見えない**
    /// ——**それは却下した案（(a) と (a')）と同じ振る舞いである。**
    /// **したがって「出ていること」を画面の実物で見る。**
    ///
    /// # `CommandLine` と同じ行を読む。ラベルだけが違う
    ///
    /// **同じラベルにしない。** **`screen-message` を読む判定は最初の 1 件を
    /// 取っており**（`xtask` の `find_map`）、**同じ名前で増やすとそちらが
    /// 別の回を見てしまう。**
    EchoArea,
    /// 台本が最後まで進んだ（DIR-1b）。**画面を読まない。**
    ///
    /// # 何も主張しない観測点である
    ///
    /// **ホスト側が「台本が終わった」を知るためだけに在る。**
    /// **以前は代替画面の観測（`AfterAlternate`）の行を合図にしていた**が、
    /// **あれを台本の途中へ移したところ、途中で待ちが切れた**（実測。DIR-1b）。
    ///
    /// **合図と主張を分ける。** **主張を持つ行を合図に使うと、
    /// 主張の置き場所を動かしたときに待ちが壊れる。**
    ScriptDone,
    /// コマンド行（最下行。e-5）。**報せ（断った理由）が出ているか。**
    ///
    /// **読むものは [`Observation::CommandLine`] と同じである**——
    /// **分けてあるのは、判定行の名前を分けて、どちらの主張かを
    /// ホスト側が見分けるためである。**
    Message,
}

/// 代替画面へ入る前に控えた画面の目印（e-3）。
///
/// # 何を控えるか
///
/// **プロンプトの緑の連なりが在った行と、その行のインクのピクセルである。**
///
/// **ピクセルまで控える**——**戻ったときにセルだけが戻って画面が代替の
/// ままなら、セルは一致してピクセルが違う。** **その形が破壊そのもので
/// ある**（`alt-screen-skip-repaint`）。
///
/// # 全角の右半分は、ここでは観測できない
///
/// **描き直しは継続セルの印（`common::screen::Cell::CONTINUATION`）を見て
/// 右半分を飛ばす**が、**いまのフォントに 2 桁のグリフが 1 つも無い**
/// （`third_party/unifont/unifont-subset.hex` は 96 字すべてが 8x16。実測）。
/// **したがって継続セルは実機の画面に一度も現れず、画面側の判定が置けない。**
/// **主張はホストの単体テストが持つ**（`common::screen` の
/// `a_wide_glyph_marks_its_right_half`）。**フォントに全角が入ったら、
/// ここへ画面側の判定を足すこと**（`docs/deferred-decisions.md` に行がある）。
#[derive(Clone, Copy)]
struct BeforeAlternateMarks {
    seen: bool,
    /// プロンプトの緑が在った行と、その行のインクのピクセル。
    prompt: Option<(u32, u32)>,
}

/// 控えた目印。**`Locked` で守る**（[`LAST_STATUS`] と同じ理由）。
static BEFORE_ALTERNATE: common::critical::Locked<BeforeAlternateMarks> =
    common::critical::Locked::new(BeforeAlternateMarks {
        seen: false,
        prompt: None,
    });

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
        Observation::BeforeAlternate => observe_before_alternate(&mut serial, console),
        Observation::AfterAlternate => observe_after_alternate(&mut serial, console),
        Observation::CommandLine => observe_command_line(&mut serial, console, "screen-command"),
        Observation::Message => observe_command_line(&mut serial, console, "screen-message"),
        Observation::EchoArea => observe_command_line(&mut serial, console, "screen-echo"),
        Observation::ZiWindow => observe_zi_window(&mut serial, console),
        Observation::ScriptDone => {
            let _ = writeln!(serial, "script-done: the script reached its end");
        }
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

    // **画面の形を出す（e-1）。** **`ioctl(TIOCGWINSZ)` が答える値の出所と
    // 同じもの**（`crate::console::foreground_geometry`）を、**カーネルの側から
    // 1 行にする。** **`zi` が受け取った値と突き合わせる**ので、
    // **どちらも期待値を写していない**——経路のどこかで入れ替われば食い違う。
    let (columns, rows) = console.size();
    let layout = console.framebuffer_layout();
    let _ = writeln!(
        serial,
        "screen-size: the console is rows={rows} columns={columns} xpixel={} ypixel={}",
        layout.width(),
        layout.height()
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

/// `zi` の本文の先頭行に出ている字を控えて出す（VIEW-a）。
///
/// # 何を主張するか
///
/// **この関数は主張しない。控えて出すだけである。**
/// **突き合わせるのはホスト側である**——**台本は窓を動かす前と後で 2 回
/// 観測し、ホストが「変わったこと」と「後のほうがファイルの後ろの行で
/// あること」を見る。** **期待値をこちらが持たない。**
///
/// # 読むのは画面の行 0 である（ADR-0046 で戻した）
///
/// **`zi` は本文を画面の先頭から並べる**（`redraw` が `move_cursor(0, 0)` から
/// 置く）ので、**行 0 が「窓の先頭に見えている行」である。**
///
/// **VIEW-a では行 1 を読んでいた。** **行 0 が診断行に上書きされていた
/// ためである**（実測。`"zi: cursor (buff"` が読めた）。**迂回であって、
/// 主張が薄かった。**
///
/// **ADR-0046 で診断の出口を画面から外した**ので、**迂回は要らなくなった。**
/// **行 0 を読むほうが主張が強い**——**窓の先頭そのものを見ている。**
///
/// **破壊 `stderr-on-screen-test` は、まさにこの行を診断行へ戻す。**
fn observe_zi_window(serial: &mut SerialPort, console: &mut crate::console::Console) {
    /// 読む画面の行（VIEW-a。ADR-0046 で 1 から戻した）。**窓の先頭である。**
    const ROW: u32 = 0;
    let (columns, _) = console.size();
    let mut text = [0u8; LABEL_MAX];
    let mut length = 0usize;
    for column in 0..columns.min(LABEL_MAX as u32) {
        let c = console.cell_char(column, ROW).unwrap_or(' ');
        text[length] = if c.is_ascii() { c as u8 } else { b'?' };
        length += 1;
    }
    // **右端の空白を落とす。** **行の長さは中身で決まり、画面の幅ではない。**
    while length > 0 && text[length - 1] == b' ' {
        length -= 1;
    }
    let _ = writeln!(
        serial,
        "screen-window: the top row of the window says {:?}",
        core::str::from_utf8(&text[..length]).unwrap_or("?")
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

    // **下から 2 行目に在ること（e-4）。**
    //
    // **`ioctl(TIOCGWINSZ)` が答えた行数を `zi` が実際に使っている**ことの
    // 主張である。**行番号はこちらが画面から取る**（`console.size()`）ので、
    // **期待値を写していない。**
    let (_, rows) = console.size();
    let at_bottom = found.is_some_and(|(row, _, _)| row + 2 == rows);
    let _ = writeln!(
        serial,
        "screen-color: the zi status line sits on the second-to-last row = {at_bottom} \
         (row {:?}, the screen has {rows} row(s))",
        found.map(|(row, _, _)| row)
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
    // **札をそのつど出す（e-2）。** **並びを見る判定が要る**——
    // 「Esc 1 回で前のモードへ戻った」は、**3 つ目の札が 1 つ目と同じで
    // 2 つ目と違うこと**で言える（**札の文字列を写さずに済む**）。
    let _ = writeln!(
        serial,
        "screen-color: the zi status line says {:?}",
        core::str::from_utf8(&label[..length]).unwrap_or("?")
    );

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
            "screen-color: the zi status line has no earlier observation to compare yet"
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

/// 代替画面へ入る前の画面を控える（e-3）。**判定は出さない。**
fn observe_before_alternate(serial: &mut SerialPort, console: &mut crate::console::Console) {
    let (_, cursor_row) = console.cursor_cell();
    let prompt = find_colored_run_in_row(console, cursor_row, PROMPT_COLOR)
        .and_then(|(row, from, to)| ink_of_run(console, row, from, to).map(|(_, ink)| (row, ink)));
    *BEFORE_ALTERNATE.lock() = BeforeAlternateMarks { seen: true, prompt };
    let _ = writeln!(
        serial,
        "screen-restore: before the alternate screen, the prompt was at row {:?}",
        prompt.map(|(row, _)| row)
    );
}

/// 代替画面から戻った画面を、控えたものと突き合わせる（e-3）。
///
/// # 主張は1つである
///
/// **元の画面が戻っていること**——プロンプトの緑が同じ行に、同じピクセルで
/// 在ることである。
///
/// **ピクセルを見る。** **セルだけを見ると、面を入れ替えただけで描き直して
/// いない形が通ってしまう。**
fn observe_after_alternate(serial: &mut SerialPort, console: &mut crate::console::Console) {
    let before = *BEFORE_ALTERNATE.lock();
    if !before.seen {
        let _ = writeln!(
            serial,
            "screen-restore: nothing was recorded before the alternate screen"
        );
        return;
    }

    let prompt_now = before.prompt.and_then(|(row, _)| {
        find_colored_run_in_row(console, row, PROMPT_COLOR).and_then(|(row, from, to)| {
            ink_of_run(console, row, from, to).map(|(_, ink)| (row, ink))
        })
    });
    let screen_came_back = before.prompt.is_some() && prompt_now == before.prompt;
    let _ = writeln!(
        serial,
        "screen-restore: the screen before zi came back = {screen_came_back} \
         (was {:?}, now {:?})",
        before.prompt.map(|(row, ink)| (row, PixelHex(ink))),
        prompt_now.map(|(row, ink)| (row, PixelHex(ink)))
    );
}

/// コマンド行（最下行）に、打っている途中が出ているか（e-4）。
///
/// # 最下行の字をそのまま出す
///
/// **`:` を打った時点で `:` が出て、`w` を打てば `:w` になる。**
/// **観測点は台本の `:w` の直後に置いてある**ので、**この時点の最下行は
/// `:w` であるはずである。**
///
/// **判定するのはホスト側である**（`xtask`）。ここは画面から読んだ字を
/// 出すだけで、**期待値を持たない。**
fn observe_command_line(
    serial: &mut SerialPort,
    console: &mut crate::console::Console,
    marker: &str,
) {
    let (columns, rows) = console.size();
    let row = rows - 1;
    let mut line = [0u8; 48];
    let mut length = 0usize;
    for column in 0..columns.min(line.len() as u32) {
        let c = console.cell_char(column, row).unwrap_or(' ');
        line[length] = if c.is_ascii() { c as u8 } else { b'?' };
        length += 1;
    }
    while length > 0 && line[length - 1] == b' ' {
        length -= 1;
    }
    let _ = writeln!(
        serial,
        "{marker}: the last row (row {row}) says {:?}",
        core::str::from_utf8(&line[..length]).unwrap_or("?")
    );
}
