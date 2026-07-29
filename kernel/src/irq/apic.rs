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
        Some(super::RedirectionEntryView::new(low))
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

    unsafe fn route(&self, irq: u8, vector: u8) {
        let Some(entry) = self.entry_for_irq(irq) else {
            return;
        };
        // 配送モードは Fixed（`000`）、宛先は物理モードで destination 0 = BSP。
        // **極性とトリガは Interrupt Source Override の解決に従う。**
        // この構成の IRQ1 には上書きが無いのでバス既定（active high・edge）に
        // なるが、**恒等であることに依存した書き方をしない。**
        let low = u32::from(vector)
            | self.mmio.redirection_flags_for_irq(irq)
            | crate::apic::ENTRY_MASKED_BIT;

        // SAFETY: 型の不変条件により写像済みのページである。**マスクビットを
        // 立てたまま書く**ので、この書き込みで割り込みが届き始めることはない。
        // high dword（宛先）は起動時の実測で全 entry が destination 0 なので触らない。
        unsafe { crate::apic::write_redirection_entry_low(self.io_apic_virt, entry, low) }
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

// **`TimerSource` はここで実装しない。S2-d-2 で足す。**
//
// 実装に要る入力が 2 つとも、この段には存在しない。
//
// - **初期カウント**は較正の戻り値（`crate::apic::TimerCalibration`）から
//   実行時に求める。**リテラルで焼かない**と決めてあるので、較正値を持たない
//   この段では正しい値を書けない。
// - **LVT Timer に載せるベクタ**（`0xFE`）は、専用スタブとハンドラを用意して
//   から使う。順序を守らないと、最初のティックで停止する。
//
// 仮の実装を置く案は採らない。`Ok` を返す空実装は「設定されていないのに
// 成功した」ように見え、`Err` を返すだけの実装は 2 つ目の実装とは呼べない。
// **書けるようになった段で書くほうが、書けないものを置くより正しい。**

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
