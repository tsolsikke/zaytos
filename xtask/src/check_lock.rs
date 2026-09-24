//! 検査の錠（2026-09-25。検査の体系の改善の ③。`ADR-0069` の決定 7 の 3。運用者の決定）。
//!
//! # 何を守るか
//!
//! **全検査（`check --full`）の間は、QEMU と VirtualBox を使う検査を走らせない**——ホストの負荷が
//! 全検査の窓と遅さの計器を揺らす（`CLAUDE.md` の絶対ルール 1）。**全検査は排他で持ち、QEMU を起こす
//! 入口は共有で持つ。** **取れなければ待たずに断る**（終了の値 [`REFUSED_EXIT_CODE`]）——**待たせると、
//! コミットの後の hook の `--commit` が harness の上限で黙って切られる**（運用者の回答 1）。
//!
//! # 置き場は 1 つに固定する——git の共通の置き場の下
//!
//! **`<git rev-parse --git-common-dir>/zaytos/check.lock`**（運用者の回答 3。2026-09-25）。
//! **環境変数で置き場が変わる形は、排他を黙って外す**——**hook の環境に `XDG_RUNTIME_DIR` が無く、
//! 全検査は `/run/user` 側を持つ、という形で錠が 2 つになりうる。**
//!
//! - **根はソースの在り処から決める**（`CARGO_MANIFEST_DIR`。Python の道具は `__file__`）。
//! - **git は `GIT_*` を外して呼ぶ**——**`GIT_DIR` だけで git の答えが変わる**（実測。2026-09-25）。
//! - **本の木からも、どの作業木（`git worktree`）からも同じ道になる**（共通の置き場は 1 つ）。
//! - **置き場は木と同じファイルシステム（ext4）で、flock が効く。** **CI の取り出しにも在る。**
//!
//! **`~/.cache` に固定する案は採らなかった**——**`HOME` も環境変数である。** 外して決めるには passwd を
//! 読むことになり、`libc` を持たない `xtask` では /etc/passwd を手で読むことになる。
//! **限界**——**別の clone は別の錠になる**（いまは clone は 1 つ）。
//!
//! # 取るたびに、置き場が flock を扱えることを確かめる
//!
//! **同じファイルを 2 度開き、片方で排他を取ると、もう片方は断られること**と、**`/proc/locks` に自分の
//! 錠が見えること**を見る（[`probe`]）。**扱えなければ検査装置の故障として止める**（運用者の回答 3）
//! ——**効かない錠は、黙って並走を通す。** **後者は、持ち主を `/proc/locks` で読む形の前提でもある。**
//!
//! # 持ち主の子は取らずに進む
//!
//! **全検査の中から起こす QEMU と道具は、錠を取らない**——**持ち主が排他で持っているので、取ろうと
//! すると自分の親に断られる。** **子へは持ち主の pid を [`OWNER_ENV`] で渡す。** **その pid が自分の
//! 祖先で、かつ `/proc/locks` でこの錠を持っているときだけ、取らずに進む**——**残った環境変数で錠を
//! すり抜けないようにするため。**
//!
//! # 死んだ持ち主
//!
//! **flock はプロセスが終われば（SIGKILL を含む）カーネルが放す**——**錠が残る状態は起きない**
//! （基底の確かめが毎回見る）。**錠の中身（pid・コマンド・コミット・木・開始・ログ）は断るときに
//! 見せるためだけのもので、正は flock である。** **断るときの持ち主は `/proc/locks` から読む**
//! ——**中身は前の持ち主のものが残っていることがある。**

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};

use crate::launch::HarnessFault;

/// 子へ持ち主の pid を渡す環境変数。**置き場は決めない**（置き場は git の共通の置き場だけで決まる）。
pub const OWNER_ENV: &str = "ZAYTOS_CHECK_LOCK_OWNER";

/// 錠が取れずに断ったときの終了の値（`EX_TEMPFAIL`）。**検査の失敗（1）とも上限（3）とも分ける。**
pub const REFUSED_EXIT_CODE: i32 = 75;

/// 錠の取り方。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// QEMU を使う入口（`--commit`・`flaky`・`run`・`screenshot`、起動の口）。**並んで持てる。**
    Shared,
    /// 全検査（`check --full`）。
    Exclusive,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Shared => "shared",
            Mode::Exclusive => "exclusive",
        }
    }

    fn parse(text: &str) -> Option<Mode> {
        match text {
            "shared" => Some(Mode::Shared),
            "exclusive" => Some(Mode::Exclusive),
            _ => None,
        }
    }
}

/// このプロセスの錠の状態。
enum State {
    /// 自分で持っている。**ファイルを開いたまま、プロセスが終わるまで持つ**（閉じると放れる）。
    Held {
        _file: File,
        mode: Mode,
        started_unix: u64,
    },
    /// 祖先の持ち主の下で走っている（取らない）。
    Covered { owner: u32 },
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// 錠の置き場（純粋な論理）。**git の共通の置き場の下の `zaytos/`。**
pub fn lock_dir_in(common_dir: &Path) -> PathBuf {
    common_dir.join("zaytos")
}

/// git を呼ぶ。**`GIT_*` を外す**——**`GIT_DIR` だけで答えが変わる**（実測。2026-09-25）。
pub fn git(root: &Path) -> Command {
    let mut git = Command::new("git");
    git.arg("-C").arg(root).env("LC_ALL", "C");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            git.env_remove(&key);
        }
    }
    git
}

/// git を呼び、標準出力を 1 行の文字列で返す。
pub fn git_line(root: &Path, args: &[&str]) -> Result<String> {
    let output = git(root)
        .args(args)
        .output()
        .with_context(|| format!("could not run git {} in {}", args.join(" "), root.display()))?;
    if !output.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// git の共通の置き場（`rev-parse --git-common-dir`）。
pub fn git_common_dir(root: &Path) -> Result<PathBuf> {
    let text = git_line(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let path = PathBuf::from(text);
    fs::canonicalize(&path).with_context(|| format!("could not resolve {}", path.display()))
}

/// 錠のファイルの道。
pub fn lock_path(root: &Path) -> Result<PathBuf> {
    Ok(lock_dir_in(&git_common_dir(root)?).join("check.lock"))
}

/// 錠を開く（無ければ作る。**中身は消さない**）。
fn open(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))
}

/// `/proc/locks` の 1 行から flock を読む（純粋な論理）。**待っている行（`->`）は読まない。**
///
/// 形は `31: FLOCK  ADVISORY  WRITE 2317934 08:30:1000459 0 EOF`（実測。2026-09-25）。
/// **装置の番号は 16 進、inode は 10 進である。**
fn parse_flock_line(line: &str) -> Option<(Mode, u32, (u32, u32, u64))> {
    let mut fields = line.split_whitespace();
    fields.next()?;
    if fields.next()? != "FLOCK" {
        return None;
    }
    fields.next()?;
    let mode = match fields.next()? {
        "WRITE" => Mode::Exclusive,
        "READ" => Mode::Shared,
        _ => return None,
    };
    let pid = fields.next()?.parse().ok()?;
    let mut device = fields.next()?.split(':');
    let major = u32::from_str_radix(device.next()?, 16).ok()?;
    let minor = u32::from_str_radix(device.next()?, 16).ok()?;
    let inode = device.next()?.parse().ok()?;
    Some((mode, pid, (major, minor, inode)))
}

/// 装置の番号を分ける（glibc の `major`・`minor` と同じ分け方。純粋な論理）。
fn device_numbers(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

/// `/proc/locks` の中身から、あるファイルに flock を持つプロセスを拾う（純粋な論理）。
fn holders_in(locks: &str, file: (u32, u32, u64)) -> Vec<(u32, Mode)> {
    locks
        .lines()
        .filter_map(parse_flock_line)
        .filter(|(_, _, identity)| *identity == file)
        .map(|(mode, pid, _)| (pid, mode))
        .collect()
}

/// このファイルに flock を持つプロセス（`/proc/locks` から）。
fn holders_of(file: &File) -> Vec<(u32, Mode)> {
    let Ok(meta) = file.metadata() else {
        return Vec::new();
    };
    let (major, minor) = device_numbers(meta.dev());
    let locks = fs::read_to_string("/proc/locks").unwrap_or_default();
    holders_in(&locks, (major, minor, meta.ino()))
}

/// `/proc/<pid>/stat` から親の pid を読む（純粋な論理）。**`comm` に空白や `)` が入りうるので、
/// 最後の `)` で切る。**
pub fn parent_pid(stat: &str) -> Option<u32> {
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// 自分の祖先（親から順に。上限 64 段）。
fn ancestors() -> Vec<u32> {
    let mut found = Vec::new();
    let mut stat = fs::read_to_string("/proc/self/stat").unwrap_or_default();
    for _ in 0..64 {
        let Some(parent) = parent_pid(&stat) else {
            break;
        };
        if parent == 0 {
            break;
        }
        found.push(parent);
        if parent == 1 {
            break;
        }
        match fs::read_to_string(format!("/proc/{parent}/stat")) {
            Ok(text) => stat = text,
            Err(_) => break,
        }
    }
    found
}

/// 持ち主の下で走っているか（純粋な論理）。**名前の pid が祖先で、その pid がこの錠を持っていること。**
/// **排他を求めるなら、持ち主も排他で持っていること**（共有の持ち主の下では、ほかの共有が居うる）。
fn covering_owner(
    named: Option<&str>,
    ancestors: &[u32],
    holders: &[(u32, Mode)],
    wanted: Mode,
) -> Option<u32> {
    let owner: u32 = named?.trim().parse().ok()?;
    let holds = holders
        .iter()
        .any(|(pid, mode)| *pid == owner && (wanted == Mode::Shared || *mode == Mode::Exclusive));
    (ancestors.contains(&owner) && holds).then_some(owner)
}

/// 置き場のファイルシステムが flock を扱えることを確かめる（[`probe_at`]）。**扱えなければ検査装置の
/// 故障である**（運用者の回答 3）。**確かめのファイルは pid ごとに作って消す**——**並んで確かめても
/// 互いを邪魔しない。**
fn probe(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let path = dir.join(format!("probe-{}", std::process::id()));
    let result = probe_at(&path);
    let _ = fs::remove_file(&path);
    result.map_err(|error| {
        anyhow::Error::new(HarnessFault(format!(
            "the check lock's directory {} does not handle flock as the lock needs: {error:#}",
            dir.display()
        )))
    })
}

fn probe_at(path: &Path) -> Result<()> {
    let first = open(path)?;
    let second = open(path)?;
    first
        .try_lock()
        .map_err(|error| anyhow!("could not lock the probe: {error}"))?;
    match second.try_lock_shared() {
        Err(TryLockError::WouldBlock) => {}
        Ok(()) => bail!("a second open of the same file got the lock while the first held it"),
        Err(TryLockError::Error(error)) => bail!("the second open could not try the lock: {error}"),
    }
    let me = std::process::id();
    if !holders_of(&first)
        .iter()
        .any(|(pid, mode)| *pid == me && *mode == Mode::Exclusive)
    {
        bail!("/proc/locks does not show this process holding the probe's lock");
    }
    first
        .unlock()
        .map_err(|error| anyhow!("could not unlock the probe: {error}"))
}

/// いまの時刻（エポック秒と、人が読む地方時）。**地方時は `date` に任せる**（依存を増やさない）。
pub fn now() -> (u64, String) {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let local = Command::new("date")
        .arg(format!("-d@{unix}"))
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "?".to_string());
    (unix, local)
}

/// 錠の中身（排他の持ち主が書く。**断るときに見せるためだけのもの**）。
pub fn owner_content(command: &str, commit: &str, tree: &str, log: &str) -> String {
    let (unix, local) = now();
    format!(
        "pid: {}\ncommand: {command}\ncommit: {commit}\ntree: {tree}\nstarted: {local}\n\
         started-unix: {unix}\nlog: {log}\n",
        std::process::id()
    )
}

/// 中身から開始のエポック秒を読む（純粋な論理）。
fn started_unix_in(content: &str) -> Option<u64> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("started-unix: "))
        .and_then(|value| value.trim().parse().ok())
}

/// 断ったときに見せるもの。
struct Refusal {
    holders: Vec<(u32, Mode, String)>,
    content: String,
}

/// 取った結果。
enum Attempt {
    Taken(State),
    Refused(Refusal),
}

/// あるファイルで錠を取ってみる（本の錠と、基底の確かめの一時の錠が使う）。
fn attempt(path: &Path, mode: Mode, content: Option<&str>) -> Result<Attempt> {
    let dir = path.parent().context("the lock path has no directory")?;
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    probe(dir)?;
    let file = open(path)?;
    let named = std::env::var(OWNER_ENV).ok();
    if let Some(owner) = covering_owner(named.as_deref(), &ancestors(), &holders_of(&file), mode) {
        return Ok(Attempt::Taken(State::Covered { owner }));
    }
    if let Some(named) = &named {
        // **言ってから取りに行く**——**黙って取りに行くと、持ち主の子が断られた理由が読めない。**
        eprintln!(
            "(info) the check lock: {OWNER_ENV}={named} does not name an ancestor holding {} as \
             needed; taking the lock in the usual way",
            path.display()
        );
    }
    let tried = match mode {
        Mode::Shared => file.try_lock_shared(),
        Mode::Exclusive => file.try_lock(),
    };
    match tried {
        Ok(()) => {
            if let (Mode::Exclusive, Some(content)) = (mode, content) {
                write_content(&file, content)?;
            }
            Ok(Attempt::Taken(State::Held {
                _file: file,
                mode,
                started_unix: now().0,
            }))
        }
        Err(TryLockError::WouldBlock) => Ok(Attempt::Refused(refusal(&file))),
        Err(TryLockError::Error(error)) => Err(anyhow::Error::new(HarnessFault(format!(
            "could not try the check lock at {}: {error}",
            path.display()
        )))),
    }
}

fn write_content(mut file: &File, content: &str) -> Result<()> {
    file.set_len(0)
        .context("could not clear the lock's content")?;
    file.seek(SeekFrom::Start(0))
        .context("could not rewind the lock")?;
    file.write_all(content.as_bytes())
        .context("could not write the lock's content")
}

fn read_content(mut file: &File) -> String {
    let mut content = String::new();
    let _ = file
        .seek(SeekFrom::Start(0))
        .and_then(|_| file.read_to_string(&mut content));
    content
}

fn refusal(file: &File) -> Refusal {
    let holders = holders_of(file)
        .into_iter()
        .map(|(pid, mode)| {
            let command = fs::read(format!("/proc/{pid}/cmdline"))
                .map(|bytes| {
                    String::from_utf8_lossy(&bytes)
                        .split('\0')
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_else(|_| "(gone)".to_string());
            (pid, mode, command)
        })
        .collect();
    Refusal {
        holders,
        content: read_content(file),
    }
}

/// 断りの文（純粋な論理）。
fn refusal_message(what: &str, path: &Path, refusal: &Refusal) -> String {
    let mut text = format!(
        "xtask: the check lock is held, so `{what}` was not run (exit {REFUSED_EXIT_CODE}).\n  \
         lock: {}\n",
        path.display()
    );
    if refusal.holders.is_empty() {
        text.push_str("  holder: none listed in /proc/locks (it may have just ended; try again)\n");
    }
    for (pid, mode, command) in &refusal.holders {
        text.push_str(&format!(
            "  holder: pid {pid} ({}) {command}\n",
            mode.label()
        ));
    }
    let exclusive = refusal
        .holders
        .iter()
        .find(|(_, mode, _)| *mode == Mode::Exclusive);
    if let Some((pid, _, _)) = exclusive {
        if refusal.content.starts_with(&format!("pid: {pid}\n")) {
            text.push_str("  the full check that holds it wrote:\n");
            for line in refusal.content.lines() {
                text.push_str(&format!("    {line}\n"));
            }
        }
    }
    text.push_str(
        "  A full check holds the lock for its whole run, and QEMU and VirtualBox checks are refused \
         meanwhile; a full check is refused while any of them runs. Run this again after the \
         holder ends.",
    );
    text
}

/// 錠を取る。**取れなければ、断りを出して [`REFUSED_EXIT_CODE`] で終える。**
///
/// **既に持っているか、持ち主の下で走っているなら何もしない。** `content` は排他で取ったときに
/// 錠へ書く中身である（[`owner_content`]）。
pub fn hold_or_exit(mode: Mode, what: &str, content: Option<String>) -> Result<()> {
    {
        let state = STATE
            .lock()
            .map_err(|_| anyhow!("the check lock's state was poisoned"))?;
        match &*state {
            Some(State::Held { mode: held, .. }) => {
                if mode == Mode::Exclusive && *held == Mode::Shared {
                    bail!("`{what}` asked for the check lock exclusively while holding it shared");
                }
                return Ok(());
            }
            Some(State::Covered { .. }) => return Ok(()),
            None => {}
        }
    }
    let path = lock_path(&crate::workspace_root()?)?;
    match attempt(&path, mode, content.as_deref())? {
        Attempt::Taken(state) => {
            if let Ok(mut slot) = STATE.lock() {
                *slot = Some(state);
            }
            Ok(())
        }
        Attempt::Refused(refusal) => {
            log_run(what, "refused");
            eprintln!("{}", refusal_message(what, &path, &refusal));
            std::process::exit(REFUSED_EXIT_CODE);
        }
    }
}

/// QEMU を起こす前に呼ぶ（起動の口の裏打ち）。**入口で取り損ねた経路も、ここで取る。**
pub fn hold_for_qemu(what: &str) -> Result<()> {
    hold_or_exit(Mode::Shared, what, None)
}

/// 子へ渡す持ち主の pid（持っていれば自分、持ち主の下なら持ち主）。**持っていなければ `None`。**
pub fn owner_for_children() -> Option<u32> {
    match &*STATE.lock().ok()? {
        Some(State::Held { .. }) => Some(std::process::id()),
        Some(State::Covered { owner }) => Some(*owner),
        None => None,
    }
}

/// 子のコマンドへ持ち主の pid を渡す（持っていなければ何もしない）。
pub fn pass_owner(command: &mut Command) {
    if let Some(owner) = owner_for_children() {
        command.env(OWNER_ENV, owner.to_string());
    }
}

/// 全検査が走っているか（`/proc/locks` に排他の持ち主が居るか）と、その下で走っているか。
/// **錠は取らない**——**基底は錠を取らずに走るので、見るだけである。**
fn full_check_state(root: &Path) -> Option<(bool, bool)> {
    let path = lock_path(root).ok()?;
    let file = OpenOptions::new().read(true).open(&path).ok()?;
    let holders = holders_of(&file);
    let running = holders.iter().any(|(_, mode)| *mode == Mode::Exclusive);
    let named = std::env::var(OWNER_ENV).ok();
    let covered = covering_owner(named.as_deref(), &ancestors(), &holders, Mode::Shared).is_some();
    Some((running, covered))
}

/// 基底の入口で呼ぶ。**全検査の間に走ったことだけを残す**（運用者の決定 (7)。止めない）。
///
/// **共有の持ち主は全検査と重ならない**（重なれば、どちらかが断られている）。**重なるのは、錠を
/// 取らない基底と、断られた走行だけである**——**だから残すのはその 2 つだけでよい。**
pub fn note_a_run_without_the_lock(what: &str) {
    let Ok(root) = crate::workspace_root() else {
        return;
    };
    if let Some((true, false)) = full_check_state(&root) {
        log_run(what, "ran during a full check");
    }
}

/// 走行を 1 行残す（`<錠の置き場>/runs.tsv`）。**止めない**——**残せなくても検査は進める。**
pub fn log_run(what: &str, outcome: &str) {
    let Ok(root) = crate::workspace_root() else {
        return;
    };
    let Ok(common) = git_common_dir(&root) else {
        return;
    };
    let (unix, local) = now();
    let line = format!(
        "{unix}\t{local}\t{}\t{what}\t{outcome}\t{}\n",
        std::process::id(),
        root.display()
    );
    let path = lock_dir_in(&common).join("runs.tsv");
    let _ = fs::create_dir_all(lock_dir_in(&common));
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// 残した走行のうち、`since` 以降のもの（純粋な論理）。
fn runs_since(text: &str, since: u64) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let unix: u64 = fields.first()?.parse().ok()?;
            (unix >= since && fields.len() >= 5)
                .then(|| format!("{} {} ({})", fields[1], fields[3], fields[4]))
        })
        .collect()
}

/// この全検査の間に走った他の検査（運用者の決定 (7)）。**持ち主の開始から数える。**
/// **持ち主でも持ち主の下でもなければ `None`。**
pub fn other_runs_during_this_full() -> Option<Vec<String>> {
    let root = crate::workspace_root().ok()?;
    let path = lock_path(&root).ok()?;
    let since = match &*STATE.lock().ok()? {
        Some(State::Held { started_unix, .. }) => *started_unix,
        Some(State::Covered { .. }) => {
            let file = OpenOptions::new().read(true).open(&path).ok()?;
            started_unix_in(&read_content(&file))?
        }
        None => return None,
    };
    let runs = fs::read_to_string(path.with_file_name("runs.tsv")).unwrap_or_default();
    Some(runs_since(&runs, since))
}

/// 基底の確かめ（2026-09-25。運用者の回答 3）。**錠の道が 3 か所から同じで、flock が効き、殺された
/// 持ち主の錠が放れ、断りが 75 で終わり、持ち主の子だけが取らずに進むこと。**
pub fn self_check(root: &Path) -> Result<String> {
    let exe = std::env::current_exe().context("could not find the xtask binary")?;
    let scratch = root.join("target").join("check-lock");
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)
        .with_context(|| format!("could not create {}", scratch.display()))?;

    // (1) 道——本の木（このプロセス）、環境を減らした子、全検査の作業木（在れば）、作った作業木。
    let here = lock_path(root)?;
    let reduced = reduced_env_path(&exe, None)?;
    if reduced != here {
        bail!(
            "the lock path differs with a reduced environment: {} here, {} there",
            here.display(),
            reduced.display()
        );
    }
    let mut places = vec!["the main tree", "a reduced environment with GIT_DIR set"];
    let full_worktree = root.join("target").join("full-check").join("wt");
    if full_worktree.join(".git").is_file() {
        let there = lock_path(&full_worktree)?;
        if there != here {
            bail!(
                "the lock path differs in the full-check worktree: {} here, {} there",
                here.display(),
                there.display()
            );
        }
        places.push("the full-check worktree");
    }
    let (repo, worktree) = make_repo_with_worktree(&scratch)?;
    let (from_repo, from_worktree) = (lock_path(&repo)?, lock_path(&worktree)?);
    let from_worktree_reduced = reduced_env_path(&exe, Some(&worktree))?;
    if from_repo != from_worktree || from_repo != from_worktree_reduced {
        bail!(
            "a made worktree gave another lock path: {} from its main tree, {} from the worktree, \
             {} from the worktree with a reduced environment",
            from_repo.display(),
            from_worktree.display(),
            from_worktree_reduced.display()
        );
    }
    places.push("a made worktree (in the usual and a reduced environment)");

    // (2) flock が効く（取るたびにも見る）。
    probe(here.parent().context("the lock path has no directory")?)?;

    // (3) 殺された持ち主の錠は放れる。(4) 断りは 75。(5) 持ち主の子だけが取らずに進む。
    let lock = scratch.join("check.lock");
    let mut holder = spawn_holder(&exe, &lock, &[])?;
    let outcome = (|| -> Result<()> {
        let file = open(&lock)?;
        if !matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock)) {
            bail!("the lock was free while a child held it exclusively");
        }
        let refused = try_in_child(&exe, &lock, Mode::Shared, None)?;
        if refused.0 != Some(REFUSED_EXIT_CODE)
            || !refused.1.contains(&format!("pid {}", holder.id()))
        {
            bail!(
                "a shared try while a child held the lock ended with {:?}, not {REFUSED_EXIT_CODE} \
                 naming the holder: {}",
                refused.0,
                refused.1
            );
        }
        let stranger = try_in_child(&exe, &lock, Mode::Shared, Some(holder.id()))?;
        if stranger.0 != Some(REFUSED_EXIT_CODE) {
            bail!(
                "a try that named the holder without being its descendant ended with {:?}, not \
                 {REFUSED_EXIT_CODE}: {}",
                stranger.0,
                stranger.1
            );
        }
        Ok(())
    })();
    let _ = holder.kill();
    let _ = holder.wait();
    outcome?;
    let file = open(&lock)?;
    file.try_lock()
        .map_err(|error| anyhow!("the lock stayed held after its holder was killed: {error}"))?;
    file.unlock()
        .map_err(|error| anyhow!("could not unlock: {error}"))?;
    drop(file);

    let mut parent = spawn_holder(&exe, &lock, &["--then-try", "shared"])?;
    let mut report = String::new();
    let waited = read_line_within(
        parent.stdout.take().context("no stdout from the holder")?,
        Duration::from_secs(20),
        2,
    );
    let _ = parent.kill();
    let _ = parent.wait();
    if let Some(lines) = waited {
        report = lines.join(" / ");
    }
    if !report.contains("child: covered by") {
        bail!("a child of the holder did not go ahead under it: {report}");
    }
    let _ = fs::remove_dir_all(&scratch);
    Ok(format!(
        "one path from {}; flock works there; a killed holder's lock was released; a refusal ended \
         with {REFUSED_EXIT_CODE} and named the holder; only a descendant of the holder went ahead \
         without taking it",
        places.join(", ")
    ))
}

/// 環境を減らした子で道を出させる（`GIT_DIR` を嘘の場所に向けて）。
fn reduced_env_path(exe: &Path, root: Option<&Path>) -> Result<PathBuf> {
    let mut child = Command::new(exe);
    child
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_DIR", "/nonexistent-git-dir")
        .env("GIT_COMMON_DIR", "/nonexistent-git-dir")
        .args(["check-lock", "path"]);
    if let Some(root) = root {
        child.arg("--root").arg(root);
    }
    let output = child
        .output()
        .context("could not run xtask with a reduced environment")?;
    if !output.status.success() {
        bail!(
            "xtask check-lock path failed with a reduced environment: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

/// 一時の置き場に、作業木を 1 つ持つ git の木を作る。
fn make_repo_with_worktree(scratch: &Path) -> Result<(PathBuf, PathBuf)> {
    let repo = scratch.join("repo");
    let worktree = scratch.join("wt");
    fs::create_dir_all(&repo).with_context(|| format!("could not create {}", repo.display()))?;
    for args in [
        vec!["-c", "init.defaultBranch=main", "init", "-q"],
        vec![
            "-c",
            "user.name=check",
            "-c",
            "user.email=check@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "check",
        ],
    ] {
        git_line(&repo, &args)?;
    }
    let worktree_arg = worktree.to_string_lossy().into_owned();
    git_line(&repo, &["worktree", "add", "-q", "--detach", &worktree_arg])?;
    Ok((repo, worktree))
}

/// 一時の錠を排他で持つ子を起こし、持ったことを確かめる。
fn spawn_holder(exe: &Path, lock: &Path, extra: &[&str]) -> Result<std::process::Child> {
    let mut child = Command::new(exe)
        .args(["check-lock", "hold"])
        .arg(lock)
        .arg("exclusive")
        .args(extra)
        .env_remove(OWNER_ENV)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start a child to hold the lock")?;
    if extra.is_empty() {
        let stdout = child.stdout.take().context("no stdout from the holder")?;
        let lines = read_line_within(stdout, Duration::from_secs(20), 1);
        if lines
            .as_deref()
            .and_then(|lines| lines.first())
            .map(String::as_str)
            != Some("held")
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the child did not report holding the lock within 20 s: {lines:?}");
        }
    }
    Ok(child)
}

/// 行を `count` 本、上限つきで読む（上限を過ぎたら `None`）。
fn read_line_within(
    stdout: std::process::ChildStdout,
    limit: Duration,
    count: usize,
) -> Option<Vec<String>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines().take(count) {
            match line {
                Ok(line) => lines.push(line),
                Err(_) => break,
            }
        }
        let _ = sender.send(lines);
    });
    receiver.recv_timeout(limit).ok()
}

/// 子で一時の錠を取ってみる（終了の値と標準エラー）。
fn try_in_child(
    exe: &Path,
    lock: &Path,
    mode: Mode,
    named: Option<u32>,
) -> Result<(Option<i32>, String)> {
    let mut child = Command::new(exe);
    child
        .args(["check-lock", "try"])
        .arg(lock)
        .arg(mode.label())
        .env_remove(OWNER_ENV);
    if let Some(named) = named {
        child.env(OWNER_ENV, named.to_string());
    }
    let output = output_within(&mut child, Duration::from_secs(20))
        .context("could not run a child to try the lock")?;
    Ok((
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// 子を走らせ、上限つきで終わりを待つ（`CLAUDE.md` の「シェルコマンドの制約」）。**上限を過ぎたら止めて
/// 誤りにする。** 出力は小さいものだけに使う。
fn output_within(command: &mut Command, limit: Duration) -> Result<std::process::Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start the child")?;
    let mut stdout = child.stdout.take().context("no stdout from the child")?;
    let mut stderr = child.stderr.take().context("no stderr from the child")?;
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("could not wait for the child")? {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the child did not end within {} s", limit.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(std::process::Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// 隠した副命令 `cargo xtask check-lock ...`（基底の確かめが使う）。
///
/// - `path [--root DIR]`——錠の道を出す。
/// - `hold FILE MODE [--then-try MODE]`——一時の錠を持ち、`held` と出して眠る（上限 60 秒）。
///   `--then-try` なら、持ったまま自分の子に同じ錠を取らせ、その結果を出す。
/// - `try FILE MODE`——一時の錠を取ってみる（取れれば 0、断られれば 75）。
pub fn command(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("path") => {
            let root = match args.iter().position(|arg| arg == "--root") {
                Some(index) => PathBuf::from(args.get(index + 1).context("--root needs a directory")?),
                None => crate::workspace_root()?,
            };
            println!("{}", lock_path(&root)?.display());
            Ok(())
        }
        Some("hold") => {
            let file = PathBuf::from(args.get(1).context("hold needs a file")?);
            let mode = args
                .get(2)
                .and_then(|text| Mode::parse(text))
                .context("hold needs shared or exclusive")?;
            let Attempt::Taken(state) = attempt(&file, mode, Some("pid: held by a check\n"))? else {
                bail!("the lock was already held");
            };
            let then = args
                .iter()
                .position(|arg| arg == "--then-try")
                .and_then(|index| args.get(index + 1))
                .and_then(|text| Mode::parse(text));
            println!("held");
            std::io::stdout().flush().ok();
            if let Some(then) = then {
                let exe = std::env::current_exe().context("could not find the xtask binary")?;
                let mut child = Command::new(exe);
                child
                    .args(["check-lock", "try"])
                    .arg(&file)
                    .arg(then.label())
                    .env(OWNER_ENV, std::process::id().to_string());
                let output = output_within(&mut child, Duration::from_secs(20))?;
                println!(
                    "child: {} (exit {:?})",
                    String::from_utf8_lossy(&output.stdout).trim(),
                    output.status.code()
                );
                std::io::stdout().flush().ok();
            }
            // **上限つきで眠る**——**親が死んでも、60 秒で自分で降りる。**
            std::thread::sleep(Duration::from_secs(60));
            drop(state);
            Ok(())
        }
        Some("try") => {
            let file = PathBuf::from(args.get(1).context("try needs a file")?);
            let mode = args
                .get(2)
                .and_then(|text| Mode::parse(text))
                .context("try needs shared or exclusive")?;
            match attempt(&file, mode, None)? {
                Attempt::Taken(State::Covered { owner }) => {
                    println!("covered by {owner}");
                    Ok(())
                }
                Attempt::Taken(_) => {
                    println!("taken");
                    Ok(())
                }
                Attempt::Refused(refusal) => {
                    eprintln!("{}", refusal_message("check-lock try", &file, &refusal));
                    std::process::exit(REFUSED_EXIT_CODE);
                }
            }
        }
        _ => bail!("usage: cargo xtask check-lock path [--root DIR] | hold FILE MODE [--then-try MODE] | try FILE MODE"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **`/proc/locks` の flock の行を読む**（形は実測。2026-09-25）。**待っている行と POSIX の錠は読まない。**
    #[test]
    fn flock_lines_are_read_from_proc_locks() {
        let locks = "31: FLOCK  ADVISORY  WRITE 2317934 08:30:1000459 0 EOF\n\
                     32: FLOCK  ADVISORY  READ  4242 08:30:1000459 0 EOF\n\
                     32: -> FLOCK  ADVISORY  WRITE 4343 08:30:1000459 0 EOF\n\
                     33: POSIX  ADVISORY  WRITE 5555 08:30:1000459 0 EOF\n\
                     34: FLOCK  ADVISORY  WRITE 6666 08:31:1000459 0 EOF\n";
        assert_eq!(
            holders_in(locks, (0x08, 0x30, 1_000_459)),
            vec![(2_317_934, Mode::Exclusive), (4242, Mode::Shared)]
        );
    }

    /// **装置の番号は glibc の分け方で出す**——**`/proc/locks` の `08:30` と比べられる**（実測。
    /// 2026-09-25 に、ここの木の置き場で `08:30` だった）。
    #[test]
    fn device_numbers_split_as_glibc_does() {
        assert_eq!(device_numbers(0x830), (0x08, 0x30));
        assert_eq!(device_numbers((259 << 8) | 3), (259, 3));
        assert_eq!(
            device_numbers(((0x1234u64 & !0xff) << 12) | 0x34 | (8 << 8)),
            (8, 0x1234)
        );
    }

    /// **祖先で、しかも錠を持つ pid の下でだけ取らずに進む。** **排他を求めるなら、持ち主も排他。**
    #[test]
    fn only_a_descendant_of_a_holder_goes_ahead_without_the_lock() {
        let holders = [(100, Mode::Exclusive), (200, Mode::Shared)];
        assert_eq!(
            covering_owner(Some("100"), &[50, 100, 1], &holders, Mode::Shared),
            Some(100)
        );
        assert_eq!(
            covering_owner(Some("100"), &[50, 100, 1], &holders, Mode::Exclusive),
            Some(100)
        );
        // **祖先でない**（残った環境変数）。
        assert_eq!(
            covering_owner(Some("100"), &[50, 1], &holders, Mode::Shared),
            None
        );
        // **祖先だが持っていない。**
        assert_eq!(
            covering_owner(Some("50"), &[50, 1], &holders, Mode::Shared),
            None
        );
        // **共有の持ち主の下で排他は求められない。**
        assert_eq!(
            covering_owner(Some("200"), &[200, 1], &holders, Mode::Exclusive),
            None
        );
        assert_eq!(covering_owner(None, &[100], &holders, Mode::Shared), None);
        assert_eq!(
            covering_owner(Some("x"), &[100], &holders, Mode::Shared),
            None
        );
    }

    /// **`comm` に空白や `)` が入っても、親の pid を読む。**
    #[test]
    fn the_parent_pid_is_read_after_the_last_parenthesis() {
        assert_eq!(parent_pid("123 (xtask) S 45 123 45 0"), Some(45));
        assert_eq!(parent_pid("123 (a b) c)) S 67 123"), Some(67));
        assert_eq!(parent_pid("garbage"), None);
    }

    /// **全検査の間に走った他の検査は、持ち主の開始より後の行だけを数える。**
    #[test]
    fn other_runs_are_counted_from_the_start_of_the_full_check() {
        let runs = "100\t2026-09-25 10:00:00\t1\tcargo xtask check\tran during a full check\t/r\n\
                    200\t2026-09-25 10:01:40\t2\tcargo xtask check --commit\trefused\t/r\n\
                    broken line\n";
        assert_eq!(
            runs_since(runs, 150),
            vec!["2026-09-25 10:01:40 cargo xtask check --commit (refused)".to_string()]
        );
        assert_eq!(runs_since(runs, 50).len(), 2);
        assert_eq!(
            started_unix_in("pid: 1\nstarted-unix: 1790000000\n"),
            Some(1_790_000_000)
        );
        assert_eq!(started_unix_in("pid: 1\n"), None);
    }

    /// **断りの文は持ち主を挙げ、中身は今の排他の持ち主のものだけを見せる。**
    #[test]
    fn a_refusal_names_the_holder_and_shows_only_its_own_content() {
        let path = Path::new("/r/.git/zaytos/check.lock");
        let current = Refusal {
            holders: vec![(42, Mode::Exclusive, "xtask full".to_string())],
            content: "pid: 42\ncommit: abc\n".to_string(),
        };
        let text = refusal_message("cargo xtask check --commit", path, &current);
        assert!(text.contains("exit 75"), "{text}");
        assert!(
            text.contains("holder: pid 42 (exclusive) xtask full"),
            "{text}"
        );
        assert!(text.contains("    commit: abc"), "{text}");
        let stale = Refusal {
            holders: vec![(43, Mode::Shared, "xtask run".to_string())],
            content: "pid: 42\ncommit: abc\n".to_string(),
        };
        let text = refusal_message("cargo xtask check --full", path, &stale);
        assert!(!text.contains("commit: abc"), "{text}");
    }
}
