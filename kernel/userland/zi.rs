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
    close, exit, length_of, open_read_only, open_write_truncate, read, write_all, STDERR, STDOUT,
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
/// ES-b の判定色（緑と赤）・カーソルのシアン・`zash` のプロンプトのマゼンタの
/// いずれとも、RGB のどれかの軸で 150 以上離れている。**
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

/// 状態行を描く（ES-d）。**本文の 1 行下に、色を付けて置く。**
///
/// # 画面の下端ではなく本文の下である
///
/// **画面の大きさを訊く手段が無い**——`ioctl` も `TIOCGWINSZ` も無く、
/// **端末の行数を知る道が 1 つも無い**（`docs/deferred-decisions.md` の
/// 環境変数の行と同じ立場である）。**下端に置くには行数が要るので、
/// 本文の下に置く。** 端末が本物の vi のように見えないのはこのためである。
///
/// # カーソルを戻して終わる
///
/// **描いた後、編集位置へカーソルを戻す。** 戻さないと、次に打った字が
/// 状態行の隣へ出る——**カーソルは画面に見えている**（ES-c）ので、
/// **戻し忘れは目でも判定でも分かる。**
fn draw_status(buffer: &Buffer, mode: Mode, cursor_row: usize, cursor_col: usize) {
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
    move_cursor(buffer.count + 1, 0);
    // **1 回で書く**（`zash` の `write_prompt` と同じ理由。色の無い札を
    // 一瞬でも出さない）。
    let mut out = [0u8; STATUS_COLOR.len() + STATUS_NORMAL.len() + SGR_RESET.len()];
    let mut at = 0usize;
    for part in [STATUS_COLOR, label, SGR_RESET] {
        out[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    write_all(STDOUT, &out[..at]);
    move_cursor(cursor_row, cursor_col);
}

/// 画面を描き直す。**全面を消してから行ごとに置く。**
///
/// **消してから描くので、前の内容が残らない。** 1 行ずつ CUP で置くのは、
/// 行の折り返しに依らず「バッファの行 = 画面の行」を保つためである。
///
/// **状態行もここで描き直す（ES-d）**——`ED(2)` が消してしまうためである。
fn redraw(buffer: &Buffer, mode: Mode, cursor_row: usize, cursor_col: usize) {
    // ED(2): 画面全体を消す。**カーソルは動かない**ので、この後に CUP を出す。
    write_all(STDOUT, b"\x1b[2J");
    for row in 0..buffer.count {
        move_cursor(row, 0);
        // EL(2): その行を消してから置く（消し残しを作らない）。
        write_all(STDOUT, b"\x1b[2K");
        let line = buffer.line(row);
        if !line.is_empty() {
            write_all(STDOUT, line);
        }
    }
    draw_status(buffer, mode, cursor_row, cursor_col);
}

/// 判定行を出す。**内部状態であって画面ではない**（モジュール doc の限界）。
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
    let fd = open_read_only(&path[..length + 1]);
    if fd < 0 {
        write_all(STDERR, OPEN_FAILED);
        exit(1);
    }
    let fd = fd as u64;

    // SAFETY: このプロセスは単一の実行文脈で、`CONTENTS` を触るのはここだけである。
    let contents: &mut [u8; MAX_LINES * MAX_LINE_LEN] =
        unsafe { &mut *core::ptr::addr_of_mut!(CONTENTS) };
    let mut total = 0usize;
    let mut chunk = [0u8; CHUNK];
    let mut overflowed = false;
    loop {
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
    close(fd);

    // SAFETY: 上と同じ。`EDITOR` を触るのはこの 1 本だけである。
    let buffer: &mut Buffer = unsafe { &mut *core::ptr::addr_of_mut!(EDITOR) };
    // **上限を越えたら開かずに拒む。** 切り詰めて保存すると、開いた時点で
    // 中身が消える——**それは編集ではなく破壊である。**
    if overflowed || !split_into_lines(&contents[..total], buffer) {
        write_all(STDERR, TOO_BIG);
        exit(4);
    }

    write_all(STDERR, b"zi: ready\n");

    let mut mode = Mode::Normal;
    let mut row = 0usize;
    let mut col = 0usize;
    let mut escape = Escape::Idle;
    // **`:` の後に溜める語と、変更があったか（zi-d-2）。**
    let mut command = [0u8; COMMAND_MAX];
    let mut command_len = 0usize;
    let mut dirty = false;
    redraw(buffer, mode, row, col);
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
            draw_status(buffer, mode, row, col);
            shown_mode = mode;
        }
        let mut byte = [0u8; 1];
        let got = read(0, &mut byte);
        if got == MINUS_EAGAIN {
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
                    move_cursor(row, col);
                    report_cursor(buffer, row, col, b"arrow");
                }
                continue;
            }
            (Escape::Esc, other) => {
                // **`[` が続かなかった。Esc 単体である**（確定。上の doc）。
                escape = Escape::Idle;
                if mode == Mode::Insert {
                    mode = Mode::Normal;
                    // **ノーマルへ戻ると、カーソルは 1 つ左へ寄る**（vi の形）。
                    col = col.saturating_sub(1);
                    move_cursor(row, col);
                    report_cursor(&buffer, row, col, b"normal");
                }
                // **溜めた Esc の次の字は、この周で扱い直す。**
                if other == ESC {
                    escape = Escape::Esc;
                    continue;
                }
                if !handle_byte(other, buffer, &mut row, &mut col, &mut mode) {
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
                    let outcome = run_command(&command[..command_len], &path[..length + 1], buffer, dirty);
                    command_len = 0;
                    mode = Mode::Normal;
                    match outcome {
                        Command::Quit => exit(0),
                        Command::Failed => exit(5),
                        // **保存したら変更は無い。**
                        Command::Saved => dirty = false,
                        Command::Refused => {}
                    }
                    redraw(buffer, mode, row, col);
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

        let changed = handle_byte(byte, buffer, &mut row, &mut col, &mut mode);
        dirty |= changed;
    }

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

    let fd = open_write_truncate(path);
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

/// コマンド行を解釈する（zi-d-2）。**戻り値は「終わってよいか」である。**
///
/// **`:q` は変更があれば拒む。** `:q!` は入れない——**「変更を捨てる」の
/// 意思表示が要るが、最小には無くてよい**（拒まれたら `:wq` を使う）。
fn run_command(command: &[u8], path: &[u8], buffer: &Buffer, dirty: bool) -> Command {
    match command {
        b"w" => {
            if save(path, buffer) {
                Command::Saved
            } else {
                Command::Failed
            }
        }
        b"q" => {
            if dirty {
                write_all(STDERR, b"zi: unsaved changes; use :wq\n");
                Command::Refused
            } else {
                Command::Quit
            }
        }
        b"wq" => {
            if save(path, buffer) {
                Command::Quit
            } else {
                Command::Failed
            }
        }
        _ => {
            write_all(STDERR, b"zi: unknown command\n");
            Command::Refused
        }
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
    byte: u8,
    buffer: &mut Buffer,
    row: &mut usize,
    col: &mut usize,
    mode: &mut Mode,
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
                    move_cursor(*row, *col);
                    report_cursor(buffer, *row, *col, b"insert");
                    // **モードを変えただけで、バッファは変わっていない。**
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
                        redraw(buffer, *mode, *row, *col);
                        report_cursor(buffer, *row, *col, b"delete");
                    }
                    return removed;
                }
                // 知らないキーは黙って捨てる。**`:` は zi-d-2 で受ける。**
                _ => false,
            };
            if moved {
                move_cursor(*row, *col);
                report_cursor(buffer, *row, *col, b"move");
            }
            // **移動はバッファを変えない。**
            false
        }
        Mode::Insert => {
            // **改行は入れない**（行の追加は zi-d-1 の範囲外。`o` も同じ）。
            if byte == b'\n' || byte == 0x08 {
                return false;
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
                redraw(buffer, *mode, *row, *col);
                report_cursor(buffer, *row, *col, b"typed");
            }
            inserted
        }
    }
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
