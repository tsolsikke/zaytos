//! CR3 の読み取りと切り替え。
//!
//! CR3 の書き換えは実行中のコードから見えるメモリ写像そのものを変えるので、
//! ホストの `cargo test` の対象にしていない。

use common::addr::PhysAddr;

use super::entry::ADDR_MASK_TABLE;

/// 現在の CR3 のアドレス部分（bit 12-51）。
///
/// PCID や PWT/PCD のビットは落として返す。呼び出し側がマスクを忘れる余地を
/// 無くすため、ここで型に落とす。
pub fn read_cr3() -> PhysAddr {
    let value: u64;
    // SAFETY: `mov reg, cr3` は読み取りだけで、メモリにもスタックにも副作用が
    // ない。値は実行環境に依存するので `options` は指定しない。
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) value);
    }
    PhysAddr::new(value & ADDR_MASK_TABLE).expect("CR3 の bit 12-51 は 52 ビットに収まる")
}

/// CR3 を `pml4_phys` へ切り替える。
///
/// 下位ビット（PWT/PCD を含む）はマスクしない。呼び出し側が「アドレス部分のみ、
/// フラグは 0」の値を渡すこと。
///
/// # Safety
///
/// 切り替えた後も、次の3つが新しいテーブルで引き続き翻訳できること。
///
/// - 実行中のコード
/// - 現在のスタック
/// - `pml4_phys` 自身が指すフレーム
///
/// 崩れていると、この命令の直後の命令フェッチかスタックアクセスでトリプル
/// フォルトする。実際に踏める（破壊 `addrspace-no-kernel-share` が
/// `CR2 == IP` の `#PF` から `#DF` を経てトリプルフォルトになる）。
///
/// 満たし方は呼び出し側による。higher-half のカーネルでは上位 256 本の PML4
/// エントリを共有すれば足りる（[`crate::address_space::AddressSpace`]）。
/// 恒等写像に依っていたのは B-2b で恒等を外す前の話である。
///
/// `options` は意図的に指定しない。CR3 の書き換えは今後のロード/ストアが
/// どの物理を指すかを変えるので、`nomem` は誤りであり、並べ替えを許さない。
pub unsafe fn switch_to(pml4_phys: PhysAddr) {
    // SAFETY: 呼び出し元契約を参照。
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) pml4_phys.as_u64());
    }
}
