//! Linux の構造体の配置（長さと欄の位置、構造体を組む関数と読む関数。x86_64 の Linux の値。
//! `ADR-0071` の決定 1 の 2 で、`crate::syscall` から移した。2026-09-30）。移したものの並びと doc は移す前のまま。

use super::values::{FB_TYPE_PACKED_PIXELS, FB_VISUAL_TRUECOLOR};

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

/// `struct stat` に書く値。**ZaytOS が持つ欄だけである**（ほかの欄は 0 のまま返す）。
///
/// **値を決めるのは共通の側で、[`stat_bytes`] は欄の位置へ書くだけである**（`ADR-0071` の決定 1 の 2 で、
/// `crate::syscall` の `sys_stat` から分けた。2026-09-30）。
pub struct Stat {
    /// `st_ino`（inode の番号）。
    pub ino: u64,
    /// `st_nlink`（リンクの数）。
    pub nlink: u64,
    /// `st_mode`（種類と許可のビット）。
    pub mode: u32,
    /// `st_size`（バイト数）。
    pub size: u64,
    /// `st_blocks`（**512 バイト単位の数**）。
    pub blocks: u64,
}

/// `struct stat` を組む（欄の位置は [`STAT_LEN`] の表）。
///
/// **0 で埋めてから、分かる欄だけを書く。** 「書かなかった欄は 0」が
/// 構造で決まるので、埋め忘れが未定義の値として出ない。
pub fn stat_bytes(stat: &Stat) -> [u8; STAT_LEN] {
    let mut out = [0u8; STAT_LEN];
    out[STAT_INO..STAT_INO + 8].copy_from_slice(&stat.ino.to_le_bytes());
    out[STAT_NLINK..STAT_NLINK + 8].copy_from_slice(&stat.nlink.to_le_bytes());
    out[STAT_MODE..STAT_MODE + 4].copy_from_slice(&stat.mode.to_le_bytes());
    out[STAT_SIZE..STAT_SIZE + 8].copy_from_slice(&stat.size.to_le_bytes());
    out[STAT_BLOCKS..STAT_BLOCKS + 8].copy_from_slice(&stat.blocks.to_le_bytes());
    out
}

/// `struct winsize` に書く値（`TIOCGWINSZ`）。
///
/// **値を決めるのは共通の側で、[`winsize_bytes`] は欄の位置へ書くだけである**（`ADR-0071` の決定 1 の 2 で、
/// `crate::syscall` の `sys_ioctl` から分けた。2026-09-30）。
pub struct Winsize {
    /// `ws_row`（行の数）。
    pub row: u16,
    /// `ws_col`（桁の数）。
    pub col: u16,
    /// `ws_xpixel`（横の画素の数）。
    pub xpixel: u16,
    /// `ws_ypixel`（縦の画素の数）。
    pub ypixel: u16,
}

/// `struct winsize` を組む（欄の並びは [`WINSIZE_LEN`] の doc）。
pub fn winsize_bytes(winsize: &Winsize) -> [u8; WINSIZE_LEN] {
    let mut out = [0u8; WINSIZE_LEN];
    out[0..2].copy_from_slice(&winsize.row.to_le_bytes());
    out[2..4].copy_from_slice(&winsize.col.to_le_bytes());
    out[4..6].copy_from_slice(&winsize.xpixel.to_le_bytes());
    out[6..8].copy_from_slice(&winsize.ypixel.to_le_bytes());
    out
}

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
/// **`smem_start`（物理アドレス）は 0 にする**——**合わせなかった。** **Ring 3 へ物理アドレスを出す理由が
/// 無い**（`mmap` は fd からマップするので、アドレスを知らなくてよい）。
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
/// **x2・y2 は含まない**（DRM の DIRTYFB と同じ半開区間）。**画面への切り詰めはコピーする側が行う**
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

#[cfg(test)]
mod tests {
    //! 構造体の配置。**`cc` の `offsetof` で測った値を機械で留める**（画面の `ioctl` の構造体は `ADR-0066` の Y-c。
    //! 2026-09-21。`cc` 13.3.0。`<linux/fb.h>` と `<drm/drm.h>`。`struct stat` は [`STAT_LEN`] の表）。**欄の位置を
    //! 動かすと、Linux の配置から外れたことがここで分かる。**

    use super::*;

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[at..at + 8]);
        u64::from_le_bytes(raw)
    }

    /// `struct stat` の欄の位置（[`STAT_LEN`] の表の値を、定数を通さずに書く。Linux の x86_64 の `asm/stat.h` を `gcc` の
    /// `offsetof` で測って確かめた。2026-09-30。`linux-libc-dev` 6.8.0。`sys/stat.h` も同じ）。**書かない欄は 0 である。**
    ///
    /// **値は、どのバイトも 0 でなく、欄ごとに違う形にする。** 小さい値では上のバイトが 0 になり、欄の幅の途中までしか
    /// 書かない誤りが見えない（`st_size` の下位 4 バイトだけを書く形で、以前の値 5000 は通った）。
    #[test]
    fn the_stat_bytes_follow_the_linux_layout() {
        let out = stat_bytes(&Stat {
            ino: 0x0102_0304_0506_0708,
            nlink: 0x1112_1314_1516_1718,
            mode: 0x2122_2324,
            size: 0x3132_3334_3536_3738,
            blocks: 0x4142_4344_4546_4748,
        });
        assert_eq!(out.len(), 144, "sizeof(struct stat)");
        assert_eq!(u64_at(&out, 8), 0x0102_0304_0506_0708, "st_ino @8");
        assert_eq!(u64_at(&out, 16), 0x1112_1314_1516_1718, "st_nlink @16");
        assert_eq!(u32_at(&out, 24), 0x2122_2324, "st_mode @24");
        assert_eq!(u64_at(&out, 48), 0x3132_3334_3536_3738, "st_size @48");
        assert_eq!(u64_at(&out, 64), 0x4142_4344_4546_4748, "st_blocks @64");
        let mut rest = out;
        for (at, len) in [(8, 8), (16, 8), (24, 4), (48, 8), (64, 8)] {
            rest[at..at + len].fill(0);
        }
        assert_eq!(rest, [0u8; 144], "the other fields are 0");
    }

    /// `struct winsize` の欄の位置（Linux の x86_64 の `asm/termios.h` を `gcc` の `offsetof` で測って確かめた。
    /// 2026-09-30。`sys/ioctl.h` も同じ）。**値は、どのバイトも 0 でなく、欄ごとに違う形にする**（stat のテストと同じ理由）。
    #[test]
    fn the_winsize_bytes_follow_the_linux_layout() {
        let out = winsize_bytes(&Winsize {
            row: 0x0102,
            col: 0x1112,
            xpixel: 0x2122,
            ypixel: 0x3132,
        });
        assert_eq!(out.len(), 8, "sizeof(struct winsize)");
        assert_eq!(u16_at(&out, 0), 0x0102, "ws_row @0");
        assert_eq!(u16_at(&out, 2), 0x1112, "ws_col @2");
        assert_eq!(u16_at(&out, 4), 0x2122, "ws_xpixel @4");
        assert_eq!(u16_at(&out, 6), 0x3132, "ws_ypixel @6");
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

    /// `struct fb_fix_screeninfo` の欄の位置。**物理アドレス（`smem_start`）は 0 のままである。**
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
