//! 最小限の ELF64 パーサー（M2-0c: ELF ローダー用）。
//!
//! バイトスライスの読み取りのみで完結する純粋ロジックであり、unsafe を
//! 一切使わない。ホスト上の `cargo test` で検証する。
//! 実際のメモリ配置（ページ確保・コピー・.bss ゼロ埋め）はハードウェア
//! 依存側（bootloader）の責務とし、ここには含めない。

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
}

/// パース済みの ELF64 実行ファイル。元のバイトスライスを借用するのみで、
/// コピーは行わない。
#[derive(Debug)]
pub struct Elf<'a> {
    data: &'a [u8],
    pub entry_point: u64,
    ph_off: usize,
    ph_num: u16,
    ph_entsize: usize,
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
    /// ELF64 ヘッダを検証し、プログラムヘッダテーブルの位置を確認する。
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

        let ph_table_len = (e_phentsize as u64)
            .checked_mul(e_phnum as u64)
            .ok_or(ElfError::ProgramHeaderOutOfBounds)?;
        let ph_table_end = e_phoff
            .checked_add(ph_table_len)
            .ok_or(ElfError::ProgramHeaderOutOfBounds)?;
        if ph_table_end > data.len() as u64 {
            return Err(ElfError::ProgramHeaderOutOfBounds);
        }

        Ok(Self {
            data,
            entry_point: e_entry,
            ph_off: e_phoff as usize,
            ph_num: e_phnum,
            ph_entsize: e_phentsize as usize,
        })
    }

    /// 全プログラムヘッダを走査する。
    pub fn program_headers(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        (0..self.ph_num as usize).map(move |i| {
            let base = self.ph_off + i * self.ph_entsize;
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
    pub fn load_segments(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        self.program_headers().filter(|ph| ph.p_type == PT_LOAD)
    }

    /// このセグメントに対応するファイル内容のバイトスライスを返す。
    pub fn segment_data(&self, ph: &ProgramHeader) -> &'a [u8] {
        let start = ph.p_offset as usize;
        let end = start + ph.p_filesz as usize;
        &self.data[start..end]
    }
}

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
        assert_eq!(elf.segment_data(&segments[0]), &[0xAA, 0xBB, 0xCC]);
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
}
