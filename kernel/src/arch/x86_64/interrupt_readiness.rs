//! `sti` 前の実行時検証（M4-d-1）。
//!
//! ADR-0018 §2 は「`sti` は 1 箇所だけで行い、直前に 7 項目をすべて検証する。
//! 1 つでも欠ければ `sti` せずに fail-fast する」と定めている。その 7 項目を
//! 実際に確かめるのがこのモジュールである。
//!
//! 検証は設定したつもりの値ではなく実際の状態を読む。`sgdt` / `sidt` /
//! `str` / セグメントレジスタ / PIC の IMR は、いずれもハードウェアから
//! 読み戻したものを使う。
//!
//! **`kernel/src/interrupts.rs` から移した**（2026-09-27。境界の段階の手順 2）。**7 項目は x86 の記述子の表と
//! PC の割り込みコントローラを読み戻すので、CPU 固有の置き場に置く。**
//!
//! **7 項目の後に、全 IRQ をマスクしたまま `sti` し、期限つきで待って何も届かないことを確かめる形
//! （[`spin_with_interrupts_enabled`]。M4-d-1）も、同じ日に interrupts.rs から移した**（`RFLAGS` の IF・TSC・
//! ベクタごとの数を読む）。

use core::sync::atomic::{AtomicU64, Ordering};

use common::arch::x86_64::cpu;
use common::log::Logger;
use common::machine::pc::serial::SerialPort;

use crate::arch::x86_64::gdt;
use crate::arch::x86_64::idt;
use crate::machine::pc::irq;

/// 検証項目 1 件の結果。
///
/// `bool` にしていない。ADR-0018 §2 の項目 4（PIC のベクタオフセット）は
/// 「OK」でも「NG」でもなく確かめる手段が無い。ICW2 が書き込み専用だから
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
    /// `Unverifiable` は妨げない。その判断が安全な理由は個別に示す必要が
    /// あり、型だけでは正当化されない。項目 4 の場合の根拠は
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
/// - 項目 4（PIC 再マップ）は `Unverifiable`。ICW2 は書き込み専用で
///   読み戻せない。それでも先へ進んで安全な根拠は、項目 5 が成立して
///   いることである。全 IRQ をマスクしているため、仮に ICW2 が誤った値に
///   なっていても割り込みは 1 つも配送されず、害が生じようがない。
///   逆に言えば、項目 5 が `Verified` でない限り項目 4 の
///   `Unverifiable` は許されない。証明されるのは M4-d-2 で最初のタイマ
///   割り込みがベクタ 0x20 として届いたときである。
/// - 項目 7（EOI）は `Unverifiable`。M4-d-1 では IRQ ハンドラを
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
    // 8259 に残っている IRQ だけを数える（S2-d-1c）。IRQ1 を I/O APIC 経由へ
    // 移すと、8259 側ではマスクされているのが正しい。移行後も IRQ1 を
    // 「開いているはず」と期待すると、正しい状態でこの検査が落ちる。
    // 移行状態を見て期待を作るので、移行の前後どちらでも成立する。
    //
    // **i8042 が無ければ IRQ1 は開けていない**（HW-b。`ADR-0068`）。8259 でも閉じている
    // のが正しい。
    if crate::machine::pc::irq::routed_to_apic(crate::keyboard::KEYBOARD_IRQ)
        || !crate::keyboard::controller_present()
    {
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
    // **全ゲートが 3 つの共通の入口のどれかへ行くこと**（2026-09-24。`ADR-0018` の Addendum 9）。
    // **上の 2 つはゲートがスタブを指すことまでを見る。** **スタブの飛び先はここが見る。**
    let common_entries = idt::check_gates_lead_to_common_entries();
    logger.info(format_args!(
        "sti-check 3b: all {} IDT gate(s) lead to one of the 3 common entries = {} (exception {}, \
         irq {}, syscall {})",
        idt::IDT_ENTRY_COUNT,
        common_entries.is_ok(),
        common_entries.exception,
        common_entries.irq,
        common_entries.syscall
    ));
    if let Some((vector, handler, target)) = common_entries.first_stray {
        logger.error(format_args!(
            "sti-check 3b: gate {vector:#04x} (stub {handler:#x}) leads to {target:#x?}, not to one of \
             the 3 common entries; an entry that skips them skips cld and the direction-flag check - \
             redo the inventory in ADR-0018 Addendum 9"
        ));
    }
    let idt_and_exception_gates = if exception_gates_ok
        && exception_stubs.is_ok()
        && irq_stubs.is_ok()
        && common_entries.is_ok()
    {
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

    // --- 4. 配送先ベクタの設定（この時点では検証不能）---
    //
    // # なぜ S2-d-2 でも格上げできないのか
    //
    // 設計時は「PIC が消えれば全経路を読み戻せるので Verified にできる」と
    // 見込んでいた。順序の制約で、この検査点では成立しない。
    //
    // Local APIC タイマへの切り替えは較正の後でなければならず、較正は
    // PIT のティックを基準にするので`sti` の後でなければ走らない。
    // つまりこの検査点では、タイマは必ずまだ 8259 経由である。
    //
    // 格上げの代わりに、切り替えの直後に LVT Timer を読み戻して照合する
    // （`switch_to_local_timer`）。8259 の ICW2 と違い LVT は読み戻せるので、
    // そちらは実際に検証されている。「この検査点では検証できない」と
    // 「どこでも検証されていない」は別である。
    //
    // 主題を書き換えた（S2-d-1c）。以前は「PIC を 0x20-0x2F へ再マップした」
    // という PIC 固有の主題だったが、配送が 2 系統になったのでどちらの
    // コントローラでも意味を持つ主題にしてある。
    //
    // 状態は UNVERIFIABLE のままである。I/O APIC の redirection entry は
    // 読み戻せるが、タイマはまだ 8259 を通っており、そちらの ICW2 は
    // write-only である。上のとおり、これは恒久的にそうである。
    // （S2-d-1c の時点では「S2-d-2 で Verified へ格上げする」と書いていた。
    // 順序の制約を見落とした見込み違いで、S2-d-2 で撤回した。）
    logger.info(format_args!(
        "sti-check 4: cannot be verified at this point; the timer still goes through the 8259 \
         here (calibration needs PIT ticks, so the move to the local APIC timer happens after \
         sti), and the 8259 ICW2 is write-only. Proceeding is safe only because check 5 holds: \
         with every IRQ masked, a wrong offset delivers nothing. Proof arrives when the first \
         timer IRQ shows up as vector {:#04x}. The other two paths are read back where they are \
         set up: the I/O APIC redirection entry (IRQ1) and the LVT timer",
        idt::PIC_TIMER_VECTOR
    ));
    // 結論が固定でも、そこへ至る枝には意味が残る。単純化しないこと。
    // 項目 4 は恒久的に `Unverifiable` だが、無条件に `Unverifiable` を返す形へ
    // まとめると、下の `Failed` の枝が守っている性質が消える。マスクが効いて
    // いないなら、検証不能を許す根拠そのものが失われる（項目 5 が
    // `Verified` でない限り項目 4 の `Unverifiable` は許されない）。
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
/// [`cpu::enable_interrupts_and_wait`]（`sti; hlt` 隣接）も用意した。
/// しかし M4-d-1 でそれを使うと、確実にハングする。
///
/// `hlt` は次の割り込みが来るまで CPU を止める命令である。M4-d-1 は全 IRQ を
/// マスクした状態で `sti` するので、そもそも起こしてくれるものが存在しない。
/// 最初の `hlt` に入った時点で永久に止まり、周回カウンタもハートビートも
/// 進まず、期限の判定にも到達しない。外から見ると「`sti` した瞬間にハング
/// した」という、まさに M4-d-1 で切り分けたい症状と区別がつかない形になる。
///
/// そこで M4-d-1 のこのループは期限つきのスピンにしてある。`hlt` を使う
/// 本来の形は、起こしてくれるタイマが実在する M4-d-2 で初めて成立する。
/// ビジーループを避ける理由（TCG のログ肥大）は、割り込みが 1 件も無い
/// M4-d-1 では問題にならない。`-d int` は割り込みが起きたときだけ記録する
/// ためである。
///
/// # Safety
///
/// 割り込みを有効化する。[`verify_ready_for_sti`](crate::arch::x86_64::interrupt_readiness::verify_ready_for_sti) が
/// [`ReadinessReport::may_enable_interrupts`](crate::arch::x86_64::interrupt_readiness::ReadinessReport::may_enable_interrupts)
/// を返した後にのみ呼ぶこと。
pub unsafe fn spin_with_interrupts_enabled(
    logger: &mut Logger<SerialPort>,
    duration_tsc: u64,
    heartbeat_interval: u64,
) {
    // SAFETY: 呼び出し側の契約により 7 項目の検証を通っている。
    //
    // `sti` を実行するのはここと [`run_timer_loop`] の 2 箇所だけである。
    // ADR-0018 §2 は「`sti` は 1 箇所だけ」と決めたが、M4-d を d-1（期限つき
    // スピンで sti 自体を検証する）と d-2（タイマループ。`hlt` で待つ本来の形）へ
    // 分けた結果、実装は 2 箇所になった（ADR-0018 Addendum 5）。どちらも
    // 7 項目の検証を通った後にしか実行しない、という §2 の本質は保たれている。
    // かつてこのコメントは両方が自分を「唯一の箇所」と書いており、実際の数と
    // 食い違っていた。「唯一」を前提に検査を設計すると許可対象を数え違える。
    unsafe {
        cpu::enable_interrupts();
    }

    // 基準点を取ってから測る。起動シーケンス中に既に発生している分
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
            // どのベクタだったかを必ず出す。合計だけでは原因の見当が
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
