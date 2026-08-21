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
    #[cfg(feature = "zi-test")]
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
    /// CSI の 3 バイト列。**不可分に届ける**（下の doc）。
    Csi(&'static [u8; 3]),
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
/// デコーダは US 配列で、非 ASCII を出さない。ここで落とすのは二重の守りである。
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
        KeyEvent::ArrowLeft => DeliveredBytes::Csi(b"\x1b[D"),
        KeyEvent::ArrowRight => DeliveredBytes::Csi(b"\x1b[C"),
        KeyEvent::ArrowUp => DeliveredBytes::Csi(b"\x1b[A"),
        KeyEvent::ArrowDown => DeliveredBytes::Csi(b"\x1b[B"),
        // **バイトへ落とせないものは落とす。** Home / End などで、
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

/// 台本を作動させる（zi-d。`zi-test` feature のときだけ効く）。
///
/// **`init` がシェルを起こす直前に呼ぶ。** それより前に流すと、起動シーケンスの
/// 検算（`syscall-test` の 51 番）が台本を食べてしまう（[`script`] の doc）。
pub fn arm_input_script() {
    #[cfg(feature = "zi-test")]
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
#[cfg(feature = "zi-test")]
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
    const SCRIPT: &[u8] = b"\x01/bin/zi /data/lines\n\
        \x1b[B\x1b[B\x1b[A\
        jjkk\
        \x1b[C\x1b[D\
        lh\x02\
        iZY\x02\x1b\x04\x02\
        xx\
        :wq\n\
        /bin/cat /data/lines\n";

    /// 観測点（ES-d）。**プロンプトの色を見る。**
    const OBSERVE_PROMPT: u8 = 0x01;
    /// 観測点（ES-d）。**`zi` の状態行を見る。**
    const OBSERVE_STATUS: u8 = 0x02;
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
            OBSERVE_PROMPT => Some(crate::console::probe::Observation::Prompt),
            OBSERVE_STATUS => Some(crate::console::probe::Observation::Status),
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
