//! 多バイトの字を扱う純粋な論理（`ADR-0054`）。
//!
//! # 誰が使うのか
//!
//! **カーネルとユーザープログラムの両方である。** カーネルは画面へ出す前に
//! バイト列を字へ復号し（[`Utf8Decoder`]）、幅を [`width_cells`] で決める。
//! **`zi` は挿入点を境界の上に置くために [`next_boundary`] と
//! [`prev_boundary`] を、画面の桁のために [`display_column`] を使う。**
//!
//! **取り込み方は `ADR-0045` の形である**（`#[path]` で取り込む。
//! `crate::` を参照しない）。
//!
//! # ここに在るのは純粋な論理だけである
//!
//! **`core` しか使わない**（`CLAUDE.md` の絶対ルール 5）。**ホストで固定できる。**

/// 収録されていない字の代わりに出すもの（U+FFFD）。
///
/// **`kernel/src/graphics/font` にも同じ定数が在る。** **あちらは「グリフを
/// 引けなかったときに描くもの」で、こちらは「復号できなかったバイトを何と
/// 見なすか」である。** **層が違うので分けてある。**
pub const REPLACEMENT: char = '\u{FFFD}';

/// 字が占めるセルの数（`ADR-0054` の Decision 1）。**1 か 2 である。**
///
/// # なぜ符号位置で決めるのか
///
/// **グリフから取らない。** **収録の無い字は置換文字のグリフになり、幅が 1 に
/// なる**（実測。2026-09-01）。**字形を足す前に幅を正しくでき、足した後も
/// 幅の出所を変えずに済む。**
///
/// # 表は最小である（`ADR-0054` の Decision 2）
///
/// **根拠は East Asian Width で、`Wide` と `Fullwidth` のうち幅が曖昧でない
/// 範囲だけを採る。** **絵文字と `Ambiguous` は採らない**——**幅が実装で割れる。**
///
/// **足すときはこの関数へ範囲を 1 行足す。**
pub fn width_cells(c: char) -> u32 {
    let code = c as u32;
    let wide = matches!(code,
        0x1100..=0x115F        // ハングル字母
        | 0x2E80..=0x303E      // CJK の部首・記号（`U+303F` は除く）
        | 0x3041..=0xA4CF      // かな・注音・CJK 統合漢字
        | 0xAC00..=0xD7A3      // ハングル音節
        | 0xF900..=0xFAFF      // CJK 互換漢字
        | 0xFE30..=0xFE6F      // CJK 互換形・小字形
        | 0xFF00..=0xFF60      // 全角形
        | 0xFFE0..=0xFFE6      // 全角の記号
        | 0x20000..=0x3FFFD    // CJK 拡張 B 以降
    );
    // 破壊 (ADR-0054, width-always-one-test): **幅を常に 1 にする。**
    // **`ADR-0054` の前の形そのものである**——**全角も 1 セルで進む。**
    // **`utf8-test` の「全角は 2 セル」と「桁が字で進む」が落ちる。**
    #[cfg(any(feature = "width-always-one", width_always_one))]
    let wide = false && wide;
    if wide {
        2
    } else {
        1
    }
}

/// `write` をまたぐ UTF-8 の復号器（`ADR-0054` の Decision 3・4）。
///
/// # なぜ状態を持つのか
///
/// **1 字が 2 回の `write` に割れて届くことがある**（システムコールはページ単位で
/// 刻む。`FOREGROUND_ANSI` と同じ事情である）。**`write` ごとに
/// `core::str::from_utf8` を通す形だと、割れた字は両方が不正になる。**
///
/// # 不正なバイトは 1 つずつ置換文字にする
///
/// **丸ごと落とさない**——**1 バイトの不正で `write` 全体が消えると、`zi` の
/// 画面が真っ白になる**（`ADR-0054` の Alternatives (b)）。
///
/// # 検算は `core` に任せる
///
/// **並びを自分で判定しない。** **`need` バイト揃った時点で
/// `core::str::from_utf8` に訊く**——**冗長な符号化も、サロゲートも、
/// あちらが落とす。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Utf8Decoder {
    buf: [u8; 4],
    len: usize,
    need: usize,
}

impl Default for Utf8Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Utf8Decoder {
    /// 何も抱えていない復号器。
    pub const fn new() -> Self {
        Self {
            buf: [0; 4],
            len: 0,
            need: 0,
        }
    }

    /// 抱えているバイトの数。**0 なら字の途中ではない。**
    pub fn pending(&self) -> usize {
        self.len
    }

    /// 抱えているものを捨てる。**捨てたバイト数を返す。**
    ///
    /// **前景が変わるときに呼ぶ**（`ADR-0054` の Decision 4）。**前のプログラムの
    /// 途中のバイトを、次のプログラムの 1 字目にくっつけない。**
    pub fn reset(&mut self) -> usize {
        let dropped = self.len;
        self.len = 0;
        self.need = 0;
        dropped
    }

    /// 1 バイト与える。**字が出来たら `emit` を呼ぶ。**
    ///
    /// **1 回の呼び出しで `emit` が複数回呼ばれることがある**——**字の途中で
    /// 別のバイトが来たとき、抱えていたぶんを 1 つずつ置換文字にしてから、
    /// 来たバイトを改めて処理する。**
    pub fn feed<F: FnMut(char)>(&mut self, byte: u8, emit: &mut F) {
        if self.len == 0 {
            self.start(byte, emit);
            return;
        }
        // **続きのバイトでなければ、抱えていたものは壊れている。**
        if !(0x80..=0xBF).contains(&byte) {
            self.flush_broken(emit);
            self.start(byte, emit);
            return;
        }
        self.buf[self.len] = byte;
        self.len += 1;
        if self.len < self.need {
            return;
        }
        match core::str::from_utf8(&self.buf[..self.len]) {
            Ok(text) => {
                for c in text.chars() {
                    emit(c);
                }
            }
            // **冗長な符号化・サロゲート・範囲外。** **1 バイトにつき 1 つ出す。**
            Err(_) => self.flush_broken(emit),
        }
        self.len = 0;
        self.need = 0;
    }

    /// 字の頭として 1 バイトを見る。
    fn start<F: FnMut(char)>(&mut self, byte: u8, emit: &mut F) {
        let need = match byte {
            0x00..=0x7F => {
                emit(byte as char);
                return;
            }
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            // **続きのバイト単体（`0x80..=0xBF`）と、頭になれないもの。**
            _ => {
                emit(REPLACEMENT);
                return;
            }
        };
        self.buf[0] = byte;
        self.len = 1;
        self.need = need;
    }

    /// 抱えていたぶんを 1 バイトにつき 1 つの置換文字にする。
    fn flush_broken<F: FnMut(char)>(&mut self, emit: &mut F) {
        for _ in 0..self.len {
            emit(REPLACEMENT);
        }
        self.len = 0;
        self.need = 0;
    }
}

/// `at` から始まる字と、そのバイト数（`ADR-0054` の Decision 5）。
///
/// **復号できなければ置換文字と 1 バイトを返す**——**`zi` はどんなファイルでも
/// 開けるので、壊れたバイトの上でも動けなければならない。**
///
/// **`at` が範囲の外なら `None`。**
pub fn char_at(bytes: &[u8], at: usize) -> Option<(char, usize)> {
    if at >= bytes.len() {
        return None;
    }
    let need = match bytes[at] {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return Some((REPLACEMENT, 1)),
    };
    let end = at + need;
    if end > bytes.len() {
        return Some((REPLACEMENT, 1));
    }
    match core::str::from_utf8(&bytes[at..end]) {
        Ok(text) => text.chars().next().map(|c| (c, need)),
        Err(_) => Some((REPLACEMENT, 1)),
    }
}

/// `at` が字の境界か（`ADR-0054` の Decision 5）。
///
/// **続きのバイト（`0x80..=0xBF`）の上でなければ境界である。**
/// **末尾（`bytes.len()`）も境界である。**
pub fn is_boundary(bytes: &[u8], at: usize) -> bool {
    match bytes.get(at) {
        None => at == bytes.len(),
        Some(byte) => !(0x80..=0xBF).contains(byte),
    }
}

/// 次の字の境界（`ADR-0054` の Decision 5）。**末尾なら動かない。**
pub fn next_boundary(bytes: &[u8], at: usize) -> usize {
    match char_at(bytes, at) {
        Some((_, length)) => at + length,
        None => at,
    }
}

/// 前の字の境界（`ADR-0054` の Decision 5）。**先頭なら動かない。**
///
/// **後ろから境界を探す**——**続きのバイト（`0x80..=0xBF`）でない位置まで戻る。**
/// **上限は 4 バイトである**（それ以上戻っても字にならない）。
pub fn prev_boundary(bytes: &[u8], at: usize) -> usize {
    if at == 0 {
        return 0;
    }
    let mut back = at - 1;
    let floor = at.saturating_sub(4);
    while back > floor && (0x80..=0xBF).contains(&bytes[back]) {
        back -= 1;
    }
    // **戻った先から数えて `at` に届かないなら、境界ではない**——
    // **壊れたバイトなので 1 つだけ戻る。**
    match char_at(bytes, back) {
        Some((_, length)) if back + length == at => back,
        _ => at - 1,
    }
}

/// `upto` までの表示の桁（`ADR-0054` の Decision 5）。
///
/// **前の字の幅の合計である。** **バイトの添字ではない。**
pub fn display_column(bytes: &[u8], upto: usize) -> usize {
    let mut at = 0usize;
    let mut column = 0usize;
    while at < upto {
        let Some((c, length)) = char_at(bytes, at) else {
            break;
        };
        column += width_cells(c) as usize;
        at += length;
    }
    column
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn feed_all(decoder: &mut Utf8Decoder, bytes: &[u8]) -> Vec<char> {
        let mut out = Vec::new();
        for byte in bytes {
            decoder.feed(*byte, &mut |c| out.push(c));
        }
        out
    }

    /// 幅は符号位置で決まる（`ADR-0054` の Decision 1）。
    #[test]
    fn wide_characters_take_two_cells() {
        assert_eq!(width_cells('a'), 1);
        assert_eq!(width_cells('~'), 1);
        assert_eq!(width_cells(REPLACEMENT), 1);
        assert_eq!(width_cells('あ'), 2);
        assert_eq!(width_cells('漢'), 2);
        assert_eq!(width_cells('Ａ'), 2); // 全角形
                                          // **境界の両側を見る。** `U+303F` は表から外してある。
        assert_eq!(width_cells('\u{303F}'), 1);
        assert_eq!(width_cells('\u{3041}'), 2);
        // **採らないと決めたもの。**
        assert_eq!(width_cells('\u{1F600}'), 1); // 絵文字
        assert_eq!(width_cells('\u{00B1}'), 1); // Ambiguous
    }

    /// 素の ASCII はそのまま出る。
    #[test]
    fn ascii_passes_through() {
        let mut decoder = Utf8Decoder::new();
        assert_eq!(feed_all(&mut decoder, b"ab\n"), ['a', 'b', '\n']);
        assert_eq!(decoder.pending(), 0);
    }

    /// 多バイトの字が 1 つになる。
    #[test]
    fn a_multibyte_character_becomes_one_char() {
        let mut decoder = Utf8Decoder::new();
        assert_eq!(feed_all(&mut decoder, "あ漢".as_bytes()), ['あ', '漢']);
    }

    /// **`write` をまたいでも 1 字になる**（`ADR-0054` の Decision 3）。
    ///
    /// **台本でこの形は作れない**（`zi` も `less` も 1 画面を 1 回で送る）。
    /// **配線の側は読んで気づくしかないので、復号器の側をここで固定する。**
    #[test]
    fn a_character_split_across_two_writes_is_one_char() {
        let bytes = "あ".as_bytes();
        let mut decoder = Utf8Decoder::new();
        let first = feed_all(&mut decoder, &bytes[..2]);
        assert!(first.is_empty());
        assert_eq!(decoder.pending(), 2);
        let second = feed_all(&mut decoder, &bytes[2..]);
        assert_eq!(second, ['あ']);
        assert_eq!(decoder.pending(), 0);
    }

    /// 不正なバイトは 1 つにつき 1 つの置換文字になる。
    #[test]
    fn a_broken_byte_becomes_one_replacement() {
        let mut decoder = Utf8Decoder::new();
        // **続きのバイト単体。**
        assert_eq!(feed_all(&mut decoder, &[0x80]), [REPLACEMENT]);
        // **頭になれないバイト。**
        assert_eq!(feed_all(&mut decoder, &[0xFF]), [REPLACEMENT]);
        // **字の途中で別のバイトが来た。** 抱えていた 1 バイトぶんと、来た字。
        assert_eq!(feed_all(&mut decoder, &[0xE3, b'a']), [REPLACEMENT, 'a']);
        // **冗長な符号化。** 3 バイトぶん出る。
        assert_eq!(
            feed_all(&mut decoder, &[0xE0, 0x80, 0x80]),
            [REPLACEMENT, REPLACEMENT, REPLACEMENT]
        );
        // **周りの字は消えない**——**丸ごと落とさないのが要である。**
        assert_eq!(
            feed_all(&mut decoder, &[b'x', 0x80, b'y']),
            ['x', REPLACEMENT, 'y']
        );
    }

    /// 抱えたまま捨てられる（`ADR-0054` の Decision 4）。
    #[test]
    fn resetting_drops_the_pending_bytes() {
        let mut decoder = Utf8Decoder::new();
        let _ = feed_all(&mut decoder, &"あ".as_bytes()[..2]);
        assert_eq!(decoder.pending(), 2);
        assert_eq!(decoder.reset(), 2);
        assert_eq!(decoder.pending(), 0);
        // **捨てた後は、次の字が汚れない。**
        assert_eq!(feed_all(&mut decoder, b"a"), ['a']);
    }

    /// 境界を跨いで動く（`ADR-0054` の Decision 5）。
    #[test]
    fn the_cursor_moves_by_characters() {
        let line = "aあb".as_bytes(); // 1 + 3 + 1
        assert_eq!(next_boundary(line, 0), 1);
        assert_eq!(next_boundary(line, 1), 4);
        assert_eq!(next_boundary(line, 4), 5);
        // **末尾では動かない。**
        assert_eq!(next_boundary(line, 5), 5);
        assert_eq!(prev_boundary(line, 5), 4);
        assert_eq!(prev_boundary(line, 4), 1);
        assert_eq!(prev_boundary(line, 1), 0);
        // **先頭では動かない。**
        assert_eq!(prev_boundary(line, 0), 0);
    }

    /// 境界かどうかを見る。
    #[test]
    fn a_continuation_byte_is_not_a_boundary() {
        let line = "aあ".as_bytes();
        assert!(is_boundary(line, 0));
        assert!(is_boundary(line, 1));
        assert!(!is_boundary(line, 2));
        assert!(!is_boundary(line, 3));
        // **末尾も境界である。**
        assert!(is_boundary(line, 4));
    }

    /// 壊れたバイトの上でも動ける。**1 バイトずつ進む。**
    #[test]
    fn a_broken_byte_moves_one_byte() {
        let line = &[b'a', 0xFF, b'b'];
        assert_eq!(char_at(line, 1), Some((REPLACEMENT, 1)));
        assert_eq!(next_boundary(line, 1), 2);
        assert_eq!(prev_boundary(line, 2), 1);
    }

    /// 桁は幅の合計である（`ADR-0054` の Decision 5）。
    #[test]
    fn the_column_is_the_sum_of_the_widths() {
        let line = "aあb".as_bytes();
        assert_eq!(display_column(line, 0), 0);
        assert_eq!(display_column(line, 1), 1);
        // **全角は 2 桁ぶんである。**
        assert_eq!(display_column(line, 4), 3);
        assert_eq!(display_column(line, 5), 4);
    }
}
