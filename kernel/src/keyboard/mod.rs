//! PS/2 キーボード。
//!
//! - [`decode`][mod@decode]: スキャンコードから文字への変換（純粋ロジック）。
//!   ハードウェアに一切触れないので、実機で問題が出たときに
//!   「デコードは正しいのだからハードウェア側だ」と切り分けられる。
//! - [`controller`][mod@controller]: i8042 の検証とポートアクセス。
//! - [`buffer`][mod@buffer]: 受け取ったスキャンコードのリングバッファ。

pub mod buffer;
pub mod controller;
pub mod decode;

use core::sync::atomic::{AtomicU64, Ordering};

/// キーボード（IRQ1）の 8259 でのベクタ。
///
/// これは 8259 の採番表が与える値であって、現在どこへ届くかではない。
/// IRQ1 は I/O APIC 経由へ移してあるので、実際の配送先は
/// [`crate::idt::IOAPIC_KEYBOARD_VECTOR`] である。
/// 現在の配送先は [`delivery_vector`] で得る。
pub const PIC_KEYBOARD_VECTOR: usize = match crate::irq::vector_for(KEYBOARD_IRQ) {
    Some(vector) => vector as usize,
    None => panic!("the keyboard IRQ has no vector"),
};

/// キーボード割り込みが現在届くベクタ。
///
/// 経路の切り替えで変わるので `const` にできない。
pub fn delivery_vector() -> usize {
    match crate::irq::routed_vector(KEYBOARD_IRQ) {
        Some(vector) => vector as usize,
        None => PIC_KEYBOARD_VECTOR,
    }
}

/// キーボードの IRQ 番号。
pub const KEYBOARD_IRQ: u8 = 1;

/// 最初のキー入力が届いたベクタ番号。まだなら [`NO_VECTOR_YET`]。
///
/// 配送経路の証拠になる。I/O APIC 経由へ移してからは 8259 が出しえない
/// ベクタになるので、値そのものが経路を示す。
static FIRST_KEYBOARD_VECTOR: AtomicU64 = AtomicU64::new(NO_VECTOR_YET);

/// 「まだ届いていない」を表す番兵。
pub const NO_VECTOR_YET: u64 = u64::MAX;

/// 最初のキー入力が届いたベクタ番号。
pub fn first_keyboard_vector() -> Option<u64> {
    match FIRST_KEYBOARD_VECTOR.load(Ordering::Relaxed) {
        NO_VECTOR_YET => None,
        vector => Some(vector),
    }
}

/// IRQ1 のハンドラ本体。割り込みハンドラから呼ばれる。
///
/// 出力しない（ADR-0018 §5）。共有状態を更新するだけで、表示はメインループが行う。
/// EOI は呼び出し元（`idt::irq_entry`）がこの関数から戻った後に送る。
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
pub(crate) fn handle_irq(vector: u64) {
    // 最初の 1 回だけベクタ番号を記録する。
    let _ = FIRST_KEYBOARD_VECTOR.compare_exchange(
        NO_VECTOR_YET,
        vector,
        Ordering::Relaxed,
        Ordering::Relaxed,
    );

    HANDLER_INVOCATIONS.fetch_add(1, Ordering::Relaxed);

    // データを伴う割り込みかどうかを、読む前に見ておく。
    let has_data = controller::output_buffer_full();

    // 必ず読む。条件分岐の後ろに置いてはならない（doc の「必ず読み切ること」）。
    // 読み飛ばす分岐を作らないことで、「満杯だから読まない」が将来入り込む余地を消す。
    // SAFETY: 0x60 の読み出しは 1 バイト消費するだけ。値は下で処理する。
    let code = unsafe { controller::read_data() };

    if has_data {
        // **中断（Ctrl+C）を、積むより先に見る（S12 前の手当て、C）。**
        //
        // **リングは前景が取られている間ずっと溜まる一方である**——
        // **デコードするのは `input::read_bytes` だけで、あれは Ring 3 が
        // `read` を出したときにしか動かない。** **止めたい相手は `read` を
        // 出さずに回っている子なので、リング越しには永久に見えない。**
        //
        // **したがってここで見る。** 積むかどうかとは独立なので、
        // **溢れて捨てられるバイトでも中断は拾える。**
        crate::input::note_scancode_for_interrupt(code);
        // 積めなければ捨てて数える。読み出しは既に済んでいるので、
        // 捨てても IRQ1 は止まらない。
        buffer::record(code);
    } else {
        STRAY_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// データを伴わなかった IRQ1 の回数。判定基準は [`handle_irq`]。
static STRAY_COUNT: AtomicU64 = AtomicU64::new(0);

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
