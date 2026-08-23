//! `rmdir`: ディレクトリを 1 つ消す（DIR-1c）。
//!
//! # crate ではない
//!
//! `cat.rs` と同じで、cargo のパッケージに属さない（`userlib.rs` の doc）。
//!
//! # 空でなければ断る
//!
//! **中身ごと消す道（`rm -r`）は無い。** **「何を消すか」を数える判断が要り、
//! 別の段である**（`common/src/ext2.rs` の `AllocError::DirectoryNotEmpty`）。
//!
//! # 1 つしか受けない
//!
//! `cat` と同じ理由である。**繰り返しの構造は同じなので、要るようになってから足す。**
//!
//! # 終了状態の意味
//!
//! - `0` 消した
//! - `1` 消せなかった（無い、空でない、ディレクトリでない）
//! - `2` 引数が無かった

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, length_of, write_all, STDERR};

/// 受け取れるパスの長さ（NUL を含む）。**カーネルの `PATH_MAX` と同じ。**
const PATH_MAX: usize = 256;

/// 引数が無いときの使い方。
const USAGE: &[u8] = b"rmdir: usage: rmdir PATH\n";
/// できなかったときの断り書き（前半）。
const FAILED_HEAD: &[u8] = b"rmdir: cannot remove ";
/// できなかったときの断り書き（後半）。
const FAILED_TAIL: &[u8] = b": cannot remove\n";

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
    let status = unsafe { userlib::rmdir(&path[..length + 1]) };
    if status < 0 {
        write_all(STDERR, FAILED_HEAD);
        write_all(STDERR, &path[..length]);
        write_all(STDERR, FAILED_TAIL);
        exit(1);
    }
    exit(0);
}
