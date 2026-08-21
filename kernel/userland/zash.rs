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
/// [`RESOLVED`] の前置きの長さ（`/bin/`）。
const DEFAULT_DIR_LEN: usize = 5;
/// 前置きの対象にする語の最大の長さ。
///
/// **越えたらそのまま渡す**（`/bin/` の下でその名前は探さない）。
/// **`/bin` に 32 バイトを越える名前を置く見込みが無い**ので、
/// **緩衝を行の長さと同じにしない**——`.text` にも `.bss` にも効く。
const NAME_MAX: usize = 32;

/// `/` を含まない語を前置して組み立てる先。**`PATH` ではない**（`run_line` の doc）。
///
/// **前置きを埋めた状態で持つ。** 毎回 `/bin/` を書き直さずに済む。
static mut RESOLVED: [u8; DEFAULT_DIR_LEN + NAME_MAX + 1] = {
    let mut buffer = [0u8; DEFAULT_DIR_LEN + NAME_MAX + 1];
    buffer[0] = b'/';
    buffer[1] = b'b';
    buffer[2] = b'i';
    buffer[3] = b'n';
    buffer[4] = b'/';
    buffer
};
/// 行が長すぎたときの断り書き。
const TOO_LONG: &[u8] = b"zash: line too long\n";

/// `-EAGAIN`。**溜まっていないという意味で、失敗ではない。**
const MINUS_EAGAIN: i64 = -11;
/// Backspace のバイト。
const BACKSPACE: u8 = 0x08;

/// プロンプトを出す（ES-d）。**色を付けてから戻す。**
///
/// **3 回書くのではなく 1 回で書く。** `write` は 1 回ごとに画面へ届いて
/// フラッシュされるので、**分けて書くと色の無いプロンプトが一瞬出る。**
/// **判定は入力待ちの時点で見るので、そこでは差が出ない**——それでも、
/// **人が見る側で点滅させる理由が無い。**
fn write_prompt() {
    // 破壊 (ES-d, zash-prompt-drop-color): 色を送らずにプロンプトを出す。
    // **プロンプトの字も位置も変わらない**ので、既存の判定はどれも動かない。
    // **画面のセルが既定前景のままになる**ので、zi-test の
    // 「プロンプトが自分の色で描かれている」判定だけが落ちる。
    #[cfg(zash_prompt_drop_color)]
    let parts: [&[u8]; 2] = [PROMPT_NAME, PROMPT_SYMBOL];
    // **記号には SGR を掛けない。** **戻してから出す**ので、記号のセルは
    // 既定前景そのものになる——**色が記号へ漏れていないことを、判定が
    // 「連なりの直後のセルが既定色であること」で見る。**
    #[cfg(not(zash_prompt_drop_color))]
    let parts: [&[u8]; 4] = [PROMPT_COLOR, PROMPT_NAME, SGR_RESET, PROMPT_SYMBOL];
    let mut out = [0u8; PROMPT_COLOR.len() + PROMPT.len() + SGR_RESET.len()];
    let mut at = 0usize;
    for part in parts {
        out[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    write_all(STDOUT, &out[..at]);
}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(_stack: *const u64) -> ! {
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
                // **左へ 1 つ。行頭より左へは動かない。**
                if cursor > 0 {
                    cursor -= 1;
                    write_all(STDOUT, b"\x08");
                }
                continue;
            }
            (Escape::Bracket, b'C') => {
                escape = Escape::Idle;
                // **右へ 1 つ。行末より右へは動かない。**
                //
                // **カーソルを右へ動かすのに、その位置の字をもう一度書く。**
                // `\x1b[C` を出す形もあるが、**画面（`Grid`）はエスケープを
                // 解釈しない**ので届かない。字なら両方で動く。
                if cursor < length {
                    write_all(STDOUT, &line[cursor..cursor + 1]);
                    cursor += 1;
                }
                continue;
            }
            (Escape::Bracket, b'A') | (Escape::Bracket, b'B') => {
                escape = Escape::Idle;
                // **上下は何もしない（zi-a）。** 履歴が無いので動かす先が無い。
                // **知らない並びの分岐へ落とさない**——あちらは最後のバイトを
                // 普通の字として行へ入れるので、**上矢印を押すたびに `A` が
                // 挿入されてしまう。** 読んで捨てるのが正しい形である。
                continue;
            }
            (Escape::Esc, _) | (Escape::Bracket, _) => {
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
                if overflowed {
                    write_all(STDOUT, TOO_LONG);
                } else if length > 0 {
                    // **終端を置いてから渡す。** 語の末尾は `line` の中の NUL で
                    // 決まるので、**前の行の残りが続きとして読まれない**ように
                    // ここで 1 バイト置く（`LINE_MAX` は 1 行より大きいので在る）。
                    line[length] = 0;
                    run_line(&mut line[..length]);
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
                overflowed = false;
                escape = Escape::Idle;
                write_prompt();
            }
            BACKSPACE => {
                // **挿入点の直前を消して、後ろを詰める（S12 前の手当て）。**
                // **行頭では何もしない。**
                if cursor > 0 {
                    line.copy_within(cursor..length, cursor - 1);
                    cursor -= 1;
                    length -= 1;
                    // **後退・空白・後退で 1 つ消す。** 画面もこれで消える
                    // （`Grid` が `\x08` でカーソルを戻すようにした）。
                    write_all(STDOUT, b"\x08 \x08");
                    // **挿入点より後ろがあるなら、書き直して詰める。**
                    // **末尾に空白を 1 つ置いて、消えた 1 セルを潰す。**
                    // そのぶんカーソルが右へ動くので、同じ数だけ戻す。
                    redraw_tail(&line[cursor..length]);
                }
            }
            other => {
                if length < line.len() {
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
    // **語へ切る。** 空白は 1 種類だけ見る（`0x20`）。**タブはまだ来ない**
    // ——`kernel/src/keyboard/decode.rs` が Tab を文字として出さない。
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
    let path: &[u8] = if command.contains(&b'/') || command.len() > NAME_MAX {
        command
    } else {
        // SAFETY: このプログラムは Ring 3 で 1 本だけ走り、割り込みハンドラも
        // シグナルも無い。**`RESOLVED` へ触るのはこの行だけである。**
        let resolved = unsafe { &mut *core::ptr::addr_of_mut!(RESOLVED) };
        resolved[DEFAULT_DIR_LEN..DEFAULT_DIR_LEN + command.len()].copy_from_slice(command);
        // **終端を置く。** 前の語のほうが長かった場合に、その残りが続きとして
        // 読まれない。
        resolved[DEFAULT_DIR_LEN + command.len()] = 0;
        &resolved[..DEFAULT_DIR_LEN + command.len()]
    };

    // SAFETY: `path` は NUL 終端で、`argv` は NULL 終端のポインタ配列である。
    // **各要素は `line` の中の NUL 終端の語を指す。**
    // **前置した側の終端は、配列の残りが 0 であることによる。**
    let status = unsafe { userlib::spawn(path, &argv[..starts.len() + 1]) };
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
fn redraw_tail(tail: &[u8]) {
    // **行末で消したなら、書き直すものが無い。** 直前の後退・空白・後退が
    // 最後のセルを潰しているので、**ここで空白をもう 1 つ置くと 1 セル余計に
    // 塗ることになる**（実測で `\x08 \x08 \x08` と 5 バイト出ていた）。
    if tail.is_empty() {
        return;
    }
    write_all(STDOUT, tail);
    write_all(STDOUT, b" ");
    for _ in 0..tail.len() + 1 {
        write_all(STDOUT, b"\x08");
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

/// エスケープの受けの状態（S12 前の手当て）。
///
/// **3 バイトしか見ない。** 矢印は `\x1b` `[` `D` / `C` で届く。
/// **数を伴う形（`\x1b[3~` など）は来ない**——落としているのは
/// `kernel/src/input.rs` で、そこが出すのはこの 2 つだけである。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    /// 何も溜めていない。
    Idle,
    /// `\x1b` を受けた。
    Esc,
    /// `\x1b[` を受けた。
    Bracket,
}
