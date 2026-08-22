//! `rm`: 通常ファイルを 1 つ消す（DIR-1b）。
//!
//! # crate ではない
//!
//! `cat.rs` と同じで、cargo のパッケージに属さない（`userlib.rs` の doc）。
//!
//! # ディレクトリは消さない
//!
//! **カーネルの `unlink` が `-EISDIR` を返す。** **`rmdir` を使うよう促す。**
//! **`-r` は無い**——**再帰は「消す対象を数える」ことから始まるので、
//! 別の判断である。要る者が来てから作る。**
//!
//! # 1 つしか受けない
//!
//! `cat` と同じ理由である。**繰り返しの構造は同じなので、要るようになってから足す。**
//!
//! # 終了状態の意味
//!
//! - `0` 消した
//! - `1` 消せなかった（無い、ディレクトリ、読み取り専用など）
//! - `2` 引数が無かった

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, length_of, write_all, STDERR};

/// 受け取れるパスの長さ（NUL を含む）。**カーネルの `PATH_MAX` と同じ。**
const PATH_MAX: usize = 256;

/// 引数が無いときの使い方。
const USAGE: &[u8] = b"rm: usage: rm PATH\n";
/// 消せなかったときの断り書き（前半）。
const FAILED_HEAD: &[u8] = b"rm: cannot remove ";
/// ディレクトリだったときの断り書き（後半）。
const IS_DIRECTORY: &[u8] = b": is a directory; use rmdir\n";
/// それ以外で消せなかったときの断り書き（後半）。
const FAILED_TAIL: &[u8] = b": cannot remove\n";

/// `-EISDIR`。**ディレクトリを渡されたことを、他の失敗と区別する。**
const MINUS_EISDIR: i64 = -21;

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
    let status = unsafe { userlib::unlink(&path[..length + 1]) };
    if status < 0 {
        write_all(STDERR, FAILED_HEAD);
        write_all(STDERR, &path[..length]);
        if status == MINUS_EISDIR {
            write_all(STDERR, IS_DIRECTORY);
        } else {
            write_all(STDERR, FAILED_TAIL);
        }
        exit(1);
    }
    exit(0);
}
