//! 稼働中のページテーブルの読み戻し（M5-a-1）。
//!
//! **unsafe を含む。** CR3 が指すテーブルを実際に辿る。
//!
//! # なぜ [`super::table::PageTableBuilder`] と型を分けるのか
//!
//! `PageTableBuilder` は **CR3 に載る前**のテーブルを組み立てるためのもので、
//! 「TLB を気にしなくてよい」「まだ誰もこのテーブルで動いていない」という
//! 前提の上に成り立っている。切り替えの時点で TLB は丸ごと入れ替わるからである。
//!
//! こちらは**稼働中**のテーブルを扱う。前提が正反対で、
//!
//! - CR3 が自分を指している（`current()` は CR3 を**読んで**構築する。
//!   「切り替えたつもり」の値を使わない）
//! - 変更したら TLB を無効化しなければならない
//! - 変更の途中でも、実行中のコード自身のマッピングが壊れてはいけない
//!
//! 同じ型に両方を持たせると、構築時の前提が稼働中の操作へ静かに漏れる。
//! `graphics::FramebufferLayout` が「検証済みであること」を型で表しているのと
//! 同じ考え方で、ここでは「稼働中であること」を型で表す。
//!
//! # M5-a-1 の範囲
//!
//! **読み戻しだけ**を実装する。分割とアンマップは M5-a-2 で足す。
//! 検証手段を先に用意しておけば、M5-a-2 の結果を「それを行ったコードとは
//! 独立に」確かめられる。M4 で `sgdt` / `sidt` / IMR の読み戻しを先に用意した
//! のと同じ順序である。

use common::cpu;

use super::entry::{self, ADDR_MASK_TABLE};
use super::switch;

/// 翻訳の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Translation {
    /// 対応する物理アドレス（ページ内オフセットを加えた値）。
    pub phys: u64,
    /// どの大きさのページで翻訳されたか。
    pub page_size: PageSize,
    /// ページを指しているエントリの生の値。フラグの照合に使う。
    pub entry: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    Size4KiB,
    Size2MiB,
}

impl PageSize {
    pub const fn bytes(self) -> u64 {
        match self {
            PageSize::Size4KiB => entry::PAGE_SIZE_4K,
            PageSize::Size2MiB => entry::PAGE_SIZE_2M,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslateError {
    /// 仮想アドレスが正規形（canonical）でない。
    ///
    /// **「マップされていない」とは別の状態である。** 非正規アドレスは
    /// そもそも CPU が受け付けず、添字計算も意味を持たない。`None` に
    /// 丸めると「マップし忘れ」と区別がつかなくなる。
    NonCanonicalAddress,
    /// PDPT レベルで 1GiB ページ（PS=1）に当たった。**未対応。**
    ///
    /// ZaytOS は 1GiB ページを作らないが、確認せずに PD へ降りると
    /// **1GiB ページのアドレスを PD のアドレスとして解釈する**ため、
    /// 明示的に弾く。
    UnsupportedGiantPage,
}

/// 稼働中（CR3 が指している）のページテーブル。
pub struct ActivePageTable {
    pml4_phys: u64,
}

impl ActivePageTable {
    /// CR3 を**読んで**構築する。
    ///
    /// # Safety
    ///
    /// CR3 が指すページテーブルが恒等マッピングされており、その物理アドレスを
    /// そのままポインタとして読めること。ZaytOS は M2-d 以降このとおりに
    /// なっている。
    pub unsafe fn current() -> Self {
        Self {
            pml4_phys: switch::read_cr3() & ADDR_MASK_TABLE,
        }
    }

    pub const fn pml4_phys(&self) -> u64 {
        self.pml4_phys
    }

    /// `table_phys[index]` を読む。
    ///
    /// # Safety
    /// `table_phys` が有効なページテーブルフレームの物理アドレスで、
    /// 恒等マッピングにより読めること。`index < 512`。
    unsafe fn read(table_phys: u64, index: usize) -> u64 {
        // SAFETY: 呼び出し元契約を参照。読み取りのみ。
        unsafe { core::ptr::read_volatile((table_phys as *const u64).add(index)) }
    }

    /// 仮想アドレスを翻訳する。**実際のテーブルを辿る。**
    ///
    /// `Ok(None)` は「マップされていない」、`Err` は「そもそも扱えない
    /// アドレス」である。両者を区別する。
    ///
    /// この関数の目的は、分割やアンマップが意図どおり効いたかを
    /// **それを行ったコードとは独立に**確かめることである。期待値は
    /// 呼び出し側が別に持つこと（同じ計算で検算すると自己参照になる）。
    pub fn translate(&self, virt: u64) -> Result<Option<Translation>, TranslateError> {
        if !entry::is_canonical(virt) {
            return Err(TranslateError::NonCanonicalAddress);
        }

        // SAFETY: `current()` の契約により、PML4 以下のテーブルは恒等
        // マッピングで読める。添字はいずれも `& 0x1FF` で 512 未満。
        unsafe {
            let pml4e = Self::read(self.pml4_phys, entry::pml4_index(virt));
            if !entry::is_present(pml4e) {
                return Ok(None);
            }

            let pdpt = entry::table_address(pml4e);
            let pdpte = Self::read(pdpt, entry::pdpt_index(virt));
            if !entry::is_present(pdpte) {
                return Ok(None);
            }
            // **PDPT レベルの PS を必ず見る。** 見ずに降りると 1GiB ページの
            // アドレスを PD のアドレスとして扱ってしまう。
            if entry::is_huge(pdpte) {
                return Err(TranslateError::UnsupportedGiantPage);
            }

            let pd = entry::table_address(pdpte);
            let pde = Self::read(pd, entry::pd_index(virt));
            if !entry::is_present(pde) {
                return Ok(None);
            }
            if entry::is_huge(pde) {
                let base = entry::page_address_2m(pde);
                return Ok(Some(Translation {
                    phys: base + (virt & (entry::PAGE_SIZE_2M - 1)),
                    page_size: PageSize::Size2MiB,
                    entry: pde,
                }));
            }

            let pt = entry::table_address(pde);
            let pte = Self::read(pt, entry::pt_index(virt));
            if !entry::is_present(pte) {
                return Ok(None);
            }
            let base = entry::page_address_4k(pte);
            Ok(Some(Translation {
                phys: base + (virt & (entry::PAGE_SIZE_4K - 1)),
                page_size: PageSize::Size4KiB,
                entry: pte,
            }))
        }
    }
}

/// TLB の無効化が CR3 リロードで足りるかどうかの判定材料。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlbFlushPrecondition {
    /// CR4 の実測値。
    pub cr4: u64,
    /// CR4.PGE が有効か。
    pub page_global_enabled: bool,
}

/// CR3 リロードで TLB を全部追い出せるかを調べる。
///
/// # 「CR3 を書き直せば全部消える」が成立する条件
///
/// 成立するのは **CR4.PGE が無効か、あるいはどのエントリにも G ビットが
/// 立っていない**場合である。
///
/// **成立しない条件**: CR4.PGE が有効で、かつエントリに G ビットが立っている。
/// この組み合わせでは、そのページの翻訳は CR3 のリロードでも TLB に残り続ける
/// （グローバルページはアドレス空間の切り替えをまたいで生き残るための仕組み
/// なので、当然そうなる）。その場合は `invlpg` を個別に発行するか、
/// CR4.PGE を一度落として立て直す必要がある。
///
/// ZaytOS は `PageTableBuilder` でも `entry::PTE_GLOBAL` を一切立てていない
/// ため、PGE の状態に関わらず現状は成立する。ただし**将来 G ビットを使い
/// 始めたら、この前提は黙って崩れる**ので実測して記録しておく。
pub fn tlb_flush_precondition() -> TlbFlushPrecondition {
    let cr4 = cpu::read_cr4();
    TlbFlushPrecondition {
        cr4,
        page_global_enabled: cr4 & cpu::CR4_PAGE_GLOBAL_ENABLE != 0,
    }
}
