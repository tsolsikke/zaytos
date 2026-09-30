//! AP を INIT と SIPI で起こす所（PC の Local APIC の手順）。
//!
//! **`kernel/src/smp.rs` から移した**（2026-09-28。境界の段階の手順 2）。**INIT と SIPI の順番と待ち、2 回目の SIPI を
//! 送る条件は、PC の Local APIC の手順なので、機械固有の置き場に置く。** **どの AP をいつ起こすかと、起動の署名を
//! 待つ所は `smp` に残る**（起きたかは呼ぶ側が答える）。

use core::fmt;
use core::sync::atomic::{AtomicU16, Ordering};

use common::addr::PhysAddr;

/// AP を起動するときの各段の待ちティック数。1 ティック = 10ms（100Hz）。
const AP_WAKE_WAIT_TICKS: u64 = 1;

/// CPU のハードウェアの番号（`ADR-0072` の 1 の B。2026-09-29。境界の段階の手順 2 の 9f）。
///
/// x86 では、ファームウェア（MADT）が示す Local APIC ID である（AArch64 では MPIDR の値になる）。**CPU のスロットの
/// 番号（`cpu_id()`）とは別の番号である**——取り違えを型で防ぐ。番号を読めるのは `machine` の中だけで、外へは
/// 表示（`Display` と `LowerHex`）しか出さない。
///
/// **作るのも `machine` の中だけである**（2026-09-30。MADT の走査と、起動した AP の表と、I/O APIC の宛先の読み戻しが
/// 返す）。**共通の側は番号の数値を持たない**ので、中身を広げるとき（x2APIC・MPIDR）に変わるのは `machine` の中だけ
/// である。
///
/// # 契約（境界の型。2026-09-30）
///
/// - 作るのは `machine` の中だけで、共通の側は比べることと表示にだけ使う（番号の数値は持たない）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProcessorId(u8);

impl ProcessorId {
    /// ファームウェアが示したハードウェアの番号から作る（今は MADT の Local APIC ID。`machine` の中だけで使う）。
    pub(in crate::machine) const fn from_hardware_id(id: u8) -> Self {
        Self(id)
    }

    /// Local APIC ID（`machine` の中だけで使う）。
    pub(in crate::machine) const fn local_apic_id(self) -> u8 {
        self.0
    }
}

impl fmt::Display for ProcessorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::LowerHex for ProcessorId {
    /// `{:#04x}` の形（I/O APIC の宛先の読み戻しの行。以前は番号の数値をそのまま出していた）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

impl Default for ProcessorId {
    /// 番号 0 を返す。MADT に候補が無いときの比べる相手に使う（起動の順序（`main.rs`）の、I/O APIC の宛先の読み戻しの
    /// 確かめ。以前の `unwrap_or(0)`）。番号 0 に特別な意味は無い。
    fn default() -> Self {
        Self(0)
    }
}

/// 起こした AP の数の上限（bootstrap processor を除いた per-CPU のスロットの数）。
const MAX_APS: usize = common::percpu::MAX_CPUS - 1;

/// 起動した AP のハードウェアの番号（S5-a。ファームウェアが示した番号で、今は Local APIC ID）。スロット 1 以降ぶん。
/// `u16` の番兵で「未設定」を表す（番号は `u8` なので `0` も有効な値である）。
///
/// **2026-09-30 に `smp.rs` から移した**——番号の数値を持つのは `machine` の中だけにする（[`ProcessorId`] の doc）。
static STARTED_PROCESSOR_ID: [AtomicU16; MAX_APS] =
    [const { AtomicU16::new(NO_PROCESSOR_ID) }; MAX_APS];

/// 「まだ起こしていない」を表す番兵（S5-a）。
const NO_PROCESSOR_ID: u16 = u16::MAX;

/// スロット `slot`（1 以降）の AP を、番号 `processor` で起こすと控える（S5-a。2026-09-30 に `smp.rs` から移した）。
///
/// # 契約（境界の関数。2026-09-30）
///
/// - 呼ぶのは BSP が、AP を起こす途中の単一の文脈で、スロットごとに 1 回である
///   （`crate::smp::wake_application_processors`）。
/// - `slot` は 1 以上、`MAX_CPUS` 未満である（外れたら止まる。カーネルの誤りである）。
/// - **起こす前に控える。** 起きたかどうかは控えない（呼ぶ側が起動の署名の数で見る）。
pub fn record_started_processor(slot: usize, processor: ProcessorId) {
    STARTED_PROCESSOR_ID[slot - 1].store(u16::from(processor.0), Ordering::SeqCst);
}

/// スロット `slot` で起こした AP のハードウェアの番号を返す（S5-a。型は 2026-09-29 の 9f で `machine` の番号の型に
/// した。表と一緒に 2026-09-30 に `smp.rs` から移した）。控えていなければ `None`。
///
/// # 契約（境界の関数。2026-09-30）
///
/// - どの CPU からも、BKL なしで呼んでよい（表は原子的な値で、控えるのも読むのも SeqCst である）。
/// - `slot` が範囲の外（0 か `MAX_CPUS` 以上）なら `None` を返す。控える前も `None` である。
/// - 返すのは、そのスロットで起こすと控えた番号である。**控えるのは起こす前なので、起こすのに失敗したスロットも
///   番号を返す**（起きた AP の数は `crate::smp::started_ap_count` が答える）。
pub fn started_processor(slot: usize) -> Option<ProcessorId> {
    let raw = STARTED_PROCESSOR_ID
        .get(slot.checked_sub(1)?)?
        .load(Ordering::SeqCst);
    (raw != NO_PROCESSOR_ID).then(|| ProcessorId::from_hardware_id(raw as u8))
}

/// 起こした CPU が最初に実行する場所（9f）。x86 では SIPI のベクタ（開始のページの番号）で、`page << 12` が最初に
/// 実行する物理アドレスである。
///
/// **作るのは、トランポリンを置いた `arch` である**（[`StartAddress::of_page`]）。SIPI で指せるのは 4 KiB の境界で
/// 1 MiB より下のページだけなので、作るときに確かめ、外れたら止まる（トランポリンを置く所の誤りで、カーネルの誤り
/// である）。
///
/// # 契約（境界の型。2026-09-30）
///
/// - 作るのはトランポリンを置いた `arch` で（[`StartAddress::of_page`]）、共通の側（`crate::smp`）は
///   [`start_processor`] へ渡すことと表示にだけ使う。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StartAddress {
    page: u8,
}

impl StartAddress {
    /// SIPI で指せるページの上限（1 MiB）。
    const LIMIT: u64 = 0x10_0000;

    /// 最初に実行する物理アドレス（トランポリンを置いたページの先頭）から作る。
    pub fn of_page(start: PhysAddr) -> Self {
        let address = start.as_u64();
        assert!(
            address.is_multiple_of(4096) && address < Self::LIMIT,
            "a processor starts from a 4 KiB page below 1 MiB, not {address:#x}"
        );
        Self {
            page: (address >> 12) as u8,
        }
    }
}

impl fmt::Display for StartAddress {
    /// `vector 0x01` の形（AP を起こす行。以前は共通の側が SIPI のベクタを受け取って同じ文言を組んでいた）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "vector {:#04x}", self.page)
    }
}

/// AP を 1 本起こす（INIT → 待つ → SIPI → 待つ → まだ起きていなければもう 1 回 SIPI）。
///
/// **直下の名前は機械に依らない名前にした**（2026-09-29。9f。以前は `start_application_processor`）。
///
/// # 契約（境界の関数。2026-09-28。引数の型は 2026-09-29 の 9f で見直した）
///
/// - `mapped` はマップ済みの Local APIC、`processor` は起こす CPU のハードウェアの番号である（[`ProcessorId`]）。
///   `start` は起こした CPU が最初に実行する場所で、トランポリンを置いたページである（[`StartAddress`]）。
/// - `wait_ticks` は、渡したティックの数だけ待つ関数である（タイマが動いていること）。`has_started` は、その AP が起動の
///   署名を出したかを答える関数で、2 回目の SIPI を送るかの判断だけに使う（走り出した AP へ SIPI を送らないため）。
/// - 戻り値は、送った IPI がどれも自分の Local APIC を出たか（配送の状態が落ちたか）である。AP が起きたかは答えない
///   （呼ぶ側が起動の署名で見る）。
/// - 呼んでよいのは BSP が、起動の途中の単一の文脈で、AP ごとに 1 回である（ティックで待つので、割り込みは有効）。
/// - 自分の Local APIC から IPI を送るだけで、起こした AP との同期は含まない（呼ぶ側の署名の待ちが受け持つ）。
///
/// # Safety
///
/// `mapped` がマップ済みの Local APIC を指し、`start` のページに実行できるトランポリンが置かれていること。
/// 起動時に、この AP へ 1 回だけ呼ぶこと。
pub unsafe fn start_processor(
    mapped: &crate::machine::pc::apic::MappedInterruptController,
    processor: ProcessorId,
    start: StartAddress,
    wait_ticks: fn(u64),
    has_started: impl Fn() -> bool,
) -> bool {
    let lapic_virt = crate::machine::pc::apic::lapic_virt_of(mapped);
    let apic_id = processor.local_apic_id();
    let sipi_vector = start.page;
    // INIT → 待つ → SIPI → 待つ → まだ起動していなければもう 1 回 SIPI。
    //
    // 2 回目を無条件に送ってはならない。既に走り出した AP へ SIPI を
    // 送ると、long mode で走っている最中に開始ベクタから再実行させる
    // ことになり、16 ビットのバイト列を 64 ビットとして解釈して #GP →
    // トリプルフォルトする。実際に踏んだ（CPU 1 が CS64・GDTR=0 で
    // オフセット 0x15 に落ちた）。規格が 2 回目を許すのは「1 回目が
    // 届かなかった場合」であって、常に 2 回送れという意味ではない。
    // SAFETY: マップ済みの Local APIC。起動時の 1 回だけ。
    let ok = unsafe {
        crate::machine::pc::apic::send_init_ipi(lapic_virt, apic_id) && {
            wait_ticks(AP_WAKE_WAIT_TICKS);
            crate::machine::pc::apic::send_startup_ipi(lapic_virt, apic_id, sipi_vector)
        }
    };
    wait_ticks(AP_WAKE_WAIT_TICKS);
    ok && (has_started() || {
        // SAFETY: 同上。まだ起動していないときだけ送る。
        unsafe { crate::machine::pc::apic::send_startup_ipi(lapic_virt, apic_id, sipi_vector) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 開始の場所は SIPI のベクタ（ページの番号）として表示され、以前の行と同じ文言になる（9f）。
    #[test]
    fn a_start_address_is_the_page_number_below_1_mib() {
        let start = StartAddress::of_page(PhysAddr::new(0x1000).unwrap());
        assert_eq!(format!("{start}"), "vector 0x01");
        assert_eq!(
            format!("{}", StartAddress::of_page(PhysAddr::new(0x9e000).unwrap())),
            "vector 0x9e"
        );
        assert_eq!(format!("{}", ProcessorId::from_hardware_id(1)), "1");
        assert_eq!(format!("{:#04x}", ProcessorId::from_hardware_id(1)), "0x01");
        assert_eq!(format!("{:#04x}", ProcessorId::default()), "0x00");
    }

    /// 4 KiB の境界に無いページは、SIPI では指せない（作るときに止まる）。
    #[test]
    #[should_panic(expected = "a processor starts from a 4 KiB page below 1 MiB")]
    fn a_start_address_off_a_page_boundary_is_refused() {
        let _ = StartAddress::of_page(PhysAddr::new(0x1800).unwrap());
    }

    /// 1 MiB より上のページは、SIPI では指せない（作るときに止まる）。
    #[test]
    #[should_panic(expected = "a processor starts from a 4 KiB page below 1 MiB")]
    fn a_start_address_above_1_mib_is_refused() {
        let _ = StartAddress::of_page(PhysAddr::new(0x10_0000).unwrap());
    }
}
