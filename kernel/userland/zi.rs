//! `zi`: ターミナル上の vi 風エディタ（zi-d-1）。
//!
//! # crate ではない
//!
//! `ls.rs` / `cat.rs` と同じで、cargo のパッケージに属さない
//! （`userlib.rs` の doc）。
//!
//! # 何が観測でき、何が観測できないか
//!
//! **この段の自動判定が見るのは `zi` の内部状態であって、画面ではない。**
//! カーソル位置の判定行（`zi: cursor ...`）は**バッファ上の行と桁**で、
//! **画面に何が描かれたかではない。** `zi` は Ring 3 に居て `Console` を
//! 読めないので、**再描画の誤り（CUP の位置が 1 つずれる等）はこの判定では
//! 捕まらない**——ファイルの中身が正しいまま画面だけが崩れる形が作れる。
//!
//! **したがって「台本が緑だから画面も正しい」とは読めない。**
//! ANSI の解釈そのもの（CUP・ED・EL がセルへどう効くか）は
//! `ansi-test` が別に固定しており（ADR-0029 の Addendum）、
//! **`zi` が意図した列を出しているかは目視の補助に委ねてある**
//! （`cargo xtask run --gui`）。
//!
//! # zi-c の契約から来る順序
//!
//! **`O_WRONLY|O_TRUNC` は open の時点で長さ 0 へ切る**（ADR-0037）ので、
//! **読みながら書き先を開いておくことはできない。** 起動時に
//! `open(O_RDONLY)` で全部読んで閉じ、`:w` のときに開き直す形になる。
//!
//! # 終了状態の意味
//!
//! - `0` 正常に終わった
//! - `1` 開けなかった
//! - `2` 引数が無かった
//! - `3` 読めなかった
//! - `4` ファイルが上限を越えている（**切り詰めない**——切り詰めて保存すると
//!   開いた時点で中身が消える）
//! - `5` `:w` が失敗した（開けない、または書いた量が要求と食い違う）

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{
    close, exit, length_of, open_read_only, open_write_create, read, write_all, STDERR, STDOUT,
};

/// パスの最大長（NUL を含む）。**カーネルの `PATH_MAX` と同じ。**
const PATH_MAX: usize = 256;

/// 収められる行数。**固定配列である**（`mmap` を先取りしない。棚卸しの判断）。
const MAX_LINES: usize = 64;
/// 1 行の最大バイト数。
const MAX_LINE_LEN: usize = 128;
/// 1 回に読む大きさ。`cat` と同じ理由で、ブロックより小さくてよい。
const CHUNK: usize = 256;

/// 引数が無いときの使い方。
const USAGE: &[u8] = b"zi: usage: zi PATH\n";
/// 開けなかったときの断り書き。
const OPEN_FAILED: &[u8] = b"zi: cannot open\n";
/// 読めなかったときの断り書き。
const READ_FAILED: &[u8] = b"zi: cannot read\n";
/// 上限を越えていたときの断り書き。**切り詰めない。**
const TOO_BIG: &[u8] = b"zi: the file does not fit the buffer\n";

/// 行数の上限に当たったときの報せ（zi-f）。**コマンド行へ出す。**
///
/// **黙って落とさない。** **`insert` が入らない字を落とすのと同じ判断だが、
/// あちらは 1 字で、こちらは「行が作れない」である**——**使う人から見て
/// 何も起きないので、言わないと分からない。**
const NO_ROOM_FOR_A_LINE: &[u8] = b"no room for another line";

/// 1 行の上限に当たって連結できなかったときの報せ（zi-f）。
const NO_ROOM_TO_JOIN: &[u8] = b"the joined line would be too long";

/// `read(0)` が「まだ無い」を返す値（`-EAGAIN`）。
const MINUS_EAGAIN: i64 = -11;
/// Esc のバイト。
const ESC: u8 = 0x1b;

/// コマンド行の最大長（`:wq` で足りるが、余裕を取る）。
const COMMAND_MAX: usize = 16;

/// 状態行の色（ES-d）。**SGR の truecolor で前景を指定する。**
///
/// # 色は判定から選んだ
///
/// **黄 `(200, 200, 0)` である。** 画面の実物を `read_pixel_raw` で読む判定が
/// 付くので、**既に画面に居る色と紛れてはならない**（`zash` の `PROMPT_COLOR`
/// と同じ規律）。**背景 `(0x10, 0x10, 0x18)`・既定前景 `(0xD0, 0xD8, 0xE0)`・
/// ES-b の判定色（緑と赤）・カーソルのシアンのいずれとも、RGB のどれかの軸で
/// 150 以上離れている。**
///
/// **`zash` のプロンプトの名前も緑 `(0, 200, 0)` である**（zi-e 前の色替え）。
/// **同じ画面に並びうるが、判定は別々に読む**——プロンプトはカーソルの居る行、
/// 状態行は本文の下である（`kernel/src/console/probe.rs`）。
const STATUS_COLOR: &[u8] = b"\x1b[38;2;200;200;0m";
/// 色を既定へ戻す（SGR 0）。
const SGR_RESET: &[u8] = b"\x1b[0m";

/// 状態行の札（ES-d）。**3 つとも同じ長さである。**
///
/// # 長さを揃えると消去が要らない
///
/// **札を上書きするだけで前の札が残らない。** 揃えないと `EL(2)` が要り、
/// **`EL(2)` を足すと `parse_zi_last_redraw`（xtask）が状態行を本文の行として
/// 拾う**——あちらは再描画の列を `\x1b[2K` で切って行を取り出している。
/// **揃えるほうが、判定の側に例外を作らずに済む。**
const STATUS_NORMAL: &[u8] = b"-- NORMAL  --";
const STATUS_INSERT: &[u8] = b"-- INSERT  --";
const STATUS_COMMAND: &[u8] = b"-- COMMAND --";

/// `:w` が書き出す先の受け皿。**`.bss` に置く**（スタックは 1 ページである）。
static mut FLUSH_BUFFER: [u8; MAX_LINES * (MAX_LINE_LEN + 1)] =
    [0; MAX_LINES * (MAX_LINE_LEN + 1)];

/// 編集中のモード。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
    /// `:` を打った後。**改行までを溜めて解釈する。**
    Command,
}

/// エスケープ列の受け（`zash` と同じ形。`kernel/src/input.rs` が落とす形）。
///
/// **矢印は 3 バイトで届く**（`\x1b` `[` に `A`/`B`/`C`/`D`）。
/// **`\x1b` の直後に `[` が来なければ Esc 単体である**——カーネルが CSI の
/// 3 バイトを不可分に組み立てるので、**この判別は確定である**
/// （`kernel/src/input.rs` の `bytes_for_event` の doc）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    Idle,
    Esc,
    Bracket,
    /// `\x1b[3` まで来た（zi-f）。**次が `~` なら Delete である。**
    ///
    /// **矢印は 3 バイトで終わるが、Delete は 4 バイトである**
    /// （`\x1b[3~`。本物の端末と同じ形。`kernel/src/input.rs`）。
    /// **数字を溜める形にはしない**——**受けるのは `3~` の 1 種類だけで、
    /// 一般の CSI パラメータを解釈する利用者がまだ居ない。**
    Tilde,
}

/// 行の集まりと読み込みの受け皿。**`.bss` の静的に置く。**
///
/// # スタックには置けない
///
/// **ユーザースタックは 1 ページ（4KiB）である**（`kernel/src/userland.rs` の
/// `USER_PROGRAM_STACK_TOP`。1 ページだけ張る）。**この段の配列は約 16KiB で、
/// 局所に置くと入口で `#PF` になる**（実測——`Folded(14)` で 0 syscall のまま
/// 死んだ）。**`.bss` なら像の一部として写像されるので収まる。**
///
/// **単一のプロセスが 1 本だけ使う**ので、静的でも取り合いは起きない。
static mut EDITOR: Buffer = Buffer::new();
/// 読み込みの受け皿。**同じ理由で静的である。**
static mut CONTENTS: [u8; MAX_LINES * MAX_LINE_LEN] = [0; MAX_LINES * MAX_LINE_LEN];

/// 行の集まり。**固定配列で持つ。**
struct Buffer {
    lines: [[u8; MAX_LINE_LEN]; MAX_LINES],
    lengths: [usize; MAX_LINES],
    count: usize,
}

impl Buffer {
    const fn new() -> Self {
        Self {
            lines: [[0; MAX_LINE_LEN]; MAX_LINES],
            lengths: [0; MAX_LINES],
            count: 1,
        }
    }

    /// 行の中身。
    fn line(&self, row: usize) -> &[u8] {
        &self.lines[row][..self.lengths[row]]
    }

    /// 1 バイトを挿入する。**入らなければ落とす**（入っていないものを
    /// 入ったように見せない。`zash` の行編集と同じ判断）。
    fn insert(&mut self, row: usize, at: usize, byte: u8) -> bool {
        let length = self.lengths[row];
        if length >= MAX_LINE_LEN || at > length {
            return false;
        }
        let line = &mut self.lines[row];
        line.copy_within(at..length, at + 1);
        line[at] = byte;
        self.lengths[row] = length + 1;
        true
    }

    /// 行を割る（zi-f。インサートモードの Enter）。
    ///
    /// **`at` 以降を次の行へ移す。** **入らなければ何もしない**
    /// ——**行数の上限（[`MAX_LINES`]）に当たった形である。**
    ///
    /// **`insert` と同じ判断である**——**入っていないものを入ったように
    /// 見せない。** **断ったことは呼ぶ側がコマンド行へ出す。**
    fn split_line(&mut self, row: usize, at: usize) -> bool {
        if self.count >= MAX_LINES || row >= self.count || at > self.lengths[row] {
            return false;
        }
        // **下の行を 1 つずつ下げる。** 上限に当たらないことは上で見た。
        for index in (row + 1..self.count).rev() {
            let (source, target) = (index, index + 1);
            let line = self.lines[source];
            self.lines[target] = line;
            self.lengths[target] = self.lengths[source];
        }
        let length = self.lengths[row];
        let tail = length - at;
        let mut moved = [0u8; MAX_LINE_LEN];
        moved[..tail].copy_from_slice(&self.lines[row][at..length]);
        self.lines[row + 1] = moved;
        self.lengths[row + 1] = tail;
        self.lengths[row] = at;
        self.count += 1;
        true
    }

    /// 次の行を末尾へ繋げる（zi-f。行頭の Backspace）。
    ///
    /// **入らなければ何もしない**（1 行の上限に当たった形である）。
    ///
    /// # `zi` の締めは「行の連結」を実装しないと書いていた
    ///
    /// **書いたのは `zi-d` の範囲としてである。** **zi-f で作ることにした**
    /// ——**行頭の Backspace が何もしないと、打ち間違いを直せない場面が
    /// 残る**（1 行目まで戻って消すしかない）。**運用者の指摘が利用者である。**
    fn join_with_next(&mut self, row: usize) -> bool {
        if row + 1 >= self.count {
            return false;
        }
        let length = self.lengths[row];
        let next = self.lengths[row + 1];
        if length + next > MAX_LINE_LEN {
            return false;
        }
        let tail = self.lines[row + 1];
        self.lines[row][length..length + next].copy_from_slice(&tail[..next]);
        self.lengths[row] = length + next;
        // **下の行を 1 つずつ上げる。**
        for index in row + 1..self.count - 1 {
            let line = self.lines[index + 1];
            self.lines[index] = line;
            self.lengths[index] = self.lengths[index + 1];
        }
        self.count -= 1;
        self.lengths[self.count] = 0;
        true
    }

    /// 1 バイト消す。**行末では何もしない**（`x` は行を繋げない）。
    fn remove(&mut self, row: usize, at: usize) -> bool {
        let length = self.lengths[row];
        if at >= length {
            return false;
        }
        let line = &mut self.lines[row];
        line.copy_within(at + 1..length, at);
        self.lengths[row] = length - 1;
        true
    }
}

/// 読み込んだバイト列を行へ割る。**上限を越えたら偽を返す**（切り詰めない）。
fn split_into_lines(bytes: &[u8], buffer: &mut Buffer) -> bool {
    buffer.count = 0;
    let mut row = 0usize;
    let mut length = 0usize;
    for byte in bytes {
        if *byte == b'\n' {
            if row >= MAX_LINES {
                return false;
            }
            buffer.lengths[row] = length;
            row += 1;
            length = 0;
            continue;
        }
        if row >= MAX_LINES || length >= MAX_LINE_LEN {
            return false;
        }
        buffer.lines[row][length] = *byte;
        length += 1;
    }
    // **末尾に改行が無い分も 1 行である。** 空のファイルは 1 行（空行）になる。
    if length > 0 {
        if row >= MAX_LINES {
            return false;
        }
        buffer.lengths[row] = length;
        row += 1;
    }
    buffer.count = if row == 0 { 1 } else { row };
    if buffer.count == 1 && row == 0 {
        buffer.lengths[0] = 0;
    }
    true
}

/// 10 進の数を桁で書き出す（`u32` まで）。**`userlib` に整数の出力は無い。**
fn write_number(out: &mut [u8; 12], value: usize) -> usize {
    if value == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut digits = [0u8; 12];
    let mut count = 0usize;
    let mut left = value;
    while left > 0 {
        digits[count] = b'0' + (left % 10) as u8;
        left /= 10;
        count += 1;
    }
    for index in 0..count {
        out[index] = digits[count - 1 - index];
    }
    count
}

/// カーソルを 1 起点の CUP で動かす。
fn move_cursor(row: usize, col: usize) {
    let mut sequence = [0u8; 32];
    let mut at = 0usize;
    sequence[at] = ESC;
    at += 1;
    sequence[at] = b'[';
    at += 1;
    let mut digits = [0u8; 12];
    let count = write_number(&mut digits, row + 1);
    sequence[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    sequence[at] = b';';
    at += 1;
    let count = write_number(&mut digits, col + 1);
    sequence[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    sequence[at] = b'H';
    at += 1;
    write_all(STDOUT, &sequence[..at]);
}

/// 画面の形と、開いているファイル（e-4）。**起動時に決まり、以後変わらない。**
struct View<'a> {
    /// 画面の行数。**`ioctl(TIOCGWINSZ)` が答えた値である**（0 なら既定へ落ちる。
    /// `userlib::window_size_or_default`）。
    rows: usize,
    /// 本文の行数。**破壊（`zi-status-below-text`）だけが使う**——
    /// **訊いた行数を使わない形が、どこへ置くかを決めるために要る。**
    #[cfg_attr(not(zi_status_below_text), allow(dead_code))]
    text_lines: usize,
    /// 開いているファイルのパス。**状態行に出す。**
    path: &'a [u8],
}

impl View<'_> {
    /// 状態行の行（下から 2 行目。e-4）。
    fn status_row(&self) -> usize {
        // 破壊 (e-4, zi-status-below-text): 訊いた行数を使わず、本文の 1 行下へ置く
        // （ES-d までの形）。**画面の下端に在ることの判定だけが落ちる**——
        // **色も札も中身も変わらない。** **`ioctl` が答えた値を実際に使って
        // いることの主張が、これで初めて偽になる。**
        #[cfg(zi_status_below_text)]
        {
            return self.text_lines + 1;
        }
        #[cfg(not(zi_status_below_text))]
        {
            self.rows - 2
        }
    }

    /// コマンド行の行（最下行。e-4）。
    fn command_row(&self) -> usize {
        self.rows - 1
    }

    /// 本文に使える行数（e-4）。
    ///
    /// **`rows - 2` である。** **0 や負にならない**——`rows` は
    /// `window_size_or_default` を通っており、**0 なら既定の 24 へ落ちる**
    /// （`userlib::WindowSize::or_default`）。**24 でも 22 行が残る。**
    fn text_rows(&self) -> usize {
        self.rows - 2
    }
}

/// いま画面へ出す状態（e-4）。**移ろう側をまとめてある。**
struct Status<'a> {
    mode: Mode,
    row: usize,
    col: usize,
    /// 保存していない変更があるか。
    dirty: bool,
    /// コマンド行に出す語（`:` を除く）。**コマンド中でなければ空である。**
    command: &'a [u8],
    in_command: bool,
    /// コマンド行に出す報せ（e-5）。**コマンド中でないときに出る。**
    ///
    /// **使う人へのものである**——断った理由、知らないコマンド、
    /// 行が一杯であること。**空なら何も出さない。**
    message: &'a [u8],
}

/// 状態行を描く（ES-d。e-4 で下から 2 行目へ移し、中身を増やした）。
///
/// # 画面の下端に置く
///
/// **e-1 で `ioctl(TIOCGWINSZ)` が入るまで、行数を知る道が無かった**ので、
/// 本文の 1 行下に置いていた。**いまは訊ける**ので、vi と同じ下端へ置く。
///
/// # 色が付くのはモードの札だけである
///
/// **ファイル名・位置・変更の印は既定の色で出す。** **判定は色の付いた
/// 連なりを読む**（`kernel/src/console/probe.rs`）ので、**位置のような
/// 動く値を色の中へ入れると、札の並びを見る判定が揺れる。**
///
/// # カーソルは戻さない（e-3）
///
/// **戻すのは [`restore_cursor`] だけである。** **描く関数が各自で戻す形は、
/// 描く場所が増えるたびに書き忘れが画面の誤りになる**（e-2 で順序依存が出た）。
/// **この関数を呼ぶ側は [`refresh`] を通すこと。**
fn draw_status(view: &View, status: &Status) {
    let mode = status.mode;
    // 破壊 (ES-d, zi-status-freeze-mode): モードが変わっても NORMAL のまま描く。
    // **色も位置も長さも変わらない**ので、「状態行が自分の色で描かれている」
    // 判定は緑のままである。**落ちるのは「モードに従って変わる」判定だけ**で、
    // **その形でしか落ちない**（`docs/verification-coverage.md` の破壊を足す基準）。
    #[cfg(zi_status_freeze_mode)]
    let mode = {
        let _ = mode;
        Mode::Normal
    };
    let label = match mode {
        Mode::Normal => STATUS_NORMAL,
        Mode::Insert => STATUS_INSERT,
        Mode::Command => STATUS_COMMAND,
    };
    move_cursor(view.status_row(), 0);
    // EL(2): 前の中身を消してから置く（位置の桁数が減ったときに残さない）。
    write_all(STDOUT, b"\x1b[2K");
    // **1 回で書く**（`zash` の `write_prompt` と同じ理由。色の無い札を
    // 一瞬でも出さない）。
    let mut out = [0u8; 160];
    let mut at = 0usize;
    for part in [STATUS_COLOR, label, SGR_RESET, b" "] {
        out[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    // **ファイル名。** NUL 終端の手前まで。
    let name = {
        let end = view
            .path
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(view.path.len());
        &view.path[..end]
    };
    let take = name.len().min(out.len() - at - 32);
    out[at..at + take].copy_from_slice(&name[..take]);
    at += take;
    // **位置は 1 起点で出す**（vi と同じ。使う人が見る数である）。
    out[at] = b' ';
    at += 1;
    let mut digits = [0u8; 12];
    let count = write_number(&mut digits, status.row + 1);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at] = b':';
    at += 1;
    let count = write_number(&mut digits, status.col + 1);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    // **保存していない変更の印。** vi の `[+]` と同じ形である。
    if status.dirty {
        let mark = b" [+]";
        out[at..at + mark.len()].copy_from_slice(mark);
        at += mark.len();
    }
    write_all(STDOUT, &out[..at]);
}

/// コマンド行（エコーエリア）を描く（e-4）。**最下行である。**
///
/// # 打っている途中が見える
///
/// **`:` を打った時点で `:` が出て、`w` を打てば `:w` になる。**
/// **打ち終わるまで何も見えない形は、打ち間違いに気づけない**
/// （運用者の指摘）。
///
/// # コマンド中でなければ空にする
///
/// # 報せの出し先である（e-5）
///
/// **`:q` を拒んだ理由、知らないコマンド、行が一杯であること**を、ここへ出す。
/// **`STDERR` へ出していたものを移した**——**`zi` は代替画面に居るので、
/// `STDERR` は「使う人が見る画面」ではない**（診断は検査の構成でしか出ない）。
/// **e-4 で作った口に、利用者がここで来た。**
fn draw_command_line(view: &View, status: &Status) {
    move_cursor(view.command_row(), 0);
    write_all(STDOUT, b"\x1b[2K");
    if !status.in_command {
        // **報せを出す（e-5）。** **コマンド中はそちらが優先である**
        // ——打っている途中を消さない。
        if !status.message.is_empty() {
            write_all(STDOUT, status.message);
        }
        return;
    }
    let mut out = [0u8; COMMAND_MAX + 1];
    out[0] = b':';
    let take = status.command.len().min(COMMAND_MAX);
    out[1..1 + take].copy_from_slice(&status.command[..take]);
    write_all(STDOUT, &out[..1 + take]);
}

/// 代替画面バッファへ入る（e-3。`?1049h`）。
///
/// # なぜ要るのか
///
/// **全画面のアプリは、抜けた後に元の画面を返すべきである。**
/// **`zi` が終わった後、編集していた本文が残り、シェルが `zi` のカーソル位置
/// から続いていた**（運用者の目視。ES 段の締めの限界の節）。
///
/// **戻す仕事はカーネル側にある**（ADR-0040 の Addendum）——
/// **Ring 3 には画面を読み戻す手段が無い。** `zi` は入る / 出るを告げるだけである。
fn enter_screen() {
    write_all(STDOUT, b"\x1b[?1049h");
}

/// 代替画面バッファから出る（e-3。`?1049l`）。**終わるすべての道で呼ぶ。**
///
/// **入った後に終わる道は 3 つある**——`:q` / `:wq` の `exit(0)`、
/// `:w` の失敗の `exit(5)`、端末が読めなくなったときの `break` である。
/// **入る前に終わる道（引数が無い・開けない・読めない・大きすぎる）では
/// 呼ばない**——**まだ入っていないので、戻す面が無い。**
fn leave_screen() {
    write_all(STDOUT, b"\x1b[?1049l");
}

/// 描き終わりにカーソルを編集位置へ戻す（e-3）。
///
/// # 責務を1つにした
///
/// **描く場所が増えるたびに「最後にカーソルを戻す」を書き足す形は、
/// 書き忘れがそのまま画面の誤りになる。** **実際に e-2 で順序依存が出た**
/// ——状態行を後から描くと、カーソルが状態行の隣に残る。
///
/// **描く関数はカーソルを戻さない。** **戻すのはここだけである。**
/// **e-4 で下から2行目と最下行の2本になっても、増えるのはこの関数の
/// 中身だけで済む。**
fn restore_cursor(cursor_row: usize, cursor_col: usize) {
    move_cursor(cursor_row, cursor_col);
}

/// 2 本（状態行とコマンド行）を描き、最後にカーソルを戻す（e-3。e-4 で 2 本になった）。
///
/// **画面を更新する入口である。** **順序はここが持つ**ので、呼ぶ側は考えない。
fn refresh(view: &View, status: &Status) {
    draw_status(view, status);
    draw_command_line(view, status);
    restore_cursor(status.row, status.col);
}

/// 画面を描き直す。**全面を消してから行ごとに置く。**
///
/// **消してから描くので、前の内容が残らない。** 1 行ずつ CUP で置くのは、
/// 行の折り返しに依らず「バッファの行 = 画面の行」を保つためである。
///
/// **状態行もここで描き直す（ES-d）**——`ED(2)` が消してしまうためである。
fn redraw(view: &View, buffer: &Buffer, status: &Status) {
    // ED(2): 画面全体を消す。**カーソルは動かない**ので、この後に CUP を出す。
    write_all(STDOUT, b"\x1b[2J");
    // **本文に使える行までしか描かない（e-4）。**
    // **スクロールは作らない**——**上限を越えるファイルは開かずに拒む**ので
    // （`MAX_LINES` = 64）、**画面が 66 行以上あれば全部入る。** 足りない画面では
    // 後ろが見えないままになる。**利用者が来たら作る。**
    for row in 0..buffer.count.min(view.text_rows()) {
        move_cursor(row, 0);
        // EL(2): その行を消してから置く（消し残しを作らない）。
        write_all(STDOUT, b"\x1b[2K");
        let line = buffer.line(row);
        if !line.is_empty() {
            write_all(STDOUT, line);
        }
    }
    refresh(view, status);
}

/// 判定行を出す。**内部状態であって画面ではない**（モジュール doc の限界）。
///
/// # 検査の構成でしか出さない（zi-e 前の手当て）
///
/// **`write` はシリアルと前景コンソールの両方へ届く**（`sys_write` は fd 1 と
/// fd 2 を区別しない）。**したがって診断行は画面にも描かれ、カーソルの居る
/// 行の本文を上書きする。** **実測で、本文の4行すべてが判定行に化けていた。**
///
/// **通常の起動では一切出さない。** 台本で駆動する構成（kernel の `zi-test`
/// feature が `zi_diagnostics` として届く）でだけ出す。**判定はその構成で
/// 走るので、主張は保たれる。**
///
/// **これは応急である。** **根の手当ては「診断の出口を画面と分ける」
/// （たとえば fd 2 をシリアル専用にする）で、`deferred-decisions.md` に
/// 行がある**——**Cの移植で `stderr` が来るので、どのみち決める必要がある。**
#[cfg(zi_diagnostics)]
fn report_cursor(buffer: &Buffer, row: usize, col: usize, tag: &[u8]) {
    let mut out = [0u8; 96];
    let mut at = 0usize;
    let head = b"zi: cursor (buffer state, not the screen) row=";
    out[at..at + head.len()].copy_from_slice(head);
    at += head.len();
    let mut digits = [0u8; 12];
    let count = write_number(&mut digits, row);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at..at + 5].copy_from_slice(b" col=");
    at += 5;
    let count = write_number(&mut digits, col);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at..at + 7].copy_from_slice(b" lines=");
    at += 7;
    let count = write_number(&mut digits, buffer.count);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at] = b' ';
    at += 1;
    let take = tag.len().min(out.len() - at - 1);
    out[at..at + take].copy_from_slice(&tag[..take]);
    at += take;
    out[at] = b'\n';
    at += 1;
    write_all(userlib::STDERR, &out[..at]);
}

/// 判定行を出さない側（既定のビルド。上の doc を参照）。
#[cfg(not(zi_diagnostics))]
fn report_cursor(_buffer: &Buffer, _row: usize, _col: usize, _tag: &[u8]) {}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(stack: *const u64) -> ! {
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let Some(pointer) = (unsafe { userlib::argument(stack, 1) }) else {
        write_all(STDERR, USAGE);
        exit(2);
    };

    let mut path = [0u8; PATH_MAX];
    // SAFETY: `argv` の要素はカーネルが NUL 終端で積んだ文字列である。
    let length = unsafe { length_of(pointer, PATH_MAX - 1) };
    for index in 0..length {
        // SAFETY: 上で数えた長さの範囲である。
        path[index] = unsafe { *pointer.add(index) };
    }
    path[length] = 0;

    // === 読み込み。**読み切って閉じる**（zi-c の契約。モジュール doc） ===
    //
    // **無いパスは「新しいファイル」である（e-5）。** **空のバッファで始め、
    // `:w` が `O_CREAT` で作る**（vi と同じ形）。**開けない理由が「無い」以外
    // なら、従来どおり断って終わる**——**権限も何も無いこの体制では、
    // ここへ来るのは像の側の失敗である。**
    let fd = open_read_only(&path[..length + 1]);
    let new_file = fd == userlib::MINUS_ENOENT;
    if fd < 0 && !new_file {
        write_all(STDERR, OPEN_FAILED);
        exit(1);
    }
    let fd = if new_file { 0 } else { fd as u64 };

    // SAFETY: このプロセスは単一の実行文脈で、`CONTENTS` を触るのはここだけである。
    let contents: &mut [u8; MAX_LINES * MAX_LINE_LEN] =
        unsafe { &mut *core::ptr::addr_of_mut!(CONTENTS) };
    let mut total = 0usize;
    let mut chunk = [0u8; CHUNK];
    let mut overflowed = false;
    // **新しいファイルは読まない（e-5）。** **fd を開いていない。**
    while !new_file {
        let got = read(fd, &mut chunk);
        if got < 0 {
            close(fd);
            write_all(STDERR, READ_FAILED);
            exit(3);
        }
        if got == 0 {
            break;
        }
        let got = got as usize;
        if total + got > contents.len() {
            overflowed = true;
            break;
        }
        contents[total..total + got].copy_from_slice(&chunk[..got]);
        total += got;
    }
    // **新しいファイルでは開いていないので閉じない（e-5）。**
    if !new_file {
        close(fd);
    }

    // SAFETY: 上と同じ。`EDITOR` を触るのはこの 1 本だけである。
    let buffer: &mut Buffer = unsafe { &mut *core::ptr::addr_of_mut!(EDITOR) };
    // **上限を越えたら開かずに拒む。** 切り詰めて保存すると、開いた時点で
    // 中身が消える——**それは編集ではなく破壊である。**
    if overflowed || !split_into_lines(&contents[..total], buffer) {
        write_all(STDERR, TOO_BIG);
        exit(4);
    }

    // **検査の構成でしか出さない**（[`report_cursor`] と同じ理由。
    // **これも診断であって、使う人に要る行ではない**）。
    #[cfg(zi_diagnostics)]
    write_all(STDERR, b"zi: ready\n");

    // **端末の大きさを訊く（e-1）。** **使うのは e-4 の2本立てである**——
    // **いまは受け取って判定行に出すだけで、置き場所には使っていない**
    // （状態行は本文の1行下のままである）。
    // **訊く経路が本物の利用者を持たないと、検算が置けない。**
    report_window_size(userlib::window_size(0));
    // **使う値は既定へ落とした側である（e-4）。** **判定は落とす前を見る**
    // （上の行）——落とした後を見ると、訊けた場合と落ちた場合が同じ値になる。
    let window = userlib::window_size_or_default(0);

    // **代替画面バッファへ入る（e-3）。** **ここから先の描画は代替の面に載り、
    // 出るときに元の画面が戻る。** **読み込みが済んで、確実に編集へ入る時点で
    // 入る**——**入る前に終わる道では、戻す面が無い。**
    enter_screen();

    let mut mode = Mode::Normal;
    let mut row = 0usize;
    let mut col = 0usize;
    let mut escape = Escape::Idle;
    // **`:` の後に溜める語と、変更があったか（zi-d-2）。**
    let mut command = [0u8; COMMAND_MAX];
    let mut command_len = 0usize;
    let mut dirty = false;
    // **コマンド行に出す報せ（e-5）。** **次の打鍵まで残す**——vi と同じで、
    // **出した瞬間に消えると読めない。**
    let mut message: &'static [u8] = b"";
    // **画面の形を訊く（e-4）。** **0 なら既定へ落ちる**
    // （`userlib::window_size_or_default`。**その形がここで初めて本番で効く**）。
    let view = View {
        rows: window.rows as usize,
        text_lines: buffer.count,
        path: &path[..length + 1],
    };
    redraw(
        &view,
        buffer,
        &Status {
            mode,
            row,
            col,
            dirty,
            command: &command[..command_len],
            in_command: false,
            message,
        },
    );
    report_cursor(buffer, row, col, b"start");
    // **いま状態行に出ている札のモード（ES-d）。**
    let mut shown_mode = mode;

    loop {
        // **モードが変わっていたら状態行を描き直す（ES-d）。**
        //
        // **読む直前に見る。** モードを変える場所は 4 つある（`i`・Esc・`:`・
        // コマンドの実行）が、**そのどれもが最後にここへ戻る**ので、
        // **`continue` が何本あっても漏れない。**
        // **`-EAGAIN` で回っている間は変わらない**ので、何度も描かない。
        if mode != shown_mode {
            refresh(
                &view,
                &Status {
                    mode,
                    row,
                    col,
                    dirty,
                    command: &command[..command_len],
                    in_command: mode == Mode::Command,
                    message,
                },
            );
            shown_mode = mode;
        }
        let mut byte = [0u8; 1];
        let got = read(0, &mut byte);
        if got == MINUS_EAGAIN {
            // **溜めた Esc をここで確定する（e-2）。** **入力が途切れたので、
            // CSI の途中ではありえない**（[`finish_pending_escape`] の doc）。
            //
            // 破壊 (e-2, zi-esc-needs-second-key): ここで確定しない。
            // **溜めた Esc は次の 1 バイトが来るまで残る**ので、
            // **使う人は Esc を 2 回押すことになる**（e-2 で直した当の形である）。
            // **落ちるのは「Esc 1 回で戻る」判定だけである**——台本の残りは
            // 次のバイトで確定するので、往復も本数も変わらない。
            #[cfg(not(zi_esc_needs_second_key))]
            if escape == Escape::Esc {
                escape = Escape::Idle;
                finish_pending_escape(
                    &view,
                    buffer,
                    row,
                    &mut col,
                    &mut mode,
                    &mut shown_mode,
                    dirty,
                );
            }
            // **溜まっていない。** 回して待つ（`zash` と同じ形）。
            continue;
        }
        if got <= 0 {
            // 端末が読めない。**この段では終わる**（`:q` は zi-d-2）。
            break;
        }
        let byte = byte[0];

        // **3 バイトの状態機械を先に通す**（`zash` と同じ形）。
        match (escape, byte) {
            (Escape::Idle, ESC) => {
                escape = Escape::Esc;
                continue;
            }
            (Escape::Esc, b'[') => {
                escape = Escape::Bracket;
                continue;
            }
            // **`\x1b[3` は Delete の途中である（zi-f）。**
            (Escape::Bracket, b'3') => {
                escape = Escape::Tilde;
                continue;
            }
            (Escape::Tilde, terminator) => {
                escape = Escape::Idle;
                if terminator != b'~' {
                    // **知らない終端は捨てる。** 字として入れない。
                    continue;
                }
                // **Delete はカーソル位置の字を消す（zi-f）。**
                // **ノーマルの `x` と同じ動きだが、インサートでも効く。**
                let removed = buffer.remove(row, col);
                if removed {
                    // **行末を越えたら 1 つ左へ寄る**（ノーマルのみ。`x` と同じ）。
                    if mode == Mode::Normal {
                        let length = buffer.lengths[row];
                        if col >= length {
                            col = length.saturating_sub(1);
                        }
                    }
                    redraw_here(&view, buffer, mode, row, col, b"");
                    report_cursor(buffer, row, col, b"delete");
                    dirty = true;
                }
                continue;
            }
            (Escape::Bracket, direction) => {
                escape = Escape::Idle;
                let moved = match direction {
                    // 破壊 (zi-d-1, zi-cursor-ignore-updown): 上下を捨てる。
                    // **カーソルが行を移らないので、編集が別の行に入る**——
                    // 台本の判定（row の推移）が捕まえる。
                    #[cfg(not(zi_cursor_ignore_updown))]
                    b'A' => move_up(buffer, &mut row, &mut col),
                    #[cfg(not(zi_cursor_ignore_updown))]
                    b'B' => move_down(buffer, &mut row, &mut col),
                    b'C' => move_right(buffer, row, &mut col, mode),
                    b'D' => move_left(&mut col),
                    // 知らない終端は捨てる。**字として入れない。**
                    _ => false,
                };
                if moved {
                    restore_cursor(row, col);
                    report_cursor(buffer, row, col, b"arrow");
                }
                continue;
            }
            (Escape::Esc, other) => {
                // **`[` が続かなかった。Esc 単体である**（確定。上の doc）。
                // **`-EAGAIN` の側と同じ確定を通る（e-2）。**
                escape = Escape::Idle;
                finish_pending_escape(
                    &view,
                    buffer,
                    row,
                    &mut col,
                    &mut mode,
                    &mut shown_mode,
                    dirty,
                );
                // **溜めた Esc の次の字は、この周で扱い直す。**
                if other == ESC {
                    escape = Escape::Esc;
                    continue;
                }
                if !handle_byte(
                    &view,
                    other,
                    buffer,
                    &mut row,
                    &mut col,
                    &mut mode,
                    &mut message,
                ) {
                    continue;
                }
                continue;
            }
            (Escape::Idle, _) => {}
        }

        // **コマンド行は改行まで溜める（zi-d-2）。**
        if mode == Mode::Command {
            match byte {
                b'\n' => {
                    let (outcome, said) = run_command(
                        &command[..command_len],
                        &path[..length + 1],
                        buffer,
                        dirty,
                    );
                    message = said;
                    command_len = 0;
                    mode = Mode::Normal;
                    match outcome {
                        Command::Quit => {
                            leave_screen();
                            exit(0)
                        }
                        Command::Failed => {
                            leave_screen();
                            exit(5)
                        }
                        // **保存したら変更は無い。**
                        Command::Saved => dirty = false,
                        Command::Refused => {}
                    }
                    redraw(
                        &view,
                        buffer,
                        &Status {
                            mode,
                            row,
                            col,
                            dirty,
                            command: &command[..command_len],
                            in_command: false,
                            message,
                        },
                    );
                    report_cursor(buffer, row, col, b"command");
                }
                ESC => {
                    // **打ちかけを捨てる。** ノーマルへ戻る。
                    command_len = 0;
                    mode = Mode::Normal;
                }
                other => {
                    if command_len < command.len() {
                        command[command_len] = other;
                        command_len += 1;
                    }
                    // **打っている途中をそのまま出す（e-4）。**
                    // **打ち終わるまで何も見えない形は、打ち間違いに
                    // 気づけない**（運用者の指摘）。
                    //
                    // 破壊 (e-4, zi-command-line-silent): 打っている間は描き直さない。
                    // **`:` を打った時点の空のコマンド行のままになる**ので、
                    // **最下行に打鍵が出ていることの判定だけが落ちる。**
                    // **コマンド自身は効く**（改行で解釈するため）ので、
                    // 往復も保存も変わらない。
                    #[cfg(not(zi_command_line_silent))]
                    {
                        refresh(
                            &view,
                            &Status {
                                mode,
                                row,
                                col,
                                dirty,
                                command: &command[..command_len],
                                in_command: true,
                                message,
                            },
                        );
                    }
                }
            }
            continue;
        }

        // **`:` でコマンド行へ入る（ノーマルのときだけ）。**
        if mode == Mode::Normal && byte == b':' {
            mode = Mode::Command;
            command_len = 0;
            continue;
        }

        let changed = handle_byte(
            &view,
            byte,
            buffer,
            &mut row,
            &mut col,
            &mut mode,
            &mut message,
        );
        dirty |= changed;
    }

    leave_screen();
    exit(0);
}

/// バッファをファイルへ書き出す（`:w`。zi-d-2）。
///
/// **順序は zi-c の契約から決まる**（モジュール doc）——
/// `open(O_WRONLY|O_TRUNC)` で長さ 0 へ切ってから、全量を 1 回で書く。
///
/// **戻り値が要求と一致することを見る。** 一致しなければ、
/// **切った後に書けていない**ので内容が失われている。**黙らない。**
fn save(path: &[u8], buffer: &Buffer) -> bool {
    // SAFETY: このプロセスは単一の実行文脈で、`FLUSH_BUFFER` を触るのはここだけ。
    let out = unsafe { &mut *core::ptr::addr_of_mut!(FLUSH_BUFFER) };
    let mut at = 0usize;
    for row in 0..buffer.count {
        let line = buffer.line(row);
        out[at..at + line.len()].copy_from_slice(line);
        at += line.len();
        // **各行の後ろに改行を置く。** 読み込みの `split_into_lines` と対である。
        out[at] = b'\n';
        at += 1;
    }

    // **無ければ作る（e-5。`O_CREAT`）。** **在れば長さ 0 へ切る**——
    // **`:w` は全置換なので、どちらの道でも同じ状態から書き始める。**
    let fd = open_write_create(path);
    if fd < 0 {
        write_all(STDERR, b"zi: cannot open for writing\n");
        return false;
    }
    let fd = fd as u64;
    // 破壊 (zi-d-2, zi-write-skip-body): 中身を書かずに閉じる。
    // **open が長さ 0 へ切った後なので、ファイルが空のまま残る**——
    // `cat` の読み戻しが空になり、往復の判定が落ちる。
    // **`:w` の戻り値は「要求 0 に対して 0」になるので、量の判定は通る**
    // ——**捕まえるのは往復のほうである。**
    #[cfg(zi_write_skip_body)]
    let at = 0usize;
    let written = write_all(STDOUT_UNUSED_MARKER.min(fd), &out[..at]);
    close(fd);

    // **要求した長さと一致すること。** `write_all` は繰り返して全量を書くので、
    // 足りないのは誤りである。
    let ok = written == at as i64;
    report_save(at, written, ok);
    ok
}

/// `save` が `write_all` へ渡す fd の目印。**`fd` をそのまま使うための飾りである**
/// ——`min` は常に `fd` を返す（`fd` は 3 以上、この値は大きい）。
///
/// **なぜこう書くか**——`write_all` の引数を `fd` と読み違えないように、
/// 「端末ではない」ことを名前で示している。
const STDOUT_UNUSED_MARKER: u64 = u64::MAX;

/// 保存の判定行。**書いた量と要求した量を並べる。**
///
/// **検査の構成でしか出さない**（[`report_cursor`] と同じ理由）。
#[cfg(zi_diagnostics)]
fn report_save(requested: usize, written: i64, ok: bool) {
    let mut out = [0u8; 96];
    let mut at = 0usize;
    let head = b"zi: saved bytes requested=";
    out[at..at + head.len()].copy_from_slice(head);
    at += head.len();
    let mut digits = [0u8; 12];
    let count = write_number(&mut digits, requested);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at..at + 9].copy_from_slice(b" written=");
    at += 9;
    // **負の値は errno である。** 桁で書けないので、目印だけ置く。
    let count = if written < 0 {
        out[at] = b'-';
        at += 1;
        write_number(&mut digits, (-written) as usize)
    } else {
        write_number(&mut digits, written as usize)
    };
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    let tail: &[u8] = if ok { b" match=true\n" } else { b" match=false\n" };
    out[at..at + tail.len()].copy_from_slice(tail);
    at += tail.len();
    write_all(STDERR, &out[..at]);
}

/// 判定行を出さない側（既定のビルド）。
#[cfg(not(zi_diagnostics))]
fn report_save(_requested: usize, _written: i64, _ok: bool) {}

/// 端末の大きさの判定行（e-1）。**受け取った値をそのまま出す。**
///
/// **突き合わせる相手はカーネルが出す行である**——**期待値をこちらが持たない。**
/// **カーネルは自分の `Console` から桁と行を読んで出しており、こちらは
/// `ioctl` を通って受け取った値を出す。** **経路のどこかで入れ替われば食い違う。**
#[cfg(zi_diagnostics)]
fn report_window_size(size: Result<userlib::WindowSize, i64>) {
    let mut out = [0u8; 96];
    let mut at = 0usize;
    let head = b"zi: winsize rows=";
    out[at..at + head.len()].copy_from_slice(head);
    at += head.len();
    let mut digits = [0u8; 12];
    let (rows, columns) = match size {
        Ok(size) => (size.rows as usize, size.columns as usize),
        // **失敗は 0 として出す。** **判定は「カーネルの値と一致すること」なので、
        // 0 は一致しない**（画面が在る構成で走るためである）。
        Err(_) => (0, 0),
    };
    let count = write_number(&mut digits, rows);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at..at + 9].copy_from_slice(b" columns=");
    at += 9;
    let count = write_number(&mut digits, columns);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at] = b'\n';
    at += 1;
    write_all(STDERR, &out[..at]);
}

/// 判定行を出さない側（既定のビルド）。
#[cfg(not(zi_diagnostics))]
fn report_window_size(_size: Result<userlib::WindowSize, i64>) {}

/// Esc 単体を確定する（e-2）。**インサートならノーマルへ戻る。**
///
/// # 2 つの経路から呼ぶ
///
/// **次の字が来たとき**と、**`read` が `-EAGAIN` を返したとき**である。
/// **後者が本命で、前者は「Esc の次にすぐ字が来た」場合の受けである。**
///
/// # なぜ `-EAGAIN` で確定してよいのか
///
/// **カーネルが CSI の 3 バイトを不可分に届けるからである**
/// （`kernel/src/input.rs` の `bytes_for_event` の doc）。
/// **`\x1b` の直後の `read` が `-EAGAIN` を返したら、それは CSI の途中では
/// ありえない。** **本物の端末と違い、ESC タイムアウトの曖昧さが生じない。**
///
/// **e-2 まで、この規約は使われていなかった**——`zi` は次の 1 バイトが
/// 来るまで待っており、**Esc を 2 回押さないとノーマルへ戻れなかった**
/// （運用者の目視で出た）。
fn finish_pending_escape(
    view: &View,
    buffer: &Buffer,
    row: usize,
    col: &mut usize,
    mode: &mut Mode,
    shown_mode: &mut Mode,
    dirty: bool,
) {
    if *mode != Mode::Insert {
        return;
    }
    *mode = Mode::Normal;
    // **ノーマルへ戻ると、カーソルは 1 つ左へ寄る**（vi の形）。
    *col = col.saturating_sub(1);
    // **札を描いてからカーソルを戻す**（[`refresh`]）。
    // **順序はあちらが持つ**ので、ここでは考えない。
    refresh(
        view,
        &Status {
            mode: *mode,
            row,
            col: *col,
            dirty,
            command: &[],
            in_command: false,
            // **モードを戻すだけなので、報せは持たない。**
            message: &[],
        },
    );
    *shown_mode = *mode;
    report_cursor(buffer, row, *col, b"normal");
}

/// コマンド行を解釈する（zi-d-2）。**戻り値は「終わってよいか」である。**
///
/// **`:q` は変更があれば拒む。** `:q!` は入れない——**「変更を捨てる」の
/// 意思表示が要るが、最小には無くてよい**（拒まれたら `:wq` を使う）。
fn run_command(command: &[u8], path: &[u8], buffer: &Buffer, dirty: bool) -> (Command, &'static [u8]) {
    match command {
        b"w" => {
            if save(path, buffer) {
                (Command::Saved, b"written")
            } else {
                (Command::Failed, b"")
            }
        }
        b"q" => {
            if dirty {
                // **報せはコマンド行へ出す（e-5）。** **`STDERR` は
                // 「使う人が見る画面」ではない**——`zi` は代替画面に居る。
                (Command::Refused, b"unsaved changes; use :wq")
            } else {
                (Command::Quit, b"")
            }
        }
        b"wq" => {
            if save(path, buffer) {
                (Command::Quit, b"")
            } else {
                (Command::Failed, b"")
            }
        }
        _ => (Command::Refused, b"unknown command"),
    }
}

/// [`run_command`] の結果。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    /// 保存した。編集を続ける。
    Saved,
    /// 終わってよい。
    Quit,
    /// 断った（変更があるのに `:q`、または知らないコマンド）。
    Refused,
    /// 保存に失敗した。**終了状態 5 で終わる。**
    Failed,
}

/// ノーマル / インサートの 1 バイトを処理する。
///
/// **戻り値は「バッファを変えたか」である**（zi-d-2 で意味を変えた。
/// `:q` が変更の有無で拒むために要る）。
fn handle_byte(
    view: &View,
    byte: u8,
    buffer: &mut Buffer,
    row: &mut usize,
    col: &mut usize,
    mode: &mut Mode,
    message: &mut &'static [u8],
) -> bool {
    match *mode {
        // **コマンド行は呼び出し側で処理する**（主ループが `continue` する）。
        Mode::Command => false,
        Mode::Normal => {
            let moved = match byte {
                b'h' => move_left(col),
                b'j' => move_down(buffer, row, col),
                b'k' => move_up(buffer, row, col),
                b'l' => move_right(buffer, *row, col, *mode),
                b'i' => {
                    *mode = Mode::Insert;
                    restore_cursor(*row, *col);
                    report_cursor(buffer, *row, *col, b"insert");
                    // **モードを変えただけで、バッファは変わっていない。**
                    return false;
                }
                // **カーソルの次から挿入する（e-4。vi の `a`）。**
                //
                // **`i` との違いは桁が 1 つ右になることだけである。**
                // **行末では動かない**——インサートでは末尾の 1 つ先まで
                // 許すので、`move_right` と同じ上限に合わせる。
                // **`A` / `o` / `O` / `I` は作らない**（利用者が求めたのは
                // `a` である。使う者がいない機構は検算が置けない）。
                b'a' => {
                    *mode = Mode::Insert;
                    // 破壊 (e-4, zi-append-like-insert): `a` を `i` と同じにする。
                    // **モードは変わり、字も入る**ので、往復も本数も変わらない。
                    // **落ちるのは「`a` は `i` より 1 つ右から始まる」判定だけである。**
                    #[cfg(not(zi_append_like_insert))]
                    {
                        if *col < buffer.lengths[*row] {
                            *col += 1;
                        }
                    }
                    restore_cursor(*row, *col);
                    // **札は `insert` と分ける**——**判定が `i` と `a` を
                    // 見分けるためである**（`a` は直前の位置より 1 つ右）。
                    report_cursor(buffer, *row, *col, b"append");
                    return false;
                }
                b'x' => {
                    let removed = buffer.remove(*row, *col);
                    if removed {
                        // **行末を越えたら 1 つ左へ寄る**（vi の形）。
                        let length = buffer.lengths[*row];
                        if *col >= length {
                            *col = length.saturating_sub(1);
                        }
                        redraw(
                            view,
                            buffer,
                            &Status {
                                mode: *mode,
                                row: *row,
                                col: *col,
                                // **消した直後なので、変更は確実にある。**
                                dirty: true,
                                command: &[],
                                in_command: false,
                                message: &[],
                            },
                        );
                        report_cursor(buffer, *row, *col, b"delete");
                    }
                    return removed;
                }
                // 知らないキーは黙って捨てる。**`:` は zi-d-2 で受ける。**
                _ => false,
            };
            if moved {
                restore_cursor(*row, *col);
                report_cursor(buffer, *row, *col, b"move");
            }
            // **移動はバッファを変えない。**
            false
        }
        Mode::Insert => {
            // **Enter で行を割る（zi-f）。** vi と同じで、カーソル以降が
            // 新しい行へ移り、カーソルは新しい行の先頭へ行く。
            if byte == b'\n' {
                // 破壊 (zi-f, zi-enter-does-nothing): Enter を捨てる。
                // **zi-d-1 までの振る舞いに戻る**（あの頃は「行の追加は
                // 範囲外」として捨てていた）。**行が増えないので、読み戻しが
                // 2 行にならない**——**「enter split the line」だけが落ちる。**
                #[cfg(zi_enter_does_nothing)]
                return false;

                #[cfg(not(zi_enter_does_nothing))]
                if !buffer.split_line(*row, *col) {
                    *message = NO_ROOM_FOR_A_LINE;
                    redraw_here(view, buffer, *mode, *row, *col, message);
                    return false;
                }
                *row += 1;
                *col = 0;
                redraw_here(view, buffer, *mode, *row, *col, b"");
                report_cursor(buffer, *row, *col, b"split");
                return true;
            }
            // **Backspace（zi-f）。** 行頭なら前の行と繋げる。
            if byte == 0x08 {
                if *col > 0 {
                    let removed = buffer.remove(*row, *col - 1);
                    if removed {
                        *col -= 1;
                        redraw_here(view, buffer, *mode, *row, *col, b"");
                        report_cursor(buffer, *row, *col, b"erase");
                    }
                    return removed;
                }
                if *row == 0 {
                    // **1 行目の行頭では何もしない**（繋げる先が無い）。
                    return false;
                }
                let landing = buffer.lengths[*row - 1];
                if !buffer.join_with_next(*row - 1) {
                    *message = NO_ROOM_TO_JOIN;
                    redraw_here(view, buffer, *mode, *row, *col, message);
                    return false;
                }
                *row -= 1;
                *col = landing;
                redraw_here(view, buffer, *mode, *row, *col, b"");
                report_cursor(buffer, *row, *col, b"join");
                return true;
            }
            // 破壊 (zi-d-2, zi-insert-drop-first): 挿入の最初の 1 字を落とす。
            // **`cat` の読み戻しが 1 字短くなる**ので、往復の判定が捕まえる。
            // **画面の再描画も 1 字少ないが、それは観測できない**（モジュール doc）。
            #[cfg(zi_insert_drop_first)]
            let inserted = {
                use core::sync::atomic::{AtomicBool, Ordering};
                static DROPPED: AtomicBool = AtomicBool::new(false);
                if DROPPED.swap(true, Ordering::SeqCst) {
                    buffer.insert(*row, *col, byte)
                } else {
                    false
                }
            };
            #[cfg(not(zi_insert_drop_first))]
            let inserted = buffer.insert(*row, *col, byte);
            if inserted {
                *col += 1;
                redraw(
                    view,
                    buffer,
                    &Status {
                        mode: *mode,
                        row: *row,
                        col: *col,
                        // **入れた直後なので、変更は確実にある。**
                        dirty: true,
                        command: &[],
                        in_command: false,
                        message: &[],
                    },
                );
                report_cursor(buffer, *row, *col, b"typed");
            }
            inserted
        }
    }
}

/// 編集の後の描き直し（zi-f で切り出した）。
///
/// **`Status` を組み立てる形が 4 か所へ増えたので、1 つにまとめる。**
/// **変更があった直後にしか呼ばない**ので、`dirty` は常に真である。
fn redraw_here(
    view: &View,
    buffer: &Buffer,
    mode: Mode,
    row: usize,
    col: usize,
    message: &[u8],
) {
    redraw(
        view,
        buffer,
        &Status {
            mode,
            row,
            col,
            dirty: true,
            command: &[],
            in_command: false,
            message,
        },
    );
}

/// 左へ 1 つ。**行頭では動かない**（前の行の末尾へは回らない）。
fn move_left(col: &mut usize) -> bool {
    if *col == 0 {
        return false;
    }
    *col -= 1;
    true
}

/// 右へ 1 つ。**行末では動かない。**
///
/// **ノーマルでは最後の字の上まで、インサートでは末尾の 1 つ先まで**
/// 動ける（vi の形。挿入は末尾へ足せる）。
fn move_right(buffer: &Buffer, row: usize, col: &mut usize, mode: Mode) -> bool {
    let length = buffer.lengths[row];
    let limit = match mode {
        // **コマンド行では矢印が来ない**（主ループが先に処理する）。
        // ノーマルと同じ扱いにしておく。
        Mode::Normal | Mode::Command => length.saturating_sub(1),
        Mode::Insert => length,
    };
    if *col >= limit {
        return false;
    }
    *col += 1;
    true
}

/// 上へ 1 行。**先頭行では動かない。** 桁は移った行の長さで切り詰める。
fn move_up(buffer: &Buffer, row: &mut usize, col: &mut usize) -> bool {
    if *row == 0 {
        return false;
    }
    *row -= 1;
    clamp_column(buffer, *row, col);
    true
}

/// 下へ 1 行。**最終行では動かない。**
fn move_down(buffer: &Buffer, row: &mut usize, col: &mut usize) -> bool {
    if *row + 1 >= buffer.count {
        return false;
    }
    *row += 1;
    clamp_column(buffer, *row, col);
    true
}

/// 移った先の行の長さへ桁を寄せる。
fn clamp_column(buffer: &Buffer, row: usize, col: &mut usize) {
    let limit = buffer.lengths[row].saturating_sub(1);
    if *col > limit {
        *col = limit;
    }
}
