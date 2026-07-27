//! ACPI のシステム記述テーブル（SDT）の共通ヘッダと、ルートテーブルの
//! エントリ配列（S1-b-2）。
//!
//! バイトスライスの読み取りだけで完結する純粋ロジックであり、unsafe を一切
//! 使わない。ホスト上の `cargo test` で検証する。物理メモリからバイト列を
//! 取り出す部分は [`super`] の責務である。
//!
//! # ルートテーブルは 2 種類ある
//!
//! XSDT は 64 ビットのエントリ、RSDT は 32 ビットのエントリを持つ。**どちらも
//! ヘッダは同じ 36 バイトである。** エントリの幅だけが違うので、[`RootEntries`]
//! が幅を持って両方を扱う。

/// 共通ヘッダの長さ。すべての SDT がこの長さのヘッダで始まる。
pub const HEADER_LENGTH: usize = 36;

/// 署名の長さ。
pub const SIGNATURE_LENGTH: usize = 4;

const OFFSET_SIGNATURE: usize = 0;
const OFFSET_LENGTH: usize = 4;
const OFFSET_REVISION: usize = 8;
const OFFSET_OEM_ID: usize = 10;

/// OEM ID のバイト数。
const OEM_ID_LENGTH: usize = 6;

/// XSDT の署名。
pub const XSDT_SIGNATURE: [u8; SIGNATURE_LENGTH] = *b"XSDT";

/// RSDT の署名。
pub const RSDT_SIGNATURE: [u8; SIGNATURE_LENGTH] = *b"RSDT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdtError {
    /// 検証に必要なバイト数に足りない。
    TooShort {
        need: usize,
        got: usize,
    },
    /// 期待した署名と違う。
    SignatureMismatch {
        expected: [u8; SIGNATURE_LENGTH],
        found: [u8; SIGNATURE_LENGTH],
    },
    /// `length` がそのテーブルの最小サイズを下回る。**不正である**
    /// （どの版のファームウェアでも正当になりえない）。
    LengthTooSmall {
        length: u32,
        minimum: u32,
    },
    BadChecksum {
        sum: u8,
    },
}

/// 共通ヘッダ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdtHeader {
    pub signature: [u8; SIGNATURE_LENGTH],
    /// テーブル全体の長さ（ヘッダを含む）。チェックサムの対象範囲でもある。
    pub length: u32,
    pub revision: u8,
    pub oem_id: [u8; OEM_ID_LENGTH],
}

impl SdtHeader {
    pub fn has_signature(&self, signature: &[u8; SIGNATURE_LENGTH]) -> bool {
        &self.signature == signature
    }
}

/// 共通ヘッダを読む。**チェックサムはここでは見られない**（テーブル全体を
/// 読むには、まずここで `length` を知る必要がある）。
///
/// `minimum_length` はそのテーブルが名乗ってよい最小の長さである。共通ヘッダ
/// だけなら [`HEADER_LENGTH`]、MADT のように固定部が長いテーブルではその長さを
/// 渡す。**呼び出し側に決めさせるのは、テーブルごとに違うからである。**
pub fn parse_header(bytes: &[u8], minimum_length: u32) -> Result<SdtHeader, SdtError> {
    if bytes.len() < HEADER_LENGTH {
        return Err(SdtError::TooShort {
            need: HEADER_LENGTH,
            got: bytes.len(),
        });
    }
    let mut signature = [0u8; SIGNATURE_LENGTH];
    signature.copy_from_slice(&bytes[OFFSET_SIGNATURE..OFFSET_SIGNATURE + SIGNATURE_LENGTH]);
    let length = u32::from_le_bytes(bytes[OFFSET_LENGTH..OFFSET_LENGTH + 4].try_into().unwrap());
    if length < minimum_length {
        return Err(SdtError::LengthTooSmall {
            length,
            minimum: minimum_length,
        });
    }
    let mut oem_id = [0u8; OEM_ID_LENGTH];
    oem_id.copy_from_slice(&bytes[OFFSET_OEM_ID..OFFSET_OEM_ID + OEM_ID_LENGTH]);
    Ok(SdtHeader {
        signature,
        length,
        revision: bytes[OFFSET_REVISION],
        oem_id,
    })
}

/// 署名を照合する。**ヘッダの解析とは分けてある。** 「読めたが期待した表では
/// なかった」と「そもそもヘッダとして壊れている」は別の事実である。
pub fn check_signature(
    header: &SdtHeader,
    expected: &[u8; SIGNATURE_LENGTH],
) -> Result<(), SdtError> {
    if header.has_signature(expected) {
        return Ok(());
    }
    Err(SdtError::SignatureMismatch {
        expected: *expected,
        found: header.signature,
    })
}

/// テーブル全体のチェックサムを検算する。`bytes` は `length` バイト以上あること。
pub fn verify_checksum(bytes: &[u8], length: u32) -> Result<(), SdtError> {
    let length = length as usize;
    if bytes.len() < length {
        return Err(SdtError::TooShort {
            need: length,
            got: bytes.len(),
        });
    }
    let sum = super::rsdp::checksum(&bytes[..length]);
    if sum != 0 {
        return Err(SdtError::BadChecksum { sum });
    }
    Ok(())
}

/// ルートテーブルのエントリ幅。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryWidth {
    /// XSDT。64 ビット。
    Xsdt,
    /// RSDT。32 ビット。
    Rsdt,
}

impl EntryWidth {
    pub const fn bytes(self) -> usize {
        match self {
            EntryWidth::Xsdt => 8,
            EntryWidth::Rsdt => 4,
        }
    }

    pub const fn signature(self) -> [u8; SIGNATURE_LENGTH] {
        match self {
            EntryWidth::Xsdt => XSDT_SIGNATURE,
            EntryWidth::Rsdt => RSDT_SIGNATURE,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            EntryWidth::Xsdt => "XSDT",
            EntryWidth::Rsdt => "RSDT",
        }
    }
}

/// ルートテーブルのエントリ配列。
///
/// # エントリは境界に載らない
///
/// ヘッダが 36 バイトなので、**XSDT の 64 ビットエントリの配列は 8 バイト境界に
/// 載らない**（36 は 8 の倍数ではない）。ここはバイトスライスから
/// `from_le_bytes` で読むので、そもそも整列の要件が無い。物理メモリ側でも
/// [`super::PhysReader`] がバッファへ写してから解釈するので、未整列の生ポインタは
/// 経路のどこにも現れない。
#[derive(Debug, Clone, Copy)]
pub struct RootEntries<'a> {
    entries: &'a [u8],
    width: EntryWidth,
    index: usize,
}

impl<'a> RootEntries<'a> {
    /// `table` は `length` バイトちょうど（以上）であること。
    pub fn new(table: &'a [u8], length: u32, width: EntryWidth) -> Self {
        let length = (length as usize).min(table.len());
        let entries = &table[HEADER_LENGTH.min(length)..length];
        Self {
            entries,
            width,
            index: 0,
        }
    }

    /// エントリ数。**端数は数に入らない。**
    ///
    /// **`count` という名前にしない。** [`Iterator::count`] と衝突し、
    /// `entries.count()` は（インヘレントメソッドがあるにもかかわらず）値
    /// レシーバで先に一致する `Iterator::count` を呼ぶ。実際にそうなって、
    /// こちらが `dead_code` になったのをコンパイラが教えてくれた。名前が
    /// 違えば取り違えようがない。
    pub fn entry_count(&self) -> usize {
        self.entries.len() / self.width.bytes()
    }

    /// エントリ配列の末尾に、1 エントリに満たない端数が残っているか。
    ///
    /// **黙って切り捨てない。** 端数があるということは `length` かエントリ幅の
    /// どちらかについての理解が違うということで、観測結果として残す価値がある。
    pub fn trailing_bytes(&self) -> usize {
        self.entries.len() % self.width.bytes()
    }
}

impl Iterator for RootEntries<'_> {
    /// エントリが指す物理アドレス。RSDT のときは 32 ビット値を広げたもの。
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        let width = self.width.bytes();
        let start = self.index * width;
        let end = start + width;
        if end > self.entries.len() {
            return None;
        }
        self.index += 1;
        Some(match self.width {
            EntryWidth::Xsdt => u64::from_le_bytes(self.entries[start..end].try_into().unwrap()),
            EntryWidth::Rsdt => {
                u32::from_le_bytes(self.entries[start..end].try_into().unwrap()) as u64
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// チェックサムのバイト（オフセット 9）を後から埋めて、和が 0 になる
    /// テーブルを組み立てる。
    fn build_table(signature: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let length = HEADER_LENGTH + body.len();
        let mut bytes = vec![0u8; length];
        bytes[OFFSET_SIGNATURE..OFFSET_SIGNATURE + SIGNATURE_LENGTH].copy_from_slice(signature);
        bytes[OFFSET_LENGTH..OFFSET_LENGTH + 4].copy_from_slice(&(length as u32).to_le_bytes());
        bytes[OFFSET_REVISION] = 1;
        bytes[OFFSET_OEM_ID..OFFSET_OEM_ID + OEM_ID_LENGTH].copy_from_slice(b"ZAYTOS");
        bytes[HEADER_LENGTH..].copy_from_slice(body);
        let mut sum = 0u8;
        for &b in bytes.iter() {
            sum = sum.wrapping_add(b);
        }
        bytes[9] = 0u8.wrapping_sub(sum);
        bytes
    }

    fn xsdt_with(addresses: &[u64]) -> Vec<u8> {
        let mut body = Vec::new();
        for &a in addresses {
            body.extend_from_slice(&a.to_le_bytes());
        }
        build_table(&XSDT_SIGNATURE, &body)
    }

    #[test]
    fn a_valid_header_parses() {
        let bytes = xsdt_with(&[0x1000, 0x2000]);
        let header = parse_header(&bytes, HEADER_LENGTH as u32).unwrap();
        assert!(header.has_signature(&XSDT_SIGNATURE));
        assert_eq!(header.length, (HEADER_LENGTH + 16) as u32);
        assert_eq!(&header.oem_id, b"ZAYTOS");
        assert_eq!(verify_checksum(&bytes, header.length), Ok(()));
    }

    #[test]
    fn a_length_below_the_minimum_is_rejected() {
        let mut bytes = xsdt_with(&[0x1000]);
        bytes[OFFSET_LENGTH..OFFSET_LENGTH + 4].copy_from_slice(&8u32.to_le_bytes());
        assert_eq!(
            parse_header(&bytes, HEADER_LENGTH as u32),
            Err(SdtError::LengthTooSmall {
                length: 8,
                minimum: HEADER_LENGTH as u32
            })
        );
    }

    /// 呼び出し側が渡す最小長は、テーブルごとに違う。共通ヘッダとしては
    /// 通る長さでも、より長い固定部を持つテーブルとしては短すぎることがある。
    #[test]
    fn the_minimum_length_is_the_callers_to_choose() {
        let bytes = xsdt_with(&[]);
        assert!(parse_header(&bytes, HEADER_LENGTH as u32).is_ok());
        assert_eq!(
            parse_header(&bytes, 44),
            Err(SdtError::LengthTooSmall {
                length: HEADER_LENGTH as u32,
                minimum: 44
            })
        );
    }

    #[test]
    fn a_short_buffer_is_rejected() {
        let bytes = xsdt_with(&[]);
        assert_eq!(
            parse_header(&bytes[..HEADER_LENGTH - 1], HEADER_LENGTH as u32),
            Err(SdtError::TooShort {
                need: HEADER_LENGTH,
                got: HEADER_LENGTH - 1
            })
        );
    }

    #[test]
    fn a_signature_mismatch_is_reported_with_both_sides() {
        let bytes = xsdt_with(&[]);
        let header = parse_header(&bytes, HEADER_LENGTH as u32).unwrap();
        assert_eq!(
            check_signature(&header, &RSDT_SIGNATURE),
            Err(SdtError::SignatureMismatch {
                expected: RSDT_SIGNATURE,
                found: XSDT_SIGNATURE,
            })
        );
    }

    #[test]
    fn a_broken_checksum_is_detected() {
        let mut bytes = xsdt_with(&[0x1000]);
        let header = parse_header(&bytes, HEADER_LENGTH as u32).unwrap();
        let last = bytes.len() - 1;
        bytes[last] = bytes[last].wrapping_add(1);
        match verify_checksum(&bytes, header.length) {
            Err(SdtError::BadChecksum { sum }) => assert_ne!(sum, 0),
            other => panic!("expected BadChecksum, got {other:?}"),
        }
    }

    /// チェックサムは `length` が示す範囲だけを見る。後ろに何が付いていても
    /// 結果は変わらない。
    #[test]
    fn the_checksum_covers_only_the_declared_length() {
        let mut bytes = xsdt_with(&[0x1000]);
        let header = parse_header(&bytes, HEADER_LENGTH as u32).unwrap();
        bytes.push(0xFF);
        assert_eq!(verify_checksum(&bytes, header.length), Ok(()));
    }

    #[test]
    fn xsdt_entries_are_read_as_64_bit_values() {
        let bytes = xsdt_with(&[0xF77D_0074, 0x1_0000_0000]);
        let header = parse_header(&bytes, HEADER_LENGTH as u32).unwrap();
        let entries = RootEntries::new(&bytes, header.length, EntryWidth::Xsdt);
        assert_eq!(entries.entry_count(), 2);
        assert_eq!(entries.trailing_bytes(), 0);
        let collected: Vec<u64> = entries.collect();
        assert_eq!(collected, vec![0xF77D_0074, 0x1_0000_0000]);
    }

    #[test]
    fn rsdt_entries_are_read_as_32_bit_values_widened_to_64() {
        let mut body = Vec::new();
        body.extend_from_slice(&0xF77D_0074u32.to_le_bytes());
        body.extend_from_slice(&0x1000u32.to_le_bytes());
        let bytes = build_table(&RSDT_SIGNATURE, &body);
        let header = parse_header(&bytes, HEADER_LENGTH as u32).unwrap();
        let collected: Vec<u64> =
            RootEntries::new(&bytes, header.length, EntryWidth::Rsdt).collect();
        assert_eq!(collected, vec![0xF77D_0074, 0x1000]);
    }

    /// 端数は数にも列挙にも入らないが、**あったことは分かる。**
    #[test]
    fn a_trailing_partial_entry_is_reported_rather_than_silently_dropped() {
        let mut bytes = xsdt_with(&[0x1000]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let length = bytes.len() as u32;
        bytes[OFFSET_LENGTH..OFFSET_LENGTH + 4].copy_from_slice(&length.to_le_bytes());
        let entries = RootEntries::new(&bytes, length, EntryWidth::Xsdt);
        assert_eq!(entries.entry_count(), 1);
        assert_eq!(entries.trailing_bytes(), 4);
        assert_eq!(entries.entry_count(), entries.into_iter().count());
    }

    #[test]
    fn a_root_table_with_no_entries_yields_nothing() {
        let bytes = xsdt_with(&[]);
        let entries = RootEntries::new(&bytes, HEADER_LENGTH as u32, EntryWidth::Xsdt);
        assert_eq!(entries.entry_count(), 0);
        assert_eq!(entries.trailing_bytes(), 0);
        assert_eq!(entries.into_iter().count(), 0);
    }
}
