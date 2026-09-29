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

/// `struct timespec` の値（`clock_gettime` が書き、`nanosleep` が読む）。
///
/// **値を決めるのも、値の範囲を見るのも共通の側で、[`timespec_bytes`] と [`parse_timespec`] は欄の位置との間で
/// 変換するだけである**（`ADR-0071` の決定 1 の 2 で、`crate::syscall` の `sys_clock_gettime` と `sys_nanosleep` から
/// 分けた。2026-09-30）。
pub struct Timespec {
    /// `tv_sec`（秒）。
    pub sec: i64,
    /// `tv_nsec`（ナノ秒）。
    pub nsec: i64,
}

/// `struct timespec` を組む（欄の位置は [`TIMESPEC_LEN`] の表）。
pub fn timespec_bytes(timespec: &Timespec) -> [u8; TIMESPEC_LEN] {
    let mut out = [0u8; TIMESPEC_LEN];
    out[..TIMESPEC_NSEC].copy_from_slice(&timespec.sec.to_le_bytes());
    out[TIMESPEC_NSEC..].copy_from_slice(&timespec.nsec.to_le_bytes());
    out
}

/// `struct timespec` を読む（欄の位置は [`TIMESPEC_LEN`] の表）。
pub fn parse_timespec(raw: &[u8; TIMESPEC_LEN]) -> Timespec {
    let mut sec = [0u8; 8];
    sec.copy_from_slice(&raw[..TIMESPEC_NSEC]);
    let mut nsec = [0u8; 8];
    nsec.copy_from_slice(&raw[TIMESPEC_NSEC..]);
    Timespec {
        sec: i64::from_le_bytes(sec),
        nsec: i64::from_le_bytes(nsec),
    }
}

/// `sockaddr_un` から読んだ値（`bind` と `connect`。`ADR-0064`）。
///
/// **欄から読むのはここで、断るかどうか（種類・空の名前・長さ）を決めるのは共通の側である**（`ADR-0071` の決定 1 の 2 で、
/// `crate::syscall` の `read_socket_name` から分けた。2026-09-30）。
pub struct SockaddrUn<'a> {
    /// `sun_family`。
    pub family: u16,
    /// `sun_path` の名前。**先頭から最初の NUL まで、NUL が無ければ渡された長さの終わりまでである**（Linux の形）。
    /// **抽象名（先頭が NUL）は空になる。**
    pub path: &'a [u8],
}

/// `sockaddr_un` を読む（`sun_family` 2 バイト、続いて `sun_path`。[`SOCKADDR_UN_LEN`]）。**`raw` は、利用者が渡した
/// 長さ（`addrlen`）の分である。** `sun_family` に届かない長さなら `None`。
pub fn parse_sockaddr_un(raw: &[u8]) -> Option<SockaddrUn<'_>> {
    let (family, rest) = raw.split_first_chunk::<2>()?;
    let len = rest
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(rest.len());
    Some(SockaddrUn {
        family: u16::from_le_bytes(*family),
        path: &rest[..len],
    })
}

/// `struct pollfd` から読んだ値（`fd` と `events`。`ADR-0066` の Y-b）。
///
/// **欄から読むのと `revents` を書くのはここで、受けるかどうかと、どの欄に印を付けるかを決めるのは共通の側である**
/// （`ADR-0071` の決定 1 の 2 で、`crate::syscall` の `poll_from_ring3` から分けた。2026-09-30）。
pub struct Pollfd {
    /// `fd`（符号つき 32 ビット）。
    pub fd: i32,
    /// `events`（待つ事象のビット）。
    pub events: u16,
}

/// `struct pollfd` の `fd` と `events` を読む（欄の並びは [`POLLFD_LEN`] の doc）。
pub fn parse_pollfd(raw: &[u8; POLLFD_LEN]) -> Pollfd {
    Pollfd {
        fd: i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
        events: u16::from_le_bytes([raw[4], raw[5]]),
    }
}

/// `struct pollfd` の `revents` を書く。**ほかの欄は触らない**（利用者へ書き戻すとき、`fd` と `events` は読んだまま）。
pub fn set_pollfd_revents(raw: &mut [u8; POLLFD_LEN], revents: u16) {
    raw[6..8].copy_from_slice(&revents.to_le_bytes());
}

/// `linux_dirent64` の 1 レコードに書く値（`getdents64`）。
///
/// **値と、収まるかどうかの判断は共通の側で、長さの規則（[`dirent64_record_len`]）と欄の位置（[`dirent64_record`]）は
/// ここである**（`ADR-0071` の決定 1 の 2 で、`crate::syscall` の `sys_getdents64` から分けた。2026-09-30）。
pub struct Dirent64<'a> {
    /// `d_ino`（inode の番号）。
    pub ino: u64,
    /// `d_off`（次のレコードの位置）。
    pub off: u64,
    /// `d_type`（`DT_REG` など）。
    pub kind: u8,
    /// `d_name`（NUL を含まない。書くときに NUL を足す）。
    pub name: &'a [u8],
}

/// 名前の長さから、レコードの長さ（`d_reclen`）を求める。**名前の NUL 終端を数え、8 バイト境界へ切り上げる**
/// （[`DIRENT64_ALIGN`]）。
pub fn dirent64_record_len(name_len: usize) -> usize {
    let needed = DIRENT64_HEADER_LEN + name_len + 1;
    // 破壊テスト (S10-b, dirent-no-align): 切り上げをやめる。**こちらの走査は
    // `d_reclen` を頼りに歩くので、外しても自分では気づけない。** 整列は
    // 呼び出し側との約束なので、**約束を見ている検算だけが検出する。**
    //
    // **S10-b の他の 3 つとは種類が違う。** `eisdir-as-enotdir`・
    // `read-no-advance`・`stat-blocks-in-bytes` は**値が間違っている**形で、
    // 正しい値を知っていれば突き合わせられる。**こちらは値ではなく、
    // 呼び出し側との約束の違反である**——どの値が返るかは変わらず、
    // **返り方の規則だけが崩れる。** 突き合わせる相手は「正しい値」ではなく
    // 「約束」なので、**約束を明文で検査していなければ、何も落ちない。**
    if cfg!(feature = "syscall-test-dirent-no-align") {
        needed
    } else {
        needed.next_multiple_of(DIRENT64_ALIGN)
    }
}

/// 1 レコードを `out` の頭へ書き、書いた長さ（`d_reclen`）を返す（欄の位置は [`DIRENT64_HEADER_LEN`] の doc）。
///
/// **書く範囲は先に 0 で埋める**（名前の後ろの NUL と詰め物）。**`out` に収まらなければ、何も書かずに `None`。**
pub fn dirent64_record(entry: &Dirent64, out: &mut [u8]) -> Option<usize> {
    let reclen = dirent64_record_len(entry.name.len());
    let name_end = DIRENT64_HEADER_LEN + entry.name.len();
    if name_end + 1 > out.len() || reclen > out.len() {
        return None;
    }
    let record = &mut out[..reclen];
    record.fill(0);
    record[0..8].copy_from_slice(&entry.ino.to_le_bytes());
    record[8..16].copy_from_slice(&entry.off.to_le_bytes());
    record[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
    record[18] = entry.kind;
    record[DIRENT64_HEADER_LEN..name_end].copy_from_slice(entry.name);
    Some(reclen)
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

    /// `struct timespec` の欄の位置（Linux の x86_64 の `linux/time_types.h` の `__kernel_timespec` を `gcc` の `offsetof` で
    /// 測って確かめた。2026-09-30。`time.h` の `struct timespec` も同じ）。**値は、どのバイトも 0 でなく、欄ごとに違う形に
    /// する**（stat のテストと同じ理由）。**読む側は、書いた値をそのまま返す**（どちらの欄も符号つきである）。
    #[test]
    fn the_timespec_bytes_follow_the_linux_layout() {
        let out = timespec_bytes(&Timespec {
            sec: 0x0102_0304_0506_0708,
            nsec: 0x1112_1314_1516_1718,
        });
        assert_eq!(out.len(), 16, "sizeof(struct timespec)");
        assert_eq!(u64_at(&out, 0), 0x0102_0304_0506_0708, "tv_sec @0");
        assert_eq!(u64_at(&out, 8), 0x1112_1314_1516_1718, "tv_nsec @8");
        let read = parse_timespec(&out);
        assert_eq!(
            (read.sec, read.nsec),
            (0x0102_0304_0506_0708, 0x1112_1314_1516_1718)
        );
        let negative = parse_timespec(&timespec_bytes(&Timespec { sec: -1, nsec: -2 }));
        assert_eq!(
            (negative.sec, negative.nsec),
            (-1, -2),
            "the fields are signed"
        );
    }

    /// `sockaddr_un` の欄の位置（Linux の x86_64 の `linux/un.h` を `gcc` の `offsetof` で測って確かめた。2026-09-30。
    /// `sys/un.h` も同じ）。**名前は最初の NUL まで、NUL が無ければ渡された長さの終わりまでである。**
    #[test]
    fn a_sockaddr_un_is_read_up_to_the_first_nul() {
        let mut raw = [0u8; 110];
        raw[0..2].copy_from_slice(&0x0102u16.to_le_bytes());
        raw[2..6].copy_from_slice(b"sock");
        let read = parse_sockaddr_un(&raw).unwrap();
        assert_eq!(read.family, 0x0102, "sun_family @0");
        assert_eq!(read.path, b"sock", "sun_path @2, up to the first NUL");
        let unterminated = parse_sockaddr_un(&raw[..5]).unwrap();
        assert_eq!(
            unterminated.path, b"soc",
            "without a NUL, up to the given length"
        );
        raw[2] = 0;
        assert_eq!(
            parse_sockaddr_un(&raw).unwrap().path,
            b"",
            "an abstract name reads as empty"
        );
        assert!(
            parse_sockaddr_un(&raw[..1]).is_none(),
            "shorter than sun_family"
        );
    }

    /// `struct pollfd` の欄の位置（Linux の x86_64 の `asm/poll.h` を `gcc` の `offsetof` で測って確かめた。2026-09-30。
    /// `poll.h` も同じ）。**`revents` を書いても、ほかの欄は変わらない。**
    #[test]
    fn the_pollfd_fields_follow_the_linux_layout() {
        let mut raw = [0x01, 0x02, 0x03, 0x04, 0x11, 0x12, 0x21, 0x22];
        let read = parse_pollfd(&raw);
        assert_eq!(read.fd, 0x0403_0201, "fd @0");
        assert_eq!(read.events, 0x1211, "events @4");
        set_pollfd_revents(&mut raw, 0x3132);
        assert_eq!(
            raw,
            [0x01, 0x02, 0x03, 0x04, 0x11, 0x12, 0x32, 0x31],
            "revents @6, the rest unchanged"
        );
        assert_eq!(parse_pollfd(&[0xff; 8]).fd, -1, "fd is signed");
    }

    /// `linux_dirent64` の欄の位置（`glibc` の `dirent.h` の `struct dirent64` を `gcc` の `offsetof` で測って確かめた。
    /// 2026-09-30。Linux の UAPI のヘッダには無い）。**長さは NUL を数えて 8 バイト境界へ切り上げ、詰め物は 0 である。**
    #[test]
    fn a_dirent64_record_follows_the_linux_layout() {
        assert_eq!(dirent64_record_len(0), 24, "19 + NUL, rounded up to 8");
        assert_eq!(dirent64_record_len(4), 24, "19 + 4 + NUL");
        assert_eq!(
            dirent64_record_len(5),
            32,
            "19 + 5 + NUL = 25, rounded up to 32"
        );
        let mut out = [0xEE; 40];
        let written = dirent64_record(
            &Dirent64 {
                ino: 0x0102_0304_0506_0708,
                off: 0x1112_1314_1516_1718,
                kind: 0x21,
                name: b"ab",
            },
            &mut out,
        );
        assert_eq!(written, Some(24));
        assert_eq!(u64_at(&out, 0), 0x0102_0304_0506_0708, "d_ino @0");
        assert_eq!(u64_at(&out, 8), 0x1112_1314_1516_1718, "d_off @8");
        assert_eq!(u16_at(&out, 16), 24, "d_reclen @16");
        assert_eq!(out[18], 0x21, "d_type @18");
        assert_eq!(&out[19..21], b"ab", "d_name @19");
        assert_eq!(&out[21..24], &[0, 0, 0], "the NUL and the padding are 0");
        assert_eq!(&out[24..], &[0xEE; 16], "nothing past d_reclen");
        let long = Dirent64 {
            ino: 1,
            off: 2,
            kind: 3,
            name: b"abcdef",
        };
        assert_eq!(
            dirent64_record(&long, &mut [0; 24]),
            None,
            "a record that does not fit"
        );
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
