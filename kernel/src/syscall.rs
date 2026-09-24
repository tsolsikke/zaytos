//! システムコール（`int 0x80`）の入口（M5-f-1）。
//!
//! ADR-0020 のとおり、レジスタ規約は Linux x86-64 に合わせる。番号は RAX、
//! 戻り値は RAX、第 1〜6 引数は RDI/RSI/RDX/R10/R8/R9、失敗は `-errno`
//! （`-1..-4095`）。**第 4 引数は RCX ではなく R10** である。`int 0x80` の間は
//! RCX/R11 は実際には保存されるが、`syscall`/`sysret` へ移る段でこれらは命令が
//! 破壊するため、**保存に依存しない**（ユーザー側ラッパはクロバー扱いにする）。
//!
//! # 入口の機構
//!
//! ベクタ 0x80 の IDT ゲートを DPL=3 の割り込みゲートにし、[`crate::idt`] の
//! `zaytos_syscall_stub` へ向ける。スタブは IRQ スタイルの復元経路を写した
//! `zaytos_syscall_common` へ jmp し、GPR 15 本を退避して [`syscall_entry`] を
//! 呼ぶ。Ring 3 からの `int 0x80` は特権変化（3→0）なので、CPU が TSS.RSP0 の
//! スタックへ自動で切り替える（M5-c/d で更新している RSP0 がここで効く）。
//!
//! `irq_entry` とは経路を分けてある。本番 IRQ 経路へ「ソフトウェア割り込みか」の
//! 分岐を足さない方針（ADR-0018 Addendum 3）と揃え、戻り値の RAX 書き戻しという
//! syscall 固有の振る舞いを IRQ 側へ持ち込まないためである。
//!
//! # M5-f-1 の範囲
//!
//! ディスパッチャは検証用の probe システムコール 1 つだけを持つ（M5-f-1-2）。
//! probe は 6 引数と番号を静的領域へ記録し、既知の戻り値 [`PROBE_RETURN`] を返す。
//! これにより「6 引数が規約どおり届き、戻り値が RAX で Ring 3 へ返る」ことを実証
//! する。ユーザーポインタを取るシステムコールは後段（M5-f-2）で足す。
//!
//! # 破壊 feature（M5-f-1-2）
//!
//! - `syscall-test-arg4-rcx`: 第 4 引数を `context.r10` でなく `context.rcx` から
//!   読む。R10 規約の実証（記録した第 4 引数が期待値と食い違う）。
//! - `syscall-test-drop-retval`: 戻り値の `context.rax` 書き戻しを落とす。ユーザーが
//!   期待した戻り値を受け取れない（ユーザースタックへ store した値が食い違う）。
//! - `syscall-test-gate-dpl0`: ゲートを DPL=0 にする（[`crate::idt`] 側）。Ring 3 から
//!   の `int 0x80` がゲート DPL<CPL で #GP になり、`syscall_entry` に到達しない。

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use common::addr::{DirectMap, PhysAddr};

use crate::idt::context::IrqContext;

/// `-ENOSYS`（未実装システムコール）の errno。失敗は `-errno` で返す。
pub const ENOSYS: i64 = 38;

/// `-EFAULT`（不正なアドレス）の errno。ユーザーポインタ検証に落ちたとき返す。
pub const EFAULT: i64 = 14;

/// `-EINVAL`（引数が不正）の errno（S9-a）。**アドレスは正しいが、値が受け付け
/// られない**ときに返す。現在の用途は [`CHECKSUM_BUF_LEN`] の超過だけである。
///
/// 値は Linux と同じ 22 である（ADR-0020 の Addendum で「errno の値を Linux に
/// 合わせる」と決めてある）。
pub const EINVAL: i64 = 22;

/// `-ENOENT`（そのパスは無い）の errno（S10-b）。値は Linux と同じ 2 である。
pub const ENOENT: i64 = 2;

/// `-EBADF`（そのファイルディスクリプタは開いていない）の errno（S10-b）。
pub const EBADF: i64 = 9;

/// `-ENOTDIR`（ディレクトリでないものをディレクトリとして辿った）の errno（S10-b）。
pub const ENOTDIR: i64 = 20;

/// `-EISDIR`（ディレクトリに対して許されない操作）の errno（S10-b）。
///
/// **この段では返さない。** `read` がディレクトリを拒む段（4 本目）で使う。
/// **先に置いてあるのは、`Ext2Error` の対応表を 1 度で書き切るためである。**
pub const EISDIR: i64 = 21;

/// `-EMFILE`（そのプロセスの fd の表が満杯）の errno（S10-b）。
pub const EMFILE: i64 = 24;

/// `-EBUSY`（装置が使用中）の errno（P-c-1）。
///
/// **`close` が像を書き戻そうとして、装置の占有が取れなかったときに返す。**
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
/// **像そのものが読めない形をここへ落とす。** 呼び出し側の引数の問題ではないので、
/// **`EINVAL` でも `ENOENT` でもない。**
pub const EIO: i64 = 5;

/// `-EAGAIN`（今は受け付けられない）の errno（S11-2）。
///
/// **遠征の深さが上限に達しているときに返す。**
pub const EAGAIN: i64 = 11;

/// `-EPIPE`（読み手の居ないパイプへ書いた）の errno（`ADR-0063` の (b3)）。値は Linux と同じ
/// 32 である。**`SIGPIPE` は送らない**——**シグナルを持たない**（`crate::pipe` の doc）。
pub const EPIPE: i64 = 32;

/// `-ECHILD`（その手形の子は居ない）の errno（`ADR-0063` の (b3)）。値は Linux と同じ 10 である。
/// **終わった後の二重待ちも同じ値である**（手形の世代が合わない。`crate::task::ring3_task_handle`）。
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
/// **e-5 で入った**——**`O_CREAT` は像の空きを使う。** 空き inode が尽きた、
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
/// **ここへ来るのは像の側が食い違っているときだけである**（引けなかったのに
/// 作ろうとしたら在った）。
pub const EEXIST: i64 = 17;

/// `-EACCES`（許されない）の errno（S11-5）。
///
/// **[`SYS_SPAWN`] が通常ファイルでないものを渡されたときに返す。**
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
/// **像が [`MAX_EXECUTABLE_SIZE`] に収まらないとき、およびフレームが尽きたときに
/// 返す。** **`EINVAL` ではない**——像は正しく、こちらの器が足りていない。
pub const ENOMEM: i64 = 12;

/// ZaytOS 独自のシステムコール番号の基点（S9-a）。
///
/// # なぜ Linux の番号表から離すのか
///
/// ADR-0020 の Addendum で「番号の割り当ては Linux x86-64 から採る」と決めた。
/// **`read`=0 や `write`=1 のように Linux に対応するものがある呼び出しは、その
/// 番号を使う。** 問題は、対応するものが無い呼び出しである。下記の検証用
/// システムコールは ZaytOS 固有で、Linux に相当するものが未来にも現れない。
///
/// **かつては 0x2A・0x2B・0x2C に置いており、Linux の 42（`connect`）・
/// 43（`accept`）・44（`sendto`）と衝突していた。** 番号表の中の空きに置くと、
/// Linux がそこを埋めた時点で衝突する（歴史的に未実装のまま空いている番号も、
/// 将来 Linux が再利用しうる）。**表の中に安全な空きは無い。**
///
/// そこで表の外へまとめる。Linux x86-64 の番号は現在 500 未満で、増え方は年に
/// 数本である。**0x1000（4096）なら当面ぶつからない。** x32 ABI が使う
/// `0x4000_0000` のビットとも重ならない。
///
/// **独自の呼び出しを足すときは、必ずこの基点より上に置くこと。**
pub const ZAYTOS_PRIVATE_BASE: u64 = 0x1000;

/// ユーザーポインタを取る検証用システムコールの番号（M5-f-2-1）。
/// 第 1 引数(RDI)=buf、第 2 引数(RSI)=len。範囲が Ring 3 からアクセス可能なら 0、
/// 不可なら -EFAULT を返す（**この段はバイトを読まない**。copy は M5-f-2-2）。
pub const SYS_CHECK_PTR: u64 = ZAYTOS_PRIVATE_BASE + 1;

/// ユーザーバッファのバイト総和（チェックサム）を返すシステムコールの番号
/// （M5-f-2-2）。第 1 引数(RDI)=buf、第 2 引数(RSI)=len。範囲を検証してから
/// 範囲内バイトを読み総和を返す。不正な範囲なら -EFAULT、長さが
/// [`CHECKSUM_BUF_LEN`] を超えるなら -EINVAL。
pub const SYS_CHECKSUM: u64 = ZAYTOS_PRIVATE_BASE + 2;

/// SYS_CHECKSUM がユーザーバイトを読み込む固定カーネルバッファの大きさ。
///
/// これを超える len は -EINVAL で弾く。**意味的には「引数の値が受け付けられない」
/// のであって、ポインタ不正（EFAULT = Bad address）ではない。** S9-a より前は
/// errno が 2 つしか無く -EFAULT で代用していた。
pub const CHECKSUM_BUF_LEN: usize = 64;

/// ユーザーポインタとして受理する下限（S9-b-3-2b）。**方針である。**
///
/// # 理由が変わった。値は変わっていない
///
/// **S9-b-1 でこの値を置いた理由は、起動順の偶然だった。** ポインタ検証の battery は
/// 恒等除去より前に走るので、その時点の低位 VA にはカーネルの恒等写像が居る。
/// 下限を 0 にすると `0x100000`（カーネル像）が範囲の検査を通ってしまい、
/// **U=1 の判定だけが拒否の根拠になる**（`validate-skip-us` の破壊で受理された）。
///
/// **S9-b-3-2b で窓を 1 つに畳んだので、その理由は当たらなくなった。** 起動時の
/// battery が使う窓は本番の空間のユーザーサブツリー（`PML4[1]` = 512 GiB 以上）で、
/// カーネル像はそもそも窓の外である。
///
/// **それでも 0 にしない。** null 近傍を**範囲の側でも**拒む層を残す。Linux の
/// `mmap_min_addr` が低位を空けておくのと同じ向きで、**層を 1 枚減らすには
/// 減らす理由が要る。** 減らす理由が無い。
///
/// **同じ値を、違う根拠で持っている。**
pub const USER_MIN_ADDR: u64 = 0x40_0000;

/// PML4 の添字 1 つ分が覆う仮想範囲の大きさ（512 GiB）。
const PML4_ENTRY_SPAN: u64 = 1 << 39;

/// ユーザーサブツリーの添字から、ポインタ検証の窓を導く（S9-b-3-2b）。
///
/// 返すのは `[start, end)` で、`start` は [`USER_MIN_ADDR`] で床を打ってある。
///
/// # 窓は 1 つである
///
/// **S9-b-1 から S9-b-3-2a までは 2 つあった**（起動時の検証用と、ユーザー
/// プログラム用）。どちらか一方に収まっていれば受理する形で、**またぐ範囲を
/// 受理しない条件を明示的に書く必要があった。**
///
/// **1 つに畳むと、その条件は消える。** またぐ範囲が受理されないのは、
/// **窓が 1 つしかないからである**（書かれた条件ではなく、構造の帰結になった）。
///
/// # 窓が有限であることが、走査の停止性を与えている
///
/// 「下位半分すべて」へ広げてはならない。広げると長さの上限が消え、
/// `over-long` のような呼び出しでページ走査が何百万回もまわる。
pub const fn window_for_subtree(index: usize) -> (u64, u64) {
    let start = (index as u64) * PML4_ENTRY_SPAN;
    let end = start + PML4_ENTRY_SPAN;
    if start < USER_MIN_ADDR {
        (USER_MIN_ADDR, end)
    } else {
        (start, end)
    }
}

/// `write(fd, buf, len)`（S9-b-1）。**Linux の番号 1 をそのまま使う**
/// （ADR-0020 の Addendum。対応するものがある呼び出しは Linux の番号を採る）。
///
/// 現在の実装は `fd` を見ず、**バイト列を静的領域へ記録して長さを返すだけである。**
/// シリアルへは出さない。`syscall_entry` は出力しないという既存の方針
/// （例外・IRQ ハンドラと同じ）に従い、**観測は畳んで戻った後に呼び出し側が
/// 記録越しに行う。**
pub const SYS_WRITE: u64 = 1;

/// [`SYS_WRITE`] が記録するバイト数の上限。
pub const WRITE_BUF_LEN: usize = 64;

/// `exit(status)`（S9-b-3-1）。**Linux の番号 60 をそのまま使う。**
///
/// # `exit_group`（231）は採らない
///
/// あちらは「呼んだスレッドが属するスレッドグループ全体を終わらせる」呼び出しで、
/// **ZaytOS にはスレッドの概念が無い。** 番号を用意しても、`exit` と区別できる
/// 振る舞いが書けない。**同じ振る舞いの入口を 2 つ置くと、どちらが正なのかが
/// 呼び出し側にも実装側にも決まらない。** スレッドを作る段で足す。
///
/// # 戻らない
///
/// **[`dispatch`] の戻り値では「戻らない」を表せない**ので、記録だけをあちらで
/// 行い、**Ring 3 へ返らない分岐は [`syscall_entry`] が持つ**（あちらの
/// 「exit は出口を通らない」の節）。
pub const SYS_EXIT: u64 = 60;

/// `read(fd, buf, count)`（S10-b）。**Linux の番号 0 をそのまま使う。**
pub const SYS_READ: u64 = 0;

/// `getdents64(fd, dirp, count)`（S10-b）。**Linux の番号 217 をそのまま使う。**
pub const SYS_GETDENTS64: u64 = 217;

/// `linux_dirent64` の固定部のバイト数。**実測で確かめた**（`d_name` の `offsetof`）。
///
/// `d_ino`(8) + `d_off`(8) + `d_reclen`(2) + `d_type`(1) = 19 である。
///
/// # `sizeof(struct dirent)` は 280 だが、それは別物である
///
/// **あれは受け皿の型の大きさ**（`d_name[256]` を含む）で、
/// **`getdents64` が書くレコードの大きさではない。** レコードは可変長で、
/// 長さは `d_reclen` が持つ。**280 を定数として持ち込まない。**
pub const DIRENT64_HEADER_LEN: usize = 19;

/// `linux_dirent64` のレコードの整列。**8 バイト境界へ切り上げる**（実測で確かめた）。
const DIRENT64_ALIGN: usize = 8;

/// `d_type`: 不明。**対応表に無い値はこれにする。**
pub const DT_UNKNOWN: u8 = 0;
/// `d_type`: ディレクトリ（実測）。
pub const DT_DIR: u8 = 4;
/// `d_type`: 通常ファイル（実測）。
pub const DT_REG: u8 = 8;

/// `stat(path, statbuf)`（S10-b）。**Linux の番号 4 をそのまま使う。**
///
/// # `fstat`（5）は置かない
///
/// あちらは fd を取る。**表の中の inode を返すだけなので実装は短いが、
/// 要ると分かってから足す**（S10-b の棚卸しの判断）。
pub const SYS_STAT: u64 = 4;

/// `struct stat` のバイト数（x86-64 の Linux）。**実測で確かめた。**
///
/// # 欄の位置
///
/// `gcc` の `offsetof` で測った値である（`sys/stat.h`）。
/// **記憶から書かない**（`docs/coding-standards.md` の「実測値は、測った条件が
/// 変わると古くなる」）。
///
/// | 欄 | 位置 | 幅 |
/// |---|---|---|
/// | `st_dev` | 0 | 8 |
/// | `st_ino` | 8 | 8 |
/// | `st_nlink` | 16 | 8 |
/// | `st_mode` | 24 | 4 |
/// | `st_uid` | 28 | 4 |
/// | `st_gid` | 32 | 4 |
/// | `st_rdev` | 40 | 8 |
/// | `st_size` | 48 | 8 |
/// | `st_blksize` | 56 | 8 |
/// | `st_blocks` | 64 | 8 |
/// | `st_atim` | 72 | 16 |
/// | `st_mtim` | 88 | 16 |
/// | `st_ctim` | 104 | 16 |
///
/// 36..40 と 120..144 は詰め物である（`__pad0` と `__unused[3]`）。
pub const STAT_LEN: usize = 144;

/// `struct stat` の欄の位置。**上の表と対になっている。**
const STAT_INO: usize = 8;
const STAT_NLINK: usize = 16;
const STAT_MODE: usize = 24;
const STAT_SIZE: usize = 48;
const STAT_BLOCKS: usize = 64;

/// `clock_gettime(clockid, timespec)`（W2-d+）。**Linux の番号 228 をそのまま使う。**
///
/// # `CLOCK_MONOTONIC` だけを実装する
///
/// **壁時計（`CLOCK_REALTIME` = 0）は持てない**——**ZaytOS に実時刻の出所が無い**
/// （RTC は未実装。`docs/deferred-decisions.md` の「時刻の欄」）。
/// **0 を返して黙って答えると嘘の時刻が広がる**ので、`-EINVAL` を返す。
pub const SYS_CLOCK_GETTIME: u64 = 228;

/// `CLOCK_MONOTONIC`（Linux x86-64 の値）。**実測で確かめた**——
/// `/usr/include/x86_64-linux-gnu/bits/time.h` が `1` と定義している（確認日 2026-09-17。
/// **`CLOCK_REALTIME` は `0` である**）。
pub const CLOCK_MONOTONIC: u64 = 1;

/// `struct timespec` のバイト数（x86-64 の Linux）。**実測で確かめた。**
///
/// # 欄の位置
///
/// `gcc` の `offsetof` で測った値である（`cc` 13.3.0。確認日 2026-09-17）。
/// **記憶から書かない**（[`STAT_LEN`] と同じ手順である）。
///
/// | 欄 | 位置 | 幅 |
/// |---|---|---|
/// | `tv_sec` | 0 | 8 |
/// | `tv_nsec` | 8 | 8 |
///
/// **どちらも符号つき 64 ビットで、詰め物は無い**（`__time_t` と `__syscall_slong_t` が
/// ともに `__SYSCALL_SLONG_TYPE` である）。
///
/// **[`STAT_LEN`] の表の `st_atim`（位置 72、幅 16）と整合する**——**あちらが既に
/// この配置を前提にしていた。**
pub const TIMESPEC_LEN: usize = 16;

/// `struct timespec` の `tv_nsec` の位置。**上の表と対になっている。**
const TIMESPEC_NSEC: usize = 8;

/// `nanosleep(req, rem)`（W2-d+）。**Linux の番号 35 をそのまま使う。**
///
/// # `rem` には書かない
///
/// **Linux が `rem` へ書くのは、シグナルで割り込まれて `EINTR` を返すときだけである。**
/// **ZaytOS にシグナルは無い**ので、**割り込まれて戻る道が無い。** **受け取って読まない。**
pub const SYS_NANOSLEEP: u64 = 35;

/// `open(path, flags, mode)`（S10-b）。**Linux の番号 2 をそのまま使う。**
///
/// # `openat`（257）は採らない
///
/// **ZaytOS には作業ディレクトリが無い**ので、`dirfd` に渡すものが無い。
/// **`AT_FDCWD` を受けるだけの引数を置いても、区別できる振る舞いが書けない**
/// （[`SYS_EXIT`] が `exit_group` を採らない理由と同じ形である）。
/// **作業ディレクトリを持つ段で足す。**
pub const SYS_OPEN: u64 = 2;

/// `close(fd)`（S10-b）。**Linux の番号 3 をそのまま使う。**
pub const SYS_CLOSE: u64 = 3;

/// `ioctl(fd, request, arg)`（e-1）。**Linux の番号 16 をそのまま使う**
/// （実測。`/usr/include/x86_64-linux-gnu/asm/unistd_64.h` の `__NR_ioctl`）。
///
/// # 入口の方針——端末の問い合わせに限る
///
/// **`ioctl` は「何でも入る雑多な入口」である。** 最初の1つを入れる時点で
/// **何を入れ、何を入れないかを決めてある**——**受けるのは端末の問い合わせだけ**
/// で、**設定の変更（`termios` 相当・`TIOCSWINSZ`）は別の判断とする。**
/// **知らない要求は `-ENOTTY` で断る**ので、**入口が黙って広がることはない。**
/// **決定の記録は `docs/deferred-decisions.md` にある**（解禁の契機は
/// 「設定の変更を要求する利用者が来たとき」。**C の移植で必ず来る**）。
pub const SYS_IOCTL: u64 = 16;

/// `TIOCGWINSZ`——端末の大きさを訊く要求（e-1）。**Linux の値をそのまま使う**
/// （実測。`/usr/include/asm-generic/ioctls.h` の `0x5413`）。
pub const TIOCGWINSZ: u64 = 0x5413;

/// `TIOCZTAKE`——溜まっているエラーを取り出す要求（ADR-0046）。
///
/// # ZaytOS の値である。Linux の値ではない
///
/// **全画面のアプリが動く間、`fd 2`はカーネルが溜める。** **アプリが
/// これで取り出し、自分のエコーエリアへ描く**（ADR-0046）。
/// **Linux にこの操作は無い**ので、**`TIOC` の空間の外に置く**
/// （`0x5A` は `Z`。`TIOCGWINSZ` の `0x5413` と衝突しない）。
///
/// **新しい syscall 番号は作らない**——**`ADR-0020`は番号を Linux から
/// 採ると決めており、相当する番号が無い。** **`ioctl`は端末固有の操作の
/// ための口である。**
pub const TIOCZTAKE: u64 = 0x5A01;

/// `TIOCZLOG`——1 行をログ（シリアル）へ出す要求（ADR-0046）。
///
/// **画面へは出さない。** **検査のための診断の出口であり、読み手は
/// ホスト側の判定である**（ADR-0046 の「線はどこに在るか」）。
pub const TIOCZLOG: u64 = 0x5A02;

/// `TIOCZTAKE` / `TIOCZLOG` がやり取りする構造の大きさ（ADR-0046）。
pub const ZDIAG_LEN: usize = crate::console::pending::ZDIAG_LEN;

/// その構造の本文が始まる位置（ADR-0046）。**手前の 4 バイトは長さと捨てた数である。**
pub const ZDIAG_TEXT_OFFSET: usize = 4;

/// `struct winsize` の大きさ（e-1）。**`u16` が 4 つである**
/// （実測。`/usr/include/x86_64-linux-gnu/bits/ioctl-types.h`。
/// 順に `ws_row` / `ws_col` / `ws_xpixel` / `ws_ypixel`）。
pub const WINSIZE_LEN: usize = 8;

/// `open` の第 2 引数のうち、アクセスモードを表すビット（Linux の `O_ACCMODE`）。
pub const O_ACCMODE: u64 = 0o3;

/// 読み取りで開く（Linux の `O_RDONLY`）。**受理するのはこれだけである。**
pub const O_RDONLY: u64 = 0o0;

/// 書き込みで開く（Linux の `O_WRONLY`）。zi-c で受理に加わった。
pub const O_WRONLY: u64 = 0o1;

/// 開くと同時に長さ 0 へ切る（Linux の `O_TRUNC`）。zi-c で受理に加わった。
pub const O_TRUNC: u64 = 0o1000;

/// 無ければ作る（Linux の `O_CREAT`）。e-5 で受理に加わった（ADR-0037 の Addendum）。
pub const O_CREAT: u64 = 0o100;

/// `lseek` の番号（Linux と同じ。DIR-1b）。
///
/// # 部品は S10-b から在り、入口が無かっただけである
///
/// **`crate::vfs::File::seek_to` が最初から在る。** **使う者が居なかったので
/// 入口を置いていなかった**（`docs/foundation-inventory.md` が
/// 「部品は在るが入口が無い」として挙げていた 2 つのうちの 1 つ）。
///
/// **利用者は `/bin/tail` である**（DIR-1b で同じ段に作った）。
pub const SYS_LSEEK: u64 = 8;

/// `mkdir` の番号（Linux と同じ。DIR-1c）。**利用者は `/bin/mkdir` である。**
pub const SYS_MKDIR: u64 = 83;

/// `rmdir` の番号（Linux と同じ。DIR-1c）。**利用者は `/bin/rmdir` である。**
pub const SYS_RMDIR: u64 = 84;

/// `brk` の番号（Linux と同じ。H-a。ADR-0044）。
///
/// # `brk(0)` は問い合わせである
///
/// **Linux と同じ形にする**——**0 を渡すと、いまの上端が返る。**
/// **別の番号を用意しない**（`sbrk` は libc の側の話である）。
///
/// # 返すのは新しい上端である
///
/// **失敗しても `-errno` を返す**（**Linux は失敗すると古い上端を返す**が、
/// **こちらは `-errno` にする**——**「動かなかった」と「そこまでしか
/// 伸びなかった」を、呼ぶ側が区別できる形にする**）。
pub const SYS_BRK: u64 = 12;

/// `unlink` の番号（Linux と同じ。DIR-1b）。
///
/// **`common::ext2::unlink_file` が S12-e から在り、入口が無かっただけである。**
/// **利用者は `/bin/rm` である。**
pub const SYS_UNLINK: u64 = 87;

/// `lseek` の `whence`——先頭からの絶対位置（`SEEK_SET`）。
///
/// # ここだけ受ける
///
/// **`SEEK_CUR` と `SEEK_END` は受けない**（`-EINVAL`）。
/// **使う者が居ない**——`/bin/tail` は `stat` で大きさを取ってから
/// `SEEK_SET` で跳ぶ。**要る者が来たら足す。**
pub const SEEK_SET: u64 = 0;

/// 書き込みを伴う `open` のフラグ（`O_CREAT` / `O_TRUNC` / `O_APPEND`）。
///
/// **アクセスモードが読み取りでも、これらは書き込みを要求する。**
/// **どれかが立っていたら `-EROFS` である。**
pub const O_WRITE_INTENT: u64 = 0o100 | 0o1000 | 0o2000;

/// カーネルが受け取るパスの最大長（NUL を含まない。S10-b）。
///
/// # Linux の `PATH_MAX`（4096）より小さい
///
/// **像の中で最も長いパスは `/data/indirect-first` の 20 バイトである。**
/// 256 はその 10 倍を超える。**4096 にしない理由は置き場所である**——
/// パスは `dispatch` の中でカーネルスタックへ写すので、
/// **4096 バイトの単一のローカル配列は `deferred-decisions.md` の
/// 「大きなスタック配列とガード幅」の解禁条件に当たる。**
///
/// # `MAX_PATH_COMPONENTS` はまだ要る
///
/// 256 バイトあれば `/a` の形で 128 要素まで書けるので、
/// **`common::ext2::MAX_PATH_COMPONENTS`（64）のほうが先に効く。**
/// **両方が意味を持っている**ので、どちらも残す
/// （あちらの doc に「どちらか一方でよい」と書いたが、**この値では一方に
/// ならなかった**）。
pub const PATH_MAX: usize = 256;

/// `spawn(path)`——像を読み、子プロセスを起こし、**終わるまで待つ**（S11-5。ZaytOS 独自）。
///
/// # なぜ `fork`（57）と `execve`（59）の番号を採らないか
///
/// **これは `fork` でも `execve` でもない。** 番号だけ借りると、
/// **Linux の意味を持たない振る舞いに Linux の名前が付く。**
///
/// - **`fork` は呼び出し側を複製する。** ここが作るのは複製ではなく、
///   **別の像から起こした別のプロセスである。** 写像も `argv` も引き継がない
/// - **`execve` は呼び出し側を置き換える。** ここは置き換えない——
///   **親はそのまま在り、子が終わるのを待って続きを実行する**
/// - **どちらも「戻る」の意味が違う。** `fork` は 2 回戻り、`execve` は成功したら
///   戻らない。**`spawn` は 1 回戻り、戻り値は子の終わり方である**
///
/// **`SYS_OPEN`（2）が `openat`（257）を採らなかったのと同じ判断である**
/// ——「振る舞いが違うなら、番号も分ける」。**あちらは作業ディレクトリが無いから
/// `openat` を採らず、こちらは意味が違うから 57 と 59 を採らない。**
///
/// **予約もしない。** [`SYS_NEVER_IMPLEMENTED`] のような「永久に実装しない」宣言では
/// なく、**`fork` と `execve` は将来ふつうに実装しうる**（`docs/vision.md` の
/// Linux バイナリを動かす構想）。**空けておけば、そのとき Linux の意味で使える。**
///
/// # 戻り値
///
/// 子が `exit(status)` で終わったなら `status & 0xFF`。
/// 畳まれて終わったなら [`SPAWN_FOLDED_FLAG`] とベクタ。
/// 起こせなかったなら `-errno`。**`docs/coding-standards.md` の「`-errno` の範囲と
/// 紛れない値にする」に従い、正の側は 0x1FFF を越えない。**
pub const SYS_SPAWN: u64 = ZAYTOS_PRIVATE_BASE + 4;

/// [`SYS_SPAWN`] の戻り値のうち「子は終了ではなく畳まれて終わった」を表すビット。
///
/// **下位 8 ビットは終了状態なので、その上に置く。** 畳まれた場合は
/// `SPAWN_FOLDED_FLAG | (vector << 9)` を返す。
///
/// # Linux の `wait` の符号化には合わせない
///
/// **`spawn` は Linux に対応するものが無い**ので、`W*` マクロの形を真似ても
/// 互換にはならない。**外から見える形を Linux に合わせるのは、Linux に同じものが
/// あるときの規則である。**
pub const SPAWN_FOLDED_FLAG: u64 = 0x100;

/// [`SYS_SPAWN`] の戻り値のうち「子は外から止められた」を表すビット
/// （Ctrl+C。S12 前の手当て、C）。
///
/// **[`SPAWN_FOLDED_FLAG`] の隣に置く。** 下位 8 ビットは終了状態なので、
/// **その上のビットで「終了以外の終わり方」を並べる形である。**
/// **`0x1FFF` を越えないので `-errno` と紛れない**（`SYS_SPAWN` の doc）。
///
/// # Linux の `128 + signo` を採らない
///
/// **`spawn` は Linux に対応するものが無い**（[`SPAWN_FOLDED_FLAG`] の doc）。
/// **加えて、まだシグナルが無い**——番号を持たないものに `128 + signo` の形を
/// 与えると、**「`SIGINT` が配送された」と読める値を、配送していないのに返す。**
/// **シグナルを実装する段（(4)）で、そのとき改めて決めること。**
pub const SPAWN_INTERRUPTED_FLAG: u64 = 0x200;

/// 起こしっぱなしで子を起こす（`ADR-0063` の (b3)）。**私物である**（[`SYS_SPAWN`] と同じ判断
/// ——Linux に同じ意味の口が無い。`posix_spawn` はライブラリの関数で、システムコールではない）。
///
/// 引数は `path` / `argv` / `envp` / `flags`（[`DETACHED_STDOUT_TO_PIPE`]）。**戻り値は手形**
/// （`crate::task::ring3_task_handle`。(b2) の形）**か `-errno`。**
///
/// # 子が Ring 3 へ入るか終わるまで戻らない
///
/// **フレームアロケータの貸し出しは大域に 1 つである**（`docs/wayland-inventory.md` の #4）。
/// **戻ってすぐシェルが右を `spawn` すると、左の読み込みと重なって `AllocatorUnavailable` に
/// なる。** **`concurrent-test` が「1 本を Ring 3 へ入れてから次を起こす」で避けたのと同じ順序を、
/// 口の中で守る。** **待ちは `Wait` を使わず、譲るの繰り返しである**——**`Wait::ChildStarted` を
/// 作れば起こす側も作れる（子が入った時点で起こす）が、待つ長さが読み込み 1 回ぶん
/// （ティックの桁）なので足さない。** **待ったティック数は計器に出す**
/// （[`detached_entry_wait_ticks_max`]）。**上限は置かない**——**読み込みは必ず成功か失敗で終わる。**
///
/// **見つからなければ同期で `-ENOENT` を返す**（起こす前に探す。`crate::userland::probe_program`）
/// ——**シェルの `PATH` の輪が次の要素へ進める。**
pub const SYS_SPAWN_DETACHED: u64 = ZAYTOS_PRIVATE_BASE + 5;

/// [`SYS_SPAWN_DETACHED`] の `flags`——子の fd 1 をパイプの書き端にし、読み手を予約する
/// （`crate::pipe` の doc の「読み手の予約」）。
pub const DETACHED_STDOUT_TO_PIPE: u64 = 1;

/// 予約したパイプの読み端を fd 0 にして、入れ子で起こす（`ADR-0063` の (b3)）。**私物。**
///
/// **[`SYS_SPAWN`] と同じ形で戻る**（終わり方のビット）。**予約が無ければ `-EINVAL`。**
/// **[`SYS_SPAWN`] に `flags` を足さない理由**——**既存の呼び手は `r10` を置かないので、
/// 4 つ目の引数を見る形にすると、置いていない値を読む。**
pub const SYS_SPAWN_WITH_PIPED_STDIN: u64 = ZAYTOS_PRIVATE_BASE + 6;

/// 起こしっぱなしの子を待って回収する（`ADR-0063` の (b3)）。**私物。**
///
/// **引数は手形。** **戻り値は終わり方のビット**（[`SYS_SPAWN`] と同じ）**か `-ECHILD`**
/// （手形が合わない・終わった後の二重待ち）。**`wait4` を採らない**——**形が合わない**
/// （`docs/architecture.md` の「合わせるのは合わせられる形について」）。
///
/// **使われなかった読み手の予約は、ここで消す**——**右が見つからなかったとき、左が満杯で
/// 永久に待つのを防ぐ**（`crate::pipe::drop_reservation`）。
pub const SYS_WAIT_CHILD: u64 = ZAYTOS_PRIVATE_BASE + 7;

/// 入力の生イベントの fd を開く（`ADR-0066` の Y-a）。**私物。**
///
/// **Linux に対応する syscall が無い**——**あちらは `/dev/input/eventX` を `open` する**が、
/// ZaytOS に装置のファイルシステムは無い。**したがって番号は私物にする**（`SYS_SPAWN` 等と
/// 同じ。`ADR-0020` の「合わせられる形について合わせる」）。
///
/// **前景の持ち主でなければ `-EBADF`**（開く時点の1箇所で守る。`ADR-0066` の
/// 「入力 fd の前景の関所」）。**読みは `read` が `struct input_event` を返す。**
pub const SYS_OPEN_INPUT: u64 = ZAYTOS_PRIVATE_BASE + 8;

/// 画面を開く口の番号（`ADR-0066` の Y-c）。**開くと図形モードへ入る。**
///
/// **Linux に対応する syscall が無い**——**あちらは `/dev/fb0`（fbdev）か `/dev/dri/card0`（DRM）を
/// `open` する**が、ZaytOS に装置のファイルシステムは無い。**番号は私物にする**（[`SYS_OPEN_INPUT`] と
/// 同じ理由）。**開いた後の形は Linux の fbdev に合わせる**——**形は `ioctl` の
/// [`FBIOGET_VSCREENINFO`] / [`FBIOGET_FSCREENINFO`]、画素は `mmap`。**
///
/// **呼んだ者が前景の系統でなければ `-EBADF`**（`crate::input::caller_is_foreground`）。
/// **既に誰かが図形モードなら `-EBUSY`、画面が無ければ `-ENODEV`。**
pub const SYS_OPEN_SCREEN: u64 = ZAYTOS_PRIVATE_BASE + 9;

/// `FBIOGET_VSCREENINFO`（Linux の fbdev。`<linux/fb.h>`）。**`struct fb_var_screeninfo` を返す。**
pub const FBIOGET_VSCREENINFO: u64 = 0x4600;
/// `FBIOGET_FSCREENINFO`（Linux の fbdev）。**`struct fb_fix_screeninfo` を返す。**
pub const FBIOGET_FSCREENINFO: u64 = 0x4602;
/// 画面の矩形を写す要求（ZaytOS 独自。`ADR-0066` の Y-c）。**引数は `struct drm_clip_rect`。**
///
/// **fbdev に対応するものが無い**——**fbdev は実物のフレームバッファを張るので、写す必要が無い。**
/// **ZaytOS は裏バッファを張る**（Q1。MMIO を Ring 3 へ出さない）**ので、写す口が要る。**
/// **Linux で近いのは DRM の `DRM_IOCTL_MODE_DIRTYFB` で、矩形の配置（`struct drm_clip_rect`）だけを
/// 採る**——**DIRTYFB そのものは DRM の大きな ABI の一部なので採らない。** **番号は [`TIOCZTAKE`] と
/// 同じ `'Z'` の帯に置く。**
pub const FBIOZPRESENT: u64 = 0x5A03;

/// `struct fb_var_screeninfo` のバイト数（`cc` の `sizeof` で測った。2026-09-21）。
pub const FB_VAR_SCREENINFO_LEN: usize = 160;
/// `struct fb_fix_screeninfo` のバイト数（同上）。
pub const FB_FIX_SCREENINFO_LEN: usize = 80;
/// `struct drm_clip_rect` のバイト数（同上。`u16` の x1・y1・x2・y2）。
pub const DRM_CLIP_RECT_LEN: usize = 8;
/// `FB_TYPE_PACKED_PIXELS`（`<linux/fb.h>`）。
const FB_TYPE_PACKED_PIXELS: u32 = 0;
/// `FB_VISUAL_TRUECOLOR`（`<linux/fb.h>`）。
const FB_VISUAL_TRUECOLOR: u32 = 2;
/// `ENODEV`（画面が無い）。
const ENODEV: i64 = 19;

/// 画素の色の並び（`struct fb_bitfield` の `offset`）。**青・緑・赤の順に返す。**
///
/// **UEFI の `Bgr` は「バイト 0 が青」、`Rgb` は「バイト 0 が赤」である**（`PixelFormat` の doc）。
/// **リトルエンディアンの 32 ビットで読むので、バイトの位置 × 8 がビットの位置になる。**
pub const fn fb_color_offsets(bgr: bool) -> (u32, u32, u32) {
    if bgr {
        (0, 8, 16)
    } else {
        (16, 8, 0)
    }
}

/// `struct fb_var_screeninfo` を組む（`ADR-0066` の Y-c）。**引数だけで決める**（ホストで固定する）。
///
/// **欄の位置は `cc` の `offsetof` で測った**（2026-09-21。`cc` 13.3.0。`<linux/fb.h>`）——
/// `xres` 0 / `yres` 4 / `xres_virtual` 8 / `yres_virtual` 12 / `bits_per_pixel` 24 /
/// `red` 32 / `green` 44 / `blue` 56 / `transp` 68（`struct fb_bitfield` は `offset` 0・`length` 4・
/// `msb_right` 8 の 12 バイト）。**それ以外の欄は 0 である**（パンも回転も持たない）。
pub fn fb_var_screeninfo(width: u32, height: u32, bgr: bool) -> [u8; FB_VAR_SCREENINFO_LEN] {
    let mut out = [0u8; FB_VAR_SCREENINFO_LEN];
    let mut put = |at: usize, value: u32| out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    put(0, width);
    put(4, height);
    put(8, width);
    put(12, height);
    put(24, 32);
    let (blue, green, red) = fb_color_offsets(bgr);
    // **`struct fb_bitfield` は `offset`・`length`・`msb_right` の順である。**
    put(32, red);
    put(36, 8);
    put(44, green);
    put(48, 8);
    put(56, blue);
    put(60, 8);
    out
}

/// `struct fb_fix_screeninfo` を組む（`ADR-0066` の Y-c）。**引数だけで決める。**
///
/// **欄の位置は `offsetof` で測った**——`id` 0（16 バイト）/ `smem_start` 16 / `smem_len` 24 /
/// `type` 28 / `visual` 36 / `line_length` 48（2026-09-21）。
///
/// **`smem_start`（物理番地）は 0 にする**——**合わせなかった。** **Ring 3 へ物理番地を出す理由が
/// 無い**（`mmap` は fd から張るので、番地を知らなくてよい）。
pub fn fb_fix_screeninfo(size_bytes: u32, line_length: u32) -> [u8; FB_FIX_SCREENINFO_LEN] {
    let mut out = [0u8; FB_FIX_SCREENINFO_LEN];
    let id = b"zaytos-fb";
    out[..id.len()].copy_from_slice(id);
    out[24..28].copy_from_slice(&size_bytes.to_le_bytes());
    out[28..32].copy_from_slice(&FB_TYPE_PACKED_PIXELS.to_le_bytes());
    out[36..40].copy_from_slice(&FB_VISUAL_TRUECOLOR.to_le_bytes());
    out[48..52].copy_from_slice(&line_length.to_le_bytes());
    out
}

/// `struct drm_clip_rect` を読む（`ADR-0066` の Y-c）。**`(x, y, 幅, 高さ)` を返す。空なら `None`。**
///
/// **x2・y2 は含まない**（DRM の DIRTYFB と同じ半開区間）。**画面への切り詰めは写す側が行う**
/// （`Console::present`）。
pub fn parse_clip_rect(raw: &[u8; DRM_CLIP_RECT_LEN]) -> Option<(u32, u32, u32, u32)> {
    let x1 = u32::from(u16::from_le_bytes([raw[0], raw[1]]));
    let y1 = u32::from(u16::from_le_bytes([raw[2], raw[3]]));
    let x2 = u32::from(u16::from_le_bytes([raw[4], raw[5]]));
    let y2 = u32::from(u16::from_le_bytes([raw[6], raw[7]]));
    if x2 <= x1 || y2 <= y1 {
        return None;
    }
    Some((x1, y1, x2 - x1, y2 - y1))
}

/// `socket` の番号（Linux x86-64。`ADR-0064`）。**番号と `sockaddr_un` の配置は Linux から採る**
/// （`ADR-0020`。**私物にしない**——**パイプの口が私物だったのは `spawn` の形に付いたからで、
/// ソケットは Linux の形そのものが在る**）。
///
/// **受けるのは `socket(AF_UNIX, SOCK_STREAM, 0)` だけである。** **`type` の旗
/// （`SOCK_CLOEXEC` / `SOCK_NONBLOCK`）も `-EINVAL` で断る**（限界。契機は `ADR-0064`）。
pub const SYS_SOCKET: u64 = 41;
/// `connect` の番号（`ADR-0064`）。**名前で繋ぐ。** **待ち受けが無ければ `-ECONNREFUSED`、
/// 待ち行列が満杯なら `-EAGAIN`。**
pub const SYS_CONNECT: u64 = 42;
/// `accept` の番号（`ADR-0064`）。**待ち行列が空なら待つ**（[`crate::task::Wait::SocketAcceptable`]）。
/// **`addr` は NULL しか受けない**（相手の名前は返さない。限界）。
pub const SYS_ACCEPT: u64 = 43;
/// `bind` の番号（`ADR-0064`）。**名前はカーネルの表に置く**——**ファイルシステムに inode は
/// 作らない。** **抽象名（先頭 NUL）は `-EINVAL`。**
pub const SYS_BIND: u64 = 49;
/// `listen` の番号（`ADR-0064`）。**`backlog` は接続の上限で頭を切る**（Linux の `somaxconn` と同じ形）。
pub const SYS_LISTEN: u64 = 50;

/// `AF_UNIX`（Linux の値）。
pub const AF_UNIX: u64 = 1;
/// `SOCK_STREAM`（Linux の値）。
pub const SOCK_STREAM: u64 = 1;
/// `sockaddr_un` の大きさ（`sa_family_t` 2 + `sun_path` 108。Linux の配置）。
pub const SOCKADDR_UN_LEN: u64 = 110;

/// `ENOTSOCK`（ソケットでない fd への `bind` など）。
pub const ENOTSOCK: i64 = 88;
/// `EPROTONOSUPPORT`（`protocol` が 0 でない）。
pub const EPROTONOSUPPORT: i64 = 93;
/// `EAFNOSUPPORT`（`AF_UNIX` 以外）。
pub const EAFNOSUPPORT: i64 = 97;
/// `EADDRINUSE`（名前が取られている）。
pub const EADDRINUSE: i64 = 98;
/// `ENOBUFS`（listener の枠が無い）。
pub const ENOBUFS: i64 = 105;
/// `EISCONN`（繋がっている fd への `connect`）。
pub const EISCONN: i64 = 106;
/// `ENOTCONN`（繋がっていないソケットへの `read` / `write`）。
pub const ENOTCONN: i64 = 107;
/// `ECONNREFUSED`（その名前で待ち受けている者が居ない）。
pub const ECONNREFUSED: i64 = 111;

/// [`SYS_SPAWN`] が受け入れる像の最大の大きさ（S11-5）。
///
/// # 32 KiB の根拠は実測である
///
/// **いま像として置いてあるのは `hello` が 8496 バイト、`syscall-test` が
/// 8648 バイトである**（`kernel/build.rs` が `rustc` で建てたもの）。
/// **32 KiB はその 3.7 倍で、ユーザープログラムが 3 倍を超えて育つまで届かない。**
///
/// # スタックへ置かない
///
/// **`deferred-decisions.md` の「大きなスタック配列とガード幅」の解禁条件に
/// 当たる**——4 KiB を超える単一のローカル配列である。**S11-3 で 1 度目が発火し、
/// 実測でガードページを踏んだ。** ここが 2 度目で、**踏む前に避ける。**
///
/// **置き場所は `crate::userland` の `static` である**
/// （`ADR-0030` で採った「スタックへ載せない」と同じ解き方である）。
pub const MAX_EXECUTABLE_SIZE: usize = 32 * 1024;

/// [`SYS_SPAWN`] が受け取る `argv` の総バイト数の上限（NUL を含む。S11-7）。
///
/// # 本当の上限はページである
///
/// **初期スタックは 1 ページしか張っていない**（`crate::userland` の
/// `build_initial_stack`）。表と文字列はその中に収める。**その判定は既にあり、
/// 入らなければ `checked_sub` が `None` を返して `ArgumentsTooLong` になる。**
///
/// **ここはカーネル側の緩衝の大きさである。** ページより先に効くので、
/// **実際に返るのは `-E2BIG` のほうである。** 1024 を採るのは、
/// **いま渡している `argv` が 19 バイト**（`syscall-test` の `"syscall-test"` と
/// `"alpha"`、NUL 込み）で、**その 50 倍を超える余裕**だからである。
///
/// # スタックへ置く
///
/// **`spawn_from_ring3` のローカルである。** 1024 バイトは
/// `deferred-decisions.md` の「大きなスタック配列とガード幅」が言う 4096 バイトを
/// 越えない。**越えるなら `static` へ移す**（`MAX_EXECUTABLE_SIZE` と同じ形）。
pub const MAX_ARGV_BYTES: usize = 1024;

/// [`SYS_SPAWN`] が受け取る `envp` の総バイト数の上限（NUL を含む。f-2。`ADR-0053`）。
///
/// # `MAX_ARGV_BYTES` と同じ 1024 にしてある
///
/// **上限が別の理由で決まっているので、定数も別に持つ**——`argv` は語の数と
/// 長さ、`envp` は環境の本数（[`crate::userland::MAX_ENVP`] = 8）と 1 行の長さ
/// （[`crate::userland::ENV_LINE_MAX`] = 128）である。**8 × 128 = 1024 で、
/// 表が満杯でも収まる。**
///
/// # ページの判定は最後に在る
///
/// **初期スタックは 1 ページである**（`crate::userland` の `build_initial_stack`）。
/// **最悪で `argv` と `envp` の文字列が 2KiB、表が 144 バイトで、1 ページに収まる。**
/// **残りはプログラム自身のスタックなので、`user-stack` の `over_half` を見ること**
/// （`ADR-0041` の Decision 4）。
pub const MAX_ENVP_BYTES: usize = 1024;

/// 検証用 probe システムコールの番号（ZaytOS 独自。[`ZAYTOS_PRIVATE_BASE`]）。
pub const PROBE_NUMBER: u64 = ZAYTOS_PRIVATE_BASE;

/// **永久に実装しない番号**（S9-b-3-2a）。`-ENOSYS` の的である。
///
/// # なぜ「空いている番号」で済ませないか
///
/// **未実装の番号は、いつか実装される。** そのとき、`-ENOSYS` が返ることを
/// 確かめていた検査は静かに別のものを見はじめる（戻り値が変わるので落ちはするが、
/// **落ちた理由が「実装したから」だと分かる材料がどこにも無い**）。
///
/// **予約しておけば、実装しようとした人がこの doc を読む。** [`ZAYTOS_PRIVATE_BASE`]
/// の上に置くので、Linux の番号表とも衝突しない。
pub const SYS_NEVER_IMPLEMENTED: u64 = ZAYTOS_PRIVATE_BASE + 0xFF;

/// probe が返す既知の戻り値。ユーザーはこれを RAX で受け取り、ユーザースタックへ
/// store する。カーネルが畳み後に読み戻して一致を確かめることで、戻り値が RAX 経由で
/// Ring 3 へ渡ったことを実証する。`-errno` の範囲（`-1..-4095`）と紛れない値にする。
pub const PROBE_RETURN: u64 = 0x00C0_FFEE;

/// probe の呼び出しでユーザーが各引数レジスタ（RDI/RSI/RDX/R10/R8/R9）へ入れる
/// 既知値。**レジスタごとに区別できる値**にする（第 4 引数を R10 でなく RCX から
/// 読む破壊が、記録した第 4 引数の食い違いとして必ず現れるように）。
pub const PROBE_ARGS: [u64; 6] = [
    0x1111_1111,
    0x2222_2222,
    0x3333_3333,
    0x4444_4444,
    0x5555_5555,
    0x6666_6666,
];

/// probe の呼び出しでユーザーが RCX へ入れる番兵。RCX は引数ではない（クロバー扱い）。
/// `syscall-test-arg4-rcx` が第 4 引数を RCX から読むと、この値が第 4 引数として
/// 記録され、`PROBE_ARGS[3]` と決定的に食い違う。
pub const SENTINEL_RCX: u64 = 0xCCCC_CCCC;

/// 遠征の 1 本が持つ、システムコール側の状態（W1-a。W1-c-3 で記録も加えた）。
///
/// # 記録は W1-c-3 でここへ移した
///
/// **W1-a では正しさの 4 つだけを置き、`WRITE_*` / `LAST_*` / `INVOCATION_COUNT` などの
/// 記録は大域に残した**——**動かすと「緑のまま、主張している中身が変わる」形になるからである。**
/// **W1-c-3 で、[`Records`] の欄（`PROBE_*` を除く 10 欄）をここへ移した**
/// ——**2 本目が最初のシステムコールで 5 個、`write` で 3 個を触る**
/// （`docs/wayland-inventory.md` の「W1-c の 2 本目は、17 個のうち何個を触るか」）。
/// **W1-c-3 の時点ではスロットが必ず 0 だったので、中身は変わらなかった。** **W1-c-4 の
/// `concurrent-test` で、足した 1 本がスロット 1 の欄を使う。**
///
/// **`PROBE_*` は移していない。** **起動時の probe しか使わない。**
struct SyscallState {
    /// 今 Ring 3 が使っている窓の下端と上端（S9-b-3-2b）。
    ///
    /// # 据えるのは Ring 3 へ落ちる側である
    ///
    /// [`crate::ring3::enter`] が遠征の間だけ据え、戻るときに元へ戻す。**据えないまま
    /// ここへ来ることはない**——[`validate_user_range`] を呼ぶのは [`dispatch`] だけで、
    /// あちらは `syscall_entry` からしか来ず、`syscall_entry` は Ring 3 からしか来ない。
    ///
    /// # 既定値は空の窓である
    ///
    /// `(0, 0)` は**どんな長さ 1 以上の範囲も受理しない。** 据え忘れたときに黙って
    /// 通る形にしない。**安全側は「窓が無ければ何も通さない」である。**
    user_window_start: AtomicU64,
    user_window_end: AtomicU64,
    /// [`SYS_EXIT`] を受け取ったか（S9-b-3-1）。**呼び出し側は、遠征から戻った理由が
    /// 終了なのか畳みなのかをこれで区別する。**
    process_exited: AtomicBool,
    /// [`SYS_EXIT`] が受け取った終了状態（RDI）。[`SyscallState::process_exited`] が真のときだけ意味を持つ。
    process_exit_status: AtomicU64,
    /// `syscall_entry` が呼ばれた回数（会計用。W1-c-3 で大域から移した）。
    invocation_count: AtomicU64,
    /// 直近に受け取った番号（RAX）。往復検証で PROBE_NUMBER と突き合わせる。
    last_number: AtomicU64,
    /// 直近に受け取った 6 引数（RDI/RSI/RDX/R10/R8/R9）。PROBE_ARGS と突き合わせる。
    last_args: [AtomicU64; 6],
    /// `syscall_entry` が走ったときの RSP（RSP0 スタックのはず）。読み戻し検証に使う。
    handler_rsp: AtomicU64,
    /// 入場時点の [`crate::ring3`] の「今 Ring 3 にいる」の値（S8-b）。**Ring 3 から
    /// 来たのなら真のはず**で、往復検証が突き合わせる。
    in_ring3_at_entry: AtomicBool,
    /// [`SYS_WRITE`] が最後に受け取った fd。
    write_fd: AtomicU64,
    /// [`SYS_WRITE`] が最後に記録したバイト数。
    write_len: AtomicU64,
    /// [`SYS_WRITE`] が最後に記録したバイト列。
    write_buf: [AtomicU8; WRITE_BUF_LEN],
}

impl SyscallState {
    const fn new() -> Self {
        Self {
            user_window_start: AtomicU64::new(0),
            user_window_end: AtomicU64::new(0),
            process_exited: AtomicBool::new(false),
            process_exit_status: AtomicU64::new(0),
            invocation_count: AtomicU64::new(0),
            last_number: AtomicU64::new(0),
            last_args: [const { AtomicU64::new(0) }; 6],
            handler_rsp: AtomicU64::new(0),
            in_ring3_at_entry: AtomicBool::new(false),
            write_fd: AtomicU64::new(0),
            write_len: AtomicU64::new(0),
            write_buf: [const { AtomicU8::new(0) }; WRITE_BUF_LEN],
        }
    }
}

/// システムコール側の状態、スロットごと（W1-a）。
static SYSCALL_STATE: [SyscallState; crate::ring3::RING3_SLOTS] =
    [const { SyscallState::new() }; crate::ring3::RING3_SLOTS];

/// 今のタスクのシステムコール側の状態を引く（W1-a。W1-c-3 でタスクのスロットから引く形にした）。
///
/// **既定の起動では必ずスロット 0 である**（`crate::ring3::current_slot`。**W1-c-4 の
/// `concurrent-test` では足した 1 本がスロット 1 を引く**）。
#[inline(always)]
fn state() -> &'static SyscallState {
    &SYSCALL_STATE[crate::ring3::current_slot()]
}

/// [`PROBE_NUMBER`] を受け取ったか（S9-b-3-2a）。
static PROBE_INVOKED: AtomicBool = AtomicBool::new(false);
/// [`PROBE_NUMBER`] の呼び出しで届いた 6 引数（S9-b-3-2a）。
///
/// # なぜ `SyscallState::last_args` で足りないか
///
/// あちらは**直近の呼び出し**を持つ。**起動時の battery は 1 回しか発行しないので
/// 足りていた**が、ユーザープログラムは 4 回発行する（probe・`write`・未実装の
/// 番号・`exit`）。**最後の `exit` で上書きされ、probe の引数は残らない。**
///
/// **番号ごとに要るのではなく、「主張したい 1 回」が要る。** 主張は
/// 「6 引数が `ADR-0020` の規約どおりに届くこと」で、それを言えるのは probe の
/// 回だけである。
static PROBE_SEEN_ARGS: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

// **`INVOCATION_COUNT` / `LAST_NUMBER` / `LAST_ARGS` / `HANDLER_RSP` /
// `IN_RING3_AT_ENTRY` は W1-c-3 で [`SyscallState`] へ移した。**

/// 検証済みのユーザー範囲を表す証明トークン（M5-f-2-2、案T）。
///
/// **フィールドは private で、公開コンストラクタを持たない。** 構築できるのは同一
/// モジュール内の [`validate_user_range`] だけである。したがって [`copy_from_user`] が
/// `&UserSlice` を要求することで、**モジュール外の全呼び出し元に対しては「検証を経ないと
/// ユーザーメモリを読めない」ことが型で保証される。**
///
/// # 型で保証される範囲と、規律で守る範囲
///
/// この保証はモジュール境界に依存する。同一 `syscall.rs` モジュール内からは private
/// フィールドに触れるため `UserSlice { .. }` を直接構築できてしまう。したがって:
/// - モジュール外: 検証を経ないと `UserSlice` が作れない（型で保証）。
/// - モジュール内: 直接構築は `copy-skip-validate` 破壊 feature 専用であり、通常コードでは
///   行わない（この規律は型ではなくレビューで守る）。`copy-skip-validate` はまさにこの境界を
///   突く破壊である。
///
/// # 有効期間
///
/// `UserSlice` は**同一 syscall 内・同一アドレス空間でのみ有効**。跨いで保持しない
/// （static 等に置かない）。higher-half B 後のプロセス別アドレス空間では、トークンは
/// 「その CR3 の下でのみ有効」になるため、CR3 を跨いで使わない制約を f-3 で型（世代/CR3 を
/// 持たせる等）または doc で担保する（再確認の申し送り。verification-coverage 参照）。
pub struct UserSlice {
    buf: u64,
    len: u64,
}

impl UserSlice {
    /// 範囲の先頭アドレス（ユーザー VA）。
    pub fn buf(&self) -> u64 {
        self.buf
    }
    /// 範囲の長さ（バイト）。
    pub fn len(&self) -> u64 {
        self.len
    }
    /// 範囲が空（len==0）か。len==0 は常に受理されるので有効なトークンとして存在しうる。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// 指定した [buf, buf+len) が Ring 3 からアクセス可能かを、**カーネルが読み書きに
/// 踏み込む前に**判定し、可なら証明トークン [`UserSlice`] を返す（M5-f-2-1 / M5-f-2-2）。
///
/// **len==0 は常に受理する。** 0 バイトのアクセスは buf を問わず安全であり、この
/// 契約はこの検証器を共有する全 syscall が継承する（呼び出し側で短絡しない）。
/// それ以外は次を満たすとき `Some`:
///   (a) 長さの加算にオーバーフローが無い（`checked_add`）。
///   (b) 範囲が**今 Ring 3 が使っている窓**に収まる（[`user_window`]）。
///   (c) 範囲を跨ぐ全 4KiB ページが present && 全階層 U=1
///       （[`crate::paging::verify::walk_user_accessible`]）。
///
/// (a)(b)(c-present) は多層防御として (c-U=1) に冗長で、単独では隔離した破壊確認が
/// できない（詳細は verification-coverage）。それらは default battery の first-line
/// 拒否者として実運用・実証される。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が walk_user_accessible の契約を満たすこと。
pub unsafe fn validate_user_range(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    buf: u64,
    len: u64,
) -> Option<UserSlice> {
    // 破壊 (M5-f-2-1, skip-all): 検証器を常に受理にする。検証器の全体機能停止を
    // battery が検出して halt する（多層防御の最後の砦の確認）。
    #[cfg(feature = "syscall-test-validate-skip-all")]
    {
        let _ = (pml4_phys, direct_map);
        return Some(UserSlice { buf, len });
    }
    #[cfg(not(feature = "syscall-test-validate-skip-all"))]
    {
        // (a) len==0 は常に受理（契約）。
        if len == 0 {
            return Some(UserSlice { buf, len });
        }
        // (a) 加算オーバーフロー無し。end は排他的上端（buf+len）。
        let end = buf.checked_add(len)?;
        // (b) 範囲が**今の窓**に収まっていること（S9-b-3-2b で 1 つに畳んだ）。
        // **またぐ範囲が受理されないのは、窓が 1 つしかないからである**（S9-b-1 から
        // S9-b-3-2a までは窓が 2 つあり、「またいだものは受理しない」と書いて
        // いた。いまは書く条件ではなく構造の帰結である）。
        let (window_start, window_end) = user_window();
        if buf < window_start || end > window_end {
            return None;
        }
        // (c) 範囲を跨ぐ全 4KiB ページを walk。境界非整列でも先頭・末尾を覆う。
        let first_page = buf & !0xFFF;
        let full_last_page = (end - 1) & !0xFFF;
        // 破壊 (M5-f-2-1, skip-laststep): 走査上端を先頭ページに潰し、先頭ページだけを
        // 検証する。無効4（跨ぎ）の末尾無効を取り逃し、battery が検出して halt する。
        let last_page = if cfg!(feature = "syscall-test-validate-skip-laststep") {
            first_page
        } else {
            full_last_page
        };
        let mut page = first_page;
        while page <= last_page {
            let virt = common::addr::VirtAddr::new(page)?;
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。読み取りのみ。
            if unsafe { crate::paging::verify::walk_user_accessible(pml4_phys, direct_map, virt) }
                .is_err()
            {
                return None;
            }
            page += 0x1000;
        }
        Some(UserSlice { buf, len })
    }
}

/// [`validate_user_range`] の bool 版（M5-f-2-1 の SYS_CHECK_PTR 用）。可なら true。
///
/// # Safety
///
/// [`validate_user_range`] と同じ契約。
pub unsafe fn user_range_accessible(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    buf: u64,
    len: u64,
) -> bool {
    // SAFETY: 呼び出し元契約による。
    unsafe { validate_user_range(pml4_phys, direct_map, buf, len) }.is_some()
}

/// 検証済みの [`UserSlice`] から `dst` へ、範囲内バイトだけを読む bounded read
/// （M5-f-2-2）。**`UserSlice` を要求するので、検証を経ないと呼べない。**
///
/// ユーザーバイトは稼働中アドレス空間の VA を直接参照する（present・U=1 でマップ済み、
/// SMAP 未有効なのでカーネルが直接読める。テーブル walk は検証で使うが、データ読みに
/// direct_map は要らない）。読んだバイト数を返す。
///
/// **TOCTOU について。** 検証と読みが実質アトミックなのは、syscall_entry が割り込みゲート
/// （IF=0）で入りプリエンプトが来ないこと、**BKL を保持したままユーザーメモリへ触ること**
/// （`spawn` は途中で BKL を解くが、パスと `argv` を写す区間は解く前に置いてある）、
/// ユーザーページをアンマップする経路が syscall 中に走らないこと、の構造条件に依存する。将来 IF を立てる
/// syscall（長時間ブロッキング等）を入れると、この前提が崩れ TOCTOU（検証後・読み前に
/// アンマップ/再マップ）が現実化するため再検証が要る（verification-coverage の申し送り）。
///
/// # Safety
///
/// `slice` が現在のアドレス空間に対して有効に検証されていること（[`validate_user_range`]
/// が返したものであること）。`dst` が読むバイト数を収められること。
pub unsafe fn copy_from_user(dst: &mut [u8], slice: &UserSlice) -> usize {
    // 破壊 (M5-f-2-2, copy-overrun): len を 1 バイト超えて読む。末尾の有効ページ内に置いた
    // 余分な既知バイトが総和へ混ざり、内容往復のチェックサムが決定的に食い違う（#PF は副次）。
    let n = slice.len as usize
        + if cfg!(feature = "syscall-test-copy-overrun") {
            1
        } else {
            0
        };
    // dst に収まる分だけ読む（copy-overrun で n が dst を超えても範囲外にしない）。
    let count = n.min(dst.len());
    for (i, slot) in dst.iter_mut().enumerate().take(count) {
        // SAFETY: slice は検証済みで、buf+i は present・U=1 のユーザーページ。SMAP 未有効。
        *slot = unsafe { core::ptr::read_volatile((slice.buf as *const u8).add(i)) };
    }
    count
}

/// 検証済みの範囲の `at` バイト目から、カーネルのバイト列を書く（S10-b）。
///
/// 書いた長さを返す。**トークンの長さを越えて書かない**ので、
/// `at + src.len()` が [`UserSlice::len`] を越える場合は、越えない分だけ書く。
///
/// # なぜ `at` を取るか
///
/// **`read` は 1 回の呼び出しで複数のブロックから写す。** ブロックごとに
/// トークンを作り直すと、**作り直すたびに検証を通さなければ意味が無い**
/// （通さずに作れば `UserSlice` の保証が崩れる）。**1 つのトークンの中を
/// 進む形にすれば、検証は 1 回で足りる。**
///
/// # Safety
///
/// `slice` が [`validate_user_range`] を通った検証済みトークンであること。
/// **その範囲は present かつ U=1 で、書き込み可能であること**——`read` が
/// 書く先はユーザーのバッファで、[`crate::paging`] が `writable: true` で
/// 張ったページである。
pub unsafe fn copy_to_user(slice: &UserSlice, at: u64, src: &[u8]) -> usize {
    let Some(room) = slice.len().checked_sub(at) else {
        return 0;
    };
    let count = src.len().min(room as usize);
    for (i, byte) in src.iter().enumerate().take(count) {
        // SAFETY: slice は検証済みで、buf+at+i は present・U=1 のユーザーページ。
        // count が room を越えないので、トークンの範囲を出ない。SMAP 未有効。
        unsafe {
            core::ptr::write_volatile(
                (slice.buf() as *mut u8).add((at + i as u64) as usize),
                *byte,
            )
        };
    }
    count
}

/// 番号を実装へ振り分ける（M5-f-1-2 / M5-f-2-1）。
///
/// probe は既知の戻り値 [`PROBE_RETURN`] を返す。SYS_CHECK_PTR はユーザーポインタの
/// 範囲を検証し、可なら 0、不可なら -EFAULT を返す（**バイトは読まない**）。それ以外は
/// 未実装で `-ENOSYS`。
///
/// **[`SYS_EXIT`] だけは記録して終わる**（S9-b-3-1）。**戻り値では「戻らない」を
/// 表せない**ので、Ring 3 へ返さない分岐は [`syscall_entry`] が持つ。
///`pml4_phys` / `direct_map` は稼働中テーブルのもの（syscall_entry
/// が用意する）で、ポインタ検証にのみ使う。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`user_range_accessible`] の契約を満たすこと。
unsafe fn dispatch(
    number: u64,
    args: &[u64; 6],
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    match number {
        PROBE_NUMBER => {
            // **この回の引数を残す（S9-b-3-2a）。** `SyscallState::last_args` は後続の呼び出しで
            // 上書きされるので、**主張したい 1 回**をここで押さえる。
            for (slot, value) in PROBE_SEEN_ARGS.iter().zip(args.iter()) {
                slot.store(*value, Ordering::SeqCst);
            }
            PROBE_INVOKED.store(true, Ordering::SeqCst);
            PROBE_RETURN
        }
        SYS_CHECK_PTR => {
            let buf = args[0];
            let len = args[1];
            // **踏み込む前に**範囲を検証する。可なら 0、不可なら -EFAULT。この段は
            // バイトを読まない（copy は M5-f-2-2）。
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            if unsafe { user_range_accessible(pml4_phys, direct_map, buf, len) } {
                0
            } else {
                (-EFAULT) as u64
            }
        }
        SYS_WRITE => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_write(args[0], args[1], args[2], pml4_phys, direct_map, bkl) }
        }
        SYS_CHECKSUM => {
            let buf = args[0];
            let len = args[1];
            // 長さがカーネルバッファを超える。**アドレスの問題ではないので
            // -EINVAL であって -EFAULT ではない**（S9-a で分けた）。
            //
            // 破壊 (S9-a, einval-as-efault): 分ける前の -EFAULT へ戻す。長さの誤りと
            // アドレスの誤りが同じ errno へ潰れ、over-long の判定行が捕まえる。
            if len as usize > CHECKSUM_BUF_LEN {
                #[cfg(not(feature = "syscall-test-einval-as-efault"))]
                let errno = EINVAL;
                #[cfg(feature = "syscall-test-einval-as-efault")]
                let errno = EFAULT;
                return (-errno) as u64;
            }
            // **踏み込む前に検証する。** 検証済みトークン UserSlice を得てから読む。
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            #[cfg(not(feature = "syscall-test-copy-skip-validate"))]
            let slice = unsafe { validate_user_range(pml4_phys, direct_map, buf, len) };
            // 破壊 (M5-f-2-2, copy-skip-validate): 検証を経ずに UserSlice をモジュール内で
            // 直接構築する（型保証の境界を突く。モジュール内なので private フィールドに触れる）。
            // カーネルポインタを渡すと、-EFAULT のはずが総和が返り verify が検出して halt する。
            #[cfg(feature = "syscall-test-copy-skip-validate")]
            let slice = Some(UserSlice { buf, len });
            let Some(slice) = slice else {
                return (-EFAULT) as u64;
            };
            let mut kbuf = [0u8; CHECKSUM_BUF_LEN];
            // SAFETY: slice は検証済み（copy-skip-validate を除く）。dst は len+破壊1 を収める。
            let read = unsafe { copy_from_user(&mut kbuf, &slice) };
            kbuf[..read].iter().map(|b| *b as u64).sum()
        }
        SYS_SPAWN => {
            // **ここへは来ない。** [`SYS_SPAWN`] は [`syscall_entry`] が持つ——
            // **BKL を解いてから入る必要があり、ガードはあちらのローカルである**
            // （[`SYS_EXIT`] が「戻らない」を表せないのであちらに在るのと同じ形）。
            //
            // **受け皿として置く。** 落とすと `-ENOSYS` へ落ち、
            // **「実装していない」と「入口を間違えた」が同じ返り値になる。**
            (-EAGAIN) as u64
        }
        SYS_READ => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_read(args[0], args[1], args[2], pml4_phys, direct_map, bkl) }
        }
        SYS_GETDENTS64 => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_getdents64(args[0], args[1], args[2], pml4_phys, direct_map) }
        }
        SYS_STAT => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_stat(args[0], args[1], pml4_phys, direct_map) }
        }
        SYS_OPEN => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_open(args[0], args[1], pml4_phys, direct_map) }
        }
        SYS_IOCTL => {
            // **画面の fd は別の口（`ADR-0066` の Y-c）。** **`present` は BKL を解いて写す**ので、
            // ガードを渡せる口へ分ける。**`#[inline(never)]` で、写しは `dispatch` の枠に乗らない。**
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            match unsafe {
                screen_ioctl_from_ring3(args[0], args[1], args[2], pml4_phys, direct_map, bkl)
            } {
                Some(result) => result,
                // SAFETY: 同上。
                None => unsafe { sys_ioctl(args[0], args[1], args[2], pml4_phys, direct_map) },
            }
        }
        SYS_BRK => {
            // SAFETY: 呼び出し元契約により direct_map は有効で、
            // 遠征の中なので CR3 はこのプロセスのものである。
            unsafe { sys_brk(args[0], direct_map) }
        }
        SYS_LSEEK => sys_lseek(args[0], args[1], args[2]),
        SYS_CLOCK_GETTIME => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_clock_gettime(args[0], args[1], pml4_phys, direct_map) }
        }
        SYS_NANOSLEEP => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_nanosleep(args[0], pml4_phys, direct_map, bkl) }
        }
        SYS_MKDIR => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_directory(args[0], DirectoryOp::Create, pml4_phys, direct_map) }
        }
        SYS_RMDIR => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_directory(args[0], DirectoryOp::Remove, pml4_phys, direct_map) }
        }
        SYS_UNLINK => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            unsafe { sys_unlink(args[0], pml4_phys, direct_map) }
        }
        // **unix ドメインのストリームソケット（`ADR-0064`）。** **5 つとも本体は
        // `#[inline(never)]` の関数である**——**この `match` の枠に局所を乗せない。**
        // **`spawn` の経路には載っていないので、`syscall-test` の高水位は動かない見込みである。**
        SYS_OPEN_INPUT => open_input_from_ring3(),
        SYS_OPEN_SCREEN => open_screen_from_ring3(),
        // **多重待ち（`ADR-0066` の Y-b）。** **`#[inline(never)]` で、写しは `dispatch` の
        // 枠に乗らない。**
        // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
        SYS_POLL => unsafe {
            poll_from_ring3(args[0], args[1], args[2], pml4_phys, direct_map, bkl)
        },
        SYS_SOCKET => socket_from_ring3(args[0], args[1], args[2]),
        // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
        SYS_BIND => unsafe { bind_from_ring3(args[0], args[1], args[2], pml4_phys, direct_map) },
        SYS_LISTEN => listen_from_ring3(args[0], args[1]),
        SYS_ACCEPT => accept_from_ring3(args[0], args[1], bkl),
        // SAFETY: 同上。
        SYS_CONNECT => unsafe {
            connect_from_ring3(args[0], args[1], args[2], pml4_phys, direct_map)
        },
        // **共有メモリと fd の受け渡し（`ADR-0065`）。** **5 つとも `#[inline(never)]` で、
        // `spawn` の経路には載っていない**——**`mmap` は `brk` と同じ `map_4kib` を使う。**
        SYS_MEMFD_CREATE => memfd_create_from_ring3(),
        SYS_FTRUNCATE => ftruncate_from_ring3(args[0], args[1]),
        // SAFETY: 呼び出し元契約により direct_map は有効で、遠征の中なので CR3 はこのプロセスのもの。
        SYS_MMAP => unsafe { mmap_from_ring3(args[1], args[2], args[4], args[5], direct_map) },
        // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
        SYS_SENDMSG => unsafe { sendmsg_from_ring3(args[0], args[1], pml4_phys, direct_map, bkl) },
        // SAFETY: 同上。
        SYS_RECVMSG => unsafe { recvmsg_from_ring3(args[0], args[1], pml4_phys, direct_map, bkl) },
        SYS_CLOSE => {
            // **書きで開いた口を閉じたら、像を装置へ書き戻す（P-c-1）。**
            //
            // **ここを選んだ理由は、`zi` の `:w` が「開く・書く・閉じる」で
            // 1 回の保存になるからである**——**書きのたびに書き戻すと、
            // 1 回の保存で何度も 2MiB を書くことになる。**
            let closed = crate::vfs::with_current_files(|files| {
                files.remove(args[0] as usize).map(|file| {
                    // **パイプとソケットの端なら返す（`ADR-0063` の (b3)、`ADR-0064`）。**
                    // **`remove` が返した `File` は `Copy` で `Drop` を持たないので、ここで
                    // 明示に返す。**
                    file.release_end();
                    file.is_writable_file()
                })
            });
            match closed {
                Ok(true) => {
                    // SAFETY: BKL を保持して入っている（[`syscall_entry`] の契約）。
                    match unsafe { flush_root_image(bkl) } {
                        Ok(()) => 0,
                        Err(errno) => (-errno) as u64,
                    }
                }
                Ok(false) => 0,
                Err(e) => (-errno_for_file_table(e)) as u64,
            }
        }
        SYS_EXIT => {
            // **記録するだけである。** Ring 3 へ返らない分岐は `syscall_entry` が
            // 持つ（[`SYS_EXIT`] の doc）。**戻り値は読まれない。**
            //
            // 破壊 (S9-b-3-1, user-exit-wrong-status): 終了状態を第 1 引数（RDI）
            // ではなく第 2 引数（RSI）から読む。**`arg4-rcx` と同じ、引数レジスタを
            // 1 本取り違える形である。** `hello` は `exit` の直前に RSI を
            // 触らない（`write` へ渡したバイト列の番地が残っている）ので、
            // **0 でない既知の値が終了状態として記録される。**
            #[cfg(not(feature = "user-exit-wrong-status"))]
            let status = args[0];
            #[cfg(feature = "user-exit-wrong-status")]
            let status = args[1];
            state().process_exit_status.store(status, Ordering::SeqCst);
            state().process_exited.store(true, Ordering::SeqCst);
            // **溜まっている描画を送る（ADR-0047）。**
            //
            // **待たずに終わるプログラムを取りこぼさない**——`cat` と `ls` は
            // 書いて、読まずに終わる。**入力を待つ時点が来ない。**
            //
            // **この経路には判定が置けない。** **次に読む者が必ず居るので、
            // 掃かなくても1つ後の `read` で送られる**（`zash` が待つ）。
            // **受け皿として置く**——**「誰も読まないまま終わる」形が来たら、
            // ここだけが残る。** **観測できないので、破壊も立てない**
            // （`docs/verification-coverage.md`）。
            if crate::console::foreground_installed() {
                drop(bkl.take());
                crate::console::flush_foreground();
                *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
            }
            0
        }
        // 失敗は -errno（-1..-4095）。
        _ => (-ENOSYS) as u64,
    }
}

/// [`SYS_SPAWN`] の本体（S11-5）。**BKL を解いてから子を走らせる。**
///
/// # なぜ [`dispatch`] ではなくここに在るのか
///
/// **BKL を解く必要があり、ガードは [`syscall_entry`] のローカルだからである。**
/// [`SYS_EXIT`] が「戻らない」を戻り値で表せずにあちらへ在るのと、置き場所の
/// 理由は同じである（**あちらは制御が戻らないから、こちらはロックを手放すから**）。
///
/// # BKL を解く理由（`ADR-0023` §1）
///
/// **`ADR-0023` §1 の定義は「カーネル入口で取り、ユーザー空間（Ring 3）へ戻るときに
/// 離す」である。** 現在の実装は S4 の Addendum が置いた等価物——
/// 「入口で取り、**その入口から戻るとき**に離す」——で、**Ring 3 が定常的に無い間は
/// 2 つが一致していた。**
///
/// **`spawn` は初めて 2 つが食い違う場所である。** 入口からはまだ戻らないが、
/// Ring 3 へは降りる。**Addendum 自身が「Ring 3 が定常状態になった段（S9 以降）で
/// §1 の字面が改めて成立する」と書いており、ここがその場所である。**
///
/// **保持したまま降りると 2 つの形で壊れる。実測ではなく構造で言える。**
///
/// - **子のシステムコール**が [`syscall_entry`] へ入り、**同じコアが BKL を
///   取り直す。** `bkl::acquire` は再帰取得を検出して停止する
/// - **Ring 3 は `RFLAGS = 0x202`（IF=1）で走る**ので、タイマが動いている段では
///   `irq_entry` が同じことをする。**`ADR-0023` の Addendum §4 の不変条件
///   「BKL を保持する区間 = IF=0 の区間」に、保持したままの降下は直接反する**
///
/// # 解く区間はどこか
///
/// **パスを写し終えてから解く。** ユーザーメモリへ触るのは [`copy_user_path`] だけで、
/// **あれは「検証と読みが実質アトミック」であることに依っている**（[`copy_from_user`] の
/// TOCTOU の注記）。**その区間は BKL の内側に残す。**
///
/// **写像も畳みも BKL の外で走る。** これは新しい形ではない——**起動時の
/// `load_user_program` は最初から BKL を保持せずに写像している**（`kernel_main` は
/// ガードを持たない）。**`crate::userland::load_user_program` はそのまま呼べる**
/// ——中で畳みのために自分で BKL を取るので、**保持したまま入ると、そこで
/// 再帰取得になる。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
/// `bkl` が、いま保持している BKL のガードであること。
/// 像を装置へ書き戻す（P-c-1）。**シェルの文脈から呼ぶ唯一の口である。**
///
/// # BKL の踊り
///
/// **`ADR-0036` が「BKL を保持したまま眠らない・待たない」と決めている。**
/// **発行だけを BKL 下で行い、解いてから眠る。** **起きたら取り直す。**
/// **形は `spawn_from_ring3` と同じである**（あちらは子が走る間、こちらは
/// 装置が書く間）。
///
/// # 装置は占有で守る
///
/// **BKL を解いている間、他のコアが同じ装置へ入りうる。** **占有の旗が
/// 止める**（`kernel::virtio::claim`）。**取れなければ `-EBUSY` を返す**
/// ——**止めるより断るほうが観測できる。**
///
/// # 使う者がまだ 1 つである
///
/// **いま断られる形は起きない**——**Ring 3 を走らせているのは前景の 1 本だけで、
/// AP は利用者を走らせていない**（実測。起動ログの `ap_sched_passes=0`）。
/// **それでも旗を置くのは、解いている間の守りが BKL では作れないからである。**
///
/// # Safety
///
/// `bkl` が、いま保持している BKL のガードであること。
/// 破壊 `flush-waits-without-device` の締切（TSC サイクル）。**約 0.3 秒**（実測で TSC は約 3.5GHz）。
///
/// **破壊にだけ在る。** **既定のビルドでは、装置が無ければ待たずに戻る。**
#[cfg_attr(not(feature = "flush-waits-without-device"), allow(dead_code))]
const FLUSH_WITHOUT_DEVICE_DEADLINE_CYCLES: u64 = 1_000_000_000;

unsafe fn flush_root_image(bkl: &mut Option<crate::bkl::BklGuard>) -> Result<(), i64> {
    // **据えられていなければ書き戻さない（P-c-1）。**
    //
    // **起動シーケンスの中でもユーザープログラムが走り、書きで開いた口を閉じる。**
    // **あれらは据える前に走る**——**断ると起動が止まる**（実測。2026-08-28）。
    // **起動シーケンスが最後に自分で書き戻すので、失われるものが無い。**
    if !crate::virtio::installed() {
        // 破壊 (HW-d, flush-waits-without-device): **装置が無いのに完了を待つ**（待ちを残した形）。
        // **RAM ディスクで動く VirtualBox では、これが「黙って固まる」形になる**——**完了割り込みは
        // 永遠に来ない。** **締切を置いて止まる形にしてある**（黙る形を、行にして見えるようにする）。
        //
        // **声は panic で出す**——**`syscall.rs` にシリアルの口は無い**（開けると直接シリアルの
        // 許可リストに項目が増える）。**パニックの方針は Halt and Dump である**（`ADR-0004`）。
        if cfg!(feature = "flush-waits-without-device") {
            let started = common::cpu::read_timestamp_counter();
            while common::cpu::read_timestamp_counter().wrapping_sub(started)
                < FLUSH_WITHOUT_DEVICE_DEADLINE_CYCLES
            {
                core::hint::spin_loop();
            }
            panic!(
                "fs-image-flush: waited for a completion that cannot come (there is no \
                 virtio-blk device)"
            );
        }
        return Ok(());
    }
    let Some(mut claim) = crate::virtio::claim() else {
        return Err(EBUSY);
    };
    let started = common::cpu::read_timestamp_counter();
    // **発行は BKL の下で行う。** リングを触るので、同じコアの再入も止める。
    // SAFETY: 呼び出し元契約により BKL を保持している。
    let Some((expected, before, bytes)) = (unsafe { claim.issue_image_write() }) else {
        return Err(EIO);
    };
    // **ここで解く。** 取り直すのは待ち終えてからである。
    drop(bkl.take());
    // SAFETY: BKL は解いてある。`expected` は直前の発行が返した値である。
    let outcome = unsafe { claim.wait_for_image_write(expected, before) };
    *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
    let cycles = common::cpu::read_timestamp_counter().wrapping_sub(started);
    crate::virtio::note_flush(bytes, cycles);
    match outcome {
        Ok(()) => Ok(()),
        Err(_) => Err(EIO),
    }
}

/// 起こしっぱなしのスロットから端末へ書いた回数（`ADR-0063` の (b3) の計器）。
static TERMINAL_WRITES_FROM_DETACHED: AtomicU64 = AtomicU64::new(0);

/// [`TERMINAL_WRITES_FROM_DETACHED`] の値。
pub fn terminal_writes_from_detached() -> u64 {
    TERMINAL_WRITES_FROM_DETACHED.load(Ordering::Relaxed)
}

/// [`SYS_SPAWN_DETACHED`] が子の入場を待ったティック数の最大（計器）。**桁で小さいことを
/// 示すために持つ**（`Wait` を足さない根拠。[`SYS_SPAWN_DETACHED`] の doc）。
static DETACHED_ENTRY_WAIT_TICKS_MAX: AtomicU64 = AtomicU64::new(0);

/// [`SYS_SPAWN_DETACHED`] を通った回数（計器）。
static DETACHED_STARTS: AtomicU64 = AtomicU64::new(0);

/// [`DETACHED_ENTRY_WAIT_TICKS_MAX`] の値。
pub fn detached_entry_wait_ticks_max() -> u64 {
    DETACHED_ENTRY_WAIT_TICKS_MAX.load(Ordering::Relaxed)
}

/// [`DETACHED_STARTS`] の値。
pub fn detached_starts() -> u64 {
    DETACHED_STARTS.load(Ordering::Relaxed)
}

/// パイプの読み端から読む（`ADR-0063` の (b3)）。**空なら待つ。**
///
/// # 窓は構造で閉じている
///
/// **`int 0x80` は割り込みゲートなので IF=0 である**——**「空だと見てから `Waiting` にする」
/// までに書き手の起こしは入らない**（`sys_read` の端末の待ちと同じ）。**BKL は解いてから譲る**
/// （`ADR-0036`）。
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
unsafe fn read_from_pipe(
    pipe: u8,
    buf: u64,
    count: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    if count == 0 {
        return 0;
    }
    let want = count.min(crate::pipe::PIPE_RING as u64);
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, buf, want) }) else {
        return (-EFAULT) as u64;
    };
    let mut kbuf = [0u8; crate::pipe::PIPE_RING];
    loop {
        match crate::pipe::read_into(pipe, &mut kbuf[..want as usize]) {
            crate::pipe::ReadOutcome::Bytes(got) => {
                // SAFETY: `slice` は検証済みで、`got` はその長さを越えない。
                let written = unsafe { copy_to_user(&slice, 0, &kbuf[..got]) };
                return written as u64;
            }
            crate::pipe::ReadOutcome::Eof => return 0,
            crate::pipe::ReadOutcome::Empty => {
                crate::pipe::note_reader_wait();
                crate::task::set_current_waiting(crate::task::Wait::PipeReadable { pipe });
                drop(bkl.take());
                crate::task::yield_now();
                *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
            }
        }
    }
}

/// パイプの書き端へ書く（`ADR-0063` の (b3)）。**満杯なら待つ。読み手が居なければ `-EPIPE`。**
///
/// **部分書きである**——**入った数を返す。** **`userlib::write_all` が残りを回す。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
unsafe fn write_to_pipe(
    pipe: u8,
    buf: u64,
    count: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    if count == 0 {
        return 0;
    }
    let want = count.min(crate::pipe::PIPE_RING as u64);
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, buf, want) }) else {
        return (-EFAULT) as u64;
    };
    let mut kbuf = [0u8; crate::pipe::PIPE_RING];
    // SAFETY: `slice` は検証済みで、`want` はその長さである。
    let read = unsafe { copy_from_user(&mut kbuf[..want as usize], &slice) };
    if read == 0 {
        return (-EFAULT) as u64;
    }
    loop {
        match crate::pipe::write_from(pipe, &kbuf[..read]) {
            crate::pipe::WriteOutcome::Bytes(put) => return put as u64,
            crate::pipe::WriteOutcome::NoReader => return (-EPIPE) as u64,
            crate::pipe::WriteOutcome::Full => {
                crate::pipe::note_writer_wait();
                crate::task::set_current_waiting(crate::task::Wait::PipeWritable { pipe });
                drop(bkl.take());
                crate::task::yield_now();
                *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
            }
        }
    }
}

/// fd がソケットならその状態。**ソケットでなければ `Err(-ENOTSOCK)`、無い fd なら `Err(-EBADF)`。**
fn socket_state_of(fd: u64) -> Result<crate::vfs::SocketState, u64> {
    let found = crate::vfs::with_current_files(|files| {
        files.get(fd as usize).map(|file| file.socket_state())
    });
    match found {
        Ok(Some(state)) => Ok(state),
        Ok(None) => Err((-ENOTSOCK) as u64),
        Err(error) => Err((-errno_for_file_table(error)) as u64),
    }
}

/// `sockaddr_un` を読み、名前を `name` へ写す。**長さを返す。失敗は `-errno`。**
///
/// **`sun_path` の先頭から最初の NUL まで、または `addrlen - 2` までが名前である**（Linux の形）。
/// **空と抽象名（先頭 NUL）は `-EINVAL`、`NAME_MAX` を超えれば `-ENAMETOOLONG`。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
unsafe fn read_socket_name(
    addr: u64,
    addrlen: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    name: &mut [u8; crate::socket::NAME_MAX],
) -> Result<usize, i64> {
    if !(2..=SOCKADDR_UN_LEN).contains(&addrlen) {
        return Err(EINVAL);
    }
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, addr, addrlen) }) else {
        return Err(EFAULT);
    };
    let mut raw = [0u8; SOCKADDR_UN_LEN as usize];
    // SAFETY: `slice` は検証済みで、`addrlen` はその長さである。
    let read = unsafe { copy_from_user(&mut raw[..addrlen as usize], &slice) };
    if read != addrlen as usize {
        return Err(EFAULT);
    }
    if u16::from_le_bytes([raw[0], raw[1]]) != AF_UNIX as u16 {
        return Err(EINVAL);
    }
    let path = &raw[2..addrlen as usize];
    let len = path
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path.len());
    if len == 0 {
        return Err(EINVAL);
    }
    if len > crate::socket::NAME_MAX {
        return Err(ENAMETOOLONG);
    }
    name[..len].copy_from_slice(&path[..len]);
    Ok(len)
}

/// [`SYS_SOCKET`] の本体。**`AF_UNIX` の `SOCK_STREAM` だけを受け、繋がっていない
/// ソケットを最小の空き fd に置く。**
#[inline(never)]
fn socket_from_ring3(domain: u64, kind: u64, protocol: u64) -> u64 {
    if domain != AF_UNIX {
        return (-EAFNOSUPPORT) as u64;
    }
    if kind != SOCK_STREAM {
        return (-EINVAL) as u64;
    }
    if protocol != 0 {
        return (-EPROTONOSUPPORT) as u64;
    }
    let inserted = crate::vfs::with_current_files(|files| {
        files.insert(crate::vfs::File::Socket {
            state: crate::vfs::SocketState::Unbound,
        })
    });
    match inserted {
        Ok(fd) => fd as u64,
        Err(error) => (-errno_for_file_table(error)) as u64,
    }
}

/// 1 回の入力読みで返す最大バイト数（`ADR-0066` の Y-a）。**イベントの整数倍**
/// （4 つ。[`crate::input::INPUT_EVENT_LEN`] × 4）。
const INPUT_READ_MAX: usize = 96;

/// [`SYS_OPEN_INPUT`] の本体（`ADR-0066` の Y-a）。**前景の持ち主にだけ入力の生イベントの
/// fd を渡す。**
///
/// # 前景の関所は開く時点の 1 箇所
///
/// **呼んだ者が前景の系統でなければ `-EBADF`**（`crate::input::caller_is_foreground`。**Y-a では
/// 大域の `foreground_is_claimed` を見ていた**——Y-c で直した）。**前景は
/// プログラムの走行の間ずっと持たれる**ので、fd が前景より長生きしない。**`SCM_RIGHTS` は
/// shm の fd だけを運ぶので、この fd は相手の表へ写らない**（`ADR-0066` の「前景の関所」）。
#[inline(never)]
fn open_input_from_ring3() -> u64 {
    // **呼んだ者が前景の系統かを見る（Y-c で直した）。** **Y-a では大域の印を見ていたので、
    // 起こしっぱなしの 1 本でも開けた**（`crate::input::caller_is_foreground` の doc）。
    if !crate::input::caller_is_foreground() {
        return (-EBADF) as u64;
    }
    let inserted = crate::vfs::with_current_files(|files| files.insert(crate::vfs::File::Input));
    match inserted {
        Ok(fd) => fd as u64,
        Err(error) => (-errno_for_file_table(error)) as u64,
    }
}

/// 入力の生イベントを読む（`ADR-0066` の Y-a）。**`read` が `File::Input` に当たったときの経路。**
///
/// **端末の `read(0)` と同じ踊り**——**溜まっていなければ `Wait::Keyboard` で待つ。** **待つ条件も
/// 端末と同じ**（前景が据えられ、台本が駆動していないとき）。**違うのは、復号済みバイトではなく
/// `struct input_event` を返すことだけである。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
#[inline(never)]
unsafe fn read_input_events(
    buf: u64,
    count: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    if count == 0 {
        return 0;
    }
    // **イベント 1 つに満たない要求は断る**（半端なイベントは返せない）。
    if count < crate::input::INPUT_EVENT_LEN as u64 {
        return (-EINVAL) as u64;
    }
    let want = count.min(INPUT_READ_MAX as u64);
    // **イベントの整数倍に切り下げる。**
    let cap = (want as usize / crate::input::INPUT_EVENT_LEN) * crate::input::INPUT_EVENT_LEN;
    // **踏み込む前に検証する。**
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, buf, cap as u64) })
    else {
        return (-EFAULT) as u64;
    };
    let mut kbuf = [0u8; INPUT_READ_MAX];
    let got = loop {
        let got = crate::input::read_events(&mut kbuf[..cap]);
        if got != 0 {
            break got;
        }
        // **待つ条件は端末と同じ**（`sys_read` の端末分岐の doc）。**据えられていない・台本が
        // 駆動している間は待たない**——**起動シーケンスと台本の族が止まらないように。**
        if !crate::console::foreground_installed() {
            return (-EAGAIN) as u64;
        }
        if crate::input::script_drives_input() {
            return (-EAGAIN) as u64;
        }
        // 破壊 (Y-a, input-read-never-waits): 待たずに `-EAGAIN` を返す。**回して待つ形へ戻る**
        // ——**判定「打鍵で起きる」が落ちる。**
        #[cfg(feature = "input-read-never-waits")]
        return (-EAGAIN) as u64;
        #[cfg(not(feature = "input-read-never-waits"))]
        {
            if wait_for_keyboard(bkl) {
                continue;
            }
            // **前景を失った。** 待ち続けない。
            return (-EBADF) as u64;
        }
    };
    // SAFETY: slice は検証済みで、got は cap を越えない。
    let written = unsafe { copy_to_user(&slice, 0, &kbuf[..got]) };
    written as u64
}

/// [`SYS_OPEN_SCREEN`] の本体（`ADR-0066` の Y-c）。**前景の系統にだけ画面の fd を渡し、図形モードへ入る。**
///
/// # 前景の関所は開く時点の 1 箇所
///
/// **[`open_input_from_ring3`] と同じ形である**——**呼んだ者が前景の系統でなければ `-EBADF`。**
/// **fd は前景より長生きしない**（前景はプログラムの走行の間ずっと持たれる）**し、`SCM_RIGHTS` は
/// shm の fd だけを運ぶので相手の表へ写らない。**
///
/// # 表に入らなければ抜ける
///
/// **図形モードへ入ってから fd を表へ入れる。** **入らなければ（`-EMFILE`）すぐ抜ける**——
/// **fd の無い図形モードを残すと、誰も抜けさせられない。**
fn open_screen_from_ring3() -> u64 {
    if !crate::input::caller_is_foreground() {
        return (-EBADF) as u64;
    }
    match crate::console::enter_graphics() {
        Ok(_) => {}
        Err(crate::console::GraphicsError::NoConsole) => return (-ENODEV) as u64,
        Err(crate::console::GraphicsError::Busy) => return (-EBUSY) as u64,
    }
    let inserted = crate::vfs::with_current_files(|files| files.insert(crate::vfs::File::Screen));
    match inserted {
        Ok(fd) => fd as u64,
        Err(error) => {
            crate::console::leave_graphics();
            (-errno_for_file_table(error)) as u64
        }
    }
}

/// fd が画面か（`ADR-0066` の Y-c）。**表の中身で見る**（番号では分けない）。
fn is_screen_fd(fd: u64) -> bool {
    crate::vfs::with_current_files(|files| {
        files
            .get(fd as usize)
            .ok()
            .map(|file| file.is_screen())
            .unwrap_or(false)
    })
}

/// 画面の fd への `ioctl`（`ADR-0066` の Y-c）。**画面の fd でなければ `None`**（`sys_ioctl` へ回す）。
///
/// - [`FBIOGET_VSCREENINFO`]——`struct fb_var_screeninfo`（Linux の配置）
/// - [`FBIOGET_FSCREENINFO`]——`struct fb_fix_screeninfo`（Linux の配置）
/// - [`FBIOZPRESENT`]——`struct drm_clip_rect` の矩形を MMIO へ写す（ZaytOS 独自）
///
/// **それ以外は `-ENOTTY`**（Linux の fbdev と同じ）。
///
/// # BKL を解いて写す
///
/// **全面の転送は 5.05M サイクル掛かる**（`crate::console::flush_foreground` の doc）。**保持したまま
/// 写すと、その間もう一方のコアがカーネルへ入れない**（`ADR-0023` の Addendum）。
///
/// # 深い枠に写しを置かない
///
/// **`#[inline(never)]` である**——**160 バイトの構造体は `dispatch` の枠に乗らない**（`ADR-0066` の Q4）。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
#[inline(never)]
unsafe fn screen_ioctl_from_ring3(
    fd: u64,
    request: u64,
    arg: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> Option<u64> {
    if !is_screen_fd(fd) {
        return None;
    }
    let Some(surface) = crate::console::graphics_surface() else {
        return Some((-ENODEV) as u64);
    };
    let bgr = matches!(surface.format, common::boot_info::PixelFormat::Bgr);
    match request {
        FBIOGET_VSCREENINFO => {
            let out = fb_var_screeninfo(surface.width, surface.height, bgr);
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            let Some(slice) =
                (unsafe { validate_user_range(pml4_phys, direct_map, arg, out.len() as u64) })
            else {
                return Some((-EFAULT) as u64);
            };
            // SAFETY: `slice` は検証済みで、長さはちょうど `out.len()` である。
            unsafe { copy_to_user(&slice, 0, &out) };
            Some(0)
        }
        FBIOGET_FSCREENINFO => {
            let out = fb_fix_screeninfo(surface.size_bytes as u32, surface.stride * 4);
            // SAFETY: 同上。
            let Some(slice) =
                (unsafe { validate_user_range(pml4_phys, direct_map, arg, out.len() as u64) })
            else {
                return Some((-EFAULT) as u64);
            };
            // SAFETY: 同上。
            unsafe { copy_to_user(&slice, 0, &out) };
            Some(0)
        }
        FBIOZPRESENT => {
            // SAFETY: 同上。
            let Some(slice) = (unsafe {
                validate_user_range(pml4_phys, direct_map, arg, DRM_CLIP_RECT_LEN as u64)
            }) else {
                return Some((-EFAULT) as u64);
            };
            let mut raw = [0u8; DRM_CLIP_RECT_LEN];
            // SAFETY: `slice` は検証済みで、長さはちょうど `raw.len()` である。
            if unsafe { copy_from_user(&mut raw, &slice) } != DRM_CLIP_RECT_LEN {
                return Some((-EFAULT) as u64);
            }
            let Some((x, y, width, height)) = parse_clip_rect(&raw) else {
                return Some((-EINVAL) as u64);
            };
            // **BKL を解いて写す**（この関数の doc）。
            drop(bkl.take());
            crate::console::present_graphics(x, y, width, height);
            *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
            Some(0)
        }
        _ => Some((-ENOTTY) as u64),
    }
}

/// 画面の `mmap` で張ったページの累計（`ADR-0066` の Y-c の計器）。
static SCREEN_PAGES_MAPPED: AtomicU64 = AtomicU64::new(0);

/// 画面の `mmap` で張ったページの累計（`ADR-0066` の Y-c）。
pub fn screen_pages_mapped() -> u64 {
    SCREEN_PAGES_MAPPED.load(Ordering::Relaxed)
}

/// 画面の fd の `mmap`（`ADR-0066` の Y-c）。**裏バッファを自分の空間の `MMAP_BASE` から上へ張る。**
///
/// # 共有メモリと同じ張り方である
///
/// **葉に `PTE_SHARED` の印を立てる**（`ADR-0065`）——**`destroy` は印の在る葉を集めない**ので、
/// **プロセスが終わっても裏バッファのフレームはアロケータへ返らない。** **返すのはコンソールで、
/// 返さない**（起動時に取って、ずっと持つ）。**参照数は使わない**（Q1。カーネル常駐）。
///
/// # 新しく取らない
///
/// **葉は裏バッファのフレームで、アロケータから取るのは中間表だけである**——**その数を破棄の会計へ
/// 足す**（共有メモリの `mmap` と同じ）。
///
/// # Safety
///
/// `direct_map` が有効であること（遠征の中で呼ぶ）。
#[inline(never)]
unsafe fn mmap_screen_from_ring3(len: u64, prot: u64, direct_map: DirectMap) -> u64 {
    use crate::paging::active::{ActivePageTable, PageAttributes};

    let Some(surface) = crate::console::graphics_surface() else {
        return (-ENODEV) as u64;
    };
    const PAGE: u64 = crate::frame_allocator::FRAME_SIZE;
    let limit = surface.size_bytes.div_ceil(PAGE) * PAGE;
    if len == 0 || len > limit {
        return (-EINVAL) as u64;
    }
    let pages = len.div_ceil(PAGE);
    let slot = crate::ring3::current_slot();
    let base = MMAP_NEXT[slot].fetch_add(pages * PAGE, Ordering::SeqCst);
    let attributes = PageAttributes {
        user: true,
        writable: prot & PROT_WRITE != 0,
        // **裏バッファは普通の RAM である**（MMIO ではない。`BackBuffer` の doc）。
        cacheable: true,
        // **印を立てる**——**`destroy` が集めない**（この関数の doc）。
        shared: true,
    };
    // SAFETY: 遠征の中なので CR3 はこのプロセスの表である。
    let mut table = unsafe { ActivePageTable::current(direct_map) };
    let Some(allocator) = crate::frame_allocator::take() else {
        return (-ENOMEM) as u64;
    };
    let free_before_map = allocator.free_frame_count();
    // **借りたら必ず返す**（`sys_brk` と同じ。**どの出口でも `give_back` する**）。
    let mut outcome = base;
    for page in 0..pages {
        let (Some(virt), Some(frame)) = (
            common::addr::VirtAddr::new(base + page * PAGE),
            common::addr::PhysAddr::new(surface.phys.as_u64() + page * PAGE),
        ) else {
            outcome = (-EINVAL) as u64;
            break;
        };
        // SAFETY: 稼働中の表へ、ユーザーの範囲を、裏バッファの物理ページで張る。**裏バッファは
        // 起動時に `pages` ぶん以上を連続で取ってある**（`limit` で切った）。
        if unsafe { table.map_4kib(virt, frame, attributes, allocator) }.is_err() {
            outcome = (-ENOMEM) as u64;
            break;
        }
        SCREEN_PAGES_MAPPED.fetch_add(1, Ordering::Relaxed);
    }
    let tables_taken = free_before_map.saturating_sub(allocator.free_frame_count());
    crate::frame_allocator::give_back(allocator);
    crate::userland::note_post_load_frames(slot, tables_taken as usize);
    outcome
}

/// `poll` の番号（Linux x86-64。`ADR-0066` の Y-b）。
///
/// # 番号と配置は Linux から採る。意味は最小の部分集合である
///
/// **`ADR-0020` に従う**——**`poll`(7) と `struct pollfd`（`fd` 4＋`events` 2＋`revents` 2）を
/// そのまま採る。** **独自番号にしない**（**Linux に対応する口が在るので、`ZAYTOS_PRIVATE_BASE`
/// は使わない**。`ADR-0066` の「番号」）。
///
/// **`ADR-0066` の Q3 は「一般の `poll` は作らない」と決めた。** **作らないのは意味の側である**
/// ——**v1 が見るのは [`POLLIN`] だけで、`timeout` は -1（無限）と 0（待たない）だけを受ける。**
/// **それ以外は `-EINVAL` である**（下の限界）。
pub const SYS_POLL: u64 = 7;

/// `struct pollfd` のバイト数（`fd` 4＋`events` 2＋`revents` 2。Linux の配置）。
const POLLFD_LEN: usize = 8;

/// `POLLIN`（読めるようになった）。**v1 が見る唯一のビットである。**
const POLLIN: u16 = 0x001;

/// 1 回の `poll` に渡せる fd の数。**待ちの集合の大きさと同じである**
/// （[`crate::task::MAX_WAIT_REASONS`]。**集合に入らない数の fd を受けても待てない**）。
const MAX_POLL_FDS: usize = crate::task::MAX_WAIT_REASONS;

/// `poll` が待った回数（`ADR-0066` の Y-b の計器）。**判定「`poll` が待った」が読む。**
static POLL_WAITS: AtomicU64 = AtomicU64::new(0);

/// `poll` が待った回数（`ADR-0066` の Y-b）。
pub fn poll_waits() -> u64 {
    POLL_WAITS.load(Ordering::Relaxed)
}

/// その fd を待つときの理由（`ADR-0066` の Y-b）。**`poll` が受ける fd はこの 3 種だけである。**
///
/// **入力 fd → [`crate::task::Wait::Keyboard`]、接続 → `SocketReadable`、listener →
/// `SocketAcceptable`。** **`Wait` の種類は減らない**（`ADR-0066` の刻みの注）——
/// **対応づけるだけである。**
///
/// **パイプと端末は v1 では受けない**（`None` を返して `-EBADF`）。**契機：パイプを待つ
/// プログラムが出たとき。**
fn poll_reason_of(fd: u64) -> Option<crate::task::Wait> {
    crate::vfs::with_current_files(|files| {
        let file = files.get(fd as usize).ok()?;
        if file.is_input() {
            return Some(crate::task::Wait::Keyboard);
        }
        match file.socket_state() {
            Some(crate::vfs::SocketState::Stream { conn, side }) => {
                Some(crate::task::Wait::SocketReadable { conn, side })
            }
            Some(crate::vfs::SocketState::Listener { listener }) => {
                Some(crate::task::Wait::SocketAcceptable { listener })
            }
            _ => None,
        }
    })
}

/// その理由が今すぐ満たされているか（`ADR-0066` の Y-b）。**覗くだけで、取らない。**
///
/// **取ってしまうと、どの理由で起きたかを返す前にイベントが消える**
/// （`crate::input::has_raw_events` の doc）。
fn poll_is_ready(reason: crate::task::Wait) -> bool {
    match reason {
        crate::task::Wait::Keyboard => crate::input::has_raw_events(),
        crate::task::Wait::SocketReadable { conn, side } => crate::socket::readable(conn, side),
        crate::task::Wait::SocketAcceptable { listener } => crate::socket::acceptable(listener),
        // **[`poll_reason_of`] が返すのは上の 3 種だけである。** **残りは待てない。**
        _ => false,
    }
}

/// [`SYS_POLL`] の本体（`ADR-0066` の Y-b）。**読める fd の数か `-errno` を返す。**
///
/// # 待つ形は W2-c からのものである
///
/// **読める者が居なければ、理由の集合で待つ**（[`crate::task::set_current_waiting_set`]）。
/// **起こされたら集合の各理由を覗き直す**——**空振りで起こしてよい**（`ADR-0061`）。
/// **BKL は解いてから譲り、起きたら取り直す**（`ADR-0036`）。
///
/// # 窓は構造で閉じている
///
/// **`int 0x80` は割り込みゲートなので IF=0 である。** **BKL を解いても IF は戻らない**
/// （`EntryInterruptGuard` は保存した RFLAGS が IF=1 のときだけ戻す。実測）——**「空だと
/// 見てから `Waiting` にする」までに合図は入らない。**
///
/// # 深い枠に写しを置かない
///
/// **`#[inline(never)]` である**（`ADR-0063` の口 3 つと同じ手）。**`struct pollfd` の写しは
/// [`MAX_POLL_FDS`] 個ぶんの 32 バイトで、`dispatch` の枠には乗らない**（`ADR-0066` の Q4）。
///
/// # v1 の限界（契機つき）
///
/// - **`events` は [`POLLIN`] だけを受ける**（`POLLOUT` などは `-EINVAL`）。**黙って無視すると、
///   書ける待ちを頼んだ側が読める待ちで眠る。** **契機：書ける待ちが要るとき。**
/// - **`timeout` は -1 と 0 だけを受ける。** **契機：締切つきの待ちが要るとき**
///   （**集合に [`crate::task::Wait::Timer`] を入れれば足りる**）。
/// - **開いていない fd は `-EBADF` である**（Linux は `revents` に `POLLNVAL` を立てて
///   その 1 件だけを失敗にする）。**契機：混ざった集合を渡す利用者が出たとき。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
#[inline(never)]
unsafe fn poll_from_ring3(
    fds: u64,
    nfds: u64,
    timeout: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    // **`timeout` は `int` である**（Linux の署名）。**下位 32 ビットを符号つきで読む。**
    let timeout = timeout as u32 as i32;
    if nfds == 0 || nfds > MAX_POLL_FDS as u64 {
        return (-EINVAL) as u64;
    }
    if timeout != -1 && timeout != 0 {
        return (-EINVAL) as u64;
    }
    let count = nfds as usize;
    let bytes = (count * POLLFD_LEN) as u64;
    // **踏み込む前に検証する。**
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, fds, bytes) }) else {
        return (-EFAULT) as u64;
    };
    let mut raw = [0u8; MAX_POLL_FDS * POLLFD_LEN];
    // SAFETY: `slice` は検証済みで、長さはちょうど `bytes` である。
    let read = unsafe { copy_from_user(&mut raw[..count * POLLFD_LEN], &slice) };
    if read != count * POLLFD_LEN {
        return (-EFAULT) as u64;
    }
    // **欄を割って、待つ理由へ対応づける。**
    let mut reasons = [None; MAX_POLL_FDS];
    for (index, slot) in reasons.iter_mut().enumerate().take(count) {
        let at = index * POLLFD_LEN;
        let fd = i32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]);
        let events = u16::from_le_bytes([raw[at + 4], raw[at + 5]]);
        if fd < 0 || events != POLLIN {
            return (-EINVAL) as u64;
        }
        let Some(reason) = poll_reason_of(fd as u64) else {
            return (-EBADF) as u64;
        };
        *slot = Some(reason);
    }
    loop {
        // **読める者を数え、`revents` を組む。**
        let mut ready = 0usize;
        let mut revents = [0u16; MAX_POLL_FDS];
        for (index, slot) in reasons.iter().enumerate().take(count) {
            let Some(reason) = *slot else {
                continue;
            };
            if !poll_is_ready(reason) {
                continue;
            }
            // 破壊 (Y-b, poll-mistakes-the-member): 隣の欄へ印を付ける。**待ちも起こしも
            // 正しいままで、「どの fd が読めるか」だけが入れ替わる**——**判定「listener で
            // 起きた」「ソケットで起きた」が落ちる。**
            let at = if cfg!(feature = "poll-mistakes-the-member") {
                (index + 1) % count
            } else {
                index
            };
            revents[at] = POLLIN;
            ready += 1;
        }
        if ready > 0 {
            for (index, revent) in revents.iter().enumerate().take(count) {
                let at = index * POLLFD_LEN;
                raw[at + 6..at + 8].copy_from_slice(&revent.to_le_bytes());
            }
            // SAFETY: `slice` は検証済みで、書く長さは検証した `bytes` を越えない。
            unsafe { copy_to_user(&slice, 0, &raw[..count * POLLFD_LEN]) };
            return ready as u64;
        }
        // **待たない頼み（`timeout` = 0）は、ここで 0 を返す。**
        if timeout == 0 {
            return 0;
        }
        // **待つ条件は端末の `read(0)` と同じである**（`sys_read` の端末分岐の doc）。
        // **対話の口が据えられていない間と、台本が入力を駆動している間は待たない**
        // ——**起動シーケンスと台本の族が止まらないようにするためである。**
        // **呼ぶ側は `-EAGAIN` を回して待つ**（`polld` と `inputd` の形）。
        if !crate::console::foreground_installed() || crate::input::script_drives_input() {
            return (-EAGAIN) as u64;
        }
        // **理由の集合を組む。**
        let mut set = crate::task::WaitSet::empty();
        for slot in reasons.iter().take(count) {
            // 破壊 (Y-b, poll-waits-on-one-member): 集合へ入れるのは最初の 1 本だけにする。
            // **落ちた合図では誰も起こさないので `polld` が戻らない**——**判定「集合に 2 本
            // 入った」が落ち、戻らないので計器の行も出ない**（`ADR-0066` の Y-b の表）。
            if cfg!(feature = "poll-waits-on-one-member") && !set.is_empty() {
                break;
            }
            let Some(reason) = *slot else {
                continue;
            };
            if !set.push(reason) {
                // **集合に入らない**——**[`MAX_POLL_FDS`] で断っているので、ここへは来ない。**
                return (-EINVAL) as u64;
            }
        }
        // 破壊 (Y-b, poll-never-waits): 待たずに 0 を返す。**呼ぶ側が回して待つ形へ戻る**
        // ——**判定「`poll` が待った」だけが落ちる。** **`cfg!` で書くのは、`#[cfg]` の早い
        // 戻りにすると「回らない回し」になって `clippy` が止めるためである。**
        if cfg!(feature = "poll-never-waits") {
            return 0;
        }
        POLL_WAITS.fetch_add(1, Ordering::Relaxed);
        crate::task::set_current_waiting_set(set);
        drop(bkl.take());
        crate::task::yield_now();
        *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
    }
}

/// [`SYS_BIND`] の本体。**名前を取り、fd を listener にする**（`listen` はまだ）。
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
#[inline(never)]
unsafe fn bind_from_ring3(
    fd: u64,
    addr: u64,
    addrlen: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> u64 {
    match socket_state_of(fd) {
        Ok(crate::vfs::SocketState::Unbound) => {}
        Ok(_) => return (-EINVAL) as u64,
        Err(errno) => return errno,
    }
    let mut name = [0u8; crate::socket::NAME_MAX];
    // SAFETY: 呼び出し元契約による。
    let len = match unsafe { read_socket_name(addr, addrlen, pml4_phys, direct_map, &mut name) } {
        Ok(len) => len,
        Err(errno) => return (-errno) as u64,
    };
    match crate::socket::bind(&name[..len]) {
        Ok(listener) => {
            crate::vfs::with_current_files(|files| {
                files.replace(
                    fd as usize,
                    crate::vfs::File::Socket {
                        state: crate::vfs::SocketState::Listener { listener },
                    },
                )
            });
            0
        }
        Err(crate::socket::BindError::NameTaken) => (-EADDRINUSE) as u64,
        Err(crate::socket::BindError::NoRoom) => (-ENOBUFS) as u64,
    }
}

/// [`SYS_LISTEN`] の本体。**`bind` 済みの fd だけを受ける。** **`backlog` は見ない**
/// （接続の上限で頭を切る）。
#[inline(never)]
fn listen_from_ring3(fd: u64, _backlog: u64) -> u64 {
    match socket_state_of(fd) {
        Ok(crate::vfs::SocketState::Listener { listener }) => {
            if crate::socket::listen(listener) {
                0
            } else {
                (-EINVAL) as u64
            }
        }
        Ok(_) => (-EINVAL) as u64,
        Err(errno) => errno,
    }
}

/// [`SYS_ACCEPT`] の本体。**待ち行列が空なら待つ。** **繋がった接続を新しい fd に置く。**
///
/// 破壊 (`ADR-0064`, socket-accept-does-not-wait): 待たずに `-EAGAIN` を返す。
/// **`sockd` が `accept failed` で終わる。**
#[inline(never)]
fn accept_from_ring3(fd: u64, addr: u64, bkl: &mut Option<crate::bkl::BklGuard>) -> u64 {
    let listener = match socket_state_of(fd) {
        Ok(crate::vfs::SocketState::Listener { listener }) => listener,
        Ok(_) => return (-EINVAL) as u64,
        Err(errno) => return errno,
    };
    // **相手の名前は返さない**（限界）。**NULL 以外は断る**——黙って書かない形にしない。
    if addr != 0 {
        return (-EINVAL) as u64;
    }
    loop {
        match crate::socket::accept(listener) {
            crate::socket::AcceptOutcome::Connection(conn) => {
                let inserted = crate::vfs::with_current_files(|files| {
                    files.insert(crate::vfs::File::Socket {
                        state: crate::vfs::SocketState::Stream {
                            conn,
                            side: crate::socket::Side::Server,
                        },
                    })
                });
                return match inserted {
                    Ok(new_fd) => new_fd as u64,
                    Err(error) => {
                        // **表が満杯なら、取った接続の server 側を閉じる**——相手は EOF を見る。
                        crate::socket::close_end(conn, crate::socket::Side::Server);
                        (-errno_for_file_table(error)) as u64
                    }
                };
            }
            crate::socket::AcceptOutcome::NoListener => return (-EINVAL) as u64,
            crate::socket::AcceptOutcome::Empty => {
                #[cfg(feature = "socket-accept-does-not-wait")]
                return (-EAGAIN) as u64;
                #[cfg(not(feature = "socket-accept-does-not-wait"))]
                {
                    crate::socket::note_accept_wait();
                    crate::task::set_current_waiting(crate::task::Wait::SocketAcceptable {
                        listener,
                    });
                    drop(bkl.take());
                    crate::task::yield_now();
                    *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
                }
            }
        }
    }
}

/// [`SYS_CONNECT`] の本体。**名前で繋ぎ、fd をその場でストリームにする。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
#[inline(never)]
unsafe fn connect_from_ring3(
    fd: u64,
    addr: u64,
    addrlen: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> u64 {
    match socket_state_of(fd) {
        Ok(crate::vfs::SocketState::Unbound) => {}
        Ok(crate::vfs::SocketState::Stream { .. }) => return (-EISCONN) as u64,
        Ok(crate::vfs::SocketState::Listener { .. }) => return (-EINVAL) as u64,
        Err(errno) => return errno,
    }
    let mut name = [0u8; crate::socket::NAME_MAX];
    // SAFETY: 呼び出し元契約による。
    let len = match unsafe { read_socket_name(addr, addrlen, pml4_phys, direct_map, &mut name) } {
        Ok(len) => len,
        Err(errno) => return (-errno) as u64,
    };
    match crate::socket::connect(&name[..len]) {
        Ok(conn) => {
            crate::vfs::with_current_files(|files| {
                files.replace(
                    fd as usize,
                    crate::vfs::File::Socket {
                        state: crate::vfs::SocketState::Stream {
                            conn,
                            side: crate::socket::Side::Client,
                        },
                    },
                )
            });
            0
        }
        Err(crate::socket::ConnectError::NoListener) => (-ECONNREFUSED) as u64,
        Err(crate::socket::ConnectError::NoRoom) => (-EAGAIN) as u64,
    }
}

/// ソケットのストリームから読む（`ADR-0064`）。**空なら待つ。相手が閉じていれば 0（EOF）。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
#[inline(never)]
unsafe fn read_from_socket(
    conn: u8,
    side: crate::socket::Side,
    buf: u64,
    count: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    if count == 0 {
        return 0;
    }
    let want = count.min(crate::socket::SOCKET_RING as u64);
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, buf, want) }) else {
        return (-EFAULT) as u64;
    };
    let mut kbuf = [0u8; crate::socket::SOCKET_RING];
    loop {
        match crate::socket::read_into(conn, side, &mut kbuf[..want as usize]) {
            crate::socket::ReadOutcome::Bytes(got) => {
                // SAFETY: `slice` は検証済みで、`got` はその長さを越えない。
                let written = unsafe { copy_to_user(&slice, 0, &kbuf[..got]) };
                return written as u64;
            }
            crate::socket::ReadOutcome::Eof => return 0,
            crate::socket::ReadOutcome::NoConnection => return (-ENOTCONN) as u64,
            crate::socket::ReadOutcome::Empty => {
                crate::socket::note_reader_wait();
                crate::task::set_current_waiting(crate::task::Wait::SocketReadable { conn, side });
                drop(bkl.take());
                crate::task::yield_now();
                *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
            }
        }
    }
}

/// ソケットのその側が読める（データか EOF）か、接続が無くなるまで待つ（2026-09-23）。
///
/// **読みはしない**——**続く [`read_from_socket`] が読む。** **待ち方は [`read_from_socket`] と同じで、
/// 待ちの数も同じ計器へ数える。** **接続が無ければ待たない**（続く読みが `-ENOTCONN` を返す）。
#[cfg_attr(feature = "socket-recvmsg-takes-fd-first", allow(dead_code))]
fn wait_until_readable(
    conn: u8,
    side: crate::socket::Side,
    bkl: &mut Option<crate::bkl::BklGuard>,
) {
    while !crate::socket::readable_or_gone(conn, side) {
        crate::socket::note_reader_wait();
        crate::task::set_current_waiting(crate::task::Wait::SocketReadable { conn, side });
        drop(bkl.take());
        crate::task::yield_now();
        *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
    }
}

/// ソケットのストリームへ書く（`ADR-0064`）。**満杯なら待つ。相手が閉じていれば `-EPIPE`。**
///
/// **部分書きである**——**入った数を返す。** **`userlib::write_all` が残りを回す。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
#[inline(never)]
unsafe fn write_to_socket(
    conn: u8,
    side: crate::socket::Side,
    buf: u64,
    count: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    if count == 0 {
        return 0;
    }
    let want = count.min(crate::socket::SOCKET_RING as u64);
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, buf, want) }) else {
        return (-EFAULT) as u64;
    };
    let mut kbuf = [0u8; crate::socket::SOCKET_RING];
    // SAFETY: `slice` は検証済みで、`want` はその長さである。
    let read = unsafe { copy_from_user(&mut kbuf[..want as usize], &slice) };
    if read == 0 {
        return (-EFAULT) as u64;
    }
    loop {
        match crate::socket::write_from(conn, side, &kbuf[..read]) {
            crate::socket::WriteOutcome::Bytes(put) => return put as u64,
            crate::socket::WriteOutcome::PeerClosed => return (-EPIPE) as u64,
            crate::socket::WriteOutcome::NoConnection => return (-ENOTCONN) as u64,
            crate::socket::WriteOutcome::Full => {
                crate::socket::note_writer_wait();
                crate::task::set_current_waiting(crate::task::Wait::SocketWritable { conn, side });
                drop(bkl.take());
                crate::task::yield_now();
                *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
            }
        }
    }
}

/// `mmap` の番号（Linux x86-64。`ADR-0065`）。**共有メモリの fd を自分の空間へ張る。**
pub const SYS_MMAP: u64 = 9;
/// `ftruncate` の番号。**共有メモリの大きさを据える（ページを取る）。**
pub const SYS_FTRUNCATE: u64 = 77;
/// `sendmsg` の番号。**iov のバイトをソケットへ、`SCM_RIGHTS` の fd を相手の表へ。**
pub const SYS_SENDMSG: u64 = 46;
/// `recvmsg` の番号。**ソケットのバイトを iov へ、渡された fd を自分の表へ。**
pub const SYS_RECVMSG: u64 = 47;
/// `memfd_create` の番号。**無名の共有メモリを作り fd を返す。**
pub const SYS_MEMFD_CREATE: u64 = 319;

/// `PROT_WRITE`（`mmap`。書ける葉を張る）。
const PROT_WRITE: u64 = 2;
/// `SOL_SOCKET`（`cmsghdr` の level）。
const SOL_SOCKET: u32 = 1;
/// `SCM_RIGHTS`（`cmsghdr` の type。fd を運ぶ）。
const SCM_RIGHTS: u32 = 1;
/// `EMSGSIZE`（補助データが規定の形でない）。
const EMSGSIZE: i64 = 90;

/// `mmap` が張る基点（プロセスごと）。**像・ヒープ・スタックは `0x400000..0x800000` に
/// 収まっているので、その上（PML4[0] の空き）へ順に張る**（`ADR-0065`。窓の拡張は要らない）。
const MMAP_BASE: u64 = 0x1000_0000;

/// 次に `mmap` で張る番地（スロットごと。`MMAP_BASE` から上へ）。
static MMAP_NEXT: [core::sync::atomic::AtomicU64; crate::ring3::RING3_SLOTS] =
    [const { core::sync::atomic::AtomicU64::new(MMAP_BASE) }; crate::ring3::RING3_SLOTS];

/// fd から共有メモリの添字を引く。**共有メモリでなければ `Err(-EBADF)`。**
fn shm_of(fd: u64) -> Result<u8, u64> {
    let found = crate::vfs::with_current_files(|files| {
        files.get(fd as usize).ok().and_then(|file| match file {
            crate::vfs::File::Shm { shm } => Some(*shm),
            _ => None,
        })
    });
    found.ok_or((-EBADF) as u64)
}

/// [`SYS_MEMFD_CREATE`] の本体。**無名の共有メモリを作り、最小の空き fd に据える。**
/// **名前と旗は見ない**（最小のため。Linux は名前をデバッグに使うだけ）。
#[inline(never)]
fn memfd_create_from_ring3() -> u64 {
    let Some(shm) = crate::shm::create() else {
        return (-ENOMEM) as u64;
    };
    let inserted =
        crate::vfs::with_current_files(|files| files.insert(crate::vfs::File::Shm { shm }));
    match inserted {
        Ok(fd) => fd as u64,
        Err(error) => {
            crate::shm::detach(shm);
            (-errno_for_file_table(error)) as u64
        }
    }
}

/// [`SYS_FTRUNCATE`] の本体。**共有メモリの大きさを据える（ページを取る）。**
#[inline(never)]
fn ftruncate_from_ring3(fd: u64, size: u64) -> u64 {
    let shm = match shm_of(fd) {
        Ok(shm) => shm,
        Err(errno) => return errno,
    };
    match crate::shm::set_size(shm, size) {
        crate::shm::TruncateOutcome::Pages(_) => 0,
        crate::shm::TruncateOutcome::TooLarge | crate::shm::TruncateOutcome::NoRoom => {
            (-ENOMEM) as u64
        }
        crate::shm::TruncateOutcome::AlreadySet => (-EINVAL) as u64,
        crate::shm::TruncateOutcome::NoShm => (-EBADF) as u64,
    }
}

/// [`SYS_MMAP`] の本体。**共有メモリの fd を自分の空間の `MMAP_BASE` から上へ張る。**
/// **`addr` は見ない（張る場所はカーネルが決める）。`offset` は 0 だけ。**
///
/// 破壊 (`ADR-0065`, shm-mmap-maps-nothing): 張らずに番地だけ返す。**読み書きが #PF になり、
/// 往復が成り立たない。**
///
/// # 安全性
///
/// 呼び出し元契約により `direct_map` は有効で、遠征の中なので CR3 はこのプロセスのもの。
#[inline(never)]
unsafe fn mmap_from_ring3(len: u64, prot: u64, fd: u64, offset: u64, direct_map: DirectMap) -> u64 {
    use crate::paging::active::{ActivePageTable, PageAttributes};

    if offset != 0 {
        return (-EINVAL) as u64;
    }
    // **画面の fd なら裏バッファを張る（`ADR-0066` の Y-c）。** **張り方は共有メモリと同じ**
    // （`PTE_SHARED`）。
    if is_screen_fd(fd) {
        // SAFETY: 呼び出し元契約をそのまま渡す。
        return unsafe { mmap_screen_from_ring3(len, prot, direct_map) };
    }
    let shm = match shm_of(fd) {
        Ok(shm) => shm,
        Err(errno) => return errno,
    };
    let mut frames = [common::addr::PhysAddr::new_const(0); crate::shm::MAX_SHM_PAGES];
    let Some((pages, shm_len)) = crate::shm::frames_of(shm, &mut frames) else {
        return (-EINVAL) as u64;
    };
    // **要求は据えた大きさを越えない。**
    if len == 0 || len > shm_len {
        return (-EINVAL) as u64;
    }
    let want_pages = crate::shm::pages_for(len);
    if want_pages > pages {
        return (-EINVAL) as u64;
    }
    let slot = crate::ring3::current_slot();
    let base = MMAP_NEXT[slot].fetch_add(
        (want_pages * crate::shm::PAGE_SIZE) as u64,
        core::sync::atomic::Ordering::SeqCst,
    );
    let attributes = PageAttributes {
        user: true,
        writable: prot & PROT_WRITE != 0,
        cacheable: true,
        // **共有メモリの葉に印を立てる（`ADR-0065`）。** **`destroy` が集めず、`crate::shm` が
        // 参照数で返す。**
        shared: true,
    };
    // SAFETY: 遠征の中なので CR3 はこのプロセスの表である。
    let mut table = unsafe { ActivePageTable::current(direct_map) };
    let Some(allocator) = crate::frame_allocator::take() else {
        return (-ENOMEM) as u64;
    };
    // **載せた後に取った PT を数える（`ADR-0065` の (a)）。** **`map_4kib` が新しい領域へ
    // 中間表を取るので、その分を破棄の会計の `taken` に足す**——**葉は共有フレームで
    // アロケータに触らないので、差は表の分だけである。**
    let free_before_map = allocator.free_frame_count();
    // **借りたら必ず返す**（`sys_brk` と同じ。**どの出口でも `give_back` する**）。
    let mut outcome = base;
    for (page, frame) in frames.iter().enumerate().take(want_pages) {
        let Some(virt) = common::addr::VirtAddr::new(base + (page * crate::shm::PAGE_SIZE) as u64)
        else {
            outcome = (-EINVAL) as u64;
            break;
        };
        // 破壊 (`ADR-0065`, shm-mmap-maps-nothing): 張らない。
        #[cfg(not(feature = "shm-mmap-maps-nothing"))]
        {
            // SAFETY: 稼働中の表へ、ユーザーの範囲を、共有メモリの物理ページで張る。
            if unsafe { table.map_4kib(virt, *frame, attributes, allocator) }.is_err() {
                outcome = (-ENOMEM) as u64;
                break;
            }
            crate::shm::note_mapped_page();
        }
        #[cfg(feature = "shm-mmap-maps-nothing")]
        {
            let _ = (&mut table, *frame, attributes, virt);
        }
    }
    let tables_taken = free_before_map.saturating_sub(allocator.free_frame_count());
    crate::frame_allocator::give_back(allocator);
    crate::userland::note_post_load_frames(crate::ring3::current_slot(), tables_taken as usize);
    outcome
}

/// `msghdr` を読んで、iov の 1 本目と `SCM_RIGHTS` の fd 1 つを取り出す（`ADR-0065`）。
///
/// **形は Wayland が打つものに絞る**——**iov は 1 本、補助データは `SCM_RIGHTS` の fd 1 つ。**
/// **それ以外は `-EMSGSIZE` / `-EINVAL` で断る**（黙って別の形を通さない）。
struct ParsedMsg {
    iov_base: u64,
    iov_len: u64,
    control: u64,
    controllen: u64,
    control_fd: Option<u64>,
}

/// `msghdr` の欄を読む（56 バイト）。**iov は 1 本だけ受ける。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
unsafe fn read_msghdr(
    msg: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    want_fd: bool,
) -> Result<ParsedMsg, i64> {
    // SAFETY: 呼び出し元契約による。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, msg, 56) }) else {
        return Err(EFAULT);
    };
    let mut hdr = [0u8; 56];
    // SAFETY: `slice` は検証済みで 56 バイト。
    if unsafe { copy_from_user(&mut hdr, &slice) } != 56 {
        return Err(EFAULT);
    }
    let u64_at = |off: usize| u64::from_le_bytes(hdr[off..off + 8].try_into().unwrap());
    let name = u64_at(0);
    let iov = u64_at(16);
    let iovlen = u64_at(24);
    let control = u64_at(32);
    let controllen = u64_at(40);
    // **受け付ける形を絞る（`ADR-0065`）。** **一覧は `ADR-0065` の「`msghdr` の絞った範囲」に
    // 在る。** **`msg_name` は NULL だけ**（繋がったストリームは宛先を持たない）。
    if name != 0 {
        return Err(EINVAL);
    }
    // **`msg_iovlen` は 1 だけ**（散らばり集めは持たない）。
    //
    // 破壊 (`ADR-0065`, socket-msghdr-ignores-iovlen): これを見ない。**iovlen が 2 でも受けて
    // 1 本目だけ送る**——**`sockc` の badmsg が `-EINVAL` を得られず、送ったバイト数が返る。**
    #[cfg(not(feature = "socket-msghdr-ignores-iovlen"))]
    if iovlen != 1 {
        return Err(EINVAL);
    }
    // **iovec を読む（16 バイト）。**
    // SAFETY: 呼び出し元契約による。
    let Some(iov_slice) = (unsafe { validate_user_range(pml4_phys, direct_map, iov, 16) }) else {
        return Err(EFAULT);
    };
    let mut iovbuf = [0u8; 16];
    // SAFETY: 検証済み 16 バイト。
    if unsafe { copy_from_user(&mut iovbuf, &iov_slice) } != 16 {
        return Err(EFAULT);
    }
    let iov_base = u64::from_le_bytes(iovbuf[0..8].try_into().unwrap());
    let iov_len = u64::from_le_bytes(iovbuf[8..16].try_into().unwrap());

    let mut control_fd = None;
    if want_fd && control != 0 && controllen >= 20 {
        // **cmsghdr を読む（16 バイト）＋ fd（4 バイト）。**
        // SAFETY: 呼び出し元契約による。
        let Some(cmsg_slice) = (unsafe { validate_user_range(pml4_phys, direct_map, control, 20) })
        else {
            return Err(EFAULT);
        };
        let mut cbuf = [0u8; 20];
        // SAFETY: 検証済み 20 バイト。
        if unsafe { copy_from_user(&mut cbuf, &cmsg_slice) } != 20 {
            return Err(EFAULT);
        }
        let level = u32::from_le_bytes(cbuf[8..12].try_into().unwrap());
        let ctype = u32::from_le_bytes(cbuf[12..16].try_into().unwrap());
        if level != SOL_SOCKET || ctype != SCM_RIGHTS {
            return Err(EINVAL);
        }
        control_fd = Some(u32::from_le_bytes(cbuf[16..20].try_into().unwrap()) as u64);
    }
    Ok(ParsedMsg {
        iov_base,
        iov_len,
        control,
        controllen,
        control_fd,
    })
}

/// [`SYS_SENDMSG`] の本体。**iov のバイトをソケットへ書き、`SCM_RIGHTS` の fd を相手へ渡す。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
#[inline(never)]
unsafe fn sendmsg_from_ring3(
    fd: u64,
    msg: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    let (conn, side) = match socket_state_of(fd) {
        Ok(crate::vfs::SocketState::Stream { conn, side }) => (conn, side),
        Ok(_) => return (-ENOTCONN) as u64,
        Err(errno) => return errno,
    };
    // SAFETY: 呼び出し元契約による。
    let parsed = match unsafe { read_msghdr(msg, pml4_phys, direct_map, true) } {
        Ok(parsed) => parsed,
        Err(errno) => return (-errno) as u64,
    };
    // **fd を先に渡す**——**`SCM_RIGHTS`。共有メモリの fd を相手の待ち行列へ。**
    if let Some(shm_fd) = parsed.control_fd {
        let shm = match shm_of(shm_fd) {
            Ok(shm) => shm,
            Err(errno) => return errno,
        };
        if !crate::socket::queue_fd(conn, side, shm) {
            return (-EAGAIN) as u64;
        }
        crate::shm::attach(shm);
        crate::shm::note_fd_sent();
    }
    // **iov のバイトを書く**（ソケットの書きと同じ経路）。
    // SAFETY: 呼び出し元契約による。
    unsafe {
        write_to_socket(
            conn,
            side,
            parsed.iov_base,
            parsed.iov_len,
            pml4_phys,
            direct_map,
            bkl,
        )
    }
}

/// [`SYS_RECVMSG`] の本体。**ソケットのバイトを iov へ、渡された fd を自分の表へ。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
#[inline(never)]
unsafe fn recvmsg_from_ring3(
    fd: u64,
    msg: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    let (conn, side) = match socket_state_of(fd) {
        Ok(crate::vfs::SocketState::Stream { conn, side }) => (conn, side),
        Ok(_) => return (-ENOTCONN) as u64,
        Err(errno) => return errno,
    };
    // SAFETY: 呼び出し元契約による。
    let parsed = match unsafe { read_msghdr(msg, pml4_phys, direct_map, false) } {
        Ok(parsed) => parsed,
        Err(errno) => return (-errno) as u64,
    };
    // **読めるようになるまで待ってから、渡された fd を取る**（2026-09-23）。
    //
    // **入口で取ると、受け手が先に待っていた回に取りこぼす**——**待つ間は BKL を放すので、その間に
    // 送り手が「fd を置く→データを書く」を済ませ、起きた後はデータだけを返していた**（`--full` で
    // 1 度落ちた。`docs/troubleshooting.md` の 2026-09-23 の項）。**送り手は fd を先に置くので、
    // データが読めるなら fd は既に置かれている。**
    //
    // 破壊 (2026-09-23, socket-recvmsg-takes-fd-first): 待たずに取る（直す前の形）。**受け手が先に
    // 待つ台本（`sockc shmlate`）で fd が届かず、`shm-ok` が返らない。**
    #[cfg(not(feature = "socket-recvmsg-takes-fd-first"))]
    wait_until_readable(conn, side, bkl);
    // **渡された fd が在れば、自分の表へ据え、cmsghdr を書き戻す。**
    if let Some(shm) = crate::socket::take_fd(conn, side) {
        let inserted =
            crate::vfs::with_current_files(|files| files.insert(crate::vfs::File::Shm { shm }));
        let new_fd = match inserted {
            Ok(new_fd) => new_fd as u64,
            Err(error) => {
                crate::shm::detach(shm);
                return (-errno_for_file_table(error)) as u64;
            }
        };
        if parsed.control == 0 || parsed.controllen < 20 {
            return (-EMSGSIZE) as u64;
        }
        // **cmsghdr（cmsg_len=20, level=SOL_SOCKET, type=SCM_RIGHTS）＋ fd を書く。**
        let mut cbuf = [0u8; 20];
        cbuf[0..8].copy_from_slice(&20u64.to_le_bytes());
        cbuf[8..12].copy_from_slice(&SOL_SOCKET.to_le_bytes());
        cbuf[12..16].copy_from_slice(&SCM_RIGHTS.to_le_bytes());
        cbuf[16..20].copy_from_slice(&(new_fd as u32).to_le_bytes());
        // SAFETY: 呼び出し元契約による。
        let Some(cslice) =
            (unsafe { validate_user_range(pml4_phys, direct_map, parsed.control, 20) })
        else {
            return (-EFAULT) as u64;
        };
        // SAFETY: 検証済み 20 バイト。
        unsafe { copy_to_user(&cslice, 0, &cbuf) };
        // **msg_controllen を 20 に書き戻す。**
        // SAFETY: 呼び出し元契約による。
        if let Some(mslice) = unsafe { validate_user_range(pml4_phys, direct_map, msg + 40, 8) } {
            // SAFETY: 検証済み 8 バイト。
            unsafe { copy_to_user(&mslice, 0, &20u64.to_le_bytes()) };
        }
        crate::shm::note_fd_received();
    }
    // **バイトを読む**（ソケットの読みと同じ経路）。
    // SAFETY: 呼び出し元契約による。
    unsafe {
        read_from_socket(
            conn,
            side,
            parsed.iov_base,
            parsed.iov_len,
            pml4_phys,
            direct_map,
            bkl,
        )
    }
}

/// [`SYS_SPAWN_DETACHED`] の本体。**引数の写しは [`spawn_from_ring3`] と同じ形である。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
unsafe fn spawn_detached_from_ring3(
    path: u64,
    argv: u64,
    envp: u64,
    flags: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    if flags & !DETACHED_STDOUT_TO_PIPE != 0 {
        return (-EINVAL) as u64;
    }
    let mut buf = [0u8; PATH_MAX];
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let len = match unsafe { copy_user_path(&mut buf, path, pml4_phys, direct_map) } {
        Ok(len) => len,
        Err(errno) => return (-errno) as u64,
    };
    let mut argv_bytes = [0u8; MAX_ARGV_BYTES];
    // SAFETY: 同上。
    let (argv_count, argv_used) = match unsafe {
        copy_user_string_array(
            &mut argv_bytes,
            argv,
            crate::userland::MAX_ARGV,
            pml4_phys,
            direct_map,
        )
    } {
        Ok(pair) => pair,
        Err(errno) => return (-errno) as u64,
    };
    let mut envp_bytes = [0u8; MAX_ENVP_BYTES];
    // SAFETY: 同上。
    let (envp_count, envp_used) = match unsafe {
        copy_user_string_array(
            &mut envp_bytes,
            envp,
            crate::userland::MAX_ENVP,
            pml4_phys,
            direct_map,
        )
    } {
        Ok(pair) => pair,
        Err(errno) => return (-errno) as u64,
    };

    // **起こす前に探す。** **無ければ同期で `-ENOENT`**（シェルの `PATH` の輪が次へ進む）。
    if let Err(error) = crate::userland::probe_program(&buf[..len]) {
        return (-errno_for_spawn(error)) as u64;
    }

    let stdout_pipe = if flags & DETACHED_STDOUT_TO_PIPE != 0 {
        match crate::pipe::create(true) {
            Some(pipe) => Some(pipe),
            None => return (-EBUSY) as u64,
        }
    } else {
        None
    };

    let Some(handle) = crate::userland::start_detached(
        &buf[..len],
        &argv_bytes[..argv_used],
        argv_count,
        Some((&envp_bytes[..envp_used], envp_count)),
        stdout_pipe,
    ) else {
        // **起こせなかった**（回収されていない子が居る・走っている最中）。**作ったパイプを
        // 片づける**——**書き端と予約の両方を返す。**
        if let Some(pipe) = stdout_pipe {
            crate::pipe::drop_reservation(pipe);
            crate::pipe::close_write_end(pipe);
        }
        return (-EAGAIN) as u64;
    };
    if let Some(pipe) = stdout_pipe {
        crate::userland::set_pending_stdin(crate::ring3::current_slot(), pipe);
    }
    DETACHED_STARTS.fetch_add(1, Ordering::Relaxed);

    // **子が Ring 3 へ入るか終わるまで戻らない**（[`SYS_SPAWN_DETACHED`] の doc）。
    //
    // 破壊 (`ADR-0063` の (b3), spawn-detached-returns-early): **入場を待たず、1 度だけ譲って
    // 戻る。** **左が読み込みに入った直後にシェルへ戻し、左の読み込み（`load_user_program`。
    // 同じ破壊が貸し出しを持ったまま 2 ティック回る）の最中に右を `spawn` させる**——
    // **右が `AllocatorUnavailable` で起こせない。** **「機会を作る」形である**——**待たない
    // だけでは、右の `spawn` は左が走り出す前（μs）に終わり、21 本の `|` で 1 度も重ならなかった。**
    // **右の読み込みに回りを置く形は誤りだった**——**システムコールの中は BKL を解いても IF=0 の
    // ままで、ティックを見られずに永久に回った**（実測。`docs/troubleshooting.md`）。
    #[cfg(feature = "spawn-detached-returns-early")]
    {
        drop(bkl.take());
        crate::task::yield_now();
        *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
    }
    #[cfg(not(feature = "spawn-detached-returns-early"))]
    {
        let since = crate::idt::timer_ticks();
        drop(bkl.take());
        while !(crate::task::ring3_task_in_excursion() || crate::task::ring3_task_finished()) {
            crate::task::yield_now();
        }
        *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
        let waited = crate::idt::timer_ticks().saturating_sub(since);
        DETACHED_ENTRY_WAIT_TICKS_MAX.fetch_max(waited, Ordering::Relaxed);
    }
    handle
}

/// [`SYS_SPAWN_WITH_PIPED_STDIN`] の本体。
///
/// **予約の消費は探した後である**——**`PATH` の輪が `-ENOENT` で次へ進む間、予約は残る。**
///
/// # 安全性
///
/// 呼び出し元契約により `pml4_phys` / `direct_map` は有効。
unsafe fn spawn_with_piped_stdin_from_ring3(
    path: u64,
    argv: u64,
    envp: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    let slot = crate::ring3::current_slot();
    let Some(pipe) = crate::userland::peek_pending_stdin(slot) else {
        return (-EINVAL) as u64;
    };
    let mut buf = [0u8; PATH_MAX];
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let len = match unsafe { copy_user_path(&mut buf, path, pml4_phys, direct_map) } {
        Ok(len) => len,
        Err(errno) => return (-errno) as u64,
    };
    if let Err(error) = crate::userland::probe_program(&buf[..len]) {
        return (-errno_for_spawn(error)) as u64;
    }
    let mut argv_bytes = [0u8; MAX_ARGV_BYTES];
    // SAFETY: 同上。
    let (argv_count, argv_used) = match unsafe {
        copy_user_string_array(
            &mut argv_bytes,
            argv,
            crate::userland::MAX_ARGV,
            pml4_phys,
            direct_map,
        )
    } {
        Ok(pair) => pair,
        Err(errno) => return (-errno) as u64,
    };
    let mut envp_bytes = [0u8; MAX_ENVP_BYTES];
    // SAFETY: 同上。
    let (envp_count, envp_used) = match unsafe {
        copy_user_string_array(
            &mut envp_bytes,
            envp,
            crate::userland::MAX_ENVP,
            pml4_phys,
            direct_map,
        )
    } {
        Ok(pair) => pair,
        Err(errno) => return (-errno) as u64,
    };

    // **ここで予約を消費する。** **探した後なので、`-ENOENT` の輪では消費されない。**
    let _ = crate::userland::take_pending_stdin(slot);
    if !crate::pipe::claim_reserved_reader(pipe) {
        return (-EINVAL) as u64;
    }
    crate::userland::set_inherit_stdin(slot, pipe);

    drop(bkl.take());
    let result = crate::userland::spawn(
        &buf[..len],
        &argv_bytes[..argv_used],
        argv_count,
        Some((&envp_bytes[..envp_used], envp_count)),
    );
    *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));

    // **消費されなかったら（読み込みの前に失敗した）、読み端を返す**——**左が `-EPIPE` で戻れる。**
    if let Some(unused) = crate::userland::take_inherit_stdin(slot) {
        crate::pipe::close_read_end(unused);
    }

    match result {
        Ok(crate::userland::SpawnOutcome::Exited(status)) => status & 0xFF,
        Ok(crate::userland::SpawnOutcome::Folded(vector)) => SPAWN_FOLDED_FLAG | (vector << 9),
        Ok(crate::userland::SpawnOutcome::Interrupted) => SPAWN_INTERRUPTED_FLAG,
        Err(error) => (-errno_for_spawn(error)) as u64,
    }
}

/// [`SYS_WAIT_CHILD`] の本体。
fn wait_child_from_ring3(handle: u64, bkl: &mut Option<crate::bkl::BklGuard>) -> u64 {
    // **使われなかった予約を消す**（[`SYS_WAIT_CHILD`] の doc）。
    //
    // 破壊 (`ADR-0063` の (b3), wait-child-keeps-reservation): 消さない。**右が見つからなかった
    // 回の後、パイプが空かず、次の `|` が `-EBUSY` になる。**
    #[cfg(not(feature = "wait-child-keeps-reservation"))]
    if let Some(pipe) = crate::userland::take_pending_stdin(crate::ring3::current_slot()) {
        crate::pipe::drop_reservation(pipe);
    }
    drop(bkl.take());
    let status = crate::userland::wait_for_ring3_task(handle);
    *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
    match status {
        // **起こせなかった子は `u64::MAX` で記録されている**（`run_detached_request`）。
        // **`-errno` の範囲と紛れない値にする**——**`-EIO` へ写す。**
        crate::userland::ChildStatus::Ended(u64::MAX) => (-EIO) as u64,
        crate::userland::ChildStatus::Ended(bits) => bits,
        crate::userland::ChildStatus::NoSuchChild => (-ECHILD) as u64,
    }
}

unsafe fn spawn_from_ring3(
    path: u64,
    argv: u64,
    envp: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    // **パスと `argv` を写す。BKL を保持したままである。**
    // **ユーザーメモリへ触るのはここだけで、区間ごと BKL の内側に残す**
    // （`copy_from_user` の TOCTOU の注記）。
    let mut buf = [0u8; PATH_MAX];
    // SAFETY: 呼び出し元契約をそのまま渡す。
    let len = match unsafe { copy_user_path(&mut buf, path, pml4_phys, direct_map) } {
        Ok(len) => len,
        Err(errno) => return (-errno) as u64,
    };
    let mut argv_bytes = [0u8; MAX_ARGV_BYTES];
    // SAFETY: 呼び出し元契約をそのまま渡す。
    let (argv_count, argv_used) = match unsafe {
        copy_user_string_array(
            &mut argv_bytes,
            argv,
            crate::userland::MAX_ARGV,
            pml4_phys,
            direct_map,
        )
    } {
        Ok(pair) => pair,
        Err(errno) => return (-errno) as u64,
    };

    // **`envp` も同じ形で写す（f-2。`ADR-0053` の Decision 2）。**
    //
    // **NULL は `-EFAULT` である**——`argv` と同じ規則を使う。**「環境が無い」は
    // 空の配列（先頭が NULL）で表す。** **新しい規則を作らない。**
    let mut envp_bytes = [0u8; MAX_ENVP_BYTES];
    // SAFETY: 呼び出し元契約をそのまま渡す。
    let (envp_count, envp_used) = match unsafe {
        copy_user_string_array(
            &mut envp_bytes,
            envp,
            crate::userland::MAX_ENVP,
            pml4_phys,
            direct_map,
        )
    } {
        Ok(pair) => pair,
        Err(errno) => return (-errno) as u64,
    };

    // **ここで解く。** 取り直すのは子が終わってからである。
    drop(bkl.take());
    let result = crate::userland::spawn(
        &buf[..len],
        &argv_bytes[..argv_used],
        argv_count,
        Some((&envp_bytes[..envp_used], envp_count)),
    );
    *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));

    match result {
        Ok(crate::userland::SpawnOutcome::Exited(status)) => status & 0xFF,
        Ok(crate::userland::SpawnOutcome::Folded(vector)) => SPAWN_FOLDED_FLAG | (vector << 9),
        Ok(crate::userland::SpawnOutcome::Interrupted) => SPAWN_INTERRUPTED_FLAG,
        Err(error) => {
            // 破壊 (S11-5, spawn-eagain-as-enosys): 深さで断ったことを
            // `-ENOSYS` として返す。**どちらも「できない」を意味するので、
            // 雑に見ると同じに見える。** `-ENOSYS` は「その番号は無い」で、
            // `-EAGAIN` は「その番号は在るが、今は受け付けられない」である。
            // **上限が効いていることを主張しているのは後者だけである。**
            #[cfg(feature = "spawn-eagain-as-enosys")]
            if matches!(error, crate::userland::SpawnError::TooDeep) {
                return (-ENOSYS) as u64;
            }
            (-errno_for_spawn(error)) as u64
        }
    }
}

/// `zaytos_syscall_common` から `extern "sysv64"` で呼ばれる。**[`SYS_EXIT`] 以外は戻る。**
///
/// # exit は出口を通らない（S9-b-3-1）
///
/// [`SYS_EXIT`] を受けたときだけ、`ring3::leave_ring3`（longjmp）で
/// `ring3::enter` の呼び出し元へ帰る。**この関数の末尾を通らない。**
///
/// **したがって BKL の解放を `Drop` に任せられない。** longjmp は `Drop` を
/// 走らせないので、**取ったまま出て二度と解かれない。** 分岐の中で明示的に
/// `drop` する。**BKL を取る入口に、出口を通らない経路ができたのはここが初めて
/// である**（`bkl.rs` の [`crate::bkl::NON_ACQUIRING_ENTRIES`] の隣の注記）。
///
/// 番号（RAX）と 6 引数（RDI/RSI/RDX/R10/R8/R9）を読み、記録し、ディスパッチして、
/// 戻り値を `context.rax` へ書き戻し、復元経路が使う RSP を返す。M5-f-1 は切り替え
/// ないので入場時の `IrqContext` 先頭をそのまま返す（`irq_entry` の no-switch と
/// 同じ）。復元経路が `pop rax` で `context.rax` を復元するので、書き戻した戻り値が
/// ユーザーの RAX に入る。
///
/// **出力しない。** 例外・IRQ ハンドラと同じく、ここでは共有状態の更新だけを行う。
/// 観測は畳んで戻った後にカーネルが記録越しに行う。
///
/// # Safety
///
/// `context` はスタブが積んだ有効な [`IrqContext`] を指していること。
/// `rsp_at_call` はスタブが `call` 直前に読んだ RSP であること。
pub(crate) fn syscall_entry(context: *mut IrqContext, rsp_at_call: u64) -> u64 {
    // **BKL を取る（S4-b-2）。** 割り込みゲート経由なので入場時点で IF=0 だが、
    // BKL の保持区間であることを型で表すためにガードを取る。
    //
    // **`Option` にしてあるのは、出口以外で手放す経路が 2 つあるからである**
    // ——[`SYS_EXIT`]（longjmp で出ていくので `Drop` が走らない）と
    // [`SYS_SPAWN`]（Ring 3 へ降りている間は保持しない。`ADR-0023` §1）。
    // **破壊（B-d）**——**カーネルへ入った時点で FP の状態を塗る。**
    // **`ADR-0058` の Decision 2（カーネルは FP を壊さない）の反証である。**
    //
    // **割り込みの入口にも同じものが在る**（`idt::irq_entry`）。**2 つとも要る**
    // ——**こちらは「必ず入る」側**（描画の途中で `malloc` が `brk` を呼ぶ）、
    // **あちらは「レジスタが生きているところへ入る」側**である。
    #[cfg(feature = "fp-clobber-on-kernel-entry-test")]
    crate::fp::clobber_on_kernel_entry();

    let mut bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));

    // カーネルへ入ったので「今 Ring 3 にいる」を降ろす（S8-b）。Ring 3 へ返る直前で
    // 立て直す。降ろす前の値を記録しておき、往復の検証で突き合わせる（Ring 3 から
    // 来たのなら真のはず）。
    // **今のタスクのシステムコール側の状態を 1 回だけ引く（W1-c-3）。** **引く箇所ごとに
    // `state()` を呼ぶと、`dev` では呼んだ箇所の数だけ一時値が枠を広げた**（実測。この関数の枠が
    // 408 から 616 バイトになった）。
    let state = state();
    state
        .in_ring3_at_entry
        .store(crate::ring3::note_kernel_entry(), Ordering::SeqCst);

    // SAFETY: スタブが直前に積んだ有効な IrqContext を指す。読み書きともこの
    // フレームに限る。
    let ctx = unsafe { &mut *context };

    // 既存の境界計算が syscall 経路でも正しいことの裏取り（IRQ と同じ検査）。
    crate::idt::check_stack_alignment(rsp_at_call, "syscall", ctx.vector);

    // 番号は RAX。**書き戻しの前に読む。**
    let number = ctx.rax;

    // 第 4 引数は R10（RCX ではない。ADR-0020）。
    // 破壊 (M5-f-1-2, arg4-rcx): 第 4 引数を RCX から読む。記録した第 4 引数が
    // PROBE_ARGS[3] と食い違い、R10 規約であることが実証される。
    #[cfg(not(feature = "syscall-test-arg4-rcx"))]
    let arg3 = ctx.r10;
    #[cfg(feature = "syscall-test-arg4-rcx")]
    let arg3 = ctx.rcx;
    let args = [ctx.rdi, ctx.rsi, ctx.rdx, arg3, ctx.r8, ctx.r9];

    state.invocation_count.fetch_add(1, Ordering::SeqCst);
    state.last_number.store(number, Ordering::SeqCst);
    for (slot, value) in state.last_args.iter().zip(args.iter()) {
        slot.store(*value, Ordering::SeqCst);
    }
    state.handler_rsp.store(rsp_at_call, Ordering::SeqCst);

    // ポインタ検証のため、稼働中テーブルの PML4 物理と登録 direct map を用意する。
    let direct_map = common::addr::direct_map();
    // SAFETY: CR3 を読んで現在のテーブルを構築するだけ（読み取り）。IF=0 の単一文脈。
    let pml4_phys =
        unsafe { crate::paging::active::ActivePageTable::current(direct_map) }.pml4_phys();

    // **[`SYS_SPAWN`] だけは、この関数が持つ。** BKL を解いてから入る必要があり、
    // ガードはここのローカルである（[`spawn_from_ring3`]）。
    //
    // SAFETY: pml4_phys / direct_map は稼働中テーブルのもので、walk の契約を満たす。
    let ret = if number == SYS_SPAWN {
        // SAFETY: 同上。`bkl` はいま保持しているガードである。
        unsafe { spawn_from_ring3(args[0], args[1], args[2], pml4_phys, direct_map, &mut bkl) }
    } else if number == SYS_SPAWN_DETACHED {
        // **`spawn_from_ring3` と同じ理由で `dispatch` の外に置く**——**写しの枠（2.3 KiB）を
        // `dispatch` の枠に乗せない。**
        // SAFETY: 同上。
        unsafe {
            spawn_detached_from_ring3(
                args[0], args[1], args[2], args[3], pml4_phys, direct_map, &mut bkl,
            )
        }
    } else if number == SYS_SPAWN_WITH_PIPED_STDIN {
        // SAFETY: 同上。
        unsafe {
            spawn_with_piped_stdin_from_ring3(
                args[0], args[1], args[2], pml4_phys, direct_map, &mut bkl,
            )
        }
    } else if number == SYS_WAIT_CHILD {
        wait_child_from_ring3(args[0], &mut bkl)
    } else {
        // SAFETY: pml4_phys / direct_map は稼働中テーブルのもので、walk の契約を満たす。
        unsafe { dispatch(number, &args, pml4_phys, direct_map, &mut bkl) }
    };

    // **exit だけは Ring 3 へ返らない。**
    //
    // 破壊 (S9-b-3-1, user-exit-ignored): 終了させずに Ring 3 へ返す。プロセスは
    // `exit` の直後に置いた `ud2` へ落ち、ベクタ 6 の畳みとして現れる。
    #[cfg(not(feature = "user-exit-ignored"))]
    if number == SYS_EXIT {
        // **BKL は自分で解く。** 下の `leave_ring3` は longjmp で、`Drop` を
        // 走らせない。**取ったまま戻ると、二度と解かれない。**
        //
        // 破壊 (S9-b-3-1, user-exit-keep-bkl): 解かずに戻る。次に BKL を取る者
        // （空間を畳む側）が、同じコアの再取得として捕まえる。
        #[cfg(not(feature = "user-exit-keep-bkl"))]
        drop(bkl.take());
        // SAFETY: Ring 3 から `int 0x80` で入った文脈で、RECOVERY は
        // `ring3::enter` が保存済みである。BKL は上で解いてある。
        unsafe { crate::ring3::leave_ring3() }
    }

    // 戻り値を RAX へ書き戻す。復元経路の pop rax がこれをユーザー RAX へ載せる。
    // 破壊 (M5-f-1-2, drop-retval): 書き戻しを落とす。ctx.rax は番号のままで、
    // ユーザーは期待した戻り値を受け取れない。
    #[cfg(not(feature = "syscall-test-drop-retval"))]
    {
        ctx.rax = ret;
    }
    #[cfg(feature = "syscall-test-drop-retval")]
    let _ = ret;

    // Ring 3 へ返る（stub の復元経路が iretq する）。立て直す（S8-b）。
    // 立て直してから実際に iretq するまでは Ring 0 なのに真だが、畳みの判定は
    // CS.RPL=0 を弾くので届かない（ring3.rs の IN_RING3 の doc）。
    crate::ring3::note_return_to_ring3();

    // M5-f-1 は切り替えない。入場時の IrqContext 先頭を返す。
    context as u64
}

/// `read(fd, buf, count)` の本体（S10-b）。
///
/// # `i_size` の手前で止まる
///
/// **返すのは要求された長さではなく、実際に写した長さである。**
/// 残り（`i_size` - 位置）より多くは写さず、**末尾に達していれば 0 を返す**
/// （Linux と同じ EOF の表し方）。
///
/// # 線2 がここでも当たる
///
/// - **位置 + 長さ**——`count` は Ring 3 から来るので `u64::MAX` でもよい。
///   **残りとの `min` を先に取る**ので、加算そのものが起きない
/// - **`i_size` - 位置**——[`crate::vfs::File`] が位置を `i_size` で飽和させて
///   いるので桁借りしない。**あちらの不変条件をここが使っている**
/// - **ブロック内のオフセット**——`pos % block_size` はブロック長未満で、
///   `block.len()` との差は飽和引き算で出す
///
/// # 借りたバイト列から写す
///
/// `common::ext2::Ext2::file_block` が返すのは**像を借りたバイト列**である。
/// **位置から必要な範囲を切り出して写す**ので、カーネル側に中継のバッファは要らない。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
/// `read(0)` が待った回数（W2-c-2 の計器）。
static KEYBOARD_WAITS: AtomicU64 = AtomicU64::new(0);

/// 起こされたが読めなかった回数（W2-c-2 の計器）。**空振りの起床である。**
///
/// **0 でなくてよい**——**離鍵のように、積まれてもバイトにならない合図が在る**（`ADR-0061`）。
static EMPTY_WAKES: AtomicU64 = AtomicU64::new(0);

/// 安全網に当たった回数（W2-c-2）。**止めない。数えるだけである。**
///
/// **本番でも 0 でないことがある**——**人が席を外せば当たる。** **だから判定に使わない**
/// （`ADR-0061`。時間の判定を避ける）。**計器の行に出すだけである。**
static SLOW_WAITS: AtomicU64 = AtomicU64::new(0);

/// 安全網の上限（W2-c-2。`ADR-0061`）。**6,000 ティック = 60 秒**（100Hz。実測の
/// `pit::TARGET_FREQUENCY_HZ`）。
///
/// # 止めない
///
/// **当たっても待ち直す。** **本番のキー待ちに「止まる上限」を置くと、人が席を外しただけで
/// 落ちる。** **「上限の無い待ちを書かない」は道具と検査の規律である**（`CLAUDE.md`）。
///
/// # これは主たる検出ではない
///
/// **起こす経路が壊れたことは、関係で見る**——**`keyboard::pushed_without_waking` が、
/// 待っている者が居たのに起こさなかった回数を数える。** **1 回目の打鍵で出るので、
/// 時間を待つ必要が無い。** **こちらは念のための網である。**
// 破壊 `read-never-waits` では待たないので、上限も待つ関数も読まれない。
#[cfg_attr(feature = "read-never-waits", allow(dead_code))]
const SLOW_WAIT_TICKS: u64 = 6_000;

/// `read(0)` が待った回数（W2-c-2）。
pub fn keyboard_waits() -> u64 {
    KEYBOARD_WAITS.load(Ordering::Relaxed)
}

/// 起こされたが読めなかった回数（W2-c-2）。
pub fn empty_wakes() -> u64 {
    EMPTY_WAKES.load(Ordering::Relaxed)
}

/// 安全網に当たった回数（W2-c-2）。**判定には使わない**（計器である）。
pub fn slow_waits() -> u64 {
    SLOW_WAITS.load(Ordering::Relaxed)
}

// **「セッションの回数」を返す関数は置かない（W2-c-2 で測って消した）。**
//
// **一度置いたが、間違った量を返していた**——**`invocation_count` はスロットの記録で、
// `spawn` が子の後に親のものへ戻す。** **`init` がシェルの後に読むと、シェルが走る前の
// 残りが出る**（実測で 76 と出た。同じ回のシェルの実数は 2,969 である）。
//
// **判定が読むべき数は、既に在る行が持っている**——**`spawn: /bin/zash ended ... after N
// syscall(s)` は、親のものへ戻す前に読んでいる。** **新しい計器を足さず、あの行を読む。**

/// 端末のバイトが来るまで待つ（W2-c-2。`ADR-0061`）。**起こされたら `true` を返す。**
///
/// **前景を失っていたら `false` を返す**——**呼び出し側は `-EBADF` を返す。**
/// **待ち続けない**（前景を持たない者は読めない。決定 1）。
///
/// # 窓は構造で閉じている
///
/// **呼ばれるのは IF=0 の文脈である**（`int 0x80` は割り込みゲート）。**BKL を解いても
/// IF は戻らない**（`EntryInterruptGuard` は保存した RFLAGS が IF=1 のときだけ戻す。実測）
/// ——**だから「欄を `Waiting` にしてから譲る」までに合図は入らない。**
///
/// # BKL を解いてから譲る
///
/// **`ADR-0036` の「保持したまま眠らない・待たない」に従う。** **起きたら取り直す。**
// 破壊 `read-never-waits` では呼ばれない（あちらは `-EAGAIN` を返して回る）。
#[cfg_attr(feature = "read-never-waits", allow(dead_code))]
fn wait_for_keyboard(bkl: &mut Option<crate::bkl::BklGuard>) -> bool {
    KEYBOARD_WAITS.fetch_add(1, Ordering::Relaxed);
    let since = crate::idt::timer_ticks();

    // **欄を `Waiting` にする。** **ここは IF=0 で、まだ BKL を持っている。**
    crate::task::set_current_waiting(crate::task::Wait::Keyboard);
    // **BKL を解く。** 保持したまま譲ると、次に走るタスクがカーネルへ入れない。
    drop(bkl.take());
    // **譲る。** `pick_next` は待っている者を飛ばし、走れる者が居なければ BSP 用アイドルへ
    // 落ちる（W2-c-1 で置いた）。**起こされるまでここへは戻らない。**
    crate::task::yield_now();
    // **起きた。BKL を取り直す。**
    *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));

    // **安全網（`ADR-0061`）。止めない。数えるだけである。**
    //
    // **行を出さない形にした（W2-c-2）。** **`syscall.rs` にシリアルの口は無く、開けると
    // 直接シリアルの許可リストに項目が増える**（`xtask` の `DIRECT_SERIAL_PORT_ALLOWLIST`）。
    // **報せる先は既存の計器の行でよい**——**`init` がセッションの後に出す行がこの数を読む。**
    // **そもそも主たる検出は関係のほうである**（`keyboard::pushed_without_waking`）。
    let waited = crate::idt::timer_ticks().saturating_sub(since);
    if waited > SLOW_WAIT_TICKS {
        SLOW_WAITS.fetch_add(1, Ordering::Relaxed);
    }

    // **前景を持っていなければ、もう読めない。**
    crate::input::foreground_is_claimed()
}

unsafe fn sys_read(
    fd: u64,
    buf: u64,
    count: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    // **溜まっている描画を送る（ADR-0047）。**
    //
    // **ここが「Ring 3 が入力を待つ側へ回る直前」である。** **返す値が入力でも
    // `-EAGAIN` でも掃く**——**溜まっていなければ `flush` は何もしない**ので、
    // `-EAGAIN` で回り続ける形でも費用は増えない。
    //
    // **BKL を解いてから呼ぶ**（`crate::console::flush_foreground` の doc。
    // **全面転送は 5.05M サイクル掛かる**——実測）。
    //
    // **端末でない fd でも掃く。** **判定に使うのは「読む側へ回った」ことだけで、
    // どの fd から読むかではない**——**ファイルを読む前に画面が古いままである
    // 理由も無い。**
    //
    // 破壊 (PERF-a, read-skip-flush-test): ここで送らない。**溜めたまま
    // 入力を待つ**ので、**画面が古いまま止まる**——**次に誰かが送るまで
    // 出ない。** **画面を読む判定が軒並み落ちる。**
    #[cfg(not(feature = "read-skip-flush-test"))]
    if crate::console::foreground_installed() {
        drop(bkl.take());
        crate::console::flush_foreground();
        *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
    }

    // **パイプの読み端（`ADR-0063` の (b3)）。** **表の中身で分岐する**（端末と同じ形）。
    let pipe_read = crate::vfs::with_current_files(|files| {
        files
            .get(fd as usize)
            .ok()
            .and_then(|file| file.pipe_read_end())
    });
    if let Some(pipe) = pipe_read {
        // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
        return unsafe { read_from_pipe(pipe, buf, count, pml4_phys, direct_map, bkl) };
    }

    // **ソケット（`ADR-0064`）。** **繋がっていなければ `-ENOTCONN`。**
    let socket = crate::vfs::with_current_files(|files| {
        files
            .get(fd as usize)
            .ok()
            .and_then(|file| file.socket_state())
    });
    match socket {
        Some(crate::vfs::SocketState::Stream { conn, side }) => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            return unsafe { read_from_socket(conn, side, buf, count, pml4_phys, direct_map, bkl) };
        }
        Some(_) => return (-ENOTCONN) as u64,
        None => {}
    }

    // **入力の生イベントの fd（Y-a。`ADR-0066`）。** **表の中身で分岐する**——**端末（inode が
    // None）と同じ枝へ落ちる前に分ける。** **`read` は `struct input_event` を返す。**
    let is_input = crate::vfs::with_current_files(|files| {
        files
            .get(fd as usize)
            .ok()
            .map(|file| file.is_input())
            .unwrap_or(false)
    });
    if is_input {
        // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
        return unsafe { read_input_events(buf, count, pml4_phys, direct_map, bkl) };
    }
    // **画面の fd は読めない（`ADR-0066` の Y-c）。** **入力 fd と同じで inode が None なので、
    // 分けないと端末の枝へ落ちて打鍵を読んでしまう。** **Linux の fbdev は `read` で画素を返すが、
    // 合わせなかった**——**画素は `mmap` で読める**ので、2 つ目の道を持たない。
    let is_screen = crate::vfs::with_current_files(|files| {
        files
            .get(fd as usize)
            .ok()
            .map(|file| file.is_screen())
            .unwrap_or(false)
    });
    if is_screen {
        return (-EINVAL) as u64;
    }

    // **表を握る区間を短くする。** ここでは inode と位置の写しだけを取り、
    // 検証とブロックの読み出しは外で行う（`Locked` は割り込みを禁止する）。
    let opened = crate::vfs::with_current_files(|files| {
        files.get(fd as usize).map(|file| {
            file.inode()
                .map(|inode| (*inode, file.offset(), file.is_writable_file()))
        })
    });
    let (inode, offset, writable) = match opened {
        // **端末である（S11-10）。** リングから取れるだけ取る。
        Ok(None) => {
            // **前景を持っていなければ読めない。** 持ち主は 1 人である
            // （`crate::input` の不変条件）。**遠征の前に取ってある**ので、
            // ここへ来る時点では持っている。
            if !crate::input::foreground_is_claimed() {
                return (-EBADF) as u64;
            }
            if count == 0 {
                return 0;
            }
            // **踏み込む前に検証する。**
            let want = count.min(TERMINAL_READ_MAX as u64);
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, buf, want) })
            else {
                return (-EFAULT) as u64;
            };
            let mut kbuf = [0u8; TERMINAL_READ_MAX];
            // **溜まっていなければ待つ（W2-c-2。`ADR-0061`）。**
            //
            // **以前は `-EAGAIN` を返し、シェルが `continue` で回していた**
            // ——**1 セッションで 1,377,679 回のシステムコールを出していた**（実測。W2-c-1）。
            //
            // # 窓は構造で閉じている
            //
            // **`int 0x80` は割り込みゲートなので、ここは IF=0 である。** **BKL を解いても
            // IF は戻らない**（`EntryInterruptGuard` は保存した RFLAGS が IF=1 のときだけ戻す。実測）。
            // **したがって「空だと見てから `Waiting` にする」までに合図は入らない。**
            //
            // # BKL は解いてから譲る
            //
            // **`ADR-0036` の「保持したまま眠らない・待たない」に従う。** **`sys_read` は
            // ガードを引数で受け取っているので、`take()` で落とすだけでよい**——**新しい配管は要らない**
            // （`SYS_SPAWN` と virtio の待ちと同じ踊りである）。
            // **この `read(0)` が既に待ったか（W2-c-2）。** **局所で持つ。**
            //
            // **大域の「誰かが待ったことがあるか」では数えられない**——**それだと、次の
            // `read(0)` の 1 周目（まだ待っていない空振り）を空振りの起床として数えてしまう。**
            // **数えたいのは「起こされたのに読めなかった」であって、「空だった」ではない。**
            // 破壊 `read-never-waits` では待たないので、書き換わらない。
            #[cfg_attr(feature = "read-never-waits", allow(unused_mut))]
            let mut waited_once = false;
            let got = loop {
                let got = crate::input::read_bytes(&mut kbuf[..want as usize]);
                if got != 0 {
                    break got;
                }
                // **起こされたのに読めなかった回数（W2-c-2 の判定 5）。**
                //
                // **離鍵のように、積まれてもバイトにならない合図で起きた回数である。**
                // **0 でなくてよい**（`ADR-0061`）。
                if waited_once {
                    EMPTY_WAKES.fetch_add(1, Ordering::Relaxed);
                }
                // **待つのは、対話の口が据えられている間だけである（W2-c-2 で測って狭めた）。**
                //
                // **`ADR-0061` の決定 1 は「待てるのは前景の持ち主だけ」と書いていたが、それでは
                // 足りなかった**——**`run_loaded_program` はどのプログラムにも前景を取らせるので、
                // 起動シーケンスの `syscall-test` も持ち主である。**
                // **あれは `read(0)` が `-EAGAIN` を返すことを主張している**（失敗コード 51 と 52）
                // ——**打鍵が無いのだから、それが正しい答えである。**
                // **実測で踏んだ**——**無条件に待つ形にしたら、起動がそこで止まり、
                // シェルまで届かなかった**（`docs/troubleshooting.md`）。
                //
                // **コンソールの前景が据えられているのは、`init` がシェルを起こす区間だけである**
                // （`console::install_foreground`）。**そこだけが「誰かが打つ」場所である。**
                if !crate::console::foreground_installed() {
                    return (-EAGAIN) as u64;
                }
                // **台本が入力を駆動している間も待たない（W2-c-2 で踏んで足した）。**
                //
                // **台本が 0 を返す場面は 3 つある**——**出し切った・休み・作動前**。
                // **どれも「もう入力は無い」であって、`-EAGAIN` がその答えだった**
                // （`crate::input::script_drives_input` の doc）。
                // **待つ形にしたら、誰も打たないので待ちが終わらず、`--full` が上限に
                // 当たった**（実測。台本の族 6 項目が落ちた。`docs/troubleshooting.md`）。
                //
                // **待ちを見るのは、本物の打鍵を使う `--shell-test` の族だけである**
                // （`docs/verification-coverage.md` の「待ちの経路を通る項目」）。
                if crate::input::script_drives_input() {
                    return (-EAGAIN) as u64;
                }
                // 破壊 (W2-c-2, read-never-waits): 待たずに `-EAGAIN` を返す。**回して待つ形へ戻る**
                // ——**判定 1（回さずに待つ）が落ちる。**
                #[cfg(feature = "read-never-waits")]
                return (-EAGAIN) as u64;
                #[cfg(not(feature = "read-never-waits"))]
                {
                    if wait_for_keyboard(bkl) {
                        // **次の周で空振りだったら数える**（上の `waited_once`）。
                        waited_once = true;
                        continue;
                    }
                    // **前景を失った**（待っている間に取り上げられた）。**待ち続けない。**
                    return (-EBADF) as u64;
                }
            };
            // SAFETY: slice は検証済みで、`got` は `want` を越えない。
            let written = unsafe { copy_to_user(&slice, 0, &kbuf[..got]) };
            return written as u64;
        }
        Ok(Some(pair)) => pair,
        Err(e) => return (-errno_for_file_table(e)) as u64,
    };
    // **書きで開いた fd への read は -EBADF である**（zi-c。ADR-0037。
    // 読みで開いた fd への write と対称——fd の向きの取り違えは両方向とも
    // -EBADF）。inode の写しは open 時点の大きさのままで、切った後の実寸とも
    // 食い違う——**読ませない理由は形（Linux の向きの規約）と実装（古い
    // i_size で読むと切る前の長さを信じる）の両方にある。**
    if writable {
        return (-EBADF) as u64;
    }
    // **ディレクトリは `read` で読めない。** 中身は `getdents64` で返す形である
    // （Linux も同じで、`read(2)` は `EISDIR` を返す）。
    //
    // 破壊 (S10-b, eisdir-as-enotdir): 対応表を 1 つ取り違え、`-ENOTDIR` を返す。
    // **どちらも「種別が違う」を意味するので、雑に見ると同じに見える。**
    // Linux は分けている——`read` がディレクトリに当たったら `EISDIR`、
    // パスの途中がディレクトリでなければ `ENOTDIR` である。
    // **syscall-test の検算が食い違いを捕まえる。**
    if inode.is_directory() {
        #[cfg(not(feature = "syscall-test-eisdir-as-enotdir"))]
        let errno = EISDIR;
        #[cfg(feature = "syscall-test-eisdir-as-enotdir")]
        let errno = ENOTDIR;
        return (-errno) as u64;
    }

    // 線2: 位置は `i_size` を越えない（[`crate::vfs::File`] の不変条件）。
    let want = count.min(inode.size() - offset);
    if want == 0 {
        // 末尾に達しているか、0 バイト要求された。**どちらも 0 である。**
        return 0;
    }
    // **踏み込む前に検証する。** 写す長さは `want` で確定している。
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, buf, want) }) else {
        return (-EFAULT) as u64;
    };

    let fs = match crate::vfs::root_filesystem() {
        Ok(fs) => fs,
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };
    let block_size = u64::from(fs.block_size());

    let mut done = 0u64;
    while done < want {
        let pos = offset + done;
        let Ok(index) = u32::try_from(pos / block_size) else {
            return (-EIO) as u64;
        };
        let within = (pos % block_size) as usize;
        let block = match fs.file_block(inode.ext2(), index) {
            Ok(bytes) => bytes,
            Err(e) => return (-errno_for_ext2(e)) as u64,
        };
        // 線2: 最後のブロックは `i_size` で切られているので、`within` が
        // その長さを越えることがある。**飽和で引く。**
        let available = block.len().saturating_sub(within);
        if available == 0 {
            // 進めない。**`i_size` と実際のブロックが食い違っている像である。**
            return (-EIO) as u64;
        }
        let chunk = (want - done).min(available as u64) as usize;
        // SAFETY: slice は検証済み。`done + chunk` は `want` を越えない。
        let written = unsafe { copy_to_user(&slice, done, &block[within..within + chunk]) };
        if written == 0 {
            return (-EFAULT) as u64;
        }
        done += written as u64;
    }

    // **位置を進めるのは、写し終えた後である。** 途中で失敗したら進めない
    // （呼び出し側から見て「読めなかったぶんは読めていない」）。
    //
    // 破壊 (S10-b, read-no-advance): 位置を進めない。**1 回だけ読むぶんには
    // 正しく見える**——短く読んでから続きを読む検算だけが食い違う。
    #[cfg(not(feature = "syscall-test-read-no-advance"))]
    crate::vfs::with_current_files(|files| {
        if let Ok(file) = files.get_mut(fd as usize) {
            file.advance(done);
        }
    });
    done
}

/// `open(path, flags, mode)` の本体（S10-b）。
///
/// # 順序に意味がある
///
/// **フラグを先に見る。** 書き込みで開かれたなら、**パスを読む前に `-EROFS` である**
/// ——読み取り専用のファイルシステムに対して、そのパスが在るかどうかは答えるべき
/// ことではない。
///
/// # パスはユーザー空間から来る
///
/// **`UserSlice` と窓がそのまま効く**（S9-b-3-2b で 1 つに畳んだ窓）。
/// NUL 終端なので長さが先に分からないが、**ページ単位で検証しながら進む**ので、
/// **踏み込む前に検証するという契約は崩れない。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
/// `O_CREAT` の本体（e-5。ADR-0037 の Addendum）。
///
/// # 親と名前へ割る
///
/// **最後の `/` で割る。** `/data/fresh` なら親が `/data`、名前が `fresh` である。
/// **`/` で終わる形と、名前が空の形は断る**（`-EINVAL`）。
///
/// # 既に在る名前はここへ来ない
///
/// **呼ぶ側が `lookup` の `NotFound` でだけ入る。** **`O_EXCL` は受けない**
/// ——**「在ったら失敗する」を要求する利用者がいない**（ADR-0037 の Addendum）。
///
/// # 作った後にもう一度引く
///
/// **`create_file` は inode 番号を返すが、開く側が要るのは [`common::ext2::Inode`]
/// である。** **像を書き換えた後に引き直す**ので、**作った結果そのものを見る**
/// ——**書けたつもりで引けない形が、ここで落ちる。**
fn create_and_lookup(path: &[u8]) -> Result<common::ext2::Inode, i64> {
    let (parent, name) = split_parent_and_name(path)?;

    let layout = crate::vfs::root_filesystem()
        .map_err(errno_for_ext2)?
        .layout();
    let dir = crate::vfs::root_filesystem()
        .map_err(errno_for_ext2)?
        .lookup(parent)
        .map_err(errno_for_ext2)?;
    if !dir.is_directory() {
        return Err(ENOTDIR);
    }

    // 破壊 (e-5, open-ignore-create-test): O_CREAT を受けても作らない。
    // **戻り値は「無い」のままなので、開く側から見ると受理していないのと
    // 同じである**——**新しいファイルが作れることの判定だけが落ちる。**
    #[cfg(feature = "open-ignore-create-test")]
    return Err(ENOENT);

    #[cfg(not(feature = "open-ignore-create-test"))]
    {
        let created = crate::vfs::with_root_image_mut(|image| {
            common::ext2::create_file(image, &layout, dir.number, name)
        })
        .ok_or(EIO)?;
        created.map_err(errno_for_alloc)?;

        crate::vfs::root_filesystem()
            .map_err(errno_for_ext2)?
            .lookup(path)
            .map_err(errno_for_ext2)
    }
}

/// パスを親と名前へ割る（e-5 で `create_and_lookup` に在ったものを DIR-1b で切り出した）。
///
/// # 最後の `/` で割る
///
/// `/data/fresh` なら親が `/data`、名前が `fresh` である。
/// **`/` で終わる形と、名前が空の形は断る**（`-EINVAL`）。
/// **`/` を含まない形も断る**——**カレントディレクトリが無いので、
/// 親を決める手段が無い**（`docs/foundation-inventory.md`）。
///
/// # 3 つが同じ割りを使う
///
/// `O_CREAT` の `open`・`unlink`・`rmdir` である。**同じ規則で割らないと、
/// 作れるが消せない名前が生じうる。**
fn split_parent_and_name(path: &[u8]) -> Result<(&[u8], &[u8]), i64> {
    let split = path.iter().rposition(|byte| *byte == b'/').ok_or(EINVAL)?;
    let (parent, name) = path.split_at(split);
    let name = &name[1..];
    if name.is_empty() {
        return Err(EINVAL);
    }
    // **親が `/` だけのときは、そのまま `/` を渡す。**
    let parent: &[u8] = if parent.is_empty() { b"/" } else { parent };
    Ok((parent, name))
}

/// [`common::ext2::AllocError`] を errno へ写す（e-5）。
///
/// **空きが尽きた形はすべて `-ENOSPC` である**——**inode でもブロックでも
/// ディレクトリの隙間でも、使う側にできることは同じ（消して空ける）である。**
fn errno_for_alloc(error: common::ext2::AllocError) -> i64 {
    use common::ext2::AllocError;
    match error {
        AllocError::Full | AllocError::NoRoomInDirectory => ENOSPC,
        AllocError::NameTaken => EEXIST,
        AllocError::BadName => EINVAL,
        // **その名前は無い**（DIR-1b。`unlink` が使う）。
        AllocError::NoSuchEntry => ENOENT,
        // **ディレクトリだった**（DIR-1b）。**`rm` はこれで「ディレクトリだ」
        // と分かり、`rmdir` を使えと言える。**
        AllocError::NotARegularFile(_) => EISDIR,
        // **ディレクトリでなかった**（DIR-1c。`rmdir` が通常ファイルを見た）。
        AllocError::NotADirectory(_) => ENOTDIR,
        // **空でなかった**（DIR-1c。Linux も `rmdir` にこれを返す）。
        AllocError::DirectoryNotEmpty(_) => ENOTEMPTY,
        // **像の側の食い違いは、使う側の入力では直らない。**
        _ => EIO,
    }
}

/// `brk(addr)` の本体（H-a。ADR-0044）。
///
/// # 上げれば写す。下げれば外して返す
///
/// **ページ単位で動く。** **要求は 1 バイト単位で受けるが、
/// 写すのはページである**（Linux も同じ）。
///
/// # 上限で断る
///
/// **[`crate::userland::HEAP_LIMIT`] を越えたら `-ENOMEM`。**
/// **ガードページは置かない**——**スタックの下端そのものが境界なので、
/// 越えなければ衝突しない**（ADR-0044 の決定 4）。
///
/// # 稼働中の表へ写す
///
/// **遠征の中では CR3 がこのプロセスのものである**
/// （`crate::userland` の `run_loaded_program` が `switch_to` してから入る）。
/// **したがって [`crate::paging::active::ActivePageTable::current`] が
/// 指すのはユーザーの表である。** **新しい経路を作らない**（ADR-0044）。
///
/// # 途中で足りなくなったら、そこまでで止める
///
/// **写せた分は残す。** **`-ENOMEM` を返すが、上端はそこまで進んでいる**
/// ——**巻き戻すと、巻き戻しの途中で失敗したときに何も言えなくなる。**
/// **呼ぶ側は `brk(0)` で確かめられる。**
///
/// # Safety
///
/// `direct_map` が有効で、遠征の中（CR3 がユーザーの表）から呼ばれること。
unsafe fn sys_brk(requested: u64, direct_map: DirectMap) -> u64 {
    use crate::paging::active::{ActivePageTable, PageAttributes};

    let (mapped, current, start) = crate::userland::with_current_heap(|heap| {
        (heap.is_mapped(), heap.break_at(), heap.start())
    });
    if !mapped {
        // **像を読む前には答えられない。** ここへ来るのは異常である。
        return (-ENOMEM) as u64;
    }

    // **0 は問い合わせである。**
    if requested == 0 {
        return current;
    }
    // **像の末尾より下げられない。** **下は像とスタックの外である。**
    if requested < start || requested > crate::userland::HEAP_LIMIT {
        return (-ENOMEM) as u64;
    }

    const PAGE_SIZE: u64 = 4096;
    let want = (requested + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let have = current.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    if want == have {
        crate::userland::with_current_heap(|heap| heap.set_break(requested));
        return requested;
    }

    let Some(allocator) = crate::frame_allocator::take() else {
        return (-ENOMEM) as u64;
    };
    // SAFETY: 遠征の中なので CR3 はこのプロセスの表である。
    let mut table = unsafe { ActivePageTable::current(direct_map) };
    let attributes = PageAttributes {
        user: true,
        writable: true,
        cacheable: true,
        shared: false,
    };

    let mut outcome = requested;
    if want > have {
        // **伸ばす。** 1 ページずつ写す。
        let mut page = have;
        while page < want {
            let Some(frame) = allocator.allocate_frame() else {
                outcome = (-ENOMEM) as u64;
                break;
            };
            let Some(virt) = common::addr::VirtAddr::new(page) else {
                let _ = allocator.deallocate_frame(frame);
                outcome = (-ENOMEM) as u64;
                break;
            };
            // **中身を 0 にしてから写す。** **前の住人の中身をユーザーへ渡さない。**
            // SAFETY: いま取ったフレームで、direct map が覆っている。
            unsafe {
                core::ptr::write_bytes(
                    direct_map.phys_to_virt(frame).as_u64() as *mut u8,
                    0,
                    PAGE_SIZE as usize,
                )
            };
            // SAFETY: 稼働中の表へ、ユーザーの範囲を写す。
            if unsafe { table.map_4kib(virt, frame, attributes, allocator) }.is_err() {
                let _ = allocator.deallocate_frame(frame);
                outcome = (-ENOMEM) as u64;
                break;
            }
            crate::userland::with_current_heap(|heap| heap.note_taken());
            page += PAGE_SIZE;
        }
        // **写せた分までを上端にする**（doc の「そこまでで止める」）。
        let reached = if outcome == requested {
            requested
        } else {
            page
        };
        crate::userland::with_current_heap(|heap| heap.set_break(reached));
    } else {
        // 破壊 (H-a, brk-skip-shrink-test): 下げる要求で外さない。
        // **上端だけ下がり、フレームは返らない。** **`brk(0)` は下がった値を
        // 返すので、使う側からは成功に見える**——**落ちるのは
        // 「伸ばして縮めたら空きフレームの数が元へ戻る」判定だけである。**
        #[cfg(not(feature = "brk-skip-shrink-test"))]
        {
            let mut page = have;
            while page > want {
                page -= PAGE_SIZE;
                if let Some(virt) = common::addr::VirtAddr::new(page) {
                    // SAFETY: 稼働中の表から外し、フレームを返す。
                    if let Ok(frame) = unsafe { table.unmap_4kib(virt) } {
                        // **`unmap_4kib` は物理番地を `u64` で返す。**
                        if let Some(frame) = PhysAddr::new(frame) {
                            let _ = allocator.deallocate_frame(frame);
                            crate::userland::with_current_heap(|heap| heap.note_given());
                        }
                    }
                }
            }
        }
        crate::userland::with_current_heap(|heap| heap.set_break(requested));
    }

    crate::frame_allocator::give_back(allocator);
    outcome
}

/// `lseek(fd, offset, whence)` の本体（DIR-1b）。
///
/// # 受けるのは `SEEK_SET` だけである
///
/// 理由は [`SEEK_SET`] の doc にある。**知らない `whence` は `-EINVAL`。**
///
/// # 末尾より先へ跳んでもよい
///
/// **`File::seek_to` が末尾で止める**（`crate::vfs` の
/// 「オフセットはファイルの末尾を越えない」）。**したがって跳んだ先が
/// 末尾より先なら、読み出しは 0 バイトになる。**
/// **穴あきファイルを作る道にはならない**——**書く側は追記しかできない。**
///
/// # 端末には効かない
///
/// **`fd` が端末なら `-ESPIPE` である**（Linux も同じ）。
/// **位置を持たないものに位置を与えない。**
fn sys_lseek(fd: u64, offset: u64, whence: u64) -> u64 {
    if whence != SEEK_SET {
        return (-EINVAL) as u64;
    }
    crate::vfs::with_current_files(|files| match files.get_mut(fd as usize) {
        Ok(file) => {
            if file.is_terminal() {
                return (-ESPIPE) as u64;
            }
            file.seek_to(offset);
            file.offset()
        }
        Err(e) => (-errno_for_file_table(e)) as u64,
    })
}

/// `unlink(path)` の本体（DIR-1b）。
///
/// # 消せるのは通常ファイルだけである
///
/// **`common::ext2::unlink_file` がディレクトリを断る**
/// （`NotARegularFile`）。**`-EISDIR` へ写す**ので、`rm` は
/// 「ディレクトリだった」と分かる。
///
/// # 開いている fd は気にしない
///
/// **Unix は「消しても、開いている者が閉じるまで中身が生きている」。**
/// **こちらはそうならない**——**inode を即座に返すので、開いたままの fd は
/// 消えた inode を指す。** **同時に走るプロセスが 1 つなので、いまは
/// その形にならない**（`spawn` は同期である）。**`fork` が来たら判断が要る。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn sys_unlink(path: u64, pml4_phys: PhysAddr, direct_map: DirectMap) -> u64 {
    let mut buf = [0u8; PATH_MAX];
    // SAFETY: 呼び出し元契約をそのまま渡す。
    let len = match unsafe { copy_user_path(&mut buf, path, pml4_phys, direct_map) } {
        Ok(len) => len,
        Err(errno) => return (-errno) as u64,
    };

    let (parent, name) = match split_parent_and_name(&buf[..len]) {
        Ok(split) => split,
        Err(errno) => return (-errno) as u64,
    };

    let layout = match crate::vfs::root_filesystem() {
        Ok(fs) => fs.layout(),
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };
    let dir = match crate::vfs::root_filesystem().and_then(|fs| fs.lookup(parent)) {
        Ok(dir) => dir,
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };
    if !dir.is_directory() {
        return (-ENOTDIR) as u64;
    }

    // 破壊 (DIR-1b, unlink-ignore-request-test): 消さずに 0 を返す。
    // **戻り値は成功のままなので、`rm` は何も言わない**——**落ちるのは
    // 「消した後の `ls` に名前が無い」判定だけである。**
    #[cfg(feature = "unlink-ignore-request-test")]
    return 0;

    #[cfg(not(feature = "unlink-ignore-request-test"))]
    {
        let removed = crate::vfs::with_root_image_mut(|image| {
            common::ext2::unlink_file(image, &layout, dir.number, name)
        });
        match removed {
            Some(Ok(())) => 0,
            Some(Err(error)) => (-errno_for_alloc(error)) as u64,
            // 複製前は書けない（埋め込みを可変にしない）。
            None => (-EROFS) as u64,
        }
    }
}

/// [`sys_directory`] がどちらを行うか（DIR-1c）。
enum DirectoryOp {
    /// `mkdir`。
    Create,
    /// `rmdir`。
    Remove,
}

/// `mkdir(path)` と `rmdir(path)` の本体（DIR-1c）。
///
/// # 1 つにまとめてある
///
/// **違うのは `common::ext2` のどちらを呼ぶかだけである。**
/// **パスの写し・親と名前への割り・親がディレクトリであることの確認は同じ**
/// ——**分けると、同じ手順を 2 つ持つことになる。**
///
/// # `mkdir -p` は無い
///
/// **親が無ければ `-ENOENT` である。** **途中を作る形は、
/// 「どこまで作ったか」を戻す判断が要る**（途中で失敗したとき）。
/// **要る者が来てから作る。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn sys_directory(
    path: u64,
    op: DirectoryOp,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> u64 {
    let mut buf = [0u8; PATH_MAX];
    // SAFETY: 呼び出し元契約をそのまま渡す。
    let len = match unsafe { copy_user_path(&mut buf, path, pml4_phys, direct_map) } {
        Ok(len) => len,
        Err(errno) => return (-errno) as u64,
    };

    let (parent, name) = match split_parent_and_name(&buf[..len]) {
        Ok(split) => split,
        Err(errno) => return (-errno) as u64,
    };

    let layout = match crate::vfs::root_filesystem() {
        Ok(fs) => fs.layout(),
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };
    let dir = match crate::vfs::root_filesystem().and_then(|fs| fs.lookup(parent)) {
        Ok(dir) => dir,
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };
    if !dir.is_directory() {
        return (-ENOTDIR) as u64;
    }

    let done = crate::vfs::with_root_image_mut(|image| match op {
        DirectoryOp::Create => {
            common::ext2::create_directory(image, &layout, dir.number, name).map(|_| ())
        }
        DirectoryOp::Remove => common::ext2::remove_directory(image, &layout, dir.number, name),
    });
    match done {
        Some(Ok(())) => 0,
        Some(Err(error)) => (-errno_for_alloc(error)) as u64,
        // 複製前は書けない（埋め込みを可変にしない）。
        None => (-EROFS) as u64,
    }
}

unsafe fn sys_open(path: u64, flags: u64, pml4_phys: PhysAddr, direct_map: DirectMap) -> u64 {
    // **受理は 2 形だけである（zi-c。ADR-0037）**——O_RDONLY と
    // O_WRONLY|O_TRUNC。**それ以外は従来どおり -EROFS**（bare O_WRONLY も
    // 拒む——位置書きの部品が無く、:w の全置換には O_TRUNC の形が対応する。
    // O_CREAT / O_APPEND は「決めないこと」である）。
    // **e-5 で `O_CREAT` が加わった**（ADR-0037 の Addendum）。**受理するのは
    // `O_WRONLY|O_TRUNC` と `O_WRONLY|O_CREAT|O_TRUNC` の 2 形である。**
    // **`O_CREAT` 単独は受けない**——**位置書きの部品が無いので、
    // 作った後にできるのは全置換だけである**（`O_TRUNC` と同じ形になる）。
    let create = flags & O_CREAT != 0;
    let write_intent = flags & O_WRITE_INTENT & !O_CREAT;
    let write_form = flags & O_ACCMODE == O_WRONLY && write_intent == O_TRUNC;
    if !write_form && (flags & O_ACCMODE != O_RDONLY || flags & O_WRITE_INTENT != 0) {
        return (-EROFS) as u64;
    }

    let mut buf = [0u8; PATH_MAX];
    // SAFETY: 呼び出し元契約をそのまま渡す。
    let len = match unsafe { copy_user_path(&mut buf, path, pml4_phys, direct_map) } {
        Ok(len) => len,
        Err(errno) => return (-errno) as u64,
    };

    let fs = match crate::vfs::root_filesystem() {
        Ok(fs) => fs,
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };
    let inode = match fs.lookup(&buf[..len]) {
        Ok(inode) => inode,
        // **無ければ作る（e-5。`O_CREAT`）。** **作るのは書きの形のときだけである。**
        Err(common::ext2::Ext2Error::NotFound) if write_form && create => {
            // SAFETY: この関数は Ring 3 からの入口で、像は BKL の内側にある。
            match create_and_lookup(&buf[..len]) {
                Ok(inode) => inode,
                Err(errno) => return (-errno) as u64,
            }
        }
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };

    let file = if write_form {
        // **既存の通常ファイルだけを書きで開ける。** ディレクトリ等は拒む
        // （Linux の EISDIR に相当する形は要る者が出たら分ける。いまは
        // 「書けない」で足りるので -EROFS に寄せる）。
        if !inode.is_regular_file() {
            return (-EROFS) as u64;
        }
        // **open の時点で長さ 0 へ切る（O_TRUNC の意味）。**
        //
        // 破壊 (zi-c, open-skip-truncate-test): 切らない。**古い中身の後ろへ
        // 追記され、読み戻しが「古い+新しい」の連結になる**——syscall-test の
        // 読み戻しの検算（58 番）が捕まえる。
        #[cfg(not(feature = "open-skip-truncate-test"))]
        {
            let layout = match crate::vfs::root_filesystem() {
                Ok(fs) => fs.layout(),
                Err(e) => return (-errno_for_ext2(e)) as u64,
            };
            // **共有借用はもう生きていない。** `layout` は `Copy` の写しで、
            // `fs` は上の束で落ちている（`common::ext2::Layout` の doc の形）。
            let truncated = crate::vfs::with_root_image_mut(|image| {
                common::ext2::truncate_to(image, &layout, inode.number, 0)
            });
            match truncated {
                Some(Ok(())) => {}
                Some(Err(_)) => return (-EIO) as u64,
                // 複製前は書けない（埋め込みを可変にしない）。
                None => return (-EROFS) as u64,
            }
        }
        crate::vfs::File::writable(crate::vfs::Inode::from_ext2(inode))
    } else {
        crate::vfs::File::new(crate::vfs::Inode::from_ext2(inode))
    };
    crate::vfs::with_current_files(|files| match files.insert(file) {
        Ok(fd) => fd as u64,
        Err(e) => (-errno_for_file_table(e)) as u64,
    })
}

/// ext2 の `file_type` を `getdents64` の `d_type` へ写す（S10-b）。
///
/// # 値が違う
///
/// **ext2 は 1=REG・2=DIR、`d_type` は 8=REG・4=DIR である。**
/// **番号が別の体系なので、写すのではなく引き当てる。** Linux も同じことを
/// している（`fs_ftype_to_dtype`）。
///
/// # 表に無い値は [`DT_UNKNOWN`] である
///
/// **`d_type` は「分からない」を表せる**ので、知らない種別は 0 で返す。
/// **symlink（ext2 の 7）は載せていない**——**この値を実測で確かめていない**
/// （像に symlink が無く、`ext2fs` のヘッダもこの環境に無い）。
/// **確かめていないものを表に書かない。** symlink は実装しないと宣言してある
/// （`docs/roadmap.md` の S10）ので、載せなくても `DT_UNKNOWN` で正しく答える。
fn dirent_type_for(file_type: u8) -> u8 {
    match file_type {
        common::ext2::DIRENT_TYPE_REGULAR => DT_REG,
        common::ext2::DIRENT_TYPE_DIRECTORY => DT_DIR,
        _ => DT_UNKNOWN,
    }
}

/// `getdents64(fd, dirp, count)` の本体（S10-b）。
///
/// # 収まらないレコードは書かない
///
/// **Linux の振る舞いを実測で確かめた。**
///
/// - **最初の 1 つも収まらない**なら `-EINVAL`（バッファが 0・8・16 バイトのとき）
/// - **収まるぶんだけ書く**（32 バイト渡しても、24 バイトのレコード 1 つで返る）
/// - **終端では 0 を返す**
///
/// **途中で切ったレコードは書かない。** 呼び出し側は `d_reclen` を頼りに歩くので、
/// 半端なレコードがあると歩けなくなる。
///
/// # 線4 がここで再来する
///
/// **`d_reclen` を積み上げる側にも上限が要る。** 上限は
/// **ユーザーバッファの残り**で、**1 レコードは必ず [`DIRENT64_HEADER_LEN`] より
/// 大きい**ので、書くたびに残りは必ず減る。**走査そのものの停止性は
/// `common::ext2` の側が持っている**（`rec_len` の 3 条件）。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn sys_getdents64(
    fd: u64,
    dirp: u64,
    count: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> u64 {
    let opened = crate::vfs::with_current_files(|files| {
        files
            .get(fd as usize)
            .map(|file| file.inode().map(|inode| (*inode, file.offset())))
    });
    let (inode, from) = match opened {
        Ok(Some(pair)) => pair,
        // **端末はディレクトリではない（S11-10）。** `getdents64` は拒む。
        Ok(None) => return (-ENOTDIR) as u64,
        Err(e) => return (-errno_for_file_table(e)) as u64,
    };
    if !inode.is_directory() {
        return (-ENOTDIR) as u64;
    }

    // **踏み込む前に検証する。** 書く量は `count` を越えない。
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, dirp, count) }) else {
        return (-EFAULT) as u64;
    };

    let fs = match crate::vfs::root_filesystem() {
        Ok(fs) => fs,
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };
    let entries = match fs.directory_entries_from(inode.ext2(), from) {
        Ok(entries) => entries,
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };

    let mut written = 0u64;
    let mut next_from = from;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => return (-errno_for_ext2(e)) as u64,
        };

        // レコードの長さ。**名前の NUL 終端を数え、8 バイト境界へ切り上げる。**
        //
        // 破壊 (S10-b, dirent-no-align): 切り上げをやめる。**こちらの走査は
        // `d_reclen` を頼りに歩くので、外しても自分では気づけない。** 整列は
        // 呼び出し側との約束なので、**約束を見ている検算だけが捕まえる。**
        //
        // **S10-b の他の 3 つとは種類が違う。** `eisdir-as-enotdir`・
        // `read-no-advance`・`stat-blocks-in-bytes` は**値が間違っている**形で、
        // 正しい値を知っていれば突き合わせられる。**こちらは値ではなく、
        // 呼び出し側との約束の違反である**——どの値が返るかは変わらず、
        // **返り方の規則だけが崩れる。** 突き合わせる相手は「正しい値」ではなく
        // 「約束」なので、**約束を明文で検査していなければ、何も落ちない。**
        let needed = DIRENT64_HEADER_LEN + entry.name.len() + 1;
        #[cfg(not(feature = "syscall-test-dirent-no-align"))]
        let reclen = needed.next_multiple_of(DIRENT64_ALIGN);
        #[cfg(feature = "syscall-test-dirent-no-align")]
        let reclen = needed;

        if written + reclen as u64 > count {
            // 収まらない。**書けたぶんで止める**（Linux と同じ）。
            break;
        }

        let mut record = [0u8; DIRENT64_MAX_RECORD];
        record[0..8].copy_from_slice(&u64::from(entry.inode).to_le_bytes());
        record[8..16].copy_from_slice(&entry.next_offset.to_le_bytes());
        record[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        record[18] = dirent_type_for(entry.file_type);
        // 名前と NUL。**`reclen` に収まる長さしか書かない。**
        let name_end = DIRENT64_HEADER_LEN + entry.name.len();
        if name_end + 1 > record.len() {
            // 名前が長すぎてレコードに収まらない。**像が壊れている。**
            return (-EIO) as u64;
        }
        record[DIRENT64_HEADER_LEN..name_end].copy_from_slice(entry.name);

        // SAFETY: slice は検証済み。`written + reclen` は `count` を越えない。
        let put = unsafe { copy_to_user(&slice, written, &record[..reclen]) };
        if put != reclen {
            return (-EFAULT) as u64;
        }
        written += reclen as u64;
        next_from = entry.next_offset;
    }

    if written == 0 && next_from == from {
        // 1 つも書いていない。**終端なのか、バッファが狭すぎたのかを分ける。**
        // **狭すぎた側は `-EINVAL` である**（Linux の実測）。
        if fs
            .directory_entries_from(inode.ext2(), from)
            .map(|mut walk| walk.next().is_some())
            .unwrap_or(false)
        {
            return (-EINVAL) as u64;
        }
        return 0;
    }

    // 次の呼び出しが続きから読めるように、位置を進める。
    crate::vfs::with_current_files(|files| {
        if let Ok(file) = files.get_mut(fd as usize) {
            file.seek_to(next_from);
        }
    });
    written
}

/// 1 レコードの作業領域。**名前は ext2 の上限（255）まで。**
const DIRENT64_MAX_RECORD: usize = (DIRENT64_HEADER_LEN + 255 + 1).next_multiple_of(DIRENT64_ALIGN);

/// `stat(path, statbuf)` の本体（S10-b）。
///
/// # 埋まる欄は 5 つで、残りは 0 である
///
/// ext2 の inode から埋まるのは `st_ino`・`st_nlink`・`st_mode`・`st_size`・
/// `st_blocks` である。**残りは 0 にする。**
///
/// **0 は未実装であって値ではない。** 内訳は次のとおりで、
/// **どれも「0 という値を持っている」のではない。**
///
/// - `st_dev` / `st_rdev`——**デバイス番号の体系が無い。** 像は 1 つで、
///   `BlockDevice` の trait も引いていない（`docs/roadmap.md` の S10 の締め）
/// - `st_blksize`——**入出力の推奨単位という概念が無い。** ブロックサイズなら
///   `Ext2::block_size` で分かるが、**`st_blksize` はそれとは別の意味である**
///   ので、分かる値で埋めない
/// - `st_atim` / `st_mtim` / `st_ctim`——**像の時刻を 0 に潰してある。**
///   `kernel/build.rs` の `zero_image_timestamps` が superblock の 3 つと全 inode の
///   4 つを 0 で上書きしており、**`mke2fs` の出力を決定的にするための帰結である。**
///   **なぜ 0 なのかは、そこに 1 箇所ある**
/// - `st_uid` / `st_gid`——**利用者の概念が無い。** ext2 の inode は値を持っているが、
///   **その値を照合する相手がカーネルの側に無い**ので、持っていることにしない
///
/// # `st_blocks` の単位
///
/// **512 バイト単位である**（ブロックサイズ単位ではない）。**ext2 の `i_blocks` も
/// 同じ単位なので、そのまま写す**（実測で確かめた。`common::ext2::Inode` の
/// `blocks_512` の doc）。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
/// `clock_gettime`（W2-d+）。**`CLOCK_MONOTONIC` だけを答える。**
///
/// # 秒とナノ秒は 1 本のティックから導く
///
/// **1 ティックは 10ms である**（実測で 100.000 Hz）。**周波数はカーネルの値を読む**
/// ——**定数を写すと、周波数を変えた日に片方だけが古くなる。**
///
/// **粒度は 10ms のままである。** **Wayland のミリ秒の分解能は形式として満たすが、
/// 入力の時刻印を付ける段で足りるかを判断すること**（`ADR-0062`）。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn sys_clock_gettime(
    clockid: u64,
    out: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> u64 {
    if clockid != CLOCK_MONOTONIC {
        return (-EINVAL) as u64;
    }
    let ticks = crate::idt::monotonic_ticks();
    // 破壊 (W2-d+, clock-goes-backwards): 呼ぶたびに減る値を返す。**単調さが壊れる。**
    // **値はもっともらしいまま進むので、2 回読んで比べる検算でしか捕まらない。**
    #[cfg(feature = "clock-goes-backwards")]
    let ticks = u64::MAX - ticks;
    let hz = u64::from(crate::irq::timer_frequency_hz());
    // **換算はホストで固定してある**（`common::time`）。
    let (secs, nsecs) = common::time::timespec_from_ticks(ticks, hz);

    let mut buf = [0u8; TIMESPEC_LEN];
    buf[..TIMESPEC_NSEC].copy_from_slice(&secs.to_le_bytes());
    buf[TIMESPEC_NSEC..].copy_from_slice(&nsecs.to_le_bytes());

    // **踏み込む前に検証する。**
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) =
        (unsafe { validate_user_range(pml4_phys, direct_map, out, TIMESPEC_LEN as u64) })
    else {
        return (-EFAULT) as u64;
    };
    // SAFETY: slice は検証済みで、長さは TIMESPEC_LEN ちょうどである。
    unsafe { copy_to_user(&slice, 0, &buf) };
    0
}

/// `nanosleep` が待ちに入った回数（W2-d+ の計器）。
static TIMER_WAITS: AtomicU64 = AtomicU64::new(0);

/// 眠った側が、起こされた時点でまだ締切に届いていなかった回数（W2-d+ の計器）。
///
/// **本番では 0 である**——**起こすのはタイマで、締切を過ぎてから起こす。**
/// **0 でなければ、誰かが締切より前に起こした**（合図を取り違えた起こし、または締切を
/// 見ないタイマ）。**眠った側は待ち直すので、所要には出ない**——**ここでしか見えない。**
static EARLY_TIMER_WAKES: AtomicU64 = AtomicU64::new(0);

/// `nanosleep` が待ちに入った回数（W2-d+）。
pub fn timer_waits() -> u64 {
    TIMER_WAITS.load(Ordering::Relaxed)
}

/// 締切より前に起こされた回数（W2-d+）。
pub fn early_timer_wakes() -> u64 {
    EARLY_TIMER_WAKES.load(Ordering::Relaxed)
}

/// `nanosleep`（W2-d+。`ADR-0062`）。**締切まで `Waiting(Timer)` で眠る。**
///
/// # 待ち方は `read(0)` と同じ踊りである
///
/// **欄を `Waiting` にし、BKL を解いて譲り、起きたら取り直す**（`wait_for_keyboard`）。
/// **呼ばれるのは IF=0 の文脈なので、「欄を変えてから譲る」までにタイマは入らない**
/// ——**窓は構造で閉じている**（W2-c-2 で実測した理由と同じ）。
///
/// # 上限は置かない
///
/// **締切は呼び手が求めた長さであって、安全網ではない**（`ADR-0061`）。
///
/// # 早く起こされたら待ち直す
///
/// **起きたら締切を見直す。** **届いていなければ数えて、また眠る**——**眠る長さは
/// 求めた長さより短くならない。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn sys_nanosleep(
    req: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) =
        (unsafe { validate_user_range(pml4_phys, direct_map, req, TIMESPEC_LEN as u64) })
    else {
        return (-EFAULT) as u64;
    };
    let mut raw = [0u8; TIMESPEC_LEN];
    // SAFETY: slice は検証済みで、長さは TIMESPEC_LEN ちょうどである。
    unsafe { copy_from_user(&mut raw, &slice) };
    let mut seconds = [0u8; 8];
    seconds.copy_from_slice(&raw[..TIMESPEC_NSEC]);
    let mut nanos = [0u8; 8];
    nanos.copy_from_slice(&raw[TIMESPEC_NSEC..]);

    let hz = u64::from(crate::irq::timer_frequency_hz());
    let Ok(ticks) = common::time::ticks_for_duration(
        i64::from_le_bytes(seconds),
        i64::from_le_bytes(nanos),
        hz,
    ) else {
        return (-EINVAL) as u64;
    };
    let deadline = crate::idt::monotonic_ticks().saturating_add(ticks);

    while crate::idt::monotonic_ticks() < deadline {
        TIMER_WAITS.fetch_add(1, Ordering::Relaxed);
        crate::task::set_current_waiting(crate::task::Wait::Timer { deadline });
        drop(bkl.take());
        crate::task::yield_now();
        *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
        if crate::idt::monotonic_ticks() < deadline {
            EARLY_TIMER_WAKES.fetch_add(1, Ordering::Relaxed);
        }
    }
    0
}

unsafe fn sys_stat(path: u64, statbuf: u64, pml4_phys: PhysAddr, direct_map: DirectMap) -> u64 {
    let mut name = [0u8; PATH_MAX];
    // SAFETY: 呼び出し元契約をそのまま渡す。
    let len = match unsafe { copy_user_path(&mut name, path, pml4_phys, direct_map) } {
        Ok(len) => len,
        Err(errno) => return (-errno) as u64,
    };

    let fs = match crate::vfs::root_filesystem() {
        Ok(fs) => fs,
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };
    let inode = match fs.lookup(&name[..len]) {
        Ok(inode) => inode,
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };

    // **0 で埋めてから、分かる欄だけを書く。** 「書かなかった欄は 0」が
    // 構造で決まるので、埋め忘れが未定義の値として出ない。
    let mut out = [0u8; STAT_LEN];
    out[STAT_INO..STAT_INO + 8].copy_from_slice(&u64::from(inode.number).to_le_bytes());
    out[STAT_NLINK..STAT_NLINK + 8].copy_from_slice(&u64::from(inode.links_count).to_le_bytes());
    out[STAT_MODE..STAT_MODE + 4].copy_from_slice(&u32::from(inode.mode).to_le_bytes());
    out[STAT_SIZE..STAT_SIZE + 8].copy_from_slice(&inode.size.to_le_bytes());
    // 破壊 (S10-b, stat-blocks-in-bytes): `st_blocks` を 512 バイト単位ではなく
    // バイト数で書く。**単位の取り違えは値が「もっともらしい」ままなので、
    // 突き合わせる相手が無いと気づけない。** syscall-test の検算が捕まえる。
    #[cfg(not(feature = "syscall-test-stat-blocks-in-bytes"))]
    let blocks = u64::from(inode.blocks_512);
    #[cfg(feature = "syscall-test-stat-blocks-in-bytes")]
    let blocks = u64::from(inode.blocks_512) * 512;
    out[STAT_BLOCKS..STAT_BLOCKS + 8].copy_from_slice(&blocks.to_le_bytes());

    // **踏み込む前に検証する。**
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) =
        (unsafe { validate_user_range(pml4_phys, direct_map, statbuf, STAT_LEN as u64) })
    else {
        return (-EFAULT) as u64;
    };
    // SAFETY: slice は検証済みで、長さは STAT_LEN ちょうどである。
    let written = unsafe { copy_to_user(&slice, 0, &out) };
    if written != STAT_LEN {
        return (-EFAULT) as u64;
    }
    0
}

/// `ioctl(fd, request, arg)`（e-1）。**端末の問い合わせだけを受ける。**
///
/// # 受けるのは `TIOCGWINSZ` だけである
///
/// **入口の方針は [`SYS_IOCTL`] の doc にある**——端末の問い合わせに限り、
/// 設定の変更は別の判断とする。**知らない要求は `-ENOTTY` で断る。**
///
/// # 断り方は 3 つある
///
/// - **無い fd** → `-EBADF`（表が答える）
/// - **端末でない fd**（`open` で開いたファイル）→ `-ENOTTY`
/// - **知らない要求** → `-ENOTTY`
///
/// **Linux も端末でない fd と知らない要求に同じ `ENOTTY` を返す。**
///
/// # 画面が無いときは 0 を返し、欄を 0 で埋める
///
/// **前景のコンソールが据えられていない文脈がある**（起動シーケンスの検算）。
/// **そこは「端末だが大きさが無い」**——**シリアルだけの端末と同じ立場である。**
/// **Linux も、大きさを知らない端末には 0 を返す**（`ws_row` が 0）。
/// **呼ぶ側は 0 を確かめること。** **`-ENOTTY` にはしない**——
/// **端末ではあるので、嘘になる。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn sys_ioctl(
    fd: u64,
    request: u64,
    arg: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> u64 {
    // **表を引いて端末かどうかを見る**（`sys_write` と同じ形。番号では分けない）。
    let is_terminal = match crate::vfs::with_current_files(|files| {
        files.get(fd as usize).map(|file| file.is_terminal())
    }) {
        Ok(is_terminal) => is_terminal,
        Err(e) => return (-errno_for_file_table(e)) as u64,
    };
    if !is_terminal {
        return (-ENOTTY) as u64;
    }
    match request {
        TIOCGWINSZ => {}
        // **溜まっているエラーを取り出す（ADR-0046）。**
        // SAFETY: 呼び出し元契約をそのまま渡す。
        TIOCZTAKE => return unsafe { ioctl_take_pending(arg, pml4_phys, direct_map) },
        // **1 行をログへ出す（ADR-0046）。**
        // SAFETY: 同上。
        TIOCZLOG => return unsafe { ioctl_log_line(arg, pml4_phys, direct_map) },
        _ => return (-ENOTTY) as u64,
    }

    // **0 で埋めてから、分かる欄だけを書く**（`sys_stat` と同じ形）。
    // **画面が無ければ 0 のままである。**
    let mut out = [0u8; WINSIZE_LEN];
    if let Some((columns, rows, width, height)) = crate::console::foreground_geometry() {
        // 破壊 (e-1, ioctl-winsize-swap): 行と桁を入れ替えて返す。
        // **どちらももっともらしい数のままなので、受けた側だけでは気づけない**
        // （`stat` の `st_blocks` を単位違いで返す破壊と同じ族である）。
        // **画面は正方形ではない**（160x50。実測）ので、入れ替えれば必ず違う値になる。
        // **カーネルが自分の値を判定行に出しており、突き合わせが捕まえる。**
        #[cfg(feature = "ioctl-winsize-swap-test")]
        let (rows, columns) = (columns, rows);
        // **`u16` へ収める。** **越えることは無い**——桁も行もセルの数で、
        // 仮定する最大は 240x67 である（`kernel_main` の `MAX_TERMINAL_CELLS`）。
        out[0..2].copy_from_slice(&(rows as u16).to_le_bytes());
        out[2..4].copy_from_slice(&(columns as u16).to_le_bytes());
        out[4..6].copy_from_slice(&(width as u16).to_le_bytes());
        out[6..8].copy_from_slice(&(height as u16).to_le_bytes());
    }

    // **踏み込む前に検証する。**
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) =
        (unsafe { validate_user_range(pml4_phys, direct_map, arg, WINSIZE_LEN as u64) })
    else {
        return (-EFAULT) as u64;
    };
    // SAFETY: slice は検証済みで、長さは WINSIZE_LEN ちょうどである。
    let written = unsafe { copy_to_user(&slice, 0, &out) };
    if written != WINSIZE_LEN {
        return (-EFAULT) as u64;
    }
    0
}

/// 溜まっているエラーを `ioctl` の形で返す（`TIOCZTAKE`。ADR-0046）。
///
/// # 何を返すか
///
/// **`[0..2]` が長さ、`[2..4]` が捨てた数、`[4..]` が本文である。**
/// **取り出したら空になる**——**同じものを 2 度出さない。**
///
/// # 空で返るのが普通である
///
/// **アプリは毎周訊きに来る。** **溜まっていなければ長さ 0 で返る**ので、
/// **呼ぶ側は「0 なら何もしない」と書けばよい。** **`-ENOENT` にはしない**
/// ——**errno は異常のためのもので、これは異常ではない。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn ioctl_take_pending(arg: u64, pml4_phys: PhysAddr, direct_map: DirectMap) -> u64 {
    let mut out = [0u8; ZDIAG_LEN];
    crate::console::take_pending(&mut out);

    // **踏み込む前に検証する。**
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) =
        (unsafe { validate_user_range(pml4_phys, direct_map, arg, ZDIAG_LEN as u64) })
    else {
        return (-EFAULT) as u64;
    };
    // SAFETY: slice は検証済みで、長さは ZDIAG_LEN ちょうどである。
    let written = unsafe { copy_to_user(&slice, 0, &out) };
    if written != ZDIAG_LEN {
        return (-EFAULT) as u64;
    }
    0
}

/// 1 行をログ（シリアル）へ出す（`TIOCZLOG`。ADR-0046）。
///
/// # 画面へは出さない
///
/// **これは診断の出口である。** **読み手はホスト側の判定で、その判定は
/// シリアルを読んでいる**（ADR-0046 の「線はどこに在るか」）。
/// **全画面のアプリの画面を、診断が壊してよい理由は無い。**
///
/// # 受ける形は `TIOCZTAKE` と同じ構造である
///
/// **`[0..2]` が長さ、`[4..]` が本文である。** **捨てた数の欄は読まない。**
/// **形を 1 つにしておくと、包む側（`userlib`）が 1 つの構造で済む。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn ioctl_log_line(arg: u64, pml4_phys: PhysAddr, direct_map: DirectMap) -> u64 {
    // **踏み込む前に検証する。**
    // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
    let Some(slice) =
        (unsafe { validate_user_range(pml4_phys, direct_map, arg, ZDIAG_LEN as u64) })
    else {
        return (-EFAULT) as u64;
    };
    let mut buf = [0u8; ZDIAG_LEN];
    // SAFETY: slice は検証済みで、長さは ZDIAG_LEN ちょうどである。
    let read = unsafe { copy_from_user(&mut buf, &slice) };
    if read != ZDIAG_LEN {
        return (-EFAULT) as u64;
    }
    let length = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    if length > ZDIAG_LEN - ZDIAG_TEXT_OFFSET {
        return (-EINVAL) as u64;
    }

    let mut port = common::serial::SerialPort::new(common::serial::SerialPort::COM1_BASE);
    port.init();
    for byte in &buf[ZDIAG_TEXT_OFFSET..ZDIAG_TEXT_OFFSET + length] {
        port.write_byte(*byte);
    }

    // 破壊 (ADR-0046, stderr-on-screen-test): 診断を画面へも書く。
    // **ADR-0046 の前の振る舞いそのものである**——**カーソルの居る行の本文が
    // 診断行に化ける。** **`screen-window` が画面の行 0 を読んで捕まえる。**
    #[cfg(feature = "stderr-on-screen-test")]
    if crate::console::foreground_installed() {
        crate::console::write_foreground_bytes(&buf[ZDIAG_TEXT_OFFSET..ZDIAG_TEXT_OFFSET + length]);
    }

    0
}

/// ユーザー空間の NUL 終端のパスを、カーネルのバッファへ写す（S10-b）。
///
/// 返るのは NUL を含まない長さである。
///
/// # ページごとに検証してから読む
///
/// **長さが先に分からないので、一度に検証できない。** そこで
/// **「今いるページの残り」を単位に検証しては読む**。**踏み込む前に検証するという
/// 契約は 1 バイトごとに保たれる。**
///
/// **上限は [`PATH_MAX`] である。** 越えたら `-ENAMETOOLONG` で、
/// **NUL が無い入力でも必ず止まる**（`common::ext2` の走査と同じ形で、
/// 進む量が正で上限が有限である）。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn copy_user_path(
    dst: &mut [u8; PATH_MAX],
    path: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> Result<usize, i64> {
    /// ページの大きさ。**検証の単位である。**
    const PAGE: u64 = 0x1000;

    let mut copied = 0usize;
    while copied < PATH_MAX {
        let addr = path.checked_add(copied as u64).ok_or(EFAULT)?;
        // 今いるページの残り。**ページ境界を越えない単位で検証する。**
        let to_page_end = PAGE - (addr & (PAGE - 1));
        let chunk = to_page_end.min((PATH_MAX - copied) as u64);
        // SAFETY: 呼び出し元契約をそのまま渡す。
        let slice =
            unsafe { validate_user_range(pml4_phys, direct_map, addr, chunk) }.ok_or(EFAULT)?;
        // SAFETY: slice は検証済み。dst の残りは chunk を収める。
        let read = unsafe { copy_from_user(&mut dst[copied..copied + chunk as usize], &slice) };
        if read == 0 {
            return Err(EFAULT);
        }
        for i in 0..read {
            if dst[copied + i] == 0 {
                return Ok(copied + i);
            }
        }
        copied += read;
    }
    Err(ENAMETOOLONG)
}

/// 端末からの `read` で 1 回に写す最大バイト数（S11-10）。
///
/// **カーネルスタックへ置く緩衝の大きさである。** 端末は溜まっている分しか
/// 返さないので、**大きくしても意味が無い**——**1 回の `read` で取り切れなければ、
/// 次の `read` が続きを取る。** 64 は `WRITE_BUF_LEN` と同じで、
/// **4096 バイトを越えないローカル配列の範囲である。**
pub const TERMINAL_READ_MAX: usize = 64;

/// 標準出力の fd（Linux と同じ 1）。
pub const STDOUT_FD: u64 = 1;
/// 標準エラー出力の fd（Linux と同じ 2）。
pub const STDERR_FD: u64 = 2;

/// `write(fd, buf, count)` の本体（S11-8）。
///
/// # 出力先を持たせた
///
/// **S11-7 まで、`write` は受け取ったバイト列を静的領域へ記録するだけだった。**
/// カーネル側の判定行がそれを読んで突き合わせる形で、**Ring 3 の出力はどこへも
/// 届いていなかった。** `hello` の "hello from ring 3" も、シリアルには出ていない。
///
/// **`ls` と `cat` を書くには、出力が届く先が要る。** 「印字するプログラム」は、
/// **印字が観測できて初めて意味を持つ。**
///
/// # シリアルへ出す。コンソールへは出さない
///
/// **シリアルは最優先の観測手段である**（`docs/architecture.md`）。
/// **コンソール（画面）へは出さない**——`deferred-decisions.md` の
/// 「コンソール / シリアルへの出力の多重化」が
/// **「出力するのはメインループだけ」という制約で運用する**と決めており、
/// **`dispatch` はメインループではない。** `Console` は `kernel_main` のローカルで、
/// lib からは届かない。**あの行の条件（前景プロセスの概念を設計する時点）を
/// 先取りしない。**
///
/// # fd を見る
///
/// **1 と 2 だけを受ける。** それ以外は `-EBADF` である。
///
/// **0/1/2 を予約する話とは別である。** `crate::vfs::FileTable` は
/// **0/1/2 を予約していない**ので、`open` は 0 番から返す。**衝突しないのは、
/// ファイルへ書く道がまだ無いからである**——`write` が表を引くことは一度も無い。
/// **`docs/roadmap.md` の「予約するか、シェルが自分で開くか」は、
/// ファイルへ書けるようになった時点（S12）で決める。**
///
/// # 長さの上限を外した
///
/// **[`WRITE_BUF_LEN`] を越えると `-EINVAL` を返していた。** あれは
/// **記録用の緩衝の大きさ**であって、`write` そのものの上限ではない。
/// **ページ単位に検証しては出す**ので、**カーネル側に長さぶんの緩衝は要らない。**
/// **記録は先頭 [`WRITE_BUF_LEN`] バイトだけ残す**——判定行が突き合わせるのは
/// そこまでである。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn sys_write(
    fd: u64,
    buf: u64,
    count: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    bkl: &mut Option<crate::bkl::BklGuard>,
) -> u64 {
    /// ページの大きさ。**検証の単位である。**
    const PAGE: u64 = 0x1000;

    // **番号ではなく、表の中身で分岐する（S11-10）。**
    //
    // **S11-8 では番号（1 と 2）を直に見ていた。** `crate::vfs::FileTable` が
    // 0 / 1 / 2 を端末として持つようになったので、**表を引いて端末かどうかを
    // 見る形にした。** **`open` が返した番号と衝突しない**——
    // あちらは 3 から返る。
    //
    // **ファイルへの書き込みはまだ無い**ので、端末でなければ `-EROFS` である
    // （読み取り専用のファイルシステム。`open` が書き込みを拒むのと同じ理由）。
    //
    // 破壊 (S11-8, write-ignores-fd): 表を引かず、何番でも出す。
    // **出力はそのまま現れるので、雑に見ると正しく動いているように見える。**
    // **見えないのは「開いていない番号が拒まれること」のほうである。**
    //
    // **エラーの出口かどうかも、ここで表から取る（ADR-0046）。**
    // **番号（2）で見ない**——**表の中身で分ける形をS11-10から続けている。**
    // **パイプの書き端（`ADR-0063` の (b3)）。** **端末と通常ファイルの手前で分かれる。**
    let pipe_write = crate::vfs::with_current_files(|files| {
        files
            .get(fd as usize)
            .ok()
            .and_then(|file| file.pipe_write_end())
    });
    if let Some(pipe) = pipe_write {
        // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
        return unsafe { write_to_pipe(pipe, buf, count, pml4_phys, direct_map, bkl) };
    }

    // **ソケット（`ADR-0064`）。** **繋がっていなければ `-ENOTCONN`。**
    let socket = crate::vfs::with_current_files(|files| {
        files
            .get(fd as usize)
            .ok()
            .and_then(|file| file.socket_state())
    });
    match socket {
        Some(crate::vfs::SocketState::Stream { conn, side }) => {
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            return unsafe { write_to_socket(conn, side, buf, count, pml4_phys, direct_map, bkl) };
        }
        Some(_) => return (-ENOTCONN) as u64,
        None => {}
    }

    #[cfg(not(feature = "write-ignores-fd"))]
    let errors = {
        let kind = crate::vfs::with_current_files(|files| {
            files.get(fd as usize).map(|file| {
                (
                    file.is_terminal(),
                    file.is_writable_file(),
                    file.inode().map(|inode| inode.number()),
                    file.is_error_terminal(),
                )
            })
        });
        match kind {
            // 端末。下のシリアル+画面の経路へ。
            Ok((true, _, _, errors)) => errors,
            // **書きで開いたファイル（zi-c。ADR-0037）。** 複製へ足して返る。
            Ok((false, true, Some(ino), _)) => {
                // SAFETY: 呼び出し元契約をそのまま渡す。
                return unsafe { sys_write_to_file(fd, buf, count, ino, pml4_phys, direct_map) };
            }
            // **読みで開いた fd への write は -EBADF である**（Linux の形。
            // ADR-0037。以前の -EROFS はファイルへ書く道が無い時代の値だった）。
            Ok((false, _, _, _)) => return (-EBADF) as u64,
            Err(e) => return (-errno_for_file_table(e)) as u64,
        }
    };
    // **表を引かない構成では、溜める判断もできない**（fd が何かを知らない）。
    #[cfg(feature = "write-ignores-fd")]
    let errors = false;

    // 破壊 (S11-9, write-half-only): 要求された長さの半分だけ書いて返す。
    //
    // **主張は「`write` は要求した長さを全部書く。書けなければ呼び出し側が
    // 繰り返す」である。** 短い書き込みが返るのは Linux でも起きるので、
    // **呼び出し側は戻り値を見て繰り返さなければならない**——
    // `kernel/userland/userlib.rs` の `write_all` がそうしている。
    //
    // **24 バイト以下は半分にしない。** 既存の 4 本（`hello` と `syscall-test` と
    // `fault-test` と `spawn-test`）は asm で直に `write` を呼んでおり、
    // **繰り返しを持たない。** あれらが使う最大の長さが 24 である。
    // **そこを半分にすると、`ls` と `cat` が起こされる前に止まってしまい、
    // 繰り返しの経路が一度も通らない。**
    // **破壊の目的は 2 つある**——**短い書き込みが検出されること**（`syscall-test` の
    // 47 番、68 バイトの行）と、**繰り返しの経路が実際に通ること**（`ls` の
    // 30 バイトの一覧が 2 周で出る）。
    #[cfg(feature = "write-half-only")]
    let count = if count > 24 { count.div_ceil(2) } else { count };

    // **システムコールの回数を数える（PERF-b）。** **刻む前に 1 回だけである**
    // ——**刻んだ後の回数は `foreground_writes` が別に持つ。**
    crate::console::note_terminal_write();
    // **起こしっぱなしのスロットから端末へ書いた回数（`ADR-0063` の (b3) の計器）。**
    // **`a | b` の左は端末へ書かないはずである**——**判定が「0」を見る。**
    if crate::ring3::current_slot() == crate::task::detached_slot() {
        TERMINAL_WRITES_FROM_DETACHED.fetch_add(1, Ordering::Relaxed);
    }

    let mut port = common::serial::SerialPort::new(common::serial::SerialPort::COM1_BASE);
    port.init();

    let mut recorded = [0u8; WRITE_BUF_LEN];
    let mut done = 0u64;
    while done < count {
        let addr = match buf.checked_add(done) {
            Some(addr) => addr,
            None => return (-EFAULT) as u64,
        };
        // **一度に扱う量は 3 つの min である。** ページの残り（検証の単位）、
        // 要求の残り、そして [`WRITE_BUF_LEN`]（スタックへ置ける緩衝の大きさ）。
        let to_page_end = PAGE - (addr & (PAGE - 1));
        let chunk = to_page_end.min(count - done).min(WRITE_BUF_LEN as u64);
        // **踏み込む前に検証する。** 検証済みトークンを得てから読む。
        // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
        let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, addr, chunk) })
        else {
            // **届いた分だけを返す。** 届いていないものを届いたことにしない。
            return if done == 0 { (-EFAULT) as u64 } else { done };
        };
        let mut kbuf = [0u8; WRITE_BUF_LEN];
        // SAFETY: slice は検証済み。dst は chunk を収める。
        let read = unsafe { copy_from_user(&mut kbuf[..chunk as usize], &slice) };
        if read == 0 {
            return if done == 0 { (-EFAULT) as u64 } else { done };
        }
        for byte in &kbuf[..read] {
            port.write_byte(*byte);
        }

        // **画面へは BKL を解いてから書く（S12 前の手当て）。**
        //
        // **1 行の描画と転送は 1 ティックの半分ほど掛かる**（実測は `console:` の
        // 判定行にある。TCG で 0.67 ティック）。**保持したまま書くと、その間
        // もう一方のコアがカーネルへ入れない。**
        //
        // **解く区間は最小である。** シリアルへの書き込みは保持したままでよい
        // （速く、既にそうなっている）ので、**画面へ書く呼び出しだけを外へ出す。**
        // **解いた区間で触るのは `Console` だけである**（`ADR-0023` の Addendum の
        // 数え上げに 6 つ目として足してある）。
        //
        // **`SYS_SPAWN` とは形が違う。** あちらは**解いたまま Ring 3 へ降り、
        // 戻ってから取り直す**。こちらは**解いて、書いて、その場で取り直す**。
        // 次に解く経路を作る人は、どちらの形かを先に決めること。
        //
        // **据えられていないときは解かない。** 画面へ書くものが無いので、
        // 解いて取り直す理由も無い（起動時の検算はこちらを通る）。
        //
        // **全画面のアプリが動く間、エラーは画面へ書かずに溜める（ADR-0046）。**
        // **描くのはアプリである**——取り出してエコーエリアへ出す
        // （`ioctl(TIOCZTAKE)`）。**カーネルが割り込んで描くと絵が壊れる。**
        //
        // 破壊 (ADR-0046, stderr-on-screen-test): 溜めずに、いままでどおり画面へ書く。
        // **`zi`の本文がカーソルの居る行ごと上書きされる形そのものである。**
        // **`screen-echo`が最下行を読んで捕まえる**（エラーがエコーエリアに
        // 出ていないことのほうが主張である）。
        #[cfg(not(feature = "stderr-on-screen-test"))]
        let deferred = errors && crate::console::push_pending_if_alternate(&kbuf[..read]);
        #[cfg(feature = "stderr-on-screen-test")]
        let deferred = {
            let _ = errors;
            false
        };
        if !deferred && crate::console::foreground_installed() {
            drop(bkl.take());
            crate::console::write_foreground_bytes(&kbuf[..read]);
            *bkl = Some(crate::bkl::acquire(crate::bkl::KernelEntry::Syscall));
        }
        // **先頭 [`WRITE_BUF_LEN`] バイトだけ控える。**
        let already = done as usize;
        if already < WRITE_BUF_LEN {
            let take = (WRITE_BUF_LEN - already).min(read);
            recorded[already..already + take].copy_from_slice(&kbuf[..take]);
        }
        done += read as u64;
    }

    // **1 回だけ引く（W1-c-3。`syscall_entry` の同じ箇所の注記）。**
    let state = state();
    state.write_fd.store(fd, Ordering::SeqCst);
    for (slot, value) in state.write_buf.iter().zip(recorded.iter()) {
        slot.store(*value, Ordering::SeqCst);
    }
    state
        .write_len
        .store(done.min(WRITE_BUF_LEN as u64), Ordering::SeqCst);
    done
}

/// `write(fd, buf, count)` のファイルの側（zi-c。ADR-0037）。
///
/// # RAM 複製への追記である
///
/// **fd は `O_WRONLY|O_TRUNC` で開かれており、open の時点で長さ 0 に切って
/// ある。** したがって**追記（`append_to_file`）が全置換の後半である。**
/// 位置（offset）は使わない——追記は像の中の `i_size` から続き、
/// **読みは `-EBADF` なので位置を読む者も居ない。**
///
/// # 検証の形はシリアルの側と同じである
///
/// **ページごとに検証し、検証済みトークン（`UserSlice`）から一時緩衝へ写し、
/// そこから複製へ足す。** 踏み込む前に検証する契約は崩れない。
///
/// # シリアルへも画面へも出さない
///
/// ファイルへの write は端末への write ではない。**出力の多重化の判断
/// （`deferred-decisions.md`）にも触れない。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn sys_write_to_file(
    _fd: u64,
    buf: u64,
    count: u64,
    ino: u32,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> u64 {
    /// ページの大きさ。**検証の単位である。**
    const PAGE: u64 = 0x1000;

    // **配置の写しを先に取る。** `Layout` は `Copy` で、`fs`（共有借用）は
    // この束で落ちる——`with_root_image_mut` の可変借用と重ならない
    // （`crate::vfs::with_root_image_mut` の doc の列挙）。
    let layout = match crate::vfs::root_filesystem() {
        Ok(fs) => fs.layout(),
        Err(e) => return (-errno_for_ext2(e)) as u64,
    };

    // 破壊 (zi-c, write-file-wrong-inode-test): 別の inode へ足す。
    // **戻り値もシリアルも正しく見える**——的のファイルだけが空のままになり、
    // syscall-test の読み戻し（58 番）が捕まえる。
    #[cfg(feature = "write-file-wrong-inode-test")]
    let ino = ino + 1;

    let mut done = 0u64;
    while done < count {
        let addr = match buf.checked_add(done) {
            Some(addr) => addr,
            None => return (-EFAULT) as u64,
        };
        // **一度に扱う量は 3 つの min である**（シリアルの側と同じ）。
        let to_page_end = PAGE - (addr & (PAGE - 1));
        let chunk = to_page_end.min(count - done).min(WRITE_BUF_LEN as u64);
        // **踏み込む前に検証する。**
        // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
        let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, addr, chunk) })
        else {
            // **届いた分だけを返す。** 届いていないものを届いたことにしない。
            return if done == 0 { (-EFAULT) as u64 } else { done };
        };
        let mut kbuf = [0u8; WRITE_BUF_LEN];
        // SAFETY: slice は検証済み。dst は chunk を収める。
        let read = unsafe { copy_from_user(&mut kbuf[..chunk as usize], &slice) };
        if read == 0 {
            return if done == 0 { (-EFAULT) as u64 } else { done };
        }

        // 破壊 (zi-c, write-file-skip-append-test): 複製へ足さない。
        // **検証も戻り値も正しい**——書いたつもりが複製に届いていない形で、
        // 戻り値では捕まらない。syscall-test の読み戻し（58 番）が捕まえる。
        #[cfg(not(feature = "write-file-skip-append-test"))]
        {
            let appended = crate::vfs::with_root_image_mut(|image| {
                common::ext2::append_to_file(image, &layout, ino, &kbuf[..read])
            });
            match appended {
                Some(Ok(())) => {}
                // 空きが尽きた等。**届いた分だけを返す**（短い write）。
                Some(Err(_)) => {
                    return if done == 0 { (-EIO) as u64 } else { done };
                }
                // 複製前は書けない（open が拒んでいるので、来ない見込み）。
                None => return (-EROFS) as u64,
            }
        }

        done += read as u64;
    }
    done
}

/// NUL 終端のユーザー文字列を 1 本写す（S11-7）。
///
/// 写したバイト数（**NUL を含む**）を返す。**`dst` に収まらなければ `-E2BIG` である**
/// ——アドレスの誤りではなく量の問題なので、`-EFAULT` でも `-EINVAL` でもない。
///
/// # [`copy_user_path`] と同じ形である
///
/// **ページごとに検証してから読む。** 長さが先に分からないので、
/// **「今いるページの残り」を単位に検証しては読む。** 上限は `dst` の長さで、
/// **NUL が無い入力でも必ず止まる。**
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn copy_user_string(
    dst: &mut [u8],
    ptr: u64,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> Result<usize, i64> {
    /// ページの大きさ。**検証の単位である。**
    const PAGE: u64 = 0x1000;

    let mut copied = 0usize;
    while copied < dst.len() {
        let addr = ptr.checked_add(copied as u64).ok_or(EFAULT)?;
        let to_page_end = PAGE - (addr & (PAGE - 1));
        let chunk = to_page_end.min((dst.len() - copied) as u64);
        // SAFETY: 呼び出し元契約をそのまま渡す。
        let slice =
            unsafe { validate_user_range(pml4_phys, direct_map, addr, chunk) }.ok_or(EFAULT)?;
        // SAFETY: slice は検証済み。dst の残りは chunk を収める。
        let read = unsafe { copy_from_user(&mut dst[copied..copied + chunk as usize], &slice) };
        if read == 0 {
            return Err(EFAULT);
        }
        for i in 0..read {
            if dst[copied + i] == 0 {
                return Ok(copied + i + 1);
            }
        }
        copied += read;
    }
    Err(E2BIG)
}

/// ユーザーの `argv` / `envp`（NULL 終端のポインタ配列）を写す（S11-7。f-2 で一般化）。
///
/// 写したバイト列を `dst` へ NUL 区切りで並べ、`(要素数, 使ったバイト数)` を返す。
///
/// # 線が当たる場所は 4 つある
///
/// **配列の終端が無い形**——NULL に当たるまで歩くので、**上限が要る。**
/// `max_count`（`argv` なら [`crate::userland::MAX_ARGV`]、`envp` なら
/// [`crate::userland::MAX_ENVP`]）を越えたら `-E2BIG` で止める。
/// **`common::ext2` の走査と同じ形で、進む量が正（8 バイト）で上限が有限である。**
///
/// **要素数の上限**——同上。**表と文字列が 1 ページに収まる根拠でもある。**
///
/// **1 本あたりの長さの上限**——[`copy_user_string`] が `dst` の残りで切る。
///
/// **全体の長さの上限**——`dst` の大きさ（`argv` なら [`MAX_ARGV_BYTES`]）。
/// **そして最後にページの判定がある**
/// （`build_initial_stack`）。**2 枚あるのは、緩衝の大きさとページの大きさが
/// 別の理由で決まっているからである。**
///
/// # 配列そのものが NULL なら `-EFAULT`
///
/// **配列を要求している。** 「引数が無い」は**空の配列**（先頭が NULL）で表す。
/// **`envp` も同じ規則である**（`ADR-0053` の Decision 2。**新しい規則を作らない**）。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`validate_user_range`] の契約を満たすこと。
unsafe fn copy_user_string_array(
    dst: &mut [u8],
    base: u64,
    max_count: usize,
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> Result<(usize, usize), i64> {
    /// 1 要素の大きさ（ポインタ）。
    const WORD: u64 = 8;

    if base == 0 {
        return Err(EFAULT);
    }

    let mut count = 0usize;
    let mut used = 0usize;
    loop {
        let slot = base.checked_add(count as u64 * WORD).ok_or(EFAULT)?;
        // SAFETY: 呼び出し元契約をそのまま渡す。
        let slice =
            unsafe { validate_user_range(pml4_phys, direct_map, slot, WORD) }.ok_or(EFAULT)?;
        let mut word = [0u8; WORD as usize];
        // SAFETY: slice は検証済みで、word は 8 バイトを収める。
        let read = unsafe { copy_from_user(&mut word, &slice) };
        if read != WORD as usize {
            return Err(EFAULT);
        }
        let pointer = u64::from_le_bytes(word);
        if pointer == 0 {
            return Ok((count, used));
        }
        if count == max_count {
            // 破壊 (S11-7, spawn-e2big-as-einval): 量の問題を `-EINVAL` で返す。
            // **どちらも「引数が受け付けられない」なので、雑に見ると同じに見える。**
            // **Linux は分けている**——`execve` は長すぎる引数に `E2BIG` を返す。
            // **`syscall-test` の検算が食い違いを捕まえる。**
            #[cfg(not(feature = "spawn-e2big-as-einval"))]
            let errno = E2BIG;
            #[cfg(feature = "spawn-e2big-as-einval")]
            let errno = EINVAL;
            return Err(errno);
        }
        // SAFETY: 呼び出し元契約をそのまま渡す。
        let written =
            match unsafe { copy_user_string(&mut dst[used..], pointer, pml4_phys, direct_map) } {
                Ok(written) => written,
                // 破壊 (S11-7, spawn-e2big-as-einval): こちらの経路も同じく潰す。
                // **要素数と長さは別の場所で落ちるので、両方を同じ形にする。**
                #[cfg(feature = "spawn-e2big-as-einval")]
                Err(E2BIG) => return Err(EINVAL),
                Err(errno) => return Err(errno),
            };
        used += written;
        count += 1;
    }
}

/// [`common::ext2::Ext2Error`] を errno へ写す（S10-b）。
///
/// # 対応表はここに置く
///
/// **`common` は errno を知らない。** あちらは `no_std` の純粋ロジックで、
/// Linux の番号体系に依存しない（`common::elf` と同じ線である）。
/// **写すのは、Linux の形で答える責任を持つ側である。**
///
/// # 全 20 種を明示する
///
/// **`_ =>` で捨てない。** 捨てると、新しい種類を足したときに黙って
/// `-EIO` のようなものへ落ちる。**列挙が増えたらここが落ちる**ようにしておく。
fn errno_for_ext2(error: common::ext2::Ext2Error) -> i64 {
    use common::ext2::Ext2Error as E;
    match error {
        // 像そのものが読めない。**呼び出し側の引数の問題ではない。**
        E::TooShort
        | E::BadMagic
        | E::UnsupportedRevision(_)
        | E::BadBlockSizeShift(_)
        | E::BadInodeSize(_)
        | E::ZeroPerGroup
        | E::UnsupportedIncompatFeatures(_)
        | E::ImageTooSmall { .. }
        | E::BlockOutOfRange(_)
        | E::GroupDescriptorsOutOfRange
        | E::InodeOutOfRange(_)
        | E::InodeTableOutOfRange { .. }
        | E::FileBlockOutOfRange(_)
        | E::SparseBlock(_)
        | E::DirEntryTruncated { .. }
        | E::DirEntryMisaligned(_)
        | E::DirEntryRecordTooSmall { .. }
        | E::DirEntryRecordPastBlock { .. } => EIO,
        // 実装していない形。
        E::IndirectBlockUnsupported(_) => EIO,
        // ここから下は、呼び出し側の引数に対する答えである。
        E::NotADirectory(_) => ENOTDIR,
        E::NotFound => ENOENT,
        E::PathNotAbsolute => EINVAL,
        E::PathTooManyComponents(_) => ENAMETOOLONG,
    }
}

/// [`crate::userland::UserLoadError`] を errno へ写す（S11-5）。
///
/// # 全 16 種を明示する
///
/// **`_ =>` で捨てない**（[`errno_for_ext2`] と同じ理由）。
///
/// # 大半は「カーネル側の不具合」である
///
/// **`Parse` と `SegmentData` だけが、渡された像に対する答えである**——
/// 像が壊れているので `-ENOEXEC`……**ではなく `-EINVAL` を返す。**
/// `ENOEXEC`（8）をまだ持っておらず、**1 つの用途のために errno を増やすより、
/// 「引数が受け付けられない」に落とすほうが小さい。** 分ける必要が出たら足す。
fn errno_for_user_load(error: crate::userland::UserLoadError) -> i64 {
    use crate::userland::UserLoadError as E;
    match error {
        // 借りられない・入れる場所が無い。**時間を置けば変わりうる。**
        E::AllocatorUnavailable => EAGAIN,
        E::OutOfFrames => ENOMEM,
        // 渡されたものに対する答え。
        E::Parse(_) | E::SegmentData(_) => EINVAL,
        E::ArgumentsTooLong => ENAMETOOLONG,
        // ここから下はカーネル側の事情である。
        E::AddressSpace(_)
        | E::NotCanonical(_)
        | E::Mapping { .. }
        | E::LeafFlags { .. }
        | E::DidNotExit
        | E::NoExitNoFold
        | E::ExitStatus(_)
        | E::DidNotFold
        | E::FoldMismatch
        | E::AbiMismatch
        | E::DestroyAccounting { .. }
        | E::WriteMismatch => EIO,
    }
}

/// [`crate::userland::SpawnError`] を errno へ写す（S11-5）。
fn errno_for_spawn(error: crate::userland::SpawnError) -> i64 {
    use crate::userland::SpawnError as E;
    match error {
        E::TooDeep => EAGAIN,
        E::Lookup(e) | E::Read(e) => errno_for_ext2(e),
        E::IsDirectory => EISDIR,
        E::NotRegularFile => EACCES,
        E::TooLarge(_) => ENOMEM,
        E::ArgvMalformed => EINVAL,
        E::Load(e) => errno_for_user_load(e),
        E::DestroyAccounting { .. } => EIO,
    }
}

/// [`crate::vfs::FileTableError`] を errno へ写す（S10-b）。
fn errno_for_file_table(error: crate::vfs::FileTableError) -> i64 {
    match error {
        crate::vfs::FileTableError::NoFreeDescriptor => EMFILE,
        crate::vfs::FileTableError::BadDescriptor(_) => EBADF,
    }
}

/// [`SYS_WRITE`] が最後に記録した fd。
pub fn last_write_fd() -> u64 {
    state().write_fd.load(Ordering::SeqCst)
}

/// [`SYS_WRITE`] が最後に記録したバイト数。
pub fn last_write_len() -> usize {
    state().write_len.load(Ordering::SeqCst) as usize
}

/// [`SYS_WRITE`] が最後に記録したバイト列を `dst` へ写す。写した長さを返す。
pub fn last_write_bytes(dst: &mut [u8]) -> usize {
    let len = last_write_len().min(dst.len()).min(WRITE_BUF_LEN);
    for (slot, value) in dst.iter_mut().zip(state().write_buf.iter()).take(len) {
        *slot = value.load(Ordering::SeqCst);
    }
    len
}

/// 会計カウンタを 0 に戻す（往復検証の直前に呼ぶ）。
///
/// **終了の記録も戻す（S9-b-3-1）。** プロセスは順に 1 本ずつ走るので、
/// **前のプロセスの終了が次のプロセスのものとして読まれない**ようにする。
///
/// **`write` の記録も戻す（S9-b-3-2a）。** 戻していなかったので、
/// **`write` を発行しないプロセスについて「送っていない」を主張できなかった**
/// ——前のプロセスが送ったバイト列がそのまま残る。**1 本しか走らない間は
/// 差が出ないので、複数になって初めて要る**（`FAULT_CS` の戻し忘れと同じ形で、
/// `verification-coverage.md` に記録がある）。
pub fn reset_counters() {
    // **1 回だけ引く（W1-c-3。`syscall_entry` の同じ箇所の注記）。**
    let state = state();
    state.invocation_count.store(0, Ordering::SeqCst);
    state.last_number.store(0, Ordering::SeqCst);
    for slot in state.last_args.iter() {
        slot.store(0, Ordering::SeqCst);
    }
    state.handler_rsp.store(0, Ordering::SeqCst);
    state.in_ring3_at_entry.store(false, Ordering::SeqCst);
    state.process_exited.store(false, Ordering::SeqCst);
    state.process_exit_status.store(0, Ordering::SeqCst);
    state.write_fd.store(0, Ordering::SeqCst);
    state.write_len.store(0, Ordering::SeqCst);
    for slot in state.write_buf.iter() {
        slot.store(0, Ordering::SeqCst);
    }
    // **probe の記録も戻す（S9-b-3-2a）。** 起動時の battery が発行した probe の
    // 引数が、ユーザープログラムのものとして読まれないようにする
    // （`verification-coverage.md` の「1 つしかない間は、リセット漏れが観測できない」）。
    PROBE_INVOKED.store(false, Ordering::SeqCst);
    for slot in PROBE_SEEN_ARGS.iter() {
        slot.store(0, Ordering::SeqCst);
    }
}

/// [`reset_counters`] が戻すもの、ひとそろい（S11-5）。
///
/// # なぜ「戻す」だけでなく「控える」が要るのか
///
/// **記録は 1 組しかない。** プロセスが順に 1 本ずつ走る間はそれで足りた——
/// **次の 1 本が始まる前に、前の 1 本の判定が済んでいる。**
///
/// **`spawn` が入れ子を作ると、そうではなくなる。** 子は親の途中で走り、
/// **[`reset_counters`] で親の記録を 0 にし、自分の `write` と `exit` を上書きする。**
/// **親の判定行は、子が送ったバイト列を親のものとして読む。**
///
/// **控えて戻す**（`crate::ring3::FoldRecord` と同じ形。あちらは畳みの記録である）。
///
/// # 大きさは 256 バイトに満たない
///
/// **スタックへ置く**（[`MAX_EXECUTABLE_SIZE`] とは扱いが違う）。
/// 内訳は `u64` が 10 個、`[u64; 6]` が 2 つ、`[u8; 64]` が 1 つ、`bool` が 3 つで、
/// **詰め物を含めても 232 バイトである。**
/// **`deferred-decisions.md` の「大きなスタック配列とガード幅」が言う 4096 バイトの
/// 前提を破らない。**
#[derive(Debug, Clone, Copy)]
pub struct Records {
    invocation_count: u64,
    last_number: u64,
    last_args: [u64; 6],
    handler_rsp: u64,
    in_ring3_at_entry: bool,
    process_exited: bool,
    process_exit_status: u64,
    write_fd: u64,
    write_len: u64,
    write_buf: [u8; WRITE_BUF_LEN],
    probe_invoked: bool,
    probe_seen_args: [u64; 6],
}

/// 今の記録を控える（S11-5）。**[`reset_counters`] が戻す欄と 1 対 1 である。**
pub fn save_records() -> Records {
    // **1 回だけ引く（W1-c-3。`syscall_entry` の同じ箇所の注記）。**
    let state = state();
    let mut last_args = [0u64; 6];
    for (slot, value) in last_args.iter_mut().zip(state.last_args.iter()) {
        *slot = value.load(Ordering::SeqCst);
    }
    let mut write_buf = [0u8; WRITE_BUF_LEN];
    for (slot, value) in write_buf.iter_mut().zip(state.write_buf.iter()) {
        *slot = value.load(Ordering::SeqCst);
    }
    let mut probe_seen_args = [0u64; 6];
    for (slot, value) in probe_seen_args.iter_mut().zip(PROBE_SEEN_ARGS.iter()) {
        *slot = value.load(Ordering::SeqCst);
    }
    Records {
        invocation_count: state.invocation_count.load(Ordering::SeqCst),
        last_number: state.last_number.load(Ordering::SeqCst),
        last_args,
        handler_rsp: state.handler_rsp.load(Ordering::SeqCst),
        in_ring3_at_entry: state.in_ring3_at_entry.load(Ordering::SeqCst),
        process_exited: state.process_exited.load(Ordering::SeqCst),
        process_exit_status: state.process_exit_status.load(Ordering::SeqCst),
        write_fd: state.write_fd.load(Ordering::SeqCst),
        write_len: state.write_len.load(Ordering::SeqCst),
        write_buf,
        probe_invoked: PROBE_INVOKED.load(Ordering::SeqCst),
        probe_seen_args,
    }
}

/// 控えた記録を戻す（S11-5）。
pub fn restore_records(records: Records) {
    // **1 回だけ引く（W1-c-3。`syscall_entry` の同じ箇所の注記）。**
    let state = state();
    state
        .invocation_count
        .store(records.invocation_count, Ordering::SeqCst);
    state
        .last_number
        .store(records.last_number, Ordering::SeqCst);
    for (slot, value) in state.last_args.iter().zip(records.last_args.iter()) {
        slot.store(*value, Ordering::SeqCst);
    }
    state
        .handler_rsp
        .store(records.handler_rsp, Ordering::SeqCst);
    state
        .in_ring3_at_entry
        .store(records.in_ring3_at_entry, Ordering::SeqCst);
    state
        .process_exited
        .store(records.process_exited, Ordering::SeqCst);
    state
        .process_exit_status
        .store(records.process_exit_status, Ordering::SeqCst);
    state.write_fd.store(records.write_fd, Ordering::SeqCst);
    state.write_len.store(records.write_len, Ordering::SeqCst);
    for (slot, value) in state.write_buf.iter().zip(records.write_buf.iter()) {
        slot.store(*value, Ordering::SeqCst);
    }
    PROBE_INVOKED.store(records.probe_invoked, Ordering::SeqCst);
    for (slot, value) in PROBE_SEEN_ARGS.iter().zip(records.probe_seen_args.iter()) {
        slot.store(*value, Ordering::SeqCst);
    }
}

/// 今 Ring 3 が使っている窓を返す（S9-b-3-2b）。
pub fn user_window() -> (u64, u64) {
    (
        state().user_window_start.load(Ordering::SeqCst),
        state().user_window_end.load(Ordering::SeqCst),
    )
}

/// 窓を据え、**据える前の値を返す**（S9-b-3-2b）。
///
/// **戻すのは呼び出し側の責任である。** 現在の呼び出し元は
/// [`crate::ring3::enter`] だけで、あちらが遠征の前後で対にしている。
/// **入れ子になる**（S11 の `spawn` から。**以前ここは「入れ子にならない」と書いていた**）。
/// **前の値を返す形にしてあるので、入れ子でも壊れない。** **W1-c-3 から窓はスロットごとに持つので、
/// W1-c-4 で 2 本が同時に走っても据え合わない。**
pub fn set_user_window(start: u64, end: u64) -> (u64, u64) {
    let previous_start = state().user_window_start.swap(start, Ordering::SeqCst);
    let previous_end = state().user_window_end.swap(end, Ordering::SeqCst);
    (previous_start, previous_end)
}

/// [`PROBE_NUMBER`] が呼ばれたか（S9-b-3-2a）。
pub fn probe_invoked() -> bool {
    PROBE_INVOKED.load(Ordering::SeqCst)
}

/// [`PROBE_NUMBER`] の呼び出しで届いた 6 引数（S9-b-3-2a）。
pub fn probe_seen_args() -> [u64; 6] {
    core::array::from_fn(|i| PROBE_SEEN_ARGS[i].load(Ordering::SeqCst))
}

/// [`SYS_EXIT`] を受け取ったか（S9-b-3-1）。
pub fn process_exited() -> bool {
    state().process_exited.load(Ordering::SeqCst)
}

/// [`SYS_EXIT`] が受け取った終了状態。[`process_exited`] が真のときだけ意味を持つ。
pub fn process_exit_status() -> u64 {
    state().process_exit_status.load(Ordering::SeqCst)
}

/// `syscall_entry` が呼ばれた回数。
pub fn invocation_count() -> u64 {
    state().invocation_count.load(Ordering::SeqCst)
}

/// 直近に受け取った番号（RAX）。
pub fn last_number() -> u64 {
    state().last_number.load(Ordering::SeqCst)
}

/// 直近に受け取った 6 引数（RDI/RSI/RDX/R10/R8/R9 の順）。
pub fn last_args() -> [u64; 6] {
    core::array::from_fn(|i| state().last_args[i].load(Ordering::SeqCst))
}

/// `syscall_entry` が走ったときの RSP。RSP0 スタック範囲との照合に使う。
pub fn handler_rsp() -> u64 {
    state().handler_rsp.load(Ordering::SeqCst)
}

/// 入場時点で「今 Ring 3 にいる」が立っていたか（S8-b）。
pub fn in_ring3_at_entry() -> bool {
    state().in_ring3_at_entry.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    //! 画面の `ioctl` が返す構造体の配置（`ADR-0066` の Y-c）。**`cc` の `offsetof` で測った値を
    //! 機械で留める**（2026-09-21。`cc` 13.3.0。`<linux/fb.h>` と `<drm/drm.h>`）。**欄の位置を
    //! 動かすと、Linux の配置から外れたことがここで分かる。**

    use super::*;

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// `struct fb_var_screeninfo` の欄の位置（`offsetof` の値）。
    #[test]
    fn the_variable_screen_info_follows_the_linux_layout() {
        let out = fb_var_screeninfo(1280, 800, true);
        assert_eq!(out.len(), 160, "sizeof(struct fb_var_screeninfo)");
        assert_eq!(u32_at(&out, 0), 1280, "xres @0");
        assert_eq!(u32_at(&out, 4), 800, "yres @4");
        assert_eq!(u32_at(&out, 8), 1280, "xres_virtual @8");
        assert_eq!(u32_at(&out, 12), 800, "yres_virtual @12");
        assert_eq!(u32_at(&out, 24), 32, "bits_per_pixel @24");
        // **`Bgr` は「バイト 0 が青」——青 0・緑 8・赤 16。**
        assert_eq!(u32_at(&out, 32), 16, "red.offset @32");
        assert_eq!(u32_at(&out, 36), 8, "red.length @36");
        assert_eq!(u32_at(&out, 44), 8, "green.offset @44");
        assert_eq!(u32_at(&out, 56), 0, "blue.offset @56");
        assert_eq!(u32_at(&out, 60), 8, "blue.length @60");
    }

    /// `Rgb` では赤と青の位置が入れ替わる。
    #[test]
    fn the_color_offsets_follow_the_pixel_order() {
        assert_eq!(fb_color_offsets(true), (0, 8, 16));
        assert_eq!(fb_color_offsets(false), (16, 8, 0));
        let out = fb_var_screeninfo(1280, 800, false);
        assert_eq!(u32_at(&out, 32), 0, "red.offset for Rgb");
        assert_eq!(u32_at(&out, 56), 16, "blue.offset for Rgb");
    }

    /// `struct fb_fix_screeninfo` の欄の位置。**物理番地（`smem_start`）は 0 のままである。**
    #[test]
    fn the_fixed_screen_info_follows_the_linux_layout() {
        let out = fb_fix_screeninfo(4_096_000, 5120);
        assert_eq!(out.len(), 80, "sizeof(struct fb_fix_screeninfo)");
        assert_eq!(&out[..9], b"zaytos-fb", "id @0");
        assert_eq!(&out[16..24], &[0u8; 8], "smem_start @16 is not given out");
        assert_eq!(u32_at(&out, 24), 4_096_000, "smem_len @24");
        assert_eq!(u32_at(&out, 28), FB_TYPE_PACKED_PIXELS, "type @28");
        assert_eq!(u32_at(&out, 36), FB_VISUAL_TRUECOLOR, "visual @36");
        assert_eq!(u32_at(&out, 48), 5120, "line_length @48");
    }

    /// `struct drm_clip_rect` は半開区間である。**空の矩形は断る。**
    #[test]
    fn a_clip_rect_is_half_open_and_refuses_empty_ones() {
        let rect = |x1: u16, y1: u16, x2: u16, y2: u16| {
            let mut raw = [0u8; DRM_CLIP_RECT_LEN];
            raw[0..2].copy_from_slice(&x1.to_le_bytes());
            raw[2..4].copy_from_slice(&y1.to_le_bytes());
            raw[4..6].copy_from_slice(&x2.to_le_bytes());
            raw[6..8].copy_from_slice(&y2.to_le_bytes());
            raw
        };
        assert_eq!(
            parse_clip_rect(&rect(200, 200, 360, 360)),
            Some((200, 200, 160, 160))
        );
        assert_eq!(parse_clip_rect(&rect(0, 0, 1, 1)), Some((0, 0, 1, 1)));
        assert_eq!(parse_clip_rect(&rect(10, 10, 10, 20)), None, "width 0");
        assert_eq!(parse_clip_rect(&rect(10, 20, 20, 10)), None, "upside down");
    }
}
