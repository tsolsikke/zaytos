//! 画面コンソール（M3）。
//!
//! - 桁送りとセルの状態は `common::screen` にある（ES-a。ADR-0040 が
//!   **端末の状態を画面から切り離した**）。純粋ロジックで、ホストの
//!   `cargo test` で検証する。
//! - [`dirty`][mod@dirty]: 未転送範囲の追跡（純粋ロジック、ホスト
//!   `cargo test` で検証）。
//! - [`backbuffer`][mod@backbuffer]: バックバッファとフレームバッファへの
//!   転送（unsafe）。
//! - [`screen`][mod@screen]: 上記を束ねた [`Console`]。`core::fmt::Write`
//!   を実装する。
//!
//! 設計の背景は ADR-0017 を参照。要点:
//! - バックバッファは通常 RAM に置き、フレームバッファと同じ形式・同じ
//!   stride で持つ。転送を単純なコピーにするため。
//! - 転送は未転送範囲の外接矩形だけを送る。フラッシュ点は `write_fmt`
//!   1 回ごと（`write!` / `writeln!` 1 回ごと）。
//! - コンソールはグローバルに置かず、ロックも持たない。割り込みハンドラ
//!   から出力する要求は M4 で生じるため、そのロック設計は割り込み安全性の
//!   モデルと一体で決める。
//! - シリアルログはコンソールから完全に独立している。コンソールの構築に
//!   失敗してもシリアルログは影響を受けない。
//! - パニック時に画面へは出さない（ADR-0013 Addendum）。

pub mod backbuffer;
pub mod dirty;
// **画面の実物を見る観測（ES-d）。** **判定のためだけに在る**ので、
// 台本が在る構成（`zi-test`）にしか置かない。
#[cfg(any(
    feature = "zi-test",
    feature = "view-test",
    feature = "persist-check-test",
    feature = "env-rewrite-test",
    feature = "keymap-rewrite-test",
    feature = "utf8-test",
    feature = "profile-test",
    feature = "history-test"
))]
pub(crate) mod probe;
pub mod screen;
// **全画面のアプリが動く間の`fd 2`の控え（ADR-0046）。**
pub(crate) mod pending;

pub use backbuffer::{BackBuffer, BackBufferError};
pub use dirty::{DirtyRegion, Rect};
// **桁送りとセルの論理は `common::screen` へ移した（ES-a。ADR-0040）。**
// **端末の状態を画面から切り離すためで、ホストテストもあちらへ移っている。**
pub use common::screen::{
    Cell, Placement, Rgb, Screen, ScreenError, Step, MAX_GLYPH_WIDTH_CELLS, TAB_WIDTH,
};
pub use screen::{Console, ConsoleError, FlushStats};

use core::sync::atomic::{AtomicPtr, Ordering};

/// 前景の [`Console`]。**遠征の間だけ据える。**
///
/// # なぜ静的が要るのか
///
/// **`Console` は `kernel_main` の局所で、`run_init` へ `&mut` で渡る。**
/// 一方 Ring 3 の `write` を受ける [`crate::syscall`] は lib にあり、
/// **その局所へ届く道が無い**。据える形にすると届く。
///
/// # 「書き手が 1 つ」は何が保証するか
///
/// **据える側が `&mut Console` を [`ForegroundConsole`] へ預けるので、
/// 据えている間は据えた側が書けない**——借用検査がそれを見る。
/// **静的にしても型の保証は消えていない。** 消えるのは「参照が 1 つしか無い」
/// ことの保証で、**それは据える区間を借用で区切ることで戻している。**
///
/// # 他のコアは書かない
///
/// **AP は画面へ書かない**（`smp::ap_after_switch` から `ap_heartbeat_loop` へ入り、
/// シリアルへしか書かない）。**`init` は `spawn` の間ブロックしている**（同期である）。
/// **遠征は入れ子でも親が待つ**ので、同時に 2 つの `write` が走らない。
static FOREGROUND: AtomicPtr<Console> = AtomicPtr::new(core::ptr::null_mut());

/// [`FOREGROUND`] へ据えている間だけ生きるガード。
///
/// **`&mut Console` を預かる。** 落ちるときに静的を戻す。
pub struct ForegroundConsole<'a> {
    /// 据えている [`Console`]。**手放すときに溜まりを送るために持つ**
    /// （`ADR-0047`。**それまでは名前を使っていなかった**）。
    console: &'a mut Console,
}

impl Drop for ForegroundConsole<'_> {
    fn drop(&mut self) {
        // **溜まっている描画を送ってから手放す（ADR-0047）。**
        // **次のプログラムへ持ち越さない**——**手放した後は誰も送れない。**
        //
        // **この経路にも判定が置けない**（`crate::syscall` の `SYS_EXIT` と
        // 同じ理由。**次に据える者が書いて読む**）。**受け皿として置く。**
        self.console.flush();
        FOREGROUND.store(core::ptr::null_mut(), Ordering::Release);
    }
}

/// 前景経路の ANSI の状態機械（zi-b。ADR-0029）。
///
/// # なぜ静的が要るのか
///
/// **1 つの CSI 列が 2 回の `write` に割れて届くことがある**（システムコールは
/// ページ単位で刻む）。状態は `write` をまたいで保つ必要があり、
/// [`FOREGROUND`] と同じ理由で静的に置く。
///
/// # 取り方——写して返す
///
/// **[`write_foreground_bytes`] はロックを保持したまま描かない。**
/// `Locked` は保持中の割り込みを禁じるが、1 行の描画と転送は 1 ティックの
/// 半分ほど掛かる（この関数の doc）。**入るときに写しを取り、描き終えてから
/// 書き戻す。** 写しで済むのは書き手が 1 つだからである（[`FOREGROUND`] の
/// doc の保証と同じ根拠。同時に 2 つの `write` は走らない）。
static FOREGROUND_ANSI: common::critical::Locked<common::ansi::AnsiParser> =
    common::critical::Locked::new(common::ansi::AnsiParser::new());

/// 前景経路の UTF-8 の復号器（`ADR-0054` の Decision 3）。
///
/// # なぜ静的が要るのか
///
/// **[`FOREGROUND_ANSI`] と同じ理由である**——**1 字が 2 回の `write` に割れて
/// 届くことがある**（システムコールはページ単位で刻む）。**状態は `write` を
/// またいで保つ必要がある。**
///
/// # 以前は `write` ごとに閉じていた
///
/// **`core::str::from_utf8` を通し、失敗したらその `write` を丸ごと落としていた。**
/// **1 バイトの不正で画面が真っ白になる**ので、`ADR-0054` で改めた
/// ——**1 バイトにつき 1 つの置換文字にする。**
static FOREGROUND_UTF8: common::critical::Locked<common::text::Utf8Decoder> =
    common::critical::Locked::new(common::text::Utf8Decoder::new());

/// 全画面のアプリが動く間に `fd 2` へ来たエラーの控え（ADR-0046）。
///
/// # なぜ静的なのか——実測で決めた
///
/// **最初は [`Console`] の中へ置いた。** **代替画面の状態と同じ場所だからである。**
/// **`Console` は `kernel_main` のローカルなので、起動時のカーネルスタックが
/// 272 バイト深くなった**（実測）。**そして 64KiB を越え、ガードページへ
/// 15 バイト書き込んだ**（実測。ガードページの先頭から 4032 バイト目から。
/// **`stack-guard` の判定行の PTE に A/D が立っていたのが最初の兆候である**）。
///
/// **したがって `.bss` の静的へ移した。** **[`FOREGROUND_ANSI`] と同じ形である。**
///
/// # 錠は [`FOREGROUND_ANSI`] と同じ根拠で足りる
///
/// **書き手は 1 つである**（[`FOREGROUND`] の doc の保証）。**同時に 2 つの
/// `write` は走らない。** **`Locked` は保持中の割り込みを禁じる**ので、
/// **保持したまま描かない**——写しを取ってから描く（[`flush_pending_to_screen`]）。
static PENDING: common::critical::Locked<pending::Pending> =
    common::critical::Locked::new(pending::Pending::new());

/// 字の途中で捨てたバイトを報せる（`ADR-0054` の Decision 4）。
///
/// # なぜ口を直に開けるのか
///
/// **この `lib` からロガーへ届かない**（`probe::observe` と同じ事情である）。
/// **`xtask` の許可リストへ載せてある。**
///
/// # 画面へ出さない
///
/// **捨てたのは前のプログラムの都合である。** **次のプログラムの 1 字目を
/// 箱にする理由が無い。** **シリアルは最優先の観測手段である**
/// （`CLAUDE.md` の絶対ルール 4）。
fn report_dropped_mid_character(dropped: usize) {
    use core::fmt::Write as _;

    let mut serial = common::serial::SerialPort::new(common::serial::SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[INFO] console: dropped {dropped} byte(s) held mid-character when the foreground changed"
    );
}

/// 前景の [`Console`] を据える。**ガードが落ちるまで有効である。**
pub fn install_foreground(console: &mut Console) -> ForegroundConsole<'_> {
    // **ANSI の状態を最初へ戻す（zi-b）。** 前のプログラムが CSI 列の途中で
    // 死んでいても、次のプログラムの 1 字目が列の続きに化けない
    // （`release_foreground` が溜まった入力を捨てるのと同じ向きの手当て）。
    FOREGROUND_ANSI.lock().reset();
    // **字の途中のバイトも捨てる（`ADR-0054` の Decision 4）。**
    // **前のプログラムが多バイトの字の途中で死んでいても、その 1〜3 バイトが
    // 次のプログラムの 1 字目にくっつかない。**
    //
    // **捨てたことはシリアルへ出す。画面へは出さない**——**捨てたのは前の
    // プログラムの都合で、次のプログラムの画面を汚す理由が無い。**
    let dropped = FOREGROUND_UTF8.lock().reset();
    if dropped > 0 {
        report_dropped_mid_character(dropped);
    }
    // **前のプログラムが残した控えを捨てる（ADR-0046）。**
    // **次のプログラムのエコーエリアへ、前のプログラムのエラーを出さない。**
    // **記録は残る**——シリアルには既に出ている。
    PENDING.lock().clear();
    FOREGROUND.store(console as *mut Console, Ordering::Release);
    ForegroundConsole { console }
}

/// 前景の [`Console`] が据えられているか。
///
/// **呼ぶ側が BKL を解くかどうかを決めるために要る。** 据えられていないなら
/// 画面へ書くものが無いので、**解いて取り直す必要も無い。**
pub fn foreground_installed() -> bool {
    !FOREGROUND.load(Ordering::Acquire).is_null()
}

/// 前景の画面の形（e-1）。**据えられていなければ `None`。**
///
/// 返すのは `(桁, 行, 横のピクセル, 縦のピクセル)` である。
///
/// # 何のために在るのか
///
/// **`ioctl(TIOCGWINSZ)` が答える値の出所である**（`crate::syscall`）。
/// **Ring 3 には画面の形を知る手段が1つも無い**ので、訊かれたら答える側が要る。
///
/// # `None` は「大きさが無い」であって「端末でない」ではない
///
/// **据えられていないのは、その fd が端末でないからではない**——
/// **画面を持たない文脈（起動シーケンスの検算など）で走っているからである。**
/// **シリアルだけの端末に大きさが無いのと同じ立場で、呼ぶ側は 0 を受け取る。**
///
/// # 書き込みと同じ根拠で読む
///
/// **速い。** 桁と行はセルの表の形で、フレームバッファの形状も値の写しである。
/// **[`write_foreground_bytes`] と違って BKL を解く理由が無い**——
/// あちらが解くのは描画と転送が 1 ティックの半分ほど掛かるためである。
pub fn foreground_geometry() -> Option<(u32, u32, u32, u32)> {
    let console = FOREGROUND.load(Ordering::Acquire);
    if console.is_null() {
        return None;
    }
    // SAFETY: 非 null なら [`install_foreground`] のガードが生きており、
    // その間は据えた側が `&mut Console` を預けたままなので書けない
    // （[`FOREGROUND`] の doc）。**読むだけで、他のコアと他の遠征が
    // 書かないことも同じ doc に挙げてある。**
    let console = unsafe { &*console };
    let (columns, rows) = console.size();
    let layout = console.framebuffer_layout();
    Some((columns, rows, layout.width(), layout.height()))
}

/// 代替画面が有効なら、エラーを控えへ溜める（ADR-0046）。**溜めたら真。**
///
/// # 見るのと溜めるのを 1 回で済ませる
///
/// **「代替画面か」を訊いてから「溜める」を呼ぶ形にしない。**
/// **2 回に割ると、その間に切り替わりうる形に見える**——
/// **実際には書き手が 1 つなので起きないが、読む人にそれを保証させない。**
///
/// # 溜めるのは速い
///
/// **BKL を解かない。** 解くのは描画と転送が 1 ティックの半分ほど掛かる
/// ためで（[`write_foreground_bytes`] の doc）、**こちらは写すだけである。**
pub fn push_pending_if_alternate(bytes: &[u8]) -> bool {
    let console = FOREGROUND.load(Ordering::Acquire);
    if console.is_null() {
        return false;
    }
    // SAFETY: 非 null なら [`install_foreground`] のガードが生きており、
    // その間は据えた側が `&mut Console` を預けたままなので書けない
    // （[`FOREGROUND`] の doc）。他のコアと他の遠征が書かないことも同じ doc に挙げてある。
    let console = unsafe { &*console };
    if !console.alternate_screen_active() {
        return false;
    }
    PENDING.lock().push(bytes);
    true
}

/// 溜まっているエラーを `ioctl` の形へ取り出す（ADR-0046）。
///
/// **前景が据えられていなければ、長さ 0 のまま返る**
/// （[`foreground_geometry`] が 0 を返すのと同じ立場。**端末ではある**）。
pub fn take_pending(out: &mut [u8; pending::ZDIAG_LEN]) {
    PENDING.lock().take_into(out);
}

/// 控えに残っているエラーを、いまの画面へそのまま書く（ADR-0046）。
///
/// # 代替画面から戻るときに呼ぶ
///
/// **取り出されないまま終わったものを、黙って捨てない。**
/// **戻った先は通常画面なので、ここへ書くのはいままでどおりの振る舞いである。**
///
/// # 解釈しない
///
/// **`put_char` へ直に置く。** **ANSI の状態機械を通さない**——
/// **エラーの文言に CSI が混ざっていても、画面を動かす権利は無い。**
///
/// # 錠を保持したまま描かない
///
/// **写しを取ってから描く**（[`PENDING`] の doc）。
fn flush_pending_to_screen(console: &mut Console) {
    let mut text = [0u8; pending::ZDIAG_TEXT];
    let length = {
        let mut guard = PENDING.lock();
        if guard.is_empty() {
            return;
        }
        let length = guard.text().len();
        text[..length].copy_from_slice(guard.text());
        guard.clear();
        length
    };
    for byte in &text[..length] {
        console.put_char(*byte as char);
    }
    // **行を閉じる。** 続けて出るものと同じ行に並ばない。
    if text[length - 1] != b'\n' {
        console.put_char('\n');
    }
}

/// 端末への `write`（システムコール 1 回）を数える（PERF-b）。
///
/// **刻む前に 1 回だけ呼ぶこと**（`crate::syscall` の `sys_write`）。
pub fn note_terminal_write() {
    let console = FOREGROUND.load(Ordering::Acquire);
    if console.is_null() {
        return;
    }
    // SAFETY: [`push_pending_if_alternate`] と同じ根拠。
    let console = unsafe { &mut *console };
    console.note_terminal_write();
}

/// 溜まっている描画を画面へ送る（ADR-0047）。**溜まっていなければ何もしない。**
///
/// # 呼ぶ側の前提
///
/// **BKL を解いてから呼ぶこと**（[`write_foreground_bytes`] と同じ理由。
/// **全面転送は 5.05M サイクル掛かる**——実測）。
///
/// # いつ呼ぶか
///
/// **3 つである**（`ADR-0047` の決定 2）——**Ring 3 が端末から `read` したとき、
/// プロセスが終わるとき、前景を手放すとき。**
pub fn flush_foreground() {
    let console = FOREGROUND.load(Ordering::Acquire);
    if console.is_null() {
        return;
    }
    // SAFETY: 非 null なら [`install_foreground`] のガードが生きており、
    // その間は据えた側が `&mut Console` を預けたままなので書けない
    // （[`FOREGROUND`] の doc）。他のコアと他の遠征が書かないことも同じ doc にある。
    let console = unsafe { &mut *console };
    console.flush();
}

/// 前景の [`Console`] へバイト列を書く。**据えられていなければ何もしない。**
///
/// # 呼ぶ側の前提
///
/// **BKL を解いてから呼ぶこと**（`ADR-0023` の Addendum）。
/// **1 行の描画と転送は 1 ティックの半分ほど掛かる**ので、保持したまま呼ぶと
/// その間もう一方のコアがカーネルへ入れない（実測は `console:` の判定行にある）。
pub fn write_foreground_bytes(bytes: &[u8]) {
    // **破壊ビルドだけが `write_str` を使う**（既定はパーサ経由で `put_char`）。
    #[cfg(feature = "ansi-console-skip-parse-test")]
    use core::fmt::Write as _;

    let console = FOREGROUND.load(Ordering::Acquire);
    if console.is_null() {
        return;
    }
    // SAFETY: 非 null なら [`install_foreground`] のガードが生きており、
    // その間は据えた側が `&mut Console` を預けたままなので書けない（上の doc）。
    // 他のコアと他の遠征が書かないことも同じ doc に挙げてある。
    let console = unsafe { &mut *console };
    // **数える（PERF）。** **落とすバイトも数える**——**Ring 3 から見れば
    // 送った量である。**
    console.note_foreground_write(bytes.len());
    // **UTF-8 を `write` をまたいで復号する（`ADR-0054` の Decision 3）。**
    // **Ring 3 から来る列に UTF-8 を要求しない**（`load_user_program` の `argv` と
    // 同じ立場）——**復号できないバイトは 1 つにつき 1 つの置換文字にする。**
    // **丸ごと落とさない**——**1 バイトの不正で画面が真っ白になっていた。**
    {
        // 破壊 (zi-b, ansi-console-skip-parse-test): パーサを通さず素のまま描く。
        // **zi-b 前の接続そのものである**——パーサは在るのに前景経路が呼ばない。
        // CSI がグリフとして画面に出る（`[2;5H` が化けて見える）ので、
        // `ansi-test` のカーソル位置とセルの判定が落ちる。

        // **前景経路が ANSI を解釈する（zi-b。ADR-0029）。** 出す側
        // （`sys_write` の fd 1/2）の経路は不変で、解釈はここに集まる。
        // カーネルのログの経路（`Console::write_str` を直に呼ぶ側）は
        // 通らない——ログの行に CSI は無く、通す理由が無い。
        #[cfg(not(feature = "ansi-console-skip-parse-test"))]
        {
            // 破壊 (ADR-0054, console-drop-invalid-chunk-test): **不正なバイトを含む
            // `write` を丸ごと落とす。** **`ADR-0054` の前の形そのものである。**
            // **画面から 1 行が消えるので、`utf8-test` の「壊れたバイトが行を
            // 消さない」判定が落ちる。**
            #[cfg(feature = "console-drop-invalid-chunk-test")]
            if core::str::from_utf8(bytes).is_err() {
                return;
            }
            // **描く費用を測る（PERF-c の測定）。** **転送とは別の層である。**
            let started = common::cpu::read_timestamp_counter();
            // **写しを取り、描き終えてから書き戻す**（[`FOREGROUND_ANSI`] の doc）。
            let mut parser = *FOREGROUND_ANSI.lock();
            let mut decoder = *FOREGROUND_UTF8.lock();
            for byte in bytes {
                // **1 バイトから出る字は最大 4 つである**——**抱えていた 3 バイトを
                // 置換文字にしてから、来たバイトを処理する場合が最大である。**
                let mut decoded = ['\0'; 4];
                let mut count = 0usize;
                decoder.feed(*byte, &mut |c| {
                    if count < decoded.len() {
                        decoded[count] = c;
                        count += 1;
                    }
                });
                for c in &decoded[..count] {
                    let c = *c;
                    // 破壊 (zi-b, ansi-console-skip-parse-test): パーサを通さず素のまま描く。
                    // **zi-b 前の接続そのものである**——パーサは在るのに前景経路が呼ばない。
                    // CSI がグリフとして画面に出る（`[2;5H` が化けて見える）ので、
                    // `ansi-test` のカーソル位置とセルの判定が落ちる。
                    #[cfg(feature = "ansi-console-skip-parse-test")]
                    console.put_char(c);
                    #[cfg(not(feature = "ansi-console-skip-parse-test"))]
                    match parser.feed(c) {
                        None => {}
                        Some(common::ansi::AnsiAction::Print(c)) => console.put_char(c),
                        Some(common::ansi::AnsiAction::CursorTo { row, col }) => {
                            // **1 起点から 0 起点へ。** 端の切り詰めは Grid が持つ。
                            console.cursor_to_cell(col - 1, row - 1);
                        }
                        Some(common::ansi::AnsiAction::EraseDisplay(scope)) => {
                            console.erase_in_display(scope)
                        }
                        Some(common::ansi::AnsiAction::EraseLine(scope)) => {
                            console.erase_in_line(scope)
                        }
                        // **行の挿入と削除（PERF-d）。** **全画面のアプリが
                        // 1 行ぶんだけ画面をずらすために要る**——**ずらせないと、
                        // 窓が 1 行動くたびに全画面を描き直すことになる。**
                        Some(common::ansi::AnsiAction::InsertLines(count)) => {
                            console.insert_lines(count)
                        }
                        Some(common::ansi::AnsiAction::DeleteLines(count)) => {
                            console.delete_lines(count)
                        }
                        // **SGR（ES-b。ADR-0040）。** 色は受理時に RGB へ
                        // 展開されている——**ここから先は形が 1 つである。**
                        Some(common::ansi::AnsiAction::SetGraphics(graphics)) => {
                            console.set_graphics(graphics)
                        }
                        // **DECTCEM（ES-c）。** 出す / 隠すを切り替えるだけで、
                        // **描き直すのは下の `flush` の直前である。**
                        Some(common::ansi::AnsiAction::ShowCursor(show)) => {
                            console.show_cursor(show)
                        }
                        // **代替画面バッファ（e-3。ADR-0040 の Addendum）。**
                        // **戻すときに描き直すのはコンソールの側である**——
                        // Ring 3 には画面を読み戻す手段が無い。
                        Some(common::ansi::AnsiAction::AlternateScreen(alternate)) => {
                            console.set_alternate_screen(alternate);
                            // **戻ったら、取り出されずに残ったエラーを流す（ADR-0046）。**
                            if !alternate {
                                flush_pending_to_screen(console);
                            }
                        }
                    }
                }
            }
            // **ここでは転送しない（ADR-0047）。** **溜めておき、Ring 3 が
            // 入力を待った時点でまとめて送る**（[`flush_foreground`]）。
            //
            // **実測が理由である**——**`write` ごとに送っていたので、
            // Ring 3 の 1,529 バイトに対して 6MB を送っていた**（`less` の
            // 1 回の移動。ADR-0047 の表）。
            //
            // 破壊 (PERF-a, flush-every-write-test): ここで送る。**ADR-0047 の
            // 前の形そのものである**——**転送の回数と量が桁で増える。**
            #[cfg(feature = "flush-every-write-test")]
            console.flush();
            *FOREGROUND_ANSI.lock() = parser;
            *FOREGROUND_UTF8.lock() = decoder;
            let elapsed = common::cpu::read_timestamp_counter().wrapping_sub(started);
            console.note_draw_cycles(elapsed);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::graphics::font;
    use common::screen::MAX_GLYPH_WIDTH_CELLS;

    /// 格子が想定するグリフ幅の上限が、実際にフォントへ収録されている最大幅を
    /// 下回っていないことを確かめる。
    ///
    /// 下回ると、折り返しても置けないグリフが生じ、`Grid` の防御的な破棄
    /// 処理へ落ちて文字が消える。日本語（全角 2 セル）を収録した時点でも
    /// この関係が保たれていることを、ここで機械的に検出する。
    #[test]
    fn the_grid_can_hold_the_widest_glyph_in_the_font() {
        assert!(
            font::max_width_cells() <= MAX_GLYPH_WIDTH_CELLS,
            "フォントに {} セル幅のグリフがあるが、格子の想定上限は {} セル。\
             common::screen::MAX_GLYPH_WIDTH_CELLS を引き上げること",
            font::max_width_cells(),
            MAX_GLYPH_WIDTH_CELLS
        );
    }
}
