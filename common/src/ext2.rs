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

/// `i_block` の添字: 単一間接ブロック。
pub const SINGLE_INDIRECT_SLOT: usize = 12;

/// `i_block` の添字: 二重間接ブロック。**辿らない。**
const DOUBLE_INDIRECT_SLOT: usize = 13;

/// `i_block` の添字: 三重間接ブロック。**辿らない。**
const TRIPLE_INDIRECT_SLOT: usize = 14;

/// 間接ブロックの項 1 つのバイト数（ブロック番号は `u32`）。
const INDIRECT_ENTRY_SIZE: u32 = 4;

/// ディレクトリエントリの固定部のバイト数（`inode`・`rec_len`・`name_len`・
/// `file_type`）。**名前はこの直後に `name_len` バイト続く。**
const DIRENT_HEADER_LEN: usize = 8;

/// `rec_len` の整列。**ext2 はエントリを 4 バイト境界へ揃える**（Linux も
/// `ext2_check_page` でここを見ている）。
const DIRENT_ALIGNMENT: u16 = 4;

/// `file_type`: 通常ファイル。**この欄が在るのは INCOMPAT の `FILETYPE` に
/// よる**（[`INCOMPAT_FILETYPE`]。`mke2fs` の既定に入っている）。
pub const DIRENT_TYPE_REGULAR: u8 = 1;

/// `file_type`: ディレクトリ。
pub const DIRENT_TYPE_DIRECTORY: u8 = 2;

/// ルートディレクトリの inode 番号。**ext2 では 2 で固定である。**
pub const ROOT_INODE: u32 = 2;

/// パスの区切り。
const PATH_SEPARATOR: u8 = b'/';

/// パス 1 本に許す要素の数（S10-a）。
///
/// # これは停止性のための上限ではない
///
/// **パス解決は、要素の有限な並びを畳む形なので、上限が無くても止まる。**
/// `..` を辿っても循環しない——**`..` は特別扱いされず、ディレクトリの中の
/// ただのエントリとして引かれる**ので、辿る回数はパスの中の区切りの数で
/// 決まり切っている。**symlink を実装しないので、要素が増える経路も無い**
/// （`docs/roadmap.md` の S10 が実装しないと宣言している）。
///
/// # 何のための上限か。**仕事の量である**
///
/// **要素 1 つにつきディレクトリを 1 回走査する。** パスは S10-b で
/// ユーザー空間から来るので、**区切りだけを並べた長いパスは、走査を要素の数だけ
/// 走らせる。** 上限を置くと、**そこで確実にエラーが返る**（黙って長く働かない）。
///
/// **上限が要る理由と、止まる理由を分けて書いておく。** 混ぜると、
/// 「上限があるから止まる」という誤った根拠が残る。
///
/// # Linux は要素数の上限を持たない
///
/// **持たなくて済むのは `PATH_MAX`（4096 バイト）が実質的な上限になるからである。**
/// パスの長さを縛れば、要素の数もそこから決まる（要素 1 つに最低 2 バイト要る）。
///
/// **したがって S10-b でパスの長さの上限を入れるなら、この上限は要らなくなりうる。**
/// **どちらか一方でよい**ので、そのとき見直すこと。
///
/// # 外す条件
///
/// **「ユーザー空間から来るパスで 64 では足りないと分かったとき」。**
/// 今の木は `/data/indirect-first` が最も深くて 2 段しかない。
pub const MAX_PATH_COMPONENTS: usize = 64;

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
    /// 二重・三重間接が要る。**実装しない**（`docs/roadmap.md` の S10 の宣言）。
    IndirectBlockUnsupported(u32),
    /// `i_block` の項が 0 なのに `i_size` の内側である（穴）。
    ///
    /// **借りて返す形なので、穴に対して返すゼロのバイト列が像の中に無い。**
    SparseBlock(u32),
    /// ディレクトリとして走査しようとした inode が、ディレクトリでない。
    NotADirectory(u32),
    /// ブロックの残りが、エントリの固定部（8 バイト）に足りない（線1）。
    ///
    /// **健全なディレクトリでは起きない。** 最後のエントリの `rec_len` が
    /// ブロックの終わりまで伸びるので、残りはちょうど 0 になる。
    DirEntryTruncated { block: u32, remaining: u32 },
    /// `rec_len` が 4 の倍数でない。**Linux も `ext2_check_page` で見ている。**
    DirEntryMisaligned(u16),
    /// `rec_len` が `8 + name_len` に足りない（**線4。`rec_len = 0` はここで止まる**）。
    ///
    /// **走査が進むことを保証しているのはこの検査である。** `rec_len` が 0 だと
    /// 位置が動かず、**上限が無ければ QEMU のタイムアウトでしか落ちない。**
    DirEntryRecordTooSmall { rec_len: u16, name_len: u8 },
    /// `rec_len` がブロックの残りを越えている（線3）。
    DirEntryRecordPastBlock { rec_len: u16, remaining: u32 },
    /// パスが `/` で始まっていない。**現在位置を持たないので相対パスは引けない。**
    PathNotAbsolute,
    /// パスの要素が [`MAX_PATH_COMPONENTS`] を越えた。
    ///
    /// **停止性のための上限ではない**（あちらの doc に理由がある）。
    PathTooManyComponents(usize),
    /// その名前のエントリが無い。
    NotFound,
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

    /// 二重・三重間接を使うファイルか。
    ///
    /// **使っていれば、そのファイルはどのブロックも読まない。** 前半だけなら
    /// 単一間接の範囲で読めるが、**「途中まで読めて途中から読めない」形は、
    /// 呼び出し側から見て「短いファイル」と区別が付かない。** ファイル単位で
    /// 拒むほうが、扱えないことが呼び出し側へ確実に伝わる。
    ///
    /// **今の像には現れない。** 二重間接が要るのは 12 + 1024 ブロック
    /// （4 MiB 超）からで、2 MiB の像には収まらない。**壊した像に対する備えである。**
    pub fn uses_unsupported_indirection(&self) -> bool {
        self.blocks[DOUBLE_INDIRECT_SLOT] != 0 || self.blocks[TRIPLE_INDIRECT_SLOT] != 0
    }
}

/// ディレクトリエントリ 1 つ分。**名前は像から借りて返す。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry<'a> {
    /// 指している inode 番号。**0 のエントリ（未使用の枠）は走査が飛ばすので、
    /// ここへは現れない。**
    pub inode: u32,
    /// [`DIRENT_TYPE_REGULAR`] などの種別。
    pub file_type: u8,
    /// 名前。**`/` も NUL も含まない生のバイト列で、UTF-8 とは限らない。**
    pub name: &'a [u8],
}

impl DirEntry<'_> {
    /// ディレクトリか。
    pub fn is_directory(&self) -> bool {
        self.file_type == DIRENT_TYPE_DIRECTORY
    }

    /// 通常ファイルか。
    pub fn is_regular_file(&self) -> bool {
        self.file_type == DIRENT_TYPE_REGULAR
    }
}

/// ディレクトリのエントリを 1 つずつ返す（S10-a）。
///
/// # 止まること（線4）
///
/// **`rec_len` が 0 だと位置が動かず、走査が無限に回る。** ACPI の MADT で
/// 同じ形を踏んでいる（エントリ長 0。`acpi-test-zero-entry-length`）。
/// **「止まらないこと」は「エラーが返ること」で観測する。**
///
/// 止まる根拠は 3 つの検査が組み合わさった形である。
///
/// - `rec_len >= 8 + name_len` なので、**`rec_len` は必ず 8 以上である。**
///   したがって**ブロック内の位置は 1 回につき 8 バイト以上進む**
/// - `rec_len <= 残りバイト数` なので、**位置はブロックの終わりを越えない**
/// - ブロックの数は `i_size` から決まる有限の値である
///
/// **したがって、どんなバイト列に対しても有限回で終わる。** 上限を別に
/// 数えるのではなく、**進むことそのものを検査している。**
///
/// # エラーの後は続けない
///
/// **1 つでも壊れたエントリを見たら、そこで終わる。** 壊れた `rec_len` の
/// 先に何があるかは分からないので、**飛ばして続けると「どこを読んでいるのか」
/// が言えなくなる。**
pub struct DirEntries<'i, 'a> {
    fs: &'i Ext2<'a>,
    inode: Inode,
    block_count: u64,
    block_index: u32,
    block: Option<&'a [u8]>,
    offset: usize,
    finished: bool,
}

impl<'a> Iterator for DirEntries<'_, 'a> {
    type Item = Result<DirEntry<'a>, Ext2Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.finished {
                return None;
            }

            let bytes = match self.block {
                Some(bytes) => bytes,
                None => {
                    if u64::from(self.block_index) >= self.block_count {
                        self.finished = true;
                        return None;
                    }
                    match self.fs.file_block(&self.inode, self.block_index) {
                        Ok(bytes) => {
                            self.offset = 0;
                            self.block = Some(bytes);
                            bytes
                        }
                        Err(e) => {
                            self.finished = true;
                            return Some(Err(e));
                        }
                    }
                }
            };

            // このブロックを読み切ったら次のブロックへ。
            if self.offset >= bytes.len() {
                self.block = None;
                self.block_index += 1;
                continue;
            }

            let remaining = bytes.len() - self.offset;
            if remaining < DIRENT_HEADER_LEN {
                self.finished = true;
                return Some(Err(Ext2Error::DirEntryTruncated {
                    block: self.block_index,
                    remaining: remaining as u32,
                }));
            }
            let header = &bytes[self.offset..self.offset + DIRENT_HEADER_LEN];
            let inode = read_u32(header, 0);
            let rec_len = read_u16(header, 4);
            let name_len = header[6];
            let file_type = header[7];

            if !rec_len.is_multiple_of(DIRENT_ALIGNMENT) {
                self.finished = true;
                return Some(Err(Ext2Error::DirEntryMisaligned(rec_len)));
            }
            // **線4 の要。** `rec_len = 0` はここで止まる。
            if usize::from(rec_len) < DIRENT_HEADER_LEN + usize::from(name_len) {
                self.finished = true;
                return Some(Err(Ext2Error::DirEntryRecordTooSmall { rec_len, name_len }));
            }
            if usize::from(rec_len) > remaining {
                self.finished = true;
                return Some(Err(Ext2Error::DirEntryRecordPastBlock {
                    rec_len,
                    remaining: remaining as u32,
                }));
            }
            // 線3: 指している inode 番号が表の外を指していないこと。
            if inode != 0 && inode > self.fs.inodes_count {
                self.finished = true;
                return Some(Err(Ext2Error::InodeOutOfRange(inode)));
            }

            let name_start = self.offset + DIRENT_HEADER_LEN;
            let name = &bytes[name_start..name_start + usize::from(name_len)];
            // **ここで初めて位置を進める。** 上の 3 つを通っているので、
            // 進む量は 8 以上、かつブロックの内側である。
            self.offset += usize::from(rec_len);

            // inode 0 は未使用の枠である。**位置は進めた上で飛ばす。**
            if inode == 0 {
                continue;
            }
            return Some(Ok(DirEntry {
                inode,
                file_type,
                name,
            }));
        }
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
    /// **辿るのは直接 12 個と単一間接だけである。** 二重・三重間接は
    /// [`Ext2Error::IndirectBlockUnsupported`] で返る（`docs/roadmap.md` の S10 が
    /// 実装しないと宣言している）。
    pub fn file_block(&self, inode: &Inode, index: u32) -> Result<&'a [u8], Ext2Error> {
        // 線2: `index * block_size` は u32 では溢れる。**u64 で出す。**
        let offset = u64::from(index) * u64::from(self.block_size);
        if offset >= inode.size {
            return Err(Ext2Error::FileBlockOutOfRange(index));
        }
        // **二重・三重を使うファイルは、どのブロックも読まない**
        // （[`Inode::uses_unsupported_indirection`] に理由がある）。
        if inode.uses_unsupported_indirection() {
            return Err(Ext2Error::IndirectBlockUnsupported(index));
        }

        let block = self.block_number_of(inode, index)?;
        let bytes = self.block_bytes(block)?;

        // 最後のブロックは `i_size` で切る。**残りはブロックサイズ以下なので
        // `usize` へ落として安全である。**
        let remaining = inode.size - offset;
        let len = remaining.min(u64::from(self.block_size)) as usize;
        Ok(&bytes[..len])
    }

    /// ファイル内の `index` 番目のブロックの、**ファイルシステム上のブロック番号**。
    ///
    /// # 単一間接をどう辿るか
    ///
    /// `i_block[12]` が指すブロックは、**ブロック番号が `u32` で並んだ表**である。
    /// 12 番目から `12 + block_size / 4` 番目までが、その表の 0..n 項に対応する。
    ///
    /// # 線がどう当たるか
    ///
    /// - **線2: 表の中の添字の算術。** `(index - 12) * 4` である。**引き算は
    ///   `index >= 12` を確かめた後にしか通らない**ので桁借りしない。掛け算は
    ///   `entries_per_block` で先に頭打ちにしてあるので `block_size` を超えない
    /// - **線1: 表からの切り出し。** 上の理由で範囲内だが、**理由に頼らず
    ///   `get` で切る**。外れたらエラーで返る
    /// - **線3: 表から読んだブロック番号。** **これは像の中の任意のバイト列である。**
    ///   `s_blocks_count` の内側を指す保証がどこにも無いので、
    ///   [`Self::block_bytes`] が突き合わせる。**`i_block` の 15 項と違い、
    ///   [`Self::inode`] では見られない**——表は inode の外にあるからである
    fn block_number_of(&self, inode: &Inode, index: u32) -> Result<u32, Ext2Error> {
        if (index as usize) < DIRECT_BLOCK_COUNT {
            let block = inode.blocks[index as usize];
            if block == 0 {
                return Err(Ext2Error::SparseBlock(index));
            }
            return Ok(block);
        }

        // **ここから下は `index >= 12` が保証されている。**
        let within = index - DIRECT_BLOCK_COUNT as u32;
        let entries_per_block = self.block_size / INDIRECT_ENTRY_SIZE;
        if within >= entries_per_block {
            // 単一間接で届く範囲を越えた。**二重間接が要る。**
            return Err(Ext2Error::IndirectBlockUnsupported(index));
        }

        let table_block = inode.blocks[SINGLE_INDIRECT_SLOT];
        if table_block == 0 {
            return Err(Ext2Error::SparseBlock(index));
        }
        let table = self.block_bytes(table_block)?;

        // 線2: `within * 4` は `entries_per_block` で頭打ちなのでブロックを
        // 越えない。**線1: それでも `get` で切る。**
        let at = (within * INDIRECT_ENTRY_SIZE) as usize;
        let entry = table
            .get(at..at + INDIRECT_ENTRY_SIZE as usize)
            .ok_or(Ext2Error::FileBlockOutOfRange(index))?;
        let block = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
        if block == 0 {
            return Err(Ext2Error::SparseBlock(index));
        }
        Ok(block)
    }

    /// ディレクトリのエントリを走査する（S10-a）。
    ///
    /// **返るのは有限回で終わる走査である**（[`DirEntries`] に根拠がある）。
    /// **未使用の枠（`inode` が 0）は飛ばす**ので、返るエントリはすべて
    /// 実在の inode を指している。
    pub fn directory_entries(&self, inode: &Inode) -> Result<DirEntries<'_, 'a>, Ext2Error> {
        if !inode.is_directory() {
            return Err(Ext2Error::NotADirectory(inode.number));
        }
        Ok(DirEntries {
            fs: self,
            inode: *inode,
            block_count: self.block_span(inode),
            block_index: 0,
            block: None,
            offset: 0,
            finished: false,
        })
    }

    /// 絶対パスを inode へ解決する（S10-a）。
    ///
    /// # 毎回ルートから辿る
    ///
    /// **`dentry` を置かない**（`docs/roadmap.md` の S10 で決めた）。
    /// キャッシュと参照カウントが目的の構造なので、**引く回数が問題になって
    /// いない段では、毎回ルートから辿れば足りる。**
    ///
    /// # 区切りの扱いは Linux に合わせる
    ///
    /// - **空の要素は飛ばす。** `//` も、先頭の `/` も、末尾の `/` も同じ扱いになる
    /// - **`.` と `..` を特別扱いしない。** ext2 のディレクトリは両方を実体の
    ///   エントリとして持っているので、**ただの名前として引けば正しく動く**
    /// - **末尾が `/` なら、行き着いた先はディレクトリでなければならない。**
    ///   `/etc/motd/` は拒む
    ///
    /// # 線がどう当たるか
    ///
    /// - **線1: 分割。** どんなバイト列でも切り出しが範囲内に収まる
    ///   （`split` は空の要素を返すだけで、範囲外を作らない）
    /// - **線3: 要素の inode 番号。** 走査が既に `s_inodes_count` と
    ///   突き合わせている
    /// - **線4: 止まること。** **要素の数はパスの長さで決まり切っている。**
    ///   [`MAX_PATH_COMPONENTS`] は仕事の量の上限であって、止まる根拠ではない
    pub fn lookup(&self, path: &[u8]) -> Result<Inode, Ext2Error> {
        if path.first() != Some(&PATH_SEPARATOR) {
            return Err(Ext2Error::PathNotAbsolute);
        }

        let mut current = self.inode(ROOT_INODE)?;
        let mut components = 0usize;
        for component in path.split(|&byte| byte == PATH_SEPARATOR) {
            // 空の要素は飛ばす。**先頭・末尾・連続する区切りが、ここで同じ形になる。**
            if component.is_empty() {
                continue;
            }
            components += 1;
            if components > MAX_PATH_COMPONENTS {
                return Err(Ext2Error::PathTooManyComponents(MAX_PATH_COMPONENTS));
            }
            // **途中の要素がディレクトリでなければ、走査が `NotADirectory` を返す。**
            // 「辿った先がディレクトリでないのに続きがある」形はここで止まる。
            current = self.lookup_in(&current, component)?;
        }

        // 末尾が区切りなら、行き着いた先はディレクトリでなければならない。
        if path.last() == Some(&PATH_SEPARATOR) && !current.is_directory() {
            return Err(Ext2Error::NotADirectory(current.number));
        }
        Ok(current)
    }

    /// ディレクトリの中を名前で 1 段だけ引く（S10-a）。
    ///
    /// **`dir` がディレクトリでなければ [`Ext2Error::NotADirectory`] で返る**
    /// （[`Self::directory_entries`] が見ている）。
    ///
    /// **名前の突き合わせはバイト単位の完全一致である。** ext2 の名前は
    /// 255 バイトまでなので、**それより長い要素はどのエントリとも一致せず
    /// [`Ext2Error::NotFound`] になる。**
    pub fn lookup_in(&self, dir: &Inode, name: &[u8]) -> Result<Inode, Ext2Error> {
        for entry in self.directory_entries(dir)? {
            let entry = entry?;
            if entry.name == name {
                return self.inode(entry.inode);
            }
        }
        Err(Ext2Error::NotFound)
    }

    /// `i_size` を覆うのに要るブロックの数。
    ///
    /// **`i_size` が 0 なら 0 である。** 頭打ちにしない——**辿れるかどうかは
    /// [`Self::file_block`] の結果で分かる**ので、ここでは大きさだけを言う。
    pub fn block_span(&self, inode: &Inode) -> u64 {
        inode.size.div_ceil(u64::from(self.block_size))
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
        // ルートディレクトリの中身。**実測した像と同じエントリを同じ `rec_len` で
        // 並べる**（`. .. lost+found bin data etc`。最後の 1 つがブロックの
        // 終わりまで伸びる）。
        write_dir_block(&mut image, ROOT_DATA_BLOCK, ROOT_ENTRIES);

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
            SHORT_FILE_CONTENT.len() as u32,
            1,
            &[SHORT_FILE_BLOCK],
        );
        let at = SHORT_FILE_BLOCK as usize * 4096;
        image[at..at + SHORT_FILE_CONTENT.len()].copy_from_slice(SHORT_FILE_CONTENT);

        // 直接を 1 バイト超える通常ファイル。**単一間接を実際に踏む側である**
        // （実測した像の `/data/indirect-first` と同じ形。直接 43-54、`(IND)` 55、
        // その先の 13 ブロック目が 56）。
        let direct: std::vec::Vec<u32> = (0..DIRECT_BLOCK_COUNT as u32)
            .map(|i| INDIRECT_FILE_FIRST_BLOCK + i)
            .collect();
        let mut slots = direct.clone();
        slots.push(INDIRECT_FILE_TABLE_BLOCK);
        write_inode(
            &mut image,
            INDIRECT_FILE_INODE,
            0o100_644,
            (DIRECT_BLOCK_COUNT * 4096) as u32 + 1,
            1,
            &slots,
        );
        for (index, block) in direct.iter().enumerate() {
            let at = *block as usize * 4096;
            image[at] = index as u8;
        }
        // 間接ブロックの 0 項が 13 ブロック目を指す。
        let table = INDIRECT_FILE_TABLE_BLOCK as usize * 4096;
        image[table..table + 4].copy_from_slice(&INDIRECT_FILE_DATA_BLOCK.to_le_bytes());
        // 13 ブロック目の先頭 1 バイトが、ファイルの最後の 1 バイトである。
        image[INDIRECT_FILE_DATA_BLOCK as usize * 4096] = INDIRECT_FILE_LAST_BYTE;

        // **中間のディレクトリ**（パス解決が辿る側）。**実測した像と同じ木にする**
        // ——`/bin/hello`・`/data/direct-max`・`/data/indirect-first`・`/etc/motd`。
        write_subdirectory(&mut image, LOST_FOUND_INODE, 21, &[]);
        write_subdirectory(
            &mut image,
            BIN_INODE,
            22,
            &[(HELLO_INODE, DIRENT_TYPE_REGULAR, b"hello")],
        );
        write_subdirectory(
            &mut image,
            DATA_INODE,
            23,
            &[
                (DIRECT_FILE_INODE, DIRENT_TYPE_REGULAR, b"direct-max"),
                (INDIRECT_FILE_INODE, DIRENT_TYPE_REGULAR, b"indirect-first"),
            ],
        );
        write_subdirectory(
            &mut image,
            ETC_INODE,
            24,
            &[(SHORT_FILE_INODE, DIRENT_TYPE_REGULAR, b"motd")],
        );
        // `/bin/hello` の実体（中身は問わない。**名前で届くことだけを見る**）。
        write_inode(&mut image, HELLO_INODE, 0o100_755, 4096, 1, &[25]);
        image
    }

    /// ルート直下のディレクトリを 1 つ作る。**`.` と `..` を先頭に置く**
    /// （ext2 のディレクトリは両方を実体のエントリとして持つ。**パス解決が
    /// それを特別扱いしないので、実際に置かないと `..` が引けない**）。
    fn write_subdirectory(image: &mut [u8], ino: u32, block: u32, children: &[(u32, u8, &[u8])]) {
        let mut entries: std::vec::Vec<(u32, u8, &[u8])> = std::vec![
            (ino, DIRENT_TYPE_DIRECTORY, &b"."[..]),
            (ROOT_INODE, DIRENT_TYPE_DIRECTORY, &b".."[..]),
        ];
        entries.extend_from_slice(children);
        write_inode(image, ino, 0o040_755, 4096, 2, &[block]);
        write_dir_block(image, block, &entries);
    }

    /// ルートディレクトリの中身が在るブロック（実測した像と同じ番号）。
    const ROOT_DATA_BLOCK: u32 = 20;
    /// 直接ブロックを使い切る通常ファイルの inode 番号と先頭ブロック。
    const DIRECT_FILE_INODE: u32 = 15;
    const DIRECT_FILE_FIRST_BLOCK: u32 = 31;
    /// 1 ブロックに満たない通常ファイル（`/etc/motd`）の inode 番号とブロック。
    const SHORT_FILE_INODE: u32 = 18;
    const SHORT_FILE_BLOCK: u32 = 58;
    /// その中身。**長さは実測した像の `motd` と同じ 18 バイトである。**
    const SHORT_FILE_CONTENT: &[u8] = b"welcome to ZaytOS\n";
    /// ルート直下のディレクトリと `/bin/hello`（実測した像と同じ番号）。
    const LOST_FOUND_INODE: u32 = 11;
    const BIN_INODE: u32 = 12;
    const HELLO_INODE: u32 = 13;
    const DATA_INODE: u32 = 14;
    const ETC_INODE: u32 = 17;
    /// 単一間接を踏む通常ファイル（実測した像と同じ配置）。
    const INDIRECT_FILE_INODE: u32 = 16;
    const INDIRECT_FILE_FIRST_BLOCK: u32 = 43;
    const INDIRECT_FILE_TABLE_BLOCK: u32 = 55;
    const INDIRECT_FILE_DATA_BLOCK: u32 = 56;
    const INDIRECT_FILE_LAST_BYTE: u8 = 0xA7;

    /// ルートディレクトリのエントリ（実測した像と同じ並び。inode 番号も同じ）。
    const ROOT_ENTRIES: &[(u32, u8, &[u8])] = &[
        (ROOT_INODE, DIRENT_TYPE_DIRECTORY, b"."),
        (ROOT_INODE, DIRENT_TYPE_DIRECTORY, b".."),
        (11, DIRENT_TYPE_DIRECTORY, b"lost+found"),
        (12, DIRENT_TYPE_DIRECTORY, b"bin"),
        (14, DIRENT_TYPE_DIRECTORY, b"data"),
        (17, DIRENT_TYPE_DIRECTORY, b"etc"),
    ];

    /// ディレクトリの 1 ブロックを組み立てる。
    ///
    /// **`rec_len` は 4 バイト境界へ切り上げ、最後の 1 つはブロックの終わりまで
    /// 伸ばす**（ext2 の作り方であり、実測した像もそうなっている）。
    fn write_dir_block(image: &mut [u8], block: u32, entries: &[(u32, u8, &[u8])]) {
        let base = block as usize * 4096;
        let mut offset = 0usize;
        for (index, (ino, file_type, name)) in entries.iter().enumerate() {
            let needed = (DIRENT_HEADER_LEN + name.len()).next_multiple_of(4);
            let rec_len = if index + 1 == entries.len() {
                4096 - offset
            } else {
                needed
            };
            let at = base + offset;
            image[at..at + 4].copy_from_slice(&ino.to_le_bytes());
            image[at + 4..at + 6].copy_from_slice(&(rec_len as u16).to_le_bytes());
            image[at + 6] = name.len() as u8;
            image[at + 7] = *file_type;
            image[at + DIRENT_HEADER_LEN..at + DIRENT_HEADER_LEN + name.len()]
                .copy_from_slice(name);
            offset += rec_len;
        }
    }

    /// エントリの固定部の在るバイト位置（テストが `rec_len` などを壊すため）。
    fn dirent_offset(block: u32, index: usize, entries: &[(u32, u8, &[u8])]) -> usize {
        let mut offset = block as usize * 4096;
        for (_, _, name) in entries.iter().take(index) {
            offset += (DIRENT_HEADER_LEN + name.len()).next_multiple_of(4);
        }
        offset
    }

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
        assert_eq!(fs.block_span(&full), DIRECT_BLOCK_COUNT as u64);
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
        assert_eq!(fs.block_span(&short), 1);
        assert_eq!(
            fs.file_block(&short, 0).unwrap(),
            SHORT_FILE_CONTENT,
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

    /// 単一間接を実際に踏み、**境界の両側**が読めること。
    ///
    /// **直接だけで収まる側と、1 バイト超えて間接へ入る側を対で見る。**
    /// 片側だけでは「間接を踏んだ」ことも「踏まずに済んだ」ことも言えない。
    #[test]
    fn reads_across_the_single_indirect_boundary() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();

        // 直接だけの側。**12 ブロックちょうどで、13 番目は範囲外である。**
        let direct = fs.inode(DIRECT_FILE_INODE).unwrap();
        assert_eq!(direct.blocks[SINGLE_INDIRECT_SLOT], 0, "no indirect block");
        assert_eq!(fs.block_span(&direct), DIRECT_BLOCK_COUNT as u64);
        assert_eq!(
            fs.file_block(&direct, DIRECT_BLOCK_COUNT as u32),
            Err(Ext2Error::FileBlockOutOfRange(DIRECT_BLOCK_COUNT as u32))
        );

        // 間接へ入る側。**13 ブロックあり、最後の 1 バイトが間接の先にある。**
        let indirect = fs.inode(INDIRECT_FILE_INODE).unwrap();
        assert_eq!(
            indirect.blocks[SINGLE_INDIRECT_SLOT], INDIRECT_FILE_TABLE_BLOCK,
            "the single indirect slot is in use"
        );
        assert_eq!(fs.block_span(&indirect), DIRECT_BLOCK_COUNT as u64 + 1);
        for index in 0..DIRECT_BLOCK_COUNT as u32 {
            assert_eq!(fs.file_block(&indirect, index).unwrap().len(), 4096);
        }
        let last = fs
            .file_block(&indirect, DIRECT_BLOCK_COUNT as u32)
            .expect("the block past the direct blocks comes from the indirect table");
        assert_eq!(last, &[INDIRECT_FILE_LAST_BYTE], "the file's last byte");
        assert_eq!(
            fs.file_block(&indirect, DIRECT_BLOCK_COUNT as u32 + 1),
            Err(Ext2Error::FileBlockOutOfRange(
                DIRECT_BLOCK_COUNT as u32 + 1
            ))
        );
    }

    /// 線3: **間接ブロックの中身**が像の外を指している。
    ///
    /// **`i_block` の 15 項と違い、この番号は `inode` では見られない**——表は
    /// inode の外にあるからである。**辿るときに突き合わせる以外に道が無い。**
    #[test]
    fn rejects_an_indirect_entry_pointing_outside_the_filesystem() {
        let mut image = build_test_image();
        let table = INDIRECT_FILE_TABLE_BLOCK as usize * 4096;
        image[table..table + 4].copy_from_slice(&512u32.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let inode = fs.inode(INDIRECT_FILE_INODE).unwrap();
        // inode 自体は通る。**壊れているのは inode の外である。**
        assert_eq!(
            inode.blocks[SINGLE_INDIRECT_SLOT],
            INDIRECT_FILE_TABLE_BLOCK
        );
        assert_eq!(
            fs.file_block(&inode, DIRECT_BLOCK_COUNT as u32),
            Err(Ext2Error::BlockOutOfRange(512))
        );
    }

    /// 間接ブロックの項が 0 なのに `i_size` の内側である（穴）。
    #[test]
    fn rejects_a_hole_reached_through_the_indirect_table() {
        let mut image = build_test_image();
        let table = INDIRECT_FILE_TABLE_BLOCK as usize * 4096;
        image[table..table + 4].copy_from_slice(&0u32.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let inode = fs.inode(INDIRECT_FILE_INODE).unwrap();
        assert_eq!(
            fs.file_block(&inode, DIRECT_BLOCK_COUNT as u32),
            Err(Ext2Error::SparseBlock(DIRECT_BLOCK_COUNT as u32))
        );
    }

    /// 単一間接そのものが 0 なのに `i_size` の内側である。
    #[test]
    fn rejects_a_missing_indirect_table() {
        let mut image = build_test_image();
        let slot =
            4 * 4096 + (INDIRECT_FILE_INODE as usize - 1) * 256 + 40 + SINGLE_INDIRECT_SLOT * 4;
        image[slot..slot + 4].copy_from_slice(&0u32.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let inode = fs.inode(INDIRECT_FILE_INODE).unwrap();
        assert_eq!(
            fs.file_block(&inode, DIRECT_BLOCK_COUNT as u32),
            Err(Ext2Error::SparseBlock(DIRECT_BLOCK_COUNT as u32))
        );
    }

    /// 二重・三重間接は実装しない。**使うファイルは 1 ブロックも読まない。**
    #[test]
    fn refuses_a_file_that_uses_the_double_or_triple_indirect_slots() {
        for slot in [DOUBLE_INDIRECT_SLOT, TRIPLE_INDIRECT_SLOT] {
            let mut image = build_test_image();
            let at = 4 * 4096 + (INDIRECT_FILE_INODE as usize - 1) * 256 + 40 + slot * 4;
            image[at..at + 4].copy_from_slice(&57u32.to_le_bytes());
            let fs = Ext2::parse(&image).unwrap();
            let inode = fs.inode(INDIRECT_FILE_INODE).unwrap();
            assert!(inode.uses_unsupported_indirection());
            // **前半は単一間接の範囲だが、それでも読まない。**
            assert_eq!(
                fs.file_block(&inode, 0),
                Err(Ext2Error::IndirectBlockUnsupported(0)),
                "i_block[{slot}] in use must refuse every block, not just the late ones"
            );
        }
    }

    /// 単一間接で届く範囲を越えた添字は、二重間接が要るので拒む。
    ///
    /// ブロック 4096 では 1 表あたり 1024 項なので、**12 + 1024 番目からである。**
    #[test]
    fn refuses_an_index_beyond_the_reach_of_the_single_indirect_table() {
        let mut image = build_test_image();
        // 単一間接で届く最後の添字より 1 つ先まで `i_size` を伸ばす。
        let reach = DIRECT_BLOCK_COUNT as u64 + 4096 / 4;
        let size_at = 4 * 4096 + (INDIRECT_FILE_INODE as usize - 1) * 256 + 4;
        image[size_at..size_at + 4].copy_from_slice(&(((reach + 1) * 4096) as u32).to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let inode = fs.inode(INDIRECT_FILE_INODE).unwrap();
        assert_eq!(
            fs.file_block(&inode, reach as u32),
            Err(Ext2Error::IndirectBlockUnsupported(reach as u32))
        );
        // **直前の添字は単一間接の範囲である**（拒みすぎていないこと）。
        // 表の項は 0 なので穴として返る——**越えたのではなく、空だからである。**
        assert_eq!(
            fs.file_block(&inode, reach as u32 - 1),
            Err(Ext2Error::SparseBlock(reach as u32 - 1))
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

    /// ルートディレクトリを走査し、実測した像と同じ並びが返ること。
    #[test]
    fn walks_the_root_directory() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).unwrap();
        let entries: std::vec::Vec<DirEntry<'_>> = fs
            .directory_entries(&root)
            .unwrap()
            .map(|e| e.expect("every entry of a sound directory parses"))
            .collect();

        let names: std::vec::Vec<&[u8]> = entries.iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            std::vec![
                &b"."[..],
                &b".."[..],
                &b"lost+found"[..],
                &b"bin"[..],
                &b"data"[..],
                &b"etc"[..]
            ]
        );
        assert_eq!(entries[0].inode, ROOT_INODE, "\".\" points at itself");
        assert_eq!(entries[1].inode, ROOT_INODE, "root's parent is root");
        assert!(entries.iter().all(|e| e.is_directory()));
    }

    /// 停止性の試験を、時間で区切って回す。
    ///
    /// # なぜ要るのか。**停止性の試験は信号の形が他と違う**
    ///
    /// 他の試験は主張が偽なら「落ちる」が、**停止性の試験は「返ってこない」。**
    /// そして**上限を数える形では falsify できない**——止まらない実装では、
    /// 数える処理そのものが動かないからである（**実測した**。模様で埋めた
    /// ブロックを流して返る数の上限を見る形は、進むことの検査を外しても通った）。
    ///
    /// # 返らないままにできない
    ///
    /// **`cargo test` には項目ごとの時間上限が無く、`cargo xtask check` も
    /// `cargo test` に上限を付けていない**（`CHECKS` を `Command::status()` で
    /// 待つだけである。実測で確かめた）。**そのままだと `check` と `--full` が
    /// 返らなくなり、`§14` の「ハングと待ちが区別できない」に落ちる。**
    ///
    /// **別スレッドで回して時間で区切り、「返ってこない」を「落ちる」へ変換する。**
    /// 空転したスレッドは止められないので**残る**が、**試験の処理が終われば
    /// プロセスごと消える**（他の試験を妨げない）。
    fn assert_returns_promptly(what: &str, body: impl FnOnce() + Send + 'static) {
        /// 停止性の試験に与える時間。**健全な実装では 1 ミリ秒もかからない。**
        /// 遅い機械でも余裕があるように大きく取ってある。
        const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

        let (done, wait) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            body();
            // 受け手が既に諦めていることはある。**失敗しても構わない。**
            let _ = done.send(());
        });
        match wait.recv_timeout(DEADLINE) {
            Ok(()) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("{what} panicked; the assertion above says why")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
                "{what} did not return within {DEADLINE:?}. The directory walk is not making \
                 progress: every entry must advance the position by at least \
                 {DIRENT_HEADER_LEN} bytes, which is what the DirEntryRecordTooSmall check \
                 guarantees."
            ),
        }
    }

    /// 線4: **`rec_len` が 0 でも走査が止まる。**
    #[test]
    fn a_zero_record_length_ends_the_walk_instead_of_spinning() {
        assert_returns_promptly("the walk over a directory with rec_len = 0", || {
            let mut image = build_test_image();
            let at = dirent_offset(ROOT_DATA_BLOCK, 2, ROOT_ENTRIES);
            image[at + 4..at + 6].copy_from_slice(&0u16.to_le_bytes());
            let fs = Ext2::parse(&image).unwrap();
            let root = fs.inode(ROOT_INODE).unwrap();

            let mut walker = fs.directory_entries(&root).unwrap();
            assert!(walker.next().unwrap().is_ok(), "\".\" still parses");
            assert!(walker.next().unwrap().is_ok(), "\"..\" still parses");
            assert_eq!(
                walker.next().unwrap(),
                Err(Ext2Error::DirEntryRecordTooSmall {
                    rec_len: 0,
                    name_len: 10
                })
            );
            // **エラーの後は続けない。**
            assert!(walker.next().is_none());
        });
    }

    /// 線4: **ゼロで埋まったブロックでも走査が止まる。**
    ///
    /// **`rec_len = 0` かつ `inode = 0` は、無限ループの正準の入力である。**
    /// 未使用の枠は飛ばす形なので、**進むことの検査が無ければ 1 つも返さずに
    /// 空転する**（上の試験は 2 つ返してから止まるので、空転の入り口が違う）。
    #[test]
    fn an_all_zero_directory_block_ends_the_walk() {
        assert_returns_promptly("the walk over an all-zero directory block", || {
            let mut image = build_test_image();
            let base = ROOT_DATA_BLOCK as usize * 4096;
            image[base..base + 4096].fill(0);
            let fs = Ext2::parse(&image).unwrap();
            let root = fs.inode(ROOT_INODE).unwrap();
            let mut walker = fs.directory_entries(&root).unwrap();
            assert_eq!(
                walker.next().unwrap(),
                Err(Ext2Error::DirEntryRecordTooSmall {
                    rec_len: 0,
                    name_len: 0
                })
            );
            assert!(walker.next().is_none());
        });
    }

    /// **どんなバイト列でもパニックせず、返る数がブロックの容量を越えない。**
    ///
    /// **停止性そのものはここでは示せない。** 空転する実装はこの `for` が
    /// 返らないだけで、`steps` の上限には到達しないからである（**実測した**——
    /// 進むことの検査を外してもこの試験は通った）。**止まることを falsify する
    /// のは上の 2 つ**で、こちらが見ているのは
    /// **「壊れた中身でも切り出しが範囲内に収まる」**（線1）である。
    #[test]
    fn no_block_contents_make_the_walk_panic() {
        assert_returns_promptly("the walk over 64 arbitrary directory blocks", || {
            const MAX_STEPS: usize = 4096 / DIRENT_HEADER_LEN;
            for seed in 0u32..64 {
                let mut image = build_test_image();
                let base = ROOT_DATA_BLOCK as usize * 4096;
                for (index, byte) in image[base..base + 4096].iter_mut().enumerate() {
                    // 種ごとに違う模様で埋める。**健全さは狙わない。**
                    *byte = (index as u32).wrapping_mul(seed | 1).wrapping_add(seed) as u8;
                }
                let fs = Ext2::parse(&image).unwrap();
                let root = fs.inode(ROOT_INODE).unwrap();
                let mut steps = 0usize;
                for _ in fs.directory_entries(&root).unwrap() {
                    steps += 1;
                    assert!(steps <= MAX_STEPS, "seed {seed} did not terminate");
                }
            }
        });
    }

    /// `rec_len` が 4 の倍数でない。**Linux も同じところを見ている。**
    #[test]
    fn rejects_a_misaligned_record_length() {
        let mut image = build_test_image();
        let at = dirent_offset(ROOT_DATA_BLOCK, 0, ROOT_ENTRIES);
        image[at + 4..at + 6].copy_from_slice(&13u16.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).unwrap();
        assert_eq!(
            fs.directory_entries(&root).unwrap().next().unwrap(),
            Err(Ext2Error::DirEntryMisaligned(13))
        );
    }

    /// 線3: `rec_len` がブロックの残りを越えている。
    #[test]
    fn rejects_a_record_length_past_the_end_of_the_block() {
        let mut image = build_test_image();
        let at = dirent_offset(ROOT_DATA_BLOCK, 0, ROOT_ENTRIES);
        image[at + 4..at + 6].copy_from_slice(&5000u16.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).unwrap();
        assert_eq!(
            fs.directory_entries(&root).unwrap().next().unwrap(),
            Err(Ext2Error::DirEntryRecordPastBlock {
                rec_len: 5000,
                remaining: 4096
            })
        );
    }

    /// `rec_len` が `8 + name_len` に足りない（0 以外の形）。
    #[test]
    fn rejects_a_record_that_cannot_hold_its_own_name() {
        let mut image = build_test_image();
        let at = dirent_offset(ROOT_DATA_BLOCK, 2, ROOT_ENTRIES);
        // `lost+found` は 10 文字なので 18 バイト要る。**16 では足りない。**
        image[at + 4..at + 6].copy_from_slice(&16u16.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).unwrap();
        let last = fs.directory_entries(&root).unwrap().last().unwrap();
        assert_eq!(
            last,
            Err(Ext2Error::DirEntryRecordTooSmall {
                rec_len: 16,
                name_len: 10
            })
        );
    }

    /// 線3: エントリが指す inode 番号が表の外である。
    #[test]
    fn rejects_an_entry_pointing_outside_the_inode_table() {
        let mut image = build_test_image();
        let at = dirent_offset(ROOT_DATA_BLOCK, 3, ROOT_ENTRIES);
        image[at..at + 4].copy_from_slice(&999u32.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).unwrap();
        let last = fs.directory_entries(&root).unwrap().last().unwrap();
        assert_eq!(last, Err(Ext2Error::InodeOutOfRange(999)));
    }

    /// `inode` が 0 の枠は未使用である。**飛ばすが、位置は進める。**
    #[test]
    fn skips_unused_entries_without_losing_the_rest() {
        let mut image = build_test_image();
        let at = dirent_offset(ROOT_DATA_BLOCK, 2, ROOT_ENTRIES);
        image[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).unwrap();
        let names: std::vec::Vec<&[u8]> = fs
            .directory_entries(&root)
            .unwrap()
            .map(|e| e.unwrap().name)
            .collect();
        assert_eq!(
            names,
            std::vec![
                &b"."[..],
                &b".."[..],
                &b"bin"[..],
                &b"data"[..],
                &b"etc"[..]
            ],
            "the unused slot is skipped and the entries after it still come back"
        );
    }

    /// 名前でファイルへ届き、中身が読めること。
    #[test]
    fn resolves_a_path_to_the_file_it_names() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();

        let motd = fs.lookup(b"/etc/motd").expect("/etc/motd resolves");
        assert_eq!(motd.number, SHORT_FILE_INODE);
        assert!(motd.is_regular_file());
        assert_eq!(fs.file_block(&motd, 0).unwrap(), SHORT_FILE_CONTENT);

        assert_eq!(
            fs.lookup(b"/data/direct-max").unwrap().number,
            DIRECT_FILE_INODE
        );
        assert_eq!(
            fs.lookup(b"/data/indirect-first").unwrap().number,
            INDIRECT_FILE_INODE
        );
        assert_eq!(fs.lookup(b"/bin/hello").unwrap().number, HELLO_INODE);
    }

    /// 線1: **区切りの並びが、どう来ても同じ形に畳まれる。**
    ///
    /// 空の要素は飛ばすので、**連続する区切りも、末尾の区切りも、
    /// 1 つの `/` と同じ扱いになる**（Linux と同じ）。
    #[test]
    fn separators_collapse_the_way_linux_collapses_them() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();

        for path in [
            &b"/"[..],
            &b"//"[..],
            &b"///"[..],
            &b"/."[..],
            &b"/./"[..],
            &b"/etc/.."[..],
            &b"/etc/../"[..],
            &b"/../.."[..],
        ] {
            assert_eq!(
                fs.lookup(path).unwrap().number,
                ROOT_INODE,
                "{:?} must land on the root",
                core::str::from_utf8(path).unwrap()
            );
        }

        for path in [
            &b"/etc//motd"[..],
            &b"//etc///motd"[..],
            &b"/./etc/./motd"[..],
        ] {
            assert_eq!(
                fs.lookup(path).unwrap().number,
                SHORT_FILE_INODE,
                "{:?} must land on /etc/motd",
                core::str::from_utf8(path).unwrap()
            );
        }
    }

    /// `..` は特別扱いしない。**ディレクトリの中の実体のエントリとして引く。**
    #[test]
    fn dot_dot_is_an_ordinary_entry_not_a_special_case() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(
            fs.lookup(b"/etc/../etc/motd").unwrap().number,
            SHORT_FILE_INODE
        );
        // **ルートの `..` はルート自身である**（実測した像もそうなっている）。
        assert_eq!(fs.lookup(b"/../etc/motd").unwrap().number, SHORT_FILE_INODE);
    }

    /// 末尾が区切りなら、行き着いた先はディレクトリでなければならない。
    #[test]
    fn a_trailing_separator_demands_a_directory() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(fs.lookup(b"/etc/").unwrap().number, ETC_INODE);
        assert_eq!(
            fs.lookup(b"/etc/motd/"),
            Err(Ext2Error::NotADirectory(SHORT_FILE_INODE))
        );
    }

    /// 途中の要素がディレクトリでないのに、パスに続きがある。
    #[test]
    fn a_path_cannot_continue_through_a_regular_file() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(
            fs.lookup(b"/etc/motd/anything"),
            Err(Ext2Error::NotADirectory(SHORT_FILE_INODE))
        );
    }

    /// 相対パスは引けない。**現在位置を持たないからである。**
    #[test]
    fn relative_paths_are_refused() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        for path in [&b""[..], &b"etc/motd"[..], &b"."[..], &b".."[..]] {
            assert_eq!(fs.lookup(path), Err(Ext2Error::PathNotAbsolute));
        }
    }

    /// 無い名前は [`Ext2Error::NotFound`] である。
    ///
    /// **255 バイトを越える要素も同じ**——ext2 の名前はそれより長くなれないので、
    /// どのエントリとも一致しない。
    #[test]
    fn a_missing_name_is_not_found() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(fs.lookup(b"/nope"), Err(Ext2Error::NotFound));
        assert_eq!(fs.lookup(b"/etc/nope"), Err(Ext2Error::NotFound));

        let mut long = std::vec![b'/'];
        long.extend(std::iter::repeat_n(b'a', 300));
        assert_eq!(fs.lookup(&long), Err(Ext2Error::NotFound));
    }

    /// 要素の数の上限。**止まるための上限ではなく、仕事の量の上限である。**
    ///
    /// **上限までは通り、越えると落ちる**（拒みすぎていないことまで見る）。
    #[test]
    fn the_component_limit_bounds_the_work_not_the_termination() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();

        // `/.` を並べる。**どれだけ並べてもルートに留まる**ので、上限だけが効く。
        let at_limit: std::vec::Vec<u8> = b"/."
            .iter()
            .copied()
            .cycle()
            .take(MAX_PATH_COMPONENTS * 2)
            .collect();
        assert_eq!(fs.lookup(&at_limit).unwrap().number, ROOT_INODE);

        let past_limit: std::vec::Vec<u8> = b"/."
            .iter()
            .copied()
            .cycle()
            .take((MAX_PATH_COMPONENTS + 1) * 2)
            .collect();
        assert_eq!(
            fs.lookup(&past_limit),
            Err(Ext2Error::PathTooManyComponents(MAX_PATH_COMPONENTS))
        );
    }

    /// 線1: **どんなバイト列をパスとして渡してもパニックしない。**
    #[test]
    fn no_path_bytes_make_the_lookup_panic() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        for seed in 0u32..256 {
            let path: std::vec::Vec<u8> = (0..seed as usize % 40)
                .map(|i| (i as u32).wrapping_mul(seed | 1).wrapping_add(seed) as u8)
                .collect();
            let _ = fs.lookup(&path);
            // 先頭を区切りにした形も見る（**分割の経路へ実際に入る**）。
            let mut absolute = std::vec![PATH_SEPARATOR];
            absolute.extend_from_slice(&path);
            let _ = fs.lookup(&absolute);
        }
    }

    /// 通常ファイルはディレクトリとして走査しない。
    #[test]
    fn refuses_to_walk_a_regular_file() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        let file = fs.inode(SHORT_FILE_INODE).unwrap();
        assert!(matches!(
            fs.directory_entries(&file),
            Err(Ext2Error::NotADirectory(n)) if n == SHORT_FILE_INODE
        ));
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
