//! ページテーブルへの実際の書き込み（M2-d, d-1）。
//!
//! unsafe を用いてページテーブル
//! フレームを確保・初期化し、x86_64 の PML4/PDPT/PD/PT エントリを
//! 直接書き込む。ここは意図的にホスト `cargo test` の対象にしていない
//! （実在しない物理アドレスへの生ポインタアクセスになるため）。マップ
//! すべき範囲・ページサイズの計算（テスト可能な部分）は [`super::plan`]
//! に分離してある。
//!
//! # 前提（呼び出し側が保証すること）
//! - 現在の CPU は、恒等マッピング（仮想アドレス = 物理アドレス）された
//!   ページテーブルの下で動作している（ADR-0009 で検証済みの UEFI 由来
//!   のページテーブル）。このモジュールが新しく確保するフレームや、
//!   `phys_addr` として渡される値は、すべてこの既存の恒等マッピングを
//!   通じてそのままポインタとして読み書きできることを前提にしている。
//! - この時点ではまだ CR3 は切り替えない（d-1 の範囲）。

use common::addr::{DirectMap, PhysAddr, VirtAddr};

use crate::frame_allocator::{FrameAllocator, FRAME_SIZE};

const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITABLE: u64 = 1 << 1;
const PTE_PCD: u64 = 1 << 4;
/// PD レベルでのみ設定する（2MiB ページ）。PDPT レベルで設定すると
/// 1GiB ページの意味になるため、このモジュールでは PDPT レベルには
/// 絶対に立てない。
const PTE_PS: u64 = 1 << 7;
/// エントリからアドレス部分（ビット12〜51）を取り出すマスク。
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageTableError {
    /// フレームアロケータの空きが尽きた。
    OutOfFrames,
    /// 中間テーブルのはずのエントリが、実は huge page (PS=1) だった
    /// （本来到達しないはずの不整合。黙って上書きせず fail-fast する）。
    UnexpectedHugePageEntry,
}

/// `table_phys[index]` の生エントリを読む。
///
/// # Safety
/// `table_phys` は 4KiB アラインされた、有効なページテーブルフレームの
/// 物理（= 恒等マッピングにより仮想として使える）アドレスであること。
/// `index < 512`。
unsafe fn read_entry(direct_map: DirectMap, table_phys: PhysAddr, index: usize) -> u64 {
    // SAFETY: 呼び出し元契約を参照。読み取りのみで副作用はない。物理
    // アドレスからポインタへは direct map を通す。
    unsafe {
        core::ptr::read_volatile(
            direct_map
                .phys_to_virt(table_phys)
                .as_ptr::<u64>()
                .add(index),
        )
    }
}

/// `table_phys[index]` に生エントリを書く。
///
/// # Safety
/// [`read_entry`] と同様の契約に加え、`value` が正しい形式の
/// ページテーブルエントリであること。
unsafe fn write_entry(direct_map: DirectMap, table_phys: PhysAddr, index: usize, value: u64) {
    // SAFETY: 呼び出し元契約を参照。変換は direct map 経由。
    unsafe {
        core::ptr::write_volatile(
            direct_map
                .phys_to_virt(table_phys)
                .as_mut_ptr::<u64>()
                .add(index),
            value,
        );
    }
}

/// 新規ページテーブル（PML4/PDPT/PD/PT）を構築するビルダー。
pub struct PageTableBuilder<'a, const CAP: usize> {
    frames: &'a mut FrameAllocator<CAP>,
    pml4_phys: PhysAddr,
    frames_used: u64,
    /// テーブルのフレームを読み書きするための窓。
    ///
    /// **構築時に受け取った値を保持する。** higher-half 移行では、
    /// 恒等の窓で新しいテーブルを組み立ててから CR3 を切り替え、
    /// 高位へ飛んでから恒等を外す。つまり組み立ての間は「古い窓」が
    /// 正しく、切り替え後の [`super::active::ActivePageTable`] は
    /// 「新しい窓」で辿る。**2 つの窓が同時に正しい期間がある**ため、
    /// グローバルな `direct_map()` を毎回引く形では表せない
    /// （`docs/deferred-decisions.md`）。
    direct_map: DirectMap,
}

impl<'a, const CAP: usize> PageTableBuilder<'a, CAP> {
    /// 新しい PML4 テーブル（ゼロ初期化済み）を確保して構築を開始する。
    pub fn new(
        frames: &'a mut FrameAllocator<CAP>,
        direct_map: DirectMap,
    ) -> Result<Self, PageTableError> {
        let pml4_phys = Self::alloc_zeroed_table(frames, direct_map)?;
        Ok(Self {
            frames,
            pml4_phys,
            frames_used: 1,
            direct_map,
        })
    }

    pub const fn pml4_phys(&self) -> PhysAddr {
        self.pml4_phys
    }

    pub const fn frames_used(&self) -> u64 {
        self.frames_used
    }

    fn alloc_zeroed_table(
        frames: &mut FrameAllocator<CAP>,
        direct_map: DirectMap,
    ) -> Result<PhysAddr, PageTableError> {
        let phys = frames.allocate_frame().ok_or(PageTableError::OutOfFrames)?;
        // SAFETY: `phys` は今このフレームアロケータから確保したばかりの、
        // 他の誰も参照していないフレームである。フレームアロケータの
        // 空き集合は `crate::memory_map::classify` の `Free` 判定に
        // 由来し、`super::plan` が同じ判定を使ってこの領域も恒等
        // マッピング対象に含めているため、現在有効な（UEFI 由来の）
        // ページテーブル下でこのアドレスへアクセスできる（ADR-0009）。
        // 1 ページ分をゼロ初期化することで、未初期化のゴミが
        // Present ビットの立った不正なエントリとして解釈されるのを
        // 防ぐ。
        unsafe {
            core::ptr::write_bytes(
                direct_map.phys_to_virt(phys).as_mut_ptr::<u8>(),
                0,
                FRAME_SIZE as usize,
            );
        }
        Ok(phys)
    }

    /// `table_phys[index]` が指す子テーブルの物理アドレスを返す。
    /// まだ存在しなければ新規にゼロ初期化して確保する。
    fn ensure_child(
        &mut self,
        table_phys: PhysAddr,
        index: usize,
    ) -> Result<PhysAddr, PageTableError> {
        // SAFETY: `table_phys` はこのビルダーが構築した、有効な
        // ページテーブルフレーム。`index` は呼び出し元 (本モジュール内)
        // が `& 0x1FF` で 512 未満に制限している。
        let existing = unsafe { read_entry(self.direct_map, table_phys, index) };
        if existing & PTE_PRESENT != 0 {
            if existing & PTE_PS != 0 {
                // 本来ここに到達しないはず（huge page として使われている
                // スロットを中間テーブルとして扱おうとしている）。
                // 黙って上書きせず fail-fast する。
                return Err(PageTableError::UnexpectedHugePageEntry);
            }
            return Ok(PhysAddr::new_const(existing & ADDR_MASK));
        }
        let child_phys = Self::alloc_zeroed_table(self.frames, self.direct_map)?;
        self.frames_used += 1;
        // SAFETY: `table_phys`/`index` は上記と同じ契約。新規に確保した
        // `child_phys` を Present + Writable な中間エントリとして書く
        // （中間テーブル自体はキャッシュ属性を特別扱いしない）。
        unsafe {
            write_entry(
                self.direct_map,
                table_phys,
                index,
                child_phys.as_u64() | PTE_PRESENT | PTE_WRITABLE,
            );
        }
        Ok(child_phys)
    }

    /// 恒等マッピング（仮想 = 物理）で 1 ページを割り付ける。
    ///
    /// `huge` が真なら 2MiB ページ（PD レベルに `PTE_PS` を立てる）、
    /// 偽なら 4KiB ページ（PT レベルまで辿る）。NX ビット（bit 63）は
    /// 意図的に立てない: EFER.NXE を有効化していないこの段階で立てると
    /// 予約ビット違反のページフォルトになる。権限の細分化は次の独立した
    /// ステップに送る。
    pub fn map_page(
        &mut self,
        phys_addr: PhysAddr,
        huge: bool,
        cacheable: bool,
    ) -> Result<(), PageTableError> {
        // 恒等マッピングの計画なので、物理アドレスをそのまま仮想アドレスと
        // して添字を取る。**higher-half 移行ではここが変わる。** 計画が
        // 物理で書かれている一方、添字は仮想アドレスから取るためである。
        let virt = VirtAddr::new(phys_addr.as_u64())
            .expect("an identity-mapped physical address is canonical");
        let pdpt = self.ensure_child(self.pml4_phys, virt.pml4_index())?;
        let pd = self.ensure_child(pdpt, virt.pdpt_index())?;

        let mut flags = PTE_PRESENT | PTE_WRITABLE;
        if !cacheable {
            flags |= PTE_PCD;
        }

        if huge {
            // SAFETY: `pd` はこのビルダーが構築した、有効な PD テーブル。
            // `pd_index` は 512 未満。`phys_addr` は 2MiB アラインされて
            // いることを呼び出し元（`super::plan::resolve_pages` の核部分）
            // が保証する。PS ビットは PD レベルにのみ立てている。
            unsafe {
                write_entry(
                    self.direct_map,
                    pd,
                    virt.pd_index(),
                    (phys_addr.as_u64() & ADDR_MASK) | flags | PTE_PS,
                );
            }
        } else {
            let pt = self.ensure_child(pd, virt.pd_index())?;
            // SAFETY: `pt` はこのビルダーが構築した、有効な PT テーブル。
            unsafe {
                write_entry(
                    self.direct_map,
                    pt,
                    virt.pt_index(),
                    (phys_addr.as_u64() & ADDR_MASK) | flags,
                );
            }
        }
        Ok(())
    }
}
