//! 外部 ABI の置き場（`ADR-0071` の決定 1 の 2）。**CPU ごとに分ける**（`abi/linux/x86_64`。将来 arm64 の
//! 番号表と構造体の配置が並ぶ）。

pub mod linux;
