//! ユーザープログラムのロードと実行（S11-4 で `main.rs` から移した）。
//!
//! # なぜ lib にあるのか
//!
//! **`spawn` を処理するのは `syscall::dispatch` で、あちらは lib にある。**
//! bin 側に置いたままだと、システムコールからプロセスを作れない
//! （`ADR-0030` の Alternatives が「順序として、所有を先に決める」と書いた案である。
//! 所有が決まったので、ここへ移した）。
//!
//! # 何が移り、何が残ったか
//!
//! **境界は「機構」と「記述」である。**
//!
//! | ここ（lib） | `main.rs`（bin） |
//! |---|---|
//! | プロセスを作って走らせる機構 | 何を走らせ、何を期待するかの記述 |
//! | [`UserProcess`]・[`UserLoadError`] | `USER_PROGRAMS` の表 |
//! | [`load_user_program`] と写像・遠征 | 終わり方の判定と会計の判定行 |
//!
//! **`common::ext2`（パーサ）と `crate::vfs`（使い方）を分けた線と同じ形である。**
//!
//! # `errno` は知らない
//!
//! [`UserLoadError`] は `errno` を持たない。**写すのは `syscall` の側である**
//! （`common::ext2::Ext2Error` と `vfs::FileTableError` に続く 3 つ目）。

use common::log::{LogLevel, Logger};
use common::serial::SerialPort;

use crate::ring3::MAX_EXCURSION_DEPTH;
use crate::syscall::{MAX_ARGV_BYTES, MAX_EXECUTABLE_SIZE, PATH_MAX};

/// ユーザープログラムを走らせる空間のユーザーサブツリーの添字（S9-b-1）。
///
/// **[`USER_PML4_INDEX`]（= 1）とは別である。** あちらは本番の空間の値で、
/// 起動時の検証 3 本が使っている。**プログラムは自分の空間を持つので、添字も
/// 自分で決められる**（`AddressSpace` が添字を持つ。S7-e）。
///
/// 0 を採るのは、`hello` を `0x400000` へリンクしているからである（Linux の
/// 非 PIE の既定と同じ）。**本番の空間の `PML4[0]` には恒等除去まで恒等が居るが、
/// 新しい空間の下位は空なので関係が無い。**
pub const USER_PROGRAM_PML4_INDEX: usize = 0;

/// ユーザープログラムのスタックの上端（S9-b-1）。1 ページだけ張る。
///
/// `hello` の像は `0x400000` から 2 ページなので、十分離れた位置に置く。
const USER_PROGRAM_STACK_TOP: u64 = 0x0080_0000;

/// 1 プロセスに渡せる `argv` の要素数の上限（S11-1）。
///
/// **見込みの最大は 2 である**（プログラム名 + 引数 1 つ）。**8 はその 4 倍で、
/// 表と文字列がスタックの 1 ページに収まる範囲である。** 越えたら
/// [`UserLoadError::ArgumentsTooLong`] で拒む。
pub const MAX_ARGV: usize = 8;

/// 埋め込んだユーザープログラムのロードと実行が失敗する形（S9-b-2）。
///
/// # なぜ `Result` にしたか
///
/// **S9-b-1 では失敗のたびに `halt_forever` していた。** 相手が自分のビルドの
/// 作った像だったので、壊れていればカーネルの不具合であり、止まるのが正しかった。
/// **S9-b-2 は壊した像を意図的に渡すので、止まってはいけない。**
///
/// **この段では呼び出し側がまだ止める。** 既定の `hello` は成功するので、
/// 振る舞いは変わらない。壊した像を渡すのは S9-b-2 の 2 つ目である。
///
/// # `ElfError` の 11 種をどう扱うか
///
/// [`common::elf::ElfError`] が返るのは [`Self::Parse`]（`Elf::parse`）と
/// [`Self::SegmentData`]（`Elf::segment_data`）で、**どちらもそのまま持ち上げる。**
/// **ローダーは種類で分岐しない。** 内訳は次のとおりで、**すべて「像が壊れている」
/// に落ちる。**
///
/// - `TooShort` / `BadMagic` / `NotElf64` / `NotLittleEndian` / `NotExecutable` /
///   `NotX86_64`: ヘッダの形。**`parse` の最初の 6 つで、いずれも 1 バイトの
///   書き換えで作れる**
/// - `ProgramHeaderOutOfBounds` / `BadProgramHeaderEntrySize`: 表の位置と 1 エントリの
///   大きさ。**`e_phoff` と `e_phentsize` の書き換えで作れる**
/// - `SegmentFileRangeOutOfBounds` / `SegmentMemorySmallerThanFile` /
///   `SegmentAddressOverflow`: 区画の数値。**`p_offset` / `p_filesz` / `p_memsz` /
///   `p_vaddr` の書き換えで作れる**
///
/// **11 種とも、既定の像の 1 バイトから 8 バイトを書き換えれば作れる。**
/// S9-b-2 の 2 つ目で壊し方を選ぶときの材料である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserLoadError {
    /// フレームアロケータを借りられなかった（S11-3。`ADR-0030`）。
    ///
    /// **起動シーケンスは単一コアの直線なので、ここへ来ること自体が異常である**
    /// ——誰かが借りたまま返していない。
    AllocatorUnavailable,
    /// `argv` と表と文字列が、スタックの 1 ページに収まらない（S11-1）。
    ArgumentsTooLong,
    /// `Elf::parse` が拒んだ。**像がバイト列として壊れている。**
    Parse(common::elf::ElfError),
    /// `Elf::segment_data` が拒んだ。**区画のファイル内範囲が像の外にある。**
    ///
    /// **`parse` も同じことを見ているので、通常はここへ来ない。** 来るとしたら
    /// 呼び出し側が `parse` を通さないヘッダを渡したときで、**多層防御の 2 枚目が
    /// 効いた形である。**
    SegmentData(common::elf::ElfError),
    /// 新しいアドレス空間を作れなかった。**像ではなくカーネル側の事情である。**
    AddressSpace(crate::address_space::AddressSpaceError),
    /// フレームが尽きた。**像ではなくカーネル側の事情である。**
    OutOfFrames,
    /// 張ろうとした仮想アドレスが正準形でない。
    NotCanonical(u64),
    /// 写像に失敗した。**区画が同じページを共有していると、後から来たほうがここへ来る。**
    Mapping {
        virt: u64,
        error: crate::address_space::AddressSpaceError,
    },
    /// 張った葉のフラグが、区画の権限と食い違った。**カーネル側の不具合である。**
    LeafFlags { count: usize },
    /// 終了せずに畳まれた（S9-b-3-1）。**`exit` が効かなかったということである。**
    ///
    /// `hello` は `exit` の直後に `ud2` を置いてあるので、**効かなければ確定的に
    /// ここへ来る**（`HELLO_UD2_OFFSET` の doc）。
    DidNotExit,
    /// 終了でも畳みでもなく Ring 3 から戻ってきた。**カーネル側の不具合である。**
    ///
    /// `ring3::enter` はこの 2 つの longjmp でしか戻らないので、**通常は構成でき
    /// ない。** 記録の側が壊れたときの受け皿である。
    NoExitNoFold,
    /// 終了状態が予期と違った。`hello` は 0 で終わる。
    ExitStatus(u64),
    /// 畳まれて終わるはずのプロセスが、終了して戻った（S9-b-3-2a）。
    ///
    /// **起こすはずの違反が起きなかったということである。**
    DidNotFold,
    /// 畳まれたが、ベクタ・RIP・CS・CR2・エラーコードのどれかが予期と違った
    /// （S9-b-3-2a）。**どれが違うかは直前の ERROR 行に出ている。**
    FoldMismatch,
    /// ユーザーが組み立てた引数が、`ADR-0020` の規約どおりに届かなかった
    /// （S9-b-3-2a）。**probe が呼ばれなかった場合も含む。**
    AbiMismatch,
    /// 畳んだ会計が合わなかった（S9-b-3-1）。**空間を畳んでも、消えたフレームが
    /// 隔離へ届いていない。**
    DestroyAccounting {
        consumed: usize,
        quarantined: usize,
        leaked: usize,
    },
    /// `write` が届けたバイト列が予期と違った。
    WriteMismatch,
}

/// ユーザープログラムの初期スタックを **Linux と同じ形で**積む（S11-1）。
///
/// 返すのは entry へ入るときの `rsp`（`argc` を指す）。
///
/// # 形は実測で確かめた
///
/// **ホストで、`_start` から `rsp` をたどる自作の静的バイナリを走らせて観測した。**
/// `rsp` の指す先から順に——`argc`、`argv` のポインタ、NULL、`envp` のポインタ、
/// NULL、そして `auxv` の `(type, value)` の対が続き、`type == 0`（`AT_NULL`）で
/// 終わる。**文字列そのものはこの表より上（高位）に置かれる。**
///
/// **記憶で書かない**（`docs/coding-standards.md` の「実測値は、測った条件が
/// 変わると古くなる」）。`docs/vision.md` の表は**Linux バイナリを動かすための
/// 記述**で、そこには `AT_PHDR` などが要るとある。**こちらが積むのは自作の
/// プログラム向けなので、要るものが違う。**
///
/// # 何を積み、何を積まないか
///
/// **`argc` と `argv` は積む。** `envp` は**空**（終端だけ）、`auxv` は
/// **`AT_NULL` だけ**である。
///
/// **`auxv` の中身は Linux バイナリを動かす段で要るものである。**
/// **自作のプログラムは読まないので、終端だけ置く。**
/// **形を合わせておくのは、後から中身を足すときに入口が変わらないからである**
/// ——そして **C の `crt0` がそのまま書ける**（`docs/vision.md` の C の構想）。
///
/// # 16 バイト整列
///
/// **`rsp` は entry の時点で 16 の倍数である**（SysV の規約。Linux もそう積む）。
/// **詰め物は表と文字列の間に入る。**
///
/// # Safety
///
/// `page` がスタックページの先頭を direct map 越しに指しており、
/// 4096 バイト書けること。単一実行文脈から呼ぶこと。
unsafe fn build_initial_stack(page: *mut u8, page_base: u64, argv: &[&[u8]]) -> Option<u64> {
    /// 表の項の大きさ。
    const WORD: usize = 8;
    /// 表の固定部——`argc`・`argv` の終端・`envp` の終端・`AT_NULL` の対。
    const FIXED_WORDS: usize = 1 + 1 + 1 + 2;
    /// `auxv` の終端。
    const AT_NULL: u64 = 0;
    /// スタックページの大きさ。**1 枚だけ張ってある**（呼び出し側）。
    const PAGE_SIZE: usize = 4096;

    if argv.len() > MAX_ARGV {
        return None;
    }

    let mut cursor = PAGE_SIZE;

    // **文字列を上から詰める。** 置いたユーザー VA を控える。
    let mut argv_addrs = [0u64; MAX_ARGV];
    for (index, arg) in argv.iter().enumerate() {
        let bytes = *arg;
        // NUL 終端のぶんを含めて下げる。
        cursor = cursor.checked_sub(bytes.len() + 1)?;
        // SAFETY: cursor はページ内で、`bytes.len() + 1` バイト書ける。
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), page.add(cursor), bytes.len());
            page.add(cursor + bytes.len()).write(0);
        }
        argv_addrs[index] = page_base + cursor as u64;
    }

    // 表を置く位置。**表の先頭が 16 の倍数になるように下げる。**
    cursor &= !0xF;
    cursor = cursor.checked_sub((FIXED_WORDS + argv.len()) * WORD)?;
    cursor &= !0xF;

    let mut at = cursor;
    let put = |value: u64, at: &mut usize| {
        // SAFETY: `at` は上で確保した範囲の中で、8 バイト書ける。
        unsafe { page.add(*at).cast::<u64>().write_unaligned(value) };
        *at += WORD;
    };
    put(argv.len() as u64, &mut at);
    for address in argv_addrs.iter().take(argv.len()) {
        put(*address, &mut at);
    }
    put(0, &mut at); // argv の終端
    put(0, &mut at); // envp は空。終端だけ

    // 破壊 (S11-1, no-auxv-terminator): `auxv` に項目を 1 つ足して、
    // **終端を書かない。** `AT_PHDR` は Linux バイナリが読む型で、
    // **自作のプログラムは `auxv` を読まないので、足しても誰も困らないように見える。**
    // **終端が無いことは、終端まで歩いた者にしか分からない。**
    #[cfg(feature = "syscall-test-no-auxv-terminator")]
    {
        /// `AT_PHDR`。**値そのものに意味は要らない**——終端の有無が主張である。
        const AT_PHDR: u64 = 3;
        put(AT_PHDR, &mut at);
        put(0, &mut at);
    }
    #[cfg(not(feature = "syscall-test-no-auxv-terminator"))]
    {
        put(AT_NULL, &mut at); // auxv の終端（type）
        put(0, &mut at); //                 （value）
    }

    Some(page_base + cursor as u64)
}

/// 同時に飛べる `spawn` の本数（S11-5）。
///
/// # 引き算に理由がある
///
/// **`spawn` は遠征の中からしか呼べない**（`dispatch` へ来るのは Ring 3 からだけで、
/// Ring 3 は遠征の中にしかない）。**したがって呼ばれた時点の深さは 1 以上である。**
/// そして [`spawn`] は深さが [`MAX_EXCURSION_DEPTH`] 以上なら断るので、
/// **実際に子を起こせるのは深さ 1 から `MAX_EXCURSION_DEPTH - 1` までである。**
///
/// **その本数だけ緩衝を持てば、入れ子で上書きされない。**
/// **`MAX_EXCURSION_DEPTH` を上げれば、ここも自動で増える。**
///
/// **S11-11 で 1 本増えた。** `init` がカーネルの直線上（深さ 0）から
/// シェルを起こすようになったので、**深さ 0 から `MAX_EXCURSION_DEPTH - 1` まで
/// が起こす側になる。**
const MAX_SPAWN_IN_FLIGHT: usize = MAX_EXCURSION_DEPTH;

/// `spawn` が読んだ像を置く場所（S11-5）。
///
/// # なぜ像を写すのか。ブロックは借りられるのに
///
/// **`common::ext2::Ext2::file_block` が返すのは像を借りたバイト列である**
/// （[`crate::vfs::FS_IMAGE`] は `&'static [u8]`）。**1 ブロックで足りるなら
/// 写さずに済む**——しかし **ELF は 4096 バイトを超え、ブロックが像の中で
/// 連続している保証は無い。** `hello` は 8496 バイトで 3 ブロックである。
/// **繋がっていないものを 1 本のバイト列として渡すには、写すしかない。**
///
/// # スタックへ置かない
///
/// [`MAX_EXECUTABLE_SIZE`] の doc（`deferred-decisions.md` の解禁条件の 2 度目）。
static mut SPAWN_IMAGES: [[u8; MAX_EXECUTABLE_SIZE]; MAX_SPAWN_IN_FLIGHT] =
    [[0; MAX_EXECUTABLE_SIZE]; MAX_SPAWN_IN_FLIGHT];

/// `spawn` が受け取ったパスを置く場所（S11-5）。
///
/// # `&'static str` が要る
///
/// [`load_user_program`] の `name` と `argv` は `&'static str` である
/// （判定行に出す名前と、初期スタックへ積む `argv[0]`）。**ユーザーから来た
/// パスはカーネルスタックのローカルなので、そのままでは渡せない。**
///
/// **像と同じく、深さごとに 1 本ずつ持つ**（[`MAX_SPAWN_IN_FLIGHT`]）。
static mut SPAWN_PATHS: [[u8; PATH_MAX]; MAX_SPAWN_IN_FLIGHT] =
    [[0; PATH_MAX]; MAX_SPAWN_IN_FLIGHT];

/// [`spawn`] が起こした子が隔離へ入れたフレームの累計（S11-5）。
///
/// # なぜ要るのか。**親の会計が閉じなくなる**
///
/// 起動時の会計は「このプログラムを走らせる前後で空きフレームがいくつ減ったか」と
/// 「そのプログラムの空間を畳んで隔離へ何枚入れたか」を突き合わせる。
/// **子を起こすと、子のぶんも前者に乗る**——隔離へ入ったフレームは世代が退くまで
/// アロケータへ戻らないので、**親から見ると「消えたまま」である。**
///
/// **実測で踏んだ**（S11-5）。`syscall-test` が 2 本の子を起こしたところ、
/// **24 枚消えて自分の隔離は 8 枚**になった。差の 16 枚が子 2 本のぶんである。
///
/// **子の側で数えて、親が足す。** 親が子の内訳を知る必要はない。
static SPAWN_QUARANTINED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// [`spawn`] が起こした子が漏らしたフレームの累計（S11-5）。
static SPAWN_LEAKED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 子の会計を 0 に戻す（S11-5）。**プログラムを 1 本走らせる直前に呼ぶ。**
pub fn reset_spawn_accounting() {
    SPAWN_QUARANTINED.store(0, core::sync::atomic::Ordering::SeqCst);
    SPAWN_LEAKED.store(0, core::sync::atomic::Ordering::SeqCst);
}

/// 子が隔離へ入れた枚数と漏らした枚数（S11-5）。
pub fn spawn_accounting() -> (usize, usize) {
    (
        SPAWN_QUARANTINED.load(core::sync::atomic::Ordering::SeqCst),
        SPAWN_LEAKED.load(core::sync::atomic::Ordering::SeqCst),
    )
}

/// `spawn` が受け取った `argv` を置く場所（S11-7）。
///
/// **NUL 区切りで並べたバイト列である。** [`load_user_program`] が要求するのは
/// `&[&[u8]]` で、**要素は `'static` でなければならない**（[`SPAWN_PATHS`] と
/// 同じ理由）。**深さごとに 1 本ずつ持つ**（[`MAX_SPAWN_IN_FLIGHT`]）。
static mut SPAWN_ARGVS: [[u8; MAX_ARGV_BYTES]; MAX_SPAWN_IN_FLIGHT] =
    [[0; MAX_ARGV_BYTES]; MAX_SPAWN_IN_FLIGHT];

/// [`spawn`] が拒む形（S11-5）。
///
/// **`errno` を知らない。** 写すのは `crate::syscall` の側である
/// （[`UserLoadError`] と同じ線。module の doc）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// 遠征の深さが上限に達している。**これ以上は入れ子にできない。**
    TooDeep,
    /// パスを引けなかった（無い、途中がディレクトリでない、像が壊れている）。
    Lookup(common::ext2::Ext2Error),
    /// 引けたがディレクトリだった。
    IsDirectory,
    /// 引けたが通常ファイルではなかった（デバイスファイル等）。
    NotRegularFile,
    /// 写した `argv` のバイト列が、要素数と食い違った（S11-7）。
    ///
    /// **カーネル側の不具合である**——`copy_user_argv` は要素ごとに NUL を付けて
    /// 並べるので、**要素数だけ NUL があるはずである。**
    ArgvMalformed,
    /// 像が [`MAX_EXECUTABLE_SIZE`] に収まらない。
    TooLarge(u64),
    /// 像を読んでいる途中でブロックが引けなかった。
    Read(common::ext2::Ext2Error),
    /// 載せられなかった、または期待どおりに終わらなかった。
    Load(UserLoadError),
    /// 子を畳んだ会計が合わなかった。**カーネル側の不具合である。**
    DestroyAccounting {
        consumed: usize,
        quarantined: usize,
        leaked: usize,
    },
}

/// 子プロセスの終わり方（S11-5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// `exit(status)` で終わった。
    Exited(u64),
    /// Ring 3 の違反が畳まれて終わった。**ベクタを持つ。**
    Folded(u64),
    /// 外から止められた（Ctrl+C。S12 前の手当て、C）。
    ///
    /// **`Folded` と分けてある。** あちらは**子が違反した**で、
    /// **こちらは子に落ち度が無い。** 混ぜると、判定行から
    /// 「落ちた」と「止めた」の区別が付かなくなる。
    ///
    /// **ベクタを持たない。** 止めた地点はタイマ割り込みで、
    /// **どのベクタで止めたかは子について何も語らない。**
    Interrupted,
}

/// 走らせるプロセス 1 つ分（S9-b-3-1）。
///
/// # 表を持たない
///
/// **同時に生きているプロセスは 1 つである。** 3 本を順に走らせ、1 本ずつ
/// 終わらせる。**表を先に作るのは先回りの抽象化になる**（`vision.md` の規定）。
/// 同時生存が複数になる段（S11 のシェル）で表にすればよく、**そのときこの型は
/// そのまま使える。**
///
/// # 「走らせるプログラムの一覧」とは別物である
///
/// あちらは [`USER_PROGRAMS`] で、**静的な記述**である（`TASK_COUNT` や
/// `build.rs` の `PROGRAMS` と同じ性質）。**混ぜると、一覧の長さが管理構造の
/// 容量に見える。** こちらは管理構造で、長さは常に 1 である。
pub struct UserProcess {
    /// このプロセスのアドレス空間。**終了で畳む。**
    space: crate::address_space::AddressSpace,
    /// 最初に飛ぶ先（ELF の entry）。
    entry: u64,
    /// ユーザースタックの上端。
    stack_top: u64,
    /// 判定行に出す名前。
    name: &'static str,
    /// このプロセスが開いているファイルの表（S10-b）。
    ///
    /// # まだ誰も開かない
    ///
    /// **この刻みでは表を置くだけである。** 開くのは次の刻み（`open`/`close`）で、
    /// **ここが未使用なのはそのためである。** `#[allow(dead_code)]` を付けている
    /// のは、**「要らないものを置いた」のではなく「使い方をまだ実装していない」**
    /// 側だからである（S9-b-3-1 で立てた判定。未使用の警告はそのどちらかを指す）。
    /// # プロセスの持ち物である
    ///
    /// 同時に生きているプロセスは今 1 つなので、グローバルに 1 つ置いても動く。
    /// **それでもここへ置く**——固定配列なので費用が変わらず、
    /// **グローバルに置くと S11 で作り直しになる。**
    ///
    /// # 遠征の間だけ `crate::vfs` へ据える
    ///
    /// **`syscall::dispatch` はプロセスを知らない**ので、Ring 3 へ落ちる直前に
    /// [`crate::vfs::swap_current_files`] で据え、戻ったら引き取る
    /// （`syscall::set_user_window` と同じ形である）。
    files: crate::vfs::FileTable,
}

/// 像を 1 つ、専用のアドレス空間へ載せて（`run` なら走らせて）畳む（S9-b-2）。
///
/// **失敗しても空間を畳む。** 途中で落ちた場合、**そこまでに張った写像と
/// 中間テーブルが残っている。** 畳まずに戻ると、そのフレームは誰にも
/// 返らない。**「壊した像でカーネルが止まらない」は、後始末まで含めて
/// 初めて言える。**
///
/// 畳んだ結果（隔離へ入れた本数と、隔離が溢れて漏らした本数）を返す。
/// **後始末が正しいことは、この会計で主張する。**
///
/// # 終わり方は判定しない（S9-b-3-2a）
///
/// 走らせた場合、返すのは像の entry である。**どう終わったかの判定は
/// [`check_user_program_outcome`] が行う**——プログラムごとに正しい終わり方が
/// 違い、それは呼び出し側の知識だからである（`ring3::enter` が畳んだ位置を
/// 主張しないのと同じ形）。`run` が偽なら 0 を返す。
pub fn load_user_program(
    logger: &mut Logger<SerialPort>,
    image: &[u8],
    run: bool,
    name: &'static str,
    argv: &[&[u8]],
) -> (Result<u64, UserLoadError>, usize, usize) {
    use crate::address_space::AddressSpace;

    let direct_map = common::addr::direct_map();
    let production = crate::paging::switch::read_cr3();

    // **アロケータを借りる（S11-3。`ADR-0030`）。** 写像の間だけ持ち、
    // **Ring 3 へ落ちる前に返す。**
    let Some(allocator) = crate::frame_allocator::take() else {
        return (Err(UserLoadError::AllocatorUnavailable), 0, 0);
    };

    // SAFETY: production は稼働中の PML4、direct_map は登録済みの窓。
    let space = match unsafe {
        AddressSpace::new(allocator, direct_map, production, USER_PROGRAM_PML4_INDEX)
    } {
        Ok(space) => space,
        Err(e) => {
            // **失敗の経路でも返す（S11-3）。** ここで持ったまま抜けると、
            // 以後の確保がすべて `None` になる。**S9-b-2 で「失敗の途中で取った
            // フレームは呼び出し側が返す」と決めた場所と同じ関数で、
            // 今度はアロケータ自体を返す。**
            crate::frame_allocator::give_back(allocator);
            return (Err(UserLoadError::AddressSpace(e)), 0, 0);
        }
    };

    // **ここからプロセスである。** stack_top は張る前から決まっているが、entry は
    // 像を読むまで分からないので、0 で作り load_user_program_into が埋める。
    let mut process = UserProcess {
        space,
        entry: 0,
        stack_top: USER_PROGRAM_STACK_TOP,
        name,
        files: crate::vfs::FileTable::new(),
    };

    // **写像まではアロケータが要る。遠征では要らない。**
    let mapped = load_user_program_into(logger, allocator, image, &mut process, argv);
    // **ここで返す。** 以降は Ring 3 の遠征があり、**その間はアロケータが
    // `static` に在るので、システムコールから取り出せる**（`ADR-0030` の要）。
    //
    // **失敗の経路でも必ず通る**——`mapped` はまだ判定していない。
    crate::frame_allocator::give_back(allocator);

    let outcome = mapped
        .and_then(|()| {
            if run {
                // SAFETY: 写像は済んでおり、entry と stack は張ったユーザーページ。
                unsafe { run_loaded_program(logger, &mut process) }
            } else {
                Ok(())
            }
        })
        .map(|()| process.entry);

    // **成否によらず畳む。** 破棄は S7-d の経路（下位を隔離へ入れ、世代が
    // 退くまで返さない）をそのまま通る。**プロセスが終了したなら、畳むのはここ
    // である**（S9-b-3-1。終了の記録は `syscall` 側、空間の始末はこちら）。
    //
    // 破壊 (S9-b-3-1, user-exit-keep-space): 畳まない。**消えたフレーム数と隔離へ
    // 入れた数の会計が合わなくなり、呼び出し側が捕まえる**（`AddressSpace` は
    // `Drop` を持たないので、落とすだけではフレームは戻らない）。
    //
    // **走らせたときだけ飛ばす。** 壊した像の後始末（S9-b-2）はこの破壊の対象では
    // なく、そちらまで飛ばすと**あちらの会計が先に落ちて、終了の側を観測できない。**
    // **実測で踏んだ**——先に落ちるほうだけを見ていた。
    let keep_space = cfg!(feature = "user-exit-keep-space") && run;
    let (held, leaked) = if keep_space {
        (0, 0)
    } else {
        let mut quarantine = crate::quarantine::Quarantine::new();
        let guard = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);
        // SAFETY: この空間はどのコアでも稼働していない。direct map は覆っている。
        unsafe { process.space.destroy(direct_map, &mut quarantine, &guard) }
    };

    (outcome, held, leaked)
}

/// 像を新しいアドレス空間へ写像し、`run` なら Ring 3 で走らせる（S9-b-1）。
///
/// # 走らせるかは引数で決まる
///
/// `run` が偽なら写像して戻る。**壊した像の扱い（S9-b-2）がこちらを使う**
/// ——張れるところまで張って拒まれることを見るので、走らせる必要が無い。
///
/// # 区画の権限をそのまま葉へ落とす
///
/// `PT_LOAD` の `p_flags` の W を [`PageAttributes::writable`] へ渡す。`hello` の
/// 2 区画はどちらも書き込み不可なので、**ここが `writable: false` の最初の実利用に
/// なる**（S9-a で足した引数が、S9-b の本命の経路で使われる）。
/// スタックだけは `writable: true` で張る。
///
/// # 失敗しても止まらない。**呼び出し側が決める**
///
/// **かつてはここで止めていた**（相手が自分のビルドの像だけだった S9-b-1 まで）。
/// **S9-b-2 で `Result` にした。** 信頼できない像を読む経路ができたので、
/// **止めるかどうかは像の出所を知っている側の判断になった**——埋め込んだ 3 本の
/// 失敗はカーネルの不具合なので呼び出し側が止め、壊した像の失敗は期待どおりなので
/// 止めない。**`docs/roadmap.md` の S9 が「いかなる入力に対しても fail-fast
/// させない」と言っているのは、後者についてである。**
///
/// # 区画が同じページを共有していると張れない
///
/// **後から来た区画が `Mapping { AlreadyMapped }` で拒まれる**（S9-b-3-2b）。
///
/// **一度は誤っていた。** ここには以前も「`AlreadyMapped` 相当で弾かれる」と
/// 書いてあったが、**それを持っていたのは
/// [`crate::paging::active::ActivePageTable::map_4kib`] の側だけで、ローダーが
/// 使う [`crate::address_space::AddressSpace::map_user_4kib`] は葉の present を
/// 見ずに書いていた。** 契約を片側だけ見て、もう片側のものとして書いていた形で
/// ある（S9-b-3-2b の数え直しで実測した）。**実測では両方「張れた」ことになり、
/// 1 つ目のフレームが写像から外れて 1 枚漏れた**（14 枚消えて隔離へ 13 枚）。
/// **漏れは会計に出てカーネルを止めるので、S9 の「いかなる入力でもカーネルを
/// fail-fast させない」に反していた。** 判定を足して直してある。
///
/// **`userland/user.ld` が区画をページ境界へ揃えているのは、この形を避けるため
/// である**（揃えないと実際に重なった。実測で `.text` が `0x400000..0x400030`、
/// `.rodata` が `0x400030` からになった）。**実際のツールチェインも同じ理由で
/// 揃える。**
///
fn load_user_program_into(
    logger: &mut Logger<SerialPort>,
    allocator: &mut crate::frame_allocator::FrameAllocator,
    image: &[u8],
    process: &mut UserProcess,
    argv: &[&[u8]],
) -> Result<(), UserLoadError> {
    use crate::paging::active::PageAttributes;
    use crate::paging::verify;
    use common::elf::Elf;

    const PAGE_SIZE: u64 = 4096;

    let direct_map = common::addr::direct_map();

    let elf = match Elf::parse(image) {
        Ok(elf) => elf,
        Err(e) => return Err(UserLoadError::Parse(e)),
    };

    // 張った VA と、期待する W を覚えておく（後で読み戻して照合する）。
    let mut mapped: [(u64, bool); 8] = [(0, false); 8];
    let mut mapped_count = 0usize;

    for ph in elf.load_segments() {
        let writable = ph.p_flags & 0x2 != 0;
        let first_page = ph.p_vaddr & !(PAGE_SIZE - 1);
        let last_page = (ph.p_vaddr + ph.p_memsz - 1) & !(PAGE_SIZE - 1);

        let file = match elf.segment_data(&ph) {
            Ok(bytes) => bytes,
            Err(e) => return Err(UserLoadError::SegmentData(e)),
        };

        let mut page = first_page;
        while page <= last_page {
            let Some(frame) = allocator.allocate_frame() else {
                return Err(UserLoadError::OutOfFrames);
            };

            // **ゼロ埋めしてからファイルの中身を重ねる。** `p_memsz` が `p_filesz` より
            // 大きい分（.bss）はゼロのまま残る。
            let dst = direct_map.phys_to_virt(frame).as_u64() as *mut u8;
            // SAFETY: いま取ったフレームで、direct map が覆っている。誰も使っていない。
            unsafe { core::ptr::write_bytes(dst, 0, PAGE_SIZE as usize) };

            // このページが覆うファイル内の範囲を切り出して書く。
            let page_start_in_segment = page.saturating_sub(ph.p_vaddr);
            let offset_in_page = ph.p_vaddr.saturating_sub(page);
            // 破壊 (S9-b-1, user-run-skip-load): ファイルの中身を写さない。ページは
            // ゼロのままになり、Ring 3 が entry からゼロを実行して ud2 へ届かない。
            if page_start_in_segment < ph.p_filesz && !cfg!(feature = "user-run-skip-load") {
                let remaining = ph.p_filesz - page_start_in_segment;
                let room = PAGE_SIZE - offset_in_page;
                let count = core::cmp::min(remaining, room) as usize;
                let from = page_start_in_segment as usize;
                // SAFETY: from + count <= p_filesz = file.len()、
                // offset_in_page + count <= PAGE_SIZE。どちらも上で押さえてある。
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        file.as_ptr().add(from),
                        dst.add(offset_in_page as usize),
                        count,
                    )
                };
            }

            let Some(virt) = common::addr::VirtAddr::new(page) else {
                return Err(UserLoadError::NotCanonical(page));
            };
            // 破壊 (S9-b-1, user-run-writable-text): 区画の権限を無視して書けるように
            // 張る。**読み取り専用のはずの葉が W=1 になり、下の読み戻しが捕まえる。**
            let attributes = PageAttributes {
                user: true,
                writable: writable || cfg!(feature = "user-run-writable-text"),
                cacheable: true,
            };
            // SAFETY: この空間はまだ稼働していない。direct map は覆っている。
            if let Err(e) = unsafe {
                process
                    .space
                    .map_user_4kib(allocator, direct_map, virt, frame, attributes)
            } {
                // **張れなかったフレームは、ここで返す。** 空間へ繋がっていないので
                // `AddressSpace::destroy` からは見えず、返さないと誰にも戻らない。
                // **実測で気づいた**——失敗の経路で空きフレームが 7 枚減るのに、
                // 隔離へ入ったのは 6 枚だった。差の 1 枚がこれである。
                let _ = allocator.deallocate_frame(frame);
                return Err(UserLoadError::Mapping {
                    virt: page,
                    error: e,
                });
            }

            if mapped_count < mapped.len() {
                mapped[mapped_count] = (page, writable);
                mapped_count += 1;
            }
            page += PAGE_SIZE;
        }

        logger.info(format_args!(
            "user-load: {} mapped PT_LOAD {:#x}..{:#x} (filesz={:#x} memsz={:#x} w={writable})",
            process.name,
            ph.p_vaddr,
            ph.p_vaddr + ph.p_memsz,
            ph.p_filesz,
            ph.p_memsz
        ));
    }

    // ユーザースタックを 1 枚。**こちらは書ける。**
    let stack_page = USER_PROGRAM_STACK_TOP - PAGE_SIZE;
    let Some(frame) = allocator.allocate_frame() else {
        return Err(UserLoadError::OutOfFrames);
    };
    let dst = direct_map.phys_to_virt(frame).as_u64() as *mut u8;
    // SAFETY: いま取ったフレーム。direct map が覆っている。
    unsafe { core::ptr::write_bytes(dst, 0, PAGE_SIZE as usize) };
    let Some(stack_virt) = common::addr::VirtAddr::new(stack_page) else {
        return Err(UserLoadError::NotCanonical(stack_page));
    };
    let stack_attributes = PageAttributes {
        user: true,
        writable: true,
        cacheable: true,
    };
    // SAFETY: この空間はまだ稼働していない。direct map は覆っている。
    if let Err(e) = unsafe {
        process
            .space
            .map_user_4kib(allocator, direct_map, stack_virt, frame, stack_attributes)
    } {
        return Err(UserLoadError::Mapping {
            virt: stack_page,
            error: e,
        });
    }
    if mapped_count < mapped.len() {
        mapped[mapped_count] = (stack_page, true);
        mapped_count += 1;
    }
    logger.info(format_args!(
        "user-load: mapped the user stack {stack_page:#x}..{USER_PROGRAM_STACK_TOP:#x} (w=true)"
    ));

    // **初期スタックを Linux の形で積む（S11-1）。**
    // SAFETY: `dst` はいま張ったスタックページの direct map 越しの先頭で、
    // 1 ページぶん書ける。単一実行文脈である。
    let Some(initial_rsp) = (unsafe { build_initial_stack(dst, stack_page, argv) }) else {
        return Err(UserLoadError::ArgumentsTooLong);
    };
    process.stack_top = initial_rsp;
    logger.info(format_args!(
        "user-load: {} initial stack at {initial_rsp:#x} (argc={}, 16-byte aligned={})",
        process.name,
        argv.len(),
        initial_rsp % 16 == 0
    ));

    // **張った側とは独立に降りて、葉のフラグを読み戻す。**
    // これが S9-a で足した `writable` が実際に W を落としていることの、
    // この経路での観測である（`ring3-vectors` の 6 本目はもう一方の経路を見ている）。
    let mut mismatches = 0usize;
    for &(virt_value, expected_writable) in mapped.iter().take(mapped_count) {
        let Some(virt) = common::addr::VirtAddr::new(virt_value) else {
            continue;
        };
        // SAFETY: この空間の PML4 は有効で、direct map が配下を覆っている。読み取りのみ。
        match unsafe { verify::walk(process.space.pml4(), direct_map, virt) } {
            Ok(resolved) => {
                let writable = resolved.entry & crate::paging::entry::PTE_WRITABLE != 0;
                let user = resolved.entry & crate::paging::entry::PTE_USER != 0;
                if writable != expected_writable || !user {
                    mismatches += 1;
                    logger.error(format_args!(
                        "user-load: {virt_value:#x} has w={writable} (expected \
                         {expected_writable}) u={user} (expected true)"
                    ));
                }
            }
            Err(e) => {
                mismatches += 1;
                logger.error(format_args!(
                    "user-load: {virt_value:#x} did not walk: {e:?}"
                ));
            }
        }
    }

    if mismatches != 0 {
        return Err(UserLoadError::LeafFlags { count: mismatches });
    }

    // === Ring 3 で走らせる（S9-b-1 の 4 つ目） ===
    //
    // **CR3 を差し替えてから iretq で落ちる。** 上位は共有なのでカーネルは動き
    // 続ける（S7-c の到達条件 4 が、実プログラムで初めて使われる）。
    // 戻りは `ud2` の #UD を S8 の畳みが受ける。
    // 破壊 (S9-b-1, user-run-wrong-entry): entry ではなく最初の PT_LOAD の先頭へ
    // 飛ぶ。**詰め物の ud2 で即座に #UD になり、フォルト RIP が期待と食い違う。**
    // 詰め物が生きていることは verify_embedded_user_elf が主張している。
    #[cfg(not(feature = "user-run-wrong-entry"))]
    let entry = elf.entry_point;
    #[cfg(feature = "user-run-wrong-entry")]
    let entry = elf
        .load_segments()
        .next()
        .map(|ph| ph.p_vaddr)
        .unwrap_or(elf.entry_point);

    // **ここで entry が確定する。** 呼び出し側は `UserProcess` から読む。
    process.entry = entry;

    Ok(())
}

/// 写像済みのプロセスを Ring 3 で走らせる（S11-3 で切り出した）。
///
/// # なぜ切り出したか
///
/// **アロケータを遠征の前に返すためである**（`ADR-0030`）。写像には要るが、
/// 遠征には要らない。**切り口は元からあった `if !run` の位置である。**
///
/// **あの分岐は S9-b-2 で「壊した像を写像だけして走らせない」ために作った。**
/// **「写像と実行を分ける」という同じ軸なので、所有の境界とも一致した**
/// ——別々の目的で引いた線が、同じ場所を通っている。
///
/// # Safety
///
/// `process` の写像が済んでおり、entry と stack が張ったユーザーページであること。
/// 起動時の単一実行文脈から呼ぶこと。
unsafe fn run_loaded_program(
    logger: &mut Logger<SerialPort>,
    process: &mut UserProcess,
) -> Result<(), UserLoadError> {
    let production = crate::paging::switch::read_cr3();

    crate::syscall::reset_counters();
    // **戻す RSP0 は「今この処理が乗っているカーネルスタックの上端」である。**
    //
    // 深さ 0 なら、ここはカーネルの直線上なのでメインのスタックである。
    // **深さが 1 以上なら、`spawn` が親の遠征の中から呼んでいる**——親は
    // その深さの遠征スタックの上でこの処理をしているので、**そこへ戻さないと
    // 親のカーネルスタックが変わってしまう**（S11-2 の入れ子の検証で同じ判断をした）。
    //
    // **深さから引ける値なので、引数で受け取らない。**
    //
    // 破壊 (S11-5, spawn-child-rsp0): 親の遠征スタックではなく、**子自身の**
    // 遠征スタックの上端へ戻す。**入れ子でないうちはこの行を通らないので、
    // 入れ子になった瞬間だけ壊れる。** 親が次にカーネルへ入るときの RSP0 が
    // 子のスタックを指し、**次に子を起こしたときに親のフレームを踏む。**
    // **`spawn` が戻り先の RSP0 を突き合わせて捕まえる。**
    let main_rsp0_top = if crate::ring3::depth() == 0 {
        crate::gdt::privilege_stack_top()
    } else if cfg!(feature = "spawn-child-rsp0") {
        crate::ring3::excursion_stack_range_at(crate::ring3::depth()).1
    } else {
        crate::ring3::excursion_stack_range().1
    };

    // SAFETY: この空間はカーネルの上位を共有しており、切り替えても実行中の
    // コードとスタックは見え続ける。
    unsafe { crate::paging::switch::switch_to(process.space.pml4()) };
    // **このプロセスの fd の表を据える（S10-b）。** `dispatch` はプロセスを
    // 知らないので、遠征の間だけ `crate::vfs` が持つ
    // （`syscall::set_user_window` と同じ形。据えるのは Ring 3 へ落ちる側である）。
    let previous_files = crate::vfs::swap_current_files(core::mem::take(&mut process.files));
    // **前景を取る（S11-10）。** 取っているあいだ、カーネル側の消費者
    // （`interrupts::drain_keyboard`）はスキャンコードを取り出さない。
    // **入力の消費者は同時に 1 つである**（`crate::input` の不変条件）。
    //
    // **入れ子でも取れる。** 親は遠征の中で `spawn` を呼んでおり、
    // **その間ずっと前景を持っている。** 子が取ろうとすると偽が返るので、
    // **親が持ったままにして、子はその前景を通して読む**——
    // **持ち主は 1 人という不変条件は保たれる。**
    let claimed_foreground = crate::input::claim_foreground();
    // **前の中断要求を持ち越さない（S12 前の手当て、C）。**
    //
    // **深さ 1 で Ctrl+C を押すと、旗は立つが誰も消費しない**——
    // **畳む地点は深さ 2 以上でしか発火しない**（`crate::idt` の
    // `fold_if_interrupted`）。**降ろさずに子を起こすと、その子が
    // 起きた瞬間に止まる。**
    //
    // 破壊 (S12 前の手当て C, kill-keep-stale-interrupt): 降ろさない。
    // **シェルで Ctrl+C を押した後、次に起こした子が即座に止まる。**
    #[cfg(not(feature = "kill-keep-stale-interrupt-test"))]
    crate::input::clear_interrupt_request();
    // **どの深さの遠征スタックを使うかを控える（S11-5）。** 戻った後は深さが
    // 元へ戻っているので、そのときには引けない。
    let entered_at_depth = crate::ring3::depth();
    // SAFETY: entry と stack は今張ったユーザーページで、`ud2` が必ずフォルト
    // する。main_rsp0_top はメインのカーネルスタック上端。単一実行文脈である。
    unsafe {
        crate::ring3::enter(
            main_rsp0_top,
            process.entry,
            process.stack_top,
            crate::syscall::window_for_subtree(USER_PROGRAM_PML4_INDEX),
        )
    };
    // **引き取る。** 遠征が畳みで戻っても `exit` で戻ってもここを通る
    // （`ring3::enter` はこの 2 つの longjmp でしか戻らない）。
    // **前景を返す。** 取った者だけが返す（入れ子の子は取れていない）。
    if claimed_foreground {
        crate::input::release_foreground();
    }
    process.files = crate::vfs::swap_current_files(previous_files);
    // **止めたときは、前景の持ち主と止めた相手を両方出す（S12 前の手当て、C）。**
    //
    // **この 2 つは同じではない。** 前景を取るのは遠征の最も外側
    // （シェル）で、**止めるのは最も内側（子）である。**
    // `claim_foreground` は入れ子では偽を返し、**親が持ったまま子はその前景を
    // 通して読む。** **どこにも書かれていなかったので、判定行に出す。**
    if crate::ring3::interrupted() {
        // **止めた打鍵そのものを捨てる。** 残すと、次にシェルが読んだときに
        // `^C` がもう 1 つ出る（実測。[`crate::input::discard_typed_input`]）。
        //
        // 破壊 (S12 前の手当て C, kill-keep-typed-input): 捨てない。
        // **止めた直後のプロンプトに `^C` が余分に出る。**
        #[cfg(not(feature = "kill-keep-typed-input-test"))]
        crate::input::discard_typed_input();
        logger.info(format_args!(
            "interrupt: stopped {} at excursion depth {entered_at_depth}; the foreground is held \
             at depth {} (the holder is the outermost excursion, the target is the innermost), \
             claimed here={claimed_foreground}",
            process.name,
            crate::input::foreground_depth()
        ));
    }
    // **この遠征で遠征スタックをどれだけ使ったかを出す（S11-5）。**
    //
    // **このスタックにはガードページが無い**（`.bss` の配列である）ので、
    // **溢れは静かに起きて、下の静的領域を書く。** 実測で `EXCURSION_DEPTH` を
    // 壊した（`docs/troubleshooting.md`）。**推測せずに毎起動測る。**
    let used = crate::ring3::excursion_stack_high_water(entered_at_depth);
    let capacity = crate::ring3::excursion_stack_capacity();
    let intact = crate::ring3::excursion_stack_canary_intact(entered_at_depth);
    logger.info(format_args!(
        "ring3: {} used {used} of {capacity} byte(s) of the depth-{entered_at_depth} \
         excursion stack ({}%), the canary at its bottom is intact={intact}",
        process.name,
        used * 100 / capacity
    ));
    if !intact {
        logger.error(format_args!(
            "ring3: the depth-{entered_at_depth} excursion stack ran into its bottom canary; \
             it has no guard page, so anything below it may already be overwritten. halting"
        ));
        common::cpu::halt_forever();
    }
    // **解禁条件を機械にする（S11-6）。**
    //
    // `deferred-decisions.md` の「遠征スタックにガードページが無い」は、解禁条件を
    // **「使用量が容量の半分を超えたとき、または見張り区間が一度でも壊れたとき」**と
    // 書いている。**後者は上で止まるが、前者は書いてあるだけだった。**
    //
    // **書いただけの条件は発火しない。** `install_kernel_stack_guard_page` が
    // 2MiB ページを見つけたら止める形と同じにする——**あちらは M5-b で条件を書き、
    // S11-5 で実際に発火して、そこで判断させた。**
    //
    // **見張り区間で止まるのでは遅い。** あれが偽になるのは残り 256 バイトまで
    // 使い切ったときで、**そこまで来たら判断する余地が無い。**
    if !crate::ring3::excursion_stack_within_budget(entered_at_depth) {
        logger.error(format_args!(
            "ring3: the depth-{entered_at_depth} excursion stack is more than half used \
             ({used} of {capacity}); the deferred decision about these stacks having no guard \
             page says to decide here - either map them the way StackBlock is mapped (page \
             aligned, one page below unmapped) or raise the capacity with a measurement. halting"
        ));
        common::cpu::halt_forever();
    }

    // **表が動いたことの観測（S10-b）。** 開いたまま戻ったものが何本あるかを出す。
    // **`syscall-test` は最後に閉じるので 0 で戻る**——ここが 0 でなければ、
    // 開いた fd が漏れている。
    logger.info(format_args!(
        "vfs: {} opened {} file(s) in total and left Ring 3 with {} still open \
         (MAX_OPEN_FILES={}); it was handed {} byte(s) of input",
        process.name,
        process.files.opened_total(),
        process.files.open_count(),
        crate::vfs::MAX_OPEN_FILES,
        crate::input::delivered_count()
    ));
    // SAFETY: 本番のテーブルへ戻す。上位は同じなので連続して実行できる。
    unsafe { crate::paging::switch::switch_to(production) };

    Ok(())
}

/// ファイルシステムから像を読み、子プロセスとして走らせ、**終わるまで待つ**（S11-5）。
///
/// # 同期である
///
/// **戻るのは子が終わった後である。** 親（呼び出し元の Ring 3）は、その間
/// 入れ子の遠征の下で止まっている。**非同期にするにはユーザープロセスの
/// スケジューラが要り、それはまだ無い**（`docs/roadmap.md` の S11）。
///
/// # 深さで断る
///
/// [`MAX_EXCURSION_DEPTH`] に達していたら [`SpawnError::TooDeep`] を返す。
/// **入る前に断る**——[`crate::ring3::enter`] は上限を越えた深さで呼ばれると
/// 遠征スタックと回復点を index 0 へ丸めるので、**親のものを踏む。**
/// **その状態には判定行が無い**ので、**踏ませずに断る側で閉じる。**
///
/// # BKL は保持していない
///
/// **呼ぶ前に解いてある**（`crate::syscall::syscall_entry`。`ADR-0023` §1）。
/// **子は Ring 3 で走り、システムコールごとに自分で BKL を取る。**
/// 保持したまま入ると、子の最初のシステムコールが同じコアの再取得になる。
///
/// # 親の観測を壊さない
///
/// 子は [`crate::syscall::reset_counters`] を通り、自分の `write` と `exit` を
/// 記録する。**親の記録はここで控えて戻す**（`crate::syscall::Records` と
/// [`crate::ring3::FoldRecord`]）。
pub fn spawn(
    path: &[u8],
    argv_bytes: &[u8],
    argv_count: usize,
) -> Result<SpawnOutcome, SpawnError> {
    // **深さの上限。入る前に断る。**
    let depth = crate::ring3::depth();
    if depth >= MAX_EXCURSION_DEPTH {
        return Err(SpawnError::TooDeep);
    }
    // **緩衝の番号は深さそのものである**（[`MAX_SPAWN_IN_FLIGHT`] の doc）。
    // **上の判定が `depth < MAX_EXCURSION_DEPTH` を保証しているので、範囲内である。**
    //
    // **深さ 0 からも呼べる（S11-11）。** `init` がカーネルの直線上から
    // シェルを起こす。**S11-5 の時点では `dispatch` からしか来なかったので、
    // 深さ 0 を不具合として拒んでいた。** 呼び出し側が増えたので、その判定を外した。
    let slot = depth;

    let mut port = SerialPort::new(SerialPort::COM1_BASE);
    port.init();
    let mut logger = Logger::new(port, LogLevel::Trace);

    let fs = crate::vfs::root_filesystem().map_err(SpawnError::Lookup)?;
    let inode = fs.lookup(path).map_err(SpawnError::Lookup)?;
    if inode.is_directory() {
        return Err(SpawnError::IsDirectory);
    }
    if !inode.is_regular_file() {
        return Err(SpawnError::NotRegularFile);
    }
    if inode.size > MAX_EXECUTABLE_SIZE as u64 {
        return Err(SpawnError::TooLarge(inode.size));
    }
    let size = inode.size as usize;

    // **パスを控える。** `name` と `argv[0]` に `&'static str` が要る
    // （[`SPAWN_PATHS`]）。**入らない分は切る**——`copy_user_path` が
    // [`PATH_MAX`] で切っているので、ここへ来る時点で収まっている。
    let name_len = path.len().min(PATH_MAX);
    // SAFETY: `slot` は [`MAX_SPAWN_IN_FLIGHT`] の範囲内で、その深さで走っている
    // のはこの 1 本だけである（深さの判定が入れ子の重なりを禁じている）。
    // 単一コアの実行文脈で、割り込みハンドラはここへ来ない。
    let path_slot: &'static mut [u8; PATH_MAX] =
        unsafe { &mut (*core::ptr::addr_of_mut!(SPAWN_PATHS))[slot] };
    path_slot[..name_len].copy_from_slice(&path[..name_len]);
    let name_bytes: &'static [u8] = &path_slot[..name_len];
    // **UTF-8 でなければ名前を伏せる。** パスは Ring 3 から来るバイト列で、
    // **ext2 も UTF-8 を要求しない。** 判定行に出すためだけの値なので、
    // **読めないことを理由に起動を拒まない。**
    let name = core::str::from_utf8(name_bytes).unwrap_or("<not utf-8>");

    // **像をブロックごとに写す。** 借りたままにできない理由は [`SPAWN_IMAGES`]。
    // SAFETY: `slot` は範囲内で、その深さで使うのはこの 1 本だけである（上と同じ）。
    let image_slot: &'static mut [u8; MAX_EXECUTABLE_SIZE] =
        unsafe { &mut (*core::ptr::addr_of_mut!(SPAWN_IMAGES))[slot] };
    {
        let block_size = fs.block_size() as usize;
        let mut done = 0usize;
        let mut index = 0u32;
        while done < size {
            let block = fs.file_block(&inode, index).map_err(SpawnError::Read)?;
            // **進む量が必ず正である**（線4）。`block_size` は 1024 以上、
            // `size - done` は正、`block.len()` はブロック長である。
            let take = block_size.min(size - done).min(block.len());
            if take == 0 {
                return Err(SpawnError::Read(common::ext2::Ext2Error::SparseBlock(
                    index,
                )));
            }
            image_slot[done..done + take].copy_from_slice(&block[..take]);
            done += take;
            index += 1;
        }
    }
    // **`size` で切る。** 32 KiB 全体を渡すと、**前回の `spawn` が残した
    // バイト列が像の続きとして読める**——`common::elf` の範囲検査は
    // 渡されたバイト列の長さに対して行うので、**長さを偽ると検査も緩む**
    // （線3。参照が像の外を指さないことは、像の端がどこかに依る）。
    let image: &'static [u8] = &image_slot[..size];

    // **親の遠征スタックの残りを測る（S11-5）。**
    //
    // **起動時の `load_user_program` はメインのカーネルスタック（ガードページ付き）の
    // 上で走るが、`spawn` からのそれは親の遠征スタックの上で走る。**
    // **遠征スタックは `.bss` の配列で、ガードページが無い**——溢れても止まらず、
    // 隣を静かに書く。**推測せずに測って出す。**
    // **深さ 0 の親は遠征スタックの上に居ない（S11-11 で直した）。**
    // `init` はカーネルの直線上から呼ぶので、**メインのカーネルスタック**
    // （ガードページ付き）の上に居る。**そちらは測らない**——
    // **測る値打ちがあるのは、ガードの無い遠征スタックのほうである。**
    let stack_probe = 0u8;
    let rsp_now = &stack_probe as *const u8 as u64;
    if depth == 0 {
        logger.info(format_args!(
            "spawn: {name} is {size} byte(s) at inode {}; entering at depth {} (the parent \
             runs on the main kernel stack, which has a guard page)",
            inode.number,
            depth + 1
        ));
    } else {
        let (excursion_bottom, excursion_top) = crate::ring3::excursion_stack_range();
        let stack_used = excursion_top.saturating_sub(rsp_now);
        let stack_left = rsp_now.saturating_sub(excursion_bottom);
        logger.info(format_args!(
            "spawn: {name} is {size} byte(s) at inode {}; entering at depth {} (the parent's \
             excursion stack {excursion_bottom:#x}..{excursion_top:#x} has {stack_used} byte(s) \
             used and {stack_left} left)",
            inode.number,
            depth + 1
        ));
    }

    // **子が起こした孫の隔離を控える（S11-11）。**
    //
    // **S11-5 で起動時の会計に同じ穴があり、そこは直した**——隔離のフレームは
    // 世代が退くまでアロケータへ戻らないので、**親から見ると消えたままである。**
    // **`spawn` 自身の会計にも同じ穴が残っていた。**
    // **シェルが `ls` と `cat` と `hello` を起こしたところで出た**——
    // 実測で 35 枚消えて、シェル自身の隔離は 9 枚だった（9 + 9 + 9 + 8）。
    let (children_before, leaked_before) = spawn_accounting();

    // **戻ってくるべき RSP0 を控える（S11-11 で直した）。**
    //
    // **S11-5 では「親の遠征スタックの上端」と突き合わせていた。** あのときは
    // `spawn` が `dispatch` からしか来ず、**親が必ず遠征の中にいた。**
    // **`init` が深さ 0 から呼ぶようになって、その前提が消えた**——
    // 深さ 0 の親はメインのカーネルスタックの上に居る。
    //
    // **控えて突き合わせる形なら、どちらの深さでも同じ 1 行で言える**
    // ——**「子が走る前と後で RSP0 が変わっていない」。**
    let rsp0_before = crate::gdt::privilege_stack_top();

    // **親の記録を控える。** 子は `reset_counters` を通る。
    let saved_records = crate::syscall::save_records();
    let saved_fold = crate::ring3::save_fold_record();

    // **会計のために借りて、すぐ返す**（`ADR-0030`）。**借りられなければ
    // 子も起こせない**ので、そのまま [`UserLoadError::AllocatorUnavailable`] へ落とす。
    let free_before = match crate::frame_allocator::take() {
        Some(allocator) => {
            let count = allocator.free_frame_count();
            crate::frame_allocator::give_back(allocator);
            count
        }
        None => {
            crate::syscall::restore_records(saved_records);
            crate::ring3::restore_fold_record(saved_fold);
            return Err(SpawnError::Load(UserLoadError::AllocatorUnavailable));
        }
    };

    // **`argv` を控えて、`&'static [u8]` の並びへ切り分ける（S11-7）。**
    //
    // **切り分けは NUL で行う。** `copy_user_argv` が要素ごとに NUL を付けて
    // 並べているので、**要素数だけ NUL があるはずである。** 無ければこちらの
    // 不具合なので、[`SpawnError::ArgvMalformed`] で止める。
    // SAFETY: `slot` は [`MAX_SPAWN_IN_FLIGHT`] の範囲内で、その深さで走っている
    // のはこの 1 本だけである（深さの判定が入れ子の重なりを禁じている）。
    // 単一コアの実行文脈で、割り込みハンドラはここへ来ない。
    let argv_slot: &'static mut [u8; MAX_ARGV_BYTES] =
        unsafe { &mut (*core::ptr::addr_of_mut!(SPAWN_ARGVS))[slot] };
    argv_slot[..argv_bytes.len()].copy_from_slice(argv_bytes);
    let stored: &'static [u8] = &argv_slot[..argv_bytes.len()];

    // 破壊 (S11-7, spawn-argv-drop-last): 最後の 1 本を落とす。
    // **終端の扱いを 1 つずらす形で、雑に見ると「ちゃんと切り分けている」ように
    // 見える。** 子が受け取る `argc` が 1 つ少なくなり、`spawn-test` の検算が
    // 食い違いを捕まえる。
    let argv_count = if cfg!(feature = "spawn-argv-drop-last") {
        argv_count.saturating_sub(1)
    } else {
        argv_count
    };

    let mut argv_slices: [&'static [u8]; MAX_ARGV] = [b""; MAX_ARGV];
    let mut at = 0usize;
    for slice in argv_slices.iter_mut().take(argv_count) {
        let Some(end) = stored[at..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|i| at + i)
        else {
            crate::syscall::restore_records(saved_records);
            crate::ring3::restore_fold_record(saved_fold);
            return Err(SpawnError::ArgvMalformed);
        };
        *slice = &stored[at..end];
        at = end + 1;
    }
    let argv: &[&[u8]] = &argv_slices[..argv_count];

    let (outcome, held, leaked) = load_user_program(&mut logger, image, true, name, argv);

    // **子の終わり方をここで読む。** 戻す前に読まなければ、親のもので上書きされる。
    // **中断を先に見る（S12 前の手当て、C）。** **`exit` も畳みも通っていない**
    // ので、先に見なければ `Folded(0)` に化ける。
    let child = if crate::ring3::interrupted() {
        SpawnOutcome::Interrupted
    } else if crate::syscall::process_exited() {
        SpawnOutcome::Exited(crate::syscall::process_exit_status())
    } else {
        SpawnOutcome::Folded(crate::ring3::fault_vector())
    };
    let syscalls = crate::syscall::invocation_count();

    // **子のカーネル入場が、子の遠征スタックの上で起きたことを見る（S11-5）。**
    //
    // **入れ子で最も静かに壊れる形がこれである**——RSP0 が親のスタックを指したまま
    // だと、子のシステムコールが**親のカーネルフレームを上書きする。**
    // **子は正しく走り終え、親が戻った先で壊れる**ので、原因から離れた場所で落ちる。
    // **実測で踏んだ**（S11-5。`docs/troubleshooting.md`）。
    // **戻ってきた RSP0 が、親の遠征スタックの上端であることを確かめる（S11-5）。**
    //
    // **ここが違うと、親が次にカーネルへ入るときのスタックが変わる。**
    // **すぐには壊れない**——親はそのまま Ring 3 へ返り、次のシステムコールで
    // 別のスタックに乗る。**壊れるのは、次に子を起こして親のフレームを踏んだ
    // ときである。** 原因から遠いので、ここで突き合わせる。
    let rsp0_after = crate::gdt::privilege_stack_top();
    if rsp0_after != rsp0_before {
        logger.error(format_args!(
            "spawn: RSP0 came back as {rsp0_after:#x} but it was {rsp0_before:#x} before the \
             child ran; the parent's next kernel entry would land on the wrong stack. halting"
        ));
        common::cpu::halt_forever();
    }

    let child_handler_rsp = crate::syscall::handler_rsp();
    let (child_bottom, child_top) = crate::ring3::excursion_stack_range_at(depth);
    let handler_on_child_stack = child_handler_rsp >= child_bottom && child_handler_rsp < child_top;

    let free_after = match crate::frame_allocator::take() {
        Some(allocator) => {
            let count = allocator.free_frame_count();
            crate::frame_allocator::give_back(allocator);
            count
        }
        None => free_before,
    };

    // **親の記録を戻す。**
    //
    // 破壊 (S11-5, spawn-keep-child-records): 戻さない。**子が送ったバイト列と
    // 終了状態が、親のものとして判定行に出る。** 親（`syscall-test`）の
    // `write` の突き合わせが食い違って捕まえる。
    #[cfg(not(feature = "spawn-keep-child-records"))]
    {
        crate::syscall::restore_records(saved_records);
        crate::ring3::restore_fold_record(saved_fold);
    }

    let consumed = free_before.saturating_sub(free_after) as usize;
    // **孫のぶんを足す。** 子が更に起こしていれば、そのぶんも消えている。
    let (children_after, leaked_after) = spawn_accounting();
    let quarantined = held + children_after.saturating_sub(children_before);
    let all_leaked = leaked + leaked_after.saturating_sub(leaked_before);
    // **親の会計へ回す（S11-5）。** 隔離へ入ったフレームは世代が退くまで
    // アロケータへ戻らないので、**親から見ると消えたままである。**
    SPAWN_QUARANTINED.fetch_add(held, core::sync::atomic::Ordering::SeqCst);
    SPAWN_LEAKED.fetch_add(leaked, core::sync::atomic::Ordering::SeqCst);
    logger.info(format_args!(
        "spawn: {name} ended ({child:?}) after {syscalls} syscall(s); its kernel entries ran on \
         RSP {child_handler_rsp:#x} (inside its own excursion stack \
         {child_bottom:#x}..{child_top:#x} = {handler_on_child_stack}); the space was destroyed \
         ({consumed} frame(s) left the allocator and {quarantined} reached quarantine \
         ({held} its own + {} from what it spawned), match={} leaked={all_leaked})",
        children_after.saturating_sub(children_before),
        consumed == quarantined
    ));

    let entry = outcome.map_err(SpawnError::Load)?;
    let _ = entry;

    if consumed != quarantined || all_leaked != 0 {
        logger.error(format_args!(
            "spawn: {name} left the allocator short: {consumed} frame(s) consumed but \
             {quarantined} quarantined ({all_leaked} leaked)"
        ));
        return Err(SpawnError::DestroyAccounting {
            consumed,
            quarantined,
            leaked: all_leaked,
        });
    }

    Ok(child)
}
