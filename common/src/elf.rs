//! 最小限の ELF64 パーサー（M2-0c: ELF ローダー用）。
//!
//! バイトスライスの読み取りのみで完結する純粋ロジックであり、unsafe を
//! 一切使わない。ホスト上の `cargo test` で検証する。
//! 実際のメモリ配置（ページ確保・コピー・.bss ゼロ埋め）はハードウェア
//! 依存側（bootloader）の責務とし、ここには含めない。
//!
//! # どんな入力でもパニックしない（S9-a）
//!
//! **この module の公開 API は、任意のバイト列に対してパニックしない。**
//! 不正な像は [`ElfError`] で返る。M2-0c の時点では信頼済みの `kernel.elf` だけを
//! 相手にしていたので、この性質は要らなかった。ユーザーの ELF を読む段では
//! 要る（`docs/roadmap.md` の S9「いかなる入力に対してもカーネルを fail-fast
//! させず、プロセスを終了させるエラーを返す」）。
//!
//! **守る範囲は 2 つに絞ってある。**
//!
//! - パーサー自身が落ちないこと（スライスの切り出しと加算）
//! - **パーサーの出力が呼び出し側に強いる算術が落ちないこと。** ゼロ埋めの長さ
//!   `p_memsz - p_filesz` と、終端アドレス `p_vaddr + p_memsz` がそれである。
//!   どちらもロードする側が必ず計算するので、ここで弾いておく
//!
//! **配置の方針に属する検査はここに置かない。** `p_vaddr` が置いてよい範囲に
//! あるか、セグメントどうしが重なっていないか、`p_align` が妥当か、`PT_LOAD` が
//! 1 つ以上あるか、エントリポイントが `PT_LOAD` の中にあるか、はいずれも
//! 「どこへどう置くか」を決めてからでないと判定できない。ロードする側で行う。

use core::ops::Range;

const EI_CLASS_OFFSET: usize = 4;
const EI_DATA_OFFSET: usize = 5;
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1; // リトルエンディアン
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;

/// `Elf64_Phdr.p_type` の値。ロード可能なセグメントを示す。
pub const PT_LOAD: u32 = 1;

/// Elf64_Ehdr のうち、パースに必要な部分の固定オフセット・サイズ。
const EHDR_SIZE: usize = 64;
const E_TYPE: usize = 16;
const E_MACHINE: usize = 18;
const E_ENTRY: usize = 24;
const E_PHOFF: usize = 32;
const E_PHENTSIZE: usize = 54;
const E_PHNUM: usize = 56;

/// Elf64_Phdr のバイトサイズ。
const PHDR_SIZE: usize = 56;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// ELF ヘッダ全体を読むにはデータが短すぎる。
    TooShort,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    /// `ET_EXEC` 以外（本プロジェクトの kernel は非 PIE の EXEC を想定。
    /// ADR-0009 参照）。
    NotExecutable,
    NotX86_64,
    /// プログラムヘッダテーブルがファイルの範囲外を指している。
    ProgramHeaderOutOfBounds,
    /// `e_phentsize` が `Elf64_Phdr` の大きさ（56）と違う。
    BadProgramHeaderEntrySize,
    /// セグメントのファイル内範囲 [p_offset, p_offset+p_filesz) が
    /// ファイルの範囲外を指している（加算のオーバーフローを含む）。
    SegmentFileRangeOutOfBounds,
    /// `p_memsz` が `p_filesz` より小さい。ゼロ埋めの長さが負になる。
    SegmentMemorySmallerThanFile,
    /// `p_vaddr + p_memsz` が u64 を超える。
    SegmentAddressOverflow,
}

/// パース済みの ELF64 実行ファイル。元のバイトスライスを借用するのみで、
/// コピーは行わない。
///
/// # 構築後に成り立っている不変条件
///
/// **フィールドは private で、構築できるのは [`Elf::parse`] だけである。**
/// したがってこの型の値が存在する時点で、次が成り立っている。
///
/// - `e_phentsize` が `PHDR_SIZE`（56）と等しい（だから 1 エントリの切り出しは固定長）
/// - `ph_off + PHDR_SIZE * ph_num <= data.len()`（だから末尾のエントリまで範囲内）
/// - 全プログラムヘッダについて、ファイル内範囲がファイルに収まり、
///   `p_memsz >= p_filesz` で、`p_vaddr + p_memsz` がオーバーフローしない
///
/// [`Elf::program_headers`] が添字で切り出せるのはこの不変条件による。
#[derive(Debug)]
pub struct Elf<'a> {
    data: &'a [u8],
    pub entry_point: u64,
    ph_off: usize,
    ph_num: u16,
}

/// `PT_LOAD` セグメント 1 つ分の情報。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

impl<'a> Elf<'a> {
    /// ELF64 ヘッダを検証し、プログラムヘッダテーブルの位置と中身を確認する。
    ///
    /// 受理した像については、この型の doc にある不変条件が成り立つ。
    /// **拒む理由は [`ElfError`] で区別できる。パニックはしない。**
    pub fn parse(data: &'a [u8]) -> Result<Self, ElfError> {
        if data.len() < EHDR_SIZE {
            return Err(ElfError::TooShort);
        }
        if data[0..4] != ELF_MAGIC {
            return Err(ElfError::BadMagic);
        }
        if data[EI_CLASS_OFFSET] != ELFCLASS64 {
            return Err(ElfError::NotElf64);
        }
        if data[EI_DATA_OFFSET] != ELFDATA2LSB {
            return Err(ElfError::NotLittleEndian);
        }

        let e_type = read_u16(data, E_TYPE);
        if e_type != ET_EXEC {
            return Err(ElfError::NotExecutable);
        }
        let e_machine = read_u16(data, E_MACHINE);
        if e_machine != EM_X86_64 {
            return Err(ElfError::NotX86_64);
        }

        let e_entry = read_u64(data, E_ENTRY);
        let e_phoff = read_u64(data, E_PHOFF);
        let e_phentsize = read_u16(data, E_PHENTSIZE);
        let e_phnum = read_u16(data, E_PHNUM);

        // **表の長さの検査より先に、1 エントリの大きさを確かめる。**
        // 表の長さは `e_phentsize * e_phnum` で測るので、`e_phentsize` が 56 より
        // 小さいと短く見積もられて検査を通る。読むほうは 1 エントリを 56 バイトとして
        // 切るので、後ろのエントリでファイル末尾を越える。**大きさの検査を後ろに
        // 置くと、この順序の穴がそのまま残る。**
        // ELF64 の `Elf64_Phdr` は 56 バイト固定であり、Linux の ELF ローダーも
        // `e_phentsize != sizeof(struct elf_phdr)` を拒む。同じ判定にする。
        if e_phentsize as usize != PHDR_SIZE {
            return Err(ElfError::BadProgramHeaderEntrySize);
        }

        let ph_table_len = (PHDR_SIZE as u64)
            .checked_mul(e_phnum as u64)
            .ok_or(ElfError::ProgramHeaderOutOfBounds)?;
        let ph_table_end = e_phoff
            .checked_add(ph_table_len)
            .ok_or(ElfError::ProgramHeaderOutOfBounds)?;
        if ph_table_end > data.len() as u64 {
            return Err(ElfError::ProgramHeaderOutOfBounds);
        }

        let elf = Self {
            data,
            entry_point: e_entry,
            ph_off: e_phoff as usize,
            ph_num: e_phnum,
        };
        // ここまでで切り出しは範囲内が保証されるので、走査してよい。
        elf.validate_program_headers()?;
        Ok(elf)
    }

    /// 全プログラムヘッダの中身を確かめる（[`Self::parse`] の最後の段）。
    ///
    /// **`PT_LOAD` に限らず全ヘッダを見る。** [`Self::segment_data`] は型の
    /// うえでは任意のヘッダを受け取れるので、`PT_LOAD` だけを確かめても
    /// 「受理した像から取り出したヘッダは安全」とは言えない。
    fn validate_program_headers(&self) -> Result<(), ElfError> {
        for ph in self.program_headers() {
            file_range(self.data.len(), &ph)?;
            // ゼロ埋めの長さ `p_memsz - p_filesz` が負にならないこと。
            if ph.p_memsz < ph.p_filesz {
                return Err(ElfError::SegmentMemorySmallerThanFile);
            }
            // 終端アドレス `p_vaddr + p_memsz` が u64 を超えないこと。
            ph.p_vaddr
                .checked_add(ph.p_memsz)
                .ok_or(ElfError::SegmentAddressOverflow)?;
        }
        Ok(())
    }

    /// 全プログラムヘッダを走査する。
    ///
    /// 添字での切り出しが範囲内なのは、この型の doc にある不変条件による
    /// （1 エントリ 56 バイト固定で、表全体がファイルに収まっている）。
    pub fn program_headers(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        (0..self.ph_num as usize).map(move |i| {
            let base = self.ph_off + i * PHDR_SIZE;
            let ph = &self.data[base..base + PHDR_SIZE];
            ProgramHeader {
                p_type: read_u32(ph, 0),
                p_flags: read_u32(ph, 4),
                p_offset: read_u64(ph, 8),
                p_vaddr: read_u64(ph, 16),
                p_paddr: read_u64(ph, 24),
                p_filesz: read_u64(ph, 32),
                p_memsz: read_u64(ph, 40),
                p_align: read_u64(ph, 48),
            }
        })
    }

    /// `PT_LOAD` セグメントのみを走査する。
    ///
    /// **1 つも無い像も受理する。** 「ロードできる区画が無い」は、この像を
    /// 実行しようとする側にとっての誤りであって、バイト列としての壊れではない。
    /// 判定はロードする側で行うこと（bootloader は `kernel.elf` について
    /// 既にそうしている）。
    pub fn load_segments(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        self.program_headers().filter(|ph| ph.p_type == PT_LOAD)
    }

    /// このセグメントに対応するファイル内容のバイトスライスを返す。
    ///
    /// # パーサーが作ったヘッダであることを前提にしない
    ///
    /// **[`ProgramHeader`] はフィールドが公開の `Copy` 型なので、誰でも直接
    /// 構築できる。** したがって「[`Self::parse`] を通った像から取り出したもの」を
    /// この関数は前提にできず、範囲は毎回確かめる。`parse` 側の検査と同じ性質を
    /// 2 箇所で守る形になるが、**どちらも単独で確かめられる**（`parse` 側は壊した
    /// 像で、こちら側は直接構築したヘッダで）。
    pub fn segment_data(&self, ph: &ProgramHeader) -> Result<&'a [u8], ElfError> {
        Ok(&self.data[file_range(self.data.len(), ph)?])
    }
}

/// セグメントのファイル内範囲 [p_offset, p_offset+p_filesz) を返す。
/// ファイルに収まらなければ [`ElfError::SegmentFileRangeOutOfBounds`]。
///
/// **`usize` へ落とす前に u64 のまま確かめる。** ホストテストは 32bit ホストでも
/// 走りうるので、`as usize` で切り詰めてから比べる形は使えない。
fn file_range(data_len: usize, ph: &ProgramHeader) -> Result<Range<usize>, ElfError> {
    let end = ph
        .p_offset
        .checked_add(ph.p_filesz)
        .ok_or(ElfError::SegmentFileRangeOutOfBounds)?;
    if end > data_len as u64 {
        return Err(ElfError::SegmentFileRangeOutOfBounds);
    }
    Ok(ph.p_offset as usize..end as usize)
}

// 以下 3 つは添字で切り出して `unwrap` する。範囲内であることは呼び出し側が
// 保証している。ehdr の読みは `parse` 冒頭の 64 バイト検査が、phdr の読みは
// `program_headers` が渡す 56 バイトちょうどのスライスが根拠で、どちらも
// オフセットは固定である。
fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`build_test_elf`] が置くプログラムヘッダのファイル内オフセット。
    ///
    /// 壊す側のテストは、正常な像を組み立ててから**このヘッダの 1 フィールドだけ**を
    /// 書き換える。壊す箇所を 1 つに限るのは、テストが落ちたときに何が拒まれたのかを
    /// 一意にするためである。
    const TEST_PHDR_OFFSET: usize = EHDR_SIZE;

    /// `Elf64_Phdr` 内のフィールドオフセット（壊すテストの書き換え先）。
    const P_OFFSET: usize = 8;
    const P_VADDR: usize = 16;
    const P_FILESZ: usize = 32;
    const P_MEMSZ: usize = 40;

    /// 像の `at` の位置へ u64 を書き込む。
    fn patch_u64(bytes: &mut [u8], at: usize, value: u64) {
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// 像の `at` の位置へ u16 を書き込む。
    fn patch_u16(bytes: &mut [u8], at: usize, value: u16) {
        bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    /// 最小限の ELF64 実行ファイル（ヘッダ + PT_LOAD 1 個）をテスト用に組み立てる。
    fn build_test_elf(entry: u64, segment_bytes: &[u8], vaddr: u64, memsz: u64) -> Vec<u8> {
        let mut buf = vec![0u8; EHDR_SIZE];
        buf[0..4].copy_from_slice(&ELF_MAGIC);
        buf[EI_CLASS_OFFSET] = ELFCLASS64;
        buf[EI_DATA_OFFSET] = ELFDATA2LSB;
        buf[E_TYPE..E_TYPE + 2].copy_from_slice(&ET_EXEC.to_le_bytes());
        buf[E_MACHINE..E_MACHINE + 2].copy_from_slice(&EM_X86_64.to_le_bytes());
        buf[E_ENTRY..E_ENTRY + 8].copy_from_slice(&entry.to_le_bytes());

        let phoff = buf.len() as u64;
        buf[E_PHOFF..E_PHOFF + 8].copy_from_slice(&phoff.to_le_bytes());
        buf[E_PHENTSIZE..E_PHENTSIZE + 2].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        buf[E_PHNUM..E_PHNUM + 2].copy_from_slice(&1u16.to_le_bytes());

        let seg_offset = phoff + PHDR_SIZE as u64;
        let mut phdr = [0u8; PHDR_SIZE];
        phdr[0..4].copy_from_slice(&PT_LOAD.to_le_bytes());
        phdr[4..8].copy_from_slice(&0u32.to_le_bytes()); // p_flags
        phdr[8..16].copy_from_slice(&seg_offset.to_le_bytes());
        phdr[16..24].copy_from_slice(&vaddr.to_le_bytes());
        phdr[24..32].copy_from_slice(&vaddr.to_le_bytes()); // p_paddr == p_vaddr
        phdr[32..40].copy_from_slice(&(segment_bytes.len() as u64).to_le_bytes());
        phdr[40..48].copy_from_slice(&memsz.to_le_bytes());
        phdr[48..56].copy_from_slice(&0x1000u64.to_le_bytes());
        buf.extend_from_slice(&phdr);
        buf.extend_from_slice(segment_bytes);

        buf
    }

    #[test]
    fn parses_entry_point_and_single_load_segment() {
        let bytes = build_test_elf(0x100650, &[0xAA, 0xBB, 0xCC], 0x100000, 0x2000);
        let elf = Elf::parse(&bytes).expect("should parse");

        assert_eq!(elf.entry_point, 0x100650);

        let segments: Vec<_> = elf.load_segments().collect();
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].p_vaddr, 0x100000);
        assert_eq!(segments[0].p_filesz, 3);
        assert_eq!(segments[0].p_memsz, 0x2000);
        assert_eq!(
            elf.segment_data(&segments[0])
                .expect("the range is in file"),
            &[0xAA, 0xBB, 0xCC]
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = build_test_elf(0, &[], 0, 0);
        bytes[0] = 0;
        assert_eq!(Elf::parse(&bytes).unwrap_err(), ElfError::BadMagic);
    }

    #[test]
    fn rejects_too_short_input() {
        assert_eq!(Elf::parse(&[0u8; 10]).unwrap_err(), ElfError::TooShort);
    }

    #[test]
    fn rejects_wrong_machine() {
        let mut bytes = build_test_elf(0, &[], 0x1000, 0x1000);
        bytes[E_MACHINE..E_MACHINE + 2].copy_from_slice(&3u16.to_le_bytes()); // EM_386
        assert_eq!(Elf::parse(&bytes).unwrap_err(), ElfError::NotX86_64);
    }

    #[test]
    fn rejects_program_header_table_out_of_bounds() {
        let mut bytes = build_test_elf(0, &[1, 2, 3], 0x1000, 0x1000);
        // e_phoff をファイル末尾より後ろに書き換える。
        let bad_phoff = (bytes.len() as u64) + 0x1000;
        bytes[E_PHOFF..E_PHOFF + 8].copy_from_slice(&bad_phoff.to_le_bytes());
        assert_eq!(
            Elf::parse(&bytes).unwrap_err(),
            ElfError::ProgramHeaderOutOfBounds
        );
    }

    /// `e_phentsize` が 56 より小さいと、表の長さの検査を通ったうえで
    /// 56 バイトの読みがファイル末尾を越える。
    ///
    /// **表の長さの検査だけでは足りないことを示す。** 表の長さは
    /// `e_phentsize * e_phnum` で測るので、`e_phentsize` が小さいほど短く見積もられ、
    /// 検査は通る。読むほうは 1 エントリを 56 バイトとして切るので、後ろのエントリで
    /// ファイル末尾を越える。
    #[test]
    fn rejects_a_program_header_entry_size_smaller_than_the_elf64_size() {
        let mut bytes = build_test_elf(0, &[1, 2, 3], 0x1000, 0x1000);
        patch_u16(&mut bytes, E_PHENTSIZE, 8);
        patch_u16(&mut bytes, E_PHNUM, 5);
        assert_eq!(
            Elf::parse(&bytes).unwrap_err(),
            ElfError::BadProgramHeaderEntrySize
        );
    }

    /// `e_phentsize` が 56 より大きいものも拒む。
    ///
    /// 落ちはしないが、**受理すると余った分の意味を決めることになる。**
    /// ELF64 の `Elf64_Phdr` は 56 バイト固定であり、Linux の ELF ローダーも
    /// `e_phentsize != sizeof(struct elf_phdr)` を拒む。同じ判定にする。
    #[test]
    fn rejects_a_program_header_entry_size_larger_than_the_elf64_size() {
        let mut bytes = build_test_elf(0, &[1, 2, 3], 0x1000, 0x1000);
        patch_u16(&mut bytes, E_PHENTSIZE, 64);
        // 表が 64 バイトになる分の余白を足し、表の長さの検査は通るようにする。
        bytes.extend_from_slice(&[0u8; 64]);
        assert_eq!(
            Elf::parse(&bytes).unwrap_err(),
            ElfError::BadProgramHeaderEntrySize
        );
    }

    /// セグメントのファイル内範囲がファイル末尾を越えている。
    #[test]
    fn rejects_a_segment_whose_file_range_is_out_of_bounds() {
        let mut bytes = build_test_elf(0, &[1, 2, 3], 0x1000, 0x1000);
        let past_the_end = bytes.len() as u64 + 1;
        patch_u64(&mut bytes, TEST_PHDR_OFFSET + P_OFFSET, past_the_end);
        assert_eq!(
            Elf::parse(&bytes).unwrap_err(),
            ElfError::SegmentFileRangeOutOfBounds
        );
    }

    /// セグメントのファイル内範囲の加算がオーバーフローする。
    #[test]
    fn rejects_a_segment_whose_file_range_overflows() {
        let mut bytes = build_test_elf(0, &[1, 2, 3], 0x1000, 0x1000);
        patch_u64(&mut bytes, TEST_PHDR_OFFSET + P_OFFSET, u64::MAX);
        patch_u64(&mut bytes, TEST_PHDR_OFFSET + P_FILESZ, 1);
        assert_eq!(
            Elf::parse(&bytes).unwrap_err(),
            ElfError::SegmentFileRangeOutOfBounds
        );
    }

    /// `p_memsz < p_filesz`。ゼロ埋めの長さ `p_memsz - p_filesz` が負になる。
    #[test]
    fn rejects_a_segment_whose_memory_size_is_smaller_than_its_file_size() {
        let mut bytes = build_test_elf(0, &[1, 2, 3], 0x1000, 0x1000);
        patch_u64(&mut bytes, TEST_PHDR_OFFSET + P_FILESZ, 3);
        patch_u64(&mut bytes, TEST_PHDR_OFFSET + P_MEMSZ, 2);
        assert_eq!(
            Elf::parse(&bytes).unwrap_err(),
            ElfError::SegmentMemorySmallerThanFile
        );
    }

    /// `p_vaddr + p_memsz` が u64 を超える。
    #[test]
    fn rejects_a_segment_whose_address_range_overflows() {
        let mut bytes = build_test_elf(0, &[1, 2, 3], 0x1000, 0x1000);
        patch_u64(&mut bytes, TEST_PHDR_OFFSET + P_VADDR, u64::MAX);
        patch_u64(&mut bytes, TEST_PHDR_OFFSET + P_MEMSZ, 0x1000);
        assert_eq!(
            Elf::parse(&bytes).unwrap_err(),
            ElfError::SegmentAddressOverflow
        );
    }

    /// [`Elf::segment_data`] は、パーサーが作っていない [`ProgramHeader`] に対しても
    /// 落ちない。
    ///
    /// **`ProgramHeader` はフィールドが公開の `Copy` 型なので、誰でも直接構築できる。**
    /// したがって `parse` の検査を通った像であることを、この関数は前提にできない
    /// （`syscall.rs` の `UserSlice` のように private フィールドで構築を縛る形とは
    /// 違う）。**同じ性質を 2 箇所で守っており、どちらも単独で確かめられる。**
    #[test]
    fn segment_data_rejects_a_header_that_the_parser_did_not_produce() {
        let bytes = build_test_elf(0, &[1, 2, 3], 0x1000, 0x1000);
        let elf = Elf::parse(&bytes).expect("should parse");
        let forged = ProgramHeader {
            p_type: PT_LOAD,
            p_flags: 0,
            p_offset: u64::MAX,
            p_vaddr: 0x1000,
            p_paddr: 0x1000,
            p_filesz: 1,
            p_memsz: 1,
            p_align: 0x1000,
        };
        assert_eq!(
            elf.segment_data(&forged).unwrap_err(),
            ElfError::SegmentFileRangeOutOfBounds
        );
    }

    /// 全プログラムヘッダの走査が、受理された像に対して落ちない。
    ///
    /// `parse` が置く不変条件（`e_phentsize == 56` かつ表がファイル内）が
    /// [`Elf::program_headers`] の切り出しを常に範囲内にすることの確認である。
    #[test]
    fn walking_all_program_headers_of_an_accepted_image_does_not_panic() {
        let mut bytes = build_test_elf(0, &[1, 2, 3], 0x1000, 0x1000);
        // 同じヘッダを 3 本並べ、末尾のエントリまで切り出せることを見る。
        let phdr: Vec<u8> = bytes[TEST_PHDR_OFFSET..TEST_PHDR_OFFSET + PHDR_SIZE].to_vec();
        let mut rebuilt = bytes[..TEST_PHDR_OFFSET].to_vec();
        for _ in 0..3 {
            rebuilt.extend_from_slice(&phdr);
        }
        rebuilt.extend_from_slice(&bytes[TEST_PHDR_OFFSET + PHDR_SIZE..]);
        bytes = rebuilt;
        patch_u16(&mut bytes, E_PHNUM, 3);
        // セグメント本体の位置が 2 本分ずれるので、p_offset を実際の位置へ直す。
        let segment_offset = (TEST_PHDR_OFFSET + PHDR_SIZE * 3) as u64;
        for i in 0..3 {
            patch_u64(
                &mut bytes,
                TEST_PHDR_OFFSET + PHDR_SIZE * i + P_OFFSET,
                segment_offset,
            );
        }

        let elf = Elf::parse(&bytes).expect("should parse");
        assert_eq!(elf.program_headers().count(), 3);
        for ph in elf.load_segments() {
            assert_eq!(
                elf.segment_data(&ph).expect("the range is in file"),
                &[1, 2, 3]
            );
        }
    }
}
