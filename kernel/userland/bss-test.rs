//! `bss-test`: `.bss` が張られていることを確かめる（ADR-0039 の到達条件 2）。
//!
//! # なぜ専用の 1 本が要るのか
//!
//! **`.bss` を持つ最初のユーザープログラムは `zi` だった。** あれは
//! `.bss` へ 16KiB の配列を置いており、**共有ページを踏んだのも偶然である。**
//! **`zi` が痩せて `.bss` が消えれば、`.bss` を張る能力は誰も検査しない
//! 機構に戻る**（ADR-0038 の穴で同じことが起きかけた）。
//!
//! **この 1 本は形を変えない。** 判定に必要な性質だけを持つ。
//!
//! # 何を主張するか
//!
//! 1. **`.bss` がゼロで読める**（ローダーがゼロ埋めした）
//! 2. **`.bss` へ書ける**（W=1 で張られた）
//! 3. **区画がページを共有している**——`.bss` の先頭が
//!    `.data` の終端と同じページに載っていること。**これが無いと、
//!    「`.bss` は読める」を主張するだけで「共有ページを通った」ことは
//!    誰も見ていない状態へ黙って戻る。**
//!
//! **3 つ目は自分の番地から確かめる。** `.data` の末尾（下の `TAIL`）と
//! `.bss` の先頭（`ZEROS`）が同じ 4KiB ページに載っているかを見る。
//!
//! # 終了状態の意味
//!
//! - `0` 3 つとも成り立った
//! - `1` `.bss` がゼロで読めなかった
//! - `2` `.bss` へ書いた値が読み戻せなかった
//! - `3` 区画がページを共有していなかった（**判定が無意味になっている**）

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, write_all, STDERR};

/// `.bss` に置く配列。**初期値がゼロなので `.bss` へ入る。**
///
/// **1 ページより大きくする。** 共有ページの先だけでなく、
/// **その次のページも張られていること**を同じ判定で見るためである。
static mut ZEROS: [u8; 8192] = [0; 8192];

/// `.data` の末尾に置く値。**非ゼロなので `.data` へ入る。**
///
/// **`ZEROS` と同じページに載ることを確かめる的である。**
static mut TAIL: u64 = 0x5A5A_5A5A_5A5A_5A5A;

/// ページの大きさ。
const PAGE: usize = 4096;

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(_stack: *const u64) -> ! {
    // SAFETY: このプロセスは単一の実行文脈で、これらを触るのはここだけである。
    let zeros = unsafe { &mut *core::ptr::addr_of_mut!(ZEROS) };
    // SAFETY: 上と同じ。
    let tail = unsafe { &*core::ptr::addr_of!(TAIL) };

    // (1) 全部ゼロで読めること。**先頭だけでなく、次のページの先まで見る。**
    if zeros.iter().any(|byte| *byte != 0) {
        write_all(STDERR, b"bss-test: the .bss was not zero\n");
        exit(1);
    }

    // (2) 書けて、読み戻せること。**両端を打つ**——最後の要素は
    // 共有ページの先のページに載っている。
    let last = zeros.len() - 1;
    zeros[0] = 0xA5;
    zeros[last] = 0x5A;
    if zeros[0] != 0xA5 || zeros[last] != 0x5A {
        write_all(STDERR, b"bss-test: the .bss did not keep what was written\n");
        exit(2);
    }

    // (3) 区画がページを共有していること。**`.data` の末尾と `.bss` の先頭が
    // 同じページに載っているか。**
    let data_page = (core::ptr::from_ref(tail) as usize) / PAGE;
    let bss_page = (zeros.as_ptr() as usize) / PAGE;
    if data_page != bss_page {
        write_all(
            STDERR,
            b"bss-test: .data and .bss do not share a page; this build cannot prove ADR-0039\n",
        );
        exit(3);
    }

    write_all(
        STDERR,
        b"bss-test: the .bss reads as zero, keeps writes, and shares a page with .data\n",
    );
    exit(0);
}
