//! PIT（8253/8254 Programmable Interval Timer）の設定（M4-d-2）。
//!
//! **unsafe を含む。** I/O ポートを直接叩く。
//!
//! ADR-0018 §6 のとおり、M4 のタイマは APIC ではなく PIT を使う。APIC は
//! ACPI（MADT）を辿る必要があり、「割り込みを初めて有効化する」ことと
//! 「ACPI を初めて辿る」ことを同時にやると切り分け軸が 2 つ増える。PIT は
//! I/O ポート直叩きで完結する。
//!
//! ## 分周値と、周期が要求値と一致しないこと
//!
//! PIT の入力クロックは 1,193,182 Hz で固定されている。この半端な値は、
//! 初代 IBM PC が NTSC のカラーバースト周波数 3.579545 MHz を 3 分周した
//! ものをそのまま使ったことに由来する（部品を共用して安く作るため）。
//!
//! 割り込み周期は「入力クロック / 分周値」で決まり、**分周値は 16bit の
//! 整数**である。したがって任意の周波数を厳密には作れない。100Hz を
//! 要求しても実際は 1193182 / 11932 = 99.9985 Hz になる。
//!
//! **「ティック数 × 10ms」を時刻として扱ってはいけない。** 1 時間で
//! 5 秒以上ずれる計算になる。時刻が必要になったら RTC など別の源を使うか、
//! 誤差を明示的に補正すること。分周値と実周波数の計算は純粋関数として
//! 切り出し、丸め誤差込みでホストテストに固定してある。

use common::critical::InterruptGuard;
use common::port::{io_wait, outb};

/// PIT の入力クロック（Hz）。
///
/// 1.193182 MHz。NTSC カラーバースト 3.579545 MHz の 1/3。
pub const INPUT_CLOCK_HZ: u32 = 1_193_182;

/// チャネル 0（IRQ0 に繋がっている）のデータポート。
const CHANNEL0_DATA_PORT: u16 = 0x40;
/// モード/コマンドレジスタ。
const COMMAND_PORT: u16 = 0x43;

/// コマンド: チャネル 0、アクセスモード lobyte/hibyte、モード 3、2 進カウント。
///
/// - bit7-6 = 00: チャネル 0
/// - bit5-4 = 11: 分周値を下位バイト → 上位バイトの順に 2 回書く
/// - bit3-1 = 011: モード 3（矩形波生成）
/// - bit0   = 0: 2 進カウント（BCD ではない）
///
/// モード 3 を選ぶ理由: 出力が半周期ごとに反転する矩形波で、IRQ0 の周期
/// 割り込みとして最も一般的な構成である。モード 2（レートジェネレータ）でも
/// 周期割り込みは得られるが、モード 3 が事実上の標準で、実機・エミュレータ
/// とも枯れている。モード 0（割り込みオンターミナルカウント）は 1 回しか
/// 発火しないため周期タイマには使えない。
const COMMAND_CHANNEL0_SQUARE_WAVE: u8 = 0b0011_0110;

/// タイマ割り込みの目標周波数（Hz）。
///
/// 100Hz を選んだ理由:
///
/// 1. 10ms は人間が「動いている」と認識できる粒度で、ハートビートの
///    間隔として扱いやすい。
/// 2. TCG で `-d int` を有効にしたときのログ量が現実的な範囲に収まる
///    （1 ティックあたり 20 行強なので毎秒 2,200 行程度）。
/// 3. M5 のプリエンプションのタイムスライスとしても標準的な値。
pub const TARGET_FREQUENCY_HZ: u32 = 100;

/// 分周値として使えない値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrequencyError {
    /// 分周値が 16bit に収まらない（周波数が低すぎる）。
    ///
    /// 分周値 0 は 65536 として扱われるため、実質の下限は約 18.2Hz。
    TooLow,
    /// 分周値が 0 になる（周波数が入力クロックより高い）。
    TooHigh,
}

/// 目標周波数に対する分周値を求める（純粋ロジック）。
///
/// 四捨五入する。切り捨てだと常に目標より速い側へ偏るため。
pub const fn divisor_for(frequency_hz: u32) -> Result<u16, FrequencyError> {
    if frequency_hz == 0 {
        return Err(FrequencyError::TooLow);
    }
    if frequency_hz > INPUT_CLOCK_HZ {
        return Err(FrequencyError::TooHigh);
    }
    // 四捨五入: (a + b/2) / b
    let divisor = (INPUT_CLOCK_HZ + frequency_hz / 2) / frequency_hz;
    if divisor == 0 {
        return Err(FrequencyError::TooHigh);
    }
    if divisor > u16::MAX as u32 {
        return Err(FrequencyError::TooLow);
    }
    Ok(divisor as u16)
}

/// 分周値から実際の割り込み周波数を求める（純粋ロジック、ミリヘルツ単位）。
///
/// 整数演算のまま誤差を見たいので、Hz ではなく mHz（1/1000 Hz）で返す。
pub const fn actual_frequency_millihertz(divisor: u16) -> u64 {
    if divisor == 0 {
        // PIT では分周値 0 は 65536 を意味する。
        return (INPUT_CLOCK_HZ as u64 * 1000) / 65536;
    }
    (INPUT_CLOCK_HZ as u64 * 1000) / divisor as u64
}

/// チャネル 0 を周期割り込みモードで設定する。
///
/// **この時点では IRQ0 はマスクされたままであること。** 設定と解禁を分けて
/// おかないと、分周値の書き込み途中で割り込みが飛び込む余地ができる。
/// 解禁は呼び出し側が `pic::unmask_irq` で明示的に行う。
///
/// # Safety
///
/// - 起動時に 1 回だけ呼ぶこと。
/// - 呼び出し時点で IRQ0 がマスクされていること。
pub unsafe fn configure_channel0(frequency_hz: u32) -> Result<u16, FrequencyError> {
    let divisor = divisor_for(frequency_hz)?;

    // **分周値は 2 回に分けて書く。この 2 回の間に割り込みが入ってはならない。**
    // 途中で別のコードが同じポートを触ると、下位バイトだけ新しい値・上位
    // バイトは古い値という壊れた分周値になる。現状は呼び出し時点でまだ
    // `sti` していないので実害は無いが、構造として正しくしておく。
    let _critical = InterruptGuard::enter();

    // SAFETY: 0x43 / 0x40 は PIT の既知のポート。コマンドを書いてから
    // 規定どおり lobyte → hibyte の順に分周値を書く。呼び出し側の契約に
    // より IRQ0 はマスクされており、この区間は割り込み禁止。
    unsafe {
        outb(COMMAND_PORT, COMMAND_CHANNEL0_SQUARE_WAVE);
        io_wait();
        outb(CHANNEL0_DATA_PORT, (divisor & 0xFF) as u8);
        io_wait();
        outb(CHANNEL0_DATA_PORT, (divisor >> 8) as u8);
        io_wait();
    }

    Ok(divisor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_divisor_for_the_target_frequency_is_known() {
        // 1193182 / 100 = 11931.82 → 四捨五入して 11932。
        assert_eq!(divisor_for(TARGET_FREQUENCY_HZ), Ok(11932));
    }

    /// **要求した周波数と実際の周波数は一致しない。**
    ///
    /// 分周値が整数である以上避けられない。ここで固定しておくのは、
    /// 「ティック数 × 10ms を時刻として使ってはいけない」根拠を、
    /// 文章だけでなく数値として残すため。
    #[test]
    fn the_actual_frequency_differs_from_the_request() {
        let divisor = divisor_for(100).unwrap();
        let actual = actual_frequency_millihertz(divisor);
        // 99.9985 Hz。要求の 100.000 Hz より僅かに遅い。
        assert_eq!(actual, 99_998);
        assert_ne!(actual, 100_000, "厳密に一致することはない");

        // 1 時間（360,000 ティック想定）でどれだけずれるか。
        // 99.998 Hz なので、360,000 ティックに掛かる実時間は
        // 360000 / 99.998 秒 = 3600.07 秒。約 0.07 秒/時 の遅れ。
        let millihertz_error = 100_000 - actual;
        assert_eq!(millihertz_error, 2, "誤差は 0.002 Hz");
    }

    #[test]
    fn rounding_goes_to_the_nearest_not_down() {
        // 1193182 / 3 = 397727.33 → 収まらない。
        // 分かりやすい例として 1193182 / 1000 = 1193.182 → 1193。
        assert_eq!(divisor_for(1000), Ok(1193));
        // 1193182 / 7 = 170454.57 → 16bit を超える。
        assert_eq!(divisor_for(7), Err(FrequencyError::TooLow));
    }

    #[test]
    fn frequencies_that_do_not_fit_in_sixteen_bits_are_rejected() {
        // 分周値の上限 65535 に対応する下限周波数は約 18.2Hz。
        assert_eq!(divisor_for(19), Ok(62799));
        assert_eq!(divisor_for(18), Err(FrequencyError::TooLow));
        assert_eq!(divisor_for(0), Err(FrequencyError::TooLow));
    }

    #[test]
    fn frequencies_above_the_input_clock_are_rejected() {
        assert_eq!(divisor_for(INPUT_CLOCK_HZ + 1), Err(FrequencyError::TooHigh));
        // 入力クロックそのものなら分周値 1。
        assert_eq!(divisor_for(INPUT_CLOCK_HZ), Ok(1));
    }
}
