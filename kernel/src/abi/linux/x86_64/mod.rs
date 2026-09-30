//! x86_64 の Linux の ABI のうち、CPU によって違うもの（システムコールの番号・引数のレジスタ・`struct stat` の配置。
//! `ADR-0071` の決定 1 の 2 で、共通の側から移し、CPU によらないものを [`super`] へ分けた。2026-09-30）。

mod layout;

pub use layout::{stat_bytes, STAT_LEN};

mod numbers;

pub use numbers::{
    SYS_ACCEPT, SYS_BIND, SYS_BRK, SYS_CLOCK_GETTIME, SYS_CLOSE, SYS_CONNECT, SYS_EXIT,
    SYS_FTRUNCATE, SYS_GETDENTS64, SYS_IOCTL, SYS_LISTEN, SYS_LSEEK, SYS_MEMFD_CREATE, SYS_MKDIR,
    SYS_MMAP, SYS_NANOSLEEP, SYS_OPEN, SYS_POLL, SYS_READ, SYS_RECVMSG, SYS_RMDIR, SYS_SENDMSG,
    SYS_SOCKET, SYS_STAT, SYS_UNLINK, SYS_WRITE,
};

mod registers;

pub use registers::{read_request, write_return, SENTINEL_RCX};
