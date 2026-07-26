//! 恒等（`PML4[0]`）の除去（higher-half B-2b-4）。
//!
//! **不可逆な一手だが、CR3 リロード（全 TLB フラッシュ）より前に検証し、失敗
//! すれば書き戻して復帰できる前段を持つ。** A-1/B-1 が確立した「落とす→独立
//! walker で検証→駄目なら巻き戻し→良ければ確定」の型を除去へ適用する。
//!
//! 除去点そのもの（呼び出し位置）は `main.rs` の `_start` にある。恒等窓を握る
//! 全検証サイトと boot_info の消費が済んだ後でなければならない（順序依存）。
//! 手順と恒等前提の網羅列挙は docs/verification-coverage.md の
//! 「higher-half B-2b」を参照。
//!
//! # なぜ本流ロジックを lib 側に置くか
//! 書き込み primitive（[`super::table::clear_pml4_entry`] /
//! [`super::table::restore_pml4_entry`]）と表フレーム走査
//! （[`super::verify::collect_subtree_table_frames`]）を `pub(crate)` に保ちつつ
//! 呼ぶには、呼び出し側も同じ lib クレート内である必要がある（`main.rs` は別の
//! bin クレートで、lib の `pub(crate)` は見えない）。鋭利な道具の可視性を lib 内へ
//! 閉じ込めるため、オーケストレーションをここへ置き、`main.rs` からは
//! [`remove_identity`]（`pub`）だけを呼ぶ。

use common::addr::{DirectMap, PhysAddr, VirtAddr};
use common::cpu;
use common::log::Logger;
use common::serial::SerialPort;

use super::{switch, table, verify};

/// 恒等除去で walk する必須領域。名前と高位 VA の対。
///
/// **恒等が無くても解決できねばならない領域。** ここに挙げた VA が除去後に独立
/// walker で present な葉へ解決すれば、CR3 リロード（全 TLB フラッシュ）へ進んで
/// よい。RIP・RSP・direct map 窓・カーネルイメージは lib が自分で導けるので
/// [`remove_identity`] が内部で足す。呼び出し側が渡すのは lib からは知り得ない
/// 高位 VA（ヒープ・フレームバッファ）だけである。
pub struct RequiredRegion {
    /// ログに出す領域名。
    pub name: &'static str,
    /// 検証する高位 VA。
    pub va: VirtAddr,
}

/// 恒等除去のフレーム会計（除去手順の (2)）。落とす前に閉じる。
///
/// `PML4[0]` 配下の中間テーブルフレーム数を数えてログに出し（何枚が到達不能に
/// なるか＝意図的リーク）、`PML4[0]/[256]/[511]` 各配下のフレーム集合が互いに
/// 交わらないことを実測する。交わり（共有）があると「`PML4[0]` を落とせば配下が
/// 到達不能になる」前提が崩れ、リーク数が過大になる（bootstrap 表は PD_shared を
/// `[0]` と `[511]` で共有していた前例がある）。
fn frame_accounting_before_removal(
    logger: &mut Logger<SerialPort>,
    cr3: PhysAddr,
    direct_map: DirectMap,
) {
    // QEMU 既定構成では各サブツリーの表フレームは 1 桁〜十数枚に収まる（恒等
    // ≒2GiB、direct map ≒物理全域を 2MiB ページ、kernel ≒数百 KiB）。溢れるのは
    // 物理が極端に大きい構成のときで、その場合は (3) の除去より前にここで halt
    // するので恒等は保たれたまま安全に失敗する。ブートスタック（16KiB）を圧迫
    // しないよう控えめにしてある。
    const CAP: usize = 64;
    let mut buf0 = [PhysAddr::new_const(0); CAP];
    let mut buf256 = [PhysAddr::new_const(0); CAP];
    let mut buf511 = [PhysAddr::new_const(0); CAP];

    // SAFETY: cr3 は稼働中の自前テーブルを指し、direct_map（高位窓）で配下の
    // テーブルを読める。読み取りのみ。
    let counts = unsafe {
        (
            verify::collect_subtree_table_frames(cr3, direct_map, 0, &mut buf0),
            verify::collect_subtree_table_frames(cr3, direct_map, 256, &mut buf256),
            verify::collect_subtree_table_frames(cr3, direct_map, 511, &mut buf511),
        )
    };
    let (n0, n256, n511) = match counts {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => {
            logger.error(format_args!(
                "identity-removal: frame accounting buffer overflow (CAP={CAP}); \
                 halting before touching the identity mapping"
            ));
            cpu::halt_forever();
        }
    };

    let s0 = &buf0[..n0];
    let s256 = &buf256[..n256];
    let s511 = &buf511[..n511];
    let disjoint = |a: &[PhysAddr], b: &[PhysAddr]| a.iter().all(|x| !b.contains(x));
    let d0_256 = disjoint(s0, s256);
    let d0_511 = disjoint(s0, s511);
    let d256_511 = disjoint(s256, s511);
    let leaked_kib = (n0 as u64) * 4;

    logger.info(format_args!(
        "identity-removal: PML4[0] subtree table frames = {n0} ({leaked_kib} KiB) become unreachable; \
         intentional leak. subtree frames [256]={n256} [511]={n511}. \
         disjoint(0,256)={d0_256} disjoint(0,511)={d0_511} disjoint(256,511)={d256_511}"
    ));

    if !(d0_256 && d0_511 && d256_511) {
        // 共有があってもリーク数が過大になるだけで除去自体は安全（落とすのは
        // PML4[0] の1エントリのみで、共有された表フレームは他経路から到達可能な
        // まま残る）。会計が狂うので目立つ形で警告する。
        logger.warn(format_args!(
            "identity-removal: subtree table frames are NOT pairwise disjoint; the unreachable-frame \
             count above is an overcount (shared frames stay reachable via another PML4 entry). \
             removal is still safe, but investigate the sharing"
        ));
    }
}

/// 恒等（`PML4[0]`）を外す。B-2b-4 の本体。手順は6段で、
/// docs/verification-coverage.md の「higher-half B-2b」の除去手順に対応する。
///
/// `high_mapped` は「恒等が無くても解決できねばならない、高位化した低位ポインタ」
/// のうち **lib からは知り得ないもの**（ヒープの高位 VA・フレームバッファの高位
/// VA）。RIP・RSP・direct map 窓・カーネルイメージは lib が自分で導けるので内部で
/// 足す（下記の同期の穴を減らすため）。
///
/// # 必須領域リストの同期（安全網の前提）
/// **原則: lib が自分で導けるものは内部に持ち（忘れられない）、呼び出し側に
/// 渡させるのは lib からは知り得ないものだけにする。** カーネルイメージは
/// `link_symbols`（`KERNEL_VIRT_BASE + KERNEL_LOAD_ADDR` = イメージ先頭 VMA）から
/// 導けるので内部に持つ。ヒープ・フレームバッファの高位 VA は実行時のフレーム
/// 確保・boot_info の物理値に依存し lib からは決まらないので呼び出し側が渡す。
/// **新しく低位ポインタを高位化したとき、それが lib から導けないものなら
/// 呼び出し側の `high_mapped` に足すこと。** このリストは
/// docs/verification-coverage.md の「解消済み」（高位化した）群と一致していなければ
/// ならない。食い違うと、落とした後に解決できない領域を見逃してフラッシュし、
/// 復帰不能で死ぬ。これは列挙した領域の安全網であって完全性の保証ではない
/// （リストに無い何かが壊れていればフラッシュ後に死ぬ。完全性は依然として grep に
/// よる sweep に依存する）。
///
/// # Safety
/// 稼働中の CR3 が自前テーブルを指し、`direct_map`（高位窓）でその配下を読み書き
/// できること。呼び出し時点で恒等窓を握る全検証サイトと boot_info の消費が
/// 済んでいること（順序依存。除去点より前に走ること）。
pub unsafe fn remove_identity(
    logger: &mut Logger<SerialPort>,
    direct_map: DirectMap,
    high_mapped: &[RequiredRegion],
) {
    const IDENTITY_INDEX: usize = 0;
    const PRESENT: u64 = 1 << 0;

    // (1) 稼働 PML4 と PML4[0] を控える。
    let cr3 = switch::read_cr3();
    // SAFETY: cr3 は稼働中の自前テーブル、direct_map（高位窓）でそのフレームを
    // 読める。読み取りのみ。
    let saved0 = unsafe { verify::read_pml4_entry(cr3, direct_map, IDENTITY_INDEX) };
    logger.info(format_args!(
        "identity-removal: begin. live PML4={:#x}, saved PML4[0]={saved0:#x}",
        cr3.as_u64()
    ));

    // (2) 落とす前にフレーム会計を閉じる。
    frame_accounting_before_removal(logger, cr3, direct_map);

    // (3) PML4[0] を落とす（単一の8バイト書き込み。unmap_4kib は使わない）。この
    // 時点では TLB に低位変換が残るので実行を継続できる。
    // SAFETY: clear_pml4_entry の契約。落とすのは恒等スロットのみ。現在の実行文脈
    // （RIP/RSP/このコードが触るデータ）は高位 VA なので恒等に依存しない。万一
    // (4) が失敗しても CR3 リロード前なので (5a) で saved0 を書き戻せば、TLB が
    // 生きたまま恒等が復活する。
    unsafe { table::clear_pml4_entry(cr3, direct_map, IDENTITY_INDEX) };

    // (4) 独立 walker で必須領域を検証する。RIP/RSP/direct map 窓/カーネルイメージ
    // は lib が導けるので内部で足す。high_mapped（ヒープ・FB）は呼び出し側が渡す。
    // 低位の一点（恒等が覆っていた VA）を後の確認用に控える。恒等窓では phys==virt
    // なので、PML4 フレームの物理値をそのまま低位 VA として使う。
    let rip = cpu::read_rip();
    let rsp = cpu::read_rsp();
    let dm_window = direct_map.phys_to_virt(cr3);
    // カーネルイメージ先頭 VMA。lib が link_symbols から導ける（main.rs の
    // image_lo と同じ式）ので、忘れないよう内部で持つ。
    let kernel_image = VirtAddr::new(
        crate::link_symbols::KERNEL_VIRT_BASE + crate::link_symbols::KERNEL_LOAD_ADDR,
    )
    .expect("the kernel image base is canonical");
    let low_probe = VirtAddr::new(cr3.as_u64()).expect("a low physical address is canonical");
    let always = [
        RequiredRegion {
            name: "RIP",
            va: VirtAddr::new(rip).expect("RIP is canonical"),
        },
        RequiredRegion {
            name: "RSP",
            va: VirtAddr::new(rsp).expect("RSP is canonical"),
        },
        RequiredRegion {
            name: "direct map window",
            va: dm_window,
        },
        RequiredRegion {
            name: "kernel image",
            va: kernel_image,
        },
    ];

    let mut all_ok = true;
    for region in always.iter().chain(high_mapped.iter()) {
        // SAFETY: cr3 は稼働テーブル、direct_map で辿れる。読み取りのみ。
        match unsafe { verify::walk(cr3, direct_map, region.va) } {
            Ok(res) => logger.info(format_args!(
                "identity-removal: required [{}] {:#x} -> phys {:#x} (huge={}): OK",
                region.name,
                region.va.as_u64(),
                res.phys.as_u64(),
                res.huge
            )),
            Err(e) => {
                all_ok = false;
                logger.error(format_args!(
                    "identity-removal: required [{}] {:#x}: FAILED ({e:?})",
                    region.name,
                    region.va.as_u64()
                ));
            }
        }
    }
    // PML4[0] が空になったこと。
    // SAFETY: cr3 は稼働テーブル、direct_map で読める。読み取りのみ。
    let pml4_0_after = unsafe { verify::read_pml4_entry(cr3, direct_map, IDENTITY_INDEX) };
    let pml4_0_empty = pml4_0_after & PRESENT == 0;
    logger.info(format_args!(
        "identity-removal: PML4[0] after clear = {pml4_0_after:#x} (empty={pml4_0_empty})"
    ));
    if !pml4_0_empty {
        all_ok = false;
    }

    if !all_ok {
        // (5a) 検証失敗。書き戻して復帰し、halt する。CR3 リロード前なので TLB が
        // 生きており、書き戻しで恒等が復活する。
        // SAFETY: restore_pml4_entry の契約。saved0 は (1) で控えた恒等エントリ。
        unsafe { table::restore_pml4_entry(cr3, direct_map, IDENTITY_INDEX, saved0) };
        // 書き戻した後、低位 VA が walk で present に戻っていることを確認してから
        // halt する（「書き戻した」だけでなく「恒等が実際に復活した」ことまで見る）。
        // SAFETY: cr3 は稼働テーブル、direct_map で読める。読み取りのみ。
        let low_after = unsafe { verify::walk(cr3, direct_map, low_probe) };
        let revived = low_after.is_ok();
        logger.error(format_args!(
            "identity-removal: verification FAILED. restored PML4[0]; low VA {:#x} walk after restore = \
             {low_after:?} revived={revived} (expected Ok = identity revived). halting",
            low_probe.as_u64()
        ));
        cpu::halt_forever();
    }

    // (5b) 成功。CR3 リロードで全 TLB をフラッシュする。invlpg では下位 512GiB 分を
    // 無効化できず、G=0 維持なので同じ PML4 を載せ直せば確実に落ちる。
    // SAFETY: 直前に必須領域が恒等なしで解決することを walk で確認した。同じ稼働
    // テーブル（cr3）を載せ直すだけ。
    unsafe { switch::switch_to(cr3) };

    // (6) フラッシュ後の読み戻し（walk。デレフしない）。高位が健全、低位が未マップ。
    let mut post_ok = true;
    for region in always.iter().chain(high_mapped.iter()) {
        // SAFETY: cr3 は稼働テーブル、direct_map で辿れる。読み取りのみ。
        if let Err(e) = unsafe { verify::walk(cr3, direct_map, region.va) } {
            post_ok = false;
            logger.error(format_args!(
                "identity-removal: post-flush [{}] {:#x} unexpectedly unmapped ({e:?})",
                region.name,
                region.va.as_u64()
            ));
        }
    }
    // 低位 VA は未マップであること（デレフするとフォルトするので walk で読む）。
    // SAFETY: cr3 は稼働テーブル、direct_map で読める。読み取りのみ。
    let low_after = unsafe { verify::walk(cr3, direct_map, low_probe) };
    let low_unmapped = matches!(low_after, Err(verify::WalkError::NotPresent));
    logger.info(format_args!(
        "identity-removal: post-flush low VA {:#x} walk = {low_after:?} (unmapped={low_unmapped})",
        low_probe.as_u64()
    ));
    if !low_unmapped {
        post_ok = false;
    }

    if !post_ok {
        logger.error(format_args!(
            "identity-removal: post-flush readback inconsistent; halting"
        ));
        cpu::halt_forever();
    }

    common::addr::mark_identity_removed();
    logger.info(format_args!(
        "identity-removal: done. identity (PML4[0]) removed; IDENTITY_REMOVED gate armed"
    ));
}
