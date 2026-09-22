//! FADT（Fixed ACPI Description Table。署名は `FACP`）の解析（HW-b。`ADR-0068`）。
//!
//! バイトスライスの読み取りだけで完結する純粋ロジックであり、unsafe を一切
//! 使わない。ホスト上の `cargo test` で検証する。
//!
//! # 読むのは `IAPC_BOOT_ARCH` の 8042 のビットだけである
//!
//! **i8042 が無い機械で、起動を止めないために読む。** 他の欄は、消費者が現れた段で足す
//! （PM タイマは HW-c）。
//!
//! # リビジョンで読むかを決める
//!
//! **8042 のビットはリビジョン 3（ACPI 2.0 の FADT）で定義された。** リビジョン 1 の FADT
//! （ACPI 1.0。QEMU の `pc` がこれで、116 バイト）では、同じ位置は予約である。**予約の欄を
//! 「無い」と読むと、在る i8042 を探らなくなる**——**リビジョンが 3 未満なら「言っていない」
//! とする。** 境界の 3 は ACPICA の `FADT2_REVISION_ID` と同じで、Linux も同じ比較で
//! このビットを読む（`arch/x86/kernel/acpi/boot.c`）。

use super::sdt;

/// FADT の署名。
pub const SIGNATURE: [u8; sdt::SIGNATURE_LENGTH] = *b"FACP";

/// ACPI 1.0 の FADT の長さ（`Flags` の終わりまで）。**これより短い FADT は、どの版でも
/// 正当でない。** `IAPC_BOOT_ARCH`（109..111）もこの内側に在る。
pub const V1_LENGTH: usize = 116;

const OFFSET_REVISION: usize = 8;
const OFFSET_PM_TMR_BLK: usize = 76;
const OFFSET_PM_TMR_LEN: usize = 91;
const OFFSET_IAPC_BOOT_ARCH: usize = 109;
const OFFSET_FLAGS: usize = 112;
const OFFSET_X_PM_TMR_BLK: usize = 208;

/// Generic Address Structure の長さ（`X_` の欄はこの形である）。
const GAS_LENGTH: usize = 12;

/// GAS の Address Space ID: System I/O。
const SPACE_SYSTEM_IO: u8 = 1;

/// `IAPC_BOOT_ARCH` の 8042 のビットが定義された最初のリビジョン。
pub const FIRST_REVISION_WITH_THE_8042_FLAG: u8 = 3;

/// `IAPC_BOOT_ARCH` の bit 1: ポート 0x60/0x64 の 8042 が在る。
pub const BOOT_ARCH_8042: u16 = 1 << 1;

/// `X_` の欄（64 ビットの GAS）が定義された最初のリビジョン。
///
/// **8042 のビットと同じ 3 である**（どちらも ACPI 2.0 の FADT で入った）。**別の定数で持つのは、
/// 同じ値であることが理由ではなく偶然だからである。**
pub const FIRST_REVISION_WITH_THE_EXTENDED_BLOCKS: u8 = 3;

/// `Flags` の bit 8（TMR_VAL_EXT）: PM タイマは 32 ビットである（落ちていれば 24 ビット）。
pub const FLAG_TMR_VAL_EXT: u32 = 1 << 8;

/// PM タイマの所在（HW-c。`ADR-0068`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PmTimerBlock {
    /// 読めるポートが在る。`bits` は 24 か 32、`from_extended` は `X_PM_TMR_BLK` から取ったか。
    Port {
        port: u16,
        bits: u8,
        from_extended: bool,
    },
    /// 無い（どちらの欄も 0、または `PM_TMR_LEN` が 4 でない）。
    Absent,
    /// 在るが、この形では読まない（System I/O でない空間、ポートに収まらない番地）。
    ///
    /// **ACPI のハードウェア縮小形では MMIO の PM タイマが在りうる**（一般論。推測）。
    /// **「無い」と混ぜない**——**読めないことと、無いことは別の事実である。**
    Unsupported { space_id: u8, address: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadtError {
    /// 読むのに要るバイト数に足りない。
    TooShort { need: usize, got: usize },
}

/// FADT から読んだもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fadt {
    pub revision: u8,
    /// `IAPC_BOOT_ARCH`。**リビジョンが 3 未満なら `None`**（その位置は予約である）。
    pub iapc_boot_arch: Option<u16>,
}

impl Fadt {
    /// FADT が i8042 について言っていること。**`None` は「言っていない」である。**
    pub fn has_8042(&self) -> Option<bool> {
        self.iapc_boot_arch.map(|flags| flags & BOOT_ARCH_8042 != 0)
    }
}

/// PM タイマの所在を読む（HW-c）。`bytes` は表の全体で、[`V1_LENGTH`] 以上あること。
///
/// **`X_PM_TMR_BLK` を先に見る**（ACPICA と同じ。**0 なら旧い 32 ビットの欄へ落ちる**）。
/// **幅は `Flags` の TMR_VAL_EXT で決まる**（立っていれば 32 ビット、落ちていれば 24 ビット）。
pub fn pm_timer_block(bytes: &[u8], revision: u8) -> PmTimerBlock {
    let flags = u32::from_le_bytes(bytes[OFFSET_FLAGS..OFFSET_FLAGS + 4].try_into().unwrap());
    let bits = if flags & FLAG_TMR_VAL_EXT != 0 {
        32
    } else {
        24
    };
    if revision >= FIRST_REVISION_WITH_THE_EXTENDED_BLOCKS
        && bytes.len() >= OFFSET_X_PM_TMR_BLK + GAS_LENGTH
    {
        let gas = &bytes[OFFSET_X_PM_TMR_BLK..OFFSET_X_PM_TMR_BLK + GAS_LENGTH];
        let address = u64::from_le_bytes(gas[4..12].try_into().unwrap());
        if address != 0 {
            let space_id = gas[0];
            return match (space_id, u16::try_from(address)) {
                (SPACE_SYSTEM_IO, Ok(port)) => PmTimerBlock::Port {
                    port,
                    bits,
                    from_extended: true,
                },
                _ => PmTimerBlock::Unsupported { space_id, address },
            };
        }
    }
    let block = u32::from_le_bytes(
        bytes[OFFSET_PM_TMR_BLK..OFFSET_PM_TMR_BLK + 4]
            .try_into()
            .unwrap(),
    );
    // **長さが 4 でなければ読まない。** 仕様は PM タイマが在るとき 4 と定めている。
    if block == 0 || bytes[OFFSET_PM_TMR_LEN] != 4 {
        return PmTimerBlock::Absent;
    }
    match u16::try_from(block) {
        Ok(port) => PmTimerBlock::Port {
            port,
            bits,
            from_extended: false,
        },
        Err(_) => PmTimerBlock::Unsupported {
            space_id: SPACE_SYSTEM_IO,
            address: u64::from(block),
        },
    }
}

/// 検証済みの FADT を読む。`bytes` は表の全体（`length` まで）で、[`V1_LENGTH`] 以上あること。
pub fn parse(bytes: &[u8]) -> Result<Fadt, FadtError> {
    if bytes.len() < V1_LENGTH {
        return Err(FadtError::TooShort {
            need: V1_LENGTH,
            got: bytes.len(),
        });
    }
    let revision = bytes[OFFSET_REVISION];
    let iapc_boot_arch = (revision >= FIRST_REVISION_WITH_THE_8042_FLAG).then(|| {
        u16::from_le_bytes([
            bytes[OFFSET_IAPC_BOOT_ARCH],
            bytes[OFFSET_IAPC_BOOT_ARCH + 1],
        ])
    });
    Ok(Fadt {
        revision,
        iapc_boot_arch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PM タイマの欄を埋めた表（`legacy` は 32 ビットの欄、`extended` は GAS の番地）。
    fn pm_table(
        revision: u8,
        length: usize,
        legacy: u32,
        legacy_length: u8,
        extended: Option<(u8, u64)>,
        flags: u32,
    ) -> Vec<u8> {
        let mut bytes = table(revision, length, 0);
        bytes[OFFSET_PM_TMR_BLK..OFFSET_PM_TMR_BLK + 4].copy_from_slice(&legacy.to_le_bytes());
        bytes[OFFSET_PM_TMR_LEN] = legacy_length;
        bytes[OFFSET_FLAGS..OFFSET_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
        if let Some((space_id, address)) = extended {
            let gas = OFFSET_X_PM_TMR_BLK;
            bytes[gas] = space_id;
            bytes[gas + 1] = 32;
            bytes[gas + 4..gas + 12].copy_from_slice(&address.to_le_bytes());
        }
        bytes
    }

    fn table(revision: u8, length: usize, iapc_boot_arch: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; length];
        bytes[..sdt::SIGNATURE_LENGTH].copy_from_slice(&SIGNATURE);
        bytes[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        bytes[OFFSET_REVISION] = revision;
        bytes[OFFSET_IAPC_BOOT_ARCH..OFFSET_IAPC_BOOT_ARCH + 2]
            .copy_from_slice(&iapc_boot_arch.to_le_bytes());
        bytes
    }

    /// QEMU の `q35` の形（リビジョン 3、244 バイト）。`i8042=off` ではビットが落ちる。
    #[test]
    fn a_revision_3_table_says_whether_there_is_an_8042() {
        let present = parse(&table(3, 244, BOOT_ARCH_8042)).unwrap();
        assert_eq!(present.iapc_boot_arch, Some(BOOT_ARCH_8042));
        assert_eq!(present.has_8042(), Some(true));
        let absent = parse(&table(3, 244, 0)).unwrap();
        assert_eq!(absent.has_8042(), Some(false));
    }

    /// **予約の位置を「無い」と読まない。** QEMU の `pc` の FADT はリビジョン 1 である。
    #[test]
    fn a_table_before_revision_3_does_not_say() {
        for revision in [0, 1, 2] {
            // 予約の位置に何が在っても読まない（0 も、ビットの立った値も）。
            for reserved in [0, BOOT_ARCH_8042, 0xFFFF] {
                let fadt = parse(&table(revision, V1_LENGTH, reserved)).unwrap();
                assert_eq!(fadt.iapc_boot_arch, None, "revision {revision}");
                assert_eq!(fadt.has_8042(), None, "revision {revision}");
            }
        }
    }

    /// **8042 のビットだけを見る。** 他のビット（レガシー装置・VGA 無し・MSI 無し・…）は答えを変えない。
    #[test]
    fn only_the_8042_bit_decides() {
        let others = 0xFFFF & !BOOT_ARCH_8042;
        assert_eq!(
            parse(&table(6, 276, others)).unwrap().has_8042(),
            Some(false)
        );
        assert_eq!(
            parse(&table(6, 276, others | BOOT_ARCH_8042))
                .unwrap()
                .has_8042(),
            Some(true)
        );
    }

    /// QEMU の `q35` の形（リビジョン 3、244 バイト、`X_PM_TMR_BLK` が 0x608、32 ビット）。
    #[test]
    fn the_extended_block_is_preferred_over_the_legacy_field() {
        let bytes = pm_table(
            3,
            244,
            0x408,
            4,
            Some((SPACE_SYSTEM_IO, 0x608)),
            FLAG_TMR_VAL_EXT,
        );
        assert_eq!(
            pm_timer_block(&bytes, 3),
            PmTimerBlock::Port {
                port: 0x608,
                bits: 32,
                from_extended: true
            }
        );
    }

    /// QEMU の `pc` の形（リビジョン 1、116 バイト。**`X_` の欄が無い**）。
    #[test]
    fn a_revision_1_table_falls_back_to_the_legacy_field() {
        let bytes = pm_table(1, V1_LENGTH, 0x608, 4, None, 0);
        assert_eq!(
            pm_timer_block(&bytes, 1),
            PmTimerBlock::Port {
                port: 0x608,
                bits: 24,
                from_extended: false
            },
            "TMR_VAL_EXT が落ちていれば 24 ビットである"
        );
    }

    /// **`X_` の欄が 0 なら旧い欄へ落ちる**（ACPICA と同じ）。
    #[test]
    fn a_zero_extended_block_falls_back_to_the_legacy_field() {
        let bytes = pm_table(
            3,
            244,
            0x608,
            4,
            Some((SPACE_SYSTEM_IO, 0)),
            FLAG_TMR_VAL_EXT,
        );
        assert_eq!(
            pm_timer_block(&bytes, 3),
            PmTimerBlock::Port {
                port: 0x608,
                bits: 32,
                from_extended: false
            }
        );
    }

    /// **無いときと、読めないときを分ける。**
    #[test]
    fn an_absent_or_unreadable_pm_timer_is_reported_as_such() {
        assert_eq!(
            pm_timer_block(&pm_table(3, 244, 0, 4, None, 0), 3),
            PmTimerBlock::Absent
        );
        assert_eq!(
            pm_timer_block(&pm_table(1, V1_LENGTH, 0x608, 0, None, 0), 1),
            PmTimerBlock::Absent,
            "PM_TMR_LEN が 4 でなければ読まない"
        );
        // ハードウェア縮小形の MMIO（Address Space ID 0 = System Memory）。
        assert_eq!(
            pm_timer_block(
                &pm_table(6, 276, 0, 4, Some((0, 0xfed0_0008)), FLAG_TMR_VAL_EXT),
                6
            ),
            PmTimerBlock::Unsupported {
                space_id: 0,
                address: 0xfed0_0008
            }
        );
    }

    #[test]
    fn a_table_shorter_than_acpi_1_0_is_rejected() {
        assert_eq!(
            parse(&table(3, V1_LENGTH - 1, BOOT_ARCH_8042)),
            Err(FadtError::TooShort {
                need: V1_LENGTH,
                got: V1_LENGTH - 1
            })
        );
    }
}
