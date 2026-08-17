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

/// `s_want_extra_isize` の superblock 内オフセット（S12-f-3）。
///
/// # [`SUPERBLOCK_MIN_LEN`] を伸ばさない
///
/// **伸ばすと、受理する像が狭まる**——**いま通っている短い像を拒む方向に働く。**
/// **f-3 が変えたいのは書く側であって、受理する範囲ではない。**
/// **そこで、届かなければ 0 として扱う**（[`Ext2::want_extra_isize`]）。
const SUPERBLOCK_WANT_EXTRA_ISIZE: usize = 350;

/// `i_extra_isize` の inode 内オフセット。**標準部の直後である。**
const INODE_EXTRA_ISIZE: usize = INODE_CORE_LEN;

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
/// **S10-b でパスの長さの上限を入れたので、そこで見直した。結論は「両方が要る」である。**
/// `kernel::syscall::PATH_MAX` は 256 で、**256 バイトあれば `/a` の形で 128 要素まで
/// 書ける。** したがって**この 64 のほうが先に効く。**
/// **どちらか一方でよいと見込んでいたが、実際の値では一方にならなかった。**
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
    /// `s_free_blocks_count`（S12-b）。**群ごとの欄と対で直す。**
    free_blocks_count: u32,
    /// `s_free_inodes_count`（S12-b）。
    free_inodes_count: u32,
    /// 新しい inode へ書く `i_extra_isize`（S12-f-3）。**0 は「書かない」である。**
    want_extra_isize: u16,
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
    /// `bg_free_blocks_count`（S12-b）。**割り当てで直す欄である。**
    pub free_blocks_count: u16,
    /// `bg_free_inodes_count`（S12-b）。
    pub free_inodes_count: u16,
    /// `bg_used_dirs_count`（S12-b）。**ディレクトリを作るときだけ動く。**
    pub used_dirs_count: u16,
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
    /// `i_blocks`。**512 バイト単位である**（ブロックサイズ単位ではない）。
    ///
    /// **間接ブロックも数に入る。** 実測で `/data/indirect-first` は 112 で、
    /// 14 ブロック分（データ 13 + 単一間接 1）× 4096 / 512 である。
    /// **Linux の `st_blocks` がそのまま同じ意味なので、写すだけで足りる。**
    pub blocks_512: u32,
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
    /// このエントリの**直後**を指す、ディレクトリ内のバイト位置。
    ///
    /// **走査を再開できる位置である。** [`Ext2::directory_entries_from`] へ渡すと、
    /// このエントリの次から続けられる。
    ///
    /// **`getdents64` の `d_off` がこれである。** Linux では `d_off` は
    /// 「`lseek` で戻れる不透明な値」で、**索引付きのディレクトリではハッシュが入る**
    /// （実測した。`tmpfs` も実ディスクの ext4 も、バイト位置ではない値を返した）。
    /// **線形のディレクトリではバイト位置であり、ここもそれに倣う。**
    ///
    /// # 「ext2 だからバイト位置」であって「`d_off` がバイト位置だから」ではない
    ///
    /// **理由を取り違えると、次の判断が変わる。** ここが線形の走査でよいのは
    /// **`dir_index`（COMPAT の機能ビット。像には立っているが実装していない）を
    /// 辿っていないからである。**
    ///
    /// **`dir_index` を実装したら、バイト位置では足りなくなる。** 索引付きの
    /// ディレクトリはハッシュ順に返すので、**「次のバイト位置」が「次のエントリ」を
    /// 指さない。** そのときは Linux と同じくハッシュを入れることになる。
    /// **不透明な値という契約は、そこまで含めて決めてある。**
    pub next_offset: u64,
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
                            // **最初のブロックだけは `from` の位置から始める。**
                            // 2 つ目からは先頭に戻す（`block` を `None` にする側が 0 を置く）。
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
                self.offset = 0;
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
                next_offset: u64::from(self.block_index) * u64::from(self.fs.block_size)
                    + self.offset as u64,
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
        let free_blocks_count = read_u32(sb, 12);
        let free_inodes_count = read_u32(sb, 16);
        let blocks_per_group = read_u32(sb, 32);
        let inodes_per_group = read_u32(sb, 40);
        if blocks_per_group == 0 || inodes_per_group == 0 {
            return Err(Ext2Error::ZeroPerGroup);
        }

        // **新しい inode へ書く `i_extra_isize`（S12-f-3）。**
        //
        // **`SUPERBLOCK_MIN_LEN` の外にあるので、届くかを見てから読む。**
        // **届かない像、値が 0 の像、追加領域が inode に収まらない像では 0 にする**
        // ——**0 は「追加領域を持たない」という妥当な ext2 である。**
        // **倒れたことは呼び出し側から見える**（[`Ext2::want_extra_isize`] が 0 を返す）。
        let want_extra_isize = {
            let at = SUPERBLOCK_OFFSET + SUPERBLOCK_WANT_EXTRA_ISIZE;
            let declared = if at + 2 <= image.len() {
                read_u16(image, at)
            } else {
                0
            };
            if declared != 0 && INODE_CORE_LEN + usize::from(declared) <= usize::from(inode_size) {
                declared
            } else {
                0
            }
        };

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
            free_blocks_count,
            free_inodes_count,
            want_extra_isize,
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
    /// 書き換えに要る配置を写して返す（S12-b）。**`Copy` なので像を借りない。**
    pub fn layout(&self) -> Layout {
        Layout {
            block_size: self.block_size,
            blocks_count: self.blocks_count,
            first_data_block: self.first_data_block,
            blocks_per_group: self.blocks_per_group,
            group_count: self.group_count,
            inodes_per_group: self.inodes_per_group,
            inode_size: self.inode_size,
            inodes_count: self.inodes_count,
            first_inode: self.first_inode,
            want_extra_isize: self.want_extra_isize,
        }
    }

    /// `s_free_blocks_count`（S12-b）。
    pub fn free_blocks_count(&self) -> u32 {
        self.free_blocks_count
    }
    /// `s_free_inodes_count`（S12-b）。
    pub fn free_inodes_count(&self) -> u32 {
        self.free_inodes_count
    }

    /// 新しい inode へ書く `i_extra_isize`（S12-f-3）。
    ///
    /// **`s_want_extra_isize` をそのまま返すのではない。**
    /// **届かない・0・inode に収まらない、のいずれかなら 0 である。**
    /// **0 は「追加領域を持たない」を意味する妥当な ext2 で、
    /// この実装が S12-f-3 より前に書いていた値でもある。**
    ///
    /// # 0 へ倒れたことが見えるようにしてある
    ///
    /// **黙って倒れると、書いているつもりで書いていない状態になる。**
    /// **カーネルは起動ログへこの値を出す**ので、
    /// **像を替えて 0 になったときに、判定行の側から気づける。**
    pub fn want_extra_isize(&self) -> u16 {
        self.want_extra_isize
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
            // 破壊 (S12-b, ext2-group-count-offset): 空きブロック数を 2 バイト
            // 先（空き inode 数の欄）から読む。**外の道具の値と食い違う。**
            #[cfg(not(feature = "ext2-group-count-offset-break"))]
            free_blocks_count: read_u16(raw, 12),
            #[cfg(feature = "ext2-group-count-offset-break")]
            free_blocks_count: read_u16(raw, 14),
            free_inodes_count: read_u16(raw, 14),
            used_dirs_count: read_u16(raw, 16),
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
            blocks_512: read_u32(raw, 28),
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
        self.directory_entries_from(inode, 0)
    }

    /// ディレクトリの `from` バイト目から走査する（S10-b）。
    ///
    /// # `from` はエントリの境界であること
    ///
    /// **[`DirEntry::next_offset`] が返した値を渡すこと。** 途中の位置を渡すと、
    /// **そこにあるバイト列をエントリとして読む**——`rec_len` の 3 条件が
    /// 通らなければエラーで返り、通ってしまえば別の並びとして読める。
    /// **どちらにしてもパニックはしないが、意味のある結果にもならない。**
    ///
    /// **範囲外の `from` は、エントリが 1 つも返らない形になる**
    /// （ブロックの数で頭打ちになる）。
    pub fn directory_entries_from(
        &self,
        inode: &Inode,
        from: u64,
    ) -> Result<DirEntries<'_, 'a>, Ext2Error> {
        if !inode.is_directory() {
            return Err(Ext2Error::NotADirectory(inode.number));
        }
        let block_size = u64::from(self.block_size);
        let block_index = u32::try_from(from / block_size).unwrap_or(u32::MAX);
        Ok(DirEntries {
            fs: self,
            inode: *inode,
            block_count: self.block_span(inode),
            block_index,
            block: None,
            offset: (from % block_size) as usize,
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
/// 像を書き換えるのに要る配置（S12-b）。
///
/// # なぜ `Ext2` を可変にしないのか
///
/// **`Ext2` は像を借りている。** 全体を可変にすると、**読み取りの経路すべてが
/// 可変借用に巻き込まれる**——`lookup` も `file_block` も、返した参照が
/// 生きている間は書けなくなる。
///
/// **代わりに、配置だけを写して持ち出す。** これは `Copy` なので、
/// **`Ext2` を落としてから像を可変で借り直せる。**
/// **読む型と書く関数が、同じ像を同時に借りない形である。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    block_size: u32,
    blocks_count: u32,
    first_data_block: u32,
    blocks_per_group: u32,
    group_count: u32,
    /// inode テーブルの位置を引くのに要る（S12-c）。
    inodes_per_group: u32,
    inode_size: u16,
    inodes_count: u32,
    /// 新しい inode へ書く `i_extra_isize`（S12-f-3）。**0 は「書かない」である。**
    want_extra_isize: u16,
    /// 配ってよい最も小さい inode 番号（`s_first_ino`。S12-e）。
    ///
    /// **これより小さい番号は ext2 が用途を決めている**（ルートは 2 など）。
    /// **ビットマップにも印が立っているはずだが、そこに頼らない**——
    /// **印が落ちた像を渡されたときに、予約された番号を配ってしまう。**
    first_inode: u32,
}

/// ビットマップの操作で起きうる誤り（S12-b）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// 空きが無い。
    Full,
    /// 番号が像の外である。
    BlockOutOfRange(u32),
    /// 解放しようとしたブロックが、そもそも使われていない。
    NotAllocated(u32),
    /// 像が短く、触ろうとした場所が入っていない。
    ImageTooSmall,
    /// 空き数の欄が、ビットマップと食い違っている。
    ///
    /// **飽和させずに断る**——0 のまま進むと、**解放で 1 増えて往復が戻らない。**
    FreeCountInconsistent,
    /// inode 番号が 0、または `s_inodes_count` を超えている（S12-e）。
    ///
    /// **[`AllocError::BlockOutOfRange`] を流用しない。** 追記と縮めは
    /// inode 番号をあちらで返しているが（S12-c）、**番号の族が違うものを
    /// 同じ変種で返すと、診断で取り違える。**
    InodeOutOfRange(u32),
    /// 名前が空、255 バイトを超える、`/` を含む、または `.` か `..`（S12-e）。
    BadName,
    /// その名前が既にディレクトリに在る（S12-e）。
    NameTaken,
    /// ディレクトリのどのブロックにも、この名前を入れる隙間が無い（S12-e）。
    ///
    /// **ディレクトリは伸ばさない**（[`create_file`] の doc に理由がある）。
    NoRoomInDirectory,
    /// ディレクトリとして辿ろうとした inode が、ディレクトリでない（S12-e）。
    NotADirectory(u32),
    /// 消そうとした inode が通常ファイルでない（S12-e）。
    ///
    /// **ディレクトリは消さない**（[`unlink_file`] の doc に理由がある）。
    NotARegularFile(u32),
    /// その名前のエントリがディレクトリに無い（S12-e）。
    NoSuchEntry,
    /// 消そうとしたエントリがブロックの先頭に在る（S12-e）。
    ///
    /// **前のエントリへ隙間を吸わせる形なので、前が要る。**
    /// **健全なディレクトリでは起きない**——先頭は必ず `.` である。
    NoPreviousEntry,
    /// ディレクトリのバイト列が、エントリの並びとして読めない（S12-e）。
    ///
    /// **書く側は読む側より厳しくてよい。** [`DirEntries`] は壊れた並びを
    /// 種類ごとに区別して返すが、**書く前に見つけたらそこで止める**——
    /// **並びが読めない場所へ書き足すと、壊し方が増えるだけである。**
    DirectoryCorrupt,
}

impl Layout {
    /// このブロックが属する群と、群の中での添字。
    fn locate(&self, block: u32) -> Option<(u32, u32)> {
        if block >= self.blocks_count {
            return None;
        }
        let index = block.checked_sub(self.first_data_block)?;
        Some((index / self.blocks_per_group, index % self.blocks_per_group))
    }

    /// inode の像内オフセット（S12-c）。**`Ext2::inode` と同じ算術である。**
    ///
    /// **テーブルの位置は群の descriptor にあるので、像を読む。**
    fn inode_at(&self, image: &[u8], ino: u32) -> Option<usize> {
        if ino == 0 || ino > self.inodes_count {
            return None;
        }
        let group = (ino - 1) / self.inodes_per_group;
        let index = (ino - 1) % self.inodes_per_group;
        let descriptor = self.descriptor_at(group)?;
        if descriptor + GROUP_DESCRIPTOR_SIZE > image.len() {
            return None;
        }
        let table = read_u32(image, descriptor + 8);
        let offset = u64::from(table) * u64::from(self.block_size)
            + u64::from(index) * u64::from(self.inode_size);
        let offset = usize::try_from(offset).ok()?;
        if offset + usize::from(self.inode_size) > image.len() {
            return None;
        }
        Some(offset)
    }

    /// 1 ブロックあたりの 512 バイト単位の数（`i_blocks` の単位。S12-c）。
    ///
    /// **`i_blocks` はバイトでもブロックでもなく、512 バイト単位である。**
    /// **既存の族に単位の取り違えがある**（`stat-blocks-in-bytes`）。
    fn sectors_per_block(&self) -> u32 {
        self.block_size / 512
    }

    /// 群の descriptor の像内オフセット。
    fn descriptor_at(&self, group: u32) -> Option<usize> {
        if group >= self.group_count {
            return None;
        }
        let table = u64::from(self.first_data_block + 1) * u64::from(self.block_size);
        let offset = table + u64::from(group) * GROUP_DESCRIPTOR_SIZE as u64;
        usize::try_from(offset).ok()
    }

    /// ブロックの像内オフセット（T3-2）。
    ///
    /// **`block * block_size` を 1 箇所へ集めた。** 書く側の 8 箇所が
    /// 同じ算術を同じ `map_err` ごと書いていた。
    ///
    /// **読む側はまだ差し替えていない**——同じ算術が [`Ext2`] の側に残っている
    /// （`block_bytes` など）。**T3 の一覧は着手時に閉じるので、読む側は
    /// 行を立てて次の整理の段で拾う**（`docs/deferred-decisions.md`）。
    ///
    /// **[`Layout::descriptor_at`] と違って `Option` でなく `Err` を返す**——
    /// あちらは呼ぶ側ごとに誤りの種類が違うが、**こちらは 8 箇所すべてが
    /// [`AllocError::ImageTooSmall`] へ写していた。** 呼ぶ側に選ばせる理由が無い。
    fn block_at(&self, block: u32) -> Result<usize, AllocError> {
        usize::try_from(u64::from(block) * u64::from(self.block_size))
            .map_err(|_| AllocError::ImageTooSmall)
    }
}

/// ビットマップの中でこの添字が住む場所（T3-2）。**`(バイト位置, マスク)` を返す。**
///
/// **`index / 8` と `index % 8` の対を 1 箇所へ集めた。** 取得と解放の 4 関数が
/// 同じ対を書いていた。**対で使う算術を別々に持つと、片方だけ直る余地が生まれる。**
///
/// **読む側は対象外である**——ビットマップを読むのは書く側だけなので、
/// こちらには残りが無い（ブロック位置の算術とは違う）。
fn bitmap_slot(bitmap: usize, index: u32) -> (usize, u8) {
    (bitmap + (index / 8) as usize, 1u8 << (index % 8))
}

/// `i_block[index]` の像内オフセット（T3-2）。
///
/// **`+ 40 + index * 4` を 1 箇所へ集めた。** 40 は inode の中の `i_block` の
/// 位置、4 はスロットの幅である。**欄の位置に名前を付けるのは T3-3 の仕事なので、
/// ここでは算術を集めるところまでにする。**
///
/// **読む側はまだ差し替えていない**——[`Ext2::inode`] が同じ位置を自分で
/// 読んでいる。**行を立てて次の整理の段で拾う**（`docs/deferred-decisions.md`）。
fn i_block_slot(inode_at: usize, index: usize) -> usize {
    inode_at + 40 + index * 4
}

/// 空きブロックを 1 つ取り、会計も直す（S12-b）。**取れた番号を返す。**
///
/// # 直すのは 3 つである
///
/// ビットマップのビット、`bg_free_blocks_count`、`s_free_blocks_count`。
/// **1 つでも落とすと `e2fsck` が食い違いを報告する**（実測。
/// 落とし方によって文言が違う）。
///
/// # 番号を選ぶ規則を主張しない
///
/// **「空いているものを 1 つ」だけである。** どれを選ぶかは判定に書かない
/// （`docs/verification-coverage.md` の「像の中の番号に依存しない」）。
pub fn allocate_block(image: &mut [u8], layout: &Layout) -> Result<u32, AllocError> {
    for group in 0..layout.group_count {
        let descriptor = layout
            .descriptor_at(group)
            .ok_or(AllocError::ImageTooSmall)?;
        if descriptor + GROUP_DESCRIPTOR_SIZE > image.len() {
            return Err(AllocError::ImageTooSmall);
        }
        let bitmap_block = read_u32(image, descriptor);
        let bitmap = layout.block_at(bitmap_block)?;

        // この群が受け持つブロック数（最後の群は端数になりうる）。
        let first = layout.first_data_block + group * layout.blocks_per_group;
        let span = layout.blocks_per_group.min(layout.blocks_count - first);
        for index in 0..span {
            let (byte, mask) = bitmap_slot(bitmap, index);
            if byte >= image.len() {
                return Err(AllocError::ImageTooSmall);
            }
            // 破壊 (S12-b, ext2-alloc-ignore-bitmap): 使用中でも取る。
            // **ビットマップは既に 1 なので変わらず、会計だけが減る。**
            // **e2fsck は会計とビットマップの食い違いとして捕まえる**（実測）。
            #[cfg(not(feature = "ext2-alloc-ignore-bitmap-break"))]
            if image[byte] & mask != 0 {
                continue;
            }
            // **飽和させない。** ビットマップに空きがあるのに会計が 0 なら、
            // **像がそもそも食い違っている。** 黙って 0 のままにすると、
            // **解放で 1 増えて往復が戻らない**（実測で踏んだ）。
            let free = read_u16(image, descriptor + 12)
                .checked_sub(1)
                .ok_or(AllocError::FreeCountInconsistent)?;
            let total = read_u32(image, SUPERBLOCK_OFFSET + 12)
                .checked_sub(1)
                .ok_or(AllocError::FreeCountInconsistent)?;
            image[byte] |= mask;
            // 破壊 (S12-b, ext2-alloc-skip-bg-count): 群の欄を直さない。
            // **e2fsck が `for group #0` 付きで報告する**（実測）。
            #[cfg(not(feature = "ext2-alloc-skip-bg-count-break"))]
            image[descriptor + 12..descriptor + 14].copy_from_slice(&free.to_le_bytes());
            // 破壊 (S12-b, ext2-alloc-skip-sb-count): superblock の欄を直さない。
            // **e2fsck が群の番号なしで報告する**（実測。文言で区別できる）。
            #[cfg(not(feature = "ext2-alloc-skip-sb-count-break"))]
            image[SUPERBLOCK_OFFSET + 12..SUPERBLOCK_OFFSET + 16]
                .copy_from_slice(&total.to_le_bytes());
            return Ok(first + index);
        }
    }
    Err(AllocError::Full)
}

/// ブロックを 1 つ返し、会計も直す（S12-b）。
///
/// **[`allocate_block`] とちょうど逆である**——3 つとも戻す。
/// **戻し方が 1 つでも違えば、往復して像が元へ戻らない。**
pub fn free_block(image: &mut [u8], layout: &Layout, block: u32) -> Result<(), AllocError> {
    let (group, index) = layout
        .locate(block)
        .ok_or(AllocError::BlockOutOfRange(block))?;
    let descriptor = layout
        .descriptor_at(group)
        .ok_or(AllocError::BlockOutOfRange(block))?;
    if descriptor + GROUP_DESCRIPTOR_SIZE > image.len() {
        return Err(AllocError::ImageTooSmall);
    }
    let bitmap_block = read_u32(image, descriptor);
    let bitmap = layout.block_at(bitmap_block)?;
    let (byte, mask) = bitmap_slot(bitmap, index);
    if byte >= image.len() {
        return Err(AllocError::ImageTooSmall);
    }
    if image[byte] & mask == 0 {
        return Err(AllocError::NotAllocated(block));
    }
    let free = read_u16(image, descriptor + 12)
        .checked_add(1)
        .ok_or(AllocError::FreeCountInconsistent)?;
    let total = read_u32(image, SUPERBLOCK_OFFSET + 12)
        .checked_add(1)
        .ok_or(AllocError::FreeCountInconsistent)?;
    // 破壊 (S12-b, ext2-free-skip-bit): ビットを落とさず、会計だけ戻す。
    // **往復しても像が元へ戻らない**——バイト一致が捕まえる。
    #[cfg(not(feature = "ext2-free-skip-bit-break"))]
    {
        image[byte] &= !mask;
    }
    image[descriptor + 12..descriptor + 14].copy_from_slice(&free.to_le_bytes());
    image[SUPERBLOCK_OFFSET + 12..SUPERBLOCK_OFFSET + 16].copy_from_slice(&total.to_le_bytes());
    Ok(())
}

/// ファイルの末尾へ書き足す（S12-c）。**追記だけである。**
///
/// # 上書きは扱わない
///
/// **既にある位置への上書きは会計を動かさない**（ブロックも `i_size` も変わらない）。
/// **S12-c が主張したいのは「割り当てたブロックが inode から参照され、会計が
/// 締まっていること」**なので、**上書きはその主張に何も足さない。**
///
/// # 末尾のブロックの空きを先に使う
///
/// **`i_size` がブロック境界にないなら、末尾のブロックに空きがある。**
/// **そこを埋めてから次を割り当てる。** **見ずに割り当てると、
/// 使わないブロックを取ることになる**（`e2fsck` は文句を言わないが、
/// 空き数が余計に減る）。
///
/// # 直すのは 3 つである
///
/// 中身、`i_size`、`i_blocks`。**`i_blocks` は 512 バイト単位である。**
/// **`i_size` を直さないと `e2fsck` が `i_size is …` と言い、
/// `i_blocks` を直さないと `i_blocks is …` と言う**（実測。文言が違う）。
///
/// # 直接ブロックだけを使う
///
/// **12 個を超えたら断る。** 単一間接は「ブロックを 1 つ余分に取って表を書く」形で、
/// **割り当ての回数と会計の締め方が変わる。** 混ぜると切り分けが利かない。
pub fn append_to_file(
    image: &mut [u8],
    layout: &Layout,
    ino: u32,
    data: &[u8],
) -> Result<(), AllocError> {
    let inode = layout
        .inode_at(image, ino)
        .ok_or(AllocError::BlockOutOfRange(ino))?;
    let size = read_u32(image, inode + 4);
    let block_size = layout.block_size;

    let mut written = 0usize;
    let mut size = size;
    while written < data.len() {
        let offset_in_block = size % block_size;
        let index = size / block_size;
        if index >= DIRECT_BLOCK_COUNT as u32 {
            return Err(AllocError::Full);
        }

        // 破壊 (S12-c, ext2-append-always-allocate): 末尾の空きを見ずに、
        // **毎回割り当てる。** **空き数が余計に減る**ので、外の道具が捕まえる。
        #[cfg(not(feature = "ext2-append-always-allocate-break"))]
        let needs_block = offset_in_block == 0;
        #[cfg(feature = "ext2-append-always-allocate-break")]
        let needs_block = true;

        // **書く枝も読む枝も、同じスロットへ触る。**
        let slot = i_block_slot(inode, index as usize);
        let block = if needs_block {
            let block = allocate_block(image, layout)?;
            // 破壊 (S12-c, ext2-append-skip-link): 取ったブロックを inode へ繋がない。
            // **割り当てたのに誰も参照しないので、`Block bitmap differences` が出る。**
            #[cfg(not(feature = "ext2-append-skip-link"))]
            image[slot..slot + 4].copy_from_slice(&block.to_le_bytes());
            block
        } else {
            read_u32(image, slot)
        };

        let room = (block_size - offset_in_block) as usize;
        let take = room.min(data.len() - written);
        let at = layout.block_at(block)? + offset_in_block as usize;
        if at + take > image.len() {
            return Err(AllocError::ImageTooSmall);
        }
        image[at..at + take].copy_from_slice(&data[written..written + take]);
        written += take;
        size += take as u32;
    }

    // 破壊 (S12-c, ext2-append-skip-size): `i_size` を直さない。
    // **`e2fsck` が `i_size is …` と言い、取り出した中身も短くなる。**
    #[cfg(not(feature = "ext2-append-skip-size"))]
    // 破壊 (S12-c, ext2-append-round-size): ブロック境界へ丸めた値を書く。
    // **`e2fsck` が期待する値そのものなので通り抜ける。** 中身の長さだけが違う。
    {
        #[cfg(not(feature = "ext2-append-round-size"))]
        let stored = size;
        #[cfg(feature = "ext2-append-round-size")]
        let stored = size.div_ceil(block_size) * block_size;
        image[inode + 4..inode + 8].copy_from_slice(&stored.to_le_bytes());
    }

    // `i_blocks` は 512 バイト単位で、使っているブロック数から導く。
    let used = size.div_ceil(block_size);
    // 破壊 (S12-c, ext2-append-blocks-in-bytes): バイト単位で書く。
    // **単位の取り違えで、既存の族と同じ機序である**（`stat-blocks-in-bytes`）。
    #[cfg(not(feature = "ext2-append-blocks-in-bytes"))]
    let sectors = used * layout.sectors_per_block();
    #[cfg(feature = "ext2-append-blocks-in-bytes")]
    let sectors = used * block_size;
    // 破壊 (S12-c, ext2-append-skip-blocks): `i_blocks` を直さない。
    #[cfg(not(feature = "ext2-append-skip-blocks"))]
    image[inode + 28..inode + 32].copy_from_slice(&sectors.to_le_bytes());

    Ok(())
}

/// 追記を巻き戻して、元の大きさへ戻す（S12-c）。
///
/// **truncate の一般形ではない。** **戻す先が「書く前の値」という既知の 1 点だけ**で、
/// **任意の長さへ縮める形は後段である。**
///
/// **書いた中身も 0 で埋め直す**——**ブロックを返すだけでは、
/// 次に同じブロックを取った者が前の中身を見る。**
pub fn truncate_to(
    image: &mut [u8],
    layout: &Layout,
    ino: u32,
    target: u32,
) -> Result<(), AllocError> {
    let inode = layout
        .inode_at(image, ino)
        .ok_or(AllocError::BlockOutOfRange(ino))?;
    let size = read_u32(image, inode + 4);
    let block_size = layout.block_size;
    if target > size {
        return Err(AllocError::NotAllocated(ino));
    }

    // 破壊 (S12-d, ext2-truncate-off-by-one): 1 ブロック余分に返す。
    // **`e2fsck` が参照されているブロックの不足を言う。**
    #[cfg(not(feature = "ext2-truncate-off-by-one"))]
    let keep = target.div_ceil(block_size);
    #[cfg(feature = "ext2-truncate-off-by-one")]
    let keep = target.div_ceil(block_size).saturating_sub(1);

    let have = size.div_ceil(block_size);

    // 破壊 (S12-d, ext2-truncate-always-free): 返す必要が無くても 1 つ返す。
    // **同じブロックの中で縮める道**（会計が動かない道）**を壊す。**
    //
    // **`have` を 1 つ増やす形では破壊にならなかった**（実測）——
    // **file の外のスロットは 0 なので、ループが素通りする。**
    // **実際に参照されているブロックを返さないと、状態が変わらない**（族の 1 つ目）。
    #[cfg(feature = "ext2-truncate-always-free")]
    let keep = if keep == have {
        keep.saturating_sub(1)
    } else {
        keep
    };

    for index in (keep..have).rev() {
        let slot = i_block_slot(inode, index as usize);
        let block = read_u32(image, slot);
        if block == 0 {
            continue;
        }
        let at = layout.block_at(block)?;
        if at + block_size as usize <= image.len() {
            image[at..at + block_size as usize].fill(0);
        }
        // 破壊 (S12-d, ext2-truncate-keep-slot): `i_block` の欄を 0 にしない。
        // **返したブロックを inode がまだ指しているので、
        // `e2fsck` が多重請求として捕まえる。**
        #[cfg(not(feature = "ext2-truncate-keep-slot"))]
        image[slot..slot + 4].copy_from_slice(&0u32.to_le_bytes());
        // 破壊 (S12-d, ext2-truncate-skip-free): ブロックを返さない。
        // **`i_size` だけが縮み、空き数が増えない。**
        #[cfg(not(feature = "ext2-truncate-skip-free"))]
        free_block(image, layout, block)?;
    }

    // **末尾のブロックの、残す長さより後ろも 0 へ戻す。**
    if keep > 0 {
        let last = read_u32(image, i_block_slot(inode, (keep - 1) as usize));
        let tail = target % block_size;
        // 破壊 (S12-d, ext2-truncate-keep-tail): 切った先を 0 で埋めない。
        // **前の中身が残るので、読み戻すと出る。**
        // **埋める前の中身が既に 0 なら効かない**（族の 1 つ目）ので、
        // **追記で書く中身は位置から決まる形にしてある。**
        #[cfg(feature = "ext2-truncate-keep-tail")]
        let tail = 0u32;
        if last != 0 && tail != 0 {
            let at = layout.block_at(last)? + tail as usize;
            let end = at + (block_size - tail) as usize;
            if end <= image.len() {
                image[at..end].fill(0);
            }
        }
    }

    image[inode + 4..inode + 8].copy_from_slice(&target.to_le_bytes());
    let sectors = keep * layout.sectors_per_block();
    // 破壊 (S12-d, ext2-truncate-skip-blocks): `i_blocks` を直さない。
    #[cfg(not(feature = "ext2-truncate-skip-blocks"))]
    image[inode + 28..inode + 32].copy_from_slice(&sectors.to_le_bytes());
    Ok(())
}

/// 名前に許す最大の長さ。**`name_len` が `u8` なので 255 で頭打ちである。**
pub const MAX_NAME_LEN: usize = 255;

/// この名前を持つエントリが占める `rec_len`（S12-e）。
///
/// **固定部 8 バイト + 名前を、4 バイト境界へ切り上げる。**
/// **`rec_len` はこれ以上でなければならない**（[`DirEntries`] が線4 で見ている）。
fn dirent_span(name_len: usize) -> usize {
    (DIRENT_HEADER_LEN + name_len).next_multiple_of(DIRENT_ALIGNMENT as usize)
}

/// 空き inode を 1 つ取り、会計も直す（S12-e）。**取れた番号を返す。**
///
/// # ブロックの側とちょうど同じ形である
///
/// [`allocate_block`] と直すものが対応している——ビットマップのビット、
/// `bg_free_inodes_count`、`s_free_inodes_count`。**違うのは 2 点だけである。**
///
/// - **欄の位置**（群は +14、superblock は +16）
/// - **予約された番号を飛ばす**（`s_first_ino` より小さい番号）
///
/// # `bg_used_dirs_count` は動かさない
///
/// **ディレクトリを作るときだけ動く欄である。** **実測で確かめた**——
/// `debugfs` でファイルを 1 つ作ると `5 directories` のまま、
/// ディレクトリを 1 つ作ると `6 directories` になった。
/// **動かすと `e2fsck` が `Directories count wrong for group #0` と言う**（実測）。
pub fn allocate_inode(image: &mut [u8], layout: &Layout) -> Result<u32, AllocError> {
    for group in 0..layout.group_count {
        let descriptor = layout
            .descriptor_at(group)
            .ok_or(AllocError::ImageTooSmall)?;
        if descriptor + GROUP_DESCRIPTOR_SIZE > image.len() {
            return Err(AllocError::ImageTooSmall);
        }
        let bitmap_block = read_u32(image, descriptor + 4);
        let bitmap = layout.block_at(bitmap_block)?;

        // この群が受け持つ inode 番号（最後の群は端数になりうる）。
        let first = group * layout.inodes_per_group + 1;
        let span = layout
            .inodes_per_group
            .min((layout.inodes_count + 1).saturating_sub(first));
        for index in 0..span {
            let ino = first + index;
            if ino < layout.first_inode {
                continue;
            }
            let (byte, mask) = bitmap_slot(bitmap, index);
            if byte >= image.len() {
                return Err(AllocError::ImageTooSmall);
            }
            if image[byte] & mask != 0 {
                continue;
            }
            // 破壊 (S12-e, ext2-create-skip-inode-bit): 使用中の印を立てない。
            // **inode を配ったのにビットマップが空きのままなので、
            // `e2fsck` が `Inode bitmap differences` を出す。**
            #[cfg(not(feature = "ext2-create-skip-inode-bit"))]
            {
                image[byte] |= mask;
            }
            // 破壊 (S12-e, ext2-create-skip-inode-count): 空き数を直さない。
            // **[`allocate_block`] と違って群と superblock を分けない**——
            // **分けても捕まえ方が同じで、破壊が 1 つ増えるだけだからである**
            // （あちらは文言が違うことを見せるために分けてある）。
            #[cfg(not(feature = "ext2-create-skip-inode-count"))]
            {
                // **飽和させない**（[`allocate_block`] と同じ理由）。
                let group_free = read_u16(image, descriptor + 14)
                    .checked_sub(1)
                    .ok_or(AllocError::FreeCountInconsistent)?;
                let total_free = read_u32(image, SUPERBLOCK_OFFSET + 16)
                    .checked_sub(1)
                    .ok_or(AllocError::FreeCountInconsistent)?;
                image[descriptor + 14..descriptor + 16].copy_from_slice(&group_free.to_le_bytes());
                image[SUPERBLOCK_OFFSET + 16..SUPERBLOCK_OFFSET + 20]
                    .copy_from_slice(&total_free.to_le_bytes());
            }
            return Ok(ino);
        }
    }
    Err(AllocError::Full)
}

/// inode を 1 つ返し、会計も直す（S12-e）。
///
/// **[`allocate_inode`] とちょうど逆である。**
/// **戻し方が 1 つでも違えば、往復して像が元へ戻らない。**
pub fn free_inode(image: &mut [u8], layout: &Layout, ino: u32) -> Result<(), AllocError> {
    if ino == 0 || ino > layout.inodes_count {
        return Err(AllocError::InodeOutOfRange(ino));
    }
    let group = (ino - 1) / layout.inodes_per_group;
    let index = (ino - 1) % layout.inodes_per_group;
    let descriptor = layout
        .descriptor_at(group)
        .ok_or(AllocError::InodeOutOfRange(ino))?;
    if descriptor + GROUP_DESCRIPTOR_SIZE > image.len() {
        return Err(AllocError::ImageTooSmall);
    }
    let bitmap_block = read_u32(image, descriptor + 4);
    let bitmap = layout.block_at(bitmap_block)?;
    let (byte, mask) = bitmap_slot(bitmap, index);
    if byte >= image.len() {
        return Err(AllocError::ImageTooSmall);
    }
    if image[byte] & mask == 0 {
        return Err(AllocError::NotAllocated(ino));
    }
    let group_free = read_u16(image, descriptor + 14)
        .checked_add(1)
        .ok_or(AllocError::FreeCountInconsistent)?;
    let total_free = read_u32(image, SUPERBLOCK_OFFSET + 16)
        .checked_add(1)
        .ok_or(AllocError::FreeCountInconsistent)?;
    image[byte] &= !mask;
    image[descriptor + 14..descriptor + 16].copy_from_slice(&group_free.to_le_bytes());
    image[SUPERBLOCK_OFFSET + 16..SUPERBLOCK_OFFSET + 20]
        .copy_from_slice(&total_free.to_le_bytes());
    Ok(())
}

/// ディレクトリの中の 1 か所（S12-e）。
///
/// **`(そのエントリの像内オフセット, 直前のエントリの像内オフセット)`。**
/// **直前が `None` なのは、ブロックの先頭に在るときである。**
type DirectorySlot = (usize, Option<usize>);

/// ディレクトリの中で名前を探し、入れられる隙間も一緒に見つける（S12-e）。
///
/// 返すのは `(名前の在るエントリ, 隙間を持つエントリ)` である。
///
/// # 1 度で両方を見る
///
/// **[`create_file`] は「名前が既に在るか」を全体について言う必要があり、
/// [`unlink_file`] は「名前の在る位置と、その直前」が要る。**
/// **走査を 2 度書くと、片方だけが壊れた並びの扱いを変える余地が生まれる。**
///
/// # 直接ブロックだけを見る
///
/// **単一間接を辿るディレクトリは扱わない。** 4096 バイトのブロックが 12 個
/// あれば、この像のディレクトリはすべて 1 ブロックで収まっている（実測）。
fn scan_directory(
    image: &[u8],
    layout: &Layout,
    dir: u32,
    name: &[u8],
    want: usize,
) -> Result<(Option<DirectorySlot>, Option<DirectorySlot>), AllocError> {
    let dir_at = layout
        .inode_at(image, dir)
        .ok_or(AllocError::InodeOutOfRange(dir))?;
    if read_u16(image, dir_at) & MODE_FORMAT_MASK != MODE_DIRECTORY {
        return Err(AllocError::NotADirectory(dir));
    }
    let block_size = layout.block_size as usize;
    let size = read_u32(image, dir_at + 4) as usize;
    let blocks = (size / block_size).min(DIRECT_BLOCK_COUNT);

    let mut found = None;
    let mut room = None;
    for index in 0..blocks {
        let block = read_u32(image, i_block_slot(dir_at, index));
        if block == 0 {
            continue;
        }
        let base = layout.block_at(block)?;
        if base + block_size > image.len() {
            return Err(AllocError::ImageTooSmall);
        }
        let end = base + block_size;
        let mut at = base;
        let mut previous = None;
        while at + DIRENT_HEADER_LEN <= end {
            let entry = read_u32(image, at);
            let rec_len = usize::from(read_u16(image, at + 4));
            let name_len = usize::from(image[at + 6]);
            // **読む側と同じ 3 つを見る**（線4 の要である `rec_len` の下限を含む）。
            // **書く前に壊れた並びを見つけたら、そこで止める。**
            if !rec_len.is_multiple_of(usize::from(DIRENT_ALIGNMENT))
                || rec_len < DIRENT_HEADER_LEN + name_len
                || at + rec_len > end
            {
                return Err(AllocError::DirectoryCorrupt);
            }
            if entry != 0
                && &image[at + DIRENT_HEADER_LEN..at + DIRENT_HEADER_LEN + name_len] == name
            {
                found = Some((at, previous));
            }
            // **使われているエントリの後ろの余りだけを見る。**
            // **`inode == 0` の枠は、この設計では現れない**——
            // [`unlink_file`] は隙間を前のエントリへ吸わせるので、枠が残らない。
            //
            // **この引き算が桁借りしないことは、上の 3 つの検査に依存している。**
            // `rec_len` は 4 の倍数で、かつ `8 + name_len` 以上である。
            // **`dirent_span(name_len)` は `8 + name_len` を 4 の倍数へ切り上げた値**
            // なので、**その 2 つが成り立てば `rec_len >= dirent_span(name_len)` になる。**
            // **上の検査を 1 つでも外すなら、ここを `checked_sub` にすること**——
            // **外した人が、引き算の破れに気づける場所がここしか無い。**
            if room.is_none() && entry != 0 && rec_len - dirent_span(name_len) >= want {
                room = Some((at, previous));
            }
            previous = Some(at);
            at += rec_len;
        }
    }
    Ok((found, room))
}

/// ディレクトリへ通常ファイルを 1 つ作る（S12-e）。**取れた inode 番号を返す。**
///
/// # 中身は空である
///
/// **inode を取って、初期化して、ディレクトリへ繋ぐところまでである。**
/// **中身は [`append_to_file`] が書く**——**割り当てと追記を混ぜると、
/// 会計がどちらの誤りで狂ったのかが切り分けられない。**
///
/// # 直すのは 4 つである
///
/// inode ビットマップと 2 つの空き数（[`allocate_inode`] が見る）、
/// inode の中身、ディレクトリのエントリ、そして前のエントリの `rec_len`。
///
/// # ディレクトリは伸ばさない
///
/// **隙間が無ければ [`AllocError::NoRoomInDirectory`] を返す。**
/// **伸ばす形はブロックの割り当てと `i_size` の更新が入り、
/// [`append_to_file`] と同じ算術をディレクトリでもう一度書くことになる。**
/// **要るようになったら足す**（この像のディレクトリは 1 ブロックに収まっている）。
///
/// # `rec_len` が 0 になる道を作らない
///
/// **割る前の `rec_len` から、前のエントリが実際に使う分を引いた残りを渡す。**
/// **残りが新しいエントリの分に足りないなら、そもそも隙間として選ばれない。**
/// **したがって両方とも 8 以上である**（[`DirEntries`] の線4 が要求する下限）。
pub fn create_file(
    image: &mut [u8],
    layout: &Layout,
    dir: u32,
    name: &[u8],
) -> Result<u32, AllocError> {
    if name.is_empty()
        || name.len() > MAX_NAME_LEN
        || name.contains(&PATH_SEPARATOR)
        || name == b"."
        || name == b".."
    {
        return Err(AllocError::BadName);
    }
    let want = dirent_span(name.len());
    let (found, room) = scan_directory(image, layout, dir, name, want)?;
    if found.is_some() {
        return Err(AllocError::NameTaken);
    }
    let (previous, _) = room.ok_or(AllocError::NoRoomInDirectory)?;

    // **割った後の 2 つの `rec_len` を先に出す。** どちらも 0 にならないことは、
    // 隙間の選び方（`rec_len - dirent_span(name_len) >= want`）から出ている。
    let previous_len = dirent_span(usize::from(image[previous + 6]));
    let entry_len = usize::from(read_u16(image, previous + 4)) - previous_len;

    let ino = allocate_inode(image, layout)?;

    // **枠ごと 0 にしてから書く。** **空いている inode の枠の中身は決まっていない**
    // ——取った枠に前の住人が残っていると、直さない欄がそのまま生き返る。
    let at = layout
        .inode_at(image, ino)
        .ok_or(AllocError::InodeOutOfRange(ino))?;
    image[at..at + usize::from(layout.inode_size)].fill(0);
    image[at..at + 2].copy_from_slice(&(MODE_REGULAR | 0o644).to_le_bytes());

    // **追加領域の大きさを名乗る（S12-f-3）。**
    //
    // **像そのものが `s_min_extra_isize` を宣言している**ので、
    // **0 のままだと、像が自分で宣言した約束を、こちらが作った inode だけが破る。**
    // **`e2fsck` は 0 を受理する**（実測。範囲外は拒むので、欄は見ている）が、
    // **それは道具の寛容さに乗っているだけである。**
    //
    // **中身は書かない。** **追加領域に在るのは時刻まわりの欄で、
    // S12-f-2 で「時刻は書かない」と決めた。** **枠は 0 で埋めてあるので、
    // `mke2fs` が建てて `build.rs` が潰した inode と同じ形になる。**
    //
    // **32 を直に書かない**——`s_want_extra_isize` から来る値である
    // （像の中の数を実装にも判定にも埋めない）。
    //
    // 破壊 (S12-f-3, ext2-create-skip-extra-isize): 名乗らない。
    // **`e2fsck` は通り抜ける**（0 を受理する）。**判定だけが落ちる。**
    #[cfg(not(feature = "ext2-create-skip-extra-isize"))]
    if layout.want_extra_isize != 0 {
        image[at + INODE_EXTRA_ISIZE..at + INODE_EXTRA_ISIZE + 2]
            .copy_from_slice(&layout.want_extra_isize.to_le_bytes());
    }

    // 破壊 (S12-e, ext2-create-skip-links): `i_links_count` を 0 のままにする。
    // **ディレクトリから指されているのに参照が 0 なので、
    // `e2fsck` が `Inode … ref count is 0, should be 1` と言う。**
    #[cfg(not(feature = "ext2-create-skip-links"))]
    image[at + 26..at + 28].copy_from_slice(&1u16.to_le_bytes());

    // 破壊 (S12-e, ext2-create-keep-prev-rec-len): 前のエントリを縮めない。
    // **新しいエントリが前の `rec_len` の内側に入るので、走査が素通りする。**
    // **ext2 として不整合ではない**（隙間の中身は自由である）——
    // **`e2fsck` は「どこからも指されていない inode」として捕まえ、
    // `debugfs` は名前を引けない。**
    #[cfg(not(feature = "ext2-create-keep-prev-rec-len"))]
    image[previous + 4..previous + 6].copy_from_slice(&(previous_len as u16).to_le_bytes());

    let entry = previous + previous_len;
    image[entry..entry + 4].copy_from_slice(&ino.to_le_bytes());
    image[entry + 4..entry + 6].copy_from_slice(&(entry_len as u16).to_le_bytes());
    image[entry + 6] = name.len() as u8;
    image[entry + 7] = DIRENT_TYPE_REGULAR;
    image[entry + DIRENT_HEADER_LEN..entry + DIRENT_HEADER_LEN + name.len()].copy_from_slice(name);

    // 破壊 (S12-e, ext2-create-move-dirs-count): ファイルでも `bg_used_dirs_count`
    // を動かす。**動くのはディレクトリのときだけである**（実測）。
    #[cfg(feature = "ext2-create-move-dirs-count")]
    {
        let group = (ino - 1) / layout.inodes_per_group;
        if let Some(descriptor) = layout.descriptor_at(group) {
            let dirs = read_u16(image, descriptor + 16).wrapping_add(1);
            image[descriptor + 16..descriptor + 18].copy_from_slice(&dirs.to_le_bytes());
        }
    }

    Ok(ino)
}

/// ディレクトリから通常ファイルを 1 つ消す（S12-e）。**[`create_file`] の逆である。**
///
/// # 逆であることが主張の中身である
///
/// **作って消せば、像はバイト単位で元へ戻るはずである。**
/// **`e2fsck` は使われていない場所の中身を見ない**ので、
/// **返し過ぎ・消し残しは往復でしか捕まらない**（S12-d で実測した）。
///
/// # 戻すのは 4 つである
///
/// 中身のブロック（[`truncate_to`] が返す）、inode の枠、inode ビットマップと
/// 空き数（[`free_inode`] が戻す）、ディレクトリの隙間。
///
/// # 隙間は前のエントリへ吸わせる
///
/// **`inode = 0` の枠として残す形も ext2 として正しい**（Linux もそう書く場面がある）。
/// **採らないのは、それでは像が元へ戻らないからである**——
/// 割った跡が `rec_len` の並びに残る。**破壊としてその形を立ててある。**
///
/// # ディレクトリは消さない
///
/// **`.` と `..` の始末と、親の `i_links_count` を戻す処理が要る。**
/// **作る側がディレクトリを作らないので、消す側にも要らない**（対を保つ）。
pub fn unlink_file(
    image: &mut [u8],
    layout: &Layout,
    dir: u32,
    name: &[u8],
) -> Result<(), AllocError> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(AllocError::BadName);
    }
    let (found, _) = scan_directory(image, layout, dir, name, usize::MAX)?;
    let (entry, previous) = found.ok_or(AllocError::NoSuchEntry)?;
    let previous = previous.ok_or(AllocError::NoPreviousEntry)?;
    let ino = read_u32(image, entry);

    let at = layout
        .inode_at(image, ino)
        .ok_or(AllocError::InodeOutOfRange(ino))?;
    if read_u16(image, at) & MODE_FORMAT_MASK != MODE_REGULAR {
        return Err(AllocError::NotARegularFile(ino));
    }

    // **中身を返してから枠を消す。** 逆にすると `i_block` が読めなくなり、
    // **持っていたブロックが誰からも参照されないまま使用中に残る。**
    truncate_to(image, layout, ino, 0)?;
    image[at..at + usize::from(layout.inode_size)].fill(0);
    free_inode(image, layout, ino)?;

    let entry_len = usize::from(read_u16(image, entry + 4));
    // 破壊 (S12-e, ext2-unlink-mark-unused): 前のエントリへ吸わせず、
    // **`inode = 0` の枠として残す。** **`e2fsck` は無傷と判定する**——
    // **ext2 として不整合ではないからである。往復のバイト一致だけが捕まえる。**
    #[cfg(feature = "ext2-unlink-mark-unused")]
    {
        // **前を探す検査そのものは残す**——**破壊で通る道が増えると、
        // 何を壊したのかが 1 つに絞れなくなる。**
        let _ = (previous, entry_len);
        image[entry..entry + 4].copy_from_slice(&0u32.to_le_bytes());
    }
    #[cfg(not(feature = "ext2-unlink-mark-unused"))]
    {
        let merged = u16::try_from(usize::from(read_u16(image, previous + 4)) + entry_len)
            .map_err(|_| AllocError::DirectoryCorrupt)?;
        image[previous + 4..previous + 6].copy_from_slice(&merged.to_le_bytes());
        // **書いたバイトを消す。** **隙間の中身は ext2 として自由なので、
        // 残しても `e2fsck` は何も言わない**——**往復のバイト一致のためである。**
        image[entry..entry + entry_len].fill(0);
    }
    Ok(())
}

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
        // **使っているブロックにビットを立てる（S12-c）。**
        //
        // **立てていなかった。** そのため [`allocate_block`] がブロック 0
        // （superblock）を返し、**追記がテスト像そのものを壊していた**
        // （実測。往復のバイト一致が落ちて気づいた）。
        //
        // **この像が使うのは 60 番までである**（メタデータ・ルート・
        // 各ファイルのブロック）。**まとめて立てる**——1 つずつ数えると、
        // ファイルを足したときに合わなくなる。
        let bitmap = 2 * 4096;
        for block in 0..=60u32 {
            image[bitmap + (block / 8) as usize] |= 1 << (block % 8);
        }

        // **使っている inode にビットを立てる（S12-e）。**
        //
        // **ブロックの側と同じ理由である**——立てないと [`allocate_inode`] が
        // **既に住人の居る枠を返し、作成がテスト像そのものを壊す。**
        // **この像が使うのは 18 番までである**（ルート・各ディレクトリ・各ファイル）。
        let inode_bitmap = 3 * 4096;
        for ino in 1..=18u32 {
            image[inode_bitmap + ((ino - 1) / 8) as usize] |= 1 << ((ino - 1) % 8);
        }

        // 空き数の3欄（S12-b）。**別々の値を書く**——同じ値だと、
        // **欄を取り違えても気づけない。**
        image[table + 12..table + 14].copy_from_slice(&7u16.to_le_bytes());
        image[table + 14..table + 16].copy_from_slice(&5u16.to_le_bytes());
        image[table + 16..table + 18].copy_from_slice(&3u16.to_le_bytes());

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
    /// `/data` の中身が在るブロック（S12-e。`write_subdirectory` へ渡す番号と同じ）。
    const DATA_DATA_BLOCK: u32 = 23;
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
        // `i_blocks` は 512 バイト単位である。**ブロックサイズ単位ではない。**
        let sectors = (blocks.len() as u32) * (4096 / 512);
        image[at + 28..at + 32].copy_from_slice(&sectors.to_le_bytes());
        // `i_extra_isize`（S12-f-3）。**実測した像では全 inode が 32 である**
        // （`debugfs`の`Size of extra inode fields`）。**置かないと、
        // 「こちらの inode が像の他と揃っている」を見る検査が、
        // 両方 0 で通ってしまう**（実測で踏んだ——族の1つ目である）。
        image[at + INODE_EXTRA_ISIZE..at + INODE_EXTRA_ISIZE + 2]
            .copy_from_slice(&32u16.to_le_bytes());
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
        put32(image, 12, 9); // s_free_blocks_count（S12-b。群の欄 7 とは別の値にする）
        put32(image, 16, 6); // s_free_inodes_count（S12-e。群の欄 5 とは別の値にする）
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
                                 // `s_want_extra_isize`（S12-f-3）。**実測した像と同じ 32 である**
                                 // （`dumpe2fs`の`Desired extra isize`）。**`s_min_extra_isize`(348)も同じ値だが、
                                 // 書く側が見るのは`want`のほうなので、そちらだけを置く。**
        put16(image, 350, 32);
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

    /// テスト像の中の、追記できる通常ファイル。
    /// **1 ブロックに満たないので、末尾に空きがある側である。**
    const TEST_WRITABLE_INO: u32 = SHORT_FILE_INODE;

    #[test]
    fn appending_within_the_last_block_does_not_allocate() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        let free_before = Ext2::parse(&image).unwrap().free_blocks_count();

        // **末尾のブロックに空きがあるので、割り当ては起きないはず。**
        append_to_file(&mut image, &layout, TEST_WRITABLE_INO, &[0xAB; 8]).unwrap();

        assert_eq!(
            Ext2::parse(&image).unwrap().free_blocks_count(),
            free_before,
            "末尾の空きを使うので空き数が動かないこと"
        );
    }

    #[test]
    fn appending_past_the_block_boundary_allocates_a_block_per_boundary() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        let free_before = Ext2::parse(&image).unwrap().free_blocks_count();

        // **末尾のブロックの残りを越える量を足す。**
        // 元が 1 ブロックに満たないので、足した後は 2 ブロックになる。
        // **したがって新しく取るのは 1 つである**（末尾の空きを先に使う）。
        let big = std::vec![0xCDu8; 5000];
        append_to_file(&mut image, &layout, TEST_WRITABLE_INO, &big).unwrap();

        let after = Ext2::parse(&image).unwrap().free_blocks_count();
        assert_eq!(after + 1, free_before, "境界を越えた分だけ取ること");

        // **書いた中身が読み戻せること。**
        //
        // **空き数の差だけでは足りない。** あれは会計の主張で、
        // **`allocating_moves_both_free_counts_by_one` が既に言っている。**
        // **ここが言うべきなのは「取ったブロックが inode から参照され、
        // 書いた中身がそこに在る」ことである。**
        //
        // **実測でこの穴を踏んだ**——テスト像がビットマップを立てておらず、
        // **割り当てがブロック 0（superblock）を返して像を潰していたのに、
        // このテストは通っていた**（空き数は 1 つ減るので）。
        let fs = Ext2::parse(&image).unwrap();
        let inode = fs.inode(TEST_WRITABLE_INO).unwrap();
        let mut read_back = std::vec::Vec::new();
        let mut left = inode.size as usize;
        let mut index = 0u32;
        while left > 0 {
            let block = fs.file_block(&inode, index).unwrap();
            let take = left.min(block.len());
            read_back.extend_from_slice(&block[..take]);
            left -= take;
            index += 1;
        }
        assert_eq!(
            &read_back[read_back.len() - big.len()..],
            &big[..],
            "追記した中身が、inode の指すブロックから読み戻せること"
        );
    }

    #[test]
    fn appending_then_truncating_restores_the_image_byte_for_byte() {
        let mut image = build_test_image();
        let original = image.clone();
        let layout = Ext2::parse(&image).unwrap().layout();
        let size_before = {
            let fs = Ext2::parse(&image).unwrap();
            fs.inode(TEST_WRITABLE_INO).unwrap().size as u32
        };

        append_to_file(&mut image, &layout, TEST_WRITABLE_INO, &[0xEF; 5000]).unwrap();
        assert_ne!(image, original, "追記で像が変わること");

        truncate_to(&mut image, &layout, TEST_WRITABLE_INO, size_before).unwrap();
        assert_eq!(image, original, "戻したら1バイトも違わないこと");
    }

    #[test]
    fn allocating_then_freeing_restores_the_image_byte_for_byte() {
        let mut image = build_test_image();
        let original = image.clone();
        let layout = Ext2::parse(&image).unwrap().layout();

        let block = allocate_block(&mut image, &layout).unwrap();
        // **割り当てた時点では像が違う。** 違わなければ、割り当てが効いていない。
        assert_ne!(image, original, "割り当てで像が変わること");

        free_block(&mut image, &layout, block).unwrap();
        // **戻したら 1 バイトも違わない。** ビットも 2 つの会計も、
        // ちょうど逆へ戻っている。
        assert_eq!(image, original, "往復で像が元へ戻ること");
    }

    #[test]
    fn allocating_moves_both_free_counts_by_one() {
        let mut image = build_test_image();
        let before = Ext2::parse(&image).unwrap();
        let layout = before.layout();
        let (sb_before, bg_before) = (
            before.free_blocks_count(),
            before.group_descriptor(0).unwrap().free_blocks_count,
        );

        allocate_block(&mut image, &layout).unwrap();

        let after = Ext2::parse(&image).unwrap();
        assert_eq!(after.free_blocks_count(), sb_before - 1, "superblock 側");
        assert_eq!(
            after.group_descriptor(0).unwrap().free_blocks_count,
            bg_before - 1,
            "group 側"
        );
    }

    #[test]
    fn freeing_a_block_that_is_not_allocated_is_refused() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        let block = allocate_block(&mut image, &layout).unwrap();
        free_block(&mut image, &layout, block).unwrap();
        // **2 回目は断る。** 断らないと会計だけが増え、像が壊れる。
        assert_eq!(
            free_block(&mut image, &layout, block),
            Err(AllocError::NotAllocated(block))
        );
    }

    /// 作る先のディレクトリ（テスト像の `/data`）。
    /// **最後のエントリがブロックの終わりまで伸びているので、隙間がある側である。**
    const TEST_DIRECTORY_INO: u32 = DATA_INODE;

    #[test]
    fn creating_then_unlinking_restores_the_image_byte_for_byte() {
        let mut image = build_test_image();
        let original = image.clone();
        let layout = Ext2::parse(&image).unwrap().layout();

        let ino = create_file(&mut image, &layout, TEST_DIRECTORY_INO, b"created").unwrap();
        assert_ne!(image, original, "作成で像が変わること");
        // **中身も書く。** **空のまま消すと、ブロックを返す道を通らない。**
        append_to_file(&mut image, &layout, ino, &[0x5A; 300]).unwrap();

        unlink_file(&mut image, &layout, TEST_DIRECTORY_INO, b"created").unwrap();
        assert_eq!(image, original, "往復で像が元へ戻ること");
    }

    #[test]
    fn a_created_inode_names_the_extra_area_the_way_the_image_asks() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        let want = Ext2::parse(&image).unwrap().want_extra_isize();
        // **テスト像は `mke2fs` と同じ 32 を宣言している。**
        // **0 だと、この検査は何も主張しなくなる**（族の1つ目）。
        assert_ne!(want, 0, "像が s_want_extra_isize を宣言していること");

        let ino = create_file(&mut image, &layout, TEST_DIRECTORY_INO, b"created").unwrap();
        let at = layout.inode_at(&image, ino).unwrap();
        assert_eq!(read_u16(&image, at + INODE_EXTRA_ISIZE), want);
        // **像の他の inode と同じ値であること。**
        let other = layout.inode_at(&image, DIRECT_FILE_INODE).unwrap();
        assert_eq!(
            read_u16(&image, at + INODE_EXTRA_ISIZE),
            read_u16(&image, other + INODE_EXTRA_ISIZE)
        );
    }

    #[test]
    fn an_extra_size_that_does_not_fit_the_inode_falls_back_to_zero() {
        let mut image = build_test_image();
        // **inode に収まらない値を宣言させる**（128 + 200 > 256）。
        image[SUPERBLOCK_OFFSET + SUPERBLOCK_WANT_EXTRA_ISIZE
            ..SUPERBLOCK_OFFSET + SUPERBLOCK_WANT_EXTRA_ISIZE + 2]
            .copy_from_slice(&200u16.to_le_bytes());
        // **拒まない。0 へ倒れる**——0 は「追加領域を持たない」という妥当な ext2 である。
        let fs = Ext2::parse(&image).unwrap();
        assert_eq!(fs.want_extra_isize(), 0);

        let layout = fs.layout();
        let ino = create_file(&mut image, &layout, TEST_DIRECTORY_INO, b"created").unwrap();
        let at = layout.inode_at(&image, ino).unwrap();
        assert_eq!(read_u16(&image, at + INODE_EXTRA_ISIZE), 0);
    }

    #[test]
    fn an_image_too_short_to_reach_the_field_is_still_accepted() {
        // **`SUPERBLOCK_MIN_LEN` を伸ばしていないので、受理する範囲は変わらない。**
        // **欄へ届かない像では 0 になるだけである。**
        let image = build_test_image();
        let short = &image[..SUPERBLOCK_OFFSET + SUPERBLOCK_MIN_LEN + 8];
        // 像そのものは短すぎて別の理由で拒まれるので、欄の位置だけを確かめる。
        assert!(short.len() < SUPERBLOCK_OFFSET + SUPERBLOCK_WANT_EXTRA_ISIZE + 2);
        assert!(Ext2::parse(short).is_err());
    }

    #[test]
    fn creating_moves_both_free_inode_counts_by_one() {
        let mut image = build_test_image();
        let before = Ext2::parse(&image).unwrap();
        let layout = before.layout();
        let (sb_before, bg_before) = (
            before.free_inodes_count(),
            before.group_descriptor(0).unwrap().free_inodes_count,
        );

        create_file(&mut image, &layout, TEST_DIRECTORY_INO, b"created").unwrap();

        let after = Ext2::parse(&image).unwrap();
        assert_eq!(after.free_inodes_count(), sb_before - 1, "superblock 側");
        assert_eq!(
            after.group_descriptor(0).unwrap().free_inodes_count,
            bg_before - 1,
            "group 側"
        );
    }

    #[test]
    fn creating_a_file_leaves_the_directory_count_alone() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        let before = Ext2::parse(&image)
            .unwrap()
            .group_descriptor(0)
            .unwrap()
            .used_dirs_count;

        create_file(&mut image, &layout, TEST_DIRECTORY_INO, b"created").unwrap();

        // **ディレクトリを作ったときだけ動く欄である**（実測で確かめた。
        // `debugfs` でファイルを作ると動かず、ディレクトリを作ると 1 増えた）。
        assert_eq!(
            Ext2::parse(&image)
                .unwrap()
                .group_descriptor(0)
                .unwrap()
                .used_dirs_count,
            before
        );
    }

    #[test]
    fn the_new_entry_is_walkable_and_no_record_length_is_zero() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        create_file(&mut image, &layout, TEST_DIRECTORY_INO, b"created").unwrap();

        // **走査そのものに読ませる。** **`rec_len` が 0 なら線4 の検査が
        // `DirEntryRecordTooSmall` を返すので、名前が出る前に落ちる。**
        let fs = Ext2::parse(&image).unwrap();
        let inode = fs.inode(TEST_DIRECTORY_INO).unwrap();
        let names: std::vec::Vec<std::vec::Vec<u8>> = fs
            .directory_entries(&inode)
            .unwrap()
            .map(|entry| entry.expect("エントリの並びが読めること").name.to_vec())
            .collect();
        assert!(names.iter().any(|name| name == b"created"), "{names:?}");
        // **元から在ったものが消えていないこと。** 前のエントリを縮めるので、
        // **縮め過ぎれば直前の名前が読めなくなる。**
        assert!(names.iter().any(|name| name == b"indirect-first"));
        assert!(names.iter().any(|name| name == b"."));

        // 引ける形になっていること。
        let found = fs.lookup(b"/data/created").unwrap();
        assert!(found.is_regular_file());
        assert_eq!(found.size, 0);
    }

    #[test]
    fn a_name_that_is_already_there_is_refused() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        let original = image.clone();
        assert_eq!(
            create_file(&mut image, &layout, TEST_DIRECTORY_INO, b"direct-max"),
            Err(AllocError::NameTaken)
        );
        // **断るときは何も書かない。** inode を取ってから断ると、取った分が漏れる。
        assert_eq!(image, original);
    }

    #[test]
    fn names_that_cannot_be_written_are_refused() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        for name in [&b""[..], b".", b"..", b"a/b", &[b'x'; MAX_NAME_LEN + 1]] {
            assert_eq!(
                create_file(&mut image, &layout, TEST_DIRECTORY_INO, name),
                Err(AllocError::BadName),
                "{name:?}"
            );
        }
    }

    #[test]
    fn creating_in_something_that_is_not_a_directory_is_refused() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        assert_eq!(
            create_file(&mut image, &layout, SHORT_FILE_INODE, b"created"),
            Err(AllocError::NotADirectory(SHORT_FILE_INODE))
        );
    }

    #[test]
    fn unlinking_a_name_that_is_not_there_is_refused() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        assert_eq!(
            unlink_file(&mut image, &layout, TEST_DIRECTORY_INO, b"missing"),
            Err(AllocError::NoSuchEntry)
        );
    }

    #[test]
    fn unlinking_a_directory_is_refused() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        // **`..` は前が在るので `NoPreviousEntry` では止まらない。**
        // **止めているのは種別の検査である。**
        assert_eq!(
            unlink_file(&mut image, &layout, TEST_DIRECTORY_INO, b".."),
            Err(AllocError::NotARegularFile(ROOT_INODE))
        );
        // **`.` はブロックの先頭に在るので、前が無い側で止まる。**
        assert_eq!(
            unlink_file(&mut image, &layout, TEST_DIRECTORY_INO, b"."),
            Err(AllocError::NoPreviousEntry)
        );
    }

    #[test]
    fn reserved_inode_numbers_are_never_handed_out() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        // **予約された番号のビットを落としても、配らないこと。**
        // **ビットマップの印に頼っていたら、ここで 3 番が返る。**
        let inode_bitmap = 3 * 4096;
        image[inode_bitmap] = 0;
        image[inode_bitmap + 1] = 0;
        let ino = allocate_inode(&mut image, &layout).unwrap();
        assert!(ino >= layout.first_inode, "{ino}");
    }

    #[test]
    fn freeing_an_inode_that_is_not_allocated_is_refused() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        let ino = allocate_inode(&mut image, &layout).unwrap();
        free_inode(&mut image, &layout, ino).unwrap();
        assert_eq!(
            free_inode(&mut image, &layout, ino),
            Err(AllocError::NotAllocated(ino))
        );
    }

    #[test]
    fn allocating_then_freeing_an_inode_restores_the_image_byte_for_byte() {
        let mut image = build_test_image();
        let original = image.clone();
        let layout = Ext2::parse(&image).unwrap().layout();

        let ino = allocate_inode(&mut image, &layout).unwrap();
        assert_ne!(image, original, "割り当てで像が変わること");
        free_inode(&mut image, &layout, ino).unwrap();
        assert_eq!(image, original, "往復で像が元へ戻ること");
    }

    #[test]
    fn a_directory_that_has_no_room_is_refused() {
        let mut image = build_test_image();
        let layout = Ext2::parse(&image).unwrap().layout();
        // **ブロックを隙間なく埋める。** **最後の `rec_len` を詰めるだけでは
        // 後ろがゼロで残り、「隙間が無い」ではなく「並びが読めない」になる**
        // （実測で踏んだ。`DirectoryCorrupt` が返った）。
        //
        // `.` と `..` に 16 バイトずつ持たせ、残りの 4064 を 32 バイトずつ配る。
        // **どのエントリも余りが `created` の 16 バイトに足りない。**
        let base = DATA_DATA_BLOCK as usize * 4096;
        image[base..base + 4096].fill(0);
        let mut at = base;
        let put = |image: &mut [u8], at: usize, ino: u32, rec_len: u16, ty: u8, name: &[u8]| {
            image[at..at + 4].copy_from_slice(&ino.to_le_bytes());
            image[at + 4..at + 6].copy_from_slice(&rec_len.to_le_bytes());
            image[at + 6] = name.len() as u8;
            image[at + 7] = ty;
            image[at + DIRENT_HEADER_LEN..at + DIRENT_HEADER_LEN + name.len()]
                .copy_from_slice(name);
        };
        put(
            &mut image,
            at,
            TEST_DIRECTORY_INO,
            16,
            DIRENT_TYPE_DIRECTORY,
            b".",
        );
        at += 16;
        put(&mut image, at, ROOT_INODE, 16, DIRENT_TYPE_DIRECTORY, b"..");
        at += 16;
        // 名前は 24 バイトで、固定部と合わせて 32 バイトちょうどになる。
        for index in 0..(4096 - 32) / 32 {
            let mut name = [b'f'; 24];
            name[0] = b'0' + (index % 10) as u8;
            put(
                &mut image,
                at,
                DIRECT_FILE_INODE,
                32,
                DIRENT_TYPE_REGULAR,
                &name,
            );
            at += 32;
        }
        assert_eq!(at, base + 4096, "ブロックを使い切っていること");
        assert_eq!(
            create_file(&mut image, &layout, TEST_DIRECTORY_INO, b"created"),
            Err(AllocError::NoRoomInDirectory)
        );
    }

    #[test]
    fn a_directory_whose_entries_do_not_parse_is_refused_before_anything_is_written() {
        let mut image = build_test_image();
        let original = image.clone();
        let layout = Ext2::parse(&image).unwrap().layout();
        // **`rec_len` を 4 の倍数でない値にする。**
        let entries: &[(u32, u8, &[u8])] = &[
            (TEST_DIRECTORY_INO, DIRENT_TYPE_DIRECTORY, b"."),
            (ROOT_INODE, DIRENT_TYPE_DIRECTORY, b".."),
        ];
        let at = dirent_offset(DATA_DATA_BLOCK, 1, entries);
        image[at + 4..at + 6].copy_from_slice(&13u16.to_le_bytes());
        let poisoned = image.clone();
        assert_eq!(
            create_file(&mut image, &layout, TEST_DIRECTORY_INO, b"created"),
            Err(AllocError::DirectoryCorrupt)
        );
        assert_eq!(image, poisoned, "断るときは何も書かないこと");
        assert_ne!(image, original, "壊した像であること");
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
                free_blocks_count: 7,
                free_inodes_count: 5,
                used_dirs_count: 3,
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
        assert_eq!(root.blocks_512, 8, "one 4096-byte block is 8 sectors");
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

    /// **`next_offset` から走査を再開できる。**
    ///
    /// **`getdents64` が続きを読むときに使う値である。**
    #[test]
    fn the_walk_can_resume_from_the_offset_it_reported() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).unwrap();

        // 3 つ目まで読み、そこで止めて位置を覚える。
        let mut walker = fs.directory_entries(&root).unwrap();
        let mut resume = 0u64;
        for _ in 0..3 {
            resume = walker.next().unwrap().unwrap().next_offset;
        }

        // 覚えた位置から続ける。**残りだけが返る。**
        let rest: std::vec::Vec<&[u8]> = fs
            .directory_entries_from(&root, resume)
            .unwrap()
            .map(|e| e.unwrap().name)
            .collect();
        assert_eq!(rest, std::vec![&b"bin"[..], &b"data"[..], &b"etc"[..]]);

        // 0 から始めれば全部返る（**再開の位置が効いていることの対照**）。
        let all = fs.directory_entries_from(&root, 0).unwrap().count();
        assert_eq!(all, ROOT_ENTRIES.len());
    }

    /// 範囲外から再開しても、エントリは 1 つも返らない。**パニックしない。**
    #[test]
    fn resuming_past_the_end_yields_nothing() {
        let image = build_test_image();
        let fs = Ext2::parse(&image).unwrap();
        let root = fs.inode(ROOT_INODE).unwrap();
        for from in [4096u64, 4097, 1 << 40, u64::MAX] {
            let count = fs
                .directory_entries_from(&root, from)
                .unwrap()
                .filter(|e| e.is_ok())
                .count();
            assert_eq!(count, 0, "resuming from {from} must yield nothing");
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
