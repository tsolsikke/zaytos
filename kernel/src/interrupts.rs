//! メインループ（M4-d-1）。
//!
//! **`sti` の前の 7 項目の確かめ（ADR-0018 §2）は、[`crate::arch::x86_64::interrupt_readiness`] へ移した**
//! （2026-09-27。境界の段階の手順 2）。**全 IRQ をマスクしたまま `sti` して期限つきで待つ確かめ（M4-d-1）も、
//! 同じ所へ移した。**

use core::sync::atomic::{AtomicU64, Ordering};

use common::arch::x86_64::cpu;
use common::log::Logger;
use common::machine::pc::serial::SerialPort;

use crate::arch::x86_64::idt;

/// 探りの各段で待つスピン上限（S5-c）。上限の無い待ちを書かない。
#[cfg(feature = "smp-tlb-shootdown-probe")]
const SHOOTDOWN_PROBE_WAIT_SPINS: u32 = 3_000_000;

/// 測定用 IPI を何本送るか（S5-a）。会計を主張できる程度の本数にする。
#[cfg(feature = "smp-ipi-probe")]
const IPI_PROBE_ROUNDS: u32 = 4;

/// 1 本ぶんの受け取りを待つスピン上限（S5-a）。上限の無い待ちを書かない。
#[cfg(feature = "smp-ipi-probe")]
const IPI_PROBE_WAIT_SPINS: u32 = 10_000_000;

/// ハートビートを出す間隔（ティック数）。
///
/// 100Hz なので 100 ティック = 約 1 秒。画面で目視して変化が分かる
/// 間隔にしてある。これより短いと画面のスクロールが速すぎて読めず、
/// 長いと「動いているのか止まっているのか」の判断が遅れる。
pub const HEARTBEAT_TICKS: u64 = 100;

/// 定常ループの観測を完了したか（S11-11）。**両方のコアが見る。**
///
/// # なぜ AP も見るのか
///
/// **BSP がシェルへ渡しても、AP は自分のループを回し続ける。**
/// **AP のハートビートが出続けると、起動ログの長さが実時間に依存する**
/// ——**捕捉を打ち切った時点で何本出ているかが決まらない。**
/// **`-smp 1` と `-smp 2` の突き合わせが、その差で落ちた**（実測）。
///
/// **観測の終わりは系全体の性質である。** 片方のコアだけ完了しても、
/// **ログとしては完了していない。**
static STEADY_OBSERVATION_CLOSED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 定常ループの観測を完了する（S11-11）。**BSP がシェルへ渡す直前に呼ぶ。**
pub fn close_steady_observation() {
    STEADY_OBSERVATION_CLOSED.store(true, core::sync::atomic::Ordering::SeqCst);
}

/// 定常ループの観測が完了しているか。**AP のハートビートが見る。**
pub fn steady_observation_is_closed() -> bool {
    STEADY_OBSERVATION_CLOSED.load(core::sync::atomic::Ordering::SeqCst)
}

/// 最初のティックを待つ上限（TSC サイクル）。
///
/// これを過ぎても 1 件も来ないなら、タイマが設定できていないか、IMR が
/// 効いていないか、ICW2 が誤っているかのいずれかである。無言で待ち続けると
/// ハングと区別がつかないので fail-fast する。
const FIRST_TICK_TIMEOUT_CYCLES: u64 = 20_000_000_000;

/// メインループが 1 回起きるあいだに進んだティック数の最大値。
///
/// これはハードウェアのティック取りこぼしではなく、メインループが
/// 「観測し損ねた」量である。1 なら毎ティック起きて観測できている。
/// 2 以上なら、起きて処理しているあいだに次のティックが来ている。
///
/// 本当の意味でのティック取りこぼし（PIT が発火したのに CPU へ届かない、
/// あるいは EOI が間に合わず次が抑止される）は、独立した第 2 の時間源が
/// 無いと検出できない。現状 TSC しか無く、その TSC も仮想化環境では
/// 信用できない（`common::arch::x86_64::cpu` 参照）。ここで測れるのは
/// 「メインループの追従の遅れ」までである。
static MAX_TICK_JUMP: AtomicU64 = AtomicU64::new(0);

pub fn max_tick_jump() -> u64 {
    MAX_TICK_JUMP.load(Ordering::Relaxed)
}

/// タイマを Local APIC タイマへ移し、結果を観測する（S2-d-2）。
///
/// 順序と割り込み禁止区間の扱いは [`crate::machine::pc::irq::switch_timer_to_lapic`] が
/// 持つ。ここはその前後の観測に徹する。
fn switch_timer_to_lapic(
    logger: &mut Logger<SerialPort>,
    calibration: crate::machine::pc::apic::TimerCalibration,
) {
    let requested_hz = crate::machine::pc::irq::timer_frequency_hz();
    let median_hz = calibration.median_hz();
    let divide = calibration.divide_configuration();

    // SAFETY: ベクタ LAPIC_TIMER_VECTOR には専用スタブのゲートが入っており
    // （`idt::init`）、ハンドラは EOI を Local APIC へ送って戻る。
    // 起動時の 1 回だけの呼び出しである。
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

/// タイマ割り込みで駆動されるメインループ。
///
/// # `hlt` を無条件に使う理由
///
/// ADR-0018 のチェックリスト 10 は「`cli` → 条件確認 → `sti; hlt`」の並びを
/// 求めている。あれが必要なのは「仕事が無ければ眠る」形のループである。
/// 仕事の有無を確認してから眠るまでの隙間に仕事が発生すると、次の割り込みまで
/// 眠り続けてしまう。
///
/// このループは眠るかどうかを条件で決めない。タイマが 100Hz で必ず起こして
/// くれるので、無条件に `hlt` → 起きたらティックを見る → また `hlt` で足りる。
/// 最悪でも 10ms 後には起きるため、取りこぼしという概念が成立しない。
/// 条件つきの形が要るのは M5 の実行キュー（仕事の有無で眠りを決める）である。
///
/// 限界: この形は、起こしてくれるものが止まった瞬間に永久ハングになる。
/// `hlt` で眠っている以上、カーネル自身はそれを検出できない（検出のための
/// コードが動かない）。外側からは検出できるので、xtask がシリアルログの
/// ハートビート回数で判定する。カーネル内部では検出できないが、テスト基盤
/// では検出できる、という切り分けである。
///
/// # Safety
///
/// 割り込みを有効化する。[`verify_ready_for_sti`](crate::arch::x86_64::interrupt_readiness::verify_ready_for_sti) を通し、タイマの設定と
/// IRQ0 の解禁が済んでいること。
pub unsafe fn run_timer_loop(
    logger: &mut Logger<SerialPort>,
    console: Option<&mut crate::console::Console>,
    stop_after_ticks: u64,
    shell_after_heartbeats: u64,
    apic: Option<&crate::machine::pc::apic::MappedApic>,
    virtio: Option<&mut crate::virtio::VirtioBlk>,
    pm_timer: Option<crate::pmtimer::PmTimer>,
) {
    // 最初のティックが来るまで何も出ないとハングと区別できないので、
    // 待ちに入ることを先に宣言する。
    logger.info(format_args!(
        "timer: waiting for the first tick (expected as vector {:#04x}); \
         if nothing arrives, suspect the PIT setup, the IMR, or ICW2",
        idt::timer_delivery_vector()
    ));

    let mut console = console;
    let started = cpu::read_timestamp_counter();

    // SAFETY: 呼び出し側の契約により、7 項目の検証とタイマ設定が済んでいる。
    //
    // `sti` を実行するのはここと [`spin_with_interrupts_enabled`] の 2 箇所
    // だけである（数と経緯は spin 側のコメントと ADR-0018 Addendum 5 を参照）。
    // こちらが「`hlt` で待つ本来の形」で、起こしてくれるタイマが実在する
    // M4-d-2 で初めて成立した（ADR-0018 Addendum 2 §3）。
    unsafe {
        cpu::enable_interrupts();
    }

    // **PIT が 1 本も刻まなかったか**（HW-c。`ADR-0068`）。**較正の基準で決まる。**
    // **ICW2 の事後証明が取れるかが、これで変わる。**
    let mut pit_never_ticked = false;

    // === S2-c: Local APIC タイマの較正 ===
    //
    // この位置でなければならない。基準に使う `TIMER_TICKS` は IRQ0 が
    // 増やすので、`sti` より前では進まない。`start_timer` は `-> !` で戻らず、
    // 通常起動では `run_timer_loop` も戻らないので、「割り込みが有効で、かつ
    // 定常ループへ入る前」という区間はここにしか存在しない。APIC 関連の他の
    // 処理（`kmain` の前半）から離れているのはこのためである。まとめないこと。
    //
    // 較正は測るだけで、LAPIC タイマをタイマとして使わない。LVT Timer は
    // マスクされたままで、LINT0 と SVR にも触らない。
    if let Some(apic) = apic {
        // 破壊テスト (HW-c, pm-timer-treated-as-absent): PM タイマを無いものとして渡す。
        // **PIT が刻まない構成（`pit=off`）で、両方無い道を通す**——**較正の基準が 1 つも
        // 無いことを示して止まる行が出る。** **直す前は黙って止まっていた。**
        let pm_timer = if cfg!(feature = "pm-timer-treated-as-absent") {
            None
        } else {
            pm_timer
        };
        let calibration = crate::machine::pc::apic::calibrate_timer(logger, apic, pm_timer);

        // === S2-d-1b: 2 つ目のコントローラ実装を 1 回だけ読ませる ===
        //
        // 切り替えない。読むだけである。配送は PIC / PIT のままで、
        // 書き込みは一切しない。
        //
        // 呼ぶ理由は、2 つ目の実装が実ハードウェアを正しく読めることを、
        // 振る舞いが変わらないうちに確かめておくためである。どこからも
        // 呼ばずに S2-d-1c（配送が変わる段階）へ入ると、そこで落ちたときに
        // 「切り替えが悪いのか、実装が悪いのか」を切り分けられない。
        //
        // 期待は「I/O APIC 経由へ移した IRQ だけが開いている」である。
        // PIC のマスク状態をここへ持ち込まないこと。別のコントローラの
        // 状態である。S2-d-1c で IRQ1 を移したので、開いているのはそれだけに
        // なる。期待は呼び出し側が持つ（境界が独自に期待を持たない）。
        let routed: &[u8] =
            if crate::machine::pc::irq::routed_to_apic(crate::keyboard::KEYBOARD_IRQ) {
                &[crate::keyboard::KEYBOARD_IRQ]
            } else {
                &[]
            };
        match crate::machine::pc::irq::survey_apic_masks(apic, routed) {
            Some(check) => logger.info(format_args!(
                "apic: the I/O APIC controller reads its redirection entries: {check}, \
                 only the routed IRQs are open={} (the PIC still owns every other line)",
                check.matches()
            )),
            None => logger.warn(format_args!(
                "apic: no I/O APIC was mapped, so the second controller implementation \
                 could not be exercised"
            )),
        }

        // === S2-d-2: タイマを Local APIC タイマへ移す ===
        //
        // 較正より後でなければならない。初期カウントを較正の戻り値から
        // 求めるためである。そして切り替えの区間ではティックが 1 本も
        // 来ないので、`TIMER_TICKS` を待つ処理（較正のエッジ待ち）は
        // ここより前に済んでいる必要がある。
        // **PIT が刻まなかった回は、ICW2 の事後証明が取れない**（HW-c。`ADR-0068`）——
        // **PIC の割り込みが 1 本も届かないので、`first_pic_vector` は `None` のままである。**
        // **較正の基準を見て決める**（`None` を一色に扱うと、ICW2 の誤りと PIT の不在が混ざる）。
        pit_never_ticked = matches!(
            calibration.as_ref().map(|value| value.reference()),
            Some(crate::machine::pc::apic::CalibrationReference::PmTimer)
        );
        if let Some(calibration) = calibration {
            switch_timer_to_lapic(logger, calibration);
        } else {
            logger.warn(format_args!(
                "apic: the local APIC timer was not calibrated, so the timer stays on the PIT; \
                 the 8259 keeps delivering IRQ0"
            ));
        }
    }

    // プリエンプティブマルチタスクのデモと検証（M5-d）。timer が動き出した
    // この時点で 1 区間だけ実行する。通常起動（stop_after_ticks == 0）でのみ行う。
    // interrupt-test の有限ループ（stop_after_ticks > 0）では実行しない。デモが
    // 終わるとワーカーは走行不可になり、以降このハートビートループは
    // プリエンプトされない（runnable がメインだけなので on_timer_tick は
    // no-op）。
    if stop_after_ticks == 0 {
        crate::task::run_preemptive_demo();

        // === S3-b-2b-1: AP を起動する ===
        //
        // 位置は 2 つの制約で決まっている。`sti` より後でなければならない
        // （10ms の待ちをタイマのティックで作る）。そしてプリエンプティブ
        // デモの後でなければならない（デモの最中に AP が起きると、周回数や
        // 窓カウントの観測に混ざる）。
        //
        // ここに `cli` / `sti` は追加していない。許可リストの数は変わらない。
        if let Some(apic) = apic {
            let mmio = apic.mmio();
            // SAFETY: `apic` はマップ済み、タイマは動いている（直前まで
            // デモが走った）、起動時の 1 回だけである。
            let report = unsafe { crate::smp::wake_application_processors(logger, apic, &mmio) };
            logger.info(format_args!(
                "smp: application processors: {} usable CPU(s) reported, {} AP(s) attempted, \
                 {} started, {} skipped for lack of a per-CPU slot (MAX_CPUS={})",
                report.usable,
                report.attempted,
                report.started,
                report.skipped_no_slot,
                common::percpu::MAX_CPUS
            ));
            if report.started != report.attempted {
                logger.error(format_args!(
                    "smp: only {} of {} application processor(s) reported their start signature",
                    report.started, report.attempted
                ));
            }
            // **起動した AP の CR0・CR4・EFER が BSP と一致すること**（2026-09-24。`ADR-0018` の
            // Addendum 9 の監視。**棚卸しの結論は全 CPU についてである**）。
            crate::arch::x86_64::cpu_state::check_aps_match_bsp(logger, report.started);

            // **シリアルの排他の演習（BSP 側）。** **合図を立ててから、AP と
            // 同時に既知の行を書く。** **`kernel/src/smp.rs` の
            // `run_serial_stress_on_ap` が相手である。**
            #[cfg(feature = "serial-stress-test")]
            run_serial_stress_on_bsp(logger);
        }

        // === S13-d: 割り込みが実際に届くことの実演 ===
        //
        // **`sti` の後・AP 起床の後である。** 主張は「届いて数えられる」だけで、
        // 完了の待ちはポーリングのまま（眠りは d-2。ADR-0036）。届かなければ
        // 上限で止まる。**エッジのまま書く破壊テストはここでは落ちない**——実測で
        // QEMU は極性とトリガを厳密に模らず、届いてしまう。あちらを検出する
        // のは配線の読み戻し（宣言との一致）である。
        // **武装済みのときだけ実演する。** I/O APIC が無い構成（ACPI の破壊テストの
        // 一群）では配線されておらず、待っても届かない——あの構成の主張は
        // 「ACPI が読めなくても起動は続く」なので、ここで止めてはならない。
        // 閉じたままであることは `start_timer` が判定行に出している。
        if crate::virtio::armed_irq().is_some() {
            if let Some(virtio) = virtio {
                // SAFETY: 配線と武装は `start_timer` が `sti` より前に済ませ、
                // いま IF=1 である。リングとポートウィンドウはこの struct だけが触り、
                // ISR ポートだけはハンドラと共有する（意図した相互作用）。
                if let Err(reason) =
                    unsafe { crate::virtio::exercise_interrupt_read(logger, virtio) }
                {
                    logger.error(format_args!(
                        "virtio-blk: the interrupt exercise failed ({reason:?}); halting"
                    ));
                    common::arch::x86_64::cpu::halt_forever();
                }
                // d-2: BKL を解いて眠り、割り込みで起きる（ADR-0036）。
                // SAFETY: 上と同じ位置（配線・武装済み、IF=1）。
                if let Err(reason) =
                    unsafe { crate::virtio::exercise_blocking_read(logger, virtio) }
                {
                    logger.error(format_args!(
                        "virtio-blk: the blocking read failed ({reason:?}); halting"
                    ));
                    common::arch::x86_64::cpu::halt_forever();
                }
            }
        }

        // 増幅器 (S4-c-4-3, sched-keep-workers-runnable): AP が起動した後で
        // デモのワーカーを走行可能へ戻す。単独では何も主張しない——
        // bootstrap processor が巡回を続けるだけで、第 1 層が AP を弾く。
        //
        // 位置はここでなければならない。デモより前だとデモの観測に混ざり、
        // 締切分岐を止める形にすると `run_preemptive_demo` が戻らずAP の起動へ
        // 到達しない（`task::rearm_workers_for_smp_stimulus` の doc）。
        #[cfg(feature = "sched-keep-workers-runnable")]
        crate::task::rearm_workers_for_smp_stimulus();

        // === S5-c: TLB シュートダウンの実証（4 段の手順）===
        //
        // 手順の理由は `smp::shootdown_probe` の doc にある。
        // 「AP が触って #PF」だけでは差が出ない——TLB に翻訳が載っていなければ、
        // 世代を上げない構成でもページテーブルを歩いて #PF になる。
        #[cfg(feature = "smp-tlb-shootdown-probe")]
        {
            use crate::smp::shootdown_probe;

            // (1)(2) AP に触らせ、触れたことを確かめる。
            shootdown_probe::command(shootdown_probe::TOUCH_FIRST);
            let mut spun = 0u32;
            while shootdown_probe::touches() == 0 && spun < SHOOTDOWN_PROBE_WAIT_SPINS {
                core::hint::spin_loop();
                spun += 1;
            }
            let first = shootdown_probe::touches();
            logger.info(format_args!(
                "smp: shootdown probe step 1-2: the ap touched the probe page {first} time(s) \
                 (this is the positive evidence that the translation is in its TLB; without it \
                 the comparison below is meaningless)"
            ));
            if first == 0 {
                logger.error(format_args!(
                    "smp: the ap never touched the probe page; the shootdown comparison is void"
                ));
            } else {
                // (3) BKL を保持したままマッピングを外し、世代を上げる。
                let flushes_before = crate::bkl::generation_flushes_for(1);
                {
                    let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);
                    // SAFETY: 稼働中のテーブルから、探り用にマップした 1 ページを外す。
                    let mut table = unsafe {
                        crate::arch::x86_64::paging::active::ActivePageTable::current(
                            common::addr::direct_map(),
                        )
                    };
                    if let Some(virt) = common::addr::VirtAddr::new(shootdown_probe::virt()) {
                        // SAFETY: 探り用にマップしたページで、他の誰も使っていない。
                        let _ = unsafe { table.unmap_4kib(virt) };
                    }
                    // 破壊テスト (S5-c, smp-tlb-no-generation-bump): 世代を上げない。
                    // AP はフラッシュしないので、古い翻訳で成功する。
                    #[cfg(not(feature = "smp-tlb-no-generation-bump"))]
                    crate::bkl::note_mapping_changed();
                }
                // 世代方式が土台である。上げた場合、AP は次の取得でフラッシュする。
                // その完了を待ってから触らせるので、勝負が時間に依らない。
                let mut spun = 0u32;
                while crate::bkl::generation_flushes_for(1) == flushes_before
                    && spun < SHOOTDOWN_PROBE_WAIT_SPINS
                {
                    core::hint::spin_loop();
                    spun += 1;
                }
                // 主張を数字ではなく文で出す（S8-d で作り直した）。判定側は
                // 「the ap flushed」/「the ap did not flush」を見る。数字は観測用。
                let flushes_after = crate::bkl::generation_flushes_for(1);
                logger.info(format_args!(
                    "smp: shootdown probe step 3: unmapped the probe page; {} (flushes {} -> {})",
                    if flushes_after > flushes_before {
                        "the ap flushed"
                    } else {
                        "the ap did not flush"
                    },
                    flushes_before,
                    flushes_after
                ));

                // (4) もう一度触らせる。
                //
                // **主張が非対称である（S8-d で作り直した）。** フラッシュした側は
                // 2 回目の触りが必ず #PF になる——翻訳が無いので歩き、マッピングが無いので
                // 落ちる。**これはフラッシュの帰結として保証される。** 一方
                // **フラッシュしなかった側の結果は主張しない**——古い翻訳が TLB に
                // 残り続けることは、アーキテクチャが**許しているだけで約束していない**
                // （実 CPU でも QEMU でも、容量の都合でいつでも捨てられてよい）。
                // かつては「古い翻訳で成功する」を期待に置いていて、TCG の TLB の
                // 追い出しがレイアウト依存で発火し、決定的に落ちた（S8-d）。
                shootdown_probe::command(shootdown_probe::TOUCH_AGAIN);
                let mut spun = 0u32;
                let attempts_before = shootdown_probe::attempts();
                let touches_before = shootdown_probe::touches();
                while shootdown_probe::attempts() == attempts_before
                    && spun < SHOOTDOWN_PROBE_WAIT_SPINS
                {
                    core::hint::spin_loop();
                    spun += 1;
                }
                // 触りが #PF になる側では、AP がダンプをシリアルへ書いている最中で
                // ある。**触れたか、予算を使い切るまで待ってから書く**——成功する側は
                // touches の増分で早く抜け、落ちる側は予算ぶんの時間が AP のダンプの
                // 完了に充てられる。
                //
                // **理由が 2 つあったうちの 1 つは失効した（`ADR-0059`）。**
                // **「シリアルにはロックが無いので、すぐ書くと AP のダンプとバイト単位
                // で混ざる」を理由に挙げていたが、いまはロックが在る。** **待ちは残す**
                // ——**もう 1 つの理由（触りが届くのを待つ）はそのまま生きている。**
                // **外すなら、この判定の周りを触るときに測って決めること**
                // （`docs/deferred-decisions.md` の「混線を避けて選んだ形」）。
                let mut spun = 0u32;
                while shootdown_probe::touches() == touches_before
                    && spun < SHOOTDOWN_PROBE_WAIT_SPINS
                {
                    core::hint::spin_loop();
                    spun += 1;
                }
                logger.info(format_args!(
                    "smp: shootdown probe step 4: attempts={} touches {first} -> {} (the \
                     flushed side must fault on the second touch: no translation, no mapping. \
                     The unflushed side's outcome is not asserted: keeping a stale translation \
                     is permitted to a TLB, never promised. The attempt count is the evidence \
                     that the ap tried)",
                    shootdown_probe::attempts(),
                    shootdown_probe::touches()
                ));
            }
        }

        // === S5-b: 世代を 1 つ上げて、AP が次の取得でフラッシュすることを見る ===
        //
        // 本番にはマッピングを変える経路が無いので、そのままでは一度も発火しない。
        // 発火させて機序を見るためだけの feature である。
        //
        // BKL を保持したまま上げる——それが `note_mapping_changed` の契約で、
        // 順序（Acquire/Release の対）が成り立つ前提でもある。
        #[cfg(feature = "smp-tlb-generation-probe")]
        {
            // 上げる前の AP のフラッシュ回数を控える（S7-d で足した）。
            // 控えないと、後から「この bump のせいで増えた」が言えない。
            // ハートビートは bump より後にしか出ないので、前の値はここでしか取れない。
            // シュートダウンの探り（S5-c）は最初から同じ形で出している。対称にした。
            let flushes_before = crate::bkl::generation_flushes_for(1);
            {
                let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);
                crate::bkl::note_mapping_changed();
            }
            // AP が実際にフラッシュするまで待つ。上限つきである。
            // 待ちの上限。シュートダウンの探りの定数はその feature の下にしか
            // 無いので、ここに持つ（同じ桁）。上限の無い待ちを書かない。
            const GENERATION_PROBE_WAIT_SPINS: u32 = 3_000_000;
            let mut spun = 0u32;
            while crate::bkl::generation_flushes_for(1) == flushes_before
                && spun < GENERATION_PROBE_WAIT_SPINS
            {
                core::hint::spin_loop();
                spun += 1;
            }
            logger.info(format_args!(
                "smp: bumped the tlb generation to {} while holding the BKL; every core must \
                 flush at its next acquire (the entry takes the BKL on every tick, so this \
                 settles within one tick); ap flushes {} -> {}",
                crate::bkl::tlb_generation(),
                flushes_before,
                crate::bkl::generation_flushes_for(1)
            ));
        }

        // === S5-a: 測定用 IPI を送る（feature `smp-ipi-probe` のときだけ）===
        //
        // 既定ビルドでは送らない。送る側は上限つきとはいえスピンで待つので、
        // 他の検査の時間の形を変える。実際に踏んだ——既定ビルドへ入れたところ、
        // S4-c-4-3 の梯子（第 2 層の実証）が 5 回中 4 回しか通らなくなった。
        // 位置を刺激の後ろへ動かしても揺れは残った。測るためのものが別の検査の
        // 前提を壊すので、測るときだけ入れる形にする。
        //
        // 位置も刺激より後ろにしてある（前に置くと AP の起動から刺激までが延びる）。
        #[cfg(feature = "smp-ipi-probe")]
        {
            // === S5-a: 測定用 IPI を 1 本送る ===
            //
            // 目的は「TCG で IPI が届くか」を測ることだけである。
            // 宛先は起動した AP で、ハンドラは per-CPU カウンタと EOI だけを行う
            // （`idt::IPI_PROBE_VECTOR`）。BKL は要求しない。
            //
            // 送信完了（ICR の delivery status）と、相手が受け取ったこと（受信
            // カウンタ）は別の量である。前者は既存の AP の起動が見ているものと
            // 同じで、後者が測りたいものである。
            if let Some(apic) = apic {
                for slot in 1..common::percpu::MAX_CPUS {
                    let Some(apic_id) = crate::smp::started_ap_apic_id(slot) else {
                        continue;
                    };
                    // 1 本ずつ、受け取りを確かめてから次を送る。
                    //
                    // まとめて送ると数が合わない。同じベクタの IPI は Local APIC の
                    // IRR の 1 ビットなので、処理より速く送るとまとめられる。
                    // 「送った数と受け取った数が一致する」を主張したいなら、
                    // まとめられない送り方にする必要がある。
                    for _ in 0..IPI_PROBE_ROUNDS {
                        let before = idt::ipi_probe_received_for(slot);
                        // SAFETY: `apic` はマップ済みで、宛先は起動を確認した AP である。
                        let accepted = unsafe {
                            crate::machine::pc::apic::send_fixed_ipi(
                                crate::machine::pc::apic::lapic_virt_of(apic),
                                apic_id,
                                idt::IPI_PROBE_VECTOR as u8,
                            )
                        };
                        if !accepted {
                            logger.error(format_args!(
                                "smp: the ICR did not accept a probe IPI for apic id {apic_id}"
                            ));
                            break;
                        }
                        idt::record_ipi_probe_sent();
                        // 上限つきで待つ（上限の無い待ちを書かない）。
                        let mut spun = 0u32;
                        while idt::ipi_probe_received_for(slot) == before
                            && spun < IPI_PROBE_WAIT_SPINS
                        {
                            core::hint::spin_loop();
                            spun += 1;
                        }
                        if idt::ipi_probe_received_for(slot) == before {
                            logger.error(format_args!(
                                "smp: a probe IPI to apic id {apic_id} was accepted by the ICR but \
                                 the target did not handle it within the spin limit"
                            ));
                            break;
                        }
                    }
                    logger.info(format_args!(
                        "smp: probe IPI (vector {:#04x}) to apic id {apic_id}: sent={} received={} \
                         (one at a time; the same vector coalesces in the IRR if sent faster than \
                         it is handled, so they are not batched)",
                        idt::IPI_PROBE_VECTOR,
                        idt::ipi_probe_sent(),
                        idt::ipi_probe_received_for(slot)
                    ));
                }
            }
        }
    }

    let mut last_ticks = 0u64;
    let mut next_heartbeat = HEARTBEAT_TICKS;
    // **1 ティックあたりの TSC サイクル。** 前のハートビートからの差で出す。
    //
    // **判定行に出すのは、揺れる値だからである**（TCG と KVM で桁が違う）。
    // **docs へ書くと測った条件が変わったときに古くなる。**
    // **対比の相手は `console:` の行の所要である**——あちらは BKL を保持している
    // 区間へ入る量で、**「1 行を書くあいだにティックが何本入りうるか」がここで出る。**
    // **`console:` の行には出せない。** あれは `sti` より前に出るので、
    // その時点ではティックが進んでいない。
    // **両方を同じ時点で読む。** 片方を `0` で始めると、ループへ入る前に
    // 進んでいたぶん（較正が回した PIT のティック）が分母に入り、
    // 1 本目の値だけが桁で外れる（実測で 154,813 と 37,563,944）。
    //
    // **1 本目は `0` が出る。** ループへ入る時点で既に閾値を越えているので、
    // 最初のハートビートは基準と同じティックで出る（差が 0 なので
    // `checked_div` が `None` を返す）。**値が乗るのは 2 本目からである。**
    let mut last_heartbeat_tsc = cpu::read_timestamp_counter();
    let mut last_heartbeat_ticks = idt::timer_ticks();
    // 出したハートビートの本数（S11-11）。**シェルへ渡すタイミングを決める。**
    let mut heartbeats = 0u64;
    let mut announced_first = false;
    let mut announced_first_key = false;
    let mut decoder = crate::keyboard::decode::Decoder::new();
    let mut line = TypedLine::new();

    loop {
        let ticks = idt::timer_ticks();

        if ticks == 0 {
            if cpu::read_timestamp_counter() - started > FIRST_TICK_TIMEOUT_CYCLES {
                logger.error(format_args!(
                    "timer: no tick arrived before the deadline; halting. \
                     Check the PIT divisor write, the IMR (IRQ0 must be unmasked), \
                     and the PIC vector offset (ICW2)"
                ));
                cpu::halt_forever();
            }
            // まだ 1 件も来ていない。`hlt` すると、タイマが動いていない場合に
            // 永久に眠ってしまい上の期限判定へ戻れない。最初の 1 件だけは
            // スピンで待つ。
            core::hint::spin_loop();
            continue;
        }

        if !announced_first {
            announced_first = true;
            // ICW2 の事後証明。実際に届いたベクタ番号を実値で確認する。
            match idt::first_pic_vector() {
                // 8259 の採番と突き合わせる。現在の配送先ではない。
                //
                // `first_pic_vector` が記録するのは「PIC の採番範囲で最初に
                // 届いたベクタ」で、定義からして 8259 由来の観測である。
                // S2-d-2 でタイマが Local APIC へ移った後も、移行より前に
                // PIT が動いていた（較正が PIT のティックを使う）ので値は
                // 残っており、ICW2 の事後証明としては依然として有効である。
                //
                // ここを `timer_delivery_vector()` にすると、移行後に
                // `0x20` と `0xfe` を突き合わせて誤って落ちる。実際に落ちた。
                Some(vector) if vector as usize == idt::PIC_TIMER_VECTOR => {
                    log_both(
                        logger,
                        console.as_deref_mut(),
                        format_args!(
                            "timer: first tick arrived as vector {vector:#04x} - this is the \
                             proof that ICW2 was written correctly (it cannot be read back)"
                        ),
                    );
                }
                // **PIT が刻まない機械では、PIC の割り込みが 1 本も届かない**（HW-c）。
                // **ICW2 の事後証明は取れない。** **止めない**——**言えないことを言えないと
                // 書く**（`sti` 前の項目 4 と同じ立ち位置）。
                None if pit_never_ticked => {
                    logger.info(format_args!(
                        "timer: no PIC interrupt ever arrived because the PIT does not tick on \
                         this machine, so ICW2 has no after-the-fact proof; the timer runs on \
                         the local APIC (the calibration used the ACPI PM timer)"
                    ));
                }
                other => {
                    logger.error(format_args!(
                        "timer: the first PIC interrupt arrived as vector {other:?}, expected \
                         {:#04x}; the PIC vector offset (ICW2) is wrong; halting",
                        idt::PIC_TIMER_VECTOR
                    ));
                    cpu::halt_forever();
                }
            }
        }

        let jump = ticks - last_ticks;
        if jump > MAX_TICK_JUMP.load(Ordering::Relaxed) {
            MAX_TICK_JUMP.store(jump, Ordering::Relaxed);
        }
        last_ticks = ticks;

        // === S4-b-2: 共有物を触る区間だけ BKL を保持する ===
        //
        // この 1 周は入口ではない。定常ループはタスクである（ADR-0023
        // Addendum §2）。触る共有物は `SCANCODES`・コンソール・i8042 と PIC の
        // ポート・シリアルの 4 種類で、`hlt` はそのどれにも触らない。
        //
        // ガードのスコープに `hlt` を含めない。含めると、保持したまま眠って
        // もう一方のコアが IF=0 で永久に待つ。構造で起きないようにしてある。
        {
            let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);

            // 破壊テスト (S4-b-2, bkl-hold-with-if-set): 保持したまま IF=1 にする。
            // 次のティックで同じコアが irq_entry から取ろうとして再帰検出が発火する。
            #[cfg(feature = "bkl-hold-with-if-set-test")]
            // SAFETY: 破壊テストの feature 専用。BKL を保持している区間である。
            unsafe {
                crate::bkl::sabotage_enable_interrupts_while_held()
            };

            // キーボードのリングバッファを吸い出す。メインループが行う。
            // ハンドラは積むだけで表示しない（ADR-0018 §5）。
            drain_keyboard(
                logger,
                console.as_deref_mut(),
                &mut announced_first_key,
                &mut decoder,
                &mut line,
            );

            if ticks >= next_heartbeat {
                next_heartbeat = ticks + HEARTBEAT_TICKS;
                heartbeats += 1;
                let now_tsc = cpu::read_timestamp_counter();
                let elapsed_ticks = ticks.wrapping_sub(last_heartbeat_ticks);
                let tsc_per_tick = now_tsc
                    .wrapping_sub(last_heartbeat_tsc)
                    .checked_div(elapsed_ticks)
                    .unwrap_or(0);
                last_heartbeat_tsc = now_tsc;
                last_heartbeat_ticks = ticks;
                // **画面へは出さない。シリアルへだけ出す。**
                //
                // **同じ行に読み手が 2 つある**——**検査（`xtask`）はシリアルを読み、
                // 人は画面を見る。** ハートビートが主張するのはタイマ経路の健全性で、
                // **それを確かめるのは検査のほうである。** 画面の側にとっては、
                // **打鍵とシェルの応答の合間に割り込んでくる行でしかない。**
                //
                // **以前は「行が空のときだけ画面にも出す」形だった**——入力中に
                // 割り込むと入力行がぶつ切りになるためで、**その対策は
                // 「画面へ出さない」に含まれる。**
                //
                // **検査の主張は 1 つも変わらない。** `xtask` が見ているのは
                // シリアルのログで、`heartbeat: ticks=` を期待マーカー（9 項目）・
                // 禁止マーカー（4 項目）・本数（`min_heartbeats` と `--shell-test`）
                // として使っているが、いずれもシリアル側である。
                log_both(
                    logger,
                    None,
                    // `heartbeat: ticks=` を行頭に保つ。この部分文字列は xtask の
                    // 期待・禁止マーカーとして 30 箇所近くで使われており、`last_heartbeat_seconds`
                    // が `heartbeat: ticks=256 (2 s), ...` の形を解析している。
                    // S4-a で足す `cpu=` は、その後ろに置く。
                    // AP 側は別の行（`smp: ap heartbeat: cpu=`）なので、この数え上げに混ざらない。
                    format_args!(
                        "heartbeat: ticks={ticks} ({} s), tsc_per_tick={tsc_per_tick}, cpu={}, ap_ticks={}, ticks_total={}, \
                     lapic_timer_deliveries={}, timer_accounting_balanced={}, \
                     max kernel entry depth={}, ap_current={} ap_sched_passes={}, \
                     ipi_sent={} ipi_recv_cpu1={}, tlb_gen={} flush_cpu1={}, \
                     heap_free={} heap_blocks={}, \
                     keys={} dropped={} \
                     stray={} spurious={} lapic_spurious={}, \
                     irq1={} balanced={}, max tick jump={}, i8042 OBF={}, PIC ISR={}, \
                     uart forced={} reentry={}",
                        ticks / crate::machine::pc::irq::timer_frequency_hz() as u64,
                        common::percpu::cpu_id(),
                        ap_tick_summary(),
                        idt::timer_ticks_total(),
                        // 合計で閉じる相手である。1 本のティックはどこか 1 コアの
                        // スロットと、このベクタ別カウンタの両方を増やす。
                        idt::timer_delivery_count(),
                        idt::timer_accounting_balances(),
                        idt::max_kernel_entry_depth(),
                        // 「割り当てられた」と「参加した」は別である（S4-c-2、
                        // 観測量は S4-c-3-2a で置き換えた）。占有は `CURRENT` が
                        // 示し、参加は `schedule_switch` を通った回数が示す。
                        // 片方では足りない——占有だけなら「割り当てたが一度も
                        // 通っていない」を通し、参加だけなら「誰の担当か分からない
                        // まま数字が増えている」を通す。
                        //
                        // 前の観測量（アイドルループの反復回数）は捨てた。
                        // 早期リターンを残した構成でも同じように増えるので、
                        // 「参加した」と「従来どおりループしている」を区別
                        // できなかった。
                        crate::task::ap_current_display(),
                        crate::task::ap_schedule_passes(),
                        idt::ipi_probe_sent(),
                        idt::ipi_probe_received_for(1),
                        crate::bkl::tlb_generation(),
                        crate::bkl::generation_flushes_for(1),
                        // 漂流の観測量（S6-c）。定常状態では動かないはずの量で、
                        // 動いたら「解放されない確保がある」ことになる。
                        //
                        // 専用の出力経路を作らない。既にBKLの内側で出ている
                        // この行へ相乗りする。行を増やすと混線の機会が増える。
                        //
                        // 判定は「全標本が同一であること」である（`docs/verification-coverage.md`）。
                        // 整数なので傾きの推定は要らない。多点の価値は「いつ動いたか」
                        // が特定できることにある。
                        crate::heap::ALLOCATOR.free_bytes(),
                        crate::heap::ALLOCATOR.free_block_count(),
                        crate::keyboard::buffer::received_count(),
                        crate::keyboard::buffer::overflow_count(),
                        crate::keyboard::stray_irq_count(),
                        idt::spurious_count(),
                        idt::lapic_spurious_count(),
                        // 会計。irq1 は IDT 側のベクタ別カウンタ。
                        // keys + stray がこれと一致しなければ経路の取り違えがある。
                        idt::interrupt_count(crate::keyboard::delivery_vector()),
                        crate::keyboard::accounting_balances(),
                        max_tick_jump(),
                        // 止まった理由の切り分け材料。キーが来なくなったとき、
                        // OBF が 1 なら「データポートを読んでいない」、
                        // PIC ISR にビットが残っていれば「EOI を送っていない」。
                        // どちらも「1 回動いて止まる」症状になるので、この 2 つが
                        // 無いと区別できない。
                        //
                        // **i8042 が無ければ読まない**（HW-b。探らないと決めた機械で
                        // ポートを叩かない）。
                        if !crate::keyboard::controller_present() {
                            "absent"
                        } else if crate::keyboard::controller::output_buffer_full() {
                            "1"
                        } else {
                            "0"
                        },
                        // SAFETY: メインループは通常文脈で、ここは割り込み禁止中
                        // ではない。i8042/PIC を同時に触りうる別の実行文脈は、この
                        // コアの割り込みハンドラだけである（この関数を走らせるのは
                        // BSP だけで、AP は `smp::ap_heartbeat_loop` へ入る）。ハンドラは
                        // ISR を読んでも元に戻す必要がない読み出し専用の操作しか
                        // しないため、競合しても値がずれるだけで壊れない。
                        // **失効条件は「AP がこの経路へ入るようになるとき」である。**
                        unsafe { crate::machine::pc::irq::service_snapshot() },
                        // **UART のロックの計測（シリアルの排他の段）。**
                        //
                        // **どちらも 0 が正常である。** **`forced` が 0 でなければ
                        // 上限か設計を見直す材料になり、`reentry` が 0 でなければ
                        // 「割り込みハンドラは何も出力しない」が破られている。**
                        // **あの規約は検査されていない**ので、ここが事後の観測になる。
                        //
                        // **判定にしない。** **揺れる値なので、揺れる行へ相乗りする**
                        // （この行は `BOOT_LOG_VOLATILE_MARKERS` に在る）。
                        common::machine::pc::serial::forced_write_count(),
                        common::machine::pc::serial::reentry_count()
                    ),
                );
            }
        }
        // ← ここで BKL を離す。`hlt` はこの外にある。

        // **シェルへ渡す（S11-11）。** 定常ループの観測はここで完了する。
        //
        // **ティック数ではなくハートビートの本数で決める。** ティックの閾値だと
        // **越えた時点で何本出ているかが揺れる**——`hlt` から起きた時点で数えるので、
        // **`-smp 1` と `-smp 2` で行数が 1 本ずれた**（実測）。
        // **本数で決めれば、どの構成でも同じ本数だけ出る。**
        //
        // **割り込みは止めない。** シェルはキーボードの割り込みで動く。
        // **戻らない**——`kernel_main` が `init` を走らせ、そちらが `-> !` である。
        //
        // **なぜここで完了するのか。** ハートビートは**タイマ経路の健全性**を見る
        // もので、**シェルとは別の主張である。** シェルが定期的に出す形にすると
        // **シェルの都合で観測の頻度が変わる。** ここまでで十分な回数のティックを
        // 観測してあるので、**最後の 1 本を出して完了する。**
        if stop_after_ticks == 0
            && shell_after_heartbeats != 0
            && heartbeats >= shell_after_heartbeats
        {
            // **揺れる値をこの行へ載せない**（S9-b-3-1 で決めた形）。
            // **観測したティック数は起動ごとに違う**——`hlt` から起きた時点で
            // 数えるので、**どこで閾値を越えるかが揺れる**（実測で 256 と 259）。
            // **数はハートビートの行が出している。** ここが主張するのは
            // **「会計が合ったまま定常ループを抜ける」**ことだけである。
            // **両方のコアの観測を完了する。** AP のハートビートも止まる。
            close_steady_observation();
            logger.info(format_args!(
                "timer: this is the end of the steady-loop observation, \
                 timer_accounting_balanced={}; the shell takes the foreground from here",
                idt::timer_accounting_balances()
            ));
            return;
        }

        if stop_after_ticks != 0 && ticks >= stop_after_ticks {
            logger.info(format_args!(
                "timer: reached the tick limit ({stop_after_ticks}); leaving the loop"
            ));
            // SAFETY: 観測が終わったので、割り込みを禁止した既知の状態へ戻す。
            unsafe {
                cpu::disable_interrupts();
            }
            return;
        }

        // 破壊テスト (S4-b-2, bkl-hold-across-hlt): 離さずに `hlt` する。
        // もう一方のコアが IF=0 で待ち続け、タイムアウトして原因を出す。
        // 「静かに止まる」を「うるさく止まる」へ変えた形の実証である。
        #[cfg(feature = "bkl-hold-across-hlt-test")]
        let _bkl_held_across_hlt = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);

        // 次のティックまで眠る。`sti` は既に効いているが、
        // `enable_interrupts_and_wait` を使うことで `sti; hlt` の隣接が
        // 常に保たれる（M5 で条件つきの形へ移す際もここを変えずに済む）。
        //
        // SAFETY: ハンドラは用意済みで、EOI も発行している。
        unsafe {
            common::arch::x86_64::enable_interrupts_and_wait();
        }
    }
}

/// AP のティック数を 1 つの表示へまとめる（S4-a）。
///
/// BSP 自身のぶんは含めない。ハートビートの `ticks=` が既に BSP のぶんで、
/// 同じ数を 2 度出すと、どちらが合計かが読めなくなる。
struct ApTickSummary;

impl core::fmt::Display for ApTickSummary {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        for cpu in 1..common::percpu::MAX_CPUS {
            if !first {
                write!(f, " ")?;
            }
            first = false;
            write!(f, "cpu{cpu}={}", idt::timer_ticks_for(cpu))?;
        }
        if first {
            write!(f, "none")?;
        }
        Ok(())
    }
}

/// [`ApTickSummary`] を作る。
const fn ap_tick_summary() -> ApTickSummary {
    ApTickSummary
}

/// シリアルと画面の両方へ 1 行出す。必ずシリアルを先に書く
/// （architecture.md §6.7）。
/// シリアルの排他の演習（BSP 側）。
///
/// **合図を立て、AP と同時に既知の行を書き、AP が書き終えるのを待つ。**
/// **待つのは、演習の途中でこの先の起動シーケンスが混ざらないようにするため
/// である**——**混ざると、判定が「錠が効いていない」と「起動の行が挟まった」を
/// 区別できなくなる。**
#[cfg(feature = "serial-stress-test")]
fn run_serial_stress_on_bsp(logger: &mut Logger<SerialPort>) {
    use core::sync::atomic::Ordering;

    crate::smp::SERIAL_STRESS_GO.store(true, Ordering::Release);
    for index in 0..crate::smp::SERIAL_STRESS_LINES {
        logger.info(format_args!(
            "serial-stress: cpu0 {index:04} {}",
            crate::smp::SERIAL_STRESS_PADDING
        ));
    }
    // **上限つきで待つ。**
    let started = common::arch::x86_64::cpu::read_timestamp_counter();
    while !crate::smp::SERIAL_STRESS_AP_DONE.load(Ordering::Acquire) {
        if common::arch::x86_64::cpu::read_timestamp_counter().wrapping_sub(started)
            > 20_000_000_000
        {
            logger.error(format_args!(
                "serial-stress: the application processor never finished; the exercise asserts \
                 nothing"
            ));
            return;
        }
        core::hint::spin_loop();
    }
    // **計測（判定にしない）。** **揺れる値なので `(info)` の側である。**
    logger.info(format_args!(
        "serial-stress: done; the uart lock was forced {} time(s) and re-entered on the same \
         core {} time(s)",
        common::machine::pc::serial::forced_write_count(),
        common::machine::pc::serial::reentry_count()
    ));
}

fn log_both(
    logger: &mut Logger<SerialPort>,
    console: Option<&mut crate::console::Console>,
    args: core::fmt::Arguments,
) {
    logger.info(args);
    if let Some(console) = console {
        use core::fmt::Write as _;
        let _ = writeln!(console, "[INFO] {args}");
    }
}

/// リングバッファを吸い出し、生のスキャンコードをシリアルへ出す。
///
/// 段階 4（ハードウェア接続の確認）で最も重要な出力である。変換もエコーも
/// せず受け取ったバイトをそのまま 16 進で出すので、ここが出ていれば
/// 「割り込みが届き、データポートが読めている」ことが確定する。以降の不具合は
/// すべてデコード側の問題に絞り込める。
fn drain_keyboard(
    logger: &mut Logger<SerialPort>,
    console: Option<&mut crate::console::Console>,
    announced_first: &mut bool,
    decoder: &mut crate::keyboard::decode::Decoder,
    line: &mut TypedLine,
) {
    use crate::keyboard::decode::KeyEvent;

    let mut console = console;

    // **前景を Ring 3 が持っているなら、こちらは取り出さない（S11-10）。**
    //
    // **入力の消費者は同時に 1 つである**（`crate::input` の不変条件）。
    // **リングは取り出したら消える**ので、2 人が取ると**どちらも全部は見ない。**
    //
    // **積むのは止めない。** 割り込みハンドラはそのままリングへ積み、
    // **前景が戻ったときに、溜まっていたぶんをこちらが読む。**
    if crate::input::foreground_is_claimed() {
        return;
    }

    loop {
        // ロックは 1 バイトごとに取って離す。保持したままログを出さない。
        // ログ出力は長く、その間ずっと割り込みが禁止されるとティックを
        // 取りこぼす。
        let code = {
            let mut ring = crate::keyboard::buffer::SCANCODES.lock();
            ring.pop()
        };
        let Some(code) = code else {
            return;
        };

        if !*announced_first {
            *announced_first = true;
            // IRQ1 の配送経路の証明。タイマで 0x20 を確認したのと同じ趣旨。**プログラムを起動する入口と
            // 同じ関数で 1 度だけ出す**（HW-e-2。`crate::keyboard::report_first_delivery_once`）。
            crate::keyboard::report_first_delivery_once(logger);
        }

        // 生のスキャンコード。押下と離脱で 2 回出るので、エコーと二重に
        // なって読みにくい。既定では出さず、切り分けが要るときだけ
        // `keyboard-raw-log` feature で有効にする。
        #[cfg(feature = "keyboard-raw-log")]
        logger.info(format_args!("keyboard: scancode {code:#04x}"));

        let Some(event) = decoder.feed(code) else {
            continue;
        };

        match event {
            KeyEvent::Char(character) => {
                line.push(character);
                echo(console.as_deref_mut(), character);
            }
            KeyEvent::Enter => {
                echo(console.as_deref_mut(), '\n');
                // 1 行分をまとめて出す。自動テストはこの行を突き合わせる。
                logger.info(format_args!("keyboard: line = \"{}\"", line.as_str()));
                line.clear();
            }
            // **カーソルの矢印と Esc は、この行では何もしない（S12 前の手当て。
            // zi-a で上下と Esc が増えた）。**
            //
            // **ここの行（`TypedLine`）は挿入点を持たない。** 前景が取られる前の
            // 診断用で、**編集を持つのはシェルの側である**（`kernel/userland/zash.rs`）。
            // **前景が取られている間、この経路は動かない。**
            KeyEvent::ArrowLeft
            | KeyEvent::ArrowRight
            | KeyEvent::ArrowUp
            | KeyEvent::ArrowDown
            | KeyEvent::Escape
            // **Delete も何もしない（zi-f）。** **この行は挿入点を持たない**
            // ——`Backspace` と同じ理由で、編集は Ring 3 の側にある。
            | KeyEvent::Delete
            // **Home と End も同じである（SE-a。`ADR-0050`）。**
            | KeyEvent::Home
            | KeyEvent::End => {}
            KeyEvent::Backspace => {
                // 画面上の消去は行わない。コンソール側でセルごとの
                // 占有種別（全角の先頭 / 後続）を管理する必要があり、
                // 割り込みとは別の仕事になる（`docs/deferred-decisions.md`）。
                // キーとして認識していることだけ示す。
                logger.info(format_args!(
                    "keyboard: backspace (not applied to the screen yet)"
                ));
            }
            KeyEvent::Unsupported(code) => {
                UNSUPPORTED_KEYS.fetch_add(1, Ordering::Relaxed);
                logger.info(format_args!("keyboard: unsupported scancode {code:#04x}"));
            }
        }
    }
}

/// 対応していないキーを受けた回数。
static UNSUPPORTED_KEYS: AtomicU64 = AtomicU64::new(0);

pub fn unsupported_key_count() -> u64 {
    UNSUPPORTED_KEYS.load(Ordering::Relaxed)
}

/// 入力された文字を画面へ出す。
///
/// メインループから呼ぶ（ADR-0018 §5）。ハンドラからは呼ばない。
///
/// # シリアルへは 1 文字ずつ出さない
///
/// シリアルへ 1 文字ずつ流すには `Logger` の内側の `SerialPort` を直接
/// 触る必要がある。そのためのアクセサを `Logger` に足すと、レベル判定と
/// 接頭辞の書式を迂回する経路を全利用者に開くことになる。ADR-0017
/// Addendum の反省（守るべき制約と、たまたま採った手段を混同しない）に
/// 照らして、ここは足さない。
///
/// 代わりにシリアルへは Enter のときに 1 行としてまとめて出す。1 文字ずつの
/// 追跡が要る場合は `keyboard-raw-log` feature で生スキャンコードを出す。
fn echo(console: Option<&mut crate::console::Console>, character: char) {
    use core::fmt::Write as _;

    if let Some(console) = console {
        let _ = write!(console, "{character}");
    }
}

/// 打ち込んだ 1 行を貯める固定長バッファ。
///
/// ヒープを使わない。長さを超えた分は捨てる（入力行が異常に長いのは
/// テストの想定外で、捨てても診断に影響しない）。
struct TypedLine {
    buffer: [u8; Self::CAPACITY],
    len: usize,
}

impl TypedLine {
    const CAPACITY: usize = 128;

    const fn new() -> Self {
        Self {
            buffer: [0; Self::CAPACITY],
            len: 0,
        }
    }

    fn push(&mut self, character: char) {
        // ASCII だけを貯める。現在のデコーダは ASCII しか返さない。
        if character.is_ascii() && self.len < Self::CAPACITY {
            self.buffer[self.len] = character as u8;
            self.len += 1;
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn as_str(&self) -> &str {
        // SAFETY: push で ASCII だけを入れているため、常に有効な UTF-8。
        core::str::from_utf8(&self.buffer[..self.len]).unwrap_or("<invalid>")
    }
}
