//! `nx-brk`: Ring 3 で、`brk` で伸ばしたページへ跳んで終了させられるユーザープログラム（2026-10-03）。
//!
//! # crate ではない
//!
//! `fault-test.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が `rustc` を 1 回呼んで単独で
//! リンクし、できた ELF を kernel が `include_bytes!` で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 何をするか
//!
//! **`brk` でヒープを決まった番地まで伸ばし、伸ばした範囲の最後のページの先頭へ `ud2` を書いて、そこへ跳ぶ。**
//! `brk` のページは、書けて実行しないページである。**`nx-stack`・`nx-data`・`nx-rodata` はローダーが写す経路を
//! 見るが、こちらは稼働中の表へ 1 枚ずつ足す経路（`brk`。`mmap` と同じ入口）を見る。** `exit` は呼ばない。
//! **このプロセスは、例外による終了処理で終わる。**
//!
//! カーネルの側は、ベクタ（14）・止まった番地・CR2・誤りコード（存在・ユーザー・命令の取り出し＝0x15）を、
//! 決まった値と突き合わせる（`kernel/src/main.rs` の `USER_PROGRAMS`）。**止まった番地と CR2 は、跳んだ先の番地で
//! ある**——命令の取り出しの違反は、取り出そうとした番地で起きる。
//!
//! # 伸ばしたまま終わる
//!
//! 縮めずに終わるので、空間ごとの会計が `brk` の伸ばした分を数えていなければ、破棄の会計の誤りが出る
//! （`/bin/ttfglyph` と同じ形。2026-10-03 に直した）。**このプログラムは、その直しが起動時の経路でも効いている
//! ことの確かめを兼ねる。**
//!
//! # 実行できてしまったときの行き先
//!
//! **跳んだ先には `ud2` を置いてある。** 実行禁止が効いていなければ、そこで #UD になり、ベクタが 14 ではなく 6 に
//! なる。カーネルの側の判定行が、食い違いとして止める。**`brk` が断られたときも `ud2` で止まる**——跳ばずに
//! `.text` の中で #UD になるので、止まった番地が跳ぶ先と違い、判定行が食い違いとして止める。

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
    // `brk(new_break)`。**ヒープの下端は像の末尾の次のページ（このプログラムでは `0x401000`）で、
    // そこから `new_break` まで伸びる。** `new_break` は `kernel/src/main.rs` の `NX_BRK_TARGET` の 1 ページ上と
    // 対になっている。
    // システムコールの入口は `int 0x80` である（`kernel/userland/userlib.rs` と同じ。`ADR-0020`）。
    "  mov eax, {sys_brk}",
    "  mov rdi, {new_break}",
    "  int 0x80",
    // 断られた（`-errno`）なら、ここで止まる。
    "  cmp rax, {new_break}",
    "  jne 2f",
    // 跳ぶ先は、伸ばした範囲の最後のページの先頭。実行できてしまったときの受け皿を、跳ぶ先に書く（`brk` のページは書ける）。
    "  mov rax, {target}",
    "  mov word ptr [rax], 0x0b0f",
    "  jmp rax",
    "2:",
    "  ud2",

    sys_brk = const 12u32,
    new_break = const 0x0041_0000u64,
    target = const 0x0040_f000u64,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
