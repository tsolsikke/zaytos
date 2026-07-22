//! ビットマップフォントのグリフ検索（M3-b）。
//!
//! グリフデータは GNU Unifont に由来する（SIL Open Font License 1.1、
//! 出典と全文は `third_party/unifont/` を参照）。テーブルは
//! `cargo xtask gen-font` が [`unifont_glyphs`] へ生成する。
//!
//! 検索は `char` を鍵にする。`u8` ではなく `char` を入口にしておくことで、
//! 日本語を追加する際にこのインターフェースを変えずに済む。
//!
//! 半角（8x16）と全角（16x16）を最初から区別する。日本語の文字は全角で、
//! コンソール上で 2 セル分の幅を占める。これを後から入れると桁計算・折り返し・
//! カーソル移動を全て見直すことになるため、ASCII しか収録していない現時点から
//! 幅を持つ設計にしてある（ADR-0016）。
//!
//! 生ポインタを使わない純粋ロジックであり、ホスト上の `cargo test` で検証する。

mod unifont_glyphs;

use unifont_glyphs::{GLYPH_INDEX, GLYPH_ROWS};

/// 1 グリフの行数。半角・全角どちらも 16 行。
pub const GLYPH_HEIGHT: u32 = 16;
/// 半角 1 セルの幅（ピクセル）。全角はこの 2 倍。
pub const CELL_WIDTH: u32 = 8;
/// 収録されていない文字の代わりに描くもの（U+FFFD REPLACEMENT CHARACTER）。
pub const REPLACEMENT_CHARACTER: char = '\u{FFFD}';

/// 1 文字分のビットマップ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Glyph {
    rows: &'static [u16],
    width_cells: u8,
}

impl Glyph {
    /// 占めるセル数。半角なら 1、全角なら 2。
    pub fn width_cells(&self) -> u32 {
        self.width_cells as u32
    }

    /// 幅（ピクセル）。
    pub fn width_pixels(&self) -> u32 {
        CELL_WIDTH * self.width_cells()
    }

    /// 高さ（ピクセル）。
    pub fn height_pixels(&self) -> u32 {
        GLYPH_HEIGHT
    }

    /// グリフ内の座標 `(x, y)` のピクセルが立っているか。
    ///
    /// 範囲外は `false` を返す。panic しないのは、描画経路から panic を
    /// 出さないため（ADR-0013）。
    pub fn is_set(&self, x: u32, y: u32) -> bool {
        if x >= self.width_pixels() || y >= GLYPH_HEIGHT {
            return false;
        }
        let Some(row) = self.rows.get(y as usize) else {
            return false;
        };
        // 行データは最上位ビットが左端。半角も上位詰めで格納してあるため、
        // 半角と全角で同じ式が使える。
        row & (0x8000u16 >> x) != 0
    }
}

/// 収録されているグリフのうち、最も広いものが占めるセル数。
///
/// コンソールの格子は、これを収められる桁数を持つ必要がある。両者が
/// 食い違っていないことは `console` のテストで検証している。
pub fn max_width_cells() -> u32 {
    GLYPH_INDEX
        .iter()
        .map(|&(_, width_cells)| width_cells as u32)
        .max()
        .unwrap_or(1)
}

/// `c` に対応するグリフを引く。収録されていなければ `None`。
pub fn lookup(c: char) -> Option<Glyph> {
    let code_point = c as u32;
    let position = GLYPH_INDEX
        .binary_search_by_key(&code_point, |&(indexed, _)| indexed)
        .ok()?;
    let (_, width_cells) = GLYPH_INDEX[position];

    let start = position * GLYPH_HEIGHT as usize;
    let end = start + GLYPH_HEIGHT as usize;
    // 生成器が index と rows を対応させているが、崩れていた場合に範囲外
    // インデックスで panic させず、収録なしとして扱う。
    let rows = GLYPH_ROWS.get(start..end)?;

    Some(Glyph { rows, width_cells })
}

/// `c` に対応するグリフを引く。収録されていなければ代替グリフを返す。
///
/// 描画側が「必ず何か描ける」ようにするための入口。代替グリフすら引けない
/// 場合は空白の半角グリフを返し、決して panic しない。
pub fn glyph(c: char) -> Glyph {
    const BLANK_ROWS: [u16; GLYPH_HEIGHT as usize] = [0; GLYPH_HEIGHT as usize];
    lookup(c)
        .or_else(|| lookup(REPLACEMENT_CHARACTER))
        .unwrap_or(Glyph {
            rows: &BLANK_ROWS,
            width_cells: 1,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_index_and_row_data_stay_in_step() {
        // 生成物が壊れていれば、他の全てのテストの前提が崩れる。
        assert_eq!(GLYPH_ROWS.len(), GLYPH_INDEX.len() * GLYPH_HEIGHT as usize);
        assert!(!GLYPH_INDEX.is_empty());
    }

    #[test]
    fn the_index_is_sorted_so_binary_search_is_valid() {
        for pair in GLYPH_INDEX.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "GLYPH_INDEX must be strictly ascending: {:#x} then {:#x}",
                pair[0].0,
                pair[1].0
            );
        }
    }

    #[test]
    fn every_entry_has_a_sane_width() {
        for &(code_point, width_cells) in GLYPH_INDEX {
            assert!(
                width_cells == 1 || width_cells == 2,
                "U+{code_point:04X} has width {width_cells}"
            );
        }
    }

    #[test]
    fn the_printable_ascii_range_is_present() {
        for code_point in 0x20u32..=0x7E {
            let c = char::from_u32(code_point).unwrap();
            assert!(lookup(c).is_some(), "U+{code_point:04X} should be present");
        }
    }

    #[test]
    fn ascii_glyphs_are_half_width() {
        assert_eq!(lookup('A').unwrap().width_cells(), 1);
        assert_eq!(lookup('A').unwrap().width_pixels(), 8);
    }

    /// 'A' の字形を実際に読み出して、ビットの並びが左右反転していないことを
    /// 確かめる。ここを間違えると全ての文字が鏡像になる。
    #[test]
    fn the_shape_of_capital_a_is_upright_and_not_mirrored() {
        let glyph = lookup('A').unwrap();
        // 横棒の行。左端と右端が空いていて内側が詰まっている。
        let crossbar: [bool; 8] = core::array::from_fn(|x| glyph.is_set(x as u32, 9));
        assert_eq!(crossbar, [false, true, true, true, true, true, true, false]);
        // 頂点の行。中央 2 ピクセルだけ立っている。
        let apex: [bool; 8] = core::array::from_fn(|x| glyph.is_set(x as u32, 4));
        assert_eq!(apex, [false, false, false, true, true, false, false, false]);
    }

    #[test]
    fn a_space_has_no_lit_pixels() {
        let glyph = lookup(' ').unwrap();
        for y in 0..GLYPH_HEIGHT {
            for x in 0..glyph.width_pixels() {
                assert!(!glyph.is_set(x, y), "space should be blank at ({x}, {y})");
            }
        }
    }

    #[test]
    fn out_of_range_coordinates_are_not_set() {
        let glyph = lookup('A').unwrap();
        assert!(!glyph.is_set(glyph.width_pixels(), 0));
        assert!(!glyph.is_set(0, GLYPH_HEIGHT));
        assert!(!glyph.is_set(u32::MAX, u32::MAX));
    }

    #[test]
    fn an_unlisted_character_falls_back_to_the_replacement_glyph() {
        // 日本語はまだ収録していない。収録するまでは代替グリフになる。
        assert_eq!(lookup('あ'), None);
        assert_eq!(glyph('あ'), lookup(REPLACEMENT_CHARACTER).unwrap());
    }

    #[test]
    fn the_replacement_glyph_itself_is_available() {
        assert!(lookup(REPLACEMENT_CHARACTER).is_some());
    }

    #[test]
    fn glyph_never_fails_for_any_character() {
        // 制御文字・非 BMP・未収録のいずれでも、何かしら返って panic しない。
        for c in ['\0', '\n', '\u{7F}', '\u{10FFFF}', '絵', '\u{1F600}'] {
            let glyph = glyph(c);
            assert!(glyph.width_cells() == 1 || glyph.width_cells() == 2);
            assert_eq!(glyph.height_pixels(), GLYPH_HEIGHT);
        }
    }
}
