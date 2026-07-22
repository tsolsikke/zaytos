//! bootloader から kernel への引き渡し情報（ADR-0008 の BootInfo）。
//!
//! ExitBootServices 後は UEFI から一切の情報を取得できなくなるため、
//! ここに含めなかった値は「その回の起動では」後から取得できない
//! （構造体定義自体は次にビルドし直す際に拡張できる）。現時点では
//! M2（メモリマップ）・M3（GOP フレームバッファ）に必要な最小限のみを
//! 含める。ACPI/RSDP など M4 以降で必要になる情報は、必要になった時点で
//! このモジュールにフィールドを追加する。

use core::fmt;

/// `BootInfo` のマジックナンバー。bootloader と kernel は別々にビルドされる
/// 2 つのバイナリであり、どちらか一方だけ古いビルドが混ざるとレイアウトの
/// 不一致（症状はトリプルフォルト等の無言の停止）が起こりうる。kernel は
/// 起動直後にこれを検証し、不一致ならシリアルにエラーを出して停止する。
use crate::addr::PhysAddr;

pub const BOOT_INFO_MAGIC: u64 = u64::from_le_bytes(*b"ZAYTBOOT");

/// `BootInfo` のレイアウトバージョン。フィールドを追加・変更したら上げる。
pub const BOOT_INFO_VERSION: u32 = 1;

/// bootloader が `BootInfo` 自身のために確保するページ数。kernel はこの値と
/// 受け取った `BootInfo` へのポインタから、bootloader が確保した領域の
/// 物理範囲を特定できる（M2-d の物理フレームアロケータへの申し送り。
/// `docs/architecture.md` 参照）。
pub const BOOT_INFO_PAGE_COUNT: usize = 1;

/// bootloader から kernel の `_start` に渡す起動情報。
///
/// # ABI
/// `#[repr(C)]` で明示的なレイアウトを保証する。bootloader・kernel は
/// 別々にコンパイルされる独立したバイナリのため、Rust の既定 repr に
/// 依存しない。
#[repr(C)]
pub struct BootInfo {
    /// [`BOOT_INFO_MAGIC`] と一致するはずの値。
    pub magic: u64,
    /// [`BOOT_INFO_VERSION`] と一致するはずの値。
    pub version: u32,
    pub memory_map: MemoryMapInfo,
    pub framebuffer: FramebufferInfo,
}

/// UEFI メモリマップ（ExitBootServices 呼び出し時に確定した最終スナップショット）
/// への参照。
///
/// ディスクリプタのサイズは UEFI 仕様上将来拡張されうるため、固定サイズを
/// 仮定せず `descriptor_size` を使ってイテレートすること
/// （`descriptors_ptr` を `u8` ポインタとして扱い、`descriptor_size` バイト
/// ごとに読み進める）。
#[repr(C)]
pub struct MemoryMapInfo {
    /// ディスクリプタ配列の先頭物理アドレス（= 仮想アドレス。ADR-0009 で
    /// 検証済みの恒等マッピング前提）。
    pub descriptors_ptr: PhysAddr,
    /// ディスクリプタ配列全体のバイトサイズ。
    pub descriptors_len: u64,
    /// 1 ディスクリプタあたりのバイトサイズ。
    pub descriptor_size: u64,
    /// ディスクリプタのバージョン番号。
    pub descriptor_version: u32,
}

/// GOP (Graphics Output Protocol) から ExitBootServices 前に取得した
/// フレームバッファ情報。
#[repr(C)]
pub struct FramebufferInfo {
    pub physical_address: PhysAddr,
    pub size_bytes: u64,
    pub width: u32,
    pub height: u32,
    /// 1 行あたりのピクセル数。パディングにより `width` と一致しない
    /// 場合があるため、描画時は必ずこちらを使うこと。
    pub stride: u32,
    pub pixel_format: PixelFormat,
    /// `pixel_format == PixelFormat::Bitmask` のときのみ有効。
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelFormat {
    Rgb = 0,
    Bgr = 1,
    Bitmask = 2,
    /// 直接フレームバッファへアクセスできず `Blt()` 経由のみのモード。
    /// `Blt()` はプロトコル関数呼び出しであり ExitBootServices 後は
    /// 使えないため、この値の場合 M3 では扱えない（fail-fast で報告する）。
    BltOnly = 3,
}

/// bootloader から kernel の `_start` への呼び出し規約。
///
/// `extern "Rust"`（既定の ABI）は仕様上安定したレイアウトを保証しないため、
/// バイナリ境界を跨ぐこの呼び出しには使えない。`extern "sysv64"` は
/// x86_64 の System V AMD64 呼び出し規約を明示的に指定するものであり、
/// コンパイル対象（`x86_64-unknown-uefi` は既定で Microsoft x64、
/// `x86_64-unknown-none` は既定で SysV）に関わらず両バイナリで一致する。
pub type KernelEntryFn = unsafe extern "sysv64" fn(boot_info: *const BootInfo) -> !;

/// `BootInfo` の検証結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    BadMagic { found: u64 },
    UnsupportedVersion { found: u32 },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { found } => write!(
                f,
                "bad BootInfo magic: expected {BOOT_INFO_MAGIC:#018x}, found {found:#018x}"
            ),
            Self::UnsupportedVersion { found } => write!(
                f,
                "unsupported BootInfo version: expected {BOOT_INFO_VERSION}, found {found}"
            ),
        }
    }
}

impl BootInfo {
    /// `magic`/`version` を検証する。bootloader と kernel のビルドが
    /// 食い違っている場合、ここで検出する（バイナリ境界の契約チェック）。
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.magic != BOOT_INFO_MAGIC {
            return Err(ValidationError::BadMagic { found: self.magic });
        }
        if self.version != BOOT_INFO_VERSION {
            return Err(ValidationError::UnsupportedVersion {
                found: self.version,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BootInfo {
        BootInfo {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            memory_map: MemoryMapInfo {
                descriptors_ptr: PhysAddr::new_const(0),
                descriptors_len: 0,
                descriptor_size: 0,
                descriptor_version: 0,
            },
            framebuffer: FramebufferInfo {
                physical_address: PhysAddr::new_const(0),
                size_bytes: 0,
                width: 0,
                height: 0,
                stride: 0,
                pixel_format: PixelFormat::Rgb,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
            },
        }
    }

    #[test]
    fn validate_accepts_matching_magic_and_version() {
        assert_eq!(sample().validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_bad_magic() {
        let mut info = sample();
        info.magic = 0xdead_beef_dead_beef;
        assert_eq!(
            info.validate(),
            Err(ValidationError::BadMagic {
                found: 0xdead_beef_dead_beef
            })
        );
    }

    #[test]
    fn validate_rejects_wrong_version() {
        let mut info = sample();
        info.version = BOOT_INFO_VERSION + 1;
        assert_eq!(
            info.validate(),
            Err(ValidationError::UnsupportedVersion {
                found: BOOT_INFO_VERSION + 1
            })
        );
    }

    /// **bootloader と kernel の境界を跨ぐ構造体のレイアウトを固定する。**
    ///
    /// T-2a で `descriptors_ptr` と `physical_address` を `u64` から
    /// `PhysAddr` へ変えた。`#[repr(transparent)]` によりレイアウトは
    /// 同一のはずだが、それは前提であって検証ではない。
    ///
    /// 不一致は「マジック値の検証は通るが中身がずれている」という形で出る。
    /// 最も診断しにくい部類なので、機械的に固定しておく。
    ///
    /// 期待値は型を変える前のコード（f83db79）から実測した値である。
    /// 推測で書いたところ 72 と 96 で食い違い、テストが誤りを捕まえた。
    #[test]
    fn the_handoff_layout_did_not_change() {
        use core::mem::{align_of, offset_of, size_of};

        assert_eq!(size_of::<BootInfo>(), 96);
        assert_eq!(align_of::<BootInfo>(), 8);
        assert_eq!(offset_of!(BootInfo, magic), 0);
        assert_eq!(offset_of!(BootInfo, version), 8);
        assert_eq!(offset_of!(BootInfo, memory_map), 16);
        assert_eq!(offset_of!(BootInfo, framebuffer), 48);

        assert_eq!(size_of::<MemoryMapInfo>(), 32);
        assert_eq!(align_of::<MemoryMapInfo>(), 8);
        assert_eq!(offset_of!(MemoryMapInfo, descriptors_ptr), 0);
        assert_eq!(offset_of!(MemoryMapInfo, descriptors_len), 8);
        assert_eq!(offset_of!(MemoryMapInfo, descriptor_size), 16);
        assert_eq!(offset_of!(MemoryMapInfo, descriptor_version), 24);

        assert_eq!(size_of::<FramebufferInfo>(), 48);
        assert_eq!(align_of::<FramebufferInfo>(), 8);
        assert_eq!(offset_of!(FramebufferInfo, physical_address), 0);
        assert_eq!(offset_of!(FramebufferInfo, size_bytes), 8);
        assert_eq!(offset_of!(FramebufferInfo, width), 16);
        assert_eq!(offset_of!(FramebufferInfo, height), 20);
        assert_eq!(offset_of!(FramebufferInfo, stride), 24);
        assert_eq!(offset_of!(FramebufferInfo, pixel_format), 28);
        assert_eq!(offset_of!(FramebufferInfo, red_mask), 32);
        assert_eq!(offset_of!(FramebufferInfo, green_mask), 36);
        assert_eq!(offset_of!(FramebufferInfo, blue_mask), 40);
    }
}
