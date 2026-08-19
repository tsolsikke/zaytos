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
//! # zi-c の契約から来る順序（`:w` は zi-d-2）
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

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{close, exit, length_of, open_read_only, read, write_all, STDERR, STDOUT};

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

/// 編集中のモード。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
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

/// 画面を描き直す。**全面を消してから行ごとに置く。**
///
/// **消してから描くので、前の内容が残らない。** 1 行ずつ CUP で置くのは、
/// 行の折り返しに依らず「バッファの行 = 画面の行」を保つためである。
fn redraw(buffer: &Buffer, cursor_row: usize, cursor_col: usize) {
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
    move_cursor(cursor_row, cursor_col);
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
    redraw(buffer, row, col);
    report_cursor(buffer, row, col, b"start");

    loop {
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

        handle_byte(byte, buffer, &mut row, &mut col, &mut mode);
    }

    exit(0);
}

/// ノーマル / インサートの 1 バイトを処理する。戻り値は使わない側もある。
fn handle_byte(
    byte: u8,
    buffer: &mut Buffer,
    row: &mut usize,
    col: &mut usize,
    mode: &mut Mode,
) -> bool {
    match *mode {
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
                    return true;
                }
                b'x' => {
                    if buffer.remove(*row, *col) {
                        // **行末を越えたら 1 つ左へ寄る**（vi の形）。
                        let length = buffer.lengths[*row];
                        if *col >= length {
                            *col = length.saturating_sub(1);
                        }
                        redraw(buffer, *row, *col);
                        report_cursor(buffer, *row, *col, b"delete");
                    }
                    return true;
                }
                // 知らないキーは黙って捨てる。**`:` は zi-d-2 で受ける。**
                _ => false,
            };
            if moved {
                move_cursor(*row, *col);
                report_cursor(buffer, *row, *col, b"move");
            }
            true
        }
        Mode::Insert => {
            // **改行は入れない**（行の追加は zi-d-1 の範囲外。`o` も同じ）。
            if byte == b'\n' || byte == 0x08 {
                return true;
            }
            if buffer.insert(*row, *col, byte) {
                *col += 1;
                redraw(buffer, *row, *col);
                report_cursor(buffer, *row, *col, b"typed");
            }
            true
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
        Mode::Normal => length.saturating_sub(1),
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
