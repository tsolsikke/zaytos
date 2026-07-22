//! スキャンコード（セット 1）から文字への変換。
//!
//! **純粋ロジック。** ポート I/O を一切含まず、ホスト `cargo test` で検証する。
//! 実機で文字が化けたとき、ここが正しいと分かっていれば原因をハードウェア側
//! （i8042 の設定、割り込み、リングバッファ）に絞り込める。
//!
//! ## 対応範囲
//!
//! **US 配列の英数字と基本的な記号のみ。** 具体的には、数字段・英字 3 段・
//! スペース・Enter・Tab・Backspace と、それらの Shift 記号である。
//!
//! 対応しないもの（受けても状態機械は壊れず、[`KeyEvent::Unsupported`] として
//! 報告する）:
//!
//! - ファンクションキー、テンキー、カーソルキー
//! - Ctrl / Alt との組み合わせ（修飾としては解釈しない）
//! - US 以外の配列
//!
//! ## セット 1 を前提にしてよい理由
//!
//! キーボード自身が出すのはセット 2 だが、i8042 のコンフィグバイト
//! bit6（translation）が立っていればコントローラがセット 1 へ翻訳する。
//! ZaytOS は起動時にこのビットを**実際に読んで確認**し、落ちていれば立てて
//! 読み直す。したがってここはセット 1 だけを扱えばよい。両方に対応すると
//! 分岐が倍になり、しかも片方は実機で試せない。
//!
//! ## Caps Lock の LED は更新しない
//!
//! LED の点灯にはキーボードへのコマンド送信（0xED + ビットマスク）と ACK の
//! 待ち合わせが必要で、i8042 への出力方向の手順が新たに要る。M4-e の主題は
//! 割り込みであり、複雑度に見合わないため見送る
//! （`docs/deferred-decisions.md`）。**内部状態としての Caps Lock は正しく
//! 管理する**ので、変換結果は LED の有無に関わらず正しい。

/// メインループが受け取るキーイベント。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    /// 表示すべき文字。
    Char(char),
    /// Enter。改行として扱う。
    Enter,
    /// Backspace。
    ///
    /// **M4-e では画面上の消去を行わない。** 消去にはコンソール側でセルごとの
    /// 占有種別（全角の先頭 / 後続）を管理する必要があり、割り込みとは別の
    /// 仕事になる（`docs/deferred-decisions.md`）。キーとしては認識する。
    Backspace,
    /// 対応していないスキャンコード。**無言で捨てず、呼び出し側が数える。**
    ///
    /// 押下（make）のときだけ報告する。離した（break）ときは報告しない。
    Unsupported(u8),
}

/// ブレークコード（キーを離した）を示すビット。
///
/// セット 1 では、押下コードに 0x80 を立てたものが離したときのコードになる。
const BREAK_BIT: u8 = 0x80;

/// 拡張スキャンコードのプレフィックス。次の 1 バイトと組で 1 つのキーを表す。
const EXTENDED_PREFIX: u8 = 0xE0;

/// Pause / Break のプレフィックス。
///
/// **`0xE0` と同じ扱いにしてはならない。** Pause は
/// `E1 1D 45 E1 9D C5` の 6 バイト列で、**内部に 2 個目の `E1` を含む**。
/// 「次の 1 バイトを飛ばす」実装だと内側の `E1` で状態機械が入れ子になり、
/// 以降のバイト境界がずれて**キー入力全体が壊れる**。しかも Pause を
/// 押さなければ再現しないため、気づくのが遅れる。
const PAUSE_PREFIX: u8 = 0xE1;

/// `PAUSE_PREFIX` の後に続き、無条件に読み捨てるバイト数。
///
/// 内側の `E1` も「数のうち」として飲み込むので、入れ子にならない。
const PAUSE_TRAILING_BYTES: u8 = 5;

// 修飾キーのスキャンコード（押下側）。
const SCANCODE_LEFT_SHIFT: u8 = 0x2A;
const SCANCODE_RIGHT_SHIFT: u8 = 0x36;
const SCANCODE_CAPS_LOCK: u8 = 0x3A;

// 文字ではないが意味を持つキー。
const SCANCODE_BACKSPACE: u8 = 0x0E;
const SCANCODE_TAB: u8 = 0x0F;
const SCANCODE_ENTER: u8 = 0x1C;

/// 変換表の大きさ。これ以上のコードは未対応として扱う。
const TABLE_LEN: usize = 0x40;

/// Shift を押していないときの文字。`'\0'` は「文字ではない」。
const UNSHIFTED: [char; TABLE_LEN] = [
    '\0', '\0', '1', '2', '3', '4', '5', '6', // 0x00-0x07
    '7', '8', '9', '0', '-', '=', '\0', '\0', // 0x08-0x0F (0x0E=BS, 0x0F=Tab)
    'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', // 0x10-0x17
    'o', 'p', '[', ']', '\0', '\0', 'a', 's', // 0x18-0x1F (0x1C=Enter, 0x1D=LCtrl)
    'd', 'f', 'g', 'h', 'j', 'k', 'l', ';', // 0x20-0x27
    '\'', '`', '\0', '\\', 'z', 'x', 'c', 'v', // 0x28-0x2F (0x2A=LShift)
    'b', 'n', 'm', ',', '.', '/', '\0', '\0', // 0x30-0x37 (0x36=RShift, 0x37=keypad *)
    '\0', ' ', '\0', '\0', '\0', '\0', '\0', '\0', // 0x38-0x3F (0x39=Space, 0x3A=Caps)
];

/// Shift を押しているときの文字。US 配列。
const SHIFTED: [char; TABLE_LEN] = [
    '\0', '\0', '!', '@', '#', '$', '%', '^', // 0x00-0x07
    '&', '*', '(', ')', '_', '+', '\0', '\0', // 0x08-0x0F
    'Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', // 0x10-0x17
    'O', 'P', '{', '}', '\0', '\0', 'A', 'S', // 0x18-0x1F
    'D', 'F', 'G', 'H', 'J', 'K', 'L', ':', // 0x20-0x27
    '"', '~', '\0', '|', 'Z', 'X', 'C', 'V', // 0x28-0x2F
    'B', 'N', 'M', '<', '>', '?', '\0', '\0', // 0x30-0x37
    '\0', ' ', '\0', '\0', '\0', '\0', '\0', '\0', // 0x38-0x3F
];

/// 複数バイトのスキャンコード列を読んでいる途中の状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sequence {
    /// 通常。次のバイトは単独のスキャンコードかプレフィックス。
    Idle,
    /// `0xE0` を受けた。次の 1 バイトが拡張キーの本体。
    Extended,
    /// `0xE1` を受けた。残りこのバイト数を無条件に読み捨てる。
    Pause { remaining: u8 },
}

/// スキャンコードを 1 バイトずつ食べて、キーイベントを吐く状態機械。
///
/// 修飾キーの状態を保持するため、**入力の途中で作り直してはいけない**
/// （Shift を押している最中に作り直すと、離したときの break を取りこぼして
/// 押しっぱなし扱いのままになる）。
#[derive(Debug, Clone, Copy)]
pub struct Decoder {
    shift_left: bool,
    shift_right: bool,
    caps_lock: bool,
    sequence: Sequence,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            shift_left: false,
            shift_right: false,
            caps_lock: false,
            sequence: Sequence::Idle,
        }
    }

    /// Shift が押されているか（左右どちらでも）。
    pub const fn shift(&self) -> bool {
        self.shift_left || self.shift_right
    }

    pub const fn caps_lock(&self) -> bool {
        self.caps_lock
    }

    /// スキャンコードを 1 バイト与える。
    ///
    /// 報告すべきことが無ければ `None`。修飾キーの更新、キーを離したこと、
    /// 複数バイト列の途中はすべて `None` になる。
    pub fn feed(&mut self, code: u8) -> Option<KeyEvent> {
        match self.sequence {
            Sequence::Pause { remaining } => {
                // 内側の 0xE1 も含めて、数えた分だけ無条件に飲み込む。
                let left = remaining.saturating_sub(1);
                if left == 0 {
                    self.sequence = Sequence::Idle;
                    // 列を最後まで消費し終えた時点で 1 回だけ報告する。
                    // 無言で捨てないため。
                    Some(KeyEvent::Unsupported(PAUSE_PREFIX))
                } else {
                    self.sequence = Sequence::Pause { remaining: left };
                    None
                }
            }
            Sequence::Extended => {
                self.sequence = Sequence::Idle;
                // 拡張キー（カーソル、右 Ctrl/Alt など）は未対応。押下のときだけ
                // 報告し、離したときは黙る。押下と離しで 2 回数えると、
                // 「押した回数」として見たときに倍になる。
                if code & BREAK_BIT == 0 {
                    Some(KeyEvent::Unsupported(code))
                } else {
                    None
                }
            }
            Sequence::Idle => self.feed_idle(code),
        }
    }

    fn feed_idle(&mut self, code: u8) -> Option<KeyEvent> {
        if code == EXTENDED_PREFIX {
            self.sequence = Sequence::Extended;
            return None;
        }
        if code == PAUSE_PREFIX {
            self.sequence = Sequence::Pause {
                remaining: PAUSE_TRAILING_BYTES,
            };
            return None;
        }

        let released = code & BREAK_BIT != 0;
        let key = code & !BREAK_BIT;

        // 修飾キーは押下と離脱の両方で状態を更新する。
        match key {
            SCANCODE_LEFT_SHIFT => {
                self.shift_left = !released;
                return None;
            }
            SCANCODE_RIGHT_SHIFT => {
                self.shift_right = !released;
                return None;
            }
            SCANCODE_CAPS_LOCK => {
                // **押下でのみトグルする。** 離脱でもトグルすると 1 回押した
                // だけで 2 回反転し、元に戻ってしまう。押しっぱなしのときに
                // リピートが来る場合も、make が来るたびに反転するのが
                // 実機の挙動と一致する。
                if !released {
                    self.caps_lock = !self.caps_lock;
                }
                return None;
            }
            _ => {}
        }

        // 離したことは報告しない。文字が出るのは押下のときだけ。
        if released {
            return None;
        }

        match key {
            SCANCODE_ENTER => Some(KeyEvent::Enter),
            SCANCODE_BACKSPACE => Some(KeyEvent::Backspace),
            SCANCODE_TAB => Some(KeyEvent::Char('\t')),
            _ => match self.character_for(key) {
                Some(character) => Some(KeyEvent::Char(character)),
                None => Some(KeyEvent::Unsupported(key)),
            },
        }
    }

    /// 押下されたキーに対応する文字。文字を持たないキーは `None`。
    ///
    /// # Shift と Caps Lock の非対称性
    ///
    /// **英字は Shift と Caps Lock の排他的論理和、記号は Shift だけが効く。**
    /// ここを取り違えると、Caps Lock 中に数字段が記号になるという分かりやすい
    /// バグと、Caps Lock + Shift で大文字のままになるという分かりにくいバグの
    /// 両方が出る。
    fn character_for(&self, key: u8) -> Option<char> {
        let index = key as usize;
        if index >= TABLE_LEN {
            return None;
        }

        let base = UNSHIFTED[index];
        if base == '\0' {
            return None;
        }

        let use_shifted = if base.is_ascii_alphabetic() {
            // 英字だけ Caps Lock が効く。両方押されていれば打ち消し合う。
            self.shift() != self.caps_lock
        } else {
            // 記号と数字は Shift のみ。Caps Lock は関係しない。
            self.shift()
        };

        let character = if use_shifted { SHIFTED[index] } else { base };
        if character == '\0' {
            None
        } else {
            Some(character)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 押下と離脱を順に食べさせ、報告されたイベントだけを集める補助。
    fn feed_all(decoder: &mut Decoder, codes: &[u8]) -> std::vec::Vec<KeyEvent> {
        extern crate std;
        codes.iter().filter_map(|&c| decoder.feed(c)).collect()
    }

    fn chars(events: &[KeyEvent]) -> std::string::String {
        extern crate std;
        events
            .iter()
            .filter_map(|e| match e {
                KeyEvent::Char(c) => Some(*c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_plain_letter_is_lowercase() {
        let mut decoder = Decoder::new();
        // 0x1E = 'a' の押下、0x9E = 'a' の離脱。
        let events = feed_all(&mut decoder, &[0x1E, 0x9E]);
        assert_eq!(events, [KeyEvent::Char('a')]);
    }

    /// **離したときには文字を出さない。**
    ///
    /// break を押下と同じに扱うと、1 回打つたびに 2 文字出る。
    #[test]
    fn releasing_a_key_produces_nothing() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(0x9E), None, "'a' の break だけでは何も出ない");
    }

    #[test]
    fn shift_makes_letters_uppercase_and_restores_on_release() {
        let mut decoder = Decoder::new();
        // LShift 押下 -> 'a' -> LShift 離脱 -> 'a'
        let events = feed_all(&mut decoder, &[0x2A, 0x1E, 0x9E, 0xAA, 0x1E, 0x9E]);
        assert_eq!(chars(&events), "Aa", "離したら小文字へ戻る");
    }

    /// 左右の Shift は独立に数える。
    ///
    /// 片方を離しただけで解除してしまうと、両手で Shift を押したときに
    /// 途中から小文字になる。
    #[test]
    fn the_two_shift_keys_are_tracked_independently() {
        let mut decoder = Decoder::new();
        // 左右とも押す -> 左だけ離す -> まだ Shift は有効
        feed_all(&mut decoder, &[0x2A, 0x36, 0xAA]);
        assert!(decoder.shift(), "右 Shift がまだ押されている");
        let events = feed_all(&mut decoder, &[0x1E]);
        assert_eq!(chars(&events), "A");
        // 右も離せば解除。
        feed_all(&mut decoder, &[0xB6]);
        assert!(!decoder.shift());
    }

    #[test]
    fn caps_lock_toggles_only_on_press() {
        let mut decoder = Decoder::new();
        assert!(!decoder.caps_lock());
        // 押下 + 離脱で 1 回だけ反転する。離脱でも反転すると元に戻ってしまう。
        feed_all(&mut decoder, &[0x3A, 0xBA]);
        assert!(decoder.caps_lock(), "1 回押したら有効のまま");
        feed_all(&mut decoder, &[0x3A, 0xBA]);
        assert!(!decoder.caps_lock(), "もう 1 回押したら解除");
    }

    /// **英字は Shift と Caps の XOR、記号は Shift のみ。**
    ///
    /// この非対称性が本モジュールで最も取り違えやすい。3 つの状態を並べて
    /// 固定する。
    #[test]
    fn shift_and_caps_lock_are_asymmetric_between_letters_and_symbols() {
        // Caps Lock だけ: 英字は大文字、数字段はそのまま。
        let mut decoder = Decoder::new();
        feed_all(&mut decoder, &[0x3A, 0xBA]); // Caps Lock ON
        let events = feed_all(&mut decoder, &[0x1E, 0x9E, 0x02, 0x82]); // 'a', '1'
        assert_eq!(chars(&events), "A1", "Caps は英字にだけ効く");

        // Caps Lock + Shift: 英字は小文字へ戻り、数字段は記号になる。
        let events = feed_all(&mut decoder, &[0x2A, 0x1E, 0x9E, 0x02, 0x82, 0xAA]);
        assert_eq!(
            chars(&events),
            "a!",
            "英字は XOR で打ち消し、記号は Shift が効く"
        );

        // Shift だけ（Caps 解除）: 英字は大文字、数字段は記号。
        feed_all(&mut decoder, &[0x3A, 0xBA]); // Caps Lock OFF
        let events = feed_all(&mut decoder, &[0x2A, 0x1E, 0x9E, 0x02, 0x82, 0xAA]);
        assert_eq!(chars(&events), "A!");
    }

    #[test]
    fn enter_tab_and_backspace_are_reported_separately() {
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(0x1C), Some(KeyEvent::Enter));
        assert_eq!(decoder.feed(0x0E), Some(KeyEvent::Backspace));
        assert_eq!(decoder.feed(0x0F), Some(KeyEvent::Char('\t')));
    }

    /// 拡張キー（0xE0 プレフィックス）は押下のときだけ 1 回報告する。
    #[test]
    fn extended_keys_are_reported_once_on_press() {
        let mut decoder = Decoder::new();
        // 0xE0 0x48 = カーソル上（押下）、0xE0 0xC8 = 同（離脱）。
        assert_eq!(decoder.feed(0xE0), None, "プレフィックス単独では何も出ない");
        assert_eq!(decoder.feed(0x48), Some(KeyEvent::Unsupported(0x48)));
        assert_eq!(decoder.feed(0xE0), None);
        assert_eq!(decoder.feed(0xC8), None, "離脱では報告しない");
    }

    /// **拡張キーの後で通常のキーが壊れない。**
    #[test]
    fn a_normal_key_after_an_extended_sequence_still_decodes() {
        let mut decoder = Decoder::new();
        feed_all(&mut decoder, &[0xE0, 0x48, 0xE0, 0xC8]);
        let events = feed_all(&mut decoder, &[0x1E, 0x9E]);
        assert_eq!(chars(&events), "a");
    }

    /// Pause は 6 バイト列で、**内部に 2 個目の 0xE1 を含む**。
    ///
    /// 0xE0 と同じ「次の 1 バイトを飛ばす」実装だと、内側の 0xE1 で状態機械が
    /// 入れ子になり、以降のバイト境界がずれてキー入力全体が壊れる。数えて
    /// 飲み込む実装であることを固定する。
    #[test]
    fn the_pause_sequence_is_swallowed_whole_including_its_inner_prefix() {
        let mut decoder = Decoder::new();
        let pause = [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5];
        let events = feed_all(&mut decoder, &pause);
        assert_eq!(
            events,
            [KeyEvent::Unsupported(0xE1)],
            "列全体で 1 回だけ報告する"
        );
        // 直後の通常キーが正しく読めること。ここがずれると全部化ける。
        let events = feed_all(&mut decoder, &[0x1E, 0x9E]);
        assert_eq!(chars(&events), "a", "Pause の後もバイト境界が保たれる");
    }

    /// Pause 列の途中に含まれる 0x1D / 0x45 が、Ctrl や NumLock として
    /// 解釈されてしまわないこと。
    #[test]
    fn bytes_inside_the_pause_sequence_are_not_interpreted_as_keys() {
        let mut decoder = Decoder::new();
        feed_all(&mut decoder, &[0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5]);
        // Shift も Caps も変化していない。
        assert!(!decoder.shift());
        assert!(!decoder.caps_lock());
    }

    #[test]
    fn unsupported_scancodes_are_reported_not_silently_dropped() {
        let mut decoder = Decoder::new();
        // 0x3B = F1。表の範囲内だが文字を持たない。
        assert_eq!(decoder.feed(0x3B), Some(KeyEvent::Unsupported(0x3B)));
        // 0x57 = F11。表の範囲外。
        assert_eq!(decoder.feed(0x57), Some(KeyEvent::Unsupported(0x57)));
        // どちらも離脱では黙る。
        assert_eq!(decoder.feed(0xBB), None);
        assert_eq!(decoder.feed(0xD7), None);
    }

    /// 表の範囲外でも状態機械が壊れないこと。
    #[test]
    fn an_out_of_range_scancode_does_not_break_the_decoder() {
        let mut decoder = Decoder::new();
        decoder.feed(0x7F);
        let events = feed_all(&mut decoder, &[0x1E, 0x9E]);
        assert_eq!(chars(&events), "a");
    }

    /// 実際に打った並びを通しで確認する。`hello!` を打つ。
    #[test]
    fn a_realistic_sequence_decodes_to_the_expected_text() {
        let mut decoder = Decoder::new();
        let codes = [
            0x23, 0xA3, // h
            0x12, 0x92, // e
            0x26, 0xA6, // l
            0x26, 0xA6, // l
            0x18, 0x98, // o
            0x2A, 0x02, 0x82, 0xAA, // Shift + 1 = '!'
        ];
        let events = feed_all(&mut decoder, &codes);
        assert_eq!(chars(&events), "hello!");
    }

    /// 変換表の 2 つが同じ長さで、対応が食い違っていないこと。
    ///
    /// 片方だけに文字があると、Shift の有無で「文字が消える」挙動になる。
    #[test]
    fn the_two_tables_agree_on_which_scancodes_produce_characters() {
        for index in 0..TABLE_LEN {
            assert_eq!(
                UNSHIFTED[index] == '\0',
                SHIFTED[index] == '\0',
                "scancode {index:#04x} で 2 つの表が食い違っている"
            );
        }
    }
}
