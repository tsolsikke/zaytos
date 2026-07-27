//! ACPI テーブルの検証つき走査（S1-b）。
//!
//! # なぜ `smp` の下に置かないのか
//!
//! MADT の消費者は S3（AP 起こし）だけではなく、S2 の割り込み層（`irq`）でも
//! ある。`smp::acpi` にすると `irq` が `smp` に依存することになり、責務の線と
//! しては向きが逆になる。ACPI は「ファームウェアが提示する構成表」であって
//! SMP の一部ではないので、`smp` と並ぶ独立したモジュールにしてある。
//!
//! # 何を公開するか
//!
//! **問いの形だけを公開する。** 生の物理アドレスやテーブルの生バイトを外へ
//! 出さない（S0-a の「境界は生の値を出さない」の適用）。S1-b の時点で外から
//! 要る問いは「ACPI に何があったか」だけで、それはログに出る。値を返す API は
//! 消費者（S2 の `irq`、S3 の `smp`）が現れた時点で、その消費者が必要とする
//! 形で足す。先回りして作らない。
//!
//! # 異常はすべて報告して継続する
//!
//! S1 は情報を集める段である。**ACPI が読めないだけで、単一コアで動いている
//! カーネルが起動しなくなるのは機能的な後退である。** したがってこのモジュール
//! は検出したものを大きく報告するだけで、`halt` しない。致命へ格上げするのは
//! ACPI 無しでは進めなくなる S2（APIC 移行）である（`roadmap.md`）。
//!
//! # 物理アドレスへ触る前に必ず walk する
//!
//! 参照する物理アドレスは、[`crate::paging::active::ActivePageTable::translate`]
//! で**実際に稼働中のページテーブルを辿って**マップ済みを確かめてから読む。
//! 計画（`MappedRanges::contains_range`）を見るだけでは「計画にあるが実際には
//! 張られていない」を見逃す（B-2a-5 が実証した穴と同じ性質である）。ACPI
//! テーブルはファームウェアが置く場所であり、`EfiReservedMemoryType` の穴
//! （実測で `0xf6ed000..0xf76d000`）の中に落ちる可能性が構造的にある。
//! そこを踏んだときに #PF で落ちるのではなく、検出して報告する。

mod rsdp;

use common::addr::{DirectMap, PhysAddr};
use common::log::Logger;
use common::serial::SerialPort;

use crate::frame_allocator::FRAME_SIZE;
use crate::memory_map;
use crate::paging::active::{ActivePageTable, TranslateError};

/// 物理メモリを読もうとして断念した理由。**「読めなかった」を一色に丸めない。**
/// 窓の外・未マップ・別の物理を指している、は原因も対処も違う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadError {
    /// direct map 窓が覆っていない物理アドレス。
    OutsideWindow { page: PhysAddr },
    /// 窓の中だが、稼働中のページテーブルに翻訳が無い。
    NotMapped { page: PhysAddr },
    /// 翻訳はあるが、写っている物理が食い違う。窓が別名になっている。
    Aliased { page: PhysAddr, got: PhysAddr },
    /// walker が扱えない形（1GiB ページ等）。
    Untranslatable {
        page: PhysAddr,
        error: TranslateError,
    },
    /// 範囲の終端が物理アドレスとして表せない。
    RangeOverflow { start: PhysAddr, len: usize },
}

/// direct map 窓経由の物理メモリ読み取り器。
///
/// **読む前に必ず walk する**（モジュールの doc を参照）。読み取りは
/// `read_volatile` で 1 バイトずつ行う。ACPI テーブルは通常の RAM 上にあるが、
/// 「ファームウェアが書いた領域をこちらは読むだけ」という関係なので、
/// コンパイラに読みをまとめたり消したりさせない形にしておく。
struct PhysReader {
    table: ActivePageTable,
    direct_map: DirectMap,
}

impl PhysReader {
    /// # Safety
    ///
    /// CR3 が指す稼働中のページテーブルを、登録済みの direct map 窓経由で
    /// 読めること（[`ActivePageTable::current`] の契約）。呼び出し位置は
    /// A-2（窓の高位化）より後でなければならない。
    unsafe fn new() -> Self {
        let direct_map = common::addr::direct_map();
        Self {
            // SAFETY: 呼び出し元契約をそのまま引き継ぐ。
            table: unsafe { ActivePageTable::current(direct_map) },
            direct_map,
        }
    }

    /// `page`（4KiB 境界）が窓経由で読めるかを、実テーブルを辿って確かめる。
    fn check_page(&self, page: PhysAddr) -> Result<(), ReadError> {
        if !self.direct_map.covers(page) {
            return Err(ReadError::OutsideWindow { page });
        }
        let virt = self.direct_map.phys_to_virt(page);
        match self.table.translate(virt) {
            // 窓は phys_to_virt(p) = base + p なので、翻訳結果は p そのものに
            // なるはずである。**一致まで見る。** 「present だった」だけでは、
            // 別の物理を読んでいても気づけない。
            Ok(Some(translation)) if translation.phys == page => Ok(()),
            Ok(Some(translation)) => Err(ReadError::Aliased {
                page,
                got: translation.phys,
            }),
            Ok(None) => Err(ReadError::NotMapped { page }),
            Err(error) => Err(ReadError::Untranslatable { page, error }),
        }
    }

    /// `start` から `dest.len()` バイトを読む。**跨ぐページを 1 枚ずつ確かめる。**
    ///
    /// ACPI のテーブルは 16 バイト境界にしか整列していないので、数十バイトの
    /// 構造体でもページ境界を跨ぎうる。先頭だけを確かめる形は、跨いだ先が
    /// 未マップのときに #PF になる。
    fn read(&self, start: PhysAddr, dest: &mut [u8]) -> Result<(), ReadError> {
        if dest.is_empty() {
            return Ok(());
        }
        let len = dest.len();
        let Some(last) = start.checked_add(len as u64 - 1) else {
            return Err(ReadError::RangeOverflow { start, len });
        };
        let (Some(first_page), Some(last_page)) =
            (start.align_down(FRAME_SIZE), last.align_down(FRAME_SIZE))
        else {
            return Err(ReadError::RangeOverflow { start, len });
        };

        let mut page = first_page;
        loop {
            self.check_page(page)?;
            if page.as_u64() >= last_page.as_u64() {
                break;
            }
            let Some(next) = page.checked_add(FRAME_SIZE) else {
                return Err(ReadError::RangeOverflow { start, len });
            };
            page = next;
        }

        // **1 バイトずつバッファへ写す。この形を単純化して戻さないこと。**
        //
        // ACPI のテーブルは 16 バイト境界にしか整列しておらず、しかもヘッダが
        // 36 バイトなので、XSDT の 64 ビットエントリの配列は**8 バイト境界に
        // 載らない**（36 は 8 の倍数ではない）。`*const u64` を作って素直に読むと
        // 未定義動作になる。`read_unaligned` を使えば安全に書けるが、それは
        // 「正しく使えば安全」であって、**この形は間違えようがない**。
        // バッファへ写してから `from_le_bytes` でスライスとして解釈する限り、
        // 未整列の生ポインタ参照は構造的に発生しない（型として作れない）。
        // 短く書けるからと `*const u64` や `read_unaligned` へ寄せると、
        // 「規律で守る」形に戻る。
        let src = self.direct_map.phys_to_virt(start).as_ptr::<u8>();
        for (index, out) in dest.iter_mut().enumerate() {
            // SAFETY: 上のループで [start, start+len) が載る全ページについて、
            // 稼働中のページテーブルに窓経由の翻訳があり、その翻訳が当の物理
            // ページを指していることを確かめてある。読み取りのみで、
            // ファームウェアが置いたテーブルの内容を変更しない。u8 の読みなので
            // 整列の要件も無い。
            *out = unsafe { core::ptr::read_volatile(src.add(index)) };
        }
        Ok(())
    }
}

/// 読み取りに失敗した理由をログへ出す。
fn report_read_error(logger: &mut Logger<SerialPort>, what: &str, error: ReadError) {
    match error {
        ReadError::OutsideWindow { page } => logger.error(format_args!(
            "acpi: {what} lies at {:#x}, outside the direct map window; not read",
            page.as_u64()
        )),
        ReadError::NotMapped { page } => logger.error(format_args!(
            "acpi: {what} needs physical page {:#x}, which the live page table does not map; \
             not read (this is the EfiReservedMemoryType hole case; see architecture.md 6.4)",
            page.as_u64()
        )),
        ReadError::Aliased { page, got } => logger.error(format_args!(
            "acpi: the direct map window resolves physical page {:#x} to {:#x} while reading \
             {what}; the window is aliased and nothing was read",
            page.as_u64(),
            got.as_u64()
        )),
        ReadError::Untranslatable { page, error } => logger.error(format_args!(
            "acpi: physical page {:#x} for {what} cannot be translated ({error:?}); not read",
            page.as_u64()
        )),
        ReadError::RangeOverflow { start, len } => logger.error(format_args!(
            "acpi: the range {:#x}..+{len} for {what} does not fit in a physical address; not read",
            start.as_u64()
        )),
    }
}

/// 物理アドレスが UEFI メモリマップのどの型に載っているかを報告する。
///
/// # なぜ型で見るのか
///
/// **`classify()` は型を見てマップするかを決める。** したがって型が分かれば、
/// その領域がマップされるかは構造的に決まる。`EfiACPIReclaimMemory` /
/// `EfiACPIMemoryNVS` なら常に写り、`EfiReservedMemoryType` なら常に写らない。
/// メモリマップの細部（各領域の大きさや個数）が起動ごとに揺れても、この関係は
/// 動かない。
///
/// 「何回起動しても毎回マップされていた」は回数を根拠にした判断であり、この
/// プロジェクトが一度踏んだ型（20 回連続 PASS を根拠に基準を決め、真の率が
/// 別だった）と同じである。**回数ではなく機序を記録する。**
fn report_memory_type(
    logger: &mut Logger<SerialPort>,
    what: &str,
    phys: PhysAddr,
    memory_map_bytes: &[u8],
    descriptor_size: u64,
) {
    match memory_map::find_entry(memory_map_bytes, descriptor_size, phys.as_u64()) {
        Ok(Some(entry)) => logger.info(format_args!(
            "acpi: {what} at {:#x} lies in UEFI memory type {} ({}), descriptor {:#x}..{:#x}; \
             classify() = {:?}",
            phys.as_u64(),
            entry.memory_type,
            memory_map::type_name(entry.memory_type),
            entry.phys_start,
            entry.phys_start + entry.page_count * FRAME_SIZE,
            memory_map::classify(entry.memory_type),
        )),
        // **メモリマップに現れない物理アドレスは実在する。** フレームバッファ
        // （PCI BAR）がそうで、ACPI 側で起きても不思議ではない。「型が無い」
        // ことも観測結果として残す。
        Ok(None) => logger.warn(format_args!(
            "acpi: {what} at {:#x} is not covered by any UEFI memory descriptor",
            phys.as_u64()
        )),
        Err(e) => logger.error(format_args!(
            "acpi: could not walk the UEFI memory map while classifying {what}: {e}"
        )),
    }
}

/// ACPI テーブルを検証しながら一巡し、見つけたものをログへ出す（S1-b）。
///
/// # 呼ぶ位置
///
/// - **A-2（direct map 窓の高位化）より後。** 窓経由で物理を読むため。
/// - **恒等除去（B-2b-4）より前。** `memory_map_bytes` は低位 VA のスライスで、
///   除去後は無効になる。**この制約は呼び出し側のコメントにも書いてある。**
///   検査そのものは高位窓の翻訳を見るので、除去を跨いでも結論は変わらない
///   （除去が落とすのは `PML4[0]` だけである）。
///
/// `rsdp_phys` が 0 のときは bootloader が RSDP を見つけられなかった場合で、
/// 何も走査せずにその旨を報告する。
pub fn survey(
    logger: &mut Logger<SerialPort>,
    rsdp_phys: PhysAddr,
    memory_map_bytes: &[u8],
    descriptor_size: u64,
) {
    if rsdp_phys.as_u64() == 0 {
        logger.error(format_args!(
            "acpi: the bootloader reported no RSDP; nothing to survey (S2 will need it)"
        ));
        return;
    }

    // SAFETY: 呼び出し位置の契約（この関数の doc）により、CR3 は自前の
    // ページテーブルを指し、登録 direct map 窓は高位で稼働している。
    let reader = unsafe { PhysReader::new() };

    report_memory_type(
        logger,
        "the RSDP",
        rsdp_phys,
        memory_map_bytes,
        descriptor_size,
    );

    // 名乗る長さを読むまで、何バイト読めばよいかが決まらない。まず ACPI 1.0
    // 部（20 バイト）だけを読む。**revision 0 のファームウェアでは、これが
    // RSDP の全部である。** 36 バイトを一律に要求すると、20 バイトしか
    // 置いていないファームウェアで「読めない」と誤って報告することになる。
    let mut buffer = [0u8; rsdp::READ_BUFFER_LENGTH];
    if let Err(e) = reader.read(rsdp_phys, &mut buffer[..rsdp::V1_LENGTH]) {
        report_read_error(logger, "the RSDP", e);
        return;
    }

    let header = match rsdp::parse_header(&buffer[..rsdp::V1_LENGTH]) {
        Ok(header) => header,
        Err(e) => {
            logger.error(format_args!(
                "acpi: the RSDP at {:#x} failed validation: {e:?}",
                rsdp_phys.as_u64()
            ));
            return;
        }
    };
    logger.info(format_args!(
        "acpi: RSDP at {:#x} validated: revision={} oem_id={:?} rsdt_address={:#x}",
        rsdp_phys.as_u64(),
        header.revision,
        core::str::from_utf8(&header.oem_id).unwrap_or("<not utf-8>"),
        header.rsdt_address
    ));

    let extended = if header.has_extended_part() {
        read_extended(logger, &reader, rsdp_phys, &mut buffer)
    } else {
        logger.info(format_args!(
            "acpi: revision {} predates ACPI 2.0, so the RSDP has no XSDT pointer",
            header.revision
        ));
        None
    };

    match rsdp::root_table(&header, extended.as_ref()) {
        Some(rsdp::RootTable::Xsdt { phys }) => logger.info(format_args!(
            "acpi: following the XSDT at {phys:#x} (64-bit entries)"
        )),
        // **RSDT へ落ちる形も残す。** revision 0 のファームウェアで XSDT だけを
        // 実装していると、ここで静かに「テーブル無し」になる。
        Some(rsdp::RootTable::Rsdt { phys }) => logger.info(format_args!(
            "acpi: following the RSDT at {phys:#x} (32-bit entries)"
        )),
        None => logger.error(format_args!(
            "acpi: the RSDP names neither an XSDT nor an RSDT; there is no table to follow"
        )),
    }

    // **ここまでが S1-b-1 である。** XSDT/RSDT と MADT の走査は S1-b-2 で足す。
    logger.info(format_args!(
        "acpi: S1-b-1 stops here; the root table itself is not walked yet (S1-b-2)"
    ));
}

/// 拡張部（ACPI 2.0 以降）を読んで検証する。失敗しても `None` を返すだけで、
/// 呼び出し側は RSDT へ落ちて続行する。
fn read_extended(
    logger: &mut Logger<SerialPort>,
    reader: &PhysReader,
    rsdp_phys: PhysAddr,
    buffer: &mut [u8; rsdp::READ_BUFFER_LENGTH],
) -> Option<rsdp::RsdpExtended> {
    // 名乗る長さを読むために、まず固定長（36 バイト）まで読み足す。読み直しに
    // なるが、オフセットを持ち回るより、範囲の検査が 1 か所で済む形を採る。
    if let Err(e) = reader.read(rsdp_phys, &mut buffer[..rsdp::V2_LENGTH]) {
        report_read_error(logger, "the extended part of the RSDP", e);
        return None;
    }
    let length = match rsdp::declared_length(&buffer[..rsdp::V2_LENGTH]) {
        Ok(rsdp::DeclaredLength::Verifiable(length)) => length,
        // **「検証できない」は「壊れている」ではない。** 将来の版が長い RSDP を
        // 出すことは仕様の範囲内なので、不正として報告しない。かといって 36 バイト
        // だけで検算して通すこともしない。検証していない値は使わない。
        Ok(rsdp::DeclaredLength::NotVerifiable { length }) => {
            logger.warn(format_args!(
                "acpi: the RSDP at {:#x} declares {length} bytes, more than our {}-byte read \
                 buffer, so its checksum was NOT verified. this is not necessarily a broken \
                 RSDP (a future revision may legitimately be longer); the XSDT pointer is left \
                 unused because it has not been checked",
                rsdp_phys.as_u64(),
                rsdp::READ_BUFFER_LENGTH
            ));
            return None;
        }
        Err(e) => {
            logger.error(format_args!(
                "acpi: the RSDP at {:#x} declares an invalid length: {e:?}",
                rsdp_phys.as_u64()
            ));
            return None;
        }
    };
    // 名乗った長さが 36 を超えるなら、その分まで読んでからチェックサムを取る。
    let length_usize = length.get() as usize;
    if length_usize > rsdp::V2_LENGTH {
        logger.info(format_args!(
            "acpi: the RSDP declares {length_usize} bytes, more than the {} the specification \
             fixes; reading the rest before checksumming",
            rsdp::V2_LENGTH
        ));
        if let Err(e) = reader.read(rsdp_phys, &mut buffer[..length_usize]) {
            report_read_error(logger, "the extended part of the RSDP", e);
            return None;
        }
    }

    match rsdp::parse_extended(&buffer[..length_usize], length) {
        Ok(extended) => {
            logger.info(format_args!(
                "acpi: the extended part of the RSDP validated: length={} xsdt_address={:#x}",
                extended.length, extended.xsdt_address
            ));
            Some(extended)
        }
        Err(e) => {
            logger.error(format_args!(
                "acpi: the extended part of the RSDP at {:#x} failed validation: {e:?}",
                rsdp_phys.as_u64()
            ));
            None
        }
    }
}
