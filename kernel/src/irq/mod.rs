//! 割り込みコントローラとタイマ源のハードウェア境界（seam整備の項目1、ADR-0023 §3）。
//!
//! # 名前の近い 3 つのモジュールの責務
//!
//! `irq` / `idt` / `interrupts` は名前が近いが、役割は重ならない。将来の
//! 整理でこの 3 つを統合しないよう、切り分けを書いておく。
//!
//! - `irq`（このモジュール）: 割り込みコントローラとタイマ源のハードウェア
//!   境界。「どの IRQ が開いているか」「EOI をどう送るか」「タイマを何 Hz に
//!   するか」に答える。**S2 で PIC/PIT を Local APIC / IO-APIC へ差し替える
//!   ときに触るのはここだけ**になるよう、呼び出しを集約する。
//! - `idt`: ベクタ表とスタブ。ベクタ番号という「器」を扱うのであって、
//!   コントローラそのものは扱わない。IDT は全 CPU で共有し、コアごとに
//!   違うのは IDTR だけである。
//! - `interrupts`: ハンドラ本体・メインループ・排他。
//!
//! # なぜ PIT がここに居るのか
//!
//! タイマ源はコントローラとは別のデバイスだが、S2 で Local APIC が
//! **コントローラとタイマ源の両方**を提供する。ここで束ねておくと、
//! 差し替えが 1 箇所で済む。PIC と PIT を別の境界に置くと、S2 で
//! 「タイマだけ APIC、マスクだけ PIC」という中間状態を表現するために
//! 境界が 2 つとも歪む。
//!
//! # この段（S0-a）では何をしていないか
//!
//! - APIC / IO-APIC への移行はしない（S2）。PIC のまま境界だけ作る。
//! - `trait` は切らない。実装が 1 つしかない段で切ると形を誤る。差し替え点が
//!   1 箇所へ集まっていることが seam の実質であり、`trait` は APIC 実装が
//!   現れる S2 で切る。
//! - IPI は含めない（S5）。PIC に等価物が無く、見越して抽象化すると PIC 側に
//!   意味のないメソッドが生える。

//! # 境界は生の値を出さない
//!
//! 公開するのは「問い」（`bool` を返す述語）と、`Display` を実装した不透明な
//! 観測値だけである。IMR の 2 バイトや ISR のビットをそのまま返すと、呼び出し
//! 側が PIC の語彙で条件を組み立てることになり、S2 で IO-APIC（redirection
//! table は 24 エントリ以上で形が違う）へ移るときに呼び出し側まで書き換えが
//! 波及する。
//!
//! 観測値を `Display` にしているのは、**ログの文言を変えずに呼び出し側から
//! 生の値を取り上げる**ためである。呼び出し側は `"pic: IMR after unmasking
//! IRQ0 {}"` のように前置きだけを持ち、値の書式は実装が決める。

pub mod pic;
pub mod pit;

use core::fmt;

/// [`init`] の失敗。
///
/// **`Debug` を手で実装して内側のエラーへ委譲している。** 内側を包んだ
/// 新しい列挙型として `derive(Debug)` すると、既存のログ
/// （`pic: rejected the vector offsets ({error:?})`）の文言が
/// `Offsets(NotAligned)` のように変わる。境界を作る作業で観測可能な出力を
/// 変えないための措置である。
pub struct InitError(pic::OffsetError);

impl fmt::Debug for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// [`configure_timer`] の失敗。`Debug` の扱いは [`InitError`] と同じ。
pub struct TimerError(pit::FrequencyError);

impl fmt::Debug for TimerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// 割り込みコントローラを初期化する。PIC では 0x20-0x2F への再マップと
/// 全 IRQ のマスクにあたる。
///
/// 成功したときに返す値は、何をプログラムしたかの観測であり、そのまま
/// ログへ流せる。ベクタオフセットは**書いた値であって読み戻した値ではない**
/// ので、その但し書きも観測値の側に持たせてある。
///
/// # Safety
///
/// - 起動時に 1 回だけ呼ぶこと。
/// - 呼び出し時点で割り込みが禁止されていること。
/// - 行き先のベクタが present なハンドラを持つ IDT で覆われていること。
pub unsafe fn init() -> Result<Programming, InitError> {
    // SAFETY: 呼び出し側の契約をそのまま引き継ぐ。
    unsafe { pic::remap(pic::MASTER_VECTOR_OFFSET, pic::SLAVE_VECTOR_OFFSET) }
        .map_err(InitError)?;
    Ok(Programming)
}

/// [`init`] が何をプログラムしたかの観測値。
///
/// # 検査との関係
///
/// この出力そのものに一致を取っているテストは無い（xtask の期待マーカーを
/// 実測で確認した）。ただし**同じベクタ範囲**が `sti-check` の要約行
/// （`4. PIC remapped to 0x20-0x2F = UNVERIFIABLE`）に埋まっており、そちらは
/// `interrupt-test enable-only` が一致を取っている。範囲を変えるならその行も
/// 一緒に見ること。
pub struct Programming;

impl fmt::Display for Programming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "master={:#04x}-{:#04x} slave={:#04x}-{:#04x} \
             (ICW2 is write-only; the offset cannot be read back)",
            pic::MASTER_VECTOR_OFFSET,
            pic::MASTER_VECTOR_OFFSET + pic::IRQS_PER_PIC - 1,
            pic::SLAVE_VECTOR_OFFSET,
            pic::SLAVE_VECTOR_OFFSET + pic::IRQS_PER_PIC - 1
        )
    }
}

/// IRQ 番号に対応するベクタ番号。範囲外なら `None`。
///
/// `const fn` を保つ。[`crate::idt::TIMER_VECTOR`] と
/// [`crate::keyboard::KEYBOARD_VECTOR`] が `const` であり、実行時関数にすると
/// 定義できなくなる。固定トールチェイン（1.97.1）で `match` による剥がしが
/// const 評価できることは確認済みである。`unwrap()` も通るが、不正な IRQ を
/// 渡したときのメッセージが読める `match` を使う。
pub const fn vector_for(irq: u8) -> Option<u8> {
    pic::irq_vector(irq)
}

/// ベクタ番号に対応する IRQ 番号。このコントローラ由来でなければ `None`。
pub const fn irq_for(vector: u8) -> Option<u8> {
    let base = pic::MASTER_VECTOR_OFFSET;
    let count = 2 * pic::IRQS_PER_PIC;
    if vector >= base && vector < base + count {
        Some(vector - base)
    } else {
        None
    }
}

/// この IRQ はマスクされているか。
pub fn is_masked(irq: u8) -> bool {
    let (master, slave) = pic::read_masks();
    if irq < pic::IRQS_PER_PIC {
        master & (1 << irq) != 0
    } else if irq < 2 * pic::IRQS_PER_PIC {
        slave & (1 << (irq - pic::IRQS_PER_PIC)) != 0
    } else {
        true
    }
}

/// すべての IRQ がマスクされているか。
///
/// **点の問い（[`is_masked`]）を 16 回呼ぶ形にしない。** PIC ではマスクの
/// 読み出しが 2 回の I/O ポート読みであり、16 回呼ぶと 32 回になる。検証が
/// 主張しているのは「全部閉じているか」という 1 つの命題なので、それを
/// そのまま問いにする。読み出し回数は現在と同じ 1 回（2 ポート）である。
pub fn all_irqs_masked() -> bool {
    pic::read_masks() == (pic::MASK_ALL, pic::MASK_ALL)
}

/// 開いている IRQ が `unmasked` に挙げたものと正確に一致するか。
///
/// 解禁の前後で確かめたい命題は「開いたのはこの IRQ だけか」である。
/// スライスを受けるのは、S2 の IO-APIC が 24 本以上を扱うため。
/// `u16` のビットマップにすると「16 本」をシグネチャに焼き込むことになる。
pub fn only_irqs_unmasked(unmasked: &[u8]) -> bool {
    let expected = expected_masks(unmasked);
    pic::read_masks() == expected
}

/// `unmasked` を開けたときに IMR がとるはずの値（純粋な計算）。
fn expected_masks(unmasked: &[u8]) -> (u8, u8) {
    let mut masks = (pic::MASK_ALL, pic::MASK_ALL);
    let mut index = 0;
    while index < unmasked.len() {
        masks = pic::masks_with_irq_unmasked(masks, unmasked[index]);
        index += 1;
    }
    masks
}

/// 指定した IRQ 1 本を解禁する。
///
/// # Safety
///
/// 解禁する IRQ には、EOI を発行するハンドラが IDT に入っていること
/// （ADR-0018 §2 の項目 5 / 7）。
pub unsafe fn unmask(irq: u8) {
    // SAFETY: ハンドラの用意は呼び出し側の契約。
    unsafe { pic::unmask_irq(irq) }
}

/// この割り込みはスプリアス（偽）か。
///
/// PIC では In-Service Register を読んで判定する。読み出し自体に副作用が
/// あるため `unsafe` にしてある。判定そのものは純粋関数
/// （[`pic::is_spurious`]）で、ホストテストで固定してある。
///
/// # Safety
///
/// コマンドポートの読み出し対象を変更する。他の実行文脈が同時に
/// コントローラを触っていないこと。
pub unsafe fn is_spurious(irq: u8) -> bool {
    // SAFETY: 排他は呼び出し側の契約。
    let isr = unsafe { pic::read_isr() };
    pic::is_spurious(irq, isr)
}

/// 割り込みの後始末。スプリアスなら偽の割り込みへ応答しない。
///
/// # Safety
///
/// 実際に発生した割り込みに対してのみ呼ぶこと。
pub unsafe fn end_of_interrupt(irq: u8, spurious: bool) {
    // SAFETY: 呼び出し側の契約。宛先の決定は純粋ロジックに委ねる。
    unsafe { pic::send_eoi_for(pic::eoi_action_for(irq, spurious)) }
}

/// 周期タイマを設定する。解禁は別（[`unmask`]）。
///
/// # Safety
///
/// - 起動時に 1 回だけ呼ぶこと。
/// - 呼び出し時点でタイマの IRQ がマスクされていること。
pub unsafe fn configure_timer(frequency_hz: u32) -> Result<TimerSetup, TimerError> {
    // SAFETY: 呼び出し側の契約をそのまま引き継ぐ。
    let divisor = unsafe { pit::configure_channel0(frequency_hz) }.map_err(TimerError)?;
    Ok(TimerSetup {
        divisor,
        requested_hz: frequency_hz,
        actual_millihertz: pit::actual_frequency_millihertz(divisor),
    })
}

/// タイマに要求する周波数。
pub const fn timer_frequency_hz() -> u32 {
    pit::TARGET_FREQUENCY_HZ
}

/// [`configure_timer`] が何を設定したかの観測値。
///
/// # 検査との関係（変更するとテストが落ちる）
///
/// この出力の文言と値に `interrupt-test no-eoi` が依存している
/// （期待マーカー `pit: channel 0 set to divisor=11932`）。分周値は PIT の
/// 語彙なので、S2 で Local APIC タイマへ移ると意味を失う。そのときは
/// マーカー側を問いベースへ移す作業が要る。
pub struct TimerSetup {
    divisor: u16,
    requested_hz: u32,
    actual_millihertz: u64,
}

impl fmt::Display for TimerSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "divisor={} for a requested {} Hz; actual is {}.{:03} Hz \
             (the divisor is an integer, so the period never matches exactly)",
            self.divisor,
            self.requested_hz,
            self.actual_millihertz / 1000,
            self.actual_millihertz % 1000
        )
    }
}

/// マスクの実状態の観測値。読み戻した値を持つ。
///
/// # 検査との関係（変更するとテストが落ちる）
///
/// この出力の文言と値に `interrupt-test timer` と `interrupt-test no-eoi` が
/// 依存している（期待マーカー
/// `pic: IMR after unmasking IRQ0 master=0xfe slave=0xff`）。IMR の 2 バイト
/// という形は PIC 固有で、IO-APIC の redirection table では成り立たない。
/// S2 ではマーカー側を問い（[`only_irqs_unmasked`]）ベースへ移すこと。
///
/// **判定には使わない。表示専用である。** 開いている IRQ が期待どおりかを
/// 判断するのは [`only_irqs_unmasked`] の仕事で、そちらは値を比較して `bool`
/// を返す。書式を判定に載せると、書式を変えた瞬間に静かに壊れる。
pub struct MaskSnapshot(u8, u8);

impl fmt::Display for MaskSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "master={:#04x} slave={:#04x}", self.0, self.1)
    }
}

/// 現在のマスクを読み戻す（ログ用）。
pub fn mask_snapshot() -> MaskSnapshot {
    let (master, slave) = pic::read_masks();
    MaskSnapshot(master, slave)
}

/// `unmasked` だけが開いているときに期待されるマスク（ログ用）。
///
/// **期待を決めるのは呼び出し側である。** 引数で「どの IRQ を開けたか」を
/// 受け取り、そこから IMR の期待値を計算するだけで、境界が独自に期待を
/// 持つことはない。検証の主語は呼び出し側のままである。
///
/// これも表示専用で、判定は [`only_irqs_unmasked`] が値の比較で行う。
pub fn expected_mask_snapshot(unmasked: &[u8]) -> MaskSnapshot {
    let (master, slave) = expected_masks(unmasked);
    MaskSnapshot(master, slave)
}

/// 配送中の割り込みの観測値（ハートビートの診断用）。
///
/// **この出力に自動検査が依存していないこと**を条件に、コントローラ固有の
/// 診断を境界から出してよいことにしている。現状ハートビートの
/// `PIC ISR=` はどのテストのマーカーにも入っていない（実測で確認済み）。
///
/// # 見ていない範囲（隠さずに書く）
///
/// **マスタの ISR しか持っていない。** 現在のハートビートがマスタ側だけを
/// 出しているので、それに合わせてある（振る舞い不変のため、ここでスレーブを
/// 足して出力を増やさない）。スレーブ側で配送中の割り込みは観測できない。
///
/// S2 では master / slave という区別自体が消える（IO-APIC に従属コントローラは
/// 無い）。この観測値は、そのときに形が変わる。
pub struct ServiceSnapshot(u8);

impl fmt::Display for ServiceSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#04x}", self.0)
    }
}

/// 配送中の割り込みを読み戻す（ログ用）。
///
/// # Safety
///
/// [`is_spurious`] と同じ。読み出し対象を変更するので排他が要る。
pub unsafe fn service_snapshot() -> ServiceSnapshot {
    // SAFETY: 排他は呼び出し側の契約。
    let (master, _slave) = unsafe { pic::read_isr() };
    ServiceSnapshot(master)
}
