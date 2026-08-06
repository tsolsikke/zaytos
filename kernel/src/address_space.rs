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
    /// 共有側（上位）へ張ろうとした。**下位にしか張れない。**
    NotPrivate,
    /// 下位に巨大ページがあった。**張る経路が無いので、前提が崩れている。**
    UnexpectedHugePage,
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

/// 下位（ユーザー側）の PML4 添字の範囲。**破棄が触ってよいのはここだけである。**
const PRIVATE_INDEX_RANGE: core::ops::Range<usize> = 0..KERNEL_PML4_FIRST_INDEX;

/// 1 回の破棄で集められるフレームの本数。
///
/// **集めてから世代を上げるので、一時的に置く場所が要る。** 溢れたら漏らす
/// （返すより安全である）。**隔離の容量と同じ桁にしてある。**
const MAX_FRAMES_PER_DESTROY: usize = crate::quarantine::QUARANTINE_CAPACITY;

impl AddressSpace {
    /// 4KiB のユーザーページを 1 枚張る（S7-d）。
    ///
    /// **下位にしか張れない。** 上位は共有なので、ここから触ると全アドレス空間へ
    /// 波及する。**添字で弾く**（[`is_shared_kernel_index`]）。
    ///
    /// # Safety
    ///
    /// - `direct_map` が、これから取る中間テーブルと `frame` を覆っていること。
    /// - **この空間がどのコアでも稼働していないこと。** 稼働中に張ると、そのコアの
    ///   TLB との整合を別に取る必要がある。**S7-d の使い方では、作ってから
    ///   切り替えるまでの間に張るので満たされる。**
    pub unsafe fn map_user_4kib(
        &mut self,
        allocator: &mut FrameAllocator,
        direct_map: DirectMap,
        virt: common::addr::VirtAddr,
        frame: PhysAddr,
    ) -> Result<(), AddressSpaceError> {
        use crate::paging::entry;

        if is_shared_kernel_index(entry::pml4_index(virt)) {
            return Err(AddressSpaceError::NotPrivate);
        }
        if !direct_map.covers(frame) {
            return Err(AddressSpaceError::Unreachable);
        }

        let mut table = self.pml4;
        for index in [
            entry::pml4_index(virt),
            entry::pdpt_index(virt),
            entry::pd_index(virt),
        ] {
            // SAFETY: `table` は覆いを確かめたテーブルで、添字は 512 未満。
            let existing = unsafe { read_entry(direct_map, table, index) };
            let child = if entry::is_present(existing) {
                if entry::is_huge(existing) {
                    // **巨大ページは扱わない。** 下位に 2MiB を張る経路が無いので、
                    // ここへ来るのは前提が崩れたときである。
                    return Err(AddressSpaceError::UnexpectedHugePage);
                }
                entry::table_address(existing)
            } else {
                let fresh = allocator
                    .allocate_frame()
                    .ok_or(AddressSpaceError::OutOfFrames)?;
                if !direct_map.covers(fresh) {
                    let _ = allocator.deallocate_frame(fresh);
                    return Err(AddressSpaceError::Unreachable);
                }
                // SAFETY: いま取ったフレームで、direct map が覆っている。
                unsafe { zero_table(direct_map, fresh) };
                // SAFETY: 中間テーブルなので U ビットを立てる。立てないと、葉で
                // 立てても CPU は全階層の AND を見るのでユーザーから触れない。
                unsafe {
                    write_entry(
                        direct_map,
                        table,
                        index,
                        fresh.as_u64() | entry::PTE_PRESENT | entry::PTE_WRITABLE | entry::PTE_USER,
                    )
                };
                fresh
            };
            table = child;
        }

        // SAFETY: 葉。ユーザーから読み書きできる 4KiB ページ。
        unsafe {
            write_entry(
                direct_map,
                table,
                entry::pt_index(virt),
                frame.as_u64() | entry::PTE_PRESENT | entry::PTE_WRITABLE | entry::PTE_USER,
            )
        };
        Ok(())
    }

    /// この空間を破棄し、**下位で使っていたフレームをすべて隔離へ入れる**（S7-d）。
    ///
    /// **アロケータへ直接は返さない。** 他コアの TLB に古い翻訳が残りうるので、
    /// **ADR-0027 の Addendum の不変条件どおり、世代が退くまで隔離する。**
    ///
    /// **触るのは下位だけである**（[`PRIVATE_INDEX_RANGE`]）。上位は共有なので、
    /// ここで返したら他のアドレス空間の写像を壊す。
    ///
    /// 返すのは (隔離へ入れた本数, 隔離が溢れて漏らした本数)。
    ///
    /// # Safety
    ///
    /// - **この空間がどのコアでも稼働していないこと。** 稼働中の CR3 を破棄すると、
    ///   そのコアは次の翻訳で死ぬ。
    /// - `guard` が示すとおり BKL を保持していること。**写像の変更と世代の更新は
    ///   BKL の内側でしか行わない**（ADR-0027 の Addendum の失効条件）。
    pub unsafe fn destroy(
        self,
        direct_map: DirectMap,
        quarantine: &mut crate::quarantine::Quarantine,
        _guard: &crate::bkl::BklGuard,
    ) -> (usize, usize) {
        use crate::paging::entry;

        // **順序が要である。** (1) 写像を外し、(2) 集め終えてから世代を上げ、
        // (3) その世代で隔離へ入れる。
        //
        // **上げてから外すと、上げた直後にフラッシュしたコアが、まだ生きている
        // 写像を読み直しうる。** **外し終えてから上げれば、その世代以降に
        // フラッシュしたコアは、外れた後の状態しか見ていない。**
        // **判定が `>=` で足りるのはこの順序による**（[`crate::bkl::generation_is_retired`]）。
        let mut collected = [None; MAX_FRAMES_PER_DESTROY];
        let mut count = 0usize;
        let mut leaked = 0usize;

        let mut collect = |frame: PhysAddr, count: &mut usize, leaked: &mut usize| {
            if *count < MAX_FRAMES_PER_DESTROY {
                collected[*count] = Some(frame);
                *count += 1;
            } else {
                // **入れ物が足りなければ漏らす。** 早く返すより漏らすほうが安全である。
                *leaked += 1;
            }
        };

        for pml4_index in PRIVATE_INDEX_RANGE {
            // SAFETY: 自分の PML4。添字は 512 未満。
            let pml4_entry = unsafe { read_entry(direct_map, self.pml4, pml4_index) };
            if !entry::is_present(pml4_entry) {
                continue;
            }
            let pdpt = entry::table_address(pml4_entry);
            for pdpt_index in 0..entry::ENTRIES_PER_TABLE {
                // SAFETY: 上で present を確かめたテーブル。
                let pdpt_entry = unsafe { read_entry(direct_map, pdpt, pdpt_index) };
                if !entry::is_present(pdpt_entry) || entry::is_huge(pdpt_entry) {
                    continue;
                }
                let pd = entry::table_address(pdpt_entry);
                for pd_index in 0..entry::ENTRIES_PER_TABLE {
                    // SAFETY: 上で present を確かめたテーブル。
                    let pd_entry = unsafe { read_entry(direct_map, pd, pd_index) };
                    if !entry::is_present(pd_entry) || entry::is_huge(pd_entry) {
                        continue;
                    }
                    let pt = entry::table_address(pd_entry);
                    for pt_index in 0..entry::ENTRIES_PER_TABLE {
                        // SAFETY: 上で present を確かめたテーブル。
                        let pt_entry = unsafe { read_entry(direct_map, pt, pt_index) };
                        if entry::is_present(pt_entry) {
                            collect(entry::page_address_4k(pt_entry), &mut count, &mut leaked);
                        }
                    }
                    collect(pt, &mut count, &mut leaked);
                }
                collect(pd, &mut count, &mut leaked);
            }
            collect(pdpt, &mut count, &mut leaked);
            // **エントリを落としてから次へ行く。** 落とさずに返すと、隔離が解けた
            // 後に残骸を辿れてしまう。
            // SAFETY: 自分の PML4 の下位エントリ。
            unsafe { write_entry(direct_map, self.pml4, pml4_index, 0) };
        }

        collect(self.pml4, &mut count, &mut leaked);

        // (2) ここまでで写像は外れている。**外し終えてから上げる。**
        crate::bkl::note_mapping_changed();
        let generation = crate::bkl::tlb_generation();

        // (3) その世代で隔離へ入れる。
        let mut held = 0usize;
        for frame in collected.iter().take(count).flatten() {
            if quarantine.push(*frame, generation) {
                held += 1;
            } else {
                leaked += 1;
            }
        }
        (held, leaked)
    }
}

/// direct map 越しにテーブルのエントリを読む。
///
/// # Safety
/// `table` を `direct_map` が覆っていること。`index` が 512 未満であること。
unsafe fn read_entry(direct_map: DirectMap, table: PhysAddr, index: usize) -> u64 {
    let base = direct_map.phys_to_virt(table).as_u64() as *const u64;
    // SAFETY: 呼び出し元契約。
    unsafe { base.add(index).read_volatile() }
}

/// direct map 越しにテーブルのエントリを書く。
///
/// # Safety
/// `table` を `direct_map` が覆っていること。`index` が 512 未満であること。
unsafe fn write_entry(direct_map: DirectMap, table: PhysAddr, index: usize, value: u64) {
    let base = direct_map.phys_to_virt(table).as_u64() as *mut u64;
    // SAFETY: 呼び出し元契約。
    unsafe { base.add(index).write_volatile(value) };
}

/// direct map 越しにテーブルを 0 で埋める。
///
/// # Safety
/// `table` を `direct_map` が覆っていること。
unsafe fn zero_table(direct_map: DirectMap, table: PhysAddr) {
    for index in 0..crate::paging::entry::ENTRIES_PER_TABLE {
        // SAFETY: 呼び出し元契約。添字は 512 未満。
        unsafe { write_entry(direct_map, table, index, 0) };
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
