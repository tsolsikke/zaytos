//! システムコール（`int 0x80`）の入口（M5-f-1）。
//!
//! ADR-0020 のとおり、レジスタ規約は Linux x86-64 に合わせる。番号は RAX、
//! 戻り値は RAX、第 1〜6 引数は RDI/RSI/RDX/R10/R8/R9、失敗は `-errno`
//! （`-1..-4095`）。**第 4 引数は RCX ではなく R10** である。`int 0x80` の間は
//! RCX/R11 は実際には保存されるが、`syscall`/`sysret` へ移る段でこれらは命令が
//! 破壊するため、**保存に依存しない**（ユーザー側ラッパはクロバー扱いにする）。
//!
//! # 入口の機構
//!
//! ベクタ 0x80 の IDT ゲートを DPL=3 の割り込みゲートにし、[`crate::idt`] の
//! `zaytos_syscall_stub` へ向ける。スタブは IRQ スタイルの復元経路を写した
//! `zaytos_syscall_common` へ jmp し、GPR 15 本を退避して [`syscall_entry`] を
//! 呼ぶ。Ring 3 からの `int 0x80` は特権変化（3→0）なので、CPU が TSS.RSP0 の
//! スタックへ自動で切り替える（M5-c/d で更新している RSP0 がここで効く）。
//!
//! `irq_entry` とは経路を分けてある。本番 IRQ 経路へ「ソフトウェア割り込みか」の
//! 分岐を足さない方針（ADR-0018 Addendum 3）と揃え、戻り値の RAX 書き戻しという
//! syscall 固有の振る舞いを IRQ 側へ持ち込まないためである。
//!
//! # M5-f-1 の範囲
//!
//! この段は**空ディスパッチャ**である。番号を読み取って記録し、未知番号として
//! `-ENOSYS` を返す。6 引数の取り出しとユーザーポインタ検証は後段（M5-f-2）で
//! 足す。この段で確かめるのは「Ring 3 から `int 0x80` が届き、`syscall_entry` が
//! RSP0 スタックで走り、`iretq` で Ring 3 へ戻る」往復の成立である。

use core::sync::atomic::{AtomicU64, Ordering};

use crate::idt::context::IrqContext;

/// `-ENOSYS`（未実装システムコール）の errno。失敗は `-errno` で返す。
pub const ENOSYS: i64 = 38;

/// M5-f-1 の往復検証でユーザールーチンが積む番号。空ディスパッチャなので未知番号
/// として扱われ `-ENOSYS` が返るが、`syscall_entry` が記録した番号がこれと一致する
/// ことで、RAX（番号）が規約どおり届いたことを実証する。
pub const PROBE_NUMBER: u64 = 0x2A;

/// `syscall_entry` が呼ばれた回数（会計用）。
static INVOCATION_COUNT: AtomicU64 = AtomicU64::new(0);
/// 直近に受け取った番号（RAX）。往復検証で PROBE_NUMBER と突き合わせる。
static LAST_NUMBER: AtomicU64 = AtomicU64::new(0);
/// `syscall_entry` が走ったときの RSP（RSP0 スタックのはず）。読み戻し検証に使う。
static HANDLER_RSP: AtomicU64 = AtomicU64::new(0);

/// 番号を実装へ振り分ける。M5-f-1 は空で、常に `-ENOSYS` を返す。
///
/// 後段で `match number { ... }` に各システムコールを足す。
fn dispatch(number: u64) -> u64 {
    let _ = number;
    // 失敗は -errno（-1..-4095）。ここでは常に未実装。
    (-ENOSYS) as u64
}

/// `zaytos_syscall_common` から `extern "sysv64"` で呼ばれる。**戻る。**
///
/// 番号を読み、ディスパッチし、戻り値を `context.rax` へ書き戻して、復元経路が
/// 使う RSP を返す。M5-f-1 は切り替えないので入場時の `IrqContext` 先頭をそのまま
/// 返す（`irq_entry` の no-switch と同じ）。復元経路が `pop rax` で `context.rax`
/// を復元するので、書き戻した戻り値がユーザーの RAX に入る。
///
/// **出力しない。** 例外・IRQ ハンドラと同じく、ここでは共有状態の更新だけを行う。
/// 観測は畳んで戻った後にカーネルが記録越しに行う。
///
/// # Safety
///
/// `context` はスタブが積んだ有効な [`IrqContext`] を指していること。
/// `rsp_at_call` はスタブが `call` 直前に読んだ RSP であること。
pub(crate) extern "sysv64" fn syscall_entry(context: *mut IrqContext, rsp_at_call: u64) -> u64 {
    // SAFETY: スタブが直前に積んだ有効な IrqContext を指す。読み書きともこの
    // フレームに限る。
    let ctx = unsafe { &mut *context };

    // 既存の境界計算が syscall 経路でも正しいことの裏取り（IRQ と同じ検査）。
    crate::idt::check_stack_alignment(rsp_at_call, "syscall", ctx.vector);

    // 番号は RAX。**書き戻しの前に読む。**
    let number = ctx.rax;

    INVOCATION_COUNT.fetch_add(1, Ordering::SeqCst);
    LAST_NUMBER.store(number, Ordering::SeqCst);
    HANDLER_RSP.store(rsp_at_call, Ordering::SeqCst);

    let ret = dispatch(number);

    // 戻り値を RAX へ書き戻す。復元経路の pop rax がこれをユーザー RAX へ載せる。
    ctx.rax = ret;

    // M5-f-1 は切り替えない。入場時の IrqContext 先頭を返す。
    context as u64
}

/// 会計カウンタを 0 に戻す（往復検証の直前に呼ぶ）。
pub fn reset_counters() {
    INVOCATION_COUNT.store(0, Ordering::SeqCst);
    LAST_NUMBER.store(0, Ordering::SeqCst);
    HANDLER_RSP.store(0, Ordering::SeqCst);
}

/// `syscall_entry` が呼ばれた回数。
pub fn invocation_count() -> u64 {
    INVOCATION_COUNT.load(Ordering::SeqCst)
}

/// 直近に受け取った番号（RAX）。
pub fn last_number() -> u64 {
    LAST_NUMBER.load(Ordering::SeqCst)
}

/// `syscall_entry` が走ったときの RSP。RSP0 スタック範囲との照合に使う。
pub fn handler_rsp() -> u64 {
    HANDLER_RSP.load(Ordering::SeqCst)
}
