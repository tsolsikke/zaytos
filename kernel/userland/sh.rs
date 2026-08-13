//! `sh`: 簡易シェル（S11-11）。
//!
//! # crate ではない
//!
//! `ls.rs` と同じで、cargo のパッケージに属さない。**Rust で書いてある**
//! （`userlib.rs` の doc）。
//!
//! # 組み込みは `exit` だけで、それ以外は `spawn` へ回す
//!
//! **外部として起こせないものが、少なくとも 1 つ要る**——分岐の形が最初から
//! 在れば、後から足すときに構造を変えずに済む（`docs/vision.md` の
//! 「コマンドの実行を組み込みと外部で分ける」）。
//!
//! **パスは解決しない。** `/bin/ls` と書いてもらう。`PATH` を持つには
//! 環境変数が要り、**`envp` をまだ開けていない。**
//!
//! # 待ち方
//!
//! **回して待つ。** `read(0)` は溜まっていなければ `-EAGAIN` を返し、
//! **カーネル側で眠らせる形はユーザープロセスのスケジューラを要求する**
//! （S11 の範囲外）。
//!
//! **`hlt` は使えない。** Ring 3 では特権命令で、呼べば畳まれる。
//! **カーネルが待つ形も採れない**——`read` の中で眠ると、**BKL を保持したまま
//! 眠ることになる**（`ADR-0023` の Addendum §2 が構造的に禁じている）。
//!
//! **したがって CPU を食う。** **スケジューラを入れたときに、待たせる形へ変える。**
//!
//! # 行の組み立て
//!
//! **`read(0)` はバイトを返す**（`kernel/src/input.rs`）。**行の区切りも編集も
//! こちらが持つ。** 見るのは改行と Backspace（`0x08`）だけである。
//!
//! # 終了状態の意味
//!
//! - `0` 組み込みの `exit` で終わった
//! - `1` 端末を読めなくなった
//!
//! **子の終了状態はここには出ない。** 0 以外なら `sh: exit status N` として
//! 表示し、**シェル自身は続ける。**

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, read, write_all, STDERR, STDOUT};

/// 1 行の最大の長さ。
///
/// # 128 で足りる
///
/// **見込みの最大は `cat /etc/motd` の 17 バイトである。**
/// **128 はその 7 倍を超える。** 越えたぶんは捨てる——**行が伸び続けて
/// スタックを踏むことのほうが害である。**
const LINE_MAX: usize = 128;

/// プロンプト。
const PROMPT: &[u8] = b"zaytos$ ";
/// 起動したことを告げる 1 行。**プロンプトは改行で終わらないので、
/// 「シェルが動いた」を行として残すものが別に要る。**
const BANNER: &[u8] = b"sh: ready\n";
/// 組み込みの `exit`。
const BUILTIN_EXIT: &[u8] = b"exit";
/// 起こせなかったときの返事（前半）。
const NOT_FOUND_HEAD: &[u8] = b"sh: ";
/// 起こせなかったときの返事（後半）。
const NOT_FOUND_TAIL: &[u8] = b": cannot run\n";
/// 0 以外で終わったときの返事（前半）。
const STATUS_HEAD: &[u8] = b"sh: exit status ";
/// `argv` の要素数の上限。**カーネルの `MAX_ARGV` と同じ。**
const MAX_ARGS: usize = 8;
/// 行が長すぎたときの断り書き。
const TOO_LONG: &[u8] = b"sh: line too long\n";

/// `-EAGAIN`。**溜まっていないという意味で、失敗ではない。**
const MINUS_EAGAIN: i64 = -11;
/// Backspace のバイト。
const BACKSPACE: u8 = 0x08;

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(_stack: *const u64) -> ! {
    write_all(STDOUT, BANNER);
    write_all(STDOUT, PROMPT);

    let mut line = [0u8; LINE_MAX];
    let mut length = 0usize;
    let mut overflowed = false;

    loop {
        let mut byte = [0u8; 1];
        let got = read(0, &mut byte);
        if got == MINUS_EAGAIN {
            // **溜まっていない。** 回して待つ。
            continue;
        }
        if got <= 0 {
            // **端末が読めない。** 失敗として終わる。
            exit(1);
        }

        match byte[0] {
            b'\n' => {
                // **打った改行を反響する。** 反響はシェルが行う——
                // **カーネルは前景を渡しているだけで、何も表示しない。**
                write_all(STDOUT, b"\n");
                if overflowed {
                    write_all(STDOUT, TOO_LONG);
                } else if length > 0 {
                    // **終端を置いてから渡す。** 語の末尾は `line` の中の NUL で
                    // 決まるので、**前の行の残りが続きとして読まれない**ように
                    // ここで 1 バイト置く（`LINE_MAX` は 1 行より大きいので在る）。
                    line[length] = 0;
                    run_line(&mut line[..length]);
                }
                length = 0;
                overflowed = false;
                write_all(STDOUT, PROMPT);
            }
            BACKSPACE => {
                if length > 0 {
                    length -= 1;
                    // **画面の消去はしない**（`ADR-0017` の保留項目）。
                    // **シリアルには後退・空白・後退を送る**——端末側が消す。
                    write_all(STDOUT, b"\x08 \x08");
                }
            }
            other => {
                if length < line.len() {
                    line[length] = other;
                    length += 1;
                    write_all(STDOUT, &byte);
                } else {
                    // **越えたぶんは捨てる。** 反響もしない——
                    // **入っていないものを入ったように見せない。**
                    overflowed = true;
                }
            }
        }
    }
}

/// 1 行を実行する。
///
/// # 組み込みと外部を分ける
///
/// **組み込みは `exit` だけである。** **外部として起こせないものが、少なくとも
/// 1 つ要る**——分岐の形が最初から在れば、後から足すときに構造を変えずに済む
/// （`docs/vision.md` の「コマンドの実行を組み込みと外部で分ける」）。
///
/// # パスは解決しない
///
/// **`/bin/ls` と書いてもらう。** `PATH` を持つには環境変数が要り、
/// **`envp` をまだ開けていない。**
///
/// # 行をその場で切る
///
/// **空白を NUL へ置き換え、各語の先頭を指す配列を作る。**
/// **写しを取らない**——`argv` の要素はカーネルが写すので、
/// **この行が生きているあいだ有効であれば足りる。**
fn run_line(line: &mut [u8]) {
    // **語へ切る。** 空白は 1 種類だけ見る（`0x20`）。**タブはまだ来ない**
    // ——`kernel/src/keyboard/decode.rs` が Tab を文字として出さない。
    let mut starts = [0usize; MAX_ARGS];
    let mut count = 0usize;
    let mut at = 0usize;
    while at < line.len() && count < MAX_ARGS {
        while at < line.len() && line[at] == b' ' {
            line[at] = 0;
            at += 1;
        }
        if at >= line.len() {
            break;
        }
        starts[count] = at;
        count += 1;
        while at < line.len() && line[at] != b' ' {
            at += 1;
        }
    }
    if count == 0 {
        return;
    }
    // **残りの空白も NUL にする。** 語の切れ目はすべて NUL になる。
    // **行末の 1 バイトは呼び出し側が置いてある。**
    for byte in line.iter_mut().skip(at) {
        *byte = 0;
    }
    run_with_terminator(line, &starts[..count])
}

/// NUL で切り終えた行から `argv` を組み立て、起こす。
fn run_with_terminator(line: &[u8], starts: &[usize]) {
    // **組み込みを先に見る。**
    let command = word_at(line, starts[0]);
    if command == BUILTIN_EXIT {
        exit(0);
    }

    let mut argv = [core::ptr::null::<u8>(); MAX_ARGS + 1];
    for (slot, start) in argv.iter_mut().zip(starts.iter()) {
        *slot = line[*start..].as_ptr();
    }

    // SAFETY: `command` は NUL 終端で、`argv` は NULL 終端のポインタ配列である。
    // **各要素は `line` の中の NUL 終端の語を指す。**
    let status = unsafe { userlib::spawn(command, &argv[..starts.len() + 1]) };
    if status < 0 {
        write_all(STDERR, NOT_FOUND_HEAD);
        write_all(STDERR, command);
        write_all(STDERR, NOT_FOUND_TAIL);
        return;
    }
    if status != 0 {
        // **0 以外は返す。** 何が起きたかは子が出している。
        write_all(STDOUT, STATUS_HEAD);
        write_decimal(status as u64);
        write_all(STDOUT, b"\n");
    }
}

/// `line` の `start` から語の終わりまでを返す。
///
/// **NUL そのものは含めない。** `spawn` へ渡すのはポインタで、
/// **カーネルは NUL まで読む**（`copy_user_path`）。**終端は `line` の中に在る。**
fn word_at(line: &[u8], start: usize) -> &[u8] {
    let mut end = start;
    while end < line.len() && line[end] != 0 {
        end += 1;
    }
    &line[start..end]
}

/// 10 進で書く。**上限は 3 桁で足りる**（終了状態は `0..=255`）。
fn write_decimal(value: u64) {
    let mut digits = [b'0'; 3];
    let mut length = 0usize;
    let mut rest = value;
    loop {
        digits[2 - length] = b'0' + (rest % 10) as u8;
        length += 1;
        rest /= 10;
        if rest == 0 || length == 3 {
            break;
        }
    }
    write_all(STDOUT, &digits[3 - length..]);
}
