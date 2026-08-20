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

use crate::screen::Rgb;

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
    /// 文字の見た目を変える（SGR。ES-b。ADR-0040）。
    ///
    /// **色は受理時にRGBへ展開してある**（`Rgb`）——16色・256色・truecolorの
    /// どれで来ても、ここから先は同じ形である。**量子化しない。**
    SetGraphics(Graphics),
}

/// SGRが指示した見た目の変化（ES-b）。
///
/// **`None`は「触らない」である。** SGRは複数の指示を1つの列に並べられるので、
/// **指定されなかったものを既定へ戻してはならない。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Graphics {
    /// すべて既定へ戻す（`0`）。**これが真なら下の2つより先に効く。**
    pub reset: bool,
    /// 前景。
    pub foreground: Option<Rgb>,
    /// 背景。
    pub background: Option<Rgb>,
}

/// パラメータの上限（ES-bで2から16へ上げた）。
///
/// **SGRは可変長である。** 前景と背景をどちらもtruecolorで指定すると
/// `38;2;r;g;b;48;2;r;g;b`で**10個**になり、属性を足せばもう少し伸びる。
/// **16はその実用上の上限に余裕を取った値である**（一般の端末は16から32を
/// 受ける）。
///
/// **越えたら列ごと捨てる。** **途中まで適用しない**——`38;2;255;0`のような
/// 半端な指定を部分適用すると、**色が中途半端に変わって「効いたのか」が
/// 読めなくなる。** 捨てれば「効かなかった」だけである。
const MAX_PARAMS: usize = 16;

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
    /// 上限を越えるパラメータが来た。終端まで飲み込んでから列ごと捨てる。
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

    /// SGRを組み立てる（ES-b。ADR-0040）。
    ///
    /// # 受ける形
    ///
    /// | パラメータ | 意味 |
    /// |---|---|
    /// | `0`（省略も同じ） | すべて既定へ戻す |
    /// | `30`..`37` / `90`..`97` | 前景（標準8色 / 明るい8色） |
    /// | `40`..`47` / `100`..`107` | 背景（同上） |
    /// | `38;5;n` / `48;5;n` | 256色（受理時にRGBへ展開する） |
    /// | `38;2;r;g;b` / `48;2;r;g;b` | truecolor |
    /// | `39` / `49` | 前景 / 背景を既定へ |
    ///
    /// **量子化しない**（ADR-0040 の Decision 2）。**どの形で来ても RGB へ
    /// 展開して同じ形にする。**
    ///
    /// # 知らないパラメータは飛ばす
    ///
    /// **列ごと捨てない。** SGRは複数の指示を並べられるので、
    /// **`1;31`（太字 + 赤）の太字を知らないからといって赤まで捨てるのは
    /// 行き過ぎである。** **知っているものだけを効かせる。**
    ///
    /// **太字・下線・反転（`1` / `4` / `7`）は受けるが、いまは効かない**
    /// ——**描く側に太字の字形が無い。** **`Cell::attrs` へ溜めない**
    /// （溜めても誰も見ないので、**観測されない状態を増やすことになる**）。
    /// **要る利用者が来たら足す。**
    fn dispatch_sgr(&self) -> Option<AnsiAction> {
        let mut graphics = Graphics::default();
        // **パラメータが1つも無い `\x1b[m` は `0` である**（配列は0で埋めて
        // あるので、そのまま `0` として読める）。
        let count = self.param_index + 1;
        let mut at = 0usize;
        while at < count {
            let param = self.params[at];
            match param {
                0 => graphics.reset = true,
                30..=37 => graphics.foreground = Some(base_color(param - 30, false)),
                90..=97 => graphics.foreground = Some(base_color(param - 90, true)),
                40..=47 => graphics.background = Some(base_color(param - 40, false)),
                100..=107 => graphics.background = Some(base_color(param - 100, true)),
                39 => graphics.foreground = None,
                49 => graphics.background = None,
                38 | 48 => {
                    // **`5;n`（256色）か `2;r;g;b`（truecolor）が続く。**
                    // **足りなければ列ごと捨てる**——半端な指定を部分適用しない
                    // （[`MAX_PARAMS`] の doc と同じ判断）。
                    let kind = self.params.get(at + 1)?;
                    let color = match kind {
                        5 => {
                            let index = self.params.get(at + 2)?;
                            if at + 2 >= count {
                                return None;
                            }
                            at += 2;
                            palette_256(*index)
                        }
                        2 => {
                            if at + 4 >= count {
                                return None;
                            }
                            let (r, g, b) = (
                                self.params[at + 2],
                                self.params[at + 3],
                                self.params[at + 4],
                            );
                            at += 4;
                            Rgb::new(clamp_u8(r), clamp_u8(g), clamp_u8(b))
                        }
                        // 知らない指定方式。**列ごと捨てる**——続く値の個数が
                        // 分からないので、飛ばす先を決められない。
                        _ => return None,
                    };
                    if param == 38 {
                        graphics.foreground = Some(color);
                    } else {
                        graphics.background = Some(color);
                    }
                }
                // 知らないパラメータは飛ばす（上の doc）。
                _ => {}
            }
            at += 1;
        }
        Some(AnsiAction::SetGraphics(graphics))
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
            // SGR（ES-b）。**パラメータが無い `\x1b[m` は `0`（全部戻す）である**
            // ——ANSI の規約どおり。
            'm' => self.dispatch_sgr(),
            _ => None,
        }
    }
}

/// 標準8色と明るい8色（ES-b）。
///
/// **xtermの慣行の値を使う。** **ZaytOSはコンソールもウィンドウも同じ
/// フレームバッファへ描くので、量子化も別プロファイルも持たない**
/// （ADR-0040）。
const fn base_color(index: u32, bright: bool) -> Rgb {
    let table = if bright {
        [
            (0x7f, 0x7f, 0x7f),
            (0xff, 0x00, 0x00),
            (0x00, 0xff, 0x00),
            (0xff, 0xff, 0x00),
            (0x5c, 0x5c, 0xff),
            (0xff, 0x00, 0xff),
            (0x00, 0xff, 0xff),
            (0xff, 0xff, 0xff),
        ]
    } else {
        [
            (0x00, 0x00, 0x00),
            (0xcd, 0x00, 0x00),
            (0x00, 0xcd, 0x00),
            (0xcd, 0xcd, 0x00),
            (0x00, 0x00, 0xee),
            (0xcd, 0x00, 0xcd),
            (0x00, 0xcd, 0xcd),
            (0xe5, 0xe5, 0xe5),
        ]
    };
    // **範囲外は来ない**（呼び出し側が 0..=7 に絞っている）が、
    // **添字で落とさない**——`get` が無い const 文脈なので剰余で閉じる。
    let (red, green, blue) = table[(index % 8) as usize];
    Rgb::new(red, green, blue)
}

/// 256色をRGBへ展開する（ES-b）。**表ではなく式で出す。**
///
/// - `0`..`15`: 標準8色と明るい8色
/// - `16`..`231`: 6×6×6の立方体
/// - `232`..`255`: 24段の灰
const fn palette_256(index: u32) -> Rgb {
    if index < 8 {
        return base_color(index, false);
    }
    if index < 16 {
        return base_color(index - 8, true);
    }
    if index < 232 {
        let value = index - 16;
        // **各軸は6段で、xtermの刻みは 0, 95, 135, 175, 215, 255 である。**
        let steps = [0u8, 95, 135, 175, 215, 255];
        let red = steps[((value / 36) % 6) as usize];
        let green = steps[((value / 6) % 6) as usize];
        let blue = steps[(value % 6) as usize];
        return Rgb::new(red, green, blue);
    }
    if index < 256 {
        // **灰は 8 から 10 刻みである**（xtermの慣行）。
        let level = (8 + (index - 232) * 10) as u8;
        return Rgb::new(level, level, level);
    }
    // 範囲外。**黒にしない**——白のほうが「知らない値が来た」と気づきやすい。
    Rgb::new(0xff, 0xff, 0xff)
}

/// SGRのパラメータをバイトへ落とす。**255で飽和する。**
const fn clamp_u8(value: u32) -> u8 {
    if value > 255 {
        255
    } else {
        value as u8
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

    /// **知らない終端は列ごと読み捨てる。** 無視は「化けない」を含む。
    ///
    /// **例をSGRから変えた（ES-b）。** かつては `\x1b[31m` を「知らない終端」の
    /// 例に使っていたが、**ES-bでSGRを解釈するようになったので例として
    /// 成り立たない。** DSR（`\x1b[6n`。カーソル位置の問い合わせ）へ移した
    /// ——**あれは応答を返す列で、入力の経路を持たないいまは扱えない。**
    #[test]
    fn an_unknown_final_byte_consumes_the_whole_sequence() {
        let mut parser = AnsiParser::new();
        assert_eq!(feed_all(&mut parser, "\x1b[6nx"), [AnsiAction::Print('x')]);
    }

    /// 知らない消去パラメータは捨てる（間違って全消去しない）。
    #[test]
    fn an_unknown_erase_parameter_is_dropped() {
        let mut parser = AnsiParser::new();
        assert_eq!(feed_all(&mut parser, "\x1b[3J"), []);
        assert_eq!(feed_all(&mut parser, "x"), [AnsiAction::Print('x')]);
    }

    /// **上限を越えるパラメータが来たら、その列ごと捨てる。**
    ///
    /// **例を変えた（ES-b）。** かつては3つで越えたが、**SGRのために上限を
    /// 16へ上げた**ので、越えるには17個要る。**CUPは余分なパラメータを
    /// 無視する形になった**（3つ目以降を読まない）——**それは端末の慣行
    /// どおりで、`\x1b[1;2;3H` は (1,2) へ動く。**
    #[test]
    fn too_many_params_drop_the_sequence() {
        let mut parser = AnsiParser::new();
        // 17 個並べる。**上限（16）を 1 つ越える。**
        let over = "\x1b[1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1;1Hx";
        assert_eq!(feed_all(&mut parser, over), [AnsiAction::Print('x')]);
    }

    /// **CUPは余分なパラメータを無視する（ES-bで上限を上げた帰結）。**
    ///
    /// **端末の慣行どおりである**——`\x1b[1;2;3H` は 3 つ目を見ずに (1,2) へ
    /// 動く。**上限が 2 だった頃は列ごと捨てていた**ので、挙動が変わった。
    #[test]
    fn cup_ignores_extra_parameters() {
        let mut parser = AnsiParser::new();
        assert_eq!(
            feed_all(&mut parser, "\x1b[1;2;3H"),
            [AnsiAction::CursorTo { row: 1, col: 2 }]
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

    /// **標準8色と明るい8色が前景・背景の両方で効く（ES-b）。**
    #[test]
    fn the_base_colors_land_on_foreground_and_background() {
        let mut parser = AnsiParser::new();
        let red = feed_all(&mut parser, "\x1b[31m");
        assert_eq!(
            red,
            [AnsiAction::SetGraphics(Graphics {
                reset: false,
                foreground: Some(Rgb::new(0xcd, 0x00, 0x00)),
                background: None,
            })]
        );
        let bright_bg = feed_all(&mut parser, "\x1b[102m");
        assert_eq!(
            bright_bg,
            [AnsiAction::SetGraphics(Graphics {
                reset: false,
                foreground: None,
                background: Some(Rgb::new(0x00, 0xff, 0x00)),
            })]
        );
    }

    /// **truecolorはそのまま通る。量子化しない**（ADR-0040）。
    #[test]
    fn truecolor_passes_through_unquantised() {
        let mut parser = AnsiParser::new();
        let actions = feed_all(&mut parser, "\x1b[38;2;12;34;56m");
        assert_eq!(
            actions,
            [AnsiAction::SetGraphics(Graphics {
                reset: false,
                foreground: Some(Rgb::new(12, 34, 56)),
                background: None,
            })]
        );
    }

    /// **256色は式で展開する**（立方体と灰）。
    #[test]
    fn the_256_palette_expands_to_rgb() {
        let mut parser = AnsiParser::new();
        // 16 は立方体の原点で、xterm では黒である。
        assert_eq!(
            feed_all(&mut parser, "\x1b[38;5;16m"),
            [AnsiAction::SetGraphics(Graphics {
                reset: false,
                foreground: Some(Rgb::new(0, 0, 0)),
                background: None,
            })]
        );
        // 231 は立方体の反対の端で白。
        assert_eq!(
            feed_all(&mut parser, "\x1b[48;5;231m"),
            [AnsiAction::SetGraphics(Graphics {
                reset: false,
                foreground: None,
                background: Some(Rgb::new(255, 255, 255)),
            })]
        );
        // 232 は灰の最初（8, 8, 8）。
        assert_eq!(
            feed_all(&mut parser, "\x1b[38;5;232m"),
            [AnsiAction::SetGraphics(Graphics {
                reset: false,
                foreground: Some(Rgb::new(8, 8, 8)),
                background: None,
            })]
        );
    }

    /// **1つの列に前景と背景を並べられる。**
    #[test]
    fn one_sequence_can_set_both_colors() {
        let mut parser = AnsiParser::new();
        let actions = feed_all(&mut parser, "\x1b[38;2;1;2;3;48;2;4;5;6m");
        assert_eq!(
            actions,
            [AnsiAction::SetGraphics(Graphics {
                reset: false,
                foreground: Some(Rgb::new(1, 2, 3)),
                background: Some(Rgb::new(4, 5, 6)),
            })]
        );
    }

    /// **`0` と省略はどちらも「全部戻す」である。**
    #[test]
    fn zero_and_omission_both_reset() {
        let mut parser = AnsiParser::new();
        let expected = [AnsiAction::SetGraphics(Graphics {
            reset: true,
            foreground: None,
            background: None,
        })];
        assert_eq!(feed_all(&mut parser, "\x1b[0m"), expected);
        assert_eq!(feed_all(&mut parser, "\x1b[m"), expected);
    }

    /// **知らないパラメータは飛ばし、知っているものは効く。**
    ///
    /// **列ごと捨てない**——太字（`1`）を知らないからといって赤まで
    /// 捨てるのは行き過ぎである。
    #[test]
    fn unknown_parameters_are_skipped_not_fatal() {
        let mut parser = AnsiParser::new();
        let actions = feed_all(&mut parser, "\x1b[1;31m");
        assert_eq!(
            actions,
            [AnsiAction::SetGraphics(Graphics {
                reset: false,
                foreground: Some(Rgb::new(0xcd, 0x00, 0x00)),
                background: None,
            })]
        );
    }

    /// **半端な指定は列ごと捨てる**（部分適用しない）。
    #[test]
    fn a_truncated_color_spec_drops_the_sequence() {
        let mut parser = AnsiParser::new();
        assert_eq!(feed_all(&mut parser, "\x1b[38;2;255;0m"), []);
        // 次の字は普通に通る（状態が残らない）。
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
