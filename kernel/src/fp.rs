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

    /// XMM0 の下位 8 バイトを書く（破壊だけが使う。B-d）。
    #[cfg(feature = "fp-clobber-on-kernel-entry-test")]
    fn set_xmm0_low(&mut self, value: u64) {
        const XMM0_OFFSET: usize = 160;
        self.0[XMM0_OFFSET..XMM0_OFFSET + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// MXCSR を書く（破壊だけが使う。B-d）。
    #[cfg(feature = "fp-clobber-on-kernel-entry-test")]
    fn set_mxcsr(&mut self, value: u32) {
        self.0[Self::MXCSR_OFFSET..Self::MXCSR_OFFSET + 4].copy_from_slice(&value.to_le_bytes());
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

/// システムコールの入口で FP の状態を塗る（破壊。B-d）。
///
/// # 何を反証するか
///
/// **`ADR-0058` の Decision 2 は「カーネルは FP を使わないので、カーネルへ
/// 入って同じタスクへ戻るだけなら退避が要らない」と言っている。**
/// **この主張には判定が無かった**——**Decision 5 の静的検査（kernel が XMM を
/// 持たない）は「壊さない」の根拠だが、「壊しても気づかない」ことは示して
/// いない。** **これがそれを示す。**
///
/// # 2 つ塗る——レジスタと MXCSR
///
/// **XMM0 だけでは、落ちるかどうかが時機に依った**（実測。2026-09-11。
/// **3 回走らせて 1 回しか落ちなかった**）。**理由は、C の呼び出し規約では
/// XMM が全部 caller-saved で、ユーザーの FP がレジスタに生きている窓が
/// 1 文字の描画の中では 100 マイクロ秒ほどしかないことである**
/// （**ティックは 10 ミリ秒**）。
///
/// **MXCSR は違う。** **丸めの制御ビットは呼び出しを跨いで保たれる約束で、
/// コンパイラは退避も復元もしない。** **したがって、入口で丸めを変えれば、
/// その後の浮動小数点は必ず違う答えを出す。**
///
/// **`A`@16px では、丸めを 0 方向へ変えるとビットマップが変わる**
/// （実測。2026-09-11。ホストで最近接偶数・0 方向・+∞ 方向の 3 通りを
/// 比べた。**`W`@48px は変わらなかった**ので、**字と倍率に依る**）。
///
/// # XMM の命令を増やさない
///
/// **[`restore`] で 512 バイトの並びを読み込ませる。** **XMM のレジスタ名を
/// 書かないので、Decision 5 の静的検査と衝突しない**（このモジュールの
/// 冒頭の注記と同じ手である）。
#[cfg(feature = "fp-clobber-on-kernel-entry-test")]
pub fn clobber_on_kernel_entry() {
    /// 塗る値。**0 ではない**——**まっさらと見分けが付かなくなる。**
    const MARKER: u64 = 0xDEAD_BEEF_DEAD_BEEF;
    /// 丸めを 0 方向にした MXCSR（既定の `0x1F80` に RC の 2 ビットを足す）。
    const MXCSR_TOWARD_ZERO: u32 = 0x0000_7f80;

    let mut area = FpArea::fresh();
    area.set_xmm0_low(MARKER);
    area.set_mxcsr(MXCSR_TOWARD_ZERO);
    // SAFETY: [`FpArea::fresh`] が作った並びに、XMM0 の 8 バイトを書いただけ
    // である。MXCSR とその mask は `fresh` のままなので、予約ビットで `#GP`
    // にはならない（[`restore`] の契約）。
    unsafe { restore(&area) };
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
