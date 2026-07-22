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
use common::critical::InterruptGuard;

use crate::frame_allocator::{FrameAllocator, FRAME_SIZE};

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

/// 稼働中テーブルの書き換えが失敗した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapUpdateError {
    /// 仮想アドレスが正規形でない。
    NonCanonicalAddress,
    /// 途中の階層のエントリが不在で、そもそもマップされていない。
    NotMapped,
    /// PDPT レベルで 1GiB ページに当たった。**分割に対応していない。**
    ///
    /// ZaytOS は 1GiB ページを作らないが、確認せずに PD へ降りると
    /// 1GiB ページのアドレスを PD のアドレスとして解釈する。
    NotSplittable,
    /// 既に 4KiB でマップされている。分割の必要が無い。
    ///
    /// **エラーとして返す。** 「何もしなかった」を成功に丸めると、
    /// 呼び出し側が分割したつもりのまま先へ進む。
    AlreadySmall,
    /// G ビットが立っている。
    ///
    /// この手順は分割後の TLB 無効化を CR3 リロードで行う。G ビットの
    /// 付いた翻訳は CR3 リロードでも残るため、前提が成立しない。
    /// ZaytOS は G ビットを一切立てない（起動時に検証している）ので、
    /// ここに来るなら前提が崩れている。
    GlobalPagePresent,
    /// ページテーブル用のフレームを確保できなかった。
    OutOfFrames,
}

/// 分割の結果。呼び出し側が照合に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitOutcome {
    /// 分割前の PDE の生の値。
    pub huge_entry: u64,
    /// 新しく確保した PT の物理アドレス。
    pub table_phys: u64,
    /// 分割した 2MiB 領域の先頭仮想アドレス。
    pub base_virt: u64,
}

impl ActivePageTable {
    /// `table_phys[index]` に書く。
    ///
    /// # Safety
    /// [`Self::read`] と同じ契約に加え、`value` が正しい形式のエントリで
    /// あること。
    unsafe fn write(table_phys: u64, index: usize, value: u64) {
        // SAFETY: 呼び出し元契約を参照。
        unsafe {
            core::ptr::write_volatile((table_phys as *mut u64).add(index), value);
        }
    }

    /// `virt` を含む PD と、その中の添字を求める。
    ///
    /// PML4 → PDPT → PD と降りる途中の検査をここへ集約する。
    fn locate_pd(&self, virt: u64) -> Result<(u64, usize), MapUpdateError> {
        if !entry::is_canonical(virt) {
            return Err(MapUpdateError::NonCanonicalAddress);
        }
        // SAFETY: `current()` の契約により、テーブルは恒等マッピングで読める。
        unsafe {
            let pml4e = Self::read(self.pml4_phys, entry::pml4_index(virt));
            if !entry::is_present(pml4e) {
                return Err(MapUpdateError::NotMapped);
            }
            let pdpt = entry::table_address(pml4e);
            let pdpte = Self::read(pdpt, entry::pdpt_index(virt));
            if !entry::is_present(pdpte) {
                return Err(MapUpdateError::NotMapped);
            }
            // **PDPT レベルの PS を必ず見る。** 1GiB ページを PD として扱わない。
            if entry::is_huge(pdpte) {
                return Err(MapUpdateError::NotSplittable);
            }
            Ok((entry::table_address(pdpte), entry::pd_index(virt)))
        }
    }

    /// `virt` を含む 2MiB ページを 512 個の 4KiB ページへ分割する。
    ///
    /// 物理アドレスもキャッシュ属性も変わらない。**変わるのは粒度だけ**である。
    ///
    /// # 手順と、その順序が安全である根拠
    ///
    /// 1. PT を 1 枚確保してゼロ埋めする
    /// 2. 512 エントリをすべて書く
    /// 3. PD エントリを自然境界の 8 バイト書き込み 1 回で差し替える
    /// 4. CR3 をリロードして TLB を落とす
    ///
    /// 手順 2 の間、CPU から見えるマッピングは古い 2MiB エントリのままで
    /// 一切変わらない。書いている先は、まだどのページテーブルからも
    /// 参照されていない新しいフレームだからである。
    ///
    /// 手順 3 は自然境界に揃った 8 バイト 1 回の書き込みで、x86_64 では
    /// アトミックである。CPU がこのエントリを読むとき、古い 2MiB エントリか
    /// 新しい PT ポインタのどちらかしか観測せず、中途半端な値は見えない。
    /// **そしてどちらを観測しても、同じ物理アドレスへ同じ属性で翻訳される。**
    /// したがって、実行中のコード自身が載るページであっても、手順のどの
    /// 瞬間で割り込まれてもマッピングは有効なままである。
    ///
    /// **順序を逆にしてはならない。** PD エントリを先に差し替えると、
    /// ゼロ埋めしただけの PT を指す状態が生じ、その領域への参照が
    /// その場でフォルトする。
    ///
    /// 手順 4 に CR3 リロードを使うのは、512 本の翻訳を一度に置き換える
    /// ためである。`invlpg` を 512 回発行するより単純で、CR3 リロードで
    /// 全部落とせる条件（G ビットが無いこと）は手順 0 で確認している。
    ///
    /// # Safety
    ///
    /// - `frames` が返すフレームが恒等マッピングで読み書きできること
    /// - このテーブルが現に CR3 に載っていること（[`Self::current`] 由来）
    /// - 呼び出し時点で他の実行文脈がこのテーブルを書き換えていないこと。
    ///   割り込みに対しては内部で [`InterruptGuard`] を取る
    pub unsafe fn split_huge_page<const CAP: usize>(
        &mut self,
        virt: u64,
        frames: &mut FrameAllocator<CAP>,
    ) -> Result<SplitOutcome, MapUpdateError> {
        // 操作全体を割り込み禁止で囲む。M5-d でタイマ割り込みからページ
        // テーブルを触る経路が生まれるため、必要になってから足すのではなく
        // 最初から入れておく。
        let _guard = InterruptGuard::enter();

        let (pd, pd_index) = self.locate_pd(virt)?;
        // SAFETY: `locate_pd` が返す PD は有効なテーブルで、添字は 512 未満。
        let pde = unsafe { Self::read(pd, pd_index) };
        if !entry::is_present(pde) {
            return Err(MapUpdateError::NotMapped);
        }
        if !entry::is_huge(pde) {
            return Err(MapUpdateError::AlreadySmall);
        }
        if pde & entry::PTE_GLOBAL != 0 {
            return Err(MapUpdateError::GlobalPagePresent);
        }

        // 手順 1。ここまでページテーブルを一切変更していないので、
        // 確保に失敗しても状態は元のままである。
        let frame = frames.allocate_frame().ok_or(MapUpdateError::OutOfFrames)?;
        let table_phys = frame * FRAME_SIZE;
        // SAFETY: 今このアロケータから確保したばかりの、他の誰も参照して
        // いないフレームである。アロケータの空き範囲がすべてマップ済みで
        // あることは起動時に検証済みなので、恒等マッピングで書ける。
        unsafe {
            core::ptr::write_bytes(table_phys as *mut u8, 0, FRAME_SIZE as usize);
        }

        // 手順 2。値の計算は純粋ロジック側（ホストテストで固定）。
        let children = entry::split_children(pde);
        for (index, child) in children.iter().enumerate() {
            // SAFETY: `table_phys` は直前にゼロ埋めした自前のフレームで、
            // 添字は 512 未満。まだどこからも参照されていない。
            unsafe { Self::write(table_phys, index, *child) };
        }

        // 手順 3。8 バイト 1 回。
        // SAFETY: `pd` は有効な PD で添字は 512 未満。書く値は PT を指す
        // 正しい形式のエントリである。
        unsafe { Self::write(pd, pd_index, entry::table_entry_for_split(pde, table_phys)) };

        // 手順 4。
        // SAFETY: CR3 の値をそのまま書き戻すだけで、指す先は変えていない。
        unsafe { switch::switch_to(switch::read_cr3()) };

        Ok(SplitOutcome {
            huge_entry: pde,
            table_phys,
            base_virt: virt & !(entry::PAGE_SIZE_2M - 1),
        })
    }

    /// `virt` を含む 4KiB ページをアンマップする。無効化前の PTE を返す。
    ///
    /// # 物理フレームは解放しない
    ///
    /// そのフレームが他から参照されているかを、この関数は知らない。
    /// 解放の判断は呼び出し側が行う。返り値の PTE に物理アドレスが
    /// 入っているので、呼び出し側はそれを使える。
    ///
    /// 2MiB ページの中を指していた場合に分割で確保した PT も解放しない。
    /// 512 本のうち 1 本を消しただけで、残り 511 本は生きている。
    ///
    /// # TLB
    ///
    /// 変わるのは 1 本だけなので `invlpg` を使う。CR3 リロードだと
    /// 無関係な翻訳まで捨てて、以降のアクセスがすべて再ウォークになる。
    ///
    /// # Safety
    ///
    /// [`Self::split_huge_page`] と同じ。加えて、**アンマップした領域へ
    /// 以後アクセスしないことは呼び出し側の責任**である。触れば #PF になる。
    pub unsafe fn unmap_4kib(&mut self, virt: u64) -> Result<u64, MapUpdateError> {
        let _guard = InterruptGuard::enter();

        let (pd, pd_index) = self.locate_pd(virt)?;
        // SAFETY: `locate_pd` の契約による。
        let pde = unsafe { Self::read(pd, pd_index) };
        if !entry::is_present(pde) {
            return Err(MapUpdateError::NotMapped);
        }
        // 2MiB のままアンマップはできない。分割は呼び出し側が先に行う
        // （フレームアロケータを要求する関数を、この中から呼びたくない）。
        if entry::is_huge(pde) {
            return Err(MapUpdateError::AlreadySmall);
        }

        let pt = entry::table_address(pde);
        let pt_index = entry::pt_index(virt);
        // SAFETY: `pt` は PD が指す有効な PT で、添字は 512 未満。
        let pte = unsafe { Self::read(pt, pt_index) };
        if !entry::is_present(pte) {
            return Err(MapUpdateError::NotMapped);
        }

        // SAFETY: 同上。0 を書いて Present を落とす。
        unsafe { Self::write(pt, pt_index, 0) };
        // SAFETY: テーブルの書き換えが終わってから落とす。順序を逆にすると、
        // 古い翻訳が残ったままテーブルだけ変わった状態になる。
        unsafe { cpu::invalidate_tlb_entry(virt) };

        Ok(pte)
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
