//! ZaytOS カーネル（M1: UEFI Hello World 段階）。
//!
//! ハードウェア依存部（[`serial`], [`cpu`]）と純粋ロジック（[`log`]）を
//! 分離し、後者はホスト上の `cargo test` で検証する。
//! `#[panic_handler]` は `no_std`/`no_main` な実バイナリ（`main.rs`）側
//! にのみ定義する。テストビルド（ホストターゲット）では std の
//! パニックハンドラと衝突するため、ここには置かない。

#![cfg_attr(not(test), no_std)]

pub mod cpu;
pub mod log;
pub mod serial;
