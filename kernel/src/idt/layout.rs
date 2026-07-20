//! IDT エントリの符号化（M4-b-1）。
//!
//! ビット配置を組み立てるだけの純粋ロジックであり、ホスト上の
//! `cargo test` で検証する。実際に `lidt` を実行する部分は [`super`] の責務。
//!
//! ロングモードの IDT エントリは 16 バイトで、ハンドラのアドレスが 3 つに
//! 分断されて格納される。ここを間違えると、CPU は「それらしいが全く別の
//! アドレス」へ飛ぶ。どこへ飛んだかは事後には分からないため、符号化と
//! 復号を両方テストして固定する。

use crate::gdt::layout::SegmentSelector;

/// ゲート種別。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GateType {
    /// 割り込みゲート。**入場時に `IF` をクリアする。**
    Interrupt = 0xE,
    /// トラップゲート。入場時に `IF` をクリアしない。
    Trap = 0xF,
}

/// ロングモードの IDT エントリ（16 バイト）。
///
/// フィールドの順序と幅は仕様で決まっている。`packed` にしないと
/// コンパイラが境界へ寄せ、CPU が別の場所を読む。
#[repr(C, packed)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct IdtEntry {
    offset_low: u16,
    selector: u16,
    /// 下位 3 ビットが IST 番号（0 なら IST を使わない）。残りは予約。
    ist: u8,
    /// P(7) | DPL(6:5) | 0(4) | 種別(3:0)。
    type_attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

/// Present ビット。立っていないエントリのベクタが発生すると #NP になる。
const PRESENT: u8 = 1 << 7;

impl IdtEntry {
    /// 存在しない（Present ビットが 0 の）エントリ。
    pub const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attributes: 0,
            offset_middle: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// ハンドラを指すエントリを組み立てる。
    ///
    /// `ist_index` は 1..=7 で IST を使う場合の番号。`None` なら現在の
    /// スタックをそのまま使う。`dpl` はこのゲートを呼べる最低特権レベル
    /// （例外ハンドラは 0）。
    pub const fn new(
        handler: u64,
        selector: SegmentSelector,
        gate: GateType,
        dpl: u8,
        ist_index: Option<u8>,
    ) -> Self {
        let ist = match ist_index {
            Some(index) => index & 0b111,
            None => 0,
        };
        Self {
            offset_low: (handler & 0xFFFF) as u16,
            selector: selector.bits(),
            ist,
            type_attributes: PRESENT | ((dpl & 0b11) << 5) | (gate as u8),
            offset_middle: ((handler >> 16) & 0xFFFF) as u16,
            offset_high: ((handler >> 32) & 0xFFFF_FFFF) as u32,
            reserved: 0,
        }
    }

    /// 格納されているハンドラのアドレスを組み立て直す。
    pub fn handler_address(&self) -> u64 {
        (self.offset_low as u64)
            | ((self.offset_middle as u64) << 16)
            | ((self.offset_high as u64) << 32)
    }

    pub fn is_present(&self) -> bool {
        self.type_attributes & PRESENT != 0
    }

    pub fn gate_type(&self) -> u8 {
        self.type_attributes & 0xF
    }

    pub fn descriptor_privilege_level(&self) -> u8 {
        (self.type_attributes >> 5) & 0b11
    }

    pub fn ist_index(&self) -> Option<u8> {
        match self.ist & 0b111 {
            0 => None,
            index => Some(index),
        }
    }

    pub fn selector(&self) -> u16 {
        self.selector
    }

    /// Present ビットを落とす。
    ///
    /// ダブルフォルトの誘発テスト（M4-b-2）で使う。#PF のゲートを不在に
    /// してからページフォルトを起こすと、例外の配送中にさらに例外が起きる
    /// ためダブルフォルトへ昇格する。
    pub fn clear_present(&mut self) {
        self.type_attributes &= !PRESENT;
    }
}

/// 例外ベクタの番号と名前。
///
/// 0..=31 は CPU が定義する範囲。予約されている番号も、発生したときに
/// 「予約ベクタが来た」と分かるよう名前を持たせる。
pub const EXCEPTION_NAMES: [&str; 32] = [
    "#DE divide error",
    "#DB debug",
    "NMI",
    "#BP breakpoint",
    "#OF overflow",
    "#BR bound range exceeded",
    "#UD invalid opcode",
    "#NM device not available",
    "#DF double fault",
    "coprocessor segment overrun (reserved)",
    "#TS invalid TSS",
    "#NP segment not present",
    "#SS stack-segment fault",
    "#GP general protection",
    "#PF page fault",
    "reserved (15)",
    "#MF x87 floating-point",
    "#AC alignment check",
    "#MC machine check",
    "#XM SIMD floating-point",
    "#VE virtualization",
    "#CP control protection",
    "reserved (22)",
    "reserved (23)",
    "reserved (24)",
    "reserved (25)",
    "reserved (26)",
    "reserved (27)",
    "#HV hypervisor injection",
    "#VC VMM communication",
    "#SX security",
    "reserved (31)",
];

/// そのベクタで CPU がエラーコードをスタックへ積むかどうか。
///
/// 積むものと積まないものでスタックのレイアウトが変わる。スタブを 2 種類
/// 用意し、積まない側はダミーを push して揃える。
pub const fn pushes_error_code(vector: u8) -> bool {
    matches!(vector, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30)
}

/// ベクタ番号に対応する名前。範囲外は汎用の文字列。
pub fn exception_name(vector: u8) -> &'static str {
    if (vector as usize) < EXCEPTION_NAMES.len() {
        EXCEPTION_NAMES[vector as usize]
    } else {
        "unexpected vector"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gdt::KERNEL_CODE_SELECTOR;

    #[test]
    fn an_entry_is_sixteen_bytes() {
        assert_eq!(core::mem::size_of::<IdtEntry>(), 16);
    }

    #[test]
    fn a_missing_entry_is_not_present() {
        let entry = IdtEntry::missing();
        assert!(!entry.is_present());
        assert_eq!(entry.handler_address(), 0);
    }

    /// ハンドラのアドレスが 3 つに分断されて格納され、正しく戻ること。
    /// ここを間違えると CPU は全く別のアドレスへ飛ぶ。
    #[test]
    fn the_handler_address_survives_a_round_trip() {
        for handler in [
            0x0000_0000_0000_0000u64,
            0x0000_0000_0000_FFFF,
            0x0000_0000_FFFF_0000,
            0xFFFF_FFFF_0000_0000,
            0x0000_0000_0010_9550,
            0xFFFF_FFFF_FFFF_FFFF,
        ] {
            let entry = IdtEntry::new(
                handler,
                KERNEL_CODE_SELECTOR,
                GateType::Interrupt,
                0,
                None,
            );
            assert_eq!(
                entry.handler_address(),
                handler,
                "handler {handler:#x} did not survive the round trip"
            );
        }
    }

    #[test]
    fn an_interrupt_gate_is_encoded_as_type_0xe() {
        let entry = IdtEntry::new(0x1000, KERNEL_CODE_SELECTOR, GateType::Interrupt, 0, None);
        assert_eq!(entry.gate_type(), 0xE);
        assert!(entry.is_present());
        assert_eq!(entry.descriptor_privilege_level(), 0);
        assert_eq!(entry.selector(), KERNEL_CODE_SELECTOR.bits());
        assert_eq!(entry.ist_index(), None);
    }

    /// トラップゲートは入場時に `IF` をクリアしない。例外ハンドラを
    /// トラップゲートにすると「ハンドラ入場時点で IF=0」という前提
    /// （ADR-0018）が崩れる。取り違えていないことを固定する。
    #[test]
    fn a_trap_gate_is_a_different_type_from_an_interrupt_gate() {
        let interrupt = IdtEntry::new(0x1000, KERNEL_CODE_SELECTOR, GateType::Interrupt, 0, None);
        let trap = IdtEntry::new(0x1000, KERNEL_CODE_SELECTOR, GateType::Trap, 0, None);
        assert_eq!(interrupt.gate_type(), 0xE);
        assert_eq!(trap.gate_type(), 0xF);
        assert_ne!(interrupt.gate_type(), trap.gate_type());
    }

    #[test]
    fn the_ist_index_is_stored_in_the_low_three_bits() {
        for index in 1..=7u8 {
            let entry = IdtEntry::new(
                0x1000,
                KERNEL_CODE_SELECTOR,
                GateType::Interrupt,
                0,
                Some(index),
            );
            assert_eq!(entry.ist_index(), Some(index));
        }
        let none = IdtEntry::new(0x1000, KERNEL_CODE_SELECTOR, GateType::Interrupt, 0, None);
        assert_eq!(none.ist_index(), None);
    }

    #[test]
    fn a_nonzero_dpl_is_encoded() {
        let entry = IdtEntry::new(0x1000, KERNEL_CODE_SELECTOR, GateType::Interrupt, 3, None);
        assert_eq!(entry.descriptor_privilege_level(), 3);
        assert!(entry.is_present());
    }

    #[test]
    fn clearing_the_present_bit_keeps_everything_else() {
        let mut entry = IdtEntry::new(
            0xDEAD_BEEF_1234,
            KERNEL_CODE_SELECTOR,
            GateType::Interrupt,
            0,
            Some(1),
        );
        entry.clear_present();
        assert!(!entry.is_present());
        // ハンドラのアドレスと種別は残る。復元できることの裏返し。
        assert_eq!(entry.handler_address(), 0xDEAD_BEEF_1234);
        assert_eq!(entry.gate_type(), 0xE);
        assert_eq!(entry.ist_index(), Some(1));
    }

    /// エラーコードを積む例外の一覧。Intel SDM Vol.3 の表に対応する。
    /// ここを取り違えるとスタックが 8 バイトずれ、ハンドラが読む値が
    /// すべて 1 つずつずれる。
    #[test]
    fn the_error_code_table_matches_the_architecture() {
        let expected = [8u8, 10, 11, 12, 13, 14, 17, 21, 29, 30];
        for vector in 0..=255u8 {
            assert_eq!(
                pushes_error_code(vector),
                expected.contains(&vector),
                "vector {vector} の判定が食い違っている"
            );
        }
    }

    #[test]
    fn every_cpu_defined_vector_has_a_name() {
        assert_eq!(EXCEPTION_NAMES.len(), 32);
        assert_eq!(exception_name(0), "#DE divide error");
        assert_eq!(exception_name(6), "#UD invalid opcode");
        assert_eq!(exception_name(8), "#DF double fault");
        assert_eq!(exception_name(14), "#PF page fault");
        // 32 以上は汎用。
        assert_eq!(exception_name(32), "unexpected vector");
        assert_eq!(exception_name(255), "unexpected vector");
    }
}
