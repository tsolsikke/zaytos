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
//! 不正なイメージは [`ElfError`] で返る。M2-0c の時点では信頼済みの `kernel.elf` だけを
//! 相手にしていたので、この性質は要らなかった。ユーザーの ELF を読む段階では
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
//! **配置の方針に属する検査はここに設けない。** `p_vaddr` が置いてよい範囲に
//! あるか、セグメントどうしが重なっていないか、`p_align` が妥当か、`PT_LOAD` が
//! 1 つ以上あるか、エントリポイントが `PT_LOAD` の中にあるか、はいずれも
//! 「どこへどう置くか」を決めてからでないと判定できない。ロードする側で行う。
//!
//! **この 5 件は「誰も見ていない」という意味ではない**（S9-b-3-2b で数え直した）。
//! ロードする側で実際に何が起きるかは 3 つに分かれる——既定ビルドが確かめている
//! もの、置けない場所には置けないので実質の判定があるもの、そもそも読んでいない
//! ので害が無いもの。
//! 内訳は `docs/verification-coverage.md` の「ELF の検査を 3 つに分ける」にある。
//!
//! # 区画の並びは、ページ単位で写す側のために別に確かめる
//!
//! **[`Elf::parse`] は区画の並びを見ない**（上の理由）。**ページ単位で写すローダーは、写す前に
//! [`Elf::check_load_layout`] を呼ぶ**——番地の順、重なり、ページへの切り上げのあふれ、同じページに
//! 載る区画の権限を確かめる。ユーザーのプログラムを読むカーネルが呼ぶ。`kernel.elf` を読む
//! bootloader は呼ばない（配置の方針が違う）。

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

/// `Elf64_Phdr.p_flags` の実行可のビット。
pub const PF_X: u32 = 1;
/// `Elf64_Phdr.p_flags` の書き込み可のビット。
pub const PF_W: u32 = 2;
/// `Elf64_Phdr.p_flags` の読み取り可のビット。
pub const PF_R: u32 = 4;
/// `p_flags` のうち、権限を表す 3 ビット。
const PF_PERMISSIONS: u32 = PF_X | PF_W | PF_R;

/// [`Elf::check_load_layout`] が前提にするページの大きさ（4KiB）。
pub const LOAD_PAGE_SIZE: u64 = 4096;

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

/// ページ単位で写すローダーが、区画の並びを受け付けられない理由（[`Elf::check_load_layout`]）。
///
/// `index` は `PT_LOAD` だけを数えた番号（0 から）である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// `p_memsz` が 0 の区画がある。**写すページが無く、最終ページの計算 `p_vaddr + p_memsz - 1` が
    /// 成り立たない。**
    EmptySegment { index: usize },
    /// 終わりの番地をページの境界へ切り上げると u64 を越える（`p_vaddr + p_memsz` のあふれを含む）。
    AddressOverflow { index: usize },
    /// 番地の順に並んでいない（前の区画より低い番地から始まる）。
    OutOfOrder { index: usize },
    /// 前の区画と番地が重なる。
    Overlap { index: usize },
    /// 前の区画の最終ページから始まり、権限（読み・書き・実行）が前の区画と違う。
    /// **1 枚のページは 1 つの権限しか持てない。**
    MixedPermissionsInPage { index: usize, page: u64 },
    /// 前の区画の最終ページから始まり、そのページに置くファイルの中身を持つ。
    /// **共有するページは前の区画が写したものを使うので、後ろの区画の中身を重ねる経路が無い。**
    FileDataInSharedPage { index: usize, page: u64 },
    /// 書けて、実行もできる区画がある（`p_flags` に W と X の両方が立っている。2026-10-03）。
    /// **書けるページは実行できない、という決まりで写すので、受け付けない。** Linux は、こういう区画も
    /// 読み込む——断るのは、このカーネルの決まりである。
    WritableAndExecutable { index: usize },
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
    /// 受理したイメージについては、この型の doc にある不変条件が成り立つ。
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
    /// **1 つも無いイメージも受理する。** 「ロードできる区画が無い」は、このイメージを
    /// 実行しようとする側にとっての誤りであって、バイト列としての壊れではない。
    /// 判定はロードする側で行うこと（bootloader は `kernel.elf` について
    /// 既にそうしている）。
    pub fn load_segments(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        self.program_headers().filter(|ph| ph.p_type == PT_LOAD)
    }

    /// ページ単位で写すローダーのために、`PT_LOAD` の並びを確かめる（[`check_load_layout`]）。
    pub fn check_load_layout(&self) -> Result<(), LayoutError> {
        check_load_layout(self.load_segments())
    }

    /// このセグメントに対応するファイル内容のバイトスライスを返す。
    ///
    /// # パーサーが作ったヘッダであることを前提にしない
    ///
    /// **[`ProgramHeader`] はフィールドが公開の `Copy` 型なので、誰でも直接
    /// 構築できる。** したがって「[`Self::parse`] を通った像から取り出したもの」を
    /// この関数は前提にできず、範囲は毎回確かめる。`parse` 側の検査と同じ性質を
    /// 2 箇所で守る形になるが、**どちらも単独で確かめられる**（`parse` 側は壊した
    /// イメージで、こちら側は直接構築したヘッダで）。
    pub fn segment_data(&self, ph: &ProgramHeader) -> Result<&'a [u8], ElfError> {
        Ok(&self.data[file_range(self.data.len(), ph)?])
    }
}

/// ページ単位（[`LOAD_PAGE_SIZE`]）で写すローダーのために、`PT_LOAD` の並びを確かめる（純粋な論理）。
///
/// **通った並びについて、次が成り立つ。** ローダーはこれを前提に計算してよい。
///
/// - どの区画も `p_memsz > 0` で、`p_vaddr + p_memsz` も、それをページの境界へ切り上げた値も u64 に収まる
///   （最終ページ `(p_vaddr + p_memsz - 1)` の切り下げと、ページを 1 枚ずつ進める加算があふれない）
/// - 区画は番地の順に並び、重ならない（`p_vaddr` が前の区画の終わり以上）
/// - 2 つの区画が同じページに載るのは、後ろの区画が前の区画の最終ページから始まる形だけで、そのとき
///   2 つの権限（`p_flags` の読み・書き・実行）は同じで、後ろの区画はファイルの中身を持たない
///   （`.data` の直後から `.bss` が始まる形。`ADR-0039`）
/// - 書けて実行もできる区画は無い（`p_flags` の W と X の両方を持つ区画は断る。2026-10-03）
///
/// **`p_vaddr` が置いてよい範囲に在るかは見ない**（ユーザーの範囲の外は、写すときに断られる）。
///
/// **[`ProgramHeader`] は誰でも作れるので、[`Elf::parse`] を通った値であることを前提にしない**
/// （加算は確かめてから行う）。`PT_LOAD` でないものが混ざっていても、同じ規則で見る。
pub fn check_load_layout(segments: impl Iterator<Item = ProgramHeader>) -> Result<(), LayoutError> {
    let page_mask = !(LOAD_PAGE_SIZE - 1);
    // 前の区画の（始まり・終わり・権限）。
    let mut previous: Option<(u64, u64, u32)> = None;
    for (index, ph) in segments.enumerate() {
        if ph.p_memsz == 0 {
            return Err(LayoutError::EmptySegment { index });
        }
        let end = ph
            .p_vaddr
            .checked_add(ph.p_memsz)
            .ok_or(LayoutError::AddressOverflow { index })?;
        end.checked_add(LOAD_PAGE_SIZE - 1)
            .ok_or(LayoutError::AddressOverflow { index })?;
        let permissions = ph.p_flags & PF_PERMISSIONS;
        if permissions & PF_W != 0 && permissions & PF_X != 0 {
            return Err(LayoutError::WritableAndExecutable { index });
        }
        if let Some((previous_start, previous_end, previous_permissions)) = previous {
            if ph.p_vaddr < previous_start {
                return Err(LayoutError::OutOfOrder { index });
            }
            if ph.p_vaddr < previous_end {
                return Err(LayoutError::Overlap { index });
            }
            let page = ph.p_vaddr & page_mask;
            // `previous_end` は前の区画の `p_memsz > 0` により `previous_start` より大きいので、1 を引ける。
            if (previous_end - 1) & page_mask == page {
                if permissions != previous_permissions {
                    return Err(LayoutError::MixedPermissionsInPage { index, page });
                }
                if ph.p_filesz != 0 {
                    return Err(LayoutError::FileDataInSharedPage { index, page });
                }
            }
        }
        previous = Some((ph.p_vaddr, end, permissions));
    }
    Ok(())
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
    /// 壊す側のテストは、正常なイメージを組み立ててから**このヘッダの 1 フィールドだけ**を
    /// 書き換える。壊す箇所を 1 つに限るのは、テストが落ちたときに何が拒まれたのかを
    /// 一意にするためである。
    const TEST_PHDR_OFFSET: usize = EHDR_SIZE;

    /// `Elf64_Phdr` 内のフィールドオフセット（壊すテストの書き換え先）。
    const P_OFFSET: usize = 8;
    const P_VADDR: usize = 16;
    const P_FILESZ: usize = 32;
    const P_MEMSZ: usize = 40;

    /// イメージの `at` の位置へ u64 を書き込む。
    fn patch_u64(bytes: &mut [u8], at: usize, value: u64) {
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// イメージの `at` の位置へ u16 を書き込む。
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
    /// したがって `parse` の検査を通ったイメージであることを、この関数は前提にできない
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

    /// 並びの確かめに渡す `PT_LOAD` を 1 つ作る。
    fn load(p_vaddr: u64, p_filesz: u64, p_memsz: u64, p_flags: u32) -> ProgramHeader {
        ProgramHeader {
            p_type: PT_LOAD,
            p_flags,
            p_offset: 0,
            p_vaddr,
            p_paddr: p_vaddr,
            p_filesz,
            p_memsz,
            p_align: 0x1000,
        }
    }

    const RX: u32 = PF_R | PF_X;
    const R: u32 = PF_R;
    const RW: u32 = PF_R | PF_W;
    const RWX: u32 = PF_R | PF_W | PF_X;

    /// **書けて実行もできる区画は断る**（2026-10-03）。どの位置の区画でも、読みのビットが無くても断る。
    #[test]
    fn a_segment_that_is_both_writable_and_executable_is_refused() {
        assert_eq!(
            check_load_layout([load(0x400000, 0x100, 0x100, RWX)].into_iter()),
            Err(LayoutError::WritableAndExecutable { index: 0 })
        );
        assert_eq!(
            check_load_layout(
                [
                    load(0x400000, 0x100, 0x100, RX),
                    load(0x401000, 0x100, 0x100, R),
                    load(0x402000, 0x100, 0x100, PF_W | PF_X),
                ]
                .into_iter()
            ),
            Err(LayoutError::WritableAndExecutable { index: 2 })
        );
        // 書けるだけ、実行できるだけの区画は通る。
        assert_eq!(
            check_load_layout(
                [
                    load(0x400000, 0x100, 0x100, RX),
                    load(0x401000, 0x100, 0x100, RW),
                ]
                .into_iter()
            ),
            Ok(())
        );
    }

    /// 今のユーザープログラムの形（`.text`・`.rodata`・`.data` がページの境界から始まり、`.bss` が
    /// `.data` の直後から始まる）は通る。区画が無い像も、1 つだけの像も通る。
    #[test]
    fn the_layout_check_accepts_the_shapes_the_toolchain_produces() {
        // `/bin/zi` の実測の形（2026-10-01）。
        let zi = [
            load(0x400000, 0x3904, 0x3904, RX),
            load(0x404000, 0xb68, 0xb68, R),
            load(0x405000, 0x40, 0x40, RW),
            load(0x405040, 0, 0x5018, RW),
        ];
        assert_eq!(check_load_layout(zi.into_iter()), Ok(()));
        assert_eq!(
            check_load_layout([load(0x400000, 0x42, 0x42, RX)].into_iter()),
            Ok(())
        );
        assert_eq!(check_load_layout(core::iter::empty()), Ok(()));
        // 前の区画がページの境界ちょうどで終わり、次がその境界から始まる形は、ページを共有しない。
        let touching = [
            load(0x400000, 0x1000, 0x1000, RX),
            load(0x401000, 0x10, 0x10, RW),
        ];
        assert_eq!(check_load_layout(touching.into_iter()), Ok(()));
    }

    /// 同じページに権限の違う区画が載る並びを断る。**重なってはいない**（後ろの区画は前の区画の
    /// 終わりより後ろから始まる）ので、重なりの確かめでは拾えない形である。
    #[test]
    fn the_layout_check_rejects_segments_with_different_permissions_in_one_page() {
        for (first, second) in [(RX, R), (RX, RW), (R, RW), (RW, R), (RW, RX)] {
            let segments = [
                load(0x400000, 0x42, 0x42, first),
                load(0x400050, 0, 0x12, second),
            ];
            assert_eq!(
                check_load_layout(segments.into_iter()),
                Err(LayoutError::MixedPermissionsInPage {
                    index: 1,
                    page: 0x400000
                }),
                "{first:#x} then {second:#x}"
            );
        }
        // 3 つ目が 2 つ目の最終ページに載る形も、同じ規則で断る。
        let third = [
            load(0x400000, 0x42, 0x42, RX),
            load(0x401000, 0x12, 0x12, R),
            load(0x401800, 0, 0x100, RW),
        ];
        assert_eq!(
            check_load_layout(third.into_iter()),
            Err(LayoutError::MixedPermissionsInPage {
                index: 2,
                page: 0x401000
            })
        );
        // 権限に入らないビットの違いは見ない。
        let other_bits = [
            load(0x402000, 0x8, 0x8, RW),
            load(0x402008, 0, 0x2000, RW | 0x0010_0000),
        ];
        assert_eq!(check_load_layout(other_bits.into_iter()), Ok(()));
    }

    /// 前の区画と同じページから始まる区画が、そのページにファイルの中身を持つ並びを断る
    /// （権限は同じ）。**ローダーは共有するページへ後ろの区画の中身を重ねない。**
    #[test]
    fn the_layout_check_rejects_file_data_in_a_shared_page() {
        let segments = [
            load(0x402000, 0x8, 0x8, RW),
            load(0x402008, 0x10, 0x2000, RW),
        ];
        assert_eq!(
            check_load_layout(segments.into_iter()),
            Err(LayoutError::FileDataInSharedPage {
                index: 1,
                page: 0x402000
            })
        );
    }

    /// 番地の順でない並びと、重なる並びを断る。**理由を分ける。**
    #[test]
    fn the_layout_check_rejects_segments_out_of_order_or_overlapping() {
        let reversed = [
            load(0x401000, 0x12, 0x12, R),
            load(0x400000, 0x42, 0x42, RX),
        ];
        assert_eq!(
            check_load_layout(reversed.into_iter()),
            Err(LayoutError::OutOfOrder { index: 1 })
        );
        // 前の区画の中身の内側から始まる（起動の途中の検査が使う形と同じ）。
        let inside = [
            load(0x400000, 0x42, 0x42, RX),
            load(0x400030, 0x12, 0x12, R),
        ];
        assert_eq!(
            check_load_layout(inside.into_iter()),
            Err(LayoutError::Overlap { index: 1 })
        );
        // 同じ番地から始まる。
        let same = [
            load(0x400000, 0x42, 0x42, RX),
            load(0x400000, 0x12, 0x12, RX),
        ];
        assert_eq!(
            check_load_layout(same.into_iter()),
            Err(LayoutError::Overlap { index: 1 })
        );
        // 前の区画の終わりちょうどから始まる形は重なりではない（権限が同じで中身が無ければ通る）。
        let adjacent = [load(0x402000, 0x8, 0x8, RW), load(0x402008, 0, 0x2000, RW)];
        assert_eq!(check_load_layout(adjacent.into_iter()), Ok(()));
    }

    /// 計算があふれる値を断る——大きさ 0 の区画（最終ページの引き算）、終わりの番地のあふれ、
    /// ページの境界への切り上げのあふれ。
    #[test]
    fn the_layout_check_rejects_values_whose_arithmetic_overflows() {
        // 番地 0 で大きさ 0。`p_vaddr + p_memsz - 1` が 0 - 1 になる形。
        assert_eq!(
            check_load_layout([load(0, 0, 0, R)].into_iter()),
            Err(LayoutError::EmptySegment { index: 0 })
        );
        let second_empty = [load(0x400000, 0x42, 0x42, RX), load(0x401000, 0, 0, R)];
        assert_eq!(
            check_load_layout(second_empty.into_iter()),
            Err(LayoutError::EmptySegment { index: 1 })
        );
        // `p_vaddr + p_memsz` が u64 を越える（`Elf::parse` も断るが、ここでも確かめる）。
        assert_eq!(
            check_load_layout([load(u64::MAX, 0, 2, R)].into_iter()),
            Err(LayoutError::AddressOverflow { index: 0 })
        );
        // 終わりの番地は収まるが、ページの境界へ切り上げると越える。
        assert_eq!(
            check_load_layout([load(u64::MAX - 0x800, 0, 0x100, R)].into_iter()),
            Err(LayoutError::AddressOverflow { index: 0 })
        );
        // 切り上げても収まる上限は通る（置いてよい範囲かは、ここでは見ない）。
        assert_eq!(
            check_load_layout([load(u64::MAX - 0x1FFF, 0, 0x1000, R)].into_iter()),
            Ok(())
        );
    }

    /// 像から取り出した区画で確かめる入口（[`Elf::check_load_layout`]）は、`PT_LOAD` だけを見る。
    #[test]
    fn the_layout_check_on_an_image_looks_at_its_load_segments() {
        let bytes = build_test_elf(0x100650, &[0xAA, 0xBB, 0xCC], 0x100000, 0x2000);
        assert_eq!(Elf::parse(&bytes).unwrap().check_load_layout(), Ok(()));
        // 大きさ 0 の区画を持つ像は、パースは通り、並びの確かめで断られる。
        let empty = build_test_elf(0, &[], 0, 0);
        assert_eq!(
            Elf::parse(&empty).unwrap().check_load_layout(),
            Err(LayoutError::EmptySegment { index: 0 })
        );
    }

    /// 全プログラムヘッダの走査が、受理されたイメージに対して落ちない。
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
