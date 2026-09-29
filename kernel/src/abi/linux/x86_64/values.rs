//! Linux の値（`d_type`・時計の番号・`ioctl` の要求・`open` のフラグ・`whence`・fbdev・ソケット・`poll`・`mmap`・
//! `cmsghdr`。x86_64 の Linux の値。`ADR-0071` の決定 1 の 2 で、`crate::syscall` から移した。2026-09-30。
//! SCRUM-19）。並びと doc は移す前のまま。

/// `d_type`: 不明。**対応表に無い値はこれにする。**
pub const DT_UNKNOWN: u8 = 0;

/// `d_type`: ディレクトリ（実測）。
pub const DT_DIR: u8 = 4;

/// `d_type`: 通常ファイル（実測）。
pub const DT_REG: u8 = 8;

/// `CLOCK_MONOTONIC`（Linux x86-64 の値）。**実測で確かめた**——
/// `/usr/include/x86_64-linux-gnu/bits/time.h` が `1` と定義している（確認日 2026-09-17。
/// **`CLOCK_REALTIME` は `0` である**）。
pub const CLOCK_MONOTONIC: u64 = 1;

/// `TIOCGWINSZ`——端末の大きさを訊く要求（e-1）。**Linux の値をそのまま使う**
/// （実測。`/usr/include/asm-generic/ioctls.h` の `0x5413`）。
pub const TIOCGWINSZ: u64 = 0x5413;

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

/// `lseek` の `whence`——先頭からの絶対位置（`SEEK_SET`）。
///
/// # ここだけ受ける
///
/// **`SEEK_CUR` と `SEEK_END` は受けない**（`-EINVAL`）。
/// **使う者が居ない**——`/bin/tail` は `stat` で大きさを取ってから
/// `SEEK_SET` で跳ぶ。**要る者が来たら足す。**
pub const SEEK_SET: u64 = 0;

/// `FBIOGET_VSCREENINFO`（Linux の fbdev。`<linux/fb.h>`）。**`struct fb_var_screeninfo` を返す。**
pub const FBIOGET_VSCREENINFO: u64 = 0x4600;

/// `FBIOGET_FSCREENINFO`（Linux の fbdev）。**`struct fb_fix_screeninfo` を返す。**
pub const FBIOGET_FSCREENINFO: u64 = 0x4602;

/// `FB_TYPE_PACKED_PIXELS`（`<linux/fb.h>`）。
pub const FB_TYPE_PACKED_PIXELS: u32 = 0;

/// `FB_VISUAL_TRUECOLOR`（`<linux/fb.h>`）。
pub const FB_VISUAL_TRUECOLOR: u32 = 2;

/// `AF_UNIX`（Linux の値）。
pub const AF_UNIX: u64 = 1;

/// `SOCK_STREAM`（Linux の値）。
pub const SOCK_STREAM: u64 = 1;

/// `POLLIN`（読めるようになった）。**v1 が見る唯一のビットである。**
pub const POLLIN: u16 = 0x001;

/// `PROT_WRITE`（`mmap`。書ける葉を作る）。
pub const PROT_WRITE: u64 = 2;

/// `SOL_SOCKET`（`cmsghdr` の level）。
pub const SOL_SOCKET: u32 = 1;

/// `SCM_RIGHTS`（`cmsghdr` の type。fd を運ぶ）。
pub const SCM_RIGHTS: u32 = 1;
