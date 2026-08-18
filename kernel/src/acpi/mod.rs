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
//! **テーブルの生バイトとパーサの型は外へ出さない**（S0-a の「境界は生の値を
//! 出さない」の適用）。出すのは、消費者が現れた時点で、その消費者が必要とする
//! 形だけである。先回りして作らない。
//!
//! S1-b の時点で外から要る問いは「ACPI に何があったか」だけで、それはログに
//! 出ていたので、この関数は何も返していなかった。**S1-c で最初の消費者が
//! 現れた。** APIC の MMIO を写像するには物理アドレスそのものが要るので、
//! [`ApicMmio`] として出す。**したがって「物理アドレスは境界の中に留まる」は
//! もう成り立たない。** 留まるのは生バイトとパーサの型である。
//!
//! **物理アドレスなら出してよい理由。** 物理アドレスは `acpi` と `paging` が
//! 共有する語彙であり、**写像する側がそれを受け取らなければ仕事ができない。**
//! 境界が隠すべきなのは、その境界の内側だけで意味を持つ表現（テーブルの生バイト、
//! パーサの型、エントリの並び）である。S0-a で `managed_vectors` がベクタ番号を
//! 露出してよかったのと同じ理由で、ベクタ番号は `irq` と `idt` が共有する語彙
//! だった。**「生の値を出さない」を、共有語彙まで隠す意味に取らないこと。**
//! 取ると、受け渡すためだけの抽象を挟むことになる。
//!
//! 出すのは写像に要る所在だけで、エントリの解釈（Interrupt Source Override の
//! 対応付けなど）は出さない。それが要るのは S2 で、そのとき S2 が必要とする形で
//! 足す。
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

mod madt;
mod rsdp;
mod sabotage;
mod sdt;

use common::addr::{DirectMap, PhysAddr};
use common::log::Logger;
use common::serial::SerialPort;

use crate::frame_allocator::FRAME_SIZE;
use crate::memory_map;
use crate::paging::active::{ActivePageTable, TranslateError};

/// テーブル 1 つを読むためのバッファの大きさ。
///
/// **この値に外部の根拠は無い。我々が選んだバッファの大きさである**
/// （[`rsdp::READ_BUFFER_LENGTH`] と同じ性質）。実測では XSDT が 100 バイト前後、
/// MADT が 200 バイト未満だが、CPU 数に比例して伸びる。これを超える長さを
/// 名乗るテーブルは**「不正」ではなく「検証不能」**として扱い、中身を使わない。
///
/// 1KiB をカーネルスタック（64KiB）の上に取る。ルートテーブルと MADT で
/// 同時に 2 枚使うことはない（ルートを走査し終えてから MADT を読む）。
///
/// **S3 への申し送り: 限界が存在する。** MADT は概ね `44 + N×8` バイトなので、
/// **100 コア規模でこのバッファを超え、「検証不能」に落ちる。** ZaytOS が
/// その規模を扱う日は遠いが、限界を知らずに踏むのとは違う。
const TABLE_READ_BUFFER_LENGTH: usize = 1024;

/// 記録する I/O APIC の上限。
///
/// **超えた分は黙って捨てない。** 実測（QEMU + OVMF）では 1 個だが、実機では
/// 複数ありうる。捨てた数を [`ApicMmio::io_apics_dropped`] で数え、呼び出し側が
/// 報告できるようにしてある。上限を設けること自体は避けられない（ヒープを
/// 使わない）が、上限に当たったことを隠すのは避けられる。
const MAX_IO_APICS: usize = 4;

/// 記録する Interrupt Source Override の上限。
///
/// ISA の IRQ は 16 本なので、それを超える上書きは意味を持たない。実測は 5 件。
const MAX_INTERRUPT_SOURCE_OVERRIDES: usize = 16;

/// 記録する使用可能な Local APIC ID の上限（S3-b-2b-1）。
///
/// **`MAX_CPUS` とは別の上限である。** ここは「MADT が報告したものを何本覚えるか」で、
/// `MAX_CPUS` は「per-CPU スロットが何本あるか」である。**覚えた本数のうち
/// `MAX_CPUS` を超える分は起こさない**（`roadmap.md` の S3-b-2b-1）。
/// **超えた分を記録できずに落とすと、起こさなかったコアがあることを報告できない**
/// ので、`MAX_CPUS` より広く取る。
const MAX_LOCAL_APIC_IDS: usize = 8;

/// I/O APIC 1 個の所在。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoApicLocation {
    /// MADT が名乗る ID。
    pub id: u8,
    /// MMIO の物理アドレス。**既定値をハードコードせず MADT から取った値である。**
    pub phys: PhysAddr,
    /// この I/O APIC が担当する割り込みの先頭 GSI。S2 の入力になる。
    pub global_system_interrupt_base: u32,
}

/// S1-c が写像するために必要な APIC の MMIO の所在。
///
/// **`survey` が MADT から読み取った値だけを持つ。** 既定値のハードコードは
/// 一切含まない。MADT が読めなかった場合や壊れていた場合は、すべて空になる
/// （[`ApicMmio::empty`]）。**「読めなかった」と「無かった」を、呼び出し側が
/// 区別する必要はない。** どちらの場合も写像すべきものが無いという結論は同じで、
/// 理由はすでに `survey` がログへ出している。
#[derive(Debug, Clone, Copy)]
pub struct ApicMmio {
    local_apic: Option<PhysAddr>,
    io_apics: [Option<IoApicLocation>; MAX_IO_APICS],
    io_apics_found: usize,
    bsp_candidate_apic_id: Option<u8>,
    interrupt_source_overrides:
        [Option<madt::InterruptSourceOverride>; MAX_INTERRUPT_SOURCE_OVERRIDES],
    interrupt_source_overrides_found: usize,
    /// 使用可能な Local APIC の ID（S3-b-2b-1）。**AP を起こすのに要る。**
    ///
    /// 先頭は bootstrap processor の候補である（MADT の並び順）。
    local_apic_ids: [Option<u8>; MAX_LOCAL_APIC_IDS],
    /// 記録できずに落とした本数。**落としたことを黙らせない。**
    local_apic_ids_dropped: usize,
    /// MADT が報告した「使用可能な」Local APIC の本数（S3-b-1）。
    ///
    /// **per-CPU スロットの境界を起動時に保証するために持つ。** `MAX_CPUS` を
    /// 超えていたら `this_cpu_ptr` が配列外を指しうるので、進む前に落とす。
    usable_local_apics: usize,
}

impl ApicMmio {
    const fn empty() -> Self {
        Self {
            local_apic: None,
            io_apics: [None; MAX_IO_APICS],
            io_apics_found: 0,
            bsp_candidate_apic_id: None,
            interrupt_source_overrides: [None; MAX_INTERRUPT_SOURCE_OVERRIDES],
            interrupt_source_overrides_found: 0,
            usable_local_apics: 0,
            local_apic_ids: [None; MAX_LOCAL_APIC_IDS],
            local_apic_ids_dropped: 0,
        }
    }

    /// 使用可能な Local APIC の ID を、MADT の並び順で返す。
    ///
    /// **先頭が bootstrap processor の候補である。** AP はそれ以降である。
    pub fn local_apic_ids(&self) -> impl Iterator<Item = u8> + '_ {
        self.local_apic_ids.iter().flatten().copied()
    }

    /// 記録できずに落とした Local APIC ID の本数。
    pub const fn local_apic_ids_dropped(&self) -> usize {
        self.local_apic_ids_dropped
    }

    /// MADT が報告した使用可能な Local APIC の本数（= 起動しうるコア数）。
    pub const fn usable_local_apics(&self) -> usize {
        self.usable_local_apics
    }

    /// レガシー IRQ が I/O APIC のどの GSI へ現れるか。
    ///
    /// Interrupt Source Override（MADT type 2）に一致があればその GSI、
    /// 無ければ IRQ 番号をそのまま GSI とする（恒等）。
    ///
    /// # この構成では、配送経路からは常に恒等が返る
    ///
    /// **非恒等の枝は表に実在する**（実測で IRQ0 が GSI2 へ移る）。しかし
    /// **現在の配送経路が問う IRQ には ISO が無い。** S2-d-1 が I/O APIC 経由へ
    /// 移すのはキーボード（IRQ1）で、実測の 5 件は IRQ 0 / 5 / 9 / 10 / 11 で
    /// あり IRQ1 を含まない。S2-d-2 のタイマは Local APIC タイマなので、
    /// そもそも I/O APIC を経由せず IRQ0 の上書きを使わない。
    ///
    /// **したがって「この関数が配送経路で効いていること」は、この構成では
    /// 破壊確認で示せない。** 無視する実装に差し替えても、配送に使う IRQ1 では
    /// 同じ答えになるからである。非恒等の枝はホストテストで固定してあり、
    /// 起動時には解決結果をログへ出して表が読めていることを示す。
    /// **示せる範囲を超えて主張しないこと**（`verification-coverage.md` の
    /// 「Interrupt Source Override の解決（S2-d-0）」）。
    pub fn gsi_for_irq(&self, irq: u8) -> u32 {
        let mut index = 0;
        while index < self.interrupt_source_overrides.len() {
            if let Some(iso) = self.interrupt_source_overrides[index] {
                if iso.source == irq {
                    return iso.global_system_interrupt;
                }
            }
            index += 1;
        }
        u32::from(irq)
    }

    /// この IRQ に Interrupt Source Override が在るか（S13-d）。
    ///
    /// **flags が 0 の override と「override が無い」を分けるために要る**——
    /// [`Self::redirection_flags_for_irq`] はどちらも 0 を返す。firmware が
    /// 宣言しているなら、その値（エッジ・ハイでも）が platform の答えである。
    pub fn has_override_for_irq(&self, irq: u8) -> bool {
        self.interrupt_source_overrides()
            .any(|iso| iso.source == irq)
    }

    /// この IRQ を I/O APIC の redirection entry へ書くときの、極性とトリガの
    /// ビット（S2-d-1c）。
    ///
    /// **Interrupt Source Override に一致があればその指定、無ければバス既定
    /// （ISA なので active high・edge）である。** 既定は両ビットとも 0 なので、
    /// 一致が無ければ 0 を返す。
    ///
    /// **恒等（上書きが無い）であることに依存した書き方をしないために、
    /// 常にこの関数を通す。** この構成の IRQ1 には上書きが無く戻り値は 0 に
    /// なるが、上書きのある IRQ を扱った瞬間に静かに誤る形を避ける。
    ///
    /// 返すのは `crate::apic` の redirection entry のビット位置に合わせた値で、
    /// **MADT の生の flags ではない。** 両者はビット位置が違う。
    pub fn redirection_flags_for_irq(&self, irq: u8) -> u32 {
        let mut flags = 0;
        for iso in self.interrupt_source_overrides() {
            if iso.source != irq {
                continue;
            }
            if iso.active_low() {
                flags |= crate::apic::ENTRY_ACTIVE_LOW_BIT;
            }
            if iso.level_triggered() {
                flags |= crate::apic::ENTRY_LEVEL_TRIGGERED_BIT;
            }
            break;
        }
        flags
    }

    /// 記録できた Interrupt Source Override。
    pub fn interrupt_source_overrides(
        &self,
    ) -> impl Iterator<Item = madt::InterruptSourceOverride> + '_ {
        self.interrupt_source_overrides
            .iter()
            .filter_map(|slot| *slot)
    }

    /// MADT にあった Interrupt Source Override の総数（上限で捨てた分を含む）。
    pub const fn interrupt_source_overrides_found(&self) -> usize {
        self.interrupt_source_overrides_found
    }

    /// Local APIC の MMIO 物理アドレス。
    ///
    /// 固定部の 32 ビット値か、Local APIC Address Override（type 5）があれば
    /// そちらである。
    pub const fn local_apic(&self) -> Option<PhysAddr> {
        self.local_apic
    }

    /// 記録できた I/O APIC。
    pub fn io_apics(&self) -> impl Iterator<Item = IoApicLocation> + '_ {
        self.io_apics.iter().filter_map(|slot| *slot)
    }

    /// MADT にあった I/O APIC の総数（上限で捨てた分を含む）。
    pub const fn io_apics_found(&self) -> usize {
        self.io_apics_found
    }

    /// 上限を超えて記録できなかった I/O APIC の数。
    pub const fn io_apics_dropped(&self) -> usize {
        self.io_apics_found.saturating_sub(MAX_IO_APICS)
    }

    /// 最初の使用可能な Local APIC の ID。
    ///
    /// **BSP の ID とは限らない。** MADT のエントリ順が BSP を先頭にする保証は
    /// 仕様に無い。読み取った Local APIC ID との突き合わせに使うが、
    /// **この突き合わせは弱い**（[`crate::apic`] の該当箇所に理由がある）。
    pub const fn bsp_candidate_apic_id(&self) -> Option<u8> {
        self.bsp_candidate_apic_id
    }
}

/// 署名や OEM ID を、そのままログへ出せる形にする。
///
/// **UTF-8 でないバイト列は実在する。** 壊れたテーブルを読んだときにここで
/// panic すると、報告するための経路がカーネルを落とす。
fn as_text(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("<not utf-8>")
}

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
///
/// # 戻り値
///
/// 見つかった APIC の MMIO の所在（S1-c が写像に使う）。走査のどこかで
/// 断念した場合は空を返す。**理由はこの関数がログへ出しているので、
/// 呼び出し側が「なぜ空か」を再構成する必要はない。**
pub fn survey(
    logger: &mut Logger<SerialPort>,
    rsdp_phys: PhysAddr,
    memory_map_bytes: &[u8],
    descriptor_size: u64,
) -> ApicMmio {
    if rsdp_phys.as_u64() == 0 {
        logger.error(format_args!(
            "acpi: the bootloader reported no RSDP; nothing to survey (S2 will need it)"
        ));
        return ApicMmio::empty();
    }

    // 破壊確認（未マップ / 窓の外）。既定ビルドでは受け取った値をそのまま返す。
    let rsdp_phys = sabotage::redirect_rsdp(logger, rsdp_phys, memory_map_bytes, descriptor_size);

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
        return ApicMmio::empty();
    }

    let header = match rsdp::parse_header(&buffer[..rsdp::V1_LENGTH]) {
        Ok(header) => header,
        Err(e) => {
            logger.error(format_args!(
                "acpi: the RSDP at {:#x} failed validation: {e:?}",
                rsdp_phys.as_u64()
            ));
            return ApicMmio::empty();
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

    let (root_phys, width) = match rsdp::root_table(&header, extended.as_ref()) {
        Some(rsdp::RootTable::Xsdt { phys }) => {
            logger.info(format_args!(
                "acpi: following the XSDT at {phys:#x} (64-bit entries)"
            ));
            (phys, sdt::EntryWidth::Xsdt)
        }
        // **RSDT へ落ちる形も残す。** revision 0 のファームウェアで XSDT だけを
        // 実装していると、ここで静かに「テーブル無し」になる。
        Some(rsdp::RootTable::Rsdt { phys }) => {
            logger.info(format_args!(
                "acpi: following the RSDT at {phys:#x} (32-bit entries)"
            ));
            (phys as u64, sdt::EntryWidth::Rsdt)
        }
        None => {
            logger.error(format_args!(
                "acpi: the RSDP names neither an XSDT nor an RSDT; there is no table to follow"
            ));
            return ApicMmio::empty();
        }
    };

    let Some(madt_phys) = walk_root_table(
        logger,
        &reader,
        root_phys,
        width,
        memory_map_bytes,
        descriptor_size,
    ) else {
        return ApicMmio::empty();
    };

    walk_madt(
        logger,
        &reader,
        madt_phys,
        memory_map_bytes,
        descriptor_size,
    )
}

/// 物理アドレスを [`PhysAddr`] にする。表せない値は報告して `None`。
///
/// **ファームウェアが書いた値をそのまま信じない。** `PhysAddr::new` は 52 ビットを
/// 超える値を弾くので、ここで落ちるということは表として壊れているということである。
fn checked_phys(logger: &mut Logger<SerialPort>, what: &str, raw: u64) -> Option<PhysAddr> {
    match PhysAddr::new(raw) {
        Some(phys) => Some(phys),
        None => {
            logger.error(format_args!(
                "acpi: {what} names {raw:#x}, which is not a representable physical address"
            ));
            None
        }
    }
}

/// テーブルを読み、署名・長さ・チェックサムを検証してバッファへ載せる。
///
/// **`length` が示す範囲全体を読むことが、そのまま「全体がマップ済み」の確認に
/// なる。** [`PhysReader::read`] が跨ぐページを 1 枚ずつ walk するので、範囲の
/// どこか一部だけが未マップという状態はここで捕まる。
///
/// 戻り値は検証済みの長さ。バッファの `..length` が使える。
struct TableRequest<'a> {
    /// ログに出す表示名。
    what: &'a str,
    /// 破壊確認の対象。**表示名では照合しない**（`sabotage::Target` の doc を参照）。
    target: sabotage::Target,
    phys: PhysAddr,
    expected_signature: &'a [u8; sdt::SIGNATURE_LENGTH],
    /// このテーブルが名乗ってよい最小の長さ。テーブルごとに違う。
    minimum_length: u32,
}

fn read_and_verify_table(
    logger: &mut Logger<SerialPort>,
    reader: &PhysReader,
    request: TableRequest<'_>,
    buffer: &mut [u8; TABLE_READ_BUFFER_LENGTH],
) -> Option<usize> {
    let TableRequest {
        what,
        target,
        phys,
        expected_signature,
        minimum_length,
    } = request;
    if let Err(e) = reader.read(phys, &mut buffer[..sdt::HEADER_LENGTH]) {
        report_read_error(logger, what, e);
        return None;
    }

    // 破壊確認（署名 / 長さ）はヘッダを読んだ直後、検証の直前に効かせる。
    sabotage::corrupt_table_header(target, &mut buffer[..sdt::HEADER_LENGTH]);

    let header = match sdt::parse_header(&buffer[..sdt::HEADER_LENGTH], minimum_length) {
        Ok(header) => header,
        Err(e) => {
            logger.error(format_args!(
                "acpi: the header of {what} at {:#x} failed validation: {e:?}",
                phys.as_u64()
            ));
            return None;
        }
    };
    if let Err(e) = sdt::check_signature(&header, expected_signature) {
        logger.error(format_args!(
            "acpi: {what} at {:#x} has the wrong signature: {e:?}",
            phys.as_u64()
        ));
        return None;
    }

    let length = header.length as usize;
    // 長すぎるテーブルは「不正」ではなく「検証不能」である（RSDP と同じ区分。
    // rsdp::READ_BUFFER_LENGTH の doc を参照）。読めないだけで、壊れているとは
    // 限らない。検証していない以上、中身は使わない。
    if length > TABLE_READ_BUFFER_LENGTH {
        logger.warn(format_args!(
            "acpi: {what} at {:#x} declares {length} bytes, more than our \
             {TABLE_READ_BUFFER_LENGTH}-byte read buffer, so its checksum was NOT verified and \
             its contents are left unused. this is not necessarily a broken table",
            phys.as_u64()
        ));
        return None;
    }

    if let Err(e) = reader.read(phys, &mut buffer[..length]) {
        report_read_error(logger, what, e);
        return None;
    }

    // 破壊確認（チェックサム / エントリ長 0）は本体を読んだ直後、検算の直前。
    sabotage::corrupt_table_body(target, &mut buffer[..length]);

    if let Err(e) = sdt::verify_checksum(&buffer[..length], header.length) {
        logger.error(format_args!(
            "acpi: {what} at {:#x} failed its checksum: {e:?}",
            phys.as_u64()
        ));
        return None;
    }

    logger.info(format_args!(
        "acpi: {what} at {:#x} validated: signature={:?} length={length} revision={} oem_id={:?}",
        phys.as_u64(),
        as_text(&header.signature),
        header.revision,
        as_text(&header.oem_id),
    ));
    Some(length)
}

/// ルートテーブル（XSDT / RSDT）を走査し、MADT の物理アドレスを返す。
///
/// 各エントリが指すテーブルのヘッダを読んで署名を出す。**黙って MADT だけを
/// 探して他を捨てない。** 何が置かれているかは S2 以降で効いてくる情報である。
fn walk_root_table(
    logger: &mut Logger<SerialPort>,
    reader: &PhysReader,
    root_phys: u64,
    width: sdt::EntryWidth,
    memory_map_bytes: &[u8],
    descriptor_size: u64,
) -> Option<PhysAddr> {
    let root_phys = checked_phys(logger, "the root table pointer", root_phys)?;
    report_memory_type(
        logger,
        width.name(),
        root_phys,
        memory_map_bytes,
        descriptor_size,
    );

    let mut buffer = [0u8; TABLE_READ_BUFFER_LENGTH];
    let length = read_and_verify_table(
        logger,
        reader,
        TableRequest {
            what: width.name(),
            target: sabotage::Target::RootTable,
            phys: root_phys,
            expected_signature: &width.signature(),
            minimum_length: sdt::HEADER_LENGTH as u32,
        },
        &mut buffer,
    )?;

    let entries = sdt::RootEntries::new(&buffer, length as u32, width);
    logger.info(format_args!(
        "acpi: {} lists {} table(s){}",
        width.name(),
        entries.entry_count(),
        if entries.trailing_bytes() == 0 {
            ""
        } else {
            " (with a trailing partial entry, see below)"
        }
    ));
    if entries.trailing_bytes() != 0 {
        logger.warn(format_args!(
            "acpi: {} has {} byte(s) left over after the last whole entry; \
             the declared length and the entry width do not agree",
            width.name(),
            entries.trailing_bytes()
        ));
    }

    let mut madt_phys: Option<PhysAddr> = None;
    let mut madt_count = 0usize;
    // MCFG（PCIe の ECAM）の数（S13-a）。**読むのは数だけで、中身は解釈しない。**
    // PCI の走査（`kernel::pci`）はポート（`0xCF8`/`0xCFC`）を使っており、
    // **その前提「i440FX に ECAM は無い」が崩れたらこの判定行で見える。**
    let mut mcfg_count = 0usize;
    let mut header_buffer = [0u8; sdt::HEADER_LENGTH];
    for (index, raw) in entries.enumerate() {
        let Some(phys) = checked_phys(logger, "a root table entry", raw) else {
            continue;
        };
        if let Err(e) = reader.read(phys, &mut header_buffer) {
            logger.error(format_args!(
                "acpi:   [{index}] {:#x}: header not readable",
                phys.as_u64()
            ));
            report_read_error(logger, "a table listed by the root table", e);
            continue;
        }
        match sdt::parse_header(&header_buffer, sdt::HEADER_LENGTH as u32) {
            Ok(header) => {
                logger.info(format_args!(
                    "acpi:   [{index}] {:#x} signature={:?} length={}",
                    phys.as_u64(),
                    as_text(&header.signature),
                    header.length
                ));
                if header.has_signature(&madt::SIGNATURE) {
                    madt_count += 1;
                    // **最初のものを採る。複数あったことは下で報告する。**
                    if madt_phys.is_none() {
                        madt_phys = Some(phys);
                    }
                }
                if header.has_signature(b"MCFG") {
                    mcfg_count += 1;
                }
            }
            Err(e) => logger.error(format_args!(
                "acpi:   [{index}] {:#x}: the header is not usable: {e:?}",
                phys.as_u64()
            )),
        }
    }

    // **PCI の走査が置いた前提の判定行である（S13-a）。** 0 でなくなったら、
    // ポート経由の構成空間アクセスという選択に判断が生まれる（設計を見直す）。
    logger.info(format_args!(
        "acpi: MCFG tables: {mcfg_count} (0 = no ECAM; the PCI scan uses ports 0xCF8/0xCFC)"
    ));

    // **黙って 1 つ目を使わない。** 同じ署名の表が複数あるのは想定外であり、
    // どちらを読むかで結論が変わりうる。
    if madt_count > 1 {
        logger.warn(format_args!(
            "acpi: the root table lists {madt_count} tables with the signature {:?}; \
             using the first one at {:#x} and ignoring the rest",
            as_text(&madt::SIGNATURE),
            madt_phys.map(|p| p.as_u64()).unwrap_or(0)
        ));
    }
    if madt_phys.is_none() {
        logger.error(format_args!(
            "acpi: no table with the signature {:?} (MADT) is listed; \
             S2 and S3 will have no APIC information",
            as_text(&madt::SIGNATURE)
        ));
    }
    madt_phys
}

/// MADT を検証して列挙する。
fn walk_madt(
    logger: &mut Logger<SerialPort>,
    reader: &PhysReader,
    madt_phys: PhysAddr,
    memory_map_bytes: &[u8],
    descriptor_size: u64,
) -> ApicMmio {
    let mut mmio = ApicMmio::empty();

    report_memory_type(
        logger,
        "the MADT",
        madt_phys,
        memory_map_bytes,
        descriptor_size,
    );

    let mut buffer = [0u8; TABLE_READ_BUFFER_LENGTH];
    let Some(length) = read_and_verify_table(
        logger,
        reader,
        TableRequest {
            what: "the MADT",
            target: sabotage::Target::Madt,
            phys: madt_phys,
            expected_signature: &madt::SIGNATURE,
            minimum_length: madt::FIXED_LENGTH as u32,
        },
        &mut buffer,
    ) else {
        return mmio;
    };

    let fixed = match madt::parse_header(&buffer[..length]) {
        Ok(fixed) => fixed,
        Err(e) => {
            logger.error(format_args!(
                "acpi: the fixed part of the MADT is not usable: {e:?}"
            ));
            return mmio;
        }
    };
    // PCAT_COMPAT は S1 では解釈しない。S2 が「PIC をマスクする必要があるか」を
    // 決める入力になるので、値として記録しておく。
    logger.info(format_args!(
        "acpi: MADT fixed part: local_apic_address={:#x} flags={:#x} (PCAT_COMPAT={})",
        fixed.local_apic_address,
        fixed.flags,
        fixed.pcat_compat()
    ));
    // 固定部の値をまず採る。type 5（Address Override）があれば、下の走査で
    // 上書きされる。**32 ビット幅なので `PhysAddr` に必ず収まる。**
    mmio.local_apic = PhysAddr::new(fixed.local_apic_address as u64);

    let mut entry_count = 0usize;
    let mut local_apic_count = 0usize;
    let mut usable_local_apic_count = 0usize;
    let mut io_apic_count = 0usize;
    let mut interrupt_source_override_count = 0usize;
    let mut stopped_early = false;

    for step in madt::Entries::new(&buffer[madt::FIXED_LENGTH..length]) {
        let entry = match step {
            Ok(entry) => entry,
            Err(e) => {
                // 壊れたエントリの先は位置が同期していないので読み進めない。
                logger.error(format_args!(
                    "acpi: the MADT entry walk stopped after {entry_count} entr(y/ies): {e:?}"
                ));
                stopped_early = true;
                break;
            }
        };
        entry_count += 1;
        match entry.entry_type {
            madt::TYPE_LOCAL_APIC => {
                if let Some(local) = madt::parse_local_apic(&entry) {
                    local_apic_count += 1;
                    if local.usable() {
                        usable_local_apic_count += 1;
                        if mmio.bsp_candidate_apic_id.is_none() {
                            mmio.bsp_candidate_apic_id = Some(local.apic_id);
                        }
                        match mmio.local_apic_ids.iter_mut().find(|slot| slot.is_none()) {
                            Some(slot) => *slot = Some(local.apic_id),
                            None => mmio.local_apic_ids_dropped += 1,
                        }
                    }
                    logger.info(format_args!(
                        "acpi:   entry type={} ({}) len={} apic_id={} processor_uid={} \
                         flags={:#x} usable={}",
                        entry.entry_type,
                        madt::entry_type_name(entry.entry_type),
                        entry.length,
                        local.apic_id,
                        local.processor_uid,
                        local.flags,
                        local.usable()
                    ));
                }
            }
            madt::TYPE_LOCAL_X2APIC => {
                if let Some(x2) = madt::parse_local_x2apic(&entry) {
                    local_apic_count += 1;
                    if x2.flags & (madt::FLAG_ENABLED | madt::FLAG_ONLINE_CAPABLE) != 0 {
                        usable_local_apic_count += 1;
                    }
                    logger.info(format_args!(
                        "acpi:   entry type={} ({}) len={} x2apic_id={} processor_uid={} \
                         flags={:#x}",
                        entry.entry_type,
                        madt::entry_type_name(entry.entry_type),
                        entry.length,
                        x2.apic_id,
                        x2.processor_uid,
                        x2.flags
                    ));
                }
            }
            madt::TYPE_IO_APIC => {
                if let Some(io) = madt::parse_io_apic(&entry) {
                    io_apic_count += 1;
                    // **上限を超えた分は捨てるが、数えてある**（`io_apics_dropped`）。
                    // アドレスは 32 ビット幅なので `PhysAddr` に必ず収まる。
                    if let Some(slot) = mmio.io_apics.get_mut(io_apic_count - 1) {
                        *slot = PhysAddr::new(io.address as u64).map(|phys| IoApicLocation {
                            id: io.id,
                            phys,
                            global_system_interrupt_base: io.global_system_interrupt_base,
                        });
                    }
                    logger.info(format_args!(
                        "acpi:   entry type={} ({}) len={} id={} address={:#x} gsi_base={}",
                        entry.entry_type,
                        madt::entry_type_name(entry.entry_type),
                        entry.length,
                        io.id,
                        io.address,
                        io.global_system_interrupt_base
                    ));
                }
            }
            madt::TYPE_INTERRUPT_SOURCE_OVERRIDE => {
                // **S2-d-1 の入力である。** レガシー IRQ が IO-APIC のどの GSI へ
                // 現れるかを述べる表で、S1 では「あった」ことだけを記録していた。
                // 配送を切り替える前に中身を読む必要があるので、ここで出す。
                if let Some(iso) = madt::parse_interrupt_source_override(&entry) {
                    interrupt_source_override_count += 1;
                    if let Some(slot) = mmio
                        .interrupt_source_overrides
                        .get_mut(interrupt_source_override_count - 1)
                    {
                        *slot = Some(iso);
                    }
                    logger.info(format_args!(
                        "acpi:   entry type={} ({}) len={} bus={} source_irq={} gsi={} \
                         flags={:#06x} (active_low={} level_triggered={})",
                        entry.entry_type,
                        madt::entry_type_name(entry.entry_type),
                        entry.length,
                        iso.bus,
                        iso.source,
                        iso.global_system_interrupt,
                        iso.flags,
                        iso.active_low(),
                        iso.level_triggered()
                    ));
                }
            }
            madt::TYPE_LOCAL_APIC_ADDRESS_OVERRIDE => {
                // **あれば固定部の 32 ビット値より優先される。** S2 が使う。
                if let Some(address) = madt::parse_local_apic_address_override(&entry) {
                    // **固定部の値を置き換える。** 64 ビット幅なので、表せない
                    // 値が来たら `checked_phys` が報告して `None` にする。その
                    // 場合は写像すべき所在が無いという結論になり、固定部の値へ
                    // 戻さない（壊れた表の一部だけを信じる形を作らない）。
                    mmio.local_apic =
                        checked_phys(logger, "the MADT Local APIC Address Override", address);
                    logger.info(format_args!(
                        "acpi:   entry type={} ({}) len={} address={address:#x} \
                         (overrides the fixed part's local_apic_address)",
                        entry.entry_type,
                        madt::entry_type_name(entry.entry_type),
                        entry.length
                    ));
                }
            }
            // 残りは種別と長さだけを出す。**未知の種別を黙って読み飛ばさない。**
            // 名前の付いた種別（type 4 の Local APIC NMI など）もここへ来る。
            // S1 は解釈せず、あったことだけを記録する。
            _ => logger.info(format_args!(
                "acpi:   entry type={} ({}) len={} (recorded, not interpreted)",
                entry.entry_type,
                madt::entry_type_name(entry.entry_type),
                entry.length
            )),
        }
    }

    if stopped_early {
        // **完了行を出さない。** 破壊確認はこの行が出ないことを見る。
        //
        // **所在も返さない。** 走査が途中で止まったということは、後続の
        // エントリを読めていないということである。Local APIC Address Override
        // （type 5）が未読の位置にあれば、固定部の値は誤りになる。**部分的に
        // 読めた表から一部だけを信じない。**
        logger.error(format_args!(
            "acpi: the MADT was not fully enumerated; the APIC inventory is incomplete"
        ));
        return ApicMmio::empty();
    }

    mmio.io_apics_found = io_apic_count;
    mmio.interrupt_source_overrides_found = interrupt_source_override_count;

    // **表が読めていることの実行時の証拠。** 配送経路はまだこの解決を使わないので
    // （キーボードの IRQ1 には上書きが無く、タイマは Local APIC タイマへ移る）、
    // ここで解決結果そのものを出しておく。**非恒等の枝が実在することが見える。**
    if interrupt_source_override_count > 0 {
        logger.info(format_args!(
            "acpi: GSI resolution: irq0 -> gsi{} irq1 -> gsi{} (identity unless an override \
             names the irq; the delivery paths of S2-d use irq1 and the local APIC timer, \
             so neither consumes a non-identity mapping)",
            mmio.gsi_for_irq(0),
            mmio.gsi_for_irq(1)
        ));
    }

    logger.info(format_args!(
        "acpi: MADT enumeration complete: {entry_count} entr(y/ies), \
         {local_apic_count} local APIC(s) of which {usable_local_apic_count} usable, \
         {io_apic_count} I/O APIC(s), \
         {interrupt_source_override_count} interrupt source override(s)"
    ));
    mmio.usable_local_apics = usable_local_apic_count;

    if mmio.io_apics_dropped() > 0 {
        logger.warn(format_args!(
            "acpi: only {MAX_IO_APICS} I/O APIC(s) were recorded; {} more were found and \
             dropped, so the mapping below does not cover them",
            mmio.io_apics_dropped()
        ));
    }

    mmio
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
