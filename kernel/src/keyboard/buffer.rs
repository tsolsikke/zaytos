//! 割り込みハンドラとメインループで共有するスキャンコードのリングバッファ。
//!
//! ## 排他方式に `Locked<T>` を選んだ理由
//!
//! インデックスを `Atomic` にしたロックフリーの単一生産者・単一消費者
//! （SPSC）構成も検討したが、**`common::critical::Locked<T>` を採る**。
//!
//! | 観点 | `Locked<T>` | ロックフリー SPSC |
//! |---|---|---|
//! | 正しさの論証 | シングルコア + 保持中 IF=0 で自明。M4-c-2 で検証済み | メモリ順序が正しいことを論証する必要がある |
//! | ホストでの検証 | drop 順と二重取得検出を検証済み | **順序の誤りが x86 では再現しない**。`Ordering` を取り違えていてもホストの `cargo test` は通る |
//! | ハンドラ内のコスト | `cli` 1 回。ただし**ハンドラは割り込みゲート経由で既に IF=0** なので実質は入れ子ガードの分岐だけ | ほぼゼロ |
//! | 二重取得の検出 | あり | なし |
//!
//! 決め手は「検証できない正しさを抱えないこと」。コスト差は実質ゼロである。
//! 同期・並行性方針（architecture.md）の「最小限のラッパーにとどめ、SMP 用の抽象化を先回りしない」
//! にも沿う。SPSC を選んだ場合に必要になる「前提が崩れる条件」の記録
//! （M5 で複数タスクが読む等）も不要になる。
//!
//! ## 溢れたときは「新しい方」を捨てる
//!
//! 古い方を捨てると**デコードの整合性が壊れる**。バッファには `0xE0`
//! プレフィックスとその後続、make と break の対が並んでおり、先頭から捨てると
//! 「`0xE0` を捨てて後続だけ残る」「make を捨てて break だけ残る」形になり、
//! 修飾キーの状態が実際と食い違う。新しい方を捨てれば、バッファは常に
//! **先頭から連続した正しい前半**になる。
//!
//! **溢れても無言にしない。** 回数をカウンタに記録し、ハートビートへ出す。
//! ADR-0004 の fail-fast に照らすと halt までは不要（入力を数個取りこぼす
//! だけで、カーネルの整合性は壊れない）だが、観測できない状態にはしない。

use core::sync::atomic::{AtomicU64, Ordering};

use common::critical::Locked;

/// リングバッファの容量（バイト）。
///
/// 100Hz で回るメインループに対し、人間の打鍵速度では溢れない。溢れたら
/// 実装側の問題である。
#[cfg(not(feature = "tiny-key-buffer"))]
pub const CAPACITY: usize = 128;

/// オーバーフローの検出が実際に働くかを確かめるための極小容量
/// （`--interrupt-test keyboard-overflow`）。
#[cfg(feature = "tiny-key-buffer")]
pub const CAPACITY: usize = 4;

/// 固定長のリングバッファ。
///
/// ヒープを使わない。割り込みハンドラから触るため、確保が失敗しうる経路を
/// 持たせたくない（ADR-0018 §5 と同じ考え方）。
pub struct ScancodeRing {
    storage: [u8; CAPACITY],
    /// 次に書く位置。
    head: usize,
    /// 次に読む位置。
    tail: usize,
    /// 現在入っている個数。`head == tail` が空と満杯のどちらなのかを
    /// 区別するために持つ（1 要素を捨てる方式は容量が減って分かりにくい）。
    len: usize,
}

impl ScancodeRing {
    pub const fn new() -> Self {
        Self {
            storage: [0; CAPACITY],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn is_full(&self) -> bool {
        self.len == CAPACITY
    }

    /// 1 バイト積む。満杯なら**積まずに** `false` を返す（新しい方を捨てる）。
    ///
    /// **呼び出し側は、これが `false` を返してもデータポートの読み出しを
    /// 省いてはならない。** 読まないとコントローラの出力バッファが空かず、
    /// 以後の IRQ1 が来なくなる。
    pub fn push(&mut self, code: u8) -> bool {
        if self.is_full() {
            return false;
        }
        self.storage[self.head] = code;
        self.head = (self.head + 1) % CAPACITY;
        self.len += 1;
        true
    }

    /// 1 バイト取り出す。空なら `None`。
    pub fn pop(&mut self) -> Option<u8> {
        if self.is_empty() {
            return None;
        }
        let code = self.storage[self.tail];
        self.tail = (self.tail + 1) % CAPACITY;
        self.len -= 1;
        Some(code)
    }
}

impl Default for ScancodeRing {
    fn default() -> Self {
        Self::new()
    }
}

/// ハンドラとメインループが共有するリングバッファの実体。
///
/// # 排他が成立する理由
///
/// - **メインループが保持している間は割り込みが禁止される。**
///   `Locked::lock` は `InterruptGuard` を取ってから `&mut T` を返し、
///   ガードが落ちるまで `IF=0` が続く。したがって**ハンドラがこのロックを
///   保持中に見つけることは構造的にない**（保持中は割り込み自体が配送
///   されない）。デッドロックも二重取得の誤検出も起きない。
/// - **ハンドラ自身がロックを取るときの入れ子も正しい。** ハンドラは割り込み
///   ゲート経由で入場するため既に `IF=0` である。そこから
///   `InterruptGuard::enter` すると、保存される RFLAGS の IF は 0 なので
///   Drop でも `sti` しない（M4-c-1 で検証済みの入れ子の正しさ）。
///   ハンドラから戻るときに `iretq` が本来の IF を復元する。
pub static SCANCODES: Locked<ScancodeRing> = Locked::new(ScancodeRing::new());

/// 溢れて捨てたスキャンコードの数。
static OVERFLOW_COUNT: AtomicU64 = AtomicU64::new(0);

/// ハンドラが受け取ったスキャンコードの総数（捨てた分も含む）。
static RECEIVED_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn overflow_count() -> u64 {
    OVERFLOW_COUNT.load(Ordering::Relaxed)
}

pub fn received_count() -> u64 {
    RECEIVED_COUNT.load(Ordering::Relaxed)
}

/// ハンドラから呼ぶ。受け取ったコードを積み、溢れたら数える。
pub(crate) fn record(code: u8) {
    RECEIVED_COUNT.fetch_add(1, Ordering::Relaxed);
    let mut ring = SCANCODES.lock();
    if !ring.push(code) {
        OVERFLOW_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_come_out_in_the_order_they_went_in() {
        let mut ring = ScancodeRing::new();
        for code in [0x1E, 0x9E, 0x2A] {
            assert!(ring.push(code));
        }
        assert_eq!(ring.pop(), Some(0x1E));
        assert_eq!(ring.pop(), Some(0x9E));
        assert_eq!(ring.pop(), Some(0x2A));
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn an_empty_ring_yields_nothing() {
        let mut ring = ScancodeRing::new();
        assert!(ring.is_empty());
        assert_eq!(ring.pop(), None);
    }

    /// **溢れたら新しい方を捨てる。** 先頭から連続した正しい前半が残ること。
    ///
    /// 古い方を捨てる実装だと、`0xE0` プレフィックスだけが落ちて後続が
    /// 残るような壊れ方をする。
    #[test]
    fn overflow_drops_the_newest_and_keeps_a_valid_prefix() {
        let mut ring = ScancodeRing::new();
        for index in 0..CAPACITY {
            assert!(ring.push(index as u8), "容量までは入る");
        }
        assert!(ring.is_full());
        assert!(!ring.push(0xFF), "満杯なら積まない");

        // 残っているのは最初に入れた分（先頭から連続）。
        for index in 0..CAPACITY {
            assert_eq!(ring.pop(), Some(index as u8));
        }
        assert_eq!(ring.pop(), None, "捨てた新しい方は入っていない");
    }

    /// 読み書きを繰り返しても位置がずれないこと。
    #[test]
    fn the_ring_wraps_around_without_losing_bytes() {
        let mut ring = ScancodeRing::new();
        // 容量の 3 倍を、1 個ずつ出し入れしながら回す。
        for round in 0..(CAPACITY * 3) {
            let code = (round % 251) as u8;
            assert!(ring.push(code));
            assert_eq!(ring.pop(), Some(code));
        }
        assert!(ring.is_empty());
    }

    /// 半分埋めた状態で折り返しても順序が保たれること。
    #[test]
    fn partial_drains_keep_the_order_across_the_wrap() {
        let mut ring = ScancodeRing::new();
        let half = CAPACITY / 2;
        for index in 0..CAPACITY {
            ring.push(index as u8);
        }
        for index in 0..half {
            assert_eq!(ring.pop(), Some(index as u8));
        }
        // 空いた分だけ新しく積む。ここで head が折り返す。
        for index in 0..half {
            assert!(ring.push(0x80 + index as u8));
        }
        for index in half..CAPACITY {
            assert_eq!(ring.pop(), Some(index as u8));
        }
        for index in 0..half {
            assert_eq!(ring.pop(), Some(0x80 + index as u8));
        }
        assert_eq!(ring.pop(), None);
    }
}
