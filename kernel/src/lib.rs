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

/// この成果物に立っている feature の一覧（`build.rs` が生成する）。
///
/// # なぜ持つのか
///
/// **どの像が走ったかを、起動ログだけで見分けるためである。**
/// 破壊 feature の項目が落ちたとき、「破壊が効かなかった」のか
/// **「破壊の無い像が走った」**のかが、判定行からは分からない
/// （`docs/verification-coverage.md` の「破壊feature が効いていない形で
/// 2 項目が落ち、単独では再現しなかった」）。
///
/// **出すのは実際にコンパイルされた構成である。** `build.rs` が `CARGO_FEATURE_*`
/// から導くので、**渡したつもりの構成ではない。**
pub mod enabled_features {
    include!(concat!(env!("OUT_DIR"), "/enabled_features.rs"));
}

pub mod acpi;
pub mod address_space;
pub mod apic;
pub mod bkl;
pub mod console;
pub mod fp;
pub mod frame_allocator;
pub mod gdt;
pub mod graphics;
pub mod heap;
pub mod idt;
pub mod input;
pub mod interrupts;
pub mod irq;
pub mod keyboard;
pub mod memory_map;
pub mod paging;
pub mod pci;
pub mod quarantine;
pub mod ring3;
pub mod smp;
pub mod stack;
pub mod syscall;
pub mod task;
pub mod userland;
pub mod vfs;
pub mod virtio;

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

    /// 再リンク（B-2a-3）でベースが高位（0xFFFFFFFF80000000）になった。
    /// イメージ VMA から物理を引き、物理から高位 VMA を足す。
    #[test]
    fn the_conversion_uses_the_high_link_base() {
        assert_eq!(link_symbols::KERNEL_VIRT_BASE, 0xFFFF_FFFF_8000_0000);
        assert_eq!(link_symbols::KERNEL_LOAD_ADDR, 0x100000);

        // イメージ先頭の VMA = base + load addr → 物理 0x100000。
        let virt = VirtAddr::new(0xFFFF_FFFF_8010_0000).unwrap();
        assert_eq!(
            kernel_phys_from_virt(virt),
            PhysAddr::new(0x10_0000).unwrap()
        );
        assert_eq!(
            kernel_virt_from_phys(PhysAddr::new(0x10_0000).unwrap()),
            virt
        );
    }

    /// 往復すること。高位側の境界（ベースそのもの、イメージ先頭、上限付近）で見る。
    /// kernel_phys_from_virt はイメージ VMA（base 以上）にしか使えないので、
    /// 入力はすべて高位にする。
    #[test]
    fn the_conversion_round_trips_at_the_boundaries() {
        for raw in [
            link_symbols::KERNEL_VIRT_BASE,
            link_symbols::KERNEL_VIRT_BASE + link_symbols::KERNEL_LOAD_ADDR,
            link_symbols::KERNEL_VIRT_BASE + link_symbols::KERNEL_LOAD_ADDR + 0xFFF,
            // 上位半分の上端付近（正規な高位 VA）。
            0xFFFF_FFFF_FFFF_F000,
        ] {
            let virt = VirtAddr::new(raw).unwrap();
            assert_eq!(kernel_virt_from_phys(kernel_phys_from_virt(virt)), virt);
        }
    }
}
