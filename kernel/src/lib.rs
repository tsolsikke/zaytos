//! ZaytOS kernel の共有ロジック。
//!
//! ハードウェア依存部（`main.rs` の `_start`/`panic.rs`）と純粋ロジック
//! （[`memory_map`], [`frame_allocator`]）を分離し、後者はホスト上の
//! `cargo test` で検証する。M2-c の物理フレーム
//! アロケータは「間違えると無言で壊れる」領域であるため、特に手厚く
//! テストする。

#![cfg_attr(not(test), no_std)]

// `alt-offset-test` は PIC を 0x30-0x3F へ再マップするため、IRQ0 のベクタが
// 0x30 になる。それは GPR 復元テストが使うテスト専用ベクタと同じ番号であり、
// 同時に有効にすると「タイマなのかテスト用の int なのか」が区別できなくなる。
// テスト用ベクタを別の番号へ逃がす案もあったが、feature によってテストの
// ベクタ番号が変わるとログを読むときの混乱要因になるため、排他にしている。
#[cfg(all(feature = "interrupt-test-irq-path", feature = "alt-offset-test"))]
compile_error!(
    "interrupt-test-irq-path and alt-offset-test cannot be enabled together: \
     alt-offset-test moves IRQ0 onto vector 0x30, which is the vector the GPR \
     restore test uses. Run them as separate builds."
);

pub mod console;
pub mod frame_allocator;
pub mod gdt;
pub mod graphics;
pub mod heap;
pub mod idt;
pub mod keyboard;
pub mod interrupts;
pub mod memory_map;
pub mod paging;
pub mod pic;
pub mod pit;
pub mod stack;
