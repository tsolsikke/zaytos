//! 補完の純粋な部分（TAB-1）。
//!
//! # 候補を溜めないので、増分で足す形にしてある
//!
//! **`zash` の書き込み可の区画はページの終わりまで 584 バイトしか無い**
//! （実測。2026-09-04）。**候補の表を持つとページを 1 つ越えるので、
//! 2 度歩いて数と共通接頭辞を求める形にした**（`ADR-0056`）。
//! **ここが持つのは「2 本の共通接頭辞」だけで、束ねるのは呼ぶ側である。**
//!
//! # ホストで固定する
//!
//! **語の切り出しも接頭辞の計算も、ハードに依らない純粋な判断である**
//! （`CLAUDE.md` の絶対規則の 5 つ目）。**`common/src/env.rs` と同じ形で、
//! `zash` は `#[path]` で取り込む。**

/// 2 本の共通接頭辞の長さ（バイト）。
///
/// # 字の境界へ丸めない
///
/// **多バイトの字の途中で切れうる。** **丸めていないのは、行がバイト列
/// だからである**——**`zash` は行をバイトで数え、Backspace も 1 バイト
/// 戻す**（`zi` と違い、挿入点が字の境界に在ることを前提にしていない）。
/// **割れた列は画面で置換文字になるが、行も像も壊れない。**
///
/// **像に入る名前は ASCII だけである**（`cargo xtask check` の静的検査）。
/// **`disk0.img` は外の道具で触れるので、多バイトの名前が入る道はある。**
pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let mut at = 0usize;
    while at < a.len() && at < b.len() && a[at] == b[at] {
        at += 1;
    }
    at
}

/// `prefix` で始まるか。
pub fn starts_with(name: &[u8], prefix: &[u8]) -> bool {
    name.len() >= prefix.len() && &name[..prefix.len()] == prefix
}

/// 補完する語（TAB-1）。**挿入点で終わる語である。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Word {
    /// 語の先頭（`line` の添字）。
    pub start: usize,
    /// 語の終わり（挿入点そのもの）。
    pub end: usize,
    /// 行の最初の語か。**コマンド名として補完するかを決める。**
    pub first: bool,
}

impl Word {
    /// 語の長さ。
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// 空の語か。
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// 挿入点で終わる語を切り出す（TAB-1）。
///
/// **区切りは空白 1 種類だけである**（`0x20`）——**`run_line` の語の切り方と
/// 同じにする。** **引用も `${}` も無いので、規則を増やさない**
/// （`ADR-0049` の「決めないこと」）。
///
/// **`first` は「前に語が無いこと」である。** **前が空白だけなら真である。**
pub fn word_at_cursor(line: &[u8], cursor: usize) -> Word {
    let end = cursor.min(line.len());
    let mut start = end;
    while start > 0 && line[start - 1] != b' ' {
        start -= 1;
    }
    let first = !line[..start].iter().any(|byte| *byte != b' ');
    Word { start, end, first }
}

/// パスの語を「ディレクトリの部分」と「接頭辞」に割る（TAB-1）。
///
/// **ディレクトリの部分は最後の `/` までを含む。** **`/` が無ければ空である。**
///
/// **`/bin/l` は `/bin/` と `l`、`/bin/` は `/bin/` と空、`l` は空と `l` である。**
pub fn split_path(word: &[u8]) -> (&[u8], &[u8]) {
    match word.iter().rposition(|byte| *byte == b'/') {
        Some(at) => (&word[..at + 1], &word[at + 1..]),
        None => (&word[..0], word),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_prefix_stops_at_the_first_difference() {
        assert_eq!(common_prefix_len(b"rm", b"rmdir"), 2);
        assert_eq!(common_prefix_len(b"rmdir", b"rm"), 2);
        assert_eq!(common_prefix_len(b"ls", b"rm"), 0);
        assert_eq!(common_prefix_len(b"echo", b"echo"), 4);
        assert_eq!(common_prefix_len(b"", b"echo"), 0);
    }

    /// **字の境界へ丸めない**（doc のとおり）。
    #[test]
    fn the_common_prefix_can_split_a_character() {
        // **`あ` と `い` は先頭の 2 バイトが同じである**（`E3 81 82` と `E3 81 84`）。
        assert_eq!(common_prefix_len("あ".as_bytes(), "い".as_bytes()), 2);
    }

    #[test]
    fn the_word_ends_at_the_cursor() {
        let line = b"echo abc";
        assert_eq!(
            word_at_cursor(line, 8),
            Word {
                start: 5,
                end: 8,
                first: false
            }
        );
        // **語の途中でも、そこで終わる語を返す。**
        assert_eq!(
            word_at_cursor(line, 7),
            Word {
                start: 5,
                end: 7,
                first: false
            }
        );
    }

    #[test]
    fn the_first_word_is_the_one_with_only_blanks_before_it() {
        assert!(word_at_cursor(b"ec", 2).first);
        assert!(word_at_cursor(b"   ec", 5).first);
        assert!(!word_at_cursor(b"echo a", 6).first);
        // **行末が空白なら、次の語は空で、最初ではない。**
        assert!(!word_at_cursor(b"echo ", 5).first);
    }

    #[test]
    fn an_empty_line_gives_an_empty_first_word() {
        let word = word_at_cursor(b"", 0);
        assert!(word.is_empty());
        assert!(word.first);
    }

    #[test]
    fn the_path_splits_at_the_last_slash() {
        assert_eq!(split_path(b"/bin/l"), (&b"/bin/"[..], &b"l"[..]));
        assert_eq!(split_path(b"/bin/"), (&b"/bin/"[..], &b""[..]));
        assert_eq!(split_path(b"l"), (&b""[..], &b"l"[..]));
        assert_eq!(split_path(b"~/.pro"), (&b"~/"[..], &b".pro"[..]));
    }
}
