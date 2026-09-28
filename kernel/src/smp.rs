//! SMP の下ごしらえ（S1）。この段階では情報を集めるだけで、AP は起動しない。
//!
//! ここが持つのは、S3（AP の起動）で要るがS1 の時点でしか確保できないものである。
//! 現在はトランポリン用フレームだけが該当する。
//!
//! **トランポリン（置く枠と恒等のスタックの予約・雛形・設置・照合）は、[`crate::arch::x86_64::ap_trampoline`] へ
//! 移した**（2026-09-28。境界の段階の手順 2）。**どの AP をいつ起こすかと、起きた AP が本番の世界へ入った後の
//! 共通の部分は、ここに残る。**

use core::sync::atomic::{AtomicU64, Ordering};

use core::fmt::Write as _;

use crate::arch::x86_64::paging::active::{ActivePageTable, PageAttributes};
#[allow(unused_imports)]
use crate::arch::x86_64::paging::verify;
use common::addr::VirtAddr;
use common::arch::x86_64::cpu;
use common::log::Logger;
use common::machine::pc::serial::SerialPort;

use crate::arch::x86_64::{ApBringUp, ApStacks};
use crate::frame_allocator::{FrameAllocator, FRAME_SIZE};

#[cfg(test)]
mod tests {
    use super::*;

    /// `map_ap_stacks` が実際にマップする並びから、通常スタックの頂点を導く。
    ///
    /// 本番のコードではなくテスト側に置いてある。production 側に同じ式を
    /// 2 本持つと片方だけが古くなるので、照合する側にだけ独立に書く。
    /// 並び = ガード + kernel + ガード + IST1 + ガード + IST2（`AP_STACK_STRIDE`）。
    fn kernel_top_from_the_layout(slot: usize) -> u64 {
        AP_STACK_REGION_BASE
            + (slot as u64) * AP_STACK_STRIDE
            + crate::arch::x86_64::stack::GUARD_SIZE as u64
            + crate::arch::x86_64::stack::KERNEL_STACK_SIZE as u64
    }

    /// AP 用アイドルタスクへ記述する範囲が、実際にマップした通常スタックと一致する
    /// （S4-c-3-2a）。
    ///
    /// # なぜホストテストで守るのか
    ///
    /// `schedule_switch` の範囲検査は切り替えが起きたときにしか走らない。
    /// AP 用アイドルタスクでは切り替えが起きないので、この記述が嘘でも実行時
    /// には誰も気づかない（気づくのは将来ここで切り替えが起きたときで、
    /// そのとき初めて落ちる）。実行時に照合されない記述を守れるのは、ここだけ
    /// である。
    #[test]
    fn the_recorded_ap_kernel_stack_range_matches_the_mapped_layout() {
        let slot = 1usize;
        let top = kernel_top_from_the_layout(slot);
        let (bottom, recorded_top) = kernel_stack_bounds_from_top(top);

        assert_eq!(recorded_top, top);
        // 幅はちょうど通常スタック 1 本ぶんで、IST を含んでいない。
        assert_eq!(
            recorded_top - bottom,
            crate::arch::x86_64::stack::KERNEL_STACK_SIZE as u64
        );

        // 下端はガードの穴より上にある。ガードはマップしない穴なので、
        // 範囲がそこへ食い込むと「ガードの上で走ってよい」と記述したことになる。
        let slot_base = AP_STACK_REGION_BASE + (slot as u64) * AP_STACK_STRIDE;
        assert_eq!(
            bottom,
            slot_base + crate::arch::x86_64::stack::GUARD_SIZE as u64
        );

        // IST1 の下端より下にある（範囲が IST へ食い込んでいない）。
        let ist1_bottom = recorded_top + crate::arch::x86_64::stack::GUARD_SIZE as u64;
        assert!(recorded_top <= ist1_bottom);

        // 次のスロットの領域へはみ出していない。
        assert!(recorded_top <= AP_STACK_REGION_BASE + ((slot + 1) as u64) * AP_STACK_STRIDE);

        // 起動ログで実測した値に釘付けする（S4-c-3-2a、`-smp 2`、スロット 1）。
        // 算術が合っていても定数がずれれば動くので、実測値を 1 点持っておく。
        //
        // **P-c-1 でカーネルスタックを 64KiB から 128KiB へ広げたので、
        // 測り直した**（2026-08-28。起動ログの `smp: mapped per-CPU stacks` の行）。
        // **釘は測って打つものなので、算術で導かない。**
        assert_eq!(bottom, 0xffff_8100_0002_c000);
        assert_eq!(recorded_top, 0xffff_8100_0004_c000);
    }
}

/// 起動しうる AP の本数（bootstrap processor を除く）。
const MAX_APS: usize = common::percpu::MAX_CPUS - 1;

/// AP が最初に入る Rust の関数（S3-b-2b-1）。戻らない。
///
/// # ここで触れるものは限られている
///
/// CR3 は静的初期テーブルで、`PML4[256]`（direct map）が無い。したがって
/// direct map 越しに触るものは一切使えない。使えるのは
/// ポート I/O（シリアル）と、高位 VA の静的データである。
///
/// `cpu_id()` を呼ばない。`sgdt` 由来の実装は自コアの GDT がロードされた後
/// でなければ正しくないが、この段階の AP は per-CPU GDT を持たない
/// （`kernel/src/arch/x86_64/gdt/mod.rs` の載荷条件）。身元は引数で受け取る。
///
/// ロックを取らない。`Logger` と `SerialPort` にロックは無いので、
/// BSP が 1 つずつ起動することで混線を避けている（同時に書くとバイトが混ざる）。
#[no_mangle]
pub extern "C" fn zaytos_ap_entry(index: u64) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();
    let _ = writeln!(
        serial,
        "[INFO] smp: application processor {index} started (long mode reached, running on the \
         static boot page table; no per-CPU GDT/TSS/IDT yet, so cpu_id() is not used here)"
    );
    AP_STARTED.fetch_add(1, Ordering::SeqCst);

    // === S3-b-2b-2: 本番の世界へ移る ===
    //
    // ここまでが b-2b-1 の範囲である（恒等 VA の 1 枚のスタック、共有の一時
    // GDT、IDT 無し）。引き継ぎ表があれば、自分の per-CPU 資産を載せて本番 CR3 へ移る。
    if let Some(info) = load_bringup(index as usize) {
        // SAFETY: トランポリンで入った直後で、割り込みは禁止のままである。
        // このコアにつき 1 回だけ呼ぶ。
        unsafe { crate::arch::x86_64::bring_up_application_processor(info, ap_after_switch) }
    }

    let _ = writeln!(
        serial,
        "[WARN] smp: ap {index} has no bring-up information, so it stays on the static boot \
         page table and halts here"
    );
    // 割り込みは有効化しない。IDT を持たないので、来ても行き先が無い。
    cpu::halt_forever()
}

/// 起動署名を出した AP の本数。BSP が会計に使う。
static AP_STARTED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 起動した AP の APIC ID（S5-a）。スロット 1 以降ぶん。`u16` の番兵で
/// 「未設定」を表す（APIC ID は `u8` なので `0` も有効な値である）。
static STARTED_AP_APIC_ID: [core::sync::atomic::AtomicU16; MAX_APS] =
    [const { core::sync::atomic::AtomicU16::new(NO_APIC_ID) }; MAX_APS];

/// 「まだ起こしていない」を表す番兵（S5-a）。
const NO_APIC_ID: u16 = u16::MAX;

/// 探り用ページの仮想アドレス（S5-c）。AP スタックの領域とは別の PML4 の穴に
/// 置く（`PML4[258]` の遥か上）。本番のマッピングと重ならない場所を選ぶ。
#[cfg(feature = "smp-tlb-shootdown-probe")]
const SHOOTDOWN_PROBE_VIRT: u64 = 0xffff_8180_0000_0000;

/// 探り用ページを 1 枚マップする（S5-c）。BSP が起動時、アロケータのある場所で呼ぶ。
///
/// 定常ループにはアロケータが無いので、マップするのはここでしかできない。
/// 外すのは定常ループ側である（`unmap_4kib` はアロケータを要らない）。
///
/// # Safety
///
/// 本番テーブルへ切り替え済みで、direct map ウィンドウが使えること。
#[cfg(feature = "smp-tlb-shootdown-probe")]
pub unsafe fn prepare_shootdown_probe<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut FrameAllocator<CAP>,
) {
    // SAFETY: 呼び出し側の契約。
    let mut table = unsafe { ActivePageTable::current(common::addr::direct_map()) };
    let Some(frame) = allocator.allocate_frame() else {
        logger.error(format_args!("smp: no frame for the shootdown probe page"));
        return;
    };
    let Some(virt) = VirtAddr::new(SHOOTDOWN_PROBE_VIRT) else {
        logger.error(format_args!("smp: the shootdown probe VA is not canonical"));
        return;
    };
    let attributes = PageAttributes {
        user: false,
        writable: true,
        cacheable: true,
        shared: false,
    };
    // SAFETY: 稼働中のテーブルへ、まだ誰も使っていない VA をマップする。
    if let Err(error) = unsafe { table.map_4kib(virt, frame, attributes, allocator) } {
        logger.error(format_args!(
            "smp: could not map the shootdown probe page: {error:?}"
        ));
        return;
    }
    shootdown_probe::set_virt(SHOOTDOWN_PROBE_VIRT);
    logger.info(format_args!(
        "smp: mapped the shootdown probe page at {SHOOTDOWN_PROBE_VIRT:#x} -> {:#x}",
        frame.as_u64()
    ));
}

/// TLB シュートダウンの実証で使う探り用ページ（S5-c）。
///
/// # なぜ 4 段の手順が要るか
///
/// 「AP が触って #PF になる」だけでは差が出ない。AP の TLB にその翻訳が
/// 載っていなければ、シュートダウンを送らない構成でもページテーブルを歩いて
/// #PF になる。両構成が同じ結果になり、比較が消える。
///
/// 手順は次の 4 段である。
///
/// 1. AP がそのアドレスを触る（翻訳を TLB へ載せる）
/// 2. 触れたことを確かめる（載せられなかったら以降の比較は無意味である）
/// 3. bootstrap processor が BKL を保持したままマッピングを外し、世代を上げる
///    （破壊テストの構成では世代を上げない）
/// 4. AP がもう一度触る——世代が上がっていれば次の取得でフラッシュ済みなので
///    #PF、上がっていなければ古い翻訳で成功する
///
/// どちらの側にも「触ったことの積極的な証拠」が要る。「落ちなかった」は
/// 「触っていない」でも満たされる（S5-a で「0 と 0 が一致する」を踏んだのと
/// 同じ形である）。そのため触った回数を数える。
#[cfg(feature = "smp-tlb-shootdown-probe")]
pub mod shootdown_probe {
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    /// 何もしない。
    pub const IDLE: u32 = 0;
    /// 触れ（1 回目。翻訳を TLB へ載せる）。
    pub const TOUCH_FIRST: u32 = 1;
    /// 触れ（2 回目。マッピングを外した後）。
    pub const TOUCH_AGAIN: u32 = 2;

    static COMMAND: AtomicU32 = AtomicU32::new(IDLE);
    static SERVED: AtomicU32 = AtomicU32::new(IDLE);
    static TOUCHES: AtomicU64 = AtomicU64::new(0);
    static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static PROBE_VIRT: AtomicU64 = AtomicU64::new(0);

    /// 探り用ページの仮想アドレスを覚える（BSP が起動時に呼ぶ）。
    pub fn set_virt(virt: u64) {
        PROBE_VIRT.store(virt, Ordering::SeqCst);
    }

    /// 探り用ページの仮想アドレス。未設定なら 0。
    pub fn virt() -> u64 {
        PROBE_VIRT.load(Ordering::SeqCst)
    }

    /// AP へ指示を出す。
    pub fn command(next: u32) {
        COMMAND.store(next, Ordering::SeqCst);
    }

    /// AP が触った回数。これが「触れたことの積極的な証拠」である。
    pub fn touches() -> u64 {
        TOUCHES.load(Ordering::SeqCst)
    }

    /// AP が触ろうとした回数。アクセスの直前に増える。
    ///
    /// # なぜ「触れた回数」だけでは足りないか
    ///
    /// 「2 回目で数が増えなかった」は「触ろうとして触れなかった」と
    /// 「そもそも 2 回目を試みなかった」の両方で成り立つ。AP が段 4 へ
    /// 到達する前に別の理由で死んでいても、触れた回数は 1 のままである。
    /// 試みた側にも積極的な証拠が要る。
    pub fn attempts() -> u64 {
        ATTEMPTS.load(Ordering::SeqCst)
    }

    /// AP 側。指示があれば触って数える。戻り値は触ったかどうか。
    ///
    /// # Safety
    ///
    /// `PROBE_VIRT` がマップ済みであること（外された後に呼ぶと #PF になる。
    /// それがこの探りの目的である）。
    pub unsafe fn service() {
        let cmd = COMMAND.load(Ordering::SeqCst);
        if cmd == IDLE || SERVED.load(Ordering::SeqCst) == cmd {
            return;
        }
        let virt = PROBE_VIRT.load(Ordering::SeqCst);
        if virt == 0 {
            return;
        }
        // 触る「前」に試行を数える。ここで #PF になると以降は実行されないので、
        // 試行と成功の差が「触ろうとして触れなかった」の証拠になる。
        ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: 呼び出し側の契約。読み取りのみ。外された後はここで #PF になる。
        let _ = unsafe { core::ptr::read_volatile(virt as *const u64) };
        TOUCHES.fetch_add(1, Ordering::SeqCst);
        SERVED.store(cmd, Ordering::SeqCst);
    }
}

/// 起動した AP の APIC ID を返す（S5-a）。起動していなければ `None`。
pub fn started_ap_apic_id(slot: usize) -> Option<u8> {
    let raw = STARTED_AP_APIC_ID
        .get(slot.checked_sub(1)?)?
        .load(Ordering::SeqCst);
    (raw != NO_APIC_ID).then_some(raw as u8)
}

/// 起動署名を出した AP の本数。
pub fn started_ap_count() -> usize {
    AP_STARTED.load(Ordering::SeqCst)
}

/// AP を起動した結果（S3-b-2b-1）。
pub struct WakeReport {
    /// MADT が報告した使用可能なコア数（bootstrap processor を含む）。
    pub usable: usize,
    /// 起動しようとした AP の本数。
    pub attempted: usize,
    /// 起動署名を出した AP の本数。
    pub started: usize,
    /// `MAX_CPUS` を超えるので起動しなかった AP の本数。
    pub skipped_no_slot: usize,
}

/// AP を起動する（S3-b-2b-1）。1 本ずつ起動し、次へ進む前に完了を待つ。
///
/// # なぜ 1 本ずつなのか
///
/// `Logger` と `SerialPort` にロックが無いので、2 つ以上の AP が同時に書くと
/// バイトが混ざる。そしてどの AP が失敗したかを切り分けられなくなる。
/// 直列にする費用はコア数 × 10ms 程度で、実害が無い。
///
/// # 起動する本数は `MAX_CPUS` で制限する
///
/// `MAX_CPUS` を超えるコアは起こさない（`roadmap.md` の S3-b-2b-1）。
/// 超えた分を起動すると、per-CPU スロットを持てない AP が生まれる。
/// 起動しなかった本数を返して、検査がそれを主張できるようにする。
///
/// # 待ち時間
///
/// INIT の後に 10ms、SIPI の後に 10ms 待つ。規格は SIPI の後 200µs だが、
/// タイマのティックが 10ms 粒度なのでそれで代用する（下限より長いだけなので
/// 安全側である。TSC は較正していないので新しい時間源を導入しない）。
///
/// # Safety
///
/// - `mapped` がマップ済みの Local APIC を指すこと。
/// - タイマが動いていること（10ms の待ちをティックのエッジで作る）。
/// - 起動時に 1 回だけ呼ぶこと。
pub unsafe fn wake_application_processors(
    logger: &mut Logger<SerialPort>,
    mapped: &crate::apic::MappedApic,
    mmio: &crate::acpi::ApicMmio,
) -> WakeReport {
    let lapic_virt = crate::apic::lapic_virt_of(mapped);
    let usable = mmio.usable_local_apics();

    let Some(frame) = crate::arch::x86_64::trampoline_frame() else {
        logger.error(format_args!(
            "smp: no AP trampoline frame was reserved, so no AP can be started; halting \
             (S1 reserved this frame and S3-b-2b-1 makes the failure fatal)"
        ));
        cpu::halt_forever();
    };

    // トランポリンを予約フレームへコピーし、絶対値を書き込む。
    // SAFETY: frame は S1 が予約した 4KiB 境界の物理フレームで、他の誰も使わない。
    // BSP は本番 CR3 で走るので direct map 越しに触る（AP は恒等で触る）。
    let installed =
        unsafe { crate::arch::x86_64::install_trampoline(logger, frame, zaytos_ap_entry) };

    // この値は BSP の ID とは限らない。MADT の最初の使用可能な Local APIC
    // エントリであって、エントリ順が BSP を先頭にする保証は仕様に無い
    // （[`crate::acpi::ApicMmio::bsp_candidate_apic_id`] の doc）。BSP が先頭で
    // ない実装では、下の `continue` が BSP を素通りさせ、BSP 自身へ INIT-SIPI を
    // 送ることになる。
    //
    // 権威のある出所は 2 つあり、どちらも既に読んでいる——`IA32_APIC_BASE` の
    // bit 8（`common::arch::x86_64::cpu::ApicBase::bootstrap_processor`）と、自コアの Local APIC
    // ID レジスタ（[`crate::apic`] が読んでいる）である。どちらも今はログへ出す
    // だけで、判定には使っていない。
    //
    // 直さない判断と解禁条件は `docs/deferred-decisions.md` にある。要点は、
    // QEMU で MADT の並びを変える手段が無く、破壊テストでの確認を構成できないことである。
    let bsp = mmio.bsp_candidate_apic_id();
    let mut report = WakeReport {
        usable,
        attempted: 0,
        started: 0,
        skipped_no_slot: 0,
    };

    // bootstrap processor を除いた AP を、MADT の並び順で起動する。
    let mut slot = 1usize;
    for apic_id in mmio.local_apic_ids() {
        if Some(apic_id) == bsp {
            continue;
        }
        if slot >= common::percpu::MAX_CPUS {
            report.skipped_no_slot += 1;
            logger.warn(format_args!(
                "smp: not starting the application processor with apic id {apic_id}: there are \
                 only {} per-CPU slot(s) and slot {slot} would be out of range. This is the \
                 documented policy (do not start more CPUs than MAX_CPUS)",
                common::percpu::MAX_CPUS
            ));
            continue;
        }

        let Some(stack) = crate::arch::x86_64::ap_stack_frame(slot - 1) else {
            logger.error(format_args!(
                "smp: no stack frame was reserved for application processor slot {slot}; halting"
            ));
            cpu::halt_forever();
        };

        // スタック頂点は恒等 VA である。静的初期テーブルに direct map が
        // 無いので、direct map の VA を渡すと最初の push で落ちる。
        let stack_top_identity = stack.as_u64() + FRAME_SIZE;
        // SAFETY: installed はコピー済みのトランポリンで、data ブロックの位置は
        // レイアウト定数で決まっている。AP はまだ走っていない。
        unsafe { installed.set_ap_parameters(stack_top_identity, slot as u64) };

        report.attempted += 1;
        STARTED_AP_APIC_ID[slot - 1].store(u16::from(apic_id), Ordering::SeqCst);
        let before = started_ap_count();
        logger.info(format_args!(
            "smp: starting application processor apic id {apic_id} as slot {slot} \
             (trampoline at {:#x}, vector {:#04x}, stack top {:#x} identity-mapped, \
             cr3 {:#x} = the static boot page table)",
            frame.as_u64(),
            installed.sipi_vector,
            stack_top_identity,
            installed.cr3
        ));

        // INIT → 待つ → SIPI → 待つ → まだ起動していなければもう 1 回 SIPI。
        //
        // 2 回目を無条件に送ってはならない。既に走り出した AP へ SIPI を
        // 送ると、long mode で走っている最中に開始ベクタから再実行させる
        // ことになり、16 ビットのバイト列を 64 ビットとして解釈して #GP →
        // トリプルフォルトする。実際に踏んだ（CPU 1 が CS64・GDTR=0 で
        // オフセット 0x15 に落ちた）。規格が 2 回目を許すのは「1 回目が
        // 届かなかった場合」であって、常に 2 回送れという意味ではない。
        // SAFETY: マップ済みの Local APIC。起動時の 1 回だけ。
        let ok = unsafe {
            crate::apic::send_init_ipi(lapic_virt, apic_id) && {
                wait_ticks(AP_WAKE_WAIT_TICKS);
                crate::apic::send_startup_ipi(lapic_virt, apic_id, installed.sipi_vector)
            }
        };
        wait_ticks(AP_WAKE_WAIT_TICKS);
        let ok = ok
            && (started_ap_count() > before || {
                // SAFETY: 同上。まだ起動していないときだけ送る。
                unsafe { crate::apic::send_startup_ipi(lapic_virt, apic_id, installed.sipi_vector) }
            });
        if !ok {
            logger.error(format_args!(
                "smp: an IPI to apic id {apic_id} never left the local APIC (delivery status \
                 stayed set); halting"
            ));
            cpu::halt_forever();
        }

        // 起動署名を待つ。上限つきで待つ（CLAUDE.md の「シェルコマンドの制約」）。
        let mut started = false;
        for _ in 0..AP_START_WAIT_TICKS {
            wait_ticks(1);
            if started_ap_count() > before {
                started = true;
                break;
            }
        }
        if started {
            report.started += 1;
        } else {
            logger.error(format_args!(
                "smp: application processor apic id {apic_id} did not report its start signature \
                 within {AP_START_WAIT_TICKS} tick(s)"
            ));
        }
        slot += 1;
    }

    report
}

/// シリアルの排他の演習で、各コアが書く行数。
///
/// **混線は稀である**（実測で 8 回に 3 回。S4-b-4 では 5 回に 1 回）。
/// **稀な事象を判定にするには、確実にする必要がある**——**2 コアが同時に
/// 何百行も書けば、ロックが無ければ必ず混ざる。**
///
/// **本数は測って決めた**（`docs/verification-coverage.md` の「シリアルの排他」）。
#[cfg(feature = "serial-stress-test")]
pub const SERIAL_STRESS_LINES: u32 = 200;

/// 演習の合図。**BSP が立て、AP が待つ。**
///
/// **揃えないと重ならない**——**AP が先に書き終えてしまえば、ロックが無くても
/// 混ざらない。**
#[cfg(feature = "serial-stress-test")]
pub static SERIAL_STRESS_GO: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// AP が演習の行を書き終えたか。**BSP が待つ。**
#[cfg(feature = "serial-stress-test")]
pub static SERIAL_STRESS_AP_DONE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 演習の行の本体。**両コアが同じ形で書く。** **cpu と番号だけが違う。**
///
/// **詰め物を付ける**——**短い行は、混ざっても形が壊れにくい。**
#[cfg(feature = "serial-stress-test")]
pub const SERIAL_STRESS_PADDING: &str = "........................................................";

/// 演習の AP 側。**合図を待って書き、終わりを報せる。**
#[cfg(feature = "serial-stress-test")]
fn run_serial_stress_on_ap(serial: &mut SerialPort, slot: usize) {
    use core::sync::atomic::Ordering;

    // **上限つきで待つ**（`CLAUDE.md` の「上限のない待機ループを書かない」）。
    let started = common::arch::x86_64::cpu::read_timestamp_counter();
    while !SERIAL_STRESS_GO.load(Ordering::Acquire) {
        if common::arch::x86_64::cpu::read_timestamp_counter().wrapping_sub(started)
            > WAIT_TIMEOUT_CYCLES
        {
            let _ = writeln!(
                serial,
                "[ERROR] serial-stress: ap {slot} never saw the go signal; the exercise asserts \
                 nothing"
            );
            return;
        }
        core::hint::spin_loop();
    }
    for index in 0..SERIAL_STRESS_LINES {
        let _ = writeln!(
            serial,
            "[INFO] serial-stress: cpu{slot} {index:04} {SERIAL_STRESS_PADDING}"
        );
    }
    SERIAL_STRESS_AP_DONE.store(true, Ordering::Release);
}

/// 演習が待てる上限（TSC のサイクル）。**BKL と同じ桁にしてある。**
#[cfg(feature = "serial-stress-test")]
const WAIT_TIMEOUT_CYCLES: u64 = 20_000_000_000;

/// AP を起動するときの各段の待ちティック数。1 ティック = 10ms（100Hz）。
const AP_WAKE_WAIT_TICKS: u64 = 1;
/// 起動署名を待つ上限（ティック）。
const AP_START_WAIT_TICKS: u64 = 50;

/// タイマのティックが `count` 回進むまで待つ。
///
/// 上限のない待ちにならない。ティックが止まっていれば進まないが、
/// 呼び出し側が回数で上限を持つ。
fn wait_ticks(count: u64) {
    let start = crate::arch::x86_64::idt::timer_ticks();
    while crate::arch::x86_64::idt::timer_ticks().wrapping_sub(start) < count {
        core::hint::spin_loop();
    }
}

// ===========================================================================
// S3-b-2b-2: AP の per-CPU スタックを PML4[258] へマップする
// ===========================================================================

/// AP の per-CPU スタックを置く仮想アドレス空間の先頭（`PML4[258]`）。
///
/// # なぜ `PML4[257]` ではないのか
///
/// `[257..510]` は SMP の per-CPU 用に温存してきた範囲で、ここがその目的どおりの
/// 初使用である。しかし `PML4[257]`（`0xffff808000000000`）は使えない。
/// あれは破壊テストの feature `highhalf-remove-verify-fail` のサボタージュ VA そのもので、
/// あの破壊テストは「そこが空であること」に依存している。使うと破壊テストが静かに意味を失う
/// （`docs/verification-coverage.md` と `docs/deferred-decisions.md` の 2 箇所に
/// 警告がある）。サボタージュ VA を移さずに済むほうを選んだ。
///
/// # なぜ静的配列にしないのか
///
/// `StackBlock` は 108.0 KiB で、静的に二重化すると `MAX_CPUS = 4` で 2MiB 境界を
/// 越える（`common/src/percpu.rs` の `MAX_CPUS` の doc）。フレームアロケータから
/// 取ってマップすればイメージが増えない。
const AP_STACK_REGION_BASE: u64 = 0xffff_8100_0000_0000;

/// 1 コアぶんのスタック領域の大きさ。BSP の `StackBlock` と同じ構成にする。
///
/// ガード（4KiB）+ kernel（64KiB）+ ガード + IST1（16KiB）+ ガード + IST2（16KiB）。
/// ガードは各スタックの下に置く（スタックは下へ伸びるので、溢れると下のガードに
/// 当たる）。BSP の `StackBlock` と同じ並びである。
const AP_STACK_STRIDE: u64 = (crate::arch::x86_64::stack::GUARD_SIZE
    + crate::arch::x86_64::stack::KERNEL_STACK_SIZE
    + crate::arch::x86_64::stack::GUARD_SIZE
    + crate::arch::x86_64::stack::IST_STACK_SIZE
    + crate::arch::x86_64::stack::GUARD_SIZE
    + crate::arch::x86_64::stack::IST_STACK_SIZE) as u64;

/// AP 用スタックをマップする（S3-b-2b-2）。
///
/// # ガードページはマップせずに「開けておく」
///
/// 3 本のスタックの下に 1 ページずつ、マップしない穴を残す。BSP 側は静的配置の
/// 上で `unmap_4kib` して穴を開けているが、こちらは最初からマップしないので
/// 分割も解除も要らない。direct map（2MiB ページ）に手を入れずに済むのが、
/// この置き方を選んだ理由の 1 つである。
///
/// # Safety
///
/// 起動時の単一文脈から、AP を起動する前に呼ぶこと。
pub unsafe fn map_ap_stacks<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    slot: usize,
    allocator: &mut FrameAllocator<CAP>,
) -> Option<ApStacks> {
    let base = AP_STACK_REGION_BASE + (slot as u64) * AP_STACK_STRIDE;
    // SAFETY: CR3 は本番テーブルを指しており、その配下は direct map ウィンドウから
    // 読み書きできる。起動時の単一文脈で、AP はまだ走っていない。
    let mut table = unsafe { ActivePageTable::current(common::addr::direct_map()) };

    // (ガードのページ数, 本体のバイト数) を下から順に。
    let layout = [
        crate::arch::x86_64::stack::KERNEL_STACK_SIZE as u64,
        crate::arch::x86_64::stack::IST_STACK_SIZE as u64,
        crate::arch::x86_64::stack::IST_STACK_SIZE as u64,
    ];

    let mut cursor = base;
    let mut tops = [0u64; 3];
    let free_before = allocator.free_frame_count();
    for (index, size) in layout.iter().enumerate() {
        // ガードぶんを空けたまま進める（マップしないので穴になる）。
        cursor += crate::arch::x86_64::stack::GUARD_SIZE as u64;
        let bottom = cursor;
        let mut offset = 0;
        while offset < *size {
            let Some(frame) = allocator.allocate_frame() else {
                logger.error(format_args!(
                    "smp: ran out of frames while mapping the per-CPU stacks for slot {slot}"
                ));
                return None;
            };
            let virt = VirtAddr::new(bottom + offset)?;
            // user=false, writable=true, cacheable=true（通常のカーネルメモリ）。
            let attributes = PageAttributes {
                user: false,
                writable: true,
                cacheable: true,
                shared: false,
            };
            // SAFETY: 稼働中のテーブルへ、まだ誰も使っていない VA をマップする。
            if let Err(error) = unsafe { table.map_4kib(virt, frame, attributes, allocator) } {
                logger.error(format_args!(
                    "smp: could not map the per-CPU stack page at {:#x} for slot {slot}: \
                     {error:?}",
                    virt.as_u64()
                ));
                return None;
            }
            offset += crate::frame_allocator::FRAME_SIZE;
        }
        cursor = bottom + *size;
        tops[index] = cursor;
    }
    let free_after = allocator.free_frame_count();

    logger.info(format_args!(
        "smp: mapped per-CPU stacks for slot {slot} at {base:#x} (PML4[258], not [257] which a \
         sabotage VA depends on): kernel top {:#x}, IST1 top {:#x}, IST2 top {:#x}; \
         {} frame(s) consumed (pages plus page tables), guards left unmapped",
        tops[0],
        tops[1],
        tops[2],
        free_before - free_after
    ));

    Some(ApStacks {
        kernel_top: tops[0],
        double_fault_top: tops[1],
        page_fault_top: tops[2],
    })
}

/// BSP が各スロットぶん用意する引き継ぎ表。AP が自分のスロットを読む。
static AP_BRINGUP: [AtomicU64; MAX_APS * 4] = [const { AtomicU64::new(0) }; MAX_APS * 4];

/// 引き継ぎ表へ書く（BSP 側）。
fn store_bringup(slot: usize, info: &ApBringUp) {
    let base = (slot - 1) * 4;
    AP_BRINGUP[base].store(info.production_cr3, Ordering::SeqCst);
    AP_BRINGUP[base + 1].store(info.stacks.kernel_top, Ordering::SeqCst);
    AP_BRINGUP[base + 2].store(info.stacks.double_fault_top, Ordering::SeqCst);
    AP_BRINGUP[base + 3].store(info.stacks.page_fault_top, Ordering::SeqCst);
}

/// AP（`slot`）の通常カーネルスタックの範囲を返す（S4-c-3-2a）。
///
/// `prepare_ap_per_cpu` の後でだけ意味を持つ。それ以前は引き継ぎ表が
/// 空なので `None` を返す。
///
/// 用途は `task::init_ap_idle_task` で、AP 用アイドルタスクが実際に走る
/// スタックを `Task` に記述するためである。IST は含めない——
/// `schedule_switch` の範囲検査が見るのは通常スタックだけである。
///
/// # IST を含めなくてよい根拠
///
/// タイマのベクタは IST を使わない。`idt::init` が IST を割り当てるのは
/// ベクタ 8（#DF）と 14（#PF）だけで、他はすべて `None` である。
/// Ring 0 から Ring 0 への割り込みではスタックが切り替わらないので、
/// AP がタイマで入ったときの `rsp` は、この通常スタックの内側にある。
/// したがって `schedule_switch` が保存する値も範囲の内側に入る。
/// IST を使うベクタが増えたら、この根拠は失効する。
pub fn ap_kernel_stack_range(slot: usize) -> Option<(u64, u64)> {
    Some(kernel_stack_bounds_from_top(
        load_bringup(slot)?.stacks.kernel_top,
    ))
}

/// 通常カーネルスタックの頂点から `[下端, 頂点)` を導く（S4-c-3-2a）。
///
/// 純粋な算術として切り出してある。この範囲は
/// `schedule_switch` の範囲検査が使うが、AP 用アイドルタスクでは
/// 切り替えが起きないので実行時には照合されない（`task::init_ap_idle_task`）。
/// 実行時に照合されない記述なので、誤りを捕まえられるのはホストテストだけである。
const fn kernel_stack_bounds_from_top(kernel_top: u64) -> (u64, u64) {
    (
        kernel_top - crate::arch::x86_64::stack::KERNEL_STACK_SIZE as u64,
        kernel_top,
    )
}

/// 引き継ぎ表から読む（AP 側）。
fn load_bringup(slot: usize) -> Option<ApBringUp> {
    let base = (slot - 1) * 4;
    let cr3 = AP_BRINGUP.get(base)?.load(Ordering::SeqCst);
    if cr3 == 0 {
        return None;
    }
    Some(ApBringUp {
        production_cr3: cr3,
        stacks: ApStacks {
            kernel_top: AP_BRINGUP[base + 1].load(Ordering::SeqCst),
            double_fault_top: AP_BRINGUP[base + 2].load(Ordering::SeqCst),
            page_fault_top: AP_BRINGUP[base + 3].load(Ordering::SeqCst),
        },
        slot,
    })
}

/// 本番 CR3 と per-CPU スタックへ移った後の AP（S3-b-2b-2）。戻らない。
extern "C" fn ap_after_switch(slot: usize) -> ! {
    let mut serial = SerialPort::new(SerialPort::COM1_BASE);
    serial.init();

    // 恒等が無いことを AP 側で読み戻す（b-2b-1 から移した到達条件）。
    // SAFETY: 稼働中のテーブルを読むだけ。
    let cr3_phys = crate::arch::x86_64::paging::switch::active_page_table_root();
    let cr3 = cr3_phys.as_u64();
    // SAFETY: 稼働中のテーブルを direct map 越しに読むだけ（本番テーブルには
    // direct map がある）。読み取りのみ。
    let pml4_0 = unsafe {
        crate::arch::x86_64::paging::verify::read_pml4_entry(
            cr3_phys,
            common::addr::direct_map(),
            0,
        )
    };
    let identity_gone = pml4_0 & 1 == 0;

    let _ = writeln!(
        serial,
        "[INFO] smp: ap {slot} switched to the production page table (cr3={cr3:#x}) and its own \
         per-CPU stack; PML4[0] read back from this core = empty:{identity_gone} (the identity \
         mapping is gone here too, so the trampoline's identity VA stack is no longer usable)"
    );
    if !identity_gone {
        let _ = writeln!(
            serial,
            "[ERROR] smp: ap {slot} still sees an identity mapping in the production table; \
             halting"
        );
        cpu::halt_forever();
    }

    // 破壊テスト (S3-b-2b-2, smp-ap-touch-scheduler): AP からスケジューラの現在タスクを
    // 読む。この段階は AP でタスクを実行しないので、sentinel を読んで落ちるのが
    // 正しい。丸めていたら「タスク 0 が走っている」と静かに答えていた。
    #[cfg(feature = "smp-ap-touch-scheduler-test")]
    {
        let _ = writeln!(
            serial,
            "[INFO] smp: ap {slot} is about to read the scheduler's current task (sabotage)"
        );
        crate::task::debug_read_current_index();
    }

    // **自分の CR0・CR4・EFER を控える**（2026-09-24）。**BSP が起床のまとめの後で突き合わせる。**
    crate::arch::x86_64::cpu_state::record_this_ap(slot);
    AP_BROUGHT_UP.fetch_add(1, Ordering::SeqCst);

    // === S4-c-3-2b: このコアの `CURRENT` を sentinel から解く ===
    //
    // `start_local_timer` より前でなければならない。あちらは戻らず、その先で
    // `sti` する。そこを過ぎると、このコアはいつでもタイマを受ける。sentinel
    // のまま受けると `current_index()` が sentinel を読んで停止する。
    //
    // BKL をここで取る。`CURRENT` は共有物で、書く時点で bootstrap processor
    // が走っている。これが `KernelEntry::ApBringUp` の唯一の取得箇所であり、
    // 列挙にあって取得箇所が無い状態がここで解消する。
    //
    // 取る区間は書き込みだけに絞る。この関数はシリアルを直に使っており、
    // そこは BKL の外のままである（S6 のログ規約の許可リストの対象であって、
    // この段階の対象ではない）。
    //
    // 破壊テスト (S4-c-3-2b, smp-ap-no-sentinel-clear): この解除を落とす。AP は最初の
    // ティックで sentinel を読んで停止する。S4-a の `smp-ap-enter-scheduler`
    // から役目を引き継いだ破壊テストである。
    #[cfg(not(feature = "smp-ap-no-sentinel-clear"))]
    {
        let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::ApBringUp);
        crate::task::adopt_idle_task_on_this_cpu();
    }

    let _ = writeln!(
        serial,
        "[INFO] smp: ap {slot} is up with its own per-CPU state (own GDT/TSS/IDT, own stacks \
         in PML4[258], production CR3); it now takes part in scheduling on its own idle task"
    );

    // **シリアルの排他の演習（AP 側）。** **合図を待ってから、既知の行を
    // [`SERIAL_STRESS_LINES`] 本書く。** **BSP も同時に書く。**
    #[cfg(feature = "serial-stress-test")]
    run_serial_stress_on_ap(&mut serial, slot);

    // 破壊テスト (S4-c-4-1, smp-ap-runs-preemptive-demo): AP にデモを呼ばせる。
    //
    // tripwire の機序の直接観測である。`require_bootstrap_processor` は
    // `run_preemptive_demo` の入口にあり、ワーカーの継続では鳴らない。開始で
    // だけ鳴る。呼び出しは 2 箇所しか無く（協調デモとプリエンプティブデモの
    // 開始）、本番経路では bootstrap processor しか通らないので、この tripwire
    // は一度も踏まれていない。踏ませて、実際に停止することを見る。
    //
    // 止まるのは入口である。`require_bootstrap_processor` が
    // `halt_forever` するので、`setup_preemptive_tasks` へは到達しない。
    // したがってワーカーは `Ready` にならず、二重選択のウィンドウも生まれない。
    // ウィンドウが要るのは S4-c-4-2 で、あちらはこの tripwire を外した構成である
    // （`docs/verification-coverage.md`。同じ起動では両立しない——
    // 一方は tripwire が在ることを、他方は無いことを要求する）。
    //
    // タイマを開ける前に置く。`start_local_timer` は戻らない。
    #[cfg(feature = "smp-ap-runs-preemptive-demo")]
    {
        let _ = writeln!(
            serial,
            "[INFO] smp: ap {slot} is about to call the preemptive demo (sabotage); the \
             bootstrap-processor tripwire at its entry must stop this core"
        );
        crate::task::run_preemptive_demo();
        let _ = writeln!(
            serial,
            "[ERROR] smp: ap {slot} returned from the preemptive demo; the tripwire did not \
             fire; halting"
        );
        cpu::halt_forever();
    }

    // === S4-a: 自分の Local APIC とタイマを開ける ===
    // SAFETY: 自コアの単一文脈で、割り込みはまだ禁止されている。
    unsafe { start_local_timer(&mut serial, slot) }
}

/// AP が自分の Local APIC タイマを開けて定常ループへ入る（S4-a）。戻らない。
///
/// # SVR は BSP の設定を引き継がない
///
/// `apic::set_spurious_vector` は BSP の Local APIC にしか効いていない。
/// SVR はコアごとにあるので、AP は自分で書く。bit 8（ソフトウェア有効化）が
/// 落ちていると LVT が 1 本も届かないので、書いた後に読み戻して確かめる。
///
/// # 較正はやり直さない。それは仮定である
///
/// BSP が測った分周と初期カウントをそのまま自分の LVT へ書く。これは
/// 「Local APIC タイマの周波数がコア間で同じ」という仮定である。
/// 仮定なので、AP 側のティックのレートをホストの実時間と突き合わせて実測検証
/// する（`lapic-timer-test` と同型の独立基準）。仮定が崩れる環境ではそこで検出される。
///
/// # Safety
///
/// 自コアの GDT / TSS / IDT が載っており、本番 CR3 と per-CPU スタックへ
/// 移った後であること。割り込みが禁止されていること。各コアにつき 1 回だけ。
unsafe fn start_local_timer(serial: &mut SerialPort, slot: usize) -> ! {
    // 1. 自分の Local APIC を有効にする。
    //
    // 破壊テスト (S4-a, smp-ap-timer-no-svr): ここを飛ばす。BSP が書いた SVR は
    // このコアには効いていないので、ティックが 1 本も来ない。
    #[cfg(not(feature = "smp-ap-timer-no-svr-test"))]
    {
        // SAFETY: 自コアの単一文脈で、割り込みは禁止されている。
        match unsafe { crate::irq::enable_local_apic_for_this_cpu() } {
            Some(enable) => {
                let _ = writeln!(
                    serial,
                    "[INFO] smp: ap {slot} wrote its own SVR: spurious vector {:#04x}, \
                     software_enabled={} (the BSP's write only reached the BSP's local APIC)",
                    enable.spurious_vector(),
                    enable.software_enabled()
                );
                if !enable.software_enabled() {
                    let _ = writeln!(
                        serial,
                        "[ERROR] smp: ap {slot} has its local APIC software-disabled, so no LVT \
                         interrupt can be delivered; halting"
                    );
                    cpu::halt_forever();
                }
            }
            None => {
                let _ = writeln!(
                    serial,
                    "[ERROR] smp: ap {slot} could not reach its local APIC to write the SVR; \
                     halting"
                );
                cpu::halt_forever();
            }
        }
    }
    #[cfg(feature = "smp-ap-timer-no-svr-test")]
    let _ = writeln!(
        serial,
        "[WARN] smp: ap {slot} is skipping its own SVR write (sabotage)"
    );

    // 2. 自分の LVT Timer を、BSP と同じ設定で開ける。
    // SAFETY: 自コアの IDT は載っており、LAPIC_TIMER_VECTOR には戻れるハンドラが
    // ある。割り込みはまだ禁止されているので、`sti` するまでは届かない。
    match unsafe { crate::irq::arm_lapic_timer_for_this_cpu() } {
        Some((divide, initial_count)) => {
            let _ = writeln!(
                serial,
                "[INFO] smp: ap {slot} armed its own LAPIC timer with the BSP's calibration \
                 (divide configuration {divide:#x}, initial count {initial_count}); sharing the \
                 calibration ASSUMES the LAPIC timer frequency is the same on every core, and \
                 that assumption is checked against host wall-clock time, not from inside"
            );
        }
        None => {
            let _ = writeln!(
                serial,
                "[ERROR] smp: ap {slot} could not arm its LAPIC timer (the BSP has not moved the \
                 timer to the local APIC yet); halting"
            );
            cpu::halt_forever();
        }
    }

    // 3. 割り込みを有効にして定常ループへ入る。
    //
    // S3 ではここが `cli; hlt` だった。BKL が無いので AP は待つだけで、
    // 割り込みを有効化しなかった。S4-a で前提が変わる。
    //
    // BKL はまだ無い。この段階の安全は「AP のハンドラが触るものが per-CPU か
    // アトミックだけである」ことに依存する条件つきのものである（`roadmap.md` の
    // S4-a に一覧がある）。S4-b で BKL が入れば、この一覧は不要になる。
    // 破壊テスト (S4-b-4, bkl-hold-forever): AP が BKL を取ったまま二度と離さない。
    // BSP がタイムアウトして原因を出す。再帰検出ではなく待ちの上限を通す
    // 唯一の形である（既存の 2 つの破壊テストはどちらも同じコアが取り直すので再帰が先に鳴る）。
    #[cfg(feature = "bkl-hold-forever-test")]
    crate::bkl::sabotage_hold_forever();

    #[cfg(not(feature = "bkl-hold-forever-test"))]
    ap_heartbeat_loop(serial, slot)
}

/// AP の定常ループ（S4-a）。戻らない。
///
/// # `sti; hlt` の隣接
///
/// BSP の `run_timer_loop` と同じく `cpu::enable_interrupts_and_halt` を使う。
/// このループは眠るかどうかを条件で決めないので、条件確認と `hlt` の間で
/// 仕事を取りこぼす形にならない（あちらの doc と同じ理由である）。
///
/// # ログの規約
///
/// BKL の外からシリアルへ書く。シリアルにもロガーにもロックが無いので、
/// BSP の出力と混線しうる。行頭にコア番号を必ず置くことで、混ざっても
/// どのコアの行かが分かるようにしてある。行の途中で混ざることは防げない。
#[cfg_attr(feature = "bkl-hold-forever-test", allow(dead_code))]
fn ap_heartbeat_loop(serial: &mut SerialPort, slot: usize) -> ! {
    let mut next_heartbeat = crate::interrupts::HEARTBEAT_TICKS;
    loop {
        let ticks = crate::arch::x86_64::idt::timer_ticks_for(slot);
        // **観測が完了していれば出さない（S11-11）。** BSP がシェルへ渡した後も
        // 出し続けると、**起動ログの長さが実時間に依存する。**
        if ticks >= next_heartbeat && !crate::interrupts::steady_observation_is_closed() {
            next_heartbeat = ticks + crate::interrupts::HEARTBEAT_TICKS;
            let _ = writeln!(
                serial,
                "[INFO] smp: ap heartbeat: cpu={slot} ticks={ticks} tsc={}",
                cpu::read_timestamp_counter()
            );
        }
        // TLB シュートダウンの探り（S5-c）。指示があるときだけ触る。
        // SAFETY: 探り用ページは BSP が起動時にマップしている。外された後に触ると
        // #PF になるが、それがこの探りの目的である。
        #[cfg(feature = "smp-tlb-shootdown-probe")]
        unsafe {
            shootdown_probe::service()
        };

        // SAFETY: 自コアの IDT は載っており、タイマのハンドラは EOI を送って戻る。
        // `sti; hlt` が隣接しているので、有効化と停止の間にウィンドウが開かない。
        unsafe {
            cpu::enable_interrupts_and_halt();
        }
    }
}

/// 本番の世界へ移った AP の本数。
static AP_BROUGHT_UP: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 本番の世界へ移った AP の本数。
pub fn brought_up_ap_count() -> usize {
    AP_BROUGHT_UP.load(Ordering::SeqCst)
}

/// AP の per-CPU 資産を用意する（S3-b-2b-2）。BSP が起動最初期に呼ぶ。
///
/// # なぜここで用意するのか
///
/// フレームアロケータと本番テーブルの両方が要る。AP を起動するのは
/// `run_timer_loop` の中だが、そこにはアロケータが無い（トランポリン用フレームと
/// AP スタック用フレームを最初期に予約したのと同じ理由）。
///
/// # Safety
///
/// 起動時の単一文脈から、本番テーブルへ切り替えた後・AP を起動する前に 1 回だけ呼ぶこと。
pub unsafe fn prepare_ap_per_cpu<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut FrameAllocator<CAP>,
) {
    let production_cr3 = crate::arch::x86_64::paging::switch::active_page_table_root().as_u64();
    for slot in 1..common::percpu::MAX_CPUS {
        // SAFETY: 呼び出し元契約。まだ AP は走っていない。
        let Some(stacks) = (unsafe { map_ap_stacks(logger, slot, allocator) }) else {
            logger.error(format_args!(
                "smp: could not map the per-CPU stacks for slot {slot}; that AP will stay on \
                 the static boot page table"
            ));
            continue;
        };
        store_bringup(
            slot,
            &ApBringUp {
                production_cr3,
                stacks,
                slot,
            },
        );
    }
}
