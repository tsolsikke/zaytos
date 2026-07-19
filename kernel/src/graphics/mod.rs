//! 画面描画（M3）。
//!
//! - [`color`][mod@color]: 色とピクセルフォーマットへの変換（純粋ロジック、
//!   ホスト `cargo test` で検証）。
//! - [`layout`][mod@layout]: フレームバッファ形状の検証と座標計算
//!   （純粋ロジック、ホスト `cargo test` で検証）。
//! - [`framebuffer`][mod@framebuffer]: 実際の書き込み（unsafe）。
//!
//! 設計の背景は ADR-0013 を参照。要点:
//! - 扱うのは `Rgb`/`Bgr` の 32bpp のみ。`Bitmask`/`BltOnly` は起動時に
//!   fail-fast する（中途半端に対応したふりをしない）。
//! - bootloader から渡された形状は信用せず、kernel 側で全項目を検証してから
//!   描画に使う。検証を通った証明として [`layout::FramebufferLayout`] を持つ。
//! - 描画経路では panic しない。画面外の座標や矩形は切り詰めるか無視する。
//! - パニック時の画面出力は行わない（シリアルのみ）。

pub mod color;
pub mod framebuffer;
pub mod layout;

pub use color::Color;
pub use framebuffer::Framebuffer;
pub use layout::{FramebufferLayout, LayoutError};
