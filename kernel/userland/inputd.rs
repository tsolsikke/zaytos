//! `inputd`: 入力の生イベントを読む最初の利用者（`ADR-0066` の Y-a）。
//!
//! # 本番の形の予行である
//!
//! **本番の利用者（Seinas。コンポジタ）はまだ無い。** **この組は Wayland の形に合わせて
//! ある**——**前景のプロセスが入力の fd を開き、生キーイベント（`struct input_event`）を
//! 読む。** **`no_keymap`(0) を選べば、クライアントが生キーコードを解釈する**（`ADR-0066`）。
//!
//! # 打鍵で起きる
//!
//! **入力の fd を開き、`read` で待つ。** **溜まっていなければカーネルが `Wait::Keyboard` で
//! 眠らせ、本物の打鍵（`sendkey`）が起こす。** **起きたら生イベントを読み、印字する。**
//!
//! # `-EAGAIN` は回して待つ
//!
//! **待たない構成（破壊 `input-read-never-waits`）では `read` が `-EAGAIN` を返す。**
//! **そのときは回して待つ**——**シェルの `read(0)` と同じ形。** **打鍵は結局届くが、
//! カーネルは眠らないので「待った回数」が 0 になる**（判定が落ちる）。
//!
//! # 終了状態の意味
//!
//! - `0` 押下のイベントを 1 つ受けて終わった
//! - `1` 入力の fd が開けなかった
//! - `2` `read` が失敗した（`-EAGAIN` 以外）

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, open_input, read, write_all, INPUT_EVENT_LEN, STDOUT};

/// 1 回に読む大きさ（イベント 4 つ分）。
const CHUNK: usize = INPUT_EVENT_LEN * 4;
/// `-EAGAIN`。**待たない構成で回って待つための印。**
const MINUS_EAGAIN: i64 = -11;

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

/// 生イベントの `code`（`struct input_event` の位置 18。リトルエンディアン u16）。
fn event_code(event: &[u8]) -> u16 {
    u16::from_le_bytes([event[18], event[19]])
}

/// 生イベントの `value`（位置 20。リトルエンディアン i32）。
fn event_value(event: &[u8]) -> i32 {
    i32::from_le_bytes([event[20], event[21], event[22], event[23]])
}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(_stack: *const u64) -> ! {
    let fd = open_input();
    if fd < 0 {
        let mut line = Line::new();
        line.push(b"inputd: open_input failed ");
        line.push_decimal(fd);
        line.end();
        exit(1);
    }
    let fd = fd as u64;
    let mut line = Line::new();
    line.push(b"inputd: opened input fd ");
    line.push_decimal(fd as i64);
    line.end();

    let mut chunk = [0u8; CHUNK];
    loop {
        let got = read(fd, &mut chunk);
        if got == MINUS_EAGAIN {
            // **待たない構成では回して待つ**（`read(0)` と同じ）。
            continue;
        }
        if got < 0 {
            let mut line = Line::new();
            line.push(b"inputd: read failed ");
            line.push_decimal(got);
            line.end();
            exit(2);
        }
        let got = got as usize;
        let mut saw_press = false;
        let mut offset = 0usize;
        while offset + INPUT_EVENT_LEN <= got {
            let event = &chunk[offset..offset + INPUT_EVENT_LEN];
            let code = event_code(event);
            let value = event_value(event);
            let mut line = Line::new();
            line.push(b"inputd: event code=");
            line.push_decimal(i64::from(code));
            line.push(b" value=");
            line.push_decimal(i64::from(value));
            line.end();
            if value == 1 {
                saw_press = true;
            }
            offset += INPUT_EVENT_LEN;
        }
        if saw_press {
            say(b"inputd: got a key press");
            break;
        }
    }
    exit(0);
}
