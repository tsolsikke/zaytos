//! `compc`: shm のプールに描いて画面サーバへ送るクライアント（`ADR-0066` の Y-d）。
//!
//! # `wl_shm` の予行である
//!
//! **`memfd_create`＋`ftruncate`＋`mmap` でプールを作り、64×64 の四角を描き、fd を
//! `SCM_RIGHTS` で送る**（`ADR-0065` の `create_pool` の形）。**サーバが合成して `ok` を返す。**
//!
//! # 4 つの象限を別の色にする
//!
//! **左上マゼンタ・右上緑・左下白・右下灰。** **どれも赤と青が等しい**——**`Rgb` でも `Bgr` でも
//! 同じ 32 ビットの値になり、並びの読み違いに判定が引きずられない。** **象限で色を変えるのは、
//! 合成の位置と向き（行と桁の取り違え・上下の反転）を判定が見分けるためである。**
//!
//! # 閉じられるまで居残る
//!
//! **`ok` を受けても閉じずに、`compd` が閉じるまで `read` で待つ。** **判定 2（3 本の集合で待った）と
//! 判定 5（集合の外の者を起こしていない）の機会を、時機ではなく形で作るためである。**
//!
//! - **`compd` が `ok` を書いた後の `poll` では、クライアントの側から読めるようにする者が居ない**
//!   ——**`compc` は書かず、閉じもしない。** **起こせるのは打鍵だけなので、`compd` は 3 本の集合で
//!   眠る。** **以前は `ok` の直後に閉じていて、`compd` が眠るかは `compc` が先に閉じるかどうかで
//!   決まっていた**（実測。眠りは既定の 1 回で 3 回、`poll` に触らない破壊の 3 回で 2 回）
//! - **打鍵が届くとき、`compc` は接続が読めるのを待って眠っている**——**合図を見ずに全部起こす
//!   破壊は、必ず `compc` を集合の外の理由で起こす**
//!
//! # 終了状態の意味
//!
//! - `0` 送って `ok` を受け、`compd` が閉じた
//! - `1` 繋げなかった／プールが作れなかった
//! - `2` 送れなかった／返事が `ok` でなかった／`compd` が閉じずに何かを書いた

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{
    close, connect, exit, ftruncate, memfd_create, mmap_shared, read, sendmsg, socket, write_all,
    MsgBuffers, STDOUT,
};

/// 繋ぐ名前（`compd` と同じ）。
const NAME: &[u8] = b"comp-0";
/// 四角の一辺と、画面の上の位置（`xtask` の判定と同じ値）。
const SIZE: u32 = 64;
const AT_X: u32 = 400;
const AT_Y: u32 = 300;
/// 象限の色（モジュールの doc）。
const MAGENTA: u32 = 0x00FF_00FF;
const GREEN: u32 = 0x0000_FF00;
const WHITE: u32 = 0x00FF_FFFF;
const GRAY: u32 = 0x0080_8080;
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
    let conn = socket();
    if conn < 0 {
        fail(b"compc: socket failed ", conn, 1);
    }
    let conn = conn as u64;
    let connected = connect(conn, NAME);
    if connected < 0 {
        fail(b"compc: connect failed ", connected, 1);
    }

    let bytes = (SIZE * SIZE * 4) as u64;
    let pool = memfd_create();
    if pool < 0 {
        fail(b"compc: memfd_create failed ", pool, 1);
    }
    let pool = pool as u64;
    if ftruncate(pool, bytes) < 0 {
        fail(b"compc: ftruncate failed ", -1, 1);
    }
    let mapped = mmap_shared(pool, bytes);
    if mapped < 0 {
        fail(b"compc: mmap failed ", mapped, 1);
    }
    let pixels = mapped as usize as *mut u32;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let color = match (x < SIZE / 2, y < SIZE / 2) {
                (true, true) => MAGENTA,
                (false, true) => GREEN,
                (true, false) => WHITE,
                (false, false) => GRAY,
            };
            // SAFETY: `y * SIZE + x` はプールの画素数の内側である。**張った面は 4 バイト境界。**
            unsafe { core::ptr::write_volatile(pixels.add((y * SIZE + x) as usize), color) };
        }
    }

    // **見出し（幅・高さ・x・y）と、プールの fd を送る。**
    let mut header = [0u8; 16];
    for (index, value) in [SIZE, SIZE, AT_X, AT_Y].iter().enumerate() {
        header[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    // SAFETY: `header` はこの関数の間だけ生き、`msg` より長生きする（`msghdr` が指す）。
    let mut msg = unsafe { MsgBuffers::new(&mut header, Some(pool as u32)) };
    let sent = sendmsg(conn, &mut msg, true);
    close(pool);
    if sent < 0 {
        fail(b"compc: sendmsg failed ", sent, 2);
    }
    let mut reply = [0u8; 8];
    let got = read(conn, &mut reply);
    if got != 2 || &reply[..2] != b"ok" {
        fail(b"compc: the reply was not ok, read ", got, 2);
    }
    say(b"compc: the server composited the tile");
    // **閉じられるまで居残る**（モジュールの doc）。**`compd` は打鍵で終わるときに閉じる。**
    let got = read(conn, &mut reply);
    if got != 0 {
        fail(b"compc: the server wrote instead of closing, read ", got, 2);
    }
    say(b"compc: the server closed the connection");
    close(conn);
    exit(0)
}
