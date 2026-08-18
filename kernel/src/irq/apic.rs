//! Local APIC / I/O APIC によるコントローラ実装（S2-d-1b）。
//!
//! **2 つ目の実装である。まだ選ばれない。** S2-d-1b は振る舞い不変の段で、
//! 配送は PIC / PIT のままである。切り替えは S2-d-1c（キーボード）と
//! S2-d-2（タイマ）で行う。
//!
//! # 1 つの型が 2 つのデバイスにまたがる
//!
//! [`Controller`] の担当が、マスクは I/O APIC、EOI とスプリアスは Local APIC
//! に分かれる。**PIC 側が 1 デバイス 1 実装だったのとは形が違う。** ここは
//! 2 デバイスをまとめたファサードであり、その事実を隠さずに書いておく。
//! 呼び出し側から見た問い（「この IRQ を開けたか」「後始末をしたか」）は
//! 1 つなので、境界の形は変えない。
//!
//! # レジスタの配置を知っているのはここではない
//!
//! オフセットとビット位置は [`crate::apic`] が持ち、こちらはそこが出す
//! 名前付きの操作だけを呼ぶ。**同じ事実を 2 箇所に置かない。**

use core::sync::atomic::{AtomicU64, Ordering};

use super::{Controller, MaskCheck, MaskState, MASK_BITMAP_WORDS};

/// 割り込み文脈から EOI を送るための Local APIC のアドレス（S2-d-1c）。
///
/// # なぜ [`Apic`] の値を持たないのか
///
/// EOI は**割り込みハンドラの中**から送る。そこで `Apic` の値を持つには
/// 内部可変性が要り、ロックを取れば割り込み文脈でのロックになる。
/// **EOI に要るのは Local APIC のアドレス 1 つだけ**なので、それだけを
/// アトミックで持つ。書き手は起動時の 1 回、読み手は割り込み文脈という形は
/// `TIMER_TICKS` と同じである。
///
/// [`Apic::new`] が設定する。[`NOT_INSTALLED`] は「まだ設定されていない」で、
/// その状態では EOI を送らない（送り先が無いので送りようがない）。
static LAPIC_EOI_BASE: AtomicU64 = AtomicU64::new(NOT_INSTALLED);

/// [`LAPIC_EOI_BASE`] の「未設定」。Local APIC が物理アドレス 0 に載ることは無い。
const NOT_INSTALLED: u64 = 0;

/// I/O APIC 経由へ移した IRQ の EOI を送る。
///
/// **IRQ 番号を取らない。** LAPIC の EOI は宛先を取らず、ISR が持つ最も
/// 優先度の高い割り込みを終わらせる。8259 のように書き手が指定する形ではない。
///
/// # Safety
///
/// 実際に配送された割り込みのハンドラの中から呼ぶこと。
pub(super) unsafe fn end_of_interrupt_for_routed_irq(spurious: bool) {
    // **スプリアスには EOI を送らない。** ただし下の関数が常に false を返すので、
    // この分岐が真になることは現時点で無い。**判定を書いておくのは、
    // 「送らない」が偶然ではなく判断の結果であることを残すためである。**
    if spurious {
        return;
    }
    let base = LAPIC_EOI_BASE.load(Ordering::Relaxed);
    if base == NOT_INSTALLED {
        // 経路が移っているのにアドレスが無い、という組み合わせは
        // `route_to_apic` の順序では起こらない（`Apic::new` が先に走る）。
        // それでも黙って 0 番地へ書かないよう、ここで止める。
        return;
    }
    // SAFETY: `Apic::new` が写像を確認した Local APIC のページ先頭を入れている。
    unsafe { crate::apic::send_end_of_interrupt(base) }
}

/// I/O APIC 経由へ移した IRQ がスプリアスか。**常に `false` である。**
///
/// 理由は [`Controller::is_spurious`] の実装と同じで、そちらに書いてある。
///
/// # Safety
///
/// 呼び出し側の契約は [`super::is_spurious`] と同じ。**実際には何も読まない**
/// ので危険は無いが、シグネチャを揃えてある。
pub(super) unsafe fn spurious_for_routed_irq(_irq: u8) -> bool {
    false
}

/// Local APIC と I/O APIC の組。
///
/// # 不変条件
///
/// `io_apic_virt` は、[`crate::apic::map_and_probe`] が写像を確認したページの
/// 先頭である。**この型を作れるのは [`Self::new`] だけ**で、そこが
/// `MappedApic` を要求するので、写像されていないアドレスから作ることはできない。
///
/// # Local APIC のアドレスを持たない
///
/// EOI に要る Local APIC のアドレスは [`LAPIC_EOI_BASE`] が持つ。
/// **割り込み文脈から読む必要があるので、値ではなくアトミックに置いてある。**
/// この型にも同じ値を持たせると、EOI の送り先が 2 箇所に出る。
/// [`Self::new`] がアトミックへ書き込み、以後はそちらだけを読む。
pub struct Apic {
    io_apic_virt: u64,
    /// この I/O APIC が担当する先頭の GSI。entry 添字への変換に要る。
    gsi_base: u32,
    /// redirection entry の**本数**（添字の最大値ではない）。
    entry_count: u32,
    /// IRQ から GSI への解決表（S2-d-0）。
    mmio: crate::acpi::ApicMmio,
}

impl Apic {
    /// 写像済みの APIC からコントローラを作る。I/O APIC が無ければ `None`。
    ///
    /// **`MappedApic` を要求するのが安全性の要である。** 生のアドレスを
    /// 受け取る形にすると、写像していないページを渡せてしまう。
    pub fn new(mapped: &crate::apic::MappedApic) -> Option<Self> {
        let direct_map = common::addr::direct_map();
        let io_apic = mapped.first_io_apic()?;
        let io_apic_virt = direct_map.phys_to_virt(io_apic.phys).as_u64();

        // SAFETY: `map_and_probe` が写像を確認したページの先頭である。読み取りのみ
        // （IOREGSEL への添字の書き込みを伴うが、割り込みの設定は変えない）。
        // 単一コアで、他の実行文脈がこの I/O APIC を触っていない。
        let entry_count = unsafe { crate::apic::redirection_entry_count(io_apic_virt) };

        let lapic_virt = direct_map.phys_to_virt(mapped.local_apic_phys()).as_u64();
        // 割り込み文脈から EOI を送るために控える（S2-d-1c）。
        LAPIC_EOI_BASE.store(lapic_virt, Ordering::Relaxed);

        Some(Self {
            io_apic_virt,
            gsi_base: io_apic.global_system_interrupt_base,
            entry_count,
            mmio: mapped.mmio(),
        })
    }

    /// IRQ に対応する redirection entry の添字。担当外なら `None`。
    ///
    /// **恒等であることに依存しない。** この系ではキーボード（IRQ1）に
    /// Interrupt Source Override が無いので結果は恒等になるが、解決表を
    /// 通す経路そのものは常に通す。恒等を前提に書くと、上書きのある IRQ を
    /// 扱った瞬間に静かに誤る。
    fn entry_for_irq(&self, irq: u8) -> Option<u8> {
        let gsi = self.mmio.gsi_for_irq(irq);
        let index = gsi.checked_sub(self.gsi_base)?;
        if index >= self.entry_count {
            return None;
        }
        u8::try_from(index).ok()
    }

    /// この IRQ の redirection entry を読み戻す。担当外なら `None`。
    pub(super) fn read_entry(&self, irq: u8) -> Option<super::RedirectionEntryView> {
        let entry = self.entry_for_irq(irq)?;
        // SAFETY: 型の不変条件により写像済みのページである。読み取りのみ。
        let low = unsafe { crate::apic::read_redirection_entry_low(self.io_apic_virt, entry) };
        // **high dword も読む（S4-a）。宛先はこちらにある。**
        // SAFETY: 同上。読み取りのみ。
        let high = unsafe { crate::apic::read_redirection_entry_high(self.io_apic_virt, entry) };
        Some(super::RedirectionEntryView::new(low, high))
    }

    /// 全 entry のマスクビットを 1 回ずつ読む。
    ///
    /// # Safety
    ///
    /// 写像済みのページであること（型の不変条件）。他の実行文脈が同じ
    /// I/O APIC を触っていないこと。
    unsafe fn read_mask_bitmap(&self) -> [u64; MASK_BITMAP_WORDS] {
        let mut masked = [0u64; MASK_BITMAP_WORDS];
        for entry in 0..self.entry_count {
            let Ok(entry) = u8::try_from(entry) else {
                break;
            };
            // SAFETY: 呼び出し元契約。読み取りのみ。
            let low = unsafe { crate::apic::read_redirection_entry_low(self.io_apic_virt, entry) };
            if low & crate::apic::ENTRY_MASKED_BIT != 0 {
                set_bit(&mut masked, entry);
            }
        }
        masked
    }
}

/// ビットマップの `index` 番目を立てる。
fn set_bit(bitmap: &mut [u64; MASK_BITMAP_WORDS], index: u8) {
    let index = index as usize;
    bitmap[index / u64::BITS as usize] |= 1u64 << (index % u64::BITS as usize);
}

impl Controller for Apic {
    unsafe fn unmask(&self, irq: u8) {
        let Some(entry) = self.entry_for_irq(irq) else {
            return;
        };
        // SAFETY: 型の不変条件により写像済みのページである。マスクビットだけを
        // 落とす read-modify-write で、ベクタ欄と配送設定は保つ。
        unsafe {
            let low = crate::apic::read_redirection_entry_low(self.io_apic_virt, entry);
            crate::apic::write_redirection_entry_low(
                self.io_apic_virt,
                entry,
                low & !crate::apic::ENTRY_MASKED_BIT,
            );
        }
    }

    unsafe fn mask_all(&self) {
        for entry in 0..self.entry_count {
            let Ok(entry) = u8::try_from(entry) else {
                break;
            };
            // SAFETY: 上と同じ。マスクビットだけを立てる。
            unsafe {
                let low = crate::apic::read_redirection_entry_low(self.io_apic_virt, entry);
                crate::apic::write_redirection_entry_low(
                    self.io_apic_virt,
                    entry,
                    low | crate::apic::ENTRY_MASKED_BIT,
                );
            }
        }
    }

    unsafe fn end_of_interrupt(&self, _irq: u8, spurious: bool) {
        // **割り込み文脈の経路と同じ関数を通す。** ここで `self.lapic_virt` を
        // 直接使うと、EOI の送り方が 2 箇所に出る。値の出所は同じ
        // （`Apic::new` が控えたもの）なので、実装を 1 つに寄せてある。
        //
        // SAFETY: 呼び出し側の契約をそのまま引き継ぐ。
        unsafe { end_of_interrupt_for_routed_irq(spurious) }
    }

    unsafe fn is_spurious(&self, _irq: u8) -> bool {
        // **常に false である。理由は 2 つあり、どちらか片方では足りない。**
        //
        // 1. **PIC の IRQ7 / IRQ15 に相当する機序が LAPIC に無い。** 8259 は
        //    割り込み要求が INTA サイクルまでに取り下げられると、既定の IRQ
        //    番号で偽の割り込みを上げる。LAPIC はその形の偽装を持たない。
        // 2. **LAPIC のスプリアスはベクタで判定される。** SVR のベクタ欄
        //    （`crate::apic::SPURIOUS_VECTOR`）で上がり、`idt::irq_entry` が
        //    IRQ 番号へ変換する手前で名指しに判定して返す（S2-d-1a）。
        //    **したがってこの経路までスプリアスは降りてこない。**
        //
        // 1 だけだと「LAPIC にスプリアスは無い」と読めて誤りであり、2 だけだと
        // 「いずれここへ来る」と読めて誤る。**両方を残すこと。**
        false
    }

    unsafe fn route(&self, irq: u8, vector: u8, signaling: super::RouteSignaling) {
        let Some(entry) = self.entry_for_irq(irq) else {
            return;
        };
        // 配送モードは Fixed（`000`）、宛先は物理モードで destination 0 = BSP。
        // **極性とトリガは Interrupt Source Override の解決に従う。**
        // この構成の IRQ1 には上書きが無いのでバス既定（active high・edge）に
        // なるが、**恒等であることに依存した書き方をしない。**
        //
        // **宛先は S4-a から主張になった。** それまでは「起動時の実測で全 entry が
        // destination 0 なので high dword を触らない」と書いていたが、
        // **実測の記憶であって主張ではなかった。** AP が割り込みを受けられるように
        // なると、「この IRQ は AP へ届かない」が S4-a の安全の根拠になるので、
        // physical モードと宛先を読み戻して主張する（`main.rs` の読み戻し）。
        // 鳴り方の決め方（S13-d。ADR-0035 の Addendum）: **firmware の宣言が
        // 最優先である。** 実測で QEMU の MADT は PCI リンクの GSI（5/9/10/11）
        // に override を持ち、IRQ 11 を level・active-high と宣言している——
        // **PCI の規定（level・low）を機械的に書くと、platform の宣言と
        // 食い違う。** 申告（`RouteSignaling`）は override が無いときの
        // 既定としてだけ使う。
        let override_flags = self.mmio.redirection_flags_for_irq(irq);
        let signaling_flags = if self.mmio.has_override_for_irq(irq) {
            override_flags
        } else {
            match signaling {
                super::RouteSignaling::EdgeHigh => 0,
                super::RouteSignaling::LevelLow => {
                    crate::apic::ENTRY_LEVEL_TRIGGERED_BIT | crate::apic::ENTRY_ACTIVE_LOW_BIT
                }
            }
        };
        // 破壊 (S13-d, virtio-intx-edge-test): 宣言も申告も無視して、生の
        // エッジ・ハイで書く。**実測で、QEMU では届いてしまう**（極性と
        // トリガを厳密に模っていない）——**捕まえるのは読み戻しである**
        // （entry の level が宣言と食い違う）。
        #[cfg(feature = "virtio-intx-edge-test")]
        let signaling_flags = {
            let _ = signaling_flags;
            0
        };
        let low = u32::from(vector) | signaling_flags | crate::apic::ENTRY_MASKED_BIT;

        // 破壊 (S4-a, ioapic-keyboard-broadcast): 宛先を logical の broadcast に
        // する。**確実に落ちるのは読み戻しの主張のほうである。** 配送が実際に
        // どうなるか（AP が受けて共有リングバッファへ積むか）は観測していない。
        #[cfg(feature = "ioapic-keyboard-broadcast-test")]
        let low = low | crate::apic::ENTRY_DESTINATION_MODE_BIT;

        // SAFETY: 型の不変条件により写像済みのページである。**マスクビットを
        // 立てたまま書く**ので、この書き込みで割り込みが届き始めることはない。
        unsafe { crate::apic::write_redirection_entry_low(self.io_apic_virt, entry, low) }

        // 破壊 (S4-a, ioapic-keyboard-broadcast): high dword の宛先も broadcast へ。
        // SAFETY: 同上。既定ビルドではこのブロックごと消える。
        #[cfg(feature = "ioapic-keyboard-broadcast-test")]
        unsafe {
            crate::apic::write_redirection_entry_high(self.io_apic_virt, entry, 0xFF00_0000)
        }
    }

    fn check_masks(&self, unmasked: &[u8]) -> MaskCheck {
        // SAFETY: 型の不変条件により写像済みのページである。読み取りのみで、
        // 単一コアの起動シーケンス中にだけ通る。
        let observed = unsafe { self.read_mask_bitmap() };

        // 期待値は「開けたと言われた IRQ 以外はすべてマスク」である。
        let mut expected = [0u64; MASK_BITMAP_WORDS];
        for entry in 0..self.entry_count {
            let Ok(entry) = u8::try_from(entry) else {
                break;
            };
            set_bit(&mut expected, entry);
        }
        for irq in unmasked {
            if let Some(entry) = self.entry_for_irq(*irq) {
                clear_bit(&mut expected, entry);
            }
        }

        let entries = self.entry_count as usize;
        MaskCheck {
            observed: MaskState::IoApic {
                entries,
                masked: observed,
            },
            expected: MaskState::IoApic {
                entries,
                masked: expected,
            },
        }
    }
}

/// ビットマップの `index` 番目を落とす。
fn clear_bit(bitmap: &mut [u64; MASK_BITMAP_WORDS], index: u8) {
    let index = index as usize;
    bitmap[index / u64::BITS as usize] &= !(1u64 << (index % u64::BITS as usize));
}

/// Local APIC タイマ（S2-d-2）。**`TimerSource` の 2 つ目の実装である。**
///
/// # 較正の戻り値を丸ごと持つ
///
/// 周波数だけを受け取る形にしない。**分周設定と対で持たないと、較正時と
/// 運用時で分周が食い違う罠が開く**（`TimerCalibration` の doc）。
pub struct LapicTimer {
    calibration: crate::apic::TimerCalibration,
}

impl LapicTimer {
    /// 較正の結果からタイマ源を作る。
    ///
    /// **`Apic::new` が先に走っていること**（Local APIC のアドレスを
    /// [`LAPIC_EOI_BASE`] へ入れるのはあちらである）。
    pub(super) const fn new(calibration: crate::apic::TimerCalibration) -> Self {
        Self { calibration }
    }
}

impl super::TimerSource for LapicTimer {
    unsafe fn configure_timer(
        &self,
        frequency_hz: u32,
    ) -> Result<super::TimerSetup, super::TimerError> {
        let base = LAPIC_EOI_BASE.load(Ordering::Relaxed);
        if base == NOT_INSTALLED {
            return Err(super::TimerError::lapic_not_mapped());
        }
        if frequency_hz == 0 {
            return Err(super::TimerError::frequency_out_of_range());
        }

        // **初期カウントは較正の戻り値から実行時に求める。リテラルで焼かない。**
        let initial_count = self.calibration.median_hz() / u64::from(frequency_hz);
        let Ok(initial_count) = u32::try_from(initial_count) else {
            return Err(super::TimerError::frequency_out_of_range());
        };
        if initial_count == 0 {
            return Err(super::TimerError::frequency_out_of_range());
        }

        // **分周は較正の戻り値に含まれているものを使う。** 別の値を書くと、
        // 較正した速さと数え下がる速さが食い違う。
        let divide = self.calibration.divide_configuration();

        // **マスクしたまま設定する。** 解禁は別（`unmask_timer`）で、
        // その前に PIC を全マスクする必要がある。
        let lvt = super::LAPIC_TIMER_VECTOR_BITS
            | crate::apic::LVT_TIMER_PERIODIC
            | crate::apic::ENTRY_MASKED_BIT;

        // SAFETY: `base` は `Apic::new` が写像を確認した Local APIC のページ
        // 先頭である。マスクを立てたまま書くので、ここでティックは始まらない。
        unsafe { crate::apic::program_timer(base, divide, lvt, initial_count) };

        // **AP が同じ設定を自分の LVT へ書けるように控える（S4-a）。**
        // **較正はやり直さない。** BSP の較正値を共有するのは「LAPIC タイマの
        // 周波数がコア間で同じ」という**仮定**である。仮定なので、AP 側の
        // ティックのレートをホストの実時間と突き合わせて実測検証する
        // （`lapic-timer-test` と同型の独立基準）。
        LAPIC_TIMER_PROGRAM.store(pack_timer_program(divide, initial_count), Ordering::Release);

        // 実効周波数は**書いた初期カウントと較正値から導く。** 要求値ではない。
        let actual_millihertz =
            self.calibration.median_hz().saturating_mul(1000) / u64::from(initial_count);

        Ok(super::TimerSetup::lapic(
            frequency_hz,
            actual_millihertz,
            initial_count,
            divide,
        ))
    }
}

/// BSP が Local APIC タイマへ書いた設定（S4-a）。**AP が同じ値を自分へ書く。**
///
/// # なぜ 2 つを 1 語に詰めるのか
///
/// **分周と初期カウントは対でなければ意味を持たない**（`TimerCalibration` の
/// doc と同じ理由である）。別々のアトミックにすると、AP が「新しい分周と古い
/// 初期カウント」を読む窓が開く。**1 語なら、その組み合わせは作れない。**
static LAPIC_TIMER_PROGRAM: AtomicU64 = AtomicU64::new(NOT_PROGRAMMED);

/// [`LAPIC_TIMER_PROGRAM`] の「まだ設定されていない」。
///
/// 初期カウント `0` はタイマを止める値なので、正当な設定として現れない。
const NOT_PROGRAMMED: u64 = 0;

/// 分周と初期カウントを 1 語へ詰める。
const fn pack_timer_program(divide_configuration: u32, initial_count: u32) -> u64 {
    ((divide_configuration as u64) << 32) | (initial_count as u64)
}

/// [`pack_timer_program`] の逆。
const fn unpack_timer_program(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// このコアの Local APIC の SVR を設定する（S4-a）。
///
/// **BSP の `apic::set_spurious_vector` は BSP の Local APIC にしか効いていない。**
/// SVR はコアごとにあるので、AP は自分で書く。
///
/// # Safety
///
/// 自コアの単一文脈から、割り込み禁止で呼ぶこと。
pub(super) unsafe fn set_spurious_vector_for_this_cpu() -> Option<crate::apic::SpuriousVectorWrite>
{
    let base = LAPIC_EOI_BASE.load(Ordering::Relaxed);
    if base == NOT_INSTALLED {
        return None;
    }
    // SAFETY: `Apic::new` が写像を確認したページである。書き込みはベクタ欄だけ。
    // **AP は bit 8 を立てる。** INIT-SIPI で起きたコアの Local APIC はリセット
    // 状態から始まり、SVR は `0x000000FF` で **bit 8 が落ちている**。
    // 実測でそうだった（`SoftwareEnable` の doc）。
    Some(unsafe { crate::apic::write_spurious_vector(base, crate::apic::SoftwareEnable::Set) })
}

/// このコアの Local APIC タイマを、BSP と同じ設定で開ける（S4-a）。
///
/// # 何をして、何をしないか
///
/// **するのは自コアの LVT・分周・初期カウントの設定と解禁だけである。**
/// 8259 の全マスクはしない（BSP が済ませた大域の操作で、コアごとではない）。
/// 移行状態も立てない（`TIMER_ON_LAPIC` は大域で、BSP が立てている）。
///
/// # Safety
///
/// - 自コアの IDT が載っており、`LAPIC_TIMER_VECTOR` に戻れるハンドラがあること。
/// - 自コアの SVR がソフトウェア有効であること（bit 8）。
/// - 割り込みが禁止されていること。**戻った時点からティックが届きうる。**
pub(super) unsafe fn arm_timer_for_this_cpu() -> Option<(u32, u32)> {
    let base = LAPIC_EOI_BASE.load(Ordering::Relaxed);
    if base == NOT_INSTALLED {
        return None;
    }
    let packed = LAPIC_TIMER_PROGRAM.load(Ordering::Acquire);
    if packed == NOT_PROGRAMMED {
        return None;
    }
    let (divide, initial_count) = unpack_timer_program(packed);

    let lvt = super::LAPIC_TIMER_VECTOR_BITS
        | crate::apic::LVT_TIMER_PERIODIC
        | crate::apic::ENTRY_MASKED_BIT;

    // SAFETY: 呼び出し元契約。**マスクを立てたまま設定してから外す**ので、
    // ベクタが載る前に満了することはない。base は自コアの Local APIC を指す
    // （この物理アドレスは実行中のコア自身の LAPIC に別名づけられている）。
    unsafe {
        crate::apic::program_timer(base, divide, lvt, initial_count);
        crate::apic::unmask_lvt_timer(base);
    }
    Some((divide, initial_count))
}

/// LVT Timer のマスクを外す（S2-d-2）。**ここからティックが届く。**
///
/// # Safety
///
/// - ベクタが設定済みで、そのベクタに戻れるハンドラが IDT にあること。
/// - **PIC 側のタイマが既に黙っていること。** 両方開くと二重に届く。
pub(super) unsafe fn unmask_timer() {
    let base = LAPIC_EOI_BASE.load(Ordering::Relaxed);
    if base == NOT_INSTALLED {
        return;
    }
    // SAFETY: 呼び出し元契約。マスクビットだけを落とす。
    unsafe { crate::apic::unmask_lvt_timer(base) }
}

/// LVT Timer の現在値を読み戻す（S2-d-2）。
pub(super) fn read_lvt_timer() -> Option<u32> {
    let base = LAPIC_EOI_BASE.load(Ordering::Relaxed);
    if base == NOT_INSTALLED {
        return None;
    }
    // SAFETY: `Apic::new` が写像を確認したページである。読み取りのみ。
    Some(unsafe { crate::apic::read_lvt_timer(base) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ビットマップの立て下げは、語をまたいでも正しい位置に効く。
    ///
    /// **境界（63 / 64）を含める。** ここを間違えると、entry 64 以上を持つ
    /// I/O APIC でマスクの判定が静かにずれる。
    #[test]
    fn the_bitmap_addresses_the_right_word_and_bit() {
        let mut bitmap = [0u64; MASK_BITMAP_WORDS];
        set_bit(&mut bitmap, 0);
        set_bit(&mut bitmap, 63);
        set_bit(&mut bitmap, 64);
        set_bit(&mut bitmap, 255);
        assert_eq!(bitmap[0], (1u64 << 63) | 1);
        assert_eq!(bitmap[1], 1);
        assert_eq!(bitmap[3], 1u64 << 63);

        clear_bit(&mut bitmap, 63);
        clear_bit(&mut bitmap, 64);
        assert_eq!(bitmap[0], 1);
        assert_eq!(bitmap[1], 0);
    }
}
