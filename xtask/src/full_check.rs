//! 全検査を別の作業木で回す口と、検査の記録（2026-09-25。検査の体系の改善の ③。`ADR-0069` の
//! 決定 7 の 3。運用者の決定）。
//!
//! # 形
//!
//! **`cargo xtask full [<コミット>]` を本の木から打つ**（既定は HEAD）。**錠を排他で取り、作業木
//! （`target/full-check/wt`）を `<コミット>` に合わせ、そこで `cargo xtask check --full` を子として回す。**
//! **検査しているのは作業木なので、走っている間も本の木は触ってよい**——**ただし QEMU と VirtualBox を
//! 使う検査は錠で断られる**（`check_lock`）。**ログは本の木の `target/full-check/logs/` に書く。**
//! **待ち方は今と同じ**（Bash の背景実行と harness の知らせ）。
//!
//! **作業木は本の木の `target/` の下に置く**（運用者の回答 2）——`tools/frame-sizes.py` の作業木と
//! 同じ形で、作業場所の中に収まる。**初回は冷えている**（組ごとの初回ビルド）。
//!
//! # 記録
//!
//! **検査の記録は本の木の `target/full-check/records.tsv` に 1 回 1 行で残す**——**基底・`--commit`・
//! `--full` の全部と、断られた回**（`cmd_check` が書く）。**作業木で走った全検査の記録も本の木へ集める。**
//! **木のハッシュと、走らせたときの作業ツリーの汚れ（`git status --porcelain` の行数）を持つ**——
//! **汚れが 0 の記録だけが「その木そのものが通った」と言える。**
//!
//! **`--status` と push の前の関門は、この記録だけを読む**（二重に持たない。運用者の足す1点）。
//! **コミットに要る検査は、`kernel/` か `common/` に触れたものは `--commit`、他は基底である**
//! （`.claude/hooks/check_after_commit.py` と同じ規則。**基底の確かめが両者の一致を見る**）。
//! **上下は `--full` ⊇ `--commit` ⊇ 基底**（`--full` は起動ログの突き合わせも回す）。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::check_lock::{self, git, git_line};
use crate::launch::{self, HarnessFault, HOST_SYSTEM_DRIVE, HOST_VHD_DRIVE};

/// `cargo xtask full` が子の全検査へログの道を渡す環境変数（錠の中身と記録に書くだけ）。
pub const LOG_ENV: &str = "ZAYTOS_CHECK_LOG";

/// `cargo xtask full` が子の全検査へ、始めに読んだ WSL の置き場の書いたセクタ数を渡す環境変数
/// （2026-09-25）。**作業木の取り出しの分も、その全検査の書いた量に含めるため。**
pub const DISK_START_ENV: &str = "ZAYTOS_CHECK_DISK_START";

/// この下に触ったコミットは `--commit` が要る（`.claude/hooks/check_after_commit.py` の
/// `IMAGE_PATH_PREFIXES` と同じ。**基底の確かめが一致を見る**）。
pub const IMAGE_PATH_PREFIXES: [&str; 2] = ["kernel/", "common/"];

/// 記録の頭の行。
///
/// **2026-09-25 に 4 欄を足した**（書いた量と、終わりの空き 3 つ。運用者の足す1点）。**足す前の 11 欄の行も読む。**
const RECORDS_HEADER: &str =
    "# unix\twhen\tlevel\toutcome\tcommit\ttree\tdirty\titems\titem_seconds\t\
     build_seconds\twritten\twsl_free\thost_free\tsystem_free\tnote";

/// 検査の段。**並びが上下である**（`--full` ⊇ `--commit` ⊇ 基底）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Base,
    Commit,
    Full,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Base => "base",
            Level::Commit => "commit",
            Level::Full => "full",
        }
    }

    fn shown(self) -> &'static str {
        match self {
            Level::Base => "the base check",
            Level::Commit => "--commit",
            Level::Full => "--full",
        }
    }

    fn parse(text: &str) -> Option<Level> {
        match text {
            "base" => Some(Level::Base),
            "commit" => Some(Level::Commit),
            "full" => Some(Level::Full),
            _ => None,
        }
    }
}

/// 記録の 1 行。
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub unix: u64,
    pub when: String,
    /// `base`・`commit`・`full`、または push の関門を旗で越えた `push`。
    pub level: String,
    /// `pass`・`fail`・`refused`・`cut`、または `override`。
    pub outcome: String,
    pub commit: String,
    pub tree: String,
    /// 走らせたときの作業ツリーの汚れ（`git status --porcelain` の行数）。
    pub dirty: usize,
    pub items: Option<usize>,
    pub item_seconds: Option<f64>,
    pub build_seconds: Option<f64>,
    /// その検査の間に WSL の置き場へ書いたバイト数（`/proc/diskstats` の差）。**他の走行の分も数える。**
    pub written: Option<u64>,
    /// 終わりの空き（WSL の中・VHD の載ったドライブ・Windows のドライブ）。**WSL の外ではドライブは `None`。**
    pub wsl_free: Option<u64>,
    pub host_free: Option<u64>,
    pub system_free: Option<u64>,
    pub note: String,
}

/// 記録を 1 行にする（純粋な論理）。**欄の区切りと改行は空白へ直す。**
fn format_record(record: &Record) -> String {
    let clean = |text: &str| text.replace(['\t', '\n', '\r'], " ");
    let number = |value: Option<f64>| value.map_or("-".to_string(), |value| format!("{value:.1}"));
    let bytes = |value: Option<u64>| value.map_or("-".to_string(), |value| value.to_string());
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        record.unix,
        clean(&record.when),
        clean(&record.level),
        clean(&record.outcome),
        clean(&record.commit),
        clean(&record.tree),
        record.dirty,
        record
            .items
            .map_or("-".to_string(), |items| items.to_string()),
        number(record.item_seconds),
        number(record.build_seconds),
        bytes(record.written),
        bytes(record.wsl_free),
        bytes(record.host_free),
        bytes(record.system_free),
        clean(&record.note)
    )
}

/// 記録の 1 行を読む（純粋な論理）。**頭の行と形の崩れた行は読まない。**
fn parse_record(line: &str) -> Option<Record> {
    if line.starts_with('#') {
        return None;
    }
    let fields: Vec<&str> = line.split('\t').collect();
    // **11 欄は 4 欄を足す前の行**（2026-09-25）。**足した欄は無いものとして読む。**
    if fields.len() != 11 && fields.len() != 15 {
        return None;
    }
    fn optional<T: std::str::FromStr>(text: &str) -> Option<T> {
        (text != "-").then(|| text.parse().ok()).flatten()
    }
    let added = |index: usize| {
        (fields.len() == 15)
            .then(|| optional(fields[index]))
            .flatten()
    };
    Some(Record {
        unix: fields[0].parse().ok()?,
        when: fields[1].to_string(),
        level: fields[2].to_string(),
        outcome: fields[3].to_string(),
        commit: fields[4].to_string(),
        tree: fields[5].to_string(),
        dirty: fields[6].parse().ok()?,
        items: optional(fields[7]),
        item_seconds: optional(fields[8]),
        build_seconds: optional(fields[9]),
        written: added(10),
        wsl_free: added(11),
        host_free: added(12),
        system_free: added(13),
        note: fields[fields.len() - 1].to_string(),
    })
}

/// 記録の置き場（本の木の `target/full-check/records.tsv`）。
pub fn records_path(root: &Path) -> Result<PathBuf> {
    Ok(check_lock::main_tree(root)?
        .join("target")
        .join("full-check")
        .join("records.tsv"))
}

/// 記録を 1 行足す。**1 回の書き込みで足す**（`O_APPEND`。並んで書いても行は混ざらない）。
pub fn append(root: &Path, record: &Record) -> Result<()> {
    let path = records_path(root)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    let fresh = !path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    let mut text = String::new();
    if fresh {
        text.push_str(RECORDS_HEADER);
        text.push('\n');
    }
    text.push_str(&format_record(record));
    file.write_all(text.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))
}

/// 記録を全部読む（無ければ空）。
pub fn read_records(root: &Path) -> Result<Vec<Record>> {
    let path = records_path(root)?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()))
        }
    };
    Ok(text.lines().filter_map(parse_record).collect())
}

/// 走り始めの木（`cmd_check` の入口で採り、終わりの記録に使う）。
struct Start {
    level: Level,
    root: PathBuf,
    commit: String,
    tree: String,
    dirty: usize,
    unix: u64,
    when: String,
    /// 始めに読んだ、WSL の置き場の書いたセクタ数。
    disk_start: Option<u64>,
}

static START: Mutex<Option<Start>> = Mutex::new(None);

/// `/proc/diskstats` の中身から、ある装置の書いたセクタ数を読む（純粋な論理）。**1 から数えて 10 番目の欄**
/// （`major minor 名前 …`）。
fn diskstats_sectors_written(text: &str, device: (u32, u32)) -> Option<u64> {
    text.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let major: u32 = fields.first()?.parse().ok()?;
        let minor: u32 = fields.get(1)?.parse().ok()?;
        ((major, minor) == device)
            .then(|| fields.get(9)?.parse().ok())
            .flatten()
    })
}

/// 木の載った装置（WSL の置き場）が書いたセクタ数（`/proc/diskstats`）。**WSL を起こし直すと 0 から
/// 数え直す。** 実測で装置は 8:48（sdd）だった（2026-09-25）。
fn sectors_written(root: &Path) -> Option<u64> {
    let device = check_lock::device_numbers(fs::metadata(root).ok()?.dev());
    diskstats_sectors_written(&fs::read_to_string("/proc/diskstats").ok()?, device)
}

/// 空き（WSL の中・VHD の載ったドライブ・Windows のドライブ）。**WSL の外では 2 つのドライブは `None`。**
fn free_spaces(root: &Path) -> (Option<u64>, Option<u64>, Option<u64>) {
    let wsl = launch::in_wsl();
    let drive = |path: &str| {
        wsl.then(|| launch::available_bytes(Path::new(path)))
            .flatten()
    };
    (
        launch::available_bytes(root),
        drive(HOST_VHD_DRIVE),
        drive(HOST_SYSTEM_DRIVE),
    )
}

/// バイトを GiB で読める形にする（純粋な論理）。
fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

/// 全検査のまとめに出す空きの行（Windows のドライブは計器。止めない）。
pub fn free_space_lines(root: &Path) -> Vec<String> {
    let (wsl, host, system) = free_spaces(root);
    let shown = |value: Option<u64>| value.map_or("unreadable".to_string(), gib);
    let mut lines = Vec::new();
    if launch::in_wsl() {
        lines.push(format!(
            "(info) free space at the end: WSL {}; the drive holding the WSL disk ({HOST_VHD_DRIVE}) {}; \
             Windows ({HOST_SYSTEM_DRIVE}) {}",
            shown(wsl),
            shown(host),
            shown(system)
        ));
        if let Some(system) = system.filter(|system| *system < launch::SYSTEM_DRIVE_WARN_BYTES) {
            lines.push(format!(
                "(warn) Windows ({HOST_SYSTEM_DRIVE}) has only {} free, under the mark of {}; ask the \
                 operator (this does not stop the check)",
                gib(system),
                gib(launch::SYSTEM_DRIVE_WARN_BYTES)
            ));
        }
    } else {
        lines.push(format!(
            "(info) free space at the end: WSL {}; the Windows drives: not watched (not WSL)",
            shown(wsl)
        ));
    }
    lines
}

/// 全検査の入口の空きの判定（純粋な論理）。**見込みの書く量＋下限を、WSL の中と VHD の載ったドライブの
/// 両方で見る**（運用者の足す1点）。**足りない置き場を全部挙げる。** **WSL の外ではドライブを見ない。**
pub fn start_shortfalls(
    estimate: u64,
    wsl_free: Option<u64>,
    in_wsl: bool,
    host_free: Option<u64>,
) -> Vec<String> {
    let mut short = Vec::new();
    let wsl_need = estimate + launch::DISK_FLOOR_BYTES;
    match wsl_free {
        Some(free) if free >= wsl_need => {}
        Some(free) => short.push(format!(
            "WSL has {} free, under the {} expected to be written plus the floor of {}",
            gib(free),
            gib(estimate),
            gib(launch::DISK_FLOOR_BYTES)
        )),
        None => short.push("the free space inside WSL could not be read".to_string()),
    }
    if in_wsl {
        let host_need = estimate + launch::HOST_DISK_FLOOR_BYTES;
        match host_free {
            Some(free) if free >= host_need => {}
            Some(free) => short.push(format!(
                "the drive holding the WSL disk ({HOST_VHD_DRIVE}) has {} free, under the {} expected \
                 to be written plus the floor of {}",
                gib(free),
                gib(estimate),
                gib(launch::HOST_DISK_FLOOR_BYTES)
            )),
            None => short.push(format!(
                "the free space of {HOST_VHD_DRIVE} could not be read; if the WSL disk moved, update \
                 HOST_VHD_DRIVE in xtask/src/launch.rs"
            )),
        }
    }
    short
}

/// 全検査が書く量の見込み（純粋な論理）。**前回の全検査の記録の書いた量**を使う。**無ければ代わりの値**
/// （本の木の `target/` の大きさ。**冷えた作業木の見込みとして**）。
fn estimate_to_write(
    records: &[Record],
    stand_in: impl FnOnce() -> Option<u64>,
) -> Option<(u64, String)> {
    match records
        .iter()
        .rev()
        .find(|record| record.level == "full" && record.written.is_some())
    {
        Some(record) => Some((
            record.written.unwrap_or(0),
            format!("what the full check of {} wrote", record.when),
        )),
        None => stand_in().map(|bytes| {
            (
                bytes,
                "the size of the main tree's target/, as no full check has recorded what it wrote"
                    .to_string(),
            )
        }),
    }
}

/// 検査の入口で木を採る。**採れなくても検査は止めない**（記録に `?` が残る）。
pub fn begin(root: &Path, level: Level) {
    let commit = git_line(root, &["rev-parse", "HEAD"]).unwrap_or_else(|_| "?".to_string());
    let tree = git_line(root, &["rev-parse", "HEAD^{tree}"]).unwrap_or_else(|_| "?".to_string());
    let dirty = git(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or(usize::MAX, |output| {
            String::from_utf8_lossy(&output.stdout).lines().count()
        });
    let (unix, when) = check_lock::now();
    // **`cargo xtask full` の子なら、親が始めに読んだ値を使う**（作業木の取り出しの分も含める）。
    let disk_start = std::env::var(DISK_START_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .or_else(|| sectors_written(root));
    if let Ok(mut start) = START.lock() {
        *start = Some(Start {
            level,
            root: root.to_path_buf(),
            commit,
            tree,
            dirty,
            unix,
            when,
            disk_start,
        });
    }
}

/// 走り始めの木（錠の中身に書く）。**`begin` の前なら `None`。**
pub fn started_commit_and_tree() -> Option<(String, String)> {
    let start = START.lock().ok()?;
    let start = start.as_ref()?;
    Some((start.commit.clone(), start.tree.clone()))
}

/// 検査の終わりに記録を 1 行書く。**書けなくても検査の結果は変えない**（言うだけ）。
pub fn end(outcome: &str, items: Option<usize>, item_seconds: Option<f64>) {
    let Some(record) = START.lock().ok().and_then(|start| {
        let start = start.as_ref()?;
        let written = start
            .disk_start
            .zip(sectors_written(&start.root))
            .map(|(before, after)| after.saturating_sub(before) * 512);
        let (wsl_free, host_free, system_free) = free_spaces(&start.root);
        Some((
            start.root.clone(),
            Record {
                unix: start.unix,
                when: start.when.clone(),
                level: start.level.label().to_string(),
                outcome: outcome.to_string(),
                commit: start.commit.clone(),
                tree: start.tree.clone(),
                dirty: start.dirty,
                items,
                item_seconds,
                build_seconds: Some(
                    crate::metrics::total_time(crate::metrics::Kind::Build).as_secs_f64(),
                ),
                written,
                wsl_free,
                host_free,
                system_free,
                note: std::env::var(LOG_ENV).unwrap_or_else(|_| "-".to_string()),
            },
        ))
    }) else {
        return;
    };
    if let Err(error) = append(&record.0, &record.1) {
        println!("(info) the check record could not be written: {error:#}");
    }
}

/// コミットに要る検査（純粋な論理）。**`kernel/` か `common/` に触れたものは `--commit`。**
pub fn needed_level(paths: &[String]) -> Level {
    if paths.iter().any(|path| {
        IMAGE_PATH_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
    }) {
        Level::Commit
    } else {
        Level::Base
    }
}

/// コミットが触ったパス（`git show --name-only`。フックと同じ見方）。
fn paths_of(root: &Path, commit: &str) -> Result<Vec<String>> {
    let text = git_line(root, &["show", "--name-only", "--pretty=format:", commit])?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// そのコミットの要る検査を満たす記録（純粋な論理）。**新しいものから探す。**
///
/// - **合格の記録で、段が要る段以上であること。**
/// - **コミットが同じか、木が同じで汚れが 0 であること**（木が同じなら中身は同じ）。
/// - **旗で越えた記録（`override`）は、そのコミットだけを満たす。**
pub fn covering<'a>(
    records: &'a [Record],
    commit: &str,
    tree: &str,
    need: Level,
) -> Option<&'a Record> {
    records.iter().rev().find(|record| {
        let passed = record.outcome == "pass"
            && Level::parse(&record.level).is_some_and(|level| level >= need)
            && (record.commit == commit || (record.tree == tree && record.dirty == 0));
        passed || (record.outcome == "override" && record.commit == commit)
    })
}

/// 記録を人が読む形にする（純粋な論理）。
fn describe(record: &Record) -> String {
    let what = match (record.outcome.as_str(), Level::parse(&record.level)) {
        ("override", _) => format!("the push gate was passed by the flag ({})", record.note),
        (outcome, Some(level)) => format!("{} {outcome}", level.shown()),
        (outcome, None) => format!("{} {outcome}", record.level),
    };
    let dirty = match record.dirty {
        0 => String::new(),
        usize::MAX => " (the working tree could not be read)".to_string(),
        count => format!(" (the working tree had {count} other change(s))"),
    };
    format!("{what} at {}{dirty}", record.when)
}

fn short(hash: &str) -> &str {
    hash.get(..8).unwrap_or(hash)
}

/// あるコミットの 1 行（要る検査と、満たす記録。無ければ最後の記録）。
fn commit_line(root: &Path, records: &[Record], commit: &str) -> Result<(String, bool)> {
    let tree = git_line(root, &["rev-parse", &format!("{commit}^{{tree}}")])?;
    let subject = git_line(root, &["log", "-1", "--format=%s", commit])?;
    let need = needed_level(&paths_of(root, commit)?);
    let (state, covered) = match covering(records, commit, &tree, need) {
        Some(record) => (describe(record), true),
        None => (
            match records.iter().rev().find(|record| record.commit == commit) {
                Some(record) => format!("NOT CHECKED (last: {})", describe(record)),
                None => "NOT CHECKED (no record)".to_string(),
            },
            false,
        ),
    };
    Ok((
        format!(
            "  {} {subject}\n      needs {}; {state}",
            short(commit),
            need.shown()
        ),
        covered,
    ))
}

/// `cargo xtask full --status`——**HEAD の木が緑か、緑の木より後のコミットと、それぞれ何で確かめたか。**
fn status(root: &Path) -> Result<()> {
    let main = check_lock::main_tree(root)?;
    let records = read_records(&main)?;
    let head = git_line(&main, &["rev-parse", "HEAD"])?;
    let head_tree = git_line(&main, &["rev-parse", "HEAD^{tree}"])?;
    let green =
        |record: &&Record| record.level == "full" && record.outcome == "pass" && record.dirty == 0;
    match records
        .iter()
        .rev()
        .filter(green)
        .find(|record| record.tree == head_tree)
    {
        Some(record) => println!(
            "HEAD {} (tree {}): the full check passed on this tree at {} ({} item(s))",
            short(&head),
            short(&head_tree),
            record.when,
            record.items.unwrap_or(0)
        ),
        None => println!(
            "HEAD {} (tree {}): no green full check of this tree is recorded",
            short(&head),
            short(&head_tree)
        ),
    }
    match records.iter().rev().find(green) {
        Some(record) => {
            println!(
                "the last green full check: {} (tree {}) at {}, {} item(s), {:.1} min of items; log {}",
                short(&record.commit),
                short(&record.tree),
                record.when,
                record.items.unwrap_or(0),
                record.item_seconds.unwrap_or(0.0) / 60.0,
                record.note
            );
            let after = git_line(
                &main,
                &["rev-list", "--reverse", &format!("{}..HEAD", record.commit)],
            )?;
            let commits: Vec<&str> = after.lines().filter(|line| !line.is_empty()).collect();
            println!("commits after it: {}", commits.len());
            for commit in commits {
                println!("{}", commit_line(&main, &records, commit)?.0);
            }
        }
        None => println!(
            "the last green full check: none recorded in {}",
            records_path(&main)?.display()
        ),
    }
    let (holders, content) = check_lock::current_holders(&main)?;
    if holders.is_empty() {
        println!("check lock: free");
    }
    for (pid, mode, command) in &holders {
        println!("check lock: held by pid {pid} ({mode:?}) {command}");
    }
    if holders
        .iter()
        .any(|(pid, _, _)| content.starts_with(&format!("pid: {pid}\n")))
    {
        for line in content.lines() {
            println!("    {line}");
        }
    }
    let marks = check_lock::vbox_marks(&main)?;
    if !marks.is_empty() {
        println!(
            "VirtualBox VM(s) marked as left running by tools/vbox-vm.py start: {}",
            marks.join(", ")
        );
    }
    // **空きの計器**（2026-09-25）。**記録の最後の値と、いまの値。**
    let shown = |value: Option<u64>| value.map_or("-".to_string(), gib);
    if let Some(record) = records
        .iter()
        .rev()
        .find(|record| record.wsl_free.is_some())
    {
        println!(
            "free space at the last recorded check ({}): WSL {}; {HOST_VHD_DRIVE} {}; {HOST_SYSTEM_DRIVE} {}",
            record.when,
            shown(record.wsl_free),
            shown(record.host_free),
            shown(record.system_free)
        );
    }
    for line in free_space_lines(&main) {
        println!("{}", line.replacen("at the end", "now", 1));
    }
    Ok(())
}

/// 前の全検査の子か QEMU が残っていたら断る（作業木を触る前に見る）。
///
/// **錠は持ち主が終われば放れるが、持ち主が上限で降りた後も子が残りうる**（固まった子は運用者に
/// 確かめてから止める。`.claude/skills/stop-a-process/SKILL.md`）。**残った子が使っている作業木を
/// 切り替えないため。**
fn refuse_if_a_previous_run_is_alive(worktree: &Path) -> Result<()> {
    let binary = worktree.join("target").join("debug").join("xtask");
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(comm) = fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        let comm = comm.trim();
        let from_worktree =
            comm == "xtask" && crate::process_binary(pid).is_some_and(|path| path == binary);
        if from_worktree || comm.starts_with("qemu-system-") {
            found.push(format!("{comm} (pid {pid})"));
        }
    }
    if found.is_empty() {
        return Ok(());
    }
    bail!(
        "cargo xtask full: a run from before is still alive: {}. It may be using {}; ask the \
         operator, then stop it with .claude/skills/stop-a-process/SKILL.md and start again",
        found.join(", "),
        worktree.display()
    )
}

/// 作業木を `commit` に合わせ、汚れていないことを確かめる。
fn prepare_worktree(main: &Path, worktree: &Path, commit: &str) -> Result<()> {
    git_line(main, &["worktree", "prune"])?;
    let listed = git_line(main, &["worktree", "list", "--porcelain"])?;
    let registered = listed.lines().any(|line| {
        line.strip_prefix("worktree ")
            .is_some_and(|path| Path::new(path) == worktree)
    });
    if registered {
        git_line(worktree, &["checkout", "-q", "--detach", "--force", commit])?;
    } else if worktree.exists() {
        bail!(
            "{} exists but is not a registered worktree; look at what is in it and remove it by \
             hand, then start again",
            worktree.display()
        );
    } else {
        if let Some(parent) = worktree.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        let path = worktree.to_string_lossy().into_owned();
        git_line(main, &["worktree", "add", "-q", "--detach", &path, commit])?;
    }
    let head = git_line(worktree, &["rev-parse", "HEAD"])?;
    if head != commit {
        bail!(
            "the worktree is at {head}, not at {commit}, after the checkout; start again after \
             looking at {}",
            worktree.display()
        );
    }
    let dirty = git_line(worktree, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        let listed: Vec<&str> = dirty.lines().take(20).collect();
        bail!(
            "the worktree {} is not clean after the checkout ({} line(s)); something wrote into it. \
             Look at these and remove them by hand, then start again:\n  {}",
            worktree.display(),
            dirty.lines().count(),
            listed.join("\n  ")
        );
    }
    Ok(())
}

/// 子を上限つきで待つ（`None` なら上限を過ぎた）。**止めはしない**——**固まった子は運用者に確かめて
/// から止める**（`ADR-0069` の決定 7 の 3 の (5)）。
fn wait_within(child: &mut std::process::Child, limit: Duration) -> Result<Option<ExitStatus>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .context("could not wait for the full check")?
        {
            return Ok(Some(status));
        }
        if started.elapsed() > limit {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// ログの締めを出す（落ちた行と、まとめの行）。
fn print_log_summary(log: &Path) {
    let Ok(text) = fs::read(log).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()) else {
        println!("(the log {} could not be read)", log.display());
        return;
    };
    let lines: Vec<&str> = text.lines().collect();
    let failed: Vec<&&str> = lines
        .iter()
        .filter(|line| line.starts_with("--- ") && line.contains(": FAILED"))
        .take(40)
        .collect();
    for line in failed {
        println!("{line}");
    }
    let from = lines
        .iter()
        .rposition(|line| line.starts_with("(info) item time total"))
        .or_else(|| lines.len().checked_sub(15))
        .unwrap_or(0);
    for line in &lines[from..] {
        println!("{line}");
    }
}

/// 本の木へ `since` の後に積まれたコミットと、それぞれに要る検査を出す。
fn print_commits_since(main: &Path, since: &str) -> Result<()> {
    let records = read_records(main)?;
    let after = git_line(main, &["rev-list", "--reverse", &format!("{since}..HEAD")])?;
    let commits: Vec<&str> = after.lines().filter(|line| !line.is_empty()).collect();
    println!(
        "commits in the main tree after {} (checked by this full check): {}",
        short(since),
        commits.len()
    );
    for commit in commits {
        println!("{}", commit_line(main, &records, commit)?.0);
    }
    Ok(())
}

/// `cargo xtask full [<コミット>]` の本体。
fn run(target: &str) -> Result<()> {
    let root = crate::workspace_root()?;
    let main = check_lock::main_tree(&root)?;
    let here =
        fs::canonicalize(&root).with_context(|| format!("could not resolve {}", root.display()))?;
    if here != main {
        bail!(
            "run cargo xtask full from the main tree ({}); this xtask belongs to {}",
            main.display(),
            here.display()
        );
    }
    let commit = git_line(
        &main,
        &["rev-parse", "--verify", &format!("{target}^{{commit}}")],
    )?;
    let tree = git_line(&main, &["rev-parse", &format!("{commit}^{{tree}}")])?;
    let (_, when) = check_lock::now();
    let stamp: String = when.chars().filter(char::is_ascii_digit).collect();
    let log = main
        .join("target")
        .join("full-check")
        .join("logs")
        .join(format!(
            "{}-{}-{}.log",
            stamp.get(..8).unwrap_or(&stamp),
            stamp.get(8..).unwrap_or(""),
            short(&tree)
        ));
    let command = format!("cargo xtask full {target}");
    check_lock::hold_or_exit(
        check_lock::Mode::Exclusive,
        &command,
        Some(check_lock::owner_content(
            &command,
            &commit,
            &tree,
            &log.display().to_string(),
        )),
    )?;
    let worktree = main.join("target").join("full-check").join("wt");
    refuse_if_a_previous_run_is_alive(&worktree)?;
    // **始める前に、見込みの書く量＋下限を、WSL の中と VHD の載ったドライブの両方で見る**（2026-09-25。
    // 運用者の足す1点）。**足りなければ検査装置の故障として断る。**
    let records = read_records(&main)?;
    let (estimate, source) =
        estimate_to_write(&records, || crate::directory_bytes(&main.join("target")))
            .context("cargo xtask full: could not estimate how much the full check writes")?;
    let (wsl_free, host_free, system_free) = free_spaces(&main);
    let shown = |value: Option<u64>| value.map_or("unreadable".to_string(), gib);
    println!(
        "full: expecting to write {} ({source}); free: WSL {}, the drive holding the WSL disk \
         ({HOST_VHD_DRIVE}) {}, Windows ({HOST_SYSTEM_DRIVE}) {}",
        gib(estimate),
        shown(wsl_free),
        if launch::in_wsl() {
            shown(host_free)
        } else {
            "not watched (not WSL)".to_string()
        },
        if launch::in_wsl() {
            shown(system_free)
        } else {
            "not watched (not WSL)".to_string()
        }
    );
    let shortfalls = start_shortfalls(estimate, wsl_free, launch::in_wsl(), host_free);
    if !shortfalls.is_empty() {
        let message = format!(
            "cargo xtask full: not enough free space to start: {}",
            shortfalls.join("; ")
        );
        append_full_record(&main, &commit, &tree, "refused", &message);
        return Err(anyhow::Error::new(HarnessFault(message)));
    }
    let disk_start = sectors_written(&main);
    prepare_worktree(&main, &worktree, &commit)?;
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    }
    let file = File::create(&log).with_context(|| format!("could not create {}", log.display()))?;
    let mut child = Command::new("cargo");
    child
        .args(["xtask", "check", "--full"])
        .current_dir(&worktree)
        .env(check_lock::OWNER_ENV, std::process::id().to_string())
        .env(LOG_ENV, &log)
        .env(
            DISK_START_ENV,
            disk_start.map_or(String::new(), |sectors| sectors.to_string()),
        )
        .stdin(Stdio::null())
        .stdout(file.try_clone().context("could not share the log")?)
        .stderr(file)
        .process_group(0);
    // **子の git が別の木を見ないように、`GIT_*` を外す**（錠の道と同じ理由）。
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            child.env_remove(&key);
        }
    }
    println!(
        "full: {} (tree {}) in {}; started {when}; log {}",
        short(&commit),
        short(&tree),
        worktree.display(),
        log.display()
    );
    let mut child = child
        .spawn()
        .context("could not start cargo xtask check --full")?;
    let limit = crate::FULL_TIME_LIMIT + Duration::from_secs(30 * 60);
    let status = wait_within(&mut child, limit)?;
    print_log_summary(&log);
    let Some(status) = status else {
        let message = format!(
            "the full check did not end within {} min; its process group {} is still running and \
             still uses {}. Ask the operator, then stop it with .claude/skills/stop-a-process/SKILL.md",
            limit.as_secs() / 60,
            child.id(),
            worktree.display()
        );
        append_full_record(&main, &commit, &tree, "cut", &message);
        println!("xtask full: {message}");
        std::process::exit(3);
    };
    if status.code().is_none() {
        append_full_record(
            &main,
            &commit,
            &tree,
            "cut",
            &format!(
                "the full check ended by a signal ({status}); log {}",
                log.display()
            ),
        );
    }
    print_commits_since(&main, &commit)?;
    match status.code() {
        Some(0) => Ok(()),
        Some(code) => std::process::exit(code),
        None => std::process::exit(1),
    }
}

/// 子が記録を書けないときの全検査の記録（上限を過ぎた・信号で終わった・空きが足りずに始めなかった）。
fn append_full_record(main: &Path, commit: &str, tree: &str, outcome: &str, note: &str) {
    let (unix, when) = check_lock::now();
    let (wsl_free, host_free, system_free) = free_spaces(main);
    let record = Record {
        unix,
        when,
        level: Level::Full.label().to_string(),
        outcome: outcome.to_string(),
        commit: commit.to_string(),
        tree: tree.to_string(),
        dirty: 0,
        items: None,
        item_seconds: None,
        build_seconds: None,
        written: None,
        wsl_free,
        host_free,
        system_free,
        note: note.to_string(),
    };
    if let Err(error) = append(main, &record) {
        println!("(info) the check record could not be written: {error:#}");
    }
}

/// push の前の関門の結果。
#[derive(Debug, PartialEq, Eq)]
pub struct Gate {
    /// 押すコミットの数。
    pub pending: usize,
    /// 要る検査の記録が無いコミット（旗で越えたものを含む）。
    pub missing: Vec<String>,
    /// 旗で越えたか。
    pub overridden: bool,
}

/// push の前の関門（運用者の足す1点。2026-09-25）。**押すコミット（どのリモートにも無いもの）の
/// それぞれに、要る検査の合格の記録が在るかを見る。** **旗の理由が在れば、無いコミットを「旗で
/// 越えた」と記録して通す。**
///
/// **読むのは `--status` と同じ記録である**（二重に持たない）。
pub fn gate(root: &Path, override_reason: Option<&str>) -> Result<Gate> {
    let main = check_lock::main_tree(root)?;
    let records = read_records(&main)?;
    let pending = git_line(
        &main,
        &["rev-list", "--reverse", "HEAD", "--not", "--remotes"],
    )?;
    let pending: Vec<&str> = pending.lines().filter(|line| !line.is_empty()).collect();
    let mut missing = Vec::new();
    for commit in &pending {
        let (line, covered) = commit_line(&main, &records, commit)?;
        if !covered {
            println!("{line}");
            missing.push(commit.to_string());
        }
    }
    let overridden = !missing.is_empty() && override_reason.is_some();
    if let (true, Some(reason)) = (overridden, override_reason) {
        let (unix, when) = check_lock::now();
        for commit in &missing {
            let tree = git_line(&main, &["rev-parse", &format!("{commit}^{{tree}}")])?;
            append(
                &main,
                &Record {
                    unix,
                    when: when.clone(),
                    level: "push".to_string(),
                    outcome: "override".to_string(),
                    commit: commit.clone(),
                    tree,
                    dirty: 0,
                    items: None,
                    item_seconds: None,
                    build_seconds: None,
                    written: None,
                    wsl_free: None,
                    host_free: None,
                    system_free: None,
                    note: reason.to_string(),
                },
            )?;
        }
    }
    Ok(Gate {
        pending: pending.len(),
        missing,
        overridden,
    })
}

/// `cargo xtask full [<コミット>] | --status | --gate [--override <理由>]`。
pub fn command(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "--status") {
        return status(&crate::workspace_root()?);
    }
    if args.iter().any(|arg| arg == "--gate") {
        let reason = match args.iter().position(|arg| arg == "--override") {
            Some(index) => {
                let reason = args
                    .get(index + 1)
                    .map(|reason| reason.trim())
                    .filter(|reason| !reason.is_empty())
                    .context("--override needs a reason")?;
                Some(reason)
            }
            None => None,
        };
        let gate = gate(&crate::workspace_root()?, reason)?;
        if gate.missing.is_empty() {
            println!(
                "push gate: {} commit(s) to push, each with the check it needs recorded as passed",
                gate.pending
            );
            return Ok(());
        }
        if gate.overridden {
            println!(
                "push gate: passed by the flag for {} of {} commit(s) without the check they need; \
                 recorded as override in {}",
                gate.missing.len(),
                gate.pending,
                records_path(&crate::workspace_root()?)?.display()
            );
            return Ok(());
        }
        bail!(
            "push gate: {} of {} commit(s) to push have no passing record of the check they need \
             (listed above). Run that check while the commit is HEAD (cargo xtask check or \
             cargo xtask check --commit), or check it in the worktree with cargo xtask full <commit>",
            gate.missing.len(),
            gate.pending
        );
    }
    let targets: Vec<&String> = args.iter().filter(|arg| !arg.starts_with("--")).collect();
    if targets.len() > 1 || args.iter().any(|arg| arg.starts_with("--")) {
        bail!(
            "usage: cargo xtask full [<commit>] | cargo xtask full --status | \
             cargo xtask full --gate [--override <reason>]"
        );
    }
    run(targets.first().map_or("HEAD", |target| target.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(level: &str, outcome: &str, commit: &str, tree: &str, dirty: usize) -> Record {
        Record {
            unix: 1,
            when: "2026-09-25 10:00:00".to_string(),
            level: level.to_string(),
            outcome: outcome.to_string(),
            commit: commit.to_string(),
            tree: tree.to_string(),
            dirty,
            items: Some(47),
            item_seconds: Some(12.3),
            build_seconds: None,
            written: Some(4096),
            wsl_free: Some(1 << 40),
            host_free: None,
            system_free: Some(20 << 30),
            note: "a\tb\nc".to_string(),
        }
    }

    /// **記録は 1 行で書いて同じものに読める**（区切りと改行は空白へ直す）。**頭の行と崩れた行は読まない。**
    #[test]
    fn a_record_reads_back_as_written() {
        let written = record("commit", "pass", "c1", "t1", 2);
        let line = format_record(&written);
        assert_eq!(line.matches('\n').count(), 1);
        let read = parse_record(line.trim_end_matches('\n')).unwrap();
        assert_eq!(read.note, "a b c");
        assert_eq!(read.items, Some(47));
        assert_eq!(read.build_seconds, None);
        assert_eq!(
            Record {
                note: "a b c".to_string(),
                ..written
            },
            read
        );
        assert_eq!(parse_record(RECORDS_HEADER), None);
        assert_eq!(parse_record("1\t2\t3"), None);
    }

    /// **`kernel/` か `common/` に触れたコミットは `--commit` が要る**（フックと同じ規則）。
    #[test]
    fn a_commit_that_touches_the_image_needs_the_commit_check() {
        let paths = |list: &[&str]| list.iter().map(|path| path.to_string()).collect::<Vec<_>>();
        assert_eq!(needed_level(&paths(&["kernel/src/task.rs"])), Level::Commit);
        assert_eq!(
            needed_level(&paths(&["docs/a.md", "common/src/x.rs"])),
            Level::Commit
        );
        assert_eq!(needed_level(&paths(&["xtask/src/main.rs"])), Level::Base);
        assert_eq!(needed_level(&paths(&["docs/kernel/a.md"])), Level::Base);
        assert_eq!(needed_level(&paths(&[])), Level::Base);
    }

    /// **要る段以上の合格だけが満たす。** **木が同じでも、汚れのある記録は別のコミットを満たさない。**
    /// **旗で越えた記録は、そのコミットだけを満たす。**
    #[test]
    fn only_a_pass_at_the_needed_level_or_above_covers_a_commit() {
        let records = vec![
            record("base", "pass", "c1", "t1", 0),
            record("commit", "refused", "c2", "t2", 0),
            record("full", "pass", "c9", "t3", 0),
            record("commit", "pass", "c4", "t4", 3),
            record("push", "override", "c5", "-", 0),
        ];
        assert!(covering(&records, "c1", "t1", Level::Base).is_some());
        assert!(covering(&records, "c1", "t1", Level::Commit).is_none());
        assert!(covering(&records, "c2", "t2", Level::Commit).is_none());
        // **木が同じで汚れが 0 の全検査は、別のコミットでも満たす**（中身が同じ）。
        assert!(covering(&records, "c3", "t3", Level::Commit).is_some());
        // **汚れのある記録は、そのコミットだけを満たす。**
        assert!(covering(&records, "c4", "t4", Level::Commit).is_some());
        assert!(covering(&records, "c4b", "t4", Level::Base).is_none());
        assert!(covering(&records, "c5", "t5", Level::Commit).is_some());
        assert!(covering(&records, "c6", "t6", Level::Base).is_none());
    }

    /// **push の前の関門**（運用者の足す1点。2026-09-25）。**記録が在るコミットは通り、無いコミットは
    /// 断られ、旗で越えると記録が残る。** **作った git の木とリモートで確かめる。**
    #[test]
    fn the_push_gate_passes_recorded_commits_refuses_the_rest_and_records_the_flag() {
        let scratch = std::env::temp_dir().join(format!("zaytos-gate-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&scratch);
        let repo = scratch.join("repo");
        let remote = scratch.join("remote.git");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&remote).unwrap();
        let run = |dir: &Path, args: &[&str]| git_line(dir, args).unwrap();
        run(
            &remote,
            &["-c", "init.defaultBranch=main", "init", "-q", "--bare"],
        );
        run(&repo, &["-c", "init.defaultBranch=main", "init", "-q"]);
        let commit = |dir: &Path, path: &str| {
            let file = dir.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, path).unwrap();
            run(dir, &["add", path]);
            run(
                dir,
                &[
                    "-c",
                    "user.name=check",
                    "-c",
                    "user.email=check@localhost",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "-m",
                    path,
                ],
            );
            run(dir, &["rev-parse", "HEAD"])
        };
        commit(&repo, "README");
        let remote_arg = remote.to_string_lossy().into_owned();
        run(&repo, &["remote", "add", "origin", &remote_arg]);
        run(&repo, &["push", "-q", "origin", "main"]);
        let kernel = commit(&repo, "kernel/src/a.rs");
        let docs = commit(&repo, "docs/a.md");
        let pass = |commit: &str, level: Level| Record {
            level: level.label().to_string(),
            ..record("-", "pass", commit, "-", 1)
        };

        // **--commit の要るコミットに基底の記録しか無ければ断る。**
        append(&repo, &pass(&kernel, Level::Base)).unwrap();
        append(&repo, &pass(&docs, Level::Base)).unwrap();
        let refused = gate(&repo, None).unwrap();
        assert_eq!(
            (refused.pending, refused.missing.clone(), refused.overridden),
            (2, vec![kernel.clone()], false)
        );

        // **要る段の合格が在れば通る。**
        append(&repo, &pass(&kernel, Level::Commit)).unwrap();
        assert!(gate(&repo, None).unwrap().missing.is_empty());

        // **旗で越えると、越えたことが記録に残る。**
        let more = commit(&repo, "common/src/b.rs");
        let flagged = gate(&repo, Some("the check was refused during a full check")).unwrap();
        assert_eq!(
            (flagged.missing.clone(), flagged.overridden),
            (vec![more.clone()], true)
        );
        let records = read_records(&repo).unwrap();
        let last = records.last().unwrap();
        assert_eq!(
            (
                last.level.as_str(),
                last.outcome.as_str(),
                last.commit.as_str(),
                last.note.as_str()
            ),
            (
                "push",
                "override",
                more.as_str(),
                "the check was refused during a full check"
            )
        );
        assert!(gate(&repo, None).unwrap().missing.is_empty());
        let _ = fs::remove_dir_all(&scratch);
    }

    /// **4 欄を足す前の 11 欄の行も読む**（足した欄は無いものとして）。
    #[test]
    fn a_record_from_before_the_added_columns_still_reads() {
        let old = "1790293333\t2026-09-25 08:42:13\tbase\tpass\tc\tt\t0\t47\t4.8\t0.1\t-";
        let read = parse_record(old).unwrap();
        assert_eq!(
            (read.items, read.written, read.wsl_free, read.note.as_str()),
            (Some(47), None, None, "-")
        );
        let new = format_record(&record("full", "pass", "c", "t", 0));
        assert_eq!(new.trim_end_matches('\n').split('\t').count(), 15);
        let read = parse_record(new.trim_end_matches('\n')).unwrap();
        assert_eq!(
            (
                read.written,
                read.wsl_free,
                read.host_free,
                read.system_free
            ),
            (Some(4096), Some(1 << 40), None, Some(20 << 30))
        );
    }

    /// **`/proc/diskstats` の 10 番目の欄が書いたセクタ数**（形は実測。2026-09-25）。
    #[test]
    fn the_sectors_written_are_read_from_diskstats() {
        let stats = "   8       0 sda 100 0 200 5 0 0 0 0 0 10 5 0 0 0 0 0 0\n\
                        8      48 sdd 5000 10 400000 900 7000 20 923728 3000 0 4000 3900 0 0 0 0 0 0\n";
        assert_eq!(diskstats_sectors_written(stats, (8, 48)), Some(923_728));
        assert_eq!(diskstats_sectors_written(stats, (8, 0)), Some(0));
        assert_eq!(diskstats_sectors_written(stats, (8, 16)), None);
        assert_eq!(diskstats_sectors_written("", (8, 48)), None);
    }

    /// **全検査の入口の空き**——**見込み＋下限を両方で見て、足りない置き場を全部挙げる。** **WSL の外では
    /// ドライブを見ない。** **読めない置き場も足りないに数える。**
    #[test]
    fn the_full_check_needs_the_estimate_plus_the_floor_on_both_places() {
        let estimate = 61 << 30;
        let wsl_need = estimate + launch::DISK_FLOOR_BYTES;
        let host_need = estimate + launch::HOST_DISK_FLOOR_BYTES;
        assert!(start_shortfalls(estimate, Some(wsl_need), true, Some(host_need)).is_empty());
        assert_eq!(
            start_shortfalls(estimate, Some(wsl_need - 1), true, Some(host_need)).len(),
            1
        );
        assert_eq!(
            start_shortfalls(estimate, Some(wsl_need), true, Some(host_need - 1)).len(),
            1
        );
        assert_eq!(start_shortfalls(estimate, Some(0), true, Some(0)).len(), 2);
        assert_eq!(start_shortfalls(estimate, None, true, None).len(), 2);
        let unreadable = start_shortfalls(estimate, Some(wsl_need), true, None);
        assert!(
            unreadable[0].contains("update HOST_VHD_DRIVE"),
            "{unreadable:?}"
        );
        // **WSL の外（CI）ではドライブを見ない。**
        assert!(start_shortfalls(estimate, Some(wsl_need), false, None).is_empty());
    }

    /// **見込みは前回の全検査の書いた量。** **無ければ代わりの値。** **書いた量の無い全検査の記録は飛ばす。**
    #[test]
    fn the_estimate_comes_from_the_last_full_check_that_recorded_its_writes() {
        let mut first = record("full", "pass", "c1", "t1", 0);
        first.written = Some(50 << 30);
        let mut refused = record("full", "refused", "c2", "t2", 0);
        refused.written = None;
        let mut base = record("base", "pass", "c3", "t3", 0);
        base.written = Some(1);
        let records = vec![first, refused, base];
        assert_eq!(
            estimate_to_write(&records, || Some(7)).map(|pair| pair.0),
            Some(50 << 30)
        );
        assert_eq!(
            estimate_to_write(&[], || Some(7)).map(|pair| pair.0),
            Some(7)
        );
        assert_eq!(estimate_to_write(&[], || None), None);
    }

    /// **記録の読み方は汚れを言う。**
    #[test]
    fn a_description_says_when_the_working_tree_had_other_changes() {
        assert_eq!(
            describe(&record("commit", "pass", "c", "t", 0)),
            "--commit pass at 2026-09-25 10:00:00"
        );
        assert_eq!(
            describe(&record("base", "pass", "c", "t", 2)),
            "the base check pass at 2026-09-25 10:00:00 (the working tree had 2 other change(s))"
        );
    }
}
