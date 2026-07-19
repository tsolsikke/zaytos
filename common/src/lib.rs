//! ZaytOS の bootloader と kernel（ADR-0008）の両方から使う共有ロジック。
//!
//! ハードウェア依存部（[`serial`], [`cpu`]）と純粋ロジック（[`log`]）を
//! 分離し、後者はホスト上の `cargo test` で検証する。
//! `#[panic_handler]` はこのクレートには置かない。`no_std`/`no_main` な
//! 実バイナリ（bootloader/kernel それぞれの `main.rs`）側にのみ定義する。
//! テストビルド（ホストターゲット）では std のパニックハンドラと衝突する
//! ため、共有ライブラリに置くことはできない。

#![cfg_attr(not(test), no_std)]

pub mod boot_info;
pub mod cpu;
pub mod elf;
pub mod log;
pub mod serial;
