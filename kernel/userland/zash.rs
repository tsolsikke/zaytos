//! `sh`: 簡易シェル（S11-11）。
//!
//! # crate ではない
//!
//! `ls.rs` と同じで、cargo のパッケージに属さない。**Rust で書いてある**
//! （`userlib.rs` の doc）。
//!
//! # 組み込みは `exit` だけで、それ以外は `spawn` へ回す
//!
//! **外部として起こせないものが、少なくとも 1 つ要る**——分岐の形が最初から
//! 在れば、後から足すときに構造を変えずに済む（`docs/vision.md` の
//! 「コマンドの実行を組み込みと外部で分ける」）。
//!
//! **パスは解決しない。** `/bin/ls` と書いてもらう。`PATH` を持つには
//! 環境変数が要り、**`envp` をまだ開けていない。**
//!
//! # 待ち方
//!
//! **回して待つ。** `read(0)` は溜まっていなければ `-EAGAIN` を返し、
//! **カーネル側で眠らせる形はユーザープロセスのスケジューラを要求する**
//! （S11 の範囲外）。
//!
//! **`hlt` は使えない。** Ring 3 では特権命令で、呼べば畳まれる。
//! **カーネルが待つ形も採れない**——`read` の中で眠ると、**BKL を保持したまま
//! 眠ることになる**（`ADR-0023` の Addendum §2 が構造的に禁じている）。
//!
//! **したがって CPU を食う。** **スケジューラを入れたときに、待たせる形へ変える。**
//!
//! # 行の組み立て
//!
//! **`read(0)` はバイトを返す**（`kernel/src/input.rs`）。**行の区切りも編集も
//! こちらが持つ。** 見るのは改行と Backspace（`0x08`）だけである。
//!
//! # 終了状態の意味
//!
//! - `0` 組み込みの `exit` で終わった
//! - `1` 端末を読めなくなった
//!
//! **子の終了状態はここには出ない。** 0 以外なら `zash: exit status N` として
//! 表示し、**シェル自身は続ける。**

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, read, write_all, STDERR, STDOUT};

/// 1 行の最大の長さ。
///
/// # 128 で足りる
///
/// **見込みの最大は `cat /etc/motd` の 17 バイトである。**
/// **128 はその 7 倍を超える。** 越えたぶんは捨てる——**行が伸び続けて
/// スタックを踏むことのほうが害である。**
const LINE_MAX: usize = 128;

/// プロンプトの名前の部分。**色が付く側である。**
const PROMPT_NAME: &[u8] = b"zaytos";
/// プロンプトの記号と区切り。**色を付けない側である。**
///
/// # 記号は `$` のままである
///
/// **`#` にしない。** **利用者と権限の概念が無いので、あの区別は意味を
/// 持たない**（`#` は root を意味する慣行である）。
const PROMPT_SYMBOL: &[u8] = b"$ ";
/// プロンプト（名前 + 記号）。**判定行や目印が見る文字列はこの並びである。**
///
/// **色を挟んでも、シリアルの上の並びは変わらない**（`zaytos$ `）。
const PROMPT: &[u8] = b"zaytos$ ";
/// 名前の部分の色。**SGR の truecolor で前景を指定する。**
///
/// # 色は判定から選んだ
///
/// **緑 `(0, 200, 0)` である。** 画面の実物を `read_pixel_raw` で読む判定が
/// 付くので、**既に画面に居る色と紛れてはならない**
/// （`kernel/src/console/screen.rs` の `CURSOR_COLOR` の doc と同じ規律）。
/// **背景 `(0x10, 0x10, 0x18)`・既定前景 `(0xD0, 0xD8, 0xE0)`・赤
/// `(200, 0, 0)`・`zi` の状態行の黄 `(200, 200, 0)`・カーソルのシアン
/// `(0, 255, 255)` のいずれとも、RGB のどれかの軸で 150 以上離れている。**
///
/// # ES-b の判定色の緑と同じ値である
///
/// **共有している。** **150 以上離れた「別の緑」は作れない**——`(0, 200, 0)`
/// から 150 離すには `R` を上げる（黄に寄る）か `B` を上げる（シアンに寄る）
/// しかなく、**どちらも別の判定色に近づく。**
///
/// **共有してよい理由は、同じ画面に並ばないことである**（実測）。
/// ES-b の緑を描くのは `exercise_ansi_console` で、**`ansi-test` feature の
/// ときだけ在る**（`kernel/src/main.rs`）。**`zi-test` の構成には入らない**ので、
/// **プロンプトの緑と ES-b の緑が同じ画面に並ぶ構成が存在しない。**
///
/// **それでも読む側を狭めてある**——プロンプトの判定は**カーソルの居る行だけ**を
/// 見る（`kernel/src/console/probe.rs`）。**構成が将来重なっても、別の判定の
/// セルを拾わない。**
const PROMPT_COLOR: &[u8] = b"\x1b[38;2;0;200;0m";
/// 色を既定へ戻す（SGR 0）。**記号と、打った字には色を付けない。**
const SGR_RESET: &[u8] = b"\x1b[0m";
/// 起動したことを告げる 1 行。**プロンプトは改行で終わらないので、
/// 「シェルが動いた」を行として残すものが別に要る。**
// **接頭辞は固定文字列である。`argv[0]` から作らない。**
//
// **`zash` と打つか `/bin/zash` と打つかで、`argv[0]` は変わる**
// （ZaytOS は打った語をそのまま渡す。`spawn-test` がそれを検算している）。
// **接頭辞を `argv[0]` 由来にすると、同じ診断が2つの形で出る。**
// **判定行はその文字列を見ているので、打ち方で壊れる。**
const BANNER: &[u8] = b"zash: ready\n";
/// 組み込みの `exit`。
const BUILTIN_EXIT: &[u8] = b"exit";
/// 起こせなかったときの返事（前半）。
const NOT_FOUND_HEAD: &[u8] = b"zash: ";
/// 起こせなかったときの返事（後半）。
const NOT_FOUND_TAIL: &[u8] = b": cannot run\n";

/// 中断（Ctrl+C）で子が止まったときに `spawn` が返す値（S12 前の手当て、C）。
///
/// **カーネル側の `SPAWN_INTERRUPTED_FLAG` と同じ値である。**
/// **ユーザープログラムはカーネルの定数を参照できない**ので、ここに写す
/// （`SYS_*` の番号を写しているのと同じ形）。
const SPAWN_INTERRUPTED: u64 = 0x200;

/// 止められたときに出す 1 行。
const INTERRUPTED_LINE: &[u8] = b"interrupted\n";

/// Ctrl+C が届くバイト（ASCII の ETX）。
const CTRL_C: u8 = 0x03;
/// 0 以外で終わったときの返事（前半）。
const STATUS_HEAD: &[u8] = b"zash: exit status ";
/// `argv` の要素数の上限。**カーネルの `MAX_ARGV` と同じ。**
const MAX_ARGS: usize = 8;
/// `PATH` の 1 要素の最大の長さ（DIR-1。ADR-0043）。
///
/// **越える要素は飛ばす。** **`/bin` を越える見込みが無い**ので、
/// 緩衝を行の長さと同じにしない（[`NAME_MAX`] と同じ理由）。
const DIR_MAX: usize = 32;

/// `PATH` を引く名前。
const PATH_NAME: &[u8] = b"PATH";

/// `PATH` の区切り。
const PATH_SEPARATOR: u8 = b':';
/// 前置きの対象にする語の最大の長さ。
///
/// **越えたらそのまま渡す**（`/bin/` の下でその名前は探さない）。
/// **`/bin` に 32 バイトを越える名前を置く見込みが無い**ので、
/// **緩衝を行の長さと同じにしない**——`.text` にも `.bss` にも効く。
const NAME_MAX: usize = 32;

/// `/` を含まない語を、`PATH` の要素の下へ組み立てる先（DIR-1。ADR-0043）。
///
/// **`要素` + `/` + `語` + NUL** が収まる大きさである。
static mut RESOLVED: [u8; DIR_MAX + 1 + NAME_MAX + 1] = {
    let mut buffer = [0u8; DIR_MAX + 1 + NAME_MAX + 1];
    buffer
};
/// 行が長すぎたときの断り書き。
const TOO_LONG: &[u8] = b"zash: line too long\n";

/// `-EAGAIN`。**溜まっていないという意味で、失敗ではない。**
const MINUS_EAGAIN: i64 = -11;
/// Backspace のバイト。
const BACKSPACE: u8 = 0x08;

/// `Ctrl+A`（SE-b。`ADR-0050`）。**行頭へ動かす。**
///
/// **デコーダが Ctrl+英字を `0x01` から `0x1A` へ落とすようになった**ので、
/// `a` は `0x01` である。
const CTRL_A: u8 = 0x01;

/// `Ctrl+E`（SE-b。`ADR-0050`）。**行末へ動かす。** `e` は `0x05` である。
const CTRL_E: u8 = 0x05;

/// 制御バイトの上限。**これ未満は字ではない（SE-b。`ADR-0050`）。**
const FIRST_PRINTABLE: u8 = 0x20;

/// 環境の値として読む上限（SE-d）。**行より長い値は入りきらないので断る。**
const VALUE_MAX: usize = 4096;

/// 破壊 (SE-d, shell-skip-expansion-test): 展開を素通りさせる。
const SKIP_EXPANSION: bool = cfg!(zash_skip_expansion);

/// 破壊 (SE-b, shell-keep-control-bytes-test): 制御バイトを捨てない。
///
/// **`cfg!` で持つ。** **`#[cfg]` を分岐へ付けると、破壊の側で
/// [`FIRST_PRINTABLE`] が使われなくなって警告が出る。**
const KEEP_CONTROL_BYTES: bool = cfg!(zash_keep_control_bytes);

/// `\x1b[` の後に来る数（SE-b）。**Home は 1、Delete は 3、End は 4 である**
/// （`kernel/src/input.rs` の `bytes_for_event`）。
const CSI_HOME: u8 = b'1';
const CSI_DELETE: u8 = b'3';
const CSI_END: u8 = b'4';

/// `Ctrl+B`（SE-c）。**左へ 1 つ。** `b` は `0x02` である。
const CTRL_B: u8 = 0x02;

/// `Ctrl+F`（SE-c）。**右へ 1 つ。** `f` は `0x06` である。
const CTRL_F: u8 = 0x06;

/// `Ctrl+D`（SE-f）。**挿入点の字を消す。** `d` は `0x04` である。
///
/// # `bash` と違う——空行でも終わらない
///
/// **`bash` の `Ctrl+D` は、空行のときは EOF でシェルが終わる。**
/// **ZaytOS に EOF の概念が無い**——`read(0)` は溜まっていなければ `-EAGAIN` を
/// 返すだけで、「もう来ない」を表す値が無い（`ADR-0020` の面）。
/// **したがって空行では何もしない。** **黙って違う形にしない**ために、ここに書く。
///
/// **終わる道は `exit` である**（組み込み。`ADR-0043`）。
const CTRL_D: u8 = 0x04;

/// `Ctrl+K`（SE-f）。**挿入点から行末まで消す。** `k` は `0x0B` である。
const CTRL_K: u8 = 0x0B;

/// `Ctrl+L`（SE-f）。**画面を消して描き直す。** `l` は `0x0C` である。
const CTRL_L: u8 = 0x0C;

/// `Ctrl+U`（SE-f）。**行頭から挿入点まで消す。** `u` は `0x15` である。
const CTRL_U: u8 = 0x15;

/// `Ctrl+W`（SE-f）。**直前の語を消す。** `w` は `0x17` である。
const CTRL_W: u8 = 0x17;

/// 画面を消して左上へ戻す並び（SE-f）。**ED(2) と CUP である**（`ADR-0029`）。
const CLEAR_SCREEN: &[u8] = b"\x1b[2J\x1b[H";

/// `Ctrl+P`（SE-c）。**1 つ前の行。** `p` は `0x10` である。
const CTRL_P: u8 = 0x10;

/// `Ctrl+N`（SE-c）。**1 つ後の行。** `n` は `0x0E` である。
const CTRL_N: u8 = 0x0E;

/// 履歴の本数（SE-c）。
///
/// # なぜ 16 本か
///
/// **`bash` の既定は 500 だが、あちらはファイルへ保存する。** **こちらは
/// 再起動で消える**ので、**1 つのセッションで辿る範囲だけあればよい。**
///
/// **1 ページに収まる本数でもある**（下の [`HISTORY`] の doc）。
const HISTORY_MAX: usize = 16;

/// 履歴の輪（SE-c）。**古いものから捨てる。**
///
/// # なぜヒープを使わないのか
///
/// **上限はどちらにせよ要る。** ヒープにしても無制限には伸ばせない
/// （`brk` が像を食う）。**上限があるなら、固定で足りる。**
///
/// **費用が静的に測れる。** **`16 * 128 + 16 * 8 = 2176` バイトが `.bss` に出る。**
/// **`zash` の書き込み可の区画は `0x403040` から始まり、`.bss` は 81 バイトだった**
/// （実測。2026-08-28）。**足しても `0x403911` で、ページの終わり `0x404000` を
/// 越えない**——**写像は 1 ページも増えない。**
///
/// **`zash` はヒープを 1 度も使っていない**（実測。`brk` は 0 箇所）。
/// **使い始めると `brk` の会計が 1 つ増える**（`zi` と `syscall-test` には
/// その判定が在る）。**履歴の大きさは行の長さと本数で決まっており、
/// 動的である必要が無い。** **会計を増やす値打ちが無い。**
static mut HISTORY: [[u8; LINE_MAX]; HISTORY_MAX] = [[0; LINE_MAX]; HISTORY_MAX];

/// 各行の長さ。
static mut HISTORY_LEN: [usize; HISTORY_MAX] = [0; HISTORY_MAX];

/// 積んだ本数。**増え続ける。輪の位置は剰余で出す。**
static mut HISTORY_COUNT: usize = 0;

/// 破壊 (SE-f, shell-shift-delete-range-test): 消す範囲を 1 つ狭める。
///
/// # なぜこの破壊が要るのか
///
/// **`keyboard-drop-ctrl-letters-test` が覆っているのは「鍵が届くこと」であって、
/// 「範囲の計算が正しいこと」ではない。** **`Ctrl+K` が誤って行頭まで消す形は、
/// 鍵が届いているので、あの破壊では捕まらない。**
///
/// **範囲を 1 つずらせば、消す鍵の判定が同時に落ちる**——**範囲の計算を
/// 守っているのがそれらの判定であることを、この破壊が主張する。**
const SHIFT_DELETE_RANGE: bool = cfg!(zash_shift_delete_range);

/// 破壊 (SE-c, shell-drop-history-test): 行を履歴へ積まない。
const DROP_HISTORY: bool = cfg!(zash_drop_history);

/// 空白を並べた種。**消すときにまとめて 1 回で書くために持つ。**
const SPACES: [u8; LINE_MAX] = [b' '; LINE_MAX];

/// 後退を並べた種。**まとめて 1 回で書くために持つ。**
///
/// **1 バイトずつ書くと、行頭へ戻るだけで最大 128 回の `write` になる**
/// ——PERF 段が減らした側である。
const BACKSPACES: [u8; LINE_MAX] = [BACKSPACE; LINE_MAX];

/// プロンプトを出す（ES-d）。**色を付けてから戻す。**
///
/// **3 回書くのではなく 1 回で書く。** `write` は 1 回ごとに画面へ届いて
/// フラッシュされるので、**分けて書くと色の無いプロンプトが一瞬出る。**
/// **判定は入力待ちの時点で見るので、そこでは差が出ない**——それでも、
/// **人が見る側で点滅させる理由が無い。**
/// `TERM` がこの値なら色を付ける（EV。ADR-0041）。
const TERM_WITH_COLOR: &[u8] = b"zaytos";

/// `TERM` を引く名前。
const TERM_NAME: &[u8] = b"TERM";

/// プロンプトに色を付けるか（EV）。**`zaytos_main` が起動時に決める。**
///
/// # なぜ既定を「付けない」にするのか
///
/// **`TERM` が読めなかったときに、読めたときと同じ見た目になってはいけない。**
/// **同じにすると、環境が届いたかどうかを画面から区別できない**
/// ——**判定が何も主張していないのと同じである**（ADR-0041 の到達条件）。
///
/// **端末の種類が分からないなら、装飾しないのが安全側でもある。**
static COLOR_PROMPT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// `PATH` の値（DIR-1。ADR-0043）。**`zaytos_main` が起動時に控える。**
///
/// # 無ければ名前だけの語は起こせない
///
/// **既定の探索先を持たない。** **持つと、`PATH` が届かなかったときに
/// 届いたときと同じ振る舞いになり、機構が観測できなくなる**
/// （`COLOR_PROMPT` と同じ形である）。
///
/// **POSIX は `PATH` が未設定のときの振る舞いを実装定義としている。**
/// **こちらは「探せない」を選ぶ。**
static mut PATH_VALUE: *const u8 = core::ptr::null();

/// `PATH` を控える（DIR-1。ADR-0043）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
unsafe fn remember_path(stack: *const u64) {
    // SAFETY: 呼び出し元契約をそのまま渡す。
    if let Some(value) = unsafe { userlib::environment(stack, PATH_NAME) } {
        // SAFETY: このプログラムは Ring 3 で 1 本だけ走る。**書くのはここだけである。**
        unsafe { PATH_VALUE = value };
    }
}

/// `PATH` の要素の下で語を起こす（DIR-1。ADR-0043）。
///
/// # 探索の規則は 2 つだけである
///
/// **左から順に試し、最初に起こせたものを採る。** **`-ENOENT` のときだけ
/// 次の要素へ進む**——**それ以外の失敗は、その要素で決まったことである**
/// （ディレクトリだった、許されなかった）。**探し続けると、最初の要素で
/// 起きた本当の理由が、最後の要素の `-ENOENT` に置き換わる。**
///
/// # 長すぎる要素は飛ばす
///
/// **[`DIR_MAX`] を越える要素は組み立てられない。** **黙って飛ばす**
/// ——**`PATH` はカーネルの定数なので、越える要素が来ることは今は無い**
/// （ADR-0043 の決定 4）。
///
/// # 見つからなければ `-ENOENT` を返す
///
/// **`PATH` が無い場合も同じである。** 呼ぶ側は区別しない
/// ——**どちらも「その名前では起こせない」である。**
///
/// # Safety
///
/// `argv` が NULL 終端のポインタ配列であること。
unsafe fn spawn_via_path(command: &[u8], argv: &[*const u8]) -> i64 {
    // SAFETY: 書き手はこのプログラムだけで、起動時に一度だけ書く。
    let path = unsafe { PATH_VALUE };
    if path.is_null() {
        return userlib::MINUS_ENOENT;
    }

    let mut at = 0usize;
    loop {
        // **要素を 1 つ取る。** 終端か区切りまで進む。
        let start = at;
        let mut length = 0usize;
        loop {
            // SAFETY: `PATH` はカーネルが NUL 終端で積んだ文字列である。
            let byte = unsafe { *path.add(start + length) };
            if byte == 0 || byte == PATH_SEPARATOR {
                break;
            }
            length += 1;
        }
        // SAFETY: 上で数えた位置である。
        let ended = unsafe { *path.add(start + length) } == 0;

        if length > 0 && length <= DIR_MAX && command.len() <= NAME_MAX {
            // SAFETY: このプログラムは Ring 3 で 1 本だけ走る。
            // **`RESOLVED` へ触るのはこの経路だけである。**
            let resolved = unsafe { &mut *core::ptr::addr_of_mut!(RESOLVED) };
            for index in 0..length {
                // SAFETY: 要素の範囲である。
                resolved[index] = unsafe { *path.add(start + index) };
            }
            let mut end = length;
            // **要素が `/` で終わっていたら重ねない。**
            if resolved[end - 1] != b'/' {
                resolved[end] = b'/';
                end += 1;
            }
            resolved[end..end + command.len()].copy_from_slice(command);
            end += command.len();
            // **終端を置く。** 前の候補のほうが長かった場合に、残りが続きとして
            // 読まれない。
            resolved[end] = 0;

            // SAFETY: 組み立てた先は NUL 終端で、`argv` は呼び出し元の契約による。
            let status = unsafe { userlib::spawn(&resolved[..end], argv) };
            if status != userlib::MINUS_ENOENT {
                return status;
            }
        }

        if ended {
            return userlib::MINUS_ENOENT;
        }
        at = start + length + 1;
    }
}

/// `TERM` を読んで、色を付けるかを決める（EV。ADR-0041）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
unsafe fn decide_prompt_color(stack: *const u64) {
    // SAFETY: 呼び出し元契約をそのまま渡す。
    let Some(value) = (unsafe { userlib::environment(stack, TERM_NAME) }) else {
        return;
    };
    // **突き合わせは NUL まで見る。** 前方一致で決めない——`zaytos2` を
    // `zaytos` として扱わない。
    let mut index = 0usize;
    loop {
        // SAFETY: 値はカーネルが NUL 終端で積んだ文字列である。
        let byte = unsafe { *value.add(index) };
        if index == TERM_WITH_COLOR.len() {
            if byte == 0 {
                COLOR_PROMPT.store(true, core::sync::atomic::Ordering::SeqCst);
            }
            return;
        }
        if byte != TERM_WITH_COLOR[index] {
            return;
        }
        index += 1;
    }
}

fn write_prompt() {
    // 破壊 (ES-d, zash-prompt-drop-color): 色を送らずにプロンプトを出す。
    // **プロンプトの字も位置も変わらない**ので、既存の判定はどれも動かない。
    // **画面のセルが既定前景のままになる**ので、zi-test の
    // 「プロンプトが自分の色で描かれている」判定だけが落ちる。
    #[cfg(zash_prompt_drop_color)]
    let colored = false;
    // **`TERM` が決める（EV。ADR-0041）。** **環境が届かなければ色を付けない**
    // ——**届いたかどうかが画面から見える形にしてある**（[`COLOR_PROMPT`]）。
    #[cfg(not(zash_prompt_drop_color))]
    let colored = COLOR_PROMPT.load(core::sync::atomic::Ordering::SeqCst);

    let mut out = [0u8; PROMPT_COLOR.len() + PROMPT.len() + SGR_RESET.len()];
    let mut at = 0usize;
    // **記号には SGR を掛けない。** **戻してから出す**ので、記号のセルは
    // 既定前景そのものになる——**色が記号へ漏れていないことを、判定が
    // 「連なりの直後のセルが既定色であること」で見る。**
    let mut put = |part: &[u8], at: &mut usize| {
        out[*at..*at + part.len()].copy_from_slice(part);
        *at += part.len();
    };
    if colored {
        put(PROMPT_COLOR, &mut at);
        put(PROMPT_NAME, &mut at);
        put(SGR_RESET, &mut at);
    } else {
        put(PROMPT_NAME, &mut at);
    }
    put(PROMPT_SYMBOL, &mut at);
    write_all(STDOUT, &out[..at]);
}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(stack: *const u64) -> ! {
    // **プロンプトを出す前に決める（EV。ADR-0041）。**
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    unsafe { decide_prompt_color(stack) };
    // **`PATH` を控える（DIR-1。ADR-0043）。**
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    unsafe { remember_path(stack) };
    write_all(STDOUT, BANNER);
    write_prompt();

    let mut line = [0u8; LINE_MAX];
    let mut length = 0usize;
    // **挿入点（S12 前の手当て）。** 次に字を入れる位置で、**常に `length` 以下**である。
    //
    // **いまは常に行末（`length` と等しい）である**——動かす手段がまだ無い。
    // **それでも概念として先に置く。** 矢印を足す段で増えるのは
    // 「動かす手段」だけになり、Backspace と挿入の側を書き直さずに済む。
    let mut cursor = 0usize;
    let mut overflowed = false;
    // **エスケープの受け（S12 前の手当て）。** 矢印は 3 バイトで届く
    // （`\x1b` `[` に `D`/`C`/`A`/`B`。`kernel/src/input.rs` が落とす形。
    // 上下は zi-a で増えた——このシェルでは読んで捨てる）。
    //
    // **解釈はここで行う。画面（`Grid`）には届かない。**
    // `ADR-0029` が決めたのは出力側の解釈で、こちらは入力側である。
    let mut escape = Escape::Idle;
    // **履歴を辿っている深さ（SE-c）。** `0` は「打ちかけの行」である。
    let mut history_back = 0usize;
    // **辿り始めたときの打ちかけの行。** `Ctrl+N` で `0` まで戻ると復す。
    let mut saved = [0u8; LINE_MAX];
    let mut saved_length = 0usize;

    loop {
        let mut byte = [0u8; 1];
        let got = read(0, &mut byte);
        if got == MINUS_EAGAIN {
            // **溜まっていない。** 回して待つ。
            continue;
        }
        if got <= 0 {
            // **端末が読めない。** 失敗として終わる。
            exit(1);
        }

        // **3 バイトの状態機械を先に通す（S12 前の手当て）。**
        //
        // **未完のまま別の字が来たら、そこで捨てて普通の字として扱う。**
        // 溜めた `\x1b` や `[` は**行へ入れない**——打っていない字を
        // 行へ混ぜると、走る語が変わる。
        match (escape, byte[0]) {
            (Escape::Idle, 0x1b) => {
                escape = Escape::Esc;
                continue;
            }
            (Escape::Esc, b'[') => {
                escape = Escape::Bracket;
                continue;
            }
            (Escape::Bracket, b'D') => {
                escape = Escape::Idle;
                // **左へ 1 つ。** **`Ctrl+B` と同じ関数を通す（SE-c）。**
                move_left(&mut cursor);
                continue;
            }
            (Escape::Bracket, b'C') => {
                escape = Escape::Idle;
                // **右へ 1 つ。** **`Ctrl+F` と同じ関数を通す（SE-c）。**
                move_right(&line, &mut cursor, length);
                continue;
            }
            (Escape::Bracket, digit) if digit.is_ascii_digit() => {
                // **`\x1b[` に続く数（SE-b）。** 次の `~` で確定する。
                escape = Escape::Number(digit);
                continue;
            }
            (Escape::Number(digit), b'~') => {
                escape = Escape::Idle;
                match digit {
                    // **Home は行頭へ（`ADR-0050`）。**
                    CSI_HOME => move_to_start(&mut cursor),
                    // **End は行末へ。**
                    CSI_END => move_to_end(&line, &mut cursor, length),
                    // **Delete は挿入点の字を消す。** 挿入点は動かない。
                    // **行末では何もしない**——消す字が無い。
                    CSI_DELETE => {
                        let at = cursor;
                        delete_range(&mut line, &mut length, &mut cursor, at, at + 1);
                    }
                    // **知らない数は捨てる。** **最後のバイトを字として
                    // 行へ入れない**——上下矢印と同じ判断である。
                    _ => {}
                }
                continue;
            }
            (Escape::Bracket, b'A') => {
                escape = Escape::Idle;
                // **1 つ前の行（SE-c）。** **`Ctrl+P` と同じ関数を通す。**
                //
                // **以前は読んで捨てていた**（zi-a。履歴が無かった）。
                // SAFETY: このプログラムは Ring 3 で 1 本だけ走る。
                unsafe {
                    history_previous(
                        &mut line,
                        &mut length,
                        &mut cursor,
                        &mut history_back,
                        &mut saved,
                        &mut saved_length,
                    );
                }
                continue;
            }
            (Escape::Bracket, b'B') => {
                escape = Escape::Idle;
                // **1 つ後の行（SE-c）。** **`Ctrl+N` と同じ関数を通す。**
                // SAFETY: 同上。
                unsafe {
                    history_next(
                        &mut line,
                        &mut length,
                        &mut cursor,
                        &mut history_back,
                        &saved,
                        saved_length,
                    );
                }
                continue;
            }
            (Escape::Esc, _) | (Escape::Bracket, _) | (Escape::Number(_), _) => {
                // **知らない並びだった。** 溜めた分は捨て、いま来た字は
                // 下の分岐で普通に扱う。
                //
                // **`zi` と同じ形だが、直していない（e-2）。**
                // **あちらは `-EAGAIN` を受けた時点で Esc 単体を確定する**
                // ——モードが在るので、確定が遅れると使う人に見える
                // （Esc を 2 回押すことになった）。
                // **`zash` に Esc 単体の意味は無い**ので、**確定が次の
                // 1 バイトまで遅れても、外から見て何も変わらない**
                // （どちらの順でも、溜めた分を捨てて次の字を普通に扱う）。
                // **見える違いが無いものを直すと、直したことを主張する
                // 判定が置けない。** **モードや意味を持たせる段で直すこと。**
                escape = Escape::Idle;
            }
            (Escape::Idle, _) => {}
        }

        match byte[0] {
            b'\n' => {
                // **打った改行を反響する。** 反響はシェルが行う——
                // **カーネルは前景を渡しているだけで、何も表示しない。**
                write_all(STDOUT, b"\n");
                // **打った行を履歴へ積む（SE-c）。** **展開の前の、打った形で積む**
                // ——**辿って出てくるのは打った行である**（`$PATH` は `$PATH` のまま）。
                // SAFETY: このプログラムは Ring 3 で 1 本だけ走る。
                unsafe { remember_line(&line[..length]) };
                history_back = 0;
                saved_length = 0;
                if overflowed {
                    write_all(STDOUT, TOO_LONG);
                } else if length > 0 && SKIP_EXPANSION {
                    // 破壊 (SE-d, shell-skip-expansion-test): 展開を通さない。
                    //
                    // **終端を置いてから渡す。** 語の末尾は `line` の中の NUL で
                    // 決まるので、**前の行の残りが続きとして読まれない**ように
                    // ここで 1 バイト置く（`LINE_MAX` は 1 行より大きいので在る）。
                    line[length] = 0;
                    run_line(&mut line[..length]);
                } else if length > 0 {
                    // **`$NAME` を展開してから語へ切る（SE-d。`ADR-0049`）。**
                    //
                    // **写しへ展開する。** **展開は長さを変えるので、その場で
                    // 伸ばすと終端の置き場が壊れる。**
                    // **終端の 1 バイトを別に持つ**（`line` と同じ形。上の SE-d の注記）。
                    let mut expanded = [0u8; LINE_MAX + 1];
                    // SAFETY: `stack` は `zaytos_main` が受けた初期スタックである。
                    match unsafe { expand_line(stack, &line[..length], &mut expanded[..LINE_MAX]) }
                    {
                        // **語が 1 つも残らなかった。** 走らせるものが無い。
                        Some(0) => {}
                        Some(count) => {
                            expanded[count] = 0;
                            run_line(&mut expanded[..count]);
                        }
                        // **入りきらなかった。** **部分的に展開した行を走らせない**
                        // （`ADR-0049` の 6）。文言は打ちすぎと同じものを使う。
                        None => {
                            write_all(STDOUT, TOO_LONG);
                        }
                    }
                }
                length = 0;
                cursor = 0;
                overflowed = false;
                // **未完のエスケープは行をまたがない。** 溜めた状態を捨てる
                // （ここへ来る時点で `Idle` のはずだが、状態を持ち越さない）。
                escape = Escape::Idle;
                write_prompt();
            }
            CTRL_C => {
                // **打ちかけの行を捨てる（S12 前の手当て、C）。**
                //
                // **子が走っていないときの Ctrl+C はここへ来る。**
                // **走っているときはこのバイトが届かない**——カーネルが
                // 子の遠征を畳んでおり、シェルは `spawn` の中で待っている。
                //
                // **`^C` を出してから改行する。** 出さないと、捨てられた行が
                // 画面に残ったまま次のプロンプトが出て、**何が起きたのか
                // 打った人に見えない。**
                write_all(STDOUT, b"^C\n");
                length = 0;
                cursor = 0;
                history_back = 0;
                saved_length = 0;
                overflowed = false;
                escape = Escape::Idle;
                write_prompt();
            }
            CTRL_B => {
                // **左へ 1 つ（SE-c）。** **左矢印と同じ関数を通す。**
                move_left(&mut cursor);
            }
            CTRL_F => {
                // **右へ 1 つ（SE-c）。** **右矢印と同じ関数を通す。**
                move_right(&line, &mut cursor, length);
            }
            CTRL_P => {
                // **1 つ前の行（SE-c）。** **上矢印と同じ関数を通す。**
                // SAFETY: このプログラムは Ring 3 で 1 本だけ走る。
                unsafe {
                    history_previous(
                        &mut line,
                        &mut length,
                        &mut cursor,
                        &mut history_back,
                        &mut saved,
                        &mut saved_length,
                    );
                }
            }
            CTRL_N => {
                // **1 つ後の行（SE-c）。** **下矢印と同じ関数を通す。**
                // SAFETY: 同上。
                unsafe {
                    history_next(
                        &mut line,
                        &mut length,
                        &mut cursor,
                        &mut history_back,
                        &saved,
                        saved_length,
                    );
                }
            }
            CTRL_A => {
                // **行頭へ（SE-b。`ADR-0050`）。** Home と同じ動きである——
                // **同じ関数を通すので、2 つの経路で振る舞いがずれない。**
                move_to_start(&mut cursor);
            }
            CTRL_E => {
                // **行末へ（SE-b。`ADR-0050`）。** End と同じ動きである。
                move_to_end(&line, &mut cursor, length);
            }
            BACKSPACE => {
                // **挿入点の直前を消す（S12 前の手当て。SE-f で `delete_range` へ寄せた）。**
                // **行頭では何もしない**——範囲が空になるので、あちらが返る。
                let at = cursor;
                delete_range(&mut line, &mut length, &mut cursor, at.saturating_sub(1), at);
            }
            CTRL_D => {
                // **挿入点の字を消す（SE-f）。** **Delete と同じ範囲である。**
                //
                // **空行では何もしない**——`bash` はそこで EOF になるが、
                // **ZaytOS に EOF の概念が無い**（[`CTRL_D`] の doc）。
                let at = cursor;
                delete_range(&mut line, &mut length, &mut cursor, at, at + 1);
            }
            CTRL_K => {
                // **挿入点から行末まで消す（SE-f）。**
                let (at, end) = (cursor, length);
                delete_range(&mut line, &mut length, &mut cursor, at, end);
            }
            CTRL_U => {
                // **行頭から挿入点まで消す（SE-f）。**
                let at = cursor;
                delete_range(&mut line, &mut length, &mut cursor, 0, at);
            }
            CTRL_W => {
                // **直前の語を消す（SE-f）。** 切れ目は空白だけである
                // （[`word_start`] の doc）。
                let at = cursor;
                let start = word_start(&line, at);
                delete_range(&mut line, &mut length, &mut cursor, start, at);
            }
            CTRL_L => {
                // **画面を消して描き直す（SE-f）。** 行は消さない。
                redraw_screen(&line, length, cursor);
            }
            other => {
                // **知らない制御バイトは行へ入れない（SE-b。`ADR-0050`）。**
                //
                // **入れると、幅を持たない字が語に混ざる。** **実測で、Tab を
                // 打つと `f` `0x09` `g` という語になり、`cannot run` が出ていた**
                // ——**打った人には、なぜ動かないのかが読めない。**
                //
                // **扱うと決めたものは上の分岐で受けてある**（改行・Ctrl+C・
                // Ctrl+A・Ctrl+E・Backspace）。**ここへ来る `0x20` 未満は、
                // 受け手が決まっていないバイトである。**
                //
                // **捨てたことは画面に出ない。** **Tab を押しても何も起きない**
                // ——**補完は作らないと決めてある**（`ADR-0050` の「決めないこと」。
                // 持ち越しに行がある）。
                if other < FIRST_PRINTABLE && !KEEP_CONTROL_BYTES {
                    continue;
                }
                // **終端の 1 バイトを残す（SE-d で直した）。**
                //
                // **以前は `length < line.len()` だった。** **`LINE_MAX` ちょうどまで
                // 入るので、`length` が `LINE_MAX` になりうる**——**そのまま Enter を
                // 打つと、下の `line[length] = 0` が配列の外を書いて畳まれる。**
                // **`overflowed` は次の 1 打まで立たないので、間に合わない。**
                //
                // **すぐ上の doc が「`LINE_MAX` は 1 行より大きいので在る」と
                // 書いており、その前提が守られていなかった。**
                // **コードから読んで見つけた。打って確かめてはいない。**
                if length + 1 < line.len() {
                    // **挿入点へ入れて、後ろをずらす。**
                    line.copy_within(cursor..length, cursor + 1);
                    line[cursor] = other;
                    cursor += 1;
                    length += 1;
                    write_all(STDOUT, &byte);
                    // **挿入点より後ろは書き直す。** 末尾の空白は要らない
                    // （消えた分が無いため）。
                    redraw_tail_without_gap(&line[cursor..length]);
                } else {
                    // **越えたぶんは捨てる。** 反響もしない——
                    // **入っていないものを入ったように見せない。**
                    overflowed = true;
                }
            }
        }
    }
}

/// 行を履歴へ積む（SE-c）。
///
/// **直前と同じ行は積まない。空行も積まない。**
/// **辿るときに同じ行が並ぶと、辿る回数が増えるだけで情報が増えない。**
///
/// # Safety
///
/// **このプログラムは Ring 3 で 1 本だけ走る。** 履歴を書くのはここだけである。
unsafe fn remember_line(line: &[u8]) {
    if line.is_empty() || DROP_HISTORY {
        return;
    }
    // SAFETY: 上記のとおり、単一の実行者である。
    let count = unsafe { HISTORY_COUNT };
    if count > 0 {
        let last = (count - 1) % HISTORY_MAX;
        // SAFETY: 同上。`last` は剰余なので範囲内である。
        let length = unsafe { HISTORY_LEN[last] };
        // SAFETY: 同上。**参照で取る**——値で取ると `[u8]` を動かすことになる。
        let stored = unsafe { &*core::ptr::addr_of!(HISTORY[last]) };
        if length == line.len() && &stored[..length] == line {
            return;
        }
    }
    let slot = count % HISTORY_MAX;
    // SAFETY: 同上。`line` は `LINE_MAX` 未満である（呼び出し側が行から渡す）。
    unsafe {
        HISTORY[slot][..line.len()].copy_from_slice(line);
        HISTORY_LEN[slot] = line.len();
        HISTORY_COUNT = count + 1;
    }
}

/// 履歴を辿る（SE-c）。`back` は「いくつ前か」で、`1` が直前である。
///
/// **持っている本数を越えたら `None`。**
///
/// # Safety
///
/// [`remember_line`] と同じ。
unsafe fn history_at(back: usize) -> Option<(&'static [u8; LINE_MAX], usize)> {
    // SAFETY: 単一の実行者である。
    let count = unsafe { HISTORY_COUNT };
    if back == 0 || back > count || back > HISTORY_MAX {
        return None;
    }
    let slot = (count - back) % HISTORY_MAX;
    // SAFETY: 同上。`slot` は剰余なので範囲内である。
    unsafe { Some((&*core::ptr::addr_of!(HISTORY[slot]), HISTORY_LEN[slot])) }
}

/// 画面の行を差し替える（SE-c）。**挿入点は行末へ置く。**
///
/// **消してから書く。** 後退・空白・後退をそれぞれ 1 回で出す
/// （1 バイトずつだと 1 行につき最大 384 回の `write` になる）。
fn replace_line(
    line: &mut [u8; LINE_MAX],
    length: &mut usize,
    cursor: &mut usize,
    new: &[u8],
) {
    move_to_end(line, cursor, *length);
    if *length > 0 {
        write_all(STDOUT, &BACKSPACES[..*length]);
        write_all(STDOUT, &SPACES[..*length]);
        write_all(STDOUT, &BACKSPACES[..*length]);
    }
    line[..new.len()].copy_from_slice(new);
    *length = new.len();
    *cursor = new.len();
    if *length > 0 {
        write_all(STDOUT, &line[..*length]);
    }
}

/// 1 つ前の行を出す（SE-c）。**上キーと `Ctrl+P` が同じここを通る。**
///
/// **辿り始めるときに、打ちかけの行を控える。** **`Ctrl+N` で戻ったときに
/// 打ちかけの行が消えていると、辿ったことが編集の取り消しになる。**
///
/// # Safety
///
/// [`history_at`] と同じ。
unsafe fn history_previous(
    line: &mut [u8; LINE_MAX],
    length: &mut usize,
    cursor: &mut usize,
    back: &mut usize,
    saved: &mut [u8; LINE_MAX],
    saved_length: &mut usize,
) {
    // SAFETY: 呼び出し元契約をそのまま渡す。
    let Some((entry, entry_length)) = (unsafe { history_at(*back + 1) }) else {
        return;
    };
    if *back == 0 {
        saved[..*length].copy_from_slice(&line[..*length]);
        *saved_length = *length;
    }
    *back += 1;
    replace_line(line, length, cursor, &entry[..entry_length]);
}

/// 1 つ後の行を出す（SE-c）。**下キーと `Ctrl+N` が同じここを通る。**
///
/// **`0` まで戻ったら、控えてあった打ちかけの行へ戻す。**
///
/// # Safety
///
/// [`history_at`] と同じ。
unsafe fn history_next(
    line: &mut [u8; LINE_MAX],
    length: &mut usize,
    cursor: &mut usize,
    back: &mut usize,
    saved: &[u8; LINE_MAX],
    saved_length: usize,
) {
    if *back == 0 {
        return;
    }
    *back -= 1;
    if *back == 0 {
        replace_line(line, length, cursor, &saved[..saved_length]);
        return;
    }
    // SAFETY: 呼び出し元契約をそのまま渡す。
    if let Some((entry, entry_length)) = unsafe { history_at(*back) } {
        replace_line(line, length, cursor, &entry[..entry_length]);
    }
}

/// 名前の先頭になれる字か（SE-d。`ADR-0049`）。
fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

/// 名前の続きになれる字か（SE-d）。
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// 語を 1 つ展開する（SE-d。`ADR-0049`）。**書けた長さを返す。入らなければ `None`。**
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
unsafe fn expand_word(stack: *const u64, word: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut used = 0usize;
    let mut at = 0usize;
    // 溜める側の受け。
    let mut put = |byte: u8, used: &mut usize| -> bool {
        if *used >= out.len() {
            return false;
        }
        out[*used] = byte;
        *used += 1;
        true
    };
    // **`~` を先に展開する（f-1。`ADR-0049` の Addendum の 4）。**
    //
    // **語の先頭に在るときだけである**（規則 1）。**`~` 単独と `~/` だけ**
    // （規則 2。`~user` は作らない）。**`HOME` が未定義なら展開しない**
    // （規則 3。**空へ落とすと `~/foo` が `/foo` になり、意味の違うパスになる**）。
    //
    // **`$NAME` より先に置くのは、`HOME` の値へ直接展開するためである**
    // （規則 4）。**`$HOME` を経由すると 2 度展開になり、値に `$` が
    // 含まれるときに差が出る。**
    if word.first() == Some(&b'~') && (word.len() == 1 || word[1] == b'/') {
        // SAFETY: 呼び出し元契約をそのまま渡す。
        if let Some(home) = (unsafe { userlib::environment(stack, b"HOME") }) {
            // SAFETY: カーネルが NUL 終端で積んだ文字列である。
            let length = unsafe { userlib::length_of(home, VALUE_MAX) };
            for index in 0..length {
                // SAFETY: 上で数えた長さの範囲である。
                if !put(unsafe { *home.add(index) }, &mut used) {
                    return None;
                }
            }
            at = 1;
        }
    }

    while at < word.len() {
        let byte = word[at];
        // **`$` の直後が名前の先頭でなければ、`$` は字である**（`ADR-0049` の 5）。
        if byte != b'$' || at + 1 >= word.len() || !is_name_start(word[at + 1]) {
            if !put(byte, &mut used) {
                return None;
            }
            at += 1;
            continue;
        }
        let start = at + 1;
        let mut end = start;
        while end < word.len() && is_name_byte(word[end]) {
            end += 1;
        }
        at = end;
        // **未定義の名前は空へ落とす**（`ADR-0049` の 2）。**何も足さない。**
        // SAFETY: 呼び出し元契約をそのまま渡す。
        let Some(value) = (unsafe { userlib::environment(stack, &word[start..end]) }) else {
            continue;
        };
        // SAFETY: カーネルが NUL 終端で積んだ文字列である。
        let length = unsafe { userlib::length_of(value, VALUE_MAX) };
        for index in 0..length {
            // SAFETY: 上で数えた長さの範囲である。
            if !put(unsafe { *value.add(index) }, &mut used) {
                return None;
            }
        }
    }
    Some(used)
}

/// 行を展開する（SE-d。`ADR-0049`）。**書けた長さを返す。入らなければ `None`。**
///
/// # 語ごとに展開する
///
/// **丸ごと空になった語を落とすためである**（`ADR-0049` の 3）。
/// **`echo a $UNSET b` は `a b` になる**——空白は 1 つである。
///
/// **展開の結果で語を割らない**（`ADR-0049` の 4）。**値の中に空白が在っても、
/// 語は増えない**——ここが `sh` と違うところである。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
unsafe fn expand_line(stack: *const u64, line: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut used = 0usize;
    let mut at = 0usize;
    let mut word = [0u8; LINE_MAX];
    while at < line.len() {
        while at < line.len() && line[at] == b' ' {
            at += 1;
        }
        if at >= line.len() {
            break;
        }
        let start = at;
        while at < line.len() && line[at] != b' ' {
            at += 1;
        }
        // SAFETY: 呼び出し元契約をそのまま渡す。
        let length = unsafe { expand_word(stack, &line[start..at], &mut word) }?;
        // **丸ごと空になった語は落とす**（`ADR-0049` の 3）。
        //
        // **ここを外しても振る舞いは変わらない。** **落とす代わりに空白が
        // 1 つ余分に出るだけで、`run_line` が空白の連なりを読み飛ばす**ので、
        // **`argv` に空の語は現れない。** **実測で確かめた**（2026-08-28。
        // 破壊 feature を書いて `--full` を通し、捕まらなかった）。
        //
        // **したがって、この規則は 2 重に守られている**——**ここと、語へ切る側である。**
        // **その形でしか落ちない判定が作れないので、破壊は置かない**
        // （`ADR-0049` の Addendum）。**明示は残す**——**語へ切る側の実装が
        // 変わったとき、ここが最後の守りになる。**
        if length == 0 {
            continue;
        }
        if used > 0 {
            if used >= out.len() {
                return None;
            }
            out[used] = b' ';
            used += 1;
        }
        if used + length > out.len() {
            return None;
        }
        out[used..used + length].copy_from_slice(&word[..length]);
        used += length;
    }
    Some(used)
}

/// 1 行を実行する。
///
/// # 組み込みと外部を分ける
///
/// **組み込みは `exit` だけである。** **外部として起こせないものが、少なくとも
/// 1 つ要る**——分岐の形が最初から在れば、後から足すときに構造を変えずに済む
/// （`docs/vision.md` の「コマンドの実行を組み込みと外部で分ける」。
/// **あそこが「コストがゼロで効く」として挙げている 2 点の 1 つである**）。
///
/// # 何を組み込みにするかの基準（S12 前の手当ての 3 本目）
///
/// **Linux と同じ基準を採る。** 向こうで組み込みになる理由は 3 種類あり、
/// **そのうち 1 つだけが ZaytOS で成り立つ。**
///
/// - **(1) シェル自身の状態を変えるので、外部では実装できない**——`cd`・`exit`・
///   `export`・`umask`。**採る。** **`exit` がこれである**——外部の `/bin/exit` を
///   `spawn` しても**子が終わるだけで、親のシェルは生き続ける。**
/// - **(2) 速さのため**——`echo`・`test`・`pwd`。外部でも書けるが、
///   **毎回プロセスを起こすのが無駄だから組み込みにする。**
///   **採らない。ZaytOS にはこの動機が無い**——[`userlib::spawn`] は同期で
///   1 本ずつ走らせる形なので、**比べる相手がいない。**
///   **「速いから組み込みにする」を理由にしない。**
/// - **(3) 文法の一部**——`if`・`while`・`for`。**制御構造を持ったときに来る。**
///   **まだ来ていない。**
///
/// **組み込みは「外部が無くてよい」という意味ではない。**
/// **`bash` も外部の `/bin/echo` を持っている。** 同じ名前の外部があってよい。
///
/// # 次に来るのは `cd` である。そしてそれはカーネルに触れる
///
/// **`cd` は (1) に当たるので、基準からは組み込みになる。**
/// **ただし「組み込みを 1 つ足す」話ではない。** 先に決めることが 3 つある。
///
/// - **作業ディレクトリを誰が持つか。** シェルが自分で持つか、
///   **カーネルがプロセスごとに持つか**
/// - **`spawn` が相対パスを受け取れるか。** **いまカーネルの `lookup` は
///   常にルートから辿る**（`common::ext2`。`/` で分割して空の要素を飛ばす）
/// - **`openat` を置かなかった判断の前提が消える。** S11 の棚卸しで
///   **「作業ディレクトリが無いので `dirfd` に渡すものが無い」**として置かなかった。
///   **`cd` はその前提を壊す側である。**
///
/// # ADR は起こさない
///
/// **分ける形は既に入っており、ここで書いたのは基準だけである。**
/// `docs/coding-standards.md` の「ADR を書くかの条件」の 3 つに当てた。
/// **当たらなかった条件も書く。**
///
/// - **条件 (1) 複数の有力案から一つを選ぶ——当たらない。**
///   **Linux の基準をそのまま採っており、選んでいない。**
///   採らなかった 2 種類も、Linux の側で理由が分かれている
/// - **条件 (2) 後から変更すると作り直しが生じる——当たらない。**
///   基準を変えても、組み込みを 1 つ増やすか減らすかである
/// - **条件 (4) 将来の保守者が単純化すると壊す——当たらない。**
///   **分岐そのものは `exit` が要求している**ので、基準を消しても分岐は残る
///
/// # `/` を含まない語は `/bin/` の下で探す
///
/// **これは `PATH` ではない。** 環境変数は一切読んでいない——
/// **`envp` をまだ開けていないので、読む先が無い。**
/// **[`DEFAULT_DIR`] は 1 つだけの固定の既定で、探索の順序も無い。**
///
/// **置き換えの条件は「`envp` を開けたとき」である。** 段の名前ではない
/// （`docs/coding-standards.md` の「段を閉じるときは、その段の名前で全 docs を
/// grep する」——**段名で書いた条件は、どの段の grep にも出ない**）。
/// **開いたら、この固定の既定を `PATH` の解決へ置き換える。**
///
/// **出所。** 初期の Unix のシェルがこの形だった。**`PATH` は後から入った**
/// もので、それまでは既定のディレクトリが決め打ちだった。
/// **`PATH` の前に固定の既定があったという順序を、そのままなぞっている。**
///
/// # ADR は起こさない
///
/// **`docs/coding-standards.md` の「ADR を書くかの条件」の 3 つに当てた。**
/// **当たらなかった条件も書く**（あそこがそう求めている）。
///
/// - **条件 (1) 複数の有力案から一つを選ぶ——当たる。**
///   固定の既定・`PATH`・何もしない、の 3 案があった。
///   **ただし記録先は ADR でなくてよい**——選んだ理由と置き換えの条件が
///   この doc に在れば、次に触る者が読む位置にある
/// - **条件 (2) 後から変更すると作り直しが生じる——当たらない。**
///   このファイルの十数行で、`envp` が開いたら差し替えるだけである
/// - **条件 (4) 将来の保守者が単純化すると壊す——当たらない。**
///   消しても `/bin/ls` と書けば動く。**壊れるのは打ちやすさだけである**
///
/// # 行をその場で切る
///
/// **空白を NUL へ置き換え、各語の先頭を指す配列を作る。**
/// **写しを取らない**——`argv` の要素はカーネルが写すので、
/// **この行が生きているあいだ有効であれば足りる。**
fn run_line(line: &mut [u8]) {
    // **語へ切る。** 空白は 1 種類だけ見る（`0x20`）。
    //
    // **以前ここには「タブはまだ来ない——decode.rs が Tab を文字として
    // 出さない」と書いてあった。誤りだった**——**`decode.rs` は Tab を
    // `Char('\t')` として出しており、実測で `0x09` が行へ入っていた**
    // （2026-08-28。`f` `0x09` `g` という語になった）。
    // **いまは入力の輪が捨てるので、ここへは来ない**（`ADR-0050`）。
    let mut starts = [0usize; MAX_ARGS];
    let mut count = 0usize;
    let mut at = 0usize;
    while at < line.len() && count < MAX_ARGS {
        while at < line.len() && line[at] == b' ' {
            line[at] = 0;
            at += 1;
        }
        if at >= line.len() {
            break;
        }
        starts[count] = at;
        count += 1;
        while at < line.len() && line[at] != b' ' {
            at += 1;
        }
    }
    if count == 0 {
        return;
    }
    // **残りの空白も NUL にする。** 語の切れ目はすべて NUL になる。
    // **行末の 1 バイトは呼び出し側が置いてある。**
    for byte in line.iter_mut().skip(at) {
        *byte = 0;
    }
    run_with_terminator(line, &starts[..count])
}

/// NUL で切り終えた行から `argv` を組み立て、起こす。
fn run_with_terminator(line: &[u8], starts: &[usize]) {
    // **組み込みを先に見る。** **既定の前置よりも前である**——
    // **`exit` が `/bin/exit` として探されることは無い。**
    let command = word_at(line, starts[0]);
    if command == BUILTIN_EXIT {
        exit(0);
    }

    let mut argv = [core::ptr::null::<u8>(); MAX_ARGS + 1];
    for (slot, start) in argv.iter_mut().zip(starts.iter()) {
        *slot = line[*start..].as_ptr();
    }

    // **`/` を含まない語は `/bin/` の下で探す。** 前置した写しを作る。
    //
    // **`argv[0]` は書き換えない。** 上で組み立てた `argv` は `line` の中の語を
    // 指したままで、**打った語がそのまま子へ届く**（Unix と同じ扱いである。
    // `spawn-test` が `argv[0]` を突き合わせているので、前置してしまうと落ちる）。
    //
    // **前置き済みの静的を使う。** `/bin/` は毎回同じなので、**書くのは語の
    // ぶんだけ**である（スタックに置いて毎回組み立てると、`.text` がそのぶん増える。
    // **この程度の差が効くほど余裕が無い**——`kernel/userland/user.ld` の
    // 受け皿の位置を見ること）。
    let status = if command.contains(&b'/') {
        // **`/` を含む語は、そのまま渡す。** 探索はしない（Unix と同じ）。
        //
        // SAFETY: `command` は `line` の中の NUL 終端の語で、`argv` は NULL 終端の
        // ポインタ配列である（各要素も `line` の中の NUL 終端の語を指す）。
        unsafe { userlib::spawn(command, &argv[..starts.len() + 1]) }
    } else {
        // **名前だけの語は `PATH` の下で探す（DIR-1。ADR-0043）。**
        //
        // SAFETY: `argv` は NULL 終端のポインタ配列で、各要素は `line` の中の
        // NUL 終端の語を指す。
        unsafe { spawn_via_path(command, &argv[..starts.len() + 1]) }
    };
    if status < 0 {
        write_all(STDERR, NOT_FOUND_HEAD);
        write_all(STDERR, command);
        write_all(STDERR, NOT_FOUND_TAIL);
        return;
    }
    // **止められた子は、状態ではなく 1 行で報せる（S12 前の手当て、C）。**
    //
    // **子に落ち度が無いので、終了状態として数字を出さない。**
    // **打った人は自分で止めたことを知っている**ので、短くてよい。
    if status as u64 == SPAWN_INTERRUPTED {
        write_all(STDOUT, INTERRUPTED_LINE);
        return;
    }
    if status != 0 {
        // **0 以外は返す。** 何が起きたかは子が出している。
        write_all(STDOUT, STATUS_HEAD);
        write_decimal(status as u64);
        write_all(STDOUT, b"\n");
    }
}

/// `line` の `start` から語の終わりまでを返す。
///
/// **NUL そのものは含めない。** `spawn` へ渡すのはポインタで、
/// **カーネルは NUL まで読む**（`copy_user_path`）。**終端は `line` の中に在る。**
fn word_at(line: &[u8], start: usize) -> &[u8] {
    let mut end = start;
    while end < line.len() && line[end] != 0 {
        end += 1;
    }
    &line[start..end]
}

/// 10 進で書く。**上限は 3 桁で足りる**（終了状態は `0..=255`）。
fn write_decimal(value: u64) {
    let mut digits = [b'0'; 3];
    let mut length = 0usize;
    let mut rest = value;
    loop {
        digits[2 - length] = b'0' + (rest % 10) as u8;
        length += 1;
        rest /= 10;
        if rest == 0 || length == 3 {
            break;
        }
    }
    write_all(STDOUT, &digits[3 - length..]);
}

/// 挿入点より後ろを書き直し、**消えた 1 セルを空白で潰してから**カーソルを戻す。
///
/// # なぜ書き直すのか
///
/// **端末も画面も「消した」ことを知らない。** 後ろを詰めたのはこちらの配列の中
/// だけなので、**同じ並びを書き直さないと画面が古いままになる。**
///
/// **末尾の空白は、詰めたぶんの 1 セルを潰すためである。** 詰めると行は 1 つ短く
/// なるが、画面には前の最後の字が残っている。
///
/// **戻す数は「書いた数」である。** 書いた後のカーソルは行の末尾にあり、
/// 挿入点はそこから `tail.len() + 1` だけ左である（空白のぶんを含む）。
/// 挿入点を左へ 1 つ動かす（SE-b で切り出した。SE-c で `Ctrl+B` も通す）。
///
/// **行頭より左へは動かない。**
fn move_left(cursor: &mut usize) {
    if *cursor > 0 {
        *cursor -= 1;
        write_all(STDOUT, b"\x08");
    }
}

/// 挿入点を右へ 1 つ動かす（SE-c で `Ctrl+F` も通す）。
///
/// **カーソルを右へ動かすのに、その位置の字をもう一度書く。**
/// `\x1b[C` を出す形もあるが、**画面（`Grid`）はエスケープを解釈しない**ので
/// 届かない。字なら両方で動く。
fn move_right(line: &[u8], cursor: &mut usize, length: usize) {
    if *cursor < length {
        write_all(STDOUT, &line[*cursor..*cursor + 1]);
        *cursor += 1;
    }
}

/// 挿入点を行頭へ動かす（SE-b）。**後退をまとめて 1 回で書く。**
fn move_to_start(cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    write_all(STDOUT, &BACKSPACES[..*cursor]);
    *cursor = 0;
}

/// 挿入点を行末へ動かす（SE-b）。
///
/// **右へ動かすのに、その位置の字をもう一度書く。** `\x1b[C` を出す形もあるが、
/// **画面（`Grid`）はエスケープを解釈しない**ので届かない（矢印と同じ判断）。
fn move_to_end(line: &[u8], cursor: &mut usize, length: usize) {
    if *cursor >= length {
        return;
    }
    write_all(STDOUT, &line[*cursor..length]);
    *cursor = length;
}

/// 行の `start..end` を消し、後ろを詰める（SE-f）。**挿入点は `start` へ置く。**
///
/// # なぜ 1 本にするのか
///
/// **消す鍵が 5 つある**（Backspace / Delete / `Ctrl+D` / `Ctrl+K` / `Ctrl+U` /
/// `Ctrl+W`）。**違うのは範囲の計算だけで、消し方と描き直しは同じである。**
/// **別々に書くと、6 つの経路で振る舞いがずれる**——`Ctrl+A` と Home を
/// 同じ関数へ通したのと同じ判断である（SE-b）。
///
/// **以前は `redraw_tail` と `redraw_tail_after_delete` の 2 つが在った。**
/// **前者は「後ろが空なら何もしない」形で、Backspace が先に 1 セル潰している
/// ことに寄りかかっていた。** **ここでは消した数だけ潰すので、寄りかかりが消える。**
///
/// **`start <= *cursor` を前提にする。** 呼ぶ側はすべて挿入点を含むか、
/// その手前までの範囲を渡す。
fn delete_range(
    line: &mut [u8; LINE_MAX],
    length: &mut usize,
    cursor: &mut usize,
    start: usize,
    end: usize,
) {
    // 破壊 (SE-f, shell-shift-delete-range-test): 先頭を 1 つ後ろへずらす。
    // **消える字が 1 つ減る**ので、走る語が変わる。
    let start = if SHIFT_DELETE_RANGE { start + 1 } else { start };
    if start >= end || end > *length {
        return;
    }
    let removed = end - start;
    // **画面の挿入点を `start` へ戻す。**
    if *cursor > start {
        write_all(STDOUT, &BACKSPACES[..*cursor - start]);
    }
    line.copy_within(end..*length, start);
    *length -= removed;
    *cursor = start;
    // **後ろを書き直し、消えたぶんのセルを空白で潰し、まとめて戻る。**
    let tail = *length - start;
    if tail > 0 {
        write_all(STDOUT, &line[start..*length]);
    }
    write_all(STDOUT, &SPACES[..removed]);
    write_all(STDOUT, &BACKSPACES[..tail + removed]);
}

/// `Ctrl+W` が消す範囲の先頭（SE-f）。
///
/// # 切れ目は空白だけである
///
/// **`bash` の `unix-word-rubout` と同じで、`run_line` が語へ切る規則とも同じ**
/// である（`0x20` だけを見る）。**記号を切れ目にしない**——**この体制に「語」の
/// 定義は 1 つしか無く、2 つ目を作ると、消える範囲と走る語がずれる。**
///
/// **挿入点の手前の空白を先に飛ばす。** `ab cd ` の末尾で打つと `ab ` が残る。
fn word_start(line: &[u8], cursor: usize) -> usize {
    let mut at = cursor;
    while at > 0 && line[at - 1] == b' ' {
        at -= 1;
    }
    while at > 0 && line[at - 1] != b' ' {
        at -= 1;
    }
    at
}

/// 画面を消して、プロンプトと打ちかけの行を描き直す（SE-f。`Ctrl+L`）。
///
/// **消しただけだと、打ちかけが見えなくなる。**
/// **挿入点の位置も戻す**——行末まで書いてから、そのぶん後退する。
fn redraw_screen(line: &[u8], length: usize, cursor: usize) {
    write_all(STDOUT, CLEAR_SCREEN);
    write_prompt();
    if length > 0 {
        write_all(STDOUT, &line[..length]);
    }
    if cursor < length {
        write_all(STDOUT, &BACKSPACES[..length - cursor]);
    }
}

/// 挿入点より後ろを書き直し、カーソルを戻す。**空白は置かない。**
///
/// 字を入れた側から呼ぶ。**行は 1 つ伸びているので、潰すセルが無い。**
fn redraw_tail_without_gap(tail: &[u8]) {
    if tail.is_empty() {
        return;
    }
    write_all(STDOUT, tail);
    for _ in 0..tail.len() {
        write_all(STDOUT, b"\x08");
    }
}

/// エスケープの受けの状態（S12 前の手当て。SE-b で 4 バイトの形が増えた）。
///
/// **2 つの形を受ける。** 矢印は `\x1b` `[` `D` / `C` / `A` / `B` の 3 バイトで、
/// **Home / Delete / End は `\x1b` `[` 数 `~` の 4 バイトである**
/// （`kernel/src/input.rs` の `bytes_for_event`。`ADR-0050`）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    /// 何も溜めていない。
    Idle,
    /// `\x1b` を受けた。
    Esc,
    /// `\x1b[` を受けた。
    Bracket,
    /// `\x1b[` に続けて数を受けた。**次が `~` なら確定する。**
    Number(u8),
}
