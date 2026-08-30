//! `zi`: ターミナル上の vi 風エディタ（zi-d-1）。
//!
//! # crate ではない
//!
//! `ls.rs` / `cat.rs` と同じで、cargo のパッケージに属さない
//! （`userlib.rs` の doc）。
//!
//! # 何が観測でき、何が観測できないか
//!
//! **この段の自動判定が見るのは `zi` の内部状態であって、画面ではない。**
//! カーソル位置の判定行（`zi: cursor ...`）は**バッファ上の行と桁**で、
//! **画面に何が描かれたかではない。** `zi` は Ring 3 に居て `Console` を
//! 読めないので、**再描画の誤り（CUP の位置が 1 つずれる等）はこの判定では
//! 捕まらない**——ファイルの中身が正しいまま画面だけが崩れる形が作れる。
//!
//! **したがって「台本が緑だから画面も正しい」とは読めない。**
//! ANSI の解釈そのもの（CUP・ED・EL がセルへどう効くか）は
//! `ansi-test` が別に固定しており（ADR-0029 の Addendum）、
//! **`zi` が意図した列を出しているかは目視の補助に委ねてある**
//! （`cargo xtask run --gui --manual`）。
//!
//! # zi-c の契約から来る順序
//!
//! **`O_WRONLY|O_TRUNC` は open の時点で長さ 0 へ切る**（ADR-0037）ので、
//! **読みながら書き先を開いておくことはできない。** 起動時に
//! `open(O_RDONLY)` で全部読んで閉じ、`:w` のときに開き直す形になる。
//!
//! # 終了状態の意味
//!
//! - `0` 正常に終わった
//! - `1` 開けなかった
//! - `2` 引数が無かった
//! - `3` 読めなかった
//! - `4` 読んでいる途中でヒープを伸ばせなかった（**切り詰めない**——切り詰めて
//!   保存すると開いた時点で中身が消える。**b-2 まではここが「上限を越えている」
//!   だった。その上限は消えた**）
//! - `5` `:w` が失敗した（開けない、または書いた量が要求と食い違う）
//! - `6` 最初の確保に失敗した（H-b-1。`brk` が断った）
//!
//! # 入れ物はヒープの上に在る（H-b-1）
//!
//! **本文は 1 本の連続バイト列で、各行の後ろに改行が 1 つ在る。**
//! **行の索引が、その中の各行の開始位置を持つ。** **どちらも
//! [`userlib::heap`] から 1 回で取った領域の中に並ぶ**（ADR-0044）。
//!
//! **`.bss` の固定配列を 3 つ持っていた**——編集中の行の集まり、読み込みの
//! 受け皿、書き出しの受け皿である。**3 つとも消えた**——
//! **読み込みの受け皿がそのまま本体になり、保存は本体をそのまま書く。**

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

// **`common` の純粋な論理を取り込む（VIEW-a。ADR-0045）。**
//
// **`less` と `more` も同じものを使う。** **自己完結でなければならない**
// （`crate::` を参照しない）。**ホストテストは `cargo test -p common` が持つ。**
// **`dead_code` を許す。** **`zi` が使わない口が在る**——`height()` は
// `less` が使う見込みで、`top()` は診断の構成でしか呼ばれない。
// **`common` の側では使われているので、あちらでは黙らせていない。**
#[path = "../../common/src/window.rs"]
#[allow(dead_code)]
mod window;

use window::Window;

use userlib::{
    close, exit, length_of, open_read_only, open_write_create, read, write_all, STDERR, STDOUT,
};

/// パスの最大長（NUL を含む）。**カーネルの `PATH_MAX` と同じ。**
const PATH_MAX: usize = 256;

/// 開くときに本文へ足す余白（H-b-2）。**1 ページである。**
///
/// # 容量はファイルの大きさから決める
///
/// **`stat` で訊いた大きさ + 改行の補い 1 + 同じ大きさ + この余白**である。
/// **「同じ大きさ」を足すのは、開いたファイルが倍に育つまで伸ばさずに済む
/// ようにするためである。** **余白のほうは、空のファイルにも 1 ページ渡る。**
///
/// **b-1 の `MAX_LINES` と `MAX_LINE_LEN` はここで消えた。**
/// **上限はヒープの上限（`0x7ff000`。ADR-0044 の決定 4）だけである。**
const TEXT_SLACK: usize = 4096;

/// 本文を伸ばすときの最小の刻み（H-b-2）。**1 ページである。**
///
/// **1 バイト足りないたびに `brk` を呼ぶ形にしない。**
const TEXT_STEP: usize = 4096;

/// 索引の初期の枠（H-b-2）。**行数 + 番兵 1 である。**
///
/// **b-1 の初期容量（64 行）をそのまま持ってきた。** **開いた直後に
/// 足りなければ [`Buffer::ensure_index`] が伸ばす**ので、**上限ではない。**
const INDEX_INITIAL: usize = 64 + 1;

/// 索引を伸ばすときの最小の刻み（H-b-2）。**行の単位である。**
const INDEX_STEP: usize = 64;

/// 1 回に読む大きさ。`cat` と同じ理由で、ブロックより小さくてよい。
///
/// # b-1 でも固定のまま残す
///
/// **これは容量ではなく、`read` に渡す長さである。** **ヒープが伸びても、
/// 1 回のシステムコールで受ける量を変える理由が無い。**
///
/// **本文へ直に読み込まないのも、ここに理由がある。** **`read` の長さを
/// 残り容量で切ると、「ちょうど埋まった」と「入りきらなかった」が同じ形になる。**
/// **中継ぎを 1 つ挟むと、`read` の長さが常に [`CHUNK`] で揃い、
/// 受け取ってから容量を確かめられる**（H-b-2 では、そこで伸ばす）。
const CHUNK: usize = 256;

/// 引数が無いときの使い方。
const USAGE: &[u8] = b"zi: usage: zi PATH\n";
/// 開けなかったときの断り書き。
const OPEN_FAILED: &[u8] = b"zi: cannot open\n";
/// 読めなかったときの断り書き。
const READ_FAILED: &[u8] = b"zi: cannot read\n";
/// 読んでいる途中でヒープを伸ばせなかったときの断り書き（H-b-3 で文言を直した）。
///
/// **切り詰めない。** 切り詰めて保存すると、開いた時点で中身が消える。
///
/// # 文言を意味へ合わせた（H-b-3）
///
/// **b-2 まで `zi: the file does not fit the buffer` だった。**
/// **あれは「64 行 x 128 バイトの固定配列に入らない」という意味だった**
/// ——**その配列は b-2 で消えた。** **いま出るのは、ヒープを伸ばせなかった
/// ときだけである。** **古い文言を残すと、次に読む者が 64 行の話だと思う。**
const OUT_OF_MEMORY_READING: &[u8] = b"zi: ran out of memory while reading the file\n";
/// 最初の確保に失敗したときの断り書き（H-b-1）。
///
/// **[`OUT_OF_MEMORY_READING`] と分けてある**——**開く前に断ったのか、
/// 読んでいる途中で足りなくなったのかが、文言で分かるようにする。**
const NO_HEAP: &[u8] = b"zi: cannot reserve memory for the edit buffer\n";

/// 行を増やせなかったときの報せ（zi-f。H-b-3 で文言を直した）。
///
/// **黙って落とさない。** **`insert` が入らない字を落とすのと同じ判断だが、
/// あちらは 1 字で、こちらは「行が作れない」である**——**使う人から見て
/// 何も起きないので、言わないと分からない。**
///
/// # 64 行という値は消えた（H-b-2）
///
/// **b-1 まではこれが `MAX_LINES` に当たった報せで、文言も
/// `no room for another line` だった。** **いま出るのは、ヒープを
/// 伸ばせなかったときだけである**（`brk` がヒープの上限で断ったか、
/// フレームが尽きたか）。**古い文言を残すと、次に読む者が 64 行の話だと思う。**
///
/// # この報せが出る形は、まだ通っていない
///
/// **4MiB を使い切る利用者が居ない**（`docs/deferred-decisions.md` の
/// 「ヒープを使い切る経路が4つとも未検査である」）。**通していないと書いておく。**
const OUT_OF_MEMORY_FOR_A_LINE: &[u8] = b"out of memory: cannot add a line";

/// `read(0)` が「まだ無い」を返す値（`-EAGAIN`）。
const MINUS_EAGAIN: i64 = -11;
/// Esc のバイト。
const ESC: u8 = 0x1b;

/// コマンド行の最大長（`:wq` で足りるが、余裕を取る）。
const COMMAND_MAX: usize = 16;

/// 状態行の色（ES-d）。**SGR の truecolor で前景を指定する。**
///
/// # 色は判定から選んだ
///
/// **黄 `(200, 200, 0)` である。** 画面の実物を `read_pixel_raw` で読む判定が
/// 付くので、**既に画面に居る色と紛れてはならない**（`zash` の `PROMPT_COLOR`
/// と同じ規律）。**背景 `(0x10, 0x10, 0x18)`・既定前景 `(0xD0, 0xD8, 0xE0)`・
/// ES-b の判定色（緑と赤）・カーソルのシアンのいずれとも、RGB のどれかの軸で
/// 150 以上離れている。**
///
/// **`zash` のプロンプトの名前も緑 `(0, 200, 0)` である**（zi-e 前の色替え）。
/// **同じ画面に並びうるが、判定は別々に読む**——プロンプトはカーソルの居る行、
/// 状態行は本文の下である（`kernel/src/console/probe.rs`）。
const STATUS_COLOR: &[u8] = b"\x1b[38;2;200;200;0m";
/// 色を既定へ戻す（SGR 0）。
const SGR_RESET: &[u8] = b"\x1b[0m";

/// 状態行の札（ES-d）。**3 つとも同じ長さである。**
///
/// # 長さを揃えると消去が要らない
///
/// **札を上書きするだけで前の札が残らない。** 揃えないと `EL(2)` が要り、
/// **`EL(2)` を足すと `parse_zi_last_redraw`（xtask）が状態行を本文の行として
/// 拾う**——あちらは再描画の列を `\x1b[2K` で切って行を取り出している。
/// **揃えるほうが、判定の側に例外を作らずに済む。**
const STATUS_NORMAL: &[u8] = b"-- NORMAL  --";
const STATUS_INSERT: &[u8] = b"-- INSERT  --";
const STATUS_COMMAND: &[u8] = b"-- COMMAND --";

/// 編集中のモード。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Insert,
    /// `:` を打った後。**改行までを溜めて解釈する。**
    Command,
}

/// エスケープ列の受け（`zash` と同じ形。`kernel/src/input.rs` が落とす形）。
///
/// **矢印は 3 バイトで届く**（`\x1b` `[` に `A`/`B`/`C`/`D`）。
/// **`\x1b` の直後に `[` が来なければ Esc 単体である**——カーネルが CSI の
/// 3 バイトを不可分に組み立てるので、**この判別は確定である**
/// （`kernel/src/input.rs` の `bytes_for_event` の doc）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Escape {
    Idle,
    Esc,
    Bracket,
    /// `\x1b[3` まで来た（zi-f）。**次が `~` なら Delete である。**
    ///
    /// **矢印は 3 バイトで終わるが、Delete は 4 バイトである**
    /// （`\x1b[3~`。本物の端末と同じ形。`kernel/src/input.rs`）。
    /// **数字を溜める形にはしない**——**受けるのは `3~` の 1 種類だけで、
    /// 一般の CSI パラメータを解釈する利用者がまだ居ない。**
    Tilde,
}

/// 行の集まり（H-b-1。H-b-2 で容量が動くようになった）。
///
/// # 形
///
/// **ヒープから取った 1 本の領域を、前半の本文と後半の索引に割って使う。**
///
///     [ 本文 text_cap バイト ][ 索引 index_cap 枠 ]
///
/// **`text[..used]` が、そのまま `:w` の書き出す形である**——
/// **各行の中身の後ろに改行が 1 つ在る。** **`starts[row]` はその行の
/// 開始位置で、`starts[count]` は `used` に等しい**（番兵）。
///
/// **したがって行 `row` の中身は `text[starts[row]..starts[row + 1] - 1]` で、
/// 末尾の 1 バイトを落とした分が改行である。**
///
/// # 索引を後ろに置いた（H-b-2）
///
/// **`brk` は上端しか動かさない。** **索引を後ろに置くと、行が増えたときは
/// 上端を伸ばすだけで済み、何も動かさなくてよい。**
///
/// **本文が伸びるときだけ、索引を後ろへずらす。** **索引は行あたり 8 バイト
/// なので、動かす量は本文よりずっと小さい。** **前後を逆にすると、
/// 本文が伸びるたびに本文の全体を動かすことになる。**
///
/// # 上限
///
/// **行数にも行の長さにも上限が無い**（b-1 の `MAX_LINES` と `MAX_LINE_LEN`
/// は消えた）。**残るのはヒープの上限だけである**——`0x7ff000` まで、
/// 実測で 1012 ページ（約 4MiB。ADR-0044 の決定 4）。
///
/// # 代償——1 字入れるたびに後ろ全部が動く
///
/// **`insert` も `remove` も、その位置から `used` までを 1 バイトずらす。**
/// **数 KiB では問題にならないが、大きなファイルでは遅い。**
/// **行ごとに確保する形なら動かさずに済むが、そちらは保存で並べ直しが
/// 復活し、小さな確保が多数になって本物の割り当て器が要る**
/// （H 段 b の設計）。**限界として引き受ける。**
struct Buffer {
    /// ヒープから取った領域の全体。
    ///
    /// **割らずに持つ（H-b-2）。** **[`userlib::heap::grow_to`] と
    /// [`userlib::heap::release`] が一意の参照を値で受け取る**ので、
    /// **渡せる形で持っておく必要がある。** **割った 2 本を持つと渡し直せない。**
    region: &'static mut [u8],
    /// 本文の容量（バイト）。**索引の開始位置でもある。**
    ///
    /// **`usize` の倍数に保つ**——**索引の整列がここで決まる。**
    text_cap: usize,
    /// 索引の枠の数（行数 + 番兵 1 まで入る）。
    index_cap: usize,
    /// 行数。
    count: usize,
    /// 使っているバイト数（改行を含む）。
    used: usize,
}

/// `usize` の整列へ切り上げる（H-b-2）。
///
/// **索引が本文の後ろに在るので、本文の容量が `usize` の倍数でないと
/// 索引の整列が崩れる。**
fn round_up_word(bytes: usize) -> usize {
    let word = userlib::heap::WORD;
    (bytes + word - 1) & !(word - 1)
}

/// 開くときの本文の容量を決める（H-b-2）。
///
/// **ファイルの大きさ + 改行の補い 1 + 同じ大きさ + [`TEXT_SLACK`]。**
/// 理由はあちらの doc にある。
fn text_capacity_for(size: usize) -> usize {
    round_up_word(size + 1 + size + TEXT_SLACK)
}

impl Buffer {
    /// ヒープから取った領域の上に、空のバッファを作る（H-b-1）。
    ///
    /// **1 行（空行）で始まる。** **`text` は改行 1 つで、`used` は 1 である**
    /// ——**空のファイルを `:w` すると 1 バイトになるのは b-1 より前と同じで、
    /// この初期状態がその形である。**
    fn new(region: &'static mut [u8], text_cap: usize, index_cap: usize) -> Option<Self> {
        if index_cap < 2 || text_cap == 0 || region.len() < text_cap + index_cap * userlib::heap::WORD
        {
            return None;
        }
        let mut buffer = Self {
            region,
            text_cap,
            index_cap,
            count: 1,
            used: 1,
        };
        let (text, starts) = buffer.parts_mut();
        text[0] = b'\n';
        starts[0] = 0;
        starts[1] = 1;
        Some(buffer)
    }

    /// 領域を本文と索引へ割る（H-b-2）。
    fn parts(&self) -> (&[u8], &[usize]) {
        let (text, index) = self.region.split_at(self.text_cap);
        // SAFETY: `text_cap` は `WORD` の倍数（[`round_up_word`]）で、領域の
        // 下端はページ境界である（`Heap::from_image_end` が切り上げる）ので、
        // `index` は `usize` の整列を満たす。長さは構築と [`Buffer::regrow`] が
        // `index_cap * WORD` 以上を保っている。
        let starts =
            unsafe { core::slice::from_raw_parts(index.as_ptr().cast::<usize>(), self.index_cap) };
        (text, starts)
    }

    /// 領域を本文と索引へ割る（可変。H-b-2）。**理由は [`Buffer::parts`]。**
    fn parts_mut(&mut self) -> (&mut [u8], &mut [usize]) {
        let index_cap = self.index_cap;
        let (text, index) = self.region.split_at_mut(self.text_cap);
        // SAFETY: [`Buffer::parts`] と同じ。**`split_at_mut` が本文との
        // 重なりを断っている**ので、別名は作られない。
        let starts =
            unsafe { core::slice::from_raw_parts_mut(index.as_mut_ptr().cast::<usize>(), index_cap) };
        (text, starts)
    }

    /// 本文（H-b-2）。
    fn text(&self) -> &[u8] {
        &self.region[..self.text_cap]
    }

    /// 本文（可変。H-b-2）。
    fn text_mut(&mut self) -> &mut [u8] {
        &mut self.region[..self.text_cap]
    }

    /// 行の開始位置。
    fn start_of(&self, row: usize) -> usize {
        self.parts().1[row]
    }

    /// 行の中身。**末尾の改行は含まない。**
    fn line(&self, row: usize) -> &[u8] {
        let (text, starts) = self.parts();
        &text[starts[row]..starts[row + 1] - 1]
    }

    /// 行の長さ。**改行を含まない。**
    fn length(&self, row: usize) -> usize {
        let starts = self.parts().1;
        starts[row + 1] - starts[row] - 1
    }

    /// `:w` が書き出すバイト列（H-b-1）。**そのまま渡す。**
    fn as_bytes(&self) -> &[u8] {
        &self.region[..self.used]
    }

    /// 本文が `needed` バイト入るようにする（H-b-2）。
    ///
    /// **足りていれば何もしない。** **伸ばすときは [`TEXT_STEP`] を下回らない**
    /// ——**1 バイト足りないたびに `brk` を呼ぶ形にしない。**
    fn ensure_text(&mut self, needed: usize) -> bool {
        if needed <= self.text_cap {
            return true;
        }
        let want = round_up_word(if needed > self.text_cap + TEXT_STEP {
            needed
        } else {
            self.text_cap + TEXT_STEP
        });
        self.regrow(want, self.index_cap)
    }

    /// 索引が `slots` 枠入るようにする（H-b-2）。**番兵の分は呼ぶ側が数える。**
    fn ensure_index(&mut self, slots: usize) -> bool {
        if slots <= self.index_cap {
            return true;
        }
        let want = if slots > self.index_cap + INDEX_STEP {
            slots
        } else {
            self.index_cap + INDEX_STEP
        };
        self.regrow(self.text_cap, want)
    }

    /// 領域を取り直して割り直す（H-b-2）。
    ///
    /// # 伸ばせなければ何も変えない
    ///
    /// **[`userlib::heap::grow_to`] は失敗すると受け取った領域をそのまま
    /// 返す**ので、**元の状態へ戻せる。** **偽を返した後も、バッファは
    /// そのまま使える。**
    fn regrow(&mut self, text_cap: usize, index_cap: usize) -> bool {
        let total = text_cap + index_cap * userlib::heap::WORD;
        let region = core::mem::take(&mut self.region);
        match userlib::heap::grow_to(region, total) {
            Ok(bigger) => {
                self.region = bigger;
                // **本文が伸びたぶんだけ索引を後ろへずらす。**
                // **索引だけを増やしたときは動かさない**（上端が伸びるだけ）。
                if text_cap != self.text_cap {
                    let moved = self.index_cap * userlib::heap::WORD;
                    let from = self.text_cap;
                    self.region.copy_within(from..from + moved, text_cap);
                }
                self.text_cap = text_cap;
                self.index_cap = index_cap;
                true
            }
            Err(same) => {
                self.region = same;
                false
            }
        }
    }

    /// 索引の `row` より後ろを `delta` だけずらす（H-b-1）。
    ///
    /// **番兵まで動かす**（`..=self.count`）——**動かし忘れると `used` と
    /// 食い違い、最終行の終わりがずれる。**
    fn shift_index(&mut self, row: usize, delta: isize) {
        let count = self.count;
        let starts = self.parts_mut().1;
        for index in row + 1..=count {
            starts[index] = starts[index].wrapping_add_signed(delta);
        }
    }

    /// 1 バイトを挿入する。**入らなければ落とす**（入っていないものを
    /// 入ったように見せない。`zash` の行編集と同じ判断）。
    ///
    /// **H-b-2 で 1 行の上限が消えた。** **落ちるのはヒープを伸ばせなかった
    /// ときだけである。**
    fn insert(&mut self, row: usize, at: usize, byte: u8) -> bool {
        if at > self.length(row) {
            return false;
        }
        if !self.ensure_text(self.used + 1) {
            return false;
        }
        let (used, count) = (self.used, self.count);
        let (text, starts) = self.parts_mut();
        let position = starts[row] + at;
        text.copy_within(position..used, position + 1);
        text[position] = byte;
        for index in row + 1..=count {
            starts[index] += 1;
        }
        self.used = used + 1;
        true
    }

    /// 行を割る（zi-f。インサートモードの Enter）。
    ///
    /// **`at` 以降を次の行へ移す。** **入らなければ何もしない**
    /// ——**H-b-2 では、ヒープを伸ばせなかった形である。**
    ///
    /// **`insert` と同じ判断である**——**入っていないものを入ったように
    /// 見せない。** **断ったことは呼ぶ側がコマンド行へ出す。**
    fn split_line(&mut self, row: usize, at: usize) -> bool {
        if row >= self.count || at > self.length(row) {
            return false;
        }
        if !self.ensure_text(self.used + 1) {
            return false;
        }
        // **行が 1 つ増えるので、番兵まで入れて `count + 2` 枠が要る。**
        if !self.ensure_index(self.count + 2) {
            return false;
        }
        let (used, count) = (self.used, self.count);
        let (text, starts) = self.parts_mut();
        // **改行を 1 つ挿し込むだけである。** 後ろの中身は動かない
        // ——**1 バイト分ずれるだけで、並びは変わらない。**
        let position = starts[row] + at;
        text.copy_within(position..used, position + 1);
        text[position] = b'\n';
        // **索引に境界を 1 つ足す。** 番兵から順に 1 つずつ後ろへ送る。
        for index in (row + 1..=count).rev() {
            starts[index + 1] = starts[index] + 1;
        }
        starts[row + 1] = position + 1;
        self.used = used + 1;
        self.count = count + 1;
        true
    }

    /// 次の行を末尾へ繋げる（zi-f。行頭の Backspace）。
    ///
    /// **次の行が無ければ何もしない。** **H-b-2 で 1 行の上限が消えたので、
    /// 長さで断ることは無くなった**——**繋げると本文は 1 バイト縮むので、
    /// ヒープも伸ばさなくてよい。**
    ///
    /// # `zi` の締めは「行の連結」を実装しないと書いていた
    ///
    /// **書いたのは `zi-d` の範囲としてである。** **zi-f で作ることにした**
    /// ——**行頭の Backspace が何もしないと、打ち間違いを直せない場面が
    /// 残る**（1 行目まで戻って消すしかない）。**運用者の指摘が利用者である。**
    fn join_with_next(&mut self, row: usize) -> bool {
        if row + 1 >= self.count {
            return false;
        }
        let (used, count) = (self.used, self.count);
        let (text, starts) = self.parts_mut();
        // **行 `row` を終えている改行を 1 つ抜くだけである。**
        let position = starts[row + 1] - 1;
        text.copy_within(position + 1..used, position);
        // **索引から境界を 1 つ抜く。** 前から順に 1 つずつ前へ詰める
        // （読む側が先で書く側が後なので、元の値を読める）。
        for index in row + 1..count {
            starts[index] = starts[index + 1] - 1;
        }
        self.used = used - 1;
        self.count = count - 1;
        true
    }

    /// 1 バイト消す。**行末では何もしない**（`x` は行を繋げない）。
    fn remove(&mut self, row: usize, at: usize) -> bool {
        if at >= self.length(row) {
            return false;
        }
        let used = self.used;
        let position = self.start_of(row) + at;
        let text = self.text_mut();
        text.copy_within(position + 1..used, position);
        self.used = used - 1;
        self.shift_index(row, -1);
        true
    }
}

/// 読み込んだ本文へ索引を張る（H-b-1。H-b-2 で上限が消えた）。
///
/// # 中身は既に本体の中に在る
///
/// **`total` は本文の先頭から読み込んだバイト数である。**
/// **どこへも写さない**——**受け皿がそのまま本体である**ので、
/// **ここでやるのは改行を数えて索引を埋めることだけである。**
///
/// # 末尾の改行を補う
///
/// **本体は「各行の後ろに改行が 1 つ」という形を常に保つ**（[`Buffer`]）。
/// **改行で終わっていないファイルは、最後に 1 つ足す。** **空のファイルは
/// 改行 1 つ（＝空行が 1 行）になる。**
///
/// # 偽を返すのは、ヒープが伸びなかったときだけである（H-b-2）
///
/// **b-1 までは行数と行の長さの上限で断っていた。** **その上限は消えた。**
fn index_lines(buffer: &mut Buffer, total: usize) -> bool {
    // **改行で終わる形へ揃える。**
    let used = if total == 0 {
        if !buffer.ensure_text(1) {
            return false;
        }
        buffer.text_mut()[0] = b'\n';
        1
    } else if buffer.text()[total - 1] == b'\n' {
        total
    } else {
        if !buffer.ensure_text(total + 1) {
            return false;
        }
        buffer.text_mut()[total] = b'\n';
        total + 1
    };

    // **行を数えてから索引の枠を確かめる**（H-b-2）。**数えるほうが先である**
    // ——**読む前には行数が分からない**（`stat` が答えるのは大きさだけである）。
    let lines = buffer.text()[..used]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count();
    // **番兵の分 + 続きを打つ余地。**
    //
    // 破壊 (H-b-2, zi-skip-grow): 索引を伸ばさない。**初期の枠に入る行までで
    // 切り詰める**——**b-1 までの上限（64 行）へ戻った形である。**
    // **`zi` は何も言わずに開き、保存すると入らなかった行が消える。**
    // **落ちるのは「編集の前後で `cat` が同じ中身を出す」判定だけである**
    // ——**開く前の `cat` は像を読むので、そちらは変わらない。**
    #[cfg(not(zi_skip_grow))]
    if !buffer.ensure_index(lines + 1 + INDEX_STEP) {
        return false;
    }

    let mut count = 0usize;
    {
        let (text, starts) = buffer.parts_mut();
        starts[0] = 0;
        for position in 0..used {
            if text[position] != b'\n' {
                continue;
            }
            // **枠に入らない行は捨てる。** **既定の構成では上の
            // `ensure_index` が枠を確かめているので、ここは通らない。**
            if count + 1 >= starts.len() {
                break;
            }
            count += 1;
            starts[count] = position + 1;
        }
    }
    // **`used` は番兵から取る。** **既定の構成では読み込んだ量と等しい**
    // （最後の改行が番兵を据えるため）。**枠が足りずに切り詰めたときだけ、
    // 入った行の終わりまで縮む**——**索引と本文が食い違ったまま残らない。**
    buffer.count = count;
    buffer.used = buffer.parts().1[count];
    true
}

/// 10 進の数を桁で書き出す（`u32` まで）。**`userlib` に整数の出力は無い。**
fn write_number(out: &mut [u8; 12], value: usize) -> usize {
    if value == 0 {
        out[0] = b'0';
        return 1;
    }
    let mut digits = [0u8; 12];
    let mut count = 0usize;
    let mut left = value;
    while left > 0 {
        digits[count] = b'0' + (left % 10) as u8;
        left /= 10;
        count += 1;
    }
    for index in 0..count {
        out[index] = digits[count - 1 - index];
    }
    count
}

/// カーソルを 1 起点の CUP で動かす。
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
    userlib::frame_push(STDOUT, &sequence[..at]);
}

/// 画面の形と、開いているファイル（e-4）。**起動時に決まり、以後変わらない。**
struct View<'a> {
    /// 画面の行数。**`ioctl(TIOCGWINSZ)` が答えた値である**（0 なら既定へ落ちる。
    /// `userlib::window_size_or_default`）。
    rows: usize,
    /// 本文の行数。**破壊（`zi-status-below-text`）だけが使う**——
    /// **訊いた行数を使わない形が、どこへ置くかを決めるために要る。**
    #[cfg_attr(not(zi_status_below_text), allow(dead_code))]
    text_lines: usize,
    /// 開いているファイルのパス。**状態行に出す。**
    path: &'a [u8],
}

impl View<'_> {
    /// 状態行の行（下から 2 行目。e-4）。
    fn status_row(&self) -> usize {
        // 破壊 (e-4, zi-status-below-text): 訊いた行数を使わず、本文の 1 行下へ置く
        // （ES-d までの形）。**画面の下端に在ることの判定だけが落ちる**——
        // **色も札も中身も変わらない。** **`ioctl` が答えた値を実際に使って
        // いることの主張が、これで初めて偽になる。**
        #[cfg(zi_status_below_text)]
        {
            return self.text_lines + 1;
        }
        #[cfg(not(zi_status_below_text))]
        {
            self.rows - 2
        }
    }

    /// コマンド行の行（最下行。e-4）。
    fn command_row(&self) -> usize {
        self.rows - 1
    }

    /// 本文に使える行数（e-4）。
    ///
    /// **`rows - 2` である。** **0 や負にならない**——`rows` は
    /// `window_size_or_default` を通っており、**0 なら既定の 24 へ落ちる**
    /// （`userlib::WindowSize::or_default`）。**24 でも 22 行が残る。**
    fn text_rows(&self) -> usize {
        self.rows - 2
    }
}

/// いま画面へ出す状態（e-4）。**移ろう側をまとめてある。**
struct Status<'a> {
    mode: Mode,
    row: usize,
    col: usize,
    /// 保存していない変更があるか。
    dirty: bool,
    /// コマンド行に出す語（`:` を除く）。**コマンド中でなければ空である。**
    command: &'a [u8],
    in_command: bool,
    /// コマンド行に出す報せ（e-5）。**コマンド中でないときに出る。**
    ///
    /// **使う人へのものである**——断った理由、知らないコマンド、
    /// 行が一杯であること。**空なら何も出さない。**
    message: &'a [u8],
}

/// 状態行を描く（ES-d。e-4 で下から 2 行目へ移し、中身を増やした）。
///
/// # 画面の下端に置く
///
/// **e-1 で `ioctl(TIOCGWINSZ)` が入るまで、行数を知る道が無かった**ので、
/// 本文の 1 行下に置いていた。**いまは訊ける**ので、vi と同じ下端へ置く。
///
/// # 色が付くのはモードの札だけである
///
/// **ファイル名・位置・変更の印は既定の色で出す。** **判定は色の付いた
/// 連なりを読む**（`kernel/src/console/probe.rs`）ので、**位置のような
/// 動く値を色の中へ入れると、札の並びを見る判定が揺れる。**
///
/// # カーソルは戻さない（e-3）
///
/// **戻すのは [`restore_cursor`] だけである。** **描く関数が各自で戻す形は、
/// 描く場所が増えるたびに書き忘れが画面の誤りになる**（e-2 で順序依存が出た）。
/// **この関数を呼ぶ側は [`refresh`] を通すこと。**
///
/// # 組み立てはスタックの固定配列のままである（H-b-1）
///
/// **ヒープへ移さない。理由は 3 つある。**
///
/// **(1) これは入れ物ではなく、1 回書くための一時の器である。**
/// **`write` を 1 回で済ませるために在る**（色の無い札を一瞬でも出さないため）。
/// **描き終われば用が無い。**
///
/// **(2) 長さが編集する中身に依らない。** **画面の 1 行に収まる量で決まって
/// おり、ファイルが大きくなっても変わらない。** **ヒープの上限が外れても、
/// ここが足りなくなることはない。**
///
/// **(3) あふれる形は既に切り詰めで守ってある**——**ファイル名は
/// `out.len() - at - 32` で切る。** **守りが在るものを動かす理由が無い。**
///
/// **同じ判断が [`draw_command_line`] にも当てはまる。**
fn draw_status(view: &View, status: &Status) {
    let mode = status.mode;
    // 破壊 (ES-d, zi-status-freeze-mode): モードが変わっても NORMAL のまま描く。
    // **色も位置も長さも変わらない**ので、「状態行が自分の色で描かれている」
    // 判定は緑のままである。**落ちるのは「モードに従って変わる」判定だけ**で、
    // **その形でしか落ちない**（`docs/verification-coverage.md` の破壊を足す基準）。
    #[cfg(zi_status_freeze_mode)]
    let mode = {
        let _ = mode;
        Mode::Normal
    };
    let label = match mode {
        Mode::Normal => STATUS_NORMAL,
        Mode::Insert => STATUS_INSERT,
        Mode::Command => STATUS_COMMAND,
    };
    move_cursor(view.status_row(), 0);
    // EL(2): 前の中身を消してから置く（位置の桁数が減ったときに残さない）。
    userlib::frame_push(STDOUT, b"\x1b[2K");
    // **1 回で書く**（`zash` の `write_prompt` と同じ理由。色の無い札を
    // 一瞬でも出さない）。
    let mut out = [0u8; 160];
    let mut at = 0usize;
    for part in [STATUS_COLOR, label, SGR_RESET, b" "] {
        out[at..at + part.len()].copy_from_slice(part);
        at += part.len();
    }
    // **ファイル名。** NUL 終端の手前まで。
    let name = {
        let end = view
            .path
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(view.path.len());
        &view.path[..end]
    };
    let take = name.len().min(out.len() - at - 32);
    out[at..at + take].copy_from_slice(&name[..take]);
    at += take;
    // **位置は 1 起点で出す**（vi と同じ。使う人が見る数である）。
    out[at] = b' ';
    at += 1;
    let mut digits = [0u8; 12];
    let count = write_number(&mut digits, status.row + 1);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at] = b':';
    at += 1;
    let count = write_number(&mut digits, status.col + 1);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    // **保存していない変更の印。** vi の `[+]` と同じ形である。
    if status.dirty {
        let mark = b" [+]";
        out[at..at + mark.len()].copy_from_slice(mark);
        at += mark.len();
    }
    userlib::frame_push(STDOUT, &out[..at]);
}

/// コマンド行（エコーエリア）を描く（e-4）。**最下行である。**
///
/// # 打っている途中が見える
///
/// **`:` を打った時点で `:` が出て、`w` を打てば `:w` になる。**
/// **打ち終わるまで何も見えない形は、打ち間違いに気づけない**
/// （運用者の指摘）。
///
/// # コマンド中でなければ空にする
///
/// # 報せの出し先である（e-5）
///
/// **`:q` を拒んだ理由、知らないコマンド、行が一杯であること**を、ここへ出す。
/// **`STDERR` へ出していたものを移した**——**`zi` は代替画面に居るので、
/// `STDERR` は「使う人が見る画面」ではない**（診断は検査の構成でしか出ない）。
/// **e-4 で作った口に、利用者がここで来た。**
///
/// # 組み立てはスタックの固定配列のままである（H-b-1）
///
/// **理由は [`draw_status`] にある。** **長さは [`COMMAND_MAX`] で決まって
/// おり、編集する中身に依らない。**
fn draw_command_line(view: &View, status: &Status) {
    move_cursor(view.command_row(), 0);
    userlib::frame_push(STDOUT, b"\x1b[2K");
    if !status.in_command {
        // **報せを出す（e-5）。** **コマンド中はそちらが優先である**
        // ——打っている途中を消さない。
        if !status.message.is_empty() {
            userlib::frame_push(STDOUT, status.message);
        }
        return;
    }
    let mut out = [0u8; COMMAND_MAX + 1];
    out[0] = b':';
    let take = status.command.len().min(COMMAND_MAX);
    out[1..1 + take].copy_from_slice(&status.command[..take]);
    userlib::frame_push(STDOUT, &out[..1 + take]);
}

/// 代替画面バッファへ入る（e-3。`?1049h`）。
///
/// # なぜ要るのか
///
/// **全画面のアプリは、抜けた後に元の画面を返すべきである。**
/// **`zi` が終わった後、編集していた本文が残り、シェルが `zi` のカーソル位置
/// から続いていた**（運用者の目視。ES 段の締めの限界の節）。
///
/// **戻す仕事はカーネル側にある**（ADR-0040 の Addendum）——
/// **Ring 3 には画面を読み戻す手段が無い。** `zi` は入る / 出るを告げるだけである。
fn enter_screen() {
    userlib::frame_push(STDOUT, b"\x1b[?1049h");
}

/// 代替画面バッファから出る（e-3。`?1049l`）。**終わるすべての道で呼ぶ。**
///
/// **入った後に終わる道は 3 つある**——`:q` / `:wq` の `exit(0)`、
/// `:w` の失敗の `exit(5)`、端末が読めなくなったときの `break` である。
/// **入る前に終わる道（引数が無い・開けない・読めない・大きすぎる）では
/// 呼ばない**——**まだ入っていないので、戻す面が無い。**
fn leave_screen() {
    userlib::frame_push(STDOUT, b"\x1b[?1049l");
    // **ここは描き終わりではない。** **この後は終わるだけで、送る機会が
    // もう無い**——**溜めたままにすると、代替画面から戻らない。**
    userlib::frame_flush(STDOUT);
}

/// 描き終わりにカーソルを編集位置へ戻す（e-3）。
///
/// # 責務を1つにした
///
/// **描く場所が増えるたびに「最後にカーソルを戻す」を書き足す形は、
/// 書き忘れがそのまま画面の誤りになる。** **実際に e-2 で順序依存が出た**
/// ——状態行を後から描くと、カーソルが状態行の隣に残る。
///
/// **描く関数はカーソルを戻さない。** **戻すのはここだけである。**
/// **e-4 で下から2行目と最下行の2本になっても、増えるのはこの関数の
/// 中身だけで済む。**
fn restore_cursor(window: &Window, cursor_row: usize, cursor_col: usize) {
    // **窓の中の位置へ直す（VIEW-a）。** **窓の外なら動かさない**
    // ——**呼ぶ前に [`follow_window`] を通していれば、その形にはならない。**
    if let Some(screen) = window.screen_row(cursor_row) {
        move_cursor(screen, cursor_col);
    }
    // **ここで 1 回だけ送る（PERF-b。回帰で位置を移した）。**
    //
    // **描き終わりは [`refresh`] だけではない。** **ノーマルモードの移動は
    // [`show_cursor`] からここへ直に来る**（`refresh` を通らない）。
    // **送る場所を `refresh` に置いていたので、その経路だけが送られず、
    // 画面のカーソルが追従しなくなった**（実測。**運用者の目視で出た**）。
    //
    // **この関数はすべての描き終わりの最後に居る**——**`refresh` も
    // `show_cursor` も、最後にカーソルを戻す。** **送る場所はここが正しい。**
    //
    // 破壊 (PERF-b の回帰, zi-skip-cursor-flush): ここで送らない。
    // **回帰そのものへ戻す**——**ノーマルモードの移動が画面へ届かず、
    // 画面のカーソルが古い位置に残る。** **バッファは正しいので、
    // 内部状態を見る判定は1つも落ちない。**
    #[cfg(not(zi_skip_cursor_flush))]
    userlib::frame_flush(STDOUT);
}

/// 窓を現在行へ追わせる（VIEW-a）。**動いたら真。**
///
/// **破壊の枝をここ 1 か所に置くために、包んである。**
fn follow_window(window: &mut Window, row: usize) -> bool {
    // 破壊 (VIEW-a, zi-window-frozen): 窓を動かさない。
    // **b-2 までの振る舞いに戻る**——**窓は先頭に据え置かれ、カーソルが
    // 下へ出ても画面は先頭の 48 行のままである。** **バッファは正しいので、
    // 内部状態を見る判定は 1 つも落ちない**——**落ちるのは画面を読む
    // 「窓が動いた」判定だけである。**
    #[cfg(zi_window_frozen)]
    {
        let _ = row;
        let _ = window;
        false
    }
    #[cfg(not(zi_window_frozen))]
    {
        window.follow(row)
    }
}

/// カーソルを動かした後の画面（VIEW-a）。
///
/// **窓が動いたら描き直し、動かなければカーソルだけ戻す。**
/// **描き直しは全面である**——**差分で描く形は測ってから決める**
/// （`docs/roadmap.md` の VIEW 段）。
fn show_cursor(view: &View, buffer: &Buffer, window: &mut Window, status: &Status) {
    let before = window.top();
    if !follow_window(window, status.row) {
        restore_cursor(window, status.row, status.col);
        return;
    }
    // **窓が 1 行だけ動いたなら、画面をずらす（PERF-e）。**
    //
    // **`less` と同じ機構である**（`common::ansi` の `IL` / `DL`）。
    // **2 つ目の経路を作らない。**
    //
    // **1 行より大きく跳んだときは描き直す**——**ずらす量が画面に近づくと、
    // ずらしても得るものが無い**（`less` の 1 画面の移動と同じ判断）。
    let moved = window.top() as isize - before as isize;
    if moved == 1 || moved == -1 {
        scroll_by_one(view, buffer, window, moved == 1);
        // **最後にカーソルを戻す**（[`refresh`] が持つ順序）。
        // **PERF-b の回帰は、この順序を崩したときに出た。**
        refresh(view, window, status);
        return;
    }
    redraw(view, buffer, window, status);
}

/// 窓が 1 行動いたぶんだけ画面をずらす（PERF-e）。
///
/// # 本文だけをずらす
///
/// **`zi` は最下の 2 行を状態行とコマンド行に使っている**（`less` は 1 本）。
/// **`DL` / `IL` は画面全体をずらすので、その 2 本も 1 行ぶん動く**——
/// **この後の [`refresh`] が両方を描き直すので、最後の絵は正しい。**
///
/// # 新しく現れた 1 行だけを描く
///
/// **窓が 1 行動くと、本文の行はすべて別の行を映す**ので、
/// **「変わった行だけ描く」では 1 字も減らない**（実測。PERF-c）。
/// **画面をずらせば、描き直すのは 1 行だけになる。**
fn scroll_by_one(view: &View, buffer: &Buffer, window: &Window, down: bool) {
    // 破壊 (PERF-e, zi-redraw-whole-screen): ずらさずに全部描き直す。
    // **PERF-e の前の形そのものである**——**出る絵は同じで、描く字が
    // 桁で増える。** **描く字の数の判定が捕まえる。**
    #[cfg(zi_redraw_whole_screen)]
    {
        let _ = down;
        let mut copy = *window;
        let top = copy.top();
        redraw(
            view,
            buffer,
            &mut copy,
            &Status {
                mode: Mode::Normal,
                row: top,
                col: 0,
                dirty: true,
                command: &[],
                in_command: false,
                message: b"",
            },
        );
        return;
    }
    #[cfg(not(zi_redraw_whole_screen))]
    {
        let visible = window.visible(buffer.count);
        let last = view.text_rows().saturating_sub(1);
        let (sequence, screen_row, line) = if down {
            (&b"\x1b[M"[..], last, visible.start + last)
        } else {
            (&b"\x1b[L"[..], 0, visible.start)
        };
        move_cursor(0, 0);
        userlib::frame_push(STDOUT, sequence);
        move_cursor(screen_row, 0);
        // EL(2): 現れた行を消してから置く（ずらした先に前の字が残らない）。
        userlib::frame_push(STDOUT, b"\x1b[2K");
        if line < visible.end {
            let text = buffer.line(line);
            if !text.is_empty() {
                userlib::frame_push(STDOUT, text);
            }
        }
    }
}

/// 2 本（状態行とコマンド行）を描き、最後にカーソルを戻す（e-3。e-4 で 2 本になった）。
///
/// **画面を更新する入口である。** **順序はここが持つ**ので、呼ぶ側は考えない。
fn refresh(view: &View, window: &Window, status: &Status) {
    draw_status(view, status);
    draw_command_line(view, status);
    restore_cursor(window, status.row, status.col);
}

/// 画面を描き直す。**全面を消してから行ごとに置く。**
///
/// **消してから描くので、前の内容が残らない。** 1 行ずつ CUP で置くのは、
/// 行の折り返しに依らず「バッファの行 = 画面の行」を保つためである。
///
/// **状態行もここで描き直す（ES-d）**——`ED(2)` が消してしまうためである。
fn redraw(view: &View, buffer: &Buffer, window: &mut Window, status: &Status) {
    // **描く前に窓を追わせる（VIEW-a）。** **描く範囲がここで決まる。**
    follow_window(window, status.row);
    // ED(2): 画面全体を消す。**カーソルは動かない**ので、この後に CUP を出す。
    userlib::frame_push(STDOUT, b"\x1b[2J");
    // **本文に使える行までしか描かない（e-4）。**
    //
    // **スクロールは作らない。** **b-1 まではそれで足りていた**——
    // **64 行を越えるファイルは開かずに拒んでいたので、画面が 66 行以上
    // あれば全部入った。**
    //
    // **H-b-2 で上限が消えたので、いまは入りきらないファイルが開く。**
    // **画面に出るのは先頭の `text_rows()` 行だけで、それより下の行は
    // 見えないまま編集される**（バッファは正しく、画面が足りない）。
    // **限界として `docs/roadmap.md` に書いた。** **スクロールは別の段である。**
    let visible = window.visible(buffer.count);
    for row in visible.clone() {
        move_cursor(row - visible.start, 0);
        // EL(2): その行を消してから置く（消し残しを作らない）。
        userlib::frame_push(STDOUT, b"\x1b[2K");
        let line = buffer.line(row);
        if !line.is_empty() {
            userlib::frame_push(STDOUT, line);
        }
    }
    refresh(view, window, status);
}

/// 判定行を出す。**内部状態であって画面ではない**（モジュール doc の限界）。
///
/// # 出口はログである。画面ではない（ADR-0046）
///
/// **[`userlib::log_line`] を通す。** **シリアルへ出て、画面へは出ない。**
///
/// **`STDERR` へ出していた。** **`write` はシリアルと前景コンソールの両方へ
/// 届く**ので、**診断行が画面にも描かれ、カーソルの居る行の本文を上書き
/// した**——**実測で、本文の4行すべてが判定行に化けていた。**
/// **応急は「検査の構成でだけ出す」で、根は「出口が1つしかない」ことだった。**
///
/// **ADR-0046 で出口を割った。** **使う人へのエラーは `STDERR` のままで、
/// カーネルが溜め、アプリがエコーエリアへ描く。** **診断はこちらで、
/// 読み手はホスト側の判定である。**
///
/// # それでも検査の構成でしか出さない
///
/// **画面は壊れなくなったが、通常の起動のシリアルに判定行を混ぜる理由は無い。**
/// 台本で駆動する構成（kernel の `zi-test` feature が `zi_diagnostics` として
/// 届く）でだけ出す。**判定はその構成で走るので、主張は保たれる。**
#[cfg(zi_diagnostics)]
fn report_cursor(buffer: &Buffer, window: &Window, row: usize, col: usize, tag: &[u8]) {
    let mut out = [0u8; 96];
    let mut at = 0usize;
    let head = b"zi: cursor (buffer state, not the screen) row=";
    out[at..at + head.len()].copy_from_slice(head);
    at += head.len();
    let mut digits = [0u8; 12];
    let count = write_number(&mut digits, row);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at..at + 5].copy_from_slice(b" col=");
    at += 5;
    let count = write_number(&mut digits, col);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at..at + 7].copy_from_slice(b" lines=");
    at += 7;
    let count = write_number(&mut digits, buffer.count);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    // **窓の位置（VIEW-a）。** **状態行へは出さない**（判定が状態行の並びを
    // 読んでおり、増やすと揺れる。運用者の判断）。**診断側に出す。**
    out[at..at + 5].copy_from_slice(b" top=");
    at += 5;
    let count = write_number(&mut digits, window.top());
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at] = b' ';
    at += 1;
    let take = tag.len().min(out.len() - at - 1);
    out[at..at + take].copy_from_slice(&tag[..take]);
    at += take;
    out[at] = b'\n';
    at += 1;
    userlib::log_line(userlib::STDERR, &out[..at]);
}

/// 判定行を出さない側（既定のビルド。上の doc を参照）。
#[cfg(not(zi_diagnostics))]
fn report_cursor(_buffer: &Buffer, _window: &Window, _row: usize, _col: usize, _tag: &[u8]) {}

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
    let length = unsafe { length_of(pointer, PATH_MAX - 1) };
    for index in 0..length {
        // SAFETY: 上で数えた長さの範囲である。
        path[index] = unsafe { *pointer.add(index) };
    }
    path[length] = 0;

    // === 読み込み。**読み切って閉じる**（zi-c の契約。モジュール doc） ===
    //
    // **無いパスは「新しいファイル」である（e-5）。** **空のバッファで始め、
    // `:w` が `O_CREAT` で作る**（vi と同じ形）。**開けない理由が「無い」以外
    // なら、従来どおり断って終わる**——**権限も何も無いこの体制では、
    // ここへ来るのは像の側の失敗である。**
    // === 入れ物をヒープから取る（H-b-1。容量は H-b-2 で動くようになった） ===
    //
    // **開く前に大きさを訊く。** **`stat` が答えるのはファイルの大きさだけで、
    // 行数は答えない**ので、**行の数は読んでから数える**（[`index_lines`]）。
    //
    // **訊けなければ 0 として始める。** **足りなければ読みながら伸ばす**ので、
    // **訊けたかどうかは速さの話であって、開けるかどうかの話ではない。**
    //
    // SAFETY: `path` は上で NUL 終端にした。
    let size = unsafe { userlib::size_of_file(&path[..length + 1]) }.unwrap_or(0) as usize;
    let text_cap = text_capacity_for(size);
    let index_cap = INDEX_INITIAL;
    let Some(region) = userlib::heap::reserve(text_cap + index_cap * userlib::heap::WORD) else {
        write_all(STDERR, NO_HEAP);
        exit(6);
    };
    let Some(mut editor) = Buffer::new(region, text_cap, index_cap) else {
        write_all(STDERR, NO_HEAP);
        exit(6);
    };
    let buffer: &mut Buffer = &mut editor;

    let fd = open_read_only(&path[..length + 1]);
    let new_file = fd == userlib::MINUS_ENOENT;
    if fd < 0 && !new_file {
        write_all(STDERR, OPEN_FAILED);
        release_and_exit(buffer, 1);
    }
    let fd = if new_file { 0 } else { fd as u64 };

    let mut total = 0usize;
    let mut chunk = [0u8; CHUNK];
    let mut overflowed = false;
    // **新しいファイルは読まない（e-5）。** **fd を開いていない。**
    while !new_file {
        let got = read(fd, &mut chunk);
        if got < 0 {
            close(fd);
            write_all(STDERR, READ_FAILED);
            release_and_exit(buffer, 3);
        }
        if got == 0 {
            break;
        }
        let got = got as usize;
        // **足りなければ伸ばす（H-b-2）。** **`+ 1` は末尾の改行を補う分である**
        // ——**改行で終わらないファイルでも、この後で 1 バイト足せる。**
        //
        // **ここが伸びる場面は実測では出ない**——**容量は `stat` の答えから
        // 決めており、読む量はそれと同じである。** **`stat` が答えなかったとき
        // （0 として始めたとき）の受けである。**
        if !buffer.ensure_text(total + got + 1) {
            overflowed = true;
            break;
        }
        buffer.text_mut()[total..total + got].copy_from_slice(&chunk[..got]);
        total += got;
    }
    // **新しいファイルでは開いていないので閉じない（e-5）。**
    if !new_file {
        close(fd);
    }

    // **入れ物を用意できなかったら開かずに拒む。** 切り詰めて保存すると、
    // 開いた時点で中身が消える——**それは編集ではなく破壊である。**
    if overflowed || !index_lines(buffer, total) {
        write_all(STDERR, OUT_OF_MEMORY_READING);
        release_and_exit(buffer, 4);
    }

    // **検査の構成でしか出さない**（[`report_cursor`] と同じ理由。
    // **これも診断であって、使う人に要る行ではない**）。
    #[cfg(zi_diagnostics)]
    userlib::log_line(STDERR, b"zi: ready\n");

    // **端末の大きさを訊く（e-1）。** **使うのは e-4 の2本立てである**——
    // **いまは受け取って判定行に出すだけで、置き場所には使っていない**
    // （状態行は本文の1行下のままである）。
    // **訊く経路が本物の利用者を持たないと、検算が置けない。**
    report_window_size(userlib::window_size(0));
    // **使う値は既定へ落とした側である（e-4）。** **判定は落とす前を見る**
    // （上の行）——落とした後を見ると、訊けた場合と落ちた場合が同じ値になる。
    let screen = userlib::window_size_or_default(0);

    // **代替画面バッファへ入る（e-3）。** **ここから先の描画は代替の面に載り、
    // 出るときに元の画面が戻る。** **読み込みが済んで、確実に編集へ入る時点で
    // 入る**——**入る前に終わる道では、戻す面が無い。**
    enter_screen();

    let mut mode = Mode::Normal;
    let mut row = 0usize;
    let mut col = 0usize;
    let mut escape = Escape::Idle;
    // **`:` の後に溜める語と、変更があったか（zi-d-2）。**
    let mut command = [0u8; COMMAND_MAX];
    let mut command_len = 0usize;
    let mut dirty = false;
    // **コマンド行に出す報せ（e-5）。** **次の打鍵まで残す**——vi と同じで、
    // **出した瞬間に消えると読めない。**
    let mut message: &'static [u8] = b"";
    // **画面の形を訊く（e-4）。** **0 なら既定へ落ちる**
    // （`userlib::window_size_or_default`。**その形がここで初めて本番で効く**）。
    let view = View {
        rows: screen.rows as usize,
        text_lines: buffer.count,
        path: &path[..length + 1],
    };
    // **見えている窓（VIEW-a。ADR-0045）。** **高さは本文に使える行数である。**
    let mut window = Window::new(view.text_rows());
    redraw(
        &view,
        buffer,
        &mut window,
        &Status {
            mode,
            row,
            col,
            dirty,
            command: &command[..command_len],
            in_command: false,
            message,
        },
    );
    report_cursor(buffer, &window, row, col, b"start");
    // **いま状態行に出ている札のモード（ES-d）。**
    let mut shown_mode = mode;
    // **カーネルが溜めたエラーの控え（ADR-0046）。**
    //
    // **代替画面に居る間、`STDERR` へ出したものは画面へ描かれない。**
    // **取り出して自分のエコーエリアへ描くのがアプリの仕事である。**
    let mut echo = userlib::Echo::new();

    loop {
        // **モードが変わっていたら状態行を描き直す（ES-d）。**
        //
        // **読む直前に見る。** モードを変える場所は 4 つある（`i`・Esc・`:`・
        // コマンドの実行）が、**そのどれもが最後にここへ戻る**ので、
        // **`continue` が何本あっても漏れない。**
        // **`-EAGAIN` で回っている間は変わらない**ので、何度も描かない。
        if mode != shown_mode {
            refresh(
                &view,
                &window,
                &Status {
                    mode,
                    row,
                    col,
                    dirty,
                    command: &command[..command_len],
                    in_command: mode == Mode::Command,
                    message,
                },
            );
            shown_mode = mode;
        }
        // **カーネルが溜めたエラーを取り出す（ADR-0046）。**
        //
        // **読む直前に見る。** モードの札と同じ理由で、**`continue` が
        // 何本あってもここへ戻る。**
        //
        // **出たらその場で描く。** **次の打鍵まで残る**——`vi` と同じで、
        // **出した瞬間に消えると読めない**（[`Status::message`] の doc）。
        if echo.take(STDERR) {
            refresh(
                &view,
                &window,
                &Status {
                    mode,
                    row,
                    col,
                    dirty,
                    command: &command[..command_len],
                    in_command: mode == Mode::Command,
                    message: echo.line(),
                },
            );
        }
        let mut byte = [0u8; 1];
        let got = read(0, &mut byte);
        if got == MINUS_EAGAIN {
            // **溜めた Esc をここで確定する（e-2）。** **入力が途切れたので、
            // CSI の途中ではありえない**（[`finish_pending_escape`] の doc）。
            //
            // 破壊 (e-2, zi-esc-needs-second-key): ここで確定しない。
            // **溜めた Esc は次の 1 バイトが来るまで残る**ので、
            // **使う人は Esc を 2 回押すことになる**（e-2 で直した当の形である）。
            // **落ちるのは「Esc 1 回で戻る」判定だけである**——台本の残りは
            // 次のバイトで確定するので、往復も本数も変わらない。
            #[cfg(not(zi_esc_needs_second_key))]
            if escape == Escape::Esc {
                escape = Escape::Idle;
                finish_pending_escape(
                    &view,
                    buffer,
                    &mut window,
                    row,
                    &mut col,
                    &mut mode,
                    &mut shown_mode,
                    dirty,
                );
            }
            // **溜まっていない。** 回して待つ（`zash` と同じ形）。
            continue;
        }
        if got <= 0 {
            // 端末が読めない。**この段では終わる**（`:q` は zi-d-2）。
            break;
        }
        let byte = byte[0];
        // **打鍵が来たら控えを空にする（ADR-0046）。** **次の描き直しで
        // 消える**——`vi` と同じで、報せは次の打鍵まで残る。
        echo.clear();

        // **3 バイトの状態機械を先に通す**（`zash` と同じ形）。
        match (escape, byte) {
            (Escape::Idle, ESC) => {
                escape = Escape::Esc;
                continue;
            }
            (Escape::Esc, b'[') => {
                escape = Escape::Bracket;
                continue;
            }
            // **`\x1b[3` は Delete の途中である（zi-f）。**
            (Escape::Bracket, b'3') => {
                escape = Escape::Tilde;
                continue;
            }
            (Escape::Tilde, terminator) => {
                escape = Escape::Idle;
                if terminator != b'~' {
                    // **知らない終端は捨てる。** 字として入れない。
                    continue;
                }
                // **Delete はカーソル位置の字を消す（zi-f）。**
                // **ノーマルの `x` と同じ動きだが、インサートでも効く。**
                let removed = buffer.remove(row, col);
                if removed {
                    // **行末を越えたら 1 つ左へ寄る**（ノーマルのみ。`x` と同じ）。
                    if mode == Mode::Normal {
                        let length = buffer.length(row);
                        if col >= length {
                            col = length.saturating_sub(1);
                        }
                    }
                    // **行の中の削除は 1 行だけが変わる（PERF-g）。**
                    redraw_line_here(&view, buffer, &window, mode, row, col, b"");
                    report_cursor(buffer, &window, row, col, b"delete");
                    dirty = true;
                }
                continue;
            }
            (Escape::Bracket, direction) => {
                escape = Escape::Idle;
                let moved = match direction {
                    // 破壊 (zi-d-1, zi-cursor-ignore-updown): 上下を捨てる。
                    // **カーソルが行を移らないので、編集が別の行に入る**——
                    // 台本の判定（row の推移）が捕まえる。
                    #[cfg(not(zi_cursor_ignore_updown))]
                    b'A' => move_up(buffer, &mut row, &mut col),
                    #[cfg(not(zi_cursor_ignore_updown))]
                    b'B' => move_down(buffer, &mut row, &mut col),
                    b'C' => move_right(buffer, row, &mut col, mode),
                    b'D' => move_left(&mut col),
                    // 知らない終端は捨てる。**字として入れない。**
                    _ => false,
                };
                if moved {
                    // **上下の矢印は行を移る（VIEW-a）。** **窓の外へ出たら
                    // 窓が動き、そのときは描き直しになる。**
                    show_cursor(
                        &view,
                        buffer,
                        &mut window,
                        &Status {
                            mode,
                            row,
                            col,
                            dirty,
                            command: &[],
                            in_command: false,
                            message,
                        },
                    );
                    report_cursor(buffer, &window, row, col, b"arrow");
                }
                continue;
            }
            (Escape::Esc, other) => {
                // **`[` が続かなかった。Esc 単体である**（確定。上の doc）。
                // **`-EAGAIN` の側と同じ確定を通る（e-2）。**
                escape = Escape::Idle;
                finish_pending_escape(
                    &view,
                    buffer,
                    &mut window,
                    row,
                    &mut col,
                    &mut mode,
                    &mut shown_mode,
                    dirty,
                );
                // **溜めた Esc の次の字は、この周で扱い直す。**
                if other == ESC {
                    escape = Escape::Esc;
                    continue;
                }
                if !handle_byte(
                    &view,
                    other,
                    buffer,
                    &mut window,
                    &mut row,
                    &mut col,
                    &mut mode,
                    dirty,
                    &mut message,
                ) {
                    continue;
                }
                continue;
            }
            (Escape::Idle, _) => {}
        }

        // **コマンド行は改行まで溜める（zi-d-2）。**
        if mode == Mode::Command {
            match byte {
                b'\n' => {
                    let (outcome, said) = run_command(
                        &command[..command_len],
                        &path[..length + 1],
                        buffer,
                        dirty,
                    );
                    message = said;
                    command_len = 0;
                    mode = Mode::Normal;
                    match outcome {
                        Command::Quit => {
                            leave_screen();
                            release_and_exit(buffer, 0)
                        }
                        // **保存に失敗しても抜けない（ADR-0046）。**
                        //
                        // **抜けると、編集した中身がそのまま失われる。**
                        // **`vi` も留まる**——`:w` が失敗しても編集は続く。
                        //
                        // **理由は次の周でエコーエリアに出る**——`save` が
                        // `STDERR` へ出したものをカーネルが溜め、
                        // [`userlib::Echo`] が取り出して最下行へ描く。
                        Command::Failed => {}
                        // **保存したら変更は無い。**
                        Command::Saved => dirty = false,
                        Command::Refused => {}
                    }
                    redraw(
                        &view,
                        buffer,
                        &mut window,
                        &Status {
                            mode,
                            row,
                            col,
                            dirty,
                            command: &command[..command_len],
                            in_command: false,
                            message,
                        },
                    );
                    report_cursor(buffer, &window, row, col, b"command");
                }
                ESC => {
                    // **打ちかけを捨てる。** ノーマルへ戻る。
                    command_len = 0;
                    mode = Mode::Normal;
                }
                other => {
                    if command_len < command.len() {
                        command[command_len] = other;
                        command_len += 1;
                    }
                    // **打っている途中をそのまま出す（e-4）。**
                    // **打ち終わるまで何も見えない形は、打ち間違いに
                    // 気づけない**（運用者の指摘）。
                    //
                    // 破壊 (e-4, zi-command-line-silent): 打っている間は描き直さない。
                    // **`:` を打った時点の空のコマンド行のままになる**ので、
                    // **最下行に打鍵が出ていることの判定だけが落ちる。**
                    // **コマンド自身は効く**（改行で解釈するため）ので、
                    // 往復も保存も変わらない。
                    #[cfg(not(zi_command_line_silent))]
                    {
                        refresh(
                            &view,
                            &window,
                            &Status {
                                mode,
                                row,
                                col,
                                dirty,
                                command: &command[..command_len],
                                in_command: true,
                                message,
                            },
                        );
                    }
                }
            }
            continue;
        }

        // **`:` でコマンド行へ入る（ノーマルのときだけ）。**
        if mode == Mode::Normal && byte == b':' {
            mode = Mode::Command;
            command_len = 0;
            continue;
        }

        let changed = handle_byte(
            &view,
            byte,
            buffer,
            &mut window,
            &mut row,
            &mut col,
            &mut mode,
            dirty,
            &mut message,
        );
        dirty |= changed;
    }

    leave_screen();
    release_and_exit(buffer, 0);
}

/// ヒープを返してから終わる（H-b-1）。**入れ物を取った後の終わる道はここを通る。**
///
/// # `unsafe` が要らない
///
/// **[`userlib::heap::release`] は領域を値で受け取る**（H-b-2）。
/// **`core::mem::take` で `Buffer` から抜いて渡すので、この関数から先に
/// 領域を指す参照が 1 つも残らない。** **契約ではなく所有で守られている。**
///
/// **b-1 は「戻らない関数の中でしか返さない」で守っていた。**
/// **b-2 で `grow_to` が所有の形を要求したので、こちらも同じ形になった。**
///
/// # 返せたかどうかは、こちらでは主張しない
///
/// **カーネルが `user-heap:` の行に、取った数と返した数を並べる**
/// （`kernel/src/userland.rs`）。**自分で書いて自分で読む形にしない。**
fn release_and_exit(buffer: &mut Buffer, status: u64) -> ! {
    // 破壊 (H-b-1, zi-skip-release): 返さずに終わる。**振る舞いは 1 つも
    // 変わらない**——**編集も保存も読み戻しも、返す前に終わっている。**
    // **落ちるのは「`zi` が取った分を返した」判定だけである**
    // ——**カーネルの `user-heap:` の行が、取った数と返した数を並べる。**
    #[cfg(not(zi_skip_release))]
    userlib::heap::release(core::mem::take(&mut buffer.region));
    exit(status)
}

/// バッファをファイルへ書き出す（`:w`。zi-d-2）。
///
/// **順序は zi-c の契約から決まる**（モジュール doc）——
/// `open(O_WRONLY|O_TRUNC)` で長さ 0 へ切ってから、全量を 1 回で書く。
///
/// **戻り値が要求と一致することを見る。** 一致しなければ、
/// **切った後に書けていない**ので内容が失われている。**黙らない。**
fn save(path: &[u8], buffer: &Buffer) -> bool {
    // **並べ直さない（H-b-1）。** **本体がそのまま書き出す形である**
    // ——**各行の後ろに改行が 1 つ在る**（[`Buffer`] の doc）。
    // **書き出しの受け皿は消えた。**
    let out = buffer.as_bytes();
    let at = out.len();

    // **無ければ作る（e-5。`O_CREAT`）。** **在れば長さ 0 へ切る**——
    // **`:w` は全置換なので、どちらの道でも同じ状態から書き始める。**
    let fd = open_write_create(path);
    if fd < 0 {
        // **使う人へのエラーである（ADR-0046）。** **`STDERR` へ出す**と、
        // **代替画面に居る間はカーネルが溜め、次の周で [`userlib::Echo`] が
        // 取り出してエコーエリアへ描く。** **画面は壊れない。**
        write_all(STDERR, b"zi: cannot open for writing\n");
        return false;
    }
    let fd = fd as u64;
    // 破壊 (zi-d-2, zi-write-skip-body): 中身を書かずに閉じる。
    // **open が長さ 0 へ切った後なので、ファイルが空のまま残る**——
    // `cat` の読み戻しが空になり、往復の判定が落ちる。
    // **`:w` の戻り値は「要求 0 に対して 0」になるので、量の判定は通る**
    // ——**捕まえるのは往復のほうである。**
    #[cfg(zi_write_skip_body)]
    let at = 0usize;
    let written = write_all(STDOUT_UNUSED_MARKER.min(fd), &out[..at]);
    // **閉じるときに装置へ書き戻る（P-c-1）。** **戻り値を見る**——
    // **書き戻せなかったら、中身は RAM に在るが装置に無い。**
    // **黙って成功にしない**（`ADR-0046`。エコーエリアへ出す）。
    //
    // **いま断られる形は起きない見込みである**——**装置の占有を争う者が
    // 1 つしか居ない**（前景の 1 本だけ。AP は利用者を走らせていない）。
    // **それでも見るのは、断られたことが観測できる形にしておくためである。**
    let closed = close(fd);
    if closed < 0 {
        write_all(STDERR, b"zi: saved to memory but not to the disk\n");
    }

    // **要求した長さと一致すること。** `write_all` は繰り返して全量を書くので、
    // 足りないのは誤りである。
    let ok = written == at as i64 && closed >= 0;
    report_save(at, written, ok);
    ok
}

/// `save` が `write_all` へ渡す fd の目印。**`fd` をそのまま使うための飾りである**
/// ——`min` は常に `fd` を返す（`fd` は 3 以上、この値は大きい）。
///
/// **なぜこう書くか**——`write_all` の引数を `fd` と読み違えないように、
/// 「端末ではない」ことを名前で示している。
const STDOUT_UNUSED_MARKER: u64 = u64::MAX;

/// 保存の判定行。**書いた量と要求した量を並べる。**
///
/// **検査の構成でしか出さない**（[`report_cursor`] と同じ理由）。
#[cfg(zi_diagnostics)]
fn report_save(requested: usize, written: i64, ok: bool) {
    let mut out = [0u8; 96];
    let mut at = 0usize;
    let head = b"zi: saved bytes requested=";
    out[at..at + head.len()].copy_from_slice(head);
    at += head.len();
    let mut digits = [0u8; 12];
    let count = write_number(&mut digits, requested);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at..at + 9].copy_from_slice(b" written=");
    at += 9;
    // **負の値は errno である。** 桁で書けないので、目印だけ置く。
    let count = if written < 0 {
        out[at] = b'-';
        at += 1;
        write_number(&mut digits, (-written) as usize)
    } else {
        write_number(&mut digits, written as usize)
    };
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    let tail: &[u8] = if ok { b" match=true\n" } else { b" match=false\n" };
    out[at..at + tail.len()].copy_from_slice(tail);
    at += tail.len();
    userlib::log_line(STDERR, &out[..at]);
}

/// 判定行を出さない側（既定のビルド）。
#[cfg(not(zi_diagnostics))]
fn report_save(_requested: usize, _written: i64, _ok: bool) {}

/// 端末の大きさの判定行（e-1）。**受け取った値をそのまま出す。**
///
/// **突き合わせる相手はカーネルが出す行である**——**期待値をこちらが持たない。**
/// **カーネルは自分の `Console` から桁と行を読んで出しており、こちらは
/// `ioctl` を通って受け取った値を出す。** **経路のどこかで入れ替われば食い違う。**
#[cfg(zi_diagnostics)]
fn report_window_size(size: Result<userlib::WindowSize, i64>) {
    let mut out = [0u8; 96];
    let mut at = 0usize;
    let head = b"zi: winsize rows=";
    out[at..at + head.len()].copy_from_slice(head);
    at += head.len();
    let mut digits = [0u8; 12];
    let (rows, columns) = match size {
        Ok(size) => (size.rows as usize, size.columns as usize),
        // **失敗は 0 として出す。** **判定は「カーネルの値と一致すること」なので、
        // 0 は一致しない**（画面が在る構成で走るためである）。
        Err(_) => (0, 0),
    };
    let count = write_number(&mut digits, rows);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at..at + 9].copy_from_slice(b" columns=");
    at += 9;
    let count = write_number(&mut digits, columns);
    out[at..at + count].copy_from_slice(&digits[..count]);
    at += count;
    out[at] = b'\n';
    at += 1;
    userlib::log_line(STDERR, &out[..at]);
}

/// 判定行を出さない側（既定のビルド）。
#[cfg(not(zi_diagnostics))]
fn report_window_size(_size: Result<userlib::WindowSize, i64>) {}

/// Esc 単体を確定する（e-2）。**インサートならノーマルへ戻る。**
///
/// # 2 つの経路から呼ぶ
///
/// **次の字が来たとき**と、**`read` が `-EAGAIN` を返したとき**である。
/// **後者が本命で、前者は「Esc の次にすぐ字が来た」場合の受けである。**
///
/// # なぜ `-EAGAIN` で確定してよいのか
///
/// **カーネルが CSI の 3 バイトを不可分に届けるからである**
/// （`kernel/src/input.rs` の `bytes_for_event` の doc）。
/// **`\x1b` の直後の `read` が `-EAGAIN` を返したら、それは CSI の途中では
/// ありえない。** **本物の端末と違い、ESC タイムアウトの曖昧さが生じない。**
///
/// **e-2 まで、この規約は使われていなかった**——`zi` は次の 1 バイトが
/// 来るまで待っており、**Esc を 2 回押さないとノーマルへ戻れなかった**
/// （運用者の目視で出た）。
fn finish_pending_escape(
    view: &View,
    buffer: &Buffer,
    window: &mut Window,
    row: usize,
    col: &mut usize,
    mode: &mut Mode,
    shown_mode: &mut Mode,
    dirty: bool,
) {
    if *mode != Mode::Insert {
        return;
    }
    *mode = Mode::Normal;
    // **ノーマルへ戻ると、カーソルは 1 つ左へ寄る**（vi の形）。
    *col = col.saturating_sub(1);
    // **札を描いてからカーソルを戻す**（[`refresh`]）。
    // **順序はあちらが持つ**ので、ここでは考えない。
    refresh(
        view,
        window,
        &Status {
            mode: *mode,
            row,
            col: *col,
            dirty,
            command: &[],
            in_command: false,
            // **モードを戻すだけなので、報せは持たない。**
            message: &[],
        },
    );
    *shown_mode = *mode;
    report_cursor(buffer, window, row, *col, b"normal");
}

/// コマンド行を解釈する（zi-d-2）。**戻り値は「終わってよいか」である。**
///
/// **`:q` は変更があれば拒む。** `:q!` は入れない——**「変更を捨てる」の
/// 意思表示が要るが、最小には無くてよい**（拒まれたら `:wq` を使う）。
fn run_command(command: &[u8], path: &[u8], buffer: &Buffer, dirty: bool) -> (Command, &'static [u8]) {
    match command {
        b"w" => {
            if save(path, buffer) {
                (Command::Saved, b"written")
            } else {
                (Command::Failed, b"")
            }
        }
        b"q" => {
            if dirty {
                // **報せはコマンド行へ出す（e-5）。** **`STDERR` は
                // 「使う人が見る画面」ではない**——`zi` は代替画面に居る。
                (Command::Refused, b"unsaved changes; use :wq")
            } else {
                (Command::Quit, b"")
            }
        }
        b"wq" => {
            if save(path, buffer) {
                (Command::Quit, b"")
            } else {
                (Command::Failed, b"")
            }
        }
        _ => (Command::Refused, b"unknown command"),
    }
}

/// [`run_command`] の結果。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    /// 保存した。編集を続ける。
    Saved,
    /// 終わってよい。
    Quit,
    /// 断った（変更があるのに `:q`、または知らないコマンド）。
    Refused,
    /// 保存に失敗した。**終了状態 5 で終わる。**
    Failed,
}

/// ノーマル / インサートの 1 バイトを処理する。
///
/// **戻り値は「バッファを変えたか」である**（zi-d-2 で意味を変えた。
/// `:q` が変更の有無で拒むために要る）。
fn handle_byte(
    view: &View,
    byte: u8,
    buffer: &mut Buffer,
    window: &mut Window,
    row: &mut usize,
    col: &mut usize,
    mode: &mut Mode,
    dirty: bool,
    message: &mut &'static [u8],
) -> bool {
    match *mode {
        // **コマンド行は呼び出し側で処理する**（主ループが `continue` する）。
        Mode::Command => false,
        Mode::Normal => {
            let moved = match byte {
                b'h' => move_left(col),
                b'j' => move_down(buffer, row, col),
                b'k' => move_up(buffer, row, col),
                b'l' => move_right(buffer, *row, col, *mode),
                b'i' => {
                    *mode = Mode::Insert;
                    // **行は変わらないので窓も動かない。** カーソルだけ戻す。
                    restore_cursor(window, *row, *col);
                    report_cursor(buffer, window, *row, *col, b"insert");
                    // **モードを変えただけで、バッファは変わっていない。**
                    return false;
                }
                // **カーソルの次から挿入する（e-4。vi の `a`）。**
                //
                // **`i` との違いは桁が 1 つ右になることだけである。**
                // **行末では動かない**——インサートでは末尾の 1 つ先まで
                // 許すので、`move_right` と同じ上限に合わせる。
                // **`A` / `o` / `O` / `I` は作らない**（利用者が求めたのは
                // `a` である。使う者がいない機構は検算が置けない）。
                b'a' => {
                    *mode = Mode::Insert;
                    // 破壊 (e-4, zi-append-like-insert): `a` を `i` と同じにする。
                    // **モードは変わり、字も入る**ので、往復も本数も変わらない。
                    // **落ちるのは「`a` は `i` より 1 つ右から始まる」判定だけである。**
                    #[cfg(not(zi_append_like_insert))]
                    {
                        if *col < buffer.length(*row) {
                            *col += 1;
                        }
                    }
                    // **行は変わらないので窓も動かない。** カーソルだけ戻す。
                    restore_cursor(window, *row, *col);
                    // **札は `insert` と分ける**——**判定が `i` と `a` を
                    // 見分けるためである**（`a` は直前の位置より 1 つ右）。
                    report_cursor(buffer, window, *row, *col, b"append");
                    return false;
                }
                b'x' => {
                    let removed = buffer.remove(*row, *col);
                    if removed {
                        // **行末を越えたら 1 つ左へ寄る**（vi の形）。
                        let length = buffer.length(*row);
                        if *col >= length {
                            *col = length.saturating_sub(1);
                        }
                        redraw(
                            view,
                            buffer,
                            window,
                            &Status {
                                mode: *mode,
                                row: *row,
                                col: *col,
                                // **消した直後なので、変更は確実にある。**
                                dirty: true,
                                command: &[],
                                in_command: false,
                                message: &[],
                            },
                        );
                        report_cursor(buffer, window, *row, *col, b"delete");
                    }
                    return removed;
                }
                // 知らないキーは黙って捨てる。**`:` は zi-d-2 で受ける。**
                _ => false,
            };
            if moved {
                // **`j` と `k` は行を移る（VIEW-a）。** **窓の外へ出たら
                // 窓が動き、そのときは描き直しになる。**
                show_cursor(
                    view,
                    buffer,
                    window,
                    &Status {
                        mode: *mode,
                        row: *row,
                        col: *col,
                        dirty,
                        command: &[],
                        in_command: false,
                        message: &[],
                    },
                );
                report_cursor(buffer, window, *row, *col, b"move");
            }
            // **移動はバッファを変えない。**
            false
        }
        Mode::Insert => {
            // **Enter で行を割る（zi-f）。** vi と同じで、カーソル以降が
            // 新しい行へ移り、カーソルは新しい行の先頭へ行く。
            if byte == b'\n' {
                // 破壊 (zi-f, zi-enter-does-nothing): Enter を捨てる。
                // **zi-d-1 までの振る舞いに戻る**（あの頃は「行の追加は
                // 範囲外」として捨てていた）。**行が増えないので、読み戻しが
                // 2 行にならない**——**「enter split the line」だけが落ちる。**
                #[cfg(zi_enter_does_nothing)]
                return false;

                #[cfg(not(zi_enter_does_nothing))]
                if !buffer.split_line(*row, *col) {
                    *message = OUT_OF_MEMORY_FOR_A_LINE;
                    redraw_here(view, buffer, window, *mode, *row, *col, message);
                    return false;
                }
                *row += 1;
                *col = 0;
                redraw_here(view, buffer, window, *mode, *row, *col, b"");
                report_cursor(buffer, window, *row, *col, b"split");
                return true;
            }
            // **Backspace（zi-f）。** 行頭なら前の行と繋げる。
            if byte == 0x08 {
                if *col > 0 {
                    let removed = buffer.remove(*row, *col - 1);
                    if removed {
                        *col -= 1;
                        // **行の中の削除は 1 行だけが変わる（PERF-g）。**
                        redraw_line_here(view, buffer, window, *mode, *row, *col, b"");
                        report_cursor(buffer, window, *row, *col, b"erase");
                    }
                    return removed;
                }
                if *row == 0 {
                    // **1 行目の行頭では何もしない**（繋げる先が無い）。
                    return false;
                }
                // 破壊 (zi-f, zi-join-does-nothing): 行頭の Backspace を捨てる。
                // **行が繋がらないので、読み戻しが 2 行のまま残る**
                // ——**「行頭の Backspace が前の行と繋げた」判定だけが落ちる。**
                // **`zi-enter-does-nothing` と対である**（あちらは割る側）。
                #[cfg(zi_join_does_nothing)]
                {
                    return false;
                }
                #[cfg(not(zi_join_does_nothing))]
                {
                    let landing = buffer.length(*row - 1);
                    // **H-b-2 で断る道が消えた。** **1 行の上限が無くなり、
                    // 繋げると本文は 1 バイト縮む**ので、**ここは必ず通る**
                    // （`*row` は行数未満なので、`*row - 1` の次の行は在る）。
                    // **戻り値は捨てない**——**通らなかったことに気づけなくなる。**
                    if !buffer.join_with_next(*row - 1) {
                        return false;
                    }
                    *row -= 1;
                    *col = landing;
                    redraw_here(view, buffer, window, *mode, *row, *col, b"");
                    report_cursor(buffer, window, *row, *col, b"join");
                    return true;
                }
            }
            // 破壊 (zi-d-2, zi-insert-drop-first): 挿入の最初の 1 字を落とす。
            // **`cat` の読み戻しが 1 字短くなる**ので、往復の判定が捕まえる。
            // **画面の再描画も 1 字少ないが、それは観測できない**（モジュール doc）。
            #[cfg(zi_insert_drop_first)]
            let inserted = {
                use core::sync::atomic::{AtomicBool, Ordering};
                static DROPPED: AtomicBool = AtomicBool::new(false);
                if DROPPED.swap(true, Ordering::SeqCst) {
                    buffer.insert(*row, *col, byte)
                } else {
                    false
                }
            };
            #[cfg(not(zi_insert_drop_first))]
            let inserted = buffer.insert(*row, *col, byte);
            if inserted {
                *col += 1;
                // **変わったのは 1 行だけである（PERF-g）。**
                redraw_line_here(view, buffer, window, *mode, *row, *col, b"");
                report_cursor(buffer, window, *row, *col, b"typed");
            }
            inserted
        }
    }
}

/// 編集の後の描き直し——変わった 1 行だけを描く（PERF-g）。
///
/// # なぜ全面ではないのか
///
/// **素の挿入と削除で変わるのは 1 行だけである**（`Buffer` は行の中で
/// バイトをずらす。**他の行は動かない**）。
///
/// **全面を描き直していた**——**実測で 1 字の挿入が 1,125 字・330.9M
/// サイクル（約95ms）だった。** **自動繰り返しは毎秒 25〜30 回来るので、
/// 生成が消費の 3 倍近くになり、押しっぱなしで溜まっていた。**
///
/// # 窓は動かさない
///
/// **行の中身が変わっただけなので、窓の位置は変わらない。**
/// **窓が動く形（カーソルが窓の外へ出る）は [`show_cursor`] が持つ。**
///
/// # 行が増減する場合は使えない
///
/// **`Enter` と行頭の `Backspace` は、その行から下が全部ずれる。**
/// **そちらは [`redraw_here`]（全面）のままである**——`IL` / `DL` で
/// ずらす形は測ってから決める（`docs/roadmap.md` の PERF 段）。
///
/// # カーソルは最後に戻す
///
/// **[`refresh`] を通す**——**`restore_cursor` がすべての描き終わりの
/// 最後に居る形を崩さない**（`PERF-b` の回帰がその順序で出た）。
fn redraw_line_here(
    view: &View,
    buffer: &Buffer,
    window: &Window,
    mode: Mode,
    row: usize,
    col: usize,
    message: &[u8],
) {
    // 破壊 (PERF-g, zi-edit-redraws-everything): 1 行ではなく全面を描き直す。
    // **PERF-g の前の形そのものである**——**絵は同じで、描く字が桁で増える。**
    #[cfg(zi_edit_redraws_everything)]
    {
        let mut window = *window;
        redraw_here(view, buffer, &mut window, mode, row, col, message);
        return;
    }
    #[cfg(not(zi_edit_redraws_everything))]
    {
        if let Some(screen_row) = window.screen_row(row) {
            move_cursor(screen_row, 0);
            // EL(2): その行を消してから置く（消し残しを作らない）。
            userlib::frame_push(STDOUT, b"\x1b[2K");
            let line = buffer.line(row);
            if !line.is_empty() {
                userlib::frame_push(STDOUT, line);
            }
        }
        refresh(
            view,
            window,
            &Status {
                mode,
                row,
                col,
                dirty: true,
                command: &[],
                in_command: false,
                message,
            },
        );
    }
}

/// 編集の後の描き直し（zi-f で切り出した）。
///
/// **`Status` を組み立てる形が 4 か所へ増えたので、1 つにまとめる。**
/// **変更があった直後にしか呼ばない**ので、`dirty` は常に真である。
fn redraw_here(
    view: &View,
    buffer: &Buffer,
    window: &mut Window,
    mode: Mode,
    row: usize,
    col: usize,
    message: &[u8],
) {
    redraw(
        view,
        buffer,
        window,
        &Status {
            mode,
            row,
            col,
            dirty: true,
            command: &[],
            in_command: false,
            message,
        },
    );
}

/// 左へ 1 つ。**行頭では動かない**（前の行の末尾へは回らない）。
fn move_left(col: &mut usize) -> bool {
    if *col == 0 {
        return false;
    }
    *col -= 1;
    true
}

/// 右へ 1 つ。**行末では動かない。**
///
/// **ノーマルでは最後の字の上まで、インサートでは末尾の 1 つ先まで**
/// 動ける（vi の形。挿入は末尾へ足せる）。
fn move_right(buffer: &Buffer, row: usize, col: &mut usize, mode: Mode) -> bool {
    let length = buffer.length(row);
    let limit = match mode {
        // **コマンド行では矢印が来ない**（主ループが先に処理する）。
        // ノーマルと同じ扱いにしておく。
        Mode::Normal | Mode::Command => length.saturating_sub(1),
        Mode::Insert => length,
    };
    if *col >= limit {
        return false;
    }
    *col += 1;
    true
}

/// 上へ 1 行。**先頭行では動かない。** 桁は移った行の長さで切り詰める。
fn move_up(buffer: &Buffer, row: &mut usize, col: &mut usize) -> bool {
    if *row == 0 {
        return false;
    }
    *row -= 1;
    clamp_column(buffer, *row, col);
    true
}

/// 下へ 1 行。**最終行では動かない。**
fn move_down(buffer: &Buffer, row: &mut usize, col: &mut usize) -> bool {
    if *row + 1 >= buffer.count {
        return false;
    }
    *row += 1;
    clamp_column(buffer, *row, col);
    true
}

/// 移った先の行の長さへ桁を寄せる。
fn clamp_column(buffer: &Buffer, row: usize, col: &mut usize) {
    let limit = buffer.length(row).saturating_sub(1);
    if *col > limit {
        *col = limit;
    }
}
