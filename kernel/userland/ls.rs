//! `ls`: ディレクトリの中身を並べる（S11-9）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。**ただしこちらは
//! `global_asm!` ではなく Rust で書いてある**——**検算ではなくプログラムだからで
//! ある**（`userlib.rs` の doc）。
//!
//! # 何をするか
//!
//! `argv[1]` があればそのパス、無ければルート（`/`）を開き、`getdents64` で走査して
//! 名前を 1 行ずつ並べる。
//!
//! # 1 回の `write` にまとめる
//!
//! **名前ごとに `write` を呼ばない。** 緩衝へ積んで、いっぱいになるか走査が
//! 終わったところで出す。**理由は観測である**——カーネル側の記録は
//! **最後の `write` の先頭 64 バイト**しか残さないので、**名前ごとに出すと
//! 最後の 1 本しか残らない。** ルートの一覧は 30 バイトほどで、**1 回に収まる。**
//!
//! **収まらなくなったら、そのぶんだけ複数回になる。** 出力そのものは変わらず、
//! **カーネル側の記録に残るのが最後のひとまとまりになるだけである。**
//!
//! # 並べ替えない
//!
//! **ディレクトリに入っている順そのままである。** 並べ替えは `ls` の仕事だが、
//! **比較関数と整列を持ち込むのは、この段では早い。** ext2 の走査順は
//! 決定的なので、**起動ログの参照が突き合わせる対象としては足りている。**
//!
//! # 終了状態の意味
//!
//! - `0` 並べ終わった
//! - `1` 開けなかった
//! - `2` 走査が失敗した
//! - `3` 出力が失敗した

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{
    close, exit, for_each_dirent, getdents64, length_of, open_read_only, write_all, STDERR, STDOUT,
};

/// パスの最大長（NUL を含む）。**カーネルの `PATH_MAX` と同じ。**
const PATH_MAX: usize = 256;
/// `getdents64` へ渡す緩衝の大きさ。
///
/// **1 ブロック（4096）ぶんのレコードが収まる大きさにする。**
/// ルートの 6 エントリで 100 バイトほどなので、**十分な余裕がある。**
const DIRENT_BUF: usize = 1024;
/// 出力を積む緩衝の大きさ。**いっぱいになったら出す。**
const OUT_BUF: usize = 512;

/// 開けなかったときの断り書き。
const OPEN_FAILED: &[u8] = b"ls: cannot open\n";
/// 走査が失敗したときの断り書き。
const READ_FAILED: &[u8] = b"ls: cannot read the directory\n";

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(stack: *const u64) -> ! {
    // **パスは `argv[1]`、無ければルートである。**
    let mut path = [0u8; PATH_MAX];
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let given = unsafe { userlib::argument(stack, 1) };
    let path: &[u8] = match given {
        Some(pointer) => {
            // SAFETY: `argv` の要素はカーネルが NUL 終端で積んだ文字列である。
            let length = unsafe { length_of(pointer, PATH_MAX - 1) };
            for index in 0..length {
                // SAFETY: 上で数えた長さの範囲である。
                path[index] = unsafe { *pointer.add(index) };
            }
            path[length] = 0;
            &path[..length + 1]
        }
        None => b"/\0",
    };

    let fd = open_read_only(path);
    if fd < 0 {
        write_all(STDERR, OPEN_FAILED);
        exit(1);
    }
    let fd = fd as u64;

    let mut entries = [0u8; DIRENT_BUF];
    let mut out = [0u8; OUT_BUF];
    let mut used = 0usize;
    let mut failed = 0u64;

    loop {
        let got = getdents64(fd, &mut entries);
        if got < 0 {
            failed = 2;
            break;
        }
        if got == 0 {
            break;
        }
        for_each_dirent(&entries[..got as usize], |name| {
            // **1 行が緩衝に収まらないなら、先に出す。**
            if used + name.len() + 1 > out.len() {
                if write_all(STDOUT, &out[..used]) < 0 {
                    failed = 3;
                }
                used = 0;
            }
            // **それでも収まらない名前は、そのまま落とさずに切る**
            // ——`ext2` の名前は 255 バイトまでで、緩衝は 512 なので起きない。
            let take = name.len().min(out.len() - used - 1);
            out[used..used + take].copy_from_slice(&name[..take]);
            used += take;
            out[used] = b'\n';
            used += 1;
        });
    }

    if used > 0 && write_all(STDOUT, &out[..used]) < 0 {
        failed = 3;
    }
    close(fd);

    if failed == 2 {
        write_all(STDERR, READ_FAILED);
    }
    exit(failed);
}
