//! `sockd`: 名前で待ち受け、繋いできた相手へ同じバイトを返す（`ADR-0064`）。
//!
//! # 本番の形の予行である
//!
//! **本番の利用者（Seinas。コンポジタ）はまだ無い。** **この組は Wayland の形に合わせて
//! ある**——**サーバーは起動時に起こされ、名前で待ち、クライアントは名前で繋ぐ。**
//! **Seinas が来たら、この組は検査（`socket-test`）に残す**——**カーネルの口の判定は
//! コンポジタの都合から切り離しておくためである**（`ADR-0064`）。
//!
//! # 1 度に 1 本ずつ受ける
//!
//! **`accept` して、相手が閉じる（EOF）まで返し、また `accept` する。** **2 本目の
//! `connect` は待ち行列で待つ**（`twice`）。**`quit` が来たら閉じて終わる**——
//! **カーネルで待っている子には Ctrl+C が届かないので、終わる口を持たせてある。**
//!
//! # 1 行は 1 回の `write` で出す
//!
//! **2 本の Ring 3 が同時に端末へ書く。** **1 回の `write` は割り込みを禁じた中で
//! 出るので途中で混ざらないが、行を 2 回に分けると間に相手の行が入る**（実測。
//! `ADR-0063` の (b3)）。**行を組んでから 1 回で出す。**
//!
//! # 終了状態の意味
//!
//! - `0` `quit` を受けて終わった
//! - `1` ソケットが作れなかった／名前が取れなかった／待ち受けられなかった
//! - `2` `accept` か `read` が失敗した

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{
    accept, bind, close, exit, length_of, listen, mmap_shared, recvmsg, socket, write_all,
    MsgBuffers, STDOUT,
};

/// 名前の最大長（カーネルの `NAME_MAX` と同じ）。
const NAME_MAX: usize = 31;
/// 引数が無いときの名前（Wayland の既定の端点と同じ）。
const DEFAULT_NAME: &[u8] = b"wayland-0";
/// 1 回に読む大きさ（輪と同じ）。
const CHUNK: usize = 1024;
/// 終わる合図。
const QUIT: &[u8] = b"quit";
/// 待ち行列の長さ（カーネルは接続の上限で頭を切る）。
const BACKLOG: u64 = 2;
/// 共有メモリの模様の長さ（`sockc` と同じ。`ADR-0065`）。
const SHM_LEN: usize = 6000;
/// 模様の種（`sockc` と同じ。**251 は素数で境で繰り返さない**）。
const BIG_SEED: usize = 7;

fn big_byte(index: usize) -> u8 {
    ((BIG_SEED + index) % 251) as u8
}

/// 1 行を組んで 1 回で出す（モジュールの doc）。
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

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(stack: *const u64) -> ! {
    let mut name = [0u8; NAME_MAX];
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let name_len = match unsafe { userlib::argument(stack, 1) } {
        Some(pointer) => {
            // SAFETY: `argv` の要素はカーネルが NUL 終端で積んだ文字列である。
            let length = unsafe { length_of(pointer, NAME_MAX) };
            for index in 0..length {
                // SAFETY: 上で数えた長さの範囲である。
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

    let listener = socket();
    if listener < 0 {
        let mut line = Line::new();
        line.push(b"sockd: socket failed ");
        line.push_decimal(listener);
        line.end();
        exit(1);
    }
    let listener = listener as u64;
    let bound = bind(listener, name);
    if bound < 0 {
        let mut line = Line::new();
        line.push(b"sockd: bind failed ");
        line.push_decimal(bound);
        line.end();
        exit(1);
    }
    let listening = listen(listener, BACKLOG);
    if listening < 0 {
        let mut line = Line::new();
        line.push(b"sockd: listen failed ");
        line.push_decimal(listening);
        line.end();
        exit(1);
    }
    let mut line = Line::new();
    line.push(b"sockd: listening on ");
    line.push(name);
    line.end();

    let mut chunk = [0u8; CHUNK];
    loop {
        let stream = accept(listener);
        if stream < 0 {
            let mut line = Line::new();
            line.push(b"sockd: accept failed ");
            line.push_decimal(stream);
            line.end();
            exit(2);
        }
        let stream = stream as u64;
        say(b"sockd: accepted");
        loop {
            // **`recvmsg` で読む**——**`SCM_RIGHTS` の fd（共有メモリ）が来るかもしれない
            // （`ADR-0065`）。** **fd が無ければ `read` と同じでバイトだけ来る。** **毎回作り直して
            // `cmsg` を 0 にする**（前の回の fd を残さない。受けた fd は 3 以上なので 0 は「無し」）。
            // SAFETY: `chunk` はこのスコープの間だけ `msghdr` が指す。
            let mut msg = unsafe { MsgBuffers::new(&mut chunk, None) };
            let got = recvmsg(stream, &mut msg);
            if got < 0 {
                let mut line = Line::new();
                line.push(b"sockd: recvmsg failed ");
                line.push_decimal(got);
                line.end();
                exit(2);
            }
            // **0 は相手が閉じた印である**（EOF）。
            if got == 0 {
                say(b"sockd: client left");
                break;
            }
            let got = got as usize;
            let shm_fd = msg.received_fd();
            if shm_fd != 0 {
                // **共有メモリの fd が来た**——**張って模様を確かめ、結果を返す（`ADR-0065`）。**
                let mapped = mmap_shared(shm_fd as u64, SHM_LEN as u64);
                let ok = if mapped < 0 {
                    false
                } else {
                    let base = mapped as usize as *const u8;
                    let mut ok = true;
                    for index in 0..SHM_LEN {
                        // SAFETY: たった今 mmap した共有メモリの範囲である。
                        if unsafe { *base.add(index) } != big_byte(index) {
                            ok = false;
                            break;
                        }
                    }
                    ok
                };
                let mut line = Line::new();
                line.push(b"sockd: shm ");
                line.push_decimal(SHM_LEN as i64);
                line.push(if ok { b" bytes ok=true" } else { b" bytes ok=false" });
                line.end();
                close(shm_fd as u64);
                let reply: &[u8] = if ok { b"shm-ok" } else { b"shm-bad" };
                if write_all(stream, reply) < 0 {
                    say(b"sockd: write failed");
                    break;
                }
                continue;
            }
            if chunk[..got].starts_with(QUIT) {
                say(b"sockd: quit");
                close(stream);
                close(listener);
                exit(0);
            }
            if write_all(stream, &chunk[..got]) < 0 {
                say(b"sockd: write failed");
                break;
            }
        }
        close(stream);
    }
}
