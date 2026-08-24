//! `/bin/more` ——ファイルを1画面ずつ出す（VIEW-c）。
//!
//! # `less` と逆である。代替画面へ入らない
//!
//! **出力は画面に残る。** **`more` が終わった後、出したものがそのまま
//! 見えているのが伝統である**——**`less` は代替画面へ入り、抜けると元の画面が
//! 戻る。** **この 2 つは互いに逆の主張であり、判定も逆である。**
//!
//! # 戻れない
//!
//! **前の画面へは戻れない。** **これは方針ではなく、作りである**——
//! **`more` は読みながら出しており、出した行を持っていない。**
//!
//! **`less` は全部読んでから見せる**（窓を持ち、上下に動く）。
//! **`more` は流しながら出す**（窓を持たない）。
//!
//! # 窓の計算（`common/src/window.rs`）は借りない。理由を書く
//!
//! **VIEW-a と VIEW-b では `common/src/window.rs` を `#[path]` で借りた**
//! （`ADR-0045`）。**`more` は借りない。**
//!
//! **借りる中身が無いからである。** **`Window` が持っているのは
//! 「どの行から何行を見せるか」と「窓を動かす」で、どちらも
//! 「見せている範囲を後から変えられる」ことを前提にしている。**
//! **`more` は変えられない**——**出したら終わりで、`top` に当たる状態を
//! 持たない。** **1 画面の行数（`rows - 1`）を数えるだけで足り、
//! それは計算ではなく引き算 1 つである。**
//!
//! **借りると、使わない状態を持つことになる**（`top` が 0 のまま動かない
//! `Window`）。**「使う者がいない機構は検算が置けない」**（この体制の規律）。
//!
//! # 何を作らないか
//!
//! **`b`（戻る）・検索・行番号・複数のファイル・`-N`。**
//! **`Space` と `q` で足りる**（運用者の指示）。
//!
//! # 組み込みではなく外部である（ADR-0043 の基準へ当てた）
//!
//! **基準は「シェル自身の状態を変えるものだけが組み込み」である。**
//! **`more` はシェルの状態を1つも変えない。** **`less` と同じ判断である。**

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{close, exit, open_read_only, read, write_all, STDERR, STDOUT};

/// 打鍵を読む fd。
const STDIN: u64 = 0;

/// `-EAGAIN`。**打鍵が溜まっていない。**
const MINUS_EAGAIN: i64 = -11;

/// パスの最大長。
const PATH_MAX: usize = 128;

/// 一度に読むバイト数。
const CHUNK: usize = 1024;

/// 1 行の上限（バイト）。**越えた分は切って出す。**
///
/// **`less` は画面の桁数で切る。** **こちらは読みながら出すので、
/// 画面の桁数ではなく、持っている器の大きさで切る**——**器は 1 行ぶんである。**
const LINE_MAX: usize = 512;

/// 待ちの札。**伝統どおりの文言である。**
const MORE_PROMPT: &[u8] = b"--More--";

const USAGE: &[u8] = b"more: usage: more <path>\n";
const OPEN_FAILED: &[u8] = b"more: cannot open\n";
const READ_FAILED: &[u8] = b"more: read failed\n";

/// 読みながら行を切り出す（VIEW-c）。
///
/// # なぜ全部読まないのか
///
/// **`more` は戻らないので、出した行を持つ必要が無い。**
/// **持たなければヒープも要らない**——**`more` は `brk` を1回も呼ばない。**
///
/// **`less` とは形が違う。** **あちらは窓を上下に動かすので、全部持つ。**
struct Lines {
    fd: u64,
    buffer: [u8; CHUNK],
    /// 緩衝の中で、まだ渡していない範囲。
    at: usize,
    filled: usize,
    /// 読み切ったか。
    eof: bool,
    /// 読めなかったか。**読み切ったことと区別する。**
    error: bool,
}

impl Lines {
    fn new(fd: u64) -> Self {
        Self {
            fd,
            buffer: [0u8; CHUNK],
            at: 0,
            filled: 0,
            eof: false,
            error: false,
        }
    }

    /// 次の 1 行を `out` へ写す。**返るのは長さで、`None` は終わりである。**
    ///
    /// **改行は含めない。** **改行で終わらないファイルの最後の行も返す。**
    fn next(&mut self, out: &mut [u8; LINE_MAX]) -> Option<usize> {
        let mut length = 0usize;
        loop {
            if self.at >= self.filled {
                if self.eof {
                    // **溜まっている分があれば、それが最後の行である。**
                    return if length > 0 { Some(length) } else { None };
                }
                let got = read(self.fd, &mut self.buffer);
                if got < 0 {
                    self.error = true;
                    self.eof = true;
                    continue;
                }
                if got == 0 {
                    self.eof = true;
                    continue;
                }
                self.filled = got as usize;
                self.at = 0;
            }
            let byte = self.buffer[self.at];
            self.at += 1;
            if byte == b'\n' {
                return Some(length);
            }
            // **器を越えた分は捨てる。** **行が切れることは、出す側からは
            // 見えない**——`less` が画面の桁で切るのと同じ立場である。
            if length < LINE_MAX {
                out[length] = byte;
                length += 1;
            }
        }
    }
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
    let length = unsafe { userlib::length_of(pointer, PATH_MAX - 1) };
    for index in 0..length {
        // SAFETY: 上で数えた長さの範囲である。
        path[index] = unsafe { *pointer.add(index) };
    }
    path[length] = 0;

    let fd = open_read_only(&path[..length + 1]);
    if fd < 0 {
        write_all(STDERR, OPEN_FAILED);
        exit(1);
    }
    let fd = fd as u64;

    // **画面の形を訊く（e-1）。** **0 なら既定へ落ちる。**
    let screen = userlib::window_size_or_default(0);
    // **1 画面に出す行数。** **最下行は待ちの札に使う。**
    let page = (screen.rows as usize).saturating_sub(1).max(1);

    // 破壊 (VIEW-c, more-uses-alternate-screen): 代替画面へ入る。
    // **`less` の振る舞いそのものである**——**抜けると元の画面が戻り、
    // `more` の出したものが消える。** **「出力が残る」判定だけが落ちる。**
    #[cfg(more_uses_alternate_screen)]
    write_all(STDOUT, b"\x1b[?1049h");

    let mut lines = Lines::new(fd);
    let mut line = [0u8; LINE_MAX];
    let mut quit = false;
    'pages: loop {
        let mut printed = 0usize;
        let mut done = false;
        while printed < page {
            match lines.next(&mut line) {
                Some(taken) => {
                    if taken > 0 {
                        write_all(STDOUT, &line[..taken]);
                    }
                    write_all(STDOUT, b"\n");
                    printed += 1;
                }
                None => {
                    done = true;
                    break;
                }
            }
        }
        if done {
            break 'pages;
        }

        // **待つ。** **札を出してから読む。**
        write_all(STDOUT, MORE_PROMPT);
        loop {
            let mut byte = [0u8; 1];
            let got = read(STDIN, &mut byte);
            if got == MINUS_EAGAIN {
                continue;
            }
            if got <= 0 {
                quit = true;
                break;
            }
            match byte[0] {
                b' ' => break,
                b'q' => {
                    quit = true;
                    break;
                }
                // **知らない打鍵は捨てる。** **戻る手段は持たない**
                //（この doc の「戻れない」）。
                _ => {}
            }
        }
        // **札を消す。** **行頭へ戻して行を消す**——**残すと、次の画面の
        // 1 行目に札が混じる。**
        write_all(STDOUT, b"\r\x1b[2K");
        if quit {
            break 'pages;
        }
    }

    close(fd);

    // 破壊 (VIEW-c, more-uses-alternate-screen): 代替画面から出る。
    // **出した行はここで消える。**
    #[cfg(more_uses_alternate_screen)]
    write_all(STDOUT, b"\x1b[?1049l");

    // **読めなかったことは、読み切ったことと `q` で抜けたことから区別して
    // 報せる。** **`q` は失敗ではない。**
    if lines.error {
        write_all(STDERR, READ_FAILED);
        exit(3);
    }
    exit(0)
}
