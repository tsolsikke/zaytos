//! `cat`: ファイルの中身をそのまま出す（S11-9）。
//!
//! # crate ではない
//!
//! `ls.rs` と同じで、cargo のパッケージに属さない。**Rust で書いてある**
//! （`userlib.rs` の doc）。
//!
//! # `argv[1]` が無いときは標準入力を読む（`ADR-0063` の (b3)）
//!
//! **Linux と同じ形である。** **S11-9 では「標準入力が来たら、そのとき Linux の形へ
//! 寄せる」と書いて、使い方を出して終了状態 2 で終わっていた。** **パイプが来たので寄せた**
//! ——**`a | cat` の右は fd 0 から読む。**
//!
//! **端末の fd 0 を読む形にもなる**（`cat` と打つと打鍵を待つ）。**それも Linux と同じである。**
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
//! - `3` 読めなかった
//! - `4` 出力が失敗した
//!
//! **`2`（引数が無かった）は (b3) で消えた**——**引数が無いのは使い方ではなく、標準入力を
//! 読む指示である。**

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

/// 標準入力の fd。**引数が無いときはこれを読む。**
const STDIN: u64 = 0;
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
    let (fd, opened) = match unsafe { userlib::argument(stack, 1) } {
        Some(pointer) => {
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
            (fd as u64, true)
        }
        // **引数が無ければ標準入力を読む**（モジュールの doc）。**開いていないので閉じない。**
        None => (STDIN, false),
    };

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
    if opened {
        close(fd);
    }

    if status == 3 {
        write_all(STDERR, READ_FAILED);
    }
    exit(status);
}
