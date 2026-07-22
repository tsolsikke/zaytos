//! バックバッファと、フレームバッファへの転送（M3-c-2）。
//!
//! **unsafe を含む。** 通常 RAM 上に確保した面へ直接読み書きし、そこから
//! フレームバッファへ転送する。
//!
//! 描画はすべてこのバックバッファ（キャッシュ有効）に対して行い、
//! フレームバッファ（PCD、キャッシュ無効）へは変更範囲の転送だけを行う
//! （ADR-0017）。形状はフレームバッファと完全に同一にしてあるため、
//! 転送は行ごとの単純なコピーで済む。
//!
//! 一括操作（全面クリア・スクロール・行消去）はスライスとして扱い、
//! `fill` / `copy_within` を使う。1 ピクセルずつの `write_volatile` では
//! 全面クリアだけで 100 万回の書き込みになるため。グリフの描画は
//! [`Framebuffer`] の描画処理をそのまま使う（描画コードを二重に持たない）。

use common::addr::VirtAddr;
use core::sync::atomic::{compiler_fence, Ordering};

use crate::graphics::layout::BYTES_PER_PIXEL;
use crate::graphics::{Color, Framebuffer, FramebufferLayout, LayoutError};

use super::dirty::Rect;

/// [`BackBuffer::new`] が拒否した理由。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BackBufferError {
    /// 形状の作り直しに失敗した。
    InvalidLayout(LayoutError),
    /// バックバッファとフレームバッファの範囲が重なっている。
    /// この状態で `copy_nonoverlapping` を使うと未定義動作になる。
    OverlapsFramebuffer {
        back_start: u64,
        back_end: u64,
        front_start: u64,
        front_end: u64,
    },
}

/// 転送直前の範囲検証に失敗したときの内訳。
///
/// この型が返る時点で、`DirtyRegion` の切り詰めか `FramebufferLayout` の
/// 検証のどちらかが壊れている。原因を特定できるよう、判断に使った値を
/// すべて持たせる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FlushRangeError {
    /// 検証に失敗した行（ピクセル単位の y 座標）。
    pub row: u32,
    /// バックバッファ側のオフセット。`None` なら座標が画面外と判定された。
    pub back_offset: Option<u64>,
    /// フレームバッファ側のオフセット。`None` なら座標が画面外と判定された。
    pub front_offset: Option<u64>,
    /// この行で転送しようとしたバイト数。
    pub bytes: u64,
    pub back_size_bytes: u64,
    pub front_size_bytes: u64,
    /// 検証対象だった矩形。
    pub rect: Rect,
}

/// フレームバッファと同じ形状を持つ、通常 RAM 上の描画面。
pub struct BackBuffer {
    /// グリフ描画用。書き込み先はバックバッファの先頭。
    surface: Framebuffer,
    base: VirtAddr,
    pixel_count: usize,
    /// 1 行あたりのピクセル数（= stride）。
    stride: u32,
}

impl BackBuffer {
    /// バックバッファを作る。
    ///
    /// # Safety
    ///
    /// 呼び出し側は以下を保証すること。
    ///
    /// - `base` から `front_layout.size_bytes()` バイトが、現在のページ
    ///   テーブルで読み書き可能にマップされていること。呼び出し側は
    ///   `MappedRanges::contains_range` で確認してから渡すこと。
    /// - その領域を他の誰も使っていないこと。`BackBuffer` は排他所有を
    ///   前提とし、同じ領域に対して複数個作らないこと。
    /// - `front_layout` が [`FramebufferLayout::from_info`] を通ったもので
    ///   あること。
    pub unsafe fn new(
        base: VirtAddr,
        front_layout: &FramebufferLayout,
    ) -> Result<Self, BackBufferError> {
        let layout = front_layout
            .with_base(base)
            .map_err(BackBufferError::InvalidLayout)?;

        // 転送に copy_nonoverlapping を使うため、両者が重ならないことを
        // 構築時に確かめる。重なっていれば未定義動作になる。
        let (back_start, back_end) = (layout.base(), layout.end());
        let (front_start, front_end) = (front_layout.base(), front_layout.end());
        if back_start < front_end && front_start < back_end {
            return Err(BackBufferError::OverlapsFramebuffer {
                back_start: back_start.as_u64(),
                back_end: back_end.as_u64(),
                front_start: front_start.as_u64(),
                front_end: front_end.as_u64(),
            });
        }

        let pixel_count = (layout.size_bytes() / BYTES_PER_PIXEL) as usize;
        let stride = layout.stride();

        // SAFETY: 呼び出し側の契約により base..end はマップ済みかつ排他所有。
        // layout は with_base で検証済み。
        let surface = unsafe { Framebuffer::new(layout) };

        Ok(Self {
            surface,
            base,
            pixel_count,
            stride,
        })
    }

    /// グリフ描画に使う面。
    pub fn surface_mut(&mut self) -> &mut Framebuffer {
        &mut self.surface
    }

    pub fn layout(&self) -> &FramebufferLayout {
        self.surface.layout()
    }

    /// バックバッファ全体をピクセルのスライスとして見る。
    ///
    /// 一括操作専用。返したスライスが生きている間に [`Self::surface_mut`]
    /// 経由の書き込みを行うと、同じメモリへの参照と生ポインタが同時に
    /// 生きることになる。どちらも `&mut self` を要求するため、借用検査で
    /// 同時に存在できないようになっている。
    fn pixels_mut(&mut self) -> &mut [u32] {
        // SAFETY: base..base + pixel_count * 4 は new の安全性要件により
        // マップ済み・排他所有であり、4 バイト境界にあることも with_base で
        // 検証済み。`&mut self` を取っているため、この期間に他の参照は無い。
        unsafe { core::slice::from_raw_parts_mut(self.base.as_mut_ptr::<u32>(), self.pixel_count) }
    }

    /// 全体を単色で塗る。
    ///
    /// フレームアロケータが返すフレームには前の内容が残っている。ヒープの
    /// 毒値（`0xDE`）が書かれた領域が回ってくることもある。最初の転送より
    /// 前にこれを呼ばないと、起動直後にノイズが画面に出る。
    pub fn clear_all(&mut self, color: Color) {
        let pixel = color.to_pixel(self.layout().format());
        self.pixels_mut().fill(pixel);
    }

    /// 指定した行範囲（ピクセル単位）を単色で塗る。
    ///
    /// 画面外へはみ出す分は切り詰める。
    pub fn clear_rows(&mut self, top: u32, height: u32, color: Color) {
        let screen_height = self.layout().height();
        if height == 0 || top >= screen_height {
            return;
        }
        let height = height.min(screen_height - top);
        let pixel = color.to_pixel(self.layout().format());
        let stride = self.stride as usize;

        let start = top as usize * stride;
        let end = start + height as usize * stride;
        let pixels = self.pixels_mut();
        // 形状の検証により end <= pixel_count は成り立つが、崩れていても
        // 範囲外アクセスで停止しないよう get_mut で受ける。
        if let Some(slice) = pixels.get_mut(start..end) {
            slice.fill(pixel);
        }
    }

    /// 内容を `lines` ピクセル分だけ上へずらし、空いた下端を `color` で埋める。
    ///
    /// 通常 RAM 内の移動であり、フレームバッファへのバス往復は発生しない
    /// （ADR-0017 の決定 4）。
    pub fn scroll_up(&mut self, lines: u32, color: Color) {
        let screen_height = self.layout().height();
        if lines == 0 {
            return;
        }
        if lines >= screen_height {
            self.clear_all(color);
            return;
        }

        let stride = self.stride as usize;
        let moved_rows = (screen_height - lines) as usize;
        let from = lines as usize * stride;
        let to_len = moved_rows * stride;

        let pixels = self.pixels_mut();
        if from + to_len <= pixels.len() {
            pixels.copy_within(from..from + to_len, 0);
        }

        self.clear_rows(screen_height - lines, lines, color);
    }

    /// 指定範囲をフレームバッファへ転送し、転送したバイト数を返す。
    ///
    /// 転送は行ごとの [`core::ptr::copy_nonoverlapping`] で行う。1 ピクセル
    /// ずつのループにはしない。stride が一致していて矩形が全幅を覆う場合は
    /// 1 本の連続コピーに短絡する。
    ///
    /// 転送直前の範囲検証に失敗した場合は [`FlushRangeError`] を返す。
    /// **この経路は到達しないはずである**: `rect` は [`super::DirtyRegion`]
    /// が画面内へ切り詰めたものであり、形状は
    /// [`FramebufferLayout::from_info`] の検証を通っているため、
    /// 各行のオフセットは必ず `size_bytes` に収まる。
    ///
    /// それでも検証を残し、失敗を握りつぶさずに返すのは、到達した場合に
    /// 「描画系のどこかが壊れている」ことを必ず観測できるようにするため。
    /// 無言でその行を飛ばすと、画面の一部が欠けるだけで原因に気づけない
    /// （ADR-0004 の fail-fast 方針）。呼び出し側はこのエラーを報告し、
    /// 停止すること。
    pub fn flush_rect(
        &self,
        framebuffer: &mut Framebuffer,
        rect: Rect,
    ) -> Result<u64, FlushRangeError> {
        let front = *framebuffer.layout();
        let back = *self.layout();

        // stride が一致している前提に依存したまま書かない。異なる場合は
        // 行ごとのオフセットが両者でずれるため、必ず行単位で転送する。
        let strides_match = front.stride() == back.stride();

        let mut transferred = 0u64;

        // 全幅かつ stride 一致なら、行の切れ目が無いので 1 本で送れる。
        if strides_match && rect.x == 0 && rect.width == front.stride() {
            let start = back.pixel_offset_bytes(0, rect.y);
            let pixels = (rect.height as u64) * (front.stride() as u64);
            let bytes = pixels * BYTES_PER_PIXEL;
            let fits = start
                .is_some_and(|s| s + bytes <= front.size_bytes() && s + bytes <= back.size_bytes());
            if !fits {
                return Err(FlushRangeError {
                    row: rect.y,
                    back_offset: start,
                    front_offset: front.pixel_offset_bytes(0, rect.y),
                    bytes,
                    back_size_bytes: back.size_bytes(),
                    front_size_bytes: front.size_bytes(),
                    rect,
                });
            }
            let start = start.unwrap_or(0);
            // SAFETY: start..start + bytes は両者の size_bytes 内であることを
            // 直前に確認した。両範囲は new で重複しないことを検証済み。
            // どちらもマップ済み・4 バイト境界（形状の検証による）。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    back.base().as_ptr::<u32>().byte_add(start as usize),
                    front.base().as_mut_ptr::<u32>().byte_add(start as usize),
                    pixels as usize,
                );
            }
            transferred += bytes;
        } else {
            for row in rect.y..rect.bottom() {
                // 転送直前に、この行が両者の検証済み範囲へ収まることを確かめる。
                let back_offset = back.pixel_offset_bytes(rect.x, row);
                let front_offset = front.pixel_offset_bytes(rect.x, row);
                let bytes = (rect.width as u64) * BYTES_PER_PIXEL;

                let fits = match (back_offset, front_offset) {
                    (Some(back_offset), Some(front_offset)) => {
                        back_offset + bytes <= back.size_bytes()
                            && front_offset + bytes <= front.size_bytes()
                    }
                    _ => false,
                };
                if !fits {
                    return Err(FlushRangeError {
                        row,
                        back_offset,
                        front_offset,
                        bytes,
                        back_size_bytes: back.size_bytes(),
                        front_size_bytes: front.size_bytes(),
                        rect,
                    });
                }
                let (back_offset, front_offset) =
                    (back_offset.unwrap_or(0), front_offset.unwrap_or(0));
                // SAFETY: 上で両者の size_bytes 内であることを確認した。
                // 範囲が重ならないことは new で検証済み。アラインメントは
                // 形状の検証（先頭が 4 バイト境界、オフセットは 4 の倍数）に
                // よる。
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        back.base().as_ptr::<u32>().byte_add(back_offset as usize),
                        front
                            .base()
                            .as_mut_ptr::<u32>()
                            .byte_add(front_offset as usize),
                        rect.width as usize,
                    );
                }
                transferred += bytes;
            }
        }

        // copy_nonoverlapping は volatile ではない。フレームバッファは PCD で
        // キャッシュされないため値はバスへ出るが、コンパイラによる並べ替えを
        // 禁じるものではない。命令を生成しないフェンスで順序だけ固定する。
        compiler_fence(Ordering::SeqCst);

        Ok(transferred)
    }
}
