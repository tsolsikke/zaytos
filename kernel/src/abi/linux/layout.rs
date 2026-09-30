//! Linux の構造体の配置のうち、CPU によらないもの（長さと欄の位置、構造体を組む関数と読む関数。`ADR-0071` の決定 1 の 2
//! で、`crate::syscall` から移し、`abi/linux/x86_64` から分けた。2026-09-30）。移したものの並びと doc は移す前のまま。
//!
//! **ここにある構造体は、x86_64 と aarch64 で大きさと欄の位置が同じである**（UAPI と glibc のヘッダを `gcc` と
//! `aarch64-linux-gnu-gcc` で測って確かめた。2026-09-30）。**`struct stat` は CPU によって違う**ので、配置と組む関数は
//! [`super::x86_64`] にある。ここに置くのは値の型 [`Stat`] だけである。

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

/// `struct timespec` のバイト数（x86-64 の Linux）。**実測で確かめた。**
///
/// # 欄の位置
///
/// `gcc` の `offsetof` で測った値である（`cc` 13.3.0。確認日 2026-09-17）。
/// **記憶から書かない**（[`STAT_LEN`](super::x86_64::STAT_LEN) と同じ手順である）。
///
/// | 欄 | 位置 | 幅 |
/// |---|---|---|
/// | `tv_sec` | 0 | 8 |
/// | `tv_nsec` | 8 | 8 |
///
/// **どちらも符号つき 64 ビットで、詰め物は無い**（`__time_t` と `__syscall_slong_t` が
/// ともに `__SYSCALL_SLONG_TYPE` である）。
///
/// **[`STAT_LEN`](super::x86_64::STAT_LEN) の表の `st_atim`（位置 72、幅 16）と整合する**——**あちらが既に
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

/// `struct msghdr` のバイト数（`msg_name` 0・`msg_namelen` 8・`msg_iov` 16・`msg_iovlen` 24・`msg_control` 32・
/// `msg_controllen` 40・`msg_flags` 48。glibc の `bits/socket.h`）。
pub const MSGHDR_LEN: usize = 56;

/// `struct msghdr` の `msg_controllen` の位置。**`recvmsg` が、書いた補助データの長さを書き戻す欄である。**
pub const MSGHDR_CONTROLLEN: usize = 40;

/// `struct iovec` のバイト数（`iov_base` 0・`iov_len` 8。glibc の `bits/types/struct_iovec.h`）。
pub const IOVEC_LEN: usize = 16;

/// fd を 1 つ運ぶ補助データのバイト数（`CMSG_LEN(sizeof(int))`）。`struct cmsghdr`（`cmsg_len` 0・`cmsg_level` 8・
/// `cmsg_type` 12 の 16 バイト。glibc の `bits/socket.h`）と、fd の 4 バイトである。
pub const CMSG_ONE_FD_LEN: usize = 20;

/// 生入力イベント 1 つのバイト数（`ADR-0066` の Y-a）。**Linux の `struct input_event` の
/// 配置に合わせる**（`ADR-0020`。`tv_sec`(8)＋`tv_usec`(8)＋`type`(2)＋`code`(2)＋`value`(4)）。
pub const INPUT_EVENT_LEN: usize = 24;

/// `struct stat` に書く値。**ZeikOS が持つ欄だけである**（ほかの欄は 0 のまま返す）。
///
/// **値を決めるのは共通の側で、[`stat_bytes`](super::x86_64::stat_bytes) は欄の位置へ書くだけである**（`ADR-0071` の決定 1 の 2 で、
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

/// `struct fb_bitfield` に書く値（色の 1 つ。`msb_right` は 0 のまま）。
pub struct FbBitfield {
    /// `offset`（ビットの位置）。
    pub offset: u32,
    /// `length`（ビットの数）。
    pub length: u32,
}

/// `struct fb_var_screeninfo` に書く値。**ZeikOS が持つ欄だけである**（ほかの欄は 0 のまま返す）。
///
/// **値を決めるのは共通の側で、[`fb_var_screeninfo_bytes`] は欄の位置へ書くだけである**（`ADR-0071` の決定 1 の 2 で、
/// `crate::syscall` の画面の `ioctl` から分けた。2026-09-30）。
pub struct FbVarScreeninfo {
    /// `xres`（横の画素の数）。
    pub xres: u32,
    /// `yres`（縦の画素の数）。
    pub yres: u32,
    /// `xres_virtual`。
    pub xres_virtual: u32,
    /// `yres_virtual`。
    pub yres_virtual: u32,
    /// `bits_per_pixel`。
    pub bits_per_pixel: u32,
    /// `red`。
    pub red: FbBitfield,
    /// `green`。
    pub green: FbBitfield,
    /// `blue`。
    pub blue: FbBitfield,
}

/// `struct fb_var_screeninfo` を組む（`ADR-0066` の Y-c）。
///
/// **欄の位置は `cc` の `offsetof` で測った**（2026-09-21。`cc` 13.3.0。`<linux/fb.h>`）——
/// `xres` 0 / `yres` 4 / `xres_virtual` 8 / `yres_virtual` 12 / `bits_per_pixel` 24 /
/// `red` 32 / `green` 44 / `blue` 56 / `transp` 68（`struct fb_bitfield` は `offset` 0・`length` 4・
/// `msb_right` 8 の 12 バイト）。**それ以外の欄は 0 である。**
pub fn fb_var_screeninfo_bytes(info: &FbVarScreeninfo) -> [u8; FB_VAR_SCREENINFO_LEN] {
    let mut out = [0u8; FB_VAR_SCREENINFO_LEN];
    let mut put = |at: usize, value: u32| out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    put(0, info.xres);
    put(4, info.yres);
    put(8, info.xres_virtual);
    put(12, info.yres_virtual);
    put(24, info.bits_per_pixel);
    // **`struct fb_bitfield` は `offset`・`length`・`msb_right` の順である。**
    put(32, info.red.offset);
    put(36, info.red.length);
    put(44, info.green.offset);
    put(48, info.green.length);
    put(56, info.blue.offset);
    put(60, info.blue.length);
    out
}

/// `struct fb_fix_screeninfo` に書く値。**ZeikOS が持つ欄だけである**（ほかの欄は 0 のまま返す）。
///
/// **値を決めるのは共通の側で、[`fb_fix_screeninfo_bytes`] は欄の位置へ書くだけである**（`ADR-0071` の決定 1 の 2 で、
/// `crate::syscall` の画面の `ioctl` から分けた。2026-09-30）。
pub struct FbFixScreeninfo {
    /// `id`（名前。16 バイトに満たない分は 0）。
    pub id: [u8; 16],
    /// `smem_start`（フレームバッファの物理アドレス）。
    pub smem_start: u64,
    /// `smem_len`（バイト数）。
    pub smem_len: u32,
    /// `type`（`FB_TYPE_PACKED_PIXELS` など）。
    pub kind: u32,
    /// `visual`（`FB_VISUAL_TRUECOLOR` など）。
    pub visual: u32,
    /// `line_length`（1 行のバイト数）。
    pub line_length: u32,
}

/// `struct fb_fix_screeninfo` を組む（`ADR-0066` の Y-c）。
///
/// **欄の位置は `offsetof` で測った**——`id` 0（16 バイト）/ `smem_start` 16 / `smem_len` 24 /
/// `type` 28 / `visual` 36 / `line_length` 48（2026-09-21）。**それ以外の欄は 0 である。**
pub fn fb_fix_screeninfo_bytes(info: &FbFixScreeninfo) -> [u8; FB_FIX_SCREENINFO_LEN] {
    let mut out = [0u8; FB_FIX_SCREENINFO_LEN];
    out[..16].copy_from_slice(&info.id);
    out[16..24].copy_from_slice(&info.smem_start.to_le_bytes());
    out[24..28].copy_from_slice(&info.smem_len.to_le_bytes());
    out[28..32].copy_from_slice(&info.kind.to_le_bytes());
    out[36..40].copy_from_slice(&info.visual.to_le_bytes());
    out[48..52].copy_from_slice(&info.line_length.to_le_bytes());
    out
}

/// `struct drm_clip_rect` から読んだ値（`ADR-0066` の Y-c）。**x2・y2 は含まない**（DRM の DIRTYFB と同じ半開区間）。
///
/// **欄から読むのはここで、空の矩形を断るのと、幅と高さを求めるのは共通の側である**（`ADR-0071` の決定 1 の 2 で、
/// `crate::syscall` の画面の `ioctl` から分けた。2026-09-30）。
pub struct DrmClipRect {
    /// `x1`。
    pub x1: u16,
    /// `y1`。
    pub y1: u16,
    /// `x2`。
    pub x2: u16,
    /// `y2`。
    pub y2: u16,
}

/// `struct drm_clip_rect` を読む（`u16` の x1・y1・x2・y2。[`DRM_CLIP_RECT_LEN`]）。
pub fn parse_drm_clip_rect(raw: &[u8; DRM_CLIP_RECT_LEN]) -> DrmClipRect {
    DrmClipRect {
        x1: u16::from_le_bytes([raw[0], raw[1]]),
        y1: u16::from_le_bytes([raw[2], raw[3]]),
        x2: u16::from_le_bytes([raw[4], raw[5]]),
        y2: u16::from_le_bytes([raw[6], raw[7]]),
    }
}

/// `struct msghdr` から読んだ値（`sendmsg` と `recvmsg`。`ADR-0065`）。**`msg_namelen` と `msg_flags` は読まない。**
///
/// **欄から読むのはここで、受ける形を絞るのは共通の側である**（`ADR-0071` の決定 1 の 2 で、`crate::syscall` の
/// `read_msghdr` から分けた。2026-09-30）。
pub struct Msghdr {
    /// `msg_name`（宛先の名前を指す）。
    pub name: u64,
    /// `msg_iov`（`struct iovec` の並びを指す）。
    pub iov: u64,
    /// `msg_iovlen`（`struct iovec` の数）。
    pub iovlen: u64,
    /// `msg_control`（補助データを指す）。
    pub control: u64,
    /// `msg_controllen`（補助データのバイト数）。
    pub controllen: u64,
}

/// `struct msghdr` を読む（欄の並びは [`MSGHDR_LEN`] の doc）。
pub fn parse_msghdr(raw: &[u8; MSGHDR_LEN]) -> Msghdr {
    let u64_at = |off: usize| u64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
    Msghdr {
        name: u64_at(0),
        iov: u64_at(16),
        iovlen: u64_at(24),
        control: u64_at(32),
        controllen: u64_at(MSGHDR_CONTROLLEN),
    }
}

/// `struct iovec` から読んだ値。
pub struct Iovec {
    /// `iov_base`。
    pub base: u64,
    /// `iov_len`。
    pub len: u64,
}

/// `struct iovec` を読む（欄の並びは [`IOVEC_LEN`] の doc）。
pub fn parse_iovec(raw: &[u8; IOVEC_LEN]) -> Iovec {
    Iovec {
        base: u64::from_le_bytes(raw[0..8].try_into().unwrap()),
        len: u64::from_le_bytes(raw[8..16].try_into().unwrap()),
    }
}

/// fd を 1 つ運ぶ補助データの値（`struct cmsghdr` の `cmsg_level` と `cmsg_type`、運ぶ fd）。
///
/// **欄の位置はここで、`SOL_SOCKET` と `SCM_RIGHTS` を置くことと、それ以外を断ることは共通の側である。**
pub struct CmsgOneFd {
    /// `cmsg_level`。
    pub level: u32,
    /// `cmsg_type`。
    pub kind: u32,
    /// 運ぶ fd。
    pub fd: u32,
}

/// fd を 1 つ運ぶ補助データを読む（欄の並びは [`CMSG_ONE_FD_LEN`] の doc）。**`cmsg_len` は読まない。**
pub fn parse_cmsg_one_fd(raw: &[u8; CMSG_ONE_FD_LEN]) -> CmsgOneFd {
    CmsgOneFd {
        level: u32::from_le_bytes(raw[8..12].try_into().unwrap()),
        kind: u32::from_le_bytes(raw[12..16].try_into().unwrap()),
        fd: u32::from_le_bytes(raw[16..20].try_into().unwrap()),
    }
}

/// fd を 1 つ運ぶ補助データを組む。**`cmsg_len` は [`CMSG_ONE_FD_LEN`] である。**
pub fn cmsg_one_fd_bytes(cmsg: &CmsgOneFd) -> [u8; CMSG_ONE_FD_LEN] {
    let mut out = [0u8; CMSG_ONE_FD_LEN];
    out[0..8].copy_from_slice(&(CMSG_ONE_FD_LEN as u64).to_le_bytes());
    out[8..12].copy_from_slice(&cmsg.level.to_le_bytes());
    out[12..16].copy_from_slice(&cmsg.kind.to_le_bytes());
    out[16..20].copy_from_slice(&cmsg.fd.to_le_bytes());
    out
}

/// `struct input_event` に書く値（`ADR-0066` の Y-a）。
///
/// **値（時刻・キーの番号・押下か離鍵か）を決めるのは共通の側で、欄の位置へ書くのはここである**（`ADR-0071` の決定 1 の 2
/// で、`crate::input` の `read_events` から分けた。2026-09-30）。
pub struct InputEvent {
    /// `time.tv_sec`。
    pub sec: u64,
    /// `time.tv_usec`。
    pub usec: u64,
    /// `type`。
    pub kind: u16,
    /// `code`。
    pub code: u16,
    /// `value`。
    pub value: i32,
}

/// `struct input_event` を組む（欄の並びは [`INPUT_EVENT_LEN`] の doc）。
pub fn input_event_bytes(event: &InputEvent) -> [u8; INPUT_EVENT_LEN] {
    let mut out = [0u8; INPUT_EVENT_LEN];
    out[0..8].copy_from_slice(&event.sec.to_le_bytes());
    out[8..16].copy_from_slice(&event.usec.to_le_bytes());
    out[16..18].copy_from_slice(&event.kind.to_le_bytes());
    out[18..20].copy_from_slice(&event.code.to_le_bytes());
    out[20..24].copy_from_slice(&event.value.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    //! 構造体の配置。**`cc` の `offsetof` で測った値を機械で留める**（画面の `ioctl` の構造体は `ADR-0066` の Y-c。
    //! 2026-09-21。`cc` 13.3.0。`<linux/fb.h>` と `<drm/drm.h>`）。**欄の位置を
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

    /// `struct fb_var_screeninfo` の欄の位置（`offsetof` の値）。**値は、どのバイトも 0 でなく、欄ごとに違う形にする**
    /// （stat のテストと同じ理由）。**書かない欄は 0 である。**
    #[test]
    fn the_variable_screen_info_follows_the_linux_layout() {
        let out = fb_var_screeninfo_bytes(&FbVarScreeninfo {
            xres: 0x0102_0304,
            yres: 0x1112_1314,
            xres_virtual: 0x2122_2324,
            yres_virtual: 0x3132_3334,
            bits_per_pixel: 0x4142_4344,
            red: FbBitfield {
                offset: 0x5152_5354,
                length: 0x6162_6364,
            },
            green: FbBitfield {
                offset: 0x7172_7374,
                length: 0x8182_8384,
            },
            blue: FbBitfield {
                offset: 0x9192_9394,
                length: 0xA1A2_A3A4,
            },
        });
        assert_eq!(out.len(), 160, "sizeof(struct fb_var_screeninfo)");
        let fields = [
            (0, 0x0102_0304, "xres @0"),
            (4, 0x1112_1314, "yres @4"),
            (8, 0x2122_2324, "xres_virtual @8"),
            (12, 0x3132_3334, "yres_virtual @12"),
            (24, 0x4142_4344, "bits_per_pixel @24"),
            (32, 0x5152_5354, "red.offset @32"),
            (36, 0x6162_6364, "red.length @36"),
            (44, 0x7172_7374, "green.offset @44"),
            (48, 0x8182_8384, "green.length @48"),
            (56, 0x9192_9394, "blue.offset @56"),
            (60, 0xA1A2_A3A4, "blue.length @60"),
        ];
        let mut rest = out;
        for (at, value, name) in fields {
            assert_eq!(u32_at(&out, at), value, "{name}");
            rest[at..at + 4].fill(0);
        }
        assert_eq!(rest, [0u8; 160], "the other fields are 0");
    }

    /// `struct fb_fix_screeninfo` の欄の位置（`offsetof` の値）。**値は、どのバイトも 0 でなく、欄ごとに違う形にする。**
    /// **書かない欄は 0 である。**
    #[test]
    fn the_fixed_screen_info_follows_the_linux_layout() {
        let id = [
            0xB0, 0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xBB, 0xBC, 0xBD,
            0xBE, 0xBF,
        ];
        let out = fb_fix_screeninfo_bytes(&FbFixScreeninfo {
            id,
            smem_start: 0x0102_0304_0506_0708,
            smem_len: 0x1112_1314,
            kind: 0x2122_2324,
            visual: 0x3132_3334,
            line_length: 0x4142_4344,
        });
        assert_eq!(out.len(), 80, "sizeof(struct fb_fix_screeninfo)");
        assert_eq!(&out[..16], &id, "id @0");
        assert_eq!(u64_at(&out, 16), 0x0102_0304_0506_0708, "smem_start @16");
        assert_eq!(u32_at(&out, 24), 0x1112_1314, "smem_len @24");
        assert_eq!(u32_at(&out, 28), 0x2122_2324, "type @28");
        assert_eq!(u32_at(&out, 36), 0x3132_3334, "visual @36");
        assert_eq!(u32_at(&out, 48), 0x4142_4344, "line_length @48");
        let mut rest = out;
        for (at, len) in [(0, 16), (16, 8), (24, 4), (28, 4), (36, 4), (48, 4)] {
            rest[at..at + len].fill(0);
        }
        assert_eq!(rest, [0u8; 80], "the other fields are 0");
    }

    /// `struct drm_clip_rect` の欄の位置（`u16` が 4 つ。x1・y1・x2・y2 の順）。
    #[test]
    fn a_drm_clip_rect_is_read_as_four_u16s() {
        let read = parse_drm_clip_rect(&[0x01, 0x02, 0x11, 0x12, 0x21, 0x22, 0x31, 0x32]);
        assert_eq!(
            (read.x1, read.y1, read.x2, read.y2),
            (0x0201, 0x1211, 0x2221, 0x3231)
        );
    }

    /// `struct msghdr` の欄の位置（glibc の `bits/socket.h` を `gcc` と `aarch64-linux-gnu-gcc` の `offsetof` で測って
    /// 確かめた。2026-09-30。両方で同じ）。**値は、どのバイトも 0 でなく、欄ごとに違う形にする**（stat のテストと同じ理由）。
    #[test]
    fn a_msghdr_is_read_from_the_linux_positions() {
        let mut raw = [0u8; MSGHDR_LEN];
        for (i, byte) in raw.iter_mut().enumerate() {
            *byte = 0x10 + i as u8;
        }
        let read = parse_msghdr(&raw);
        assert_eq!(read.name, 0x1716_1514_1312_1110, "msg_name @0");
        assert_eq!(read.iov, 0x2726_2524_2322_2120, "msg_iov @16");
        assert_eq!(read.iovlen, 0x2f2e_2d2c_2b2a_2928, "msg_iovlen @24");
        assert_eq!(read.control, 0x3736_3534_3332_3130, "msg_control @32");
        assert_eq!(read.controllen, 0x3f3e_3d3c_3b3a_3938, "msg_controllen @40");
    }

    /// `struct iovec` の欄の位置（`sys/uio.h` を同じく測った。両方で同じ）。
    #[test]
    fn an_iovec_is_read_from_the_linux_positions() {
        let mut raw = [0u8; IOVEC_LEN];
        for (i, byte) in raw.iter_mut().enumerate() {
            *byte = 0x40 + i as u8;
        }
        let read = parse_iovec(&raw);
        assert_eq!(read.base, 0x4746_4544_4342_4140, "iov_base @0");
        assert_eq!(read.len, 0x4f4e_4d4c_4b4a_4948, "iov_len @8");
    }

    /// fd を 1 つ運ぶ補助データ（`struct cmsghdr` と fd）の欄の位置と `cmsg_len`（`CMSG_LEN(sizeof(int))` は 20。
    /// `sys/socket.h` を同じく測った。両方で同じ）。
    #[test]
    fn a_control_message_with_one_fd_follows_the_linux_layout() {
        let cmsg = CmsgOneFd {
            level: 0x0403_0201,
            kind: 0x1413_1211,
            fd: 0x2423_2221,
        };
        let bytes = cmsg_one_fd_bytes(&cmsg);
        assert_eq!(u64_at(&bytes, 0), 20, "cmsg_len @0");
        assert_eq!(u32_at(&bytes, 8), 0x0403_0201, "cmsg_level @8");
        assert_eq!(u32_at(&bytes, 12), 0x1413_1211, "cmsg_type @12");
        assert_eq!(u32_at(&bytes, 16), 0x2423_2221, "the fd @16");
        let read = parse_cmsg_one_fd(&bytes);
        assert_eq!(
            (read.level, read.kind, read.fd),
            (cmsg.level, cmsg.kind, cmsg.fd)
        );
    }

    /// `struct input_event` の欄の位置（`linux/input.h` を `gcc` と `aarch64-linux-gnu-gcc` の `offsetof` で測って確かめた。
    /// 2026-09-30。両方で同じ）。**値は、どのバイトも 0 でなく、欄ごとに違う形にする。**
    #[test]
    fn an_input_event_follows_the_linux_layout() {
        let bytes = input_event_bytes(&InputEvent {
            sec: 0x0807_0605_0403_0201,
            usec: 0x1817_1615_1413_1211,
            kind: 0x2221,
            code: 0x3231,
            value: 0x4443_4241,
        });
        assert_eq!(u64_at(&bytes, 0), 0x0807_0605_0403_0201, "time.tv_sec @0");
        assert_eq!(u64_at(&bytes, 8), 0x1817_1615_1413_1211, "time.tv_usec @8");
        assert_eq!(u16_at(&bytes, 16), 0x2221, "type @16");
        assert_eq!(u16_at(&bytes, 18), 0x3231, "code @18");
        assert_eq!(u32_at(&bytes, 20), 0x4443_4241, "value @20");
    }
}
