//! AP を INIT と SIPI で起こす所（PC の Local APIC の手順）。
//!
//! **`kernel/src/smp.rs` から移した**（2026-09-28。境界の段階の手順 2）。**INIT と SIPI の順番と待ち、2 回目の SIPI を
//! 送る条件は、PC の Local APIC の手順なので、機械固有の置き場に置く。** **どの AP をいつ起こすかと、起動の署名を
//! 待つ所は `smp` に残る**（起きたかは呼ぶ側が答える）。

/// AP を起動するときの各段の待ちティック数。1 ティック = 10ms（100Hz）。
const AP_WAKE_WAIT_TICKS: u64 = 1;

/// AP を 1 本起こす（INIT → 待つ → SIPI → 待つ → まだ起きていなければもう 1 回 SIPI）。
///
/// # 契約（境界の関数。2026-09-28）
///
/// - `mapped` はマップ済みの Local APIC、`apic_id` は起こす AP の APIC ID である。`sipi_vector` は開始のページの番号で、
///   `sipi_vector << 12` が AP の最初に実行する**物理アドレス**になる（トランポリンを置いた所）。
/// - `wait_ticks` は、渡したティックの数だけ待つ関数である（タイマが動いていること）。`has_started` は、その AP が起動の
///   署名を出したかを答える関数で、2 回目の SIPI を送るかの判断だけに使う（走り出した AP へ SIPI を送らないため）。
/// - 戻り値は、送った IPI がどれも自分の Local APIC を出たか（配送の状態が落ちたか）である。AP が起きたかは答えない
///   （呼ぶ側が起動の署名で見る）。
/// - 呼んでよいのは BSP が、起動の途中の単一の文脈で、AP ごとに 1 回である（ティックで待つので、割り込みは有効）。
/// - 自分の Local APIC から IPI を送るだけで、起こした AP との同期は含まない（呼ぶ側の署名の待ちが受け持つ）。
///
/// # Safety
///
/// `mapped` がマップ済みの Local APIC を指し、`sipi_vector << 12` に実行できるトランポリンが置かれていること。
/// 起動時に、この AP へ 1 回だけ呼ぶこと。
pub unsafe fn start_application_processor(
    mapped: &crate::machine::pc::apic::MappedApic,
    apic_id: u8,
    sipi_vector: u8,
    wait_ticks: fn(u64),
    has_started: impl Fn() -> bool,
) -> bool {
    let lapic_virt = crate::machine::pc::apic::lapic_virt_of(mapped);
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
