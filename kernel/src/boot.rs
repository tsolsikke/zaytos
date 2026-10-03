//! 起動の終わりの目印（2026-10-03。`ADR-0071` の決定 5 と `ADR-0072` の 5 を 1 つにまとめた）。
//!
//! # 1 つしか無い
//!
//! **「起動の間」と「起動の後」を分ける目印は、ここの 1 つである。** **起動の終わり（`run_init` の直前）に
//! [`finish`] を 1 回だけ呼び、以後は [`finished`] が真を返す。** **「起動の後はしない」決まりは、どれもこれを読む。**
//!
//! - 起動の後に、カーネル側の PML4 の項目を新しく作らない（`ADR-0071` の決定 5。ページテーブルの `ensure_child` の守り）。
//! - 起動の後に、割り込みの処理を登録しない（`ADR-0072` の 5。`crate::interrupts` の登録の入口）。
//!
//! **2026-10-03 までは、目印が 2 か所にあった**——カーネル側の PML4 の指紋（`paging`。0 が「まだ」）と、割り込みの
//! 処理の表の閉じた印（`interrupts`）で、どちらも `kernel_main` の同じ所で立っていた。**3 つ目の決まりを入れるときに、
//! 機械に依らない共通の側へ 1 つにまとめた**（`docs/deferred-decisions.md` の「「起動の後」を示す目印を1つにまとめるか」）。
//! **指紋は指紋の値（突き合わせの相手）として残し、「起動の後か」の意味は持たせない。**
//!
//! # 機械の言葉を持たない
//!
//! **この置き場は、ページテーブルも割り込みの表も知らない。** 読むのは機械ごとの側（`arch`）と共通の側の両方で、
//! 立てるのは `kernel_main` だけである。

use core::sync::atomic::{AtomicBool, Ordering};

/// 起動が終わったか。**偽から真へ 1 度だけ変わる。**
static FINISHED: AtomicBool = AtomicBool::new(false);

/// 起動の終わりを告げる（`kernel_main` が `run_init` の直前に 1 回だけ呼ぶ）。
///
/// **2 度目は名前つきで止まる**——2 度呼ばれる形は、起動の順が崩れていることである（破壊テストが、ここで
/// 止まることを見る）。
pub fn finish() {
    if FINISHED.swap(true, Ordering::SeqCst) {
        panic!(
            "boot: finish() was called twice; the end of boot is announced exactly once (ADR-0071)"
        );
    }
}

/// 起動が終わっているか。**「起動の後はしない」決まりが読む。**
pub fn finished() -> bool {
    FINISHED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    // **`static` は 1 つなので、順に依らない形で見る**——偽のまま始まり、1 度立てたら真で、2 度目は止まる。
    // ホストのテストは並行に走るので、この置き場のテストは 1 本にまとめる。
    #[test]
    fn the_end_of_boot_is_announced_exactly_once() {
        assert!(!super::finished());
        super::finish();
        assert!(super::finished());
        assert!(std::panic::catch_unwind(super::finish).is_err());
        assert!(super::finished());
    }
}
