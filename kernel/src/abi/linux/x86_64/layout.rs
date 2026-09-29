//! x86_64 の `struct stat` の配置（長さと欄の位置と、組む関数。`ADR-0071` の決定 1 の 2 で、`crate::syscall` から移した。
//! 2026-09-30）。移したものの並びと doc は移す前のまま。
//!
//! **`struct stat` は CPU によって違う**——x86_64 は 144 バイトで、aarch64 は 128 バイトであり、`st_nlink` と `st_mode` の
//! 位置と幅も違う（`asm/stat.h` を `gcc` と `aarch64-linux-gnu-gcc` で測って確かめた。2026-09-30）。値の型 [`Stat`] は
//! CPU によらないので、[`crate::abi::linux`] にある。

use crate::abi::linux::Stat;

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

#[cfg(test)]
mod tests {
    //! `struct stat` の配置（x86_64）。**`gcc` の `offsetof` で測った値を機械で留める**（[`STAT_LEN`] の表）。**欄の位置を
    //! 動かすと、Linux の配置から外れたことがここで分かる。**

    use super::*;

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
}
