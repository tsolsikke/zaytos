//! プロセスの初めのスタックの形（`argc`・`argv`・`envp`・`auxv`。Linux と同じ形。S11-1）。
//!
//! **CPU によらない**——x86_64 も aarch64 も、entry の時点のスタックの指す先から、`argc`、`argv` のポインタ、NULL、
//! `envp` のポインタ、NULL、`auxv` の対と並ぶ（2026-09-30 に確かめた。`crt1.o` の `_start` は両方とも `argc` をスタック
//! の指す先から、`argv` をその 8 バイト上から読む。`libc.a` の `__libc_start_main` は両方とも `envp` を
//! `argv + argc + 1` とし、`envp` の NULL の次を `auxv` として読む。`AT_NULL` と `AT_PHDR` の値は `linux/auxvec.h` で
//! 同じ）。**16 バイト整列**: aarch64 の `_start` はスタックを整列し直さずに使い、x86_64 の `_start` は整列し直す。
//! 16 に揃えて積めば、どちらでも足りる。
//!
//! `ADR-0071` の決定 1 の 2 で、`crate::userland` の `build_initial_stack` から分けた（2026-09-30）。**`argv` と `envp` の
//! 数の上限を見るのと、スタックページを切り出すのは共通の側である。**

/// ユーザープログラムの初期スタックを **Linux と同じ形で**積む（S11-1）。**収まらなければ `None`。**
///
/// `page` はスタックの 1 ページ、`page_base` はその先頭の利用者の仮想アドレスである（文字列を指すポインタに使う）。
/// 返すのは entry へ入るときのスタックの位置（`argc` を指す）。
///
/// # 形は実測で確かめた
///
/// **ホストで、`_start` から `rsp` をたどる自作の静的バイナリを走らせて観測した。**
/// `rsp` の指す先から順に——`argc`、`argv` のポインタ、NULL、`envp` のポインタ、
/// NULL、そして `auxv` の `(type, value)` の対が続き、`type == 0`（`AT_NULL`）で
/// 終わる。**文字列そのものはこの表より上（高位）に置かれる。**
///
/// **記憶で書かない**（`docs/coding-standards.md` の「実測値は、測った条件が
/// 変わると古くなる」）。`docs/vision.md` の表は**Linux バイナリを動かすための
/// 記述**で、そこには `AT_PHDR` などが要るとある。**こちらが積むのは自作の
/// プログラム向けなので、要るものが違う。**
///
/// # 何を積み、何を積まないか
///
/// **`argc` と `argv` と `envp` を積む。** `auxv` は **`AT_NULL` だけ**である。
///
/// **`envp` は EV で中身が入った**（ADR-0041）。**並びは変えていない**
/// ——S11-1 が終端だけ置いていた場所に、ポインタ列が入っただけである。
///
/// **`auxv` の中身は Linux バイナリを動かす段階で要るものである。**
/// **自作のプログラムは読まないので、終端だけ置く。**
/// **形を合わせておくのは、後から中身を足すときに入口が変わらないからである**
/// ——そして **C の `crt0` がそのまま書ける**（`docs/vision.md` の C の構想）。
///
/// # 16 バイト整列
///
/// **`rsp` は entry の時点で 16 の倍数である**（SysV の規約。Linux もそう積む）。
/// **詰め物は表と文字列の間に入る。**
///
/// # 文字列の位置
///
/// **文字列を上から詰めた後、同じ順にたどり直して位置を求める**——控えの配列を持たない（`argv` と `envp` の数の
/// 上限は共通の側が見る）。
pub fn build_initial_stack(
    page: &mut [u8],
    page_base: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
) -> Option<u64> {
    /// 表の項の大きさ。
    const WORD: usize = 8;
    /// 表の固定部——`argc`・`argv` の終端・`envp` の終端・`AT_NULL` の対。
    ///
    /// **`argv` と `envp` の本体はここに入らない。** 呼ぶ側が要素数を足す。
    const FIXED_WORDS: usize = 1 + 1 + 1 + 2;
    /// `auxv` の終端。
    const AT_NULL: u64 = 0;

    let mut cursor = page.len();

    // **文字列を上から詰める。**
    //
    // **`argv` と `envp` を同じ手順で詰める（EV）。** **並びの上では
    // `argv` の表が先に来るが、文字列の置き場に順序の要求は無い**
    // ——ポインタで指すためである。
    for item in argv.iter().chain(envp) {
        // NUL 終端のぶんを含めて下げる。
        cursor = cursor.checked_sub(item.len() + 1)?;
        page[cursor..cursor + item.len()].copy_from_slice(item);
        page[cursor + item.len()] = 0;
    }

    // 表を置く位置。**表の先頭が 16 の倍数になるように下げる。**
    cursor &= !0xF;
    cursor = cursor.checked_sub((FIXED_WORDS + argv.len() + envp.len()) * WORD)?;
    cursor &= !0xF;

    // **文字列の位置は、上から詰めた順にたどり直して求める。**
    let mut string_at = page.len();
    let mut at = cursor;
    let mut put = |value: u64| {
        page[at..at + WORD].copy_from_slice(&value.to_le_bytes());
        at += WORD;
    };
    put(argv.len() as u64);
    for item in argv {
        string_at -= item.len() + 1;
        put(page_base + string_at as u64);
    }
    put(0); // argv の終端
            // **環境（EV。ADR-0041）。** **並びは変えていない**——ここに中身が入った
            // だけである。**空なら終端だけになり、S11-1 の形と同じである。**
    for item in envp {
        string_at -= item.len() + 1;
        put(page_base + string_at as u64);
    }
    put(0); // envp の終端

    // 破壊テスト (S11-1, no-auxv-terminator): `auxv` に項目を 1 つ足して、
    // **終端を書かない。** `AT_PHDR` は Linux バイナリが読む型で、
    // **自作のプログラムは `auxv` を読まないので、足しても誰も困らないように見える。**
    // **終端が無いことは、終端まで歩いた者にしか分からない。**
    #[cfg(feature = "syscall-test-no-auxv-terminator")]
    {
        /// `AT_PHDR`。**値そのものに意味は要らない**——終端の有無が主張である。
        const AT_PHDR: u64 = 3;
        put(AT_PHDR);
        put(0);
    }
    #[cfg(not(feature = "syscall-test-no-auxv-terminator"))]
    {
        put(AT_NULL); // auxv の終端（type）
        put(0); //                 （value）
    }

    Some(page_base + cursor as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word_at(page: &[u8], at: usize) -> u64 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&page[at..at + 8]);
        u64::from_le_bytes(raw)
    }

    /// 表の並び（`argc`・`argv`・NULL・`envp`・NULL・`AT_NULL` の対）と、文字列の置き場と、16 バイト整列。
    #[test]
    fn the_initial_stack_follows_the_linux_layout() {
        const BASE: u64 = 0x7FFF_0000_0000;
        let mut page = [0xEE; 4096];
        let sp = build_initial_stack(&mut page, BASE, &[b"a", b"bc"], &[b"X=1"]).unwrap();
        assert_eq!(sp % 16, 0, "16-byte aligned");
        let at = (sp - BASE) as usize;
        assert_eq!(word_at(&page, at), 2, "argc");
        assert_eq!(word_at(&page, at + 8), BASE + 4094, "argv[0]");
        assert_eq!(word_at(&page, at + 16), BASE + 4091, "argv[1]");
        assert_eq!(word_at(&page, at + 24), 0, "the end of argv");
        assert_eq!(word_at(&page, at + 32), BASE + 4087, "envp[0]");
        assert_eq!(word_at(&page, at + 40), 0, "the end of envp");
        assert_eq!(
            (word_at(&page, at + 48), word_at(&page, at + 56)),
            (0, 0),
            "AT_NULL"
        );
        assert_eq!(&page[4094..4096], b"a\0");
        assert_eq!(&page[4091..4094], b"bc\0");
        assert_eq!(&page[4087..4091], b"X=1\0");
    }

    /// 文字列と表がページに収まらなければ `None` である。
    #[test]
    fn an_initial_stack_that_does_not_fit_is_refused() {
        let mut page = [0; 64];
        let long = [b'x'; 60];
        assert_eq!(build_initial_stack(&mut page, 0, &[&long[..]], &[]), None);
    }
}
