//! パイプ（`ADR-0063` の (b3)）。**片方向のバイトの通り道で、読む側が待ち、書く側が起こす。**
//!
//! # 1 本だけ、輪は 256 バイト
//!
//! **利用者はシェルの `a | b` だけである。** **2 本目の Ring 3 が 1 本しか無いので、
//! `a | b | c` は作れない**——**[`MAX_PIPES`] は 1 である。**
//!
//! **輪は 256 バイトにした**（[`PIPE_RING`]）。**Linux の 4 KiB を採らない理由は、像に
//! 4 KiB を超える種が無く、書き手が待つ機会が作れないことである**（「機会が無い」）。
//! **256 なら `/data/big`（2181 バイト）で書き手が 8 回待つ機会が出る。** **私物の口なので、
//! 大きさを Linux に合わせる義務は無い**（`docs/architecture.md` の「ABI の形は合わせる」）。
//!
//! # 読み手の予約
//!
//! **`a | b` は左を先に起こす。** **左が右より先に書くと、読み手が居ないので `-EPIPE` に
//! なる**——**それを防ぐために、右が起きるまで読み手を「予約」しておく**
//! （[`create`] の `reserve_reader`）。**予約は右の `spawn` が消費する。** **右が起きなかった
//! とき（見つからなかったとき）は、待つ口（`SYS_WAIT_CHILD`）が予約を消す**——**消さないと、
//! 左が満杯で永久に待つ。**
//!
//! # 起こすのはここである
//!
//! **閉じる経路は `close` からも、プロセスの終わり（表の `Drop`）からも来る。** **起こす場所を
//! 1 つにしておかないと、片方が起こし忘れる。** **書いたら読み手を、読んだら書き手を、
//! 最後の書き手が閉じたら読み手を（EOF を見に行かせる）、最後の読み手が閉じたら書き手を
//! （`-EPIPE` を見に行かせる）起こす。**
//!
//! # `SIGPIPE` は無い
//!
//! **シグナルを持たないので、読み手の居ないパイプへの書きは `-EPIPE` だけである**
//! （Linux も `-EPIPE` は返す。**送らないのはシグナルのほうである**）。

use core::sync::atomic::{AtomicU64, Ordering};

use common::critical::Locked;

use crate::ring::Ring;

/// 輪の大きさ（バイト）。**256 の根拠はモジュールの doc にある。**
pub const PIPE_RING: usize = 256;

/// 同時に在れるパイプの数。**`a | b` に 1 本。**
pub const MAX_PIPES: usize = 1;

/// パイプ 1 本の状態。
struct Pipe {
    /// 輪（`crate::ring`。**ソケットと共通の核**）。
    ring: Ring<PIPE_RING>,
    /// 読み端を持つ者の数。
    readers: u8,
    /// 書き端を持つ者の数。
    writers: u8,
    /// 読み手がまだ起きていないが、来ることが決まっている。
    reserved_reader: bool,
    /// 使われているか。**両端が閉じ、予約も無ければ空く。**
    in_use: bool,
}

impl Pipe {
    const EMPTY: Self = Self {
        ring: Ring::EMPTY,
        readers: 0,
        writers: 0,
        reserved_reader: false,
        in_use: false,
    };

    fn release_if_unused(&mut self) {
        if self.readers == 0 && self.writers == 0 && !self.reserved_reader {
            *self = Self::EMPTY;
        }
    }
}

static PIPES: [Locked<Pipe>; MAX_PIPES] = [Locked::new(Pipe::EMPTY)];

/// 読み手が空で待った回数（計器）。
static READER_WAITS: AtomicU64 = AtomicU64::new(0);
/// 書き手が満杯で待った回数（計器）。
static WRITER_WAITS: AtomicU64 = AtomicU64::new(0);
/// 読み手の居ないパイプへ書こうとした回数（計器。`-EPIPE`）。
static EPIPE_SEEN: AtomicU64 = AtomicU64::new(0);
/// 作ったパイプの本数（計器）。
static CREATED: AtomicU64 = AtomicU64::new(0);
/// 待つ口が消した予約の数（計器）。**右が起きなかった回数である。**
static RESERVATIONS_DROPPED: AtomicU64 = AtomicU64::new(0);
/// 書きが起こした読み手の数（計器）。
///
/// **破壊 `pipe-write-does-not-wake-reader` はこれで落とす。** **止まる形では落ちなかった**
/// ——**読み手が「空」で待っている最中に書きが来る場面は `sleep 0.2 | cat` の末尾の 1 行だけで、
/// そこは書き手がすぐ閉じるので、閉じの起こしが書きの起こしを肩代わりして通る**（実測。
/// 3 回のうち 1 回しか落ちなかった）。**「誰が起こしたか」を数えれば、肩代わりは見える。**
static READERS_WOKEN_BY_WRITE: AtomicU64 = AtomicU64::new(0);

pub fn reader_waits() -> u64 {
    READER_WAITS.load(Ordering::Relaxed)
}

pub fn writer_waits() -> u64 {
    WRITER_WAITS.load(Ordering::Relaxed)
}

pub fn epipe_seen() -> u64 {
    EPIPE_SEEN.load(Ordering::Relaxed)
}

pub fn created() -> u64 {
    CREATED.load(Ordering::Relaxed)
}

pub fn reservations_dropped() -> u64 {
    RESERVATIONS_DROPPED.load(Ordering::Relaxed)
}

pub fn readers_woken_by_write() -> u64 {
    READERS_WOKEN_BY_WRITE.load(Ordering::Relaxed)
}

/// 読み手が空で待つことを数える。**待つのは呼び出し側である**（`syscall::sys_read`）。
pub fn note_reader_wait() {
    READER_WAITS.fetch_add(1, Ordering::Relaxed);
}

/// 書き手が満杯で待つことを数える。
pub fn note_writer_wait() {
    WRITER_WAITS.fetch_add(1, Ordering::Relaxed);
}

/// パイプを 1 本作る。**書き端が 1 つ付いた状態で返す。** **空いていなければ `None`。**
///
/// **`reserve_reader` が真なら、読み手が来ることを予約する**（モジュールの doc）。
///
/// 破壊 (`ADR-0063` の (b3), pipe-reader-not-reserved): 予約しない。**右が起きる前の
/// 左の書きが `-EPIPE` になる**——**`hello` が届かない。**
pub fn create(reserve_reader: bool) -> Option<u8> {
    for (index, slot) in PIPES.iter().enumerate() {
        let mut pipe = slot.lock();
        if pipe.in_use {
            continue;
        }
        *pipe = Pipe::EMPTY;
        pipe.in_use = true;
        pipe.writers = 1;
        #[cfg(not(feature = "pipe-reader-not-reserved"))]
        {
            pipe.reserved_reader = reserve_reader;
        }
        #[cfg(feature = "pipe-reader-not-reserved")]
        {
            let _ = reserve_reader;
        }
        CREATED.fetch_add(1, Ordering::Relaxed);
        return Some(index as u8);
    }
    None
}

/// 予約していた読み手が起きた。**予約を読み端 1 つへ変える。** **予約が無ければ偽。**
pub fn claim_reserved_reader(pipe: u8) -> bool {
    let Some(slot) = PIPES.get(pipe as usize) else {
        return false;
    };
    let mut pipe = slot.lock();
    if !pipe.in_use || !pipe.reserved_reader {
        return false;
    }
    pipe.reserved_reader = false;
    pipe.readers = pipe.readers.saturating_add(1);
    true
}

/// 使われなかった予約を消す。**消したら真。** **読み手が居なくなるので書き手を起こす**
/// （`-EPIPE` を見に行かせる）。
pub fn drop_reservation(pipe: u8) -> bool {
    let Some(slot) = PIPES.get(pipe as usize) else {
        return false;
    };
    let dropped = {
        let mut pipe = slot.lock();
        if !pipe.in_use || !pipe.reserved_reader {
            false
        } else {
            pipe.reserved_reader = false;
            pipe.release_if_unused();
            true
        }
    };
    if dropped {
        RESERVATIONS_DROPPED.fetch_add(1, Ordering::Relaxed);
        crate::task::wake_tasks_waiting_on(crate::task::Wait::PipeWritable { pipe });
    }
    dropped
}

/// 読んだ結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// これだけ取れた（1 以上）。
    Bytes(usize),
    /// 空で、書き手はもう居ない。
    Eof,
    /// 空だが、書き手がまだ居る。**待つこと。**
    Empty,
}

/// 溜まっているバイトを `dst` へ移す。**取れたら書き手を起こす**（空きができた）。
///
/// 破壊 (`ADR-0063` の (b3), pipe-read-empty-returns-zero): 空を EOF と誤る。
/// **読み手が途中で終わり、`hello` が欠ける。**
pub fn read_into(pipe: u8, dst: &mut [u8]) -> ReadOutcome {
    let Some(slot) = PIPES.get(pipe as usize) else {
        return ReadOutcome::Eof;
    };
    let outcome = {
        let mut state = slot.lock();
        if state.ring.is_empty() {
            #[cfg(feature = "pipe-read-empty-returns-zero")]
            {
                ReadOutcome::Eof
            }
            #[cfg(not(feature = "pipe-read-empty-returns-zero"))]
            if state.writers == 0 {
                ReadOutcome::Eof
            } else {
                ReadOutcome::Empty
            }
        } else {
            ReadOutcome::Bytes(state.ring.take(dst))
        }
    };
    if matches!(outcome, ReadOutcome::Bytes(_)) {
        crate::task::wake_tasks_waiting_on(crate::task::Wait::PipeWritable { pipe });
    }
    outcome
}

/// 書いた結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// これだけ入った（1 以上。**部分書きである**——`userlib::write_all` が回す）。
    Bytes(usize),
    /// 1 バイトも入らない。**待つこと。**
    Full,
    /// 読み手が居らず、来る予定も無い（`-EPIPE`）。
    NoReader,
}

/// `src` を入るだけ入れる。**入ったら読み手を起こす。**
///
/// 破壊 (`ADR-0063` の (b3), pipe-write-does-not-wake-reader): 起こさない。**閉じの起こしが
/// 肩代わりするので止まる形では落ちにくい**（[`READERS_WOKEN_BY_WRITE`] の doc）——**「書きが
/// 起こした読み手」の計器が 0 になることで落ちる。** **読み手が空で待っている最中に書き手が
/// 満杯まで書けば止まる形でも落ちる**（3 回のうち 1 回はそれで止まった。実測）。
///
/// 破壊 (`ADR-0063` の (b3), pipe-write-ignores-full): 満杯を見ずに上書きする。
/// **`/data/big` の中身が食い違う。**
pub fn write_from(pipe: u8, src: &[u8]) -> WriteOutcome {
    let Some(slot) = PIPES.get(pipe as usize) else {
        return WriteOutcome::NoReader;
    };
    let outcome = {
        let mut state = slot.lock();
        if state.readers == 0 && !state.reserved_reader {
            EPIPE_SEEN.fetch_add(1, Ordering::Relaxed);
            WriteOutcome::NoReader
        } else {
            #[cfg(not(feature = "pipe-write-ignores-full"))]
            let put = state.ring.put(src);
            #[cfg(feature = "pipe-write-ignores-full")]
            let put = state.ring.put_overwriting(src);
            if put == 0 {
                WriteOutcome::Full
            } else {
                WriteOutcome::Bytes(put)
            }
        }
    };
    #[cfg(not(feature = "pipe-write-does-not-wake-reader"))]
    if matches!(outcome, WriteOutcome::Bytes(_)) {
        let woken = crate::task::wake_tasks_waiting_on(crate::task::Wait::PipeReadable { pipe });
        READERS_WOKEN_BY_WRITE.fetch_add(woken as u64, Ordering::Relaxed);
    }
    outcome
}

/// 読み端を 1 つ閉じる。**最後の読み手なら書き手を起こす**（`-EPIPE` を見に行かせる）。
pub fn close_read_end(pipe: u8) {
    let Some(slot) = PIPES.get(pipe as usize) else {
        return;
    };
    let last = {
        let mut state = slot.lock();
        if !state.in_use || state.readers == 0 {
            false
        } else {
            state.readers -= 1;
            let last = state.readers == 0;
            state.release_if_unused();
            last
        }
    };
    if last {
        crate::task::wake_tasks_waiting_on(crate::task::Wait::PipeWritable { pipe });
    }
}

/// 書き端を 1 つ閉じる。**最後の書き手なら読み手を起こす**（EOF を見に行かせる）。
///
/// 破壊 (`ADR-0063` の (b3), pipe-close-keeps-writer-count): 数を減らさない。
/// **EOF が来ず、読み手が永久に待つ。**
pub fn close_write_end(pipe: u8) {
    let Some(slot) = PIPES.get(pipe as usize) else {
        return;
    };
    let last = {
        let mut state = slot.lock();
        if !state.in_use || state.writers == 0 {
            false
        } else {
            #[cfg(not(feature = "pipe-close-keeps-writer-count"))]
            {
                state.writers -= 1;
            }
            let last = state.writers == 0;
            state.release_if_unused();
            last
        }
    };
    if last {
        crate::task::wake_tasks_waiting_on(crate::task::Wait::PipeReadable { pipe });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // **輪の計算は `crate::ring` が持ち、そこで検査する。** **ここは端の数と予約だけを見る**
    // （`Locked` と起こしはホストでは動かない）。
    #[test]
    fn the_pipe_frees_itself_only_when_both_ends_and_the_reservation_are_gone() {
        let mut pipe = Pipe::EMPTY;
        pipe.in_use = true;
        pipe.reserved_reader = true;
        pipe.readers = 0;
        pipe.writers = 0;
        pipe.release_if_unused();
        assert!(pipe.in_use, "a reservation keeps the pipe");
        pipe.reserved_reader = false;
        pipe.release_if_unused();
        assert!(!pipe.in_use);
    }
}
