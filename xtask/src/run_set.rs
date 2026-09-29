//! 試験の一覧を、k 本ずつ同時に走らせる（案 B の ②。2026-09-29。`cargo xtask run-set`）。
//!
//! **手で順に回していた試験の一覧**（`target/boundary-step2/run_*.sh` の中身のような、`cargo xtask run …` の並び）を
//! 受け取り、子のプロセスとして k 本ずつ走らせる。**回ごとの置き場（`RunDir`）があるので、同時に走らせても互いの
//! ESP・ディスク・ログを書き換えない。** kernel の ELF とブートローダの EFI は組ごとの写しから取る（ビルドから写しまでを
//! 錠の中で行う）ので、ほかの組のビルドと取り違えない。
//!
//! # 一覧の形
//!
//! 1 行に、`cargo xtask` に渡す引数を 1 つ分書く（例: `run --smp-ap-test ap-timer`）。空の行と `#` で始まる行は
//! 読まない。引数は空白で分ける（引用符は読まない）。
//!
//! **行の頭に `!` を書いた行は、落ちるのが正しい行である**（破壊テストなど。例: `!run --virtio-irq-test --sabotage
//! virtio-skip-eoi-test`）。落ちたら通過と数え、**通ったら落ちと数える**——落ちるはずの行が通ったのは、破壊テストが
//! 働いていないことだからである（2026-09-29。運用者の決定）。
//!
//! # 出力
//!
//! 子の標準出力と標準エラーは、子ごとの回の置き場の `xtask.log` に残す（子が QEMU を起動するときは、その回の
//! 置き場がさらに別に取られ、`(info) run` の行がこの記録に出る）。**終わりに、一覧の順に 1 行ずつ結果を並べる。**
//!
//! # 止め方
//!
//! **子は自分のプロセスの組で走る。** 端末の Ctrl-C は run-set だけを止め、走っている子はそれぞれの期限まで走って、
//! 自分の QEMU を片付けてから終わる（子を端末の組に置くと、子が片付けずに終わり、自分の組で走る QEMU が残る）。
//! **1 本が上限（[`PER_TEST_LIMIT`]）を越えたら、子の組を止めてから、子孫（自分の組で走る QEMU を含む）を
//! `/proc` で辿って止める。**
//!
//! # 全検査との関係
//!
//! **全検査の中で表の行を同時に走らせる形は、全検査の側にある**（`crate::batch`）。全検査の間は、子が QEMU を
//! 起動するところで検査の錠が断る（終了の値 75）。

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::run_dir::RunDir;

/// 1 本の試験の上限（越えたら子を止める。上限の無い待ちを書かない）。
const PER_TEST_LIMIT: Duration = Duration::from_secs(30 * 60);

/// 子を見に行く間隔。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 一覧の 1 本（純粋な論理）。
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    /// 一覧に書いたとおりの行（結果の行に出す）。
    pub line: String,
    pub args: Vec<String>,
    /// 落ちるのが正しい行か（行の頭の `!`）。
    pub expect_failure: bool,
}

/// 一覧を読む（純粋な論理）。
pub fn parse_list(text: &str) -> Vec<Entry> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (expect_failure, command) = match line.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            Entry {
                line: line.to_string(),
                args: command.split_whitespace().map(str::to_string).collect(),
                expect_failure,
            }
        })
        .collect()
}

/// 1 本の判定（純粋な論理）。**落ちるのが正しい行は、落ちたら通過、通ったら落ちである。** シグナルで終わった行と、
/// 上限を越えて止めた行は、どちらの行でも落ちとする（終わった理由が試験の判定ではない）。
fn verdict(outcome: &Outcome, expect_failure: bool) -> (bool, String) {
    match (outcome, expect_failure) {
        (Outcome::Exited(0), false) => (true, "PASS".to_string()),
        (Outcome::Exited(code), false) => (false, format!("FAIL (exit {code})")),
        (Outcome::Exited(0), true) => (false, "FAIL (exit 0, but it must fail)".to_string()),
        (Outcome::Exited(code), true) => (true, format!("PASS (failed as it must, exit {code})")),
        (Outcome::Signalled, _) => (false, "FAIL (killed by a signal)".to_string()),
        (Outcome::TimedOut, _) => (
            false,
            format!("FAIL (over {} s; stopped)", PER_TEST_LIMIT.as_secs()),
        ),
    }
}

/// 1 本の結果。
enum Outcome {
    Exited(i32),
    Signalled,
    TimedOut,
}

struct Running {
    index: usize,
    child: Child,
    started: Instant,
    log: PathBuf,
    _run: RunDir,
}

struct Finished {
    outcome: Outcome,
    took: Duration,
    log: PathBuf,
}

/// `cargo xtask run-set [--jobs K] <一覧のファイル>`。
pub fn cmd_run_set(workspace_root: &Path, args: &[String]) -> Result<()> {
    let mut jobs = 1usize;
    let mut list = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--jobs" => {
                index += 1;
                let value = args.get(index).context("--jobs needs a number")?;
                jobs = value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n >= 1)
                    .with_context(|| {
                        format!("--jobs takes a number of 1 or more, not {value:?}")
                    })?;
            }
            other if other.starts_with('-') => {
                bail!(
                    "run-set does not take {other:?} (cargo xtask run-set [--jobs K] <list file>)"
                )
            }
            other => {
                if list.replace(PathBuf::from(other)).is_some() {
                    bail!("run-set takes one list file");
                }
            }
        }
        index += 1;
    }
    let list =
        list.context("run-set needs a list file (one `cargo xtask` argument list per line)")?;
    let text =
        fs::read_to_string(&list).with_context(|| format!("could not read {}", list.display()))?;
    let entries = parse_list(&text);
    if entries.is_empty() {
        bail!("{} lists no test", list.display());
    }
    let program =
        std::env::current_exe().context("could not find the xtask binary to run the tests")?;
    println!(
        "run-set: {} test(s) from {}, {jobs} at a time",
        entries.len(),
        list.display()
    );
    let started = Instant::now();
    let mut results: Vec<Option<Finished>> = entries.iter().map(|_| None).collect();
    let mut running: Vec<Running> = Vec::new();
    let mut next = 0;
    while next < entries.len() || !running.is_empty() {
        while running.len() < jobs && next < entries.len() {
            running.push(start(workspace_root, &program, next, &entries[next])?);
            next += 1;
        }
        std::thread::sleep(POLL_INTERVAL);
        let mut still = Vec::new();
        for mut test in running.drain(..) {
            let took = test.started.elapsed();
            let outcome = match test.child.try_wait() {
                Ok(Some(status)) => Some(match status.code() {
                    Some(code) => Outcome::Exited(code),
                    None => Outcome::Signalled,
                }),
                Ok(None) if took > PER_TEST_LIMIT => {
                    stop_tree(&mut test.child);
                    Some(Outcome::TimedOut)
                }
                Ok(None) => None,
                Err(_) => Some(Outcome::Signalled),
            };
            match outcome {
                Some(outcome) => {
                    println!(
                        "run-set: finished [{}] {} ({:.1}s)",
                        test.index + 1,
                        entries[test.index].line,
                        took.as_secs_f64()
                    );
                    results[test.index] = Some(Finished {
                        outcome,
                        took,
                        log: test.log.clone(),
                    });
                }
                None => still.push(test),
            }
        }
        running = still;
    }
    let wall = started.elapsed();
    println!("=== run-set results (in the order of the list)");
    let mut failed = 0;
    let mut sum = Duration::ZERO;
    for (entry, result) in entries.iter().zip(results.iter()) {
        let Some(result) = result else {
            continue;
        };
        sum += result.took;
        let (passed, verdict) = verdict(&result.outcome, entry.expect_failure);
        if !passed {
            failed += 1;
        }
        println!(
            "{verdict:<28} {:>7.1}s  {}  ({})",
            result.took.as_secs_f64(),
            entry.line,
            result.log.display()
        );
    }
    println!(
        "run-set: {} test(s), {} passed, {failed} failed; wall {:.1}s, the tests' own time summed {:.1}s, {jobs} at a time",
        entries.len(),
        entries.len() - failed,
        wall.as_secs_f64(),
        sum.as_secs_f64()
    );
    if failed > 0 {
        bail!("run-set: {failed} test(s) failed");
    }
    Ok(())
}

/// 子を 1 本起こす。**子は自分の組で走らせる**（モジュールの doc の「止め方」）。
fn start(workspace_root: &Path, program: &Path, index: usize, entry: &Entry) -> Result<Running> {
    let run = RunDir::create(workspace_root, &format!("run-set: {}", entry.line))?;
    let log = run.file("xtask.log");
    let out = File::create(&log).with_context(|| format!("could not create {}", log.display()))?;
    let err = out
        .try_clone()
        .context("could not share the log between stdout and stderr")?;
    let child = {
        use std::os::unix::process::CommandExt;
        Command::new(program)
            .args(&entry.args)
            .current_dir(workspace_root)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .process_group(0)
            .spawn()
            .with_context(|| format!("could not start `xtask {}`", entry.line))?
    };
    println!(
        "run-set: started [{}] {} -> {}",
        index + 1,
        entry.line,
        log.display()
    );
    Ok(Running {
        index,
        child,
        started: Instant::now(),
        log,
        _run: run,
    })
}

/// 上限を越えた子を止める。**子の組を止めて（SIGSTOP）から子孫を辿り、深い方から SIGKILL を送る**——子の組が
/// 止まっているので、辿った後に子孫が増えない。**自分の組で走る QEMU は子の組に居ないので、辿って止める。**
fn stop_tree(child: &mut Child) {
    let pid = child.id();
    let group = format!("-{pid}");
    let _ = Command::new("kill").args(["-STOP", "--", &group]).status();
    let below = tree_below(pid, &parents());
    for id in &below {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &id.to_string()])
            .status();
    }
    let _ = Command::new("kill").args(["-KILL", "--", &group]).status();
    let _ = child.wait();
    println!(
        "run-set: stopped pid {pid} and {} process(es) below it",
        below.len()
    );
}

/// いま在るプロセスの `(pid, 親の pid)`（`/proc/<pid>/stat` を読む。読めないものは飛ばす）。
fn parents() -> Vec<(u32, u32)> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            Some((pid, parent_in_stat(&stat)?))
        })
        .collect()
}

/// `/proc/<pid>/stat` の親の pid（純粋な論理）。**名前は括弧の中で空白や括弧を含みうるので、最後の `)` の後から読む。**
fn parent_in_stat(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// `root` の子孫の pid を、深い方から並べる（純粋な論理）。
fn tree_below(root: u32, parents: &[(u32, u32)]) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for &(pid, ppid) in parents {
            if ppid == parent && pid != root && !found.contains(&pid) {
                found.push(pid);
                frontier.push(pid);
            }
        }
    }
    found.reverse();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **空の行と `#` の行は読まず、1 行を空白で引数に分ける。**
    #[test]
    fn a_list_is_read_one_test_per_line() {
        let entries =
            parse_list("# 9f の確かめ\n\nrun --acpi-smp-test\n  run --smp-ap-test ap-timer  \n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].args, vec!["run", "--acpi-smp-test"]);
        assert_eq!(entries[1].line, "run --smp-ap-test ap-timer");
        assert_eq!(entries[1].args, vec!["run", "--smp-ap-test", "ap-timer"]);
        assert!(!entries[1].expect_failure);
    }

    /// **行の頭の `!` は、落ちるのが正しい行である。** **落ちたら通過、通ったら落ち。** シグナルと上限は、どちらでも落ち。
    #[test]
    fn a_row_marked_to_fail_passes_only_when_it_fails() {
        let entries = parse_list("!run --pipe-test --sabotage pipe-reader-not-reserved\n");
        assert!(entries[0].expect_failure);
        assert_eq!(
            entries[0].args,
            vec![
                "run",
                "--pipe-test",
                "--sabotage",
                "pipe-reader-not-reserved"
            ]
        );
        assert_eq!(
            entries[0].line,
            "!run --pipe-test --sabotage pipe-reader-not-reserved"
        );
        assert!(verdict(&Outcome::Exited(1), true).0);
        assert!(!verdict(&Outcome::Exited(0), true).0);
        assert!(verdict(&Outcome::Exited(0), false).0);
        assert!(!verdict(&Outcome::Exited(1), false).0);
        assert!(!verdict(&Outcome::Signalled, true).0);
        assert!(!verdict(&Outcome::TimedOut, true).0);
    }

    /// **親の pid は名前の括弧の後から読む。** **子孫は深い方から並び、自分の組で走る QEMU も入る。**
    #[test]
    fn the_processes_below_a_child_are_listed_deepest_first() {
        assert_eq!(parent_in_stat("4242 (xtask) S 4000 4242 0"), Some(4000));
        assert_eq!(parent_in_stat("4243 (a (b) c) R 4242 4242 0"), Some(4242));
        assert_eq!(parent_in_stat("no parenthesis"), None);
        // 4242（子）の下に cargo 4243、その下に rustc 4244。QEMU 4250 は自分の組だが親は 4242。4300 は関係が無い。
        let parents = [
            (4242, 4000),
            (4243, 4242),
            (4244, 4243),
            (4250, 4242),
            (4300, 1),
        ];
        let below = tree_below(4242, &parents);
        assert_eq!(below.len(), 3, "{below:?}");
        let at = |pid: u32| below.iter().position(|&p| p == pid);
        assert!(at(4244).unwrap() < at(4243).unwrap(), "{below:?}");
        assert!(at(4250).is_some(), "{below:?}");
        assert!(at(4300).is_none(), "{below:?}");
    }
}
