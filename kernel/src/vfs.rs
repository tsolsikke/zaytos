//! 最小の VFS（S10-b）。
//!
//! **`inode` と `file` の概念だけを置く。** trait は引かず、`dentry` も置かない
//! （`docs/roadmap.md` の S10 で決めた）。**名前と概念は Linux に寄せ、
//! 間接は入れない。**
//!
//! # trait を引かない理由
//!
//! **実装が 1 つしかない段階の境界は、形を誤りやすい。** 抽出の条件は
//! **「2 つ目のファイルシステムを足すとき、または同じ操作を 2 箇所で書いたとき」**
//! である。**どちらもまだ起きていない。**
//!
//! # `dentry` を置かない理由
//!
//! あれは**キャッシュと参照カウントが目的の構造**である。**引く回数が問題に
//! なっていないので、パス解決は毎回ルートから辿れば足りる**
//! （`common::ext2::Ext2::lookup`）。
//!
//! # Linux との対応
//!
//! | ここ | Linux |
//! |---|---|
//! | [`Inode`] | `struct inode`（+ `ext2_inode_info`） |
//! | [`File`] | `struct file` |
//! | [`FileTable`] | `files_struct` |
//!
//! **Linux は VFS の `inode` と ext2 のオンディスク inode を別の型に分け、
//! `ext2_inode_info` で繋いでいる。** ここでは繋ぎを置かず、
//! **[`Inode`] が [`common::ext2::Inode`] を直に持つ。**
//! **2 つ目のファイルシステムを足すときに、そこが分かれる場所である。**

use common::ext2;

/// 1 つのプロセスが同時に開けるファイルの数（S10-b）。
///
/// # なぜ 16 か
///
/// **見込みの最大は 3 である。** この先で開く場面は
/// **`cat /etc/motd`（ファイル 1 本）**と**`ls`（ディレクトリ 1 本）**、
/// そして **S11 のシェルがリダイレクトを持つ場合の 3 本**である
/// （`docs/roadmap.md` の S11 の到達条件が `ls` と `cat` と `/bin/hello`）。
/// **16 は見込みの最大の 5 倍を超える余裕である。**
///
/// # なぜ固定配列か
///
/// **このカーネルの様式に合う**（`MAX_CPUS`・`WORKER_COUNT`・`TASK_COUNT`・
/// `MAX_PATH_COMPONENTS`、いずれも固定）。そして
/// **S6-c の 59 標本で裏づけた「定常経路に解放されない確保は無い」が守れる**
/// ——ヒープに載せると、開閉を繰り返す経路が新しい漂流の面になる。
///
/// **`ADR-0012` にもヒープにも触れずに閉じている。** S10-a はヒープを 1 バイトも
/// 使っておらず、この段でも使わない。
///
/// # 足りなくなったら
///
/// **`ADR-0012` の解禁条件がそこにある**（`deferred-decisions.md` の
/// 「カーネルヒープの拡張」。条件は「S10-b で固定配列では足りないと分かったとき」）。
/// **Linux も `files_struct` に `fd_array[64]` を埋め込み、超えたときだけ
/// 別に確保する形である**——「まず固定配列」は Linux の形でもある。
pub const MAX_OPEN_FILES: usize = 16;

/// 開いている実体（ファイルまたはディレクトリ）。
///
/// **中身は [`common::ext2::Inode`] を直に持つ。** Linux が `ext2_inode_info` で
/// 繋いでいる場所を、**実装が 1 つしかない今は繋がずに畳んでいる。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inode {
    ext2: ext2::Inode,
}

impl Inode {
    /// ext2 の inode から作る。
    pub fn from_ext2(inode: ext2::Inode) -> Self {
        Self { ext2: inode }
    }

    /// inode 番号。
    pub fn number(&self) -> u32 {
        self.ext2.number
    }

    /// `i_mode`（種別 + 許可）。
    pub fn mode(&self) -> u16 {
        self.ext2.mode
    }

    /// バイト長。
    pub fn size(&self) -> u64 {
        self.ext2.size
    }

    /// ハードリンク数。
    pub fn links_count(&self) -> u16 {
        self.ext2.links_count
    }

    /// ディレクトリか。
    pub fn is_directory(&self) -> bool {
        self.ext2.is_directory()
    }

    /// 通常ファイルか。
    pub fn is_regular_file(&self) -> bool {
        self.ext2.is_regular_file()
    }

    /// 中身の ext2 の inode。**ブロックを辿る側が要る**
    /// （`common::ext2::Ext2::file_block` が受け取る）。
    pub fn ext2(&self) -> &ext2::Inode {
        &self.ext2
    }
}

/// 開いたファイル 1 つ分（Linux の `struct file`）。
///
/// # 位置を持つのは `file` であって `inode` ではない
///
/// **同じ実体を 2 回開けば、位置は 2 つある。** Linux が位置を `struct file` に
/// 置いているのと同じ理由で、**[`Inode`] は位置を持たない。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File {
    inode: Inode,
    offset: u64,
}

impl File {
    /// 先頭から読む状態で開く。
    pub fn new(inode: Inode) -> Self {
        Self { inode, offset: 0 }
    }

    /// 実体。
    pub fn inode(&self) -> &Inode {
        &self.inode
    }

    /// 次に読む位置（バイト）。
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// 位置を進める。**`i_size` を越えない**ように飽和させる。
    ///
    /// **越えさせない理由は算術である。** 位置が `i_size` を越えると、
    /// 「残り = `i_size` - 位置」が桁借りする。**`common::ext2` が線2 として
    /// 守っているのと同じ形を、こちら側でも閉じておく。**
    pub fn advance(&mut self, bytes: u64) {
        self.offset = self.offset.saturating_add(bytes).min(self.inode.size());
    }

    /// 位置を直に置く（`lseek` 相当。**この段では呼ばない**）。
    pub fn seek_to(&mut self, offset: u64) {
        self.offset = offset.min(self.inode.size());
    }
}

/// ファイルディスクリプタの表が拒む理由（S10-b）。
///
/// **`errno` をここでは知らない。** 対応づけは syscall の側が持つ
/// （`common::ext2::Ext2Error` と同じ線である。**下の層を `errno` から
/// 独立に保つ**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTableError {
    /// 空きが無い（Linux なら `EMFILE`）。
    NoFreeDescriptor,
    /// その番号は開いていない、または範囲外（Linux なら `EBADF`）。
    BadDescriptor(usize),
}

/// 1 プロセス分のファイルディスクリプタの表（Linux の `files_struct`）。
///
/// # プロセスごとに持つ
///
/// **表はプロセスの持ち物である**（`docs/roadmap.md` の S10-b の棚卸し）。
/// 同時に生きているプロセスは今 1 つだが、**グローバルに 1 つ置くと
/// S11 で作り直しになる。** 固定配列なので、プロセスごとにしても費用は同じである。
///
/// # 番号は最小の空きを返す
///
/// **POSIX がそう規定している**（`open` は使われていない最小の番号を返す）。
/// **外から見える形なので Linux に合わせる。**
///
/// # 0・1・2 を予約しない
///
/// **stdin/stdout/stderr は存在しない。** 予約すると「開いていない番号が使える」
/// 形になり、**表の不変条件（`Some` の番号だけが有効）が緩む。**
/// **Linux でもカーネルが予約しているわけではなく、`init` が開いた結果である。**
#[derive(Debug)]
pub struct FileTable {
    slots: [Option<File>; MAX_OPEN_FILES],
}

impl Default for FileTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTable {
    /// 空の表。
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_OPEN_FILES],
        }
    }

    /// 最小の空き番号へ置き、その番号を返す。
    pub fn insert(&mut self, file: File) -> Result<usize, FileTableError> {
        for (fd, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(file);
                return Ok(fd);
            }
        }
        Err(FileTableError::NoFreeDescriptor)
    }

    /// 番号を閉じる。**開いていない番号は拒む。**
    pub fn remove(&mut self, fd: usize) -> Result<File, FileTableError> {
        self.slots
            .get_mut(fd)
            .and_then(Option::take)
            .ok_or(FileTableError::BadDescriptor(fd))
    }

    /// 番号を引く。
    pub fn get(&self, fd: usize) -> Result<&File, FileTableError> {
        self.slots
            .get(fd)
            .and_then(Option::as_ref)
            .ok_or(FileTableError::BadDescriptor(fd))
    }

    /// 番号を引いて書き換える（位置を進める側が使う）。
    pub fn get_mut(&mut self, fd: usize) -> Result<&mut File, FileTableError> {
        self.slots
            .get_mut(fd)
            .and_then(Option::as_mut)
            .ok_or(FileTableError::BadDescriptor(fd))
    }

    /// 開いている本数。**判定行に出す。**
    pub fn open_count(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 試験用の inode。**ext2 の像を組み立てずに作る**（表の論理だけを見る）。
    fn inode(number: u32, size: u64, mode: u16) -> Inode {
        Inode::from_ext2(ext2::Inode {
            number,
            mode,
            size,
            links_count: 1,
            blocks: [0; ext2::INODE_BLOCK_COUNT],
        })
    }

    /// 通常ファイルの `i_mode`。
    const REGULAR: u16 = 0o100_644;
    /// ディレクトリの `i_mode`。
    const DIRECTORY: u16 = 0o040_755;

    /// 表そのものの大きさ。**固定配列なので、これが常に占める量である。**
    ///
    /// **判断の材料として測っておく**（`MAX_OPEN_FILES` の doc）。
    /// **ここが落ちたら、`File` の中身が増えたということである。**
    #[test]
    fn the_table_is_small_enough_to_live_inside_a_process() {
        // `File` = ext2 の inode（`i_block` の 60 バイトを含む）+ 位置。
        assert_eq!(core::mem::size_of::<File>(), 88);
        // **`Option` が 8 バイト増やす**（`File` に空き表現が無いため、
        // 判別子が別に要る）。**枠 1 つは 96 バイトである。**
        assert_eq!(core::mem::size_of::<Option<File>>(), 96);
        assert_eq!(core::mem::size_of::<FileTable>(), 96 * MAX_OPEN_FILES);
        // **4 KiB を超えない。** 超えると `deferred-decisions.md` の
        // 「大きなスタック配列とガード幅」の解禁条件に当たる。
        assert!(core::mem::size_of::<FileTable>() < 4096);
    }

    #[test]
    fn a_new_table_is_empty() {
        let table = FileTable::new();
        assert_eq!(table.open_count(), 0);
        assert_eq!(table.get(0), Err(FileTableError::BadDescriptor(0)));
    }

    /// **最小の空き番号を返す**（POSIX と同じ）。
    #[test]
    fn descriptors_are_handed_out_lowest_first() {
        let mut table = FileTable::new();
        for expected in 0..4 {
            let fd = table
                .insert(File::new(inode(10 + expected, 1, REGULAR)))
                .unwrap();
            assert_eq!(fd as u32, expected);
        }
        // 途中を閉じると、次はそこが返る。**「最後の次」ではない。**
        table.remove(1).unwrap();
        assert_eq!(table.insert(File::new(inode(99, 1, REGULAR))).unwrap(), 1);
        assert_eq!(table.open_count(), 4);
    }

    /// **0・1・2 を予約していない。** 最初に開いたものが 0 を取る。
    #[test]
    fn the_first_open_takes_descriptor_zero() {
        let mut table = FileTable::new();
        assert_eq!(
            table.insert(File::new(inode(2, 4096, DIRECTORY))).unwrap(),
            0
        );
    }

    #[test]
    fn the_table_fills_up_and_says_so() {
        let mut table = FileTable::new();
        for _ in 0..MAX_OPEN_FILES {
            table.insert(File::new(inode(1, 1, REGULAR))).unwrap();
        }
        assert_eq!(table.open_count(), MAX_OPEN_FILES);
        assert_eq!(
            table.insert(File::new(inode(1, 1, REGULAR))),
            Err(FileTableError::NoFreeDescriptor)
        );
    }

    /// 開いていない番号と範囲外の番号は、どちらも拒む。
    #[test]
    fn unopened_and_out_of_range_descriptors_are_refused() {
        let mut table = FileTable::new();
        let fd = table.insert(File::new(inode(7, 1, REGULAR))).unwrap();
        table.remove(fd).unwrap();

        for bad in [fd, MAX_OPEN_FILES, MAX_OPEN_FILES + 1, usize::MAX] {
            assert_eq!(table.get(bad), Err(FileTableError::BadDescriptor(bad)));
            assert_eq!(
                table.get_mut(bad).err(),
                Some(FileTableError::BadDescriptor(bad))
            );
            assert_eq!(
                table.remove(bad).err(),
                Some(FileTableError::BadDescriptor(bad))
            );
        }
    }

    /// **同じ実体を 2 回開くと、位置は 2 つある。**
    #[test]
    fn two_descriptors_on_one_inode_keep_separate_offsets() {
        let mut table = FileTable::new();
        let target = inode(18, 18, REGULAR);
        let first = table.insert(File::new(target)).unwrap();
        let second = table.insert(File::new(target)).unwrap();

        table.get_mut(first).unwrap().advance(8);
        assert_eq!(table.get(first).unwrap().offset(), 8);
        assert_eq!(table.get(second).unwrap().offset(), 0);
        assert_eq!(
            table.get(first).unwrap().inode().number(),
            table.get(second).unwrap().inode().number()
        );
    }

    /// 位置は `i_size` を越えない。**越えると「残り」の引き算が桁借りする。**
    #[test]
    fn the_offset_never_passes_the_end_of_the_file() {
        let mut file = File::new(inode(18, 18, REGULAR));
        file.advance(10);
        assert_eq!(file.offset(), 10);
        file.advance(1_000);
        assert_eq!(file.offset(), 18, "saturates at i_size");
        file.advance(u64::MAX);
        assert_eq!(file.offset(), 18, "no wrap-around");

        file.seek_to(3);
        assert_eq!(file.offset(), 3);
        file.seek_to(u64::MAX);
        assert_eq!(file.offset(), 18);
    }

    /// 種別は ext2 の inode がそのまま答える。
    #[test]
    fn the_kind_comes_straight_from_the_ext2_inode() {
        let directory = inode(2, 4096, DIRECTORY);
        assert!(directory.is_directory());
        assert!(!directory.is_regular_file());

        let file = inode(18, 18, REGULAR);
        assert!(file.is_regular_file());
        assert!(!file.is_directory());
        assert_eq!(file.number(), 18);
        assert_eq!(file.size(), 18);
        assert_eq!(file.mode(), REGULAR);
        assert_eq!(file.links_count(), 1);
    }
}
