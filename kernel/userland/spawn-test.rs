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
//! # 終了状態の意味
//!
//! **カーネル側の `SPAWN_TEST_STATUS` と対になっている。**
//!
//! - `0` 孫の `spawn` が `-EAGAIN` で断られた
//! - `1` 断られなかった、または別の値で断られた
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

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // 孫を起こそうとする。**深さの上限に達しているので断られるはず。**
    "  mov eax, {sys_spawn}",
    "  lea rdi, [rip + HELLO_PATH]",
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

    sys_spawn = const SYS_SPAWN,
    sys_exit = const SYS_EXIT,
    minus_eagain = const MINUS_EAGAIN,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
