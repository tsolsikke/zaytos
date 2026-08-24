//! 全画面のアプリが動く間、`fd 2`へ来たエラーを溜めておく控え（ADR-0046）。
//!
//! # なぜ溜めるのか
//!
//! **代替画面に居るアプリの画面へ、カーネルが勝手に書くと絵が壊れる。**
//! **実際に壊れた**——`zi`の本文がカーソルの居る行ごと上書きされた
//! （`docs/troubleshooting.md`の2026-08-21）。
//!
//! **かといって捨てない。** **使う人が読むはずのものだからである。**
//! **溜めて、アプリが取り出し、アプリ自身がエコーエリアへ描く**
//! （`ioctl(TIOCZTAKE)`。ADR-0046）。**描くのがアプリなので、
//! アプリの再描画と競合しない。**
//!
//! # 記録ではない。画面へ出すための控えである
//!
//! **`sys_write`はシリアルへ先に書く。** **溢れても記録は全部残る**ので、
//! **ここは「次の1行を画面へ出す」ためだけの器でよい。**
//!
//! # 溜める場所は`.bss`の静的である
//!
//! **`crate::console::PENDING`にある。** **最初は[`crate::console::Console`]の
//! 中へ置いたが、あれは`kernel_main`のローカルで、起動時のスタックが
//! 272バイト深くなり、64KiBを越えてガードページを踏んだ**（実測。
//! `crate::console::PENDING`の doc に経緯がある）。

/// `ioctl`でやり取りする構造の大きさ（バイト）。
///
/// **`[0..2]`が長さ、`[2..4]`が捨てた数、`[4..]`が本文である。**
pub(crate) const ZDIAG_LEN: usize = 256;

/// 本文に使える大きさ（バイト）。
pub(crate) const ZDIAG_TEXT: usize = ZDIAG_LEN - 4;

/// 溜めてあるエラーの控え。
pub(crate) struct Pending {
    text: [u8; ZDIAG_TEXT],
    len: usize,
    /// 入りきらずに捨てたバイト数。**飽和させる。**
    dropped: u16,
}

impl Pending {
    pub(crate) const fn new() -> Self {
        Self {
            text: [0u8; ZDIAG_TEXT],
            len: 0,
            dropped: 0,
        }
    }

    /// 溜める。**入る分だけ入れ、残りは数える。**
    ///
    /// # 先頭を残す
    ///
    /// **最初のエラーが原因であることが多い。** **後から来たほうを捨て、
    /// 捨てた数を控える**——**黙って捨てない**（ADR-0046）。
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        let room = ZDIAG_TEXT - self.len;
        let take = bytes.len().min(room);
        self.text[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        let lost = bytes.len() - take;
        if lost > 0 {
            self.dropped = self
                .dropped
                .saturating_add(lost.min(u16::MAX as usize) as u16);
        }
    }

    /// 溜まっているか。
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 本文。
    pub(crate) fn text(&self) -> &[u8] {
        &self.text[..self.len]
    }

    /// 空にする。
    pub(crate) fn clear(&mut self) {
        self.len = 0;
        self.dropped = 0;
    }

    /// `ioctl`が返す形へ書き出し、空にする。
    ///
    /// **溜まっていなければ長さ 0 を返す。** **「無い」は誤りではない**——
    /// アプリは毎周訊きに来るので、**空で返るほうが普通である。**
    pub(crate) fn take_into(&mut self, out: &mut [u8; ZDIAG_LEN]) {
        out[0..2].copy_from_slice(&(self.len as u16).to_le_bytes());
        out[2..4].copy_from_slice(&self.dropped.to_le_bytes());
        out[4..4 + self.len].copy_from_slice(&self.text[..self.len]);
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_line_goes_in_whole() {
        let mut pending = Pending::new();
        pending.push(b"zi: cannot open for writing\n");
        assert_eq!(pending.text(), b"zi: cannot open for writing\n");
        assert!(!pending.is_empty());
    }

    #[test]
    fn the_overflow_is_counted_and_the_head_is_kept() {
        let mut pending = Pending::new();
        pending.push(&[b'a'; ZDIAG_TEXT]);
        pending.push(b"bbb");
        assert_eq!(pending.text().len(), ZDIAG_TEXT);
        assert!(pending.text().iter().all(|byte| *byte == b'a'));
        let mut out = [0u8; ZDIAG_LEN];
        pending.take_into(&mut out);
        assert_eq!(u16::from_le_bytes([out[2], out[3]]), 3, "捨てた数");
    }

    #[test]
    fn taking_empties_it() {
        let mut pending = Pending::new();
        pending.push(b"boom");
        let mut out = [0u8; ZDIAG_LEN];
        pending.take_into(&mut out);
        assert_eq!(u16::from_le_bytes([out[0], out[1]]), 4);
        assert_eq!(&out[4..8], b"boom");
        assert!(pending.is_empty());
        // **2 回目は空である。** 同じものを 2 度出さない。
        let mut again = [0u8; ZDIAG_LEN];
        pending.take_into(&mut again);
        assert_eq!(u16::from_le_bytes([again[0], again[1]]), 0);
    }
}
