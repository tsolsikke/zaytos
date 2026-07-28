//! MADT（Multiple APIC Description Table）の解析（S1-b-2）。
//!
//! バイトスライスの読み取りだけで完結する純粋ロジックであり、unsafe を一切
//! 使わない。ホスト上の `cargo test` で検証する。
//!
//! # 走査ループが止まることの保証
//!
//! MADT の本体は「type(1) + length(1) + 本体」という可変長エントリの列である。
//! **`length` はファームウェアが書いた値なので、0 でありうる。** 素朴に
//! `offset += length` と書くと、0 のとき同じ位置を読み続けて**無限ループ**に
//! なる。カーネルはそこで黙って止まり、症状は「起動ログが途中で止まる」に
//! なるので、原因の切り分けが難しい形で出る。
//!
//! [`Entries`] は次の 2 つで守っている。**どちらか片方では足りない。**
//!
//! 1. **エントリ長。** `length < MIN_ENTRY_LENGTH`（type と length 自身の
//!    2 バイト）なら [`EntryError::ZeroLength`] か
//!    [`EntryError::LengthBelowMinimum`] で停止する。したがって前進量は
//!    必ず 2 以上になる。
//! 2. **残り長。** `length > 残りバイト数` なら
//!    [`EntryError::LengthExceedsRemaining`] で停止する。したがって残りは
//!    負にならず、スライスの範囲外にも出ない。
//!
//! 1 だけでは、最後のエントリが残りを超える長さを名乗ったときに範囲外へ出る。
//! 2 だけでは、`length == 0` のとき残りが減らずに回り続ける。両方あって初めて
//! 「残りは単調に減り、かつ 0 未満にならない」が成立し、有限回で終わる。
//! **停止は打ち切りカウンタではなく、この不変条件で保証する。** カウンタは
//! 「いくつまでなら正常か」という別の根拠のない数字を持ち込むことになる。

use super::sdt;

/// MADT の署名。
pub const SIGNATURE: [u8; sdt::SIGNATURE_LENGTH] = *b"APIC";

/// MADT の固定部の長さ。共通ヘッダ 36 + Local APIC Address 4 + Flags 4。
/// **`length` はこれ以上でなければならない。**
pub const FIXED_LENGTH: usize = 44;

const OFFSET_LOCAL_APIC_ADDRESS: usize = 36;
const OFFSET_FLAGS: usize = 40;

/// Flags のビット 0。**立っていると「この系にはデュアル 8259 がある」**という
/// 意味で、Local APIC / IO-APIC へ移る前に PIC をマスクする必要があることを示す。
///
/// **S1 では解釈しない。値をログへ出して記録するだけである。** 実際に PIC を
/// どう扱うかは S2 の判断で、その入力としてここに残す。
pub const FLAG_PCAT_COMPAT: u32 = 1 << 0;

/// エントリのヘッダ（type と length）の長さ。前進量の下限でもある。
pub const MIN_ENTRY_LENGTH: usize = 2;

// 解釈するエントリ種別。ここに無いものは「未知」として type と length を
// ログへ出す（黙って読み飛ばさない）。
//
// **入れ子の `pub mod entry_type` へ戻さないこと。** この境界（`kernel/src/acpi/`）は
// 可視性の静的検査の対象で、配下の `mod` 宣言に可視性修飾を付けられない。
// 実効可視性としては親が非公開なので外からは触れないが、`mod madt` を `pub` に
// する 1 文字で漏れる形へ変わるため、検査は保守的に禁じている。**実際にここで
// 発火した**（`pub mod entry_type` と書いて FAIL した）。接頭辞で名前空間を
// 作れば同じ読みやすさが得られるので、整理のつもりで入れ子へ戻さない。
pub const TYPE_LOCAL_APIC: u8 = 0;
pub const TYPE_IO_APIC: u8 = 1;
pub const TYPE_INTERRUPT_SOURCE_OVERRIDE: u8 = 2;
pub const TYPE_NMI_SOURCE: u8 = 3;
pub const TYPE_LOCAL_APIC_NMI: u8 = 4;
pub const TYPE_LOCAL_APIC_ADDRESS_OVERRIDE: u8 = 5;
pub const TYPE_LOCAL_X2APIC: u8 = 9;
pub const TYPE_LOCAL_X2APIC_NMI: u8 = 10;

/// 種別ごとの最小長（仕様が定める固定長）。
///
/// **未知の種別には最小長が無い。** 分かるのは「type と length が入る
/// [`MIN_ENTRY_LENGTH`] 以上であること」だけなので、その値を返す。将来の版が
/// 足した種別を、こちらの無知を理由に不正と報告しないためである。
pub const fn minimum_length(entry_type: u8) -> u8 {
    match entry_type {
        TYPE_LOCAL_APIC => 8,
        TYPE_IO_APIC => 12,
        TYPE_INTERRUPT_SOURCE_OVERRIDE => 10,
        TYPE_NMI_SOURCE => 8,
        TYPE_LOCAL_APIC_NMI => 6,
        TYPE_LOCAL_APIC_ADDRESS_OVERRIDE => 12,
        TYPE_LOCAL_X2APIC => 16,
        TYPE_LOCAL_X2APIC_NMI => 12,
        _ => MIN_ENTRY_LENGTH as u8,
    }
}

/// 種別の名前（ログ用）。未知の種別は数値だけで報告する。
pub const fn entry_type_name(entry_type: u8) -> &'static str {
    match entry_type {
        TYPE_LOCAL_APIC => "Processor Local APIC",
        TYPE_IO_APIC => "I/O APIC",
        TYPE_INTERRUPT_SOURCE_OVERRIDE => "Interrupt Source Override",
        TYPE_NMI_SOURCE => "NMI Source",
        TYPE_LOCAL_APIC_NMI => "Local APIC NMI",
        TYPE_LOCAL_APIC_ADDRESS_OVERRIDE => "Local APIC Address Override",
        TYPE_LOCAL_X2APIC => "Processor Local x2APIC",
        TYPE_LOCAL_X2APIC_NMI => "Local x2APIC NMI",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadtError {
    TooShort { need: usize, got: usize },
}

/// MADT の固定部。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MadtHeader {
    /// Local APIC の MMIO 物理アドレス（32 ビット）。
    ///
    /// **既定値をハードコードしない根拠がここにある。** この値はファームウェアが
    /// 提示するもので、`0xFEE00000` は既定にすぎない。さらに
    /// [`TYPE_LOCAL_APIC_ADDRESS_OVERRIDE`] のエントリがあれば、そちらが
    /// 優先される。
    pub local_apic_address: u32,
    pub flags: u32,
}

impl MadtHeader {
    /// PCAT_COMPAT が立っているか。**判断には使わない**（S2 への入力である）。
    pub fn pcat_compat(&self) -> bool {
        self.flags & FLAG_PCAT_COMPAT != 0
    }
}

/// 固定部を読む。`bytes` は [`FIXED_LENGTH`] 以上あること。
pub fn parse_header(bytes: &[u8]) -> Result<MadtHeader, MadtError> {
    if bytes.len() < FIXED_LENGTH {
        return Err(MadtError::TooShort {
            need: FIXED_LENGTH,
            got: bytes.len(),
        });
    }
    Ok(MadtHeader {
        local_apic_address: u32::from_le_bytes(
            bytes[OFFSET_LOCAL_APIC_ADDRESS..OFFSET_LOCAL_APIC_ADDRESS + 4]
                .try_into()
                .unwrap(),
        ),
        flags: u32::from_le_bytes(bytes[OFFSET_FLAGS..OFFSET_FLAGS + 4].try_into().unwrap()),
    })
}

/// 走査を打ち切った理由。**「終わった」と「壊れていた」を区別する。**
/// 前者はエントリを使い切った場合で、[`Entries`] は単に `None` を返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryError {
    /// type と length を読むには残りが足りない。
    TruncatedHeader { remaining: usize },
    /// `length` が 0。**前進しないので、そのまま進めば無限ループになる。**
    /// 最小長違反の一種だが、症状が致命的なので独立させてある。
    ZeroLength,
    /// `length` が種別の最小長を下回る。
    LengthBelowMinimum {
        entry_type: u8,
        length: u8,
        minimum: u8,
    },
    /// `length` が残りバイト数を超える。
    LengthExceedsRemaining {
        entry_type: u8,
        length: u8,
        remaining: usize,
    },
}

/// エントリ 1 つ分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'a> {
    pub entry_type: u8,
    pub length: u8,
    /// type と length を含むエントリ全体。
    pub bytes: &'a [u8],
}

/// MADT の可変長エントリ列。
///
/// **異常を見つけたらそこで止まる。** 1 つ壊れたエントリの先を読み進めても、
/// 位置が同期していない以上、読めるのはゴミである。`Err` を 1 回返した後は
/// `None` を返し続ける。
#[derive(Debug, Clone, Copy)]
pub struct Entries<'a> {
    rest: &'a [u8],
    stopped: bool,
}

impl<'a> Entries<'a> {
    /// `body` は MADT の [`FIXED_LENGTH`] 以降、`length` までの範囲。
    pub fn new(body: &'a [u8]) -> Self {
        Self {
            rest: body,
            stopped: false,
        }
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>, EntryError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped || self.rest.is_empty() {
            return None;
        }
        // 以降、どの経路で抜けても `rest` は必ず縮むか `stopped` が立つ。
        // モジュールの doc の不変条件はこの関数だけで閉じている。
        if self.rest.len() < MIN_ENTRY_LENGTH {
            self.stopped = true;
            return Some(Err(EntryError::TruncatedHeader {
                remaining: self.rest.len(),
            }));
        }
        let entry_type = self.rest[0];
        let length = self.rest[1];
        if length == 0 {
            self.stopped = true;
            return Some(Err(EntryError::ZeroLength));
        }
        let minimum = minimum_length(entry_type);
        if length < minimum {
            self.stopped = true;
            return Some(Err(EntryError::LengthBelowMinimum {
                entry_type,
                length,
                minimum,
            }));
        }
        if length as usize > self.rest.len() {
            self.stopped = true;
            return Some(Err(EntryError::LengthExceedsRemaining {
                entry_type,
                length,
                remaining: self.rest.len(),
            }));
        }
        let (entry, rest) = self.rest.split_at(length as usize);
        self.rest = rest;
        Some(Ok(Entry {
            entry_type,
            length,
            bytes: entry,
        }))
    }
}

/// Processor Local APIC（type 0）の中身。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalApic {
    pub processor_uid: u8,
    pub apic_id: u8,
    pub flags: u32,
}

/// Local APIC / x2APIC の Flags のビット 0。立っていれば使用可能である。
pub const FLAG_ENABLED: u32 = 1 << 0;

/// 同ビット 1。`Enabled` が落ちていても、後から online にできる。
pub const FLAG_ONLINE_CAPABLE: u32 = 1 << 1;

impl LocalApic {
    /// 起動対象になりうるか。**S1 では判断に使わない**（記録のためだけ）。
    pub fn usable(&self) -> bool {
        self.flags & (FLAG_ENABLED | FLAG_ONLINE_CAPABLE) != 0
    }
}

/// type 0 として読む。長さが足りなければ `None`（走査側が最小長を確かめて
/// いるので通常は起きないが、呼び違えても壊れないようにしておく）。
pub fn parse_local_apic(entry: &Entry<'_>) -> Option<LocalApic> {
    if entry.entry_type != TYPE_LOCAL_APIC || entry.bytes.len() < 8 {
        return None;
    }
    Some(LocalApic {
        processor_uid: entry.bytes[2],
        apic_id: entry.bytes[3],
        flags: u32::from_le_bytes(entry.bytes[4..8].try_into().unwrap()),
    })
}

/// Processor Local x2APIC（type 9）の中身。**APIC ID が 32 ビットになる。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalX2Apic {
    pub apic_id: u32,
    pub flags: u32,
    pub processor_uid: u32,
}

pub fn parse_local_x2apic(entry: &Entry<'_>) -> Option<LocalX2Apic> {
    if entry.entry_type != TYPE_LOCAL_X2APIC || entry.bytes.len() < 16 {
        return None;
    }
    Some(LocalX2Apic {
        apic_id: u32::from_le_bytes(entry.bytes[4..8].try_into().unwrap()),
        flags: u32::from_le_bytes(entry.bytes[8..12].try_into().unwrap()),
        processor_uid: u32::from_le_bytes(entry.bytes[12..16].try_into().unwrap()),
    })
}

/// I/O APIC（type 1）の中身。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoApic {
    pub id: u8,
    /// MMIO の物理アドレス。**既定値 `0xFEC00000` をハードコードせず、ここから取る。**
    pub address: u32,
    /// この I/O APIC が担当する割り込みの先頭 GSI。
    pub global_system_interrupt_base: u32,
}

pub fn parse_io_apic(entry: &Entry<'_>) -> Option<IoApic> {
    if entry.entry_type != TYPE_IO_APIC || entry.bytes.len() < 12 {
        return None;
    }
    Some(IoApic {
        id: entry.bytes[2],
        address: u32::from_le_bytes(entry.bytes[4..8].try_into().unwrap()),
        global_system_interrupt_base: u32::from_le_bytes(entry.bytes[8..12].try_into().unwrap()),
    })
}

/// Interrupt Source Override（type 2）の中身。
///
/// **レガシー IRQ が、IO-APIC のどの GSI へ現れるかを述べる表である。**
/// PIC では IRQ 番号がそのまま線の番号だったが、IO-APIC では
/// 「レガシー IRQ n は GSI m へ配線されている」という対応が入りうる。
/// 典型は IRQ0 が GSI2 へ移る形だが、**それは仕様が決めることではなく
/// ファームウェアが述べることである。**表を読んで従う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptSourceOverride {
    /// 常に 0（ISA）。仕様が他の値を定義していないが、読んで記録する。
    pub bus: u8,
    /// 元のレガシー IRQ 番号。
    pub source: u8,
    /// 実際に現れる GSI。
    pub global_system_interrupt: u32,
    /// 極性とトリガ。[`INTI_POLARITY_MASK`] / [`INTI_TRIGGER_MASK`] で取り出す。
    pub flags: u16,
}

/// MPS INTI フラグの極性（bits 1:0）とトリガ（bits 3:2）。
///
/// どちらも `00` は「バスの既定に従う」で、ISA の既定は
/// **アクティブハイ・エッジトリガ**である。
pub const INTI_POLARITY_MASK: u16 = 0b11;
pub const INTI_TRIGGER_MASK: u16 = 0b11 << 2;
pub const INTI_POLARITY_ACTIVE_LOW: u16 = 0b11;
pub const INTI_TRIGGER_LEVEL: u16 = 0b11 << 2;

impl InterruptSourceOverride {
    /// 極性の生値（bits 1:0）。
    pub const fn polarity(&self) -> u16 {
        self.flags & INTI_POLARITY_MASK
    }

    /// トリガの生値（bits 3:2）。
    pub const fn trigger(&self) -> u16 {
        self.flags & INTI_TRIGGER_MASK
    }

    /// アクティブローか。**`00`（バス既定）は ISA ではアクティブハイなので偽である。**
    pub const fn active_low(&self) -> bool {
        self.polarity() == INTI_POLARITY_ACTIVE_LOW
    }

    /// レベルトリガか。**`00`（バス既定）は ISA ではエッジなので偽である。**
    pub const fn level_triggered(&self) -> bool {
        self.trigger() == INTI_TRIGGER_LEVEL
    }
}

/// type 2 として読む。長さが足りなければ `None`。
pub fn parse_interrupt_source_override(entry: &Entry<'_>) -> Option<InterruptSourceOverride> {
    if entry.entry_type != TYPE_INTERRUPT_SOURCE_OVERRIDE || entry.bytes.len() < 10 {
        return None;
    }
    Some(InterruptSourceOverride {
        bus: entry.bytes[2],
        source: entry.bytes[3],
        global_system_interrupt: u32::from_le_bytes(entry.bytes[4..8].try_into().unwrap()),
        flags: u16::from_le_bytes(entry.bytes[8..10].try_into().unwrap()),
    })
}

/// Local APIC Address Override（type 5）。**あれば固定部の 32 ビット値より優先される。**
pub fn parse_local_apic_address_override(entry: &Entry<'_>) -> Option<u64> {
    if entry.entry_type != TYPE_LOCAL_APIC_ADDRESS_OVERRIDE || entry.bytes.len() < 12 {
        return None;
    }
    Some(u64::from_le_bytes(entry.bytes[4..12].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_bytes(local_apic_address: u32, flags: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; FIXED_LENGTH];
        bytes[..sdt::SIGNATURE_LENGTH].copy_from_slice(&SIGNATURE);
        bytes[OFFSET_LOCAL_APIC_ADDRESS..OFFSET_LOCAL_APIC_ADDRESS + 4]
            .copy_from_slice(&local_apic_address.to_le_bytes());
        bytes[OFFSET_FLAGS..OFFSET_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
        bytes
    }

    fn local_apic_entry(uid: u8, apic_id: u8, flags: u32) -> Vec<u8> {
        let mut bytes = vec![TYPE_LOCAL_APIC, 8, uid, apic_id];
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes
    }

    fn io_apic_entry(id: u8, address: u32, gsi_base: u32) -> Vec<u8> {
        let mut bytes = vec![TYPE_IO_APIC, 12, id, 0];
        bytes.extend_from_slice(&address.to_le_bytes());
        bytes.extend_from_slice(&gsi_base.to_le_bytes());
        bytes
    }

    #[test]
    fn the_fixed_part_yields_the_apic_address_and_flags() {
        let bytes = header_bytes(0xFEE0_0000, FLAG_PCAT_COMPAT);
        let header = parse_header(&bytes).unwrap();
        assert_eq!(header.local_apic_address, 0xFEE0_0000);
        assert!(header.pcat_compat());
    }

    #[test]
    fn pcat_compat_is_false_when_the_bit_is_clear() {
        let header = parse_header(&header_bytes(0xFEE0_0000, 0)).unwrap();
        assert!(!header.pcat_compat());
    }

    #[test]
    fn a_fixed_part_shorter_than_44_bytes_is_rejected() {
        let bytes = header_bytes(0, 0);
        assert_eq!(
            parse_header(&bytes[..FIXED_LENGTH - 1]),
            Err(MadtError::TooShort {
                need: FIXED_LENGTH,
                got: FIXED_LENGTH - 1
            })
        );
    }

    #[test]
    fn entries_are_walked_in_order() {
        let mut body = local_apic_entry(0, 0, FLAG_ENABLED);
        body.extend(local_apic_entry(1, 1, 0));
        body.extend(io_apic_entry(0, 0xFEC0_0000, 0));
        let entries: Vec<_> = Entries::new(&body).map(|e| e.unwrap()).collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].entry_type, TYPE_LOCAL_APIC);
        assert_eq!(entries[2].entry_type, TYPE_IO_APIC);

        let first = parse_local_apic(&entries[0]).unwrap();
        assert_eq!(first.apic_id, 0);
        assert!(first.usable());
        let second = parse_local_apic(&entries[1]).unwrap();
        assert!(!second.usable());

        let io = parse_io_apic(&entries[2]).unwrap();
        assert_eq!(io.address, 0xFEC0_0000);
        assert_eq!(io.global_system_interrupt_base, 0);
    }

    /// **走査ループが止まることの直接の確認。** 長さ 0 のエントリで、
    /// 無限ループにならず 1 回で停止する。
    #[test]
    fn a_zero_length_entry_stops_the_walk_instead_of_looping_forever() {
        let mut body = local_apic_entry(0, 0, FLAG_ENABLED);
        body.extend_from_slice(&[TYPE_LOCAL_APIC, 0, 0, 0]);
        body.extend(local_apic_entry(1, 1, FLAG_ENABLED));

        let collected: Vec<_> = Entries::new(&body).collect();
        assert_eq!(collected.len(), 2);
        assert!(collected[0].is_ok());
        assert_eq!(collected[1], Err(EntryError::ZeroLength));
    }

    /// 停止した後は何も返さない。壊れたエントリの先を読み進めない。
    #[test]
    fn nothing_is_yielded_after_a_stop() {
        let body = vec![TYPE_LOCAL_APIC, 0, 0, 0, 0, 0, 0, 0];
        let mut entries = Entries::new(&body);
        assert_eq!(entries.next(), Some(Err(EntryError::ZeroLength)));
        assert_eq!(entries.next(), None);
        assert_eq!(entries.next(), None);
    }

    #[test]
    fn an_entry_shorter_than_its_type_minimum_is_rejected() {
        let body = vec![TYPE_LOCAL_APIC, 6, 0, 0, 0, 0];
        assert_eq!(
            Entries::new(&body).next(),
            Some(Err(EntryError::LengthBelowMinimum {
                entry_type: TYPE_LOCAL_APIC,
                length: 6,
                minimum: 8,
            }))
        );
    }

    /// **残り長を超える長さは範囲外へ出る。** エントリ長の下限だけを見ていても
    /// これは捕まらない。
    #[test]
    fn an_entry_longer_than_the_remaining_bytes_is_rejected() {
        let mut body = local_apic_entry(0, 0, FLAG_ENABLED);
        body.extend_from_slice(&[TYPE_IO_APIC, 12, 0, 0]);
        assert_eq!(
            Entries::new(&body).nth(1),
            Some(Err(EntryError::LengthExceedsRemaining {
                entry_type: TYPE_IO_APIC,
                length: 12,
                remaining: 4,
            }))
        );
    }

    #[test]
    fn a_single_trailing_byte_cannot_hold_an_entry_header() {
        let mut body = local_apic_entry(0, 0, FLAG_ENABLED);
        body.push(TYPE_IO_APIC);
        assert_eq!(
            Entries::new(&body).nth(1),
            Some(Err(EntryError::TruncatedHeader { remaining: 1 }))
        );
    }

    #[test]
    fn an_empty_body_yields_nothing_and_is_not_an_error() {
        assert_eq!(Entries::new(&[]).next(), None);
    }

    /// 未知の種別は最小長を持たない。**こちらの無知を理由に不正としない。**
    /// 2 バイト以上あれば通し、種別と長さは呼び出し側がログへ出す。
    #[test]
    fn an_unknown_entry_type_is_passed_through_rather_than_rejected() {
        let body = vec![0x7F, 4, 0xAA, 0xBB];
        let entry = Entries::new(&body).next().unwrap().unwrap();
        assert_eq!(entry.entry_type, 0x7F);
        assert_eq!(entry.length, 4);
        assert_eq!(entry_type_name(0x7F), "unknown");
        assert_eq!(minimum_length(0x7F), MIN_ENTRY_LENGTH as u8);
    }

    /// 未知の種別でも、長さ 0 は通さない（前進しないため）。
    #[test]
    fn an_unknown_entry_type_with_zero_length_still_stops_the_walk() {
        let body = vec![0x7F, 0, 0, 0];
        assert_eq!(
            Entries::new(&body).next(),
            Some(Err(EntryError::ZeroLength))
        );
    }

    #[test]
    fn x2apic_entries_carry_a_32_bit_apic_id() {
        let mut body = vec![TYPE_LOCAL_X2APIC, 16, 0, 0];
        body.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        body.extend_from_slice(&FLAG_ENABLED.to_le_bytes());
        body.extend_from_slice(&7u32.to_le_bytes());
        let entry = Entries::new(&body).next().unwrap().unwrap();
        let x2 = parse_local_x2apic(&entry).unwrap();
        assert_eq!(x2.apic_id, 0x1234_5678);
        assert_eq!(x2.processor_uid, 7);
        assert_eq!(x2.flags & FLAG_ENABLED, FLAG_ENABLED);
    }

    #[test]
    fn an_address_override_entry_yields_a_64_bit_address() {
        let mut body = vec![TYPE_LOCAL_APIC_ADDRESS_OVERRIDE, 12, 0, 0];
        body.extend_from_slice(&0x1_FEE0_0000u64.to_le_bytes());
        let entry = Entries::new(&body).next().unwrap().unwrap();
        assert_eq!(
            parse_local_apic_address_override(&entry),
            Some(0x1_FEE0_0000)
        );
    }

    fn iso_entry(source: u8, gsi: u32, flags: u16) -> Vec<u8> {
        let mut bytes = vec![TYPE_INTERRUPT_SOURCE_OVERRIDE, 10, 0, source];
        bytes.extend_from_slice(&gsi.to_le_bytes());
        bytes.extend_from_slice(&flags.to_le_bytes());
        bytes
    }

    /// 典型例（IRQ0 が GSI2 へ移る）を読める。
    #[test]
    fn an_interrupt_source_override_maps_a_legacy_irq_to_a_gsi() {
        let body = iso_entry(0, 2, 0);
        let entry = Entries::new(&body).next().unwrap().unwrap();
        let iso = parse_interrupt_source_override(&entry).unwrap();
        assert_eq!(iso.source, 0);
        assert_eq!(iso.global_system_interrupt, 2);
        assert_eq!(iso.bus, 0);
    }

    /// **`00` は「バスの既定」であって「アクティブロー」でも「レベル」でもない。**
    /// ISA の既定はアクティブハイ・エッジなので、どちらも偽になる。
    /// ここを取り違えると、極性を反転して割り込みが来なくなる。
    #[test]
    fn the_bus_default_flags_mean_active_high_edge_on_isa() {
        let body = iso_entry(0, 2, 0);
        let entry = Entries::new(&body).next().unwrap().unwrap();
        let iso = parse_interrupt_source_override(&entry).unwrap();
        assert!(!iso.active_low());
        assert!(!iso.level_triggered());
    }

    /// 明示的なアクティブロー・レベルトリガを読める（PCI 由来で現れる形）。
    #[test]
    fn explicit_active_low_level_triggered_flags_are_decoded() {
        let body = iso_entry(9, 9, INTI_POLARITY_ACTIVE_LOW | INTI_TRIGGER_LEVEL);
        let entry = Entries::new(&body).next().unwrap().unwrap();
        let iso = parse_interrupt_source_override(&entry).unwrap();
        assert!(iso.active_low());
        assert!(iso.level_triggered());
        assert_eq!(iso.source, 9);
    }

    /// 種別が違えば解釈しない。取り違えを型ではなく値で防いでいる箇所なので、
    /// 固定しておく。
    #[test]
    fn a_decoder_refuses_an_entry_of_another_type() {
        let body = io_apic_entry(0, 0xFEC0_0000, 0);
        let entry = Entries::new(&body).next().unwrap().unwrap();
        assert_eq!(parse_local_apic(&entry), None);
        assert_eq!(parse_local_x2apic(&entry), None);
        assert_eq!(parse_local_apic_address_override(&entry), None);
        assert_eq!(parse_interrupt_source_override(&entry), None);
    }

    /// **残りは単調に減る。** 走査ループが止まることの一般形での確認。
    /// どんなバイト列を与えても、`Entries` は有限回で終わる。
    #[test]
    fn the_walk_terminates_for_arbitrary_bytes() {
        for seed in 0u16..=255 {
            let body: Vec<u8> = (0..64u16)
                .map(|i| (seed.wrapping_mul(31).wrapping_add(i * 7) & 0xFF) as u8)
                .collect();
            // 打ち切りカウンタを置かずに数え切れることが、そのまま停止の証明である。
            let steps = Entries::new(&body).count();
            assert!(steps <= body.len() / MIN_ENTRY_LENGTH + 1);
        }
    }
}
