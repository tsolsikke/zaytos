//! 画面描画。
//!
//! - [`color`][mod@color]: 色とピクセルフォーマットへの変換（純粋ロジック）。
//! - [`layout`][mod@layout]: フレームバッファ形状の検証と座標計算（純粋ロジック）。
//! - [`font`][mod@font]: ビットマップフォントのグリフ検索（純粋ロジック）。
//!   グリフデータは GNU Unifont 由来（SIL OFL 1.1、`third_party/unifont/`）。
//! - [`framebuffer`][mod@framebuffer]: 実際の書き込み（`unsafe`）。
//! - [`text`][mod@text]: 文字列の描画。改行・折り返しは扱わない。
//!
//! # 守ること（ADR-0013）
//!
//! - 扱うのは `Rgb`/`Bgr` の 32bpp のみ。`Bitmask`/`BltOnly` は起動時に fail-fast する。
//! - bootloader から渡された形状は信用せず、kernel 側で全項目を検証してから使う。
//!   検証を通った証明が [`layout::FramebufferLayout`] である。
//! - 描画経路では panic しない。画面外の座標や矩形は切り詰めるか無視する。
//! - パニック時の画面出力は行わない（シリアルのみ）。

pub mod color;
pub mod font;
pub mod framebuffer;
pub mod layout;
pub mod text;

pub use color::Color;
pub use font::Glyph;
pub use framebuffer::Framebuffer;
pub use layout::{FramebufferLayout, LayoutError};
pub use text::text_width_pixels;
