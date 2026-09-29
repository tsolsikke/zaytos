//! 検査の時間の計測（2026-09-24。検査の体系の見直しの 7.(1)。**計測のためだけの実行を増やさない**——
//! 完了時の `--full` に相乗りさせる）。
//!
//! **項目ごとに、ビルド・イメージの準備・外の道具・固定の待ち・空回りする待ちの回数と時間を数える。** QEMU の実行は
//! 起動の入口（`launch`）が数える。**「項目の数」「ビルドの回数」「VM の起動の回数」を分けて数える**
//! ——**ビルドは `cargo` を呼んだ回数で、中身が変わらず何もしなかった回も 1 回と数える**（時間は
//! その分短い）。**共有の準備（同じイメージを使い回す等）は、準備した項目にだけ数える。**

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// 数える種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `cargo build`（カーネル・ブートローダ）。
    Build,
    /// イメージの準備（ESP の組み立て・起動媒体のイメージ・OVMF の変数のコピー）。
    Stage,
    /// 外の道具（`debugfs`・`e2fsck`・`objdump`・`sfdisk` 等）。
    Tool,
    /// 固定の待ち（決まった時間眠る）。
    FixedWait,
    /// 空回りする待ち（条件を見ては短く眠る。その眠りの合計）。
    PollWait,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Build,
        Kind::Stage,
        Kind::Tool,
        Kind::FixedWait,
        Kind::PollWait,
    ];

    fn label(self) -> &'static str {
        match self {
            Kind::Build => "build",
            Kind::Stage => "stage",
            Kind::Tool => "tools",
            Kind::FixedWait => "fixed waits",
            Kind::PollWait => "poll waits",
        }
    }
}

/// 回数と時間（ナノ秒）の対。
struct Counter {
    count: AtomicU64,
    nanos: AtomicU64,
}

impl Counter {
    const fn new() -> Self {
        Counter {
            count: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        }
    }

    fn add(&self, elapsed: Duration) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.nanos.fetch_add(
            elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::SeqCst,
        );
    }

    fn read(&self) -> (u64, Duration) {
        (
            self.count.load(Ordering::SeqCst),
            Duration::from_nanos(self.nanos.load(Ordering::SeqCst)),
        )
    }
}

std::thread_local! {
    /// いまの項目の分（`finish_item` が読んで空にする）。**項目を走らせる糸ごとに持つ**（2026-09-29。SCRUM-31）——同時に走る項目の計数を混ぜない。
    static ITEM: std::cell::RefCell<[(u64, Duration); 5]> =
        const { std::cell::RefCell::new([(0, Duration::ZERO); 5]) };
}
/// 全体の分。
static TOTAL: [Counter; 5] = [const { Counter::new() }; 5];

/// 1 回分を足す。
pub fn record(kind: Kind, elapsed: Duration) {
    ITEM.with(|item| {
        let slot = &mut item.borrow_mut()[kind as usize];
        slot.0 += 1;
        slot.1 = slot.1.saturating_add(elapsed);
    });
    TOTAL[kind as usize].add(elapsed);
}

/// 関数を走らせ、その時間を足す。
pub fn timed<T>(kind: Kind, body: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let value = body();
    record(kind, started.elapsed());
    value
}

/// 固定の待ち（決まった時間眠る）。
pub fn sleep_fixed(duration: Duration) {
    timed(Kind::FixedWait, || std::thread::sleep(duration));
}

/// 空回りする待ちの 1 回分の眠り。
pub fn sleep_poll(duration: Duration) {
    timed(Kind::PollWait, || std::thread::sleep(duration));
}

/// 数を 1 行に並べる（純粋な論理）。**0 回の種類も出す**——出ないと「数えていない」と区別できない。
fn line(values: &[(u64, Duration); 5]) -> String {
    Kind::ALL
        .iter()
        .zip(values)
        .map(|(kind, (count, time))| {
            format!("{} {count} ({:.1}s)", kind.label(), time.as_secs_f64())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// いまの項目の分を 1 行にして、空にする。
pub fn take_item_line() -> String {
    let values = ITEM.with(|item| std::mem::take(&mut *item.borrow_mut()));
    line(&values)
}

/// 全体の分の時間（検査の記録へ残す。`cargo` の時間は遅さの計測が差し引く）。
pub fn total_time(kind: Kind) -> Duration {
    TOTAL[kind as usize].read().1
}

/// 全体の分を 1 行にする。
pub fn total_line() -> String {
    let values = [0, 1, 2, 3, 4].map(|index| TOTAL[index].read());
    line(&values)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **0 回の種類も出す**（出ないと「数えていない」と区別できない）。
    #[test]
    fn the_line_names_every_kind_even_at_zero() {
        let values = [
            (2, Duration::from_millis(1500)),
            (0, Duration::ZERO),
            (1, Duration::from_millis(300)),
            (0, Duration::ZERO),
            (40, Duration::from_secs(4)),
        ];
        assert_eq!(
            line(&values),
            "build 2 (1.5s), stage 0 (0.0s), tools 1 (0.3s), fixed waits 0 (0.0s), poll waits 40 (4.0s)"
        );
    }
}
