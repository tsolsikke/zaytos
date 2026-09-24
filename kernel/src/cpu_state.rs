//! 棚卸しの前提を起動のたびに確かめる（2026-09-24。`ADR-0018` の Addendum 9）。
//!
//! # なぜ要るのか
//!
//! **「ユーザーが変えられて、カーネルが前提にしている CPU の状態」の棚卸しは、いまの形に依って
//! いる**——CR0.AM・CR4.SMAP・CR4.FSGSBASE・CR4.PKE・CR4.OSXSAVE・EFER.SCE が 0 であること。
//! **どれかを立てる変更をすると、棚卸しの結論が黙って偽になる**（通る理由が変わる族）。
//! **ここで起動のたびに読み、立っていたら止める。** **値は起動ログの参照にも載る**
//! ——**棚卸しに関わらないビット（SMEP・UMIP など）が変わっても、参照の突き合わせが落ちる。**
//!
//! # AP も見る（2026-09-24。レビューの足す1点）
//!
//! **AP の CR0・CR4・EFER は、BSP とは別の経路（トランポリン）で作られる。** **INIT の直後の値から
//! 始まり、トランポリンは PAE・LME・PG と PE しか立てない**——**実測で、AP は CD と NW が 1（キャッシュが
//! 効かない形）で、WP と NE が 0 のまま走っていた**（`docs/troubleshooting.md`）。**棚卸しの結論は全 CPU に
//! ついてなので、見張りも全 CPU に要る。**
//!
//! **AP は起きた直後に BSP の値を写し**（[`adopt_bsp_state_on_this_ap`]）、**起動の終わりに自分の値を
//! 読んで控える**（[`record_this_ap`]）。**BSP は、起きた AP の値が自分の値と一致することを確かめ、
//! 食い違えば止まる**（[`check_aps_match_bsp`]）。

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::log::Logger;
use common::percpu::MAX_CPUS;
use common::serial::SerialPort;

/// 棚卸しが 0 であることに依っているビットの在りか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Register {
    Cr0,
    Cr4,
    Efer,
}

/// 棚卸しが 0 であることに依っているビット（在りか・ビット・名前・立てたときに崩れる結論）。
pub const INVENTORY_BITS: [(Register, u32, &str, &str); 6] = [
    (
        Register::Cr0,
        18,
        "CR0.AM",
        "Ring 3 could raise #AC (vector 17), which is not folded, by setting AC",
    ),
    (
        Register::Cr4,
        21,
        "CR4.SMAP",
        "interrupts do not clear AC, so the entries would need clac",
    ),
    (
        Register::Cr4,
        16,
        "CR4.FSGSBASE",
        "Ring 3 could write the FS and GS bases with wrfsbase and wrgsbase",
    ),
    (
        Register::Cr4,
        22,
        "CR4.PKE",
        "Ring 3 could change PKRU with wrpkru",
    ),
    (
        Register::Cr4,
        18,
        "CR4.OSXSAVE",
        "Ring 3 could use AVX state that fxsave does not save",
    ),
    (
        Register::Efer,
        0,
        "EFER.SCE",
        "the syscall instruction would become a 4th entry that skips the IDT stubs",
    ),
];

/// **カーネルのコードが依っているビット**（2026-09-24。レビューの足す1点）——在りか・ビット・名前・
/// あるべき値・崩れたときに困ること。**棚卸しの「0 であるべき」（[`INVENTORY_BITS`]）と対にする**——
/// あちらはユーザーが変えられる状態についての結論が依るビット、こちらはカーネルのコードそのものが
/// 依るビットである。
///
/// # 誰が立てるか
///
/// - **CR0 の WP・NE を立て、CD・NW を落とすのは [`establish_required_bits_on_bsp`] である**（BSP）。
///   **前はファームウェアが良い値で渡していたので動いていただけで、カーネルは自分で決めていなかった**
///   ——**OVMF と VirtualBox の EFI は同じ値で渡すので、2 台の機械では見えない。**
/// - **MP・EM・OSFXSR・OSXMMEXCPT は `fp::enable_on_this_cpu` が持つ**（`ADR-0058`）。
/// - **PE・PG・PAE・LME・LMA は、長モードで走っている時点で立っている**（ブートローダとトランポリン）。
/// - **AP は BSP を丸ごと写す**（[`adopt_bsp_state_on_this_ap`]）。
///
/// # NXE を入れていない理由
///
/// **カーネルのページ表は実行禁止のビット（XD。63 番）を使っていない**（`kernel/src/paging`）。
/// **NXE が 0 なら XD は予約のビットになる**（Intel SDM Vol.3A）が、**立てていないので落ちない。**
/// **実行禁止の保護を入れる段で「1 であるべき」に移す**（`docs/deferred-decisions.md`）。
pub const REQUIRED_BITS: [(Register, u32, &str, bool, &str); 13] = [
    (
        Register::Cr0,
        0,
        "CR0.PE",
        true,
        "the kernel runs in protected and long mode",
    ),
    (
        Register::Cr0,
        1,
        "CR0.MP",
        true,
        "fxsave is used for the user FP state (ADR-0058)",
    ),
    (
        Register::Cr0,
        2,
        "CR0.EM",
        false,
        "SSE instructions would raise #UD",
    ),
    (
        Register::Cr0,
        5,
        "CR0.NE",
        true,
        "x87 errors must arrive as #MF, which is folded",
    ),
    (
        Register::Cr0,
        16,
        "CR0.WP",
        true,
        "read-only pages must stop kernel writes too",
    ),
    (
        Register::Cr0,
        29,
        "CR0.NW",
        false,
        "caching must be the normal write-back kind",
    ),
    (Register::Cr0, 30, "CR0.CD", false, "the caches must be on"),
    (
        Register::Cr0,
        31,
        "CR0.PG",
        true,
        "the kernel runs with paging",
    ),
    (Register::Cr4, 5, "CR4.PAE", true, "4-level paging needs it"),
    (
        Register::Cr4,
        9,
        "CR4.OSFXSR",
        true,
        "fxsave and SSE need it (ADR-0058)",
    ),
    (
        Register::Cr4,
        10,
        "CR4.OSXMMEXCPT",
        true,
        "SIMD errors must arrive as #XM, which is folded",
    ),
    (
        Register::Efer,
        8,
        "EFER.LME",
        true,
        "the kernel runs in long mode",
    ),
    (
        Register::Efer,
        10,
        "EFER.LMA",
        true,
        "the kernel runs in long mode",
    ),
];

/// あるべき値と違うビット（純粋ロジック）。
pub fn required_violations(
    cr0: u64,
    cr4: u64,
    efer: u64,
) -> impl Iterator<Item = &'static (Register, u32, &'static str, bool, &'static str)> {
    REQUIRED_BITS
        .iter()
        .filter(move |(register, bit, _, must_be_set, _)| {
            let value = match register {
                Register::Cr0 => cr0,
                Register::Cr4 => cr4,
                Register::Efer => efer,
            };
            (value & (1u64 << bit) != 0) != *must_be_set
        })
}

/// [`establish_required_bits_on_bsp`] の前後の CR0（起動ログへ出すため）。
static ESTABLISHED_CR0: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// **BSP で、カーネルが要る CR0 のビットを自分で立てる・落とす**（2026-09-24）——**WP と NE を立て、
/// CD と NW を落とす。** **FP のビットは触らない**（`fp::enable_on_this_cpu` が持つ）。
///
/// # Safety
///
/// **起動の最初期に、BSP で 1 回だけ呼ぶこと。** **PE と PG には触れない。** **CD と NW は同時に落とす**
/// （CD が 0 で NW が 1 の組は `#GP` になる）。
pub unsafe fn establish_required_bits_on_bsp() {
    use common::cpu::{
        read_cr0, CR0_CACHE_DISABLE, CR0_NOT_WRITE_THROUGH, CR0_NUMERIC_ERROR, CR0_WRITE_PROTECT,
    };
    let before = read_cr0();
    let after = (before | CR0_WRITE_PROTECT | CR0_NUMERIC_ERROR)
        & !(CR0_CACHE_DISABLE | CR0_NOT_WRITE_THROUGH);
    // 破壊 (2026-09-24, bsp-keeps-cd): **ファームウェアが CD を立てて渡し、カーネルが落とさない形**を
    // 作る。**OVMF と VirtualBox の EFI は CD を落として渡すので、立てて作る。** **見張りが CR0.CD を
    // 名指しして止まる。**
    #[cfg(feature = "bsp-keeps-cd-test")]
    let after = after | CR0_CACHE_DISABLE;
    if after != before {
        // SAFETY: 呼び出し側の契約。PE と PG を保ち、WP・NE・CD・NW だけを変える。
        unsafe { common::cpu::write_cr0(after) };
    }
    ESTABLISHED_CR0[0].store(before, Ordering::SeqCst);
    ESTABLISHED_CR0[1].store(common::cpu::read_cr0(), Ordering::SeqCst);
}

/// [`establish_required_bits_on_bsp`] の前後の CR0 を 1 行出す（ロガーが使えるようになってから）。
pub fn report_established_bits(logger: &mut Logger<SerialPort>) {
    let before = ESTABLISHED_CR0[0].load(Ordering::SeqCst);
    let after = ESTABLISHED_CR0[1].load(Ordering::SeqCst);
    logger.info(format_args!(
        "cpu-state: the kernel set the CR0 bits it needs on the BSP (WP and NE set, CD and NW \
         clear): CR0 {before:#x} -> {after:#x} [read back]"
    ));
}

/// 立っていて棚卸しを崩すビット（純粋ロジック）。
pub fn inventory_violations(
    cr0: u64,
    cr4: u64,
    efer: u64,
) -> impl Iterator<Item = &'static (Register, u32, &'static str, &'static str)> {
    INVENTORY_BITS.iter().filter(move |(register, bit, _, _)| {
        let value = match register {
            Register::Cr0 => cr0,
            Register::Cr4 => cr4,
            Register::Efer => efer,
        };
        value & (1u64 << bit) != 0
    })
}

/// BSP の CR0・CR4・EFER（[`check_and_report`] が控える。**AP が写し、BSP が突き合わせる元**）。
static BSP_STATE: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// BSP の値を控えたか。
static BSP_RECORDED: AtomicBool = AtomicBool::new(false);
/// AP ごとに控えた値（添字はスロット）。
static AP_STATE: [[AtomicU64; 3]; MAX_CPUS] =
    [const { [const { AtomicU64::new(0) }; 3] }; MAX_CPUS];
/// AP ごとに控えたか。
static AP_RECORDED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// BSP が AP の控えを待つ上限（ティック。1 ティック = 10ms）。
const AP_RECORD_WAIT_TICKS: u64 = 200;

fn read_this_cpu() -> [u64; 3] {
    [
        common::cpu::read_cr0(),
        common::cpu::read_cr4(),
        common::cpu::read_efer().raw(),
    ]
}

/// CR0・CR4・EFER を読み、1 行出し、棚卸しを崩すビットが立っていれば止める。**BSP の値を控える。**
pub fn check_and_report(logger: &mut Logger<SerialPort>) {
    let [cr0, cr4, efer] = read_this_cpu();
    for (slot, value) in BSP_STATE.iter().zip([cr0, cr4, efer]) {
        slot.store(value, Ordering::SeqCst);
    }
    BSP_RECORDED.store(true, Ordering::SeqCst);
    // 破壊 (2026-09-24, cpu-state-sees-sce): EFER.SCE が立っているものとして判定する。
    // **MSR は書かない**——**`syscall` 命令が本当に入口になる形は作らない。**
    #[cfg(feature = "cpu-state-sees-sce-test")]
    let efer = efer | common::cpu::Efer::SYSCALL_ENABLE;
    // 破壊 (2026-09-24, kernel-uses-gs): `gs:` を読む関数を像に残す（呼ばない）。
    // **基底の項目（逆アセンブルで `fs:`・`gs:` を数える）が捕まえる。**
    #[cfg(feature = "kernel-uses-gs-test")]
    core::hint::black_box(read_through_gs as fn() -> u64);
    logger.info(format_args!(
        "cpu-state: CR0={cr0:#x} CR4={cr4:#x} EFER={efer:#x}; the inventory of user-changeable CPU \
         state (ADR-0018 Addendum 9) rests on CR0.AM, CR4.SMAP, CR4.FSGSBASE, CR4.PKE, \
         CR4.OSXSAVE and EFER.SCE being 0"
    ));
    let mut violated = false;
    for (_, _, name, must_be_set, needs) in required_violations(cr0, cr4, efer) {
        violated = true;
        logger.error(format_args!(
            "cpu-state: {name} is {}, but the kernel needs it to be {} ({needs}); the bits the \
             kernel needs are set on the BSP by cpu_state::establish_required_bits_on_bsp and the \
             APs copy the BSP",
            u8::from(!*must_be_set),
            u8::from(*must_be_set)
        ));
    }
    if !violated {
        logger.info(format_args!(
            "cpu-state: the {} bit(s) the kernel needs hold (CR0.PE, MP, NE, WP, PG set; EM, NW, CD \
             clear; CR4.PAE, OSFXSR, OSXMMEXCPT set; EFER.LME, LMA set)",
            REQUIRED_BITS.len()
        ));
    }
    for (_, _, name, breaks) in inventory_violations(cr0, cr4, efer) {
        violated = true;
        logger.error(format_args!(
            "cpu-state: {name} is 1, but the inventory rests on it being 0 ({breaks}); redo the \
             inventory in ADR-0018 Addendum 9 before turning it on"
        ));
    }
    if violated {
        logger.error(format_args!("cpu-state: halting"));
        common::cpu::halt_forever();
    }
}

/// AP が BSP の CR0・CR4・EFER を写す（2026-09-24）。**AP の Rust の入口の最初で 1 回だけ呼ぶ。**
///
/// **写す順は CR4 → EFER → CR0 である**——**CR0 で CD と NW を落とし（キャッシュが効く）、WP を立てる**
/// のを最後にする。**BSP の値が控えられていなければ何もしない**（突き合わせが、控えが無いことで止まる）。
///
/// # Safety
///
/// **AP の起動の途中で、長モードに居て、割り込みが禁止されていること。** **BSP の値は同じカーネルの
/// 同じ長モードの値である**（PG・PE・PAE・LME は BSP でも立っている）。
pub unsafe fn adopt_bsp_state_on_this_ap() {
    // 破壊 (2026-09-24, ap-keeps-its-own-control-registers): 写さない。**直す前の形である**——
    // **AP は INIT の直後の CR0（CD・NW が 1、WP・NE が 0）のまま走り、突き合わせで止まる。**
    if cfg!(feature = "ap-keeps-its-own-control-registers-test") {
        return;
    }
    if !BSP_RECORDED.load(Ordering::SeqCst) {
        return;
    }
    let [cr0, cr4, efer] = [0, 1, 2].map(|index| BSP_STATE[index].load(Ordering::SeqCst));
    // SAFETY: 呼び出し側の契約。3 つとも同じカーネルの BSP が長モードで使っている値である。
    unsafe {
        common::cpu::write_cr4(cr4);
        common::cpu::write_efer(common::cpu::Efer::from_raw(efer));
        common::cpu::write_cr0(cr0);
    }
}

/// AP が自分の CR0・CR4・EFER を読んで控える（2026-09-24。起動の終わりに呼ぶ）。
pub fn record_this_ap(slot: usize) {
    if slot >= MAX_CPUS {
        return;
    }
    for (cell, value) in AP_STATE[slot].iter().zip(read_this_cpu()) {
        cell.store(value, Ordering::SeqCst);
    }
    AP_RECORDED[slot].store(true, Ordering::SeqCst);
}

/// ビットの名前（純粋ロジック）。**知らないビットは `None`**（行には番号で出す）。
pub fn bit_name(register: Register, bit: u32) -> Option<&'static str> {
    Some(match (register, bit) {
        (Register::Cr0, 0) => "PE",
        (Register::Cr0, 1) => "MP",
        (Register::Cr0, 2) => "EM",
        (Register::Cr0, 3) => "TS",
        (Register::Cr0, 4) => "ET",
        (Register::Cr0, 5) => "NE",
        (Register::Cr0, 16) => "WP",
        (Register::Cr0, 18) => "AM",
        (Register::Cr0, 29) => "NW",
        (Register::Cr0, 30) => "CD",
        (Register::Cr0, 31) => "PG",
        (Register::Cr4, 2) => "TSD",
        (Register::Cr4, 3) => "DE",
        (Register::Cr4, 4) => "PSE",
        (Register::Cr4, 5) => "PAE",
        (Register::Cr4, 6) => "MCE",
        (Register::Cr4, 7) => "PGE",
        (Register::Cr4, 8) => "PCE",
        (Register::Cr4, 9) => "OSFXSR",
        (Register::Cr4, 10) => "OSXMMEXCPT",
        (Register::Cr4, 11) => "UMIP",
        (Register::Cr4, 16) => "FSGSBASE",
        (Register::Cr4, 17) => "PCIDE",
        (Register::Cr4, 18) => "OSXSAVE",
        (Register::Cr4, 20) => "SMEP",
        (Register::Cr4, 21) => "SMAP",
        (Register::Cr4, 22) => "PKE",
        (Register::Efer, 0) => "SCE",
        (Register::Efer, 8) => "LME",
        (Register::Efer, 10) => "LMA",
        (Register::Efer, 11) => "NXE",
        _ => return None,
    })
}

/// BSP と AP で違うビットを、名前と「AP の側で立っているか」で並べる（行に出すため。確保しない）。
pub struct DifferingBits {
    pub register: Register,
    pub bsp: u64,
    pub ap: u64,
}

impl fmt::Display for DifferingBits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for bit in 0..64u32 {
            let mask = 1u64 << bit;
            if (self.bsp ^ self.ap) & mask == 0 {
                continue;
            }
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            let side = if self.ap & mask != 0 {
                "set on the AP"
            } else {
                "clear on the AP"
            };
            match bit_name(self.register, bit) {
                Some(name) => write!(f, "{name} {side}")?,
                None => write!(f, "bit {bit} {side}")?,
            }
        }
        Ok(())
    }
}

/// **起きた AP の CR0・CR4・EFER が、BSP の値と一致することを確かめる**（2026-09-24）。
/// **食い違えば、違うビットの名前を出して止まる。** **AP の控えは上限つきで待つ。**
pub fn check_aps_match_bsp(logger: &mut Logger<SerialPort>, started: usize) {
    let bsp = [0, 1, 2].map(|index| BSP_STATE[index].load(Ordering::SeqCst));
    let names = ["CR0", "CR4", "EFER"];
    let registers = [Register::Cr0, Register::Cr4, Register::Efer];
    let mut failed = false;
    for slot in 1..=started.min(MAX_CPUS - 1) {
        let start = crate::idt::timer_ticks();
        while !AP_RECORDED[slot].load(Ordering::SeqCst)
            && crate::idt::timer_ticks().wrapping_sub(start) < AP_RECORD_WAIT_TICKS
        {
            core::hint::spin_loop();
        }
        if !AP_RECORDED[slot].load(Ordering::SeqCst) {
            logger.error(format_args!(
                "cpu-state: ap {slot} did not record its CR0, CR4 and EFER within \
                 {AP_RECORD_WAIT_TICKS} tick(s)"
            ));
            failed = true;
            continue;
        }
        let ap = [0, 1, 2].map(|index| AP_STATE[slot][index].load(Ordering::SeqCst));
        if ap == bsp {
            logger.info(format_args!(
                "cpu-state: ap {slot} CR0={:#x} CR4={:#x} EFER={:#x} matches the BSP",
                ap[0], ap[1], ap[2]
            ));
            continue;
        }
        failed = true;
        for index in 0..3 {
            if ap[index] != bsp[index] {
                logger.error(format_args!(
                    "cpu-state: ap {slot} differs from the BSP in {} ({:#x} on the BSP, {:#x} on \
                     the AP): {}; the inventory in ADR-0018 Addendum 9 is about every CPU - redo the \
                     inventory in ADR-0018 Addendum 9",
                    names[index],
                    bsp[index],
                    ap[index],
                    DifferingBits {
                        register: registers[index],
                        bsp: bsp[index],
                        ap: ap[index],
                    }
                ));
            }
        }
    }
    if failed {
        logger.error(format_args!("cpu-state: halting"));
        common::cpu::halt_forever();
    }
    logger.info(format_args!(
        "cpu-state: {started} started AP(s) match the BSP's CR0, CR4 and EFER"
    ));
}

/// 破壊 `kernel-uses-gs-test` の本体。**呼ばない**（像に残すだけ）。
#[cfg(feature = "kernel-uses-gs-test")]
fn read_through_gs() -> u64 {
    let value: u64;
    // SAFETY: 呼ばれない（`check_and_report` が番地を取るだけ）。呼ばれた場合も GS の基底 + 0 を
    // 読むだけで、書かない。
    unsafe {
        core::arch::asm!("mov {}, gs:[0]", out(reg) value, options(nostack, readonly, preserves_flags));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-24 の実測（カーネルが走る間の `-d int` の記録。8535 回とも同じ値）。
    const CR0: u64 = 0x8001_0033;
    const CR4: u64 = 0x668;
    const EFER: u64 = 0xd00;

    #[test]
    fn the_measured_state_breaks_nothing() {
        assert_eq!(inventory_violations(CR0, CR4, EFER).count(), 0);
    }

    #[test]
    fn each_inventory_bit_is_named_when_set() {
        let names = |cr0, cr4, efer| -> Vec<&str> {
            inventory_violations(cr0, cr4, efer)
                .map(|(_, _, name, _)| *name)
                .collect()
        };
        assert_eq!(names(CR0 | 1 << 18, CR4, EFER), vec!["CR0.AM"]);
        assert_eq!(names(CR0, CR4 | 1 << 21, EFER), vec!["CR4.SMAP"]);
        assert_eq!(names(CR0, CR4 | 1 << 16, EFER), vec!["CR4.FSGSBASE"]);
        assert_eq!(names(CR0, CR4 | 1 << 22, EFER), vec!["CR4.PKE"]);
        assert_eq!(names(CR0, CR4 | 1 << 18, EFER), vec!["CR4.OSXSAVE"]);
        assert_eq!(names(CR0, CR4, EFER | 1), vec!["EFER.SCE"]);
    }

    /// 2026-09-24 の実測（QEMU の `-smp 2`。直す前の AP と BSP）で、違うビットを名前で並べる。
    #[test]
    fn the_measured_ap_differences_are_named() {
        let cr0 = DifferingBits {
            register: Register::Cr0,
            bsp: CR0,
            ap: 0xe000_0013,
        };
        assert_eq!(
            format!("{cr0}"),
            "NE clear on the AP, WP clear on the AP, NW set on the AP, CD set on the AP"
        );
        let cr4 = DifferingBits {
            register: Register::Cr4,
            bsp: CR4,
            ap: 0x620,
        };
        assert_eq!(format!("{cr4}"), "DE clear on the AP, MCE clear on the AP");
        let unknown = DifferingBits {
            register: Register::Efer,
            bsp: 0,
            ap: 1 << 13,
        };
        assert_eq!(format!("{unknown}"), "bit 13 set on the AP");
    }

    #[test]
    fn the_measured_state_has_every_required_bit() {
        assert_eq!(required_violations(CR0, CR4, EFER).count(), 0);
    }

    /// **直す前の AP の値**（2026-09-24 の実測）で、崩れていたビットを名前で拾う。
    #[test]
    fn the_measured_ap_before_the_fix_breaks_the_required_bits() {
        let names: Vec<&str> = required_violations(0xe000_0013, 0x620, 0x500)
            .map(|(_, _, name, _, _)| *name)
            .collect();
        assert_eq!(names, vec!["CR0.NE", "CR0.WP", "CR0.NW", "CR0.CD"]);
    }

    /// **SMEP と UMIP は棚卸しの結論を崩さない**（起動ログの参照が見る）。
    #[test]
    fn smep_and_umip_do_not_break_the_inventory() {
        assert_eq!(
            inventory_violations(CR0, CR4 | 1 << 20 | 1 << 11, EFER).count(),
            0
        );
    }
}
