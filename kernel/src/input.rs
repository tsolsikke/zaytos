//! Ring 3 への入力の配送（S11-10）。
//!
//! # 不変条件——**入力の消費者は同時に 1 つである**
//!
//! **これを先に書く。** スキャンコードのリング（[`crate::keyboard::buffer::SCANCODES`]）
//! は**取り出したら消える**。**2 人が同時に取ると、どちらも全部は見ない。**
//! 片方が `a` を、もう片方が `b` を受け取る形になり、**どちらの側から見ても
//! 入力が壊れている。**
//!
//! **リングを 2 本にする形は採らない。** 同じバイトを 2 人へ配ると、
//! **「誰が消費したか」が決まらず、行の組み立てが両方で進む。**
//!
//! **したがって、消費者を切り替える。** 前景（foreground）を持っている側だけが
//! 取り出す。**持ち主は 1 人である。**
//!
//! # いまの消費者はカーネルである
//!
//! `crate::interrupts` の `run_timer_loop` が `drain_keyboard` で抜き取り、
//! デコードしてコンソールへ反響し、1 行になったらログへ出している。
//! **Ring 3 が読むなら、その間カーネル側の消費を止める必要がある。**
//!
//! # 前景は遠征の前後で取り、戻すのは同じ場所である
//!
//! **`crate::vfs::swap_current_files` と `crate::syscall::set_user_window` と
//! 同じ形である**（S9-b-3-2b から続く形）。**据える側が戻す。**
//!
//! # 待たない
//!
//! **バイトが無ければ `-EAGAIN` である**（`crate::syscall` が写す）。
//! **待つには、待っている間に他を走らせる仕組みが要る**——それはユーザープロセスの
//! スケジューラで、**まだ無い**（`docs/roadmap.md` の S11）。
//!
//! **`0` を返さない。** Linux では `read` の 0 は末尾（EOF）である。
//! **「今は無い」と「もう来ない」を同じ値にしない。**
//!
//! # 行ではなくバイトを返す
//!
//! **行に組み立てるのは上の層である**（S11 の棚卸しで決めた）。
//! ここが返すのは**デコード済みの文字のバイト**で、**行の区切りも編集も持たない。**
//!
//! **生モード（`docs/vision.md` の `zi`）を後から足せる形になっている**——
//! **バイトを返す路はそのままで、上に「行に組み立てる層」を足すか外すかである。**
//! **ここを行で返す形にすると、生モードを足すときに路そのものを作り直すことになる。**
//! **S11 では切り替えを入れない**（棚卸しの決定）。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::critical::Locked;

/// Ring 3 が前景を持っているか（S11-10）。
///
/// **真のあいだ、カーネル側の消費者（`drain_keyboard`）は取り出さない。**
static FOREGROUND: AtomicBool = AtomicBool::new(false);

/// Ring 3 が受け取ったバイトの累計（判定行に出す）。
static DELIVERED: AtomicU64 = AtomicU64::new(0);

/// 前景でデコードした文字を溜める場所（S11-10）。
///
/// # なぜスキャンコードのリングを直に読まないのか
///
/// **スキャンコードは 1 バイトが 1 文字ではない。** 押下と離脱で 2 回出て、
/// 拡張コード（`0xE0`）は 2 バイトである。**デコーダは状態を持つ**
/// （`crate::keyboard::decode::Decoder`）。
///
/// **デコードは割り込みハンドラでは行わない**（`ADR-0018` §5。ハンドラは
/// リングへ積むだけである）。**したがって、読む側でデコードする。**
/// **デコーダの状態は読む側が持つ**ので、ここに置く。
static PENDING: Locked<PendingBytes> = Locked::new(PendingBytes::new());

/// デコード済みのバイトを溜める小さな環。
///
/// # 8 バイトで足りる
///
/// **1 回の `read` で取り切る形なので、溜まるのは「1 回のデコードで出た分」
/// だけである。** 1 つのスキャンコードから出る文字は多くて 1 つなので、
/// **実際には 1 バイトずつしか入らない。** 8 はその余裕である。
struct PendingBytes {
    bytes: [u8; 8],
    length: usize,
}

impl PendingBytes {
    const fn new() -> Self {
        Self {
            bytes: [0; 8],
            length: 0,
        }
    }
}

/// 前景を取る（S11-10）。**既に誰かが持っていれば偽を返す。**
///
/// # `compare_exchange` を使う
///
/// **`load` してから `store` する形にしない**——2 つの実行文脈が同時に
/// 通り抜けられる（`ADR-0030` で同じ判断をした）。
/// **いまは単一コアの直線なので実害は出ないが、形を先に正しくしておく。**
pub fn claim_foreground() -> bool {
    FOREGROUND
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// 前景を返す（S11-10）。
pub fn release_foreground() {
    FOREGROUND.store(false, Ordering::SeqCst);
    // **溜まっていたバイトは捨てる。** 次に前景を取る者は、**前の持ち主へ
    // 打たれたバイトを受け取らない。**
    PENDING.lock().length = 0;
}

/// Ring 3 が前景を持っているか。**カーネル側の消費者が見る。**
pub fn foreground_is_claimed() -> bool {
    FOREGROUND.load(Ordering::SeqCst)
}

/// Ring 3 へ届けたバイトの累計（判定行）。
pub fn delivered_count() -> u64 {
    DELIVERED.load(Ordering::SeqCst)
}

/// デコード済みのバイトを取り出す。**取れた数を返す（0 もありうる）。**
///
/// # スキャンコードをここでデコードする
///
/// **リングから取り、デコーダへ食わせ、文字が出たら `dst` へ置く。**
/// **`dst` が埋まったら、余りは [`PENDING`] へ残す**——次の `read` が続きを取る。
///
/// # デコーダの状態は静的に持つ
///
/// **拡張コードの途中で `read` が戻ることがある**ので、
/// **状態を呼び出しをまたいで保つ必要がある。**
pub fn read_bytes(dst: &mut [u8]) -> usize {
    /// デコーダ。**前景の読み手が使う唯一のものである。**
    static DECODER: Locked<crate::keyboard::decode::Decoder> =
        Locked::new(crate::keyboard::decode::Decoder::new());

    let mut written = 0usize;

    // **まず溜まっている分を出す。**
    {
        let mut pending = PENDING.lock();
        while written < dst.len() && pending.length > 0 {
            dst[written] = pending.bytes[0];
            let length = pending.length;
            pending.bytes.copy_within(1..length, 0);
            pending.length = length - 1;
            written += 1;
        }
    }

    while written < dst.len() {
        let code = {
            let mut ring = crate::keyboard::buffer::SCANCODES.lock();
            ring.pop()
        };
        let Some(code) = code else {
            break;
        };
        let event = { DECODER.lock().feed(code) };
        let Some(event) = event else {
            continue;
        };
        // **矢印は 3 バイトへ落とす（S12 前の手当て）。**
        //
        // **形は CSI である**（`\x1b[D` と `\x1b[C`）。`ADR-0029` が画面制御に
        // ANSI を選んでいるので、**入力の側も同じ表現にしておくと後で噛み合う。**
        // **解釈するのはシェルであって、コンソールではない**——このバイト列が
        // `Grid` へ届くことはない。
        //
        // **`dst` に 3 バイト入らないことがある。** 溜め場（[`PENDING`]）が
        // 既にその形を持っているので、入る分だけ置いて残りを預ける。
        if let crate::keyboard::decode::KeyEvent::ArrowLeft
        | crate::keyboard::decode::KeyEvent::ArrowRight = event
        {
            let sequence: &[u8] = match event {
                crate::keyboard::decode::KeyEvent::ArrowLeft => b"\x1b[D",
                _ => b"\x1b[C",
            };
            for (index, byte) in sequence.iter().enumerate() {
                if written < dst.len() {
                    dst[written] = *byte;
                    written += 1;
                } else {
                    let mut pending = PENDING.lock();
                    let at = pending.length;
                    // **溜め場は 8 バイトで、ここへ来るのは多くて 2 バイトである。**
                    // 溢れるなら落とす——**落としたことが分かる形は無いが、
                    // 入らないものを入ったことにはしない。**
                    if at < pending.bytes.len() {
                        pending.bytes[at] = *byte;
                        pending.length = at + 1;
                    }
                    let _ = index;
                }
            }
            continue;
        }

        // **バイトへ落とす。** 行の区切りも編集もここでは持たない。
        let byte = match event {
            crate::keyboard::decode::KeyEvent::Char(character) => {
                // **ASCII だけを通す。** デコーダは US 配列で、非 ASCII を出さない。
                if character.is_ascii() {
                    character as u8
                } else {
                    continue;
                }
            }
            crate::keyboard::decode::KeyEvent::Enter => b'\n',
            crate::keyboard::decode::KeyEvent::Backspace => 0x08,
            // **バイトへ落とせないものは落とす。** 上下や Home などで、
            // **行の編集を持つ層が要る形である**（`docs/vision.md` の `zi`）。
            // **左右の矢印は上で 3 バイトへ落としてある。**
            crate::keyboard::decode::KeyEvent::Unsupported(_) => continue,
            // 上の分岐で返しているので、ここへは来ない。
            crate::keyboard::decode::KeyEvent::ArrowLeft
            | crate::keyboard::decode::KeyEvent::ArrowRight => continue,
        };
        dst[written] = byte;
        written += 1;
    }

    DELIVERED.fetch_add(written as u64, Ordering::SeqCst);
    written
}
