//! x86_64 の Linux のシステムコールの番号（`asm/unistd_64.h`。`ADR-0071` の決定 1 の 2 で `crate::syscall` から移した。
//! 2026-09-30）。
//!
//! **Linux に同じ入口がある呼び出しは、Linux の番号をそのまま使う**（`ADR-0020` の Addendum）。**ここには番号と、
//! どのヘッダの値かの 1 行だけを置く**——受ける形・断り方・採らない入口などの振る舞いの説明は、`crate::syscall` の
//! 処理の側にある。Linux に無い独自の番号は [`crate::abi::private`] にある。
//!
//! **CPU によって違う**——aarch64 は番号が違い、`open`・`stat`・`poll`・`mkdir`・`rmdir`・`unlink` の入口が無い
//! （`openat`・`newfstatat`・`ppoll`・`mkdirat`・`unlinkat` の形だけがある。2026-09-30 に `aarch64-linux-gnu-gcc` と
//! クロスのヘッダで測った）。

/// `write(fd, buf, len)` の番号（S9-b-1。`asm/unistd_64.h` の `__NR_write`）。
pub const SYS_WRITE: u64 = 1;

/// `exit(status)` の番号（S9-b-3-1。`asm/unistd_64.h` の `__NR_exit`）。
pub const SYS_EXIT: u64 = 60;

/// `read(fd, buf, count)` の番号（S10-b。`asm/unistd_64.h` の `__NR_read`）。
pub const SYS_READ: u64 = 0;

/// `getdents64(fd, dirp, count)` の番号（S10-b。`asm/unistd_64.h` の `__NR_getdents64`）。
pub const SYS_GETDENTS64: u64 = 217;

/// `stat(path, statbuf)` の番号（S10-b。`asm/unistd_64.h` の `__NR_stat`）。
pub const SYS_STAT: u64 = 4;

/// `clock_gettime(clockid, timespec)` の番号（W2-d+。`asm/unistd_64.h` の `__NR_clock_gettime`）。
pub const SYS_CLOCK_GETTIME: u64 = 228;

/// `nanosleep(req, rem)` の番号（W2-d+。`asm/unistd_64.h` の `__NR_nanosleep`）。
pub const SYS_NANOSLEEP: u64 = 35;

/// `open(path, flags, mode)` の番号（S10-b。`asm/unistd_64.h` の `__NR_open`）。
pub const SYS_OPEN: u64 = 2;

/// `close(fd)` の番号（S10-b。`asm/unistd_64.h` の `__NR_close`）。
pub const SYS_CLOSE: u64 = 3;

/// `ioctl(fd, request, arg)` の番号（e-1。`asm/unistd_64.h` の `__NR_ioctl`）。
pub const SYS_IOCTL: u64 = 16;

/// `lseek` の番号（DIR-1b。`asm/unistd_64.h` の `__NR_lseek`）。
pub const SYS_LSEEK: u64 = 8;

/// `mkdir` の番号（DIR-1c。`asm/unistd_64.h` の `__NR_mkdir`）。
pub const SYS_MKDIR: u64 = 83;

/// `rmdir` の番号（DIR-1c。`asm/unistd_64.h` の `__NR_rmdir`）。
pub const SYS_RMDIR: u64 = 84;

/// `brk` の番号（H-a。ADR-0044。`asm/unistd_64.h` の `__NR_brk`）。
pub const SYS_BRK: u64 = 12;

/// `unlink` の番号（DIR-1b。`asm/unistd_64.h` の `__NR_unlink`）。
pub const SYS_UNLINK: u64 = 87;

/// `socket` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_socket`）。
pub const SYS_SOCKET: u64 = 41;

/// `connect` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_connect`）。
pub const SYS_CONNECT: u64 = 42;

/// `accept` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_accept`）。
pub const SYS_ACCEPT: u64 = 43;

/// `bind` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_bind`）。
pub const SYS_BIND: u64 = 49;

/// `listen` の番号（`ADR-0064`。`asm/unistd_64.h` の `__NR_listen`）。
pub const SYS_LISTEN: u64 = 50;

/// `poll` の番号（`ADR-0066` の Y-b。`asm/unistd_64.h` の `__NR_poll`）。
pub const SYS_POLL: u64 = 7;

/// `mmap` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_mmap`）。
pub const SYS_MMAP: u64 = 9;

/// `ftruncate` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_ftruncate`）。
pub const SYS_FTRUNCATE: u64 = 77;

/// `sendmsg` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_sendmsg`）。
pub const SYS_SENDMSG: u64 = 46;

/// `recvmsg` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_recvmsg`）。
pub const SYS_RECVMSG: u64 = 47;

/// `memfd_create` の番号（`ADR-0065`。`asm/unistd_64.h` の `__NR_memfd_create`）。
pub const SYS_MEMFD_CREATE: u64 = 319;
