//! `gfxc`: 前景でない者が、画面と入力の fd を開けないことを見る（`ADR-0066` の Y-c）。
//!
//! # 起こしっぱなしの 1 本である
//!
//! **`gfxd` がスロット 1 へ起こす。** **前景を取れるのはスロット 0 の系統だけである**
//! （`crate::input::claim_foreground`）。**親が前景を持っていても、この 1 本は前景ではない。**
//!
//! # 2 つとも `-EBADF` のはずである
//!
//! **Y-a の `open_input` は大域の印を見ていたので、ここで開けてしまっていた**（Y-c の下調べで
//! 見つけた。`crate::input::caller_is_foreground` の doc）。**画面も入力も、同じ関所で断られる
//! ことを見る。**
//!
//! # 終了状態の意味
//!
//! - `0` 2 つとも断られた
//! - `1` どちらかが開けてしまった

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, open_input, open_screen, write_all, STDOUT};
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



/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(_stack: *const u64) -> ! {
    let screen = open_screen();
    let mut line = Line::new();
    line.push(b"gfxc: open_screen returned ");
    line.push_decimal(screen);
    line.end();
    let input = open_input();
    let mut line = Line::new();
    line.push(b"gfxc: open_input returned ");
    line.push_decimal(input);
    line.end();
    // **開けてしまったら 1 で終わる**（fd は終わりの表の解放で閉じる。**図形モードもそこで抜ける**）。
    if screen >= 0 || input >= 0 {
        exit(1);
    }
    exit(0)
}
