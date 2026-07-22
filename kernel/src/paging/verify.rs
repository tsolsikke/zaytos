//! ページテーブルの独立読み戻し（H-2）。
//!
//! **構築側とコードを共有しない。** `table` や `active` の walk を呼ぶと、
//! 「同じ式で作ったものを同じ式で検算する」ことになり、構築側と検証側が
//! 同じ誤りを持つ場合に検出できない。ここでは階層を降りるループを
//! 独立に書いてある。
//!
//! # 何を共有し、何を共有しないか
//!
//! 共有してよいもの
//! - [`VirtAddr`] の添字抽出メソッド。T-1 でホストテスト済みで、
//!   中継の繋ぎ間違いも `entry` 側のテストで固定してある
//!
//! 共有しないもの
//! - 階層を降りるループ（PML4 → PDPT → PD → PT）
//! - エントリの解釈。Present / PS / アドレスマスクのビット位置を
//!   このファイルに書き直してある。**重複しているのが検出力の源である。**
//!   `entry` の定数を参照すると、そちらが誤っていた場合に一緒に誤る
//!
//! # CR3 に載っていないテーブルを辿る
//!
//! [`super::active::ActivePageTable`] は CR3 を読んで構築するため、
//! まだ載せていないテーブルには使えない。ここは PML4 の物理アドレスを
//! 引数で受け取る。

use common::addr::{DirectMap, PhysAddr, VirtAddr};

/// 独立に書き直したビット定義。**`entry` の定数を参照しない。**
mod bits {
    /// Present。
    pub const PRESENT: u64 = 1 << 0;
    /// PD / PDPT レベルの「ページそのもの」ビット。
    pub const PAGE_SIZE: u64 = 1 << 7;
    /// 中間テーブルと 4KiB ページのアドレス部分（ビット 12-51）。
    pub const ADDR_4K: u64 = 0x000F_FFFF_FFFF_F000;
    /// 2MiB ページのアドレス部分（ビット 21-51）。
    pub const ADDR_2M: u64 = 0x000F_FFFF_FFE0_0000;
}

/// 独立 walk の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// 対応する物理アドレス（ページ内オフセットを含む）。
    pub phys: PhysAddr,
    /// 2MiB ページで解決されたか。
    pub huge: bool,
}

/// 辿れなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkError {
    /// 途中の階層のエントリが不在。
    NotPresent,
    /// PDPT レベルで 1GiB ページに当たった。ZaytOS は作らない。
    GiantPage,
}

/// 指定した PML4 を根として、仮想アドレスを辿る。
///
/// # Safety
///
/// `pml4_phys` が有効なページテーブルフレームを指し、`direct_map` を通して
/// そのフレームと配下のテーブルが読めること。
pub unsafe fn walk(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    virt: VirtAddr,
) -> Result<Resolved, WalkError> {
    // SAFETY: 呼び出し元契約による。読み取りのみ。
    let read = |table: PhysAddr, index: usize| -> u64 {
        unsafe {
            core::ptr::read_volatile(direct_map.phys_to_virt(table).as_ptr::<u64>().add(index))
        }
    };

    // **降り方をここに独立して書く。** 構築側のループとは別物である。
    let pml4e = read(pml4_phys, virt.pml4_index());
    if pml4e & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }

    let pdpt = PhysAddr::new_const(pml4e & bits::ADDR_4K);
    let pdpte = read(pdpt, virt.pdpt_index());
    if pdpte & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }
    if pdpte & bits::PAGE_SIZE != 0 {
        return Err(WalkError::GiantPage);
    }

    let pd = PhysAddr::new_const(pdpte & bits::ADDR_4K);
    let pde = read(pd, virt.pd_index());
    if pde & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }
    if pde & bits::PAGE_SIZE != 0 {
        // 2MiB ページ。オフセットは下位 21 ビット。
        let base = pde & bits::ADDR_2M;
        let offset = virt.as_u64() & 0x1F_FFFF;
        return Ok(Resolved {
            phys: PhysAddr::new_const(base + offset),
            huge: true,
        });
    }

    let pt = PhysAddr::new_const(pde & bits::ADDR_4K);
    let pte = read(pt, virt.pt_index());
    if pte & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }
    let base = pte & bits::ADDR_4K;
    let offset = virt.as_u64() & 0xFFF;
    Ok(Resolved {
        phys: PhysAddr::new_const(base + offset),
        huge: false,
    })
}

/// 指定した PML4 の、ある階層のエントリをそのまま読む。
///
/// 恒等側のテーブルが構築処理で書き換わっていないことを確かめるために使う。
///
/// # Safety
///
/// [`walk`] と同じ契約。
pub unsafe fn read_pml4_entry(pml4_phys: PhysAddr, direct_map: DirectMap, index: usize) -> u64 {
    // SAFETY: 呼び出し元契約による。読み取りのみ。
    unsafe {
        core::ptr::read_volatile(
            direct_map
                .phys_to_virt(pml4_phys)
                .as_ptr::<u64>()
                .add(index),
        )
    }
}
