//! x86_64 の最小限のプリミティブ。
//!
//! パニックハンドラ（ADR-0004: fail-fast）と正常終了時の停止処理の
//! 両方から使う、共有の低レイヤ操作をここに集約する。

/// 現在のスタックポインタ（RSP）の値を読み取る。
///
/// 呼び出し元自身のスタックポインタを読むだけの操作であり、
/// 呼び出しコンテキストに関わらず常に安全に実行できる。
pub fn read_rsp() -> u64 {
    let rsp: u64;
    // SAFETY: `mov` によるレジスタ読み取りのみで、メモリアクセスや
    // 制御フローの変更を伴わない。
    unsafe {
        core::arch::asm!(
            "mov {}, rsp",
            out(reg) rsp,
            options(nomem, nostack, preserves_flags),
        );
    }
    rsp
}

/// 現在の実行位置に近い命令ポインタ（RIP）の値を読み取る。
///
/// x86_64 には「RIP を汎用レジスタへ読み出す」命令が存在しないため、
/// `lea reg, [rip]` で「次の命令のアドレス」を取得する（呼び出し直後の
/// 命令アドレスに近い値になる）。ページテーブルの検証等、「現在
/// 実行中のコード周辺がマップされているか」を確認する目的には十分な
/// 精度である。
pub fn read_rip() -> u64 {
    let rip: u64;
    // SAFETY: `lea` によるアドレス計算のみで、メモリアクセスや制御フローの
    // 変更を伴わない。
    unsafe {
        core::arch::asm!(
            "lea {}, [rip]",
            out(reg) rip,
            options(nomem, nostack, preserves_flags),
        );
    }
    rip
}

/// 割り込みを禁止し、`hlt` ループで停止し続ける。
///
/// ADR-0004 の fail-fast 方針（パニック時は即停止する）と、M1 の
/// 正常終了時（それ以上進む処理がない状態）の両方で使う停止処理。
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `cli` はマスク可能割り込みを禁止し、`hlt` は次の割り込み
        // までCPUを停止させる。ループで囲むことで、`hlt` が NMI 等で
        // 一時的に起床しても実行を再開させない。
        unsafe {
            core::arch::asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}
