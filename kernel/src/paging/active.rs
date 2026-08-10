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

use common::addr::{DirectMap, PhysAddr, VirtAddr};

use super::entry;
use super::switch;

/// 翻訳の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Translation {
    /// 対応する物理アドレス（ページ内オフセットを加えた値）。
    pub phys: PhysAddr,
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
    /// PDPT レベルで 1GiB ページ（PS=1）に当たった。**未対応。**
    ///
    /// ZaytOS は 1GiB ページを作らないが、確認せずに PD へ降りると
    /// **1GiB ページのアドレスを PD のアドレスとして解釈する**ため、
    /// 明示的に弾く。
    UnsupportedGiantPage,
}

/// 稼働中（CR3 が指している）のページテーブル。
pub struct ActivePageTable {
    pml4_phys: PhysAddr,
    /// テーブルのフレームを読むための窓。
    ///
    /// **構築時に受け取った値を保持する。** higher-half 移行では、
    /// 恒等の窓で組み立てたテーブルへ CR3 を切り替え、高位へ飛んでから
    /// 恒等を外す。その過程で「古い窓」と「新しい窓」が同時に正しい期間が
    /// あるため、グローバルな `direct_map()` を毎回引くのではなく、
    /// どちらの窓を使うかを呼び出し側が決められる形にしてある
    /// （`docs/deferred-decisions.md`）。
    direct_map: DirectMap,
}

impl ActivePageTable {
    /// CR3 を**読んで**構築する。
    ///
    /// # Safety
    ///
    /// CR3 が指すページテーブルが恒等マッピングされており、その物理アドレスを
    /// そのままポインタとして読めること。ZaytOS は M2-d 以降このとおりに
    /// なっている。
    pub unsafe fn current(direct_map: DirectMap) -> Self {
        Self {
            pml4_phys: switch::read_cr3(),
            direct_map,
        }
    }

    pub const fn pml4_phys(&self) -> PhysAddr {
        self.pml4_phys
    }

    /// `table_phys[index]` を読む。
    ///
    /// # Safety
    /// `table_phys` が有効なページテーブルフレームの物理アドレスで、
    /// 恒等マッピングにより読めること。`index < 512`。
    unsafe fn read(&self, table_phys: PhysAddr, index: usize) -> u64 {
        // SAFETY: 呼び出し元契約を参照。読み取りのみ。物理アドレスから
        // ポインタへは direct map を通す（生の値をポインタにする経路は
        // 型として存在しない）。
        unsafe {
            core::ptr::read_volatile(
                self.direct_map
                    .phys_to_virt(table_phys)
                    .as_ptr::<u64>()
                    .add(index),
            )
        }
    }

    /// 仮想アドレスを翻訳する。**実際のテーブルを辿る。**
    ///
    /// `Ok(None)` は「マップされていない」、`Err` は「そもそも扱えない
    /// アドレス」である。両者を区別する。
    ///
    /// この関数の目的は、分割やアンマップが意図どおり効いたかを
    /// **それを行ったコードとは独立に**確かめることである。期待値は
    /// 呼び出し側が別に持つこと（同じ計算で検算すると自己参照になる）。
    pub fn translate(&self, virt: VirtAddr) -> Result<Option<Translation>, TranslateError> {
        // SAFETY: `current()` の契約により、PML4 以下のテーブルは恒等
        // マッピングで読める。添字はいずれも `& 0x1FF` で 512 未満。
        unsafe {
            let pml4e = self.read(self.pml4_phys, entry::pml4_index(virt));
            if !entry::is_present(pml4e) {
                return Ok(None);
            }

            let pdpt = entry::table_address(pml4e);
            let pdpte = self.read(pdpt, entry::pdpt_index(virt));
            if !entry::is_present(pdpte) {
                return Ok(None);
            }
            // **PDPT レベルの PS を必ず見る。** 見ずに降りると 1GiB ページの
            // アドレスを PD のアドレスとして扱ってしまう。
            if entry::is_huge(pdpte) {
                return Err(TranslateError::UnsupportedGiantPage);
            }

            let pd = entry::table_address(pdpte);
            let pde = self.read(pd, entry::pd_index(virt));
            if !entry::is_present(pde) {
                return Ok(None);
            }
            if entry::is_huge(pde) {
                let base = entry::page_address_2m(pde);
                return Ok(Some(Translation {
                    phys: base
                        .checked_add(virt.as_u64() & (entry::PAGE_SIZE_2M - 1))
                        .expect("a 2MiB page base plus its offset stays in range"),
                    page_size: PageSize::Size2MiB,
                    entry: pde,
                }));
            }

            let pt = entry::table_address(pde);
            let pte = self.read(pt, entry::pt_index(virt));
            if !entry::is_present(pte) {
                return Ok(None);
            }
            let base = entry::page_address_4k(pte);
            Ok(Some(Translation {
                phys: base
                    .checked_add(virt.page_offset())
                    .expect("a 4KiB page base plus its offset stays in range"),
                page_size: PageSize::Size4KiB,
                entry: pte,
            }))
        }
    }
}

/// 稼働中テーブルの書き換えが失敗した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapUpdateError {
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
    /// 張ろうとした葉が既に present。二重マップを黙って上書きしない（M5-e-2）。
    AlreadyMapped,
}

/// [`ActivePageTable::map_4kib`] が張る 1 枚に与える属性（S9-a）。
///
/// # なぜ bool を並べずに構造体にするか
///
/// 引数が 3 つとも `bool` になる。位置引数で並べると、取り違えても型が通り、
/// **静かに違う属性のページが張られる。** フィールド名を書かせる形なら、
/// 取り違えはコンパイルエラーになるか、読めば分かる。
/// **写像の属性は「ガードを写像の不在で作る」と同じで、間違えたことが後から
/// 症状としてしか出ない種類の値である。**
///
/// # 足りない属性
///
/// **実行可否（NX）は無い。** `EFER.NXE` が未有効で、立てると予約ビット違反の
/// #PF になる。有効化は別項の解禁条件に従う（`docs/deferred-decisions.md`）。
/// G と PWT と PAT も無い。前者は立てない方針、後の 2 つは要求が出ていない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageAttributes {
    /// Ring 3 から到達できるか（U/S）。**中間エントリへも伝播する。**
    pub user: bool,
    /// 書き込めるか（W）。**葉だけに効く。中間へは伝播しない。**
    pub writable: bool,
    /// キャッシュしてよいか。偽なら PCD を立てる（MMIO 用）。
    pub cacheable: bool,
}

/// 分割の結果。呼び出し側が照合に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitOutcome {
    /// 分割前の PDE の生の値。
    pub huge_entry: u64,
    /// 新しく確保した PT の物理アドレス。
    pub table_phys: PhysAddr,
    /// 分割した 2MiB 領域の先頭仮想アドレス。
    pub base_virt: VirtAddr,
}

impl ActivePageTable {
    /// `table_phys[index]` に書く。
    ///
    /// # Safety
    /// [`Self::read`] と同じ契約に加え、`value` が正しい形式のエントリで
    /// あること。
    unsafe fn write(&self, table_phys: PhysAddr, index: usize, value: u64) {
        // SAFETY: 呼び出し元契約を参照。変換は direct map 経由。
        unsafe {
            core::ptr::write_volatile(
                self.direct_map
                    .phys_to_virt(table_phys)
                    .as_mut_ptr::<u64>()
                    .add(index),
                value,
            );
        }
    }

    /// `virt` を含む PD と、その中の添字を求める。
    ///
    /// PML4 → PDPT → PD と降りる途中の検査をここへ集約する。
    fn locate_pd(&self, virt: VirtAddr) -> Result<(PhysAddr, usize), MapUpdateError> {
        // SAFETY: `current()` の契約により、テーブルは恒等マッピングで読める。
        unsafe {
            let pml4e = self.read(self.pml4_phys, entry::pml4_index(virt));
            if !entry::is_present(pml4e) {
                return Err(MapUpdateError::NotMapped);
            }
            let pdpt = entry::table_address(pml4e);
            let pdpte = self.read(pdpt, entry::pdpt_index(virt));
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
        virt: VirtAddr,
        frames: &mut FrameAllocator<CAP>,
    ) -> Result<SplitOutcome, MapUpdateError> {
        // 操作全体を割り込み禁止で囲む。M5-d でタイマ割り込みからページ
        // テーブルを触る経路が生まれるため、必要になってから足すのではなく
        // 最初から入れておく。
        let _guard = InterruptGuard::enter();

        let (pd, pd_index) = self.locate_pd(virt)?;
        // SAFETY: `locate_pd` が返す PD は有効なテーブルで、添字は 512 未満。
        let pde = unsafe { self.read(pd, pd_index) };
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
        let table_phys = frames.allocate_frame().ok_or(MapUpdateError::OutOfFrames)?;
        // SAFETY: 今このアロケータから確保したばかりの、他の誰も参照して
        // いないフレームである。アロケータの空き範囲がすべてマップ済みで
        // あることは起動時に検証済みなので、恒等マッピングで書ける。
        unsafe {
            core::ptr::write_bytes(
                self.direct_map.phys_to_virt(table_phys).as_mut_ptr::<u8>(),
                0,
                FRAME_SIZE as usize,
            );
        }

        let children = entry::split_children(pde);

        // 検証用に、わざと手順 3 を先に行う（`paging-test-wrong-order`）。
        //
        // **順序を逆にしただけでは何も起きない。** 中間状態（PD がゼロ埋めの
        // PT を指す状態）が存在するのは 512 回の書き込みの間だけで、その間に
        // CPU がその領域へ触らなければ、そのまま完了してしまう。
        // 「順序を守らないと壊れる」ことを示すには、中間状態を意図的に
        // 踏む必要がある。差し替えた直後に対象領域を 1 バイト読む。
        #[cfg(feature = "paging-test-wrong-order")]
        {
            // SAFETY: `pd` は有効な PD で添字は 512 未満。
            unsafe { self.write(pd, pd_index, entry::table_entry_for_split(pde, table_phys)) };
            // SAFETY: この読み取りは #PF を起こすことを期待している。
            // ページテーブルはこの瞬間、この領域を「不在」として指している。
            unsafe {
                let base = VirtAddr::new(virt.as_u64() & !(entry::PAGE_SIZE_2M - 1))
                    .expect("masking low bits of a canonical address keeps it canonical");
                core::ptr::read_volatile(base.as_ptr::<u8>());
            }
        }

        // 手順 2。値の計算は純粋ロジック側（ホストテストで固定）。
        for (index, child) in children.iter().enumerate() {
            // SAFETY: `table_phys` は直前にゼロ埋めした自前のフレームで、
            // 添字は 512 未満。まだどこからも参照されていない。
            unsafe { self.write(table_phys, index, *child) };
        }

        // 手順 3。8 バイト 1 回。
        // SAFETY: `pd` は有効な PD で添字は 512 未満。書く値は PT を指す
        // 正しい形式のエントリである。
        #[cfg(not(feature = "paging-test-wrong-order"))]
        unsafe {
            self.write(pd, pd_index, entry::table_entry_for_split(pde, table_phys))
        };

        // 手順 4。
        // SAFETY: CR3 の値をそのまま書き戻すだけで、指す先は変えていない。
        unsafe { switch::switch_to(switch::read_cr3()) };

        Ok(SplitOutcome {
            huge_entry: pde,
            table_phys,
            base_virt: VirtAddr::new(virt.as_u64() & !(entry::PAGE_SIZE_2M - 1))
                .expect("masking low bits of a canonical address keeps it canonical"),
        })
    }

    /// 2MiB ページのエントリにフラグを足す（**検証用**）。
    ///
    /// 通常のマッピングは `plan` と `PageTableBuilder` が決める。これは
    /// 「PCD 付きの 2MiB ページを分割したとき、512 エントリすべてに PCD が
    /// 残るか」を実機で確かめるためだけのものである。
    ///
    /// 実機で PCD 付きの 2MiB ページはフレームバッファしか無いが、そこを
    /// 分割対象にすると失敗したときに画面が壊れ、観測手段の一部を失う。
    /// 誰も使っていないスクラッチ領域に PCD を立ててから分割すれば、
    /// 同じ性質を安全に試せる。
    ///
    /// # Safety
    ///
    /// [`Self::split_huge_page`] と同じ。加えて `add` が、そのページに
    /// 付けて安全なフラグであること。
    #[cfg(feature = "paging-test")]
    pub unsafe fn add_huge_page_flags(
        &mut self,
        virt: VirtAddr,
        add: u64,
    ) -> Result<u64, MapUpdateError> {
        let _guard = InterruptGuard::enter();

        let (pd, pd_index) = self.locate_pd(virt)?;
        // SAFETY: `locate_pd` の契約による。
        let pde = unsafe { self.read(pd, pd_index) };
        if !entry::is_present(pde) {
            return Err(MapUpdateError::NotMapped);
        }
        if !entry::is_huge(pde) {
            return Err(MapUpdateError::AlreadySmall);
        }
        // SAFETY: 同上。フラグを足すだけでアドレスは変えない。
        unsafe { self.write(pd, pd_index, pde | add) };
        // SAFETY: CR3 の値をそのまま書き戻す。指す先は変えていない。
        unsafe { switch::switch_to(switch::read_cr3()) };
        Ok(pde)
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
    pub unsafe fn unmap_4kib(&mut self, virt: VirtAddr) -> Result<u64, MapUpdateError> {
        let _guard = InterruptGuard::enter();

        let (pd, pd_index) = self.locate_pd(virt)?;
        // SAFETY: `locate_pd` の契約による。
        let pde = unsafe { self.read(pd, pd_index) };
        if !entry::is_present(pde) {
            return Err(MapUpdateError::NotMapped);
        }
        // 2MiB のままアンマップはできない。分割は呼び出し側が先に行う
        // （フレームアロケータを要求する関数を、この中から呼びたくない）。
        if entry::is_huge(pde) {
            return Err(MapUpdateError::AlreadySmall);
        }

        let pt = entry::table_address(pde);
        // 検証用に、わざと添字を間違える（`paging-test-bad-index`）。
        // translate() が「別のページを None にしている」ことを捕まえられるかを
        // 確かめるためのもの。
        #[cfg(feature = "paging-test-bad-index")]
        let pt_index = entry::pd_index(virt);
        #[cfg(not(feature = "paging-test-bad-index"))]
        let pt_index = entry::pt_index(virt);
        // SAFETY: `pt` は PD が指す有効な PT で、添字は 512 未満。
        let pte = unsafe { self.read(pt, pt_index) };
        if !entry::is_present(pte) {
            return Err(MapUpdateError::NotMapped);
        }

        // SAFETY: 同上。0 を書いて Present を落とす。
        unsafe { self.write(pt, pt_index, 0) };
        // 検証用に、わざと invlpg を落とす（`paging-test-no-invlpg`）。
        // 古い翻訳が TLB に残っていれば、アンマップしたはずのアドレスへ
        // アクセスしてもフォルトしない。
        #[cfg(not(feature = "paging-test-no-invlpg"))]
        // SAFETY: テーブルの書き換えが終わってから落とす。順序を逆にすると、
        // 古い翻訳が残ったままテーブルだけ変わった状態になる。
        unsafe {
            cpu::invalidate_tlb_entry(virt.as_u64())
        };

        Ok(pte)
    }

    /// 稼働中テーブルへ 4KiB ページを 1 枚張る（M5-e-2）。
    ///
    /// 途中の中間テーブル（PDPT/PD/PT）が不在なら確保して作る。
    /// [`PageAttributes::user`] が真なら、**作る中間エントリと葉 PTE の両方**で
    /// U/S ビット（[`entry::PTE_USER`]）を立て、Ring 3 から到達可能にする。
    /// CPU は各階層の U/S を AND で合成するため、ユーザーページは PML4 から PT まで
    /// 全階層で U=1 が要る。
    ///
    /// # 既存の中間テーブルの U ビットは触らない
    ///
    /// [`Self::ensure_child`] は、既に present の中間テーブルを見つけたら、その
    /// U ビットを立て直さずにそのまま使う。したがって**ユーザーページは、
    /// カーネルと中間を共有しない専用サブツリー（空き PML4 エントリの配下）へ
    /// 張ること**が呼び出し側の責任である。カーネルの中間へ U=1 を混ぜないのは
    /// この設計で構造的に保証する（既存カーネルマッピングを 1 ビットも変えない）。
    /// 逆に、既存のカーネル中間（U=0）の下にユーザーページを張ろうとしても、
    /// AND 合成により Ring 3 からは到達できない（安全側に倒れる）。
    ///
    /// # 属性
    ///
    /// G は立てない（TLB を CR3 リロード / `invlpg` で管理する前提。
    /// `tlb_flush_precondition` の G=0 前提）。NX も立てない（EFER.NXE 未有効。
    /// 予約ビット違反の #PF を避ける）。書き込み可否は
    /// [`PageAttributes::writable`]、キャッシュは [`PageAttributes::cacheable`] で
    /// 制御する。
    ///
    /// # 書き込み可否は葉だけで表す。中間へは伝播しない
    ///
    /// U/S と違い、W は中間へ伝播させない。**書き込みの可否も各階層の W の AND で
    /// 決まるので、中間を W=0 にすると、その配下の葉が 1 枚残らず読み取り専用に
    /// なる。** 中間は常に許す側（W=1）に置き、可否は葉で表す。
    ///
    /// # この段で入れたのは W だけである。`W^X` ではない
    ///
    /// **X の側（NX ビット）は入っていない。** 立てるには `EFER.NXE` の有効化が
    /// 要り、それは別項の解禁条件に従う（`docs/deferred-decisions.md` の
    /// 「`EFER.NXE` の有効化と NX」）。**したがってこの API はまだ
    /// 「書き込めるが実行もできる」ページしか作れず、`W^X` は成立していない。**
    /// 名前だけ先に使うと、到達していないものを到達したように書くことになる。
    ///
    /// # `writable: false` で張ったページについて、何を主張してよいか
    ///
    /// **主張してよいのは「Ring 3 から書くと #PF になる」までである。**
    /// 「カーネル（Ring 0）から書いても落ちる」は主張しない。それには `CR0.WP` が
    /// 要り、**AP では WP が立っていない**（BSP は立っている。実測値と経緯は
    /// `docs/deferred-decisions.md` の「AP の制御レジスタが BSP と違う」）。
    /// 揃えるかどうかはその項目の解禁条件に従う。
    ///
    /// # Safety
    ///
    /// [`Self::split_huge_page`] と同じ。加えて `phys` が有効な物理フレームで、
    /// `virt` にまだ 4KiB マッピングが無いこと（既にあれば
    /// [`MapUpdateError::AlreadyMapped`] を返して何も変えない）。
    pub unsafe fn map_4kib<const CAP: usize>(
        &mut self,
        virt: VirtAddr,
        phys: PhysAddr,
        attributes: PageAttributes,
        frames: &mut FrameAllocator<CAP>,
    ) -> Result<(), MapUpdateError> {
        let _guard = InterruptGuard::enter();

        // 中間エントリのフラグ。U だけを伝播する（AND 合成のため全階層に要る）。
        // W は伝播しない（上の doc）。
        let mut table_flags = entry::PTE_PRESENT | entry::PTE_WRITABLE;
        if attributes.user {
            table_flags |= entry::PTE_USER;
        }

        // PML4 → PDPT → PD を辿り、不在の中間を確保して作る。
        // SAFETY: pml4_phys は current() が読んだ稼働中 PML4。添字は 512 未満。
        let pdpt = unsafe {
            self.ensure_child(self.pml4_phys, entry::pml4_index(virt), table_flags, frames)?
        };
        // SAFETY: 直前に得た有効な PDPT。
        let pd = unsafe { self.ensure_child(pdpt, entry::pdpt_index(virt), table_flags, frames)? };
        // SAFETY: 直前に得た有効な PD。
        let pt = unsafe { self.ensure_child(pd, entry::pd_index(virt), table_flags, frames)? };

        // 葉。既に present なら二重マップとして弾く（黙って上書きしない）。
        let pt_index = entry::pt_index(virt);
        // SAFETY: pt は PD が指す有効な PT、添字は 512 未満。
        let existing = unsafe { self.read(pt, pt_index) };
        if entry::is_present(existing) {
            return Err(MapUpdateError::AlreadyMapped);
        }

        let mut leaf_flags = entry::PTE_PRESENT;
        // 破壊 (S9-a, map-force-writable): 書き込み可否の引数を無視して常に W=1 に
        // する。読み取り専用で張ったユーザーページへ Ring 3 が書けてしまい、
        // ring3-vectors の #PF-write-ro が #PF ではなく後続の ud2 で畳まれる。
        if attributes.writable || cfg!(feature = "map-force-writable") {
            leaf_flags |= entry::PTE_WRITABLE;
        }
        if attributes.user {
            leaf_flags |= entry::PTE_USER;
        }
        if !attributes.cacheable {
            leaf_flags |= entry::PTE_PCD;
        }
        // SAFETY: pt/添字は上記の契約。書く値は 4KiB ページを指す正しい PTE。
        unsafe {
            self.write(
                pt,
                pt_index,
                (phys.as_u64() & entry::ADDR_MASK_4K) | leaf_flags,
            )
        };
        // SAFETY: テーブルの書き換えが終わってから、追加した 1 本を落とす。
        unsafe { cpu::invalidate_tlb_entry(virt.as_u64()) };
        Ok(())
    }

    /// `table_phys[index]` が指す子テーブルの物理を返す。不在なら 1 枚確保して
    /// ゼロ埋めし、`table_flags` で親エントリを書く（M5-e-2）。
    ///
    /// 既に present の中間があればそれをそのまま返し、U ビットを立て直さない
    /// （[`Self::map_4kib`] のドキュメント参照）。
    ///
    /// # Safety
    ///
    /// `table_phys` が有効なページテーブルフレーム、`index < 512`。`frames` の
    /// 返すフレームが恒等 / direct map で読み書きできること。
    unsafe fn ensure_child<const CAP: usize>(
        &mut self,
        table_phys: PhysAddr,
        index: usize,
        table_flags: u64,
        frames: &mut FrameAllocator<CAP>,
    ) -> Result<PhysAddr, MapUpdateError> {
        // SAFETY: 呼び出し元契約による。読み取りのみ。
        let existing = unsafe { self.read(table_phys, index) };
        if entry::is_present(existing) {
            if entry::is_huge(existing) {
                // huge ページのスロットを中間テーブルとして扱わない。
                return Err(MapUpdateError::NotSplittable);
            }
            return Ok(entry::table_address(existing));
        }
        let child = frames.allocate_frame().ok_or(MapUpdateError::OutOfFrames)?;
        // SAFETY: 今確保したばかりの、他から参照されていないフレーム。空き集合は
        // すべてマップ済みなので direct map で書ける。ゼロ埋めして、ゴミが
        // Present の立った不正なエントリに解釈されるのを防ぐ。
        unsafe {
            core::ptr::write_bytes(
                self.direct_map.phys_to_virt(child).as_mut_ptr::<u8>(),
                0,
                FRAME_SIZE as usize,
            );
        }
        // SAFETY: table_phys/index は上記契約。新規に確保した child を指す
        // 中間エントリを書く。table_flags は呼び出し側が U を含めて決める。
        unsafe {
            self.write(
                table_phys,
                index,
                (child.as_u64() & entry::ADDR_MASK_4K) | table_flags,
            )
        };
        Ok(child)
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
