//! `sti` 前の実行時検証とメインループ（M4-d-1）。
//!
//! ADR-0018 §2 は「`sti` は 1 箇所だけで行い、直前に 7 項目をすべて検証する。
//! 1 つでも欠ければ `sti` せずに fail-fast する」と定めている。その 7 項目を
//! 実際に確かめるのがこのモジュールである。
//!
//! 検証は**設定したつもりの値ではなく実際の状態**を読む。`sgdt` / `sidt` /
//! `str` / セグメントレジスタ / PIC の IMR は、いずれもハードウェアから
//! 読み戻したものを使う。

use core::sync::atomic::{AtomicU64, Ordering};

use common::cpu;
use common::log::Logger;
use common::serial::SerialPort;

use crate::gdt;
use crate::idt;
use crate::irq;

/// 検証項目 1 件の結果。
///
/// **`bool` にしていない。** ADR-0018 §2 の項目 4（PIC のベクタオフセット）は
/// 「OK」でも「NG」でもなく**確かめる手段が無い**。ICW2 が書き込み専用だから
/// である（ADR-0018 Addendum）。これを `true` に丸めると、検証していない
/// ものを検証済みとして数えることになり、本プロジェクトで過去 2 回起きた
/// 誤りを再導入する。第 3 の状態として型で区別する。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckState {
    /// 実際の状態を読み、期待どおりだった。
    Verified,
    /// 実際の状態を読み、期待と違った。`sti` してはならない。
    Failed,
    /// 確かめる手段が無い。理由と、いつ確かめられるようになるかを添える。
    Unverifiable,
}

impl CheckState {
    /// ログに出す短い記号。3 状態が一目で区別できるようにする。
    pub fn label(self) -> &'static str {
        match self {
            CheckState::Verified => "VERIFIED",
            CheckState::Failed => "FAILED",
            CheckState::Unverifiable => "UNVERIFIABLE",
        }
    }

    /// `sti` を妨げるか。
    ///
    /// `Unverifiable` は妨げない。**その判断が安全な理由は個別に示す必要が
    /// あり、型だけでは正当化されない。** 項目 4 の場合の根拠は
    /// [`verify_ready_for_sti`] のコメントに書いてある。
    pub fn blocks_sti(self) -> bool {
        matches!(self, CheckState::Failed)
    }
}

/// 7 項目すべての結果。
pub struct ReadinessReport {
    pub gdt_and_segments: CheckState,
    pub tss_and_ist: CheckState,
    pub idt_and_exception_gates: CheckState,
    pub pic_remapped: CheckState,
    pub irqs_masked: CheckState,
    pub interrupt_safe_locks: CheckState,
    pub handlers_send_eoi: CheckState,
}

impl ReadinessReport {
    fn all(&self) -> [(&'static str, CheckState); 7] {
        [
            ("1. GDT loaded, CS/DS/SS are ours", self.gdt_and_segments),
            ("2. TSS loaded, IST stack present", self.tss_and_ist),
            (
                "3. IDT loaded, all exception gates present",
                self.idt_and_exception_gates,
            ),
            (
                "4. interrupt delivery vectors are set as intended",
                self.pic_remapped,
            ),
            ("5. IRQs without a handler are masked", self.irqs_masked),
            (
                "6. Locked<T> disables interrupts while held",
                self.interrupt_safe_locks,
            ),
            ("7. handlers issue EOI", self.handlers_send_eoi),
        ]
    }

    pub fn may_enable_interrupts(&self) -> bool {
        !self.all().iter().any(|(_, state)| state.blocks_sti())
    }
}

/// ADR-0018 §2 の 7 項目を実行時に検証する。
///
/// M4-d-1 時点での各項目の扱い:
///
/// - **項目 4（PIC 再マップ）は `Unverifiable`。** ICW2 は書き込み専用で
///   読み戻せない。**それでも先へ進んで安全な根拠は、項目 5 が成立して
///   いること**である。全 IRQ をマスクしているため、仮に ICW2 が誤った値に
///   なっていても割り込みは 1 つも配送されず、害が生じようがない。
///   逆に言えば、**項目 5 が `Verified` でない限り項目 4 の
///   `Unverifiable` は許されない。** 証明されるのは M4-d-2 で最初のタイマ
///   割り込みがベクタ 0x20 として届いたときである。
/// - **項目 7（EOI）は `Unverifiable`。** M4-d-1 では IRQ ハンドラを
///   マスク解除しないため、EOI を発行する対象そのものが存在しない。
///   M4-d-2 で実装と同時に `Verified` へ昇格させる。
pub fn verify_ready_for_sti(logger: &mut Logger<SerialPort>) -> ReadinessReport {
    verify_ready(logger, &[], false)
}

/// タイマとキーボードを解禁した後の 7 項目検証。
///
/// [`verify_ready_for_sti`] との違いは 2 点だけ。項目 5 の期待値が
/// 「全マスク」から「IRQ0 だけ解除」へ変わることと、項目 7（EOI）が
/// `Unverifiable` ではなくなることである。項目 7 が `Verified` へ移るのは
/// 実際にティックが増え続けたときなので、この時点では
/// 「実装済み・これから検証」として扱う。
pub fn verify_ready_for_sti_with_timer(logger: &mut Logger<SerialPort>) -> ReadinessReport {
    // IRQ0（タイマ）と IRQ1（キーボード）を解禁した状態。ハンドラを書いた
    // ベクタだけが開いていることを、実際の IMR と突き合わせる。
    //
    // **8259 に残っている IRQ だけを数える**（S2-d-1c）。IRQ1 を I/O APIC 経由へ
    // 移すと、8259 側では**マスクされているのが正しい。** 移行後も IRQ1 を
    // 「開いているはず」と期待すると、正しい状態でこの検査が落ちる。
    // **移行状態を見て期待を作る**ので、移行の前後どちらでも成立する。
    if crate::irq::routed_to_apic(crate::keyboard::KEYBOARD_IRQ) {
        return verify_ready(logger, &[0], true);
    }
    verify_ready(logger, &[0, crate::keyboard::KEYBOARD_IRQ], true)
}

fn verify_ready(
    logger: &mut Logger<SerialPort>,
    unmasked: &[u8],
    timer_enabled: bool,
) -> ReadinessReport {
    // --- 1. GDT と CS/DS/SS ---
    let (gdt_base, _) = gdt::current_gdt();
    let code = gdt::current_code_selector();
    let (data, stack) = gdt::current_data_selectors();
    let expected_data = gdt::KERNEL_DATA_SELECTOR.bits();
    logger.info(format_args!(
        "sti-check 1: GDT base={gdt_base:#x} (expected {:#x}), CS={code:#06x} (expected {:#06x}), \
         DS={data:#06x} SS={stack:#06x} (expected {expected_data:#06x})",
        gdt::gdt_base(),
        gdt::KERNEL_CODE_SELECTOR.bits()
    ));
    let gdt_and_segments = if gdt_base == gdt::gdt_base()
        && code == gdt::KERNEL_CODE_SELECTOR.bits()
        && data == expected_data
        && stack == expected_data
    {
        CheckState::Verified
    } else {
        CheckState::Failed
    };

    // --- 2. TSS と IST ---
    let task_register = gdt::current_task_register();
    let ist_top = gdt::double_fault_stack_top();
    let ist_entry = idt::entry(8).and_then(|e| e.ist_index());
    logger.info(format_args!(
        "sti-check 2: TR={task_register:#06x} (expected {:#06x}), IST1 top={ist_top:#x}, \
         #DF gate IST index={ist_entry:?}",
        gdt::TSS_SELECTOR.bits()
    ));
    let tss_and_ist = if task_register == gdt::TSS_SELECTOR.bits()
        && ist_top != 0
        && ist_entry == Some(gdt::DOUBLE_FAULT_IST_INDEX as u8)
    {
        CheckState::Verified
    } else {
        CheckState::Failed
    };

    // --- 3. IDT と例外ゲート ---
    let (idt_base, idt_limit) = idt::current_idt();
    // 32 個の CPU 例外ベクタすべてが present であること（ADR-0018 §2 項目 3）。
    let mut exception_gates_ok = idt_base == idt::idt_base() && idt_limit == idt::expected_limit();
    for vector in 0..32 {
        exception_gates_ok &= idt::entry(vector).is_some_and(|e| {
            e.is_present() && e.gate_type() == 0xE && e.descriptor_privilege_level() == 0
        });
    }
    // スタブ表は 2 系統ある。片方の検証がもう片方を保証しない。
    let exception_stubs = idt::check_stub_table();
    let irq_stubs = idt::check_irq_stub_table();
    logger.info(format_args!(
        "sti-check 3: IDT base={idt_base:#x} limit={idt_limit}, first 32 gates ok={exception_gates_ok}, \
         exception stub table ok={}, irq stub table ok={} ({:#x}..{:#x}, size={} expected={})",
        exception_stubs.is_ok(),
        irq_stubs.is_ok(),
        irq_stubs.base,
        irq_stubs.end,
        irq_stubs.actual_size,
        irq_stubs.expected_size
    ));
    let idt_and_exception_gates =
        if exception_gates_ok && exception_stubs.is_ok() && irq_stubs.is_ok() {
            CheckState::Verified
        } else {
            CheckState::Failed
        };

    // --- 5. IRQ マスク（項目 4 の判断に必要なので先に評価する）---
    // 判定と表示は同じ 1 回の読み出しから導く（`MaskCheck`）。別々に読むと
    // ログの値と判定の根拠が食い違いうる。
    let masks = irq::check_masks(unmasked);
    logger.info(format_args!("sti-check 5: PIC IMR {masks}"));
    let irqs_masked = if masks.matches() {
        CheckState::Verified
    } else {
        CheckState::Failed
    };

    // --- 4. 配送先ベクタの設定（検証不能）---
    //
    // **主題を書き換えた**（S2-d-1c）。以前は「PIC を 0x20-0x2F へ再マップした」
    // という PIC 固有の主題だったが、配送が 2 系統になったのでどちらの
    // コントローラでも意味を持つ主題にしてある。
    //
    // **状態は UNVERIFIABLE のままである。** I/O APIC の redirection entry は
    // 読み戻せるが、**タイマはまだ 8259 を通っており、そちらの ICW2 は
    // write-only である。** 全経路が読み戻せるようになるのは、PIC が消える
    // S2-d-2 である。そこで Verified へ格上げする。
    logger.info(format_args!(
        "sti-check 4: cannot be verified here; the timer still goes through the 8259, whose \
         ICW2 is write-only, so its vector offset cannot be read back. Proceeding is safe only \
         because check 5 holds: with every IRQ masked, a wrong offset delivers nothing. Proof \
         arrives when the first timer IRQ shows up as vector {:#04x}. The I/O APIC path is \
         covered separately: its redirection entry is read back after routing (IRQ1)",
        idt::TIMER_VECTOR
    ));
    let pic_remapped = if timer_enabled {
        // タイマを解禁した以上、マスクによる保護はもう無い。ここから先は
        // 「最初のティックがベクタ 0x20 で届くか」で事後的に判定する。
        // まだ届いていないので、この時点では未検証のままである。
        CheckState::Unverifiable
    } else if irqs_masked == CheckState::Verified {
        CheckState::Unverifiable
    } else {
        // マスクが効いていないなら、検証不能を許す根拠そのものが失われる。
        CheckState::Failed
    };

    // --- 6. 割り込み保存版の Locked<T> ---
    // 型として差し替え済みであることは M4-c-2 のコンパイル時点で決まって
    // いるが、実際に IF が落ちるかは実測する。
    let interrupt_safe_locks = verify_lock_disables_interrupts(logger);

    // --- 7. EOI ---
    let handlers_send_eoi = if timer_enabled {
        logger.info(format_args!(
            "sti-check 7: the timer handler issues EOI; this is proven only by ticks continuing \
             to arrive, so it stays unverified until the loop has seen at least two"
        ));
        CheckState::Unverifiable
    } else {
        logger.info(format_args!(
            "sti-check 7: no IRQ is unmasked, so there is no interrupt to acknowledge"
        ));
        CheckState::Unverifiable
    };

    let report = ReadinessReport {
        gdt_and_segments,
        tss_and_ist,
        idt_and_exception_gates,
        pic_remapped,
        irqs_masked,
        interrupt_safe_locks,
        handlers_send_eoi,
    };

    for (name, state) in report.all() {
        logger.info(format_args!(
            "sti-check summary: {name} = {}",
            state.label()
        ));
    }

    report
}

/// ロックの保持中に実際に IF が落ちることを測る（項目 6）。
fn verify_lock_disables_interrupts(logger: &mut Logger<SerialPort>) -> CheckState {
    use common::critical::Locked;

    static PROBE: Locked<u64> = Locked::new(0);

    fn if_set() -> bool {
        cpu::read_rflags() & cpu::RFLAGS_INTERRUPT_FLAG != 0
    }

    let before = if_set();
    let inside = {
        let _guard = PROBE.lock();
        if_set()
    };
    let after = if_set();

    logger.info(format_args!(
        "sti-check 6: IF before={before} while holding the lock={inside} after={after} \
         (must be false while held, and back to the entry value afterwards)"
    ));

    if !inside && after == before {
        CheckState::Verified
    } else {
        CheckState::Failed
    }
}

/// [`spin_with_interrupts_enabled`] が観測した、スピン中の割り込み増加分。
static SPIN_INTERRUPT_DELTA: AtomicU64 = AtomicU64::new(0);

/// スピン中に増えた割り込みの合計（絶対値ではなく増加分）。
pub fn spin_interrupt_delta() -> u64 {
    SPIN_INTERRUPT_DELTA.load(Ordering::Relaxed)
}

/// メインループが 1 周するたびに増やす周回カウンタ。
///
/// 「回っているが割り込みが来ない」と「そもそも回っていない」を区別する
/// ためのもの。カウンタが増えないなら、`hlt` から起きていないか、そこへ
/// 到達していない。
static LOOP_ITERATIONS: AtomicU64 = AtomicU64::new(0);

pub fn loop_iterations() -> u64 {
    LOOP_ITERATIONS.load(Ordering::Relaxed)
}

/// 割り込みを有効にした状態で一定時間アイドルし、何も届かないことを確かめる。
///
/// # なぜ M4-d-1 では `hlt` しないのか
///
/// ADR-0018 §7 はメインループを `hlt` で待つ形にすると定めており、そのための
/// [`cpu::enable_interrupts_and_halt`]（`sti; hlt` 隣接）も用意した。
/// **しかし M4-d-1 でそれを使うと、確実にハングする。**
///
/// `hlt` は次の割り込みが来るまで CPU を止める命令である。M4-d-1 は全 IRQ を
/// マスクした状態で `sti` するので、**そもそも起こしてくれるものが存在しない**。
/// 最初の `hlt` に入った時点で永久に止まり、周回カウンタもハートビートも
/// 進まず、期限の判定にも到達しない。外から見ると「`sti` した瞬間にハング
/// した」という、まさに M4-d-1 で切り分けたい症状と区別がつかない形になる。
///
/// そこで M4-d-1 のこのループは**期限つきのスピン**にしてある。`hlt` を使う
/// 本来の形は、起こしてくれるタイマが実在する M4-d-2 で初めて成立する。
/// ビジーループを避ける理由（TCG のログ肥大）は、割り込みが 1 件も無い
/// M4-d-1 では問題にならない。`-d int` は割り込みが起きたときだけ記録する
/// ためである。
///
/// # Safety
///
/// 割り込みを有効化する。[`verify_ready_for_sti`] が
/// [`ReadinessReport::may_enable_interrupts`] を返した後にのみ呼ぶこと。
pub unsafe fn spin_with_interrupts_enabled(
    logger: &mut Logger<SerialPort>,
    duration_tsc: u64,
    heartbeat_interval: u64,
) {
    // SAFETY: 呼び出し側の契約により 7 項目の検証を通っている。
    //
    // **`sti` を実行するのはここと [`run_timer_loop`] の 2 箇所だけである。**
    // ADR-0018 §2 は「`sti` は 1 箇所だけ」と決めたが、M4-d を d-1（期限つき
    // スピンで sti 自体を検証する）と d-2（タイマループ。`hlt` で待つ本来の形）へ
    // 分けた結果、実装は 2 箇所になった（ADR-0018 Addendum 5）。どちらも
    // 7 項目の検証を通った後にしか実行しない、という §2 の本質は保たれている。
    // かつてこのコメントは両方が自分を「唯一の箇所」と書いており、実際の数と
    // 食い違っていた。**「唯一」を前提に検査を設計すると許可対象を数え違える。**
    unsafe {
        cpu::enable_interrupts();
    }

    // **基準点を取ってから測る。** 起動シーケンス中に既に発生している分
    // （`--interrupt-test irq-path` のソフトウェア割り込みなど）を「今
    // 届いたもの」と取り違えないようにする。
    let baseline = idt::snapshot_counts();

    let started = cpu::read_timestamp_counter();
    let if_after_sti = cpu::read_rflags() & cpu::RFLAGS_INTERRUPT_FLAG != 0;
    logger.info(format_args!(
        "sti: interrupts are now enabled (IF={if_after_sti}, read back from RFLAGS)"
    ));
    if !if_after_sti {
        logger.error(format_args!("sti: IF did not become set; halting"));
        cpu::halt_forever();
    }

    let deadline = started + duration_tsc;
    let mut next_heartbeat = started + heartbeat_interval;

    loop {
        let now = cpu::read_timestamp_counter();

        let (total, first) = idt::delta_since(&baseline);
        if let Some(vector) = first {
            // M4-d-1 では 1 件も来ないのが正しい。届いたならマスクが効いて
            // いないか、PIC 以外の経路（LAPIC）が生きている。NMI（ベクタ 2）は
            // `cli` でマスクできないため、理論上はここに現れうる。
            // **どのベクタだったかを必ず出す。** 合計だけでは原因の見当が
            // つかない。
            logger.error(format_args!(
                "idle: an interrupt arrived while every IRQ is masked \
                 (total={total}, first non-zero vector={vector:#04x}, count for it={})",
                idt::interrupt_count(vector)
            ));
            break;
        }

        if now >= next_heartbeat {
            next_heartbeat = now + heartbeat_interval;
            logger.info(format_args!(
                "heartbeat: loop iterations={}, interrupts seen={total}, IF={}",
                loop_iterations(),
                cpu::read_rflags() & cpu::RFLAGS_INTERRUPT_FLAG != 0
            ));
        }

        LOOP_ITERATIONS.fetch_add(1, Ordering::Relaxed);

        if now >= deadline {
            break;
        }
    }

    let (final_total, _) = idt::delta_since(&baseline);
    SPIN_INTERRUPT_DELTA.store(final_total, Ordering::Relaxed);

    // 後片付け。M4-d-2 まで再び禁止しておく。
    // SAFETY: 観測が終わったので、割り込みを禁止した既知の状態へ戻す。
    unsafe {
        cpu::disable_interrupts();
    }
}

/// ハートビートを出す間隔（ティック数）。
///
/// 100Hz なので 100 ティック = 約 1 秒。**画面で目視して変化が分かる
/// 間隔にしてある。** これより短いと画面のスクロールが速すぎて読めず、
/// 長いと「動いているのか止まっているのか」の判断が遅れる。
pub const HEARTBEAT_TICKS: u64 = 100;

/// 最初のティックを待つ上限（TSC サイクル）。
///
/// これを過ぎても 1 件も来ないなら、タイマが設定できていないか、IMR が
/// 効いていないか、ICW2 が誤っているかのいずれかである。無言で待ち続けると
/// ハングと区別がつかないので fail-fast する。
const FIRST_TICK_TIMEOUT_CYCLES: u64 = 20_000_000_000;

/// メインループが 1 回起きるあいだに進んだティック数の最大値。
///
/// **これはハードウェアのティック取りこぼしではなく、メインループが
/// 「観測し損ねた」量である。** 1 なら毎ティック起きて観測できている。
/// 2 以上なら、起きて処理しているあいだに次のティックが来ている。
///
/// 本当の意味でのティック取りこぼし（PIT が発火したのに CPU へ届かない、
/// あるいは EOI が間に合わず次が抑止される）は、独立した第 2 の時間源が
/// 無いと検出できない。現状 TSC しか無く、その TSC も仮想化環境では
/// 信用できない（`common::cpu` 参照）。ここで測れるのは
/// 「メインループの追従の遅れ」までである。
static MAX_TICK_JUMP: AtomicU64 = AtomicU64::new(0);

pub fn max_tick_jump() -> u64 {
    MAX_TICK_JUMP.load(Ordering::Relaxed)
}

/// タイマ割り込みで駆動されるメインループ。
///
/// # `hlt` を無条件に使う理由
///
/// ADR-0018 のチェックリスト 10 は「`cli` → 条件確認 → `sti; hlt`」の並びを
/// 求めている。あれが必要なのは**「仕事が無ければ眠る」形のループ**である。
/// 仕事の有無を確認してから眠るまでの隙間に仕事が発生すると、次の割り込みまで
/// 眠り続けてしまう。
///
/// このループは眠るかどうかを条件で決めない。タイマが 100Hz で必ず起こして
/// くれるので、無条件に `hlt` → 起きたらティックを見る → また `hlt` で足りる。
/// 最悪でも 10ms 後には起きるため、取りこぼしという概念が成立しない。
/// 条件つきの形が要るのは M5 の実行キュー（仕事の有無で眠りを決める）である。
///
/// **限界**: この形は、起こしてくれるものが止まった瞬間に永久ハングになる。
/// `hlt` で眠っている以上、カーネル自身はそれを検出できない（検出のための
/// コードが動かない）。**外側からは検出できる**ので、xtask がシリアルログの
/// ハートビート回数で判定する。カーネル内部では検出できないが、テスト基盤
/// では検出できる、という切り分けである。
///
/// # Safety
///
/// 割り込みを有効化する。[`verify_ready_for_sti`] を通し、タイマの設定と
/// IRQ0 の解禁が済んでいること。
pub unsafe fn run_timer_loop(
    logger: &mut Logger<SerialPort>,
    console: Option<&mut crate::console::Console>,
    stop_after_ticks: u64,
    apic: Option<&crate::apic::MappedApic>,
) {
    // 最初のティックが来るまで何も出ないとハングと区別できないので、
    // 待ちに入ることを先に宣言する。
    logger.info(format_args!(
        "timer: waiting for the first tick (expected as vector {:#04x}); \
         if nothing arrives, suspect the PIT setup, the IMR, or ICW2",
        idt::TIMER_VECTOR
    ));

    let mut console = console;
    let started = cpu::read_timestamp_counter();

    // SAFETY: 呼び出し側の契約により、7 項目の検証とタイマ設定が済んでいる。
    //
    // **`sti` を実行するのはここと [`spin_with_interrupts_enabled`] の 2 箇所
    // だけである**（数と経緯は spin 側のコメントと ADR-0018 Addendum 5 を参照）。
    // こちらが「`hlt` で待つ本来の形」で、起こしてくれるタイマが実在する
    // M4-d-2 で初めて成立した（ADR-0018 Addendum 2 §3）。
    unsafe {
        cpu::enable_interrupts();
    }

    // === S2-c: Local APIC タイマの較正 ===
    //
    // **この位置でなければならない。** 基準に使う `TIMER_TICKS` は IRQ0 が
    // 増やすので、`sti` より前では進まない。`start_timer` は `-> !` で戻らず、
    // 通常起動では `run_timer_loop` も戻らないので、「割り込みが有効で、かつ
    // 定常ループへ入る前」という区間はここにしか存在しない。**APIC 関連の他の
    // 処理（`kmain` の前半）から離れているのはこのためである。まとめないこと。**
    //
    // 較正は測るだけで、LAPIC タイマをタイマとして使わない。LVT Timer は
    // マスクされたままで、LINT0 と SVR にも触らない。
    if let Some(apic) = apic {
        let _ = crate::apic::calibrate_timer(logger, apic);

        // === S2-d-1b: 2 つ目のコントローラ実装を 1 回だけ読ませる ===
        //
        // **切り替えない。読むだけである。** 配送は PIC / PIT のままで、
        // 書き込みは一切しない。
        //
        // 呼ぶ理由は、2 つ目の実装が**実ハードウェアを正しく読めることを、
        // 振る舞いが変わらないうちに確かめておく**ためである。どこからも
        // 呼ばずに S2-d-1c（配送が変わる段）へ入ると、そこで落ちたときに
        // 「切り替えが悪いのか、実装が悪いのか」を切り分けられない。
        //
        // 期待は「I/O APIC 経由へ移した IRQ だけが開いている」である。
        // **PIC のマスク状態をここへ持ち込まないこと。** 別のコントローラの
        // 状態である。S2-d-1c で IRQ1 を移したので、開いているのはそれだけに
        // なる。**期待は呼び出し側が持つ**（境界が独自に期待を持たない）。
        let routed: &[u8] = if crate::irq::routed_to_apic(crate::keyboard::KEYBOARD_IRQ) {
            &[crate::keyboard::KEYBOARD_IRQ]
        } else {
            &[]
        };
        match crate::irq::survey_apic_masks(apic, routed) {
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
    }

    // プリエンプティブマルチタスクのデモと検証（M5-d）。timer が動き出した
    // この時点で 1 区間だけ回す。通常起動（stop_after_ticks == 0）でのみ行う。
    // interrupt-test の有限ループ（stop_after_ticks > 0）では回さない。デモが
    // 終わるとワーカーは走行不可になり、以降このハートビートループは
    // プリエンプトされない（runnable がメインだけなので on_timer_tick は
    // no-op）。
    if stop_after_ticks == 0 {
        crate::task::run_preemptive_demo();
    }

    let mut last_ticks = 0u64;
    let mut next_heartbeat = HEARTBEAT_TICKS;
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
            // **ICW2 の事後証明。** 実際に届いたベクタ番号を実値で確認する。
            match idt::first_pic_vector() {
                Some(vector) if vector as usize == idt::TIMER_VECTOR => {
                    log_both(
                        logger,
                        console.as_deref_mut(),
                        format_args!(
                            "timer: first tick arrived as vector {vector:#04x} - this is the \
                             proof that ICW2 was written correctly (it cannot be read back)"
                        ),
                    );
                }
                other => {
                    logger.error(format_args!(
                        "timer: the first PIC interrupt arrived as vector {other:?}, expected \
                         {:#04x}; the PIC vector offset (ICW2) is wrong; halting",
                        idt::TIMER_VECTOR
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

        // キーボードのリングバッファを吸い出す。**メインループが行う。**
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
            // **入力中は画面へ出さない。** ハートビートとエコーが同じ
            // コンソールに出るため、打っている途中に割り込むと入力行が
            // ぶつ切りになって読めなくなる。行が空のときだけ画面にも出す。
            // シリアルへは常に出るので、観測手段は失われない。
            let console_for_heartbeat = if line.is_empty() {
                console.as_deref_mut()
            } else {
                None
            };
            log_both(
                logger,
                console_for_heartbeat,
                format_args!(
                    "heartbeat: ticks={ticks} ({} s), keys={} dropped={} stray={} \
                     spurious={} lapic_spurious={}, \
                     irq1={} balanced={}, max tick jump={}, i8042 OBF={}, PIC ISR={}",
                    ticks / crate::irq::timer_frequency_hz() as u64,
                    crate::keyboard::buffer::received_count(),
                    crate::keyboard::buffer::overflow_count(),
                    crate::keyboard::stray_irq_count(),
                    idt::spurious_count(),
                    idt::lapic_spurious_count(),
                    // **会計。** irq1 は IDT 側のベクタ別カウンタ。
                    // keys + stray がこれと一致しなければ経路の取り違えがある。
                    idt::interrupt_count(crate::keyboard::delivery_vector()),
                    crate::keyboard::accounting_balances(),
                    max_tick_jump(),
                    // **止まった理由の切り分け材料。** キーが来なくなったとき、
                    // OBF が 1 なら「データポートを読んでいない」、
                    // PIC ISR にビットが残っていれば「EOI を送っていない」。
                    // どちらも「1 回動いて止まる」症状になるので、この 2 つが
                    // 無いと区別できない。
                    crate::keyboard::controller::output_buffer_full() as u8,
                    // SAFETY: メインループは通常文脈で、ここは割り込み禁止中
                    // ではないが、シングルコアなので i8042/PIC を同時に触る
                    // 別の実行文脈は割り込みハンドラだけである。ハンドラは
                    // ISR を読んでも元に戻す必要がない読み出し専用の操作しか
                    // しないため、競合しても値がずれるだけで壊れない。
                    unsafe { crate::irq::service_snapshot() }
                ),
            );
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

        // 次のティックまで眠る。`sti` は既に効いているが、
        // `enable_interrupts_and_halt` を使うことで `sti; hlt` の隣接が
        // 常に保たれる（M5 で条件つきの形へ移す際もここを変えずに済む）。
        //
        // SAFETY: ハンドラは用意済みで、EOI も発行している。
        unsafe {
            cpu::enable_interrupts_and_halt();
        }
    }
}

/// シリアルと画面の両方へ 1 行出す。**必ずシリアルを先に**書く
/// （architecture.md §6.7）。
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
/// **段階 4（ハードウェア接続の確認）で最も重要な出力である。** 変換もエコーも
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

    loop {
        // ロックは 1 バイトごとに取って離す。**保持したままログを出さない。**
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
            // **IRQ1 の配送経路の証明。** タイマで 0x20 を確認したのと同じ趣旨。
            match crate::keyboard::first_keyboard_vector() {
                Some(vector) if vector as usize == crate::keyboard::delivery_vector() => {
                    logger.info(format_args!(
                        "keyboard: first key arrived as vector {vector:#04x} - IRQ1 is wired \
                         through our stub correctly"
                    ));
                }
                other => {
                    logger.error(format_args!(
                        "keyboard: the first key arrived as vector {other:?}, expected {:#04x}; \
                         halting",
                        crate::keyboard::delivery_vector()
                    ));
                    cpu::halt_forever();
                }
            }
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
            KeyEvent::Backspace => {
                // **画面上の消去は行わない。** コンソール側でセルごとの
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
/// **メインループから呼ぶ**（ADR-0018 §5）。ハンドラからは呼ばない。
///
/// # シリアルへは 1 文字ずつ出さない
///
/// シリアルへ 1 文字ずつ流すには `Logger` の内側の `SerialPort` を直接
/// 触る必要がある。そのためのアクセサを `Logger` に足すと、**レベル判定と
/// 接頭辞の書式を迂回する経路**を全利用者に開くことになる。ADR-0017
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

    const fn is_empty(&self) -> bool {
        self.len == 0
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
