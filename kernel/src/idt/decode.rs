//! 例外エラーコードの復号（M4-b-2）。
//!
//! ビット列を人間が読める意味へ変換するだけの純粋ロジックであり、ホスト上の
//! `cargo test` で検証する。
//!
//! 生の 16 進数だけをログに出しても、その場で SDM を引かないと何も分からない。
//! 「どのアドレスに、読みか書きか、存在しないのか権限違反か」がログだけで
//! 分かる状態にしておく。
//!
//! 文字列はすべて `&'static str` を返す。例外ハンドラは確保もロックもできない
//! ため、組み立てた文字列を返す設計にはできない（ADR-0018）。

/// #PF（ベクタ 14）のエラーコード。
///
/// フォルトしたアドレスはエラーコードではなく CR2 に入る。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageFaultErrorCode(pub u64);

impl PageFaultErrorCode {
    /// bit 0: 0 ならページが存在しない、1 なら権限違反。
    pub fn is_protection_violation(&self) -> bool {
        self.0 & (1 << 0) != 0
    }

    /// bit 1: 0 なら読み、1 なら書き。
    pub fn is_write(&self) -> bool {
        self.0 & (1 << 1) != 0
    }

    /// bit 2: 0 ならスーパーバイザ（CPL 0-2）、1 ならユーザー（CPL 3）。
    pub fn is_user_mode(&self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// bit 3: ページテーブルの予約ビットに 1 が立っていた。
    /// ページテーブルの構築を間違えたときに立つ。
    pub fn is_reserved_bit_violation(&self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// bit 4: 命令フェッチが原因。NX ビットを立てた領域を実行した場合など。
    pub fn is_instruction_fetch(&self) -> bool {
        self.0 & (1 << 4) != 0
    }

    /// bit 5: プロテクションキー違反。
    pub fn is_protection_key_violation(&self) -> bool {
        self.0 & (1 << 5) != 0
    }

    /// bit 6: シャドースタックへのアクセス。
    pub fn is_shadow_stack(&self) -> bool {
        self.0 & (1 << 6) != 0
    }

    /// 原因（存在しない / 権限違反）。
    pub fn cause(&self) -> &'static str {
        if self.is_protection_violation() {
            "protection violation"
        } else {
            "page not present"
        }
    }

    /// アクセスの種類。命令フェッチは書き込みビットより優先して表示する。
    pub fn access(&self) -> &'static str {
        if self.is_instruction_fetch() {
            "instruction fetch"
        } else if self.is_write() {
            "write"
        } else {
            "read"
        }
    }

    /// アクセス元の特権レベル。
    pub fn mode(&self) -> &'static str {
        if self.is_user_mode() {
            "user"
        } else {
            "supervisor"
        }
    }
}

/// エラーコードがどのディスクリプタテーブルを指しているか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DescriptorTable {
    Gdt,
    Idt,
    Ldt,
}

impl DescriptorTable {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Gdt => "GDT",
            Self::Idt => "IDT",
            Self::Ldt => "LDT",
        }
    }
}

/// セグメント関連の例外（#TS, #NP, #SS, #GP）のエラーコード。
///
/// どのディスクリプタが問題だったかを指す。0 の場合は「特定のディスクリプタに
/// 起因しない」ことを意味する。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SelectorErrorCode(pub u64);

impl SelectorErrorCode {
    /// bit 0: 外部イベント（ハードウェア割り込み等）が原因。
    pub fn is_external(&self) -> bool {
        self.0 & (1 << 0) != 0
    }

    /// bit 1 が立っていれば IDT。立っていなければ bit 2（TI）で GDT / LDT。
    pub fn table(&self) -> DescriptorTable {
        if self.0 & (1 << 1) != 0 {
            DescriptorTable::Idt
        } else if self.0 & (1 << 2) != 0 {
            DescriptorTable::Ldt
        } else {
            DescriptorTable::Gdt
        }
    }

    /// ディスクリプタのインデックス（bit 3 以降）。
    pub fn index(&self) -> u16 {
        ((self.0 >> 3) & 0x1FFF) as u16
    }

    /// エラーコード全体が 0 か。特定のディスクリプタに起因しないことを意味する。
    pub fn is_null(&self) -> bool {
        self.0 == 0
    }
}

/// そのベクタのエラーコードをどう解釈すべきか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorCodeKind {
    /// エラーコードを積まない例外。
    None,
    /// #PF。ページフォルト固有のビット列。
    PageFault,
    /// #TS / #NP / #SS / #GP。セレクタを指す。
    Selector,
    /// #DF。**常に 0** であり、意味を持たない。
    /// Intel SDM に「エラーコードは常に 0」と明記されている。
    AlwaysZero,
    /// 上記以外でエラーコードを積むもの（#AC, #CP, #HV, #VC）。
    /// 意味はベクタごとに異なるため、生の値だけを出す。
    Raw,
}

/// ベクタ番号からエラーコードの種類を決める。
///
/// 各腕は連番ではなく、**個別の例外の集まり**として書いてある。
/// 10-13 は #TS / #NP / #SS / #GP で、たまたま番号が連続しているだけである。
/// 範囲記法にすると「10 から 13 までの何か」に見え、どの例外を指しているのか
/// が読み取れなくなる。17 / 21 / 29 / 30 が連続していないことからも、
/// この 4 つが集合であって範囲ではないことが分かる。
#[allow(clippy::manual_range_patterns)]
pub const fn error_code_kind(vector: u8) -> ErrorCodeKind {
    match vector {
        8 => ErrorCodeKind::AlwaysZero,
        10 | 11 | 12 | 13 => ErrorCodeKind::Selector,
        14 => ErrorCodeKind::PageFault,
        17 | 21 | 29 | 30 => ErrorCodeKind::Raw,
        _ => ErrorCodeKind::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_fault_on_a_missing_page_read_from_the_kernel() {
        // すべて 0 = 存在しないページを、スーパーバイザが、読もうとした。
        let code = PageFaultErrorCode(0);
        assert!(!code.is_protection_violation());
        assert!(!code.is_write());
        assert!(!code.is_user_mode());
        assert_eq!(code.cause(), "page not present");
        assert_eq!(code.access(), "read");
        assert_eq!(code.mode(), "supervisor");
    }

    #[test]
    fn a_page_fault_on_a_write_to_a_present_page_from_user_mode() {
        // bit0=1 権限違反, bit1=1 書き込み, bit2=1 ユーザー
        let code = PageFaultErrorCode(0b111);
        assert!(code.is_protection_violation());
        assert!(code.is_write());
        assert!(code.is_user_mode());
        assert_eq!(code.cause(), "protection violation");
        assert_eq!(code.access(), "write");
        assert_eq!(code.mode(), "user");
    }

    /// 命令フェッチは書き込みビットより優先して表示する。両方立つことは
    /// 通常ないが、立った場合に「write」と出ると誤解を招く。
    #[test]
    fn an_instruction_fetch_takes_priority_over_the_write_bit() {
        let code = PageFaultErrorCode(0b1_0010);
        assert!(code.is_instruction_fetch());
        assert!(code.is_write());
        assert_eq!(code.access(), "instruction fetch");
    }

    #[test]
    fn the_reserved_bit_violation_is_detected() {
        // ページテーブルの構築ミスで立つ。見逃すと原因が分からなくなる。
        let code = PageFaultErrorCode(0b1000);
        assert!(code.is_reserved_bit_violation());
        assert!(!code.is_instruction_fetch());
    }

    #[test]
    fn the_upper_page_fault_bits_are_decoded() {
        assert!(PageFaultErrorCode(1 << 5).is_protection_key_violation());
        assert!(PageFaultErrorCode(1 << 6).is_shadow_stack());
    }

    #[test]
    fn a_selector_error_code_points_at_a_gdt_entry() {
        // インデックス 2（カーネルデータセグメント）を指す例。
        let code = SelectorErrorCode(2 << 3);
        assert_eq!(code.table(), DescriptorTable::Gdt);
        assert_eq!(code.index(), 2);
        assert!(!code.is_external());
        assert!(!code.is_null());
    }

    #[test]
    fn a_selector_error_code_can_point_at_the_idt() {
        // bit1 が立っていれば IDT。TI ビットは無視される。
        let code = SelectorErrorCode((14 << 3) | 0b010);
        assert_eq!(code.table(), DescriptorTable::Idt);
        assert_eq!(code.index(), 14);

        let both = SelectorErrorCode((14 << 3) | 0b110);
        assert_eq!(both.table(), DescriptorTable::Idt, "IDT ビットが優先される");
    }

    #[test]
    fn a_selector_error_code_can_point_at_the_ldt() {
        let code = SelectorErrorCode((3 << 3) | 0b100);
        assert_eq!(code.table(), DescriptorTable::Ldt);
        assert_eq!(code.index(), 3);
    }

    #[test]
    fn an_external_event_is_flagged() {
        let code = SelectorErrorCode(0b001);
        assert!(code.is_external());
    }

    #[test]
    fn a_zero_selector_error_code_is_null() {
        assert!(SelectorErrorCode(0).is_null());
        assert!(!SelectorErrorCode(8).is_null());
    }

    /// 種類の割り当てが、エラーコードを積むベクタの一覧と整合すること。
    /// 積むのに `None` を返すと、エラーコードが表示されなくなる。
    #[test]
    fn the_kind_matches_the_error_code_table() {
        use crate::idt::layout::pushes_error_code;
        for vector in 0..=255u8 {
            let kind = error_code_kind(vector);
            let pushes = pushes_error_code(vector);
            assert_eq!(
                kind != ErrorCodeKind::None,
                pushes,
                "vector {vector}: kind={kind:?} と pushes_error_code={pushes} が食い違う"
            );
        }
    }

    #[test]
    fn the_double_fault_error_code_is_marked_as_always_zero() {
        assert_eq!(error_code_kind(8), ErrorCodeKind::AlwaysZero);
        assert_eq!(error_code_kind(14), ErrorCodeKind::PageFault);
        assert_eq!(error_code_kind(13), ErrorCodeKind::Selector);
        assert_eq!(error_code_kind(17), ErrorCodeKind::Raw);
        assert_eq!(error_code_kind(0), ErrorCodeKind::None);
    }
}
