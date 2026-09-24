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
//! # 見ているのは BSP だけである
//!
//! **AP は同じ値へ揃える前提で、ここでは読まない**（限界。`ADR-0018` の Addendum 9）。

use common::log::Logger;
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

/// CR0・CR4・EFER を読み、1 行出し、棚卸しを崩すビットが立っていれば止める。
pub fn check_and_report(logger: &mut Logger<SerialPort>) {
    let cr0 = common::cpu::read_cr0();
    let cr4 = common::cpu::read_cr4();
    let efer = common::cpu::read_efer().raw();
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

    /// **SMEP と UMIP は棚卸しの結論を崩さない**（起動ログの参照が見る）。
    #[test]
    fn smep_and_umip_do_not_break_the_inventory() {
        assert_eq!(
            inventory_violations(CR0, CR4 | 1 << 20 | 1 << 11, EFER).count(),
            0
        );
    }
}
