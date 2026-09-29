//! x86_64 の Linux の ABI のうち、CPU によって違うもの（引数のレジスタ・`struct stat` の配置。`ADR-0071` の決定 1 の 2 で、
//! 共通の側から移し、CPU によらないものを [`super`] へ分けた。2026-09-30）。

mod layout;

pub use layout::{stat_bytes, STAT_LEN};

mod registers;

pub use registers::{read_request, write_return};
