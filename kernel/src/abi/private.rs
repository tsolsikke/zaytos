//! Linux の ABI ではない、ZaytOS だけの外部の決まり（`ADR-0071` の決定 1 の 2 で `crate::syscall` から移した。
//! 2026-09-30）。
//!
//! 独自のシステムコールの番号、独自の `ioctl` の要求、`spawn` と子の待ちが返す値のビットと `flags` を置く。
//! **Linux の ABI ではないものを [`crate::abi::linux`] に混ぜない。**
//!
//! **CPU によらない**——独自のシステムコールの番号は、x86_64 と aarch64 のどちらの Linux の番号とも重ならない
//! （[`ZAYTOS_PRIVATE_BASE`] の doc に、測った最大値がある）。`ioctl` の要求とビットは値をそのまま決めているので、
//! CPU で変わらない。
//!
//! **置くのは、値と値の決め方（番号の範囲、独自にした理由、引数と戻り値の形、ビットの位置）である。** 断り方や
//! 待ち方などの振る舞いの説明は、`crate::syscall` の処理の側にある。

/// ZaytOS 独自のシステムコール番号の基点（S9-a）。
///
/// # なぜ Linux の番号表から離すのか
///
/// ADR-0020 の Addendum で「番号の割り当ては Linux x86-64 から採る」と決めた。
/// **`read`=0 や `write`=1 のように Linux に対応するものがある呼び出しは、その
/// 番号を使う。** 問題は、対応するものが無い呼び出しである。下記の検証用
/// システムコールは ZaytOS 固有で、Linux に相当するものが未来にも現れない。
///
/// **かつては 0x2A・0x2B・0x2C に置いており、Linux の 42（`connect`）・
/// 43（`accept`）・44（`sendto`）と衝突していた。** 番号表の中の空きに置くと、
/// Linux がそこを埋めた時点で衝突する（歴史的に未実装のまま空いている番号も、
/// 将来 Linux が再利用しうる）。**表の中に安全な空きは無い。**
///
/// そこで表の外へまとめる。**独自の番号（0x1000 から [`SYS_NEVER_IMPLEMENTED`] の 0x10FF まで）は、x86_64 と
/// aarch64 のどちらの Linux の番号とも重ならない**——Linux の番号の最大は、x86_64 が 461（`asm/unistd_64.h`）、
/// aarch64 も 461（`asm/unistd.h`。`__NR_syscalls` が 462）で、どちらも `lsm_list_modules` である（2026-09-30 に、
/// linux-libc-dev 6.8 の x86_64 のヘッダと aarch64 のクロスのヘッダを、`gcc` と `aarch64-linux-gnu-gcc` で読んで
/// 測った）。461 から 0x1000（4096）までは 3635 離れている。**424 から上の番号は両方で同じだった**（両方にある
/// 38 個が同じ番号で、片方にだけあるものは無い）ので、2 つの表は同じ番号で伸びている。**したがって、CPU によらず
/// 同じ番号にできる。** x32 ABI が使う `0x4000_0000` のビット（`__X32_SYSCALL_BIT`）とも重ならない。
///
/// **独自の呼び出しを足すときは、必ずこの基点より上に置くこと。**
pub const ZAYTOS_PRIVATE_BASE: u64 = 0x1000;

/// 検証用 probe システムコールの番号（ZaytOS 独自。[`ZAYTOS_PRIVATE_BASE`]）。
pub const PROBE_NUMBER: u64 = ZAYTOS_PRIVATE_BASE;

/// ユーザーポインタを取る検証用システムコールの番号（M5-f-2-1）。第 1 引数が `buf`、第 2 引数が `len`。
pub const SYS_CHECK_PTR: u64 = ZAYTOS_PRIVATE_BASE + 1;

/// ユーザーバッファのバイト総和（チェックサム）を返すシステムコールの番号（M5-f-2-2）。第 1 引数が `buf`、
/// 第 2 引数が `len`。
pub const SYS_CHECKSUM: u64 = ZAYTOS_PRIVATE_BASE + 2;

/// `spawn(path, argv, envp)`——イメージを読み、子プロセスを起動し、**終わるまで待つ**（S11-5。ZaytOS 独自）。
///
/// # なぜ `fork`（57）と `execve`（59）の番号を採らないか
///
/// **これは `fork` でも `execve` でもない。** 番号だけ借りると、
/// **Linux の意味を持たない振る舞いに Linux の名前が付く。**
///
/// - **`fork` は呼び出し側を複製する。** ここが作るのは複製ではなく、
///   **別のイメージから起動した別のプロセスである。** マッピングも `argv` も引き継がない
/// - **`execve` は呼び出し側を置き換える。** ここは置き換えない——
///   **親はそのまま在り、子が終わるのを待って続きを実行する**
/// - **どちらも「戻る」の意味が違う。** `fork` は 2 回戻り、`execve` は成功したら
///   戻らない。**`spawn` は 1 回戻り、戻り値は子の終わり方である**
///
/// **`SYS_OPEN`（2）が `openat`（257）を採らなかったのと同じ判断である**
/// ——「振る舞いが違うなら、番号も分ける」。**あちらは作業ディレクトリが無いから
/// `openat` を採らず、こちらは意味が違うから 57 と 59 を採らない。**
///
/// **予約もしない。** [`SYS_NEVER_IMPLEMENTED`] のような「永久に実装しない」宣言では
/// なく、**`fork` と `execve` は将来ふつうに実装しうる**（`docs/vision.md` の
/// Linux バイナリを動かす構想）。**空けておけば、そのとき Linux の意味で使える。**
///
/// # 戻り値
///
/// 子が `exit(status)` で終わったなら `status & 0xFF`。
/// 終了させられたなら [`SPAWN_FOLDED_FLAG`] とベクタ。
/// 起動できなかったなら `-errno`。**`docs/coding-standards.md` の「`-errno` の範囲と
/// 紛れない値にする」に従い、正の側は 0x1FFF を越えない。**
pub const SYS_SPAWN: u64 = ZAYTOS_PRIVATE_BASE + 4;

/// [`SYS_SPAWN`] の戻り値のうち「子は終了ではなく畳まれて終わった」を表すビット。
///
/// **下位 8 ビットは終了状態なので、その上に置く。** 終了させられた場合は
/// `SPAWN_FOLDED_FLAG | vector` を返す（[`spawn_status`](crate::syscall::spawn_status)）。
///
/// # Linux の `wait` の符号化には合わせない
///
/// **`spawn` は Linux に対応するものが無い**ので、`W*` マクロの形を真似ても
/// 互換にはならない。**外から見える形を Linux に合わせるのは、Linux に同じものが
/// あるときの規則である。**
pub const SPAWN_FOLDED_FLAG: u64 = 0x100;

/// [`SYS_SPAWN`] の戻り値のうち「子は外から止められた」を表すビット
/// （Ctrl+C。S12 前の手当て、C）。
///
/// **[`SPAWN_FOLDED_FLAG`] の隣に置く。** 下位 8 ビットは終了状態なので、
/// **その上のビットで「終了以外の終わり方」を並べる形である。**
/// **`0x1FFF` を越えないので `-errno` と紛れない**（`SYS_SPAWN` の doc）。
///
/// # Linux の `128 + signo` を採らない
///
/// **`spawn` は Linux に対応するものが無い**（[`SPAWN_FOLDED_FLAG`] の doc）。
/// **加えて、まだシグナルが無い**——番号を持たないものに `128 + signo` の形を
/// 与えると、**「`SIGINT` が配送された」と読める値を、配送していないのに返す。**
/// **シグナルを実装する段階（(4)）で、そのとき改めて決めること。**
pub const SPAWN_INTERRUPTED_FLAG: u64 = 0x200;

/// 子を切り離して起動する（`ADR-0063` の (b3)）。**私物である**（[`SYS_SPAWN`] と同じ判断
/// ——Linux に同じ意味の入口が無い。`posix_spawn` はライブラリの関数で、システムコールではない）。
///
/// 引数は `path` / `argv` / `envp` / `flags`（[`DETACHED_STDOUT_TO_PIPE`]）。**戻り値はハンドル**
/// （`crate::task::ring3_task_handle`。(b2) の形）**か `-errno`。**
pub const SYS_SPAWN_DETACHED: u64 = ZAYTOS_PRIVATE_BASE + 5;

/// [`SYS_SPAWN_DETACHED`] の `flags`——子の fd 1 をパイプの書き端にし、読み手を予約する
/// （`crate::pipe` の doc の「読み手の予約」）。
pub const DETACHED_STDOUT_TO_PIPE: u64 = 1;

/// 予約したパイプの読み端を fd 0 にして、入れ子で起動する（`ADR-0063` の (b3)）。**私物。**
///
/// **[`SYS_SPAWN`] と同じ形で戻る**（終わり方のビット）。
/// **[`SYS_SPAWN`] に `flags` を足さない理由**——**既存の呼び手は `r10` を置かないので、
/// 4 つ目の引数を見る形にすると、置いていない値を読む。**
pub const SYS_SPAWN_WITH_PIPED_STDIN: u64 = ZAYTOS_PRIVATE_BASE + 6;

/// 切り離して起動した子を待って回収する（`ADR-0063` の (b3)）。**私物。**
///
/// **引数はハンドル。** **戻り値は終わり方のビット**（[`SYS_SPAWN`] と同じ）**か `-ECHILD`**
/// （ハンドルが合わない・終わった後の二重待ち）。**`wait4` を採らない**——**形が合わない**
/// （`docs/architecture.md` の「合わせるのは合わせられる形について」）。
pub const SYS_WAIT_CHILD: u64 = ZAYTOS_PRIVATE_BASE + 7;

/// 入力の生イベントの fd を開く（`ADR-0066` の Y-a）。**私物。**
///
/// **Linux に対応する syscall が無い**——**あちらは `/dev/input/eventX` を `open` する**が、
/// ZaytOS に装置のファイルシステムは無い。**したがって番号は私物にする**（`SYS_SPAWN` 等と
/// 同じ。`ADR-0020` の「合わせられる形について合わせる」）。
///
/// **読みは `read` が `struct input_event` を返す。**
pub const SYS_OPEN_INPUT: u64 = ZAYTOS_PRIVATE_BASE + 8;

/// 画面を開く入口の番号（`ADR-0066` の Y-c）。**開くと図形モードへ入る。**
///
/// **Linux に対応する syscall が無い**——**あちらは `/dev/fb0`（fbdev）か `/dev/dri/card0`（DRM）を
/// `open` する**が、ZaytOS に装置のファイルシステムは無い。**番号は私物にする**（[`SYS_OPEN_INPUT`] と
/// 同じ理由）。**開いた後の形は Linux の fbdev に合わせる**——**形は `ioctl` の
/// [`FBIOGET_VSCREENINFO`](crate::abi::linux::FBIOGET_VSCREENINFO) /
/// [`FBIOGET_FSCREENINFO`](crate::abi::linux::FBIOGET_FSCREENINFO)、画素は `mmap`。**
pub const SYS_OPEN_SCREEN: u64 = ZAYTOS_PRIVATE_BASE + 9;

/// **永久に実装しない番号**（S9-b-3-2a）。`-ENOSYS` の的である。
///
/// # なぜ「空いている番号」で済ませないか
///
/// **未実装の番号は、いつか実装される。** そのとき、`-ENOSYS` が返ることを
/// 確かめていた検査は静かに別のものを見はじめる（戻り値が変わるので落ちはするが、
/// **落ちた理由が「実装したから」だと分かる材料がどこにも無い**）。
///
/// **予約しておけば、実装しようとした人がこの doc を読む。** [`ZAYTOS_PRIVATE_BASE`]
/// の上に置くので、Linux の番号表とも衝突しない。
pub const SYS_NEVER_IMPLEMENTED: u64 = ZAYTOS_PRIVATE_BASE + 0xFF;

/// `TIOCZTAKE`——溜まっているエラーを取り出す要求（ADR-0046）。
///
/// # ZaytOS の値である。Linux の値ではない
///
/// **Linux にこの操作は無い**ので、**`TIOC` の空間の外に置く**
/// （`0x5A` は `Z`。`TIOCGWINSZ` の `0x5413` と衝突しない）。
///
/// **新しい syscall 番号は作らない**——**`ADR-0020`は番号を Linux から
/// 採ると決めており、相当する番号が無い。** **`ioctl`は端末固有の操作の
/// ための入口である。**
pub const TIOCZTAKE: u64 = 0x5A01;

/// `TIOCZLOG`——1 行をログ（シリアル）へ出す要求（ADR-0046）。
pub const TIOCZLOG: u64 = 0x5A02;

/// 画面の矩形をコピーする要求（ZaytOS 独自。`ADR-0066` の Y-c）。**引数は `struct drm_clip_rect`。**
///
/// **fbdev に対応するものが無い**——**fbdev は実物のフレームバッファをマップするので、コピーする必要が無い。**
/// **ZaytOS は裏バッファをマップする**（Q1。MMIO を Ring 3 へ出さない）**ので、コピーする入口が要る。**
/// **Linux で近いのは DRM の `DRM_IOCTL_MODE_DIRTYFB` で、矩形の配置（`struct drm_clip_rect`）だけを
/// 採る**——**DIRTYFB そのものは DRM の大きな ABI の一部なので採らない。** **番号は [`TIOCZTAKE`] と
/// 同じ `'Z'` の帯に置く。**
pub const FBIOZPRESENT: u64 = 0x5A03;
