//! `syscall-test`: ABI の契約をユーザー側から確かめるプログラム（S9-b-3-2a）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が
//! `rustc` を 1 回呼んで単独でリンクし、できた ELF を kernel が `include_bytes!`
//! で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 確かめているのは dispatch ではない
//!
//! カーネルには既に往復の検証がある（`verify_syscall_roundtrip`）。**あちらは
//! ユーザールーチンの機械語をカーネルが Rust で組み立てている**——オペコードを
//! 直接並べ、即値を `to_le_bytes` で埋める。**つまり ABI の符号化はカーネルの
//! 著者の手にある。**
//!
//! こちらは asm で `mov rdi, ...` と書き、**アセンブラが符号化し、リンカが配置し、
//! ローダーが写像し、`iretq` が飛ばす。** 確かめているのは dispatch ではなく、
//! **`ADR-0020` が決めた ABI の契約そのものが、その経路を通っても成り立つこと**
//! である。
//!
//! # 二重に見る
//!
//! - **カーネル側**——`dispatch` が記録した番号と 6 引数を、既知値と突き合わせる。
//!   **第 4 引数が R10 から来ていることが `ADR-0020` の要である。**
//! - **ユーザー側**——戻り値を自分で検算し、結果を `exit` の終了状態で返す。
//!   **終了状態の一致は S9-b-3-1 で判定行になっており、反証も置いてある**ので、
//!   新しい観測路を作らずに済む。
//!
//! # 終了状態の意味
//!
//! **値に意味がある。** どの検算が落ちたかは、この値でしか分からない。
//! **カーネル側の `SYSCALL_TEST_STATUS` と対になっている。**
//!
//! - `0` すべて通った
//! - `1` probe の戻り値が `PROBE_RETURN` でなかった
//! - `2` `write` が渡したバイト数を返さなかった
//! - `3` 実装していない番号が `-ENOSYS` を返さなかった
//! - `4` `open("/etc/motd", O_RDONLY)` が 0 番を返さなかった
//! - `5` `close(0)` が 0 を返さなかった
//! - `6` 閉じた直後の `open` が 0 番を返さなかった（枠が空いていない）
//! - `7` `open("/nope")` が `-ENOENT` を返さなかった
//! - `8` `open("/etc/motd", O_WRONLY)` が `-EROFS` を返さなかった
//! - `9` `close` の 2 度目が `-EBADF` を返さなかった
//! - `10` `open(NULL)` が `-EFAULT` を返さなかった
//!
//! # `open` はこのプログラムの `.rodata` のパスを渡す
//!
//! **カーネルが受け取るのはユーザー空間のポインタである。** `UserSlice` と
//! 窓（`S9-b-3-2b` で 1 つに畳んだもの）がそのまま効くことを、**この経路が
//! 実際に通ることで確かめている。**
//!
//! # 定数はカーネルの写しである
//!
//! このファイルは crate に属さないので、`kernel/src/syscall.rs` の定数を
//! `use` できない。**同じ値を 2 か所で持つが、食い違えば判定行が落ちる**ので
//! 静かには残らない（`hello.rs` の `.org` と `HELLO_UD2_OFFSET` の関係と同じ）。

#![no_std]
#![no_main]

/// 検証用 probe の番号（`ZAYTOS_PRIVATE_BASE`）。
const PROBE_NUMBER: u32 = 0x1000;
/// probe が返す既知の値。
const PROBE_RETURN: u32 = 0x00C0_FFEE;
/// probe へ渡す 6 引数。**レジスタごとに区別できる値である。**
const PROBE_ARG0: u32 = 0x1111_1111;
const PROBE_ARG1: u32 = 0x2222_2222;
const PROBE_ARG2: u32 = 0x3333_3333;
const PROBE_ARG3: u32 = 0x4444_4444;
const PROBE_ARG4: u32 = 0x5555_5555;
const PROBE_ARG5: u32 = 0x6666_6666;
/// RCX へ入れる番兵。**RCX は引数ではない**（`ADR-0020`。第 4 引数は R10）。
const SENTINEL_RCX: u32 = 0xCCCC_CCCC;
/// `write` の番号（Linux と同じ 1）。
const SYS_WRITE: u32 = 1;
/// `exit` の番号（Linux と同じ 60）。
const SYS_EXIT: u32 = 60;
/// **永久に実装しない番号。** `-ENOSYS` が返ることを確かめるための的である。
const NEVER_IMPLEMENTED: u32 = 0x10FF;
/// `-ENOSYS`。失敗は `-errno` で返る（`ADR-0020`）。
const MINUS_ENOSYS: i32 = -38;
/// 送るバイト列の長さ。
const MESSAGE_LEN: u32 = 24;
/// `open` の番号（Linux と同じ 2）。
const SYS_OPEN: u32 = 2;
/// `close` の番号（Linux と同じ 3）。
const SYS_CLOSE: u32 = 3;
/// 読み取りで開く（`O_RDONLY`）。
const O_RDONLY: u32 = 0;
/// 書き込みで開く（`O_WRONLY`）。**読み取り専用なので拒まれるはずである。**
const O_WRONLY: u32 = 1;
/// `-ENOENT`（そのパスは無い）。
const MINUS_ENOENT: i32 = -2;
/// `-EBADF`（そのファイルディスクリプタは開いていない）。
const MINUS_EBADF: i32 = -9;
/// `-EROFS`（読み取り専用のファイルシステム）。
const MINUS_EROFS: i32 = -30;
/// `-EFAULT`（不正なアドレス）。
const MINUS_EFAULT: i32 = -14;

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // --- 1. probe。6 引数を規約どおりのレジスタへ置く ---
    "  mov edi, {arg0}",
    "  mov esi, {arg1}",
    "  mov edx, {arg2}",
    "  mov r10d, {arg3}",
    "  mov r8d, {arg4}",
    "  mov r9d, {arg5}",
    // **RCX は引数ではない。** 番兵を置くので、カーネルが第 4 引数を RCX から
    // 読んでいれば、記録される値が食い違う。
    "  mov ecx, {sentinel}",
    "  mov eax, {probe}",
    "  int 0x80",
    "  cmp rax, {probe_ret}",
    "  mov edi, 1",
    "  jne 9f",

    // --- 2. write。渡したバイト数が返るはず ---
    "  mov eax, {sys_write}",
    "  mov edi, 1",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {msg_len}",
    "  int 0x80",
    "  cmp rax, {msg_len}",
    "  mov edi, 2",
    "  jne 9f",

    // --- 3. 実装していない番号。-ENOSYS が返るはず ---
    "  mov eax, {never}",
    "  int 0x80",
    "  cmp rax, {minus_enosys}",
    "  mov edi, 3",
    "  jne 9f",

    // --- 4. open("/etc/motd", O_RDONLY)。最初の fd は 0 のはず ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 4",
    "  jne 9f",

    // --- 5. close(0)。0 が返るはず ---
    "  mov eax, {sys_close}",
    "  xor edi, edi",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 5",
    "  jne 9f",

    // --- 6. もう一度 open。**閉じた枠が空いているので、また 0 のはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 6",
    "  jne 9f",

    // --- 7. 無いパス。-ENOENT が返るはず ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MISSING_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_enoent}",
    "  mov edi, 7",
    "  jne 9f",

    // --- 8. 書き込みで開く。**読み取り専用なので -EROFS のはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_wronly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_erofs}",
    "  mov edi, 8",
    "  jne 9f",

    // --- 9. 同じ fd を 2 度閉じる。**2 度目は -EBADF のはず** ---
    "  mov eax, {sys_close}",
    "  xor edi, edi",
    "  int 0x80",
    "  mov eax, {sys_close}",
    "  xor edi, edi",
    "  int 0x80",
    "  cmp rax, {minus_ebadf}",
    "  mov edi, 9",
    "  jne 9f",

    // --- 10. パスに NULL を渡す。**窓の下端より下なので -EFAULT のはず** ---
    // **`open` がユーザーポインタを検証していることの、否定側の観測である。**
    "  mov eax, {sys_open}",
    "  xor edi, edi",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_efault}",
    "  mov edi, 10",
    "  jne 9f",

    // すべて通った。
    "  xor edi, edi",

    // --- 4. exit(status)。ここから戻らない ---
    "9:",
    "  mov eax, {sys_exit}",
    "  int 0x80",
    // **`exit` が戻ってきたときの受け皿**（`hello.rs` と同じ規律）。位置を
    // `.org` で固定してあるので、ここへ落ちたことが RIP で分かる。
    ".org 0x200, 0x90",
    "  ud2",

    ".section .rodata",
    "MESSAGE:",
    "  .ascii \"syscall-test wrote this\\n\"",
    "MOTD_PATH:",
    "  .asciz \"/etc/motd\"",
    "MISSING_PATH:",
    "  .asciz \"/nope\"",

    arg0 = const PROBE_ARG0,
    arg1 = const PROBE_ARG1,
    arg2 = const PROBE_ARG2,
    arg3 = const PROBE_ARG3,
    arg4 = const PROBE_ARG4,
    arg5 = const PROBE_ARG5,
    sentinel = const SENTINEL_RCX,
    probe = const PROBE_NUMBER,
    probe_ret = const PROBE_RETURN,
    sys_write = const SYS_WRITE,
    msg_len = const MESSAGE_LEN,
    never = const NEVER_IMPLEMENTED,
    minus_enosys = const MINUS_ENOSYS,
    sys_exit = const SYS_EXIT,
    sys_open = const SYS_OPEN,
    sys_close = const SYS_CLOSE,
    o_rdonly = const O_RDONLY,
    o_wronly = const O_WRONLY,
    minus_enoent = const MINUS_ENOENT,
    minus_ebadf = const MINUS_EBADF,
    minus_erofs = const MINUS_EROFS,
    minus_efault = const MINUS_EFAULT,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
