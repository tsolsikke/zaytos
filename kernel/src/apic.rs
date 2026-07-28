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

/// LVT の並び。**存在する本数は Max LVT Entry + 1 で決まる**ので、
/// 添字がその範囲に収まるものだけを読む。
///
/// 実測（Max LVT Entry = 5、すなわち 6 本）では Timer から Error までが存在し、
/// CMCI（`0x2F0`）は存在しない。**存在しないレジスタを読まない**のは、
/// 未定義の値を観測値として記録しないためである。
const LAPIC_LVT_ENTRIES: [(&str, u64); 6] = [
    ("Timer", 0x320),
    ("Thermal", 0x330),
    ("PMC", 0x340),
    ("LINT0", 0x350),
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
