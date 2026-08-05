//! 解放したフレームを、TLB から消えるまで隔離する（S7-b）。
//!
//! # 何を守っているか
//!
//! ADR-0027 の Addendum の不変条件である——**フレームは、全コアの
//! `SEEN_GENERATION` が解放時の世代を追い越すまで配られない。**
//!
//! **写像を外して世代を上げても、他コアの TLB には古い翻訳が残りうる。** 外した
//! フレームを即座にアロケータへ返して再利用すると、**その窓で古い翻訳が生きた
//! ページを別用途に使われる。** ここはその窓を閉じる。
//!
//! # ack も新しいプロトコルも要らない
//!
//! カーネル入口はティックごとに BKL を取るので、**走っている全コアは 100Hz で必ず
//! `bkl::acquire` を通る。** したがって**隔離は遅くとも 1 ティックで解ける。**
//! 送るものも待つものも増えない。
//!
//! # 固定長である
//!
//! **ヒープを使わない。** このカーネルの様式に合うことと（`MAX_CPUS`・
//! `WORKER_COUNT`・フレームアロケータの範囲上限、いずれも固定である）、
//! **S6-c で測って確かめた「定常経路に解放されない確保は無い」を守るためである。**
//!
//! # この形は 2 案のうちの一方である
//!
//! 不変条件の実装には、**別の隔離リストで持つ**案（ここ）と、**フリーリストの
//! エントリに解放時の世代を刻む**案がある。**ADR はどちらも同じ不変条件だと述べて
//! おり、選択は対象外である**（`docs/roadmap.md` の S7）。**フレームアロケータの
//! データ構造の見直し（S7-d）で確定させる。** 刻む案を採るなら、このモジュールは
//! 落ちる。

use crate::frame_allocator::FrameAllocator;
use common::addr::PhysAddr;

/// 隔離できるフレームの本数。
///
/// **1 ティック（10ms）の間に解放した分だけを持てばよい。** 隔離はそれで解けるので、
/// ここが溜まり続けることはない。**プロセス 1 つの破棄で必要なのは PML4 と下位
/// テーブル数枚とユーザーページであり**、この桁で足りる見込みである。
///
/// **見込みであって、測っていない。** 溢れたときの扱いは [`Quarantine::push`] の doc。
pub const QUARANTINE_CAPACITY: usize = 64;

/// 隔離中の 1 件。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Held {
    frame: u64,
    /// 解放した時点の世代。**この世代を全コアが追い越したら配ってよい。**
    generation: u64,
}

/// 解放したフレームの隔離。
///
/// **`retired` を外から受け取る。** 「その世代が退いたか」を判定するのは
/// [`crate::bkl::generation_is_retired`] だが、**ここはそれを知らない形にしてある**
/// ——ホスト上の単体テストで、世代の進み方を自由に作って検査するためである
/// （ハード依存と純粋ロジックの分離）。
pub struct Quarantine {
    held: [Option<Held>; QUARANTINE_CAPACITY],
    /// 隔離が溢れて捨てられなかった回数。**観測用。**
    overflow_count: u64,
}

impl Quarantine {
    pub const fn new() -> Self {
        Self {
            held: [None; QUARANTINE_CAPACITY],
            overflow_count: 0,
        }
    }

    /// 隔離へ入れる。
    ///
    /// **溢れたら `false` を返し、呼び出し側がフレームを保持し続ける**（漏らす）。
    /// **アロケータへ返してはならない**——それが閉じようとしている窓そのものである。
    /// **漏らすほうが、早く配るより安全である。**
    #[must_use]
    pub fn push(&mut self, frame: PhysAddr, generation: u64) -> bool {
        let entry = Held {
            frame: frame.as_u64(),
            generation,
        };
        for slot in self.held.iter_mut() {
            if slot.is_none() {
                *slot = Some(entry);
                return true;
            }
        }
        self.overflow_count += 1;
        false
    }

    /// 退いた世代のフレームをアロケータへ返す。**返した本数を返す。**
    ///
    /// **BKL を保持したまま呼ぶこと。** 判定と再利用の間に他コアが割り込むと、
    /// 追い越しの判定が古くなる（ADR-0027 の Addendum の失効条件）。
    pub fn release_retired(
        &mut self,
        allocator: &mut FrameAllocator,
        retired: impl Fn(u64) -> bool,
    ) -> usize {
        let mut released = 0;
        for slot in self.held.iter_mut() {
            let Some(entry) = *slot else {
                continue;
            };
            if !retired(entry.generation) {
                continue;
            }
            let Some(frame) = PhysAddr::new(entry.frame) else {
                continue;
            };
            // **返せなかったら隔離に残す。** 消してしまうと、どこにも属さない
            // フレームができる。
            if allocator.deallocate_frame(frame).is_ok() {
                *slot = None;
                released += 1;
            }
        }
        released
    }

    /// 隔離中の本数。**観測用。**
    pub fn held_count(&self) -> usize {
        self.held.iter().filter(|slot| slot.is_some()).count()
    }

    /// 溢れて漏らした回数。**観測用。**
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count
    }
}

impl Default for Quarantine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_allocator::FrameAllocator;

    fn allocator_with(start_frame: u64, count: u64) -> FrameAllocator {
        let mut allocator = FrameAllocator::new();
        allocator
            .insert_free_range(start_frame, count)
            .expect("the test range fits");
        allocator
    }

    fn frame(index: u64) -> PhysAddr {
        PhysAddr::new(index * 4096).expect("aligned test frame")
    }

    #[test]
    fn a_frame_is_not_returned_while_its_generation_is_still_live() {
        let mut allocator = allocator_with(1000, 1);
        let mut quarantine = Quarantine::new();
        assert!(quarantine.push(frame(2000), 7));

        let released = quarantine.release_retired(&mut allocator, |_| false);

        assert_eq!(released, 0);
        assert_eq!(quarantine.held_count(), 1);
        assert_eq!(allocator.free_frame_count(), 1);
    }

    #[test]
    fn a_frame_is_returned_once_every_core_has_passed_its_generation() {
        let mut allocator = allocator_with(1000, 1);
        let mut quarantine = Quarantine::new();
        assert!(quarantine.push(frame(2000), 7));

        let released = quarantine.release_retired(&mut allocator, |generation| generation < 8);

        assert_eq!(released, 1);
        assert_eq!(quarantine.held_count(), 0);
        assert_eq!(allocator.free_frame_count(), 2);
    }

    #[test]
    fn only_the_retired_generations_are_returned() {
        let mut allocator = allocator_with(1000, 1);
        let mut quarantine = Quarantine::new();
        assert!(quarantine.push(frame(2000), 3));
        assert!(quarantine.push(frame(2001), 9));

        // 世代 8 まで退いた状態。**3 は退き、9 はまだである。**
        let released = quarantine.release_retired(&mut allocator, |generation| generation < 8);

        assert_eq!(released, 1);
        assert_eq!(quarantine.held_count(), 1);
        assert_eq!(allocator.free_frame_count(), 2);
    }

    #[test]
    fn overflowing_the_quarantine_leaks_instead_of_handing_the_frame_back() {
        let mut quarantine = Quarantine::new();
        for index in 0..QUARANTINE_CAPACITY {
            assert!(quarantine.push(frame(3000 + index as u64), 1));
        }

        // **溢れた 1 本は `false` になる。** 呼び出し側が保持し続ける（漏らす）。
        assert!(!quarantine.push(frame(9999), 1));
        assert_eq!(quarantine.held_count(), QUARANTINE_CAPACITY);
        assert_eq!(quarantine.overflow_count(), 1);
    }

    #[test]
    fn a_released_slot_can_be_reused_by_a_later_free() {
        let mut allocator = allocator_with(1000, 1);
        let mut quarantine = Quarantine::new();
        for index in 0..QUARANTINE_CAPACITY {
            assert!(quarantine.push(frame(3000 + index as u64), 1));
        }
        assert_eq!(
            quarantine.release_retired(&mut allocator, |_| true),
            QUARANTINE_CAPACITY
        );

        assert!(quarantine.push(frame(9999), 2));
        assert_eq!(quarantine.held_count(), 1);
    }
}
