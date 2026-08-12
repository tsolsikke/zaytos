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
//!
//! **穴（sparse file）も扱わない。** ここは「読み取りに要らない」からではなく、
//! **借りて返す形の帰結である**（[`Ext2::block_bytes`]）。穴に対して返すべき
//! ゼロのバイト列が像の中に無いので、借りようがない。[`Ext2Error::SparseBlock`]
//! で拒む。`mke2fs -d` は穴を作らないので、この段の像には現れない。
//!
//! **扱う条件は「穴を持つ像を読む必要が生じたとき」である。** 書き込みを実装すると
//! **自分で穴を作れるようになる**ので、そこで発火しうる。**両立させる案は 2 つあり、
//! どちらも今は要らない**——`common` に 1 ブロックぶんのゼロを静的に置いて借りる形と、
//! 戻り値を「借りたバイト列」か「長さだけの穴」かの列挙にする形である。
//! **要らないものを先回りで置かない**（`docs/vision.md`）。

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

/// inode の標準部のバイト数。**`s_inode_size` が 256 でも、こちらが読むのは
/// 先頭の 128 バイトだけである**（追加領域は `i_crtime` などで、読み取りに要らない）。
const INODE_CORE_LEN: usize = 128;

/// `i_block` の要素数（直接 12 + 単一間接 + 二重間接 + 三重間接）。
pub const INODE_BLOCK_COUNT: usize = 15;

/// `i_block` のうち直接ブロックの数。
pub const DIRECT_BLOCK_COUNT: usize = 12;

/// ルートディレクトリの inode 番号。**ext2 では 2 で固定である。**
pub const ROOT_INODE: u32 = 2;

/// `i_mode` のうちファイル種別を表すビット。
const MODE_FORMAT_MASK: u16 = 0xF000;

/// `i_mode` のファイル種別: 通常ファイル。
const MODE_REGULAR: u16 = 0x8000;

/// `i_mode` のファイル種別: ディレクトリ。
const MODE_DIRECTORY: u16 = 0x4000;

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
    /// inode 番号が 0、または `s_inodes_count` を超えている（線3）。
    ///
    /// **ext2 の inode 番号は 1 始まりである。** 0 は「無い」を意味する値で、
    /// **`(ino - 1)` を先に計算すると桁借りする**（線2）。
    InodeOutOfRange(u32),
    /// inode の在るはずのバイト位置が像の外へ出る（線2・線3）。
    ///
    /// **group descriptor の `inode_table` が像の中を指していても、そこから
    /// `index * s_inode_size` だけ進んだ先が中とは限らない。**
    InodeTableOutOfRange { inode: u32, needed: u64 },
    /// ファイル内のブロック番号が `i_size` の外を指している。
    FileBlockOutOfRange(u32),
    /// 単一間接より先が要る（この段では直接ブロックだけを読む）。
    IndirectBlockUnsupported(u32),
    /// `i_block` の項が 0 なのに `i_size` の内側である（穴）。
    ///
    /// **借りて返す形なので、穴に対して返すゼロのバイト列が像の中に無い。**
    SparseBlock(u32),
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

/// inode 1 つ分（読み取りに要る欄だけ）。
///
/// # 構築後に成り立っている不変条件
///
/// **`blocks` の 0 でない項は、すべて `s_blocks_count` の内側を指している**
/// （線3。[`Ext2::inode`] が全項を見てから返す）。**ここを構築時に確かめて
/// おくと、`i_block` を辿る側が番号の妥当性を持ち回らずに済む。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inode {
    /// 番号（1 始まり）。**診断で「どの inode か」が要るので持つ。**
    pub number: u32,
    /// `i_mode`。上位 4 ビットが種別、下位 12 ビットが許可である。
    pub mode: u16,
    /// バイト長。**通常ファイルでは `i_size_high` を上位 32 ビットとして足す**
    /// （RO_COMPAT の `large_file`）。**ディレクトリでは足さない**——あの欄は
    /// `i_dir_acl` で、意味が違う。**この分岐は Linux と同じである。**
    pub size: u64,
    /// `i_links_count`。
    pub links_count: u16,
    /// `i_block`。0..12 が直接、12 が単一間接、13 が二重、14 が三重である。
    pub blocks: [u32; INODE_BLOCK_COUNT],
}

impl Inode {
    /// ディレクトリか。
    pub fn is_directory(&self) -> bool {
        self.mode & MODE_FORMAT_MASK == MODE_DIRECTORY
    }

    /// 通常ファイルか。
    pub fn is_regular_file(&self) -> bool {
        self.mode & MODE_FORMAT_MASK == MODE_REGULAR
    }
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

    /// inode を 1 つ読む（S10-a）。
    ///
    /// # 線がどう当たるか
    ///
    /// - **線3: 番号の範囲。** `ino` は 1 始まりで、`s_inodes_count` 以下でなければ
    ///   ならない。**0 を弾くのは範囲の話だけではない**——`ino - 1` が桁借りする
    /// - **線2: テーブル内の位置の算術。** `inode_table * block_size` も
    ///   `index * inode_size` も u32 では溢れうるので、**u64 で組み立てる**
    /// - **線3: `i_block` の 15 項。** 0 でない項が像の外を指していないことを、
    ///   **返す前に全部見る**。**辿る側に番号の妥当性を持ち回らせない**
    pub fn inode(&self, ino: u32) -> Result<Inode, Ext2Error> {
        if ino == 0 || ino > self.inodes_count {
            return Err(Ext2Error::InodeOutOfRange(ino));
        }
        // **ここから下は `ino >= 1` が保証されている。**
        let zero_based = ino - 1;
        let group = zero_based / self.inodes_per_group;
        let index = zero_based % self.inodes_per_group;
        let table = self.group_descriptor(group)?.inode_table;

        // 線2: u64 で組み立てる。**3 つとも u32 では溢れうる。**
        let start = u64::from(table) * u64::from(self.block_size)
            + u64::from(index) * u64::from(self.inode_size);
        // **飽和させる。** 桁あふれは u32 由来の項の積では起きないが、起きた場合も
        // 「像より大きい」へ倒れるので、静かに巻き戻らない。
        let end = start.saturating_add(INODE_CORE_LEN as u64);
        if end > self.image.len() as u64 {
            return Err(Ext2Error::InodeTableOutOfRange {
                inode: ino,
                needed: end,
            });
        }
        let raw = &self.image[start as usize..end as usize];

        let mode = read_u16(raw, 0);
        let mut blocks = [0u32; INODE_BLOCK_COUNT];
        for (slot, block) in blocks.iter_mut().enumerate() {
            *block = read_u32(raw, 40 + slot * 4);
            // 線3: 0 は「無い」を表す値なので範囲の外にあってよい。
            if *block != 0 && *block >= self.blocks_count {
                return Err(Ext2Error::BlockOutOfRange(*block));
            }
        }

        // `i_size_high` は通常ファイルでのみ上位 32 ビットである。ディレクトリでは
        // 同じ位置が `i_dir_acl` なので足さない（**Linux と同じ分岐**）。
        let size_low = u64::from(read_u32(raw, 4));
        let size = if mode & MODE_FORMAT_MASK == MODE_REGULAR {
            size_low | (u64::from(read_u32(raw, 108)) << 32)
        } else {
            size_low
        };

        Ok(Inode {
            number: ino,
            mode,
            size,
            links_count: read_u16(raw, 26),
            blocks,
        })
    }

    /// ファイル内の `index` 番目のブロックを**借りて返す**（S10-a）。
    ///
    /// **返るのは有効なバイトだけである。** 最後のブロックは `i_size` で切る
    /// ので、呼び出し側が長さを計算し直さなくてよい（**線2 の「呼び出し側に
    /// 強いる算術」を、こちら側で閉じている**）。
    ///
    /// **この段では直接ブロックだけを読む。** 12 番目から先は
    /// [`Ext2Error::IndirectBlockUnsupported`] で返る（単一間接は次の刻み）。
    pub fn file_block(&self, inode: &Inode, index: u32) -> Result<&'a [u8], Ext2Error> {
        // 線2: `index * block_size` は u32 では溢れる。**u64 で出す。**
        let offset = u64::from(index) * u64::from(self.block_size);
        if offset >= inode.size {
            return Err(Ext2Error::FileBlockOutOfRange(index));
        }
        if index as usize >= DIRECT_BLOCK_COUNT {
            return Err(Ext2Error::IndirectBlockUnsupported(index));
        }

        let block = inode.blocks[index as usize];
        if block == 0 {
            return Err(Ext2Error::SparseBlock(index));
        }
        let bytes = self.block_bytes(block)?;

        // 最後のブロックは `i_size` で切る。**残りはブロックサイズ以下なので
        // `usize` へ落として安全である。**
        let remaining = inode.size - offset;
        let len = remaining.min(u64::from(self.block_size)) as usize;
        Ok(&bytes[..len])
    }

    /// `i_size` を覆うのに要る直接ブロックの数。
    ///
    /// **`i_size` が 0 なら 0 である。** 間接が要る大きさでも、ここは 12 で頭打ち
    /// になる（**足りているかは呼び出し側が [`Self::file_block`] の結果で知る**）。
    pub fn direct_block_span(&self, inode: &Inode) -> u32 {
        let blocks = inode.size.div_ceil(u64::from(self.block_size));
        blocks.min(DIRECT_BLOCK_COUNT as u64) as u32
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

        // ルート inode（2 番）。**実測した像と同じ形にする**（`debugfs -R "stat <2>"`。
        // mode 040755・size 4096・`i_block[0]` = 20）。
        write_inode(
            &mut image,
            ROOT_INODE,
            0o040_755,
            4096,
            6,
            &[ROOT_DATA_BLOCK],
        );
        // ルートディレクトリの中身。**先頭 4 バイトは `.` の inode 番号である。**
        let data = ROOT_DATA_BLOCK as usize * 4096;
        image[data..data + 4].copy_from_slice(&ROOT_INODE.to_le_bytes());

        // 直接ブロックをちょうど使い切る通常ファイル（12 ブロック）。**最後の
        // ブロックが `i_size` で切られないことを見る側である。**
        let direct: std::vec::Vec<u32> = (0..DIRECT_BLOCK_COUNT as u32)
            .map(|i| DIRECT_FILE_FIRST_BLOCK + i)
            .collect();
        write_inode(
            &mut image,
            DIRECT_FILE_INODE,
            0o100_644,
            (DIRECT_BLOCK_COUNT * 4096) as u32,
            1,
            &direct,
        );
        for (index, block) in direct.iter().enumerate() {
            let at = *block as usize * 4096;
            image[at] = index as u8;
        }

        // 1 ブロックに満たない通常ファイル。**最後のブロックが切られる側である。**
        write_inode(
            &mut image,
            SHORT_FILE_INODE,
            0o100_644,
            18,
            1,
            &[SHORT_FILE_BLOCK],
        );
        let at = SHORT_FILE_BLOCK as usize * 4096;
        image[at..at + 18].copy_from_slice(b"ZaytOS ext2 short.");
        image
    }

    /// ルートディレクトリの中身が在るブロック（実測した像と同じ番号）。
    const ROOT_DATA_BLOCK: u32 = 20;
    /// 直接ブロックを使い切る通常ファイルの inode 番号と先頭ブロック。
    const DIRECT_FILE_INODE: u32 = 15;
    const DIRECT_FILE_FIRST_BLOCK: u32 = 31;
    /// 1 ブロックに満たない通常ファイルの inode 番号とブロック。
    const SHORT_FILE_INODE: u32 = 18;
    const SHORT_FILE_BLOCK: u32 = 58;

    /// inode テーブルへ 1 つ書く。**テストの像は group 0 だけである。**
    fn write_inode(image: &mut [u8], ino: u32, mode: u16, size: u32, links: u16, blocks: &[u32]) {
        const INODE_TABLE_BLOCK: usize = 4;
        const INODE_SIZE: usize = 256;
        let at = INODE_TABLE_BLOCK * 4096 + (ino as usize - 1) * INODE_SIZE;
        image[at..at + 2].copy_from_slice(&mode.to_le_bytes());
        image[at + 4..at + 8].copy_from_slice(&size.to_le_bytes());
        image[at + 26..at + 28].copy_from_slice(&links.to_le_bytes());
        for (slot, block) in blocks.iter().enumerate() {
            let field = at + 40 + slot * 4;
            image[field..field + 4].copy_from_slice(&block.to_le_bytes());
        }
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

    /// ルート inode が、実測した像と同じ値で読めること。
    #[test]
    fn reads_the_root_inode() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).expect("the root inode is readable");
        assert_eq!(root.number, ROOT_INODE);
        assert_eq!(root.mode, 0o040_755);
        assert_eq!(root.size, 4096);
        assert_eq!(root.links_count, 6);
        assert_eq!(root.blocks[0], ROOT_DATA_BLOCK);
        assert!(root.is_directory());
        assert!(!root.is_regular_file());
    }

    /// 線3: inode 番号の範囲。**0 と `s_inodes_count` 超えの両方を弾く。**
    ///
    /// **0 は範囲の話だけではない。** `ino - 1` を先に計算する形だと桁借りする。
    #[test]
    fn rejects_inode_numbers_outside_the_table() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(fs.inode(0), Err(Ext2Error::InodeOutOfRange(0)));
        assert_eq!(fs.inode(257), Err(Ext2Error::InodeOutOfRange(257)));
        assert_eq!(
            fs.inode(u32::MAX),
            Err(Ext2Error::InodeOutOfRange(u32::MAX))
        );
        // **境界の内側は通る。**弾きすぎていないことまで見る。
        assert!(fs.inode(256).is_ok());
    }

    /// 線2: inode テーブルの位置の算術が像の外へ出る。
    ///
    /// `s_inodes_count` を上げると、末尾の inode が像の外に落ちる。**番号の検査
    /// （線3）を通ってから位置の検査（線2）で止まることを見る。**
    #[test]
    fn rejects_an_inode_whose_position_falls_outside_the_image() {
        let mut image = build_test_image();
        // inode を 1 グループぶん増やし、テーブルが像に収まらない状態にする。
        image[SUPERBLOCK_OFFSET..SUPERBLOCK_OFFSET + 4].copy_from_slice(&600_000u32.to_le_bytes());
        image[SUPERBLOCK_OFFSET + 40..SUPERBLOCK_OFFSET + 44]
            .copy_from_slice(&600_000u32.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        assert!(matches!(
            fs.inode(600_000).unwrap_err(),
            Ext2Error::InodeTableOutOfRange { inode: 600_000, .. }
        ));
    }

    /// 線3: `i_block` のブロック番号が `s_blocks_count` の外を指している。
    ///
    /// **返す前に 15 項すべてを見る。** 直接ブロックだけでなく、この段では
    /// まだ辿らない間接の 3 項も見る。**辿る側に妥当性を持ち回らせないためである。**
    #[test]
    fn rejects_an_inode_whose_block_pointer_leaves_the_filesystem() {
        for slot in 0..INODE_BLOCK_COUNT {
            let mut image = build_test_image();
            let at = 4 * 4096 + (ROOT_INODE as usize - 1) * 256 + 40 + slot * 4;
            image[at..at + 4].copy_from_slice(&512u32.to_le_bytes());
            let fs = Ext2::parse(&image).unwrap();
            assert_eq!(
                fs.inode(ROOT_INODE),
                Err(Ext2Error::BlockOutOfRange(512)),
                "i_block[{slot}] pointing at block 512 must be refused"
            );
        }
    }

    /// **0 は「無い」を表す値なので、範囲の外にあってよい。**
    #[test]
    fn accepts_zero_block_pointers_in_the_unused_slots() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).unwrap();
        assert_eq!(root.blocks[1..], [0u32; INODE_BLOCK_COUNT - 1]);
    }

    /// 直接ブロックを辿り、最後のブロックが `i_size` で切られること。
    #[test]
    fn reads_a_file_through_its_direct_blocks() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();

        let full = fs.inode(DIRECT_FILE_INODE).unwrap();
        assert!(full.is_regular_file());
        assert_eq!(fs.direct_block_span(&full), DIRECT_BLOCK_COUNT as u32);
        for index in 0..DIRECT_BLOCK_COUNT as u32 {
            let bytes = fs.file_block(&full, index).unwrap();
            assert_eq!(bytes.len(), 4096, "block {index} is whole");
            assert_eq!(bytes[0], index as u8);
        }

        // 端: `i_size` を覆い切ったので、次の番号は範囲外である。
        assert_eq!(
            fs.file_block(&full, DIRECT_BLOCK_COUNT as u32),
            Err(Ext2Error::FileBlockOutOfRange(DIRECT_BLOCK_COUNT as u32))
        );

        let short = fs.inode(SHORT_FILE_INODE).unwrap();
        assert_eq!(fs.direct_block_span(&short), 1);
        assert_eq!(
            fs.file_block(&short, 0).unwrap(),
            b"ZaytOS ext2 short.",
            "the last block is cut at i_size, not at the block size"
        );
        assert_eq!(
            fs.file_block(&short, 1),
            Err(Ext2Error::FileBlockOutOfRange(1))
        );
    }

    /// 線2: ファイル内のブロック番号の算術が u32 で溢れる値。
    ///
    /// `index * block_size` を u32 で計算すると `0x40_0000` で 0 に巻き戻り、
    /// **「`i_size` の内側」と誤って判定する。** u64 で出していれば範囲外になる。
    #[test]
    fn a_file_block_index_that_would_overflow_in_u32_is_out_of_range() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        let short = fs.inode(SHORT_FILE_INODE).unwrap();
        assert_eq!(
            fs.file_block(&short, 0x40_0000),
            Err(Ext2Error::FileBlockOutOfRange(0x40_0000))
        );
        assert_eq!(
            fs.file_block(&short, u32::MAX),
            Err(Ext2Error::FileBlockOutOfRange(u32::MAX))
        );
    }

    /// この段では単一間接より先を読まない。**`i_size` の内側でも拒む。**
    #[test]
    fn refuses_to_follow_the_indirect_blocks_in_this_step() {
        let mut image = build_test_image();
        // 直接 12 ブロックぶんより 1 バイト大きくする（単一間接が要る形）。
        let size_at = 4 * 4096 + (DIRECT_FILE_INODE as usize - 1) * 256 + 4;
        image[size_at..size_at + 4]
            .copy_from_slice(&((DIRECT_BLOCK_COUNT * 4096) as u32 + 1).to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let inode = fs.inode(DIRECT_FILE_INODE).unwrap();
        assert_eq!(
            fs.file_block(&inode, DIRECT_BLOCK_COUNT as u32),
            Err(Ext2Error::IndirectBlockUnsupported(
                DIRECT_BLOCK_COUNT as u32
            ))
        );
    }

    /// 穴は拒む。**借りて返す形なので、返すゼロのバイト列が像の中に無い。**
    #[test]
    fn refuses_a_hole_because_there_is_nothing_to_borrow() {
        let mut image = build_test_image();
        let at = 4 * 4096 + (DIRECT_FILE_INODE as usize - 1) * 256 + 40 + 4 * 4;
        image[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let inode = fs.inode(DIRECT_FILE_INODE).unwrap();
        assert_eq!(fs.file_block(&inode, 4), Err(Ext2Error::SparseBlock(4)));
    }

    /// `i_size_high` は通常ファイルでだけ上位 32 ビットである。
    ///
    /// **ディレクトリでは同じ位置が `i_dir_acl` なので足さない。**
    #[test]
    fn the_high_size_field_counts_only_for_regular_files() {
        let mut image = build_test_image();
        let put_high = |image: &mut [u8], ino: u32| {
            let at = 4 * 4096 + (ino as usize - 1) * 256 + 108;
            image[at..at + 4].copy_from_slice(&1u32.to_le_bytes());
        };
        put_high(&mut image, ROOT_INODE);
        put_high(&mut image, SHORT_FILE_INODE);
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(fs.inode(ROOT_INODE).unwrap().size, 4096);
        assert_eq!(
            fs.inode(SHORT_FILE_INODE).unwrap().size,
            (1u64 << 32) | 18,
            "a regular file takes i_size_high as the upper 32 bits"
        );
    }

    /// 線1: どんな短さでも `inode` がパニックしない。
    ///
    /// **`parse` を通った像だけが `inode` に届く**ので、切り詰めた像は
    /// `parse` で落ちる。**そこを抜けた形でも落ちないことを見るため、
    /// `s_blocks_count` を下げて像だけを短くする。**
    #[test]
    fn reading_an_inode_from_a_truncated_image_does_not_panic() {
        for blocks in 1u32..64 {
            let mut image = build_test_image();
            image[SUPERBLOCK_OFFSET + 4..SUPERBLOCK_OFFSET + 8]
                .copy_from_slice(&blocks.to_le_bytes());
            let truncated = &image[..blocks as usize * 4096];
            let Ok(fs) = Ext2::parse(truncated) else {
                continue;
            };
            for ino in [1u32, ROOT_INODE, DIRECT_FILE_INODE, SHORT_FILE_INODE, 256] {
                let _ = fs.inode(ino);
            }
        }
    }

    #[test]
    fn block_bytes_borrows_and_rejects_out_of_range() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(fs.block_bytes(0).unwrap().len(), 4096);
        assert_eq!(fs.block_bytes(512), Err(Ext2Error::BlockOutOfRange(512)));
    }
}
