//! 例外発生時のレジスタ一式（M4-b-2）。
//!
//! **フィールドの順序がアセンブリの push 順と一対一で対応している。**
//! ここがずれると、ダンプは出るのに中身だけが入れ替わる。値がもっともらしい
//! ため気づきにくく、以降の例外解析をすべて誤らせる。
//!
//! そのため二重に防いでいる。
//!
//! 1. このモジュールのテストで、各フィールドのオフセットが push 順から
//!    計算される位置と一致することを固定する
//! 2. `--exception-test invalid-opcode` で、例外の直前に各 GPR へレジスタ
//!    ごとに異なる既知の値を入れ、ダンプに正しい名前で現れることを実機で
//!    突き合わせる（`.bss` のゼロ埋め検証で毒値を使ったのと同じ考え方）
//!
//! 順序を変える場合は、`super` のアセンブリの push 順も必ず同時に変えること。

/// 例外ハンドラが受け取るレジスタ一式。
///
/// スタックは下へ伸びるため、**最後に push したものが先頭（オフセット 0）に
/// 来る**。アセンブリ側は r15 から順に push し、最後に CR2 を push する。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ExceptionContext {
    /// フォルトしたアドレス。**#PF のときだけ意味を持つ。**
    ///
    /// ハンドラ内で別のページフォルトが起きると上書きされてしまうため、
    /// スタブが入場直後に読んで積んでいる。他の例外では、直前の #PF の
    /// 残骸か未定義の値が入っている。
    pub cr2: u64,

    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,

    /// スタブが push したベクタ番号。
    pub vector: u64,
    /// CPU が積んだエラーコード。積まない例外ではスタブが 0 を入れている。
    pub error_code: u64,

    // --- ここから下は CPU が積んだ割り込みスタックフレーム ---
    /// 例外を起こした命令のアドレス（フォルトなら再実行される命令そのもの）。
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    /// 例外発生時点のスタックポインタ。ハンドラ自身の RSP ではない。
    pub rsp: u64,
    pub ss: u64,
}

impl ExceptionContext {
    /// 汎用レジスタを `(名前, 値)` の並びで返す。ダンプ用。
    ///
    /// 確保をしないよう固定長配列で返す。
    pub fn general_purpose_registers(&self) -> [(&'static str, u64); 15] {
        [
            ("rax", self.rax),
            ("rbx", self.rbx),
            ("rcx", self.rcx),
            ("rdx", self.rdx),
            ("rsi", self.rsi),
            ("rdi", self.rdi),
            ("rbp", self.rbp),
            ("r8", self.r8),
            ("r9", self.r9),
            ("r10", self.r10),
            ("r11", self.r11),
            ("r12", self.r12),
            ("r13", self.r13),
            ("r14", self.r14),
            ("r15", self.r15),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    /// アセンブリの push 順から計算される位置と、構造体のオフセットが
    /// 一致すること。
    ///
    /// アセンブリ側は次の順に push する（後に push したものほど低いアドレス）。
    ///
    /// ```text
    /// r15, r14, r13, r12, r11, r10, r9, r8, rbp, rdi, rsi, rdx, rcx, rbx, rax, cr2
    /// ```
    ///
    /// したがってオフセット 0 から cr2, rax, rbx, ... r15 の順に並び、その上に
    /// スタブが積んだ vector と error_code、さらに CPU が積んだ 5 個が続く。
    #[test]
    fn the_field_offsets_match_the_assembly_push_order() {
        // 最後に push したものが先頭。
        assert_eq!(offset_of!(ExceptionContext, cr2), 0);

        // push rax が cr2 の直前 = オフセット 8。以降 push した逆順に並ぶ。
        let expected = [
            ("rax", 8),
            ("rbx", 16),
            ("rcx", 24),
            ("rdx", 32),
            ("rsi", 40),
            ("rdi", 48),
            ("rbp", 56),
            ("r8", 64),
            ("r9", 72),
            ("r10", 80),
            ("r11", 88),
            ("r12", 96),
            ("r13", 104),
            ("r14", 112),
            ("r15", 120),
        ];
        let actual = [
            ("rax", offset_of!(ExceptionContext, rax)),
            ("rbx", offset_of!(ExceptionContext, rbx)),
            ("rcx", offset_of!(ExceptionContext, rcx)),
            ("rdx", offset_of!(ExceptionContext, rdx)),
            ("rsi", offset_of!(ExceptionContext, rsi)),
            ("rdi", offset_of!(ExceptionContext, rdi)),
            ("rbp", offset_of!(ExceptionContext, rbp)),
            ("r8", offset_of!(ExceptionContext, r8)),
            ("r9", offset_of!(ExceptionContext, r9)),
            ("r10", offset_of!(ExceptionContext, r10)),
            ("r11", offset_of!(ExceptionContext, r11)),
            ("r12", offset_of!(ExceptionContext, r12)),
            ("r13", offset_of!(ExceptionContext, r13)),
            ("r14", offset_of!(ExceptionContext, r14)),
            ("r15", offset_of!(ExceptionContext, r15)),
        ];
        assert_eq!(actual, expected);
    }

    /// スタブが積む 2 個と、CPU が積む 5 個の位置。
    #[test]
    fn the_stub_and_cpu_pushed_fields_follow_the_registers() {
        assert_eq!(offset_of!(ExceptionContext, vector), 128);
        assert_eq!(offset_of!(ExceptionContext, error_code), 136);
        assert_eq!(offset_of!(ExceptionContext, rip), 144);
        assert_eq!(offset_of!(ExceptionContext, cs), 152);
        assert_eq!(offset_of!(ExceptionContext, rflags), 160);
        assert_eq!(offset_of!(ExceptionContext, rsp), 168);
        assert_eq!(offset_of!(ExceptionContext, ss), 176);
    }

    #[test]
    fn the_context_has_no_padding() {
        // 内訳: CR2 が 1、汎用レジスタが 15、スタブが積む vector と
        // error_code で 2、CPU が積む rip/cs/rflags/rsp/ss で 5。合計 23 個。
        // 隙間があるとアセンブリ側とずれる。
        assert_eq!(size_of::<ExceptionContext>(), 23 * 8);
        // 最後のフィールドの位置とサイズが整合すること。
        assert_eq!(
            offset_of!(ExceptionContext, ss) + 8,
            size_of::<ExceptionContext>()
        );
    }

    /// ダンプ用の並びが構造体のフィールドと対応していること。名前と値の
    /// 組を取り違えると、ダンプだけが嘘をつく。
    #[test]
    fn the_dump_order_pairs_each_name_with_its_own_field() {
        let context = ExceptionContext {
            cr2: 0,
            rax: 1,
            rbx: 2,
            rcx: 3,
            rdx: 4,
            rsi: 5,
            rdi: 6,
            rbp: 7,
            r8: 8,
            r9: 9,
            r10: 10,
            r11: 11,
            r12: 12,
            r13: 13,
            r14: 14,
            r15: 15,
            vector: 0,
            error_code: 0,
            rip: 0,
            cs: 0,
            rflags: 0,
            rsp: 0,
            ss: 0,
        };
        let expected: [(&str, u64); 15] = [
            ("rax", 1),
            ("rbx", 2),
            ("rcx", 3),
            ("rdx", 4),
            ("rsi", 5),
            ("rdi", 6),
            ("rbp", 7),
            ("r8", 8),
            ("r9", 9),
            ("r10", 10),
            ("r11", 11),
            ("r12", 12),
            ("r13", 13),
            ("r14", 14),
            ("r15", 15),
        ];
        assert_eq!(context.general_purpose_registers(), expected);
    }
}
