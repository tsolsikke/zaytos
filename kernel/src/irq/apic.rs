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

use super::{Controller, MaskCheck, MaskState, MASK_BITMAP_WORDS};

/// Local APIC と I/O APIC の組。
///
/// # 不変条件
///
/// `lapic_virt` と `io_apic_virt` は、[`crate::apic::map_and_probe`] が写像を
/// 確認したページの先頭である。**この型を作れるのは
/// [`Self::new`] だけ**で、そこが `MappedApic` を要求するので、
/// 写像されていないアドレスから作ることはできない。
pub struct Apic {
    lapic_virt: u64,
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

        Some(Self {
            lapic_virt: direct_map.phys_to_virt(mapped.local_apic_phys()).as_u64(),
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
        // **スプリアスには EOI を送らない。** PIC と同じ結論だが理由は違い、
        // こちらは「LAPIC がスプリアスを ISR に載せないので、送ると別の
        // 割り込みを終わらせてしまう」である。
        if spurious {
            return;
        }
        // **IRQ 番号を捨てる。** LAPIC の EOI は宛先を取らず、ISR が持つ
        // 最も優先度の高い割り込みを終わらせる。PIC のように「どの IRQ か」を
        // 書き手が指定する形ではない。
        //
        // SAFETY: 型の不変条件により写像済みのページである。実際に配送された
        // 割り込みのハンドラからのみ呼ばれることは、呼び出し側の契約である。
        unsafe { crate::apic::send_end_of_interrupt(self.lapic_virt) }
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
