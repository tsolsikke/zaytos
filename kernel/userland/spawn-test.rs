//! `spawn-test`: 入れ子の上限が効いていることを、子の側から確かめる（S11-5）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が
//! `rustc` を 1 回呼んで単独でリンクする。**`cargo fmt` と `clippy` はこの
//! ファイルを見ない。**
//!
//! # 上から起動しない
//!
//! **このプログラムは `USER_PROGRAMS` に載っていない。** カーネルの直線上から
//! 走らせると深さ 1 になり、**`spawn` が成功してしまうので何も主張しない。**
//!
//! **`syscall-test` が `spawn("/bin/spawn-test")` で起こす。** そのとき深さは 2 で、
//! **`MAX_EXCURSION_DEPTH` に達している。** ここから更に `spawn` を呼べば、
//! **孫は断られなければならない。**
//!
//! # 断られ方まで見る
//!
//! **`-EAGAIN` であることを確かめる。** 「何かの失敗が返った」では足りない——
//! **`-ENOSYS` なら「その番号は無い」、`-EAGAIN` なら「その番号は在るが、今は
//! 受け付けられない」である。** 上限が効いていることを主張しているのは後者だけで、
//! **破壊 `spawn-eagain-as-enosys` がその差を突く。**
//!
//! # 引数も検算する（S11-7）
//!
//! **親が `spawn` へ渡した `argv` が、そのまま届いていることを見る。**
//! **入口の `rsp` から `argc` と `argv[]` を読む**（`syscall-test` と同じ形。
//! カーネルが Linux と同じ形で積んでいる）。
//!
//! **`spawn` で起こされたプロセスとして見るのはここが初めてである**——
//! `syscall-test` は起動シーケンスの直線から起こされており、`argv` はカーネルの
//! 表（`USER_PROGRAMS`）が渡している。**こちらは Ring 3 から来た `argv` である。**
//!
//! # 終了状態の意味
//!
//! **意味の表はここにしか無い。** このプログラムは `USER_PROGRAMS` に載っていない
//! ので、カーネル側に対になる表が無い。**終了状態は `spawn` の判定行に
//! `Exited(N)` として出る**ので、値からここを引く。
//! **0 以外で終われば、親（`syscall-test`）の検算 40 番が落ちる。**
//!
//! - `0` 孫の `spawn` が `-EAGAIN` で断られた
//! - `1` 断られなかった、または別の値で断られた
//! - `2` `argc` が 2 でなかった
//! - `3` `argv[0]` が "spawn-test" でなかった
//! - `4` `argv[1]` が "beta" でなかった
//! - `5` `argv[2]`（終端）が NULL でなかった
//!
//! # 定数はカーネルの写しである
//!
//! このファイルは crate に属さないので、`kernel/src/syscall.rs` の定数を
//! 参照できない。**食い違えば検算が落ちる**ので、静かには残らない。

#![no_std]
#![no_main]

/// `spawn` の番号（`ZAYTOS_PRIVATE_BASE + 4`）。
const SYS_SPAWN: u32 = 0x1004;
/// `exit` の番号（Linux と同じ 60）。
const SYS_EXIT: u32 = 60;
/// `-EAGAIN`（今は受け付けられない）。**上限に達したときの答えである。**
const MINUS_EAGAIN: i32 = -11;
/// 期待する `argc`。**親（`syscall-test`）が渡す `argv` と対になっている。**
const EXPECTED_ARGC: u32 = 2;
/// `argv[0]` の長さ（NUL を含む）。
const ARGV0_LEN: u32 = 11;
/// `argv[1]` の長さ（NUL を含む）。
const ARGV1_LEN: u32 = 5;

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // --- 2..5. 親が渡した argv が届いていること ---
    // **入口の rsp をそのまま使う。** ここより前で何も push していない。
    "  mov rax, [rsp]",
    "  cmp rax, {argc}",
    "  mov edi, 2",
    "  jne 9f",
    // argv[0] を突き合わせる。
    "  mov rsi, [rsp + 8]",
    "  lea rdi, [rip + ARGV0_TEXT]",
    "  mov ecx, {argv0_len}",
    "  repe cmpsb",
    "  mov edi, 3",
    "  jne 9f",
    // argv[1] を突き合わせる。
    "  mov rsi, [rsp + 16]",
    "  lea rdi, [rip + ARGV1_TEXT]",
    "  mov ecx, {argv1_len}",
    "  repe cmpsb",
    "  mov edi, 4",
    "  jne 9f",
    // argv の終端。
    "  mov rax, [rsp + 24]",
    "  test rax, rax",
    "  mov edi, 5",
    "  jne 9f",

    // 孫を起こそうとする。**深さの上限に達しているので断られるはず。**
    //
    // **`envp` を明示的に置く（f-2。`ADR-0053` の Decision 2）。**
    // **RDX に意味ができた**——**置かないと、入口の値がそのまま `envp` として
    // 読まれ、`-EAGAIN` ではなく `-EFAULT` が返りうる**（写しは深さの判定より
    // 前に走る）。**たまたま 0 でも置く**——**偶然に頼った捕捉は捕捉ではない。**
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + HELLO_PATH]",
    "  lea rsi, [rip + ARGV_HELLO]",
    "  lea rdx, [rip + ENVP_EMPTY]",
    "  int 0x80",
    "  cmp rax, {minus_eagain}",
    "  mov edi, 1",
    "  jne 9f",
    "  xor edi, edi",

    // exit(status)。ここから戻らない。
    "9:",
    "  mov eax, {sys_exit}",
    "  int 0x80",
    // **`exit` が戻ってきたときの受け皿**（`hello.rs` と同じ規律）。
    // **位置はリンカが決める**（`user.ld` の `USER_RECEIVER_OFFSET`）。
    ".section .userland.receiver,\"ax\"",
    "  ud2",

    ".section .rodata",
    "HELLO_PATH:",
    "  .asciz \"/bin/hello\"",
    // **孫へ渡す `argv`。** 断られるので届かないが、**入口の形は同じにする。**
    ".balign 8",
    "ARGV_HELLO:",
    "  .quad HELLO_ARG0",
    "  .quad 0",
    // **空の `envp`。** **「環境が無い」は空の配列で表す**（`argv` と同じ規則。
    // NULL は `-EFAULT` である）。
    ".balign 8",
    "ENVP_EMPTY:",
    "  .quad 0",
    "HELLO_ARG0:",
    "  .asciz \"hello\"",
    // **親が渡した `argv` の写し。** 食い違えば 3 番か 4 番が落ちる。
    "ARGV0_TEXT:",
    "  .asciz \"spawn-test\"",
    "ARGV1_TEXT:",
    "  .asciz \"beta\"",

    sys_spawn = const SYS_SPAWN,
    sys_exit = const SYS_EXIT,
    minus_eagain = const MINUS_EAGAIN,
    argc = const EXPECTED_ARGC,
    argv0_len = const ARGV0_LEN,
    argv1_len = const ARGV1_LEN,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
