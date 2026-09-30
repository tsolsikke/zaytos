//! PS/2 キーボード。
//!
//! - [`decode`][mod@decode]: スキャンコードから文字への変換（純粋ロジック）。
//!   ハードウェアに一切触れないので、実機で問題が出たときに
//!   「デコードは正しいのだからハードウェア側だ」と切り分けられる。
//! - i8042 の検証とポートアクセスは、機械に固有なので [`crate::machine::pc::i8042`] に置く。
//! - [`buffer`][mod@buffer]: 受け取ったスキャンコードのリングバッファ。

pub mod buffer;
pub mod decode;

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::interrupts::InterruptSource;
use crate::machine::pc::{keyboard_data_ready, read_keyboard_data, IsaIrq};

/// キーボード（i8042）が上げる ISA の IRQ（`ADR-0072` の 7 の PS/2）。
///
/// **ベクタはここに置かない**（2026-09-29。9e。`ADR-0072` の 3。ベクタを共通の側に出さない）。以前は 8259 での
/// ベクタ（`PIC_KEYBOARD_VECTOR`）と今の配送先（`delivery_vector`）をここで持っていた。今の配送先は `machine` が
/// 引く（`crate::machine::pc::irq::delivery_vector`）。
pub const KEYBOARD_IRQ: IsaIrq = IsaIrq::new(1);

/// キーボードの割り込みの源の番号（`machine` が [`KEYBOARD_IRQ`] から解決する。`ADR-0072` の 7）。
pub fn interrupt_source() -> InterruptSource {
    crate::machine::pc::source_for_isa_irq(KEYBOARD_IRQ)
}

/// i8042 を確かめて IRQ1 を開けたか（HW-b。`ADR-0068`）。**`setup_keyboard` が開ける直前に立てる。**
///
/// **立っていなければ、ポート 0x60/0x64 を読まない**——**FADT が「無い」と示した機械では
/// 探らないと決めた**（`ADR-0068` の HW-b）ので、心拍の行も読まない。**IRQ1 の期待
/// （`sti` 前の検証）もこれで決まる。**
static CONTROLLER_PRESENT: AtomicBool = AtomicBool::new(false);

/// i8042 を確かめたことを記録する。**起動の順路で 1 回だけ呼ぶ。**
pub fn mark_controller_present() {
    CONTROLLER_PRESENT.store(true, Ordering::Relaxed);
}

/// i8042 を確かめて IRQ1 を開けたか。
pub fn controller_present() -> bool {
    CONTROLLER_PRESENT.load(Ordering::Relaxed)
}

/// IRQ1 の処理（`handle_irq`）を、割り込みの処理の表へ登録する（`ADR-0072` の 5。2026-09-28）。
///
/// **起動の途中で、IRQ1 を開ける前に 1 回だけ呼ぶ**（処理を登録してから源を許可する）。起動の後に呼ぶと、
/// 登録する側（[`crate::interrupts::register_interrupt_handler`]）が名前つきで止める。
pub fn register_irq_handler() {
    crate::interrupts::register_interrupt_handler(interrupt_source(), handle_irq);
}

/// [`report_first_delivery_once`] が既に出したか。
static FIRST_DELIVERY_REPORTED: AtomicBool = AtomicBool::new(false);

/// 最初のキー入力が届いたベクタを 1 度だけ報せる（IRQ1 の配送経路の証明。HW-e-2。`ADR-0068`）。
///
/// **行の境目から呼ぶ**——**起動前のループと、プログラムを起動する入口（`userland::spawn`）の 2 か所である。**
/// **以前は起動前のループでしか出なかったので、シェルが起動してから打つ機械（VirtualBox の実行）では配送の
/// 証拠が残らなかった。**
///
/// # 読み手の中では出さない
///
/// **最初は端末の `read` と入力の fd の読み手から出していた**が、**シェルがエコーしている行の途中へ
/// 割り込み、`--full` の打鍵の検査 4 本が「打った行がエコーされた」で落ちた**（2026-09-24）。
/// **起動する入口はシェルが Enter のエコーを終えた後なので、行の途中に入らない。** **パスを引く前に呼ぶ
/// ので、無い名前（`a`）を打った回でも出る。**
///
/// **違うベクタで届いていたら止める**（今までどおり）。**8259 経由（0x21）なら、I/O APIC へ移したはずの
/// IRQ1 が 8259 から来たことになる。**
///
/// **最初の到着を記録し、今の配送先と比べるのは `machine` である**（`crate::machine::pc::first_arrival`。2026-09-29。
/// 9e。ベクタを共通の側に出さない）。記録は処理のある源の完了のときだけなので、処理を登録しなかった回
/// （破壊テスト `keyboard-handler-not-registered-test`）では、今までどおりこの行は出ない。
pub fn report_first_delivery_once(logger: &mut common::log::Logger<common::machine::pc::Serial>) {
    let Some(first) = crate::machine::pc::first_arrival(interrupt_source()) else {
        return;
    };
    if FIRST_DELIVERY_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    if first.arrived_on_the_current_route() {
        logger.info(format_args!(
            "keyboard: first key arrived as {first} - IRQ1 is wired through our stub correctly"
        ));
    } else {
        logger.error(format_args!(
            "keyboard: the first key arrived as {}; halting",
            first.mismatch()
        ));
        common::arch::x86_64::cpu::halt_forever();
    }
}

/// IRQ1 の処理の本体。割り込みの処理の表から呼ばれる（[`register_irq_handler`] が登録する。`ADR-0072` の 5）。
///
/// 出力しない（ADR-0018 §5）。共有状態を更新するだけで、表示はメインループが行う。
/// 完了（EOI）は、共通の側の入口関数（`crate::interrupts::on_external_interrupt`）が、この関数から戻った後に
/// `machine` へ頼む。
///
/// # データポートを必ず読み切ること
///
/// リングバッファが満杯でも、データポート 0x60 は必ず読む。i8042 の出力バッファを
/// 空にしないとコントローラは次の IRQ1 を上げない。症状は「1 回だけ動いて止まる」で、
/// EOI 忘れと見分けがつかない。
///
/// そのため読む → 積む（満杯なら捨てて数える）の順に固定してある。
/// 積めるかどうかの判断より前に読み終えているので、どの経路でも出力バッファは空く。
///
/// # データを伴わない IRQ1 を弾く
///
/// i8042 は OBF が立つたびに IRQ1 を上げるが、これはキー入力に限らない。
/// コントローラ宛コマンドへの応答でも OBF は立つ。その割り込みは IRQ1 をマスクして
/// いる間に PIC の IRR へラッチされ、マスクを外した瞬間に 1 回だけ配送される。
/// 応答バイト自体は既に読み終えているので、ハンドラが 0x60 を読んでも意味のある値は
/// 返らない（起動直後に押していないキーのコード `0x67` が 1 回現れる事象を観測した）。
///
/// そこで読む前に OBF を見る。立っていなければ読んだ値は捨てて
/// [`stray_irq_count`] で数える。読み出し自体は省かない。
///
/// この判定にはレースがある。OBF を読んで「立っていない」と判断した直後、
/// 0x60 を読む前に本物のキーが届くと stray として捨てられる。
/// 既知の限界として受け入れ、修正しない（`docs/deferred-decisions.md`）。
///
/// # stray の判定基準
///
/// - 起動直後に 0 回または 1 回: 正常（実測で run ごとに揺れる。同一構成の 4 回で 1,1,1,0）
/// - 起動後、キーを打っていないのに増え続ける: 異常
///
/// 揺れる機序は突き止めていない（`docs/deferred-decisions.md`）。常に 0 にすると
/// 「増え続けるか」という指標を失うので、揺れたまま使う。
fn handle_irq(_source: InterruptSource) {
    HANDLER_INVOCATIONS.fetch_add(1, Ordering::Relaxed);

    // データを伴う割り込みかどうかを、読む前に見ておく。
    let has_data = keyboard_data_ready();

    // 必ず読む。条件分岐の後ろに置いてはならない（doc の「必ず読み切ること」）。
    // 読み飛ばす分岐を作らないことで、「満杯だから読まない」が将来入り込む余地を消す。
    // SAFETY: 0x60 の読み出しは 1 バイト消費するだけ。値は下で処理する。
    let code = unsafe { read_keyboard_data() };

    if has_data {
        // **中断（Ctrl+C）を、積むより先に見る（S12 前の手当て、C）。**
        //
        // **リングは前景が取られている間ずっと溜まる一方である**——
        // **デコードするのは `input::read_bytes` だけで、あれは Ring 3 が
        // `read` を出したときにしか動かない。** **止めたい相手は `read` を
        // 出さずに空回りしている子なので、リング越しには永久に見えない。**
        //
        // **したがってここで見る。** 積むかどうかとは独立なので、
        // **溢れて捨てられるバイトでも中断は拾える。**
        crate::input::note_scancode_for_interrupt(code);
        // **積む前に「待っている者が居るか」を見る（W2-c-2）。**
        //
        // **積んだ後では遅い**——**起こした側が `Ready` にしてしまうので、「積んだ時点で
        // 待っていた」が読めなくなる。** **積んだ時点の事実は、積む側にしか分からない。**
        let someone_was_waiting = crate::task::someone_waits_on(crate::task::Wait::Keyboard);
        // 積めなければ捨てて数える。読み出しは既に済んでいるので、
        // 捨てても IRQ1 は止まらない。
        buffer::record(code);
        // **待っている者を起こす（W2-c-2。`ADR-0061` の決定 4）。**
        //
        // **積んだ直後に起こす。** **デコードの結果では起こせない**——**デコードは
        // `input::read_bytes` の中、すなわち読み手の文脈でしか動かない。**
        // **したがって空振りの起床が起きる**（離鍵など、バイトにならないコード）。
        // **起こされた側は読めなければまた待つ**ので、それでよい。
        //
        // **IF=0 かつ BKL の内側である**（割り込みゲート経由。`irq_entry` が取っている）
        // ——**切り替えが状態を書くのと同じ文脈である。**
        //
        // 破壊テスト (W2-c-2, keyboard-does-not-wake): 起こさない。**積んだ数と起こした数の関係が
        // 食い違い、判定 4 が落ちる**（時間の上限を待たずに、1 回目の打鍵で出る）。
        #[cfg(not(feature = "keyboard-does-not-wake"))]
        let woken = crate::task::wake_tasks_waiting_on(crate::task::Wait::Keyboard);
        #[cfg(feature = "keyboard-does-not-wake")]
        let woken = 0usize;
        // **積んだのに起こさなかった回数を数える（W2-c-2 の関係の検出器）。**
        //
        // **これが「起こさない」の主たる検出である**——**時間に依らない。**
        // **待っている者が居たのに 1 本も起こさなかったら、起こす経路が壊れている。**
        if someone_was_waiting && woken == 0 {
            PUSHED_WITHOUT_WAKING.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        STRAY_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// データを伴わなかった IRQ1 の回数。判定基準は [`handle_irq`]。
static STRAY_COUNT: AtomicU64 = AtomicU64::new(0);

/// 待っている者が居たのに、1 本も起こさなかった回数（W2-c-2 の関係の検出器）。
///
/// **本番では 0 でなければならない。** **0 でなければ、起こす経路が壊れている**
/// ——**上限の時間を待たずに、1 回目の打鍵で出る**（`ADR-0061`。時間の判定を避ける）。
static PUSHED_WITHOUT_WAKING: AtomicU64 = AtomicU64::new(0);

/// 待っている者が居たのに起こさなかった回数（W2-c-2）。**本番では 0 である。**
pub fn pushed_without_waking() -> u64 {
    PUSHED_WITHOUT_WAKING.load(Ordering::Relaxed)
}

/// ハンドラが呼ばれた回数。
///
/// 会計を閉じるために持つ。`handler_invocations == received + stray` が常に
/// 成り立たなければ、どこかで経路を取り違えている。`idt` 側のベクタ別カウンタとも
/// 一致するので、3 つの数字が互いを検算する。
static HANDLER_INVOCATIONS: AtomicU64 = AtomicU64::new(0);

/// ハンドラが呼ばれた回数。
pub fn handler_invocations() -> u64 {
    HANDLER_INVOCATIONS.load(Ordering::Relaxed)
}

/// 会計が閉じているか。`受け取った数 + stray == ハンドラ呼び出し回数`。
pub fn accounting_balances() -> bool {
    buffer::received_count() + stray_irq_count() == handler_invocations()
}

/// データを伴わなかった IRQ1 の回数。
pub fn stray_irq_count() -> u64 {
    STRAY_COUNT.load(Ordering::Relaxed)
}
