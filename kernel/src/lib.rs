//! ZaytOS kernel の共有ロジック。
//!
//! ハードウェア依存部（`main.rs` の `_start`/`panic.rs`）と純粋ロジック
//! （[`memory_map`], [`frame_allocator`]）を分離し、後者はホスト上の
//! `cargo test` で検証する。M2-c の物理フレーム
//! アロケータは「間違えると無言で壊れる」領域であるため、特に手厚く
//! テストする。

#![cfg_attr(not(test), no_std)]

/// リンカスクリプトから生成した定数（`build.rs`）。
///
/// **手で書かない。** `link.ld` の `KERNEL_VIRT_BASE` と
/// `KERNEL_LOAD_ADDR` を build.rs が読み取って生成する。二重に持つと
/// 片方だけ直したときに食い違い、「リンクは通るがアドレス変換が一段
/// ずれる」という最も診断しにくい形で出る。
pub mod link_symbols {
    include!(concat!(env!("OUT_DIR"), "/link_symbols.rs"));
}

pub mod console;
pub mod frame_allocator;
pub mod gdt;
pub mod graphics;
pub mod heap;
pub mod idt;
pub mod interrupts;
pub mod keyboard;
pub mod memory_map;
pub mod paging;
pub mod pic;
pub mod pit;
pub mod stack;

/// kernel イメージ内の仮想アドレスを物理アドレスへ写す。
///
/// # この変換は direct map ではない
///
/// **kernel イメージの物理位置は「リンクアドレスとロードアドレスの差」で
/// 決まる。** bootloader が ELF をどこへ置いたかで決まるものであって、
/// direct physical map の窓とは無関係である。両者は現在たまたま一致して
/// いる（どちらも恒等）ため、区別せずに書いても動く。移行後は一致しない。
///
/// 差は [`link_symbols::KERNEL_VIRT_BASE`] で、`link.ld` から生成される。
/// 現在は 0 なので、この関数は値をそのまま移すだけである。
///
/// # 範囲外を弾く
///
/// kernel イメージの外にある仮想アドレスを渡してはならない。この対応は
/// イメージの中でしか成り立たない。差を引けない（アンダーフローする）
/// 場合は panic する。黙って別のアドレスを返すより、そこで止まるほうがよい。
pub fn kernel_phys_from_virt(virt: common::addr::VirtAddr) -> common::addr::PhysAddr {
    let raw = virt
        .as_u64()
        .checked_sub(link_symbols::KERNEL_VIRT_BASE)
        .expect("the address is below the kernel's link base");
    common::addr::PhysAddr::new(raw).expect("a kernel image address fits in 52 bits")
}

/// [`kernel_phys_from_virt`] の逆。
pub fn kernel_virt_from_phys(phys: common::addr::PhysAddr) -> common::addr::VirtAddr {
    let raw = phys
        .as_u64()
        .checked_add(link_symbols::KERNEL_VIRT_BASE)
        .expect("the kernel image stays within the address space");
    common::addr::VirtAddr::new(raw).expect("a kernel image address is canonical")
}

#[cfg(test)]
mod link_symbol_tests {
    use super::*;
    use common::addr::{PhysAddr, VirtAddr};

    /// 現在は恒等である。移行でここが変わる。
    #[test]
    fn the_conversion_is_identity_while_the_base_is_zero() {
        assert_eq!(link_symbols::KERNEL_VIRT_BASE, 0);
        assert_eq!(link_symbols::KERNEL_LOAD_ADDR, 0x100000);

        let virt = VirtAddr::new(0x10_0000).unwrap();
        assert_eq!(
            kernel_phys_from_virt(virt),
            PhysAddr::new(0x10_0000).unwrap()
        );
        assert_eq!(
            kernel_virt_from_phys(PhysAddr::new(0x10_0000).unwrap()),
            virt
        );
    }

    /// 往復すること。境界（ロードアドレスそのもの、0、上限付近）で見る。
    #[test]
    fn the_conversion_round_trips_at_the_boundaries() {
        for raw in [
            0u64,
            link_symbols::KERNEL_LOAD_ADDR,
            link_symbols::KERNEL_LOAD_ADDR + 0xFFF,
            // 下位半分の上端。0x000F_FFFF_FFFF_F000 は物理としては表せるが
            // 仮想としては非正規なので使えない。境界の取り方を間違えて
            // 一度ここで落ちた。
            0x0000_7FFF_FFFF_F000,
        ] {
            let virt = VirtAddr::new(raw).unwrap();
            assert_eq!(kernel_virt_from_phys(kernel_phys_from_virt(virt)), virt);
        }
    }
}
