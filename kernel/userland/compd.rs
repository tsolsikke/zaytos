//! `compd`: 画面・入力・ソケットを 1 つの組で通す画面サーバ（`ADR-0066` の Y-d）。
//!
//! # 本番の形の予行である
//!
//! **Seinas（コンポジタ）が来たら同じことをする**——**入力 fd と画面を開き、名前で待ち受け、
//! {入力, listener, クライアント} を同じ集合で待つ。** **クライアントが shm のプールを
//! `SCM_RIGHTS` で送ってきたら、プールを `mmap` して裏バッファへ合成し、`present` で写す。**
//! **打鍵で終わり、クライアントとの接続を閉じる**（`compc` は閉じられるまで居残る。
//! `kernel/userland/compc.rs` の doc）。**Y-a（入力 fd）・Y-b（多重待ち）・Y-c（画面）と
//! `ADR-0065`（共有メモリ）を 1 本の流れで通す**（設計 Q5）。
//!
//! # 送られてくるもの
//!
//! **16 バイトの見出し（`u32` の幅・高さ・x・y。リトルエンディアン）と、プールの fd 1 つ。**
//! **プールは幅×高さ×4 バイトの画素（1 行の詰め物は無い）である。**
//!
//! # 終了状態の意味
//!
//! - `0` 合成して写し、打鍵を受けて終わった
//! - `1` 入力・画面・名前・`compc` のどれかが用意できなかった
//! - `2` `poll`・`accept`・`recvmsg`・`mmap` が失敗した

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{
    accept, bind, close, exit, listen, mmap_shared, open_input, open_screen, poll, present, read,
    recvmsg, screen_info, socket, spawn_detached, wait_child, write_all, MsgBuffers, PollFd,
    INPUT_EVENT_LEN, POLL_FOREVER, STDOUT,
};

/// 待ち受ける名前。
const NAME: &[u8] = b"comp-0";
/// 待ち行列の長さ。
const BACKLOG: u64 = 1;
/// クライアントの像と `argv[0]`（NUL 終端）。
const PEER_PATH: &[u8] = b"/bin/compc\0";
const PEER_ARG0: &[u8] = b"compc\0";
/// 見出しのバイト数（`u32` × 4）。
const HEADER_LEN: usize = 16;
/// 受けるプールの上限（`MAX_SHM_PAGES` = 8 ページ。`ADR-0065`）。
const POOL_MAX: u32 = 8 * 4096;
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

/// 読める者が出るまで `poll` を回す（`polld` と同じ形。**0 と `-EAGAIN` は回す**）。
fn poll_until(fds: &mut [PollFd]) -> i64 {
    loop {
        for entry in fds.iter_mut() {
            entry.revents = 0;
        }
        let ready = poll(fds, POLL_FOREVER);
        if ready > 0 || (ready < 0 && ready != MINUS_EAGAIN) {
            return ready;
        }
    }
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(_stack: *const u64) -> ! {
    let input = open_input();
    if input < 0 {
        fail(b"compd: open_input failed ", input, 1);
    }
    let input = input as u64;
    let screen = open_screen();
    if screen < 0 {
        fail(b"compd: open_screen failed ", screen, 1);
    }
    let screen = screen as u64;
    let info = match screen_info(screen) {
        Ok(info) => info,
        Err(errno) => fail(b"compd: screen_info failed ", errno, 1),
    };
    let mapped = mmap_shared(screen, u64::from(info.smem_len));
    if mapped < 0 {
        fail(b"compd: mmap of the screen failed ", mapped, 2);
    }
    let frame = mapped as usize as *mut u8;

    let listener = socket();
    if listener < 0 {
        fail(b"compd: socket failed ", listener, 1);
    }
    let listener = listener as u64;
    let bound = bind(listener, NAME);
    if bound < 0 {
        fail(b"compd: bind failed ", bound, 1);
    }
    let listening = listen(listener, BACKLOG);
    if listening < 0 {
        fail(b"compd: listen failed ", listening, 1);
    }
    say(b"compd: listening on comp-0");

    // **クライアントを起こしっぱなしで起こす**（先に待ち受けたので、1 回目の `connect` で繋がる）。
    let peer_argv: [*const u8; 2] = [PEER_ARG0.as_ptr(), core::ptr::null()];
    let peer_envp: [*const u8; 1] = [core::ptr::null()];
    // SAFETY: パスと `argv` の各要素は NUL 終端で、表は NULL で終わっている。
    let peer = unsafe { spawn_detached(PEER_PATH, &peer_argv, &peer_envp, 0) };
    if peer < 0 {
        fail(b"compd: spawn_detached failed ", peer, 1);
    }

    let mut client: Option<u64> = None;
    let mut header = [0u8; HEADER_LEN];
    let mut events = [0u8; INPUT_EVENT_LEN * 4];
    'session: loop {
        // **集合は {入力, listener} に、繋がっていればクライアントを足した 3 本である。**
        let mut fds = [
            PollFd::readable(input),
            PollFd::readable(listener),
            PollFd::readable(client.unwrap_or(0)),
        ];
        let count = if client.is_some() { 3 } else { 2 };
        let ready = poll_until(&mut fds[..count]);
        if ready < 0 {
            fail(b"compd: poll failed ", ready, 2);
        }
        let mut line = Line::new();
        line.push(b"compd: poll ready=");
        let names: [&[u8]; 3] = [b"input", b"listener", b"client"];
        let mut first = true;
        for (index, entry) in fds[..count].iter().enumerate() {
            if entry.is_readable() {
                if !first {
                    line.push(b"+");
                }
                line.push(names[index]);
                first = false;
            }
        }
        line.end();

        if fds[1].is_readable() && client.is_none() {
            let accepted = accept(listener);
            if accepted < 0 {
                fail(b"compd: accept failed ", accepted, 2);
            }
            client = Some(accepted as u64);
            say(b"compd: accepted a client");
        }
        if count == 3 && fds[2].is_readable() {
            let conn = client.unwrap_or(0);
            // SAFETY: `header` はこのスコープの間だけ `msghdr` が指す。
            let mut msg = unsafe { MsgBuffers::new(&mut header, None) };
            let got = recvmsg(conn, &mut msg);
            if got < 0 {
                fail(b"compd: recvmsg failed ", got, 2);
            }
            if got == 0 {
                // **クライアントが閉じた。** **集合から外して待ち続ける。**
                close(conn);
                client = None;
                say(b"compd: the client left");
            } else {
                let pool_fd = msg.received_fd();
                let (width, height) = (le_u32(&header, 0), le_u32(&header, 4));
                let (x, y) = (le_u32(&header, 8), le_u32(&header, 12));
                let bytes = width.saturating_mul(height).saturating_mul(4);
                if got as usize != HEADER_LEN
                    || pool_fd == 0
                    || bytes == 0
                    || bytes > POOL_MAX
                    || x + width > info.width
                    || y + height > info.height
                {
                    fail(b"compd: a malformed tile, got ", got, 2);
                }
                let pool = mmap_shared(u64::from(pool_fd), u64::from(bytes));
                if pool < 0 {
                    fail(b"compd: mmap of the pool failed ", pool, 2);
                }
                let pool = pool as usize as *const u8;
                // **合成する**——**プールの行を裏バッファの行へ写す**（プールは詰め物の無い行）。
                for row in 0..height {
                    // SAFETY: 行はプールと面の両方の範囲の内側である（上で大きさを確かめた）。
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            pool.add((row * width * 4) as usize),
                            frame.add(((y + row) * info.line_length + x * 4) as usize),
                            (width * 4) as usize,
                        );
                    }
                }
                let presented = present(
                    screen,
                    x as u16,
                    y as u16,
                    (x + width) as u16,
                    (y + height) as u16,
                );
                close(u64::from(pool_fd));
                let mut line = Line::new();
                line.push(b"compd: composited ");
                line.push_decimal(i64::from(width));
                line.push(b"x");
                line.push_decimal(i64::from(height));
                line.push(b" at ");
                line.push_decimal(i64::from(x));
                line.push(b",");
                line.push_decimal(i64::from(y));
                line.push(b" present=");
                line.push_decimal(presented);
                line.end();
                write_all(conn, b"ok");
            }
        }
        if fds[0].is_readable() {
            let got = read(input, &mut events);
            let mut offset = 0usize;
            while got > 0 && offset + INPUT_EVENT_LEN <= got as usize {
                let value = i32::from_le_bytes([
                    events[offset + 20],
                    events[offset + 21],
                    events[offset + 22],
                    events[offset + 23],
                ]);
                if value == 1 {
                    say(b"compd: a key ended the session");
                    break 'session;
                }
                offset += INPUT_EVENT_LEN;
            }
        }
    }

    // **クライアントとの接続を閉じる**——**居残っている `compc` の `read` が 0 を返して終わる。**
    // **閉じずに回収を待つと、互いに待ち合って止まる。**
    if let Some(conn) = client {
        close(conn);
    }
    // **画面を閉じる**（図形モードから抜け、文字の画面へ戻る）。**クライアントを回収する。**
    close(screen);
    let status = wait_child(peer as u64);
    let mut line = Line::new();
    line.push(b"compd: compc ended ");
    line.push_decimal(status);
    line.end();
    say(b"compd: done");
    exit(0)
}
