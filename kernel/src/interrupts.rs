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
use crate::pic;

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
            ("3. IDT loaded, all exception gates present", self.idt_and_exception_gates),
            ("4. PIC remapped to 0x20-0x2F", self.pic_remapped),
            ("5. IRQs without a handler are masked", self.irqs_masked),
            ("6. Locked<T> disables interrupts while held", self.interrupt_safe_locks),
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
        exception_gates_ok &= idt::entry(vector)
            .is_some_and(|e| e.is_present() && e.gate_type() == 0xE && e.descriptor_privilege_level() == 0);
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
    let (master_mask, slave_mask) = pic::read_masks();
    logger.info(format_args!(
        "sti-check 5: PIC IMR master={master_mask:#04x} slave={slave_mask:#04x} \
         (expected {:#04x}/{:#04x}, every IRQ masked) [read back from hardware]",
        pic::MASK_ALL,
        pic::MASK_ALL
    ));
    let irqs_masked = if master_mask == pic::MASK_ALL && slave_mask == pic::MASK_ALL {
        CheckState::Verified
    } else {
        CheckState::Failed
    };

    // --- 4. PIC 再マップ（検証不能）---
    logger.info(format_args!(
        "sti-check 4: cannot be verified here; the 8259 ICW2 is write-only, so the vector \
         offset cannot be read back. Proceeding is safe only because check 5 holds: with every \
         IRQ masked, a wrong offset delivers nothing. Proof arrives in M4-d-2 when the first \
         timer IRQ shows up as vector {:#04x}",
        pic::MASTER_VECTOR_OFFSET
    ));
    let pic_remapped = if irqs_masked == CheckState::Verified {
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
    logger.info(format_args!(
        "sti-check 7: no IRQ is unmasked in M4-d-1, so there is no interrupt to acknowledge; \
         EOI is implemented and verified in M4-d-2"
    ));
    let handlers_send_eoi = CheckState::Unverifiable;

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
        logger.info(format_args!("sti-check summary: {name} = {}", state.label()));
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
    // SAFETY: 呼び出し側の契約により 7 項目の検証を通っている。ここが
    // ADR-0018 §2 の言う「`sti` を実行する唯一の箇所」である。
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
