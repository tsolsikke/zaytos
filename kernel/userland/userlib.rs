//! ユーザープログラムが共有する最小の包み（S11-9）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。**`kernel/build.rs` が
//! `rustc` を呼ぶとき、根のファイルから `mod userlib;` で取り込まれる。**
//! **`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # なぜ置いたか。**同じものを 2 箇所で書いたから**
//!
//! **抽出の条件は「2 つ目のファイルシステムを足すとき、または同じ操作を 2 箇所で
//! 書いたとき」である**（`kernel/src/vfs.rs` が trait を引かない理由として書いた
//! 条件を、そのまま使う）。**`ls` と `cat` が 2 箇所目になる。**
//!
//! # 既存の 4 本は `global_asm!` のまま残す
//!
//! `hello` / `fault-test` / `syscall-test` / `spawn-test` は**この包みを使わない。**
//! **あれらは ABI そのものの検算である**——`syscall-test` の doc が書いているとおり、
//! **「asm で `mov rdi, ...` と書き、アセンブラが符号化し、リンカが配置し、
//! ローダーが写像し、`iretq` が飛ばす」**ことを確かめている。
//! **包みを通すと、確かめている当のものが包みの中へ隠れる。**
//!
//! **したがって「4 箇所で書いている」は重複ではない。** 重複になるのは、
//! **検算ではないプログラムが 2 本目を数えたときである。**

#![allow(dead_code)]

/// `read` の番号（Linux と同じ）。
pub const SYS_READ: u64 = 0;
/// `write` の番号（Linux と同じ）。
pub const SYS_WRITE: u64 = 1;
/// `open` の番号（Linux と同じ）。
pub const SYS_OPEN: u64 = 2;
/// `close` の番号（Linux と同じ）。
pub const SYS_CLOSE: u64 = 3;

/// `ioctl(fd, request, arg)`（e-1）。**カーネルの `SYS_IOCTL` と同じ値である。**
pub const SYS_IOCTL: u64 = 16;

/// 端末の大きさを訊く要求（`TIOCGWINSZ`。e-1）。
///
/// **カーネルの `TIOCGWINSZ` と同じ値である**（`SYS_*` の番号を写しているのと
/// 同じ形。ユーザープログラムはカーネルの定数を参照できない）。
pub const TIOCGWINSZ: u64 = 0x5413;

/// 溜まっているエラーを取り出す要求（`TIOCZTAKE`。ADR-0046）。
///
/// **カーネルの `TIOCZTAKE` と同じ値である。**
pub const TIOCZTAKE: u64 = 0x5A01;

/// 1 行をログ（シリアル）へ出す要求（`TIOCZLOG`。ADR-0046）。
///
/// **カーネルの `TIOCZLOG` と同じ値である。**
pub const TIOCZLOG: u64 = 0x5A02;

/// `TIOCZTAKE` / `TIOCZLOG` の構造の大きさ（ADR-0046）。
pub const ZDIAG_LEN: usize = 256;

/// その構造の本文が始まる位置（ADR-0046）。
pub const ZDIAG_TEXT_OFFSET: usize = 4;

/// 本文に使える大きさ（ADR-0046）。
pub const ZDIAG_TEXT: usize = ZDIAG_LEN - ZDIAG_TEXT_OFFSET;
/// `exit` の番号（Linux と同じ）。
pub const SYS_EXIT: u64 = 60;
/// `getdents64` の番号（Linux と同じ）。
pub const SYS_GETDENTS64: u64 = 217;

/// 読み取りで開く（`O_RDONLY`）。
pub const O_RDONLY: u64 = 0;

/// 書き込みで開く（`O_WRONLY`。ADR-0037）。
pub const O_WRONLY: u64 = 1;

/// 開くと同時に長さ 0 へ切る（`O_TRUNC`。ADR-0037）。
pub const O_TRUNC: u64 = 0o1000;

/// 無ければ作る（`O_CREAT`。e-5。ADR-0037 の Addendum）。
pub const O_CREAT: u64 = 0o100;

/// `-ENOENT`（そのパスは無い）。**新規作成の判別に使う。**
pub const MINUS_ENOENT: i64 = -2;

/// 標準出力の fd。
pub const STDOUT: u64 = 1;
/// 標準エラー出力の fd。
pub const STDERR: u64 = 2;

/// `linux_dirent64` の欄の位置（実測。`kernel/src/syscall.rs` の写しである）。
pub const DIRENT_RECLEN_OFFSET: usize = 16;
/// `linux_dirent64` の名前の開始位置。
pub const DIRENT_NAME_OFFSET: usize = 19;

/// システムコールを 1 回発行する（引数 3 つまで）。
///
/// # 戻り値は符号つきである
///
/// **失敗は `-errno` で返る**（`ADR-0020`）。`-1..-4095` の範囲を負の整数として
/// そのまま受ける。
///
/// # Safety
///
/// 番号と引数がそのシステムコールの契約を満たすこと。**ポインタを渡す場合は、
/// カーネルが読み書きしてよい範囲を指していること。**
pub unsafe fn syscall3(number: u64, a: u64, b: u64, c: u64) -> i64 {
    let ret: i64;
    // SAFETY: 呼び出し元契約による。`int 0x80` は RAX 以外を保存して戻る
    // （スタブが `IrqContext` へ積んで復元する）。
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") number => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
        );
    }
    ret
}

/// `exit(status)`。**戻らない。**
pub fn exit(status: u64) -> ! {
    // SAFETY: `exit` は引数を 1 つ取り、戻らない。
    unsafe { syscall3(SYS_EXIT, status, 0, 0) };
    // **戻ってきた場合の行き先。** 受け皿の `ud2` へ落ちる。
    // SAFETY: `exit` が効かなかったということなので、確定的に落とす。
    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}

/// 端末の大きさ（e-1）。`ioctl(TIOCGWINSZ)` が返す `struct winsize` である。
///
/// **`0` は「分からない」である**（`kernel/src/syscall.rs` の `sys_ioctl`）。
/// **画面を持たない文脈で走ると 0 が返る**ので、**使う側は 0 を確かめること。**
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WindowSize {
    pub rows: u16,
    pub columns: u16,
    pub width_pixels: u16,
    pub height_pixels: u16,
}

/// 大きさが分からないときの既定の行数（e-1）。**端末の慣行である。**
pub const DEFAULT_ROWS: u16 = 24;
/// 大きさが分からないときの既定の桁数（e-1）。**端末の慣行である。**
pub const DEFAULT_COLUMNS: u16 = 80;

impl WindowSize {
    /// 0 と失敗を既定へ落とす（e-1）。**使う側はこれを通すこと。**
    ///
    /// # なぜ 1 箇所で決めるのか
    ///
    /// **`0` は「分からない」で、返りうる**（画面を持たない文脈で走ったとき）。
    /// **使う側が「行数 - 2」のような計算をすると、0 は負か 0 除算になる。**
    /// **各所で場当たりに守る形にすると、守り忘れた場所だけが落ちる。**
    ///
    /// **落とす先は 24x80 である**——**端末の慣行で、`vi` も `less` もこれを
    /// 既定にしている。** **ピクセルは 0 のままにする**——**使う者がまだ
    /// 居らず、代わりに置ける慣行の値も無い**（画面が無いのだから、
    /// 「たぶんこのくらい」を置く根拠が無い）。
    ///
    /// **判定は落とす前の値を見る**（`zi` の判定行）——**落とした後を見ると、
    /// 「訊けた」と「訊けなかったが既定へ落ちた」が同じ値になる。**
    pub fn or_default(self) -> Self {
        Self {
            rows: if self.rows == 0 {
                DEFAULT_ROWS
            } else {
                self.rows
            },
            columns: if self.columns == 0 {
                DEFAULT_COLUMNS
            } else {
                self.columns
            },
            width_pixels: self.width_pixels,
            height_pixels: self.height_pixels,
        }
    }
}

/// 端末の大きさを訊き、分からなければ既定へ落とす（e-1）。
///
/// **使う側の入口はこちらである。** [`window_size`] は生の値を返すので、
/// **判定と診断が使う**（落とす前と後を区別するため。[`WindowSize::or_default`]）。
pub fn window_size_or_default(fd: u64) -> WindowSize {
    window_size(fd).unwrap_or(WindowSize {
        rows: 0,
        columns: 0,
        width_pixels: 0,
        height_pixels: 0,
    })
    .or_default()
}

/// 端末の大きさを訊く（e-1）。**失敗したら `-errno` を返す。**
///
/// **`fd` は端末であること**——そうでなければカーネルが `-ENOTTY` を返す。
///
/// **返るのは生の値である**（`0` を含む）。**使う側は
/// [`window_size_or_default`] を通すこと。**
pub fn window_size(fd: u64) -> Result<WindowSize, i64> {
    let mut raw = [0u8; 8];
    // SAFETY: `raw` は自分のスタックの上にあり、カーネルが書く長さ（8）を収める。
    let ret = unsafe { syscall3(SYS_IOCTL, fd, TIOCGWINSZ, raw.as_mut_ptr() as u64) };
    if ret < 0 {
        return Err(ret);
    }
    Ok(WindowSize {
        rows: u16::from_le_bytes([raw[0], raw[1]]),
        columns: u16::from_le_bytes([raw[2], raw[3]]),
        width_pixels: u16::from_le_bytes([raw[4], raw[5]]),
        height_pixels: u16::from_le_bytes([raw[6], raw[7]]),
    })
}

/// カーネルが溜めたエラーの控え（ADR-0046）。
///
/// # 全画面のアプリのためのものである
///
/// **代替画面に居る間、`fd 2`へ書いたものはカーネルが溜める。**
/// **画面へ勝手に描かれると絵が壊れる**ためで、**取り出して自分の
/// エコーエリアへ描くのはアプリの仕事である**（ADR-0046）。
///
/// **`zi`と`less`が同じものを使う。** **描く場所だけがそれぞれ違う**
/// （`zi`はコマンド行、`less`は状態行）。
///
/// # 次に取り出すまで残す
///
/// **[`Self::take`]は、溜まっていなければ前の中身を保つ。** **`vi`と同じで、
/// 出した瞬間に消えると読めない。** **消すのは呼ぶ側である**（[`Self::clear`]）。
pub struct Echo {
    text: [u8; ZDIAG_TEXT],
    len: usize,
}

/// 溢れたことを示す印（ADR-0046）。**捨てた数そのものは出さない。**
///
/// **1 行に収まる長さで、かつ「これで全部ではない」と分かる形にする。**
/// **正確な数はシリアルにある**——**そちらが記録で、こちらは控えである。**
const ECHO_TRUNCATED: &[u8] = b" ...";

impl Echo {
    pub const fn new() -> Self {
        Self {
            text: [0u8; ZDIAG_TEXT],
            len: 0,
        }
    }

    /// カーネルから取り出す。**新しく出てきたら真。**
    ///
    /// **偽のときは前の中身が残る**（この型の doc）。
    pub fn take(&mut self, fd: u64) -> bool {
        let mut raw = [0u8; ZDIAG_LEN];
        // SAFETY: `raw` は自分のスタックの上にあり、カーネルが書く長さ
        // （`ZDIAG_LEN`）をちょうど収める。
        let ret = unsafe { syscall3(SYS_IOCTL, fd, TIOCZTAKE, raw.as_mut_ptr() as u64) };
        if ret < 0 {
            return false;
        }
        let length = u16::from_le_bytes([raw[0], raw[1]]) as usize;
        if length == 0 || length > ZDIAG_TEXT {
            return false;
        }
        let dropped = u16::from_le_bytes([raw[2], raw[3]]);
        self.len = 0;
        // **改行は落とす。** **エコーエリアは 1 行である**——
        // **そのまま出すと、次の行へ送ってしまう。**
        for byte in &raw[ZDIAG_TEXT_OFFSET..ZDIAG_TEXT_OFFSET + length] {
            if *byte == b'\n' || *byte == b'\r' {
                continue;
            }
            self.text[self.len] = *byte;
            self.len += 1;
        }
        // **溢れたことを隠さない（ADR-0046）。** **入らなければ印も出さない**
        // ——**その場合は本文のほうが情報である。**
        if dropped > 0 && self.len + ECHO_TRUNCATED.len() <= ZDIAG_TEXT {
            self.text[self.len..self.len + ECHO_TRUNCATED.len()].copy_from_slice(ECHO_TRUNCATED);
            self.len += ECHO_TRUNCATED.len();
        }
        self.len > 0
    }

    /// 出す 1 行。**空なら空である。**
    pub fn line(&self) -> &[u8] {
        &self.text[..self.len]
    }

    /// 空にする。
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl Default for Echo {
    fn default() -> Self {
        Self::new()
    }
}

/// 1 行をログ（シリアル）へ出す（ADR-0046）。
///
/// # 画面へは出ない
///
/// **診断の出口である。** **読み手はホスト側の判定で、判定はシリアルを
/// 読んでいる**（ADR-0046 の「線はどこに在るか」）。**全画面のアプリの
/// 画面を、診断が壊してよい理由は無い。**
///
/// **使う人へのエラーはこちらへ出さない**——あちらは`STDERR`で、
/// カーネルが溜め、アプリがエコーエリアへ描く。
pub fn log_line(fd: u64, bytes: &[u8]) {
    let mut raw = [0u8; ZDIAG_LEN];
    let take = bytes.len().min(ZDIAG_TEXT);
    raw[0..2].copy_from_slice(&(take as u16).to_le_bytes());
    raw[ZDIAG_TEXT_OFFSET..ZDIAG_TEXT_OFFSET + take].copy_from_slice(&bytes[..take]);
    // SAFETY: `raw` は自分のスタックの上にあり、カーネルが読む長さ
    // （`ZDIAG_LEN`）をちょうど収める。
    let _ = unsafe { syscall3(SYS_IOCTL, fd, TIOCZLOG, raw.as_ptr() as u64) };
}

/// 1 画面ぶんを組み立ててから 1 回で送る器（PERF-b）。
///
/// # なぜ在るのか
///
/// **`write` のたびにカーネルへ入り、BKL を取り直している**（`sys_write` は
/// 画面へ書く前に解いて、書いてから取り直す）。**`less` の 1 回の移動で
/// `write` が 152 回だった**（実測。`ADR-0047` の表）——**1 行につき 3 回
/// （カーソル移動・行消去・本文）である。**
///
/// **運ぶ量は軽い**（1 回の移動で 1,529 バイト）。**重いのは回数のほうである。**
///
/// # 自由関数である。持ち回らない
///
/// **画面は 1 つで、プログラムも 1 本である。** **引数で持ち回る形にすると、
/// 描く関数すべての署名が変わる**——`zi` は描く関数を 7 つ持っている。
/// **`userlib::heap` が静的 1 本を貸すのと同じ立場である。**
///
/// # 置き場所は `.bss` の固定配列である
///
/// **ヒープは使わない。** **`userlib::heap` は 1 本しか貸さず、`zi` と `less` は
/// それを本文に使っている。**
///
/// **大きさは FHD の上限から取る**（`240 * 67 = 16080` セル。カーネルの
/// `MAX_TERMINAL_CELLS` と同じ根拠）。**行ごとの制御と色の列のぶんを足して
/// 丸めた値である。**
///
/// # 溢れたら送る。捨てない
///
/// **入りきらなければ、そこまでを送ってから続きを溜める。**
/// **`write` の回数が増えるだけで、出るものは変わらない。**
pub const FRAME_MAX: usize = 20 * 1024;

/// 器の実体。**`.bss` に置く。**
///
/// # なぜ `static mut` なのか
///
/// **単一の実行文脈である**（このモジュールの doc。`heap` の `BASE` と同じ立場）。
static mut FRAME_BUFFER: [u8; FRAME_MAX] = [0u8; FRAME_MAX];

/// 溜まっている量。
static mut FRAME_USED: usize = 0;

/// 溜める（PERF-b）。**入りきらなければ、そこまでを送ってから続ける。**
pub fn frame_push(fd: u64, bytes: &[u8]) {
    // 破壊 (PERF-b, frame-write-per-piece): 溜めずに、来たそのつど送る。
    // **PERF-b の前の形そのものである**——**システムコールの回数が桁で増える。**
    // **出るものは変わらない**ので、**画面を読む判定は1つも落ちない。**
    #[cfg(frame_write_per_piece)]
    {
        write_all(fd, bytes);
        return;
    }
    #[cfg(not(frame_write_per_piece))]
    {
    let mut at = 0usize;
    while at < bytes.len() {
        // SAFETY: 単一の実行文脈である（[`FRAME_MAX`] の doc）。値を読むだけ。
        let used = unsafe { FRAME_USED };
        if used == FRAME_MAX {
            frame_flush(fd);
            continue;
        }
        let take = (FRAME_MAX - used).min(bytes.len() - at);
        // SAFETY: 単一の実行文脈であり、`used + take` は `FRAME_MAX` を越えない。
        // **参照は作らず、ポインタで写す。**
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr().add(at),
                core::ptr::addr_of_mut!(FRAME_BUFFER).cast::<u8>().add(used),
                take,
            );
            FRAME_USED = used + take;
        }
        at += take;
    }
    }
}

/// 溜めたものを 1 回で送る（PERF-b）。**空なら何もしない。**
pub fn frame_flush(fd: u64) {
    // SAFETY: 単一の実行文脈である。
    let used = unsafe { FRAME_USED };
    if used == 0 {
        return;
    }
    // 破壊 (PERF-b, frame-write-per-piece-test): 溜めずに、来たそのつど送る。
    // **PERF-b の前の形そのものである**——**システムコールの回数が桁で増える。**
    // **出るものは変わらない**ので、画面を読む判定は 1 つも落ちない。
    // SAFETY: 単一の実行文脈であり、`used` は `FRAME_MAX` を越えない。
    let bytes =
        unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(FRAME_BUFFER).cast::<u8>(), used) };
    write_all(fd, bytes);
    // SAFETY: 単一の実行文脈である。
    unsafe { FRAME_USED = 0 };
}

/// バイト列を fd へ**すべて**書く。書けた総数、または最初の `-errno` を返す。
///
/// # 繰り返す形にした
///
/// **`write` は「届いた分だけ」を返しうる**（`kernel/src/syscall.rs` の `sys_write`
/// が、途中で検証に失敗したらそこまでの数を返す）。**Linux も同じである。**
///
/// **「1 回で全部書ける」は `write` の実装に依存する仮定である。**
/// いまの実装では 1 回で全部書けるが、**その仮定を呼び出し側へ持ち込まない。**
/// **繰り返す形は、仮定が変わっても壊れない。**
pub fn write_all(fd: u64, bytes: &[u8]) -> i64 {
    let mut done = 0usize;
    while done < bytes.len() {
        // SAFETY: `bytes` は自分の像かスタックの中で、残りの長さを正しく渡す。
        let written = unsafe {
            syscall3(
                SYS_WRITE,
                fd,
                bytes.as_ptr() as u64 + done as u64,
                (bytes.len() - done) as u64,
            )
        };
        if written <= 0 {
            // **0 も失敗として扱う。** 進まないので、繰り返しても終わらない
            // （`common::ext2` の走査と同じ形で、進む量が正でなければ止める）。
            return if done == 0 { written } else { done as i64 };
        }
        done += written as usize;
    }
    done as i64
}

/// `open(path, O_RDONLY)`。**パスは NUL 終端であること。**
pub fn open_read_only(path: &[u8]) -> i64 {
    // SAFETY: `path` は NUL 終端のバイト列を指す。
    unsafe { syscall3(SYS_OPEN, path.as_ptr() as u64, O_RDONLY, 0) }
}

/// 書き込みで開き、同時に長さ 0 へ切る（`O_WRONLY|O_TRUNC`。zi-d-2）。
///
/// **カーネルが受理する 2 形のうちの一方である**（ADR-0037）。
/// **読みながら書き先を開いておくことはできない**——open の時点で切るので、
/// **読み切って閉じてから開き直す。**
pub fn open_write_truncate(path: &[u8]) -> i64 {
    // SAFETY: `path` は NUL 終端のバイト列を指す。
    unsafe { syscall3(SYS_OPEN, path.as_ptr() as u64, O_WRONLY | O_TRUNC, 0) }
}

/// 無ければ作り、書き込みで開き、長さ 0 へ切る
/// （`O_WRONLY|O_CREAT|O_TRUNC`。e-5。ADR-0037 の Addendum）。
///
/// **`O_CREAT` 単独は受理されない**——**位置書きの部品が無いので、
/// 作った後にできるのは全置換だけである。**
pub fn open_write_create(path: &[u8]) -> i64 {
    // SAFETY: `path` は NUL 終端のバイト列を指す。
    unsafe {
        syscall3(
            SYS_OPEN,
            path.as_ptr() as u64,
            O_WRONLY | O_CREAT | O_TRUNC,
            0,
        )
    }
}

/// `lseek` の番号（Linux と同じ。DIR-1b）。
pub const SYS_LSEEK: u64 = 8;
/// `unlink` の番号（Linux と同じ。DIR-1b）。
pub const SYS_UNLINK: u64 = 87;
/// `lseek` の `whence`——先頭からの絶対位置。**カーネルはこれだけ受ける。**
pub const SEEK_SET: u64 = 0;

/// `stat` の番号（Linux と同じ。DIR-1b）。
pub const SYS_STAT: u64 = 4;

/// `struct stat` のバイト数（x86-64 の Linux。カーネルの `STAT_SIZE` と同じ）。
pub const STAT_BYTES: usize = 144;

/// `struct stat` の `st_size` の位置（実測。`syscall-test` の `STAT_SIZE_OFFSET`）。
pub const STAT_SIZE_OFFSET: usize = 48;

/// ファイルの大きさを訊く（DIR-1b）。
///
/// **`stat` の欄をここで解かない。** **`st_size` の 8 バイトだけを取り出す**
/// ——**他の欄を読む者がまだ居ない。**
///
/// # Safety
///
/// `path` が NUL 終端のバイト列を指すこと。
pub unsafe fn size_of_file(path: &[u8]) -> Result<u64, i64> {
    let mut buffer = [0u8; STAT_BYTES];
    // SAFETY: `path` は NUL 終端で、`buffer` は `STAT_BYTES` を収める。
    let status = unsafe {
        syscall3(
            SYS_STAT,
            path.as_ptr() as u64,
            buffer.as_mut_ptr() as u64,
            0,
        )
    };
    if status < 0 {
        return Err(status);
    }
    let mut size = [0u8; 8];
    size.copy_from_slice(&buffer[STAT_SIZE_OFFSET..STAT_SIZE_OFFSET + 8]);
    Ok(u64::from_le_bytes(size))
}

/// `lseek(fd, offset, SEEK_SET)`（DIR-1b）。**戻るのは新しい位置である。**
///
/// **`whence` を引数に取らない。** **カーネルが `SEEK_SET` しか受けない**ので、
/// **渡せない値を渡せる形にしない。**
pub fn seek_to(fd: u64, offset: u64) -> i64 {
    // SAFETY: 引数は fd と数だけである。
    unsafe { syscall3(SYS_LSEEK, fd, offset, SEEK_SET) }
}

/// `mkdir` の番号（Linux と同じ。DIR-1c）。
pub const SYS_MKDIR: u64 = 83;
/// `rmdir` の番号（Linux と同じ。DIR-1c）。
pub const SYS_RMDIR: u64 = 84;

/// `mkdir(path)`（DIR-1c）。**親が無ければ `-ENOENT`（`-p` は無い）。**
///
/// # Safety
///
/// `path` が NUL 終端のバイト列を指すこと。
pub unsafe fn mkdir(path: &[u8]) -> i64 {
    // SAFETY: 呼び出し元契約により `path` は NUL 終端である。
    unsafe { syscall3(SYS_MKDIR, path.as_ptr() as u64, 0, 0) }
}

/// `rmdir(path)`（DIR-1c）。**空でなければ `-ENOTEMPTY`。**
///
/// # Safety
///
/// `path` が NUL 終端のバイト列を指すこと。
pub unsafe fn rmdir(path: &[u8]) -> i64 {
    // SAFETY: 呼び出し元契約により `path` は NUL 終端である。
    unsafe { syscall3(SYS_RMDIR, path.as_ptr() as u64, 0, 0) }
}

/// `unlink(path)`（DIR-1b）。**通常ファイルだけを消せる。**
///
/// **ディレクトリなら `-EISDIR` が返る**（`rmdir` を使うこと）。
///
/// # Safety
///
/// `path` が NUL 終端のバイト列を指すこと。
pub unsafe fn unlink(path: &[u8]) -> i64 {
    // SAFETY: 呼び出し元契約により `path` は NUL 終端である。
    unsafe { syscall3(SYS_UNLINK, path.as_ptr() as u64, 0, 0) }
}

/// `close(fd)`。
pub fn close(fd: u64) -> i64 {
    // SAFETY: 引数は fd だけである。
    unsafe { syscall3(SYS_CLOSE, fd, 0, 0) }
}

/// `read(fd, buf, len)`。
pub fn read(fd: u64, buf: &mut [u8]) -> i64 {
    // SAFETY: `buf` は自分のスタックの中で、長さを正しく渡す。
    unsafe { syscall3(SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

/// `getdents64(fd, buf, len)`。
pub fn getdents64(fd: u64, buf: &mut [u8]) -> i64 {
    // SAFETY: `buf` は自分のスタックの中で、長さを正しく渡す。
    unsafe {
        syscall3(
            SYS_GETDENTS64,
            fd,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    }
}

/// `spawn` の番号（`ZAYTOS_PRIVATE_BASE + 4`。ZaytOS 独自）。
pub const SYS_SPAWN: u64 = 0x1004;

/// `spawn(path, argv)`。**子が終わるまで戻らない。**
///
/// 戻り値は子の終了状態（`0..=255`）か `-errno` である
/// （`kernel/src/syscall.rs` の `SYS_SPAWN`）。
///
/// # Safety
///
/// `path` が NUL 終端であること。`argv` が NULL 終端のポインタ配列で、
/// **各要素が NUL 終端の文字列を指していること。**
pub unsafe fn spawn(path: &[u8], argv: &[*const u8], envp: &[*const u8]) -> i64 {
    // SAFETY: 呼び出し元契約による。
    unsafe {
        syscall3(
            SYS_SPAWN,
            path.as_ptr() as u64,
            argv.as_ptr() as u64,
            envp.as_ptr() as u64,
        )
    }
}

/// `argc` と `argv` を、`_start` の時点の `rsp` から読む。
///
/// # 形は Linux と同じである
///
/// `rsp` の指す先から順に、`argc`・`argv` のポインタ・NULL・`envp` のポインタ・
/// NULL・`auxv` である（`kernel/src/userland.rs` の `build_initial_stack`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
pub unsafe fn argument(stack: *const u64, index: usize) -> Option<*const u8> {
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let argc = unsafe { *stack } as usize;
    if index >= argc {
        return None;
    }
    // SAFETY: `argv` は `argc` 本ぶん並んでおり、添字は範囲内である。
    let pointer = unsafe { *stack.add(1 + index) };
    if pointer == 0 {
        None
    } else {
        Some(pointer as *const u8)
    }
}

/// `envp` の `index` 番目を返す（f-2。`ADR-0053`）。**終端に達したら `None`。**
///
/// # 名前で引く口と分けてある
///
/// **[`environment`] は名前で引く。** **こちらは並びをそのまま歩く**
/// ——**シェルが起動時に自分の表へ写すために要る**（`ADR-0053` の Decision 1）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
pub unsafe fn environment_at(stack: *const u64, index: usize) -> Option<*const u8> {
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let argc = unsafe { *stack } as usize;
    // `argc` の 1 語 + `argv` の `argc` 本 + `argv` の終端 1 語。
    let at = 1 + argc + 1 + index;
    // SAFETY: `envp` は NULL で終わる。**終端より先は読まない**——
    // 呼ぶ側は `None` が返った時点で止める契約である。
    let pointer = unsafe { *stack.add(at) };
    if pointer == 0 {
        None
    } else {
        Some(pointer as *const u8)
    }
}

/// 環境変数を引く（EV。ADR-0041）。**`getenv` の最小である。**
///
/// # 形は Linux と同じである
///
/// **`envp` は `argv` の終端の次から始まり、NULL で終わる**
/// （`kernel/src/userland.rs` の `build_initial_stack`）。
/// **要素は `NAME=VALUE` の NUL 終端バイト列である。**
///
/// # `name` に `=` を含めないこと
///
/// **突き合わせるのは `name` と、それに続く `=` である。** `TERM` を渡すと
/// `TERM=` で始まる要素を探し、**返るのは `=` の次を指すポインタである。**
///
/// **前方一致では引かない**——`TERM` が `TERMINFO` に当たってしまう。
///
/// # 見つからなければ `None`
///
/// **既定値をここで決めない。** 決めるのは読む側である
/// （`zash` は「無ければ色を付けない」を選んだ）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
pub unsafe fn environment(stack: *const u64, name: &[u8]) -> Option<*const u8> {
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let argc = unsafe { *stack } as usize;
    // `argc` の 1 語 + `argv` の `argc` 本 + `argv` の終端 1 語。
    let mut at = 1 + argc + 1;

    loop {
        // SAFETY: `envp` は NULL で終わる。終端まで歩く。
        let pointer = unsafe { *stack.add(at) };
        if pointer == 0 {
            return None;
        }
        let entry = pointer as *const u8;

        // **`name` と、それに続く `=` を突き合わせる。**
        let mut index = 0usize;
        let matched = loop {
            // SAFETY: 要素はカーネルが NUL 終端で積んだ文字列である。
            let byte = unsafe { *entry.add(index) };
            if index == name.len() {
                break byte == b'=';
            }
            if byte == 0 || byte != name[index] {
                break false;
            }
            index += 1;
        };
        if matched {
            // SAFETY: 上で `=` を見た位置の次である。
            return Some(unsafe { entry.add(name.len() + 1) });
        }
        at += 1;
    }
}

/// NUL 終端のバイト列の長さ（NUL を含まない）を数える。**上限つきである。**
///
/// # 上限が要る
///
/// **NUL が無い入力でも必ず止まる**（`common::ext2` の走査と同じ形で、
/// 進む量が正で上限が有限である）。
///
/// # Safety
///
/// `ptr` が読める範囲を指していること。
pub unsafe fn length_of(ptr: *const u8, limit: usize) -> usize {
    let mut length = 0usize;
    while length < limit {
        // SAFETY: 呼び出し元契約により、上限までは読める。
        if unsafe { *ptr.add(length) } == 0 {
            break;
        }
        length += 1;
    }
    length
}

/// `getdents64` が返した緩衝を歩き、名前を 1 つずつ渡す。
///
/// **`d_reclen` を頼りに進む。** 0 なら止める——**進まない量で歩き続けない。**
///
/// # Safety
///
/// `buf` が `getdents64` の返したバイト数ぶんの、正しいレコード列であること。
pub fn for_each_dirent(buf: &[u8], mut body: impl FnMut(&[u8])) {
    let mut at = 0usize;
    while at + DIRENT_NAME_OFFSET <= buf.len() {
        let reclen =
            u16::from_le_bytes([buf[at + DIRENT_RECLEN_OFFSET], buf[at + DIRENT_RECLEN_OFFSET + 1]])
                as usize;
        if reclen < DIRENT_NAME_OFFSET || at + reclen > buf.len() {
            // **壊れたレコードでは止める。** 名前の終わりが読めない。
            return;
        }
        let name_start = at + DIRENT_NAME_OFFSET;
        let mut name_end = name_start;
        while name_end < at + reclen && buf[name_end] != 0 {
            name_end += 1;
        }
        body(&buf[name_start..name_end]);
        at += reclen;
    }
}

/// `brk` の番号（Linux と同じ。H-b-1。ADR-0044）。
///
/// **`brk(0)` は問い合わせである**——**いまの上端が返る。**
/// **`sbrk` は無い**（libc の側の話である。`kernel/src/syscall.rs` の `SYS_BRK`）。
pub const SYS_BRK: u64 = 12;

/// ユーザーのヒープ（H-b-1。ADR-0044）。
///
/// # `malloc` ではない
///
/// **1 本の伸びる領域だけを持つ。** **free リストも複数ブロックも持たない。**
/// **`malloc` の下地と呼ばない**——**要る者が来たら、そのとき広げる。**
///
/// # 領域は 1 本だけである。伸びるが、増えない
///
/// **`brk` が答えるのは「どこまで使ってよいか」だけである**（ADR-0044 の
/// 「決めないこと」）。**2 本目を取れる形にすると、「どちらを先に返すか」と
/// 「間に空いた穴をどうするか」が始まる**——**それは割り当て器の仕事である。**
/// **2 回目の [`heap::reserve`] は `None` を返す。**
///
/// **伸ばすのは [`heap::grow_to`] である**（H-b-2）。**同じ 1 本が長くなるだけで、
/// 下端は動かない**——**`brk` は上端しか動かさない。**
///
/// # 一意の参照を受け渡して守る（H-b-2）
///
/// **[`heap::grow_to`] と [`heap::release`] は、領域の `&'static mut [u8]` を
/// 値で受け取る。** **一意の参照は `Copy` ではないので、渡した側は以後それを
/// 使えない。** **「返した領域を後から触らない」を、契約ではなく所有で守る形である。**
///
/// **H-b-1 は「戻らない関数の中でしか返さない」という構造で守っていた。**
/// **b-2 で `grow_to` が所有の形を要求したので、`release` も同じ形へ寄せた**
/// ——**保証の出所を 2 種類持たない。**
///
/// # 単一の実行文脈を前提にする
///
/// **同じ空間に 2 本の実行文脈は居ない**（スレッドが無い。ADR-0044 の
/// 「決めないこと」）。**したがって状態を静的に 1 つ持てる。**
/// **スレッドが来たら、この前提から作り直すこと。**
pub mod heap {
    use super::{syscall3, SYS_BRK};

    /// 取った領域の下端。**0 は「取っていない」である。**
    ///
    /// **[`reserve`] が据え、[`release`] が消す。** **[`grow_to`] は動かさない**
    /// ——**`brk` は上端しか動かさない。**
    static mut BASE: u64 = 0;

    /// いま取っている長さ（H-b-2）。**渡された領域が本物かを見るために持つ。**
    static mut LENGTH: usize = 0;

    /// `usize` のバイト数（H-b-2）。**領域を割る側が整列に使う。**
    pub const WORD: usize = core::mem::size_of::<usize>();

    /// ヒープから `bytes` バイトを取る（H-b-1）。
    ///
    /// # 返るのは 0 で埋まった領域である
    ///
    /// **カーネルが写す前にフレームを 0 で埋める**（`kernel/src/syscall.rs` の
    /// `sys_brk`。前の住人の中身をユーザーへ渡さないため）。
    /// **したがって `.bss` と同じ前提で使える。**
    ///
    /// # 取れなかったら何も残さない
    ///
    /// **`brk` は途中で足りなくなると、写せた分を残したまま `-ENOMEM` を返す**
    /// （あちらの doc の「そこまでで止める」）。**ここで元の上端へ戻す**
    /// ——**半端に伸びた状態を呼ぶ側へ渡さない。** **戻せば会計も釣り合う。**
    pub fn reserve(bytes: usize) -> Option<&'static mut [u8]> {
        if bytes == 0 {
            return None;
        }
        // SAFETY: 単一の実行文脈である（モジュールの doc）。値を写すだけで、
        // 参照は作らない。
        if unsafe { BASE } != 0 {
            return None;
        }
        // SAFETY: `brk(0)` は問い合わせで、ポインタを渡さない。
        let current = unsafe { syscall3(SYS_BRK, 0, 0, 0) };
        if current <= 0 {
            return None;
        }
        let base = current as u64;
        let wanted = base.checked_add(bytes as u64)?;
        // SAFETY: 渡すのは数だけである。
        let reached = unsafe { syscall3(SYS_BRK, wanted, 0, 0) };
        if reached < 0 || reached as u64 != wanted {
            // **半端に伸びた分を返す。** **戻り値は見ない**——**ここで
            // 戻せなかったことを言う先が無い。** **釣り合わなければ
            // `user-heap:` の行に出る**（下の [`release`] の doc）。
            // SAFETY: 渡すのは数だけである。
            unsafe { syscall3(SYS_BRK, base, 0, 0) };
            return None;
        }
        // SAFETY: 単一の実行文脈である。
        unsafe {
            BASE = base;
            LENGTH = bytes;
        };
        // SAFETY: `brk` が `base..wanted` を写した。**この範囲を渡すのは
        // ここ 1 回だけである**（上で 2 回目を断っている）ので、
        // **別名は作られない。** 領域は [`release`] まで生き続ける。
        Some(unsafe { core::slice::from_raw_parts_mut(base as *mut u8, bytes) })
    }

    /// 領域を `bytes` まで伸ばす（H-b-2）。**下端は動かない。**
    ///
    /// # 所有で守る
    ///
    /// **古い参照を値で受け取り、新しい参照を返す。** **一意の参照は `Copy`
    /// ではないので、渡した側は以後それを使えない**——**「伸ばした後に
    /// 古い長さで触らない」を、契約ではなく所有で守る形である。**
    ///
    /// # 失敗しても領域を落とさない
    ///
    /// **伸ばせなかったら、受け取った参照をそのまま `Err` で返す。**
    /// **`Option` にすると、失敗したときに領域そのものが消える**
    /// ——**呼ぶ側は返す先を失い、`release` すら呼べなくなる。**
    ///
    /// # 縮めない
    ///
    /// **`bytes` がいまの長さ以下なら `Err` である。** **縮める利用者が
    /// 1 人も居ない**——`zi` は開いている間だけ伸ばし、終わるときに全部返す。
    /// **使う者がいない機構は検算が置けない。**
    pub fn grow_to(
        region: &'static mut [u8],
        bytes: usize,
    ) -> Result<&'static mut [u8], &'static mut [u8]> {
        // SAFETY: 単一の実行文脈である（モジュールの doc）。
        let (base, length) = unsafe { (BASE, LENGTH) };
        // **渡されたものが、いま持っている領域そのものであること。**
        if base == 0 || region.as_ptr() as u64 != base || region.len() != length {
            return Err(region);
        }
        if bytes <= length {
            return Err(region);
        }
        let Some(wanted) = base.checked_add(bytes as u64) else {
            return Err(region);
        };
        // SAFETY: 渡すのは数だけである。
        let reached = unsafe { syscall3(SYS_BRK, wanted, 0, 0) };
        if reached < 0 || reached as u64 != wanted {
            // **半端に伸びた分を元へ戻す**（[`reserve`] と同じ手当て）。
            // SAFETY: 渡すのは数だけである。
            unsafe { syscall3(SYS_BRK, base + length as u64, 0, 0) };
            return Err(region);
        }
        // SAFETY: 単一の実行文脈である。
        unsafe { LENGTH = bytes };
        // **古い参照はここで終わる。** **`drop` は呼ばない**——
        // **`&mut [u8]` は `Drop` を持たないので何もせず、意図も伝わらない**
        // （rustc も「参照を drop しても何も起きない」と警告する）。
        // **終わらせているのは所有である**——**値で受け取っているので、
        // 呼ぶ側はこの時点で既に古い参照を持っていない。**
        // SAFETY: `brk` が `base..base+bytes` を写した。古い参照はこれ以降
        // 使わず、呼ぶ側も手放しているので、この範囲を指す参照は 1 本だけである。
        Ok(unsafe { core::slice::from_raw_parts_mut(base as *mut u8, bytes) })
    }

    /// 領域を返す（H-b-2）。**上端を下げ、フレームがカーネルへ戻る。**
    ///
    /// # 所有で守る
    ///
    /// **値で受け取る。** **渡した側は以後それを使えない**——
    /// **「返した領域を後から触らない」が、契約ではなく所有で保たれる。**
    /// **触れば `#PF` になる話を、呼ぶ側の注意力に預けない。**
    ///
    /// # 観測はカーネルの側にある
    ///
    /// **成否を返さない。** **返ったかどうかは `user-heap:` の行が言う**
    /// （`kernel/src/userland.rs`。`brk` が取った数と返した数を並べる）。
    /// **こちらが「返した」と主張する形は、自分で書いて自分で読む形である。**
    pub fn release(region: &'static mut [u8]) {
        // SAFETY: 単一の実行文脈である。
        let base = unsafe { BASE };
        if base == 0 || region.as_ptr() as u64 != base {
            return;
        }
        // **参照はここで終わる**（上の [`grow_to`] と同じ理由で `drop` は
        // 呼ばない）。**値で受け取っているので、下げた後に触れる道は
        // 呼ぶ側にも残っていない。**
        // SAFETY: 渡すのは数だけである。**下げる要求なので、カーネルは
        // 写像を外してフレームを返す。**
        unsafe { syscall3(SYS_BRK, base, 0, 0) };
        // SAFETY: 単一の実行文脈である。
        unsafe {
            BASE = 0;
            LENGTH = 0;
        };
    }
}

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",
    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // **入口の rsp をそのまま第 1 引数へ渡す。** ここより前で何も push していない。
    // `call` が戻り番地を 1 つ積むので、呼ばれた側の rsp は 16 の倍数 + 8 になる
    // （SysV の規約どおり）。
    "  mov rdi, rsp",
    "  call zaytos_main",
    // **ここへは戻らない。** `zaytos_main` は `-> !` で、型として戻れない。
    // **それでも `call` の直後を空けない**——`exit` が効かなかったときに
    // 詰め物を走り抜けて次に置かれたものを実行する形にしない
    // （`hello.rs` の受け皿と同じ規律）。
    "  ud2",
);

// **`.userland.receiver` を持たない。**
//
// **`hello` と `syscall-test` はあの節を持つ**——`exit` が戻ってきたときの
// 行き先を、カーネル側が `entry + USER_RECEIVER_OFFSET` として主張するためである
// （`USER_PROGRAMS` の `receiver_offset`）。
//
// **こちらは `USER_PROGRAMS` に載らない**（`spawn` で起こす）ので、
// **位置を主張する相手がいない。** そして**受け皿そのものは `exit` が持っている**
// ——`userlib::exit` はシステムコールの直後に `ud2` を置いてある。
//
// **持たないほうがよい理由もある。** あの節は `USER_LOAD_ADDR + 0x800` に
// 固定で置かれるので、**`.text` がそこを越えるプログラムでは置けない。**
// `ls` の `.text` は実測で 0xEAE である。

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // **到達しない。** `no_std` のバイナリに必須なので置く。
    // 到達したら受け皿と同じ形で落とす。
    // SAFETY: 確定的に #UD にする。
    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}
