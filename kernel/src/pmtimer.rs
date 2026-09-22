//! ACPI の Power Management タイマ（HW-c。`ADR-0068`）。
//!
//! **unsafe を含む。** ポートを 1 本読む。
//!
//! # 何のために在るのか
//!
//! **PIT が刻まない機械で、Local APIC タイマを較正する基準にする。** **PIT のティックが 1 本も
//! 来ないと、較正は諦め、タイマは PIT のまま進み、ティックを待つ所で黙る**（実測。`pit=off`。
//! `docs/hardware-inventory.md`）。**PM タイマは割り込みを要らず、ポートを読むだけである。**
//!
//! # 何をしないか
//!
//! - **時計として使わない。** **較正の窓を測るためだけに読む**（`crate::apic`）。
//! - **書かない。** **PM タイマは読み出し専用のカウンタである。**
//! - **在りかは決めない。** **ポートと幅は FADT が言う**（`crate::acpi`）——**既定値を焼き込まない。**
//!
//! # 幅は 24 か 32 である
//!
//! **FADT の `Flags` の TMR_VAL_EXT が言う。** **24 ビットなら約 4.7 秒で一周する**
//! （3.579545MHz）。**差を取るときは幅で包み込む**（[`PmTimer::elapsed`]）——**忘れると、
//! 一周した窓で巨大な差が出て、較正が過大になる。**

use common::port::inl;

/// PM タイマの周波数（ACPI が定める値）。**機械に依らない。**
///
/// 破壊 (HW-c, pm-timer-double-frequency): **2 倍にする。** **較正が 2 倍に出て、タイマは半分の
/// 速さで走る。** **カーネル内の比は自己無矛盾のままなので、実時間と突き合わせて初めて見える**
/// （`lapic-timer-test` の速さの判定）。
pub const HZ: u64 = if cfg!(feature = "pm-timer-double-frequency") {
    2 * 3_579_545
} else {
    3_579_545
};

/// いちばん狭い幅（ビット）。**24 ビットの PM タイマは約 4.7 秒で一周する。**
///
/// **較正の窓がこの一周より十分短いことを、`crate::apic` が const assert で守る。**
pub const NARROWEST_WIDTH_BITS: u32 = 24;

/// PM タイマの所在。**FADT から作る**（`crate::acpi`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PmTimer {
    port: u16,
    bits: u8,
}

impl PmTimer {
    /// FADT が言ったポートと幅で作る。
    pub const fn new(port: u16, bits: u8) -> Self {
        Self { port, bits }
    }

    /// 読むポート（判定行に出す）。
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// 数える幅（24 か 32）。
    pub const fn bits(&self) -> u8 {
        self.bits
    }

    /// いまの値を読む。
    ///
    /// **副作用は無い**（読み出し専用のカウンタである）。**幅が 24 ビットなら上位は 0 である**
    /// （仕様。**こちらで切らない**——**切ると「上位にゴミを返す機械」を隠す。** 差は
    /// [`Self::elapsed`] が幅で包み込む）。
    pub fn read(&self) -> u32 {
        // SAFETY: FADT が PM タイマの在りかとして名乗ったポートを読むだけである。
        // **書かない。** 読み出しはカウンタの値を返すだけで、装置の状態を変えない。
        unsafe { inl(self.port) }
    }

    /// 2 回の読みの間に進んだ刻み（純粋ロジック）。**幅で包み込む。**
    pub const fn elapsed(&self, before: u32, after: u32) -> u32 {
        elapsed_with_width(before, after, self.bits)
    }
}

/// 幅を明示して差を取る（純粋ロジック。テストのために分けてある）。
pub const fn elapsed_with_width(before: u32, after: u32, bits: u8) -> u32 {
    let mask = if bits >= 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    };
    after.wrapping_sub(before) & mask
}

/// Local APIC タイマの周波数（Hz）を、減った数と PM タイマの刻みから求める（純粋ロジック）。
///
/// **PIT 基準の式と同じ形である**——**窓の実時間で割る**（`crate::apic` の較正）。
/// **刻みが 0 なら 0 を返す**（割らない）。
pub const fn lapic_hz_from_ticks(lapic_counts: u64, pm_ticks: u64) -> u64 {
    if pm_ticks == 0 {
        return 0;
    }
    lapic_counts * HZ / pm_ticks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **幅で包み込む。** 24 ビットの一周を跨いだ窓でも、差は窓の長さである。
    #[test]
    fn the_difference_wraps_at_the_declared_width() {
        let timer = PmTimer::new(0x608, 24);
        assert_eq!(timer.elapsed(10, 30), 20);
        assert_eq!(timer.elapsed(0xff_fff0, 0x10), 0x20, "24 ビットで一周した");
        assert_eq!(
            PmTimer::new(0x608, 32).elapsed(0xff_fff0, 0x10),
            0xff00_0020,
            "32 ビットなら同じ値は一周していない"
        );
    }

    /// **幅の外のビットは差に混ぜない**（上位にゴミを返す機械でも、24 ビットぶんだけを見る）。
    #[test]
    fn bits_above_the_width_do_not_reach_the_difference() {
        let timer = PmTimer::new(0x608, 24);
        // 幅の外（上位 8 ビット）が違っていても、差は下位 24 ビットの差である。
        assert_eq!(timer.elapsed(0xdead_0010, 0xbeef_0030), 0x42_0020);
        assert_eq!(
            timer.elapsed(0xdead_0010, 0xbeef_0030),
            timer.elapsed(0xad_0010, 0xef_0030)
        );
    }

    /// **周波数は窓の実時間で割る。**
    #[test]
    fn the_frequency_comes_from_the_observed_window() {
        // 100ms の窓（PM タイマの刻み 357,954）で 1,000,000 数え下がれば約 10MHz。
        assert_eq!(lapic_hz_from_ticks(1_000_000, 357_954), 10_000_013);
        assert_eq!(lapic_hz_from_ticks(1_000_000, 0), 0, "割らない");
    }
}
