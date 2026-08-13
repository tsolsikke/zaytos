//! `sh`: 簡易シェルの骨格（S11-11）。
//!
//! # crate ではない
//!
//! `ls.rs` と同じで、cargo のパッケージに属さない。**Rust で書いてある**
//! （`userlib.rs` の doc）。
//!
//! # この刻みでは組み込みの `exit` だけを持つ
//!
//! **外部コマンドの実行は次の刻みである。** ここで確かめたいのは
//! **「打鍵が Ring 3 まで届き、行に組み立てられ、応答が返る」**ことである。
//! **知らないコマンドは、名前を返して次の行を待つ。**
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

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, read, write_all, STDOUT};

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
/// 知らないコマンドへの返事（前半）。
const NOT_FOUND_HEAD: &[u8] = b"sh: ";
/// 知らないコマンドへの返事（後半）。
const NOT_FOUND_TAIL: &[u8] = b": not found\n";
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
                } else {
                    run_line(&line[..length]);
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

/// 1 行を実行する。**この刻みでは組み込みの `exit` だけである。**
fn run_line(line: &[u8]) {
    let command = first_word(line);
    if command.is_empty() {
        return;
    }
    if command == BUILTIN_EXIT {
        exit(0);
    }
    // **知らないコマンド。** 名前を返して次の行を待つ。
    // **外部コマンドの実行は次の刻みである。**
    write_all(STDOUT, NOT_FOUND_HEAD);
    write_all(STDOUT, command);
    write_all(STDOUT, NOT_FOUND_TAIL);
}

/// 先頭の空白を飛ばし、次の空白までを返す。
///
/// **空白は 1 種類だけ見る**（`0x20`）。**タブはまだ来ない**——
/// `kernel/src/keyboard/decode.rs` が Tab を文字として出さない。
fn first_word(line: &[u8]) -> &[u8] {
    let mut start = 0usize;
    while start < line.len() && line[start] == b' ' {
        start += 1;
    }
    let mut end = start;
    while end < line.len() && line[end] != b' ' {
        end += 1;
    }
    &line[start..end]
}
