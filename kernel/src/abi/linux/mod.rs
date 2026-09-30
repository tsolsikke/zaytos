//! Linux と同じ形の ABI（`ADR-0069` の 4 節）。**CPU によらないものはここに、CPU によって違うもの（システムコールの
//! 番号・引数のレジスタ・`struct stat` の配置）は CPU ごとの置き場（[`x86_64`]）に置く**（`ADR-0071` の決定 1 の 2。
//! 2026-09-30）。**ここにあるものは、x86_64 と aarch64 のヘッダで同じ定義であることを測って確かめた**（各モジュールの
//! doc）。

mod errno;

pub use errno::{
    E2BIG, EACCES, EADDRINUSE, EAFNOSUPPORT, EAGAIN, EBADF, EBUSY, ECHILD, ECONNREFUSED, EEXIST,
    EFAULT, EINVAL, EIO, EISCONN, EISDIR, EMFILE, EMSGSIZE, ENAMETOOLONG, ENOBUFS, ENODEV, ENOENT,
    ENOMEM, ENOSPC, ENOSYS, ENOTCONN, ENOTDIR, ENOTEMPTY, ENOTSOCK, ENOTTY, EPIPE, EPROTONOSUPPORT,
    EROFS, ESPIPE,
};

mod initial_stack;

pub use initial_stack::build_initial_stack;

mod layout;

pub use layout::{
    cmsg_one_fd_bytes, dirent64_record, dirent64_record_len, fb_fix_screeninfo_bytes,
    fb_var_screeninfo_bytes, parse_cmsg_one_fd, parse_drm_clip_rect, parse_iovec, parse_msghdr,
    parse_pollfd, parse_sockaddr_un, parse_timespec, set_pollfd_revents, timespec_bytes,
    winsize_bytes, CmsgOneFd, Dirent64, DrmClipRect, FbBitfield, FbFixScreeninfo, FbVarScreeninfo,
    Iovec, Msghdr, Pollfd, SockaddrUn, Stat, Timespec, Winsize, CMSG_ONE_FD_LEN, DIRENT64_ALIGN,
    DIRENT64_HEADER_LEN, DRM_CLIP_RECT_LEN, FB_FIX_SCREENINFO_LEN, FB_VAR_SCREENINFO_LEN,
    IOVEC_LEN, MSGHDR_LEN, POLLFD_LEN, SOCKADDR_UN_LEN, TIMESPEC_LEN, WINSIZE_LEN,
};

mod request;

pub use request::SyscallRequest;

mod values;

pub use values::{
    AF_UNIX, CLOCK_MONOTONIC, DT_DIR, DT_REG, DT_UNKNOWN, FBIOGET_FSCREENINFO, FBIOGET_VSCREENINFO,
    FB_TYPE_PACKED_PIXELS, FB_VISUAL_TRUECOLOR, O_ACCMODE, O_APPEND, O_CREAT, O_RDONLY, O_TRUNC,
    O_WRONLY, POLLIN, PROT_WRITE, SCM_RIGHTS, SEEK_SET, SOCK_STREAM, SOL_SOCKET, TIOCGWINSZ,
};

pub mod x86_64;
