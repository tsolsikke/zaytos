//! `cat`: ファイルの中身をそのまま出す（S11-9）。
//!
//! # crate ではない
//!
//! `ls.rs` と同じで、cargo のパッケージに属さない。**Rust で書いてある**
//! （`userlib.rs` の doc）。
//!
//! # `argv[1]` が無いときは読まない
//!
//! **Linux は標準入力を読む。ここにはまだ標準入力が無い**——`write` は fd 1 と 2 を
//! 受けるが、**fd 0 は配送路が無いので拒まれる**（`kernel/src/syscall.rs` の
//! `sys_write`）。
//!
//! **黙って何もしない形にはしない。** 使い方を標準エラーへ出して、終了状態 2 で
//! 終わる。**「引数を忘れた」と「空のファイルだった」が区別できる形にする。**
//!
//! **標準入力が来たら、そのとき Linux の形へ寄せる**
//! （`docs/roadmap.md` の S11 の入力の配送）。
//!
//! # 1 つしか受けない
//!
//! **Linux は複数のファイルを繋げる。** ここは 1 つだけである——
//! **繰り返しの構造は同じなので、要るようになってから足す。**
//!
//! # 終了状態の意味
//!
//! - `0` 出し終わった
//! - `1` 開けなかった
//! - `2` 引数が無かった
//! - `3` 読めなかった
//! - `4` 出力が失敗した

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{close, exit, length_of, open_read_only, read, write_all, STDERR, STDOUT};

/// パスの最大長（NUL を含む）。**カーネルの `PATH_MAX` と同じ。**
const PATH_MAX: usize = 256;
/// 1 回に読む大きさ。
///
/// **ブロック（4096）より小さくてよい。** `read` は要求した長さと `i_size` の
/// 小さいほうを返すので、**繰り返せば端まで届く。**
const CHUNK: usize = 256;

/// 引数が無いときの使い方。
const USAGE: &[u8] = b"cat: usage: cat PATH\n";
/// 開けなかったときの断り書き。
const OPEN_FAILED: &[u8] = b"cat: cannot open\n";
/// 読めなかったときの断り書き。
const READ_FAILED: &[u8] = b"cat: cannot read\n";

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

    let fd = open_read_only(&path[..length + 1]);
    if fd < 0 {
        write_all(STDERR, OPEN_FAILED);
        exit(1);
    }
    let fd = fd as u64;

    let mut chunk = [0u8; CHUNK];
    let mut status = 0u64;
    loop {
        let got = read(fd, &mut chunk);
        if got < 0 {
            status = 3;
            break;
        }
        // **0 は末尾である**（Linux と同じ EOF の表し方）。
        if got == 0 {
            break;
        }
        if write_all(STDOUT, &chunk[..got as usize]) < 0 {
            status = 4;
            break;
        }
    }
    close(fd);

    if status == 3 {
        write_all(STDERR, READ_FAILED);
    }
    exit(status);
}
