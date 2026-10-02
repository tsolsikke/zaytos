//! 自前ページテーブル構築（M2-d）。
//!
//! - [`plan`][mod@plan]: マップ対象範囲とページサイズの解決（純粋ロジック、
//!   ホスト `cargo test` で検証）。
//! - [`permissions`][mod@permissions]: ページの権限（CPU に依らない言い方。純粋ロジック）。
//! - [`table`][mod@crate::arch::x86_64::paging::table]: 実際のページテーブルへの書き込み（unsafe）。
//! - [`switch`][mod@crate::arch::x86_64::paging::switch]: CR3 の読み取り・切り替え（unsafe）。

pub mod permissions;
pub mod plan;
