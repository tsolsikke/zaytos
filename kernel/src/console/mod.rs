//! 画面コンソール（M3）。
//!
//! - [`grid`][mod@grid]: カーソルと桁送りの管理（純粋ロジック、ホスト
//!   `cargo test` で検証）。
//! - [`dirty`][mod@dirty]: 未転送範囲の追跡（純粋ロジック、ホスト
//!   `cargo test` で検証）。
//! - [`backbuffer`][mod@backbuffer]: バックバッファとフレームバッファへの
//!   転送（unsafe）。
//! - [`screen`][mod@screen]: 上記を束ねた [`Console`]。`core::fmt::Write`
//!   を実装する。
//!
//! 設計の背景は ADR-0017 を参照。要点:
//! - バックバッファは通常 RAM に置き、フレームバッファと同じ形式・同じ
//!   stride で持つ。転送を単純なコピーにするため。
//! - 転送は未転送範囲の外接矩形だけを送る。フラッシュ点は `write_fmt`
//!   1 回ごと（`write!` / `writeln!` 1 回ごと）。
//! - コンソールはグローバルに置かず、ロックも持たない。割り込みハンドラ
//!   から出力する要求は M4 で生じるため、そのロック設計は割り込み安全性の
//!   モデルと一体で決める。
//! - シリアルログはコンソールから完全に独立している。コンソールの構築に
//!   失敗してもシリアルログは影響を受けない。
//! - パニック時に画面へは出さない（ADR-0013 Addendum）。

pub mod backbuffer;
pub mod dirty;
pub mod grid;
pub mod screen;

pub use backbuffer::{BackBuffer, BackBufferError};
pub use dirty::{DirtyRegion, Rect};
pub use grid::{Grid, GridError, Placement, Step, MAX_GLYPH_WIDTH_CELLS, TAB_WIDTH};
pub use screen::{Console, ConsoleError, FlushStats};

use core::sync::atomic::{AtomicPtr, Ordering};

/// 前景の [`Console`]。**遠征の間だけ据える。**
///
/// # なぜ静的が要るのか
///
/// **`Console` は `kernel_main` の局所で、`run_init` へ `&mut` で渡る。**
/// 一方 Ring 3 の `write` を受ける [`crate::syscall`] は lib にあり、
/// **その局所へ届く道が無い**。据える形にすると届く。
///
/// # 「書き手が 1 つ」は何が保証するか
///
/// **据える側が `&mut Console` を [`ForegroundConsole`] へ預けるので、
/// 据えている間は据えた側が書けない**——借用検査がそれを見る。
/// **静的にしても型の保証は消えていない。** 消えるのは「参照が 1 つしか無い」
/// ことの保証で、**それは据える区間を借用で区切ることで戻している。**
///
/// # 他のコアは書かない
///
/// **AP は画面へ書かない**（`smp::ap_after_switch` から `ap_heartbeat_loop` へ入り、
/// シリアルへしか書かない）。**`init` は `spawn` の間ブロックしている**（同期である）。
/// **遠征は入れ子でも親が待つ**ので、同時に 2 つの `write` が走らない。
static FOREGROUND: AtomicPtr<Console> = AtomicPtr::new(core::ptr::null_mut());

/// [`FOREGROUND`] へ据えている間だけ生きるガード。
///
/// **`&mut Console` を預かる。** 落ちるときに静的を戻す。
pub struct ForegroundConsole<'a> {
    _console: &'a mut Console,
}

impl Drop for ForegroundConsole<'_> {
    fn drop(&mut self) {
        FOREGROUND.store(core::ptr::null_mut(), Ordering::Release);
    }
}

/// 前景の [`Console`] を据える。**ガードが落ちるまで有効である。**
pub fn install_foreground(console: &mut Console) -> ForegroundConsole<'_> {
    FOREGROUND.store(console as *mut Console, Ordering::Release);
    ForegroundConsole { _console: console }
}

/// 前景の [`Console`] が据えられているか。
///
/// **呼ぶ側が BKL を解くかどうかを決めるために要る。** 据えられていないなら
/// 画面へ書くものが無いので、**解いて取り直す必要も無い。**
pub fn foreground_installed() -> bool {
    !FOREGROUND.load(Ordering::Acquire).is_null()
}

/// 前景の [`Console`] へバイト列を書く。**据えられていなければ何もしない。**
///
/// # 呼ぶ側の前提
///
/// **BKL を解いてから呼ぶこと**（`ADR-0023` の Addendum）。
/// **1 行の描画と転送は 1 ティックの半分ほど掛かる**ので、保持したまま呼ぶと
/// その間もう一方のコアがカーネルへ入れない（実測は `console:` の判定行にある）。
pub fn write_foreground_bytes(bytes: &[u8]) {
    use core::fmt::Write as _;

    let console = FOREGROUND.load(Ordering::Acquire);
    if console.is_null() {
        return;
    }
    // SAFETY: 非 null なら [`install_foreground`] のガードが生きており、
    // その間は据えた側が `&mut Console` を預けたままなので書けない（上の doc）。
    // 他のコアと他の遠征が書かないことも同じ doc に挙げてある。
    let console = unsafe { &mut *console };
    // **UTF-8 でないバイトは落とす。** Ring 3 から来る列に UTF-8 を要求しない
    // （`load_user_program` の `argv` と同じ立場）。画面へ出せるのは字だけである。
    if let Ok(text) = core::str::from_utf8(bytes) {
        let _ = console.write_str(text);
        console.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::grid::MAX_GLYPH_WIDTH_CELLS;
    use crate::graphics::font;

    /// 格子が想定するグリフ幅の上限が、実際にフォントへ収録されている最大幅を
    /// 下回っていないことを確かめる。
    ///
    /// 下回ると、折り返しても置けないグリフが生じ、`Grid` の防御的な破棄
    /// 処理へ落ちて文字が消える。日本語（全角 2 セル）を収録した時点でも
    /// この関係が保たれていることを、ここで機械的に検出する。
    #[test]
    fn the_grid_can_hold_the_widest_glyph_in_the_font() {
        assert!(
            font::max_width_cells() <= MAX_GLYPH_WIDTH_CELLS,
            "フォントに {} セル幅のグリフがあるが、格子の想定上限は {} セル。\
             console::grid::MAX_GLYPH_WIDTH_CELLS を引き上げること",
            font::max_width_cells(),
            MAX_GLYPH_WIDTH_CELLS
        );
    }
}
