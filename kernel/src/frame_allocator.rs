//! 物理フレームアロケータ（M2-c）。
//!
//! データ構造として「空き物理フレームの範囲（start_frame, frame_count）を
//! 固定長配列で保持するソート済みリスト」を採用している。
//!
//! 選択理由:
//! - **ビットマップ**（1フレーム=1ビット）は、フレーム数分のビット列を
//!   格納する領域が別途必要になる。現段階ではヒープがなく（M2-e 未実装）、
//!   そのビットマップ自身をどこに置くかという別の鶏卵問題が生じる。
//! - **侵入型フリーリスト**（各空きフレームの先頭に次フレームへの
//!   ポインタを書き込む）は追加のメモリを必要としない点は良いが、
//!   実際にフレームへポインタを書き込む操作が常に unsafe になり、
//!   ロジックの大部分をホスト上でテストできなくなる。
//! - **範囲リスト**は、UEFI メモリマップが最初から「範囲」の集合として
//!   与えられることと相性が良く、範囲の個数は実測で高々 100〜200 程度
//!   であるため固定長配列に収まる。範囲の追加・結合・分割は生ポインタを
//!   一切使わない純粋なデータ操作であり、ハードウェア依存からの分離と
//!   ホスト `cargo test` での検証をそのまま満たせる。
//!
//! ADR 化するかどうかは、この説明を見た人間の判断に委ねる。

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::addr::PhysAddr;

use crate::memory_map::memory_type;

pub const FRAME_SIZE: u64 = 4096;

/// 実運用で使う既定の容量。実測（QEMU + OVMF, 256MiB 割り当て）では
/// `EfiConventionalMemory` のエントリ数は数十程度であり、余裕を見て
/// この値にしている。
pub const DEFAULT_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameRange {
    start_frame: u64,
    frame_count: u64,
}

impl FrameRange {
    const fn end_frame(&self) -> u64 {
        self.start_frame + self.frame_count
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAllocatorError {
    /// 空き範囲の数が固定長配列の容量を超えた。
    CapacityExceeded,
}

/// 除外した物理ページの、メモリ型別の内訳。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExclusionBreakdown {
    pub reserved_pages: u64,
    pub loader_code_pages: u64,
    pub loader_data_pages: u64,
    pub boot_services_code_pages: u64,
    pub boot_services_data_pages: u64,
    pub runtime_services_code_pages: u64,
    pub runtime_services_data_pages: u64,
    pub unusable_pages: u64,
    pub acpi_reclaim_pages: u64,
    pub acpi_nvs_pages: u64,
    pub mmio_pages: u64,
    pub mmio_port_space_pages: u64,
    pub pal_code_pages: u64,
    pub persistent_pages: u64,
    pub unaccepted_pages: u64,
    pub vendor_reserved_pages: u64,
    /// 型は `EfiConventionalMemory` だったが、物理アドレス 0 を含むため
    /// 個別に除外したページ数（ADR-0010 とは別件、ヌルポインタ対策）。
    pub null_page_excluded_pages: u64,
    /// 既知のいずれの型にも一致しなかったページ数（通常は発生しない）。
    pub unknown_type_pages: u64,
}

impl ExclusionBreakdown {
    pub const fn total_pages(&self) -> u64 {
        self.reserved_pages
            + self.loader_code_pages
            + self.loader_data_pages
            + self.boot_services_code_pages
            + self.boot_services_data_pages
            + self.runtime_services_code_pages
            + self.runtime_services_data_pages
            + self.unusable_pages
            + self.acpi_reclaim_pages
            + self.acpi_nvs_pages
            + self.mmio_pages
            + self.mmio_port_space_pages
            + self.pal_code_pages
            + self.persistent_pages
            + self.unaccepted_pages
            + self.vendor_reserved_pages
            + self.null_page_excluded_pages
            + self.unknown_type_pages
    }

    fn add(&mut self, ty: u32, pages: u64) {
        if pages == 0 {
            return;
        }
        match ty {
            memory_type::RESERVED => self.reserved_pages += pages,
            memory_type::LOADER_CODE => self.loader_code_pages += pages,
            memory_type::LOADER_DATA => self.loader_data_pages += pages,
            memory_type::BOOT_SERVICES_CODE => self.boot_services_code_pages += pages,
            memory_type::BOOT_SERVICES_DATA => self.boot_services_data_pages += pages,
            memory_type::RUNTIME_SERVICES_CODE => self.runtime_services_code_pages += pages,
            memory_type::RUNTIME_SERVICES_DATA => self.runtime_services_data_pages += pages,
            memory_type::UNUSABLE => self.unusable_pages += pages,
            memory_type::ACPI_RECLAIM => self.acpi_reclaim_pages += pages,
            memory_type::ACPI_NVS => self.acpi_nvs_pages += pages,
            memory_type::MMIO => self.mmio_pages += pages,
            memory_type::MMIO_PORT_SPACE => self.mmio_port_space_pages += pages,
            memory_type::PAL_CODE => self.pal_code_pages += pages,
            memory_type::PERSISTENT => self.persistent_pages += pages,
            memory_type::UNACCEPTED => self.unaccepted_pages += pages,
            t if t >= memory_type::VENDOR_RESERVED_START => self.vendor_reserved_pages += pages,
            _ => self.unknown_type_pages += pages,
        }
    }
}

/// 初期化時にシリアルへ出力すべき統計情報。
#[derive(Debug, Clone, Copy)]
pub struct FrameAllocatorStats {
    pub free_frame_count: u64,
    pub exclusions: ExclusionBreakdown,
}

impl FrameAllocatorStats {
    pub const fn free_mib(&self) -> u64 {
        (self.free_frame_count * FRAME_SIZE) / (1024 * 1024)
    }

    pub const fn excluded_mib(&self) -> u64 {
        (self.exclusions.total_pages() * FRAME_SIZE) / (1024 * 1024)
    }
}

/// 空き物理フレームを、ソート済み・隣接結合済みの範囲リストとして管理する。
///
/// `CAP` は保持できる範囲（連続していない空き領域の断片）の最大数。
/// 通常の利用は [`DEFAULT_CAPACITY`] を使う。テストでは容量超過の挙動を
/// 検証するために小さい値を明示的に指定する。
pub struct FrameAllocator<const CAP: usize = DEFAULT_CAPACITY> {
    ranges: [FrameRange; CAP],
    range_count: usize,
}

impl<const CAP: usize> FrameAllocator<CAP> {
    pub const fn new() -> Self {
        Self {
            ranges: [FrameRange {
                start_frame: 0,
                frame_count: 0,
            }; CAP],
            range_count: 0,
        }
    }

    /// 現在の空きフレーム総数。個別のカウンタを持たず、範囲リストから
    /// その都度計算する（範囲の個数は高々 CAP 程度で軽量、かつ
    /// カウンタと実体がズレる不整合の可能性を構造的に排除できる）。
    pub fn free_frame_count(&self) -> u64 {
        self.ranges[..self.range_count]
            .iter()
            .map(|r| r.frame_count)
            .sum()
    }

    pub fn free_range_count(&self) -> usize {
        self.range_count
    }

    /// 現在の空き範囲を `(start_frame, frame_count)` として列挙する。
    /// ページング側で「アロケータが配りうるフレームはすべてマップ済みか」
    /// を検証する用途を想定した、読み取り専用のアクセサ。
    pub fn free_ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.ranges[..self.range_count]
            .iter()
            .map(|r| (r.start_frame, r.frame_count))
    }

    /// 連続して確保できる最大のフレーム数。
    ///
    /// 連続確保が失敗したときの診断に使う。空きフレーム総数と併せて見ることで、
    /// 「空き自体が不足している」のか「空きはあるが連続領域が足りない
    /// （断片化）」のかを区別できる。
    pub fn largest_contiguous_free_frames(&self) -> u64 {
        self.free_ranges()
            .map(|(_, frame_count)| frame_count)
            .max()
            .unwrap_or(0)
    }

    /// 空きフレーム範囲を1つ追加する。前後の既存範囲と隣接・重複していれば
    /// 結合する。呼び出し側は、追加する範囲が他の空き範囲と重複しないこと
    /// （ある物理フレームを二重に空き扱いしないこと）を保証すること。
    pub fn insert_free_range(
        &mut self,
        start_frame: u64,
        frame_count: u64,
    ) -> Result<(), FrameAllocatorError> {
        if frame_count == 0 {
            return Ok(());
        }
        let new_end = start_frame + frame_count;

        let mut insert_at = self.range_count;
        for i in 0..self.range_count {
            if self.ranges[i].start_frame > start_frame {
                insert_at = i;
                break;
            }
        }

        // 直前の範囲と隣接/重複していれば結合し、そこからさらに直後とも
        // 結合できるか確認する（例: 隙間をちょうど埋める場合）。
        if insert_at > 0 && self.ranges[insert_at - 1].end_frame() >= start_frame {
            let prev = &mut self.ranges[insert_at - 1];
            let merged_end = prev.end_frame().max(new_end);
            prev.frame_count = merged_end - prev.start_frame;
            self.try_merge_forward(insert_at - 1);
            return Ok(());
        }

        // 直後の範囲と隣接/重複していれば結合。
        if insert_at < self.range_count && new_end >= self.ranges[insert_at].start_frame {
            let next = &mut self.ranges[insert_at];
            let merged_start = start_frame.min(next.start_frame);
            let merged_end = new_end.max(next.end_frame());
            next.start_frame = merged_start;
            next.frame_count = merged_end - merged_start;
            return Ok(());
        }

        // どちらとも結合できなければ新規範囲として挿入する。
        if self.range_count >= CAP {
            return Err(FrameAllocatorError::CapacityExceeded);
        }
        for i in (insert_at..self.range_count).rev() {
            self.ranges[i + 1] = self.ranges[i];
        }
        self.ranges[insert_at] = FrameRange {
            start_frame,
            frame_count,
        };
        self.range_count += 1;
        Ok(())
    }

    /// `ranges[idx]` が結合によって拡張された結果、直後の範囲とも隣接/
    /// 重複するようになっていないかを確認し、必要なら結合する。
    fn try_merge_forward(&mut self, idx: usize) {
        if idx + 1 < self.range_count
            && self.ranges[idx].end_frame() >= self.ranges[idx + 1].start_frame
        {
            let merged_end = self.ranges[idx]
                .end_frame()
                .max(self.ranges[idx + 1].end_frame());
            self.ranges[idx].frame_count = merged_end - self.ranges[idx].start_frame;
            for i in (idx + 1)..(self.range_count - 1) {
                self.ranges[i] = self.ranges[i + 1];
            }
            self.range_count -= 1;
        }
    }

    /// 空きフレームを1つ確保する。確保順は先頭（最小のフレーム番号）から。
    pub fn allocate_frame(&mut self) -> Option<PhysAddr> {
        if self.range_count == 0 {
            return None;
        }
        let frame = self.ranges[0].start_frame;
        self.ranges[0].start_frame += 1;
        self.ranges[0].frame_count -= 1;
        if self.ranges[0].frame_count == 0 {
            for i in 0..(self.range_count - 1) {
                self.ranges[i] = self.ranges[i + 1];
            }
            self.range_count -= 1;
        }
        PhysAddr::from_frame_number(frame)
    }

    /// フレームを解放し、空きリストへ戻す（隣接する空き範囲とは結合される）。
    pub fn deallocate_frame(&mut self, addr: PhysAddr) -> Result<(), FrameAllocatorError> {
        let frame = addr.frame_number();
        self.insert_free_range(frame, 1)
    }

    /// 連続する `count` 個のフレームを1つの範囲として確保する（first-fit）。
    /// `count` 以上の大きさを持つ最初の空き範囲の先頭から切り出す。
    /// 十分な大きさの「連続した」空き範囲が見つからなければ `None` を返す
    /// （複数の小さい範囲の合計が `count` 以上でも、それだけでは確保しない）。
    ///
    /// M2-e のヒープ初期アリーナ（連続領域が必要）と、将来 M3 で想定される
    /// フレームバッファ用の大きな連続確保（ヒープを経由しない経路）の
    /// 両方から使われることを想定している。
    /// 境界を揃えた連続フレームを確保する。
    ///
    /// `align_frames` はフレーム単位の境界で、2 のべき乗であること。
    /// 2MiB 境界に揃えたいなら 512 を渡す。
    ///
    /// # なぜ必要か
    ///
    /// [`Self::allocate_contiguous`] は境界を保証しない。M5-a-2 の分割検証は
    /// **2MiB ページとしてマップされている領域**を対象にする必要があり、
    /// `plan::resolve_pages` が 2MiB ページを作るのは 2MiB 境界に揃った
    /// 範囲だけである。境界の揃っていない 512 フレームを取ると、そこは
    /// 4KiB に分解されていて分割対象にならない。
    ///
    /// 4MiB 取って中の揃った部分を使う、という手もあるが、余りが
    /// 恒久的に失われる。ここで揃えて取れば無駄が出ない。
    pub fn allocate_contiguous_aligned(
        &mut self,
        count: u64,
        align_frames: u64,
    ) -> Option<PhysAddr> {
        if count == 0 || align_frames == 0 || !align_frames.is_power_of_two() {
            return None;
        }
        for i in 0..self.range_count {
            let start = self.ranges[i].start_frame;
            let end = start + self.ranges[i].frame_count;
            // 範囲内で最初に境界へ揃うフレーム。
            let aligned = start.next_multiple_of(align_frames);
            if aligned.checked_add(count).is_none_or(|last| last > end) {
                continue;
            }

            let leading = aligned - start;
            let trailing = end - (aligned + count);

            if leading == 0 && trailing == 0 {
                for j in i..(self.range_count - 1) {
                    self.ranges[j] = self.ranges[j + 1];
                }
                self.range_count -= 1;
            } else if leading == 0 {
                self.ranges[i].start_frame = aligned + count;
                self.ranges[i].frame_count = trailing;
            } else if trailing == 0 {
                self.ranges[i].frame_count = leading;
            } else {
                // 前後に空きが残る。範囲が 1 つ増えるので容量を確認する。
                if self.range_count == CAP {
                    return None;
                }
                self.ranges[i].frame_count = leading;
                for j in (i + 1..self.range_count).rev() {
                    self.ranges[j + 1] = self.ranges[j];
                }
                self.ranges[i + 1] = FrameRange {
                    start_frame: aligned + count,
                    frame_count: trailing,
                };
                self.range_count += 1;
            }
            return PhysAddr::from_frame_number(aligned);
        }
        None
    }

    pub fn allocate_contiguous(&mut self, count: u64) -> Option<PhysAddr> {
        if count == 0 {
            return None;
        }
        for i in 0..self.range_count {
            if self.ranges[i].frame_count >= count {
                let start = self.ranges[i].start_frame;
                if self.ranges[i].frame_count == count {
                    for j in i..(self.range_count - 1) {
                        self.ranges[j] = self.ranges[j + 1];
                    }
                    self.range_count -= 1;
                } else {
                    self.ranges[i].start_frame += count;
                    self.ranges[i].frame_count -= count;
                }
                return PhysAddr::from_frame_number(start);
            }
        }
        None
    }
}

impl<const CAP: usize> Default for FrameAllocator<CAP> {
    fn default() -> Self {
        Self::new()
    }
}

/// `BootInfo.memory_map` が指す生のメモリマップから、物理フレーム
/// アロケータを構築する。
///
/// 「空き」として扱うかどうかは、必ず [`crate::memory_map::classify`] を
/// 経由して判定する（[`crate::memory_map::RegionPolicy::Free`] のみが
/// 空き）。**ページング側（M2-d, `crate::paging`）も同じ `classify` を
/// 使って「マップすべき領域」を判定しており、判定基準を独自に持たない。**
/// これにより「アロケータが配ったフレームが新しいページテーブルに
/// マップされていない」という不整合を構造的に防いでいる。
///
/// さらに物理アドレス 0 を含むページは、型に関わらず除外する（ヌル
/// ポインタ参照がバグ検出不能になることを防ぐ）。
///
/// kernel 本体・`BootInfo`・メモリマップバッファはいずれも
/// `EfiLoaderData` として確保されている（`bootloader/src/loader.rs` で
/// 確認済み）ため、型ベースのこの判定だけで自動的に除外される。
/// 個別のアドレス範囲を特別扱いする必要はない。
/// カーネル全体で 1 つのフレームアロケータの実体（S11-3。`ADR-0030`）。
///
/// # 値で渡さない。**動かすと 4104 バイトがスタックへ乗る**
///
/// **`FrameRange` が 16 バイト、`CAP` が 256 なので、この型は 4104 バイトである。**
/// **借り手へ値で返す形にしたら、カーネルスタックがガードページへ落ちた**（実測）——
/// `deferred-decisions.md` の「大きなスタック配列とガード幅」が言うとおり、
/// **ガード幅 1 ページの前提は「スタック上の単一の物が 4096 バイト以下」である。**
///
/// **そこで実体はここに置いたまま、借り手には `&'static mut` を渡す。**
/// **コピーが起きない。**
static mut STORAGE: FrameAllocator = FrameAllocator::new();

/// 実体が預けられているか（S11-3）。**起動の最初は空である。**
static PRESENT: AtomicBool = AtomicBool::new(false);

/// 貸し出し中か（S11-3）。**排他はこの旗が持つ。**
static ON_LOAN: AtomicBool = AtomicBool::new(false);

/// [`take`] が成功した回数（S11-3）。
static TAKEN: AtomicU64 = AtomicU64::new(0);
/// [`give_back`] が呼ばれた回数（S11-3）。
static RETURNED: AtomicU64 = AtomicU64::new(0);

/// 起動時に一度だけ預ける（S11-3）。
///
/// # なぜ [`give_back`] と分けるのか
///
/// **最初の 1 回は「返却」ではない。** アロケータは `kernel_main` のローカルとして
/// 生まれ、**借りずに預けられる。** [`give_back`] で数えると、
/// **貸し借りの釣り合いが最初から 1 ずれる**（実測で `lent out 5, given back 6` が出た）。
///
/// # Safety
///
/// **起動シーケンスから一度だけ呼ぶこと。** まだ誰も借りていない時点で呼ぶこと。
pub unsafe fn deposit(allocator: FrameAllocator) {
    // SAFETY: 呼び出し元契約により、まだ誰も借りていない単一実行文脈である。
    unsafe {
        *core::ptr::addr_of_mut!(STORAGE) = allocator;
    }
    PRESENT.store(true, Ordering::SeqCst);
}

/// アロケータを借りる（S11-3）。**返すのは呼び出し側の責任である。**
///
/// # 排他は `compare_exchange` が持つ
///
/// **`load` してから `store` する形にしない。** それだと**2 つの実行文脈が同時に
/// 通り抜けられる。** いまは単一コアの直線なので実害は出ないが、
/// **S13 で I/O 待ちが入り、割り込みハンドラから取る経路ができた時点で壊れる。**
/// **`compare_exchange` なら、その時点でも契約が壊れない。**
///
/// **`None` は「今は借りられない」である。** 起動シーケンスは単一コアの直線なので、
/// **そこで `None` が返るのは返し忘れを意味する。**
pub fn take() -> Option<&'static mut FrameAllocator> {
    if !PRESENT.load(Ordering::SeqCst) {
        return None;
    }
    if ON_LOAN
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return None;
    }
    TAKEN.fetch_add(1, Ordering::SeqCst);
    // SAFETY: **`compare_exchange` が成功した者だけがここへ来る。**
    // 旗は [`give_back`] が戻すまで立ったままなので、**この `&mut` は唯一である。**
    Some(unsafe { &mut *core::ptr::addr_of_mut!(STORAGE) })
}

/// アロケータを返す（S11-3）。
///
/// **借りた `&'static mut` を渡す。** 渡した側はそれ以降使えない
/// （**返した後に使う形が型で作れない**）。
pub fn give_back(_allocator: &'static mut FrameAllocator) {
    RETURNED.fetch_add(1, Ordering::SeqCst);
    ON_LOAN.store(false, Ordering::SeqCst);
}

/// 貸し借りの回数と、今この場に在るか（S11-3）。**判定行に出す。**
///
/// **取り出した回数と返した回数が一致していれば、その時点で返し忘れは無い。**
pub fn lending_counts() -> (u64, u64, bool) {
    (
        TAKEN.load(Ordering::SeqCst),
        RETURNED.load(Ordering::SeqCst),
        !ON_LOAN.load(Ordering::SeqCst),
    )
}

pub fn build(
    raw: &[u8],
    descriptor_size: u64,
) -> Result<(FrameAllocator<DEFAULT_CAPACITY>, FrameAllocatorStats), &'static str> {
    let mut allocator = FrameAllocator::<DEFAULT_CAPACITY>::new();
    let mut exclusions = ExclusionBreakdown::default();

    for entry in crate::memory_map::parse_entries(raw, descriptor_size)? {
        if entry.page_count == 0 {
            continue;
        }
        if crate::memory_map::classify(entry.memory_type) != crate::memory_map::RegionPolicy::Free {
            exclusions.add(entry.memory_type, entry.page_count);
            continue;
        }

        let mut start_frame = entry.phys_start / FRAME_SIZE;
        let mut frame_count = entry.page_count;

        if start_frame == 0 {
            exclusions.null_page_excluded_pages += 1;
            start_frame = 1;
            frame_count -= 1;
        }
        if frame_count == 0 {
            continue;
        }

        allocator.insert_free_range(start_frame, frame_count).map_err(|_| {
            "frame allocator free-range capacity exceeded while building from the UEFI memory map"
        })?;
    }

    let stats = FrameAllocatorStats {
        free_frame_count: allocator.free_frame_count(),
        exclusions,
    };
    Ok((allocator, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト内でフレーム番号から期待値の物理アドレスを作る補助。
    fn f(frame: u64) -> PhysAddr {
        PhysAddr::from_frame_number(frame).unwrap()
    }

    /// 境界を揃えた確保。範囲の途中から取り、前後に空きが残る場合。
    #[test]
    fn aligned_allocation_splits_the_range_in_three() {
        let mut allocator = FrameAllocator::<8>::new();
        // フレーム 3..2000。2MiB 境界（512 フレーム）に揃うのは 512。
        allocator.insert_free_range(3, 1997).unwrap();

        let start = allocator.allocate_contiguous_aligned(512, 512).unwrap();
        assert_eq!(start, f(512), "最初に境界へ揃うフレーム");

        // 前（3..512）と後ろ（1024..2000）が空きとして残る。
        let ranges: Vec<(u64, u64)> = allocator.free_ranges().collect();
        assert_eq!(ranges, vec![(3, 509), (1024, 976)]);
        assert_eq!(allocator.free_frame_count(), 509 + 976);
    }

    /// 先頭が既に揃っている場合は前の空きが出ない。
    #[test]
    fn aligned_allocation_without_a_leading_remainder() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(512, 1024).unwrap();

        let start = allocator.allocate_contiguous_aligned(512, 512).unwrap();
        assert_eq!(start, f(512));
        assert_eq!(
            allocator.free_ranges().collect::<Vec<_>>(),
            vec![(1024, 512)]
        );
    }

    /// ちょうど使い切る場合は範囲そのものが消える。
    #[test]
    fn aligned_allocation_consuming_the_whole_range() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(1024, 512).unwrap();

        assert_eq!(
            allocator.allocate_contiguous_aligned(512, 512),
            Some(f(1024))
        );
        assert_eq!(allocator.free_range_count(), 0);
        assert_eq!(allocator.free_frame_count(), 0);
    }

    /// **境界に揃わないだけで足りない場合を、確保できたことにしない。**
    ///
    /// 空きフレーム数は足りているが、揃った位置から連続で取れない。
    /// ここを見落とすと、揃っていないアドレスを返して 2MiB ページでは
    /// ない領域を分割対象にしてしまう。
    #[test]
    fn aligned_allocation_fails_when_only_the_alignment_is_missing() {
        let mut allocator = FrameAllocator::<8>::new();
        // 600 フレームあるが、512 の境界は 512 の 1 箇所だけで、
        // そこから 512 フレームは取れない（600 - (512 - 100) = 88 しかない）。
        allocator.insert_free_range(100, 600).unwrap();

        assert_eq!(allocator.free_frame_count(), 600);
        assert_eq!(allocator.allocate_contiguous_aligned(512, 512), None);
        // 失敗しても空き状態を壊さないこと。
        assert_eq!(allocator.free_frame_count(), 600);
        assert_eq!(
            allocator.free_ranges().collect::<Vec<_>>(),
            vec![(100, 600)]
        );
    }

    /// 引数の検証。0 と 2 のべき乗でない境界は拒否する。
    #[test]
    fn aligned_allocation_rejects_bad_arguments() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 4096).unwrap();

        assert_eq!(allocator.allocate_contiguous_aligned(0, 512), None);
        assert_eq!(allocator.allocate_contiguous_aligned(512, 0), None);
        assert_eq!(allocator.allocate_contiguous_aligned(512, 3), None);
        assert_eq!(allocator.free_frame_count(), 4096);
    }

    /// 範囲が 1 つ増えるため、容量が尽きていたら確保しない。
    ///
    /// 握りつぶして「揃っていない位置」を返すより、取れないと言う方がよい。
    #[test]
    fn aligned_allocation_respects_the_capacity_limit() {
        let mut allocator = FrameAllocator::<2>::new();
        allocator.insert_free_range(3, 1997).unwrap();
        allocator.insert_free_range(4000, 10).unwrap();
        assert_eq!(allocator.free_range_count(), 2);

        // 前後に空きが残る形になるので範囲が 3 つ必要だが、容量は 2。
        assert_eq!(allocator.allocate_contiguous_aligned(512, 512), None);
        assert_eq!(allocator.free_frame_count(), 1997 + 10);
    }
    use crate::memory_map::test_support::build_map_bytes;

    // --- FrameAllocator 単体のテスト（範囲リストの挙動） ---

    #[test]
    fn new_allocator_is_empty() {
        let allocator = FrameAllocator::<8>::new();
        assert_eq!(allocator.free_frame_count(), 0);
        assert_eq!(allocator.free_range_count(), 0);
    }

    #[test]
    fn insert_then_allocate_returns_frames_from_the_range() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(10, 3).unwrap();
        assert_eq!(allocator.free_frame_count(), 3);

        assert_eq!(allocator.allocate_frame(), Some(f(10)));
        assert_eq!(allocator.allocate_frame(), Some(f(11)));
        assert_eq!(allocator.free_frame_count(), 1);
        assert_eq!(allocator.allocate_frame(), Some(f(12)));
        assert_eq!(allocator.free_frame_count(), 0);
        assert_eq!(allocator.allocate_frame(), None);
    }

    #[test]
    fn deallocate_returns_a_frame_to_the_free_set() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(10, 1).unwrap();
        let frame = allocator.allocate_frame().unwrap();
        assert_eq!(allocator.free_frame_count(), 0);

        allocator.deallocate_frame(frame).unwrap();
        assert_eq!(allocator.free_frame_count(), 1);
        assert_eq!(allocator.allocate_frame(), Some(frame));
    }

    #[test]
    fn adjacent_free_regions_are_coalesced_into_one_range() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 5).unwrap(); // [0,5)
        allocator.insert_free_range(5, 5).unwrap(); // [5,10) touches the first
        assert_eq!(allocator.free_range_count(), 1);
        assert_eq!(allocator.free_frame_count(), 10);
    }

    #[test]
    fn insert_that_exactly_fills_a_gap_merges_both_neighbors() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 5).unwrap(); // [0,5)
        allocator.insert_free_range(8, 5).unwrap(); // [8,13)
        assert_eq!(allocator.free_range_count(), 2);

        allocator.insert_free_range(5, 3).unwrap(); // [5,8) fills the gap exactly
        assert_eq!(allocator.free_range_count(), 1);
        assert_eq!(allocator.free_frame_count(), 13);
    }

    #[test]
    fn deallocate_coalesces_adjacent_singleton_frames() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 10).unwrap();
        let a = allocator.allocate_frame().unwrap();
        let b = allocator.allocate_frame().unwrap();
        assert_eq!(allocator.free_range_count(), 1); // [2,10)

        // a, b (連続した2フレーム) を解放すると、既存の空き範囲と結合される。
        allocator.deallocate_frame(a).unwrap();
        allocator.deallocate_frame(b).unwrap();
        assert_eq!(allocator.free_range_count(), 1);
        assert_eq!(allocator.free_frame_count(), 10);
    }

    #[test]
    fn non_adjacent_regions_stay_as_separate_ranges() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 2).unwrap();
        allocator.insert_free_range(10, 2).unwrap();
        assert_eq!(allocator.free_range_count(), 2);
        assert_eq!(allocator.free_frame_count(), 4);
    }

    #[test]
    fn capacity_exceeded_is_reported_as_an_error() {
        let mut allocator = FrameAllocator::<2>::new();
        allocator.insert_free_range(0, 1).unwrap();
        allocator.insert_free_range(10, 1).unwrap();
        // 3つ目の非隣接範囲は容量(2)を超える。
        let result = allocator.insert_free_range(20, 1);
        assert_eq!(result, Err(FrameAllocatorError::CapacityExceeded));
        // 失敗時も既存の状態は破壊されていない。
        assert_eq!(allocator.free_frame_count(), 2);
    }

    #[test]
    fn allocate_contiguous_carves_out_the_front_of_a_range() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(10, 20).unwrap(); // [10, 30)
        assert_eq!(allocator.allocate_contiguous(5), Some(f(10)));
        assert_eq!(allocator.free_frame_count(), 15);
        assert_eq!(allocator.free_range_count(), 1);
        // 残りは [15, 30) のまま連続している。
        assert_eq!(allocator.allocate_frame(), Some(f(15)));
    }

    #[test]
    fn allocate_contiguous_exact_size_removes_the_range() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(10, 5).unwrap();
        allocator.insert_free_range(100, 5).unwrap();
        assert_eq!(allocator.allocate_contiguous(5), Some(f(10)));
        assert_eq!(allocator.free_range_count(), 1);
        assert_eq!(allocator.free_frame_count(), 5);
    }

    #[test]
    fn allocate_contiguous_does_not_combine_separate_ranges() {
        let mut allocator = FrameAllocator::<8>::new();
        // 合計は10だが、どちらの範囲も単独では8に満たない。
        allocator.insert_free_range(0, 4).unwrap();
        allocator.insert_free_range(100, 4).unwrap();
        assert_eq!(allocator.allocate_contiguous(8), None);
        assert_eq!(allocator.free_frame_count(), 8); // 何も消費していない
    }

    #[test]
    fn allocate_contiguous_picks_first_fitting_range_not_largest() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 6).unwrap(); // 先に見つかる、十分な大きさ
        allocator.insert_free_range(100, 100).unwrap(); // より大きいが後ろにある
        assert_eq!(allocator.allocate_contiguous(5), Some(f(0)));
    }

    #[test]
    fn allocate_contiguous_zero_count_returns_none() {
        let mut allocator = FrameAllocator::<8>::new();
        allocator.insert_free_range(0, 10).unwrap();
        assert_eq!(allocator.allocate_contiguous(0), None);
        assert_eq!(allocator.free_frame_count(), 10);
    }

    #[test]
    fn boundary_near_top_of_address_space_does_not_overflow() {
        let mut allocator = FrameAllocator::<4>::new();
        // **表現できる物理アドレス空間（52 ビット）の末尾付近。**
        // 以前は 2^64 の末尾付近で試していたが、T-2c で確保 API が
        // `PhysAddr` を返すようになり、52 ビットを超えるフレームは
        // そもそも物理アドレスとして表せなくなった。境界が移っている。
        const PHYS_LIMIT: u64 = 0x0010_0000_0000_0000;
        let near_top_frame = (PHYS_LIMIT / FRAME_SIZE) - 2;
        allocator.insert_free_range(near_top_frame, 2).unwrap();
        assert_eq!(allocator.free_frame_count(), 2);
        assert_eq!(allocator.allocate_frame(), Some(f(near_top_frame)));
        assert_eq!(allocator.allocate_frame(), Some(f(near_top_frame + 1)));
    }

    /// **52 ビットを超えるフレームは確保できない。**
    ///
    /// T-2c で確保 API が `PhysAddr` を返すようになった結果の新しい不変条件。
    /// 表せないアドレスを返すくらいなら、確保できないと言うほうがよい。
    /// ページテーブルへ書いた時点で CPU が弾く値を、その前に止められる。
    #[test]
    fn frames_beyond_the_physical_address_limit_cannot_be_allocated() {
        let mut allocator = FrameAllocator::<4>::new();
        const PHYS_LIMIT: u64 = 0x0010_0000_0000_0000;
        allocator
            .insert_free_range(PHYS_LIMIT / FRAME_SIZE, 2)
            .unwrap();

        assert_eq!(allocator.free_frame_count(), 2, "空きとしては数えられる");
        assert_eq!(
            allocator.allocate_frame(),
            None,
            "物理アドレスとして表せないので確保できない"
        );
    }

    // --- build() のテスト（メモリ型ポリシーの適用） ---

    #[test]
    fn build_treats_only_conventional_memory_as_free() {
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x100000, 4),
                (memory_type::BOOT_SERVICES_DATA, 0x200000, 4),
                (memory_type::LOADER_DATA, 0x300000, 4),
                (memory_type::RESERVED, 0x400000, 4),
            ],
            48,
        );
        let (allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(allocator.free_frame_count(), 4);
        assert_eq!(stats.free_frame_count, 4);
    }

    #[test]
    fn build_excludes_loader_data_adjacent_to_conventional() {
        // kernel 本体・BootInfo・メモリマップバッファは LOADER_DATA として
        // 確保されている（bootloader/src/loader.rs で確認済み）。これが
        // CONVENTIONAL な空き領域のすぐ隣にあっても、空きとして誤って
        // 取り込まれない（型で自動的に守られる）ことを確認する。
        let bytes = build_map_bytes(
            &[
                (memory_type::LOADER_DATA, 0x100000, 6), // kernel/BootInfo/mmap buffer 相当
                (memory_type::CONVENTIONAL, 0x106000, 4), // すぐ隣の空き領域
            ],
            48,
        );
        let (allocator, stats) = build(&bytes, 48).unwrap();

        assert_eq!(allocator.free_frame_count(), 4);
        assert_eq!(stats.exclusions.loader_data_pages, 6);
        // LOADER_DATA の範囲にあるフレーム番号は一切割り当てられない。
        let loader_data_start_frame = 0x100000 / FRAME_SIZE;
        let loader_data_end_frame = 0x106000 / FRAME_SIZE;
        let mut allocator = allocator;
        while let Some(frame) = allocator.allocate_frame() {
            assert!(
                !(loader_data_start_frame..loader_data_end_frame).contains(&frame.frame_number())
            );
        }
    }

    #[test]
    fn build_excludes_boot_services_data_per_adr_0010() {
        let bytes = build_map_bytes(
            &[
                (memory_type::BOOT_SERVICES_DATA, 0x100000, 8),
                (memory_type::CONVENTIONAL, 0x108000, 2),
            ],
            48,
        );
        let (_allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(stats.exclusions.boot_services_data_pages, 8);
        assert_eq!(stats.free_frame_count, 2);
    }

    #[test]
    fn build_excludes_the_page_containing_physical_address_zero() {
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0, 4)], 48);
        let (mut allocator, stats) = build(&bytes, 48).unwrap();

        assert_eq!(stats.exclusions.null_page_excluded_pages, 1);
        assert_eq!(allocator.free_frame_count(), 3);
        // フレーム 0 (物理アドレス 0) は絶対に配られない。
        while let Some(frame) = allocator.allocate_frame() {
            assert_ne!(frame, f(0));
        }
    }

    #[test]
    fn build_excludes_entire_entry_when_it_is_only_the_null_page() {
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0, 1)], 48);
        let (allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(stats.exclusions.null_page_excluded_pages, 1);
        assert_eq!(allocator.free_frame_count(), 0);
    }

    #[test]
    fn build_ignores_zero_page_count_descriptors() {
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x100000, 0),
                (memory_type::CONVENTIONAL, 0x200000, 4),
            ],
            48,
        );
        let (_allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(stats.free_frame_count, 4);
    }

    #[test]
    fn build_coalesces_adjacent_conventional_entries_from_the_map() {
        let bytes = build_map_bytes(
            &[
                (memory_type::CONVENTIONAL, 0x100000, 4), // frames 256..260
                (memory_type::CONVENTIONAL, 0x104000, 4), // frames 260..264 (隣接)
            ],
            48,
        );
        let (allocator, stats) = build(&bytes, 48).unwrap();
        assert_eq!(stats.free_frame_count, 8);
        assert_eq!(allocator.free_range_count(), 1);
    }

    #[test]
    fn build_works_with_descriptor_size_other_than_48() {
        let bytes = build_map_bytes(&[(memory_type::CONVENTIONAL, 0x100000, 4)], 64);
        let (_allocator, stats) = build(&bytes, 64).unwrap();
        assert_eq!(stats.free_frame_count, 4);
    }

    #[test]
    fn build_reports_exclusion_breakdown_by_type() {
        let bytes = build_map_bytes(
            &[
                (memory_type::RESERVED, 0x1000, 1),
                (memory_type::LOADER_CODE, 0x2000, 2),
                (memory_type::LOADER_DATA, 0x3000, 3),
                (memory_type::BOOT_SERVICES_CODE, 0x4000, 4),
                (memory_type::BOOT_SERVICES_DATA, 0x5000, 5),
                (memory_type::RUNTIME_SERVICES_CODE, 0x6000, 6),
                (memory_type::RUNTIME_SERVICES_DATA, 0x7000, 7),
                (memory_type::UNUSABLE, 0x8000, 8),
                (memory_type::ACPI_RECLAIM, 0x9000, 9),
                (memory_type::ACPI_NVS, 0xA000, 10),
                (memory_type::MMIO, 0xB000, 11),
                (memory_type::MMIO_PORT_SPACE, 0xC000, 12),
                (memory_type::PAL_CODE, 0xD000, 13),
                (memory_type::PERSISTENT, 0xE000, 14),
                (memory_type::UNACCEPTED, 0xF000, 15),
                (0x8000_0000, 0x1000_0000, 16), // OS ベンダー予約 (型の値)
            ],
            48,
        );
        let (_allocator, stats) = build(&bytes, 48).unwrap();
        let e = &stats.exclusions;
        assert_eq!(e.reserved_pages, 1);
        assert_eq!(e.loader_code_pages, 2);
        assert_eq!(e.loader_data_pages, 3);
        assert_eq!(e.boot_services_code_pages, 4);
        assert_eq!(e.boot_services_data_pages, 5);
        assert_eq!(e.runtime_services_code_pages, 6);
        assert_eq!(e.runtime_services_data_pages, 7);
        assert_eq!(e.unusable_pages, 8);
        assert_eq!(e.acpi_reclaim_pages, 9);
        assert_eq!(e.acpi_nvs_pages, 10);
        assert_eq!(e.mmio_pages, 11);
        assert_eq!(e.mmio_port_space_pages, 12);
        assert_eq!(e.pal_code_pages, 13);
        assert_eq!(e.persistent_pages, 14);
        assert_eq!(e.unaccepted_pages, 15);
        assert_eq!(e.vendor_reserved_pages, 16);
        assert_eq!(stats.free_frame_count, 0);
    }

    #[test]
    fn free_mib_and_excluded_mib_convert_frames_to_mib() {
        let stats = FrameAllocatorStats {
            free_frame_count: 256, // 256 * 4096 = 1 MiB
            exclusions: ExclusionBreakdown {
                reserved_pages: 512, // 2 MiB
                ..Default::default()
            },
        };
        assert_eq!(stats.free_mib(), 1);
        assert_eq!(stats.excluded_mib(), 2);
    }
}
