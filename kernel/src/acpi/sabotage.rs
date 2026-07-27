//! ACPI の検証経路を意図的に壊す仕掛け（S1-b-2）。**既定ビルドには入らない。**
//!
//! # なぜ要るのか
//!
//! S1-b-1 の時点で実機を通ったのは正常系だけだった。署名・チェックサム・長さ・
//! エントリ長・未マップの検出はホストテストで判定側を閉じただけで、**実機では
//! 一度も働いていない。** このプロジェクトは「検査があるように見えて何も検査して
//! いなかった」を 3 件出しているので、検出経路ごとに壊して確かめる。
//!
//! # 観測するもの
//!
//! いずれも `panic` ではなく、**検出のログが出ること**と、**列挙の完了行
//! （`acpi: MADT enumeration complete`）が出ないこと**の 2 つで判定する。S1 の
//! ACPI 経路は異常を見つけても停止しないので、停止を観測の材料にできない。
//!
//! # アドレスをリテラルで焼き込まない
//!
//! 未マップの物理アドレスは、**メモリマップから実行時に導く。** 実測で
//! `EfiReservedMemoryType` の穴は `0xf6ed000..0xf76d000` にあるが、この範囲は
//! 起動ごとに揺れる（`descriptors_len` が 130〜132 で変わることが実測済みである）。
//! リテラルで持つと、穴が動いた起動で**破壊が破壊にならず、検査が静かに通る。**
//! 時点依存の数字を導出元から離れた場所に書かない、の適用である。

use common::addr::PhysAddr;
use common::log::Logger;
use common::serial::SerialPort;

/// 壊す対象のテーブル。**文字列で照合しない。** ログ用の表示名とは別に持つ。
/// 表示名は文言を直した瞬間に一致しなくなるが、こちらは型で結び付く。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// XSDT / RSDT。
    RootTable,
    Madt,
}

/// ヘッダを読んだ直後、検証の直前に効かせる破壊（署名・長さ）。
///
/// 既定ビルドでは何もしない。引数を使わないことによる警告を避けるため、
/// 冒頭で明示的に触れておく（借用なので、後続の破壊コードが使える）。
pub fn corrupt_table_header(target: Target, bytes: &mut [u8]) {
    let _ = (target, &bytes);

    // 署名の 1 バイト目を壊す。署名の照合が働けば、長さもチェックサムも
    // 見ないうちに弾かれる。
    #[cfg(feature = "acpi-test-bad-signature")]
    if target == Target::Madt && !bytes.is_empty() {
        bytes[0] = b'X';
    }

    // `length` を仕様の最小長より小さくする。MADT の最小は 44 なので、
    // 8 を書けば `LengthTooSmall` になる。**チェックサム破れとは別の経路である。**
    #[cfg(feature = "acpi-test-bad-length")]
    if target == Target::Madt && bytes.len() >= 8 {
        bytes[4..8].copy_from_slice(&8u32.to_le_bytes());
    }
}

/// 本体を読んだ直後、チェックサム検算の直前に効かせる破壊
/// （チェックサム・エントリ長 0）。
pub fn corrupt_table_body(target: Target, bytes: &mut [u8]) {
    let _ = (target, &bytes);

    // 末尾の 1 バイトを変える。署名も長さも無傷なので、チェックサムだけが
    // 破れる。**署名の破壊と経路が違うことを、この位置で保証している。**
    #[cfg(feature = "acpi-test-bad-checksum")]
    if target == Target::Madt && !bytes.is_empty() {
        let last = bytes.len() - 1;
        bytes[last] = bytes[last].wrapping_add(1);
    }

    // 最初のエントリの `length` を 0 にする。走査が前進しなくなる形で、守って
    // いなければ無限ループになる。
    //
    // **チェックサムのバイトを合わせ直す。** そうしないと先にチェックサムが
    // 破れて走査へ到達せず、「エントリ長 0 の検出」を見たことにならない。
    // 壊した経路と観測した経路が食い違う形は、このプロジェクトが繰り返して
    // いる誤りの型そのものである。
    #[cfg(feature = "acpi-test-zero-entry-length")]
    if target == Target::Madt && bytes.len() > super::madt::FIXED_LENGTH + 1 {
        let length_offset = super::madt::FIXED_LENGTH + 1;
        let removed = bytes[length_offset];
        bytes[length_offset] = 0;
        // 共通ヘッダのチェックサムはオフセット 9 にある。減った分を足し戻す。
        bytes[9] = bytes[9].wrapping_add(removed);
    }
}

/// RSDP の物理アドレスを、届かないアドレスへ差し替える。
///
/// 既定ビルドでは受け取った値をそのまま返す。
pub fn redirect_rsdp(
    logger: &mut Logger<SerialPort>,
    rsdp_phys: PhysAddr,
    memory_map_bytes: &[u8],
    descriptor_size: u64,
) -> PhysAddr {
    let _ = (&logger, memory_map_bytes, descriptor_size);

    // 稼働中のページテーブルに翻訳が無いアドレスを踏ませる。**#PF にならず、
    // 検出して報告することの実証である。**
    #[cfg(feature = "acpi-test-unmapped-rsdp")]
    {
        match largest_unmapped_midpoint(memory_map_bytes, descriptor_size) {
            Some(phys) => {
                logger.warn(format_args!(
                    "acpi: [acpi-test-unmapped-rsdp] redirecting the RSDP from {:#x} to {:#x}, \
                     the midpoint of the largest descriptor that classify() leaves unmapped",
                    rsdp_phys.as_u64(),
                    phys.as_u64()
                ));
                return phys;
            }
            None => logger.error(format_args!(
                "acpi: [acpi-test-unmapped-rsdp] found no unmapped descriptor to aim at; \
                 the sabotage did nothing and this run proves nothing"
            )),
        }
    }

    // direct map 窓が覆っていないアドレスを渡す。窓長そのものが「窓の外の
    // 最初のアドレス」である（`covers` は `phys < length`）。**窓長から導くので、
    // ここにリテラルは無い。**
    #[cfg(feature = "acpi-test-rsdp-outside-window")]
    {
        let length = common::addr::direct_map().length();
        match PhysAddr::new(length) {
            Some(phys) => {
                logger.warn(format_args!(
                    "acpi: [acpi-test-rsdp-outside-window] redirecting the RSDP from {:#x} to \
                     {:#x}, the first address the direct map window does not cover",
                    rsdp_phys.as_u64(),
                    phys.as_u64()
                ));
                return phys;
            }
            None => logger.error(format_args!(
                "acpi: [acpi-test-rsdp-outside-window] the window length {length:#x} is not a \
                 representable physical address; the sabotage did nothing"
            )),
        }
    }

    rsdp_phys
}

/// `classify()` が `Unmapped` にする記述子のうち最大のものの中点。
///
/// **中点を採るのは、端では足りないためである。** マッピングは 2MiB ページを
/// 使うので、マップ済みの範囲に隣接する未マップ領域の先頭は、隣の huge page に
/// 巻き込まれて写っていることがありうる。最大の記述子（実測では 12GiB 規模）の
/// 中点なら、その可能性が構造的に無い。
#[cfg(feature = "acpi-test-unmapped-rsdp")]
fn largest_unmapped_midpoint(memory_map_bytes: &[u8], descriptor_size: u64) -> Option<PhysAddr> {
    use crate::memory_map::{self, RegionPolicy};

    let entries = memory_map::parse_entries(memory_map_bytes, descriptor_size).ok()?;
    let mut best: Option<(u64, u64)> = None;
    for entry in entries {
        if memory_map::classify(entry.memory_type) != RegionPolicy::Unmapped {
            continue;
        }
        if entry.page_count == 0 {
            continue;
        }
        let better = match best {
            Some((_, pages)) => entry.page_count > pages,
            None => true,
        };
        if better {
            best = Some((entry.phys_start, entry.page_count));
        }
    }
    let (start, pages) = best?;
    let offset = (pages / 2).checked_mul(memory_map::UEFI_PAGE_SIZE)?;
    PhysAddr::new(start.checked_add(offset)?)
}
