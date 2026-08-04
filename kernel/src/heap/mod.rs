//! カーネルヒープ（M2-e）。
//!
//! - [`plan`][mod@plan]: 空きブロックの配置・分割・結合の判断ロジック
//!   （純粋ロジック、ホスト `cargo test` で検証）。
//! - [`allocator`][mod@allocator]: 侵入型連結リストと `GlobalAlloc`
//!   実装（unsafe）。
//!
//! 設計の背景は ADR-0012 を参照。要点:
//! - 連結リスト単体（隣接結合あり）。サイズクラス分割は、実際に問題が
//!   観測されるまで導入しない（過剰設計の回避）。
//! - ヒープは初期化時に固定サイズのアリーナを1回確保するのみで、
//!   実行時に拡張はしない（[`DEFAULT_HEAP_FRAME_COUNT`]）。
//! - M3 で想定される大きな連続確保（フレームバッファ用バッファ等）は
//!   このヒープを経由せず、`frame_allocator::FrameAllocator::allocate_contiguous`
//!   から直接取得する。

pub mod allocator;
pub mod plan;

/// ヒープの初期アリーナに使う既定のフレーム数（256 フレーム = 1MiB）。
/// 拡張機構は持たないため、不足した場合は `alloc` が診断ログを出して
/// パニックする（ADR-0012）。その時点で必要サイズを見積もり直し、この値を
/// 調整するか、拡張方式を別 ADR として検討する。
pub const DEFAULT_HEAP_FRAME_COUNT: u64 = 256;

/// ヒープ本体。
///
/// **`kernel/src/main.rs` から lib 側へ移した（S6-c）。** 移した理由は、
/// **ハートビートの行が空きバイト数を読むためである**——`interrupts` は lib 側に
/// あり、bin 側の静的は見えない。**登録（`#[global_allocator]`）ごと移している**
/// ので、プログラム全体でのアロケータは 1 つのままである。
/// **ホストテストでは登録しない。** このクレートは `cargo test` でホスト向けにも
/// ビルドされ、**登録するとテストの実行ファイル自身がこの空のヒープを使って
/// SIGSEGV で落ちる**（実際に踏んだ）。ベアメタル側では `cfg(test)` が偽なので
/// 登録される。
#[cfg_attr(not(test), global_allocator)]
pub static ALLOCATOR: allocator::LockedHeap = allocator::LockedHeap::empty();
