//! Local APIC のタイマへ移した結果の読み戻しと確かめ（S2-d-2）。
//!
//! **`kernel/src/interrupts.rs` から移した**（2026-09-28。境界の段階の手順 2 の 9c。`ADR-0072` の 7）。**切り替えの本体は
//! [`crate::machine::pc::irq::switch_timer_to_lapic`] が持ち、ここは前後の読み戻しと確かめに徹する。**

use common::arch::x86_64::cpu;
use common::log::Logger;
use common::machine::pc::serial::SerialPort;

use crate::arch::x86_64::idt;

/// タイマを Local APIC タイマへ移し、結果を観測する（S2-d-2）。
///
/// 順序と割り込み禁止区間の扱いは [`crate::machine::pc::irq::switch_timer_to_lapic`] が
/// 持つ。ここはその前後の観測に徹する。
///
/// # 契約（境界の関数。2026-09-28）
///
/// - 呼ぶのは BSP で、起動の途中、`sti` と Local APIC のタイマの較正の後に 1 回だけである（タイマはまだ 8259 と PIT で
///   動いている）。`calibration` は、その較正の戻り値である。
/// - 戻った時点で、タイマは Local APIC のタイマ（[`idt::LAPIC_TIMER_VECTOR`]）で届き、8259 は全部禁止され、LVT が
///   書いたとおりで、実効の周波数が許容幅に入っていることを確かめてある。どれかが外れたら、理由を出して止まる（戻らない）。
/// - 設定するのはこの CPU（BSP）の Local APIC だけである。AP は自分のタイマを自分で開ける
///   （[`crate::machine::pc::enable_local_timer_for_this_cpu`]）。
///
/// # Safety
///
/// - BSP が、起動の途中に 1 回だけ呼ぶこと。
/// - IDT の [`idt::LAPIC_TIMER_VECTOR`] に、戻れるハンドラがあること。
pub unsafe fn switch_to_local_timer(
    logger: &mut Logger<SerialPort>,
    calibration: crate::machine::pc::apic::TimerCalibration,
) {
    let requested_hz = crate::machine::pc::irq::timer_frequency_hz();
    let median_hz = calibration.median_hz();
    let divide = calibration.divide_configuration();

    // SAFETY: 切り替えの本体が求める 2 つ（起動の途中に 1 回だけ呼ぶこと、LAPIC_TIMER_VECTOR に戻れるハンドラが
    // あること）は、この関数の `# Safety` として呼ぶ側に求めている。
    let setup = match unsafe {
        crate::machine::pc::irq::switch_timer_to_lapic(calibration, requested_hz)
    } {
        Ok(setup) => setup,
        Err(error) => {
            logger.error(format_args!(
                "lapic-timer: could not switch the timer to the local APIC ({error:?}); halting"
            ));
            cpu::halt_forever();
        }
    };

    logger.info(format_args!(
        "lapic-timer: programmed from the calibration ({median_hz} Hz at divide \
         configuration {divide:#x}): {setup}"
    ));

    // 設定の読み戻し。8259 の ICW2 と違い、LVT Timer は書いた値を
    // 読み戻せる。ただしこれは `sti` 前の項目 4 を格上げしない。
    // あちらは `sti` の手前の検査点の話で、その時点ではタイマはまだ 8259
    // 経由である（切り替えは較正の後、較正は `sti` の後）。ここで示せるのは
    // 「LVT については、設定した内容がどこかで検証されている」ことだけである。
    match crate::machine::pc::irq::lvt_timer_readback() {
        Some(lvt) => {
            let expected_vector = idt::LAPIC_TIMER_VECTOR as u8;
            let ok = lvt.vector() == expected_vector && !lvt.masked() && lvt.periodic();
            logger.info(format_args!(
                "lapic-timer: LVT timer read back: {lvt}, matches what we programmed \
                 (vector {expected_vector:#04x}, unmasked, periodic) = {ok}"
            ));
            if !ok {
                logger.error(format_args!(
                    "lapic-timer: the LVT timer does not carry what we programmed; halting"
                ));
                cpu::halt_forever();
            }
        }
        None => {
            logger.error(format_args!(
                "lapic-timer: the LVT timer could not be read back; halting"
            ));
            cpu::halt_forever();
        }
    }

    // PIC が黙っていることを読み戻す。`irq::mask_all()` が実際に
    // 呼ばれたことの観測でもある（S2-b で足してから呼び出し元が無かった）。
    let masks = crate::machine::pc::irq::check_masks(&[]);
    logger.info(format_args!(
        "lapic-timer: the 8259 is fully masked after mask_all(): {masks}, all masked={}",
        masks.matches()
    ));
    if !masks.matches() {
        logger.error(format_args!(
            "lapic-timer: the 8259 is not fully masked, so IRQ0 could still be delivered; halting"
        ));
        cpu::halt_forever();
    }

    // 許容幅の判定。ただし何を示しているかに注意が要る。
    //
    // ここが比べているのは「要求周波数」と「較正値と初期カウントから導いた
    // 実効周波数」で、どちらもカーネルの内側の値である。整数の割り算で
    // 生じるずれは検出されるが、較正値そのものが間違っている場合は検出されない。
    // 較正値が 2 倍になれば初期カウントも 2 倍になり、比は 100Hz のまま
    // 一致する（自己無矛盾）。
    //
    // 較正値が現実と合っているかは、独立の時間基準でしか測れない。
    // PIT は今マスクしたので、カーネル内にはもう基準が無い。ホスト側の
    // 実時間と突き合わせる検査を xtask に用意してある（`lapic-timer-test`）。
    let requested_millihertz = u64::from(requested_hz) * 1000;
    let actual = setup.actual_millihertz();
    let deviation = actual.abs_diff(requested_millihertz);
    let within_tolerance =
        deviation * TIMER_TOLERANCE_DENOMINATOR <= requested_millihertz * TIMER_TOLERANCE_NUMERATOR;
    logger.info(format_args!(
        "lapic-timer: effective {}.{:03} Hz against a requested {requested_hz} Hz, deviation \
         {deviation} mHz, within {TIMER_TOLERANCE_NUMERATOR}/{TIMER_TOLERANCE_DENOMINATOR} = \
         {within_tolerance} (this compares two kernel-side values; it cannot show that the \
         calibration itself matches real time)",
        actual / 1000,
        actual % 1000
    ));
    if !within_tolerance {
        logger.error(format_args!(
            "lapic-timer: the effective frequency is outside the tolerance; halting"
        ));
        cpu::halt_forever();
    }

    logger.info(format_args!(
        "lapic-timer: the timer now arrives as vector {:#04x}, which the 8259 cannot produce",
        idt::timer_delivery_vector()
    ));
}

/// タイマの実効周波数の許容幅（±0.5%）。分子と分母で持つのは浮動小数点を
/// 使わないためである。
///
/// # 導出（`verification-coverage.md` の S2-c）
///
/// 較正自身の誤差の上限 320 ppm と、起動をまたいだ中央値のばらつき 865 ppm
/// （実測）を足して約 1,200 ppm。その約 4 倍が 5,000 ppm = 0.5% である。
const TIMER_TOLERANCE_NUMERATOR: u64 = 5;
const TIMER_TOLERANCE_DENOMINATOR: u64 = 1000;
