//! SMP の下ごしらえ（S1）。**この段では情報を集めるだけで、AP は起こさない。**
//!
//! ここが持つのは、S3（AP 起こし）で要るが**S1 の時点でしか確保できないもの**である。
//! 現在はトランポリン用フレームだけが該当する。

use core::sync::atomic::{AtomicU64, Ordering};

use common::addr::PhysAddr;

use crate::frame_allocator::{FrameAllocator, FRAME_SIZE};

/// AP のトランポリンを置ける物理アドレスの上限（この値未満）。
///
/// # なぜ 1MiB 未満なのか
///
/// AP は SIPI（Startup IPI）で起こす。SIPI が運べるのは**8 ビットのベクタ**だけで、
/// AP はリアルモードで `vector << 12` から実行を始める。したがって開始アドレスは
/// 物理 `0x00000`〜`0xFF000` に限られる。**ZaytOS のカーネルイメージは物理
/// `0x100000`（ちょうど 1MiB）から始まる**ので、トランポリンはイメージの外、
/// 1MiB 未満に別途確保するしかない。
///
/// # なぜ `0xA0000` ではなく `0x9F000` なのか
///
/// 実測では、1MiB 未満の空きは `0x1000..0xA0000` の 159 フレームだけである
/// （`0xA0000` 以降はレガシー領域で、UEFI メモリマップに `EfiConventionalMemory`
/// として現れない）。上限を `0xA0000` にしても届く範囲としては足りるが、
/// **1 ページぶんの余裕を残す**ために `0x9F000` にしてある。トランポリンのコードが
/// 1 ページに収まらなかった場合、次のページへ跨ぐ余地が要るためである。
///
/// **収まらない場合の隣接ページの確保は、この段では扱わない。** S1 は 1 枚しか
/// 予約せず、隣が空いている保証も与えない（S3 の到達条件へ送った）。
pub const TRAMPOLINE_MAX_START: u64 = 0x9F000;

/// 予約が無いことを表す値。物理アドレス 0 はフレームアロケータが必ず除外するので
/// （ヌルポインタ対策）、有効な予約と衝突しない。
const NO_FRAME: u64 = 0;

/// S1-d で予約したトランポリン用フレームの物理アドレス。
static TRAMPOLINE_FRAME: AtomicU64 = AtomicU64::new(NO_FRAME);

/// トランポリン用フレームの予約に失敗した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrampolineError {
    /// 空きフレームが 1 枚も無い。
    NoFreeFrame,
    /// 取れたが 1MiB 未満ではない。**取れたフレームはアロケータへ返してある**
    /// ので、この失敗で空きが減ることはない。
    TooHigh { got: PhysAddr },
}

/// トランポリン用フレームを 1 枚予約する。
///
/// # 呼ぶ位置
///
/// **フレームアロケータのスモークテストの直後、本流ページテーブルの構築より前。**
/// `allocate_frame` は常に最小のフレーム番号から配るので、ここが「1MiB 未満が
/// まだ誰にも取られていない」唯一の地点である。これより後ろへ移すと、ページ
/// テーブルが低位から食っていくため、取れなくなる。
///
/// # 失敗しても停止しない
///
/// S1 は情報を集める段であり、AP はまだ起こさない。ここで停止すると、現在
/// 単一コアで動いているカーネルが「トランポリン用の 1 枚が取れない」だけで
/// 起動しなくなり、機能的な後退になる。**失敗は大きく報告して継続する。**
/// 致命として扱うのは S3（AP 起こし）である。
pub fn reserve_trampoline_frame<const CAP: usize>(
    allocator: &mut FrameAllocator<CAP>,
) -> Result<PhysAddr, TrampolineError> {
    let Some(frame) = allocator.allocate_frame() else {
        return Err(TrampolineError::NoFreeFrame);
    };
    if frame.as_u64() >= TRAMPOLINE_MAX_START {
        // 取ったものを返す。失敗で空きが減らないようにする。
        let _ = allocator.deallocate_frame(frame);
        return Err(TrampolineError::TooHigh { got: frame });
    }
    TRAMPOLINE_FRAME.store(frame.as_u64(), Ordering::Relaxed);
    Ok(frame)
}

/// 予約済みのトランポリン用フレーム。まだ予約していなければ `None`。
///
/// **S1 の時点では誰も呼ばない。** それでも `dead_code` にならないのは `pub` だから
/// であって、使われているからではない（公開範囲が広いと未使用が見えない、という
/// 一般則をここでは意図的に使っている）。**S3 で実際に読まれることを、その段の
/// 到達条件にしてある**（`roadmap.md`）。そうしないと、使い忘れても誰も気づかない。
pub fn trampoline_frame() -> Option<PhysAddr> {
    match TRAMPOLINE_FRAME.load(Ordering::Relaxed) {
        NO_FRAME => None,
        value => PhysAddr::new(value),
    }
}

/// 予約したフレームが SIPI のベクタとして表せるか（4KiB 境界にあるか）。
///
/// `allocate_frame` はフレーム単位で配るので、境界から外れることは通常起きない。
/// **検査というより、SIPI のベクタ計算（`vector << 12`）が成立する前提を
/// コードの形で残すためのものである。**
pub fn is_sipi_addressable(frame: PhysAddr) -> bool {
    frame.as_u64().is_multiple_of(FRAME_SIZE) && frame.as_u64() < TRAMPOLINE_MAX_START
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 高位のフレームしか持たないアロケータ。**失敗経路を判定側だけ閉じるため**の
    /// もので、起動が継続することまでは確かめられない（それは S3 で致命へ格上げ
    /// するときに見る）。
    fn allocator_with_only_high_frames() -> FrameAllocator<4> {
        let mut allocator = FrameAllocator::<4>::new();
        // 物理 1MiB（フレーム番号 0x100）から 16 枚。
        allocator.insert_free_range(0x100, 16).unwrap();
        allocator
    }

    #[test]
    fn a_high_only_allocator_is_rejected() {
        let mut allocator = allocator_with_only_high_frames();
        let error = reserve_trampoline_frame(&mut allocator).unwrap_err();
        match error {
            TrampolineError::TooHigh { got } => assert_eq!(got.as_u64(), 0x100000),
            other => panic!("expected TooHigh, got {other:?}"),
        }
        // 予約は成立していない。
        assert_eq!(trampoline_frame(), None);
        // 取ったフレームは返してあるので、空きは減っていない。
        assert_eq!(allocator.free_frame_count(), 16);
    }

    #[test]
    fn an_empty_allocator_reports_no_free_frame() {
        let mut allocator = FrameAllocator::<4>::new();
        assert_eq!(
            reserve_trampoline_frame(&mut allocator).unwrap_err(),
            TrampolineError::NoFreeFrame
        );
    }

    #[test]
    fn a_low_frame_is_sipi_addressable() {
        assert!(is_sipi_addressable(PhysAddr::new(0x1000).unwrap()));
        assert!(is_sipi_addressable(PhysAddr::new(0x9E000).unwrap()));
    }

    #[test]
    fn the_limit_itself_is_not_addressable() {
        // 上限は「この値未満」なので、境界そのものは弾く。
        assert!(!is_sipi_addressable(
            PhysAddr::new(TRAMPOLINE_MAX_START).unwrap()
        ));
    }

    #[test]
    fn a_frame_above_one_mib_is_not_addressable() {
        assert!(!is_sipi_addressable(PhysAddr::new(0x100000).unwrap()));
    }
}
