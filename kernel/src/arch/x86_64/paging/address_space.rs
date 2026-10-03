//! プロセスごとのアドレス空間（S7-c）。
//!
//! # 何を作っているか
//!
//! **上位（カーネル）を共有し、下位（ユーザー）だけを差し替える。** PML4 は 512 本の
//! エントリを持ち、**上位 256 本（添字 256..512）がカーネルの取り分**である
//! （higher-half、ADR-0024）。新しいアドレス空間を作るとは、**PML4 のフレームを 1 枚
//! 取り、上位 256 本をそのままコピーし、下位 256 本を空にする**ことである。
//!
//! # 共有するのであって、複製するのではない
//!
//! **コピーするのは PML4 のエントリ（8 バイトの値）であって、その先のテーブルではない。**
//! したがって**カーネルのマッピングはすべてのアドレス空間で同一の実体を指す。** 片方で
//! カーネル側を変えれば、もう片方からも見える。**これが「共有」の意味であり、到達
//! 条件の「稼働中テーブルの共有カーネル部分が一致すること」が示していることである。**
//!
//! # なぜ上位をコピーするだけで足りるのか
//!
//! **カーネルは higher-half にあり、恒等マッピングは既に落としてある**（B-2b）。
//! したがってカーネルのコード・スタック・direct map はすべて上位 256 本の下にある。
//! **CR3 を差し替えても、上位が同じなら実行中のコードもスタックも見え続ける。**
//!
//! **これは検査できる主張である**（到達条件 4）。破壊テスト `addrspace-no-kernel-share`
//! は上位をコピーしない。**切り替えた瞬間に命令フェッチが翻訳できなくなる。**

use core::sync::atomic::{AtomicU64, Ordering};

use crate::frame_allocator::FrameAllocator;
use crate::paging::permissions::PagePermissions;
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
/// **純粋関数にしてある。** 「どこからどこまでを共有するか」はマッピングの実体を触らずに
/// 決まる判断なので、ホスト上で検査できる形に切り出す。
///
/// **ユーザー空間の添字（`USER_PML4_INDEX`）との突き合わせは、ここには設けられない。**
/// あれは `kernel/src/main.rs`（bin 側）に居り、lib からは見えない。**S7 でこの添字を
/// 動かすとき**（`docs/deferred-decisions.md`）**、動かした先が共有側へ入り込んで
/// いないことを、bin 側で確かめること。**
pub const fn is_shared_kernel_index(index: usize) -> bool {
    index >= KERNEL_PML4_FIRST_INDEX && index < PML4_ENTRY_COUNT
}

/// アドレス空間の作成でしくじる形。
///
/// # 契約（境界の型。2026-09-30）
///
/// - アドレス空間を作るときや写像を足すときのしくじりの形である。共通の側は、読み込みの誤りの中に包んで
///   ログへ出す（`crate::userland`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AddressSpaceError {
    /// PML4 用のフレームが取れなかった。
    OutOfFrames,
    /// direct map 越しに PML4 を触れなかった（マッピングの外を指している）。
    Unreachable,
    /// 共有側（上位）へマップしようとした。**下位にしかマップできない。**
    NotPrivate,
    /// 下位に巨大ページがあった。**マップする経路が無いので、前提が崩れている。**
    UnexpectedHugePage,
    /// その仮想アドレスには既に葉がマップされている（S9-b-3-2b）。
    ///
    /// **上書きしない。** 上書きすると前のフレームがマッピングから外れ、破棄から
    /// 見えなくなって漏れる。**区画が同じ 4KiB ページを共有する ELF がここへ
    /// 来る。**
    AlreadyMapped,
    /// カーネル側の PML4 の項目が、起動の終わりに採った指紋と違う（2026-09-27。`ADR-0071` の決定 5）。
    ///
    /// **前提（起動の後は、カーネル側の PML4 の項目を誰も変えない）が崩れている。** **コピーした上位が
    /// 稼働中の表と食い違っているかもしれないので、この空間を作らない。**
    KernelTopChanged,
    /// ユーザーから届かない権限で、ユーザーの側へマップしようとした（2026-10-02）。
    ///
    /// **[`AddressSpace::map_user_4kib`] は、ユーザーのページをマップするためだけに在る。** 以前は、渡された
    /// 属性に関わらず U を立てていた。権限の型を 1 つにしてからは、ユーザーから届かない権限を断る。
    NotForUser,
    /// 書けて、実行もできる権限で、ユーザーの側へマップしようとした（2026-10-03）。
    ///
    /// **書けるページは実行できない、という決まりで写す**（写像ごとの W^X）。ELF の区画でこの形のものは、
    /// 区画の並びの確かめ（`common::elf::check_load_layout`）が先に断る。ここは 2 枚目の守りである。
    WritableAndExecutable,
}

/// 起動の終わり（`run_init` の前）に採った、カーネル側の PML4 の項目（添字 256〜511）の指紋
/// （2026-09-27。`ADR-0071` の決定 5）。**0 は「まだ採っていない」を表す**（[`kernel_top_digest`] は 0 を返さない）。
static FROZEN_KERNEL_TOP: AtomicU64 = AtomicU64::new(0);

/// 起動が終わったか（カーネル側の PML4 の指紋を採ったか）。**「起動の間」と「起動の後」を分ける目印は、
/// 指紋を採った時点（`run_init` の直前）で切り替わるこれ 1 つである**（2026-09-27。`ADR-0071` の決定 5）。
/// **書く側の守り（`ActivePageTable` の `ensure_child`）と突き合わせ（[`AddressSpace::new`]）は、
/// どちらも同じ `FROZEN_KERNEL_TOP` を読む**——**目印を 2 か所で持たない。**
pub fn kernel_top_is_frozen() -> bool {
    FROZEN_KERNEL_TOP.load(Ordering::SeqCst) != 0
}

/// カーネル側の PML4 の項目を新しく作る書き込みを断るか（純粋な論理。2026-09-27）。**起動の後に、
/// カーネル側の添字（256〜511）へ作るときだけ断る。** **ユーザー側の添字は、起動の後も各空間が自分の
/// PML4 に作る。**
pub const fn kernel_top_write_is_refused(index: usize, frozen: bool) -> bool {
    frozen && is_shared_kernel_index(index)
}

/// カーネル側の PML4 の項目の指紋（純粋な論理）。**FNV-1a（64 ビット）で、添字と値を順に混ぜる。**
/// **CPU が立てるアクセス済み（A、5 番）とダーティ（D、6 番）のビットは除く**——**表を辿るだけで立つので、
/// 項目の変更ではない。** **最下位のビットを立てて返すので 0 にならない**（0 は「まだ採っていない」）。
pub fn kernel_top_digest(entries: impl IntoIterator<Item = (usize, u64)>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    const SET_BY_THE_CPU: u64 = super::entry::PTE_ACCESSED | super::entry::PTE_DIRTY;
    let mut hash = OFFSET;
    for (index, value) in entries {
        let value = value & !SET_BY_THE_CPU;
        for byte in (index as u64)
            .to_le_bytes()
            .into_iter()
            .chain(value.to_le_bytes())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash | 1
}

/// 起動の終わりに、稼働中の PML4 のカーネル側の指紋を採る（2026-09-27。`ADR-0071` の決定 5）。**`run_init` の前に
/// 1 度だけ呼ぶ。** **ここから後は、[`AddressSpace::new`] がコピーした上位をこの指紋と突き合わせる。**
/// **戻り値は present な項目の数**（起動ログに出す）。**覆いの外なら `None` で、採らない。**
///
/// **指紋は PML4 の上位の項目だけを見て、その下の段（PDPT など）は見ない**——**下の段は全部のアドレス空間が
/// 同じ表を共有しているので、変わってもすべての空間に同時に見える。** **前提が守るのは、PML4 の項目を
/// コピーした時点と今とで食い違わないことである。**
///
/// # Safety
///
/// `current_pml4` が稼働中の PML4 で、`direct_map` がそれを覆っていること。**読むだけである。**
pub unsafe fn freeze_kernel_top(direct_map: DirectMap, current_pml4: PhysAddr) -> Option<usize> {
    if !direct_map.covers(current_pml4) {
        return None;
    }
    let table = direct_map.phys_to_virt(current_pml4).as_u64() as *const u64;
    let mut present = 0;
    let digest = kernel_top_digest((KERNEL_PML4_FIRST_INDEX..PML4_ENTRY_COUNT).map(|index| {
        // SAFETY: direct map 越しの稼働中 PML4 の読み。覆いは上で確かめ、添字は 512 未満。
        let value = unsafe { table.add(index).read_volatile() };
        if value & super::entry::PTE_PRESENT != 0 {
            present += 1;
        }
        (index, value)
    }));
    // 破壊テスト (2026-09-27, kernel-top-digest-mismatch-test): **違う指紋を控える。** **起動の後の最初の
    // アドレス空間の作成が `KernelTopChanged` で断られ、突き合わせる所が働くことを確かめる。**
    #[cfg(feature = "kernel-top-digest-mismatch-test")]
    let digest = digest ^ 2;
    FROZEN_KERNEL_TOP.store(digest, Ordering::SeqCst);
    Some(present)
}

/// プロセス 1 つ分のアドレス空間。
///
/// # 契約（境界の型。2026-09-30。2026-10-02 に壊す入口を分けた）
///
/// - プロセス 1 つ分のアドレス空間である。作るのはプログラムを読み込む所（`crate::userland`）である。
/// - **この型が持つのは、ページテーブルの操作だけである**——作る（[`AddressSpace::new`]）、ユーザーのページを
///   1 枚足す（[`AddressSpace::map_user_4kib`]）、ユーザーの側の項目を外してフレームを集める
///   （[`AddressSpace::detach`]）。
/// - **壊す順序（外す → 変換の控えの世代を上げる → その世代で隔離へ入れる）は、共通の側が持つ**
///   （`crate::quarantine` の `retire_address_space`。BKL を持っていることも、そちらが引数で示す）。
///   この型は、BKL も隔離も知らない。
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
    /// **この空間のために取ったフレームの本数**（`ADR-0063` の (b1)。2026-09-18）。
    ///
    /// # なぜ空間が数えるのか
    ///
    /// **破棄の会計を、大域の空きフレーム数の差ではなく、空間ごとの数で閉じるためである。**
    /// **大域の差は、2 本の `spawn` のウィンドウが交差すると相手の分を取り込む**——**実測で、
    /// 2 本の「消費」と「隔離」がちょうど入れ替わった**（`ADR-0063`）。
    ///
    /// **数えるのは 3 か所である**——**PML4（[`AddressSpace::new`]）、途中のテーブル、
    /// 葉（どちらも [`AddressSpace::map_user_4kib`]）。** **葉のフレームは呼び出し側が
    /// 取るが、マップしたときに数えるので、呼び出し側は数えなくてよい**——**取ったのに
    /// マップしなかったフレームは、この空間のものにならない**（呼び出し側が戻す）。
    ///
    /// **[`AddressSpace::detach`] が集める本数と突き合わせる**——**あちらは
    /// ページテーブルから辿れるものを集めるので、「取ったのに繋がっていない」が差になる。**
    frames_taken: usize,
}

impl AddressSpace {
    /// 稼働中のテーブルからカーネル部分をコピーして、新しいアドレス空間を作る。
    ///
    /// **コピー元は、いま稼働している表である**（この関数が自分で読む。2026-10-02。以前は、呼ぶ側が根を渡していた。
    /// 呼ぶ側は 3 か所とも、稼働中の根を読んで渡していただけである）。
    ///
    /// `user_window` は、この空間がユーザーのページを置く枝の番号である（x86_64 では、最上位の表の下位半分の添字）。
    ///
    /// # Safety
    ///
    /// - `direct_map` が、稼働中の最上位の表と、これから取るフレームの両方を覆っていること。
    /// - **呼び出しの間に、カーネル側の PML4 の項目（添字 256〜511）が変わらないこと。**
    ///   **起動の後（`run_init` から）は誰も変えない**——**書く経路は起動の間だけである**（AP の per-CPU の
    ///   置き場と AP スタック〔添字 258〕と、試しの feature `smp-tlb-shootdown-probe` のときだけ作る探り用の
    ///   ページ〔添字 259〕。どれも `kernel_main` の中で `run_init` より前に呼ぶ。`ADR-0071` の決定 5）。
    ///   **BKL には依らない**（`spawn` は BKL を解いてから呼ぶ）。
    ///   **起動の間に呼ぶなら、同じ文脈がカーネル側の項目を書いていないこと**（AP を起動する前の
    ///   単一の文脈から呼ぶ）。**起動の後は、コピーした上位を起動の終わりの指紋
    ///   （[`freeze_kernel_top`]）と突き合わせ、違えば [`AddressSpaceError::KernelTopChanged`] で断る。**
    pub unsafe fn new(
        allocator: &mut FrameAllocator,
        direct_map: DirectMap,
        user_window: usize,
    ) -> Result<Self, AddressSpaceError> {
        let user_pml4_index = user_window;
        if is_shared_kernel_index(user_pml4_index) {
            return Err(AddressSpaceError::NotPrivate);
        }
        let current_pml4 = crate::arch::x86_64::paging::switch::active_page_table_root();
        let pml4 = allocator
            .allocate_frame()
            .ok_or(AddressSpaceError::OutOfFrames)?;
        // **PML4 も、外すときに集める**（[`AddressSpace::detach`]）ので、ここで 1 本数える。

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
            // 破壊テスト (S7-c, addrspace-no-kernel-share): **上位をコピーしない。**
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

        // **起動の後は、コピーした上位を起動の終わりの指紋と突き合わせる**（`ADR-0071` の決定 5）。
        // **違えば、前提が崩れているので作らない。**
        let frozen = FROZEN_KERNEL_TOP.load(Ordering::SeqCst);
        if frozen != 0 {
            let copied =
                kernel_top_digest((KERNEL_PML4_FIRST_INDEX..PML4_ENTRY_COUNT).map(|index| {
                    // SAFETY: いま書いたフレームの、direct map 越しの読み。範囲は 512 エントリ内。
                    (index, unsafe { new_table.add(index).read_volatile() })
                }));
            if copied != frozen {
                let _ = allocator.deallocate_frame(pml4);
                return Err(AddressSpaceError::KernelTopChanged);
            }
        }

        Ok(Self {
            pml4,
            user_pml4_index,
            frames_taken: 1,
        })
    }

    /// この空間の PML4 の物理アドレス。
    pub fn root(&self) -> PhysAddr {
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
    /// [`crate::arch::x86_64::paging::verify::audit_user_supervisor`] と同じ契約。
    pub unsafe fn audit_user_supervisor(
        &self,
        direct_map: DirectMap,
    ) -> crate::arch::x86_64::paging::verify::UserSupervisorAudit {
        // SAFETY: 呼び出し元契約。添字はこの空間のものである。
        unsafe {
            crate::arch::x86_64::paging::verify::audit_user_supervisor(
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
    /// [`Self::new`] が上位をコピーしているので、**カーネルのコード・スタック・direct map は
    /// 切り替えの前後で同じ物理を指す。** ただし**下位は空である**——切り替えた後に
    /// ユーザー空間のアドレスへ触ると `#PF` になる。
    ///
    /// **BKL を保持したまま呼ぶこと。** CR3 は per-CPU の状態だが、マッピングの共有部分を
    /// 他コアが同時に変えていないことに依存する。
    pub unsafe fn activate(&self) {
        // SAFETY: 上記の契約。上位をコピーしてあるので、実行中のコードとスタックは見え続ける。
        unsafe { crate::arch::x86_64::paging::switch::set_active_page_table_root(self.pml4) }
    }
}

/// 下位（ユーザー側）の PML4 添字の範囲。**破棄が触ってよいのはここだけである。**
const PRIVATE_INDEX_RANGE: core::ops::Range<usize> = 0..KERNEL_PML4_FIRST_INDEX;

impl AddressSpace {
    /// 4KiB のユーザーページを 1 枚マップする（S7-d）。
    ///
    /// **下位にしかマップできない。** 上位は共有なので、ここから触ると全アドレス空間へ
    /// 波及する。**添字で弾く**（[`is_shared_kernel_index`]）。
    ///
    /// # 属性（S9-b-1）
    ///
    /// **この関数はユーザーページをマップするためだけにある。** 権限は [`PagePermissions`] の、ユーザーの側の
    /// 名前（`user_program`・`user_data`・`user_shared`）で受ける。ユーザーから届かない権限は
    /// [`AddressSpaceError::NotForUser`] で断る。ビットへ直すのは `entry` の変換である（2026-10-02）。
    ///
    /// **W は葉だけに効く。中間へは伝播しない**（[`crate::arch::x86_64::paging::active::ActivePageTable::map_4kib`] と
    /// 同じ理由。中間を W=0 にすると配下の葉が 1 枚残らず読み取り専用になる）。
    ///
    /// **NX は無い。** `EFER.NXE` が未有効である（別項の解禁条件に従う）。
    ///
    /// # 写像の経路が 2 つあることについて
    ///
    /// **同じ「4KiB を 1 枚張る」を、この関数と [`crate::arch::x86_64::paging::active::ActivePageTable::map_4kib`] の
    /// 2 か所が別々に実装している。** 前者は稼働していない空間のテーブルを
    /// direct map 越しに書き、後者は稼働中のテーブルを書いて `invlpg` する。
    /// **S9-b では統合せず、両方に同じ属性を通す。**
    ///
    /// **統合の合図は「どちらかの経路に 3 つ目の属性を足す必要が生じたとき」で
    /// ある。** 同じ変更を 2 度加えることになった時点が、2 つ持っている費用が
    /// 表に出た時点である。**今回（W を足す）が 1 度目である。**
    ///
    /// **「同じ変更を 2 度」の 1 件目が出た（S9-b-3-2b）。** 葉が既にマップされて
    /// いるかの判定である。**`map_4kib` は最初から持っていて、こちらは持って
    /// いなかった**——2 つの経路が同じ性質を持つべきなのに、片方だけが持って
    /// いた。**合図には当たらない**（足したのは属性ではなく検査である）。
    /// **カウントの 1 件目として数える。**
    ///
    /// # Safety
    ///
    /// - `direct_map` が、これから取る中間テーブルと `frame` を覆っていること。
    /// - **この空間がどのコアでも稼働していないこと。** 稼働中にマップすると、そのコアの
    ///   TLB との整合を別に取る必要がある。**S7-d の使い方では、作ってから
    ///   切り替えるまでの間にマップするので満たされる。**
    pub unsafe fn map_user_4kib(
        &mut self,
        allocator: &mut FrameAllocator,
        direct_map: DirectMap,
        virt: common::addr::VirtAddr,
        frame: PhysAddr,
        permissions: PagePermissions,
    ) -> Result<(), AddressSpaceError> {
        use crate::arch::x86_64::paging::entry;

        // **この空間のユーザーサブツリーの中でなければ弾く（S7-e）。**
        // 共有側でないことだけでは足りない——**別の添字へマップすると、監査の主張
        // （U=1 はこの空間のユーザーサブツリーの外に存在しない）が破れる。**
        if entry::pml4_index(virt) != self.user_pml4_index {
            return Err(AddressSpaceError::NotPrivate);
        }
        // **ユーザーから届かない権限は断る。** 途中の項目の U は葉の権限から決まるので、届かない権限を通すと、
        // ユーザーの枝の中に、ユーザーから届かない項目ができる。
        if !permissions.user() {
            return Err(AddressSpaceError::NotForUser);
        }
        // **書けて実行もできる権限は断る**（2026-10-03）。破壊テスト (user-map-allows-writable-executable): 断らない。
        if permissions.writable_and_executable()
            && !cfg!(feature = "user-map-allows-writable-executable-test")
        {
            return Err(AddressSpaceError::WritableAndExecutable);
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
                    // **巨大ページは扱わない。** 下位に 2MiB をマップする経路が無いので、
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
                // **この空間のものになった**（`ADR-0063` の (b1)）。**戻す枝より後で数える。**
                self.frames_taken += 1;
                // SAFETY: いま取ったフレームで、direct map が覆っている。
                unsafe { zero_table(direct_map, fresh) };
                // SAFETY: 中間テーブルなので U ビットを立てる。立てないと、葉で
                // 立てても CPU は全階層の AND を見るのでユーザーから触れない。
                // （権限がユーザーから届くことは、上で確かめた。U を立てるのは `entry::table_entry` である。）
                unsafe {
                    write_entry(
                        direct_map,
                        table,
                        index,
                        entry::table_entry(fresh, permissions),
                    )
                };
                fresh
            };
            table = child;
        }

        // **既にマップされている葉は上書きしない（S9-b-3-2b）。**
        //
        // **重なる区画を持つイメージがここへ来る。** 上書きすると、前の葉が指していた
        // フレームがマッピングから外れ、`detach` から見えなくなって 1 枚漏れる
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

        // **葉のビットは `entry::leaf_entry` が決める**（もう一方の経路の `ActivePageTable::map_4kib` と同じ変換）。
        // 破壊テスト `map-force-writable` は変換の中に在り、両方の経路に効く。
        let leaf = entry::leaf_entry(frame, permissions, entry::LeafSize::Small);
        // SAFETY: 葉。ユーザーから到達できる 4KiB ページ。
        unsafe { write_entry(direct_map, table, leaf_index, leaf) };
        // **葉もこの空間のものである**（`ADR-0063` の (b1)）。**マップした後で数える**
        // ——**途中で弾かれた枝（`AlreadyMapped` など）では、呼び出し側がフレームを持ったままである。**
        self.frames_taken += 1;
        Ok(())
    }

    /// この空間のために取ったフレームの本数（`ADR-0063` の (b1)）。
    ///
    /// **破棄の前に聞く。** [`AddressSpace::detach`] は自分を取るので、後からは聞けない。
    pub fn frames_taken(&self) -> usize {
        self.frames_taken
    }

    /// この空間のユーザーの側の項目を全部外し、外したフレームを `into` へ集める（S7-d。2026-10-02 に、
    /// 壊す入口のうちページテーブルを触る分だけにした）。
    ///
    /// 集めるのは、下位で使っていた葉のフレーム（共有の印の付いた葉を除く）、途中の表、最上位の表そのものである。
    /// **触るのは下位だけである**（`PRIVATE_INDEX_RANGE`）。上位は共有なので、ここで返したら他のアドレス空間の
    /// マッピングを壊す。
    ///
    /// 返すのは（`into` へ集めた本数, 入り切らなかった本数）。**入り切らなかったフレームは、どこにも返らない**
    /// （漏れる。早く返すより、漏らすほうが安全である）。
    ///
    /// **集めたフレームを、すぐにアロケータへ返してはならない。** ほかのコアの変換の控え（TLB）に、古い翻訳が
    /// 残りうる。**外した後に世代を上げ、その世代で隔離へ入れる順序は、呼ぶ側が持つ**
    /// （`crate::quarantine` の `retire_address_space`。`ADR-0027` の Addendum の不変条件）。
    ///
    /// # Safety
    ///
    /// - **この空間がどのコアでも稼働していないこと。** 稼働中の表を外すと、そのコアは次の翻訳で死ぬ。
    /// - **BKL を持って呼ぶこと。** マッピングの変更は BKL の内側でしか行わない（`ADR-0027` の Addendum の
    ///   失効条件）。呼ぶのは `crate::quarantine` の `retire_address_space` だけで、BKL を持っていることは、
    ///   そちらが引数（`BklGuard`）で受けて示す。
    /// - `direct_map` が、この空間の表を覆っていること。
    pub unsafe fn detach(
        self,
        direct_map: DirectMap,
        into: &mut [Option<PhysAddr>],
    ) -> (usize, usize) {
        use crate::arch::x86_64::paging::entry;

        let capacity = into.len();
        let mut count = 0usize;
        let mut leaked = 0usize;

        let mut collect = |frame: PhysAddr, count: &mut usize, leaked: &mut usize| {
            if *count < capacity {
                into[*count] = Some(frame);
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
                            // **共有メモリの葉は集めない（`ADR-0065` の「共有フレームの寿命」）。**
                            // **目印は PTE のビット 9（`is_shared`。立てるのは `mmap` だけ）。**
                            // **返すのは `crate::shm` の参照数である**——**ここで集めると、もう片側の
                            // fd がまだ在るのに返り、二重解放になる。** **テーブルのフレーム（下の
                            // `pt`/`pd`/…）はアロケータのものなので集める。**
                            if !entry::is_shared(pt_entry) {
                                collect(entry::page_address_4k(pt_entry), &mut count, &mut leaked);
                            }
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
        (count, leaked)
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
    for index in 0..crate::arch::x86_64::paging::entry::ENTRIES_PER_TABLE {
        // SAFETY: 呼び出し元契約。添字は 512 未満。
        unsafe { write_entry(direct_map, table, index, 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **指紋は、どの項目の値が変わっても、同じ値の項目が別の添字へ移っても変わり、0 にならない**
    /// （2026-09-27。`ADR-0071` の決定 5）。
    /// **起動の後に、カーネル側の添字へ項目を作る書き込みだけを断る**（2026-09-27。`ADR-0071` の決定 5）。
    /// **起動の間は断らず、ユーザー側の添字は起動の後も断らない。**
    #[test]
    fn a_kernel_half_entry_is_refused_only_after_boot() {
        for index in [KERNEL_PML4_FIRST_INDEX, 258, 260, PML4_ENTRY_COUNT - 1] {
            assert!(
                !kernel_top_write_is_refused(index, false),
                "index {index} during boot"
            );
            assert!(
                kernel_top_write_is_refused(index, true),
                "index {index} after boot"
            );
        }
        for index in [0, 1, KERNEL_PML4_FIRST_INDEX - 1] {
            assert!(
                !kernel_top_write_is_refused(index, true),
                "user index {index}"
            );
        }
    }

    #[test]
    fn the_kernel_top_digest_changes_with_any_entry_and_is_never_zero() {
        let base: Vec<(usize, u64)> = (KERNEL_PML4_FIRST_INDEX..PML4_ENTRY_COUNT)
            .map(|index| {
                (
                    index,
                    if index % 64 == 0 {
                        0x1000 * index as u64 | 3
                    } else {
                        0
                    },
                )
            })
            .collect();
        let digest = kernel_top_digest(base.iter().copied());
        assert_ne!(digest, 0);
        assert_eq!(digest, kernel_top_digest(base.iter().copied()));
        for position in 0..base.len() {
            let mut changed = base.clone();
            changed[position].1 ^= 1;
            assert_ne!(
                kernel_top_digest(changed),
                digest,
                "entry {}",
                base[position].0
            );
        }
        // **CPU が立てる A と D のビットは、指紋を変えない。**
        let mut walked = base.clone();
        walked[0].1 |= 1 << 5;
        walked[1].1 |= 1 << 6;
        assert_eq!(kernel_top_digest(walked), digest);
        let mut moved = base.clone();
        moved[0].1 = 0;
        moved[1].1 = base[0].1;
        assert_ne!(kernel_top_digest(moved), digest);
        assert_ne!(kernel_top_digest(core::iter::empty()), 0);
    }

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
