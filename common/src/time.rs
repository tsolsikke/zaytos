//! 単調な時刻とティックの換算（W2-d+。`ADR-0062`）。
//!
//! **ハードに依らない純粋な論理である**——**周波数は引数で受け取る。** **カーネルの
//! `clock_gettime` と `nanosleep` が使い、ホストのテストで固定する**
//! （`CLAUDE.md` の「ハードウェア依存部と純粋ロジックを分離」）。

/// 1 秒のナノ秒。
pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// `timespec` の中身が範囲の外である（`nanosleep` が `-EINVAL` を返す場合）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidTimespec;

/// ティック数を秒とナノ秒へ換算する（`clock_gettime` の答え）。
///
/// **ナノ秒は常に `NANOS_PER_SECOND` 未満である。**
pub fn timespec_from_ticks(ticks: u64, hz: u64) -> (u64, u64) {
    let seconds = ticks / hz;
    let nanos = (ticks % hz) * (NANOS_PER_SECOND / hz);
    (seconds, nanos)
}

/// 秒とナノ秒を、待つティック数へ換算する（`nanosleep` の締切）。
///
/// # 切り上げる
///
/// **求められた長さより短く眠ってはいけない**（POSIX の `nanosleep` の約束）。
/// **1 ティックに満たない端数は 1 ティックへ切り上げる。**
///
/// # 範囲
///
/// **秒が負、またはナノ秒が 0 未満か `NANOS_PER_SECOND` 以上なら範囲の外である**
/// （Linux の `nanosleep(2)` が `EINVAL` を返す条件）。**掛け算が溢れる長さも範囲の外にする。**
pub fn ticks_for_duration(seconds: i64, nanos: i64, hz: u64) -> Result<u64, InvalidTimespec> {
    if seconds < 0 || nanos < 0 || nanos as u64 >= NANOS_PER_SECOND {
        return Err(InvalidTimespec);
    }
    let nanos_per_tick = NANOS_PER_SECOND / hz;
    let whole = (seconds as u64).checked_mul(hz).ok_or(InvalidTimespec)?;
    let partial = (nanos as u64).div_ceil(nanos_per_tick);
    whole.checked_add(partial).ok_or(InvalidTimespec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_become_seconds_and_nanos() {
        assert_eq!(timespec_from_ticks(0, 100), (0, 0));
        assert_eq!(timespec_from_ticks(4382, 100), (43, 820_000_000));
        assert_eq!(timespec_from_ticks(99, 100), (0, 990_000_000));
    }

    #[test]
    fn whole_seconds_become_exact_ticks() {
        assert_eq!(ticks_for_duration(1, 0, 100), Ok(100));
        assert_eq!(ticks_for_duration(0, 0, 100), Ok(0));
    }

    #[test]
    fn a_partial_tick_rounds_up_so_the_sleep_is_never_short() {
        assert_eq!(ticks_for_duration(0, 1, 100), Ok(1));
        assert_eq!(ticks_for_duration(0, 10_000_000, 100), Ok(1));
        assert_eq!(ticks_for_duration(0, 10_000_001, 100), Ok(2));
        assert_eq!(ticks_for_duration(0, 500_000_000, 100), Ok(50));
    }

    #[test]
    fn out_of_range_values_are_refused() {
        assert_eq!(ticks_for_duration(-1, 0, 100), Err(InvalidTimespec));
        assert_eq!(ticks_for_duration(0, -1, 100), Err(InvalidTimespec));
        assert_eq!(
            ticks_for_duration(0, NANOS_PER_SECOND as i64, 100),
            Err(InvalidTimespec)
        );
        assert_eq!(ticks_for_duration(i64::MAX, 0, 100), Err(InvalidTimespec));
    }
}
