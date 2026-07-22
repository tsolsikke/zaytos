//! Unifont の hex 形式から、kernel が使うグリフテーブルを生成する
//! （`cargo xtask gen-font`）。
//!
//! 上流の hex 形式を単一の情報源として保ち、そこから Rust のテーブルを機械的に
//! 生成する。日本語を追加するときは `third_party/unifont/unifont-subset.hex` へ
//! 範囲を足して再生成するだけで済む。
//!
//! hex 形式は 1 行 1 グリフで、`コードポイント:ビットパターン`。ビットパターンは
//! 16 行分を上から並べたもので、半角（8x16）は 1 行 1 バイトの計 32 桁、
//! 全角（16x16）は 1 行 2 バイトの計 64 桁になる。各行は最上位ビットが左端の
//! ピクセルに対応する。
//!
//! 生成物では、半角・全角どちらの行も `u16` に最上位ビット詰めで格納する。
//! こうすると kernel 側は幅に関わらず「ビット `0x8000 >> x` が立っているか」だけを
//! 見ればよくなり、半角と全角で描画コードを分けずに済む。

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};

/// 1 グリフの行数。半角・全角どちらも 16 行。
pub const GLYPH_HEIGHT: usize = 16;

/// hex 1 行を解釈した結果。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParsedGlyph {
    pub code_point: u32,
    /// 1 セル（半角）か 2 セル（全角）か。
    pub width_cells: u8,
    /// 最上位ビット詰めの行データ。長さは常に [`GLYPH_HEIGHT`]。
    pub rows: Vec<u16>,
}

/// hex 形式 1 行を解釈する。空行と `#` 始まりのコメント行は `None`。
pub fn parse_line(line: &str) -> Result<Option<ParsedGlyph>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }

    let (code_point_text, bitmap_text) = line
        .split_once(':')
        .with_context(|| format!("missing ':' separator in unifont hex line: {line:?}"))?;

    let code_point = u32::from_str_radix(code_point_text, 16)
        .with_context(|| format!("invalid code point {code_point_text:?}"))?;
    if char::from_u32(code_point).is_none() {
        bail!("code point U+{code_point:04X} is not a valid Unicode scalar value");
    }

    // 1 行あたりのバイト数から幅を決める。上流の形式では桁数が幅そのものを表す。
    let width_cells = match bitmap_text.len() {
        32 => 1u8,
        64 => 2u8,
        other => bail!(
            "unexpected bitmap length {other} for U+{code_point:04X}; \
             expected 32 (half width) or 64 (full width)"
        ),
    };
    let bytes_per_row = bitmap_text.len() / GLYPH_HEIGHT / 2;

    let mut rows = Vec::with_capacity(GLYPH_HEIGHT);
    for row_index in 0..GLYPH_HEIGHT {
        let start = row_index * bytes_per_row * 2;
        let chunk = &bitmap_text[start..start + bytes_per_row * 2];
        let raw = u16::from_str_radix(chunk, 16)
            .with_context(|| format!("invalid bitmap data {chunk:?} for U+{code_point:04X}"))?;
        // 半角は 1 バイトしか無いので、最上位ビット詰めになるよう 8 ビット寄せる。
        let row = if bytes_per_row == 1 { raw << 8 } else { raw };
        rows.push(row);
    }

    Ok(Some(ParsedGlyph {
        code_point,
        width_cells,
        rows,
    }))
}

/// hex ファイル全体を解釈し、コードポイント順に並べて返す。
pub fn parse_hex(contents: &str) -> Result<Vec<ParsedGlyph>> {
    let mut glyphs = Vec::new();
    for (line_number, line) in contents.lines().enumerate() {
        match parse_line(line)
            .with_context(|| format!("failed to parse line {}", line_number + 1))?
        {
            Some(glyph) => glyphs.push(glyph),
            None => continue,
        }
    }

    glyphs.sort_by_key(|glyph| glyph.code_point);

    // kernel 側は二分探索で引くため、重複があると引ける方が不定になる。
    for pair in glyphs.windows(2) {
        if pair[0].code_point == pair[1].code_point {
            bail!(
                "duplicate glyph for U+{:04X} in the unifont hex input",
                pair[0].code_point
            );
        }
    }

    if glyphs.is_empty() {
        bail!("the unifont hex input contains no glyphs");
    }

    Ok(glyphs)
}

/// 生成する Rust ソースを組み立てる。
pub fn render_table(glyphs: &[ParsedGlyph], source_note: &str) -> String {
    let mut out = String::new();

    writeln!(out, "// このファイルは `cargo xtask gen-font` が生成した。").unwrap();
    writeln!(out, "// 手で編集しないこと。").unwrap();
    writeln!(out, "//").unwrap();
    writeln!(out, "// 生成元: {source_note}").unwrap();
    writeln!(
        out,
        "// グリフデータは GNU Unifont に由来し、SIL Open Font License 1.1 の下にある。"
    )
    .unwrap();
    writeln!(
        out,
        "// 全文と出典は third_party/unifont/ を参照（COPYING / README.md）。"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "/// コードポイントと幅（セル数）の対。コードポイント昇順で、二分探索して引く。"
    )
    .unwrap();
    writeln!(
        out,
        "/// i 番目のグリフの行データは `GLYPH_ROWS[i * GLYPH_HEIGHT ..][..GLYPH_HEIGHT]`。"
    )
    .unwrap();
    writeln!(out, "pub static GLYPH_INDEX: &[(u32, u8)] = &[").unwrap();
    for glyph in glyphs {
        writeln!(
            out,
            "    (0x{:04X}, {}),",
            glyph.code_point, glyph.width_cells
        )
        .unwrap();
    }
    writeln!(out, "];").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "/// 全グリフの行データを連結したもの。各行は最上位ビットが左端のピクセル。"
    )
    .unwrap();
    // 1 グリフ 1 行に並べ、行末へ対応する符号位置を書く形は、目で読める
    // ようにするための意図的な整列である。rustfmt は 1 行の桁数だけを見て
    // これを詰め直し、グリフと符号位置の対応を壊す。生成物が
    // `cargo fmt --all -- --check` を通らない状態になるのも困るので、
    // ここだけ整形の対象から外す。
    writeln!(out, "#[rustfmt::skip]").unwrap();
    writeln!(out, "pub static GLYPH_ROWS: &[u16] = &[").unwrap();
    for glyph in glyphs {
        write!(out, "    ").unwrap();
        for (index, row) in glyph.rows.iter().enumerate() {
            if index > 0 {
                write!(out, " ").unwrap();
            }
            write!(out, "0x{row:04X},").unwrap();
        }
        writeln!(out, " // U+{:04X}", glyph.code_point).unwrap();
    }
    writeln!(out, "];").unwrap();

    out
}

/// `cargo xtask gen-font` の本体。
pub fn generate(workspace_root: &Path) -> Result<()> {
    let input_path = workspace_root
        .join("third_party")
        .join("unifont")
        .join("unifont-subset.hex");
    let output_path = workspace_root
        .join("kernel")
        .join("src")
        .join("graphics")
        .join("font")
        .join("unifont_glyphs.rs");

    let contents = fs::read_to_string(&input_path)
        .with_context(|| format!("failed to read {}", input_path.display()))?;
    let glyphs = parse_hex(&contents)
        .with_context(|| format!("failed to parse {}", input_path.display()))?;

    let half = glyphs.iter().filter(|g| g.width_cells == 1).count();
    let full = glyphs.len() - half;

    let table = render_table(&glyphs, "third_party/unifont/unifont-subset.hex");
    let parent = output_path
        .parent()
        .context("failed to resolve the output directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::write(&output_path, table)
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    let bytes = glyphs.len() * GLYPH_HEIGHT * 2;
    println!(
        "generated {}: {} glyph(s) ({} half width, {} full width), {} bytes of glyph data",
        output_path.display(),
        glyphs.len(),
        half,
        full,
        bytes
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 半角 1 セルの幅（ピクセル）。ビット位置の検証にだけ使う。
    const CELL_WIDTH: usize = 8;

    /// 上流の実データ。'A' は 8x16 の半角。
    const LATIN_CAPITAL_A: &str = "0041:0000000018242442427E424242420000";
    /// 上流の実データ。'あ' は 16x16 の全角。
    const HIRAGANA_A: &str =
        "3042:00000400020002C01F000480048007E00D101508220826082A10106000000000";

    #[test]
    fn a_half_width_glyph_is_parsed_as_one_cell() {
        let glyph = parse_line(LATIN_CAPITAL_A).unwrap().unwrap();
        assert_eq!(glyph.code_point, 0x41);
        assert_eq!(glyph.width_cells, 1);
        assert_eq!(glyph.rows.len(), GLYPH_HEIGHT);
    }

    #[test]
    fn a_full_width_glyph_is_parsed_as_two_cells() {
        let glyph = parse_line(HIRAGANA_A).unwrap().unwrap();
        assert_eq!(glyph.code_point, 0x3042);
        assert_eq!(glyph.width_cells, 2);
        assert_eq!(glyph.rows.len(), GLYPH_HEIGHT);
    }

    #[test]
    fn half_width_rows_are_shifted_to_the_most_significant_bits() {
        let glyph = parse_line(LATIN_CAPITAL_A).unwrap().unwrap();
        // 元データの 5 行目は 0x18。最上位ビット詰めなので 0x1800 になる。
        assert_eq!(glyph.rows[4], 0x1800);
        // 'A' の横棒は 0x7E。
        assert_eq!(glyph.rows[9], 0x7E00);
    }

    #[test]
    fn full_width_rows_keep_all_sixteen_bits() {
        let glyph = parse_line(HIRAGANA_A).unwrap().unwrap();
        assert_eq!(glyph.rows[0], 0x0000);
        assert_eq!(glyph.rows[1], 0x0400);
        assert_eq!(glyph.rows[4], 0x1F00);
    }

    /// 左端のピクセルが最上位ビットに来ていることを、実際の字形で確かめる。
    /// 'A' の横棒（0x7E00）は左端と右端が空いていて内側が詰まっている。
    #[test]
    fn the_most_significant_bit_is_the_leftmost_pixel() {
        let glyph = parse_line(LATIN_CAPITAL_A).unwrap().unwrap();
        let row = glyph.rows[9];
        let lit: Vec<bool> = (0..CELL_WIDTH).map(|x| row & (0x8000 >> x) != 0).collect();
        assert_eq!(
            lit,
            vec![false, true, true, true, true, true, true, false],
            "the crossbar of 'A' should be lit between both edges"
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        assert_eq!(parse_line("").unwrap(), None);
        assert_eq!(parse_line("   ").unwrap(), None);
        assert_eq!(parse_line("# a comment").unwrap(), None);
    }

    #[test]
    fn a_line_without_a_separator_is_rejected() {
        assert!(parse_line("0041 0000").is_err());
    }

    #[test]
    fn an_unexpected_bitmap_length_is_rejected() {
        assert!(parse_line("0041:00FF").is_err());
        assert!(parse_line(&format!("0041:{}", "0".repeat(48))).is_err());
    }

    #[test]
    fn a_non_hex_code_point_is_rejected() {
        assert!(parse_line("XYZW:0000000018242442427E424242420000").is_err());
    }

    #[test]
    fn a_surrogate_code_point_is_rejected() {
        // U+D800 はサロゲートであり Unicode スカラ値ではない。char へ変換できない
        // ものを通すと、kernel 側の char による検索と噛み合わなくなる。
        let line = format!("D800:{}", "0".repeat(32));
        assert!(parse_line(&line).is_err());
    }

    #[test]
    fn glyphs_are_sorted_by_code_point() {
        let input = format!("{HIRAGANA_A}\n{LATIN_CAPITAL_A}\n");
        let glyphs = parse_hex(&input).unwrap();
        assert_eq!(glyphs[0].code_point, 0x41);
        assert_eq!(glyphs[1].code_point, 0x3042);
    }

    #[test]
    fn duplicate_code_points_are_rejected() {
        let input = format!("{LATIN_CAPITAL_A}\n{LATIN_CAPITAL_A}\n");
        assert!(parse_hex(&input).is_err());
    }

    #[test]
    fn an_empty_input_is_rejected() {
        assert!(parse_hex("\n# nothing here\n").is_err());
    }

    #[test]
    fn the_rendered_table_keeps_index_and_rows_in_step() {
        let input = format!("{LATIN_CAPITAL_A}\n{HIRAGANA_A}\n");
        let glyphs = parse_hex(&input).unwrap();
        let table = render_table(&glyphs, "test");
        assert!(table.contains("(0x0041, 1),"));
        assert!(table.contains("(0x3042, 2),"));
        // 行データは 1 グリフあたり 16 個。2 グリフで 32 個。
        assert_eq!(table.matches("0x").count() - 2, glyphs.len() * GLYPH_HEIGHT);
    }
}
