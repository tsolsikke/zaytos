//! `mkdir`: ディレクトリを 1 つ作る（DIR-1c）。
//!
//! # crate ではない
//!
//! `cat.rs` と同じで、cargo のパッケージに属さない（`userlib.rs` の doc）。
//!
//! # `-p` は無い
//!
//! **親が無ければ断る。** **途中を作る形は「どこまで作ったか」を戻す判断が要る**
//! ——**要る者が来てから作る**（`kernel/src/syscall.rs` の `sys_directory`）。
//!
//! # 1 つしか受けない
//!
//! `cat` と同じ理由である。**繰り返しの構造は同じなので、要るようになってから足す。**
//!
//! # 終了状態の意味
//!
//! - `0` 作った
//! - `1` 作れなかった（親が無い、同じ名前が在る、空きが無い）
//! - `2` 引数が無かった

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, length_of, write_all, STDERR};

/// 受け取れるパスの長さ（NUL を含む）。**カーネルの `PATH_MAX` と同じ。**
const PATH_MAX: usize = 256;

/// 引数が無いときの使い方。
const USAGE: &[u8] = b"mkdir: usage: mkdir PATH\n";
/// できなかったときの断り書き（前半）。
const FAILED_HEAD: &[u8] = b"mkdir: cannot create ";
/// できなかったときの断り書き（後半）。
const FAILED_TAIL: &[u8] = b": cannot create\n";

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
    let status = unsafe { userlib::mkdir(&path[..length + 1]) };
    if status < 0 {
        write_all(STDERR, FAILED_HEAD);
        write_all(STDERR, &path[..length]);
        write_all(STDERR, FAILED_TAIL);
        exit(1);
    }
    exit(0);
}
