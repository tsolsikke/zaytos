//! `touch`: ファイルが無ければ作る（DIR-1c）。
//!
//! # crate ではない
//!
//! `cat.rs` と同じで、cargo のパッケージに属さない（`userlib.rs` の doc）。
//!
//! # 既存のファイルには触らない。**中身を消さない**
//!
//! **カーネルが受ける書きの形は `O_WRONLY|O_CREAT|O_TRUNC` である**
//! （ADR-0037 の Addendum）。**それで既存のファイルを開くと中身が消える。**
//! **`touch` がそれをしてはいけない。**
//!
//! **したがって、先に読み取りで開いてみる。** **開けたら閉じて何もしない。**
//! **`-ENOENT` なら作る。** **カーネルの面も ADR-0037 も変えない。**
//!
//! **裸の `O_CREAT` を受理させる形は採らない**——**位置書きの部品が無いので、
//! 「切らずに開く」の意味が決まらない**（ADR-0037 の判断のまま）。
//!
//! # 時刻は更新しない
//!
//! **Linux の `touch` は `mtime` を更新する。** **ZaytOS には壁時計の源が
//! 1 つも無い**（`docs/foundation-inventory.md`。実測）。**したがって
//! 既存のファイルに対しては、本当に何もしない。**
//!
//! **黙って成功する。** **「無ければ作る」という主張だけは満たしている。**
//!
//! # 終了状態の意味
//!
//! - `0` 在った、または作った
//! - `1` 作れなかった
//! - `2` 引数が無かった

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{close, exit, length_of, open_read_only, open_write_create, write_all, STDERR};

/// 受け取れるパスの長さ（NUL を含む）。**カーネルの `PATH_MAX` と同じ。**
const PATH_MAX: usize = 256;

/// 引数が無いときの使い方。
const USAGE: &[u8] = b"touch: usage: touch PATH\n";
/// 作れなかったときの断り書き（前半）。
const FAILED_HEAD: &[u8] = b"touch: cannot create ";
/// 作れなかったときの断り書き（後半）。
const FAILED_TAIL: &[u8] = b"\n";

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

    // **在るかどうかを読み取りで見る。** 在れば何もしない。
    let existing = open_read_only(&path[..length + 1]);
    if existing >= 0 {
        let _ = close(existing as u64);
        exit(0);
    }

    let created = open_write_create(&path[..length + 1]);
    if created < 0 {
        write_all(STDERR, FAILED_HEAD);
        write_all(STDERR, &path[..length]);
        write_all(STDERR, FAILED_TAIL);
        exit(1);
    }
    let _ = close(created as u64);
    exit(0);
}
