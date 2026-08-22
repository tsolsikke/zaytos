//! `tail`: ファイルの末尾だけを出す（DIR-1b）。
//!
//! # crate ではない
//!
//! `cat.rs` と同じで、cargo のパッケージに属さない（`userlib.rs` の doc）。
//!
//! # この 1 本が `lseek` の利用者である
//!
//! **`lseek` の部品（`vfs::File::seek_to`）は S10-b から在り、入口が無かった**
//! （`docs/foundation-inventory.md` の「部品は在るが入口が無い」）。
//! **入口だけ足しても、使う者が居なければ検算が置けない**
//! （`docs/verification-coverage.md`）。**そこでこの 1 本を同じ段で作った。**
//!
//! # 大きさは `stat` に訊く。**跳んだ先を推測しない**
//!
//! **`stat` で `st_size` を取り、`size - TAIL_BYTES` へ跳ぶ。**
//! **`lseek` の戻り値（新しい位置）と、跳ぼうとした位置を突き合わせる**
//! ——**食い違えば、跳べていない。**
//!
//! **「大きな値へ跳んで、刈り込まれた戻り値を大きさとして使う」形は採らない。**
//! **`File::seek_to` が末尾で飽和することに寄りかかることになり、
//! あの飽和は算術の安全のために在るもので、大きさを答えるためではない。**
//!
//! # 行では数えない
//!
//! **Linux の `tail` は行で数える。** ここはバイトで数える——
//! **行で数えるには後ろ向きに走査する必要があり、`lseek` の利用者としては
//! 余分である。** **要る者が来たら足す。**
//!
//! # 終了状態の意味
//!
//! - `0` 出し終わった
//! - `1` 開けなかった、または大きさが分からなかった
//! - `2` 引数が無かった
//! - `3` 読めなかった
//! - `4` 出力が失敗した
//! - `5` 跳べなかった（`lseek` の戻り値が跳ぼうとした位置と違う）

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{close, exit, length_of, open_read_only, read, write_all, STDERR, STDOUT};

/// 受け取れるパスの長さ（NUL を含む）。
const PATH_MAX: usize = 256;
/// 1 回に読む大きさ。`cat` と同じ理由で、ブロックより小さくてよい。
const CHUNK: usize = 256;

/// 出す末尾のバイト数。
///
/// **行ではなくバイトである**（モジュール doc）。
///
/// # この値は判定に効く。増やすと主張が消える
///
/// **`/data/lines` の 4 行（`zi` が編集した後で 26 バイト）が全部は入らない
/// 大きさにしてある。** **入ってしまうと `cat` と同じ出力になり、
/// 「跳んだ」ことを出力から言えなくなる**——`--zi-test` の
/// 「tail printed the end of the file and nothing more」は、
/// **`cat` の出力の末尾と一致し、かつ短いこと**を見ている。
///
/// **したがって、増やすときは判定を先に見ること。** **12 を選んだのは、
/// 4 行のうち後ろ 2 行に届き、前 2 行に届かない大きさだからである**（実測で
/// `"arlie\ndelta"` になる）。
const TAIL_BYTES: u64 = 12;

/// 引数が無いときの使い方。
const USAGE: &[u8] = b"tail: usage: tail PATH\n";
/// 開けなかったときの断り書き。
const OPEN_FAILED: &[u8] = b"tail: cannot open\n";
/// 読めなかったときの断り書き。
const READ_FAILED: &[u8] = b"tail: cannot read\n";
/// 跳べなかったときの断り書き。
const SEEK_FAILED: &[u8] = b"tail: cannot seek\n";

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
    let length = unsafe { length_of(pointer, PATH_MAX - 1) };
    for index in 0..length {
        // SAFETY: 上で数えた長さの範囲である。
        path[index] = unsafe { *pointer.add(index) };
    }
    path[length] = 0;

    // SAFETY: `path` は NUL 終端である。
    let Ok(size) = (unsafe { userlib::size_of_file(&path[..length + 1]) }) else {
        write_all(STDERR, OPEN_FAILED);
        exit(1);
    };

    let fd = open_read_only(&path[..length + 1]);
    if fd < 0 {
        write_all(STDERR, OPEN_FAILED);
        exit(1);
    }
    let fd = fd as u64;

    // **末尾から `TAIL_BYTES` の位置へ跳ぶ。** 小さいファイルは先頭から出す。
    let from = size.saturating_sub(TAIL_BYTES);
    let landed = userlib::seek_to(fd, from);
    if landed < 0 || landed as u64 != from {
        write_all(STDERR, SEEK_FAILED);
        let _ = close(fd);
        exit(5);
    }

    let mut buffer = [0u8; CHUNK];
    loop {
        let got = read(fd, &mut buffer);
        if got < 0 {
            write_all(STDERR, READ_FAILED);
            let _ = close(fd);
            exit(3);
        }
        if got == 0 {
            break;
        }
        if write_all(STDOUT, &buffer[..got as usize]) < 0 {
            let _ = close(fd);
            exit(4);
        }
    }
    let _ = close(fd);
    exit(0);
}
