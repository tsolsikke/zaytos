//! `sockc`: 名前で繋ぎ、`sockd` の返事を見る（`ADR-0064`）。
//!
//! # 本番の形の予行である
//!
//! **Wayland のクライアントと同じ形で繋ぐ**——**名前（既定は `wayland-0`）で `connect` し、
//! 書いて、返事を読む。** **`sockd` と対で、`socket-test` の台本が 6 つの形を 1 つずつ打つ。**
//! **Seinas が来たら、この組は検査に残す**（`sockd` の doc）。
//!
//! # 形（`argv[1]`）
//!
//! - `hello` 書いて返事を読む
//! - `big` `/data/big`（2,181 バイト）を 1 回の `write_all` で送り、同じ長さを読み戻して比べる。
//!   **輪（1,024）より長いので、書き手が満杯で待つ機会になる**
//! - `nobody` 無い名前へ繋ぐ（`-ECONNREFUSED`）
//! - `bind` `sockd` が取った名前を取ろうとする（`-EADDRINUSE`）
//! - `twice` 2 本繋いで、1 本目を閉じてから 2 本目で話す（待ち行列）
//! - `quit` `quit` を送り、相手が閉じた後の `read`（0）と `write`（`-EPIPE`）を見る
//!
//! **`argv[2]` が在ればそれを名前にする。**
//!
//! # 1 行は 1 回の `write` で出す
//!
//! **`sockd` の doc と同じ理由である。**
//!
//! # 終了状態の意味
//!
//! - `0` 形を打ち終えた（結果は行に出る。**失敗も行で言う**）
//! - `1` 形が無い／知らない形

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{bind, close, connect, exit, length_of, read, socket, write_all, STDOUT};

/// 名前の最大長（カーネルの `NAME_MAX` と同じ）。
const NAME_MAX: usize = 31;
/// 引数が無いときの名前（Wayland の既定の端点と同じ）。
const DEFAULT_NAME: &[u8] = b"wayland-0";
/// 返事の入れ物。
const REPLY_CAP: usize = 64;
/// 無い名前。
const NOBODY: &[u8] = b"nobody";

/// 1 行を組んで 1 回で出す。
struct Line {
    buf: [u8; 128],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self {
            buf: [0; 128],
            len: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.len < self.buf.len() {
                self.buf[self.len] = *byte;
                self.len += 1;
            }
        }
    }

    fn push_decimal(&mut self, value: i64) {
        if value < 0 {
            self.push(b"-");
        }
        let mut digits = [b'0'; 20];
        let mut length = 0usize;
        let mut rest = value.unsigned_abs();
        loop {
            digits[19 - length] = b'0' + (rest % 10) as u8;
            length += 1;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        self.push(&digits[20 - length..]);
    }

    fn end(mut self) {
        self.push(b"\n");
        write_all(STDOUT, &self.buf[..self.len]);
    }
}

/// 繋いだ fd を返す。**繋げなければ行に出して `None`。**
fn connected(name: &[u8]) -> Option<u64> {
    let fd = socket();
    if fd < 0 {
        let mut line = Line::new();
        line.push(b"sockc: socket failed ");
        line.push_decimal(fd);
        line.end();
        return None;
    }
    let fd = fd as u64;
    let result = connect(fd, name);
    if result < 0 {
        let mut line = Line::new();
        line.push(b"sockc: connect failed ");
        line.push_decimal(result);
        line.end();
        close(fd);
        return None;
    }
    Some(fd)
}

/// 書いて、返事を 1 回読む。**読めた長さと中身を行にする。**
fn ask(fd: u64, text: &[u8], line: &mut Line) {
    write_all(fd, text);
    let mut reply = [0u8; REPLY_CAP];
    let got = read(fd, &mut reply);
    if got < 0 {
        line.push(b" read=");
        line.push_decimal(got);
    } else {
        line.push(b" reply=");
        line.push(&reply[..got as usize]);
    }
}

fn mode_hello(name: &[u8]) {
    let Some(fd) = connected(name) else {
        return;
    };
    let mut line = Line::new();
    line.push(b"sockc: hello");
    ask(fd, b"hello", &mut line);
    line.end();
    close(fd);
}

/// `big` の 1 バイト目の値（`i` 番目は `(BIG_SEED + i) % 251`）。**251 は素数なので、
/// 輪（1,024）や 256 の境で模様が繰り返さない。**
const BIG_SEED: usize = 7;

fn big_byte(index: usize) -> u8 {
    ((BIG_SEED + index) % 251) as u8
}

/// `big`——輪（1,024）より大きい 1,536 バイトを 1 回の `write_all` で送り、読み戻して
/// バイト単位で比べる（`ADR-0064`）。
///
/// # 大きさ
///
/// **1,536 は輪より大きいので、`write_all` が満杯で待つ**（判定 2 の「書き手が待った」）。
/// **2 × 輪（2,048）より小さいので、`sockd` の折り返しと詰まらない**——**送り切ってから
/// 読むので、両方の輪が同時に満杯で双方が止まる形にならない**（`ADR-0064` の限界の裏返し）。
/// **入れ物は 1 つ（1,536）と読み戻しの 256 だけで、ユーザースタック 1 ページ（4,096）に収まる。**
fn mode_big(name: &[u8]) {
    const BIG_LEN: usize = 1536;
    let mut data = [0u8; BIG_LEN];
    for (index, byte) in data.iter_mut().enumerate() {
        *byte = big_byte(index);
    }

    let Some(fd) = connected(name) else {
        return;
    };
    let sent = write_all(fd, &data);

    let mut reply = [0u8; 256];
    let mut got_back = 0usize;
    let mut matched = true;
    while got_back < BIG_LEN {
        let got = read(fd, &mut reply);
        if got <= 0 {
            break;
        }
        for offset in 0..got as usize {
            if reply[offset] != big_byte(got_back + offset) {
                matched = false;
            }
        }
        got_back += got as usize;
    }
    let ok = matched && got_back == BIG_LEN && sent == BIG_LEN as i64;
    let mut line = Line::new();
    line.push(b"sockc: big sent=");
    line.push_decimal(sent);
    line.push(b" got=");
    line.push_decimal(got_back as i64);
    line.push(if ok { b" match=true" } else { b" match=false" });
    line.end();
    close(fd);
}

fn mode_nobody() {
    let fd = socket();
    let result = if fd < 0 { fd } else { connect(fd as u64, NOBODY) };
    let mut line = Line::new();
    line.push(b"sockc: connect nobody -> ");
    line.push_decimal(result);
    line.end();
    if fd >= 0 {
        close(fd as u64);
    }
}

fn mode_bind(name: &[u8]) {
    let fd = socket();
    let result = if fd < 0 { fd } else { bind(fd as u64, name) };
    let mut line = Line::new();
    line.push(b"sockc: bind ");
    line.push(name);
    line.push(b" -> ");
    line.push_decimal(result);
    line.end();
    if fd >= 0 {
        close(fd as u64);
    }
}

fn mode_twice(name: &[u8]) {
    let mut line = Line::new();
    line.push(b"sockc: twice");
    let Some(first) = connected(name) else {
        return;
    };
    // **2 本目は待ち行列で待つ**——`sockd` は 1 本目が閉じるまで `accept` しない。
    let second = socket();
    let second_result = if second < 0 {
        second
    } else {
        connect(second as u64, name)
    };
    line.push(b" second=");
    line.push_decimal(second_result);
    ask(first, b"one", &mut line);
    close(first);
    if second_result >= 0 {
        ask(second as u64, b"two", &mut line);
        close(second as u64);
    }
    line.end();
}

fn mode_quit(name: &[u8]) {
    let Some(fd) = connected(name) else {
        return;
    };
    write_all(fd, b"quit");
    // **相手が閉じるまで `read` は待ち、閉じたら 0 を返す。**
    let mut reply = [0u8; REPLY_CAP];
    let got = read(fd, &mut reply);
    let written = write_all(fd, b"more");
    let mut line = Line::new();
    line.push(b"sockc: quit read=");
    line.push_decimal(got);
    line.push(b" write=");
    line.push_decimal(written);
    line.end();
    close(fd);
}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(stack: *const u64) -> ! {
    let mut mode = [0u8; 16];
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let mode_len = match unsafe { userlib::argument(stack, 1) } {
        Some(pointer) => {
            // SAFETY: `argv` の要素はカーネルが NUL 終端で積んだ文字列である。
            let length = unsafe { length_of(pointer, mode.len()) };
            for index in 0..length {
                // SAFETY: 上で数えた長さの範囲である。
                mode[index] = unsafe { *pointer.add(index) };
            }
            length
        }
        None => {
            write_all(STDOUT, b"sockc: usage: sockc <hello|big|nobody|bind|twice|quit> [name]\n");
            exit(1);
        }
    };
    let mut name = [0u8; NAME_MAX];
    // SAFETY: 同上。
    let name_len = match unsafe { userlib::argument(stack, 2) } {
        Some(pointer) => {
            // SAFETY: 同上。
            let length = unsafe { length_of(pointer, NAME_MAX) };
            for index in 0..length {
                // SAFETY: 同上。
                name[index] = unsafe { *pointer.add(index) };
            }
            length
        }
        None => {
            name[..DEFAULT_NAME.len()].copy_from_slice(DEFAULT_NAME);
            DEFAULT_NAME.len()
        }
    };
    let name = &name[..name_len];

    match &mode[..mode_len] {
        b"hello" => mode_hello(name),
        b"big" => mode_big(name),
        b"nobody" => mode_nobody(),
        b"bind" => mode_bind(name),
        b"twice" => mode_twice(name),
        b"quit" => mode_quit(name),
        _ => {
            write_all(STDOUT, b"sockc: unknown mode\n");
            exit(1);
        }
    }
    exit(0);
}
