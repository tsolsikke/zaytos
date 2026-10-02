//! 実行禁止のビットを付けた、試し専用のページ（2026-10-02。`ADR-0071` の手順 4）。
//!
//! # 何を確かめるのか
//!
//! **ページテーブルの項目の実行禁止のビット（63 番）は、`EFER.NXE` が 0 の CPU では予約のビットで、立てた項目を
//! 引くと `#PF` になる。** カーネルの写像に実行禁止を入れる前に、**そのビットを持つ項目を、起こす CPU の全部が
//! 引けること**を、実物で確かめる。
//!
//! - BSP が、起動の途中（AP を起こす前）に、このビットを付けたページを 1 枚マップして読む。
//! - AP は、本番の表へ切り替えた後に、同じページを読む（`crate::smp`）。
//! - どちらも、読めた値と、そのときの `EFER.NXE` を起動ログへ出す。
//!
//! **読むだけである。** 実行はしない——実行できないことは、変換が実行の欄を読むようになってから、権限の違反の
//! 破壊テストで見る。ここが見るのは、「このビットを持つ項目を引いても、予約のビットの違反にならない」ことである。

use core::sync::atomic::{AtomicBool, Ordering};

use common::addr::VirtAddr;
use common::arch::x86_64::cpu::{read_efer, Efer};
use common::log::Logger;
use common::machine::pc::serial::Serial;

use super::paging::active::ActivePageTable;
use super::paging::entry;
use crate::frame_allocator::FrameAllocator;

/// 試しのページの仮想アドレス（`PML4[261]`）。**カーネルの側の、ほかに誰も使わない添字に置く**
/// （256 は直接マッピング、258 は AP の CPU ごとのスタック、257・259・260 は試しの feature の番地である）。
pub const PROBE_VIRT: u64 = 0xFFFF_8280_0000_0000;

/// 試しのページの先頭に書いておく値（ASCII で `NX_PROBE`）。**読めた値がこれと同じなら、項目を引けている。**
pub const PROBE_PATTERN: u64 = 0x4e58_5f50_524f_4245;

/// 試しのページをマップしたか。
static MAPPED: AtomicBool = AtomicBool::new(false);

/// 試しのページを読んだ結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProbeReading {
    /// ページの先頭から読めた値。
    pub value: u64,
    /// 読んだときの、この CPU の `EFER.NXE`。
    pub nxe: bool,
}

impl ProbeReading {
    /// 値が期待どおりで、NXE が立っているか（純粋ロジック）。
    pub fn holds(self) -> bool {
        self.value == PROBE_PATTERN && self.nxe
    }
}

/// 試しのページを 1 枚マップし、BSP で読んで確かめる（2026-10-02）。**期待と違えば、名前つきで止まる。**
///
/// # 契約
///
/// - 呼ぶのは BSP で、本番のページテーブルへ切り替えた後、AP を起こす前に 1 回だけである。
/// - 呼んだ後は、[`PROBE_VIRT`] に、実行禁止のビットを付けた読むだけのページが 1 枚マップされている
///   （ページの権限の一覧には `execute-disable probe` の名前で出る）。AP は [`read_execute_disable_probe`] で読む。
///
/// # Safety
///
/// 本番のページテーブルが CR3 に載っていて、直接マッピングが使えること。起動時の単一の文脈で、AP がまだ
/// 走っていないこと。**BSP の `EFER.NXE` が立っていること**（`cpu_state::report_established_bits` が、
/// 立っていなければ、ここより前で起動を止めている）。
pub unsafe fn map_execute_disable_probe<const CAP: usize>(
    logger: &mut Logger<Serial>,
    allocator: &mut FrameAllocator<CAP>,
) {
    let direct_map = common::addr::direct_map();
    let virt = VirtAddr::new(PROBE_VIRT).expect("the probe address is canonical");
    let Some(frame) = allocator.allocate_frame() else {
        logger.error(format_args!(
            "nx-probe: no frame for the probe page; halting"
        ));
        common::arch::x86_64::cpu::halt_forever();
    };
    // SAFETY: いま取ったばかりの、誰も使っていないフレームを、直接マッピング越しに書く。
    unsafe {
        direct_map
            .phys_to_virt(frame)
            .as_mut_ptr::<u64>()
            .write_volatile(PROBE_PATTERN);
    }
    // SAFETY: 呼び出し側の契約。稼働中の表へ、まだ誰も使っていない番地をマップする。
    let mut table = unsafe { ActivePageTable::current(direct_map) };
    // **読むだけの権限でマップする。** 実行しない権限なので、変換が実行禁止のビットを付ける（2026-10-02。
    // 以前は、変換が実行の欄を読まなかったので、このページ専用の入口でビットを足していた）。
    // SAFETY: 同上。BSP の NXE は立っている（呼び出し側の契約）。AP は、トランポリンで NXE を立ててから来る。
    if let Err(error) = unsafe {
        table.map_4kib(
            virt,
            frame,
            crate::paging::permissions::PagePermissions::kernel_read_only(),
            allocator,
        )
    } {
        logger.error(format_args!(
            "nx-probe: could not map the probe page: {error:?}; halting"
        ));
        common::arch::x86_64::cpu::halt_forever();
    }
    // 破壊テストの傘 (2026-10-02, nx-probe-only-leaf): 変換が実行禁止のビットを立てないビルドなので、試しのページの
    // 葉にだけ、ここで足す。**実行禁止のビットを持つ葉が、このページの 1 枚だけになる。**
    #[cfg(feature = "nx-probe-only-leaf-test")]
    // SAFETY: いまマップした、4KiB の葉の読むだけのページである。実行はしない。
    if let Err(error) = unsafe { table.mark_leaf_execute_disable(virt) } {
        logger.error(format_args!(
            "nx-probe: could not mark the probe page: {error:?}; halting"
        ));
        common::arch::x86_64::cpu::halt_forever();
    }
    crate::page_survey::register(
        "execute-disable probe",
        PROBE_VIRT,
        PROBE_VIRT + (1 << 39),
        true,
    );
    // **葉を読み戻して、実行禁止のビットが本当に付いたことを見る**（付いていなければ、下の読みは何も確かめない）。
    let leaf = match table.translate(virt) {
        Ok(Some(translation)) => translation.entry,
        other => {
            logger.error(format_args!(
                "nx-probe: the probe page does not translate after it was mapped ({other:?}); halting"
            ));
            common::arch::x86_64::cpu::halt_forever();
        }
    };
    let marked = leaf & entry::PTE_NO_EXECUTE != 0;
    logger.info(format_args!(
        "nx-probe: mapped a probe page at {PROBE_VIRT:#x} -> {:#x} with the execute-disable bit \
         (entry {leaf:#x}, bit 63 set = {marked}) [read back from the live table]",
        frame.as_u64()
    ));
    if !marked {
        logger.error(format_args!(
            "nx-probe: the probe page was mapped without the execute-disable bit, so reading it \
             would prove nothing; halting"
        ));
        common::arch::x86_64::cpu::halt_forever();
    }
    MAPPED.store(true, Ordering::SeqCst);

    let reading = read_execute_disable_probe(0);
    match reading {
        Some(reading) if reading.holds() => logger.info(format_args!(
            "nx-probe: cpu 0 read the probe page: value={:#x} (expected {PROBE_PATTERN:#x}), \
             EFER.NXE={} (expected 1)",
            reading.value,
            u8::from(reading.nxe)
        )),
        other => {
            logger.error(format_args!(
                "nx-probe: cpu 0 read the probe page and got {other:?}, but the value must be \
                 {PROBE_PATTERN:#x} with EFER.NXE set; halting"
            ));
            common::arch::x86_64::cpu::halt_forever();
        }
    }
}

/// この CPU で、試しのページを読む。**マップする前なら `None`**（読まない）。`slot` は、この CPU のスロットの
/// 番号（BSP が 0）で、破壊テストがどの CPU で NXE を落とすかを決めるためだけに使う。
///
/// **NXE が 0 の CPU で呼ぶと、戻らない**——ページを引いた時点で、予約のビットの違反の `#PF` になる
/// （破壊テスト `nx-probe-bsp-without-nxe-test` と `nx-probe-ap-without-nxe-test` が、その形を作る）。
///
/// # 契約（境界の関数。2026-10-02）
///
/// - 呼ぶのは、本番のページテーブルを載せた CPU である（BSP は [`map_execute_disable_probe`] の中から、AP は
///   本番の表へ切り替えた後に `crate::smp` から）。割り込みの状態は問わない。BKL は要らない（読むだけで、
///   ページは起動の間に 1 回マップしたきり変わらない）。
/// - 変えるのは、破壊テストの形のときの、この CPU の `EFER.NXE` だけである。
pub fn read_execute_disable_probe(slot: usize) -> Option<ProbeReading> {
    if !MAPPED.load(Ordering::SeqCst) {
        return None;
    }
    // 破壊テスト (2026-10-02, nx-probe-bsp-without-nxe / nx-probe-ap-without-nxe): 読む直前に、この CPU の
    // NXE を落とす。**実行禁止のビットを持つ項目を、NXE が 0 のまま引く形である**——読んだ時点で、予約のビットの
    // 違反の #PF になる（CR2 が試しのページ）。
    let sabotaged = (cfg!(feature = "nx-probe-bsp-without-nxe-test") && slot == 0)
        || (cfg!(feature = "nx-probe-ap-without-nxe-test") && slot != 0);
    if sabotaged {
        // SAFETY: 破壊テスト。NXE だけを落とす（LME はそのまま）。この後の読みで #PF になり、止まる。
        unsafe {
            common::arch::x86_64::cpu::write_efer(Efer::from_raw(
                read_efer().raw() & !Efer::NO_EXECUTE_ENABLE,
            ));
        }
    }
    let nxe = read_efer().raw() & Efer::NO_EXECUTE_ENABLE != 0;
    // SAFETY: [`PROBE_VIRT`] は、[`map_execute_disable_probe`] がマップした読むだけのページである（上で確かめた）。
    // 読むだけで、ページは起動の間に 1 回マップしたきり変わらない。
    let value = unsafe { (PROBE_VIRT as *const u64).read_volatile() };
    Some(ProbeReading { value, nxe })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **試しのページの番地は、カーネルの側の `PML4[261]` の先頭である。**
    #[test]
    fn the_probe_sits_at_the_start_of_kernel_slot_261() {
        let virt = VirtAddr::new(PROBE_VIRT).unwrap();
        assert_eq!(virt.top_index(), 261);
        assert_eq!(PROBE_VIRT & ((1 << 39) - 1), 0);
    }

    /// **値が同じで、NXE が立っているときだけ、確かめは成り立つ。**
    #[test]
    fn the_reading_holds_only_with_the_pattern_and_nxe() {
        assert!(ProbeReading {
            value: PROBE_PATTERN,
            nxe: true
        }
        .holds());
        assert!(!ProbeReading {
            value: PROBE_PATTERN,
            nxe: false
        }
        .holds());
        assert!(!ProbeReading {
            value: 0,
            nxe: true
        }
        .holds());
    }
}
