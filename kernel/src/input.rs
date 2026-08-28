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

/// 中断（Ctrl+C）が要求されたか（S12 前の手当て、C）。
///
/// # なぜ割り込みの側で立てるのか
///
/// **デコードの経路は 1 本しかなく、それは [`read_bytes`] である。**
/// **あれは Ring 3 が `read` を出したときにしか動かない。**
/// **止めたい相手は `read` を出さずに回っている子なので、あの経路では見えない。**
///
/// **したがって、割り込みの側で見るしかない**（`crate::keyboard::handle_irq`）。
/// **あちらはデコードせず、この旗のためだけの最小の追跡を持つ**
/// （[`note_scancode_for_interrupt`]）。
///
/// # 消費するのは 1 か所である
///
/// **畳む地点だけが消費する**（`crate::idt` の LAPIC タイマの分岐）。
/// **深さ 1 では消費されず、立ったまま残る。** それでよい——
/// **畳む地点は深さ 2 以上でしか発火しないので、残っていても何も起きない。**
/// **次に子を起こすとき、`crate::userland` が起こす直前に降ろす**
/// （前の中断要求を新しい子へ持ち越さない）。
static INTERRUPT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Ctrl が押されているか（割り込みの側の追跡。S12 前の手当て、C）。
///
/// **`Decoder` が持つものとは別である。** あちらは文字を作る側で、
/// **`read` の文脈でしか動かない。** こちらは中断を見つける側で、
/// **キーが押された瞬間に動く。** **住んでいる文脈が違うので分けてある。**
///
/// **統合するにはデコーダを割り込みハンドラへ持ち込むことになり、
/// `ADR-0018` §5（ハンドラはリングへ積むだけ）に反する。**
static CTRL_DOWN: AtomicBool = AtomicBool::new(false);

/// 直前に拡張接頭辞（`0xE0`）を見たか。
static EXTENDED_PENDING: AtomicBool = AtomicBool::new(false);

/// Pause（`0xE1`）の後に読み捨てる残りバイト数。
///
/// **`0xE1` の列は `E1 1D 45 E1 9D C5` で、`0x1D`（Ctrl）を含む。**
/// **飲み込まないと、Pause を押しただけで Ctrl が押されたことになる。**
/// `Decoder` が同じ罠を `PAUSE_PREFIX` の doc に書いている。
static PAUSE_REMAINING: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// 割り込みの側から 1 バイト与える（S12 前の手当て、C）。
///
/// **デコーダではない。** 表も Shift も Caps Lock も見ない。
/// **Ctrl の押下状態と、Ctrl+C の一致だけを見る。**
///
/// **ロックを取らない。** 割り込み文脈から呼ばれるので、
/// **状態はすべてアトミックで持つ**（この関数が増やすロックは 0 である）。
pub fn note_scancode_for_interrupt(code: u8) {
    use crate::keyboard::decode::{SCANCODE_C, SCANCODE_CTRL};

    // Pause の列を飲み込む。
    let remaining = PAUSE_REMAINING.load(Ordering::SeqCst);
    if remaining > 0 {
        PAUSE_REMAINING.store(remaining - 1, Ordering::SeqCst);
        return;
    }
    if code == crate::keyboard::decode::PAUSE_PREFIX {
        PAUSE_REMAINING.store(
            crate::keyboard::decode::PAUSE_TRAILING_BYTES,
            Ordering::SeqCst,
        );
        return;
    }
    if code == crate::keyboard::decode::EXTENDED_PREFIX {
        EXTENDED_PENDING.store(true, Ordering::SeqCst);
        return;
    }

    // **接頭辞は読み捨てる。** 右 Ctrl（`0xE0 0x1D`）を左と同じに扱うためである
    // （`Decoder` 側と同じ判断）。
    let _extended = EXTENDED_PENDING.swap(false, Ordering::SeqCst);

    let released = code & 0x80 != 0;
    let key = code & !0x80;

    if key == SCANCODE_CTRL {
        CTRL_DOWN.store(!released, Ordering::SeqCst);
        return;
    }

    // 破壊 (S12 前の手当て C, kill-ignore-interrupt): 旗を立てない。
    // **走っている子が Ctrl+C で止まらなくなる。**
    #[cfg(not(feature = "kill-ignore-interrupt-test"))]
    if !released && key == SCANCODE_C && CTRL_DOWN.load(Ordering::SeqCst) {
        INTERRUPT_REQUESTED.store(true, Ordering::SeqCst);
    }
}

/// 中断が要求されているかを見て、要求を降ろす（S12 前の手当て、C）。
///
/// **`swap` なので消費は原子的である。** 二重に消費されない。
pub fn take_interrupt_request() -> bool {
    INTERRUPT_REQUESTED.swap(false, Ordering::SeqCst)
}

/// 中断の要求を降ろす（S12 前の手当て、C）。
///
/// **子を起こす直前に呼ぶ。** 前の中断要求を新しい子へ持ち越さない。
pub fn clear_interrupt_request() {
    INTERRUPT_REQUESTED.store(false, Ordering::SeqCst);
}

/// 溜まっている入力を捨てる（S12 前の手当て、C）。
///
/// # なぜ要るのか。**止めた打鍵そのものが残る**
///
/// **子を止めた Ctrl+C は、スキャンコードのリングにも積まれている。**
/// **積む側（`crate::keyboard::handle_irq`）と、中断を見つける側は独立で、
/// 見つけたからといって積むのをやめるわけではない。**
///
/// **そのまま戻ると、シェルが次に `read` を出したときにあの打鍵をデコードする。**
/// **実測で、止めた直後のプロンプトに `^C` が 1 つ余分に出た**
/// （`spin` を止めた後の `zaytos$ ^C`）。
///
/// **端末の慣行と一致する。** 実際の端末も割り込み時に入力待ち行列を流す。
/// **打ち込んでおいた先の入力も一緒に消えるが、それが期待される振る舞いである**
/// ——止めた後に、止める前へ打った語が走り出すほうが驚く。
///
/// # 割り込み文脈から呼ばない
///
/// **ここはロックを 2 つ取る。** 呼ぶのは遠征から戻った後の通常の文脈で、
/// **畳む地点（タイマ割り込みの中）からは呼ばない。**
/// **割り込み文脈で増やすロックは 0 のままにしてある。**
pub fn discard_typed_input() {
    {
        let mut ring = crate::keyboard::buffer::SCANCODES.lock();
        while ring.pop().is_some() {}
    }
    PENDING.lock().length = 0;
}

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
    let claimed = FOREGROUND
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    if claimed {
        // **取れた深さを控える（S12 前の手当て、C）。** 中断の判定行が、
        // **持ち主（最も外側）と止めた相手（最も内側）を並べて出す。**
        FOREGROUND_DEPTH.store(crate::ring3::depth(), Ordering::SeqCst);
    }
    claimed
}

/// 前景を取った遠征の深さ（S12 前の手当て、C）。**判定行に出すためだけに持つ。**
static FOREGROUND_DEPTH: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 前景を取った遠征の深さ。**取られていないときの値は意味を持たない。**
pub fn foreground_depth() -> usize {
    FOREGROUND_DEPTH.load(Ordering::SeqCst)
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

    // 検証（zi-d, zi-test）: 打鍵の代わりに台本を返す。**時間に依存しない**
    // ——`sendkey` は打鍵の間隔と待ちに依るが、これは読み手が要求したぶんだけ
    // 進む。**測っているものが違う**——実打鍵が届くことは `--shell-test` が
    // 主張しており（zi-a で Esc と矢印を足した判定）、ここが主張するのは
    // エディタの論理である。**層が違うものを同じ項目で測らない。**
    #[cfg(any(feature = "zi-test", feature = "view-test"))]
    {
        let taken = script::next_bytes(dst);
        if taken > 0 {
            DELIVERED.fetch_add(taken as u64, Ordering::SeqCst);
            return taken;
        }
        // 台本を出し切った。**以後は本物の打鍵の経路へ落ちる**（`-EAGAIN` で
        // 回り続ける形になる）。
    }

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
        // **バイト列への写像は純粋関数が持つ**（[`bytes_for_event`]。zi-a で
        // 切り出した）。**Esc 単体と CSI の出し分けの規約もそちらの doc にある。**
        match bytes_for_event(event) {
            DeliveredBytes::None => continue,
            DeliveredBytes::Single(byte) => {
                dst[written] = byte;
                written += 1;
            }
            // **`dst` に 3 バイト入らないことがある。** 溜め場（[`PENDING`]）が
            // 既にその形を持っているので、入る分だけ置いて残りを預ける。
            DeliveredBytes::Csi(sequence) => {
                for byte in sequence {
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
                    }
                }
            }
        }
    }

    DELIVERED.fetch_add(written as u64, Ordering::SeqCst);
    written
}

/// [`bytes_for_event`] が返す、Ring 3 へ届けるバイトの形。
pub(crate) enum DeliveredBytes {
    /// 届けない（バイトへ落とせないキー）。
    None,
    /// 1 バイト。
    Single(u8),
    /// CSI の列。**不可分に届ける**（下の doc）。
    ///
    /// # 長さを固定しない（zi-f）
    ///
    /// **以前は `[u8; 3]` だった**（矢印がすべて 3 バイトだったため）。
    /// **Delete は `\x1b[3~` の 4 バイトである**——**本物の端末が送る形で、
    /// 3 バイトの版は無い。** **器のほうを合わせる。**
    ///
    /// **[`PENDING`] は 8 バイト持つ**ので、溢れない。
    Csi(&'static [u8]),
}

/// キーイベントからバイト列への写像（zi-a で純粋関数へ切り出した）。
///
/// # Esc 単体と CSI の出し分け
///
/// **Esc キーは素の 1 バイト（`\x1b`）、矢印は CSI の 3 バイト列である**
/// （`\x1b[D` / `\x1b[C` / `\x1b[A` / `\x1b[B`。S12 前の手当てで左右、zi-a で
/// 上下）。**形は ANSI の CSI である**——`ADR-0029` が画面制御に ANSI を
/// 選んでいるので、入力の側も同じ表現にしておくと後で噛み合う。
/// **解釈するのはシェルや `zi` であって、コンソールではない**——この列が
/// `Grid` へ届くことはない。
///
/// **受け手は「`\x1b` の直後に `[` が続くか」で区別する。** カーネル側の
/// 保証は 1 つ——**CSI の 3 バイトはここで不可分に組み立てられ、間に他の
/// キーのバイトが挟まらない**（`read_bytes` はイベント 1 つを写してから次を
/// 取り出す。`dst` に入り切らない残りも [`PENDING`] が順序を保って預かる）。
/// したがって受け手が `\x1b` を読んで**次のバイトがすぐ取れないなら、それは
/// Esc 単体である**——人間が Esc と `[` を同じ read に収まる速さで打つことは
/// なく、`zi` はこの規約で足りる（vi と同じ割り切りである）。
/// **不可分の保証があるので、この判別は確定である**——`\x1b` の直後の `read` が
/// `-EAGAIN` を返したら、それは CSI の途中ではありえない。**本物の端末と違い
/// ESC タイムアウトの曖昧さが生じず、`zi` はタイムアウト機構を作らずに済む。**
///
/// # `Char` は ASCII だけを通す
///
/// デコーダは JIS 配列で、非 ASCII を出さない。ここで落とすのは二重の守りである。
/// **`¥` キーが `\` を出すのは、この関門があるためである**
/// （`crate::keyboard::decode` のモジュール doc）。
pub(crate) fn bytes_for_event(event: crate::keyboard::decode::KeyEvent) -> DeliveredBytes {
    use crate::keyboard::decode::KeyEvent;
    match event {
        KeyEvent::Char(character) => {
            if character.is_ascii() {
                DeliveredBytes::Single(character as u8)
            } else {
                DeliveredBytes::None
            }
        }
        KeyEvent::Enter => DeliveredBytes::Single(b'\n'),
        KeyEvent::Backspace => DeliveredBytes::Single(0x08),
        // **Esc は素の 1 バイトである（zi-a）。** CSI に包むと「Esc を押した」が
        // 表せなくなる——`zi` のノーマルモード入りは Esc 単体で起きる。
        KeyEvent::Escape => DeliveredBytes::Single(0x1b),
        // **Delete は `\x1b[3~` である（zi-f）。** **本物の端末と同じ形にする**
        // ——**こちらの都合で 1 バイトを割り当てると、`terminfo` を書く日に
        // 合わなくなる**（C の移植で来る）。
        KeyEvent::Delete => DeliveredBytes::Csi(b"\x1b[3~"),
        KeyEvent::ArrowLeft => DeliveredBytes::Csi(b"\x1b[D"),
        KeyEvent::ArrowRight => DeliveredBytes::Csi(b"\x1b[C"),
        KeyEvent::ArrowUp => DeliveredBytes::Csi(b"\x1b[A"),
        KeyEvent::ArrowDown => DeliveredBytes::Csi(b"\x1b[B"),
        // **Home と End は `\x1b[1~` と `\x1b[4~` である（SE-a。`ADR-0050`）。**
        //
        // **`\x1b[H` と `\x1b[F`（xterm の形）を採らない。** 理由は 3 つある。
        // **(1) `\x1b[3~` と同じ「CSI 数字 ~」の族で揃い、入力を解釈する側の
        // 形が 1 つで済む。** **(2) `\x1b[H` は出力側の CUP と終端が同じで、
        // 次に読む者が入力と出力を取り違える**（`ADR-0029`）。
        // **(3) 端末は ZaytOS 自身なので xterm 互換の利得が無い**——
        // **Delete は全端末で `3~` だが、Home / End は端末によって割れている。**
        KeyEvent::Home => DeliveredBytes::Csi(b"\x1b[1~"),
        KeyEvent::End => DeliveredBytes::Csi(b"\x1b[4~"),
        // **バイトへ落とせないものは落とす。** ファンクションキーやテンキーで、
        // 扱う層がまだ無い（扱うと決めたら decode 側で種を得る。zi-a の形）。
        KeyEvent::Unsupported(_) => DeliveredBytes::None,
    }
}

#[cfg(test)]
mod tests {
    use super::{bytes_for_event, DeliveredBytes};
    use crate::keyboard::decode::KeyEvent;

    /// **Esc は素の 1 バイト、矢印は CSI**——出し分けの規約を固定する（zi-a）。
    #[test]
    fn esc_is_a_bare_byte_and_arrows_are_csi_sequences() {
        assert!(matches!(
            bytes_for_event(KeyEvent::Escape),
            DeliveredBytes::Single(0x1b)
        ));
        assert!(matches!(
            bytes_for_event(KeyEvent::ArrowUp),
            DeliveredBytes::Csi(b"\x1b[A")
        ));
        assert!(matches!(
            bytes_for_event(KeyEvent::ArrowDown),
            DeliveredBytes::Csi(b"\x1b[B")
        ));
        assert!(matches!(
            bytes_for_event(KeyEvent::ArrowLeft),
            DeliveredBytes::Csi(b"\x1b[D")
        ));
        assert!(matches!(
            bytes_for_event(KeyEvent::ArrowRight),
            DeliveredBytes::Csi(b"\x1b[C")
        ));
    }

    /// **Home と End は `\x1b[1~` と `\x1b[4~` である（SE-a。`ADR-0050`）。**
    ///
    /// **`\x1b[3~`（Delete）と同じ「CSI 数字 ~」の族に揃えた**——
    /// **`\x1b[H` を採ると出力側の CUP と終端が同じになる。**
    #[test]
    fn home_and_end_are_csi_number_tilde() {
        assert!(matches!(
            bytes_for_event(KeyEvent::Home),
            DeliveredBytes::Csi(b"\x1b[1~")
        ));
        assert!(matches!(
            bytes_for_event(KeyEvent::End),
            DeliveredBytes::Csi(b"\x1b[4~")
        ));
        // **族が揃っていること自体を主張する。** 3 つとも `~` で終わる。
        for event in [KeyEvent::Delete, KeyEvent::Home, KeyEvent::End] {
            let DeliveredBytes::Csi(bytes) = bytes_for_event(event) else {
                panic!("{event:?} は CSI で届くはずである");
            };
            assert_eq!(bytes.last(), Some(&b'~'), "{event:?} の終端は ~ である");
        }
    }

    /// 既存の 1 バイト系と「落とすもの」が変わっていないこと。
    #[test]
    fn plain_bytes_and_dropped_events_are_unchanged() {
        assert!(matches!(
            bytes_for_event(KeyEvent::Char('a')),
            DeliveredBytes::Single(b'a')
        ));
        assert!(matches!(
            bytes_for_event(KeyEvent::Enter),
            DeliveredBytes::Single(b'\n')
        ));
        assert!(matches!(
            bytes_for_event(KeyEvent::Backspace),
            DeliveredBytes::Single(0x08)
        ));
        assert!(matches!(
            bytes_for_event(KeyEvent::Unsupported(0x47)),
            DeliveredBytes::None
        ));
    }
}

/// 台本を作動させる（zi-d。`zi-test` / `view-test` feature のときだけ効く）。
///
/// **`init` がシェルを起こす直前に呼ぶ。** それより前に流すと、起動シーケンスの
/// 検算（`syscall-test` の 51 番）が台本を食べてしまう（[`script`] の doc）。
pub fn arm_input_script() {
    #[cfg(any(feature = "zi-test", feature = "view-test"))]
    script::arm();
}

/// 決定的な台本入力（zi-d。`zi-test` feature）。
///
/// # なぜ打鍵の注入ではないのか
///
/// **`sendkey` は時間に依存する。** 打鍵の間隔と、行が処理されるまでの待ちを
/// ホスト側が見積もる形になり、**遅い機械では落ちる。** `--shell-test` が
/// その形で、**1 本 42.58 秒掛かる**（実測）。
///
/// **ここは読み手が要求したぶんだけ進む。** `zash` も `zi` も `-EAGAIN` で
/// 回る形のままで、**待ちが要らない。** `ansi-test` を決定的に作ったのと
/// 同じ判断である。
///
/// # スキャンコードのリングへ流し込まない
///
/// **前景が取られるまで、カーネル側の消費者（`drain_keyboard`）が食べてしまう。**
/// リングは 128 バイトでもあり、台本を先に置く形は取れない。
/// **デコード後のバイトを返す層（[`read_bytes`]）へ差し込む。**
#[cfg(any(feature = "zi-test", feature = "view-test"))]
pub(crate) mod script {
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// 台本の現在位置。
    static AT: AtomicUsize = AtomicUsize::new(0);

    /// 台本が作動しているか。**`init` がシェルを起こす直前に立てる。**
    ///
    /// # なぜ最初から流さないのか
    ///
    /// **起動シーケンスの検算が先に `read(0)` を出す**——`syscall-test` の
    /// 51 番が「打鍵が無ければ `-EAGAIN`」を主張しており、**台本を最初から
    /// 流すとあれが台本を 食べてしまい、51 番が落ちる**（実測でそうなった）。
    ///
    /// **作動前は 0 を返す**ので、本物の打鍵の経路（空なので `-EAGAIN`）へ
    /// そのまま落ちる。**検算の主張は変わらない。**
    static ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

    /// 台本を作動させる（`init` がシェルを起こす直前に呼ぶ）。
    pub(crate) fn arm() {
        ARMED.store(true, Ordering::SeqCst);
    }

    /// 台本（zi-d-1）。**`zash` への行と、`zi` への打鍵が 1 本に並ぶ。**
    ///
    /// **矢印は CSI の 3 バイトで書く**（`kernel/src/input.rs` の
    /// `bytes_for_event` が実打鍵から作る形と同じ）。
    ///
    /// 順に——`zi /data/writable` を起こし、**上下でカーソルを動かし**
    /// （zi-a で `zi-d` へ委ねた分担の条件。名指しで含めてある）、
    /// **`hjkl` でも動かし**、`i` で挿入して `Esc` で戻り、`x` で 2 字消し、
    /// **`:wq` で保存して抜け、`cat` で読み戻す**（zi-d-2）。
    ///
    /// **読み戻しはシェルの文脈で行う**——`zi` が書いた内容と `cat` の出力の
    /// 一致を、ホスト側が突き合わせる。**期待値をホストが持たない形である。**
    ///
    /// # 末尾に DIR-1b の 3 行が付いている
    ///
    /// **`tail`（`lseek` の利用者）と `rm`（`unlink` の利用者）と、
    /// 消した後の `ls` である。** **`/bin/ls /data` はこれで 3 回目になる**
    /// ——**1 回目は `fresh` が無い、2 回目は在る、3 回目はまた無い。**
    ///
    /// **`tail` の出力は `cat` の出力の末尾と突き合わせる**ので、
    /// **ホストは期待値を持たない。**
    ///
    /// # 末尾に DIR-1c の 8 行が付いている
    ///
    /// **ディレクトリの一巡である**——`mkdir` → `touch` → `ls` → `cat` →
    /// **空でない `rmdir`（断られる）** → `rm` → `rmdir` → `ls`。
    ///
    /// **`/tmp` は像に在る**（ADR-0042 で「作る」と決めた唯一のもの）。
    ///
    /// **`cat` は空のファイルを読む**ので何も出さない。**単独の判定は
    /// 持たない**——**代わりに「この一巡で失敗したのは断られた `rmdir` の
    /// 1 回だけ」を見る**（`zash` が 0 以外の終了状態を 1 行で報せる）。
    ///
    /// # 末尾に zi-f の 4 行が付いている
    ///
    /// **Enter と Backspace と Delete である。** **新しいファイルを開き、
    /// 3 つを通してから保存し、`cat` で読み戻す。**
    ///
    /// 打つのは `i` `a` `b` Enter `c` `d` `X` Backspace 左 Delete Esc である。
    ///
    /// - `ab` を入れる
    /// - **Enter で行を割る**（`ab` / 空）
    /// - `cdX` を入れる（`ab` / `cdX`）
    /// - **Backspace で `X` を消す**（`ab` / `cd`）
    /// - 左へ 1 つ動く
    /// - **Delete で `d` を消す**（`ab` / `c`）
    ///
    /// **したがって読み戻しは `ab` と `c` の 2 行である。**
    /// **3 つのどれが効かなくても、この 2 行にはならない。**
    ///
    /// # 代替画面の観測点を `zi` を抜けた直後へ移した
    ///
    /// **`\x05`（`OBSERVE_AFTER_ALT`）は台本の末尾に在った。** **DIR-1b で
    /// 3 行足したところ、画面が流れてプロンプトの行が変わり、判定が落ちた**
    /// （実測）。**あの判定は「控えた行に、同じ色のプロンプトが在ること」を
    /// 見ている**（`crate::console::probe`）。
    ///
    /// **主張は変えていない。時点を厳密にしただけである**——**抜けた直後に
    /// 見るほうが、間に何を挟んでも動かない。**
    /// **台本を変えるときは、台本に寄りかかっている判定を数え直すこと**
    /// （`docs/verification-coverage.md`）。
    // **ADR-0046 で `/nope/x` の回を足した。** **`zi` が代替画面に居る間に
    // エラーを出す唯一の道である**——**親のディレクトリが無いので `:w` が
    // 断られる**（`create_and_lookup` が `/nope` を引けない）。
    // **何も打っていないので `dirty` は偽で、`:q` はそのまま抜ける。**
    // **観測（`\x0f`）はエコーエリアを読む**——**エラーがそこに出ていること
    // が主張である**（溜めるだけで描かなければ「見えなくする」と同じになる）。
    //
    // **PERF-g で、インサートの 1 字の前後にも計器の観測点を置いた**
    // （`i\x12Z\x12\x1b`）。**`/data/big` は画面を埋めるので、
    // 編集の描き直しの費用がここで出る。**
    //
    // **PERF-f で、空読み 1 回の前後にも計器の観測点を置いた**
    // （`\x12\x04\x12`）。**`\x04` は 1 回だけ空を返す休みである**——
    // **アプリは `-EAGAIN` を受けて回る。** **その 1 周で画面へ何が起きるかを
    // 測る**（`ADR-0047` で、読むたびに掃く形にしたためである）。
    //
    // **PERF-e で、60 回目の `j` の前後に計器の観測点を置いた**（`\x12`）。
    // **窓が動く 1 行の移動を測るためである**——**48 回目から窓が動くので、
    // 最初の `j` を測っても窓は動かない。** **`j` の数は 60 のままである。**
    //
    // **VIEW-b の前に `k` を 60 足した。** **窓を上へ戻す形が QEMU で一度も
    // 通っていなかった**（台本は下へ 60 行だけだった）。**`follow` の「上へ
    // 出たら先頭にする」枝はホストテストが覆っているが、実機では未通過で
    // あった。** **台本を触る回に一度に払う**（行頭 Backspace のときと同じ）。
    // **観測は 3 つになり、3 つ目は 1 つ目と同じ行に戻るはずである。**
    //
    // **VIEW-a で `/data/big` の回に窓の観測を 2 つ足した。**
    // **`iZ` で先頭行を編集してから、`j` を 60 回送って窓を動かす**
    // ——**本文に使える行数は実測で 48 なので、48 回目から窓が動く。**
    // **観測は動かす前と後の 2 回で、ホストが「先頭行が変わったこと」と
    // 「後のほうがファイルの後ろの行であること」を突き合わせる。**
    #[cfg(feature = "zi-test")]
    const SCRIPT: &[u8] = b"\x01\x06/bin/ls /data\n\
        /bin/zi /data/fresh\n\
        iNEW\x1b\x04\
        :q\n\x0b\
        :wq\n\
        /bin/ls /data\n\
        /bin/cat /data/fresh\n\
        /bin/zi /data/lines\n\
        \x1b[B\x1b[B\x1b[A\
        jjkk\
        \x1b[C\x1b[D\
        lh\x02\x13\
        iZY\x02\x1b\x04\x02\
        xx\
        j\
        aQ\x1b\x04\x14\
        :w\x07q\n\x05\
        /bin/cat /data/lines\n\
        /bin/tail /data/lines\n\
        /bin/rm /data/fresh\n\
        /bin/ls /data\n\
        /bin/mkdir /tmp/box\n\
        /bin/touch /tmp/box/note\n\
        /bin/ls /tmp/box\n\
        /bin/cat /tmp/box/note\n\
        /bin/rmdir /tmp/box\n\
        /bin/rm /tmp/box/note\n\
        /bin/rmdir /tmp/box\n\
        /bin/ls /tmp\n\
        /bin/zi /data/edited\n\
        iab\ncdX\x08\x1b[D\x1b[3~\x1b\x04\
        :wq\n\
        /bin/cat /data/edited\n\
        /bin/zi /data/joined\n\
        iab\ncd\x1b[D\x1b[D\x08\x1b\x04\
        :wq\n\
        /bin/cat /data/joined\n\
        /bin/cat /data/big\n\
        /bin/zi /data/big\n\
        i\x12Z\x12\x1b\x04\x0e\x12\
        jjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjj\
        \x12j\x12\x0e\x12\x04\x12\
        kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk\x0e\
        :wq\n\
        /bin/cat /data/big\n\
        /bin/zi /nope/x\n\
        :w\n\x0f\
        :q\n\x0c";

    /// 台本（VIEW-b）。**`less` を駆動する。**
    ///
    /// # `zi-test` と分けてある
    ///
    /// **`zi-test` は21秒掛かり、破壊19構成すべてに掛かる**（実測）。
    /// **`less` の打鍵をあちらへ足すと、19回ぶん伸びる。** **別に立てて、
    /// `less` の破壊だけがこちらへ掛かる形にする**（運用者の承認。VIEW-b の 4-4）。
    ///
    /// # 何を見せるか
    ///
    /// 1. **`/data/big` を `cat` で撮る**——**期待値をホストが持たない**
    ///    （画面に出た行が、`cat` の出した並びのどこに在るかで見る）
    /// 2. **入る前の画面を控え**（`\x06`）、**`less /data/big` を起こす**
    /// 3. **窓の上端と下端を観測する**（`\x10`）——**ファイルの先頭行が出ている**
    /// 4. **`Space` で1画面ぶん下げ、また観測する**——**窓の外に在った行が
    ///    見えるようになったこと**が主張である。
    ///    **前後で描画の計器も撮る**（`\x12`。PERF）——**1画面の移動に
    ///    `write` が何回、字が何回、転送が何回・何バイト掛かるか。**
    ///    **続けて `j` の前後でも撮る**——**1行の移動と1画面の移動を比べる。**
    /// 5. **`j` を3回、`k` を3回、`b` で1画面ぶん戻して観測する**
    ///    ——**3 と同じ行に戻るはずである**（`j` と `k` が釣り合い、
    ///    `b` が `Space` を打ち消す）
    /// 6. **`q` で抜け、戻った画面を控えたものと突き合わせる**（`\x05`）
    /// 7. **`cat` をもう一度撮る**——**`less` が像を変えていないこと**
    /// 8. **`more /data/big` を 2 回起こす。** **1 回目は `Space` の後に `q` で
    ///    抜け**（**打ち切る道**）、**2 回目は `Space` を 2 回送って読み切らせる**
    ///    （**`more` が自分で終わる道**。`/data/big` は 100 行で、1 画面は 49 行
    ///    なので 3 画面である）。**抜けた後の画面の下 3 行を観測する**（`\x11`）。
    ///    **`less` と逆の主張である**——**出したものが残っている。**
    ///
    ///    **打鍵の空白は `\x20` と書く。** **行頭に空白を書くと、Rust の行継続が
    ///    落とす**（実測。**`\` の次の行の先頭の空白は消える**）。
    /// 9. **`cat` をもう一度撮る**——**`more` も像を変えていないこと**
    ///
    /// **観測点の値は `zi-test` の台本と同じものを使う**（同じ表を引く）。
    #[cfg(feature = "view-test")]
    const SCRIPT: &[u8] = b"\x01/bin/cat /data/big\n\
        \x06/bin/less /data/big\n\
        \x10\x12 \x12\x10\
        \x12j\x12\
        jjkkkb\x10\
        q\x05\
        /bin/cat /data/big\n\
        /bin/more /data/big\n\x20q\
        /bin/more /data/big\n\x12\x20\x12\x20\x11\
        /bin/cat /data/big\n\x0c";

    /// 観測点（ES-d）。**プロンプトの色を見る。**
    const OBSERVE_PROMPT: u8 = 0x01;
    /// 観測点（ES-d）。**`zi` の状態行を見る。**
    const OBSERVE_STATUS: u8 = 0x02;
    /// 観測点（e-3）。**代替画面へ入る前の画面を控える。**
    const OBSERVE_BEFORE_ALT: u8 = 0x06;
    /// 観測点（e-3）。**代替画面から戻った画面を、控えたものと突き合わせる。**
    ///
    /// # 台本の最後に置く
    ///
    /// **`:wq` の直後には置けない。** **観測の出力はシリアルへ出るので、
    /// プロンプトと、その後に反響されるコマンドの間へ割り込む**——
    /// **`zaytos$ /bin/cat /data/lines` を目印にしている判定が、
    /// 割られた瞬間に当たらなくなる**（実測でそうなった）。
    ///
    /// **最後に置いても主張は変わらない。** 戻った画面はそのまま残っており、
    /// **控えた行と桁を読み直すだけである**（`cat` の出力は下の行へ足される）。
    const OBSERVE_AFTER_ALT: u8 = 0x05;
    /// 観測点（e-4）。**コマンド行（最下行）に打っている途中が出ているか。**
    const OBSERVE_COMMAND_LINE: u8 = 0x07;
    /// 観測点（e-5）。**コマンド行に報せ（断った理由）が出ているか。**
    ///
    /// **`0x08` は使わない**——**Backspace がそのバイトで届く**
    /// （`bytes_for_event`）。**台本の中でしか使わないので衝突はしないが、
    /// 読む人が入力と取り違える。**
    const OBSERVE_MESSAGE: u8 = 0x0b;
    /// 台本の終わり（DIR-1b）。**ホスト側の待ちの合図である。**
    ///
    /// **主張を持つ観測点を合図に使わない**（`crate::console::probe` の
    /// `Observation::ScriptDone`）。
    const OBSERVE_DONE: u8 = 0x0c;
    /// 観測点（PERF-g）。**本文の先頭 6 行を画面から読む。**
    const OBSERVE_TEXT_ROWS: u8 = 0x14;
    /// 観測点（PERF-b の後）。**画面の側のカーソルの位置。**
    const OBSERVE_CURSOR: u8 = 0x13;
    /// 観測点（PERF）。**描画の層ごとの数。** **差分で読む。**
    const OBSERVE_DRAW_STATS: u8 = 0x12;
    /// 観測点（VIEW-c）。**`more` が抜けた後の画面の下 3 行。**
    const OBSERVE_MORE_OUTPUT: u8 = 0x11;
    /// 観測点（VIEW-b）。**`less` の窓の上端と、いちばん下の本文行。**
    const OBSERVE_VIEW_WINDOW: u8 = 0x10;
    /// 観測点（ADR-0046）。**エコーエリア（最下行）にエラーが出ているか。**
    ///
    /// **`0x0b`（[`Self::OBSERVE_MESSAGE`]）と読むものは同じで、名前だけが
    /// 違う**——**判定がどちらの主張かを見分けるためである**
    /// （`crate::console::probe` の `Observation::EchoArea`）。
    const OBSERVE_ECHO: u8 = 0x0f;
    /// 観測点（VIEW-a）。**`zi` の本文の先頭行に何が出ているか。**
    ///
    /// **`0x0e` を使う。** **`0x09`（Tab）・`0x0a`（LF）・`0x0d`（CR）は
    /// 打鍵として届きうるので避ける。** **`0x03` は Ctrl+C である。**
    const OBSERVE_ZI_WINDOW: u8 = 0x0e;
    /// 休み（e-2）。**その `read` は何も返さない**（`-EAGAIN` になる）。
    ///
    /// # 何のために在るのか
    ///
    /// **台本が作動している間、`read` は必ず 1 バイトを返す。** つまり
    /// **読み手は「入力が途切れた」状態を一度も見ない。**
    /// **`zi` の Esc の確定はまさにそこで起きる**（`-EAGAIN` を受けたら
    /// Esc 単体と確定する。`kernel/userland/zi.rs`）ので、
    /// **途切れを作らないと、その経路が一度も通らない。**
    ///
    /// **1 回の `read` だけを空にする。** 次の `read` は台本の続きを返す。
    ///
    /// # 値の選び方
    ///
    /// **`0x04` は打鍵から作られないバイトである**（`bytes_for_event` が
    /// 返すのは字と `0x08` と `0x03` と CSI である）。**そもそも Ring 3 へは
    /// 届けない**ので衝突しないが、**読む人が「これは入力ではない」と
    /// 分かる値を選んである。**
    const SCRIPT_PAUSE: u8 = 0x04;

    /// 台本の中の観測点か（ES-d）。
    ///
    /// **観測点は入力ではない。** [`next_bytes`] が食べて、Ring 3 へは
    /// 届けない。**打鍵として届くバイトと衝突しない値を選んである**——
    /// `0x01` と `0x02` は `bytes_for_event` がどのキーからも作らない。
    fn observation_at(byte: u8) -> Option<crate::console::probe::Observation> {
        match byte {
            OBSERVE_ZI_WINDOW => Some(crate::console::probe::Observation::ZiWindow),
            OBSERVE_ECHO => Some(crate::console::probe::Observation::EchoArea),
            OBSERVE_VIEW_WINDOW => Some(crate::console::probe::Observation::ViewWindow),
            OBSERVE_MORE_OUTPUT => Some(crate::console::probe::Observation::MoreOutput),
            OBSERVE_DRAW_STATS => Some(crate::console::probe::Observation::DrawStats),
            OBSERVE_CURSOR => Some(crate::console::probe::Observation::CursorCell),
            OBSERVE_TEXT_ROWS => Some(crate::console::probe::Observation::TextRows),
            OBSERVE_PROMPT => Some(crate::console::probe::Observation::Prompt),
            OBSERVE_STATUS => Some(crate::console::probe::Observation::Status),
            OBSERVE_BEFORE_ALT => Some(crate::console::probe::Observation::BeforeAlternate),
            OBSERVE_AFTER_ALT => Some(crate::console::probe::Observation::AfterAlternate),
            OBSERVE_COMMAND_LINE => Some(crate::console::probe::Observation::CommandLine),
            OBSERVE_MESSAGE => Some(crate::console::probe::Observation::Message),
            OBSERVE_DONE => Some(crate::console::probe::Observation::ScriptDone),
            _ => None,
        }
    }

    /// 台本の残りを `dst` へ写す。**返した数が 0 なら台本は尽きている。**
    ///
    /// **観測点は先に食べる（ES-d）。** **ここへ来たということは、読み手が
    /// それまでの入力を処理し終えて次を要求したということ**なので、
    /// **画面はその時点で最新である**（`crate::console::probe` の doc）。
    ///
    /// **休み（e-2）は 0 を返して終わる。** **「台本が尽きた」と同じ返り値だが、
    /// 位置は進めてある**ので、次の呼び出しは続きを返す。
    pub(crate) fn next_bytes(dst: &mut [u8]) -> usize {
        if !ARMED.load(Ordering::SeqCst) {
            return 0;
        }
        let mut at = AT.load(Ordering::SeqCst);
        while let Some(kind) = SCRIPT.get(at).copied().and_then(observation_at) {
            crate::console::probe::observe(kind);
            at += 1;
        }
        if at >= SCRIPT.len() {
            AT.store(at, Ordering::SeqCst);
            return 0;
        }
        // **休み（e-2）。** **1 回だけ空を返す**——読み手に「入力が途切れた」を
        // 見せるためである（[`SCRIPT_PAUSE`] の doc）。
        if SCRIPT[at] == SCRIPT_PAUSE {
            AT.store(at + 1, Ordering::SeqCst);
            return 0;
        }
        // **次の観測点の手前までしか渡さない。** 1 度に多くを求められても、
        // **観測点を跨いで渡すと、見るはずだった時点を通り過ぎる。**
        let until = SCRIPT[at..]
            .iter()
            .position(|byte| observation_at(*byte).is_some() || *byte == SCRIPT_PAUSE)
            .map_or(SCRIPT.len(), |offset| at + offset);
        let take = dst.len().min(until - at);
        dst[..take].copy_from_slice(&SCRIPT[at..at + take]);
        AT.store(at + take, Ordering::SeqCst);
        take
    }
}
