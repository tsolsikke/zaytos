//! 外部 ABI の置き場（`ADR-0071` の決定 1 の 2）。**CPU によらないものは `abi/linux` に、CPU によって違うものは CPU
//! ごとの置き場に分ける**（`abi/linux/x86_64`。将来 arm64 の番号表と `struct stat` の配置が並ぶ）。**Linux の ABI ではない
//! ZaytOS だけの決まり（独自の番号・`ioctl`・`spawn` の戻り値のビット）は `abi/private` に置く**（CPU によらない。
//! 2026-09-30）。

pub mod linux;
pub mod private;
