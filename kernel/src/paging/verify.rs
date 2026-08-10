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
    /// User/Supervisor。立っていれば Ring 3 から到達可能（M5-e-2）。
    pub const USER: u64 = 1 << 2;
    /// PD / PDPT レベルの「ページそのもの」ビット。
    pub const PAGE_SIZE: u64 = 1 << 7;
    /// 中間テーブルと 4KiB ページのアドレス部分（ビット 12-51）。
    pub const ADDR_4K: u64 = 0x000F_FFFF_FFFF_F000;
    /// 2MiB ページのアドレス部分（ビット 21-51）。
    pub const ADDR_2M: u64 = 0x000F_FFFF_FFE0_0000;
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
    /// **フラグを読み戻すために持つ。** 張った側とは独立にここまで降りてきた
    /// 値なので、`W` や `U` が実際に立っているかをこの値で照合できる。
    pub entry: u64,
}

/// 辿れなかった理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkError {
    /// 途中の階層のエントリが不在。
    NotPresent,
    /// PDPT レベルで 1GiB ページに当たった。ZaytOS は作らない。
    GiantPage,
}

/// 指定した PML4 を根として、仮想アドレスを辿る。
///
/// # Safety
///
/// `pml4_phys` が有効なページテーブルフレームを指し、`direct_map` を通して
/// そのフレームと配下のテーブルが読めること。
pub unsafe fn walk(
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
    let pml4e = read(pml4_phys, virt.pml4_index());
    if pml4e & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }

    let pdpt = PhysAddr::new_const(pml4e & bits::ADDR_4K);
    let pdpte = read(pdpt, virt.pdpt_index());
    if pdpte & bits::PRESENT == 0 {
        return Err(WalkError::NotPresent);
    }
    if pdpte & bits::PAGE_SIZE != 0 {
        return Err(WalkError::GiantPage);
    }

    let pd = PhysAddr::new_const(pdpte & bits::ADDR_4K);
    let pde = read(pd, virt.pd_index());
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
    let pte = read(pt, virt.pt_index());
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
/// [`walk`] と同じ契約。
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

/// ユーザーアクセス可能性の walk の失敗理由（M5-f-2-1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAccessError {
    /// 途中の階層または葉が不在。
    NotPresent,
    /// present だが、ある階層で U=0（Ring 3 から到達不可）。**U/S は全階層の AND**
    /// なので、中間階層が U=0 でも Ring 3 からは届かない。
    SupervisorOnly,
    /// PDPT レベルで 1GiB ページ。ZaytOS は作らない。
    GiantPage,
    /// PD レベルで 2MiB ページ。ユーザーページは 4KiB のみを想定し、想定外として弾く。
    HugePage,
}

/// 指定 VA が Ring 3 からアクセス可能な 4KiB ユーザーページに解決されることを、
/// **各階層で present かつ U=1** を確かめながら独立に walk して判定する（M5-f-2-1）。
///
/// 既存の [`walk`] は present のみを見る（M5-e-2 の呼び出し元がそれを前提にする）ため
/// 別関数にする。**U/S は全階層の AND であり、葉の U だけを見る `translate` では中間
/// 階層の U=0 を見逃す。** ここは PML4→PT の全階層で U=1 を要求してその穴を塞ぐ。
/// 既存 walk / audit_user_supervisor の契約は一切変えない。
///
/// # Safety
///
/// [`walk`] と同じ契約。
pub unsafe fn walk_user_accessible(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    virt: VirtAddr,
) -> Result<(), UserAccessError> {
    let read = |table: PhysAddr, index: usize| -> u64 {
        // SAFETY: 呼び出し元契約による。読み取りのみ。
        unsafe {
            core::ptr::read_volatile(direct_map.phys_to_virt(table).as_ptr::<u64>().add(index))
        }
    };

    // ある階層のエントリが present && U=1 であることを確かめる。
    let present_and_user = |entry: u64| -> Result<(), UserAccessError> {
        if entry & bits::PRESENT == 0 {
            return Err(UserAccessError::NotPresent);
        }
        // 破壊 (M5-f-2-1, skip-us): U=1 判定を外す。ユーザー範囲内で present だが U=0 の
        // ページ（無効3）が誤って受理され、battery が「拒否すべきを受理」を検出して halt
        // する。walk_user_accessible を新設した中核（U 判定）そのものの破壊確認。
        #[cfg(not(feature = "syscall-test-validate-skip-us"))]
        if entry & bits::USER == 0 {
            return Err(UserAccessError::SupervisorOnly);
        }
        Ok(())
    };

    let pml4e = read(pml4_phys, virt.pml4_index());
    present_and_user(pml4e)?;

    let pdpt = PhysAddr::new_const(pml4e & bits::ADDR_4K);
    let pdpte = read(pdpt, virt.pdpt_index());
    present_and_user(pdpte)?;
    if pdpte & bits::PAGE_SIZE != 0 {
        return Err(UserAccessError::GiantPage);
    }

    let pd = PhysAddr::new_const(pdpte & bits::ADDR_4K);
    let pde = read(pd, virt.pd_index());
    present_and_user(pde)?;
    if pde & bits::PAGE_SIZE != 0 {
        return Err(UserAccessError::HugePage);
    }

    let pt = PhysAddr::new_const(pde & bits::ADDR_4K);
    let pte = read(pt, virt.pt_index());
    present_and_user(pte)?;

    Ok(())
}

/// 指定した PML4 の、ある階層のエントリをそのまま読む。
///
/// 恒等側のテーブルが構築処理で書き換わっていないことを確かめるために使う。
///
/// # Safety
///
/// [`walk`] と同じ契約。
pub unsafe fn read_pml4_entry(pml4_phys: PhysAddr, direct_map: DirectMap, index: usize) -> u64 {
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
/// 表は PD_shared を `[0]` と `[511]` で共有していた前例がある）を、落とす前に
/// 実測する。重複除去しているので、共有された表フレームがあっても二重に数えない。
///
/// # Safety
///
/// [`walk`] と同じ契約。
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
            // PT 配下は 4KiB 葉のみ。表フレームではないので降りない。
        }
    }

    Some(count)
}
