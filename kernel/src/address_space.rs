//! プロセスごとのアドレス空間（S7-c）。
//!
//! # 何を作っているか
//!
//! **上位（カーネル）を共有し、下位（ユーザー）だけを差し替える。** PML4 は 512 本の
//! エントリを持ち、**上位 256 本（添字 256..512）がカーネルの取り分**である
//! （higher-half、ADR-0024）。新しいアドレス空間を作るとは、**PML4 のフレームを 1 枚
//! 取り、上位 256 本をそのまま写し、下位 256 本を空にする**ことである。
//!
//! # 共有するのであって、複製するのではない
//!
//! **写すのは PML4 のエントリ（8 バイトの値）であって、その先のテーブルではない。**
//! したがって**カーネルの写像はすべてのアドレス空間で同一の実体を指す。** 片方で
//! カーネル側を変えれば、もう片方からも見える。**これが「共有」の意味であり、到達
//! 条件の「稼働中テーブルの共有カーネル部分が一致すること」が言っていることである。**
//!
//! # なぜ上位を写すだけで足りるのか
//!
//! **カーネルは higher-half にあり、恒等写像は既に落としてある**（B-2b）。
//! したがってカーネルのコード・スタック・direct map はすべて上位 256 本の下にある。
//! **CR3 を差し替えても、上位が同じなら実行中のコードもスタックも見え続ける。**
//!
//! **これは検査できる主張である**（到達条件 4）。破壊 `addrspace-no-kernel-share`
//! は上位を写さない。**切り替えた瞬間に命令フェッチが翻訳できなくなる。**

use crate::frame_allocator::FrameAllocator;
use common::addr::{DirectMap, PhysAddr};

/// PML4 のエントリ数。
pub const PML4_ENTRY_COUNT: usize = 512;

/// カーネルの取り分が始まる添字。**ここから上が共有である。**
///
/// higher-half のカーネルは `0xFFFF_8000_0000_0000` 以上に居り、その PML4 添字は
/// 256 である（符号拡張された上位半分の先頭）。
pub const KERNEL_PML4_FIRST_INDEX: usize = 256;

/// その添字がカーネルの取り分か（共有するか）。
///
/// **純粋関数にしてある。** 「どこからどこまでを共有するか」は写像の実体を触らずに
/// 決まる判断なので、ホスト上で検査できる形に切り出す。
///
/// **ユーザー空間の添字（`USER_PML4_INDEX`）との突き合わせは、ここには置けない。**
/// あれは `kernel/src/main.rs`（bin 側）に居り、lib からは見えない。**S7 でこの添字を
/// 動かすとき**（`docs/deferred-decisions.md`）**、動かした先が共有側へ入り込んで
/// いないことを、bin 側で確かめること。**
pub const fn is_shared_kernel_index(index: usize) -> bool {
    index >= KERNEL_PML4_FIRST_INDEX && index < PML4_ENTRY_COUNT
}

/// アドレス空間の作成でしくじる形。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AddressSpaceError {
    /// PML4 用のフレームが取れなかった。
    OutOfFrames,
    /// direct map 越しに PML4 を触れなかった（写像の外を指している）。
    Unreachable,
}

/// プロセス 1 つ分のアドレス空間。
///
/// **まだ破棄を持たない。** 破棄は S7-d である。**持たせないのは、破棄が隔離
/// （[`crate::quarantine`]）と一体だからで、片方だけ先に作ると「返してよい」判断が
/// 無いまま返す形が書けてしまう。**
pub struct AddressSpace {
    pml4: PhysAddr,
}

impl AddressSpace {
    /// 稼働中のテーブルからカーネル部分を写して、新しいアドレス空間を作る。
    ///
    /// # Safety
    ///
    /// - `current_pml4` が稼働中の PML4 を指していること。
    /// - `direct_map` がその PML4 と、これから取るフレームの両方を覆っていること。
    /// - **呼び出し中に他コアがカーネル側の PML4 を変えないこと。** 現在これは BKL が
    ///   与える（写像の変更は BKL の内側でのみ行う。ADR-0027 の Addendum）。
    pub unsafe fn new(
        allocator: &mut FrameAllocator,
        direct_map: DirectMap,
        current_pml4: PhysAddr,
    ) -> Result<Self, AddressSpaceError> {
        let pml4 = allocator
            .allocate_frame()
            .ok_or(AddressSpaceError::OutOfFrames)?;

        // **direct map が覆っているかを先に見る。** `phys_to_virt` は覆いを検査せず
        // 加算するだけなので、**覆いの外を渡すと黙って別のアドレスを返す。**
        if !direct_map.covers(pml4) || !direct_map.covers(current_pml4) {
            // 触れないフレームを抱えたままにしない。**取ったものは返す。**
            let _ = allocator.deallocate_frame(pml4);
            return Err(AddressSpaceError::Unreachable);
        }
        let new_virt = direct_map.phys_to_virt(pml4);
        let current_virt = direct_map.phys_to_virt(current_pml4);

        let new_table = new_virt.as_u64() as *mut u64;
        let current_table = current_virt.as_u64() as *const u64;

        for index in 0..PML4_ENTRY_COUNT {
            // 破壊 (S7-c, addrspace-no-kernel-share): **上位を写さない。**
            // 切り替えた瞬間に命令フェッチが翻訳できなくなる。
            #[cfg(feature = "addrspace-no-kernel-share")]
            let value = 0u64;
            #[cfg(not(feature = "addrspace-no-kernel-share"))]
            let value = if is_shared_kernel_index(index) {
                // SAFETY: direct map 越しの稼働中 PML4 の読み。覆いは上で確認済みで、
                // 添字は 512 エントリ内である。
                unsafe { current_table.add(index).read_volatile() }
            } else {
                0
            };
            // SAFETY: いま取ったフレームの、direct map 越しの書き。範囲は 512 エントリ内。
            unsafe { new_table.add(index).write_volatile(value) };
        }

        Ok(Self { pml4 })
    }

    /// この空間の PML4 の物理アドレス。
    pub fn pml4(&self) -> PhysAddr {
        self.pml4
    }

    /// この空間へ切り替える。
    ///
    /// # Safety
    ///
    /// [`Self::new`] が上位を写しているので、**カーネルのコード・スタック・direct map は
    /// 切り替えの前後で同じ物理を指す。** ただし**下位は空である**——切り替えた後に
    /// ユーザー空間のアドレスへ触ると `#PF` になる。
    ///
    /// **BKL を保持したまま呼ぶこと。** CR3 は per-CPU の状態だが、写像の共有部分を
    /// 他コアが同時に変えていないことに依存する。
    pub unsafe fn activate(&self) {
        // SAFETY: 上記の契約。上位を写してあるので、実行中のコードとスタックは見え続ける。
        unsafe { crate::paging::switch::switch_to(self.pml4) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lower_half_is_not_shared() {
        assert!(!is_shared_kernel_index(0));
        assert!(!is_shared_kernel_index(1));
        assert!(!is_shared_kernel_index(KERNEL_PML4_FIRST_INDEX - 1));
    }

    #[test]
    fn the_upper_half_is_shared() {
        assert!(is_shared_kernel_index(KERNEL_PML4_FIRST_INDEX));
        assert!(is_shared_kernel_index(PML4_ENTRY_COUNT - 1));
    }

    #[test]
    fn indices_past_the_table_are_not_shared() {
        assert!(!is_shared_kernel_index(PML4_ENTRY_COUNT));
        assert!(!is_shared_kernel_index(PML4_ENTRY_COUNT + 1));
    }

    /// **半分ちょうどで割れていること。** 256 本ずつでなくなったら、higher-half の
    /// 前提（カーネルは上位半分に居る）が変わっている。
    #[test]
    fn the_split_is_exactly_half_of_the_table() {
        let shared = (0..PML4_ENTRY_COUNT)
            .filter(|index| is_shared_kernel_index(*index))
            .count();
        assert_eq!(shared, PML4_ENTRY_COUNT / 2);
    }
}
