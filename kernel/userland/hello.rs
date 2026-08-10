//! `hello`: 埋め込み ELF から Ring 3 で走る最初のユーザープログラム（S9-b-1）。
//!
//! # crate ではない
//!
//! このファイルは cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を
//! 1 回呼んで単独でリンクし、できた ELF を kernel が `include_bytes!` で抱える。
//! cargo を入れ子にしないのは、`--full` が kernel を feature 構成ごとに何十回も
//! 建てるためである（詳細は `build.rs`）。
//!
//! **`cargo fmt` と `clippy` はこのファイルを見ない。** crate に属さないためで
//! ある。対象へ入れるのは、ユーザープログラムが 3 本を超えるとき、または 1 本が
//! 100 行を超えるときとする。
//!
//! # 何をするか
//!
//! `write(1, "...", n)` を `int 0x80` で 1 回発行し、`ud2` で止まる。
//!
//! **`exit` は呼ばない。** プロセスの実体がまだ無く、終了させる先が無い。
//! `ud2` は S8 が作った畳みの機構（`FOLDABLE_VECTORS` にベクタ 6 が在る）に
//! 乗るための出口である。S9-b-3 でプロセスの終了へ置き換わる。

#![no_std]
#![no_main]

core::arch::global_asm!(
    // **entry の手前に詰め物を置く。**
    //
    // 詰め物が無いと、`_start` が最初の `PT_LOAD` の先頭と同じアドレスになる。
    // すると「entry ではなくセグメントの先頭へ飛ぶ」破壊が破壊にならない。
    //
    // **像を破壊のために歪めているのではなく、より一般的な形にしている。**
    // 実際の ELF は `.text` の前に別の節が来るので、entry とセグメントの先頭は
    // 一致しないほうが普通である。極小のプログラムだけが一致する。
    //
    // 中身は `ud2` で埋める。ここへ飛んだ場合は先頭で #UD になり、
    // 正しい entry から走ったときの畳み位置（`int 0x80` の後）と RIP で区別できる。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // write(fd=1, buf=MESSAGE, len)。番号と引数の並びは Linux x86-64（ADR-0020）。
    "  mov rax, 1",
    "  mov rdi, 1",
    "  lea rsi, [rip + MESSAGE]",
    "  mov rdx, {message_len}",
    "  int 0x80",
    // カーネルへ戻る出口。畳みはここで起きる。
    "  ud2",

    ".section .rodata",
    "MESSAGE:",
    "  .ascii \"hello from ring 3\\n\"",
    message_len = const 18,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
