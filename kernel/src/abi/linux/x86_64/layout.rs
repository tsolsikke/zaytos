//! Linux の構造体の配置（長さと欄の位置。x86_64 の Linux の値。`ADR-0071` の決定 1 の 2 で、`crate::syscall` から
//! 移した。2026-09-30）。並びと doc は移す前のまま。

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
pub const DIRENT64_ALIGN: usize = 8;

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
pub const STAT_INO: usize = 8;

pub const STAT_NLINK: usize = 16;

pub const STAT_MODE: usize = 24;

pub const STAT_SIZE: usize = 48;

pub const STAT_BLOCKS: usize = 64;

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
pub const TIMESPEC_NSEC: usize = 8;

/// `struct winsize` の大きさ（e-1）。**`u16` が 4 つである**
/// （実測。`/usr/include/x86_64-linux-gnu/bits/ioctl-types.h`。
/// 順に `ws_row` / `ws_col` / `ws_xpixel` / `ws_ypixel`）。
pub const WINSIZE_LEN: usize = 8;

/// `struct fb_var_screeninfo` のバイト数（`cc` の `sizeof` で測った。2026-09-21）。
pub const FB_VAR_SCREENINFO_LEN: usize = 160;

/// `struct fb_fix_screeninfo` のバイト数（同上）。
pub const FB_FIX_SCREENINFO_LEN: usize = 80;

/// `struct drm_clip_rect` のバイト数（同上。`u16` の x1・y1・x2・y2）。
pub const DRM_CLIP_RECT_LEN: usize = 8;

/// `sockaddr_un` の大きさ（`sa_family_t` 2 + `sun_path` 108。Linux の配置）。
pub const SOCKADDR_UN_LEN: u64 = 110;

/// `struct pollfd` のバイト数（`fd` 4＋`events` 2＋`revents` 2。Linux の配置）。
pub const POLLFD_LEN: usize = 8;
