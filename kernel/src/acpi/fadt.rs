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
const OFFSET_IAPC_BOOT_ARCH: usize = 109;

/// `IAPC_BOOT_ARCH` の 8042 のビットが定義された最初のリビジョン。
pub const FIRST_REVISION_WITH_THE_8042_FLAG: u8 = 3;

/// `IAPC_BOOT_ARCH` の bit 1: ポート 0x60/0x64 の 8042 が在る。
pub const BOOT_ARCH_8042: u16 = 1 << 1;

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
