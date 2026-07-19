//! ZaytOS kernel の共有ロジック。
//!
//! ハードウェア依存部（`main.rs` の `_start`/`panic.rs`）と純粋ロジック
//! （[`memory_map`], [`frame_allocator`]）を分離し、後者はホスト上の
//! `cargo test` で検証する。M2-c の物理フレーム
//! アロケータは「間違えると無言で壊れる」領域であるため、特に手厚く
//! テストする。

#![cfg_attr(not(test), no_std)]

pub mod frame_allocator;
pub mod heap;
pub mod memory_map;
pub mod paging;
