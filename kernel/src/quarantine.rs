//! 解放したフレームを、TLB から消えるまで隔離する（S7-b）。
//!
//! # 何を守っているか
//!
//! ADR-0027 の Addendum の不変条件である——**フレームは、全コアの
//! `SEEN_GENERATION` が解放時の世代を追い越すまで配られない。**
//!
//! **マッピングを外して世代を上げても、他コアの TLB には古い翻訳が残りうる。** 外した
//! フレームを即座にアロケータへ返して再利用すると、**そのウィンドウで古い翻訳が生きた
//! ページを別用途に使われる。** ここはそのウィンドウを閉じる。
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

use crate::arch::x86_64::AddressSpace;
use crate::frame_allocator::FrameAllocator;
use common::addr::{DirectMap, PhysAddr};

/// 隔離できるフレームの本数。
///
/// **1 ティック（10ms）の間に解放した分だけを持てばよい。** 隔離はそれで解けるので、
/// ここが溜まり続けることはない。
///
/// # 「この桁で足りる」は偽だった（B-d。2026-09-11）
///
/// **以前ここは「プロセス 1 つの破棄で必要なのは PML4 と下位テーブル数枚と
/// ユーザーページであり、この桁で足りる見込みである」と書き、「見込みであって、
/// 測っていない」と断っていた。** **B-d で初めて偽になった。**
///
/// **`/bin/ttfglyph` はフォントを読むために 512 KiB のヒープを取る**——
/// **実測で 142 フレームを消費し、64 しか隔離できず、78 枚を漏らした**
/// （**破棄の会計がその場で `DestroyAccounting` として示した**。
/// **黙って漏れなかったのは、会計が在ったからである**）。
///
/// **256 へ上げた。** **`brk` が伸ばせる上限はユーザースタックの下端までで
/// あり**（`crate::userland::HEAP_LIMIT`。約 8 MiB）、**どんな固定容量でも
/// 原理的には溢れうる。** **溢れは漏れとして必ず示される**ので、**ここは
/// 「測った値に余裕を足す」形のままにする**——**穴を持つアロケータも、
/// ヒープを返さずに終わるプログラムも、まだ無い。**
///
/// 溢れたときの扱いは [`Quarantine::push`] の doc。
pub const QUARANTINE_CAPACITY: usize = 256;

/// アドレス空間を壊すときに、外したフレームを一時的に置く場所（B-d で静的にした。2026-10-02 に、
/// ページテーブルの置き場からここへ移した）。
///
/// # なぜスタックに置かないか
///
/// **`Option<PhysAddr>` は 16 バイトで、容量に比例してスタックを食う。**
/// **B-d で隔離の容量を 64 から 256 へ上げたとき、これが 1 KiB から 4 KiB へ
/// 育ち、遠征スタックの高水位が半分を越えて起動が止まった**（実測）。
/// **静的に置けば、容量をいくつにしても遠征スタックは 1 バイトも増えない。**
///
/// # 同時に 2 つ走らない
///
/// **[`retire_address_space`] は BKL の内側でしか呼べない**（引数の `BklGuard` が型で示す）。
/// **入れ子にもならない**——壊す処理は、ほかの空間を壊す処理を呼ばない。
static mut RETIRING_FRAMES: [Option<PhysAddr>; QUARANTINE_CAPACITY] = [None; QUARANTINE_CAPACITY];

/// アドレス空間を壊し、**下位で使っていたフレームをすべて隔離へ入れる**（S7-d。2026-10-02 に、順序を持つ側を
/// ページテーブルの置き場からここへ移した）。
///
/// **アロケータへ直接は返さない。** ほかのコアの変換の控え（TLB）に古い翻訳が残りうるので、
/// **`ADR-0027` の Addendum の不変条件どおり、世代が退くまで隔離する。**
///
/// # 順序が要である
///
/// (1) マッピングを外し（[`AddressSpace::detach`]）、(2) 外し終えてから世代を上げ、(3) その世代で隔離へ入れる。
///
/// **上げてから外すと、上げた直後に控えを捨てたコアが、まだ生きているマッピングを読み直しうる。**
/// **外し終えてから上げれば、その世代以降に捨てたコアは、外れた後の状態しか見ていない。**
/// **判定が `>=` で足りるのはこの順序による**（[`crate::bkl::generation_is_retired`]）。
///
/// 返すのは（隔離へ入れた本数, 漏らした本数）。漏らすのは、一時の置き場か隔離に入り切らなかった分である。
///
/// # Safety
///
/// - **この空間がどのコアでも稼働していないこと。** 稼働中の表を壊すと、そのコアは次の翻訳で死ぬ。
/// - `direct_map` が、この空間の表を覆っていること。
/// - BKL を持っていること（`_guard` が示す）。**マッピングの変更と世代の更新は、BKL の内側でしか行わない。**
pub unsafe fn retire_address_space(
    space: AddressSpace,
    direct_map: DirectMap,
    quarantine: &mut Quarantine,
    _guard: &crate::bkl::BklGuard,
) -> (usize, usize) {
    // SAFETY: BKL を保持している（`_guard`）。**壊す処理は入れ子にならないので、この参照が生きている間、
    // ほかに触る者は居ない**（[`RETIRING_FRAMES`] の doc）。
    let frames: &mut [Option<PhysAddr>; QUARANTINE_CAPACITY] =
        unsafe { &mut *core::ptr::addr_of_mut!(RETIRING_FRAMES) };

    // (1) マッピングを外し、外したフレームを集める。
    // SAFETY: 呼び出し元の契約（稼働していない空間、覆っている direct map、BKL の内側）。
    let (count, mut leaked) = unsafe { space.detach(direct_map, frames) };

    // (2) ここまででマッピングは外れている。**外し終えてから上げる。**
    crate::bkl::note_mapping_changed();
    let generation = crate::bkl::tlb_generation();

    // (3) その世代で隔離へ入れる。
    let mut held = 0usize;
    for frame in frames.iter().take(count).flatten() {
        if quarantine.push(*frame, generation) {
            held += 1;
        } else {
            leaked += 1;
        }
    }
    (held, leaked)
}

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
    /// **アロケータへ返してはならない**——それが閉じようとしているウィンドウそのものである。
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

    /// 中身を空にする（B-d）。
    ///
    /// # なぜ `new()` の代入で済ませないか
    ///
    /// **静的な置き場を使い回すためである。** **`*self = Self::new()` と書くと、
    /// 512 バイトを超える値がいったんスタックに積まれうる**——**それを避ける
    /// ために静的へ移したので、代入で戻しては意味が無い。** **その場で潰す。**
    pub fn reset(&mut self) {
        for slot in self.held.iter_mut() {
            *slot = None;
        }
        self.overflow_count = 0;
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
