//! `gfxd`: 画面へ画素を出す最初の利用者（`ADR-0066` の Y-c）。
//!
//! # 本番の形の予行である
//!
//! **本番の利用者（Seinas。コンポジタ）はまだ無い。** **コンポジタは画面を開き、面を `mmap` し、
//! 合成した矩形を `present` で写す**——**その 3 つをここで回す。** **画面の形は Linux の fbdev の
//! `ioctl` で訊く**（`FBIOGET_VSCREENINFO` / `FBIOGET_FSCREENINFO`）。
//!
//! # 順序
//!
//! 1. **`/bin/gfxc` を起こしっぱなしで起こし、終わるまで待つ**——**前景でない者が画面と入力の
//!    fd を開けないことを、図形モードへ入る前に見る**（入った後だと `-EBUSY` で断られ、前景の関所を
//!    見たことにならない）。
//! 2. **画面を開き、形を訊き、面を `mmap` する。**
//! 3. **四角をマゼンタで塗り、その矩形だけを `present` で写す。** **マゼンタ（赤と青が 0xFF、緑が 0）は
//!    `Rgb` でも `Bgr` でも同じ 32 ビットの値になる**——**並びを取り違えても色が変わらないので、
//!    判定が並びの読み違いに引きずられない。**
//! 4. **打鍵を待つ**——**その間に `xtask` が画面を読み戻す。**
//! 5. **画面の fd を閉じる**（図形モードから抜ける）。
//!
//! # 終了状態の意味
//!
//! - `0` 描いて写し、打鍵を受けて抜けた
//! - `1` `gfxc` を起こせなかった／画面か入力が開けなかった
//! - `2` 形が訊けなかった／`mmap` か `present` が失敗した

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{
    close, exit, mmap_shared, open_input, open_screen, present, read, screen_info,
    spawn_detached, wait_child, write_all, INPUT_EVENT_LEN, STDOUT,
};

/// 繋がない側の像。**NUL 終端である**（`spawn_detached` はポインタで渡す）。
const PEER_PATH: &[u8] = b"/bin/gfxc\0";
/// 繋がない側の `argv[0]`。
const PEER_ARG0: &[u8] = b"gfxc\0";
/// 四角の左上と一辺（ピクセル）。**`xtask` の判定と同じ値である。**
const SQUARE_X: u32 = 200;
const SQUARE_Y: u32 = 200;
const SQUARE_SIZE: u32 = 160;
/// マゼンタ（赤と青が 0xFF、緑が 0）。**`Rgb` でも `Bgr` でも同じ値になる**（モジュールの doc）。
const MAGENTA: u32 = 0x00FF_00FF;
/// `-EAGAIN`。
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
    // **1. 前景でない者が開けないことを先に見る**（モジュールの doc）。
    let peer_argv: [*const u8; 2] = [PEER_ARG0.as_ptr(), core::ptr::null()];
    let peer_envp: [*const u8; 1] = [core::ptr::null()];
    // SAFETY: パスと `argv` の各要素は NUL 終端で、表は NULL で終わっている。
    let peer = unsafe { spawn_detached(PEER_PATH, &peer_argv, &peer_envp, 0) };
    if peer < 0 {
        fail(b"gfxd: spawn_detached failed ", peer, 1);
    }
    let status = wait_child(peer as u64);
    let mut line = Line::new();
    line.push(b"gfxd: gfxc ended ");
    line.push_decimal(status);
    line.end();

    // **2. 画面を開き、形を訊き、面を張る。**
    let screen = open_screen();
    if screen < 0 {
        fail(b"gfxd: open_screen failed ", screen, 1);
    }
    let screen = screen as u64;
    let mut line = Line::new();
    line.push(b"gfxd: opened the screen fd ");
    line.push_decimal(screen as i64);
    line.end();
    let info = match screen_info(screen) {
        Ok(info) => info,
        Err(errno) => fail(b"gfxd: screen_info failed ", errno, 2),
    };
    let mut line = Line::new();
    line.push(b"gfxd: screen ");
    line.push_decimal(i64::from(info.width));
    line.push(b"x");
    line.push_decimal(i64::from(info.height));
    line.push(b" bpp ");
    line.push_decimal(i64::from(info.bits_per_pixel));
    line.push(b" line ");
    line.push_decimal(i64::from(info.line_length));
    line.push(b" size ");
    line.push_decimal(i64::from(info.smem_len));
    line.end();
    if info.bits_per_pixel != 32
        || info.width < SQUARE_X + SQUARE_SIZE
        || info.height < SQUARE_Y + SQUARE_SIZE
    {
        fail(b"gfxd: the screen is not what this test draws on, bpp ", i64::from(info.bits_per_pixel), 2);
    }
    let mapped = mmap_shared(screen, u64::from(info.smem_len));
    if mapped < 0 {
        fail(b"gfxd: mmap failed ", mapped, 2);
    }
    let base = mapped as usize as *mut u8;

    // **3. 四角を塗り、その矩形だけを写す。**
    for y in SQUARE_Y..SQUARE_Y + SQUARE_SIZE {
        for x in SQUARE_X..SQUARE_X + SQUARE_SIZE {
            let at = (y * info.line_length + x * 4) as usize;
            // SAFETY: `at` は `smem_len` の内側である（上で形を確かめた）。**張った面は 4 バイト境界。**
            unsafe { core::ptr::write_volatile(base.add(at).cast::<u32>(), MAGENTA) };
        }
    }
    let presented = present(
        screen,
        SQUARE_X as u16,
        SQUARE_Y as u16,
        (SQUARE_X + SQUARE_SIZE) as u16,
        (SQUARE_Y + SQUARE_SIZE) as u16,
    );
    if presented < 0 {
        fail(b"gfxd: present failed ", presented, 2);
    }
    say(b"gfxd: presented");

    // **4. 打鍵を待つ**——**その間に `xtask` が画面を読み戻す。**
    let input = open_input();
    if input < 0 {
        fail(b"gfxd: open_input failed ", input, 1);
    }
    let input = input as u64;
    say(b"gfxd: waiting for a key");
    let mut events = [0u8; INPUT_EVENT_LEN * 4];
    loop {
        let got = read(input, &mut events);
        if got == MINUS_EAGAIN {
            continue;
        }
        if got < 0 {
            fail(b"gfxd: input read failed ", got, 1);
        }
        if got > 0 {
            break;
        }
    }

    // **5. 画面の fd を閉じる**——**図形モードから抜け、文字の画面へ戻る。**
    close(screen);
    say(b"gfxd: left the screen");
    exit(0)
}
