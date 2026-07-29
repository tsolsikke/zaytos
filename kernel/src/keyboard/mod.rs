//! PS/2 キーボード（M4-e）。
//!
//! - [`decode`][mod@decode]: スキャンコードから文字への変換（**純粋ロジック**、
//!   ホスト `cargo test` で検証）。ハードウェアに一切触れない。
//!
//! ハードウェア依存部（i8042 の検証・IRQ1 ハンドラ・リングバッファ）は
//! M4-e の後半で追加する。先に純粋ロジックだけを完成させておけば、実機で
//! 問題が出たときに「デコードは正しいのだからハードウェア側だ」と切り分け
//! られる。

pub mod buffer;
pub mod controller;
pub mod decode;

use core::sync::atomic::{AtomicU64, Ordering};

/// キーボード（IRQ1）の**8259 での**ベクタ。PIC のベクタ採番に追随する。
///
/// # これは現在の配送先とは限らない（S2-d-1c）
///
/// **名前が事実と食い違わないよう改名した**（旧 `KEYBOARD_VECTOR`）。
/// S2-d-1c で IRQ1 を I/O APIC 経由へ移すと、実際の配送先は
/// [`crate::idt::IOAPIC_KEYBOARD_VECTOR`] になる。この定数はあくまで
/// **8259 の採番表が与える値**であって、現在どこへ届くかではない。
///
/// 現在の配送先を知りたい場合は [`delivery_vector`] を使うこと。
pub const PIC_KEYBOARD_VECTOR: usize = match crate::irq::vector_for(KEYBOARD_IRQ) {
    Some(vector) => vector as usize,
    None => panic!("the keyboard IRQ has no vector"),
};

/// キーボード割り込みが**現在**届くベクタ。
///
/// 移行前は [`PIC_KEYBOARD_VECTOR`]、移行後は
/// [`crate::idt::IOAPIC_KEYBOARD_VECTOR`] である。**実行時に決まる**ので
/// `const` にできない。
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
/// **IRQ1 の配送経路が正しいことの証明になる。** タイマで 0x20 を確認したのと
/// 同じ趣旨で、こちらは実値で確かめる。**S2-d-1c 以降は 8259 が出しえない
/// ベクタになるので、この値がそのまま配送経路の証拠になる。**
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

/// IRQ1 のハンドラ本体。**割り込みハンドラから呼ばれる。**
///
/// # データポートを必ず読み切ること
///
/// **リングバッファが満杯でも、データポート 0x60 は必ず読む。** i8042 の
/// 出力バッファを空にしないと、コントローラは次の IRQ1 を上げない。症状は
/// 「1 回だけ動いて止まる」で、**EOI 忘れと見分けがつかない**。
/// 「満杯だから読まない」は書いてはならない。
///
/// そのため、読む → 積む（満杯なら捨てて数える）の順に固定してある。
/// 積めるかどうかの判断より前に読み終えているので、どの経路を通っても
/// 出力バッファは空く。
///
/// # データを伴わない IRQ1 を弾く
///
/// **IRQ1 が上がっても、出力バッファが空のことがある。** i8042 は OBF が
/// 立つたびに IRQ1 を上げるが、これは**キー入力に限らない**。コントローラ宛の
/// コマンド（こちらが送るコンフィグバイトの読み出しなど）への応答でも OBF は
/// 立ち、IRQ1 が上がる。その割り込みは IRQ1 をマスクしている間に PIC の IRR へ
/// ラッチされ、**マスクを外した瞬間に 1 回だけ配送される**。
///
/// このとき応答バイト自体は既にこちらが読み終えているので、ハンドラが
/// 0x60 を読んでも意味のある値は返らない。実際、起動直後に `0x67` という
/// 押してもいないキーのコードが 1 回だけ現れる事象を観測した。
///
/// そこで**読む前に OBF を見る**。立っていなければデータを伴わない割り込みで
/// あり、読んだ値は捨てて専用のカウンタで数える。**読み出し自体は省かない**
/// （省く条件分岐を入れると、そこが将来「満杯なら読まない」に育ちうる）。
///
/// **この判定にはレースがある。** i8042 は外部のハードウェアなので、割り込み
/// 禁止中でも OBF を立てられる。OBF を読んで「立っていない」と判断した直後、
/// 0x60 を読む前に本物のキーが届くと、そのキーは stray として捨てられる。
/// 窓は数命令ぶんで実際に起きる確率は極めて低く、起きても
/// [`stray_irq_count`] が増えるので観測はできる。**既知の限界として受け入れ、
/// 修正しない**（`docs/deferred-decisions.md`）。
///
/// ## 起動直後の stray は 0 回か 1 回（タイミング依存）
///
/// **実測では run ごとに 0 と 1 が入れ替わる**（同一構成の 4 回で 1,1,1,0）。
/// 起動シーケンス中に IRQ1 が PIC の IRR へラッチされるかどうかが、
/// ドレインとマスク解除の間に入る動作のタイミングで変わるためと考えている。
/// **ただしこれは仮説であり、機構を突き止めてはいない。** 断定しないこと。
///
/// 判定基準として使えるのは次の形である。
///
/// - 起動直後に 0 回または 1 回: 正常
/// - 起動後、キーを打っていないのに増え続ける: 異常
///   （i8042 の扱いがどこか噛み合っていない）
///
/// コマンド送信中だけキーボード割り込みを無効化すればラッチを防げる見込みは
/// あるが、対処しない。値が 0/1 で揺れても「増え続けるかどうか」という
/// 指標としては機能し、常に 0 にして異常検出の手掛かりを失うより価値が
/// あると判断した。
///
/// EOI は呼び出し元（`idt::irq_entry`）が**この関数から戻った後**に送る。
/// ハンドラの仕事を終えてから次の割り込みを許すため。
///
/// **出力しない**（ADR-0018 §5）。共有状態を更新するだけで、表示は
/// メインループが行う。
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

    // **必ず読む。** 条件分岐の後ろに置いてはならない。データが無い場合でも
    // 読んで構わない（空のポートを読むだけで副作用は無い）。読み飛ばす分岐を
    // 作らないことで、「満杯だから読まない」という誤りが将来入り込む余地を
    // 消しておく。
    // SAFETY: 0x60 の読み出しは 1 バイト消費するだけ。値は下で処理する。
    let code = unsafe { controller::read_data() };

    if has_data {
        // 積めなければ捨てて数える。読み出しは既に済んでいるので、
        // 捨てても IRQ1 は止まらない。
        buffer::record(code);
    } else {
        STRAY_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// データを伴わなかった IRQ1 の回数。
///
/// **起動直後の 0 回か 1 回は正常。増え続けるのが異常**
/// （[`handle_irq`] 参照）。
static STRAY_COUNT: AtomicU64 = AtomicU64::new(0);

/// ハンドラが呼ばれた回数。
///
/// **会計を閉じるために持つ。** `handler_invocations == received + stray` が
/// 常に成り立たなければならない。成り立たなければ、どこかで経路を取り違えて
/// いる（M4-a のフレーム数会計と同じ考え方）。`idt` 側のベクタ別カウンタとも
/// 一致するはずで、3 つの数字が互いを検算する形になる。
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
