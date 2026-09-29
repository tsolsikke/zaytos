//! x86_64 の Linux の ABI（システムコールの番号・引数のレジスタ・構造体の配置・errno。`ADR-0071` の
//! 決定 1 の 2 で、共通の側から移す）。

mod errno;

pub use errno::{
    E2BIG, EACCES, EADDRINUSE, EAFNOSUPPORT, EAGAIN, EBADF, EBUSY, ECHILD, ECONNREFUSED, EEXIST,
    EFAULT, EINVAL, EIO, EISCONN, EISDIR, EMFILE, EMSGSIZE, ENAMETOOLONG, ENOBUFS, ENODEV, ENOENT,
    ENOMEM, ENOSPC, ENOSYS, ENOTCONN, ENOTDIR, ENOTEMPTY, ENOTSOCK, ENOTTY, EPIPE, EPROTONOSUPPORT,
    EROFS, ESPIPE,
};

mod layout;

pub use layout::{
    dirent64_record, dirent64_record_len, fb_fix_screeninfo, fb_var_screeninfo, parse_clip_rect,
    parse_pollfd, parse_sockaddr_un, parse_timespec, set_pollfd_revents, stat_bytes,
    timespec_bytes, winsize_bytes, Dirent64, Pollfd, SockaddrUn, Stat, Timespec, Winsize,
    DIRENT64_ALIGN, DIRENT64_HEADER_LEN, DRM_CLIP_RECT_LEN, FB_FIX_SCREENINFO_LEN,
    FB_VAR_SCREENINFO_LEN, POLLFD_LEN, SOCKADDR_UN_LEN, STAT_LEN, TIMESPEC_LEN, WINSIZE_LEN,
};

mod values;

pub use values::{
    AF_UNIX, CLOCK_MONOTONIC, DT_DIR, DT_REG, DT_UNKNOWN, FBIOGET_FSCREENINFO, FBIOGET_VSCREENINFO,
    FB_TYPE_PACKED_PIXELS, FB_VISUAL_TRUECOLOR, O_ACCMODE, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY,
    POLLIN, PROT_WRITE, SCM_RIGHTS, SEEK_SET, SOCK_STREAM, SOL_SOCKET, TIOCGWINSZ,
};

mod registers;

pub use registers::{read_request, write_return, SyscallRequest};
