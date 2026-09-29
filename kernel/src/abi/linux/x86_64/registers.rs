//! システムコールのレジスタの形（x86_64 の Linux。`ADR-0020`。`ADR-0071` の決定 1 の 2 で、`crate::syscall` の入口から
//! 分けた。2026-09-30）。
//!
//! 番号は RAX、戻り値は RAX、第 1〜6 引数は RDI/RSI/RDX/R10/R8/R9、失敗は `-errno`
//! （`-1..-4095`）。**第 4 引数は RCX ではなく R10** である。`int 0x80` の間は
//! RCX/R11 は実際には保存されるが、`syscall`/`sysret` へ移る段階でこれらは命令が
//! 破壊するため、**保存に依存しない**（ユーザー側ラッパはクロバー扱いにする）。
//!
//! **入口の文脈から読むのも、書き戻すのもここだけである。** 共通の側（`crate::syscall` の `syscall_entry`）は、読んだ
//! 番号と引数で振り分け、戻り値をここへ渡す。
//!
//! # 破壊テストの feature（M5-f-1-2）
//!
//! - `syscall-test-arg4-rcx`: 第 4 引数を `context.r10` でなく `context.rcx` から
//!   読む。R10 規約の実証（記録した第 4 引数が期待値と食い違う）。
//! - `syscall-test-drop-retval`: 戻り値の `context.rax` 書き戻しを落とす。ユーザーが
//!   期待した戻り値を受け取れない（ユーザースタックへ store した値が食い違う）。

use crate::arch::x86_64::idt::context::IrqContext;

/// システムコールの番号と 6 つの引数（入口の文脈から読んだもの）。
pub struct SyscallRequest {
    pub number: u64,
    pub args: [u64; 6],
}

/// 入口の文脈から、番号と 6 つの引数を読む。**戻り値を書き戻す前に読む**（書き戻すと、番号の RAX が上書きされる）。
pub fn read_request(context: &IrqContext) -> SyscallRequest {
    // 第 4 引数は R10（RCX ではない。ADR-0020）。
    // 破壊テスト (M5-f-1-2, arg4-rcx): 第 4 引数を RCX から読む。記録した第 4 引数が
    // PROBE_ARGS[3] と食い違い、R10 規約であることが実証される。
    #[cfg(not(feature = "syscall-test-arg4-rcx"))]
    let arg3 = context.r10;
    #[cfg(feature = "syscall-test-arg4-rcx")]
    let arg3 = context.rcx;
    SyscallRequest {
        number: context.rax,
        args: [
            context.rdi,
            context.rsi,
            context.rdx,
            arg3,
            context.r8,
            context.r9,
        ],
    }
}

/// 戻り値を文脈の RAX へ書き戻す。復元経路の `pop rax` がこれをユーザーの RAX へ載せる。
pub fn write_return(context: &mut IrqContext, value: u64) {
    // 破壊テスト (M5-f-1-2, drop-retval): 書き戻しを落とす。context.rax は番号のままで、
    // ユーザーは期待した戻り値を受け取れない。
    #[cfg(not(feature = "syscall-test-drop-retval"))]
    {
        context.rax = value;
    }
    #[cfg(feature = "syscall-test-drop-retval")]
    let _ = (context, value);
}
