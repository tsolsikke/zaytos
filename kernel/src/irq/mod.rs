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
//!
//! # 公開面の内訳（trait 委譲か、そうでないか）
//!
//! **trait は境界の全部を覆っていない。** 覆っていると誤解すると、
//! 「実装を差し替えれば全部が切り替わる」と読めてしまう。どれがどちらかを
//! ここに列挙する。**公開関数を足したらこの表も足すこと。**
//!
//! | 公開関数 | 扱い | 理由 |
//! |---|---|---|
//! | [`unmask`] | [`Controller`] へ委譲 | 実装ごとに答えが変わる |
//! | [`mask_all`] | [`Controller`] へ委譲 | 同上 |
//! | [`end_of_interrupt`] | [`Controller`] へ委譲 | 同上 |
//! | [`is_spurious`] | [`Controller`] へ委譲 | 同上 |
//! | [`check_masks`] | [`Controller`] へ委譲 | 同上 |
//! | [`configure_timer`] | [`TimerSource`] へ委譲 | 同上 |
//! | [`init`] | **PIC 専用** | 8259 の再マップ（ICW1 から ICW4）そのもので、APIC 側に対応物が無い。I/O APIC 側の初期設定は形が違うので、**S2-d-1c で `init` の扱いと合わせて改めて判断する** |
//! | [`service_snapshot`] | **PIC 専用** | 8259 の ISR を読む診断であり、LAPIC の ISR は 8 本で形が違う。配送が移る段（S2-d-1c 以降）で形を決める |
//! | [`vector_for`] | モジュール関数 | `const fn` である。固定トールチェイン（1.97.1）で const trait method が安定しておらず、trait へ入れると [`crate::idt::TIMER_VECTOR`] が定義できない |
//! | [`irq_for`] | モジュール関数 | 同上 |
//! | [`managed_vectors`] | モジュール関数 | 同上 |
//! | [`timer_frequency_hz`] | モジュール関数 | 同上 |
//! | [`survey_apic_masks`] | どちらでもない | 2 つ目の実装を 1 回読ませるための一時的な入口（S2-d-1b）。切り替えが済めば要らなくなる |
//!
//! # `TimerSource` は実装が 1 つしかない。**これは原則の例外である**
//!
//! S0-a は「実装が 2 つになるまで trait を切らない」と決めており、S2-d-1b は
//! [`Controller`] についてはそれを満たす（`Legacy` と `Apic`）。
//! **[`TimerSource`] は満たしていない。** 実装は PIT の 1 つだけで、Local APIC
//! タイマ側は S2-d-2 で足す。**単一実装の trait であることを、書かずに
//! 通さない。**
//!
//! 遅らせなかった理由は、**2 本の trait を責務で対にして決めた**ことにある。
//! 片方だけ S2-d-2 まで遅らせると、[`configure_timer`] だけがモジュール関数
//! として残り、上の公開面の表がもう 1 種類増える。切り分けの軸としては、
//! 「trait 化」を 1 回で終えて「実装を足す」を別の段に置くほうが読みやすい。
//!
//! Local APIC タイマの実装をこの段で書かなかった理由は `irq/apic.rs` の
//! 末尾にある（初期カウントは較正の戻り値から求めるもので、較正値を持たない
//! この段では正しい値を書けない）。
//!
//! # 境界の外に、境界が所有すべき書き込み操作がある（未解決）
//!
//! S0-a の「IMR への書き込みは境界の内側だけに存在する」は、**PIC については
//! 真だが、I/O APIC については偽である。** redirection entry を読み書きする
//! 操作は [`crate::apic`] にあり、割り込み層の外である。可視性の静的検査は
//! `kernel/src/irq/` の内側だけを見るので、**ここは捕まらない。**
//!
//! **守れない箇所を守れると書かないために、非対称を明示しておく。**
//! レジスタの配置を知るモジュールを 1 つに保つほうを優先した結果であり、
//! 到達範囲は `pub(crate)` まで狭めてある。解禁条件つきで
//! `deferred-decisions.md` に置いた。
//!
//! # まだ置き場の決まっていない操作（S2-d-1c で決める）
//!
//! **redirection entry の設定を担う操作が、trait にも境界にも無い。**
//! [`Controller::unmask`] はマスクを外すだけだが、I/O APIC ではその前に
//! **ベクタ・配送モード・宛先を entry へ書き込む**必要がある。PIC 側では
//! この役割を [`init`] が担っていたが、その `init` は PIC 専用として残した
//! ので、APIC 側には置き場が無い。
//!
//! **`unmask` の中でついでに設定する形にしないこと。** マスクを外す操作と
//! 経路を設定する操作を 1 つに畳むことになり、後で分けたくなったときに高くつく
//! （マスクの開け閉めは何度も起きるが、経路の設定は 1 回である）。
//! S2-d-1c で `init` の扱いと合わせて決める。

mod apic;
mod pic;
mod pit;

use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

/// レガシー IRQ の本数（2 台の 8259 で 16 本）。移行状態の器の大きさを決める。
const MAX_LEGACY_IRQS: usize = 16;

/// I/O APIC 経由へ移した IRQ のビットマップ（S2-d-1c）。
///
/// # なぜ IRQ 単位なのか。**単一の状態では成立しない**
///
/// S2-d-1c の中間状態では、**IRQ1 が I/O APIC 経由、IRQ0 が依然として
/// 8259 経由**である。この 2 つは **EOI の宛先が違う。** 8259 経由の割り込みは
/// ExtINT 配送なので LAPIC の ISR に載らず、LAPIC EOI は要らない代わりに
/// 8259 への EOI が要る。I/O APIC 経由の割り込みは LAPIC の ISR に載るので
/// LAPIC EOI が要る。
///
/// **「どちらのコントローラが有効か」を 1 つの状態で持つと、それを APIC へ
/// 倒した瞬間にタイマ割り込みの EOI が LAPIC へ送られ、8259 は EOI を
/// 受け取らない。タイマが止まる。**
///
/// # 書き手と読み手
///
/// 書くのは起動時の切り替えだけ（[`route_to_apic`]）。読むのは**割り込み
/// ハンドラ**である（`idt::irq_entry` から EOI とスプリアス判定の経路）。
/// **素の `static mut` にしない。** 書き手 1 人・読み手が割り込み文脈という
/// 形は `TIMER_TICKS` と同じなので、同じ道具（アトミック）を使う。
///
/// **アトミック 1 つでは足りない。** 切り替えは 3 操作（PIC でマスク →
/// 状態を立てる → I/O APIC で解禁）に分かれ、その途中で割り込みが入ると
/// 古い状態で EOI を送る経路が成立する。**3 操作を割り込み禁止区間で囲う。**
static ROUTED_TO_APIC: AtomicU32 = AtomicU32::new(0);

/// I/O APIC 経由へ移した IRQ の配送先ベクタ。移していない IRQ は
/// [`NO_ROUTED_VECTOR`]。
///
/// [`irq_for_vector`] がベクタから IRQ を逆引きするのに使う。
static ROUTED_VECTOR: [AtomicU8; MAX_LEGACY_IRQS] =
    [const { AtomicU8::new(NO_ROUTED_VECTOR) }; MAX_LEGACY_IRQS];

/// 「この IRQ は移していない」を表す番兵。**ベクタ 0 は CPU の #DE なので、
/// 割り込みの配送先として現れることはない。**
const NO_ROUTED_VECTOR: u8 = 0;

/// この IRQ が I/O APIC 経由へ移っているか。
pub fn routed_to_apic(irq: u8) -> bool {
    if irq as usize >= MAX_LEGACY_IRQS {
        return false;
    }
    ROUTED_TO_APIC.load(Ordering::Relaxed) & (1u32 << irq) != 0
}

/// この IRQ の配送先ベクタ。移していなければ `None`。
pub fn routed_vector(irq: u8) -> Option<u8> {
    if irq as usize >= MAX_LEGACY_IRQS {
        return None;
    }
    match ROUTED_VECTOR[irq as usize].load(Ordering::Relaxed) {
        NO_ROUTED_VECTOR => None,
        vector => Some(vector),
    }
}

/// ベクタ番号に対応する IRQ 番号。**移行済みの経路も含めて引く。**
///
/// # [`irq_for`] との違い
///
/// [`irq_for`] は `const fn` で、**PIC の採番表しか見ない。**
/// [`crate::idt::TIMER_VECTOR`] が `const` 項目なので消せないが、
/// I/O APIC 経由のベクタは PIC の採番表に載っていないため、あれだけでは
/// 引けない。
///
/// **これは EOI だけの問題ではない。** `idt::irq_entry` は
/// 「ベクタから IRQ が引けたか」で IRQ 処理全体を分岐しており、
/// タイマのティック加算もキーボードのハンドラ呼び出しもその中にある。
/// **引けなければ、キーボードのハンドラごと呼ばれない。**
///
/// 移行済みの表を先に見るのは、そちらが現在の事実だからである。
pub fn irq_for_vector(vector: u8) -> Option<u8> {
    for (irq, slot) in ROUTED_VECTOR.iter().enumerate() {
        if slot.load(Ordering::Relaxed) == vector && vector != NO_ROUTED_VECTOR {
            return u8::try_from(irq).ok();
        }
    }
    irq_for(vector)
}

/// 割り込みコントローラ。**S2-d-1b で切った。**
///
/// # 何がこの trait に入り、何が入らないか
///
/// **この trait は境界の全部を覆っていない。** 境界の公開関数のうち、
/// 実装ごとに答えが変わるものだけがここに入る。入らなかったものと理由は
/// モジュール doc の一覧にある（[`init`] と [`service_snapshot`]、および
/// `const fn` の 4 本）。
///
/// # なぜ 2 本に分けるのか
///
/// 「LAPIC タイマが LAPIC の一部である」のは**実装の事情であって責務では
/// ない**。1 本に畳むと、I/O APIC が「タイマ源でもある」ことを強いられる。
/// 1 つの型が両方を実装すればよいので、分けても手間は増えない。
trait Controller {
    /// 指定した IRQ 1 本を解禁する。
    ///
    /// # Safety
    ///
    /// [`unmask`] と同じ。
    unsafe fn unmask(&self, irq: u8);

    /// すべての IRQ をマスクする。
    ///
    /// # Safety
    ///
    /// [`mask_all`] と同じ。
    unsafe fn mask_all(&self);

    /// 割り込みの後始末。
    ///
    /// # Safety
    ///
    /// [`end_of_interrupt`] と同じ。
    unsafe fn end_of_interrupt(&self, irq: u8, spurious: bool);

    /// この割り込みはスプリアス（偽）か。
    ///
    /// # Safety
    ///
    /// [`is_spurious`] と同じ。
    unsafe fn is_spurious(&self, irq: u8) -> bool;

    /// マスクの実状態を 1 回読み、`unmasked` だけが開いているかを判定する。
    fn check_masks(&self, unmasked: &[u8]) -> MaskCheck;

    /// この IRQ の配送先ベクタを設定する。**マスクは触らない。**
    ///
    /// # マスクを外す操作と分けてある
    ///
    /// [`Controller::unmask`] は何度も起きるが、**経路の設定は 1 回である。**
    /// 1 つに畳むと、後で分けたくなったときに高くつく。
    ///
    /// # Safety
    ///
    /// - 配送先のベクタに、戻れるハンドラが IDT に入っていること。
    /// - この IRQ がマスクされていること（設定の途中で届かせない）。
    unsafe fn route(&self, irq: u8, vector: u8);
}

/// 周期タイマ源。
trait TimerSource {
    /// 周期タイマを設定する。解禁は別（[`Controller::unmask`] または LVT）。
    ///
    /// # Safety
    ///
    /// [`configure_timer`] と同じ。
    unsafe fn configure_timer(&self, frequency_hz: u32) -> Result<TimerSetup, TimerError>;
}

/// 8259A PIC と 8254 PIT の組。**現在動いている実装である。**
///
/// 2 つのデバイスにまたがるが、どちらもレガシーの組で、片方だけを差し替える
/// 場面が無いので 1 つの型にしてある。
struct Legacy;

impl Controller for Legacy {
    unsafe fn unmask(&self, irq: u8) {
        // SAFETY: ハンドラの用意は呼び出し側の契約。
        unsafe { pic::unmask_irq(irq) }
    }

    unsafe fn mask_all(&self) {
        // SAFETY: 呼び出し側の契約。全ビットを立てるので、どの IRQ も通らない。
        unsafe { pic::set_masks(pic::MASK_ALL, pic::MASK_ALL) }
    }

    unsafe fn end_of_interrupt(&self, irq: u8, spurious: bool) {
        // SAFETY: 呼び出し側の契約。宛先の決定は純粋ロジックに委ねる。
        unsafe { pic::send_eoi_for(pic::eoi_action_for(irq, spurious)) }
    }

    unsafe fn is_spurious(&self, irq: u8) -> bool {
        // SAFETY: 排他は呼び出し側の契約。
        let isr = unsafe { pic::read_isr() };
        pic::is_spurious(irq, isr)
    }

    unsafe fn route(&self, _irq: u8, _vector: u8) {
        // **何もしない。8259 は IRQ 単位で行き先を選べない。**
        //
        // 経路はベクタオフセット（ICW2）で決まり、IRQ 番号を足したものが
        // ベクタになる。1 本だけ別のベクタへ向けることはできないので、
        // ここに書けることが無い。**空実装は「できない」の表現であって、
        // 未実装ではない。** `Apic::is_spurious` が定数 `false` を返すのと
        // 同じ扱いである。
        //
        // ベクタオフセットそのものを変えるのは [`init`] の仕事で、
        // あれは「1 本の経路を決める」ではなく「コントローラを初期化する」
        // である。役割が違うので同じ名前へ寄せていない。
    }

    fn check_masks(&self, unmasked: &[u8]) -> MaskCheck {
        let (master, slave) = pic::read_masks();
        let (expected_master, expected_slave) = expected_masks(unmasked);
        MaskCheck {
            observed: MaskState::Pic { master, slave },
            expected: MaskState::Pic {
                master: expected_master,
                slave: expected_slave,
            },
        }
    }
}

impl TimerSource for Legacy {
    unsafe fn configure_timer(&self, frequency_hz: u32) -> Result<TimerSetup, TimerError> {
        // SAFETY: 呼び出し側の契約をそのまま引き継ぐ。
        let divisor = unsafe { pit::configure_channel0(frequency_hz) }.map_err(TimerError)?;
        Ok(TimerSetup {
            requested_hz: frequency_hz,
            actual_millihertz: pit::actual_frequency_millihertz(divisor),
            source: TimerSourceSetup::Pit { divisor },
        })
    }
}

/// 現在有効なコントローラ。
///
/// # なぜ列挙子が無いのか（S2-d-1b の時点）
///
/// 切り替えが実行時に起きる以上、最終的な dispatch は列挙子になる
/// （関連型では実行時の切り替えを跨げず、trait object は値として返せない）。
/// **ただし S2-d-1b は切り替えない段である。** 状態が 1 つしか無いうちに
/// 列挙子を置くと、選ばれることのない腕を先回りで作ることになる。
/// **使うから足すのであって、将来のために足すのではない。**
/// 列挙子は、実際に 2 つの状態を持つ S2-d-1c で入れる。
const ACTIVE: Legacy = Legacy;

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
/// （`4. interrupt delivery vectors are set as intended = UNVERIFIABLE`）に隣接しており、そちらは
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
/// [`crate::keyboard::PIC_KEYBOARD_VECTOR`] が `const` であり、実行時関数にすると
/// 定義できなくなる。固定トールチェイン（1.97.1）で `match` による剥がしが
/// const 評価できることは確認済みである。`unwrap()` も通るが、不正な IRQ を
/// 渡したときのメッセージが読める `match` を使う。
pub const fn vector_for(irq: u8) -> Option<u8> {
    pic::irq_vector(irq)
}

/// ベクタ番号に対応する IRQ 番号。このコントローラ由来でなければ `None`。
///
/// # マスタとスレーブを別々に見る
///
/// **以前は `MASTER_VECTOR_OFFSET .. +16` という 1 つの連続範囲で見ていた。**
/// これはスレーブのオフセットがマスタ + 8 であることへの暗黙の依存で、
/// `deferred-decisions.md` に保留項目として記録してあった。現在の 2 構成
/// （通常の `0x20`/`0x28` と `alt-offset-test` の `0x30`/`0x38`）ではどちらも
/// 成立しているため実害は無かったが、**依存が崩れうる構成が出た時点で
/// 閉じる**という条件だった。S2 で IO-APIC へ移ればスレーブという概念自体が
/// 消えるので、ここで閉じる。
///
/// 2 つのオフセットを別々に見るので、スレーブがマスタ + 8 でなくても正しい。
/// 現行の 2 構成では結果が以前と一致する（どちらもスレーブ = マスタ + 8）。
pub const fn irq_for(vector: u8) -> Option<u8> {
    if vector >= pic::MASTER_VECTOR_OFFSET && vector < pic::MASTER_VECTOR_OFFSET + pic::IRQS_PER_PIC
    {
        return Some(vector - pic::MASTER_VECTOR_OFFSET);
    }
    if vector >= pic::SLAVE_VECTOR_OFFSET && vector < pic::SLAVE_VECTOR_OFFSET + pic::IRQS_PER_PIC {
        return Some(pic::IRQS_PER_PIC + (vector - pic::SLAVE_VECTOR_OFFSET));
    }
    None
}

/// マスクの実状態を 1 回読み、`unmasked` だけが開いているかを判定する。
///
/// **判定と表示を同じ観測から導くための型である。** 判定用に 1 回・表示用に
/// もう 1 回読むと、ログに出る値と判定の根拠が食い違いうる（「expected と
/// actual が一致して見えるのに判定は Failed」）。デバッグを最も誤らせる形
/// なので、読み出しも期待値の計算も 1 回にまとめ、[`MaskCheck::matches`] と
/// `Display` の両方がその 1 つの値から導かれるようにしてある。
///
/// 点の問い（「この IRQ は閉じているか」）を 16 回呼ぶ形にはしない。PIC では
/// マスクの読み出しが 2 回の I/O ポート読みであり、16 回呼ぶと 32 回になる。
/// 検証が主張しているのは「開いたのはこの IRQ だけか」という 1 つの命題で、
/// それをそのまま 1 回の読み出しで確かめる。
///
/// `unmasked` がスライスなのは、S2 の IO-APIC が 24 本以上を扱うためである。
/// `u16` のビットマップにすると「16 本」をシグネチャに焼き込むことになる。
pub fn check_masks(unmasked: &[u8]) -> MaskCheck {
    ACTIVE.check_masks(unmasked)
}

/// 2 つ目のコントローラ実装（Local APIC / I/O APIC）でマスクを読み、その観測を返す。
///
/// **切り替えない。読むだけである。** S2-d-1b は振る舞い不変の段で、配送は
/// PIC / PIT のままである。
///
/// # なぜ読むのか
///
/// 2 つ目の実装を書いても、どこからも呼ばなければ**実ハードウェアを正しく
/// 読めるかが分からないまま S2-d-1c へ入る**。1c は配送が変わる段なので、
/// そこで初めて落ちると「切り替えが悪いのか、実装が悪いのか」を切り分け
/// られない。**振る舞いを変えない段のうちに、読めることだけを確かめておく。**
/// S1-c で「写像したうえで読んで確かめた」のと同じ形である。
///
/// 読むのは I/O APIC の redirection entry のマスクビットだけで、書き込みは
/// 一切しない。S2-a が同じレジスタを読んでいるので、新しい危険は無い。
///
/// I/O APIC が 1 台も写像できていなければ `None`。
pub fn survey_apic_masks(mapped: &crate::apic::MappedApic, unmasked: &[u8]) -> Option<MaskCheck> {
    let controller = apic::Apic::new(mapped)?;
    Some(controller.check_masks(unmasked))
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
    // SAFETY: 呼び出し側の契約をそのまま実装へ引き継ぐ。
    unsafe { ACTIVE.unmask(irq) }
}

/// すべての IRQ をマスクする。
///
/// # なぜ `init()` を流用しないのか
///
/// [`init`] も結果として全マスクにするが、あちらは ICW1 から ICW4 を書く
/// **初期化**である。ここで要るのは**無効化**で、意図が違う。副作用が一致する
/// ことを理由に流用すると、後から読んだ人が「なぜ移行の途中で初期化するのか」を
/// 毎回考えることになる。
///
/// # いつ使うか
///
/// **S2 で Local APIC / IO-APIC へ移るときに要る。** MADT の Flags で
/// PCAT_COMPAT が立っている（実測で確定済み）ので、この系にはデュアル 8259 が
/// あり、APIC へ移る前に黙らせる必要がある。**需要があるから足すのであって、
/// 将来のために足すのではない。**
///
/// # S2-d-2 まで誰も呼ばない
///
/// **`dead_code` にならないのは `pub` だからであって、使われているからではない。**
/// `smp::trampoline_frame` が S1 で置かれて S3 まで呼ばれなかったのと同じ状態で
/// ある。**呼び忘れても警告では気づけない**ので、roadmap の S2-d-2 の到達条件に
/// 「この関数が実際に呼ばれること」を入れてある。PIC のマスクを別の方法で書いて
/// しまっても誰も気づかない、という形を塞ぐためである。
///
/// # Safety
///
/// 呼び出し後、マスクした IRQ は届かなくなる。タイマを含むので、**別の配送
/// 経路を用意する前に呼ぶと時間が止まる。**
pub unsafe fn mask_all() {
    // SAFETY: 呼び出し側の契約をそのまま実装へ引き継ぐ。
    unsafe { ACTIVE.mask_all() }
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
    // **移行済みの IRQ を、もう所有していないコントローラに問い合わせない。**
    //
    // # 予測は外れた。外れたことを書いておく
    //
    // 設計時は「8259 の ISR を読むと、その IRQ は載っていないのでビットが
    // 立っておらず、スプリアスと誤判定して EOI を送らない側へ倒れる」と
    // 予測していた。**破壊で確かめたところ、そうならなかった。**
    // [`pic::is_spurious`] は **IRQ7 と IRQ15 以外では ISR を見ずに `false` を
    // 返す**ので、IRQ1 では読んでも判定が変わらない。
    //
    // **したがってこの分岐は、現在の構成では観測可能な効果を持たない。**
    // 残してあるのは、(1) 所有していないコントローラの I/O ポートを割り込み
    // ハンドラの中で読まずに済むこと、(2) IRQ7 か IRQ15 を I/O APIC 経由へ
    // 移した場合には**実際に判定が変わる**ことによる。
    // **「必要だから入れた」ではなく「今は効果を観測できない」と書く。**
    if routed_to_apic(irq) {
        // SAFETY: 呼び出し側の契約をそのまま実装へ引き継ぐ。
        return unsafe { apic::spurious_for_routed_irq(irq) };
    }
    // SAFETY: 呼び出し側の契約をそのまま実装へ引き継ぐ。
    unsafe { ACTIVE.is_spurious(irq) }
}

/// 割り込みの後始末。スプリアスなら偽の割り込みへ応答しない。
///
/// # Safety
///
/// 実際に発生した割り込みに対してのみ呼ぶこと。
pub unsafe fn end_of_interrupt(irq: u8, spurious: bool) {
    // **宛先は IRQ 単位で決まる。** 中間状態では 8259 経由と I/O APIC 経由が
    // 併存し、前者は 8259 への EOI、後者は LAPIC への EOI が要る。
    // 単一の状態で切り替えると、倒した瞬間にもう片方が EOI を受け取らなくなる。
    if routed_to_apic(irq) {
        // SAFETY: 呼び出し側の契約をそのまま実装へ引き継ぐ。
        unsafe { apic::end_of_interrupt_for_routed_irq(spurious) };
        return;
    }
    // SAFETY: 呼び出し側の契約をそのまま実装へ引き継ぐ。
    unsafe { ACTIVE.end_of_interrupt(irq, spurious) }
}

/// 1 本の IRQ を I/O APIC 経由へ移す（S2-d-1c）。
///
/// # 順序
///
/// 1. redirection entry へ配送先を書く（マスクは立てたまま）
/// 2. **PIC 側でその IRQ をマスクする**
/// 3. 移行状態を立てる（ビットマップと配送先ベクタ）
/// 4. **I/O APIC 側でマスクを外す**
///
/// 2 と 4 が逆だと、両方が開いた瞬間に二重配送しうる。1 でマスクを立てた
/// まま書くのは、設定の途中で届かせないためである。
///
/// # 2 から 4 は割り込み禁止区間で行う
///
/// 状態そのものはアトミックだが、**3 操作の途中で割り込みが入ると、
/// 古い状態で EOI を送る経路が成立する。** アトミック 1 つでは足りない。
///
/// # Safety
///
/// - 配送先のベクタに、戻れるハンドラが IDT に入っていること。
/// - 起動時に呼ぶこと（この関数は割り込みを一時的に禁止する）。
pub unsafe fn route_to_apic(
    mapped: &crate::apic::MappedApic,
    irq: u8,
    vector: u8,
) -> Result<(), RouteError> {
    if irq as usize >= MAX_LEGACY_IRQS {
        return Err(RouteError::IrqOutOfRange);
    }
    if vector == NO_ROUTED_VECTOR {
        return Err(RouteError::VectorReserved);
    }
    let controller = apic::Apic::new(mapped).ok_or(RouteError::NoIoApic)?;

    // 1. 経路を設定する。**マスクは立てたまま**なので、まだ届かない。
    // SAFETY: ゲートの用意は呼び出し側の契約。この IRQ は I/O APIC 側で
    // マスクされたままである（起動時の redirection entry は全本マスク）。
    unsafe { controller.route(irq, vector) };

    // 2 から 4 をひとまとめにする。区間内でログも確保も行わない。
    {
        let _critical = common::critical::InterruptGuard::enter();

        // 2. 旧経路を閉じる。
        // SAFETY: 8259 側のマスクを立てるだけ。新経路はまだマスクされている
        // ので、この瞬間からこの IRQ はどこにも届かない。
        //
        // 破壊 (S2-d-1c, ioapic-keep-pic-irq1): ここを飛ばすと両経路が開き、
        // 二重配送になる。経路ごとにベクタが違うので、旧ベクタで届いたキーが
        // あることとして観測できるはずである。
        #[cfg(not(feature = "ioapic-keep-pic-irq1-test"))]
        unsafe {
            pic::mask_irq(irq)
        };

        // 3. 状態を立てる。**旧経路を閉じた後、新経路を開ける前である。**
        ROUTED_VECTOR[irq as usize].store(vector, Ordering::Relaxed);
        ROUTED_TO_APIC.fetch_or(1u32 << irq, Ordering::Relaxed);

        // 4. 新経路を開ける。
        // SAFETY: ゲートは用意済みで、状態も立っている。ここから届いてよい。
        //
        // 破壊 (S2-d-1c, ioapic-skip-unmask): ここを飛ばすと、設定は正しいが
        // 配送されない。読み戻しの主張は通り、到達の主張だけが落ちる。
        #[cfg(not(feature = "ioapic-skip-unmask-test"))]
        unsafe {
            controller.unmask(irq)
        };
    }
    Ok(())
}

/// 移行済み IRQ の redirection entry を読み戻す（S2-d-1c）。
///
/// **到達の観測とは独立した検出経路である。** 読み戻しは「書いた値が
/// entry に載っているか」、到達は「そのベクタで実際に届くか」を見る。
/// 片方だけでは、設定できても配送されない形（マスクの外し忘れ）と、
/// 保持されているかを見ていない形を、それぞれ通す。
///
/// 移していない IRQ や、I/O APIC が無い場合は `None`。
pub fn routed_entry_readback(
    mapped: &crate::apic::MappedApic,
    irq: u8,
) -> Option<RedirectionEntryView> {
    if !routed_to_apic(irq) {
        return None;
    }
    apic::Apic::new(mapped)?.read_entry(irq)
}

/// redirection entry 1 本の観測値（[`routed_entry_readback`]）。
///
/// **生の `u32` を出さない。** 呼び出し側が必要なのは「我々が書いたベクタか」
/// という問いと、ログへ流せる表示だけである。
pub struct RedirectionEntryView {
    low: u32,
}

impl RedirectionEntryView {
    /// 生の low dword から作る。**`irq` の内側からのみ作れる。**
    const fn new(low: u32) -> Self {
        Self { low }
    }

    /// この entry の配送先ベクタ。
    pub const fn vector(&self) -> u8 {
        (self.low & 0xFF) as u8
    }

    /// この entry はマスクされているか。
    pub fn masked(&self) -> bool {
        self.low & crate::apic::ENTRY_MASKED_BIT != 0
    }
}

impl fmt::Display for RedirectionEntryView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "vector={:#04x} masked={} level_triggered={} active_low={}",
            self.vector(),
            self.masked(),
            self.low & crate::apic::ENTRY_LEVEL_TRIGGERED_BIT != 0,
            self.low & crate::apic::ENTRY_ACTIVE_LOW_BIT != 0
        )
    }
}

/// [`route_to_apic`] の失敗。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RouteError {
    /// レガシー IRQ の範囲外。
    IrqOutOfRange,
    /// I/O APIC が 1 台も写像できていない。
    NoIoApic,
    /// 番兵と衝突するベクタ（0）を指定した。
    VectorReserved,
}

/// 周期タイマを設定する。解禁は別（[`unmask`]）。
///
/// # Safety
///
/// - 起動時に 1 回だけ呼ぶこと。
/// - 呼び出し時点でタイマの IRQ がマスクされていること。
pub unsafe fn configure_timer(frequency_hz: u32) -> Result<TimerSetup, TimerError> {
    // SAFETY: 呼び出し側の契約をそのまま実装へ引き継ぐ。
    unsafe { ACTIVE.configure_timer(frequency_hz) }
}

/// タイマに要求する周波数。
pub const fn timer_frequency_hz() -> u32 {
    pit::TARGET_FREQUENCY_HZ
}

/// タイマ源ごとに違う設定値。**実装が増えたら列挙子を足す。**
///
/// # なぜ列挙子で持つのか（S2-b）
///
/// 分周値は PIT の語彙で、Local APIC タイマには無い（あちらは初期カウントと
/// 分周設定である）。境界の型が実装ごとに違う値を持つ必要があるので、その
/// 差分をここへ閉じ込める。
///
/// **2 つ目の実装が来ても、この列挙子に 1 つ足すだけで済む。** 既存の列挙子と
/// その `Display` の腕は触らないので、**PIT の出力する文字列は変わらない。**
/// 振る舞い不変のリファクタを 2 度行わずに済ませるための形である。
///
/// trait の関連型にしない理由は、**切り替えが実行時に起きる**ためである。
/// S2-d は起動の途中で PIT から Local APIC タイマへ移る。関連型にすると
/// 呼び出し側が実装ごとに総称化され、実行時の切り替えを跨げない。
/// trait object にしない理由は、値として返して保持したいためである。
enum TimerSourceSetup {
    Pit { divisor: u16 },
    // S2-d-2 で足す: LapicTimer { initial_count: u32, divide_configuration: u32 }。
    // **S2-d-1b では足していない。** 初期カウントは較正の戻り値から実行時に
    // 求めるもので、較正値を持たないこの段では正しい値を書けない
    // （`irq/apic.rs` の末尾に理由がある）。
}

/// [`configure_timer`] が何を設定したかの観測値。
///
/// # 検査との関係（変更するとテストが落ちる）
///
/// この出力の文言と値に `interrupt-test no-eoi` が依存している
/// （期待マーカー `pit: channel 0 set to divisor=11932`）。分周値は PIT の
/// 語彙なので、S2-d で Local APIC タイマへ移ると意味を失う。そのときは
/// マーカー側を問いベース（[`TimerSetup::requested_hz`] など）へ移す作業が要る。
pub struct TimerSetup {
    requested_hz: u32,
    actual_millihertz: u64,
    source: TimerSourceSetup,
}

impl TimerSetup {
    /// 要求した周波数。**実装に依存しない問いである。**
    pub const fn requested_hz(&self) -> u32 {
        self.requested_hz
    }

    /// 実際に設定された周波数（ミリヘルツ）。**実装に依存しない問いである。**
    ///
    /// 整数の分周や初期カウントを使う以上、要求どおりぴったりにはならない。
    /// S2-d でマーカーを問いベースへ移すとき、一致ではなく許容幅で見るのは
    /// この値である。
    pub const fn actual_millihertz(&self) -> u64 {
        self.actual_millihertz
    }
}

impl fmt::Display for TimerSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.source {
            // **この腕の文字列を変えない。** `interrupt-test no-eoi` が
            // `divisor=11932` に一致を取っている。
            TimerSourceSetup::Pit { divisor } => write!(
                f,
                "divisor={} for a requested {} Hz; actual is {}.{:03} Hz \
                 (the divisor is an integer, so the period never matches exactly)",
                divisor,
                self.requested_hz,
                self.actual_millihertz / 1000,
                self.actual_millihertz % 1000
            ),
        }
    }
}

/// [`check_masks`] の結果。実測と期待の両方を、同じ 1 回の読み出しから持つ。
///
/// # 検査との関係（変更するとテストが落ちる）
///
/// この `Display` の文言と値に `interrupt-test timer` と
/// `interrupt-test no-eoi` が依存している（期待マーカー
/// `pic: IMR after unmasking IRQ0 master=0xfe slave=0xff`）。IMR の 2 バイト
/// という形は PIC 固有で、IO-APIC の redirection table では成り立たない。
/// S2 ではマーカー側を [`MaskCheck::matches`] の結果ベースへ移すこと。
///
/// # 期待を決めるのは呼び出し側である
///
/// [`check_masks`] が引数で「どの IRQ を開けたか」を受け取り、そこから
/// 期待値を計算する。境界が独自に期待を持つことはない。検証の主語は
/// 呼び出し側のままである。
pub struct MaskCheck {
    observed: MaskState,
    expected: MaskState,
}

/// コントローラごとのマスクの持ち方。**実装が増えたら列挙子を足す。**
///
/// # なぜ列挙子で持つのか（S2-b）
///
/// IMR の 2 バイトという形は PIC 固有で、IO-APIC の redirection table では
/// 成り立たない（実測で 24 本ある）。[`TimerSourceSetup`] と同じ理由で、
/// **2 つ目の実装が来ても列挙子を 1 つ足すだけで済む形**にしてある。
/// 既存の腕を触らないので、PIC の出力する文字列は変わらない。
#[derive(PartialEq, Eq, Clone, Copy)]
enum MaskState {
    Pic {
        master: u8,
        slave: u8,
    },
    /// I/O APIC の redirection entry のマスクビット（S2-d-1b で足した）。
    ///
    /// **ビットマップで持つ。** `bool` の配列にすると本数ぶんの領域が要り、
    /// この型は値として返す。添字は entry 番号 = GSI（この系では I/O APIC が
    /// 1 台で GSI base が 0）である。
    ///
    /// 幅は 256 ビット固定で、**取りこぼしが起こらない**。Max Redirection
    /// Entry は Version レジスタの 8 ビット欄なので、entry は最大 256 本である。
    IoApic {
        entries: usize,
        masked: [u64; MASK_BITMAP_WORDS],
    },
}

/// [`MaskState::IoApic`] のビットマップの語数。256 ビット = redirection entry の
/// 取りうる最大本数。
const MASK_BITMAP_WORDS: usize = 4;

impl MaskCheck {
    /// 実測が期待と一致しているか。**判定はこの値の比較で行い、`Display` の
    /// 文字列を突き合わせる形にはしない。** 書式を判定に載せると、書式を
    /// 変えた瞬間に静かに壊れる。
    pub fn matches(&self) -> bool {
        self.observed == self.expected
    }

    /// 実測部分だけの表示。期待値の書き方が呼び出し側ごとに違う場合に使う
    /// （「must still be …」のように前置きではなく後置きで書く行がある）。
    /// **同じ [`MaskCheck`] から導くので、判定と別の読み出しにはならない。**
    pub fn observed(&self) -> ObservedMasks {
        ObservedMasks {
            state: self.observed,
            with_bits: false,
        }
    }

    /// 実測部分を 2 進表記つきで表示する。再マップ前の IMR は「どのビットが
    /// 開いていたか」を後から読むための記録なので、ビット列で残している。
    pub fn observed_with_bits(&self) -> ObservedMasks {
        ObservedMasks {
            state: self.observed,
            with_bits: true,
        }
    }
}

/// 実測部分だけを表示する観測値（[`MaskCheck::observed`]）。
///
/// # 検査との関係
///
/// この表示を使う 2 行（`pic: IMR before remap …` と
/// `pit: IMR after configuring the PIT …`）に一致を取っているテストは
/// **現時点で無い**（xtask の期待マーカーを実測で確認した）。依存があるのは
/// [`MaskCheck`] の全体表示（`interrupt-test timer` / `no-eoi`）の方である。
pub struct ObservedMasks {
    state: MaskState,
    with_bits: bool,
}

impl fmt::Display for ObservedMasks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.state {
            // **この腕の文字列を変えない。**
            MaskState::Pic { master, slave } => {
                if self.with_bits {
                    write!(
                        f,
                        "master={master:#04x} ({master:#010b}) slave={slave:#04x} ({slave:#010b})"
                    )
                } else {
                    write!(f, "master={master:#04x} slave={slave:#04x}")
                }
            }
            MaskState::IoApic { entries, masked } => {
                write!(f, "entries={entries} masked=")?;
                write_mask_bitmap(f, entries, &masked)
            }
        }
    }
}

/// マスクのビットマップを 16 進で書く。**必要な語だけ出す。**
///
/// 本数に関係なく 4 語すべてを出すと、24 本の系で意味の無い 0 が 3 語並ぶ。
/// 上位の語から書くので、左端が最も大きい entry 番号側になる。
fn write_mask_bitmap(
    f: &mut fmt::Formatter<'_>,
    entries: usize,
    masked: &[u64; MASK_BITMAP_WORDS],
) -> fmt::Result {
    let words = entries
        .div_ceil(u64::BITS as usize)
        .clamp(1, MASK_BITMAP_WORDS);
    write!(f, "0x")?;
    for word in masked[..words].iter().rev() {
        write!(f, "{word:016x}")?;
    }
    Ok(())
}

/// このコントローラが担当するベクタ番号の範囲。
///
/// **ベクタ番号は境界の共通語彙である。** `idt` 側も
/// [`crate::idt::TIMER_VECTOR`] のようにベクタ番号で話すので、これを出すのは
/// 「生の値を出さない」方針に反しない。反するのは IMR のビットや ISR のような
/// **コントローラ内部の状態**であって、ベクタ番号ではない。
///
/// 返すのは連続範囲の下端と**上端（この値を含む）**であって、**IRQ の本数では
/// ない**。上端が包含であることを明記するのは、`0x2F`（含む）と `0x30`（含まない）
/// の取り違えが呼び出し側の比較を 1 つずらすためである。S2 で IO-APIC へ移す
/// ときに最も踏みやすい off-by-one になる。
///
/// 本数を返さないのは、「16 本」がシグネチャに焼き込まれ、24 本以上を扱う
/// IO-APIC で合わなくなるためである（[`check_masks`] がスライスを受けるのと
/// 同じ理由）。
/// 呼び出し側はこの範囲を IDT の覆う範囲と突き合わせるだけで、本数を知る
/// 必要が無い。
///
/// # 検査との関係
///
/// この値を使う行（`pic: target vectors 0x20..=0x2f are covered …`）に一致を
/// 取っているテストは現時点で無い。
pub const fn managed_vectors() -> (u8, u8) {
    let first = pic::MASTER_VECTOR_OFFSET;
    let last = pic::SLAVE_VECTOR_OFFSET + pic::IRQS_PER_PIC - 1;
    (first, last)
}

impl fmt::Display for MaskCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // **この腕の文字列を変えない。** `interrupt-test timer` と
        // `interrupt-test alt-offset` が `master=0xfe slave=0xff` に一致を取っている。
        match (self.observed, self.expected) {
            (
                MaskState::Pic { master, slave },
                MaskState::Pic {
                    master: expected_master,
                    slave: expected_slave,
                },
            ) => write!(
                f,
                "master={master:#04x} slave={slave:#04x} \
                 (expected {expected_master:#04x}/{expected_slave:#04x}) [read back from hardware]"
            ),
            (
                MaskState::IoApic { entries, masked },
                MaskState::IoApic {
                    masked: expected_masked,
                    ..
                },
            ) => {
                write!(f, "entries={entries} masked=")?;
                write_mask_bitmap(f, entries, &masked)?;
                write!(f, " (expected ")?;
                write_mask_bitmap(f, entries, &expected_masked)?;
                write!(f, ") [read back from hardware]")
            }
            // **実装をまたいだ比較は行わない。** 観測と期待は同じ
            // `check_masks` の呼び出しから作るので、腕が食い違うことはない。
            // 食い違ったら実装の誤りなので、黙って一致扱いにせず明示する。
            _ => write!(f, "observed and expected come from different controllers"),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `vector_for` と `irq_for` は互いの逆である。**往復で固定する。**
    ///
    /// 片方だけを見るテストだと、両方を同じ向きに間違えたときに通ってしまう。
    #[test]
    fn the_vector_and_irq_mappings_are_inverses() {
        for irq in 0..2 * pic::IRQS_PER_PIC {
            let vector = vector_for(irq).expect("every IRQ of the two PICs has a vector");
            assert_eq!(
                irq_for(vector),
                Some(irq),
                "round trip failed for IRQ {irq}"
            );
        }
    }

    /// 担当範囲の外は `None` を返す。
    #[test]
    fn vectors_outside_the_managed_range_have_no_irq() {
        let (first, last) = managed_vectors();
        for vector in 0..=u8::MAX {
            let inside = vector >= first && vector <= last;
            assert_eq!(
                irq_for(vector).is_some(),
                inside,
                "vector {vector:#04x} was classified wrongly"
            );
        }
    }

    /// **スレーブのベクタはマスタのベクタと連続している必要がない。**
    ///
    /// S2-b でこの依存を閉じた。以前は `MASTER_VECTOR_OFFSET .. +16` という 1 つの
    /// 連続範囲で見ており、スレーブ = マスタ + 8 を仮定していた。ここでは
    /// **その仮定が成り立つことを実際に確かめる**（現行の 2 構成ではどちらも
    /// 成り立つので、この確認は今は自明に通る）。仮定が崩れた構成が入ったとき、
    /// 上の 2 つのテストが連続範囲の実装を落とす。
    #[test]
    fn the_slave_range_is_derived_from_its_own_offset() {
        // マスタの最終ベクタの次がスレーブの先頭とは限らない、という前提で書く。
        let master_last = pic::MASTER_VECTOR_OFFSET + pic::IRQS_PER_PIC - 1;
        let slave_first = pic::SLAVE_VECTOR_OFFSET;

        assert_eq!(irq_for(master_last), Some(pic::IRQS_PER_PIC - 1));
        assert_eq!(irq_for(slave_first), Some(pic::IRQS_PER_PIC));

        // スレーブの先頭は、マスタのオフセットからの距離ではなく
        // スレーブ自身のオフセットから導かれている。
        assert_eq!(
            irq_for(slave_first + pic::IRQS_PER_PIC - 1),
            Some(2 * pic::IRQS_PER_PIC - 1)
        );
    }

    /// 期待マスクの計算は、開けた IRQ の分だけビットを落とす。
    #[test]
    fn the_expected_masks_open_only_the_requested_irqs() {
        assert_eq!(expected_masks(&[]), (pic::MASK_ALL, pic::MASK_ALL));
        let (master, slave) = expected_masks(&[0]);
        assert_eq!((master, slave), (0xFE, pic::MASK_ALL));
    }

    /// `TimerSetup` の問いは、実装固有の値と別に取り出せる。
    ///
    /// **S2-d でマーカーを問いベースへ移す先がここである。**
    #[test]
    fn the_timer_setup_exposes_implementation_independent_questions() {
        let setup = TimerSetup {
            requested_hz: 100,
            actual_millihertz: 99_998,
            source: TimerSourceSetup::Pit { divisor: 11_932 },
        };
        assert_eq!(setup.requested_hz(), 100);
        assert_eq!(setup.actual_millihertz(), 99_998);
    }

    /// `MaskCheck` の判定は値の比較で行い、`Display` の文字列に依存しない。
    #[test]
    fn the_mask_check_compares_values_rather_than_formatting() {
        let same = MaskCheck {
            observed: MaskState::Pic {
                master: 0xFE,
                slave: 0xFF,
            },
            expected: MaskState::Pic {
                master: 0xFE,
                slave: 0xFF,
            },
        };
        assert!(same.matches());

        let different = MaskCheck {
            observed: MaskState::Pic {
                master: 0xFF,
                slave: 0xFF,
            },
            expected: MaskState::Pic {
                master: 0xFE,
                slave: 0xFF,
            },
        };
        assert!(!different.matches());
    }
}
