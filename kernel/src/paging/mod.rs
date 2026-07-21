//! 自前ページテーブル構築（M2-d）。
//!
//! - [`plan`][mod@plan]: マップ対象範囲とページサイズの解決（純粋ロジック、
//!   ホスト `cargo test` で検証）。
//! - [`table`][mod@table]: 実際のページテーブルへの書き込み（unsafe）。
//! - [`switch`][mod@switch]: CR3 の読み取り・切り替え（unsafe）。

pub mod entry;
pub mod active;
pub mod plan;
pub mod switch;
pub mod table;
