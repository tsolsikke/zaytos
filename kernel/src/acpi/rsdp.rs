//! RSDP（Root System Description Pointer）の検証（S1-b-1）。
//!
//! バイトスライスの読み取りだけで完結する純粋ロジックであり、unsafe を一切
//! 使わない。ホスト上の `cargo test` で検証する。物理メモリからバイト列を
//! 取り出す部分（マップ済みかの確認と実際の読み取り）はハードウェア依存側
//! （[`super`]）の責務とし、ここには含めない（`memory_map` / `common::elf`
//! と同じ分け方である）。
//!
//! # 何段階に分かれているか
//!
//! 「何バイト読めばよいか」が読んだ結果で決まるため、1 回の呼び出しでは
//! 完結しない。ACPI 1.0 の RSDP は 20 バイト固定だが、2.0 以降は `length`
//! フィールドが全体の長さを名乗り、チェックサムはその長さ全体にかかる。
//! したがって
//!
//! 1. [`parse_header`]（先頭 20 バイト）で署名・チェックサム・`revision`
//! 2. [`declared_length`]（先頭 24 バイト）で名乗る長さ
//! 3. [`parse_extended`]（名乗った長さ全体）で拡張チェックサムと XSDT
//!
//! の順に、呼び出し側が必要な分だけ読み足しながら進む。

/// RSDP の署名。**末尾は空白 1 文字**で、8 バイトちょうどである。
pub const SIGNATURE: [u8; 8] = *b"RSD PTR ";

/// ACPI 1.0 の RSDP の長さ。`revision` によらず、この 20 バイトは必ずある。
pub const V1_LENGTH: usize = 20;

/// ACPI 2.0 以降の RSDP の長さ。`length` フィールドが名乗る値の下限でもある。
pub const V2_LENGTH: usize = 36;

/// `revision` がこの値以上なら拡張部（`length` / `xsdt_address` / 拡張
/// チェックサム）を持つ。ACPI 1.0 は 0、2.0 以降は 2 である（1 は仕様に無い）。
pub const EXTENDED_REVISION: u8 = 2;

/// 読み取りバッファの大きさ。**これを超える `length` を名乗る RSDP は検証できない。**
///
/// # この 64 という値に外部の根拠は無い
///
/// 仕様が定める長さでも、ファームウェアが名乗る値の上限でもない。**我々が
/// 選んだバッファの大きさである。** 仕様上 RSDP は 36 バイトだが、`length` は
/// 「テーブル全体の長さ」を名乗るフィールドなので、将来の版がこれより長い
/// RSDP を出すことは仕様の範囲内でありうる。そのときこちらが読める上限が
/// この値である。
///
/// # 超えた場合は「不正」ではなく「検証不能」である
///
/// 長すぎる RSDP が壊れているとは限らない。**壊れていると決めつけると、
/// 観測していないことを観測したと書くことになる。** かといって黙って 36 バイトで
/// 検算するのは「検証したことにする」であって検証ではない。したがって
/// [`DeclaredLength::NotVerifiable`] という third state を置く。`sti` 前の 7 項目が
/// 「検証済み / 失敗 / 検証不能」の 3 状態を持ち、PIC の再マップが書き込み専用
/// レジスタゆえに「検証不能」へ落ちるのと同じ形である（ADR-0018 §2）。
pub const READ_BUFFER_LENGTH: usize = 64;

// RSDP 内のフィールドオフセット（バイト）。
const OFFSET_SIGNATURE: usize = 0;
const OFFSET_OEM_ID: usize = 9;
const OFFSET_REVISION: usize = 15;
const OFFSET_RSDT_ADDRESS: usize = 16;
const OFFSET_LENGTH: usize = 20;
const OFFSET_XSDT_ADDRESS: usize = 24;

/// `length` を読むために最低限必要なバイト数。
const LENGTH_FIELD_END: usize = OFFSET_LENGTH + 4;

/// OEM ID のバイト数。
const OEM_ID_LENGTH: usize = 6;

/// チェックサムを計算した範囲。**どちらが破れたかを区別する。** 片方だけが
/// 破れる状況（拡張部だけ壊れている等）があり、丸めると原因が消える。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumScope {
    /// ACPI 1.0 部（先頭 20 バイト）。
    V1,
    /// `length` が示す全体（ACPI 2.0 以降）。
    Whole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsdpError {
    /// 検証に必要なバイト数に足りない。
    TooShort {
        need: usize,
        got: usize,
    },
    BadSignature {
        found: [u8; 8],
    },
    BadChecksum {
        scope: ChecksumScope,
        sum: u8,
    },
    /// `length` が [`V2_LENGTH`] 未満。構造体が名乗る長さとして成立しない。
    ///
    /// **これは「不正」である。** 仕様が定める最小サイズを下回る値は、どの版の
    /// ファームウェアであっても正当になりえない。長すぎる場合（検証不能）とは
    /// 区別する。
    LengthTooSmall {
        length: u32,
    },
}

/// バイト列の和（8 ビットで折り返す）。ACPI のチェックサムは「和が 0」である。
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
}

/// ACPI 1.0 部（先頭 20 バイト）の検証結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsdpHeader {
    pub revision: u8,
    pub oem_id: [u8; OEM_ID_LENGTH],
    /// RSDT の物理アドレス（32 ビット）。0 なら RSDT を持たない。
    pub rsdt_address: u32,
}

impl RsdpHeader {
    /// 拡張部（ACPI 2.0 以降）を持つか。
    pub fn has_extended_part(&self) -> bool {
        self.revision >= EXTENDED_REVISION
    }
}

/// 拡張部（ACPI 2.0 以降）の検証結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsdpExtended {
    /// RSDP 全体の長さ（チェックサムの対象範囲）。
    pub length: u32,
    /// XSDT の物理アドレス（64 ビット）。0 なら XSDT を持たない。
    pub xsdt_address: u64,
}

/// 先頭 20 バイトを検証する。署名・チェックサム・`revision` はここで確定する。
pub fn parse_header(bytes: &[u8]) -> Result<RsdpHeader, RsdpError> {
    if bytes.len() < V1_LENGTH {
        return Err(RsdpError::TooShort {
            need: V1_LENGTH,
            got: bytes.len(),
        });
    }

    let mut found = [0u8; SIGNATURE.len()];
    found.copy_from_slice(&bytes[OFFSET_SIGNATURE..OFFSET_SIGNATURE + SIGNATURE.len()]);
    if found != SIGNATURE {
        return Err(RsdpError::BadSignature { found });
    }

    // **チェックサムは署名の後に見る。** 順序を逆にすると、そもそも RSDP で
    // ないバイト列に対して「チェックサムが合わない」と報告することになり、
    // 原因の言い当てを誤る。
    let sum = checksum(&bytes[..V1_LENGTH]);
    if sum != 0 {
        return Err(RsdpError::BadChecksum {
            scope: ChecksumScope::V1,
            sum,
        });
    }

    let mut oem_id = [0u8; OEM_ID_LENGTH];
    oem_id.copy_from_slice(&bytes[OFFSET_OEM_ID..OFFSET_OEM_ID + OEM_ID_LENGTH]);

    Ok(RsdpHeader {
        revision: bytes[OFFSET_REVISION],
        oem_id,
        rsdt_address: u32::from_le_bytes(
            bytes[OFFSET_RSDT_ADDRESS..OFFSET_RSDT_ADDRESS + 4]
                .try_into()
                .unwrap(),
        ),
    })
}

/// 検証できると分かった長さ。**[`declared_length`] だけがこれを作れる。**
///
/// 中身が非公開なので、境界の外（`super`）は検証不能な長さを持ったまま
/// [`parse_extended`] を呼べない。「長さを確かめてから呼ぶ」を規律で守るのでは
/// なく、型で不可能にしている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiableLength(u32);

impl VerifiableLength {
    pub fn get(self) -> u32 {
        self.0
    }
}

/// 名乗られた長さの扱い。**「検証できる」と「検証できない」を分ける。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredLength {
    /// 読み取りバッファに収まる。名乗った長さ全体でチェックサムを検算できる。
    Verifiable(VerifiableLength),
    /// [`READ_BUFFER_LENGTH`] を超える長さを名乗っている。**壊れているとは
    /// 限らない**（将来の版なら正当でありうる）。こちらが読めないだけである。
    /// チェックサムを計算していないので、拡張部の値を信用してはならない。
    NotVerifiable { length: u32 },
}

/// 拡張部が名乗る長さを読む。
///
/// **チェックサムの検証より前に要る。** 何バイト読めばよいかがこの値で決まる
/// ため、独立した段にしてある。読み足す前にここで区分を決めないと、名乗った
/// 長さをそのまま信じて範囲外を読みに行くことになる。
pub fn declared_length(bytes: &[u8]) -> Result<DeclaredLength, RsdpError> {
    if bytes.len() < LENGTH_FIELD_END {
        return Err(RsdpError::TooShort {
            need: LENGTH_FIELD_END,
            got: bytes.len(),
        });
    }
    let length = u32::from_le_bytes(bytes[OFFSET_LENGTH..OFFSET_LENGTH + 4].try_into().unwrap());
    if (length as usize) < V2_LENGTH {
        return Err(RsdpError::LengthTooSmall { length });
    }
    if (length as usize) > READ_BUFFER_LENGTH {
        return Ok(DeclaredLength::NotVerifiable { length });
    }
    Ok(DeclaredLength::Verifiable(VerifiableLength(length)))
}

/// 拡張部を検証する。`length` は [`declared_length`] が `Verifiable` として
/// 返した値で、`bytes` はその長さ以上あること。
///
/// 超過分は無視する（チェックサムの対象は名乗った長さちょうどである）。
pub fn parse_extended(bytes: &[u8], length: VerifiableLength) -> Result<RsdpExtended, RsdpError> {
    let length = length.get();
    let length_usize = length as usize;
    if bytes.len() < length_usize {
        return Err(RsdpError::TooShort {
            need: length_usize,
            got: bytes.len(),
        });
    }

    let sum = checksum(&bytes[..length_usize]);
    if sum != 0 {
        return Err(RsdpError::BadChecksum {
            scope: ChecksumScope::Whole,
            sum,
        });
    }

    Ok(RsdpExtended {
        length,
        xsdt_address: u64::from_le_bytes(
            bytes[OFFSET_XSDT_ADDRESS..OFFSET_XSDT_ADDRESS + 8]
                .try_into()
                .unwrap(),
        ),
    })
}

/// 次に辿るテーブル。
///
/// **RSDT へ落ちる形を持つ。** revision 0 のファームウェアは XSDT を持たない
/// ので、XSDT だけを実装すると 1.0 しか出さないファームウェアで静かに
/// 「テーブル無し」になる。S1-a で RSDP の GUID を 1.0 / 2.0 の両方に対応
/// させたのと同じ理由である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootTable {
    /// 64 ビットのエントリを持つ（ACPI 2.0 以降）。
    Xsdt { phys: u64 },
    /// 32 ビットのエントリを持つ（ACPI 1.0）。
    Rsdt { phys: u32 },
}

/// 辿る先を決める。**どちらを辿るかはここだけで決まる。**
///
/// 判定は「revision >= 2 かつ XSDT のアドレスが 0 でなければ XSDT、そうで
/// なければ RSDT」である。`extended` が `None` なのは revision が 1.0 相当か、
/// 拡張部の検証に失敗した場合で、いずれも XSDT を信用できないので RSDT へ落とす。
/// 両方 0 なら辿る先が無い（`None`）。
pub fn root_table(header: &RsdpHeader, extended: Option<&RsdpExtended>) -> Option<RootTable> {
    if let Some(extended) = extended {
        if extended.xsdt_address != 0 {
            return Some(RootTable::Xsdt {
                phys: extended.xsdt_address,
            });
        }
    }
    if header.rsdt_address != 0 {
        return Some(RootTable::Rsdt {
            phys: header.rsdt_address,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `declared_length` が `Verifiable` を返すことを前提に長さを取り出す。
    /// 前提が崩れたテストは、その場で理由の分かる形で落ちてほしい。
    fn verifiable_length(bytes: &[u8]) -> VerifiableLength {
        match declared_length(bytes) {
            Ok(DeclaredLength::Verifiable(length)) => length,
            other => panic!("expected a verifiable length, got {other:?}"),
        }
    }

    /// チェックサムのバイトを後から埋めて、和が 0 になる RSDP を組み立てる。
    ///
    /// **テスト側で和を 0 にする計算を持つ。** 検証側と同じ関数で期待値を
    /// 作ると自己参照になるため、ここでは「0 から引く」形で独立に求める。
    fn build_rsdp(revision: u8, rsdt_address: u32, xsdt_address: u64, length: u32) -> Vec<u8> {
        let total = if revision >= EXTENDED_REVISION {
            (length as usize).max(V2_LENGTH)
        } else {
            V1_LENGTH
        };
        let mut bytes = vec![0u8; total];
        bytes[OFFSET_SIGNATURE..OFFSET_SIGNATURE + SIGNATURE.len()].copy_from_slice(&SIGNATURE);
        bytes[OFFSET_OEM_ID..OFFSET_OEM_ID + OEM_ID_LENGTH].copy_from_slice(b"ZAYTOS");
        bytes[OFFSET_REVISION] = revision;
        bytes[OFFSET_RSDT_ADDRESS..OFFSET_RSDT_ADDRESS + 4]
            .copy_from_slice(&rsdt_address.to_le_bytes());
        if revision >= EXTENDED_REVISION {
            bytes[OFFSET_LENGTH..OFFSET_LENGTH + 4].copy_from_slice(&length.to_le_bytes());
            bytes[OFFSET_XSDT_ADDRESS..OFFSET_XSDT_ADDRESS + 8]
                .copy_from_slice(&xsdt_address.to_le_bytes());
        }

        // v1 チェックサム（オフセット 8）: 先頭 20 バイトの和が 0 になる値。
        bytes[8] = 0u8.wrapping_sub(sum_of(&bytes[..V1_LENGTH]));
        if revision >= EXTENDED_REVISION {
            // 拡張チェックサム（オフセット 32）: length バイト全体の和が 0。
            let end = (length as usize).min(bytes.len());
            bytes[32] = 0u8.wrapping_sub(sum_of(&bytes[..end]));
        }
        bytes
    }

    fn sum_of(bytes: &[u8]) -> u8 {
        let mut sum = 0u8;
        for &b in bytes {
            sum = sum.wrapping_add(b);
        }
        sum
    }

    #[test]
    fn a_valid_v1_rsdp_parses() {
        let bytes = build_rsdp(0, 0x1234_5678, 0, 0);
        let header = parse_header(&bytes).unwrap();
        assert_eq!(header.revision, 0);
        assert_eq!(header.rsdt_address, 0x1234_5678);
        assert_eq!(&header.oem_id, b"ZAYTOS");
        assert!(!header.has_extended_part());
    }

    #[test]
    fn a_valid_v2_rsdp_parses_both_parts() {
        let bytes = build_rsdp(2, 0x1234_5678, 0xF77E_0000, V2_LENGTH as u32);
        let header = parse_header(&bytes).unwrap();
        assert!(header.has_extended_part());
        let extended = parse_extended(&bytes, verifiable_length(&bytes)).unwrap();
        assert_eq!(extended.length, V2_LENGTH as u32);
        assert_eq!(extended.xsdt_address, 0xF77E_0000);
    }

    #[test]
    fn a_bad_signature_is_rejected_before_the_checksum() {
        let mut bytes = build_rsdp(2, 1, 2, V2_LENGTH as u32);
        bytes[0] = b'X';
        match parse_header(&bytes) {
            Err(RsdpError::BadSignature { found }) => assert_eq!(found[0], b'X'),
            other => panic!("expected BadSignature, got {other:?}"),
        }
    }

    /// 署名の末尾は空白である。`"RSD PTR"` + NUL を通してはならない。
    #[test]
    fn the_signature_must_end_with_a_space() {
        let mut bytes = build_rsdp(0, 1, 0, 0);
        bytes[7] = 0;
        assert!(matches!(
            parse_header(&bytes),
            Err(RsdpError::BadSignature { .. })
        ));
    }

    #[test]
    fn a_broken_v1_checksum_is_detected() {
        let mut bytes = build_rsdp(0, 1, 0, 0);
        bytes[16] = bytes[16].wrapping_add(1);
        match parse_header(&bytes) {
            Err(RsdpError::BadChecksum {
                scope: ChecksumScope::V1,
                sum,
            }) => assert_ne!(sum, 0),
            other => panic!("expected a V1 BadChecksum, got {other:?}"),
        }
    }

    /// **v1 部が無傷でも拡張部だけが壊れることがある。** 範囲を分けて数えて
    /// いなければ、この壊れ方は素通りする。
    #[test]
    fn a_broken_extended_checksum_is_detected_while_the_v1_part_stays_valid() {
        let mut bytes = build_rsdp(2, 1, 0xF000, V2_LENGTH as u32);
        bytes[24] = bytes[24].wrapping_add(1);
        assert!(parse_header(&bytes).is_ok());
        match parse_extended(&bytes, verifiable_length(&bytes)) {
            Err(RsdpError::BadChecksum {
                scope: ChecksumScope::Whole,
                sum,
            }) => assert_ne!(sum, 0),
            other => panic!("expected a Whole BadChecksum, got {other:?}"),
        }
    }

    #[test]
    fn a_short_buffer_is_rejected() {
        let bytes = build_rsdp(0, 1, 0, 0);
        match parse_header(&bytes[..V1_LENGTH - 1]) {
            Err(RsdpError::TooShort { need, got }) => {
                assert_eq!(need, V1_LENGTH);
                assert_eq!(got, V1_LENGTH - 1);
            }
            other => panic!("expected TooShort, got {other:?}"),
        }
    }

    #[test]
    fn a_length_below_the_minimum_is_rejected() {
        let bytes = build_rsdp(2, 1, 0xF000, 24);
        assert_eq!(
            declared_length(&bytes),
            Err(RsdpError::LengthTooSmall { length: 24 })
        );
    }

    /// **読み取りバッファを超える長さは「不正」ではなく「検証不能」である。**
    /// 将来の版なら正当でありうるので、壊れていると決めつけない。黙って 36 バイトで
    /// 検算して「検証した」ことにもしない。この 2 つを両方避ける区分がこれである。
    #[test]
    fn a_length_above_the_read_buffer_is_not_verifiable_rather_than_invalid() {
        let mut bytes = build_rsdp(2, 1, 0xF000, V2_LENGTH as u32);
        let too_long = (READ_BUFFER_LENGTH + 1) as u32;
        bytes[OFFSET_LENGTH..OFFSET_LENGTH + 4].copy_from_slice(&too_long.to_le_bytes());
        assert_eq!(
            declared_length(&bytes),
            Ok(DeclaredLength::NotVerifiable { length: too_long })
        );
    }

    /// 読み取りバッファちょうどの長さは検証できる側にある（境界は含む）。
    #[test]
    fn a_length_exactly_at_the_read_buffer_is_still_verifiable() {
        let bytes = build_rsdp(2, 1, 0xF000, READ_BUFFER_LENGTH as u32);
        assert_eq!(verifiable_length(&bytes).get(), READ_BUFFER_LENGTH as u32);
    }

    /// 名乗った長さぶん読めていないなら、チェックサムは計算できない。
    #[test]
    fn an_extended_part_shorter_than_its_declared_length_is_rejected() {
        let bytes = build_rsdp(2, 1, 0xF000, 40);
        match parse_extended(&bytes[..38], verifiable_length(&bytes)) {
            Err(RsdpError::TooShort { need, got }) => {
                assert_eq!(need, 40);
                assert_eq!(got, 38);
            }
            other => panic!("expected TooShort, got {other:?}"),
        }
    }

    /// 36 バイトより長い RSDP でも、名乗った長さ全体でチェックサムが合えば通る。
    #[test]
    fn a_longer_than_minimal_rsdp_is_accepted_when_the_whole_range_sums_to_zero() {
        let bytes = build_rsdp(2, 1, 0xF000, 48);
        assert_eq!(
            parse_extended(&bytes, verifiable_length(&bytes))
                .unwrap()
                .length,
            48
        );
    }

    #[test]
    fn revision_two_with_an_xsdt_follows_the_xsdt() {
        let header = RsdpHeader {
            revision: 2,
            oem_id: *b"ZAYTOS",
            rsdt_address: 0x1000,
        };
        let extended = RsdpExtended {
            length: V2_LENGTH as u32,
            xsdt_address: 0xF77E_0000,
        };
        assert_eq!(
            root_table(&header, Some(&extended)),
            Some(RootTable::Xsdt { phys: 0xF77E_0000 })
        );
    }

    /// **XSDT のアドレスが 0 なら RSDT へ落ちる。** revision だけで決めると、
    /// ここで「テーブル無し」になる。
    #[test]
    fn revision_two_without_an_xsdt_falls_back_to_the_rsdt() {
        let header = RsdpHeader {
            revision: 2,
            oem_id: *b"ZAYTOS",
            rsdt_address: 0x1000,
        };
        let extended = RsdpExtended {
            length: V2_LENGTH as u32,
            xsdt_address: 0,
        };
        assert_eq!(
            root_table(&header, Some(&extended)),
            Some(RootTable::Rsdt { phys: 0x1000 })
        );
    }

    #[test]
    fn revision_zero_follows_the_rsdt() {
        let header = RsdpHeader {
            revision: 0,
            oem_id: *b"ZAYTOS",
            rsdt_address: 0x1000,
        };
        assert_eq!(
            root_table(&header, None),
            Some(RootTable::Rsdt { phys: 0x1000 })
        );
    }

    #[test]
    fn no_addresses_at_all_means_there_is_nothing_to_follow() {
        let header = RsdpHeader {
            revision: 0,
            oem_id: *b"ZAYTOS",
            rsdt_address: 0,
        };
        assert_eq!(root_table(&header, None), None);
    }

    #[test]
    fn the_checksum_of_an_empty_slice_is_zero() {
        assert_eq!(checksum(&[]), 0);
    }

    #[test]
    fn the_checksum_wraps_at_eight_bits() {
        assert_eq!(checksum(&[0xFF, 0x01]), 0);
        assert_eq!(checksum(&[0x80, 0x80, 0x03]), 3);
    }
}
