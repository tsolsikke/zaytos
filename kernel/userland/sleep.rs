//! `sleep`（W2-d+。`ADR-0062`）。**指定した秒数だけ眠る。**
//!
//! # タイマで起こされる最初の利用者である
//!
//! **`nanosleep` で眠り、前後で `clock_gettime(CLOCK_MONOTONIC)` を読む。**
//! **時刻の口を Ring 3 から通る最初の利用者でもある**（`ADR-0062` の (a) の限界）。
//!
//! # 書き出す行
//!
//! **`sleep: asked N ms, the monotonic clock advanced M ms`** ——**判定は M が N 以上で
//! あることを見る**（`xtask` の `--shell-test`）。**時間を外から測らない**——**比べるのは
//! カーネルが答えた 2 つの時刻である。**
//!
//! # 引数
//!
//! **秒を 10 進で受ける。** **小数点の後は 9 桁まで**（`0.5` など）。**単位の接尾辞は受けない。**
//!
//! # 終了状態の意味
//!
//! - `0` 眠り、時刻が求めた長さ以上に進んだ
//! - `1` 時刻の進みが求めた長さより短かった
//! - `2` 時刻が戻った（後に読んだ値が前より小さい）
//! - `3` `nanosleep` か `clock_gettime` が失敗した
//! - `4` 引数が読めなかった

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, length_of, syscall3, write_all, STDOUT};

/// `nanosleep`（Linux x86-64 の番号）。
const SYS_NANOSLEEP: u64 = 35;
/// `clock_gettime`（Linux x86-64 の番号）。
const SYS_CLOCK_GETTIME: u64 = 228;
/// `CLOCK_MONOTONIC`（Linux x86-64 の値）。
const CLOCK_MONOTONIC: u64 = 1;
/// 1 秒のナノ秒。
const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// 1 ミリ秒のナノ秒。
const NANOS_PER_MILLISECOND: u64 = 1_000_000;
/// 引数として読む上限。
const ARG_MAX: usize = 64;

/// `struct timespec`（Linux x86-64 の配置。`tv_sec` と `tv_nsec` がともに 8 バイト）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

impl Timespec {
    fn total_nanos(self) -> u64 {
        (self.tv_sec as u64)
            .wrapping_mul(NANOS_PER_SECOND)
            .wrapping_add(self.tv_nsec as u64)
    }
}

/// 単調な時刻を読む。
fn monotonic_now() -> Option<Timespec> {
    let mut now = Timespec::default();
    // SAFETY: `now` は 16 バイトの書ける領域で、カーネルはその範囲だけを書く。
    let result = unsafe {
        syscall3(
            SYS_CLOCK_GETTIME,
            CLOCK_MONOTONIC,
            &mut now as *mut Timespec as u64,
            0,
        )
    };
    (result == 0).then_some(now)
}

/// `1` や `0.5` を秒として読み、ナノ秒で返す。
fn parse_seconds(text: &[u8]) -> Option<u64> {
    let mut parts = text.splitn(2, |&byte| byte == b'.');
    let whole = parts.next()?;
    if whole.is_empty() && text.first() != Some(&b'.') {
        return None;
    }
    let mut seconds = 0u64;
    for &byte in whole {
        if !byte.is_ascii_digit() {
            return None;
        }
        seconds = seconds.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    let mut nanos = 0u64;
    if let Some(fraction) = parts.next() {
        if fraction.len() > 9 {
            return None;
        }
        let mut scale = NANOS_PER_SECOND / 10;
        for &byte in fraction {
            if !byte.is_ascii_digit() {
                return None;
            }
            nanos += u64::from(byte - b'0') * scale;
            scale /= 10;
        }
    }
    seconds.checked_mul(NANOS_PER_SECOND)?.checked_add(nanos)
}

/// 10 進で書き出す。
fn decimal(value: u64, out: &mut [u8; 20]) -> &[u8] {
    let mut at = out.len();
    let mut rest = value;
    loop {
        at -= 1;
        out[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    &out[at..]
}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(stack: *const u64) -> ! {
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let Some(pointer) = (unsafe { userlib::argument(stack, 1) }) else {
        write_all(STDOUT, b"sleep: usage: sleep SECONDS\n");
        exit(4);
    };
    // SAFETY: `argv` の要素はカーネルが NUL 終端で積んだ文字列である。
    let length = unsafe { length_of(pointer, ARG_MAX) };
    // SAFETY: 上で数えた長さは NUL の手前までで、同じ割り当ての中である。
    let text = unsafe { core::slice::from_raw_parts(pointer, length) };
    let Some(asked) = parse_seconds(text) else {
        write_all(STDOUT, b"sleep: invalid time interval\n");
        exit(4);
    };

    let Some(before) = monotonic_now() else {
        exit(3);
    };
    let request = Timespec {
        tv_sec: (asked / NANOS_PER_SECOND) as i64,
        tv_nsec: (asked % NANOS_PER_SECOND) as i64,
    };
    // SAFETY: `request` は 16 バイトの読める領域である。`rem` は渡さない（0）。
    let slept = unsafe { syscall3(SYS_NANOSLEEP, &request as *const Timespec as u64, 0, 0) };
    if slept != 0 {
        exit(3);
    }
    let Some(after) = monotonic_now() else {
        exit(3);
    };

    let (before, after) = (before.total_nanos(), after.total_nanos());
    if after < before {
        write_all(STDOUT, b"sleep: the monotonic clock went backwards\n");
        exit(2);
    }
    let advanced = after - before;

    let mut buffer = [0u8; 20];
    write_all(STDOUT, b"sleep: asked ");
    write_all(STDOUT, decimal(asked / NANOS_PER_MILLISECOND, &mut buffer));
    write_all(STDOUT, b" ms, the monotonic clock advanced ");
    write_all(STDOUT, decimal(advanced / NANOS_PER_MILLISECOND, &mut buffer));
    write_all(STDOUT, b" ms\n");
    exit(if advanced >= asked { 0 } else { 1 });
}
