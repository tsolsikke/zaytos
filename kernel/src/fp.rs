//! 浮動小数点（x87 / SSE）の状態を退避・復元する（`ADR-0058`）。
//!
//! # ここが持つのは道具だけである
//!
//! **「いつ退避するか」は呼ぶ側が決める**——**切り替え（[`crate::task`]）と、
//! 遠征の境界（[`crate::userland`]）の 2 箇所である**（`ADR-0058` の Decision 2）。
//!
//! # カーネルは XMM を使わない
//!
//! **`ADR-0058` の Decision 5 である。** **カーネルが FP を使わないからこそ、
//! 「カーネルへ入って同じタスクへ戻るだけなら退避が要らない」と言える。**
//! **この前提は静的検査が守る**（`cargo xtask check` の「kernel が XMM を持たない」）。
//!
//! **したがって、ここでも XMM のレジスタ名を書かない。** 目印を載せるときも
//! [`restore`] を使う——**512 バイトの並びを作って読み込ませれば、XMM の命令は
//! 1 つも要らない。**

use common::cpu;

/// `fxsave` / `fxrstor` が使う領域（512 バイト・16 バイト境界）。
///
/// **境界は命令の要求である。** **外すと `#GP` になる**ので、型で守る。
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct FpArea([u8; Self::BYTES]);

impl FpArea {
    /// `fxsave` の領域の大きさ（Intel SDM）。
    pub const BYTES: usize = 512;

    /// FCW（x87 の制御語）の位置と、`fninit` 後の値。
    const FCW_OFFSET: usize = 0;
    const FCW_DEFAULT: u16 = 0x037f;
    /// MXCSR の位置と、リセット後の値（全例外マスク）。
    const MXCSR_OFFSET: usize = 24;
    const MXCSR_DEFAULT: u32 = 0x0000_1f80;
    /// MXCSR_MASK の位置。**0 のまま `fxrstor` すると、実装によっては
    /// 既定のマスク（0xFFBF）とみなされる。** **明示しておく。**
    const MXCSR_MASK_OFFSET: usize = 28;
    const MXCSR_MASK_DEFAULT: u32 = 0x0000_ffbf;

    /// まっさらな状態（`ADR-0058` の Decision 4）。
    ///
    /// **XMM のスロットは全部 0 である。** **前のプログラムのレジスタが
    /// 次のプログラムから読めてはならない。**
    pub const fn fresh() -> Self {
        let mut bytes = [0u8; Self::BYTES];
        let fcw = Self::FCW_DEFAULT.to_le_bytes();
        bytes[Self::FCW_OFFSET] = fcw[0];
        bytes[Self::FCW_OFFSET + 1] = fcw[1];
        let mxcsr = Self::MXCSR_DEFAULT.to_le_bytes();
        let mut index = 0;
        while index < 4 {
            bytes[Self::MXCSR_OFFSET + index] = mxcsr[index];
            index += 1;
        }
        let mask = Self::MXCSR_MASK_DEFAULT.to_le_bytes();
        index = 0;
        while index < 4 {
            bytes[Self::MXCSR_MASK_OFFSET + index] = mask[index];
            index += 1;
        }
        Self(bytes)
    }

    /// XMM0 の下位 8 バイトを読む（判定と破壊の観測に使う）。
    ///
    /// **XMM レジスタの並びは領域の 160 バイト目から、1 本 16 バイトである。**
    pub fn xmm0_low(&self) -> u64 {
        const XMM0_OFFSET: usize = 160;
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.0[XMM0_OFFSET..XMM0_OFFSET + 8]);
        u64::from_le_bytes(bytes)
    }
}

/// このコアで SSE を有効にする（`ADR-0058` の Decision 3）。
///
/// **CR0 と CR4 はコアごとのレジスタである。** **BSP と AP の両方で呼ぶ。**
/// **忘れたコアでは、SSE 命令が `#UD` で落ちる。**
///
/// # Safety
///
/// **起動の途中、そのコアにつき 1 回だけ呼ぶこと。** 割り込みが来ても
/// 困らないが、**FP を使うコードが走り始める前でなければならない。**
pub unsafe fn enable_on_this_cpu() {
    // **読んで、必要なビットだけを変えて書く。** 他のビットを落とさない。
    let cr0 = (cpu::read_cr0() | cpu::CR0_MONITOR_COPROCESSOR) & !cpu::CR0_EMULATION;
    // SAFETY: MP を立て EM を落とすだけで、PE や PG には触れていない。
    // 呼び出し側の契約（起動の途中、1 コア 1 回）。
    unsafe { cpu::write_cr0(cr0) };
    let cr4 = cpu::read_cr4() | cpu::CR4_OS_FXSR | cpu::CR4_OS_XMM_EXCEPT;
    // SAFETY: 既存の CR4 へ 2 ビットを足すだけである。PAE も PGE も落とさない。
    unsafe { cpu::write_cr4(cr4) };
}

/// いまこのコアで SSE が有効かを、レジスタから読んで返す（判定行に出すため）。
///
/// **書いたつもりではなく、読み戻した値で言う**（`gdt::set_rsp0` の読み戻しと
/// 同じ形である）。**AP でも呼ぶ**——**CR0 と CR4 はコアごとなので、
/// 「BSP で立てたから大丈夫」は言えない。**
pub fn enabled_state() -> Enabled {
    Enabled {
        cr0: cpu::read_cr0(),
        cr4: cpu::read_cr4(),
    }
}

/// [`enabled_state`] が読んだ値。
pub struct Enabled {
    /// いまの CR0。
    pub cr0: u64,
    /// いまの CR4。
    pub cr4: u64,
}

impl Enabled {
    /// 4 つのビットが意図どおりか。**これが判定である。**
    pub fn as_intended(&self) -> bool {
        self.cr0 & cpu::CR0_MONITOR_COPROCESSOR != 0
            && self.cr0 & cpu::CR0_EMULATION == 0
            && self.cr4 & cpu::CR4_OS_FXSR != 0
            && self.cr4 & cpu::CR4_OS_XMM_EXCEPT != 0
    }
}

/// いまの FP の状態を領域へ書き出す。
///
/// # Safety
///
/// `area` は 16 バイト境界の 512 バイトであること（型が保証する）。
pub unsafe fn save(area: &mut FpArea) {
    // SAFETY: `fxsave` は領域 512 バイトへ書くだけで、他には触れない。
    // 境界は `FpArea` の `repr(align(16))` が保証する。
    unsafe {
        core::arch::asm!("fxsave [{}]", in(reg) area.0.as_mut_ptr(), options(nostack));
    }
}

/// 領域から FP の状態を戻す。
///
/// # Safety
///
/// **領域の中身が `fxsave` が書いたものか、[`FpArea::fresh`] であること。**
/// **でたらめなバイト列を渡すと、MXCSR の予約ビットで `#GP` になる。**
pub unsafe fn restore(area: &FpArea) {
    // SAFETY: 呼び出し側の契約。`fxrstor` は領域 512 バイトを読むだけである。
    unsafe {
        core::arch::asm!("fxrstor [{}]", in(reg) area.0.as_ptr(), options(nostack, readonly));
    }
}
