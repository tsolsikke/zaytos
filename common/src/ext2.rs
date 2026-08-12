//! 最小限の ext2 リーダー（S10-a）。
//!
//! バイトスライスの読み取りのみで完結する純粋ロジックであり、unsafe を
//! 一切使わない。ホスト上の `cargo test` で検証する。**像をどこから持って
//! くるかは呼び出し側の責務**で、ここには含めない（S10-a では
//! `kernel/build.rs` が `mke2fs` で建てた像を `include_bytes!` で抱える）。
//!
//! # どんな入力でもパニックしない
//!
//! **この module の公開 API は、任意のバイト列に対してパニックしない。**
//! 不正な像は [`Ext2Error`] で返る。`docs/roadmap.md` の S10 が
//! 「壊したイメージでカーネルメモリを壊さずエラーを返すこと」を求めている。
//!
//! # 守る範囲は 4 つに分けてある
//!
//! `common::elf` は 2 つ（パーサー自身が落ちない / パーサーの出力が呼び出し側に
//! 強いる算術が落ちない）だった。**ext2 では 2 つでは足りない。**
//!
//! - **線1: パーサー自身が落ちないこと。** 切り出しと加算
//! - **線2: パーサーの出力が呼び出し側に強いる算術が落ちないこと。**
//!   ext2 では `block * block_size`（ブロック番号からバイト位置を出す）と、
//!   ブロックサイズそのもの（`1024 << s_log_block_size` が桁あふれする）
//! - **線3: 参照が像の外を指さないこと。** ext2 は「番号」で他所を指す形が
//!   ELF より多い——ブロック番号、inode 番号、group descriptor が持つ 3 つの
//!   ブロック番号。**番号だけを見ても妥当性が分からず、像の大きさと突き合わせて
//!   初めて分かる**
//! - **線4: 走査が止まること。** ディレクトリエントリの `rec_len` が 0 だと
//!   走査が進まない。**ACPI の MADT で同じ形を踏んでいる**（エントリ長 0。
//!   `acpi-test-zero-entry-length`）。**S10-a のディレクトリの刻みで当たる**
//!
//! # 扱わないもの
//!
//! **読み取りに要らないものは見ない。** ビットマップと実際の使用状況の矛盾
//! （`e2fsck` の仕事である）、チェックサム（ext2 に無い。ext4 の機能）、
//! バックアップ superblock との突き合わせ、`s_state` が clean でないこと
//! （**読み取りは拒まない。Linux も読み取り専用マウントは許す**）。

/// ext2 の magic（`s_magic`）。
const EXT2_MAGIC: u16 = 0xEF53;

/// superblock の像内オフセット。**ブロックサイズに依らず 1024 で固定である。**
const SUPERBLOCK_OFFSET: usize = 1024;

/// superblock のうち、この module が読む範囲。
const SUPERBLOCK_MIN_LEN: usize = 104;

/// group descriptor 1 つのバイト数（ext2。ext4 の 64 バイトではない）。
pub const GROUP_DESCRIPTOR_SIZE: usize = 32;

/// 受理する `s_rev_level`。
///
/// **rev 0 を受理しない。** あちらは `s_inode_size` を持たず 128 固定で、
/// `s_first_ino` も 11 固定である。**`mke2fs` の既定は rev 1 なので、
/// 受理する形を 1 つに絞る**（`docs/roadmap.md` の S10）。
const EXT2_DYNAMIC_REV: u32 = 1;

/// 実装が理解している INCOMPAT の機能ビット。
///
/// **`FILETYPE` だけである。** ディレクトリエントリが `file_type` を持つ形で、
/// `mke2fs` の既定に入っている（実測で `s_feature_incompat = 0x02`）。
pub const INCOMPAT_FILETYPE: u32 = 0x0002;

/// 理解している INCOMPAT ビットの全体。**ここに無いビットが立っていたら拒む。**
const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ext2Error {
    /// superblock を読むにはデータが短すぎる（線1）。
    TooShort,
    /// `s_magic` が `0xEF53` でない。
    BadMagic,
    /// `s_rev_level` が 1 でない。
    UnsupportedRevision(u32),
    /// `s_log_block_size` が大きすぎて、ブロックサイズが桁あふれする（線2）。
    BadBlockSizeShift(u32),
    /// `s_inode_size` が 128 未満、または 2 の冪でない、またはブロックより大きい。
    BadInodeSize(u16),
    /// `s_inodes_per_group` か `s_blocks_per_group` が 0（**0 除算になる**。線2）。
    ZeroPerGroup,
    /// 理解していない INCOMPAT の機能ビットが立っている。
    ///
    /// **RO_COMPAT と COMPAT の未知ビットでは拒まない。** 読み取りは
    /// 前者を無視してよく、後者は常に無視してよい。**この非対称は Linux と
    /// 同じ規則である。**
    UnsupportedIncompatFeatures(u32),
    /// ファイルシステムが名乗る大きさが、像より大きい（線3）。
    ImageTooSmall { needed: u64, actual: u64 },
    /// ブロック番号が `s_blocks_count` の外を指している（線3）。
    BlockOutOfRange(u32),
    /// group descriptor テーブルが像の外へ出る（線3）。
    GroupDescriptorsOutOfRange,
}

/// 受理した ext2 の像。**元のバイトスライスを借用するのみで、コピーしない。**
///
/// # 構築後に成り立っている不変条件
///
/// - `block_size` は 1024..=65536 の 2 の冪で、`block_size * blocks_count` が
///   像の長さ以下である
/// - `blocks_per_group` と `inodes_per_group` は 0 でない
/// - INCOMPAT の未知ビットが立っていない
/// - **group descriptor テーブル全体が像の中にある**
pub struct Ext2<'a> {
    image: &'a [u8],
    block_size: u32,
    blocks_count: u32,
    inodes_count: u32,
    first_data_block: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    inode_size: u16,
    first_inode: u32,
    feature_compat: u32,
    feature_incompat: u32,
    feature_ro_compat: u32,
    group_count: u32,
}

/// **`Debug` は手で書く。** `derive` すると像そのもの（2 MiB）が
/// `unwrap_err` の診断へ出る。**出したいのは形であって中身ではない。**
impl core::fmt::Debug for Ext2<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ext2")
            .field("image_len", &self.image.len())
            .field("block_size", &self.block_size)
            .field("blocks_count", &self.blocks_count)
            .field("inodes_count", &self.inodes_count)
            .field("inode_size", &self.inode_size)
            .field("group_count", &self.group_count)
            .finish()
    }
}

/// group descriptor 1 つ分（読むのは 3 つのブロック番号だけ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockGroupDescriptor {
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
}

impl<'a> Ext2<'a> {
    /// superblock を検証し、group descriptor テーブルが像に収まることまで確かめる。
    ///
    /// **拒む理由は [`Ext2Error`] で区別できる。パニックはしない。**
    pub fn parse(image: &'a [u8]) -> Result<Self, Ext2Error> {
        // 線1: superblock を読める長さがあるか。
        let sb_end = SUPERBLOCK_OFFSET
            .checked_add(SUPERBLOCK_MIN_LEN)
            .ok_or(Ext2Error::TooShort)?;
        if image.len() < sb_end {
            return Err(Ext2Error::TooShort);
        }
        let sb = &image[SUPERBLOCK_OFFSET..sb_end];

        if read_u16(sb, 56) != EXT2_MAGIC {
            return Err(Ext2Error::BadMagic);
        }
        let rev = read_u32(sb, 76);
        if rev != EXT2_DYNAMIC_REV {
            return Err(Ext2Error::UnsupportedRevision(rev));
        }

        // 線2: ブロックサイズは `1024 << s_log_block_size` である。
        // **シフト量を先に見る。** 見ないと桁あふれする。
        let shift = read_u32(sb, 24);
        if shift > 6 {
            return Err(Ext2Error::BadBlockSizeShift(shift));
        }
        let block_size = 1024u32 << shift;

        let inode_size = read_u16(sb, 88);
        if inode_size < 128 || !inode_size.is_power_of_two() || u32::from(inode_size) > block_size {
            return Err(Ext2Error::BadInodeSize(inode_size));
        }

        let feature_incompat = read_u32(sb, 96);
        let unsupported = feature_incompat & !INCOMPAT_SUPPORTED;
        if unsupported != 0 {
            return Err(Ext2Error::UnsupportedIncompatFeatures(unsupported));
        }

        let blocks_count = read_u32(sb, 4);
        let inodes_count = read_u32(sb, 0);
        let blocks_per_group = read_u32(sb, 32);
        let inodes_per_group = read_u32(sb, 40);
        if blocks_per_group == 0 || inodes_per_group == 0 {
            return Err(Ext2Error::ZeroPerGroup);
        }

        // 線3: 名乗った大きさが像に収まるか。**u64 で掛ける**（u32 では溢れる）。
        let needed = u64::from(blocks_count) * u64::from(block_size);
        if needed > image.len() as u64 {
            return Err(Ext2Error::ImageTooSmall {
                needed,
                actual: image.len() as u64,
            });
        }

        let first_data_block = read_u32(sb, 20);
        if first_data_block >= blocks_count {
            return Err(Ext2Error::BlockOutOfRange(first_data_block));
        }

        let group_count = blocks_count
            .saturating_sub(first_data_block)
            .div_ceil(blocks_per_group);

        let ext2 = Self {
            image,
            block_size,
            blocks_count,
            inodes_count,
            first_data_block,
            blocks_per_group,
            inodes_per_group,
            inode_size,
            first_inode: read_u32(sb, 84),
            feature_compat: read_u32(sb, 92),
            feature_incompat,
            feature_ro_compat: read_u32(sb, 100),
            group_count,
        };

        // 線3: group descriptor テーブル全体が像の中にあるか。
        // **テーブルは superblock の次のブロックから始まる。**
        let table_start = u64::from(first_data_block + 1) * u64::from(block_size);
        let table_len = u64::from(group_count) * GROUP_DESCRIPTOR_SIZE as u64;
        let table_end = table_start
            .checked_add(table_len)
            .ok_or(Ext2Error::GroupDescriptorsOutOfRange)?;
        if table_end > image.len() as u64 {
            return Err(Ext2Error::GroupDescriptorsOutOfRange);
        }

        Ok(ext2)
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }
    pub fn blocks_count(&self) -> u32 {
        self.blocks_count
    }
    pub fn inodes_count(&self) -> u32 {
        self.inodes_count
    }
    pub fn first_data_block(&self) -> u32 {
        self.first_data_block
    }
    pub fn blocks_per_group(&self) -> u32 {
        self.blocks_per_group
    }
    pub fn inodes_per_group(&self) -> u32 {
        self.inodes_per_group
    }
    pub fn inode_size(&self) -> u16 {
        self.inode_size
    }
    pub fn first_inode(&self) -> u32 {
        self.first_inode
    }
    pub fn feature_compat(&self) -> u32 {
        self.feature_compat
    }
    pub fn feature_incompat(&self) -> u32 {
        self.feature_incompat
    }
    pub fn feature_ro_compat(&self) -> u32 {
        self.feature_ro_compat
    }
    pub fn group_count(&self) -> u32 {
        self.group_count
    }

    /// ブロック 1 つ分のバイト列を**借りて返す**（S10-a）。
    ///
    /// # なぜコピーしないか
    ///
    /// **像は既に RAM にあり、読み取り専用で、寿命が `'static` である**
    /// （カーネルは `include_bytes!` で `.rodata` に抱える）。**コピーする形
    /// （`read_block(&self, block, dst)`）にすると、コピーを 1 つ増やすだけに
    /// なる。**
    ///
    /// **借りて返す形が成立するのは、像が RAM 上にあるからである。実デバイスは
    /// 要求してから届くので、S13（永続ブロックストレージ）では
    /// `read_block` の形になる。****そこが trait を引く境界である。**
    pub fn block_bytes(&self, block: u32) -> Result<&'a [u8], Ext2Error> {
        if block >= self.blocks_count {
            return Err(Ext2Error::BlockOutOfRange(block));
        }
        // 線2: ここが「呼び出し側に強いる算術」である。**u64 で計算する。**
        let start = u64::from(block) * u64::from(self.block_size);
        let end = start + u64::from(self.block_size);
        if end > self.image.len() as u64 {
            return Err(Ext2Error::BlockOutOfRange(block));
        }
        Ok(&self.image[start as usize..end as usize])
    }

    /// group descriptor を 1 つ読む。**3 つのブロック番号が像の外を指していない
    /// ことまで確かめる**（線3）。
    pub fn group_descriptor(&self, group: u32) -> Result<BlockGroupDescriptor, Ext2Error> {
        if group >= self.group_count {
            return Err(Ext2Error::GroupDescriptorsOutOfRange);
        }
        let table_start = u64::from(self.first_data_block + 1) * u64::from(self.block_size);
        let offset = table_start + u64::from(group) * GROUP_DESCRIPTOR_SIZE as u64;
        let end = offset + GROUP_DESCRIPTOR_SIZE as u64;
        if end > self.image.len() as u64 {
            return Err(Ext2Error::GroupDescriptorsOutOfRange);
        }
        let raw = &self.image[offset as usize..end as usize];

        let descriptor = BlockGroupDescriptor {
            block_bitmap: read_u32(raw, 0),
            inode_bitmap: read_u32(raw, 4),
            inode_table: read_u32(raw, 8),
        };
        for block in [
            descriptor.block_bitmap,
            descriptor.inode_bitmap,
            descriptor.inode_table,
        ] {
            if block >= self.blocks_count {
                return Err(Ext2Error::BlockOutOfRange(block));
            }
        }
        Ok(descriptor)
    }
}

// 添字で切り出して `unwrap` する。範囲内であることは呼び出し側が保証している
// （`parse` 冒頭の長さ検査と、`group_descriptor` が渡す 32 バイトちょうどの
// スライスが根拠で、どちらもオフセットは固定である）。
fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実測した `mke2fs 1.47.0` の既定に合わせた像を組み立てる。
    ///
    /// 2 MiB・ブロック 4096・inode 256・rev 1・INCOMPAT は FILETYPE だけ。
    /// **`kernel/build.rs` が建てる像と同じ寸法にしてある**（`dumpe2fs` の実測。
    /// 512 ブロック・256 inode）。**中身は superblock と group descriptor だけで、
    /// 残りはゼロである**（この段が読むのはそこまでである）。
    fn build_test_image() -> std::vec::Vec<u8> {
        const IMAGE_LEN: usize = 2 * 1024 * 1024;
        let mut image = std::vec![0u8; IMAGE_LEN];
        write_superblock(&mut image);
        // group 0 の descriptor（block bitmap 2 / inode bitmap 3 / inode table 4）。
        let table = 4096;
        image[table..table + 4].copy_from_slice(&2u32.to_le_bytes());
        image[table + 4..table + 8].copy_from_slice(&3u32.to_le_bytes());
        image[table + 8..table + 12].copy_from_slice(&4u32.to_le_bytes());
        image
    }

    fn write_superblock(image: &mut [u8]) {
        let put32 = |image: &mut [u8], offset: usize, value: u32| {
            image[SUPERBLOCK_OFFSET + offset..SUPERBLOCK_OFFSET + offset + 4]
                .copy_from_slice(&value.to_le_bytes());
        };
        let put16 = |image: &mut [u8], offset: usize, value: u16| {
            image[SUPERBLOCK_OFFSET + offset..SUPERBLOCK_OFFSET + offset + 2]
                .copy_from_slice(&value.to_le_bytes());
        };
        put32(image, 0, 256); // s_inodes_count
        put32(image, 4, 512); // s_blocks_count
        put32(image, 20, 0); // s_first_data_block
        put32(image, 24, 2); // s_log_block_size -> 4096
        put32(image, 32, 32768); // s_blocks_per_group
        put32(image, 40, 256); // s_inodes_per_group
        put16(image, 56, EXT2_MAGIC);
        put32(image, 76, EXT2_DYNAMIC_REV);
        put32(image, 84, 11); // s_first_ino
        put16(image, 88, 256); // s_inode_size
        put32(image, 92, 0x38); // COMPAT: dir_index | resize_inode | ext_attr
        put32(image, 96, INCOMPAT_FILETYPE);
        put32(image, 100, 0x03); // RO_COMPAT: sparse_super | large_file
    }

    #[test]
    fn accepts_an_image_shaped_like_mke2fs_defaults() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).expect("the default-shaped image is accepted");
        assert_eq!(fs.block_size(), 4096);
        assert_eq!(fs.inode_size(), 256);
        assert_eq!(fs.first_inode(), 11);
        assert_eq!(fs.blocks_count(), 512);
        assert_eq!(fs.group_count(), 1);
        assert_eq!(fs.feature_incompat(), INCOMPAT_FILETYPE);
    }

    #[test]
    fn reads_the_group_descriptor_of_group_zero() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(
            fs.group_descriptor(0).unwrap(),
            BlockGroupDescriptor {
                block_bitmap: 2,
                inode_bitmap: 3,
                inode_table: 4,
            }
        );
        assert_eq!(
            fs.group_descriptor(1),
            Err(Ext2Error::GroupDescriptorsOutOfRange)
        );
    }

    /// 線1: どんな短さでもパニックしない。
    #[test]
    fn any_prefix_is_rejected_without_panicking() {
        let image = build_test_image();
        for len in 0..2048 {
            assert!(Ext2::parse(&image[..len]).is_err());
        }
    }

    #[test]
    fn rejects_a_bad_magic() {
        let mut image = build_test_image();
        image[SUPERBLOCK_OFFSET + 56] = 0;
        assert_eq!(Ext2::parse(&image).unwrap_err(), Ext2Error::BadMagic);
    }

    #[test]
    fn rejects_revision_zero() {
        let mut image = build_test_image();
        image[SUPERBLOCK_OFFSET + 76..SUPERBLOCK_OFFSET + 80].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            Ext2::parse(&image).unwrap_err(),
            Ext2Error::UnsupportedRevision(0)
        );
    }

    /// 線2: `1024 << shift` が桁あふれする値を拒む。
    #[test]
    fn rejects_a_block_size_shift_that_would_overflow() {
        let mut image = build_test_image();
        image[SUPERBLOCK_OFFSET + 24..SUPERBLOCK_OFFSET + 28].copy_from_slice(&31u32.to_le_bytes());
        assert_eq!(
            Ext2::parse(&image).unwrap_err(),
            Ext2Error::BadBlockSizeShift(31)
        );
    }

    #[test]
    fn rejects_an_inode_size_that_is_not_a_power_of_two() {
        let mut image = build_test_image();
        image[SUPERBLOCK_OFFSET + 88..SUPERBLOCK_OFFSET + 90]
            .copy_from_slice(&200u16.to_le_bytes());
        assert_eq!(
            Ext2::parse(&image).unwrap_err(),
            Ext2Error::BadInodeSize(200)
        );
    }

    /// **未知の INCOMPAT は拒み、未知の RO_COMPAT と COMPAT は受理する。**
    #[test]
    fn rejects_unknown_incompat_but_accepts_unknown_ro_compat_and_compat() {
        let mut image = build_test_image();
        image[SUPERBLOCK_OFFSET + 96..SUPERBLOCK_OFFSET + 100]
            .copy_from_slice(&(INCOMPAT_FILETYPE | 0x40).to_le_bytes());
        assert_eq!(
            Ext2::parse(&image).unwrap_err(),
            Ext2Error::UnsupportedIncompatFeatures(0x40)
        );

        let mut image = build_test_image();
        image[SUPERBLOCK_OFFSET + 100..SUPERBLOCK_OFFSET + 104]
            .copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        image[SUPERBLOCK_OFFSET + 92..SUPERBLOCK_OFFSET + 96]
            .copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(Ext2::parse(&image).is_ok());
    }

    #[test]
    fn rejects_zero_per_group() {
        let mut image = build_test_image();
        image[SUPERBLOCK_OFFSET + 32..SUPERBLOCK_OFFSET + 36].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(Ext2::parse(&image).unwrap_err(), Ext2Error::ZeroPerGroup);
    }

    /// 線3: 名乗った大きさが像より大きい。
    #[test]
    fn rejects_an_image_smaller_than_the_filesystem_claims() {
        let mut image = build_test_image();
        image[SUPERBLOCK_OFFSET + 4..SUPERBLOCK_OFFSET + 8]
            .copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(matches!(
            Ext2::parse(&image).unwrap_err(),
            Ext2Error::ImageTooSmall { .. }
        ));
    }

    /// 線3: group descriptor の 3 つのブロック番号が像の外を指す。
    #[test]
    fn rejects_a_group_descriptor_pointing_outside_the_filesystem() {
        let mut image = build_test_image();
        image[4096 + 8..4096 + 12].copy_from_slice(&9999u32.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(
            fs.group_descriptor(0),
            Err(Ext2Error::BlockOutOfRange(9999))
        );
    }

    #[test]
    fn block_bytes_borrows_and_rejects_out_of_range() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(fs.block_bytes(0).unwrap().len(), 4096);
        assert_eq!(fs.block_bytes(512), Err(Ext2Error::BlockOutOfRange(512)));
    }
}
