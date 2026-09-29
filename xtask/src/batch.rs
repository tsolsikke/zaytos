//! 全検査の表の行を、k 本ずつ同時に走らせる（2026-09-29。SCRUM-31。案 B の ④）。
//!
//! **表ごとに並べる**——全検査の表のループ（`cmd_check` の `for … in 表`）が、行ごとの項目を [`Job`] にして渡す。
//! **全体の予定の一覧は作らない。** 表の中で同時に走らせない行（`main.rs` の `NOT_CONCURRENT`）は、並べた行が
//! 全部終わってから、呼んだ糸で順に走らせる。
//!
//! **並べた行は、それぞれ自分の糸で、自分の塊を持って走る**（`crate::item_log`）。項目の状態（出力の写し・時計・
//! 失敗の分け方・QEMU の実行の記録・計数）は糸ごとに在るので、項目の中の判定は、順に回したときと同じ形で読む。
//! **項目が終わったら、塊をまとめて書く**（終わった順）。落ちて巻き戻るときも書く。
//!
//! **QEMU の vCPU の数の上限は、起動の入口が持つ**（`crate::launch`）——`-smp 2`・`4` の回は、vCPU の数で数えて
//! 並べる数を抑える。

use std::collections::VecDeque;
use std::sync::Mutex;

/// 表の 1 行の項目。
pub struct Job<'a, R> {
    /// 同時に走らせてよいか（同時に走らせない表に載っていなければ真）。
    pub concurrent: bool,
    pub run: Box<dyn FnOnce() -> R + Send + 'a>,
}

/// `threads` 本の糸で並べて走らせ、表の順に結果を返す。
///
/// **`stop` が真を返したら、まだ始めていない行は始めない**（`None` のまま返す。全検査の上限）。**`threads` が 1 以下
/// なら、全部をこの糸で順に走らせる**（塊を持たない。順に回したときと同じ出力になる）。
pub fn run<'a, R: Send>(
    jobs: Vec<Job<'a, R>>,
    threads: usize,
    stop: &(dyn Fn() -> bool + Sync),
) -> Vec<Option<R>> {
    let mut results: Vec<Option<R>> = jobs.iter().map(|_| None).collect();
    let mut together = VecDeque::new();
    let mut alone = Vec::new();
    for (index, job) in jobs.into_iter().enumerate() {
        if threads > 1 && job.concurrent {
            together.push_back((index, job.run));
        } else {
            alone.push((index, job.run));
        }
    }
    if !together.is_empty() {
        let workers = threads.min(together.len());
        let queue = Mutex::new(together);
        let done: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    if stop() {
                        return;
                    }
                    let Some((index, run)) =
                        queue.lock().ok().and_then(|mut queue| queue.pop_front())
                    else {
                        return;
                    };
                    crate::item_log::begin();
                    let block = WriteTheBlock;
                    let result = run();
                    drop(block);
                    if let Ok(mut done) = done.lock() {
                        done.push((index, result));
                    }
                });
            }
        });
        for (index, result) in done.into_inner().unwrap_or_default() {
            results[index] = Some(result);
        }
    }
    for (index, run) in alone {
        if stop() {
            break;
        }
        results[index] = Some(run());
    }
    results
}

/// 項目の塊をまとめて書く（**落ちて巻き戻るときも書く**——項目の途中までの出力を失わない）。
struct WriteTheBlock;

impl Drop for WriteTheBlock {
    fn drop(&mut self) {
        if let Some(text) = crate::item_log::take() {
            crate::item_log::write(&text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn job<'a>(concurrent: bool, run: impl FnOnce() -> usize + Send + 'a) -> Job<'a, usize> {
        Job {
            concurrent,
            run: Box::new(run),
        }
    }

    /// **結果は表の順に返る。** **同時に走らせない行は、並べた行の後に、呼んだ糸で走る**（塊を持たない）。
    #[test]
    fn results_come_back_in_the_order_of_the_table() {
        let caller = std::thread::current().id();
        let alone_on_the_caller = AtomicUsize::new(0);
        let jobs = vec![
            job(true, || 10),
            job(false, || {
                if std::thread::current().id() == caller && crate::item_log::sink().is_none() {
                    alone_on_the_caller.fetch_add(1, Ordering::SeqCst);
                }
                11
            }),
            job(true, || 12),
            job(true, || 13),
        ];
        let results = run(jobs, 2, &|| false);
        assert_eq!(results, vec![Some(10), Some(11), Some(12), Some(13)]);
        assert_eq!(alone_on_the_caller.load(Ordering::SeqCst), 1);
    }

    /// **並べた行は、自分の糸で塊を持って走る。** **糸が 1 本なら、全部を呼んだ糸で、塊を持たずに走る。**
    #[test]
    fn a_concurrent_row_holds_its_own_block_only_when_rows_are_spread() {
        let spread = run(
            vec![job(true, || usize::from(crate::item_log::sink().is_some()))],
            2,
            &|| false,
        );
        assert_eq!(spread, vec![Some(1)]);
        let one_thread = run(
            vec![job(true, || usize::from(crate::item_log::sink().is_some()))],
            1,
            &|| false,
        );
        assert_eq!(one_thread, vec![Some(0)]);
    }

    /// **止める合図が立ったら、まだ始めていない行は始めない**（全検査の上限）。
    #[test]
    fn rows_not_yet_started_are_left_when_told_to_stop() {
        let started = AtomicUsize::new(0);
        let stop = || started.load(Ordering::SeqCst) >= 1;
        let jobs = vec![
            job(false, || {
                started.fetch_add(1, Ordering::SeqCst);
                1
            }),
            job(false, || 2),
        ];
        let results = run(jobs, 1, &stop);
        assert_eq!(results, vec![Some(1), None]);
    }
}
