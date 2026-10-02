//! ページテーブルの独立読み戻し（H-2）。
//!
//! **構築側とコードを共有しない。** `table` や `active` の walk を呼ぶと、
//! 「同じ式で作ったものを同じ式で検算する」ことになり、構築側と検証側が
//! 同じ誤りを持つ場合に検出できない。ここでは階層を降りるループを
//! 独立に書いてある。
//!
//! # 何を共有し、何を共有しないか
//!
//! 共有してよいもの
//! - [`VirtAddr`] の添字抽出メソッド。T-1 でホストテスト済みで、
//!   中継の繋ぎ間違いも `entry` 側のテストで固定してある
//!
//! 共有しないもの
//! - 階層を降りるループ（PML4 → PDPT → PD → PT）
//! - エントリの解釈。Present / PS / アドレスマスクのビット位置を
//!   このファイルに書き直してある。**重複しているのが検出力の源である。**
//!   `entry` の定数を参照すると、そちらが誤っていた場合に一緒に誤る
//!
//! # CR3 に載っていないテーブルを辿る
//!
//! [`super::active::ActivePageTable`] は CR3 を読んで構築するため、
//! まだ載せていないテーブルには使えない。ここは PML4 の物理アドレスを
//! 引数で受け取る。

use common::addr::{DirectMap, PhysAddr, VirtAddr};

/// 独立に書き直したビット定義。**`entry` の定数を参照しない。**
mod bits {
    /// Present。
    pub const PRESENT: u64 = 1 << 0;
    /// Read/Write。立っていれば書き込める（2026-09-30。walk の結果を読むメソッドのために足した）。
    pub const WRITABLE: u64 = 1 << 1;
    /// User/Supervisor。立っていれば Ring 3 から到達可能（M5-e-2）。
    pub const USER: u64 = 1 << 2;
    /// PD / PDPT レベルの「ページそのもの」ビット。
    pub const PAGE_SIZE: u64 = 1 << 7;
    /// 中間テーブルと 4KiB ページのアドレス部分（ビット 12-51）。
    pub const ADDR_4K: u64 = 0x000F_FFFF_FFFF_F000;
    /// 2MiB ページのアドレス部分（ビット 21-51）。
    pub const ADDR_2M: u64 = 0x000F_FFFF_FFE0_0000;
    /// 実行の禁止。立っていれば、そのページの命令は実行できない（2026-10-02。walk の結果を読むメソッドのために足した）。
    pub const NO_EXECUTE: u64 = 1 << 63;
}

/// 独立 walk の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// 対応する物理アドレス（ページ内オフセットを含む）。
    pub phys: PhysAddr,
    /// 2MiB ページで解決されたか。
    pub huge: bool,
    /// 解決に使った葉のエントリの生の値（S9-b-1）。
    ///
    /// **フラグを読み戻すために持つ。** マップした側とは独立にここまで降りてきた
    /// 値なので、`W` や `U` が実際に立っているかをこの値で照合できる。
    ///
    /// **共通の側はこの欄のビットを直に読まず、メソッド（[`Resolved::leaf_writable`]・[`Resolved::leaf_user_accessible`]）
    /// を使う**（2026-09-30）。数値で直に読む形は、直下を通す検査では捕まらないので、ここに書いて残す。
    pub entry: u64,
}

impl Resolved {
    /// 葉のエントリが書き込みを許すか（x86 では R/W ビット。2026-09-30）。**共通の側はビットを読まず、これを使う。**
    ///
    /// **見るのは葉だけである**——書けるかは、途中の段のビットも揃って初めて決まる（x86）。
    pub const fn leaf_writable(&self) -> bool {
        self.entry & bits::WRITABLE != 0
    }

    /// 葉のエントリが実行を許すか（x86 では、実行禁止のビットが立っていないこと。2026-10-02）。
    ///
    /// **見るのは葉だけである**——途中の段に実行禁止が立っていれば実行できないが、途中の段には立てない
    /// （`entry::table_flags`）。
    pub const fn leaf_executable(&self) -> bool {
        self.entry & bits::NO_EXECUTE == 0
    }

    /// 葉のエントリがユーザーから触れる印を持つか（x86 では U/S ビット。2026-09-30）。
    ///
    /// **見るのは葉だけである**——中間の段も含めて確かめるのは [`walk_page_table_user_accessible`] である。
    pub const fn leaf_user_accessible(&self) -> bool {
        self.entry & bits::USER != 0
    }
}

/// 辿れなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkError {
    /// 途中の階層のエントリが不在。
    NotPresent,
    /// PDPT レベルで 1GiB ページに当たった。ZeikOS は作らない。
    GiantPage,
}

/// 指定した PML4 を根として、仮想アドレスを辿る。
///
/// # 契約（境界の関数。2026-09-30）
///
/// - 根から仮想番地を辿って訳を返すだけで、何も変えない。
/// - 共通の側は、読み込んだプログラムの写像の確かめ（`crate::userland`）に使う。書けるか・ユーザーから触れるかは、
///   訳のメソッド（[`Resolved::leaf_writable`]・[`Resolved::leaf_user_accessible`]）で読み、項目のビットを直に読まない。
///
/// # Safety
///
/// `pml4_phys` が有効なページテーブルフレームを指し、`direct_map` を通して
/// そのフレームと配下のテーブルが読めること。
pub unsafe fn walk_page_table(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    virt: VirtAddr,
) -> Result<Resolved, WalkError> {
    let read = |table: PhysAddr, index: usize| -> u64 {
        // SAFETY: 呼び出し元契約による。読み取りのみ。
        unsafe {
            core::ptr::read_volatile(direct_map.phys_to_virt(table).as_ptr::<u64>().add(index))
        }
    };

    // **降り方をここに独立して書く。** 構築側のループとは別物である。
    let pml4e = read(pml4_phys, virt.top_index());
    if pml4e & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }

    let pdpt = PhysAddr::new_const(pml4e & bits::ADDR_4K);
    let pdpte = read(pdpt, virt.upper_index());
    if pdpte & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }
    if pdpte & bits::PAGE_SIZE != 0 {
        return Err(WalkError::GiantPage);
    }

    let pd = PhysAddr::new_const(pdpte & bits::ADDR_4K);
    let pde = read(pd, virt.middle_index());
    if pde & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }
    if pde & bits::PAGE_SIZE != 0 {
        // 2MiB ページ。オフセットは下位 21 ビット。
        let base = pde & bits::ADDR_2M;
        let offset = virt.as_u64() & 0x1F_FFFF;
        return Ok(Resolved {
            phys: PhysAddr::new_const(base + offset),
            huge: true,
            entry: pde,
        });
    }

    let pt = PhysAddr::new_const(pde & bits::ADDR_4K);
    let pte = read(pt, virt.leaf_index());
    if pte & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }
    let base = pte & bits::ADDR_4K;
    let offset = virt.as_u64() & 0xFFF;
    Ok(Resolved {
        phys: PhysAddr::new_const(base + offset),
        huge: false,
        entry: pte,
    })
}

/// U/S 監査の結果（M5-e-2）。
///
/// テーブル全体を独立に歩き、present な全エントリ（中間 3 段 + 葉）の U/S を
/// 数える。ユーザーサブツリー（`user_pml4_index` 配下）は全て U=1、それ以外は
/// 全て U=0 が期待値。違反があれば `*_violations` が非ゼロになる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UserSupervisorAudit {
    /// ユーザーサブツリー配下で見た present エントリ数。
    pub user_entries: u64,
    /// そのうち U=0 だったもの（ユーザーページなのに Ring 3 から届かない）。
    pub user_violations: u64,
    /// ユーザーサブツリー以外で見た present エントリ数。
    pub kernel_entries: u64,
    /// そのうち U=1 だったもの（カーネルへの U=1 漏れ。権限分離の穴）。
    pub kernel_violations: u64,
}

/// 稼働中テーブルを独立に歩き、全 present エントリの U/S を監査する（M5-e-2）。
///
/// `user_pml4_index` の PML4 エントリとその配下（中間 + 葉）は全て U=1、
/// それ以外の present な PML4 エントリとその配下は全て U=0 であることを、
/// **構築側とは別のループ**で確かめる。降り方とビット定義はこのファイルの
/// [`bits`] に独立に書いてあり、`entry` の定数を参照しない。
///
/// 2MiB / 1GiB ページ（huge）は葉として扱い、そこで降りるのをやめる。
///
/// # Safety
///
/// [`walk_page_table`] と同じ契約。
pub unsafe fn audit_user_supervisor(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    user_pml4_index: usize,
) -> UserSupervisorAudit {
    let read = |table: PhysAddr, index: usize| -> u64 {
        // SAFETY: 呼び出し元契約による。読み取りのみ。
        unsafe {
            core::ptr::read_volatile(direct_map.phys_to_virt(table).as_ptr::<u64>().add(index))
        }
    };

    let mut audit = UserSupervisorAudit::default();

    // ある 1 本のエントリを、期待側（ユーザーかカーネルか）に応じて計上する。
    let account = |entry: u64, in_user_subtree: bool, a: &mut UserSupervisorAudit| {
        let is_user = entry & bits::USER != 0;
        if in_user_subtree {
            a.user_entries += 1;
            if !is_user {
                a.user_violations += 1;
            }
        } else {
            a.kernel_entries += 1;
            if is_user {
                a.kernel_violations += 1;
            }
        }
    };

    for pml4_index in 0..512usize {
        let pml4e = read(pml4_phys, pml4_index);
        if pml4e & bits::PRESENT == 0 {
            continue;
        }
        let in_user = pml4_index == user_pml4_index;
        account(pml4e, in_user, &mut audit);

        let pdpt = PhysAddr::new_const(pml4e & bits::ADDR_4K);
        for pdpt_index in 0..512usize {
            let pdpte = read(pdpt, pdpt_index);
            if pdpte & bits::PRESENT == 0 {
                continue;
            }
            account(pdpte, in_user, &mut audit);
            if pdpte & bits::PAGE_SIZE != 0 {
                // 1GiB ページ。葉として扱い、これ以上降りない。
                continue;
            }

            let pd = PhysAddr::new_const(pdpte & bits::ADDR_4K);
            for pd_index in 0..512usize {
                let pde = read(pd, pd_index);
                if pde & bits::PRESENT == 0 {
                    continue;
                }
                account(pde, in_user, &mut audit);
                if pde & bits::PAGE_SIZE != 0 {
                    // 2MiB ページ。葉として扱う。
                    continue;
                }

                let pt = PhysAddr::new_const(pde & bits::ADDR_4K);
                for pt_index in 0..512usize {
                    let pte = read(pt, pt_index);
                    if pte & bits::PRESENT == 0 {
                        continue;
                    }
                    account(pte, in_user, &mut audit);
                }
            }
        }
    }

    audit
}

/// カーネルがユーザーの範囲へどう触るか（2026-10-01）。[`walk_page_table_user_accessible`] に渡す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAccess {
    /// 読む。ユーザーから触れることだけを求める。
    Read,
    /// 書く。ユーザーから触れることに加えて、書き込み可を求める。
    Write,
}

/// ユーザーアクセス可能性の walk の失敗理由（M5-f-2-1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAccessError {
    /// 途中の階層または葉が不在。
    NotPresent,
    /// present だが、ある階層で U=0（Ring 3 から到達不可）。**U/S は全階層の AND**
    /// なので、中間階層が U=0 でも Ring 3 からは届かない。
    SupervisorOnly,
    /// present でユーザーから触れるが、ある階層で W=0（書けない）。[`UserAccess::Write`] のときだけ返す。
    /// **書き込み可も全階層の AND である。**
    ReadOnly,
    /// PDPT レベルで 1GiB ページ。ZeikOS は作らない。
    GiantPage,
    /// PD レベルで 2MiB ページ。ユーザーページは 4KiB のみを想定し、想定外として弾く。
    HugePage,
}

/// 1 つの階層のエントリが、求める触り方を許すか（純粋な論理。2026-10-01）。
///
/// **present と U=1 はどの触り方でも求め、[`UserAccess::Write`] なら W=1 も求める。** 順は present、U、W である
/// （不在のエントリのほかのビットは意味を持たない）。
fn level_allows(entry: u64, access: UserAccess) -> Result<(), UserAccessError> {
    if entry & bits::PRESENT == 0 {
        return Err(UserAccessError::NotPresent);
    }
    // 破壊テスト (M5-f-2-1, skip-us): U=1 判定を外す。ユーザー範囲内で present だが U=0 の
    // ページ（無効3）が誤って受理され、battery が「拒否すべきを受理」を検出して halt
    // する。walk_page_table_user_accessible を新設した中核（U 判定）そのものの破壊テストでの確認。
    #[cfg(not(feature = "syscall-test-validate-skip-us"))]
    if entry & bits::USER == 0 {
        return Err(UserAccessError::SupervisorOnly);
    }
    // 破壊テスト (2026-10-01, validate-skip-writable): 書き込み可を見ない。読み取り専用のユーザーページを
    // 書き込み先に渡した `read` が断られず、カーネルが書いて Ring 0 の #PF で止まる。
    if access == UserAccess::Write
        && entry & bits::WRITABLE == 0
        && !cfg!(feature = "syscall-test-validate-skip-writable")
    {
        return Err(UserAccessError::ReadOnly);
    }
    Ok(())
}

/// 指定 VA が Ring 3 からアクセス可能な 4KiB ユーザーページに解決されることを、
/// **各階層で present かつ U=1** を確かめながら独立に walk して判定する（M5-f-2-1）。
/// **`access` が [`UserAccess::Write`] なら、各階層で W=1 も確かめる**（2026-10-01）——カーネルがユーザーの
/// 範囲へ書く前の確かめである。書き込み禁止のページへカーネルが書くと、`CR0.WP` が立っていれば Ring 0 の
/// #PF になり、直接写像を通して書けば CPU は書き込み禁止を守らない。どちらも、書く前にここで断る。
///
/// 既存の [`walk_page_table`] は present のみを見る（M5-e-2 の呼び出し元がそれを前提にする）ため
/// 別関数にする。**U/S は全階層の AND であり、葉の U だけを見る `translate` では中間
/// 階層の U=0 を見逃す。** ここは PML4→PT の全階層で U=1 を要求してその穴を塞ぐ。
/// 既存 walk / audit_user_supervisor の契約は一切変えない。
///
/// # 契約（境界の関数。2026-09-30）
///
/// - 根から辿り、どの段でもユーザーから触れる印（書くなら書き込み可の印も）が立っているかを確かめるだけで、
///   何も変えない。
/// - 共通の側は、ユーザーのポインタを確かめる所（`crate::syscall`）で使う。
/// - **答えは呼んだ時点の表についてのものである。** 確かめた後に表が変わらないことは、呼ぶ側が保証する。
///
/// # Safety
///
/// [`walk_page_table`] と同じ契約。
pub unsafe fn walk_page_table_user_accessible(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    virt: VirtAddr,
    access: UserAccess,
) -> Result<(), UserAccessError> {
    let read = |table: PhysAddr, index: usize| -> u64 {
        // SAFETY: 呼び出し元契約による。読み取りのみ。
        unsafe {
            core::ptr::read_volatile(direct_map.phys_to_virt(table).as_ptr::<u64>().add(index))
        }
    };

    let pml4e = read(pml4_phys, virt.top_index());
    level_allows(pml4e, access)?;

    let pdpt = PhysAddr::new_const(pml4e & bits::ADDR_4K);
    let pdpte = read(pdpt, virt.upper_index());
    level_allows(pdpte, access)?;
    if pdpte & bits::PAGE_SIZE != 0 {
        return Err(UserAccessError::GiantPage);
    }

    let pd = PhysAddr::new_const(pdpte & bits::ADDR_4K);
    let pde = read(pd, virt.middle_index());
    level_allows(pde, access)?;
    if pde & bits::PAGE_SIZE != 0 {
        return Err(UserAccessError::HugePage);
    }

    let pt = PhysAddr::new_const(pde & bits::ADDR_4K);
    let pte = read(pt, virt.leaf_index());
    level_allows(pte, access)?;

    Ok(())
}

/// 指定した PML4 の、ある階層のエントリをそのまま読む。
///
/// 恒等側のテーブルが構築処理で書き換わっていないことを確かめるために使う。
///
/// # 契約（境界の関数。2026-09-30）
///
/// - 根の表の 1 項目をそのまま読むだけで、何も変えない。
/// - 共通の側は、AP の起動で恒等の写像が消えたことを確かめるのに使う（`crate::smp`）。
///
/// # Safety
///
/// [`walk_page_table`] と同じ契約。
pub unsafe fn read_top_level_entry(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    index: usize,
) -> u64 {
    // SAFETY: 呼び出し元契約による。読み取りのみ。
    unsafe {
        core::ptr::read_volatile(
            direct_map
                .phys_to_virt(pml4_phys)
                .as_ptr::<u64>()
                .add(index),
        )
    }
}

/// 指定した PML4 インデックス配下の中間テーブルフレーム（PDPT/PD/PT）の物理
/// アドレスを、重複を除いて `out` へ集める。葉（1GiB/2MiB/4KiB ページ）が指す
/// データフレームは含めない。返り値は集めた枚数。`out` に収まらなければ `None`。
///
/// 恒等除去（B-2b-4）のフレーム会計に使う。`PML4[0]` 配下の何枚が到達不能に
/// なるか（意図的リーク）、また `PML4[0]/[256]/[511]` 各配下のフレーム集合が
/// 交わらないか（共有があると「落とせば到達不能になる」前提が崩れる。bootstrap
/// テーブルは PD_shared を `[0]` と `[511]` で共有していた前例がある）を、落とす前に
/// 実測する。重複除去しているので、共有されたテーブルフレームがあっても二重に数えない。
///
/// # Safety
///
/// [`walk_page_table`] と同じ契約。
pub(crate) unsafe fn collect_subtree_table_frames(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    pml4_index: usize,
    out: &mut [PhysAddr],
) -> Option<usize> {
    let read = |table: PhysAddr, index: usize| -> u64 {
        // SAFETY: 呼び出し元契約による。読み取りのみ。
        unsafe {
            core::ptr::read_volatile(direct_map.phys_to_virt(table).as_ptr::<u64>().add(index))
        }
    };

    let mut count = 0usize;
    // 既に `out[..count]` にあれば何もしない。無ければ push する。溢れたら None。
    let mut push = |frame: PhysAddr, count: &mut usize| -> Option<()> {
        if out[..*count].contains(&frame) {
            return Some(());
        }
        if *count >= out.len() {
            return None;
        }
        out[*count] = frame;
        *count += 1;
        Some(())
    };

    let pml4e = read(pml4_phys, pml4_index);
    if pml4e & bits::PRESENT == 0 {
        return Some(0);
    }
    let pdpt = PhysAddr::new_const(pml4e & bits::ADDR_4K);
    push(pdpt, &mut count)?;

    for pdpt_index in 0..512usize {
        let pdpte = read(pdpt, pdpt_index);
        if pdpte & bits::PRESENT == 0 {
            continue;
        }
        if pdpte & bits::PAGE_SIZE != 0 {
            // 1GiB ページ（葉）。データフレームなので数えない。
            continue;
        }
        let pd = PhysAddr::new_const(pdpte & bits::ADDR_4K);
        push(pd, &mut count)?;

        for pd_index in 0..512usize {
            let pde = read(pd, pd_index);
            if pde & bits::PRESENT == 0 {
                continue;
            }
            if pde & bits::PAGE_SIZE != 0 {
                // 2MiB ページ（葉）。
                continue;
            }
            let pt = PhysAddr::new_const(pde & bits::ADDR_4K);
            push(pt, &mut count)?;
            // PT 配下は 4KiB 葉のみ。テーブルフレームではないので降りない。
        }
    }

    Some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **walk の訳のメソッドは、葉のエントリの R/W と U/S だけを見る**（2026-09-30）。ほかのビットが立っていても
    /// 答えは変わらない。
    #[test]
    fn a_resolved_leaf_answers_writable_and_user_from_its_own_bits() {
        let leaf = |entry| Resolved {
            phys: PhysAddr::new_const(0x1000),
            huge: false,
            entry,
        };
        assert!(!leaf(bits::PRESENT).leaf_writable());
        assert!(!leaf(bits::PRESENT).leaf_user_accessible());
        assert!(leaf(bits::PRESENT | bits::WRITABLE).leaf_writable());
        assert!(!leaf(bits::PRESENT | bits::WRITABLE).leaf_user_accessible());
        assert!(leaf(bits::PRESENT | bits::USER).leaf_user_accessible());
        assert!(!leaf(bits::PRESENT | bits::USER).leaf_writable());
        let all =
            leaf(bits::PRESENT | bits::WRITABLE | bits::USER | bits::PAGE_SIZE | bits::ADDR_4K);
        assert!(all.leaf_writable() && all.leaf_user_accessible());
    }

    /// **1 つの階層の確かめは、読むなら present と U、書くなら W も求める**（2026-10-01）。
    /// **読み取り専用のユーザーページ（present・U=1・W=0）は、読むなら通り、書くなら断る。**
    #[test]
    fn a_level_allows_a_write_only_when_it_is_writable_as_well() {
        let read_only = bits::PRESENT | bits::USER;
        let writable = bits::PRESENT | bits::USER | bits::WRITABLE;
        assert_eq!(level_allows(read_only, UserAccess::Read), Ok(()));
        assert_eq!(
            level_allows(read_only, UserAccess::Write),
            Err(UserAccessError::ReadOnly)
        );
        assert_eq!(level_allows(writable, UserAccess::Read), Ok(()));
        assert_eq!(level_allows(writable, UserAccess::Write), Ok(()));
        // 不在は、ほかのビットが立っていても不在である。
        for access in [UserAccess::Read, UserAccess::Write] {
            assert_eq!(
                level_allows(bits::USER | bits::WRITABLE, access),
                Err(UserAccessError::NotPresent)
            );
            // カーネルだけのページは、書き込み可でも断る。**理由は「ユーザーから触れない」が先である。**
            assert_eq!(
                level_allows(bits::PRESENT | bits::WRITABLE, access),
                Err(UserAccessError::SupervisorOnly)
            );
            assert_eq!(
                level_allows(bits::PRESENT, access),
                Err(UserAccessError::SupervisorOnly)
            );
        }
    }
}
