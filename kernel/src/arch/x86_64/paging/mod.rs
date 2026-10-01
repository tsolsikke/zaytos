//! x86_64 のページテーブルの操作（項目の形・表の作成・稼働中の表・CR3 の切り替え・確かめ・取り外し）。
//! **どの範囲をどの大きさでマップするかの計画は、共通の側（`crate::paging::plan`）に残る。**

pub mod active;
pub mod address_space;
pub mod entry;
pub mod remove;
pub mod survey;
pub mod switch;
pub mod table;
pub mod verify;
