//! ANSIエスケープシーケンスの解釈（zi-b。ADR-0029）。
//!
//! **画面制御はANSIで行い、専用のシステムコールを作らない**（ADR-0029の
//! Decision）。ユーザープログラムは `write` でバイト列を送り、カーネルの
//! コンソールの**前景経路**がこの状態機械を通して解釈する。
//!
//! # 純粋ロジックである
//!
//! `keyboard::decode::Decoder` と同じ形——**状態機械であって、装置に触らない。**
//! 置き場所も設計もADR-0029の記述どおり（`common` に置き、ホストで試験する）。
//!
//! # 解釈するのは3つだけ（zi-bの最小範囲）
//!
//! | 列 | 名 | 動作 |
//! |---|---|---|
//! | `\x1b[<row>;<col>H` | CUP | カーソル移動（1起点。省略と0は1） |
//! | `\x1b[<n>J` | ED | 画面消去（0=カーソル以後 / 1=以前 / 2=全体） |
//! | `\x1b[<n>K` | EL | 行消去（同上） |
//!
//! **SGR（色）・スクロールリージョン・カーソル表示/非表示は解釈しない**
//! （zi-bの範囲外。ADR-0029は着手時に確定するとしていた——ここで確定した）。
//! **知らない終端は列ごと読み捨てる。** 端末の慣行と同じで、SGRを送っても
//! 画面に `[31m` が出ることはない——**無視は「化けない」を含む。**
//!
//! # 崩れた列は捨て、いま来た字からやり直す
//!
//! `\x1b` の後に `[` 以外が来たら、また列の途中に構成要素でない字が来たら、
//! **溜めた分は捨てて、いま来た字を最初から扱い直す**（`zash` の入力側の
//! 状態機械と同じ判断——溜めた字を画面へ混ぜない）。

/// 消去の範囲（EDとELで共通。ANSIのパラメータ 0 / 1 / 2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EraseScope {
    /// カーソルから後ろ（カーソルを含む）。パラメータ0（省略時の既定）。
    After,
    /// 先頭からカーソルまで（カーソルを含む）。パラメータ1。
    Before,
    /// 全体。パラメータ2。
    All,
}

impl EraseScope {
    /// パラメータ値から。**0/1/2以外は`None`**——知らない値の列は捨てる
    /// （間違って全消去するより、何もしないほうが安全側である）。
    fn from_param(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::After),
            1 => Some(Self::Before),
            2 => Some(Self::All),
            _ => None,
        }
    }
}

/// 1文字を食わせた結果、呼び出し側がやるべきこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnsiAction {
    /// 普通の字として描く。
    Print(char),
    /// カーソル移動（CUP）。**1起点である**（ANSIの規約のまま返す。
    /// 0起点への変換と画面への切り詰めは呼び出し側が行う）。
    CursorTo { row: u32, col: u32 },
    /// 画面消去（ED）。
    EraseDisplay(EraseScope),
    /// 行消去（EL）。
    EraseLine(EraseScope),
}

/// パラメータの上限。CUPの `row;col` の2つで足りる。
///
/// **3つ目の`;`が来たら列ごと捨てる**——解釈する3列に3パラメータのものは
/// 無いので、それは知らない列である。
const MAX_PARAMS: usize = 2;

/// 状態機械の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 普通の字を待っている。
    Ground,
    /// `\x1b` を見た。次が `[` ならCSIへ。
    Escape,
    /// CSIの中（パラメータと終端を読んでいる）。
    Csi,
}

/// ANSIエスケープの状態機械。
///
/// # 状態は呼び出しをまたいで保つ
///
/// **1つの列が2回の `write` に割れて届くことがある**（システムコールは
/// ページ単位で刻む）。`keyboard::decode::Decoder` と同じ理由で、
/// 呼び出し側が状態を持ち続けること。
#[derive(Debug, Clone, Copy)]
pub struct AnsiParser {
    state: State,
    /// 読み取ったパラメータ。値が来ていない位置は0（＝省略。CUPでは1に読む）。
    params: [u32; MAX_PARAMS],
    /// いま何番目のパラメータへ数字を積んでいるか。
    param_index: usize,
    /// 3つ目以降のパラメータが来た。終端まで飲み込んでから列ごと捨てる。
    too_many_params: bool,
}

impl AnsiParser {
    pub const fn new() -> Self {
        Self {
            state: State::Ground,
            params: [0; MAX_PARAMS],
            param_index: 0,
            too_many_params: false,
        }
    }

    /// 状態を最初へ戻す。**前景を据え直すときに呼ぶ**——前のプログラムが
    /// 列の途中で死んでいても、次のプログラムの1字目が列の続きに化けない。
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// 1文字を与える。動作が確定しなければ `None`（列の途中）。
    ///
    /// 崩れた列では、**いま来た字を最初から扱い直した結果**を返す
    /// （`\x1b` なら次の列の開始、普通の字なら `Print`）。
    pub fn feed(&mut self, c: char) -> Option<AnsiAction> {
        match self.state {
            State::Ground => {
                if c == '\x1b' {
                    self.enter_escape();
                    None
                } else {
                    Some(AnsiAction::Print(c))
                }
            }
            State::Escape => {
                if c == '[' {
                    self.state = State::Csi;
                    None
                } else {
                    // 知らない並びだった。捨てて、いま来た字からやり直す。
                    self.state = State::Ground;
                    self.feed(c)
                }
            }
            State::Csi => self.feed_csi(c),
        }
    }

    fn enter_escape(&mut self) {
        self.state = State::Escape;
        self.params = [0; MAX_PARAMS];
        self.param_index = 0;
        self.too_many_params = false;
    }

    fn feed_csi(&mut self, c: char) -> Option<AnsiAction> {
        match c {
            '0'..='9' => {
                // **飽和で積む。** 画面の桁行に対して大きすぎる値は、
                // どのみち呼び出し側の切り詰めで端に丸まる。
                let digit = c as u32 - '0' as u32;
                if let Some(slot) = self.params.get_mut(self.param_index) {
                    *slot = slot.saturating_mul(10).saturating_add(digit);
                }
                None
            }
            ';' => {
                self.param_index += 1;
                if self.param_index >= MAX_PARAMS {
                    // 3つ目のパラメータ。知らない列なので、終端まで飲み込んで捨てる。
                    self.too_many_params = true;
                    self.param_index = MAX_PARAMS - 1;
                }
                None
            }
            // 終端（CSIの最終バイトは 0x40-0x7E）。解釈するのは3つだけで、
            // 残りは読み捨てる。
            '\u{40}'..='\u{7e}' => {
                let action = if self.too_many_params {
                    None
                } else {
                    self.dispatch_final(c)
                };
                self.state = State::Ground;
                action
            }
            _ => {
                // 構成要素でない字（制御文字や8bit）。列を捨てて、
                // いま来た字からやり直す。
                self.state = State::Ground;
                self.feed(c)
            }
        }
    }

    /// 終端バイトから動作を組み立てる。知らない終端は `None`（読み捨て）。
    fn dispatch_final(&self, final_byte: char) -> Option<AnsiAction> {
        match final_byte {
            'H' => {
                // **省略と0は1である**（ANSIの規約。`\x1b[H` は左上）。
                let row = self.params[0].max(1);
                let col = self.params[1].max(1);
                Some(AnsiAction::CursorTo { row, col })
            }
            'J' => EraseScope::from_param(self.params[0]).map(AnsiAction::EraseDisplay),
            'K' => EraseScope::from_param(self.params[0]).map(AnsiAction::EraseLine),
            _ => None,
        }
    }
}

impl Default for AnsiParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 文字列を全部食わせ、出た動作を集める。
    fn feed_all(parser: &mut AnsiParser, text: &str) -> std::vec::Vec<AnsiAction> {
        text.chars().filter_map(|c| parser.feed(c)).collect()
    }

    #[test]
    fn plain_characters_pass_through_as_print() {
        let mut parser = AnsiParser::new();
        let actions = feed_all(&mut parser, "ab");
        assert_eq!(actions, [AnsiAction::Print('a'), AnsiAction::Print('b')]);
    }

    #[test]
    fn cup_with_both_params_is_one_based() {
        let mut parser = AnsiParser::new();
        let actions = feed_all(&mut parser, "\x1b[3;7H");
        assert_eq!(actions, [AnsiAction::CursorTo { row: 3, col: 7 }]);
    }

    /// **省略と0は1である**（`\x1b[H` は左上。`\x1b[0;0H` も同じ）。
    #[test]
    fn cup_defaults_and_zeros_mean_one() {
        let mut parser = AnsiParser::new();
        assert_eq!(
            feed_all(&mut parser, "\x1b[H"),
            [AnsiAction::CursorTo { row: 1, col: 1 }]
        );
        assert_eq!(
            feed_all(&mut parser, "\x1b[0;0H"),
            [AnsiAction::CursorTo { row: 1, col: 1 }]
        );
        assert_eq!(
            feed_all(&mut parser, "\x1b[5H"),
            [AnsiAction::CursorTo { row: 5, col: 1 }]
        );
    }

    #[test]
    fn erase_scopes_default_to_after() {
        let mut parser = AnsiParser::new();
        assert_eq!(
            feed_all(&mut parser, "\x1b[J"),
            [AnsiAction::EraseDisplay(EraseScope::After)]
        );
        assert_eq!(
            feed_all(&mut parser, "\x1b[1J"),
            [AnsiAction::EraseDisplay(EraseScope::Before)]
        );
        assert_eq!(
            feed_all(&mut parser, "\x1b[2J"),
            [AnsiAction::EraseDisplay(EraseScope::All)]
        );
        assert_eq!(
            feed_all(&mut parser, "\x1b[K"),
            [AnsiAction::EraseLine(EraseScope::After)]
        );
        assert_eq!(
            feed_all(&mut parser, "\x1b[2K"),
            [AnsiAction::EraseLine(EraseScope::All)]
        );
    }

    /// **1つの列が2回の呼び出しに割れても解釈できる**（状態の持ち越し）。
    #[test]
    fn a_sequence_split_across_feeds_still_parses() {
        let mut parser = AnsiParser::new();
        assert_eq!(feed_all(&mut parser, "\x1b["), []);
        assert_eq!(
            feed_all(&mut parser, "5;1H"),
            [AnsiAction::CursorTo { row: 5, col: 1 }]
        );
    }

    /// **知らない終端は列ごと読み捨てる。** SGR（`m`）を送っても
    /// `[31m` が画面へ出ない——無視は「化けない」を含む。
    #[test]
    fn an_unknown_final_byte_consumes_the_whole_sequence() {
        let mut parser = AnsiParser::new();
        assert_eq!(feed_all(&mut parser, "\x1b[31mx"), [AnsiAction::Print('x')]);
    }

    /// 知らない消去パラメータは捨てる（間違って全消去しない）。
    #[test]
    fn an_unknown_erase_parameter_is_dropped() {
        let mut parser = AnsiParser::new();
        assert_eq!(feed_all(&mut parser, "\x1b[3J"), []);
        assert_eq!(feed_all(&mut parser, "x"), [AnsiAction::Print('x')]);
    }

    /// 3つ目のパラメータが来たら、その列は知らない列である。
    #[test]
    fn too_many_params_drop_the_sequence() {
        let mut parser = AnsiParser::new();
        assert_eq!(
            feed_all(&mut parser, "\x1b[1;2;3Hx"),
            [AnsiAction::Print('x')]
        );
    }

    /// `\x1b` の後に `[` 以外: 捨てて、いま来た字からやり直す。
    #[test]
    fn a_broken_escape_reprocesses_the_current_character() {
        let mut parser = AnsiParser::new();
        assert_eq!(feed_all(&mut parser, "\x1bz"), [AnsiAction::Print('z')]);
        // やり直す字が `\x1b` 自身なら、次の列の開始になる。
        assert_eq!(
            feed_all(&mut parser, "\x1b\x1b[2J").as_slice(),
            [AnsiAction::EraseDisplay(EraseScope::All)]
        );
    }

    /// 列の途中の構成要素でない字も同じ（制御文字で列が化けない）。
    /// **やり直された `\n` は `Print` で出る**——改行として扱うのは
    /// コンソールの側である（`Grid::advance` が `\n` を解釈する）。
    #[test]
    fn a_control_character_inside_csi_drops_the_sequence() {
        let mut parser = AnsiParser::new();
        assert_eq!(
            feed_all(&mut parser, "\x1b[3\nx"),
            [AnsiAction::Print('\n'), AnsiAction::Print('x')]
        );
    }

    /// `reset` は列の途中の状態を消す（前景の据え直しで呼ぶ）。
    #[test]
    fn reset_clears_a_half_read_sequence() {
        let mut parser = AnsiParser::new();
        assert_eq!(feed_all(&mut parser, "\x1b[3"), []);
        parser.reset();
        assert_eq!(feed_all(&mut parser, "x"), [AnsiAction::Print('x')]);
    }

    /// 大きすぎる値は飽和する（呼び出し側の切り詰めに任せる）。
    #[test]
    fn huge_parameters_saturate_instead_of_wrapping() {
        let mut parser = AnsiParser::new();
        let actions = feed_all(&mut parser, "\x1b[99999999999;1H");
        assert_eq!(actions.len(), 1);
        let AnsiAction::CursorTo { row, .. } = actions[0] else {
            panic!("expected CursorTo");
        };
        assert!(
            row > 1_000_000,
            "飽和して大きな値のまま届く（丸めは呼び出し側）"
        );
    }
}
