//! `pollc`: `polld` へ繋ぎ、合図を受けて返す（`ADR-0066` の Y-b）。
//!
//! # 待つ側を眠らせるためだけに居る
//!
//! **`polld` の 3 回の待ちのうち、2 回はこちらが起こす**——**繋ぐ（listener が読める）と、
//! 返す（接続が読める）である。** **時刻で間を作らない**——**`polld` の合図を待ってから
//! 返すので、順序が時間に依らない**（`ADR-0061` の「時間の判定を避ける」と同じ考え）。
//!
//! # 返した後も、閉じられるまで居残る
//!
//! **こちらが先に閉じると、接続が EOF になって「読める」になる**——**`poll` の 3 回目が
//! 打鍵ではなく EOF で起きてしまい、判定が測れない**（実測で踏んだ。2026-09-21）。
//! **`polld` が閉じるまで 2 度目の `read` で待つ。**
//!
//! # その待ちが、集合の不変条件を試す側でもある
//!
//! **3 回目の待ちの間、2 本が別の理由で待っている**——**`polld` は {入力, 接続}、こちらは
//! 接続だけである。** **打鍵で起こすとき、集合に入っていないこちらを起こしてはならない**
//! （`WOKEN_FOR_ANOTHER_REASON`。`ADR-0066` の Q3）。**破壊 `wake-ignores-the-reason` は
//! ここで落ちる。**
//!
//! # 終了状態の意味
//!
//! - `0` 合図を受けて返した
//! - `1` ソケットが作れなかった／繋げなかった
//! - `2` `read` か `write` が失敗した

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{close, connect, exit, read, socket, write_all, STDOUT};

/// 繋ぐ名前（`polld` と同じ）。
const NAME: &[u8] = b"poll-0";
/// 返す合図。
const PONG: &[u8] = b"pong";
/// 1 回に読む大きさ。
const CHUNK: usize = 32;

/// 1 行を組んで 1 回で出す（`sockd` と同じ形。`ADR-0063` の (b3)）。
struct Line {
    buf: [u8; 96],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self {
            buf: [0; 96],
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

fn say(text: &[u8]) {
    let mut line = Line::new();
    line.push(text);
    line.end();
}

fn fail(text: &[u8], code: i64, status: u64) -> ! {
    let mut line = Line::new();
    line.push(text);
    line.push_decimal(code);
    line.end();
    exit(status)
}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(_stack: *const u64) -> ! {
    let stream = socket();
    if stream < 0 {
        fail(b"pollc: socket failed ", stream, 1);
    }
    let stream = stream as u64;
    let connected = connect(stream, NAME);
    if connected < 0 {
        fail(b"pollc: connect failed ", connected, 1);
    }
    say(b"pollc: connected");

    let mut chunk = [0u8; CHUNK];
    let got = read(stream, &mut chunk);
    if got <= 0 {
        fail(b"pollc: read failed ", got, 2);
    }
    say(b"pollc: got the ping");
    let sent = write_all(stream, PONG);
    if sent < 0 {
        fail(b"pollc: write failed ", sent, 2);
    }
    say(b"pollc: sent the pong");

    // **相手が閉じるまで待つ**（モジュールの doc）。**0 が EOF である。**
    let got = read(stream, &mut chunk);
    if got < 0 {
        fail(b"pollc: second read failed ", got, 2);
    }
    let mut line = Line::new();
    line.push(b"pollc: the server closed (got ");
    line.push_decimal(got);
    line.push(b")");
    line.end();
    close(stream);
    exit(0)
}
