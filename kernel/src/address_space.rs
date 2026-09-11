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
use crate::paging::active::PageAttributes;
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
    /// その仮想アドレスには既に葉が張られている（S9-b-3-2b）。
    ///
    /// **上書きしない。** 上書きすると前のフレームが写像から外れ、破棄から
    /// 見えなくなって漏れる。**区画が同じ 4KiB ページを共有する ELF がここへ
    /// 来る。**
    AlreadyMapped,
}

/// プロセス 1 つ分のアドレス空間。
///
/// **まだ破棄を持たない。** 破棄は S7-d である。**持たせないのは、破棄が隔離
/// （[`crate::quarantine`]）と一体だからで、片方だけ先に作ると「返してよい」判断が
/// 無いまま返す形が書けてしまう。**
pub struct AddressSpace {
    pml4: PhysAddr,
    /// **この空間のユーザーサブツリーの添字（S7-e）。**
    ///
    /// **空間が自分で持つ。** 監査（[`AddressSpace::audit_user_supervisor`]）は
    /// これを空間から取るので、**呼び出し側が別の添字を渡す余地が無い。**
    /// **渡し間違いが構造的に起きない形にしてある**（「ガードは写像の不在で作る」）。
    ///
    /// **プロセスごとに違ってよい。** 全空間が同じ添字を使う前提は、
    /// **プロセス別アドレス空間の目的と逆を向いている**（S7-e で言い換えた）。
    user_pml4_index: usize,
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
        user_pml4_index: usize,
    ) -> Result<Self, AddressSpaceError> {
        if is_shared_kernel_index(user_pml4_index) {
            return Err(AddressSpaceError::NotPrivate);
        }
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

        Ok(Self {
            pml4,
            user_pml4_index,
        })
    }

    /// この空間の PML4 の物理アドレス。
    pub fn pml4(&self) -> PhysAddr {
        self.pml4
    }

    /// この空間のユーザーサブツリーの添字。
    pub fn user_pml4_index(&self) -> usize {
        self.user_pml4_index
    }

    /// **この空間について U/S の監査を行う（S7-e）。**
    ///
    /// 主張は**「U=1 は、この空間のユーザーサブツリーの外に存在しない」**である。
    ///
    /// **前提が言い換わっている。** 単一アドレス空間のときは「U=1 はユーザー
    /// サブツリーの外に一切存在しない」という**大域の主張**だった。**プロセスごとに
    /// なると、主張は空間ごとになる**——**どの空間について言っているかが付いて回る。**
    ///
    /// **添字は空間から取る。** 呼び出し側は渡せない。
    ///
    /// # Safety
    ///
    /// [`crate::paging::verify::audit_user_supervisor`] と同じ契約。
    pub unsafe fn audit_user_supervisor(
        &self,
        direct_map: DirectMap,
    ) -> crate::paging::verify::UserSupervisorAudit {
        // SAFETY: 呼び出し元契約。添字はこの空間のものである。
        unsafe {
            crate::paging::verify::audit_user_supervisor(
                self.pml4,
                direct_map,
                self.user_pml4_index,
            )
        }
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

/// 集めたフレームを一時的に置く場所（B-d で静的へ移した）。
///
/// # なぜスタックに置かないか
///
/// **`Option<PhysAddr>` は 16 バイトで、容量に比例してスタックを食う。**
/// **B-d で隔離の容量を 64 から 256 へ上げたとき、これが 1 KiB から 4 KiB へ
/// 育ち、遠征スタックの高水位が半分を越えて起動が止まった**（実測。
/// **「半分を越えたらガードページを張るか、測って容量を上げるかを決める」と
/// いう持ち越しの行が発火した**）。
///
/// **持ち越しの行は発火させない**——**育てたのはこちらの都合であって、
/// 遠征スタックの要件が変わったわけではない。** **静的へ移せば、容量を
/// いくつにしても遠征スタックは 1 バイトも増えない。**
///
/// # 同時に 2 つ走らない
///
/// **[`AddressSpace::destroy`] は BKL の内側でしか呼べない**（`_guard` が
/// それを型で示している）。**入れ子にもならない**——**破棄は他の破棄を
/// 呼ばない。**
static mut COLLECTED_FRAMES: [Option<PhysAddr>; MAX_FRAMES_PER_DESTROY] =
    [None; MAX_FRAMES_PER_DESTROY];

impl AddressSpace {
    /// 4KiB のユーザーページを 1 枚張る（S7-d）。
    ///
    /// **下位にしか張れない。** 上位は共有なので、ここから触ると全アドレス空間へ
    /// 波及する。**添字で弾く**（[`is_shared_kernel_index`]）。
    ///
    /// # 属性（S9-b-1）
    ///
    /// `user` は常に真なので取らない。**この関数はユーザーページを張るためだけに
    /// ある。** 取るのは [`PageAttributes::writable`] と
    /// [`PageAttributes::cacheable`] である。
    ///
    /// **W は葉だけに効く。中間へは伝播しない**（[`crate::paging::active::ActivePageTable::map_4kib`] と
    /// 同じ理由。中間を W=0 にすると配下の葉が 1 枚残らず読み取り専用になる）。
    ///
    /// **NX は無い。** `EFER.NXE` が未有効である（別項の解禁条件に従う）。
    ///
    /// # 写像の経路が 2 つあることについて
    ///
    /// **同じ「4KiB を 1 枚張る」を、この関数と [`crate::paging::active::ActivePageTable::map_4kib`] の
    /// 2 か所が別々に実装している。** 前者は稼働していない空間のテーブルを
    /// direct map 越しに書き、後者は稼働中のテーブルを書いて `invlpg` する。
    /// **S9-b では統合せず、両方に同じ属性を通す。**
    ///
    /// **統合の合図は「どちらかの経路に 3 つ目の属性を足す必要が生じたとき」で
    /// ある。** 同じ変更を 2 度加えることになった時点が、2 つ持っている費用が
    /// 表に出た時点である。**今回（W を足す）が 1 度目である。**
    ///
    /// **「同じ変更を 2 度」の 1 件目が出た（S9-b-3-2b）。** 葉が既に張られて
    /// いるかの判定である。**`map_4kib` は最初から持っていて、こちらは持って
    /// いなかった**——2 つの経路が同じ性質を持つべきなのに、片方だけが持って
    /// いた。**合図には当たらない**（足したのは属性ではなく検査である）。
    /// **カウントの 1 件目として数える。**
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
        attributes: PageAttributes,
    ) -> Result<(), AddressSpaceError> {
        use crate::paging::entry;

        // **この空間のユーザーサブツリーの中でなければ弾く（S7-e）。**
        // 共有側でないことだけでは足りない——**別の添字へ張ると、監査の主張
        // （U=1 はこの空間のユーザーサブツリーの外に存在しない）が破れる。**
        if entry::pml4_index(virt) != self.user_pml4_index {
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

        // **既に張られている葉は上書きしない（S9-b-3-2b）。**
        //
        // **重なる区画を持つ像がここへ来る。** 上書きすると、前の葉が指していた
        // フレームが写像から外れ、`destroy` から見えなくなって 1 枚漏れる
        // （実測で 14 枚消えて隔離へ 13 枚）。**漏れは会計に出てカーネルが
        // 止まるので、S9 の「いかなる入力でもカーネルを fail-fast させない」に
        // 反していた。**
        //
        // `ActivePageTable::map_4kib` は最初からこの判定を持っている。
        // **2 つの経路が同じ性質を持つべきなのに、片方だけが持っていた**
        // （この関数の doc の「写像の経路が 2 つあることについて」）。
        let leaf_index = entry::pt_index(virt);
        // SAFETY: table は上の走査で得た present な中間テーブルの物理。読み取りのみ。
        let existing_leaf = unsafe { read_entry(direct_map, table, leaf_index) };
        if entry::is_present(existing_leaf) {
            return Err(AddressSpaceError::AlreadyMapped);
        }

        let mut leaf = frame.as_u64() | entry::PTE_PRESENT | entry::PTE_USER;
        // 破壊 (S9-a, map-force-writable): 書き込み可否の引数を無視して常に W=1 に
        // する。**もう一方の経路（`ActivePageTable::map_4kib`）と同じ破壊で両方が
        // 落ちる。** 経路が 2 つあることを、破壊の側でも 1 本にまとめてある。
        if attributes.writable || cfg!(feature = "map-force-writable") {
            leaf |= entry::PTE_WRITABLE;
        }
        if !attributes.cacheable {
            leaf |= entry::PTE_PCD;
        }
        // SAFETY: 葉。ユーザーから到達できる 4KiB ページ。
        unsafe { write_entry(direct_map, table, leaf_index, leaf) };
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
        // SAFETY: BKL を保持している（`_guard`）。**破棄は入れ子にならないので、
        // この参照が生きている間、他に触る者は居ない**（[`COLLECTED_FRAMES`] の doc）。
        let collected: &mut [Option<PhysAddr>; MAX_FRAMES_PER_DESTROY] =
            unsafe { &mut *core::ptr::addr_of_mut!(COLLECTED_FRAMES) };
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
