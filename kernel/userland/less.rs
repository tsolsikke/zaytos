//! `/bin/less` ——ファイルを画面ずつ見る（VIEW-b）。
//!
//! # 何を作り、何を作らないか
//!
//! **作るのは最小である**——**代替画面へ入り、上下に動き、`q` で終わる。**
//! **抜けたら元の画面へ戻る。**
//!
//! **作らないもの**——**検索（`/`）・行番号への移動・`-N`・横方向の移動・
//! 複数のファイル。** **運用者が言うまで作らない**（運用者の指示。2026-08-23）。
//!
//! **`Ctrl+F` / `Ctrl+B` は入れない。** **いま `Ctrl` の経路は Ctrl+C の旗しか
//! 持たない**（`kernel/src/input.rs` の `note_scancode_for_interrupt`）ので、
//! **押されたことが Ring 3 まで届かない。** **実測してから決めること**と
//! なっていた項目で、**測った結果「届く道が無い」ので、この段では作らない。**
//!
//! # 組み込みではなく外部である（ADR-0043 の基準へ当てた）
//!
//! **基準は「シェル自身の状態を変えるものだけが組み込み」である。**
//! **`less` はシェルの状態を1つも変えない**——作業ディレクトリも環境も
//! 触らず、自分の画面を出して終わるだけである。**したがって外部で、
//! `/bin/less` の実行ファイルにする。**
//!
//! **`zi` と同じ立場である**（あちらも全画面で、外部である）。
//! **速さと文法は基準に採らない**（ADR-0043 の決定 1）。
//!
//! # 窓の計算は `common` から借りる（ADR-0045）
//!
//! **`#[path]` で `common/src/window.rs` を取り込む。** **`zi` と同じものを
//! 2つ書かない**——**規則は同じである**（どの行から何行を見せるか）。
//!
//! **`zi` との違いは、動かすものである**——**`zi` はカーソルを動かし、窓が
//! それを追う。** **`less` はカーソルを持たず、窓そのものを動かす。**
//! **その差は `Window::scroll_down` / `scroll_up` として `common` に在る**
//! （`more` も同じものを使う）。
//!
//! # エラーの行き先（ADR-0046）
//!
//! **代替画面に居る間、`STDERR` はカーネルが溜める。** **[`userlib::Echo`] で
//! 取り出して状態行へ出す**——**`zi` がコマンド行へ出すのと同じ形である。**
//!
//! **いまのところ、代替画面に居る間に出るエラーは無い**——**開くのも読むのも
//! 入る前に済ませており、入った後は画面を描くだけである。** **それでも
//! 取り出す側を持つのは、`zi` と同じ包みを通すためである**（**仕組みを
//! 2つ書かない**）。**この形が変わったら**（読み込みを遅らせる、途中で
//! 読み足すなど）、**そこが最初の利用者になる。**

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

// **窓の計算（VIEW-a。ADR-0045）。** **`zi` と同じものを取り込む。**
#[path = "../../common/src/window.rs"]
mod window;

use userlib::{close, exit, open_read_only, read, write_all, STDERR, STDOUT};
use window::Window;

/// 打鍵を読む fd。
const STDIN: u64 = 0;

/// `-EAGAIN`。**打鍵が溜まっていない**（`zash` と `zi` と同じ形）。
const MINUS_EAGAIN: i64 = -11;

/// Esc（CSI の始まり）。
const ESC: u8 = 0x1b;

/// パスの最大長。
const PATH_MAX: usize = 128;

/// 一度に読むバイト数。
const CHUNK: usize = 1024;

/// 索引の初期の枠数（行数 + 番兵 1）。**足りなければ伸ばす。**
const INDEX_INITIAL: usize = 64;

/// 状態行の色（VIEW-b）。**`zi` の状態行（黄）とは別の色にする**——
/// **同じ画面には並ばないが、判定が色で探すときに取り違えない。**
///
/// **`ansi-test` のカーソルのシアン（0, 255, 255）と近い。** **いまは
/// どの判定もこの色を見ていないので離していないが、`less` の状態行を色で
/// 判定する日が来たら、そのとき離すこと**（運用者の指示。2026-08-24）。
const STATUS_COLOR: (u8, u8, u8) = (0, 200, 200);

const USAGE: &[u8] = b"less: usage: less <path>\n";
const OPEN_FAILED: &[u8] = b"less: cannot open\n";
const READ_FAILED: &[u8] = b"less: read failed\n";
const NO_HEAP: &[u8] = b"less: out of memory\n";

/// 読み込んだ本文と、行の索引。
///
/// # 領域は1本である
///
/// **ヒープから1本取り、前半を本文、後半を索引にする**（`zi` と同じ割り方）。
/// **`userlib::heap` は1本しか貸さない**ので、2本に分けて持てない。
///
/// # 索引はリトルエンディアンのバイト列で持つ
///
/// **`usize` の配列として読み書きしない。** **整列の前提を持たずに済み、
/// `unsafe` が要らない**（`zi` は整列を保つために切り上げている）。
struct Doc {
    region: &'static mut [u8],
    /// 本文に使う容量（索引の開始位置でもある）。
    text_cap: usize,
    /// 本文の長さ（バイト）。
    used: usize,
    /// 行数。
    count: usize,
}

/// `usize` 1つぶんのバイト数。
const WORD: usize = core::mem::size_of::<usize>();

impl Doc {
    fn index_cap(&self) -> usize {
        (self.region.len() - self.text_cap) / WORD
    }

    fn set_index(&mut self, at: usize, value: usize) {
        let base = self.text_cap + at * WORD;
        self.region[base..base + WORD].copy_from_slice(&value.to_le_bytes());
    }

    fn index(&self, at: usize) -> usize {
        let base = self.text_cap + at * WORD;
        let mut bytes = [0u8; WORD];
        bytes.copy_from_slice(&self.region[base..base + WORD]);
        usize::from_le_bytes(bytes)
    }

    /// `line` 行目の中身（改行を含まない）。
    fn line(&self, line: usize) -> &[u8] {
        if line >= self.count {
            return &[];
        }
        let start = self.index(line);
        let end = self.index(line + 1);
        // **末尾の改行を落とす。** **索引は次の行の頭を指している。**
        let end = if end > start && self.region[end - 1] == b'\n' {
            end - 1
        } else {
            end
        };
        &self.region[start..end]
    }
}

/// 領域を返して終わる。**終わるすべての道でここを通る**（`zi` と同じ形）。
fn release_and_exit(doc: Doc, status: u64) -> ! {
    userlib::heap::release(doc.region);
    exit(status)
}

/// 代替画面バッファへ入る（`?1049h`）。
fn enter_screen() {
    write_all(STDOUT, b"\x1b[?1049h");
}

/// 代替画面バッファから出る（`?1049l`）。**終わるすべての道で呼ぶ。**
fn leave_screen() {
    write_all(STDOUT, b"\x1b[?1049l");
}

/// カーソルを動かす（CUP。0 起点で受け取り、1 起点で出す）。
fn move_cursor(row: usize, col: usize) {
    let mut sequence = [0u8; 32];
    let mut at = 0usize;
    sequence[at] = ESC;
    at += 1;
    sequence[at] = b'[';
    at += 1;
    let mut digits = [0u8; 12];
    let count = write_number(&mut digits, row + 1);
    sequence[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    sequence[at] = b';';
    at += 1;
    let count = write_number(&mut digits, col + 1);
    sequence[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    sequence[at] = b'H';
    at += 1;
    write_all(STDOUT, &sequence[..at]);
}

/// 10 進の数を書く。**返るのは書いた桁数である。**
fn write_number(out: &mut [u8; 12], value: usize) -> usize {
    if value == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut digits = [0u8; 12];
    let mut count = 0usize;
    let mut rest = value;
    while rest > 0 {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
    }
    for index in 0..count {
        out[index] = digits[count - 1 - index];
    }
    count
}

/// 画面。**大きさと、本文に使える行数を持つ。**
struct View {
    rows: usize,
    columns: usize,
}

impl View {
    /// 本文に使える行数。**最下行は状態行である。**
    fn text_rows(&self) -> usize {
        self.rows.saturating_sub(1)
    }

    /// 状態行の行番号。
    fn status_row(&self) -> usize {
        self.rows.saturating_sub(1)
    }
}

/// 本文を描く。**窓の中の行だけを、画面の先頭から並べる。**
///
/// # 長い行は切る
///
/// **画面の桁数を越える行をそのまま出すと、コンソールが折り返して
/// 以降の行が1つずつ下へずれる。** **`less` は横へは動かないので、
/// 切って捨てるのが素直である**（本物の `less` も既定では切る）。
fn draw_text(view: &View, doc: &Doc, window: &Window) {
    let visible = window.visible(doc.count);
    for row in 0..view.text_rows() {
        move_cursor(row, 0);
        // EL(2): その行を消してから置く（消し残しを作らない）。
        write_all(STDOUT, b"\x1b[2K");
        let line = visible.start + row;
        if line < visible.end {
            let text = doc.line(line);
            let take = text.len().min(view.columns);
            if take > 0 {
                write_all(STDOUT, &text[..take]);
            }
        }
    }
}

/// 状態行を描く。**色を付けるのは札の部分だけである。**
///
/// **出すのは「どこを見ているか」と「終わり方」である。**
/// **報せ（[`userlib::Echo`]）が在れば、そちらを優先して出す**——
/// **エラーは、位置の表示より読まれるべきものである。**
fn draw_status(view: &View, doc: &Doc, window: &Window, echo: &userlib::Echo) {
    move_cursor(view.status_row(), 0);
    write_all(STDOUT, b"\x1b[2K");

    if !echo.line().is_empty() {
        let take = echo.line().len().min(view.columns);
        write_all(STDOUT, &echo.line()[..take]);
        return;
    }

    let visible = window.visible(doc.count);
    let mut out = [0u8; 96];
    let mut at = 0usize;
    let head = b"less: lines ";
    out[at..at + head.len()].copy_from_slice(head);
    at += head.len();
    let mut digits = [0u8; 12];
    // **1 起点で出す。** **人が読む数である**（`zi` の状態行と同じ）。
    let first = if doc.count == 0 { 0 } else { visible.start + 1 };
    let count = write_number(&mut digits, first);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at] = b'-';
    at += 1;
    let count = write_number(&mut digits, visible.end);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at..at + 4].copy_from_slice(b" of ");
    at += 4;
    let count = write_number(&mut digits, doc.count);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    let tail = b" (q to quit)";
    out[at..at + tail.len()].copy_from_slice(tail);
    at += tail.len();

    let mut color = [0u8; 24];
    let mut used = 0usize;
    let head = b"\x1b[38;2;";
    color[used..used + head.len()].copy_from_slice(head);
    used += head.len();
    for (index, value) in [STATUS_COLOR.0, STATUS_COLOR.1, STATUS_COLOR.2]
        .iter()
        .enumerate()
    {
        if index > 0 {
            color[used] = b';';
            used += 1;
        }
        let count = write_number(&mut digits, *value as usize);
        color[used..used + count].copy_from_slice(&digits[..count]);
        used += count;
    }
    color[used] = b'm';
    used += 1;
    write_all(STDOUT, &color[..used]);
    write_all(STDOUT, &out[..at.min(view.columns)]);
    // **既定の色へ戻す。** **次に描くものへ色を引きずらない。**
    write_all(STDOUT, b"\x1b[0m");
}

/// 画面を描き直す。
fn redraw(view: &View, doc: &Doc, window: &Window, echo: &userlib::Echo) {
    draw_text(view, doc, window);
    draw_status(view, doc, window, echo);
}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(stack: *const u64) -> ! {
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let Some(pointer) = (unsafe { userlib::argument(stack, 1) }) else {
        write_all(STDERR, USAGE);
        exit(2);
    };

    let mut path = [0u8; PATH_MAX];
    // SAFETY: `argv` の要素はカーネルが NUL 終端で積んだ文字列である。
    let length = unsafe { userlib::length_of(pointer, PATH_MAX - 1) };
    for index in 0..length {
        // SAFETY: 上で数えた長さの範囲である。
        path[index] = unsafe { *pointer.add(index) };
    }
    path[length] = 0;

    // **入れ物を先に取る。** **大きさは `stat` の答えから決める**
    // （`zi` と同じ形。**読む量はそれと同じである**）。
    // SAFETY: `path` は上で NUL 終端にした。
    let size = unsafe { userlib::size_of_file(&path[..length + 1]) }.unwrap_or(0) as usize;
    // **改行の補いで 1 バイト足す。** **改行で終わらないファイルでも、
    // 最後の行を索引に載せられる。**
    let text_cap = size + 1;
    let Some(region) = userlib::heap::reserve(text_cap + INDEX_INITIAL * WORD) else {
        write_all(STDERR, NO_HEAP);
        exit(6);
    };
    let mut doc = Doc {
        region,
        text_cap,
        used: 0,
        count: 0,
    };

    let fd = open_read_only(&path[..length + 1]);
    if fd < 0 {
        write_all(STDERR, OPEN_FAILED);
        release_and_exit(doc, 1);
    }
    let fd = fd as u64;

    let mut chunk = [0u8; CHUNK];
    loop {
        let got = read(fd, &mut chunk);
        if got < 0 {
            close(fd);
            write_all(STDERR, READ_FAILED);
            release_and_exit(doc, 3);
        }
        if got == 0 {
            break;
        }
        let got = got as usize;
        if doc.used + got > doc.text_cap {
            // **`stat` の答えより大きい。** **切り詰めて見せない**——
            // **見えているものが全部だと誤解させる。**
            close(fd);
            write_all(STDERR, READ_FAILED);
            release_and_exit(doc, 3);
        }
        let at = doc.used;
        doc.region[at..at + got].copy_from_slice(&chunk[..got]);
        doc.used += got;
    }
    close(fd);

    // **改行で終わっていなければ補う。** **最後の行が索引から落ちない。**
    if doc.used > 0 && doc.region[doc.used - 1] != b'\n' {
        let at = doc.used;
        doc.region[at] = b'\n';
        doc.used += 1;
    }

    // **行を数えてから索引を作る。** **枠が足りなければ伸ばす**
    // （`userlib::heap::grow_to`）。
    let lines = doc.region[..doc.used]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    if lines + 1 > doc.index_cap() {
        let wanted = doc.text_cap + (lines + 1) * WORD;
        match userlib::heap::grow_to(doc.region, wanted) {
            Ok(region) => doc.region = region,
            Err(region) => {
                doc.region = region;
                write_all(STDERR, NO_HEAP);
                release_and_exit(doc, 6);
            }
        }
    }
    let mut start = 0usize;
    let mut count = 0usize;
    for at in 0..doc.used {
        if doc.region[at] == b'\n' {
            doc.set_index(count, start);
            count += 1;
            start = at + 1;
        }
    }
    doc.set_index(count, doc.used);
    doc.count = count;

    // **画面の形を訊く（e-1）。** **0 なら既定へ落ちる。**
    let screen = userlib::window_size_or_default(0);
    let view = View {
        rows: screen.rows as usize,
        columns: screen.columns as usize,
    };
    let mut window = Window::new(view.text_rows());
    let mut echo = userlib::Echo::new();

    // **ここから代替画面である。** **戻す面が要るので、読み込みが済んで
    // 確実に見せる時点で入る**（`zi` と同じ判断）。
    enter_screen();
    redraw(&view, &doc, &window, &echo);

    let mut escape = Escape::Idle;
    loop {
        // **カーネルが溜めたエラーを取り出す（ADR-0046）。**
        if echo.take(STDERR) {
            draw_status(&view, &doc, &window, &echo);
        }

        let mut byte = [0u8; 1];
        let got = read(STDIN, &mut byte);
        if got == MINUS_EAGAIN {
            // **溜めた Esc をここで確定する（e-2 と同じ形）。**
            // **入力が途切れたので、CSI の途中ではありえない。**
            escape = Escape::Idle;
            continue;
        }
        if got <= 0 {
            // 端末が読めない。**戻してから終わる。**
            break;
        }
        let byte = byte[0];
        echo.clear();

        // **3 バイトの状態機械を先に通す**（`zash` と `zi` と同じ形）。
        match (escape, byte) {
            (Escape::Idle, ESC) => {
                escape = Escape::Esc;
                continue;
            }
            (Escape::Esc, b'[') => {
                escape = Escape::Bracket;
                continue;
            }
            (Escape::Bracket, final_byte) => {
                escape = Escape::Idle;
                let moved = match final_byte {
                    b'B' => scroll_down(&mut window, 1, doc.count),
                    b'A' => scroll_up(&mut window, 1),
                    // **知らない終端は捨てる。**
                    _ => false,
                };
                if moved {
                    redraw(&view, &doc, &window, &echo);
                }
                continue;
            }
            (Escape::Esc, _) => {
                // **Esc の次が `[` でなければ、Esc 単体として捨てる。**
                escape = Escape::Idle;
            }
            (Escape::Idle, _) => {}
        }

        let moved = match byte {
            b'j' => scroll_down(&mut window, 1, doc.count),
            b'k' => scroll_up(&mut window, 1),
            // **`Space` は1画面ぶん下へ、`b` は1画面ぶん上へ。**
            b' ' => scroll_down(&mut window, view.text_rows(), doc.count),
            b'b' => scroll_up(&mut window, view.text_rows()),
            b'q' => {
                leave_screen();
                release_and_exit(doc, 0);
            }
            _ => false,
        };
        if moved {
            redraw(&view, &doc, &window, &echo);
        } else {
            // **動かなくても状態行は描き直す**——**端に着いたことが
            // 分かるように、いまの位置を出し続ける。**
            draw_status(&view, &doc, &window, &echo);
        }
    }

    leave_screen();
    release_and_exit(doc, 0)
}

/// 窓を下へ動かす（VIEW-b）。**破壊の口はここ 1 つである。**
///
/// # なぜ包むのか
///
/// **`Window` の口を直に呼ぶ場所が 4 つある**（`j` / `Space` / 矢印の下 /
/// 矢印の上）。**破壊を 4 箇所へ書くと、片方だけ効く形が作れてしまう。**
/// **通る道を 1 本にして、そこへ置く。**
fn scroll_down(window: &mut Window, lines: usize, total: usize) -> bool {
    // 破壊 (VIEW-b, less-window-frozen): 窓を動かさない。
    // **画面は最初の 1 枚のままになる**——**打鍵は届いており、状態行も
    // 描き直されるので、雑に見ると動いていないことに気づけない。**
    // **落ちるのは「窓の外に在った行が見えるようになった」判定だけである。**
    #[cfg(less_window_frozen)]
    {
        let _ = (window, lines, total);
        return false;
    }
    #[cfg(not(less_window_frozen))]
    window.scroll_down(lines, total)
}

/// 窓を上へ動かす（VIEW-b）。**破壊の口は [`scroll_down`] と同じ理由でここである。**
fn scroll_up(window: &mut Window, lines: usize) -> bool {
    // 破壊 (VIEW-b, less-window-frozen): 窓を動かさない。
    #[cfg(less_window_frozen)]
    {
        let _ = (window, lines);
        return false;
    }
    #[cfg(not(less_window_frozen))]
    window.scroll_up(lines)
}

/// CSI の途中かどうか（`zi` と同じ形）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    Idle,
    Esc,
    Bracket,
}
