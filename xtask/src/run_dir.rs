//! 回ごとの置き場（案 B の ①。2026-09-29）。
//!
//! **1 回の QEMU の実行が読み書きするもの**——ESP・`disk0.img`・OVMF の変数の写し・シリアルのログ・`qemu-debug.log`・
//! 画面の写し・一時ファイル・monitor のソケット——**を、回ごとの置き場 `target/runs/<回の番号>/` に置く。**
//! 以前は `target/` の下の決まった名前に置いていたので、2 つの実行を同時に走らせると、互いの ESP やログを
//! 書き換えた。**置き場の名前を組み立てるのは、この型（[`RunDir`]）だけである**——基本の検査が、ほかの所に
//! 決まった名前が無いことを数える（`fixed_run_names`）。
//!
//! # 回の番号
//!
//! `target/runs/` の下にある番号の最大に 1 を足し、`mkdir` で取る。**`mkdir` は、取れるか取れないかが 1 度で
//! 決まる**ので、同時に走るほかのプロセスとは別の番号になる（取れなければ次の番号を試す）。
//!
//! # 使っている間はロックを持つ
//!
//! 置き場の `lock` を flock で持つ（プロセスが終われば、殺された場合も、カーネルが放す）。**古い置き場を
//! 片付けるときは、ロックが取れた置き場だけを触る**——走っている実行の置き場は消さない。
//!
//! # 残す量
//!
//! 新しい順に [`KEEP_WHOLE`] 個は丸ごと残す（以前の決まった名前と同じく、直前の実行の像とログを後から見られる）。
//! それより古いものは、ログ（`.log`）と説明（`run.txt`）だけを残し、像と一時ファイルを消す。[`KEEP_LOGS`] 個より
//! 古いものは丸ごと消す。**既定の像として示している置き場（[`RunDir::publish_as_default_image`] が示した置き場）は消さない。**
//! 1 回の像は約 12.7 MB（ESP 9.7 MB・`disk0.img` 2 MB・OVMF の変数 0.5 MB）、ログは約 0.5 MB である
//! （2026-09-29 に全検査の作業ツリーで量った）。

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

/// 丸ごと残す置き場の数（新しい順）。
pub const KEEP_WHOLE: usize = 16;

/// ログだけでも残す置き場の数（新しい順）。これより古いものは丸ごと消す。
pub const KEEP_LOGS: usize = 1000;

/// 番号を取り合って負けたときに、次の番号を試す回数の上限（上限の無い繰り返しを書かない）。
const ALLOCATION_ATTEMPTS: u64 = 1000;

/// 1 回の QEMU の実行の置き場。**作ってから落とすまで、置き場のロックを持つ。**
pub struct RunDir {
    number: u64,
    path: PathBuf,
    _lock: Option<File>,
}

impl RunDir {
    /// 新しい置き場を取る。`what` は人が読む名前（`run.txt` と、取ったときの 1 行に出す）。
    ///
    /// **取る前に、古い置き場を片付ける**（モジュールの doc の「残す量」）。
    pub fn create(workspace_root: &Path, what: &str) -> Result<RunDir> {
        let runs = runs_dir(workspace_root);
        fs::create_dir_all(&runs)
            .with_context(|| format!("failed to create {}", runs.display()))?;
        prune(&runs);
        let mut number = highest_number(&runs).map_or(1, |n| n + 1);
        for _ in 0..ALLOCATION_ATTEMPTS {
            let path = runs.join(number.to_string());
            match fs::create_dir(&path) {
                Ok(()) => {
                    let lock = OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(path.join("lock"))
                        .with_context(|| {
                            format!("failed to create the lock in {}", path.display())
                        })?;
                    lock.lock()
                        .with_context(|| format!("failed to lock {}", path.display()))?;
                    let started = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis());
                    let mut note = File::create(path.join("run.txt")).with_context(|| {
                        format!("failed to write run.txt in {}", path.display())
                    })?;
                    writeln!(note, "what: {what}")?;
                    writeln!(note, "pid: {}", std::process::id())?;
                    writeln!(note, "started (unix ms): {started}")?;
                    println!("(info) run {number}: {} ({what})", path.display());
                    return Ok(RunDir {
                        number,
                        path,
                        _lock: Some(lock),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => number += 1,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to create {}", path.display()))
                }
            }
        }
        bail!(
            "could not take a run number under {} after {ALLOCATION_ATTEMPTS} attempt(s)",
            runs.display()
        )
    }

    /// 置き場だけを表す値（ロックも番号の取り合いも無い。**ホストのテストで引数の形を見るためだけに使う**）。
    #[cfg(test)]
    pub fn for_test(path: &Path, number: u64) -> RunDir {
        RunDir {
            number,
            path: path.to_path_buf(),
            _lock: None,
        }
    }

    /// 置き場そのもの。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// ESP のディレクトリ（QEMU には `fat:rw:` で渡す）。
    pub fn esp(&self) -> PathBuf {
        self.path.join("esp")
    }

    /// virtio-blk のディスクのイメージ。
    pub fn disk_image(&self) -> PathBuf {
        self.path.join("disk0.img")
    }

    /// OVMF の変数の写し（毎回テンプレートから作り直す）。
    pub fn ovmf_vars(&self) -> PathBuf {
        self.path.join("OVMF_VARS_4M.fd")
    }

    /// シリアルのログ。
    pub fn serial_log(&self) -> PathBuf {
        self.path.join("serial.log")
    }

    /// QEMU の `-d` の記録。
    pub fn debug_log(&self) -> PathBuf {
        self.path.join("qemu-debug.log")
    }

    /// そのほかのファイル（画面の写し・取り出したイメージなど）。**名前は置き場の中だけで意味を持つ。**
    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// monitor のソケット。**名前に回の番号を入れる**——同じプロセスの中で並べても、ぶつからない。
    /// 道の長さの上限の確かめは、呼ぶ側が今までどおり行う（`ensure_socket_path_fits`）。
    pub fn monitor_socket(&self, kind: &str) -> PathBuf {
        PathBuf::from(format!(
            "/tmp/zeikos-xtask-{kind}-{}-{}.sock",
            std::process::id(),
            self.number
        ))
    }

    /// 道具（`tools/stack-deepest.py` など）が使う既定の像として、この置き場を示す（`target/runs/default-image`
    /// を、この置き場への印にする）。
    pub fn publish_as_default_image(&self) -> Result<()> {
        let Some(runs) = self.path.parent() else {
            bail!("the run directory {} has no parent", self.path.display());
        };
        let link = runs.join(DEFAULT_IMAGE);
        let staged = runs.join(format!("{DEFAULT_IMAGE}.{}", self.number));
        let _ = fs::remove_file(&staged);
        std::os::unix::fs::symlink(self.number.to_string(), &staged)
            .with_context(|| format!("failed to make {}", staged.display()))?;
        // **置き換えは rename で 1 度に行う**——読む側が、印の無い瞬間を見ない。
        fs::rename(&staged, &link)
            .with_context(|| format!("failed to replace {}", link.display()))?;
        Ok(())
    }

    /// 直前の回のディスクのイメージ（`--keep-disk`・`--manual` で持ち越すもの）。**この置き場より前で、ディスクの
    /// イメージを持つ最も新しい置き場のもの**である。無ければ `None`。
    pub fn previous_disk_image(&self) -> Option<PathBuf> {
        let runs = self.path.parent()?;
        let mut numbers: Vec<u64> = numbered_entries(runs)
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| *n < self.number)
            .collect();
        numbers.sort_unstable_by(|a, b| b.cmp(a));
        numbers
            .into_iter()
            .map(|n| runs.join(n.to_string()).join("disk0.img"))
            .find(|path| path.is_file())
    }
}

/// 既定の像の印の名前。
const DEFAULT_IMAGE: &str = "default-image";

/// 回の置き場の親。
fn runs_dir(workspace_root: &Path) -> PathBuf {
    workspace_root.join("target").join("runs")
}

/// 置き場の番号と道（数字の名前のディレクトリだけ）。
fn numbered_entries(runs: &Path) -> Vec<(u64, PathBuf)> {
    let Ok(entries) = fs::read_dir(runs) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let number = entry.file_name().to_str()?.parse::<u64>().ok()?;
            Some((number, entry.path()))
        })
        .collect()
}

fn highest_number(runs: &Path) -> Option<u64> {
    numbered_entries(runs).into_iter().map(|(n, _)| n).max()
}

/// 片付けの分け方（純粋な論理）。新しい順に並べた番号から、丸ごと残すもの・ログだけ残すもの・消すものを返す。
fn prune_plan(mut numbers: Vec<u64>, keep: Option<u64>) -> (Vec<u64>, Vec<u64>) {
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    let mut logs_only = Vec::new();
    let mut remove = Vec::new();
    for (index, number) in numbers.into_iter().enumerate() {
        if Some(number) == keep {
            continue;
        }
        if index >= KEEP_LOGS {
            remove.push(number);
        } else if index >= KEEP_WHOLE {
            logs_only.push(number);
        }
    }
    (logs_only, remove)
}

/// 古い置き場を片付ける。**ロックの取れない置き場（走っている実行のもの）は触らない。** 失敗は無視する
/// （片付けは検査の結果に効かない）。
fn prune(runs: &Path) {
    let keep = fs::read_link(runs.join(DEFAULT_IMAGE))
        .ok()
        .and_then(|target| target.to_str()?.parse::<u64>().ok());
    let numbers: Vec<u64> = numbered_entries(runs).into_iter().map(|(n, _)| n).collect();
    let (logs_only, remove) = prune_plan(numbers, keep);
    for number in remove {
        let dir = runs.join(number.to_string());
        if let Some(_held) = lock_if_idle(&dir) {
            let _ = fs::remove_dir_all(&dir);
        }
    }
    for number in logs_only {
        let dir = runs.join(number.to_string());
        let Some(_held) = lock_if_idle(&dir) else {
            continue;
        };
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "lock" || name == "run.txt" || name.ends_with(".log") {
                continue;
            }
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

/// 置き場のロックが取れれば取って返す（使っている置き場なら `None`）。
fn lock_if_idle(dir: &Path) -> Option<File> {
    let file = OpenOptions::new().write(true).open(dir.join("lock")).ok()?;
    match file.try_lock() {
        Ok(()) => Some(file),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Error(_)) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **新しい順に KEEP_WHOLE 個は触らず、KEEP_LOGS 個までは像だけ消し、それより古いものは丸ごと消す。**
    /// **既定の像として示している置き場は、古くても触らない。**
    #[test]
    fn the_newest_runs_are_kept_whole_and_the_oldest_are_removed() {
        let numbers: Vec<u64> = (1..=(KEEP_LOGS as u64 + 5)).collect();
        let (logs_only, remove) = prune_plan(numbers, Some(2));
        assert_eq!(remove, vec![5, 4, 3, 1]);
        assert_eq!(logs_only.len(), KEEP_LOGS - KEEP_WHOLE);
        let newest_logs_only = KEEP_LOGS as u64 + 5 - KEEP_WHOLE as u64;
        assert_eq!(logs_only.first(), Some(&newest_logs_only));
        let (logs_only, remove) = prune_plan((1..=3).collect(), None);
        assert!(logs_only.is_empty() && remove.is_empty());
    }

    /// **置き場の中の名前と、ソケットの名前に回の番号が入ること。**
    #[test]
    fn the_names_in_a_run_directory_come_from_the_run() {
        let run = RunDir::for_test(Path::new("/w/target/runs/7"), 7);
        assert_eq!(run.esp(), Path::new("/w/target/runs/7/esp"));
        assert_eq!(run.disk_image(), Path::new("/w/target/runs/7/disk0.img"));
        assert_eq!(run.serial_log(), Path::new("/w/target/runs/7/serial.log"));
        assert!(run
            .monitor_socket("shell")
            .to_string_lossy()
            .ends_with(&format!("-{}-7.sock", std::process::id())));
    }
}
