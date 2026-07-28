//! APIC の MMIO を direct map 窓へ写像し、Local APIC を読めることを確かめる（S1-c）。
//!
//! # この段でやらないこと
//!
//! **APIC へは移行しない。** 割り込みは PIC のままである（S2 が移行する）。
//! ここでやるのは写像と、写像が効いていることの確認だけで、APIC のレジスタへ
//! **一切書き込まない**。AP 起こし（S3）、per-CPU、IPI も範囲外である。
//!
//! # 粒度が 4KiB であることは、書いた結果ではなく構造で決まる
//!
//! [`ActivePageTable::map_4kib`] には 2MiB へ昇格する経路が存在しない。
//! したがって「意図せず 2MiB で張ってしまう」形はここには書けない。
//!
//! 対照的に、ページテーブル構築時の `extra`（`paging::plan`）へ足す形は採らない。
//! 順序が合わない（MADT を読むのは写像計画を組み立てた後である）ことに加えて、
//! `MappedRanges::build` の隣接結合を通るため、**将来 `0xFEC00000` や
//! `0xFEE01000` に隣接する MMIO 記述子を出すファームウェアでは、結合された
//! 範囲の 2MiB 整列した核が huge へ昇格しうる。** 実測の構成では起きないが、
//! 起きない理由が「今のファームウェアがそう出すから」になる。ここを通せば、
//! その依存が構造的に消える。
//!
//! # なぜ 2MiB で張らないのか
//!
//! Local APIC（実測 `0xFEE00000`）も IO-APIC（実測 `0xFEC00000`）も 2MiB 境界に
//! 載っているので、2MiB で張ること自体はできる。しかしそうすると HPET など
//! 近傍のデバイス MMIO まで巻き込んで写す。属性が PCD なので直ちに害は無いが、
//! 意図していない領域を写すことになる（`deferred-decisions.md`）。
//!
//! # 異常はすべて報告して継続する
//!
//! `acpi` と同じである。S1 は情報を集める段で、APIC を触れないだけで単一コアの
//! カーネルが起動しなくなるのは機能的な後退である。致命へ格上げするのは S2 である。

use common::addr::PhysAddr;
use common::cpu;
use common::log::Logger;
use common::serial::SerialPort;

use crate::acpi::{ApicMmio, IoApicLocation};
use crate::frame_allocator::{FrameAllocator, FRAME_SIZE};
use crate::paging::active::{ActivePageTable, MapUpdateError};
use crate::paging::entry;

/// Local APIC の ID レジスタのオフセット。ID は**ビット 31:24** にある。
const LAPIC_REGISTER_ID: u64 = 0x20;

/// Local APIC の Version レジスタのオフセット。
///
/// ビット 7:0 が version、ビット 23:16 が Max LVT Entry である。
const LAPIC_REGISTER_VERSION: u64 = 0x30;

/// ID レジスタ内での APIC ID の位置。
const LAPIC_ID_SHIFT: u32 = 24;

/// Version レジスタ内での Max LVT Entry の位置。
///
/// **この値は個数ではない。** Intel SDM は "Max LVT Entry" を**LVT エントリの
/// 個数から 1 を引いた値**と定義している。したがって実測の 5 は「5 個」ではなく
/// 「エントリ番号の最大が 5」、すなわち 6 個を意味する。
///
/// 生の値をそのまま持ち、名前を事実に合わせてある（`max_lvt_entry`）。+1 して
/// 個数として持つ形は採らない。どこで +1 したかを追う必要が出るためである。
const LAPIC_MAX_LVT_SHIFT: u32 = 16;

/// 写像と確認の結果。**ログに出す以上のことはしない。**
struct MappedPage {
    /// 写像を新しく張ったのか、既に張られていたのか。
    already_mapped: bool,
}

/// 写像できた APIC の MMIO。**S2-a のレジスタ読みが使う。**
///
/// `acpi::ApicMmio` が「MADT が名乗った所在」であるのに対し、こちらは
/// **実際に写像を確認できた所在**である。読む側が「MADT にあったが写像に
/// 失敗したもの」を触らないよう、区別してある。
pub struct MappedApic {
    local_apic: PhysAddr,
    io_apics: [Option<IoApicLocation>; MAX_MAPPED_IO_APICS],
    io_apic_count: usize,
}

/// 写像を記録する I/O APIC の上限。`acpi` 側の上限と同じ理由で置く。
const MAX_MAPPED_IO_APICS: usize = 4;

/// APIC の MMIO を写像し、Local APIC を読めることを確かめる。
///
/// # 呼ぶ位置
///
/// [`crate::acpi::survey`] の直後。survey が返した所在をそのまま使うので、
/// 値の産地と利用点を離さない。**`survey` と違って恒等除去より前である必要は
/// 無い**（UEFI メモリマップのスライスを使わないため）が、離す理由も無い。
///
/// direct map 窓が高位で稼働していること（A-2 より後）は必要である。
pub fn map_and_probe<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut FrameAllocator<CAP>,
    mmio: &ApicMmio,
) -> Option<MappedApic> {
    // --- 1. IA32_APIC_BASE を読む。MMIO へ触る前に必ず通る関門である ---
    let Some(base) = cpu::apic_base() else {
        logger.error(format_args!(
            "apic: CPUID reports no local APIC, so IA32_APIC_BASE does not exist; \
             nothing is mapped and nothing is read"
        ));
        return None;
    };
    logger.info(format_args!(
        "apic: IA32_APIC_BASE raw={:#x} base={:#x} enabled={} x2apic={} bsp={}",
        base.raw, base.base, base.enabled, base.x2apic, base.bootstrap_processor
    ));

    let Some(lapic_phys) = mmio.local_apic() else {
        logger.error(format_args!(
            "apic: the MADT gave no local APIC address; nothing is mapped and nothing is read"
        ));
        return None;
    };

    // 破壊確認（突き合わせ）。既定ビルドでは受け取った値をそのまま返す。
    let lapic_phys = sabotage_local_apic_address(logger, lapic_phys);

    // --- 2. MMIO で触ってよい状態かを確かめる ---
    //
    // **EN だけでは足りない。** x2APIC が有効だと MMIO によるアクセスは
    // 無効化されており、触ると #GP になる。
    if !base.mmio_accessible() {
        logger.error(format_args!(
            "apic: the local APIC is not reachable over MMIO (enabled={} x2apic={}); \
             nothing is mapped and nothing is read. x2APIC uses MSRs instead, which is \
             an S2 concern",
            base.enabled, base.x2apic
        ));
        return None;
    }

    // --- 3. MSR と MADT を突き合わせる ---
    //
    // **食い違ったら写像もしない。** 「MADT のアドレスに Local APIC が無い
    // かもしれない」と判断した直後にそのアドレスを PCD で写像すると、後段が
    // 「写っているのだから正しいのだろう」と読む余地を作る。**正当化できない
    // 写像を残さない。** S1 は情報を集める段なので、食い違いという情報が
    // 得られた時点で目的は達している。
    if base.base != lapic_phys.as_u64() {
        logger.error(format_args!(
            "apic: IA32_APIC_BASE names {:#x} but the MADT names {:#x}; the two disagree, \
             so nothing is mapped and nothing is read",
            base.base,
            lapic_phys.as_u64()
        ));
        return None;
    }
    logger.info(format_args!(
        "apic: IA32_APIC_BASE and the MADT agree on {:#x}",
        lapic_phys.as_u64()
    ));

    // --- 4. 写像する。フレーム会計を前後で取る ---
    let frames_before = allocator.free_frame_count();

    let lapic_mapping = map_mmio_page(logger, allocator, "the local APIC", lapic_phys);

    let mut mapped = MappedApic {
        local_apic: lapic_phys,
        io_apics: [None; MAX_MAPPED_IO_APICS],
        io_apic_count: 0,
    };

    for io_apic in mmio.io_apics() {
        // **写像はするが読まない**（S1-c の範囲）。IO-APIC のレジスタを読むには
        // IOREGSEL へ書いてから IOWIN を読む必要があり、それは書き込みである。
        // セレクタであって割り込みの設定ではないと主張はできるが、書き込みで
        // あることは事実なので、この段では避ける。
        //
        // **したがって IO-APIC の MMIO が本当にデコードされるかは S2-a の
        // レジスタ読みまで未確認である。** ここで確かめられるのは翻訳が
        // 張られたことだけである。
        if map_mmio_page(logger, allocator, "an I/O APIC", io_apic.phys).is_some() {
            if let Some(slot) = mapped.io_apics.get_mut(mapped.io_apic_count) {
                *slot = Some(io_apic);
                mapped.io_apic_count += 1;
            }
        }
    }

    let frames_after = allocator.free_frame_count();
    logger.info(format_args!(
        "apic: frame accounting: {frames_before} free before, {frames_after} free after, \
         {} frame(s) consumed for page tables ({} I/O APIC(s) mapped, {} dropped)",
        frames_before.saturating_sub(frames_after),
        mmio.io_apics().count(),
        mmio.io_apics_dropped()
    ));

    // --- 5. Local APIC を読む。**写像できたときだけである** ---
    let Some(mapping) = lapic_mapping else {
        logger.error(format_args!(
            "apic: the local APIC MMIO page is not mapped, so its registers are NOT read"
        ));
        return None;
    };
    if mapping.already_mapped {
        logger.info(format_args!(
            "apic: the local APIC MMIO page was already mapped to the expected physical \
             address, so nothing was changed"
        ));
    }

    probe_local_apic(logger, lapic_phys, mmio.bsp_candidate_apic_id());

    Some(mapped)
}

/// MMIO の 1 ページを direct map 窓へ 4KiB・PCD で張る。
///
/// 張る前と張った後の両方で `translate` を撮る。**前が「未マップ」で後が
/// 「期待の物理」であることの対が、写像が実際に何かを変えたことの観測になる**
/// （破壊 feature を使わずに済む形である）。
fn map_mmio_page<const CAP: usize>(
    logger: &mut Logger<SerialPort>,
    allocator: &mut FrameAllocator<CAP>,
    what: &str,
    phys: PhysAddr,
) -> Option<MappedPage> {
    // ページ境界に載っていない MMIO ベースは、表として壊れている。切り捨てて
    // 続けると、読む先が 1 ページずれたまま「読めた」ことになる。
    if !phys.as_u64().is_multiple_of(FRAME_SIZE) {
        logger.error(format_args!(
            "apic: {what} is at {:#x}, which is not 4KiB aligned; not mapped",
            phys.as_u64()
        ));
        return None;
    }

    let direct_map = common::addr::direct_map();
    if !direct_map.covers(phys) {
        logger.error(format_args!(
            "apic: {what} at {:#x} lies outside the direct map window (length {:#x}); not mapped",
            phys.as_u64(),
            direct_map.length()
        ));
        return None;
    }
    let virt = direct_map.phys_to_virt(phys);

    // SAFETY: 呼び出し位置の契約（`map_and_probe` の doc）により、CR3 は自前の
    // ページテーブルを指し、登録 direct map 窓は高位で稼働している。
    let mut table = unsafe { ActivePageTable::current(direct_map) };

    // --- 張る前 ---
    let before = table.translate(virt);
    match before {
        Ok(None) => logger.info(format_args!(
            "apic: before mapping, {what} at {:#x} (virt {:#x}) has no translation, as expected",
            phys.as_u64(),
            virt.as_u64()
        )),
        // **既にマップされていることは失敗とは限らない。** 別のファームウェアが
        // この領域を `EfiMemoryMappedIO` として出せば、direct map が 2MiB の huge で
        // 既に覆っている。下で「期待の物理を指しているか」を見て判断する。
        Ok(Some(translation)) => logger.warn(format_args!(
            "apic: before mapping, {what} at {:#x} (virt {:#x}) already translates to {:#x} \
             ({:?}); checking whether it already points where we want",
            phys.as_u64(),
            virt.as_u64(),
            translation.phys.as_u64(),
            translation.page_size
        )),
        Err(e) => logger.error(format_args!(
            "apic: before mapping, {what} at {:#x} could not be translated: {e:?}",
            phys.as_u64()
        )),
    }

    let mut already_mapped = false;
    if skip_mapping(logger, what) {
        // 破壊確認（写像の省略）。下の「張った後」の確認が捕まえる。
    } else {
        let target = sabotage_map_target(logger, what, phys);
        // SAFETY: `virt` は direct map 窓の中の 4KiB 境界に載ったアドレスで、
        // `target` は有効な物理フレーム。`user=false` なのでカーネルの中間
        // テーブルへ U ビットを混ぜない。`cacheable=false` により PCD が立つ
        // （MMIO では読み書きの順序と副作用が意味を持つため。ADR-0015）。
        // 既に葉が present なら `AlreadyMapped` が返り、何も書き換えない。
        let result = unsafe { table.map_4kib(virt, target, false, false, allocator) };
        match result {
            Ok(()) => {}
            // **一律に失敗としない。** 既に正しく写っているなら、その環境で
            // 不必要に失敗することになる。期待の物理を指しているかは下で見る。
            Err(MapUpdateError::AlreadyMapped) => already_mapped = true,
            Err(e) => {
                logger.error(format_args!(
                    "apic: mapping {what} at {:#x} failed: {e:?}",
                    phys.as_u64()
                ));
                return None;
            }
        }
    }

    // --- 張った後 ---
    //
    // **期待の物理を指していることまで見る。** 「翻訳がある」だけでは、
    // 別の物理を指す翻訳でも通ってしまう。
    match table.translate(virt) {
        Ok(Some(translation)) if translation.phys == phys => {
            logger.info(format_args!(
                "apic: mapped {what} at {:#x} to virt {:#x} as {:?}, PCD={}, already_mapped={}",
                phys.as_u64(),
                virt.as_u64(),
                translation.page_size,
                translation.entry & entry::PTE_PCD != 0,
                already_mapped
            ));
            Some(MappedPage { already_mapped })
        }
        Ok(Some(translation)) => {
            logger.error(format_args!(
                "apic: after mapping, {what} at {:#x} (virt {:#x}) translates to {:#x}, not the \
                 expected physical address; not usable",
                phys.as_u64(),
                virt.as_u64(),
                translation.phys.as_u64()
            ));
            None
        }
        Ok(None) => {
            logger.error(format_args!(
                "apic: after mapping, {what} at {:#x} (virt {:#x}) still has no translation; \
                 not usable",
                phys.as_u64(),
                virt.as_u64()
            ));
            None
        }
        Err(e) => {
            logger.error(format_args!(
                "apic: after mapping, {what} at {:#x} could not be translated: {e:?}",
                phys.as_u64()
            ));
            None
        }
    }
}

/// Local APIC の ID と Version を読む。**読むだけで、何も書かない。**
fn probe_local_apic(
    logger: &mut Logger<SerialPort>,
    lapic_phys: PhysAddr,
    bsp_candidate_apic_id: Option<u8>,
) {
    let direct_map = common::addr::direct_map();
    let base_virt = direct_map.phys_to_virt(lapic_phys);

    // SAFETY: 直前に写像を確認したページの中を読む。APIC のレジスタは 32 ビット
    // 幅で、境界に載った 32 ビットアクセスでなければならない（ID は +0x20、
    // Version は +0x30 で、いずれも 16 バイト境界に載っている）。`read_volatile`
    // なのでコンパイラが読みをまとめたり消したりしない。MMIO なので、まとめられ
    // ては困る。**書き込みは一切しない。**
    let (id_raw, version_raw) = unsafe {
        let id_ptr = (base_virt.as_u64() + LAPIC_REGISTER_ID) as *const u32;
        let version_ptr = (base_virt.as_u64() + LAPIC_REGISTER_VERSION) as *const u32;
        (id_ptr.read_volatile(), version_ptr.read_volatile())
    };

    // **ID はビット 31:24 にある。** 生の 32 ビット値をそのまま比較しないこと。
    let apic_id = id_raw >> LAPIC_ID_SHIFT;
    let version = version_raw & 0xFF;
    // **個数ではなく添字の最大値である**（[`LAPIC_MAX_LVT_SHIFT`] の doc）。
    // 名前を `max_lvt_entry` にしてあるのは、`max_lvt_entries` と書くと個数を
    // 1 つ少なく主張することになるためである。
    let max_lvt_entry = (version_raw >> LAPIC_MAX_LVT_SHIFT) & 0xFF;

    logger.info(format_args!(
        "apic: LAPIC probe: id_raw={id_raw:#010x} id={apic_id} \
         version_raw={version_raw:#010x} version={version:#x} max_lvt_entry={max_lvt_entry} \
         (SDM defines this as the LVT entry count minus one)"
    ));

    // **Version レジスタが、読みが Local APIC へ届いていることの主たる根拠である。**
    // 非ゼロで構造のある値なので、ゼロ埋めされた RAM ページや未マップの読みと
    // 区別がつく。統合 APIC の version は概ね 0x10 から 0x15 の範囲に入る。
    // 0x00 と 0xFF は、それぞれ「何も無いところを読んだ」と「デコードされて
    // いない MMIO を読んだ」の典型的な見え方である。
    if version == 0x00 || version == 0xFF {
        logger.error(format_args!(
            "apic: the local APIC version register reads {version:#x}, which is what an \
             unbacked or zero-filled page looks like; the mapping is present but the read \
             does not appear to reach a local APIC"
        ));
        return;
    }

    // ID の突き合わせ。**この検査は今は弱い。**
    //
    // BSP の APIC ID は 0 であり、MADT が名乗る値も 0 である。したがって
    // 両辺が 0 になり、この比較は「シフトを忘れた」「ゼロ埋めのページを読んだ」
    // 「読みが APIC へ届いていない」のいずれでも通ってしまう。**「ID が一致した
    // から届いている」とは読まないこと。** 届いていることの根拠は上の Version で
    // ある。この比較が意味を持つのは、ID が 0 でないコアが現れる S3 以降である。
    match bsp_candidate_apic_id {
        Some(expected) if u32::from(expected) == apic_id => logger.info(format_args!(
            "apic: the local APIC ID {apic_id} matches the first usable MADT entry \
             (both are {apic_id}; on the BSP this comparison is degenerate, see the code)"
        )),
        Some(expected) => logger.warn(format_args!(
            "apic: the local APIC ID is {apic_id} but the first usable MADT entry names \
             {expected}; the MADT entry order does not have to put the BSP first, so this \
             is recorded rather than treated as an error"
        )),
        None => logger.warn(format_args!(
            "apic: the MADT listed no usable local APIC to compare the ID {apic_id} against"
        )),
    }
}

/// 破壊確認: MADT が名乗る Local APIC のアドレスをずらす。
///
/// **MSR との突き合わせ（`map_and_probe` の 3）が捕まえる経路を見る。**
/// ずらす量は 1 ページで、`0x1000` を足す。窓の中に留まるので「窓の外」の
/// 経路とは混ざらない。
fn sabotage_local_apic_address(logger: &mut Logger<SerialPort>, phys: PhysAddr) -> PhysAddr {
    let _ = &logger;

    #[cfg(feature = "apic-test-base-mismatch")]
    {
        match PhysAddr::new(phys.as_u64() + FRAME_SIZE) {
            Some(moved) => {
                logger.warn(format_args!(
                    "apic: [apic-test-base-mismatch] moving the MADT local APIC address from \
                     {:#x} to {:#x} so that it disagrees with IA32_APIC_BASE",
                    phys.as_u64(),
                    moved.as_u64()
                ));
                return moved;
            }
            None => logger.error(format_args!(
                "apic: [apic-test-base-mismatch] the moved address is not representable; \
                 the sabotage did nothing and this run proves nothing"
            )),
        }
    }

    phys
}

/// 破壊確認: 写像そのものを省く。
///
/// **「張った後」の `translate` が捕まえる経路を見る。** 読みが写像に依存して
/// いることの証明になる。
fn skip_mapping(logger: &mut Logger<SerialPort>, what: &str) -> bool {
    let _ = (&logger, what);

    #[cfg(feature = "apic-test-skip-map")]
    {
        logger.warn(format_args!(
            "apic: [apic-test-skip-map] skipping the mapping of {what} so that the \
             after-mapping check has something to catch"
        ));
        true
    }
    #[cfg(not(feature = "apic-test-skip-map"))]
    {
        false
    }
}

/// 破壊確認: 写像先の物理だけを差し替える。
///
/// **「期待の物理を指しているか」の確認が捕まえる経路を見る。** 翻訳は張られる
/// ので、「翻訳がある」だけを見る検査では通ってしまう形である。
///
/// 差し替え先はリテラルで持たず、**PML4 の物理**を使う。稼働中のページテーブル
/// そのものなので、必ず存在し、必ず写像済みで、APIC ではないことが確実である。
///
/// # この手法は破壊専用である。通常経路へ持ち込まないこと
///
/// ここで作るのは、**同一物理ページに対するメモリ型の異なる別名**である。PML4 の
/// フレームは本流の direct map 窓から WB で写っているので、そこへ PCD の写像を
/// 重ねることになる。x86 では同一物理ページに異なるメモリ型の別名を張ることは
/// 未定義の領域である。
///
/// サボタージュビルド限定で、しかも直後の照合が検出して読みへ進まないため、
/// 破壊の手段としてはこのまま採ってよい。**便利な手筋として通常の写像経路へ
/// 再利用しないこと。** 静かな不具合の温床になる。
fn sabotage_map_target(logger: &mut Logger<SerialPort>, what: &str, phys: PhysAddr) -> PhysAddr {
    let _ = (&logger, what, phys);

    #[cfg(feature = "apic-test-wrong-target")]
    {
        let pml4 = crate::paging::switch::read_cr3();
        logger.warn(format_args!(
            "apic: [apic-test-wrong-target] pointing the mapping of {what} at {:#x} \
             (the live PML4) instead of {:#x}",
            pml4.as_u64(),
            phys.as_u64()
        ));
        pml4
    }
    #[cfg(not(feature = "apic-test-wrong-target"))]
    {
        phys
    }
}

// ===========================================================================
// S2-a: レジスタを読むだけの棚卸し
//
// **割り込みの経路は一切変えない。** ここでやるのは、S2-b 以降の設計に要る
// 現在値を実測して記録することだけである。PIC / PIT はそのまま動き続ける。
// ===========================================================================

/// Local APIC の Spurious Interrupt Vector Register。
///
/// **bit 8 がソフトウェア有効化**で、bits 7:0 がスプリアス割り込みのベクタである。
const LAPIC_REGISTER_SVR: u64 = 0xF0;

/// SVR の bit 8（APIC Software Enable）。
const LAPIC_SVR_SOFTWARE_ENABLE: u32 = 1 << 8;

/// Task Priority Register。
const LAPIC_REGISTER_TPR: u64 = 0x80;

/// In-Service Register / Interrupt Request Register の先頭。
///
/// **どちらも 32 ビット × 8 本が 16 バイト間隔で並ぶ。** 連続していないので、
/// 添字に 0x10 を掛けて進める。
const LAPIC_REGISTER_ISR_BASE: u64 = 0x100;
const LAPIC_REGISTER_IRR_BASE: u64 = 0x200;
const LAPIC_STATUS_REGISTER_COUNT: u64 = 8;
const LAPIC_STATUS_REGISTER_STRIDE: u64 = 0x10;

/// LVT Timer と LINT0 のオフセット。
///
/// 一覧（[`LAPIC_LVT_ENTRIES`]）にも入っているが、名前で参照する箇所があるので
/// 定数にしてある。**マジックナンバーを散らさない。**
const LAPIC_REGISTER_LVT_TIMER: u64 = 0x320;
const LAPIC_REGISTER_LVT_LINT0: u64 = 0x350;

/// LVT の並び。**存在する本数は Max LVT Entry + 1 で決まる**ので、
/// 添字がその範囲に収まるものだけを読む。
///
/// 実測（Max LVT Entry = 5、すなわち 6 本）では Timer から Error までが存在し、
/// CMCI（`0x2F0`）は存在しない。**存在しないレジスタを読まない**のは、
/// 未定義の値を観測値として記録しないためである。
const LAPIC_LVT_ENTRIES: [(&str, u64); 6] = [
    ("Timer", LAPIC_REGISTER_LVT_TIMER),
    ("Thermal", 0x330),
    ("PMC", 0x340),
    ("LINT0", LAPIC_REGISTER_LVT_LINT0),
    ("LINT1", 0x360),
    ("Error", 0x370),
];

/// LVT / redirection entry の共通ビット。
const ENTRY_VECTOR_MASK: u32 = 0xFF;
const ENTRY_DELIVERY_MODE_SHIFT: u32 = 8;
const ENTRY_DELIVERY_MODE_MASK: u32 = 0b111;
const ENTRY_DELIVERY_STATUS_BIT: u32 = 1 << 12;
const ENTRY_ACTIVE_LOW_BIT: u32 = 1 << 13;
const ENTRY_REMOTE_IRR_BIT: u32 = 1 << 14;
const ENTRY_LEVEL_TRIGGERED_BIT: u32 = 1 << 15;
const ENTRY_MASKED_BIT: u32 = 1 << 16;

/// LVT Timer だけが持つタイマモード（bits 18:17）。
///
/// **他の LVT には無いビットである**ので、Timer のときだけ復号する。
const LVT_TIMER_MODE_SHIFT: u32 = 17;
const LVT_TIMER_MODE_MASK: u32 = 0b11;

/// タイマモードの名前。
///
/// **生値だけを残さない。** 報告のために人が復号するなら、それはログが復号
/// すべき値である。生値も併記して、復号の側が誤っていても原資料が残る形にする
/// （`max_lvt_entry` / `max_redirection_entry` と同じ扱い）。
const fn timer_mode_name(mode: u32) -> &'static str {
    match mode {
        0b00 => "one-shot",
        0b01 => "periodic",
        0b10 => "TSC-deadline",
        _ => "reserved",
    }
}

/// 配送モードの名前。**ExtINT かどうかが S2-d の刻みを左右する**ので、
/// 数値だけでなく名前で出す。
const fn delivery_mode_name(mode: u32) -> &'static str {
    match mode {
        0b000 => "Fixed",
        0b001 => "LowestPriority",
        0b010 => "SMI",
        0b100 => "NMI",
        0b101 => "INIT",
        0b111 => "ExtINT",
        _ => "reserved",
    }
}

/// I/O APIC の IOREGSEL（書き込む添字）と IOWIN（読み書きする窓）。
const IOAPIC_REGISTER_SELECT: u64 = 0x00;
const IOAPIC_REGISTER_WINDOW: u64 = 0x10;

/// I/O APIC の内部レジスタ番号。
const IOAPIC_INDEX_ID: u8 = 0x00;
const IOAPIC_INDEX_VERSION: u8 = 0x01;
const IOAPIC_INDEX_REDIRECTION_BASE: u8 = 0x10;

/// ID レジスタ内での I/O APIC ID の位置（bits 27:24）。
const IOAPIC_ID_SHIFT: u32 = 24;
const IOAPIC_ID_MASK: u32 = 0xF;

/// Version レジスタ内での Max Redirection Entry の位置。
///
/// **Max LVT Entry と同じ罠がある。** SDM はこれを**エントリの個数から 1 を
/// 引いた値**と定義している。名前を `max_redirection_entry` にしてあるのは、
/// `..._count` と書くと個数を 1 つ少なく主張することになるためである。
const IOAPIC_MAX_REDIRECTION_SHIFT: u32 = 16;

/// APIC のレジスタを読んで現在値を記録する（S2-a）。
///
/// # 何もしない
///
/// **割り込みの構成は一切変えない。** Local APIC へは書き込まない。
/// I/O APIC へは IOREGSEL（添字レジスタ）にだけ書く。これは読みたい
/// レジスタを選ぶセレクタで、割り込みの設定ではないが、**書き込みである
/// ことは事実である。S2 で最初の書き込みがここである。**
pub fn survey_registers(logger: &mut Logger<SerialPort>, mapped: &MappedApic) {
    let direct_map = common::addr::direct_map();
    let lapic_virt = direct_map.phys_to_virt(mapped.local_apic);

    // --- Local APIC ---
    //
    // SAFETY: `map_and_probe` が写像を確認したページの中だけを読む。APIC の
    // レジスタは 16 バイト境界に載った 32 ビット幅で、`read_volatile` なので
    // コンパイラが読みをまとめたり消したりしない。**書き込みは行わない。**
    let (svr, tpr, version_raw) = unsafe {
        (
            read_lapic(lapic_virt.as_u64(), LAPIC_REGISTER_SVR),
            read_lapic(lapic_virt.as_u64(), LAPIC_REGISTER_TPR),
            read_lapic(lapic_virt.as_u64(), LAPIC_REGISTER_VERSION),
        )
    };
    let max_lvt_entry = (version_raw >> LAPIC_MAX_LVT_SHIFT) & 0xFF;
    let lvt_present = (max_lvt_entry as usize).saturating_add(1);

    logger.info(format_args!(
        "apic: LAPIC SVR={svr:#010x} software_enabled={} spurious_vector={:#04x} TPR={tpr:#010x}",
        svr & LAPIC_SVR_SOFTWARE_ENABLE != 0,
        svr & ENTRY_VECTOR_MASK
    ));

    // **LINT0 が ExtINT かどうかが S2-d の刻みを決める。** LAPIC を
    // ソフトウェア有効化した後も 8259 経由の割り込みが届くのは、LINT0 が
    // ExtINT に設定されている場合（virtual wire mode）だけである。
    logger.info(format_args!(
        "apic: LAPIC has {lvt_present} LVT entr(y/ies) (Max LVT Entry = {max_lvt_entry}); \
         reading only those"
    ));
    for (index, (name, offset)) in LAPIC_LVT_ENTRIES.iter().enumerate() {
        if index >= lvt_present {
            logger.info(format_args!(
                "apic:   LVT {name}: not present on this LAPIC; not read"
            ));
            continue;
        }
        // SAFETY: 上と同じ。存在する本数の範囲内だけを読む。
        let value = unsafe { read_lapic(lapic_virt.as_u64(), *offset) };
        let mode = (value >> ENTRY_DELIVERY_MODE_SHIFT) & ENTRY_DELIVERY_MODE_MASK;
        logger.info(format_args!(
            "apic:   LVT {name}: raw={value:#010x} vector={:#04x} delivery={} ({}) \
             masked={} level_triggered={} active_low={} pending={}",
            value & ENTRY_VECTOR_MASK,
            mode,
            delivery_mode_name(mode),
            value & ENTRY_MASKED_BIT != 0,
            value & ENTRY_LEVEL_TRIGGERED_BIT != 0,
            value & ENTRY_ACTIVE_LOW_BIT != 0,
            value & ENTRY_DELIVERY_STATUS_BIT != 0
        ));
        // タイマモードは Timer にしか無いので、そこでだけ復号する。
        if *name == "Timer" {
            let timer_mode = (value >> LVT_TIMER_MODE_SHIFT) & LVT_TIMER_MODE_MASK;
            logger.info(format_args!(
                "apic:     LVT Timer mode={timer_mode} ({}) [bits 18:17 of the raw value above]",
                timer_mode_name(timer_mode)
            ));
        }
    }

    // ISR / IRR。**sti-check の項目 7（ハンドラが EOI を発行）を UNVERIFIABLE
    // から格上げできるかの材料である。** ここで読めることは「読み戻せる」を
    // 示すだけで、EOI が効いていることの証明ではない。それには**ハンドラの
    // 中で**読む必要があり、S2-a はハンドラに触らない。
    let mut isr_any = 0u32;
    let mut irr_any = 0u32;
    for index in 0..LAPIC_STATUS_REGISTER_COUNT {
        let offset = index * LAPIC_STATUS_REGISTER_STRIDE;
        // SAFETY: 上と同じ。ISR / IRR は 8 本が 16 バイト間隔で並ぶ。
        let (isr, irr) = unsafe {
            (
                read_lapic(lapic_virt.as_u64(), LAPIC_REGISTER_ISR_BASE + offset),
                read_lapic(lapic_virt.as_u64(), LAPIC_REGISTER_IRR_BASE + offset),
            )
        };
        isr_any |= isr;
        irr_any |= irr;
    }
    logger.info(format_args!(
        "apic: LAPIC ISR/IRR are readable (OR of all 8 dwords: ISR={isr_any:#010x} \
         IRR={irr_any:#010x}); read outside any handler, so this shows readability only, \
         not that EOI works"
    ));

    // --- I/O APIC ---
    for slot in mapped.io_apics.iter().take(mapped.io_apic_count) {
        let Some(io_apic) = slot else { continue };
        survey_io_apic(
            logger,
            direct_map.phys_to_virt(io_apic.phys).as_u64(),
            io_apic,
        );
    }
}

/// I/O APIC 1 台のレジスタを読む。
fn survey_io_apic(logger: &mut Logger<SerialPort>, base_virt: u64, io_apic: &IoApicLocation) {
    // SAFETY: `map_and_probe` が写像を確認したページの中だけを触る。
    // IOREGSEL への書き込みと IOWIN からの読み出しは、この 1 ページに閉じる。
    let (id_raw, version_raw) = unsafe {
        (
            read_io_apic(base_virt, IOAPIC_INDEX_ID),
            read_io_apic(base_virt, IOAPIC_INDEX_VERSION),
        )
    };

    // **個数ではなく添字の最大値である**（`IOAPIC_MAX_REDIRECTION_SHIFT` の doc）。
    let max_redirection_entry = (version_raw >> IOAPIC_MAX_REDIRECTION_SHIFT) & 0xFF;
    let entry_count = max_redirection_entry.saturating_add(1);

    logger.info(format_args!(
        "apic: I/O APIC id={} at {:#x}: ID reg={id_raw:#010x} VER reg={version_raw:#010x} \
         version={:#x} max_redirection_entry={max_redirection_entry} \
         (SDM defines this as the entry count minus one, so there are {entry_count}) \
         gsi_base={}",
        io_apic.id,
        io_apic.phys.as_u64(),
        version_raw & 0xFF,
        io_apic.global_system_interrupt_base
    ));

    // **判定を 1 行にまとめる。** S1-c では「IO-APIC の MMIO が実際にデコードされるか」
    // を未確認のまま残していた（読むには IOREGSEL への書き込みが要り、S1-c は書き込みを
    // 行わない段だったため）。ここがその積み残しを閉じる行である。
    //
    // **根拠を 2 つ独立に取る。**
    //   version レジスタが未デコードの見え方（全 0 / 全 1）でないこと
    //   ID レジスタの名乗る ID が、MADT が名乗った ID と一致すること
    // 後者は出所が別（MMIO とファームウェアの表）なので、偶然の一致になりにくい。
    // 片方だけでは弱い。全 0 のページでも ID は 0 に見えるので、MADT の ID が 0 の
    // 環境では ID の一致だけでは未デコードと区別できない。
    let version_plausible = version_raw != 0 && version_raw != u32::MAX;
    let reported_id = ((id_raw >> IOAPIC_ID_SHIFT) & IOAPIC_ID_MASK) as u8;
    let id_matches = reported_id == io_apic.id;

    if version_plausible && id_matches {
        logger.info(format_args!(
            "apic: I/O APIC MMIO decodes: the version register is not an undecoded read and \
             the ID register reports {reported_id}, matching the MADT; \
             {entry_count} redirection entr(y/ies)"
        ));
    } else {
        logger.error(format_args!(
            "apic: the I/O APIC MMIO does not look decoded (version={version_raw:#010x} \
             plausible={version_plausible}, ID register reports {reported_id} while the MADT \
             says {}, matching={id_matches}); the redirection entries below are not trustworthy",
            io_apic.id
        ));
    }

    for entry in 0..entry_count {
        let index = IOAPIC_INDEX_REDIRECTION_BASE.wrapping_add((entry as u8).wrapping_mul(2));
        // SAFETY: 上と同じ。エントリは低位 32 ビットと高位 32 ビットの 2 本組で、
        // 添字は version レジスタが名乗った本数の範囲に収めてある。
        let (low, high) = unsafe {
            (
                read_io_apic(base_virt, index),
                read_io_apic(base_virt, index.wrapping_add(1)),
            )
        };
        let mode = (low >> ENTRY_DELIVERY_MODE_SHIFT) & ENTRY_DELIVERY_MODE_MASK;
        logger.info(format_args!(
            "apic:   redirection entry {entry:>2} (gsi {}): low={low:#010x} high={high:#010x} \
             vector={:#04x} delivery={} masked={} level_triggered={} active_low={} \
             remote_irr={} destination={:#04x}",
            io_apic.global_system_interrupt_base + entry,
            low & ENTRY_VECTOR_MASK,
            delivery_mode_name(mode),
            low & ENTRY_MASKED_BIT != 0,
            low & ENTRY_LEVEL_TRIGGERED_BIT != 0,
            low & ENTRY_ACTIVE_LOW_BIT != 0,
            low & ENTRY_REMOTE_IRR_BIT != 0,
            high >> 24
        ));
    }
}

/// Local APIC のレジスタを 1 本読む。
///
/// # Safety
///
/// `base_virt` が写像済みの Local APIC ページの先頭で、`offset` がその
/// ページ内の 16 バイト境界に載ったレジスタであること。
unsafe fn read_lapic(base_virt: u64, offset: u64) -> u32 {
    // SAFETY: 呼び出し元契約。MMIO なので `read_volatile` で読む。
    unsafe { ((base_virt + offset) as *const u32).read_volatile() }
}

/// I/O APIC の内部レジスタを 1 本読む。
///
/// **IOREGSEL への書き込みを伴う。** これが S2 で最初の書き込みである。
/// 書くのは「次に窓から読む対象」を選ぶ添字だけで、割り込みの設定ではない。
///
/// # Safety
///
/// `base_virt` が写像済みの I/O APIC ページの先頭であること。
/// **他の実行文脈が同時に同じ I/O APIC を触っていないこと**（IOREGSEL は
/// 台ごとに 1 本しかない共有の状態なので、割り込まれると読む対象が変わる）。
/// 現在は単一コアで、この経路は割り込み禁止の起動シーケンス中にだけ通る。
unsafe fn read_io_apic(base_virt: u64, index: u8) -> u32 {
    // SAFETY: 呼び出し元契約。添字を選んでから窓を読む、の順序が必須である。
    unsafe {
        ((base_virt + IOAPIC_REGISTER_SELECT) as *mut u32).write_volatile(index as u32);
        ((base_virt + IOAPIC_REGISTER_WINDOW) as *const u32).read_volatile()
    }
}

// ===========================================================================
// S2-b: スプリアスベクタを、CPU の予約例外ベクタの外へ移す
// ===========================================================================

/// Local APIC のスプリアス割り込みに使うベクタ。
///
/// # なぜ `0xFF` なのか
///
/// **ファームウェアが残した値は `0x0f` で、CPU の予約例外ベクタの範囲
/// （0 から 31）の中にある**（S2-a の実測）。今は Local APIC 由来の割り込みを
/// 1 つも構成していないので潜在的だが、LAPIC が配送を担い始める S2-d-1 より前に
/// 移しておかないと、スプリアスが起きたときに予約例外として解釈される。
///
/// **下位 4 ビットが `F` の値を採る。** 古い CPU では SVR の下位 4 ビットが 1 に
/// 固定されており、書いた値がそのまま読み戻せなかった。現行の CPU では書ける
/// が、慣例に合わせておけば読み戻しが書いた値と食い違わない。
///
/// `0xFF` は PIC の担当範囲（`0x20`-`0x2F`、`alt-offset-test` では `0x30`-`0x3F`）の
/// 外で、syscall（`0x80`）とも重ならない。
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// SVR のうちベクタ欄だけを [`SPURIOUS_VECTOR`] へ書き換える（S2-b）。
///
/// # 振る舞いは変わらない
///
/// スプリアス割り込みは現在発生しない（Local APIC 由来の割り込みを 1 つも
/// 構成していない）。したがってベクタ欄を動かしても配送は変わらない。
///
/// # 安全条件
///
/// **read-modify-write で bit 8 を保つ。** bit 8 を落とすと Local APIC が
/// 無効になり、LINT0 経由で届いている 8259 の IRQ0 が即座に止まる
/// （`verification-coverage.md` の「APICのレジスタの現在値（S2-a）」）。
/// **危険なのは bit 8 であって、ベクタ欄ではない。**
pub fn set_spurious_vector(logger: &mut Logger<SerialPort>, mapped: &MappedApic) {
    let direct_map = common::addr::direct_map();
    let lapic_virt = direct_map.phys_to_virt(mapped.local_apic).as_u64();

    // SAFETY: `map_and_probe` が写像を確認したページの中を読む。
    let before = unsafe { read_lapic(lapic_virt, LAPIC_REGISTER_SVR) };

    // **ベクタ欄以外を 1 ビットも変えない。** bit 8（有効化）はもちろん、
    // bit 9（focus processor checking）や bit 12（EOI broadcast suppression）も
    // ファームウェアが立てているかもしれないので、読んだ値を土台にする。
    let after_intended = (before & !ENTRY_VECTOR_MASK) | u32::from(SPURIOUS_VECTOR);

    // SAFETY: 上と同じページの 16 バイト境界に載ったレジスタへ、読んだ値の
    // ベクタ欄だけを差し替えて書き戻す。割り込みは禁止されている起動シーケンス
    // 中で、他の実行文脈はこの LAPIC を触っていない。
    unsafe {
        ((lapic_virt + LAPIC_REGISTER_SVR) as *mut u32).write_volatile(after_intended);
    }

    // SAFETY: 上と同じ。書いた結果を読み戻す。
    let after = unsafe { read_lapic(lapic_virt, LAPIC_REGISTER_SVR) };

    let enabled_kept = after & LAPIC_SVR_SOFTWARE_ENABLE != 0;
    let vector_now = (after & ENTRY_VECTOR_MASK) as u8;
    logger.info(format_args!(
        "apic: SVR spurious vector {:#04x} -> {vector_now:#04x} (raw {before:#010x} -> \
         {after:#010x}); software_enabled kept = {enabled_kept}",
        before & ENTRY_VECTOR_MASK
    ));

    // **bit 8 を落としていないことを読み戻しで確かめる。** ここが落ちていれば
    // タイマが止まるので、黙って進まない。
    if !enabled_kept {
        logger.error(format_args!(
            "apic: the SVR software-enable bit is no longer set after writing the spurious \
             vector; the local APIC is disabled and 8259 delivery through LINT0 has stopped"
        ));
    }
    if vector_now != SPURIOUS_VECTOR {
        logger.error(format_args!(
            "apic: the SVR vector field reads {vector_now:#04x} after writing \
             {SPURIOUS_VECTOR:#04x}; the write did not take"
        ));
    }

    // IDT のゲートを読み戻す。
    //
    // **ゲートを新しく足す必要は無かった。** IDT は 256 本すべてが present で
    // （起動ログの `idt: 256 entries, all present=true`）、`0xFF` にも既に
    // スタブが入っている。**したがってここで確かめるのは「足したこと」ではなく
    // 「既にあること」である。**
    match crate::idt::entry(SPURIOUS_VECTOR as usize) {
        Some(entry) if entry.is_present() => logger.info(format_args!(
            "apic: the IDT gate for the spurious vector {SPURIOUS_VECTOR:#04x} is present \
             (gate type {:#x}, DPL {}); it was already there because the IDT fills all 256 \
             entries",
            entry.gate_type(),
            entry.descriptor_privilege_level()
        )),
        _ => logger.error(format_args!(
            "apic: the IDT has no present gate for the spurious vector {SPURIOUS_VECTOR:#04x}"
        )),
    }

    // **S2-d-1 でこのベクタを IRQ スタイルのスタブへ移した。**
    //
    // S2-b の時点では例外スタイルのスタブのままで、起きればダンプして停止する
    // 状態だった（`idt::init` は 256 本を例外スタイルで埋め、IRQ スタイルで
    // 上書きするのは `0x20`-`0x40` と yield / syscall だけで、`0xFF` はどれにも
    // 当たらなかった）。**専用スタブ（`zaytos_spurious_stub`）を置いて
    // `zaytos_irq_common` へ合流させ、戻れる経路にした。**
    //
    // EOI を送らない判定も明示にした。以前送られなかったのは「PIC の担当範囲の
    // 外だから」という偶然で、S2-d で Local APIC が配送を担うと壊れる一致だった。
    // 現在は `idt::irq_entry` がこのベクタを名指しで判定し、回数を
    // PIC のスプリアスとは別に数えて、EOI を送らずに戻る。
    logger.info(format_args!(
        "apic: vector {SPURIOUS_VECTOR:#04x} is on the dedicated IRQ-style stub; the dispatch \
         recognises it by name, counts it separately from the 8259 spurious IRQs, and returns \
         without sending EOI. spurious interrupts cannot happen yet (nothing is delivered by \
         the local APIC), so this path has not been exercised at runtime"
    ));
}

// ===========================================================================
// S2-c: Local APIC タイマの較正
//
// **タイマとしては使わない。** LVT Timer はマスクされたままで、LAPIC タイマ由来の
// 割り込みは 1 本も発生しない。**LINT0 と SVR の bit 8 には触らない。**
// ===========================================================================

/// タイマの初期カウント。書くと数え下がりが始まる。
const LAPIC_REGISTER_TIMER_INITIAL_COUNT: u64 = 0x380;

/// 現在のカウント。読むだけ。
const LAPIC_REGISTER_TIMER_CURRENT_COUNT: u64 = 0x390;

/// 分周設定。
const LAPIC_REGISTER_TIMER_DIVIDE: u64 = 0x3E0;

/// 16 分周（Divide Configuration Register のビット 3・1・0 で `0b0011`）。
///
/// **32 ビットのカウンタが窓の間に一周しないことが条件である。** 仮に APIC の
/// 入力が 1GHz でも 16 分周で 62.5MHz、`u32::MAX` からの数え下がりは約 68 秒
/// もつ。較正窓（下記）は 100ms なので、桁が 2 つ以上余っている。
const LAPIC_TIMER_DIVIDE_BY_16: u32 = 0b0011;

/// 較正窓に使う PIT ティック数。
///
/// # なぜ 10 なのか（先に決めていない。誤差の見積りから決めた）
///
/// 窓の両端を**ティックのエッジで揃える**ので、窓の実時間は
/// `N × 10ms ± (エッジ検出の遅延 + 割り込み遅延のばらつき + 読み取り粒度)` になる。
/// 片端の見積りは、`timer_ticks()` のポーリング粒度を 5µs、割り込み遅延の
/// ばらつきを 10µs（TCG なので大きめに見る）、Current Count の読み取り粒度を
/// 1µs として **16µs**、両端で **32µs** である。
///
/// | N | 窓 | 相対誤差の上限 |
/// |---|---|---|
/// | 1 | 10ms | 0.32% |
/// | 5 | 50ms | 0.064% |
/// | **10** | **100ms** | **0.032%** |
/// | 20 | 200ms | 0.016% |
///
/// **較正自身の誤差が、許容幅の下限を決める。** 0.032% は起動ごとのばらつきより
/// 十分小さいと見込めるので、これ以上窓を伸ばして起動を遅くする理由が無い。
const CALIBRATION_WINDOW_TICKS: u64 = 10;

/// 1 回の起動で取る標本数。
///
/// 単発の外れ値と、系統的なずれを分けるために複数取る。
///
/// # 標本数と中央値は、許容幅が成立するための前提である
///
/// **簡略化してよい実装詳細ではない。** 実測で、第 1 標本は 7 起動すべてで残りの
/// 平均を下回った（不足は 677 ppm から 15,310 ppm）。最も外れた起動では中央値との
/// 差が約 1.59% で、**S2-d-2 で使う許容幅 ±0.5% の 3 倍を超える。**
///
/// つまり「標本を 5 つ取って中央値を採る」ことをやめて 1 標本にすると、較正値が
/// 許容幅を外れて起動が落ちるか、**落ちない幅まで許容幅を緩めることになる。**
/// 後者は検査があるように見えて何も検査していない形である。**減らさないこと。**
const CALIBRATION_SAMPLES: usize = 5;

/// エッジ待ちの上限（TSC サイクル）。
///
/// **上限のない待機ループを書かない。** ティックが来なければ較正を諦めて報告する。
/// 停止はしない（S2 は情報を集める段の延長である）。10ms のティックに対して
/// 十分に長く、かつ人が待てる範囲にしてある。
const CALIBRATION_EDGE_TIMEOUT_CYCLES: u64 = 10_000_000_000;

/// 較正の結果。
pub struct TimerCalibration {
    /// 標本ごとの周波数（Hz）。
    samples: [u64; CALIBRATION_SAMPLES],
    /// 中央の標本（ソート後）。**平均ではない。** 外れ値に引きずられない。
    median_hz: u64,
    /// 最小と最大の差。
    spread_hz: u64,
}

impl TimerCalibration {
    /// 較正で得た周波数。**S2-d-2 で初期カウントを計算するのはこの値からである。**
    /// **リテラルで焼かない。**
    pub const fn median_hz(&self) -> u64 {
        self.median_hz
    }

    /// 標本のばらつき。**許容幅を決める入力である。**
    pub const fn spread_hz(&self) -> u64 {
        self.spread_hz
    }

    /// 標本そのもの。**中央値とばらつきだけでは分布の形が分からない**ので、
    /// 呼び出し側が必要なら生の並びを見られるようにしておく。
    pub fn samples(&self) -> &[u64] {
        &self.samples
    }
}

/// `timer_ticks()` が 1 つ進むまで待ち、進んだ直後の値を返す。
///
/// **エッジで揃えるための関数である。** 任意の時点でサンプルすると、10ms 刻みの
/// カウンタに対して ±1 ティック = ±10ms の誤差が乗る。N=10 の窓なら ±10% で、
/// 較正としては使えない。**変化した瞬間を捉えれば、誤差は µs 級へ落ちる。**
///
/// 読みは [`crate::idt::timer_ticks`] を通す。**`AtomicU64` のロードなので、
/// コンパイラがループの外へ持ち上げることはない。** 素の読みだと持ち上げられて
/// 無限ループになりうる（`verification-coverage.md` の「待ちループでの読み」）。
///
/// 戻り値は `(進んだ後の値, 何ティック進んだか)`。**進み幅を返すのは、
/// 取りこぼしを較正自身が検出するためである**（[`calibrate_timer`] を参照）。
///
/// 期限を過ぎたら `None`。
fn wait_for_tick_edge() -> Option<(u64, u64)> {
    let start = crate::idt::timer_ticks();
    let deadline_base = cpu::read_timestamp_counter();
    loop {
        let now = crate::idt::timer_ticks();
        if now != start {
            return Some((now, now.wrapping_sub(start)));
        }
        if cpu::read_timestamp_counter().wrapping_sub(deadline_base)
            > CALIBRATION_EDGE_TIMEOUT_CYCLES
        {
            return None;
        }
        core::hint::spin_loop();
    }
}

/// Local APIC タイマの周波数を PIT 基準で測る（S2-c）。
///
/// # 何に触るか
///
/// **書くのは Divide Configuration と Initial Count の 2 本だけである。**
/// LVT Timer には触らない（マスクされたまま、ファームウェアが残した periodic の
/// まま）。**LINT0 と SVR には触らない。** 呼び出しの前後で読み戻して確かめる。
///
/// LVT Timer がマスクされているので、Initial Count を書いて数え下がりが始まっても
/// 割り込みは 1 本も発生しない。窓（100ms）はカウンタの一周（数十秒規模）より
/// 遥かに短いので、periodic のままでも窓の途中で再装填されない。
///
/// # 呼ぶ位置
///
/// **`sti` の後でなければならない。** 基準に使う `TIMER_TICKS` は IRQ0 が
/// 増やすので、割り込みが有効でないと進まない。`run_timer_loop` が `sti` した
/// 直後、定常ループへ入る前に呼ぶ。**この位置は APIC 関連の他の処理（`kmain` の
/// 前半）から離れている。離れている理由はこれである。**
pub fn calibrate_timer(
    logger: &mut Logger<SerialPort>,
    mapped: &MappedApic,
) -> Option<TimerCalibration> {
    let direct_map = common::addr::direct_map();
    let lapic = direct_map.phys_to_virt(mapped.local_apic).as_u64();

    // 触らないことにしたレジスタを、触らなかったことを示すために控える。
    // SAFETY: `map_and_probe` が写像を確認したページの中を読む。
    let (svr_before, lint0_before, lvt_timer_before) = unsafe {
        (
            read_lapic(lapic, LAPIC_REGISTER_SVR),
            read_lapic(lapic, LAPIC_REGISTER_LVT_LINT0),
            read_lapic(lapic, LAPIC_REGISTER_LVT_TIMER),
        )
    };

    if lvt_timer_before & ENTRY_MASKED_BIT == 0 {
        logger.error(format_args!(
            "apic: the LVT timer is not masked (raw {lvt_timer_before:#010x}); calibration \
             would deliver interrupts, so it is not attempted"
        ));
        return None;
    }

    // SAFETY: 上と同じページ。分周設定と初期カウントだけを書く。LVT Timer が
    // マスクされていることは直前に確かめたので、数え下がりが始まっても割り込みは
    // 発生しない。
    unsafe {
        write_lapic(lapic, LAPIC_REGISTER_TIMER_DIVIDE, LAPIC_TIMER_DIVIDE_BY_16);
        write_lapic(lapic, LAPIC_REGISTER_TIMER_INITIAL_COUNT, u32::MAX);
    }

    let mut samples = [0u64; CALIBRATION_SAMPLES];
    // **較正自身が取りこぼしを見る。** `interrupts::max_tick_jump()` はこの時点では
    // 使えない。あれを更新するのは `run_timer_loop` の定常ループで、較正はその
    // 手前で走るので、前後どちらを読んでも 0 のままになる。**前後が同じ 0 なのは
    // 「取りこぼしが無い」のではなく「まだ数えていない」である。** 一度その形で
    // 書いてしまったので、較正の中で自前に数える形へ直した。
    let mut widest_edge_advance = 0u64;
    for slot in samples.iter_mut() {
        // 窓の始まりをティックのエッジへ揃える。
        let Some((begin_tick, _)) = wait_for_tick_edge() else {
            logger.error(format_args!(
                "apic: no timer tick arrived within the calibration deadline; the local APIC \
                 timer was NOT calibrated"
            ));
            return None;
        };
        // SAFETY: 上と同じ。読み取りのみ。
        let count_begin = unsafe { read_lapic(lapic, LAPIC_REGISTER_TIMER_CURRENT_COUNT) };

        // 窓の終わりも同じくエッジで揃える。
        let count_end;
        let elapsed_ticks;
        loop {
            let Some((now, advance)) = wait_for_tick_edge() else {
                logger.error(format_args!(
                    "apic: the calibration window did not close within the deadline; the local \
                     APIC timer was NOT calibrated"
                ));
                return None;
            };
            // SAFETY: 上と同じ。読み取りのみ。
            let observed = unsafe { read_lapic(lapic, LAPIC_REGISTER_TIMER_CURRENT_COUNT) };
            widest_edge_advance = widest_edge_advance.max(advance);
            if now.wrapping_sub(begin_tick) >= CALIBRATION_WINDOW_TICKS {
                count_end = observed;
                // **名目の N ではなく、実際に進んだティック数で割る。**
                // 取りこぼして N を飛び越えた場合、窓の実時間は N×10ms より長い。
                // 名目で割ると較正結果が過大になる。実測で割れば、飛び越えても
                // 結果は正しいままである（系統誤差が構造的に消える）。
                elapsed_ticks = now.wrapping_sub(begin_tick);
                break;
            }
        }

        // 数え下がりなので begin > end。
        let elapsed_counts = u64::from(count_begin.wrapping_sub(count_end));
        // 窓は elapsed_ticks × (1 / timer_frequency_hz) 秒である。
        // **PIT の周波数もリテラルで持たない。** 境界の問いから取る。
        let reference_hz = u64::from(crate::irq::timer_frequency_hz());
        *slot = elapsed_counts * reference_hz / elapsed_ticks;
    }

    let mut sorted = samples;
    sorted.sort_unstable();
    let median_hz = sorted[CALIBRATION_SAMPLES / 2];
    let spread_hz = sorted[CALIBRATION_SAMPLES - 1] - sorted[0];

    logger.info(format_args!(
        "apic: LAPIC timer calibration: median={median_hz} Hz spread={spread_hz} Hz \
         (divide by 16, window {CALIBRATION_WINDOW_TICKS} PIT tick(s) at {} Hz, \
         {CALIBRATION_SAMPLES} sample(s), edges aligned to tick transitions)",
        crate::irq::timer_frequency_hz()
    ));
    logger.info(format_args!(
        "apic: LAPIC timer calibration samples: {:?} Hz",
        samples
    ));
    // **取りこぼしは較正を過大評価させる。** 窓の実時間が名目より長くなり、
    // そのぶん減少量が増えるためである。上の計算は実測ティック数で割っている
    // ので系統誤差は構造的に消えているが、**取りこぼしが起きたかどうかは
    // それ自体が観測に値する**ので出す。1 なら 1 ティックずつ捉えている。
    logger.info(format_args!(
        "apic: widest tick advance seen inside the calibration windows = \
         {widest_edge_advance} (1 means every edge was caught; the frequency above divides by \
         the observed tick count, not the nominal one, so a larger value would not inflate it)"
    ));

    // 触らないことにしたレジスタが変わっていないことを読み戻す。
    // SAFETY: 上と同じ。読み取りのみ。
    let (svr_after, lint0_after, lvt_timer_after) = unsafe {
        (
            read_lapic(lapic, LAPIC_REGISTER_SVR),
            read_lapic(lapic, LAPIC_REGISTER_LVT_LINT0),
            read_lapic(lapic, LAPIC_REGISTER_LVT_TIMER),
        )
    };
    logger.info(format_args!(
        "apic: untouched after calibration: SVR {svr_before:#010x}->{svr_after:#010x} \
         (software_enabled={}), LINT0 {lint0_before:#010x}->{lint0_after:#010x}, \
         LVT timer {lvt_timer_before:#010x}->{lvt_timer_after:#010x} (masked={})",
        svr_after & LAPIC_SVR_SOFTWARE_ENABLE != 0,
        lvt_timer_after & ENTRY_MASKED_BIT != 0
    ));
    if svr_after != svr_before || lint0_after != lint0_before {
        logger.error(format_args!(
            "apic: SVR or LINT0 changed during calibration; they were supposed to be untouched"
        ));
    }

    Some(TimerCalibration {
        samples,
        median_hz,
        spread_hz,
    })
}

/// Local APIC のレジスタを 1 本書く。
///
/// # Safety
///
/// [`read_lapic`] と同じ。加えて、書き込みが割り込みの配送を変えないことを
/// 呼び出し側が確かめていること。
unsafe fn write_lapic(base_virt: u64, offset: u64, value: u32) {
    // SAFETY: 呼び出し元契約。MMIO なので `write_volatile` で書く。
    unsafe { ((base_virt + offset) as *mut u32).write_volatile(value) }
}
