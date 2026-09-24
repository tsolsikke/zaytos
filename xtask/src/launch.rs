//! QEMU を起こす口（2026-09-24。`ADR-0068` の HW-e を閉じる前。ホストの保護）。
//!
//! # なぜ 1 か所へ寄せるか
//!
//! **読む側には上限（`read_bounded`。256 MiB）を置いたが、書く側（QEMU の `-D`）には無かった。**
//! **例外の嵐の間は 1 分で 8.7 GB 伸びる**（実測。2026-09-24 に WSL2 ごと止まった回。
//! `docs/troubleshooting.md`）。**QEMU を起こす箇所が 40 あったので、ここへ寄せて上限を 1 か所で
//! 掛ける。**
//!
//! # 書く側の上限はカーネルに持たせる
//!
//! **`prlimit --fsize` で起こす**——**QEMU が書くどのファイル（`-D` の記録・シリアル・ディスクの像・
//! screendump・pmemsave）も上限を越えられない**（RLIMIT_FSIZE）。**見張りの糸が遅れても越えない。**
//! **上限はディスクの像への書き込みにも掛かる**ので、[`OTHER_WRITES`] より大きく保つ（ホストのテスト）。
//!
//! # コアを吐かせない
//!
//! **RLIMIT_FSIZE を越える書き込みは、既定では SIGXFSZ で止まり、コアを吐く**（一般論）。
//! **WSL の `core_pattern` はパイプである**（`|/wsl-capture-crash %t %E %p %s`。実測）——**パイプへは
//! RLIMIT_CORE が効かない**（Linux の `fs/coredump.c` の「Normally core limits are irrelevant to
//! pipes」）。**行き先は Windows 側の `%TEMP%\wsl-crashes` で、実測で既に 3 つ（46 MiB）在った。**
//! **QEMU のコアにはゲストのメモリがまるごと入りうる。**
//!
//! **だから SIGXFSZ を無視した状態で起こす**（`trap '' XFSZ` の後で `exec`。無視は `exec` を越えて
//! 引き継がれる）——**越える書き込みは EFBIG で失敗するだけで、QEMU は落ちない。** **止めるのは見張りの
//! 糸で、SIGKILL（コアを吐かない）で組ごと止める。** **`--core=0` も掛ける**（パイプでない
//! `core_pattern` の機械のため。レビューの足す1点）。
//!
//! # 判定の 4 分け（[`classify`]）
//!
//! - **log-limit**——ファイルの上限か空きの下限で切った。**判定の真偽より先に立てる**——**切った走行
//!   では「出なかった」を言えない。**
//! - **harness**——検査装置の故障（QEMU が起きない、準備やビルドの失敗、空きが足りない）。
//! - **timeout**——期限に着き、判定が偽。**期限に着くのが正常な項目は除く**（その選択は、使う箇所を移すときに足す）。
//! - **os**——走行は普通に終わったのに、判定が偽。
//!
//! **QEMU を起こさなかった項目（静的な検査など）は `check` とする**——上の 4 つのどれでもない。

use std::fmt;
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;

/// QEMU が書くファイル 1 つの上限（運用者の承認。2026-09-24）。**観測した最大（`-D` の記録の
/// 2,121,900,922 バイト）の約 1.9 倍。** **締めの `--full` の計測で決め直す。**
pub const FILE_LIMIT_BYTES: u64 = 4 << 30;

/// 置き場の空きの下限（運用者の承認。2026-09-24）。
pub const DISK_FLOOR_BYTES: u64 = 20 << 30;

/// 見張りの間隔。**書く側の上限はカーネルが持つ**ので、この間隔は「越えた後に止めるまでの遅れ」
/// だけを決める（その間 QEMU は書けない）。
const WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// **QEMU が書く、`-D` の記録以外のファイルの最大**（実測。2026-09-24。レビューの判断 (a)）。
/// **上限はこれらより大きく保つ**——**fsize の上限はディスクの像への書き込みにも掛かる。**
/// **monitor の socket は大きさを持たない。** **メモリ全体を pmemsave で書き出す検査は無い**
/// （実測。pmemsave は ext2 の像の RAM の写し 2 MiB と、カーネルスタック 128 KiB だけ）。
/// **ホストのテストだけが読む**（上限と比べる表）。
#[cfg(test)]
pub const OTHER_WRITES: &[(&str, u64)] = &[
    ("the boot media image (target/media/*.img)", 69_206_016),
    ("the AAVMF_VARS.fd copy (pflash, aarch64)", 67_108_864),
    ("a screendump PPM", 3_072_016),
    ("disk0.img (virtio-blk)", 2_097_152),
    (
        "pmemsave of the fs image copy (cmd_fs_image_extract)",
        2_097_152,
    ),
    ("the OVMF_VARS_4M.fd copy (pflash)", 540_672),
    ("the largest serial log (zi redraw-whole-screen)", 217_101),
    (
        "pmemsave of a kernel stack (tools/stack-deepest.py)",
        131_072,
    ),
];

/// **SIGXFSZ を無視してから、残りの引数を `exec` する**（上の「コアを吐かせない」）。
/// `sh -c <これ> <名前> <命令> <引数>...` の形で使う（`$0` が名前、`$@` が命令と引数）。
const IGNORE_XFSZ_THEN_EXEC: &str = "trap '' XFSZ; exec \"$@\"";

/// 期限に着いたことの意味（[`classify`] が使う）。**「期限に着くのが正常」（止まることや、窓いっぱい
/// 待つことを見る項目）の選択は、それを使う箇所を移すときに足す。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Deadline {
    /// 期限に着いたら失敗（「着くまで待つ」項目）。**判定が偽なら `timeout` に分ける。**
    Failure,
}

/// 走行を切った理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cut {
    /// ファイルが上限に着いた（**上限そのものも持つ**——項目ごとに小さくできるので）。
    FileLimit {
        file: PathBuf,
        bytes: u64,
        limit: u64,
    },
    /// 置き場の空きが下限を割った。
    DiskFloor { available: u64 },
}

impl fmt::Display for Cut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cut::FileLimit { file, bytes, limit } => write!(
                f,
                "{} reached {bytes} byte(s), the per-file limit of {limit}",
                file.display()
            ),
            Cut::DiskFloor { available } => write!(
                f,
                "only {available} byte(s) were free, under the floor of {DISK_FLOOR_BYTES}"
            ),
        }
    }
}

/// 検査装置の故障（[`classify`] で `harness` に分ける）。**QEMU を起こせない、準備やビルドの失敗、
/// 空きが足りない。**
#[derive(Debug)]
pub struct HarnessFault(pub String);

impl fmt::Display for HarnessFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "harness fault: {}", self.0)
    }
}

impl std::error::Error for HarnessFault {}

/// `Err` を検査装置の故障として包む（ビルドや準備の失敗に使う）。
pub fn as_harness<T>(result: Result<T>, what: &str) -> Result<T> {
    result.map_err(|error| anyhow::Error::new(HarnessFault(format!("{what}: {error:#}"))))
}

/// 1 回の走行の記録（項目の分け方と、検査の時間の計測に使う）。
#[derive(Clone, Debug)]
pub struct RunRecord {
    pub what: String,
    pub elapsed: Duration,
    pub cut: Option<Cut>,
    /// 期限に着いた（[`Deadline::Failure`] の走行だけ真になりうる）。
    pub reached_deadline: bool,
    pub status: Option<ExitStatus>,
    /// 見張った出力のうち、最も大きかったもののバイト数。
    pub largest_output: u64,
}

/// 項目の中の走行（`begin_item` で空にする）。
static ITEM_RUNS: Mutex<Vec<RunRecord>> = Mutex::new(Vec::new());
/// 起こした走行の数（全体。計測のため）。
static RUNS_STARTED: AtomicU64 = AtomicU64::new(0);

/// 項目の始めに、その項目の走行の記録を空にする。
pub fn reset_item_runs() {
    if let Ok(mut runs) = ITEM_RUNS.lock() {
        runs.clear();
    }
}

/// いまの項目の走行の記録。
pub fn item_runs() -> Vec<RunRecord> {
    ITEM_RUNS
        .lock()
        .map(|runs| runs.clone())
        .unwrap_or_default()
}

/// 起こした走行の数（全体）。
pub fn runs_started() -> u64 {
    RUNS_STARTED.load(Ordering::SeqCst)
}

/// 失敗の分け方（`--full` の失敗の行に出す）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Os,
    Timeout,
    LogLimit,
    Harness,
    /// QEMU を起こさなかった項目（静的な検査など）。
    Check,
}

impl Category {
    pub const ALL: [Category; 5] = [
        Category::Os,
        Category::Timeout,
        Category::LogLimit,
        Category::Harness,
        Category::Check,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Os => "os",
            Category::Timeout => "timeout",
            Category::LogLimit => "log-limit",
            Category::Harness => "harness",
            Category::Check => "check",
        }
    }
}

/// **失敗した項目を分ける**（純粋な論理）。**切った走行が 1 つでも在れば log-limit を先に立てる**
/// ——**切った走行では「出なかった」を言えない。** 次に検査装置の故障、次に最後の走行が期限に
/// 着いたか。**走行が 1 つも無く故障でもなければ `check`。**
pub fn classify(harness: bool, runs: &[RunRecord]) -> Category {
    if runs.iter().any(|run| run.cut.is_some()) {
        return Category::LogLimit;
    }
    if harness {
        return Category::Harness;
    }
    match runs.last() {
        None => Category::Check,
        Some(run) if run.reached_deadline => Category::Timeout,
        Some(_) => Category::Os,
    }
}

/// 誤りが検査装置の故障か。
pub fn is_harness(error: &anyhow::Error) -> bool {
    error.downcast_ref::<HarnessFault>().is_some()
}

/// `df --output=avail -B1` の出力から、空きのバイト数を読む（純粋な論理）。**表示の形に寄りかからない
/// よう、欄と単位を指定して出させた最後の行だけを読む**（レビューの判断 (d)）。
pub fn parse_df_avail(output: &str) -> Option<u64> {
    output
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())?
        .parse()
        .ok()
}

/// 置き場の空き（バイト）。
fn available_bytes(dir: &Path) -> Option<u64> {
    let output = Command::new("df")
        .args(["--output=avail", "-B1"])
        .arg(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_df_avail(&String::from_utf8_lossy(&output.stdout))
}

/// 止める相手（QEMU の組。組の番号は QEMU の pid と同じ）。**端末の組のまま起こす選択（手で触る
/// 起動。別の組だと、端末から読んだ時点で SIGTTIN で止まる）は、その箇所を移すときに足す。**
#[derive(Clone, Copy, Debug)]
struct KillTarget(u32);

impl KillTarget {
    /// 組ごと SIGKILL を送る（コアを吐かない）。**外の `kill` を使う**——依存を増やさない
    /// （レビューの判断 (d)）。
    fn kill(self) {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", self.0)])
            .status();
    }

    /// 組の中にまだ誰か残っているか（`kill -0`）。
    fn alive(self) -> bool {
        Command::new("kill")
            .args(["-0", "--", &format!("-{}", self.0)])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// 見張りの糸と分け合う状態。
struct Watch {
    stop: AtomicBool,
    cut: Mutex<Option<Cut>>,
    largest: AtomicU64,
}

/// 起こした QEMU。**`Child` と同じ名前の操作（`try_wait`・`kill`・`wait`）を持つ**——移し替えで
/// 呼ぶ側の形を変えないため。**待たずに落としても、組ごと止めて記録する**（`Drop`）。
pub struct QemuRun {
    child: Child,
    target: KillTarget,
    watch: Arc<Watch>,
    watcher: Option<JoinHandle<()>>,
    started: Instant,
    timeout: Duration,
    deadline: Deadline,
    what: String,
    status: Option<ExitStatus>,
    recorded: bool,
}

/// 起こし方。
pub struct Spec<'a> {
    /// QEMU の本体（`qemu-system-x86_64` 等）。
    pub program: &'a str,
    pub args: &'a [std::ffi::OsString],
    /// **見張る出力**（シリアルの記録・`-D` の記録）。**最初のものの置き場で空きを見る。**
    pub outputs: &'a [&'a Path],
    /// 失敗の行に出す名前。
    pub what: &'a str,
    /// 呼ぶ側の期限（[`Deadline`] の判断に使う）。
    pub timeout: Duration,
    pub deadline: Deadline,
    /// ファイル 1 つの上限（既定は [`FILE_LIMIT_BYTES`]。上限を確かめる項目だけが小さくする）。
    pub file_limit: u64,
}

impl<'a> Spec<'a> {
    /// 自動の検査の既定（自分の組、[`FILE_LIMIT_BYTES`]）。
    pub fn new(
        args: &'a [std::ffi::OsString],
        outputs: &'a [&'a Path],
        what: &'a str,
        timeout: Duration,
        deadline: Deadline,
    ) -> Self {
        Spec {
            program: "qemu-system-x86_64",
            args,
            outputs,
            what,
            timeout,
            deadline,
            file_limit: FILE_LIMIT_BYTES,
        }
    }
}

/// **QEMU を起こす**（唯一の口）。**起こす前に空きを確かめ、下限を割っていれば故障として断る。**
pub fn spawn(spec: &Spec<'_>) -> Result<QemuRun> {
    let dir = spec
        .outputs
        .first()
        .and_then(|path| path.parent())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    match available_bytes(&dir) {
        Some(available) if available < DISK_FLOOR_BYTES => {
            return Err(anyhow::Error::new(HarnessFault(format!(
                "{}: only {available} byte(s) are free under {}, under the floor of \
                 {DISK_FLOOR_BYTES}; not starting QEMU",
                spec.what,
                dir.display()
            ))));
        }
        Some(_) => {}
        None => {
            return Err(anyhow::Error::new(HarnessFault(format!(
                "{}: could not read the free space under {} with df",
                spec.what,
                dir.display()
            ))));
        }
    }
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(IGNORE_XFSZ_THEN_EXEC)
        .arg("zaytos-qemu")
        .arg("prlimit")
        .arg(format!("--fsize={}", spec.file_limit))
        .arg("--core=0")
        .arg("--")
        .arg(spec.program)
        .args(spec.args);
    // **自分の組で起こす**——終わりに組ごと止めれば、QEMU の子も残らない。
    command.process_group(0);
    let child = command.spawn().map_err(|error| {
        anyhow::Error::new(HarnessFault(format!(
            "{}: failed to launch {} under prlimit ({error}); are qemu-system-x86 and util-linux \
             installed?",
            spec.what, spec.program
        )))
    })?;
    RUNS_STARTED.fetch_add(1, Ordering::SeqCst);
    let target = KillTarget(child.id());
    let watch = Arc::new(Watch {
        stop: AtomicBool::new(false),
        cut: Mutex::new(None),
        largest: AtomicU64::new(0),
    });
    let outputs: Vec<PathBuf> = spec.outputs.iter().map(|path| path.to_path_buf()).collect();
    let file_limit = spec.file_limit;
    let shared = Arc::clone(&watch);
    let watcher = thread::spawn(move || watch_outputs(&shared, &outputs, &dir, file_limit, target));
    Ok(QemuRun {
        child,
        target,
        watch,
        watcher: Some(watcher),
        started: Instant::now(),
        timeout: spec.timeout,
        deadline: spec.deadline,
        what: spec.what.to_string(),
        status: None,
        recorded: false,
    })
}

/// 見張りの糸の本体。**ファイルが上限に着くか、空きが下限を割ったら、SIGKILL で止めて理由を残す。**
fn watch_outputs(
    watch: &Watch,
    outputs: &[PathBuf],
    dir: &Path,
    file_limit: u64,
    target: KillTarget,
) {
    let step = Duration::from_millis(100);
    loop {
        let mut waited = Duration::ZERO;
        while waited < WATCH_INTERVAL {
            if watch.stop.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(step);
            waited += step;
        }
        let mut cut = None;
        for path in outputs {
            let bytes = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
            watch.largest.fetch_max(bytes, Ordering::SeqCst);
            if cut.is_none() && bytes >= file_limit {
                cut = Some(Cut::FileLimit {
                    file: path.clone(),
                    bytes,
                    limit: file_limit,
                });
            }
        }
        if cut.is_none() {
            if let Some(available) = available_bytes(dir) {
                if available < DISK_FLOOR_BYTES {
                    cut = Some(Cut::DiskFloor { available });
                }
            }
        }
        if let Some(cut) = cut {
            if let Ok(mut slot) = watch.cut.lock() {
                *slot = Some(cut);
            }
            target.kill();
            return;
        }
    }
}

impl QemuRun {
    /// `Child::try_wait` と同じ。
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.status = status;
        }
        Ok(status)
    }

    /// **組ごと SIGKILL で止める**（`Child::kill` の代わり）。
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.target.kill();
        let _ = self.child.kill();
        Ok(())
    }

    /// `Child::wait` と同じ。**見張りを止め、走行を記録する。**
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.status = Some(status);
        self.finish();
        Ok(status)
    }

    /// 見張りが走行を切ったか。**待ちのループは、これが真なら抜ける**（切った後は何も出ない）。
    pub fn was_cut(&self) -> bool {
        self.watch
            .cut
            .lock()
            .map(|cut| cut.is_some())
            .unwrap_or(false)
    }

    /// 組の中にまだ誰か残っているか（上限を確かめる項目が使う）。
    pub fn anyone_left(&self) -> bool {
        self.target.alive()
    }

    /// 見張りを止め、走行を記録する（1 度だけ）。
    fn finish(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        self.watch.stop.store(true, Ordering::SeqCst);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        let elapsed = self.started.elapsed();
        let record = RunRecord {
            what: self.what.clone(),
            elapsed,
            cut: self.watch.cut.lock().ok().and_then(|cut| cut.clone()),
            reached_deadline: self.deadline == Deadline::Failure && elapsed >= self.timeout,
            status: self.status,
            largest_output: self.watch.largest.load(Ordering::SeqCst),
        };
        if let Some(cut) = &record.cut {
            println!("{}: (warn) the run was cut: {cut}", self.what);
        }
        if let Some(status) = self.status {
            if status.core_dumped() {
                println!("{}: (warn) QEMU dumped core ({status})", self.what);
            }
        }
        if let Ok(mut runs) = ITEM_RUNS.lock() {
            runs.push(record);
        }
    }

    /// 走行の記録（`wait` の後）。
    pub fn record(&self) -> Option<RunRecord> {
        ITEM_RUNS
            .lock()
            .ok()
            .and_then(|runs| runs.iter().rev().find(|run| run.what == self.what).cloned())
    }

    /// QEMU の pid（`/proc` を読む項目が使う）。
    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for QemuRun {
    /// **待たずに落としても、組ごと止めて記録する**——途中の `?` で抜けた呼ぶ側が QEMU を残さない。
    fn drop(&mut self) {
        if self.status.is_none() {
            self.target.kill();
            let _ = self.child.kill();
            self.status = self.child.wait().ok();
        }
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cut: Option<Cut>, reached_deadline: bool) -> RunRecord {
        RunRecord {
            what: "t".into(),
            elapsed: Duration::from_secs(1),
            cut,
            reached_deadline,
            status: None,
            largest_output: 0,
        }
    }

    /// **切った走行が在れば、故障や期限より先に log-limit である**（設計の順序）。
    #[test]
    fn a_cut_run_is_log_limit_before_anything_else() {
        let cut = Some(Cut::DiskFloor { available: 1 });
        assert_eq!(
            classify(true, &[run(cut.clone(), true)]),
            Category::LogLimit
        );
        assert_eq!(
            classify(false, &[run(None, false), run(cut, false)]),
            Category::LogLimit
        );
        assert_eq!(classify(true, &[run(None, true)]), Category::Harness);
        assert_eq!(classify(false, &[run(None, true)]), Category::Timeout);
        assert_eq!(classify(false, &[run(None, false)]), Category::Os);
        assert_eq!(classify(false, &[]), Category::Check);
        assert_eq!(classify(true, &[]), Category::Harness);
    }

    /// **上限は、QEMU が書く `-D` の記録以外のどのファイルよりも大きい**（fsize はディスクの像にも
    /// 掛かる。レビューの判断 (a)）。**空きの下限は上限より大きい**——1 つのファイルで下限を割らない。
    #[test]
    fn the_file_limit_exceeds_every_other_write() {
        for (what, bytes) in OTHER_WRITES {
            assert!(*bytes < FILE_LIMIT_BYTES, "{what}");
        }
        // 観測した最大の `-D` の記録（2026-09-24）より大きく、空きの下限より小さい。
        const { assert!(FILE_LIMIT_BYTES > 2_121_900_922) };
        const { assert!(DISK_FLOOR_BYTES > FILE_LIMIT_BYTES) };
    }

    /// `df --output=avail -B1` の形（実測）。**見出しを飛ばし、最後の行の数だけを読む。**
    #[test]
    fn df_avail_is_read_from_the_last_line() {
        assert_eq!(
            parse_df_avail("        Avail\n913749635072\n"),
            Some(913_749_635_072)
        );
        assert_eq!(parse_df_avail("Avail\n"), None);
        assert_eq!(parse_df_avail(""), None);
    }

    /// **故障は、上から文脈を重ねても故障と分かる。**
    #[test]
    fn a_harness_fault_survives_added_context() {
        let error = as_harness::<()>(Err(anyhow::anyhow!("cargo failed")), "building the kernel")
            .unwrap_err()
            .context("pipe-test");
        assert!(is_harness(&error));
        assert!(!is_harness(&anyhow::anyhow!("pipe-test: FAILED")));
    }

    /// **SIGXFSZ を無視してから exec する**（コアを吐かせない）。
    #[test]
    fn the_wrapper_ignores_sigxfsz_before_exec() {
        assert!(IGNORE_XFSZ_THEN_EXEC.starts_with("trap '' XFSZ;"));
        assert!(IGNORE_XFSZ_THEN_EXEC.contains("exec \"$@\""));
    }
}
