//! クリティカルセクション（M4-c-1）。
//!
//! シングルコア前提では、共有データの排他は「割り込み禁止」で行う（architecture.md
//! の同期・並行性方針）。[`InterruptGuard`] は RAII で `cli`/`sti` を対にし、**入れ子でも
//! 正しく動く**ようにする。
//!
//! 復元の判断（保存時に IF=1 だった場合のみ `sti`）は純粋ロジックとして
//! [`crate::cpu::should_restore_interrupts`] に切り出し、ホストテストで固定
//! してある。ここはそれを使ってハードウェアを操作するだけ。

use core::marker::PhantomData;

use crate::cpu;

/// 割り込みを禁止するクリティカルセクションのガード。
///
/// [`InterruptGuard::enter`] で現在の RFLAGS を保存して `cli` し、Drop で
/// **保存時に IF=1 だった場合のみ** `sti` する。無条件に `sti` しないのが
/// 入れ子の正しさの要である。
///
/// - 外側が IF=1 で `enter` → `cli`（保存値 IF=1）
/// - 内側が `enter` → 既に IF=0 なので保存値 IF=0、Drop でも `sti` しない
/// - 外側の Drop で初めて `sti`
///
/// # スレッド安全性
///
/// このガードは「今この CPU が張っている割り込み禁止区間」を表す。別の
/// コンテキスト（M5 以降のタスク）へ移動すると、cli した文脈と sti する
/// 文脈がずれて意味が壊れる。そのため `PhantomData<*const ()>` を持たせて
/// **`!Send` かつ `!Sync`** にしてある。生ポインタは `Send`/`Sync` を
/// 自動導出しないため、これだけでガード全体が移動・共有不可になる。
/// 現状シングルコアでスレッドも無いため実害は無いが、M5 で意味を持つ。
pub struct InterruptGuard {
    saved_rflags: u64,
    /// `!Send` + `!Sync` にするためのマーカー。値としては使わない。
    _not_send_sync: PhantomData<*const ()>,
}

impl InterruptGuard {
    /// 現在の割り込み状態を保存して割り込みを禁止する。
    ///
    /// 戻り値のガードが生きている間、割り込みは禁止される。ガードを早く
    /// 落とせば、その時点で（保存状態に応じて）復元される。
    #[must_use = "the guard must be held for the critical section; dropping it immediately \
                  ends the section right away"]
    pub fn enter() -> Self {
        let saved_rflags = cpu::read_rflags();
        // SAFETY: これはまさにクリティカルセクションへ入る操作であり、割り込みを
        // 禁止してよい文脈。保存した状態は Drop で復元する。
        unsafe {
            cpu::disable_interrupts();
        }
        Self {
            saved_rflags,
            _not_send_sync: PhantomData,
        }
    }

    /// 保存した RFLAGS（診断用）。
    pub fn saved_rflags(&self) -> u64 {
        self.saved_rflags
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        if cpu::should_restore_interrupts(self.saved_rflags) {
            // SAFETY: enter した時点で IF=1 だった、つまり呼び出し元は割り込みが
            // 有効な文脈にいた。その状態へ戻すだけなので有効化してよい。
            // IF=0 だった場合はこの分岐に入らないため、勝手に有効化しない。
            unsafe {
                cpu::enable_interrupts();
            }
        }
    }
}

/// `InterruptGuard` が `Send` でも `Sync` でもないことをコンパイル時に固定する。
///
/// 手法（`static_assertions` クレートの `assert_not_impl_any!` と同じ）:
/// 「すべての型」に実装した版（型引数 `()`）と「`Send` な型」に実装した版
/// （型引数 `u8`）の 2 つを用意する。呼び出し側は型引数を推論穴 `_` にして
/// メソッドを参照する。対象が `Send` だと両方の impl が該当し `_` を一意に
/// 決められずコンパイルに失敗する。`Send` でなければ `()` 版だけが該当して
/// 通る。**この `const _` が通ること自体が `!Send` の証明**であり、将来
/// `PhantomData` を外して誤って `Send` になると、ここでビルドが落ちる。
///
/// 型引数を `_` にするのが要点。`<()>` と明示すると常に第 1 impl へ解決して
/// しまい、負の検査にならない。
const _: fn() = || {
    trait AmbiguousIfSend<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}

    // 推論穴 `_`。InterruptGuard が Send だと曖昧になりビルドが落ちる。
    let _ = <InterruptGuard as AmbiguousIfSend<_>>::some_item;
};

const _: fn() = || {
    trait AmbiguousIfSync<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}

    let _ = <InterruptGuard as AmbiguousIfSync<_>>::some_item;
};
