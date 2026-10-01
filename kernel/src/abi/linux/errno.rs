//! errno（Linux と同じ値。`ADR-0020` の Addendum で、errno の値を Linux に合わせると決めた）。**失敗は `-errno` で
//! 返す**（`crate::syscall`）。
//!
//! **CPU によらない**——x86_64 と aarch64 で同じ値である（`asm/errno.h` を `gcc` と `aarch64-linux-gnu-gcc` で測って
//! 確かめた。2026-09-30）。`ADR-0071` の決定 1 の 2 で、`crate::syscall` から移し、`abi/linux/x86_64` から分けた。
//! 並びと doc は移す前のまま。

/// `-ENOSYS`（未実装システムコール）の errno。失敗は `-errno` で返す。
pub const ENOSYS: i64 = 38;

/// `-EFAULT`（不正なアドレス）の errno。ユーザーポインタ検証に落ちたとき返す。
pub const EFAULT: i64 = 14;

/// `-EINVAL`（引数が不正）の errno（S9-a）。**アドレスは正しいが、値が受け付け
/// られない**ときに返す。現在の用途は [`crate::syscall::CHECKSUM_BUF_LEN`] の超過だけである。
///
/// 値は Linux と同じ 22 である（ADR-0020 の Addendum で「errno の値を Linux に
/// 合わせる」と決めてある）。
pub const EINVAL: i64 = 22;

/// `-ENOENT`（そのパスは無い）の errno（S10-b）。値は Linux と同じ 2 である。
pub const ENOENT: i64 = 2;

/// `-ENOEXEC`（実行できる形でない）の errno（2026-10-01）。値は Linux と同じ 8 である
/// （`/usr/include/asm-generic/errno-base.h`）。**像が実行できる形でないときに返す**——バイト列として
/// 壊れている、または区画の並びを受け付けられない（`crate::userland` の `UserLoadError` の `Parse`・
/// `SegmentData`・`Layout`）。
pub const ENOEXEC: i64 = 8;

/// `-EBADF`（そのファイルディスクリプタは開いていない）の errno（S10-b）。
pub const EBADF: i64 = 9;

/// `-ENOTDIR`（ディレクトリでないものをディレクトリとして辿った）の errno（S10-b）。
pub const ENOTDIR: i64 = 20;

/// `-EISDIR`（ディレクトリに対して許されない操作）の errno（S10-b）。
///
/// **この段階では返さない。** `read` がディレクトリを拒む段階（4 本目）で使う。
/// **先に置いてあるのは、`Ext2Error` の対応表を 1 度で書き切るためである。**
pub const EISDIR: i64 = 21;

/// `-EMFILE`（そのプロセスの fd の表が満杯）の errno（S10-b）。
pub const EMFILE: i64 = 24;

/// `-EBUSY`（装置が使用中）の errno（P-c-1）。
///
/// **`close` がイメージを書き戻そうとして、装置の占有が取れなかったときに返す。**
/// **止めるより断るほうが観測できる。**
pub const EBUSY: i64 = 16;

/// `-EROFS`（読み取り専用のファイルシステム）の errno（S10-b）。
///
/// **書き込みで開かれたら、これを返す。** S10 は読み取りだけである
/// （`docs/roadmap.md` の S10 の「実装しない」）。書き込みは S12 である。
pub const EROFS: i64 = 30;

/// `-ENAMETOOLONG`（パスが長すぎる）の errno（S10-b）。
pub const ENAMETOOLONG: i64 = 36;

/// `-EIO`（入出力エラー）の errno（S10-b）。
///
/// **イメージそのものが読めない形をここへ落とす。** 呼び出し側の引数の問題ではないので、
/// **`EINVAL` でも `ENOENT` でもない。**
pub const EIO: i64 = 5;

/// `-EAGAIN`（今は受け付けられない）の errno（S11-2）。
///
/// **遠征の深さが上限に達しているときに返す。**
pub const EAGAIN: i64 = 11;

/// `-EPIPE`（読み手の居ないパイプへ書いた）の errno（`ADR-0063` の (b3)）。値は Linux と同じ
/// 32 である。**`SIGPIPE` は送らない**——**シグナルを持たない**（`crate::pipe` の doc）。
pub const EPIPE: i64 = 32;

/// `-ECHILD`（そのハンドルの子は居ない）の errno（`ADR-0063` の (b3)）。値は Linux と同じ 10 である。
/// **終わった後の二重待ちも同じ値である**（ハンドルの世代が合わない。`crate::task::ring3_task_handle`）。
pub const ECHILD: i64 = 10;

/// **端末に対する要求ではない**（Linux の `ENOTTY` = 25。実測。
/// `/usr/include/asm-generic/errno-base.h`）。
///
/// **2 つの場面で返す**（e-1）——**端末でない fd への `ioctl`** と、
/// **知らない要求**。**Linux も同じ値を両方に使う。**
pub const ENOTTY: i64 = 25;

/// **場所が無い**（Linux の `ENOSPC` = 28。実測。
/// `/usr/include/asm-generic/errno-base.h`）。
///
/// **e-5 で入った**——**`O_CREAT` はイメージの空きを使う。** 空き inode が尽きた、
/// 空きブロックが尽きた、ディレクトリに隙間が無い、のどれでもこれである。
pub const ENOSPC: i64 = 28;

/// **位置を持たないものに位置を与えようとした**（Linux の `ESPIPE` = 29。実測。
/// `/usr/include/asm-generic/errno-base.h`）。**DIR-1b で入った**——
/// 端末の fd に `lseek` を出したときである。
pub const ESPIPE: i64 = 29;

/// **ディレクトリが空でない**（Linux の `ENOTEMPTY` = 39。実測。
/// `/usr/include/asm-generic/errno.h`）。**DIR-1c で入った**——
/// `rmdir` が中身の在るディレクトリを渡されたときである。
pub const ENOTEMPTY: i64 = 39;

/// **その名前は既に在る**（Linux の `EEXIST` = 17。実測）。
///
/// **e-5 で入った。** **`O_CREAT` の経路は「無いとき」しか通らない**ので、
/// **ここへ来るのはイメージの側が食い違っているときだけである**（引けなかったのに
/// 作ろうとしたら在った）。
pub const EEXIST: i64 = 17;

/// `-EACCES`（許されない）の errno（S11-5）。
///
/// **[`crate::abi::private::SYS_SPAWN`] が通常ファイルでないものを渡されたときに返す。**
/// **Linux の `execve` も、実行できない相手に `EACCES` を返す。**
pub const EACCES: i64 = 13;

/// `-E2BIG`（引数が多すぎる、または長すぎる）の errno（S11-7）。
///
/// # `EINVAL` と分ける
///
/// **どちらも「引数が受け付けられない」だが、Linux は分けている**——
/// `execve` は引数と環境が長すぎるときに `E2BIG` を返す。
/// **「値が変」と「量が多い」は、呼び出し側の直し方が違う。**
pub const E2BIG: i64 = 7;

/// `-ENOMEM`（入れる場所が無い）の errno（S11-5）。
///
/// **イメージが [`crate::syscall::MAX_EXECUTABLE_SIZE`] に収まらないとき、およびフレームが尽きたときに
/// 返す。** **`EINVAL` ではない**——イメージは正しく、こちらの器が足りていない。
pub const ENOMEM: i64 = 12;

/// `ENODEV`（画面が無い）。
pub const ENODEV: i64 = 19;

/// `ENOTSOCK`（ソケットでない fd への `bind` など）。
pub const ENOTSOCK: i64 = 88;

/// `EPROTONOSUPPORT`（`protocol` が 0 でない）。
pub const EPROTONOSUPPORT: i64 = 93;

/// `EAFNOSUPPORT`（`AF_UNIX` 以外）。
pub const EAFNOSUPPORT: i64 = 97;

/// `EADDRINUSE`（名前が取られている）。
pub const EADDRINUSE: i64 = 98;

/// `ENOBUFS`（listener のスロットが無い）。
pub const ENOBUFS: i64 = 105;

/// `EISCONN`（繋がっている fd への `connect`）。
pub const EISCONN: i64 = 106;

/// `ENOTCONN`（繋がっていないソケットへの `read` / `write`）。
pub const ENOTCONN: i64 = 107;

/// `ECONNREFUSED`（その名前で待ち受けている者が居ない）。
pub const ECONNREFUSED: i64 = 111;

/// `EMSGSIZE`（補助データが規定の形でない）。
pub const EMSGSIZE: i64 = 90;
