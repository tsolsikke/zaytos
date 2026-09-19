//! バイトの輪（`ADR-0064`）。**パイプとストリームソケットが共通に使う核である。**
//!
//! # `ADR-0063` の「共通の核」を型にした
//!
//! **(b3) はパイプを「ストリームソケットと共通の核を、いちばん小さい面で確かめる段」として
//! 置いた。** **ソケットを作る段で、その核を `pipe.rs` から切り出した**——**輪の計算は
//! 両方で同じで、違うのは持ち主（誰が待ち、誰を起こすか）だけである。**
//!
//! # 純粋な計算である
//!
//! **錠も起こしも持たない。** **溜まっているバイトを移す・入るだけ入れる・空きを数える、
//! だけである。** **待つか起こすかは持ち主が決める**（`pipe` / `socket`）。
//! **ホストで検査できる**（`cargo test`）。
//!
//! # 大きさは持ち主が決める
//!
//! **パイプは 256 バイト、ソケットは向きごとに 1,024 バイトである**（根拠はそれぞれの
//! モジュールの doc）。**定数ジェネリクスで持つので、置き場は持ち主の静的領域である。**

/// 固定長のバイトの輪。**`N` バイトまで溜められる。**
pub struct Ring<const N: usize> {
    buf: [u8; N],
    /// 次に読む位置。
    head: usize,
    /// 溜まっているバイト数。
    len: usize,
}

impl<const N: usize> Ring<N> {
    /// 空の輪。**`const` なので静的領域の初期値に使える。**
    pub const EMPTY: Self = Self {
        buf: [0; N],
        head: 0,
        len: 0,
    };

    /// 溜められる上限（バイト）。
    pub const CAPACITY: usize = N;

    /// 溜まっているバイト数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 1 バイトも無いか。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// あと何バイト入るか。
    pub fn room(&self) -> usize {
        N - self.len
    }

    /// 溜まっているバイトを `dst` へ移す。**移した数を返す**（`dst` と溜まりの短いほう）。
    pub fn take(&mut self, dst: &mut [u8]) -> usize {
        let take = dst.len().min(self.len);
        for byte in dst.iter_mut().take(take) {
            *byte = self.buf[self.head];
            self.head = (self.head + 1) % N;
        }
        self.len -= take;
        take
    }

    /// `src` を入るだけ入れる。**入れた数を返す**（満杯なら 0）。
    pub fn put(&mut self, src: &[u8]) -> usize {
        let put = src.len().min(self.room());
        for byte in &src[..put] {
            let at = (self.head + self.len) % N;
            self.buf[at] = *byte;
            self.len += 1;
        }
        put
    }

    /// 空きを見ずに上書きする。**破壊のためだけに在る**（`ADR-0063` の (b3),
    /// pipe-write-ignores-full）。**溜まりの数は上限で飽和し、古いバイトが消える。**
    #[cfg(feature = "pipe-write-ignores-full")]
    pub fn put_overwriting(&mut self, src: &[u8]) -> usize {
        let put = src.len().min(N);
        for byte in &src[..put] {
            let at = (self.head + self.len) % N;
            self.buf[at] = *byte;
            self.len = (self.len + 1).min(N);
        }
        put
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 256;

    #[test]
    fn the_ring_wraps_without_losing_order() {
        let mut ring = Ring::<N>::EMPTY;
        let mut out = [0u8; 300];
        // 200 入れて 150 取り、また 200 入れる——境を跨ぐ。
        let first: [u8; 200] = core::array::from_fn(|i| i as u8);
        assert_eq!(ring.put(&first), 200);
        assert_eq!(ring.take(&mut out[..150]), 150);
        assert_eq!(&out[..150], &first[..150]);
        let second: [u8; 200] = core::array::from_fn(|i| (i + 100) as u8);
        assert_eq!(ring.put(&second), 200);
        assert_eq!(ring.len(), 250);
        assert_eq!(ring.take(&mut out[..250]), 250);
        assert_eq!(&out[..50], &first[150..]);
        assert_eq!(&out[50..250], &second[..]);
        assert!(ring.is_empty());
    }

    #[test]
    fn a_full_ring_takes_nothing_more() {
        let mut ring = Ring::<N>::EMPTY;
        let block = [7u8; N];
        assert_eq!(ring.put(&block), N);
        assert_eq!(ring.room(), 0);
        assert_eq!(ring.put(&[1, 2, 3]), 0);
        let mut out = [0u8; 4];
        assert_eq!(ring.take(&mut out), 4);
        assert_eq!(ring.put(&[1, 2, 3]), 3);
    }

    #[test]
    fn the_capacity_is_the_parameter() {
        assert_eq!(Ring::<64>::CAPACITY, 64);
        assert_eq!(Ring::<64>::EMPTY.room(), 64);
    }
}
