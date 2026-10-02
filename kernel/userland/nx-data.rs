//! `nx-data`: Ring 3 で、実行できないページへ跳んで終了させられるユーザープログラム（2026-10-03）。
//!
//! # crate ではない
//!
//! `fault-test.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独で
//! リンクし、できた ELF を kernel が `include_bytes!` で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何をするか
//!
//! **自分の `.data` の先頭へ跳ぶ。**`.data` は、書けて実行しない区画である。**** ローダーは、実行しない権限のページに実行禁止の印を付けて写すので、Ring 3 が
//! そこから命令を取り出そうとした時点で #PF になる。`exit` は呼ばない。**このプロセスは、例外による終了処理で終わる。**
//!
//! カーネルの側は、ベクタ（14）・止まった番地・CR2・誤りコード（存在・ユーザー・命令の取り出し＝0x15）を、
//! 決まった値と突き合わせる（`kernel/src/main.rs` の `USER_PROGRAMS`）。**止まった番地と CR2 は、跳んだ先の番地で
//! ある**——命令の取り出しの違反は、取り出そうとした番地で起きる。
//!
//! # 実行できてしまったときの行き先
//!
//! **跳んだ先には `ud2` を置いてある。** 実行禁止が効いていなければ、そこで #UD になり、ベクタが 14 ではなく 6 に
//! なる。カーネルの側の判定行が、食い違いとして止める。**破壊が成功した後の行き先を、破壊と一緒に用意する。**

#![no_std]
#![no_main]

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。entry と最初の `PT_LOAD` の先頭を一致させない。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // 跳ぶ先は、`.data` の先頭。**`kernel/userland/user.ld` の並び（`.text` の次のページ）と対になっている**
    // ——このプログラムは `.rodata` を持たないので、`.data` は `0x401000` に来る。
    "  lea rax, [rip + NX_TARGET]",
    "  jmp rax",

    // 実行できてしまったときの受け皿（`ud2`）。
    ".section .data,\"aw\"",
    "NX_TARGET:",
    "  ud2",
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
