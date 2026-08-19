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

use common::critical::Locked;
use common::ext2;

/// 埋め込んだ ext2 の像（S10-a で置き、S10-b で `main.rs` からここへ移した）。
///
/// # なぜ lib 側へ移したか
///
/// **`open` を処理するのは `syscall::dispatch` で、あちらは lib にある。**
/// bin 側に置いたままだと像を渡す道が要る。**ファイルシステムは
/// カーネルに 1 つしかない**（Linux の root filesystem と同じ）ので、
/// **持ち主は lib が正しい。**
///
/// **写しは 1 つである。** bin 側は [`FS_IMAGE`] を参照するだけで、
/// `include_bytes!` を二重に置かない（2 MiB が 2 つになる）。
///
/// # 抱えるのをやめる条件
///
/// **「像を書き換える必要が生じたとき」または「像の大きさが起動時のコピーで
/// 測れるほど効いたとき」**（`docs/roadmap.md` の S10）。
/// **`.rodata` に置いた像は書けない**ので、S12（書き込み）ではフレームへの
/// 複製が要る。**この記録は S10-a で `main.rs` に置き、S10-b でここへ移した。**
///
/// # ホストのテストビルドには像が無い
///
/// **`kernel/build.rs` は `x86_64-unknown-none` のときだけ像を建てる**
/// （ホストのテストで `mke2fs` を毎回走らせない）。**そのため `cargo test` の
/// 構成では `include_bytes!` の相手が存在しない。**
///
/// **既定側（カーネル）が本物で、ホスト側は空の像である。**
/// 空なら [`root_filesystem`] が `TooShort` で返るので、
/// **黙って別のものを読むことにはならない。**
#[cfg(target_os = "none")]
pub static FS_IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fs.img"));

/// ホストのテストビルドの [`FS_IMAGE`]。**像は建てられていないので空である。**
#[cfg(not(target_os = "none"))]
pub static FS_IMAGE: &[u8] = &[];

/// 根のファイルシステムを組み立てる。
///
/// # 毎回組み立て直す
///
/// **`Ext2` は状態を持たない**——像を借り、superblock から読んだ値を写しているだけ
/// である。**組み立ては superblock を 1 回読むだけなので、静的に持ち回るより安い。**
/// **借用を `static` へ置かずに済む**ぶん、形も単純になる。
pub fn root_filesystem() -> Result<ext2::Ext2<'static>, ext2::Ext2Error> {
    ext2::Ext2::parse(root_image())
}

/// 根の像の在り処（S12-b の 1 段目）。**0 なら、まだ複製へ向いていない。**
///
/// # なぜ差し替えるのか
///
/// **書く先と読む先を同じにするためである。** S12-a は像をフレームへ複製したが、
/// **読む側は `.rodata` の [`FS_IMAGE`] を見たままだった。**
/// **そのまま書き始めると、書いた先と読む先が別物になる。**
///
/// # 向ける前も動く
///
/// **複製は起動の途中で作られる。** それより前に像を読む経路がある
/// （`verify_embedded_fs_image`）ので、**向くまでは埋め込みの側を返す。**
static ROOT_IMAGE_PTR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// [`ROOT_IMAGE_PTR`] が指す長さ。
static ROOT_IMAGE_LEN: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 根の像を複製へ向ける（S12-b の 1 段目）。**複製した側が 1 度だけ呼ぶ。**
pub fn set_root_image(image: &'static [u8]) {
    ROOT_IMAGE_LEN.store(image.len(), core::sync::atomic::Ordering::SeqCst);
    ROOT_IMAGE_PTR.store(
        image.as_ptr() as usize,
        core::sync::atomic::Ordering::SeqCst,
    );
}

/// いま読んでいる像。
///
/// 破壊 (S12-b, fs-read-from-rodata): 常に埋め込みの側を返す。
/// **向け直したことが観測できなくなる**——判定行に出る番地がカーネル像の中になり、
/// 複製の番地と食い違う。
pub fn root_image() -> &'static [u8] {
    #[cfg(feature = "fs-read-from-rodata-test")]
    return FS_IMAGE;

    #[cfg(not(feature = "fs-read-from-rodata-test"))]
    {
        let ptr = ROOT_IMAGE_PTR.load(core::sync::atomic::Ordering::SeqCst);
        let len = ROOT_IMAGE_LEN.load(core::sync::atomic::Ordering::SeqCst);
        if ptr == 0 {
            return FS_IMAGE;
        }
        // SAFETY: [`set_root_image`] が渡した `&'static [u8]` の中身をそのまま
        // 組み直している。**複製先のフレームは起動中ずっと生きており、返さない。**
        unsafe { core::slice::from_raw_parts(ptr as *const u8, len) }
    }
}

/// RAM 複製を可変で貸す（zi-c。ADR-0037 の「書き手の口」）。
///
/// **複製前（[`ROOT_IMAGE_PTR`] が 0）は `None` である。** そのとき読める側は
/// `.rodata` の埋め込み（[`FS_IMAGE`]）で、**あれは共有の `&'static` である——
/// 絶対に可変で貸してはならない。**
///
/// # Safety（&mut と & の重なりが無いことの、参照生成箇所の全数列挙）
///
/// この像への参照が生成される場所は、次で全部である（zi-c で数えた。
/// **像への参照を作る経路を足すときは、この列挙へ足すこと**）。
///
/// 1. [`root_filesystem`]（唯一の共有借用の構成点。`Ext2::parse(root_image())`）。
///    呼び出し元は 5 つで、**いずれも自分の呼び出しの中で借りて落とす**——
///    `sys_open` / `sys_read` / `sys_getdents64` / `sys_stat`
///    （`kernel/src/syscall.rs`）、`load_user_program`
///    （`kernel/src/userland.rs`。ELF を `SPAWN_IMAGES` へ写してから落とす——
///    **借りたまま Ring 3 へ入らないことは、あちらの「像をブロックごとに写す。
///    借りたままにできない」の doc が根拠である**）
/// 2. [`root_image`] の直接の呼び出し元は `kernel/src/main.rs` の判定行 1 箇所で、
///    **番地の値だけを読む**（参照を保持しない）
/// 3. 起動シーケンス（`copy_fs_image_to_frames` と exercise 群）。**スケジューラ
///    より前・Ring 3 より前の単一文脈**で、syscall はまだ来ない
///
/// そのうえで、重ならない根拠は 2 つである。
///
/// - **BKL が syscall どうしを直列化する**（§6）。上の 1 の借用は各 syscall の
///   呼び出し区間で閉じており、この関数が `&mut` を貸すのは別の syscall
///   （`sys_open` の truncate と `sys_write` の append）の区間である
/// - **貸す区間は closure の間だけである。** closure の中から
///   [`root_filesystem`] / [`root_image`] を呼び戻さないこと（いまの利用者は
///   `common::ext2` の純関数へ渡すだけである）
pub fn with_root_image_mut<R>(f: impl FnOnce(&mut [u8]) -> R) -> Option<R> {
    let ptr = ROOT_IMAGE_PTR.load(core::sync::atomic::Ordering::SeqCst);
    let len = ROOT_IMAGE_LEN.load(core::sync::atomic::Ordering::SeqCst);
    if ptr == 0 {
        return None;
    }
    // SAFETY: `set_root_image` が渡した複製のフレームで、起動中ずっと生きている。
    // 生きた共有借用と重ならないことは、上の doc の全数列挙が示す。
    let image = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len) };
    Some(f(image))
}

/// 今 Ring 3 が使っている表（Linux の `current->files`）。
///
/// # 据えるのは遠征の前後である
///
/// **`syscall::set_user_window` と同じ形である**（S9-b-3-2b）。表はプロセスの
/// 持ち物だが、**`dispatch` はプロセスを知らない。** そこで遠征の間だけここへ
/// 据え、戻るときに引き取る。**入れ子にはならない**（Ring 3 の遠征は入れ子に
/// ならない）が、**前の値を返す形にしてあるので、入れ子になっても壊れない。**
///
/// # 既定値は空の表である
///
/// **据え忘れたときに、前のプロセスの fd が見える形にしない。**
/// 空の表なら、どの番号を引いても `BadDescriptor` である。
static CURRENT_FILES: Locked<FileTable> = Locked::new(FileTable::new());

/// 表を据え、**据える前の表を返す**。
///
/// **戻すのは呼び出し側の責任である**（[`CURRENT_FILES`] の doc）。
pub fn swap_current_files(table: FileTable) -> FileTable {
    core::mem::replace(&mut *CURRENT_FILES.lock(), table)
}

/// 今の表へ触る。**`dispatch` が `open` と `close` で使う。**
pub fn with_current_files<R>(body: impl FnOnce(&mut FileTable) -> R) -> R {
    body(&mut CURRENT_FILES.lock())
}

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

/// 標準入力の fd（Linux と同じ 0）。
pub const STDIN_FD: usize = 0;
/// 標準出力の fd（Linux と同じ 1）。
pub const STDOUT_FD: usize = 1;
/// 標準エラー出力の fd（Linux と同じ 2）。
pub const STDERR_FD: usize = 2;

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

    /// 占めている 512 バイト単位のブロック数（Linux の `st_blocks` と同じ単位）。
    pub fn blocks_512(&self) -> u32 {
        self.ext2.blocks_512
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
pub enum File {
    /// ファイルシステムの実体を開いたもの。
    ///
    /// `writable` は開いた向き（zi-c。ADR-0037）。**読みで開いたら偽、
    /// `O_WRONLY|O_TRUNC` で開いたら真である。** 向きの取り違えは
    /// 両方向とも `-EBADF`（syscall の側が写す）。
    Regular {
        inode: Inode,
        offset: u64,
        writable: bool,
    },
    /// 端末（S11-10）。**0 / 1 / 2 に据えてある。**
    ///
    /// # なぜ表の中に置くのか
    ///
    /// **番号で特別扱いすると、`open` が返した番号と衝突する。**
    /// 表に置けば、**`read` と `write` は番号ではなく中身で分岐する。**
    /// **ファイルへ書けるようになった段（S12）でも、同じ形のまま通る。**
    ///
    /// # 位置を持たない
    ///
    /// **端末は端まで戻れない。** 読んだバイトは消える。
    Terminal,
}

impl File {
    /// 先頭から読む状態で開く。
    pub fn new(inode: Inode) -> Self {
        Self::Regular {
            inode,
            offset: 0,
            writable: false,
        }
    }

    /// 書き込みで開く（zi-c。`O_WRONLY|O_TRUNC` の形だけがここへ来る）。
    ///
    /// **`inode` は open 時点の写しである。** 切った後の大きさ（0）とは
    /// 食い違うが、**書きで開いた fd は読まない**（read は `-EBADF`）ので、
    /// 位置の飽和（`advance` が `i_size` で切る形）に使われることは無い。
    pub fn writable(inode: Inode) -> Self {
        Self::Regular {
            inode,
            offset: 0,
            writable: true,
        }
    }

    /// 書き込みで開いたか。**端末は偽である**（あちらは常に書ける）。
    pub fn is_writable_file(&self) -> bool {
        matches!(self, Self::Regular { writable: true, .. })
    }

    /// 端末。
    pub fn terminal() -> Self {
        Self::Terminal
    }

    /// 端末か。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal)
    }

    /// 実体。**端末には無い。**
    pub fn inode(&self) -> Option<&Inode> {
        match self {
            Self::Regular { inode, .. } => Some(inode),
            Self::Terminal => None,
        }
    }

    /// 次に読む位置（バイト）。**端末は 0 である。**
    pub fn offset(&self) -> u64 {
        match self {
            Self::Regular { offset, .. } => *offset,
            Self::Terminal => 0,
        }
    }

    /// 位置を進める。**`i_size` を越えない**ように飽和させる。
    ///
    /// **越えさせない理由は算術である。** 位置が `i_size` を越えると、
    /// 「残り = `i_size` - 位置」が桁借りする。**`common::ext2` が線2 として
    /// 守っているのと同じ形を、こちら側でも閉じておく。**
    pub fn advance(&mut self, bytes: u64) {
        if let Self::Regular { inode, offset, .. } = self {
            *offset = offset.saturating_add(bytes).min(inode.size());
        }
    }

    /// 位置を直に置く（`lseek` 相当。**この段では呼ばない**）。
    pub fn seek_to(&mut self, to: u64) {
        if let Self::Regular { inode, offset, .. } = self {
            *offset = to.min(inode.size());
        }
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
    /// これまでに開いた本数（累計。閉じても減らない）。
    ///
    /// # なぜ「今開いている本数」だけでは足りないか
    ///
    /// **最後に全部閉じたプロセスと、一度も開かなかったプロセスは、
    /// 今開いている本数では区別が付かない**（どちらも 0 である）。
    /// **判定行が「表が動いた」を主張するには、累計が要る。**
    opened: usize,
}

impl Default for FileTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTable {
    /// 0 / 1 / 2 が端末で、それ以外は空の表（S11-10）。
    ///
    /// # 予約する形へ変えた
    ///
    /// **S10-b では予約しないと決めていた**——「予約すると『開いていない番号が
    /// 使える』形になり、表の不変条件（`Some` の番号だけが有効）が緩む」。
    /// **その懸念は、端末を表の中身として置くことで消える**——
    /// **0 / 1 / 2 は `Some` であり、開いている。**
    ///
    /// **`init` が開く形（Linux）は採らない。** **開く相手が無い**——
    /// デバイスノードを置く仕組みが無く、`/dev/console` は像に存在しない。
    /// **存在しないパスを特別扱いするほうが、番号を据えるより見えにくい。**
    ///
    /// **`docs/roadmap.md` が「予約するか、シェルが自分で開くか」と書いた判断は、
    /// ここで前者に決まった。**
    pub const fn new() -> Self {
        let mut slots = [None; MAX_OPEN_FILES];
        slots[STDIN_FD] = Some(File::Terminal);
        slots[STDOUT_FD] = Some(File::Terminal);
        slots[STDERR_FD] = Some(File::Terminal);
        Self { slots, opened: 0 }
    }

    /// 最小の空き番号へ置き、その番号を返す。**端末の 3 つは埋まっているので、
    /// 最初の `open` は 3 を返す**（Linux と同じ）。
    pub fn insert(&mut self, file: File) -> Result<usize, FileTableError> {
        for (fd, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(file);
                self.opened = self.opened.saturating_add(1);
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
    ///
    /// # 端末は数えない（S11-10）
    ///
    /// **0 / 1 / 2 は最初から在り、閉じられることを想定していない。**
    /// **数えると、判定行の「開いたまま戻ったものが何本あるか」が
    /// 常に 3 から始まる**——**主張したいのは「このプロセスが開いて閉じ忘れた本数」
    /// である。**
    pub fn open_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| matches!(slot, Some(file) if !file.is_terminal()))
            .count()
    }

    /// これまでに開いた本数（累計）。**判定行に出す。**
    ///
    /// **0 なら、このプロセスは 1 度も開いていない。**
    pub fn opened_total(&self) -> usize {
        self.opened
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
            blocks_512: size.div_ceil(512) as u32,
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
    /// **開いた向きの印（zi-c）。** 読みで開けば偽、書きで開けば真。
    /// 端末は偽である（あちらは常に書ける——向きの概念が無い）。
    #[test]
    fn the_writable_mark_follows_how_the_file_was_opened() {
        let target = inode(12, 18, REGULAR);
        assert!(!File::new(target).is_writable_file());
        assert!(File::writable(target).is_writable_file());
        assert!(!File::terminal().is_writable_file());
    }

    #[test]
    fn the_table_is_small_enough_to_live_inside_a_process() {
        // `File` = ext2 の inode（`i_block` の 60 バイトを含む）+ 位置 + 判別子。
        //
        // **S11-10 で 88 から 96 へ増えた。** `File` が列挙になり、
        // **端末と実体を区別する判別子が入った**（`Terminal` は中身を持たないので、
        // 大きさを決めているのは `Regular` のほうである）。
        assert_eq!(core::mem::size_of::<File>(), 96);
        // **`Option` は増やさない。** 列挙になったことで**空き表現ができた**
        // ——判別子の使っていない値を `None` に使える。**枠 1 つは 96 バイトのまま
        // である**（S11-10 の前は 88 + 8 = 96 だった）。
        assert_eq!(core::mem::size_of::<Option<File>>(), 96);
        // 表は枠 16 個 + 累計のカウンタ 8 バイトである。
        assert_eq!(
            core::mem::size_of::<FileTable>(),
            96 * MAX_OPEN_FILES + core::mem::size_of::<usize>()
        );
        // **4 KiB を超えない。** 超えると `deferred-decisions.md` の
        // 「大きなスタック配列とガード幅」の解禁条件に当たる。
        assert!(core::mem::size_of::<FileTable>() < 4096);
    }

    /// **一度も開いていないことと、開いて全部閉じたことは区別が付く。**
    #[test]
    fn the_running_total_separates_never_opened_from_all_closed() {
        let mut untouched = FileTable::new();
        assert_eq!(untouched.open_count(), 0);
        assert_eq!(untouched.opened_total(), 0);

        let fd = untouched.insert(File::new(inode(18, 18, REGULAR))).unwrap();
        untouched.remove(fd).unwrap();
        assert_eq!(untouched.open_count(), 0, "nothing is open any more");
        assert_eq!(untouched.opened_total(), 1, "but the table did move");
    }

    /// **新しい表には端末が 3 つ在り、ファイルは 1 つも無い（S11-10）。**
    #[test]
    fn a_new_table_holds_the_three_terminals_and_no_files() {
        let table = FileTable::new();
        assert_eq!(table.open_count(), 0, "terminals are not counted");
        assert_eq!(table.opened_total(), 0);
        assert!(table.get(STDIN_FD).unwrap().is_terminal());
        assert!(table.get(STDOUT_FD).unwrap().is_terminal());
        assert!(table.get(STDERR_FD).unwrap().is_terminal());
        assert_eq!(table.get(3), Err(FileTableError::BadDescriptor(3)));
    }

    /// **最小の空き番号を返す**（POSIX と同じ）。**端末の 3 つは埋まっている。**
    #[test]
    fn descriptors_are_handed_out_lowest_first() {
        let mut table = FileTable::new();
        for expected in 3..7 {
            let fd = table
                .insert(File::new(inode(10 + expected, 1, REGULAR)))
                .unwrap();
            assert_eq!(fd as u32, expected);
        }
        // 途中を閉じると、次はそこが返る。**「最後の次」ではない。**
        table.remove(4).unwrap();
        assert_eq!(table.insert(File::new(inode(99, 1, REGULAR))).unwrap(), 4);
        assert_eq!(table.open_count(), 4);
    }

    /// **0・1・2 は端末である（S11-10）。** 最初に開いたものは 3 を取る。
    #[test]
    fn the_first_open_takes_descriptor_three() {
        let mut table = FileTable::new();
        assert_eq!(
            table.insert(File::new(inode(2, 4096, DIRECTORY))).unwrap(),
            3
        );
    }

    #[test]
    fn the_table_fills_up_and_says_so() {
        let mut table = FileTable::new();
        // **端末の 3 つを引いた本数しか入らない（S11-10）。**
        for _ in 0..MAX_OPEN_FILES - 3 {
            table.insert(File::new(inode(1, 1, REGULAR))).unwrap();
        }
        assert_eq!(table.open_count(), MAX_OPEN_FILES - 3);
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
            table.get(first).unwrap().inode().unwrap().number(),
            table.get(second).unwrap().inode().unwrap().number()
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
