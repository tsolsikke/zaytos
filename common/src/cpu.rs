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

/// RFLAGS レジスタのうち、割り込みフラグ（IF, bit 9）を表すビットマスク。
pub const RFLAGS_INTERRUPT_FLAG: u64 = 1 << 9;

/// 現在の RFLAGS レジスタの値を読み取る。
pub fn read_rflags() -> u64 {
    let rflags: u64;
    // SAFETY: `pushfq` で RFLAGS をスタックへ積み、`pop` で読み出すだけ。
    // スタックを一時的に使うため `nomem`/`nostack` は指定しない。
    unsafe {
        core::arch::asm!("pushfq", "pop {}", out(reg) rflags);
    }
    rflags
}

/// 割り込みを禁止する（`cli`）。呼び出し元がその状態を維持する責任を負う
/// （`sti` で戻す処理はここには含まない。M4 で自前 IDT を導入するまで、
/// kernel は起動直後からこの状態を維持し続ける方針。
/// `docs/architecture.md` §6.5 参照）。
pub fn disable_interrupts() {
    // SAFETY: `cli` はマスク可能割り込みの受付を止めるだけで、メモリ
    // レイアウトや制御フローを変えない。
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
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

/// タイムスタンプカウンタ（TSC）を読む。
///
/// **計測専用。時刻源として使わないこと。** TSC は CPU の起動からの
/// サイクル数を数えるだけのもので、周波数はハイパーバイザや電源管理の
/// 影響を受けうる。時刻が必要になったら、M4 で導入するタイマを使うこと。
///
/// **`rdtsc` は直列化命令ではない。** 前後の命令と実行順序が入れ替わりうる
/// ため、数命令程度の短い区間を測る用途には向かない。数十万サイクル以上の
/// 塊を測る前提で使うこと。厳密な区間計測が必要になった場合は、`lfence` を
/// 併用するか `rdtscp` を検討する。
///
/// **仮想化環境での注意**: 本プロジェクトの開発環境（WSL2 上の QEMU）で
/// KVM を使う場合、WSL2 自体が Hyper-V 上の VM であるため入れ子の仮想化に
/// なる。TCG（純粋エミュレーション）よりは遥かに実機へ近いが、実機その
/// ものではない。計測結果は絶対値ではなく、比較対象との比で判断すること
/// （ADR-0015 Addendum）。
pub fn read_timestamp_counter() -> u64 {
    // SAFETY: `rdtsc` は特権を必要とせず（CR4.TSD が立っていない限り）、
    // メモリにも制御フローにも副作用が無い。EDX:EAX に値を返すだけ。
    unsafe { core::arch::x86_64::_rdtsc() }
}
