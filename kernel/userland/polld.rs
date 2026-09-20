//! `polld`: 入力とソケットを同時に待つ最初の利用者（`ADR-0066` の Y-b）。
//!
//! # 本番の形の予行である
//!
//! **本番の利用者（Seinas。コンポジタ）はまだ無い。** **この組は Wayland の形に合わせて
//! ある**——**コンポジタは入力の fd と、クライアントの繋ぎ口と、繋がった接続を、同時に
//! 待つ。** **仕様は `poll` を要求しないが、待つ理由が 2 つ以上あることは要求する**
//! （`docs/wayland-inventory.md`）。
//!
//! # 3 回待って、起きた理由を印字する
//!
//! 1. **{入力, listener}** を待つ——**`pollc` が繋いでくると listener が読めるようになる。**
//! 2. **{入力, 接続}** を待つ——**`pollc` が返してくると接続が読めるようになる。**
//! 3. **{入力, 接続}** を待つ——**本物の打鍵（`sendkey`）で入力が読めるようになる。**
//!
//! **同じ集合で、起きる理由が 2 回入れ替わる。** **判定はどの回にどちらで起きたかを見る。**
//!
//! # `-EAGAIN` と 0 は回して待つ
//!
//! **待たない構成（破壊 `poll-never-waits`）では `poll` が 0 を返す。** **そのときは回して
//! 待つ**——**`inputd` が `-EAGAIN` を回すのと同じ形である。** **打鍵も返しも結局届くが、
//! カーネルは眠らないので「待った回数」が 0 になる**（判定が落ちる）。
//!
//! # 終了状態の意味
//!
//! - `0` 3 回とも起きて、最後に打鍵を受けた
//! - `1` 入力の fd が開けなかった／名前が取れなかった／`pollc` を起こせなかった
//! - `2` `poll` / `accept` / `read` が失敗した

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{
    accept, bind, close, exit, listen, open_input, poll, read, socket, spawn_detached, wait_child,
    write_all, PollFd, POLL_FOREVER, STDOUT,
};

/// 待ち受ける名前（`sockd` と分ける。同時に走ることは無いが、混ざると読み解けない）。
const NAME: &[u8] = b"poll-0";
/// 待ち行列の長さ。
const BACKLOG: u64 = 1;
/// 繋ぐ側の像。**NUL 終端である**（`spawn_detached` はポインタで渡すので、終端が要る）。
const PEER_PATH: &[u8] = b"/bin/pollc\0";
/// 繋ぐ側の `argv[0]`。**NUL 終端である。**
const PEER_ARG0: &[u8] = b"pollc\0";
/// こちらから送る合図。
const PING: &[u8] = b"ping";
/// 1 回に読む大きさ（生イベント 2 つ分より大きく取る）。
const CHUNK: usize = 64;
/// `-EAGAIN`。**待たない構成と、対話の口が据わっていない間に返る。**
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

/// 読める者が出るまで `poll` を回す（モジュールの doc の「回して待つ」）。
///
/// **0 と `-EAGAIN` は回す。** **本物の失敗（それ以外の負）は返す。**
fn poll_until(fds: &mut [PollFd]) -> i64 {
    loop {
        for entry in fds.iter_mut() {
            entry.revents = 0;
        }
        let ready = poll(fds, POLL_FOREVER);
        if ready > 0 {
            return ready;
        }
        if ready == 0 || ready == MINUS_EAGAIN {
            continue;
        }
        return ready;
    }
}

/// 起きた理由を 1 行に出す（判定が読む）。**`ready=` の後は欄の順に `+` で並べる。**
fn report(round: i64, fds: &[PollFd], names: [&[u8]; 2]) {
    let mut line = Line::new();
    line.push(b"polld: poll ");
    line.push_decimal(round);
    line.push(b" ready=");
    let mut first = true;
    for (index, entry) in fds.iter().enumerate() {
        if !entry.is_readable() {
            continue;
        }
        if !first {
            line.push(b"+");
        }
        line.push(names[index]);
        first = false;
    }
    if first {
        line.push(b"none");
    }
    line.end();
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
        fail(b"polld: open_input failed ", input, 1);
    }
    let input = input as u64;

    let listener = socket();
    if listener < 0 {
        fail(b"polld: socket failed ", listener, 1);
    }
    let listener = listener as u64;
    let bound = bind(listener, NAME);
    if bound < 0 {
        fail(b"polld: bind failed ", bound, 1);
    }
    let listening = listen(listener, BACKLOG);
    if listening < 0 {
        fail(b"polld: listen failed ", listening, 1);
    }
    say(b"polld: listening on poll-0");

    // **繋ぐ側を起こしっぱなしで起こす**（`ADR-0063` の (b3) の口）。**先に待ち受けてから
    // 起こすので、相手は 1 回目の `connect` で繋がる**——**回して繋ぎ直す形にしない。**
    // **表は NULL で終える**（`spawn_detached` の契約。`userlib` の doc）。
    let peer_argv: [*const u8; 2] = [PEER_ARG0.as_ptr(), core::ptr::null()];
    let peer_envp: [*const u8; 1] = [core::ptr::null()];
    // SAFETY: パスと `argv` の各要素は NUL 終端で、表は NULL で終わっている。
    let peer = unsafe { spawn_detached(PEER_PATH, &peer_argv, &peer_envp, 0) };
    if peer < 0 {
        fail(b"polld: spawn_detached failed ", peer, 1);
    }

    // **待ち 1——{入力, listener}。** **相手が繋いでくるまで眠る。**
    let mut first = [PollFd::readable(input), PollFd::readable(listener)];
    let ready = poll_until(&mut first);
    if ready < 0 {
        fail(b"polld: poll 1 failed ", ready, 2);
    }
    report(1, &first, [b"input", b"listener"]);

    let stream = accept(listener);
    if stream < 0 {
        fail(b"polld: accept failed ", stream, 2);
    }
    let stream = stream as u64;
    say(b"polld: accepted");
    let sent = write_all(stream, PING);
    if sent < 0 {
        fail(b"polld: write failed ", sent, 2);
    }

    // **待ち 2——{入力, 接続}。** **相手の返しが来るまで眠る。**
    let mut second = [PollFd::readable(input), PollFd::readable(stream)];
    let ready = poll_until(&mut second);
    if ready < 0 {
        fail(b"polld: poll 2 failed ", ready, 2);
    }
    report(2, &second, [b"input", b"socket"]);
    let mut chunk = [0u8; CHUNK];
    let got = read(stream, &mut chunk);
    if got < 0 {
        fail(b"polld: socket read failed ", got, 2);
    }
    let mut line = Line::new();
    line.push(b"polld: socket gave ");
    line.push_decimal(got);
    line.push(b" byte(s)");
    line.end();

    // **待ち 3——同じ集合で、こんどは打鍵で起きる。**
    let mut third = [PollFd::readable(input), PollFd::readable(stream)];
    let ready = poll_until(&mut third);
    if ready < 0 {
        fail(b"polld: poll 3 failed ", ready, 2);
    }
    report(3, &third, [b"input", b"socket"]);
    let got = read(input, &mut chunk);
    if got < 0 && got != MINUS_EAGAIN {
        fail(b"polld: input read failed ", got, 2);
    }
    let mut line = Line::new();
    line.push(b"polld: input gave ");
    line.push_decimal(got);
    line.push(b" byte(s)");
    line.end();

    // **接続を閉じてから回収する。** **相手は 2 度目の `read` で居残っている**——**閉じると
    // EOF で戻って終わる**（`pollc` の doc）。**先に閉じさせると、待ち 3 が打鍵ではなく
    // EOF で起きてしまう**（実測で踏んだ。2026-09-21）。
    close(stream);
    // **繋ぐ側を回収する**（`ADR-0063` の (b2)）。**残すと「回収していない子」が 1 本
    // 残ったまま締める。**
    let status = wait_child(peer as u64);
    let mut line = Line::new();
    line.push(b"polld: pollc ended ");
    line.push_decimal(status);
    line.end();

    say(b"polld: done");
    exit(0)
}
