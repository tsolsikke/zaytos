//! kernel のビルドを 1 本の裏の流れにまとめ、全検査の間に QEMU の試験の裏で先に作る（2026-09-29。運用者の決定。
//! 試験の時間を縮める案の A）。
//!
//! # なぜ在るのか
//!
//! **冷えた全検査は、feature の組ごとに kernel を約 340 回コンパイルし直し、40〜51 分かかる**（2026-09-25〜28 の
//! 実測）。**1 回のコンパイルは CPU を 1 つ分しか使わず（102%）、QEMU の試験も 1 つに届かない（66%）**——WSL が
//! 使える 8 つのうち、残りは空いている。**ビルドを試験の裏で先に済ませれば、冷えた回の所要はビルド以外の時間まで
//! 縮む**（2026-09-28 の夜の回の項目ごとの時間で計算すると、128.1 分が 78.9 分）。
//!
//! # 取り違えを起こさない形
//!
//! **cargo は、組によらず同じ置き場（`target/x86_64-unknown-none/debug/kernel`）へ ELF を書く。** **全検査の QEMU の
//! 区間では、kernel のビルドをこの流れだけが行う**（項目は結果を受け取るだけ）。**基本の検査の区間では、表 `CHECKS` の
//! `build kernel (none)` と `tools/frame-sizes.py` も作業ツリーで kernel を作る**——流れはそのとき何も作っていない
//! （先に作り始める前で、項目も求めていない）。**作った直後、次のビルドを始める前に、ELF を組ごとの置き場へ写し、
//! 同じ cargo の出力から取った `OUT_DIR` と対にして返す**——`KernelBuild` の doc の「ビルドした側と載せる側を対にして
//! 持つ」を保つ。
//!
//! **写した後、その組の features で作った cargo の成果物と同じ中身かを確かめ、違えば失敗にする**（[`confirm_the_copy`]。
//! 2026-09-29。運用者の決定）。**上の前提は、読んで確かめたものでしかない**——この先、QEMU の区間でほかの cargo が
//! kernel を作る項目が入ると、写す前に ELF が書き換わりうる。**そのとき、組と違う kernel で試験が走り、しかも通る、
//! という気づけない形にしない。** 比べる相手は、組ごとに名前の違う成果物（`deps/kernel-<hash>`）で、同じ hash の
//! fingerprint が有効だった features を持つ（[`Artifacts`]）。
//!
//! **ブートローダも、全検査の間は組ごとに 1 回だけ作って写しを使う**（[`bootloader`]）。**項目ごとに cargo を
//! 呼ぶと、裏のビルドが持つ `target/` の鍵を待たされる**（2026-09-29 の実測で 7.84 秒）。
//!
//! # 順番
//!
//! **前回の全検査が kernel を求めた順で先に作る**（git の共通の置き場の `zaytos/kernel-build-order.txt`。
//! 全検査の終わりに書く）。**無ければ、cargo が `target/` に残した組ごとの記録（fingerprint）を、作った時刻の
//! 順に並べて使う**（入れた後の最初の回のため）。**当たらなかった組は、求められたときに先に作る。**
//! **先に作り始めるのは [`build_ahead`] の後である**——基本の検査の項目（`cargo test` や `clippy`）が
//! `target/` の鍵を待たないように、QEMU の項目の手前で始める。
//!
//! # 組の名前
//!
//! **同じビルドには 1 つの名前を付ける**（[`canonical_key`]。2026-09-29。運用者の決定）。**項目が求める組・順の
//! 記録・fingerprint の 3 つが、同じ関数を通る**——空の名前と、既定の構成やほかの feature から辿れる feature を落とす。
//! **入れた後の最初の全検査で、名前が食い違った**: fingerprint からは依存で縮めた名前（`pipe-write-does-not-wake-reader`）を、
//! 項目は親も並べた名前（`pipe-test,pipe-write-does-not-wake-reader`）を使い、36 組を先に作ったのに使えず、求められてから
//! 作り直した。`ACPI_SMP_TESTS` の `feature: ""` も、既定の構成と別の組として 2 度作った。
//!
//! # 全検査でだけ使う
//!
//! **基本の検査・`--commit`・手の実行は、今までどおりその場でビルドする**（[`is_running`] が偽）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

/// feature の組（名前を並べ替え、重ねを除いたもの）。**空は既定の構成である。**
pub type Key = Vec<String>;

/// 1 回のビルドの結果。
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    pub elf: PathBuf,
    pub out_dir: PathBuf,
    /// cargo が stderr へ書いたもの（**最初に求めた項目の中で出す**）。
    pub cargo_output: String,
}

/// ビルドする関数（本物は `main.rs` が渡す。テストは偽物を渡す）。
pub type Builder = dyn Fn(&Key) -> std::result::Result<Outcome, String> + Send + Sync;

/// 組の名前を揃える関数（本物は `main.rs` が feature の表から作り、[`canonical_key`] を呼ぶ）。
pub type Normalize = dyn Fn(&[&str]) -> Key + Send + Sync;

/// 項目が受け取るもの。
#[derive(Debug)]
pub struct Received {
    pub result: std::result::Result<Outcome, String>,
    /// この組を初めて受け取ったか（**cargo の出力と、先に作れたかの行は、初めての項目の中でだけ出す**）。
    pub first: bool,
    /// 求める前に、どれだけ前に作り終えていたか（**先に作れた組だけ**）。
    pub ready_before: Option<Duration>,
    /// 求めてから受け取るまで待った時間。
    pub waited: Duration,
    /// cargo が掛かった時間。
    pub build_seconds: f64,
}

struct Done {
    result: std::result::Result<Outcome, String>,
    finished: Instant,
    seconds: f64,
    asked: bool,
    taken: bool,
}

#[derive(Default)]
struct Queue {
    ahead: VecDeque<Key>,
    asked: VecDeque<Key>,
    building: Option<Key>,
    done: HashMap<Key, Done>,
    ahead_enabled: bool,
    stopping: bool,
    /// 項目が初めて求めた順（**全検査の終わりに、次の回の順として書く**）。
    requests: Vec<Key>,
    asks: usize,
    ready_on_ask: usize,
    waited: Duration,
}

/// 流れの本体（**テストは偽物のビルドで直に使う**）。
pub struct Service {
    queue: Mutex<Queue>,
    changed: Condvar,
    /// 組の名前を揃える関数（**先に作る順と、項目が求める組の両方に使う**）。
    normalize: Arc<Normalize>,
}

impl Service {
    /// **先に作る順も、ここで名前を揃える**（記録や fingerprint がどんな名前で書いていても、求める側と同じ名前になる）。
    pub fn new(ahead: Vec<Key>, normalize: Arc<Normalize>) -> Self {
        Service {
            queue: Mutex::new(Queue {
                ahead: canonical_order(&ahead, normalize.as_ref()).into(),
                ..Queue::default()
            }),
            changed: Condvar::new(),
            normalize,
        }
    }

    /// feature の名前で組を求める（**名前を揃えてから [`Service::ask`] へ渡す**）。
    pub fn ask_features(&self, features: &[&str]) -> Received {
        self.ask((self.normalize)(features))
    }

    /// 先に作り始める。
    pub fn build_ahead(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.ahead_enabled = true;
        }
        self.changed.notify_all();
    }

    /// 止める（**作っている 1 つは作り終える。先の分は始めない**）。
    pub fn stop(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.stopping = true;
        }
        self.changed.notify_all();
    }

    /// 裏の流れ（止めるまで回る）。**1 度に 1 つだけ作る**——cargo の置き場の ELF を写し終えてから次を始める。
    pub fn run(&self, build: &Builder) {
        loop {
            let (key, asked) = {
                let Ok(mut queue) = self.queue.lock() else {
                    return;
                };
                loop {
                    if let Some(job) = next_job(&mut queue) {
                        queue.building = Some(job.0.clone());
                        break job;
                    }
                    if queue.stopping {
                        return;
                    }
                    queue = match self.changed.wait(queue) {
                        Ok(queue) => queue,
                        Err(_) => return,
                    };
                }
            };
            let started = Instant::now();
            let result = build(&key);
            let seconds = started.elapsed().as_secs_f64();
            if let Ok(mut queue) = self.queue.lock() {
                queue.building = None;
                queue.done.insert(
                    key,
                    Done {
                        result,
                        finished: Instant::now(),
                        seconds,
                        asked,
                        taken: false,
                    },
                );
            }
            self.changed.notify_all();
        }
    }

    /// 組を求めて、できるまで待つ。**まだ作っていなければ、次に作る。**
    pub fn ask(&self, key: Key) -> Received {
        let asked_at = Instant::now();
        let Ok(mut queue) = self.queue.lock() else {
            return Received {
                result: Err("the build queue is poisoned".to_string()),
                first: true,
                ready_before: None,
                waited: Duration::ZERO,
                build_seconds: 0.0,
            };
        };
        queue.asks += 1;
        if !queue.requests.contains(&key) {
            queue.requests.push(key.clone());
        }
        let mut ready_at_once = true;
        loop {
            if let Some(done) = queue.done.get_mut(&key) {
                let first = !done.taken;
                done.taken = true;
                let ready_before = (first && ready_at_once && !done.asked)
                    .then(|| asked_at.saturating_duration_since(done.finished));
                let received = Received {
                    result: done.result.clone(),
                    first,
                    ready_before,
                    waited: asked_at.elapsed(),
                    build_seconds: done.seconds,
                };
                if ready_at_once {
                    queue.ready_on_ask += 1;
                }
                queue.waited += received.waited;
                return received;
            }
            ready_at_once = false;
            if queue.building.as_ref() != Some(&key) && !queue.asked.contains(&key) {
                queue.asked.push_back(key.clone());
                self.changed.notify_all();
            }
            queue = match self.changed.wait(queue) {
                Ok(queue) => queue,
                Err(_) => {
                    return Received {
                        result: Err("the build queue is poisoned".to_string()),
                        first: true,
                        ready_before: None,
                        waited: asked_at.elapsed(),
                        build_seconds: 0.0,
                    }
                }
            };
        }
    }

    /// 項目が初めて求めた順。
    pub fn requests(&self) -> Vec<Key> {
        self.queue
            .lock()
            .map(|queue| queue.requests.clone())
            .unwrap_or_default()
    }

    /// まとめの数。
    pub fn tally(&self) -> Tally {
        let Ok(queue) = self.queue.lock() else {
            return Tally::default();
        };
        let mut tally = Tally {
            asks: queue.asks,
            ready_on_ask: queue.ready_on_ask,
            waited: queue.waited,
            ..Tally::default()
        };
        for done in queue.done.values() {
            tally.builds += 1;
            tally.build_seconds += done.seconds;
            if done.result.is_err() {
                tally.failed += 1;
            }
            if done.asked {
                tally.built_when_asked += 1;
            } else if done.taken {
                tally.built_ahead_and_used += 1;
            } else {
                tally.built_ahead_unused += 1;
                tally.unused_seconds += done.seconds;
            }
        }
        tally
    }
}

/// 次に作る組（**求められた組が先。先の分は、始めてよいときだけ**。純粋な論理）。
fn next_job(queue: &mut Queue) -> Option<(Key, bool)> {
    while let Some(key) = queue.asked.pop_front() {
        if !queue.done.contains_key(&key) {
            return Some((key, true));
        }
    }
    if queue.stopping || !queue.ahead_enabled {
        return None;
    }
    while let Some(key) = queue.ahead.pop_front() {
        if !queue.done.contains_key(&key) {
            return Some((key, false));
        }
    }
    None
}

/// まとめの数。
#[derive(Default, Debug, PartialEq)]
pub struct Tally {
    pub builds: usize,
    pub build_seconds: f64,
    pub failed: usize,
    pub built_when_asked: usize,
    pub built_ahead_and_used: usize,
    pub built_ahead_unused: usize,
    pub unused_seconds: f64,
    pub asks: usize,
    pub ready_on_ask: usize,
    pub waited: Duration,
}

/// 組の名前を揃える（並べ替え、重ねを除く。純粋な論理）。
pub fn key_of(features: &[&str]) -> Key {
    let mut key: Key = features.iter().map(|feature| feature.to_string()).collect();
    key.sort();
    key.dedup();
    key
}

/// 同じビルドに 1 つの名前を付ける（2026-09-29。運用者の決定。純粋な論理）。
///
/// **落とすもの**: 空の名前（`""` は既定の構成）、既定の構成から辿れる feature（`default` と `heap-poison`）、
/// 組の中のほかの feature から辿れる feature（`pipe-write-does-not-wake-reader` があれば `pipe-test`）。
/// **cargo が有効にする feature の全部は変わらない**ので、同じ成果物になる。
/// `reached` は、1 つの feature から辿れる kernel の feature の全部を返す（自分を含む。知らない名前は空）。
/// **知らない名前は残す**——cargo が「その feature は無い」と落とす。
pub fn canonical_key(features: &[&str], reached: &dyn Fn(&str) -> Vec<String>) -> Key {
    let from_default = reached("default");
    let named: Vec<&str> = features
        .iter()
        .map(|feature| feature.trim())
        .filter(|feature| !feature.is_empty() && !from_default.iter().any(|d| d == feature))
        .collect();
    let minimal: Vec<&str> = named
        .iter()
        .copied()
        .filter(|feature| {
            !named
                .iter()
                .any(|other| other != feature && reached(other).iter().any(|name| name == feature))
        })
        .collect();
    key_of(&minimal)
}

/// 並びの名前を揃え、揃えた後に重なった組は最初の 1 つだけを残す（順は保つ。純粋な論理）。
pub fn canonical_order(keys: &[Key], normalize: &Normalize) -> Vec<Key> {
    let mut ordered: Vec<Key> = Vec::new();
    for key in keys {
        let key = normalize(&key.iter().map(String::as_str).collect::<Vec<_>>());
        if !ordered.contains(&key) {
            ordered.push(key);
        }
    }
    ordered
}

/// 組の写しの置き場の名前（**空は `default`**。純粋な論理）。
pub fn directory_name(key: &Key) -> String {
    if key.is_empty() {
        "default".to_string()
    } else {
        key.join("+")
    }
}

/// 順の記録を読む（1 行 1 組。`-` は既定の構成。`#` で始まる行は読まない。純粋な論理）。
pub fn parse_order(text: &str) -> Vec<Key> {
    let mut keys = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = if line == "-" {
            Vec::new()
        } else {
            key_of(&line.split(',').collect::<Vec<_>>())
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// 順の記録を書く形にする。**この回に求めた順を先に、前の記録にしか無い組を後ろに残す**（途中で止まった回が
/// 残りの順を消さないように。純粋な論理）。
pub fn render_order(requests: &[Key], previous: &[Key]) -> String {
    let mut text = String::from(
        "# 全検査が kernel を求めた順（xtask の kernel_builds。次の全検査が、この順で先に作る）\n",
    );
    let mut written: Vec<&Key> = Vec::new();
    for key in requests.iter().chain(previous) {
        if written.contains(&key) {
            continue;
        }
        written.push(key);
        text.push_str(&if key.is_empty() {
            "-".to_string()
        } else {
            key.join(",")
        });
        text.push('\n');
    }
    text
}

/// fingerprint の `bin-kernel.json` から、有効だった feature を読む（純粋な論理）。
pub fn fingerprint_features(json: &str) -> Option<Vec<String>> {
    const HEAD: &str = "\"features\":\"[";
    let start = json.find(HEAD)? + HEAD.len();
    let rest = &json[start..];
    let end = rest.find("]\"")?;
    Some(
        rest[..end]
            .split(',')
            .map(|name| {
                name.trim()
                    .trim_matches(|c| c == '\\' || c == '"')
                    .to_string()
            })
            .filter(|name| !name.is_empty())
            .collect(),
    )
}

/// cargo の fingerprint から、作った時刻の順に、有効だった feature の組を並べる。**いちばん新しいものから
/// `window` の中だけ**を取る（前の全検査が作った分）。読めないものは飛ばす。
pub fn fingerprint_order(fingerprints: &Path, window: Duration) -> Vec<Vec<String>> {
    let Ok(entries) = fs::read_dir(fingerprints) else {
        return Vec::new();
    };
    let mut found: Vec<(SystemTime, Vec<String>)> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("kernel-") {
            continue;
        }
        let json = entry.path().join("bin-kernel.json");
        let (Ok(text), Ok(meta)) = (fs::read_to_string(&json), fs::metadata(&json)) else {
            continue;
        };
        let (Some(features), Ok(when)) = (fingerprint_features(&text), meta.modified()) else {
            continue;
        };
        found.push((when, features));
    }
    order_by_time(found, window)
}

/// 時刻の順に並べ、同じ組は新しい方だけを残し、いちばん新しいものから `window` の中だけを取る（純粋な論理）。
fn order_by_time(mut found: Vec<(SystemTime, Vec<String>)>, window: Duration) -> Vec<Vec<String>> {
    found.sort_by_key(|(when, _)| *when);
    let Some(newest) = found.last().map(|(when, _)| *when) else {
        return Vec::new();
    };
    let mut ordered: Vec<Vec<String>> = Vec::new();
    for (when, mut features) in found.into_iter().rev() {
        if newest.duration_since(when).unwrap_or_default() > window {
            continue;
        }
        features.sort();
        if !ordered.contains(&features) {
            ordered.push(features);
        }
    }
    ordered.reverse();
    ordered
}

/// cargo が組ごとに名前を変えて残す kernel の成果物の索引（`deps/kernel-<hash>`。同じ hash の fingerprint の
/// `bin-kernel.json` が、有効だった features を持つ。2026-09-29）。**写しを確かめるのに使う**（[`confirm_the_copy`]）。
///
/// **1 つの組に成果物が 2 つ以上在ることがある**——rustc を上げる前に作ったものが残る（2026-09-29 に、隣の作業ツリーで
/// 約 340 組に 672 個）。**どれか 1 つと同じ中身なら、その組の features で作ったものである。**
pub struct Artifacts {
    fingerprints: PathBuf,
    deps: PathBuf,
    index: Mutex<ArtifactIndex>,
}

#[derive(Default)]
struct ArtifactIndex {
    /// 読んだ fingerprint の置き場の名前（**同じ hash の features は変わらない**ので、読み直さない）。
    seen: HashSet<OsString>,
    /// features（並べ替えたもの）→ 成果物の道。
    by_features: HashMap<Vec<String>, Vec<PathBuf>>,
}

impl Artifacts {
    /// `debug` は `target/<標的>/debug`。
    pub fn new(debug: &Path) -> Self {
        Artifacts {
            fingerprints: debug.join(".fingerprint"),
            deps: debug.join("deps"),
            index: Mutex::new(ArtifactIndex::default()),
        }
    }

    /// その features で作った成果物の道（**まだ読んでいない fingerprint を読み足してから引く**）。
    pub fn with_features(&self, features: &[String]) -> Vec<PathBuf> {
        let mut wanted = features.to_vec();
        wanted.sort();
        let Ok(mut index) = self.index.lock() else {
            return Vec::new();
        };
        if let Ok(entries) = fs::read_dir(&self.fingerprints) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if index.seen.contains(&name) {
                    continue;
                }
                let text = name.to_string_lossy().to_string();
                let Some(hash) = text.strip_prefix("kernel-") else {
                    index.seen.insert(name);
                    continue;
                };
                // **ライブラリとビルドスクリプトの置き場には `bin-kernel.json` が無い**（読んだことにはしない。
                // 次に読み足すときも見る。1 つ当たり 1 回の読みの失敗で済む）。
                let Ok(json_text) = fs::read_to_string(entry.path().join("bin-kernel.json")) else {
                    continue;
                };
                let Some(mut enabled) = fingerprint_features(&json_text) else {
                    continue;
                };
                enabled.sort();
                let artifact = self.deps.join(format!("kernel-{hash}"));
                index.by_features.entry(enabled).or_default().push(artifact);
                index.seen.insert(name);
            }
        }
        index.by_features.get(&wanted).cloned().unwrap_or_default()
    }
}

/// 写しが、その組の features で作った成果物のどれかと同じ中身かを確かめ、同じだった成果物を返す（2026-09-29。
/// 運用者の決定）。`features` は、その組で cargo が有効にする features の全部（`default` とそこから辿れるものを含む）。
pub fn confirm_the_copy(
    artifacts: &Artifacts,
    features: &[String],
    copy: &Path,
) -> std::result::Result<PathBuf, String> {
    // **成果物の無い fingerprint は除く**（`clippy` の検査の単位も同じ名前で fingerprint を残す）。
    let candidates: Vec<PathBuf> = artifacts
        .with_features(features)
        .into_iter()
        .filter(|candidate| candidate.is_file())
        .collect();
    if candidates.is_empty() {
        return Err(format!(
            "no cargo artifact was built with the features of this set (looked for {} whose fingerprint lists {:?})",
            artifacts.deps.join("kernel-<hash>").display(),
            features
        ));
    }
    let copied =
        fs::read(copy).map_err(|error| format!("could not read {}: {error}", copy.display()))?;
    for candidate in &candidates {
        if fs::read(candidate).is_ok_and(|built| built == copied) {
            return Ok(candidate.clone());
        }
    }
    Err(format!(
        "{} is not a build of this set: it matches none of the {} cargo artifact(s) built with its features ({}); \
         another cargo may have rewritten the ELF that cargo leaves at one place for every set before it was copied",
        copy.display(),
        candidates.len(),
        candidates
            .iter()
            .map(|candidate| candidate.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// 動いている流れ。
struct Handle {
    service: Arc<Service>,
    worker: JoinHandle<()>,
    /// 順をどこから取ったか（まとめに出す）。
    source: String,
}

static RUNNING: Mutex<Option<Handle>> = Mutex::new(None);

/// ブートローダの写し（組ごと。**全検査の間だけ**）。
static BOOTLOADERS: Mutex<Vec<(Key, PathBuf)>> = Mutex::new(Vec::new());

/// 流れを始める（全検査の入口。**先に作り始めるのは [`build_ahead`] の後**）。**`normalize` は、先に作る順と、
/// 項目が求める組の両方の名前を揃える。**
pub fn start(ahead: Vec<Key>, source: String, build: Box<Builder>, normalize: Arc<Normalize>) {
    let Ok(mut running) = RUNNING.lock() else {
        return;
    };
    if running.is_some() {
        return;
    }
    let service = Arc::new(Service::new(ahead, normalize));
    let worker = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || service.run(build.as_ref()))
    };
    *running = Some(Handle {
        service,
        worker,
        source,
    });
}

/// 先に作り始める（QEMU の項目の手前）。
pub fn build_ahead() {
    if let Some(service) = service() {
        service.build_ahead();
    }
}

/// 流れが動いているか（**全検査の間だけ真**）。
pub fn is_running() -> bool {
    service().is_some()
}

fn service() -> Option<Arc<Service>> {
    RUNNING
        .lock()
        .ok()
        .and_then(|running| running.as_ref().map(|handle| Arc::clone(&handle.service)))
}

/// kernel の組を求める（**流れが動いていなければ `None`**——呼ぶ側がその場でビルドする）。**名前は流れが揃える。**
pub fn kernel(features: &[&str]) -> Option<Received> {
    service().map(|service| service.ask_features(features))
}

/// ブートローダの組を、全検査の間は 1 回だけ作る（`build` が作り、`copy` が写しの置き場を返す）。
/// **流れが動いていなければ、毎回 `build` を呼ぶ**（今までどおり）。
pub fn bootloader(
    features: &[&str],
    build: impl FnOnce() -> anyhow::Result<PathBuf>,
    copy: impl FnOnce(&Key, &Path) -> anyhow::Result<PathBuf>,
) -> anyhow::Result<PathBuf> {
    if !is_running() {
        return build();
    }
    let key = key_of(features);
    if let Some(path) = BOOTLOADERS.lock().ok().and_then(|made| {
        made.iter()
            .find(|(made_key, _)| *made_key == key)
            .map(|(_, path)| path.clone())
    }) {
        return Ok(path);
    }
    let built = build()?;
    let copied = copy(&key, &built)?;
    if let Ok(mut made) = BOOTLOADERS.lock() {
        made.push((key, copied.clone()));
    }
    Ok(copied)
}

/// 流れを止めたときに返すもの（全検査のまとめ）。
pub struct Finished {
    /// 項目が初めて求めた順（揃えた名前）。
    pub requests: Vec<Key>,
    pub tally: Tally,
    /// 順をどこから取ったか。
    pub source: String,
    /// 組の名前を揃える関数（**前の記録を、この回と同じ名前へ揃えてから書き直すため**）。
    pub normalize: Arc<Normalize>,
}

/// 流れを止め、項目が求めた順と、まとめの数を返す（全検査のまとめ。**動いていなければ `None`**）。
pub fn finish() -> Option<Finished> {
    let handle = RUNNING.lock().ok().and_then(|mut running| running.take())?;
    handle.service.stop();
    let _ = handle.worker.join();
    if let Ok(mut made) = BOOTLOADERS.lock() {
        made.clear();
    }
    Some(Finished {
        requests: handle.service.requests(),
        tally: handle.service.tally(),
        source: handle.source,
        normalize: Arc::clone(&handle.service.normalize),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn outcome(key: &Key) -> Outcome {
        Outcome {
            elf: PathBuf::from(format!("/elf/{}", directory_name(key))),
            out_dir: PathBuf::from(format!("/out/{}", directory_name(key))),
            cargo_output: format!("built {}", directory_name(key)),
        }
    }

    fn keys(names: &[&str]) -> Vec<Key> {
        names.iter().map(|name| key_of(&[name])).collect()
    }

    fn owned(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    /// 名前を並べ替えるだけの揃え方（feature の依存を持たない偽物）。
    fn plain() -> Arc<Normalize> {
        Arc::new(|features: &[&str]| key_of(features))
    }

    /// 小さな feature の表（`kernel/Cargo.toml` と同じ形の依存）で、1 つの feature から辿れるものを返す。
    fn reached_in_a_small_graph(feature: &str) -> Vec<String> {
        let graph: &[(&str, &[&str])] = &[
            ("default", &["heap-poison"]),
            ("heap-poison", &[]),
            ("pipe-test", &[]),
            ("pipe-write-does-not-wake-reader", &["pipe-test"]),
            ("serial-stress-test", &[]),
            (
                "serial-no-lock-test",
                &["serial-stress-test", "common/serial-no-lock"],
            ),
            ("a-test", &[]),
            ("b-test", &[]),
        ];
        let mut reached: Vec<String> = Vec::new();
        let mut pending = vec![feature.to_string()];
        while let Some(name) = pending.pop() {
            let Some((_, deps)) = graph.iter().find(|(declared, _)| *declared == name) else {
                continue;
            };
            if reached.contains(&name) {
                continue;
            }
            reached.push(name);
            pending.extend(deps.iter().map(|dep| dep.to_string()));
        }
        reached
    }

    /// **写しが、その組の features で作った成果物のどれかと同じ中身なら通し、違えば落とす。**
    /// **後から作った組も、fingerprint を読み足して見つける。**
    #[test]
    fn a_copy_is_accepted_only_when_it_is_a_build_of_its_set() {
        let debug =
            std::env::temp_dir().join(format!("zaytos-artifacts-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&debug);
        let write = |path: PathBuf, text: &str| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        let fingerprint = |hash: &str, features: &[&str]| {
            let listed = features
                .iter()
                .map(|feature| format!("\\\"{feature}\\\""))
                .collect::<Vec<_>>()
                .join(", ");
            write(
                debug
                    .join(".fingerprint")
                    .join(format!("kernel-{hash}"))
                    .join("bin-kernel.json"),
                &format!(
                    "{{\"rustc\":1,\"features\":\"[{listed}]\",\"declared_features\":\"[]\"}}"
                ),
            );
        };
        let with_x = owned(&["default", "heap-poison", "x-test"]);
        fingerprint("aaa", &["default", "heap-poison", "x-test"]);
        write(debug.join("deps").join("kernel-aaa"), "x build");
        // **rustc を上げる前の同じ組**（中身が違う）。
        fingerprint("ccc", &["x-test", "heap-poison", "default"]);
        write(
            debug.join("deps").join("kernel-ccc"),
            "x build with the old rustc",
        );
        fingerprint("bbb", &["default", "heap-poison"]);
        write(debug.join("deps").join("kernel-bbb"), "default build");
        // **ライブラリの置き場と、成果物の無い fingerprint（`clippy` の単位）は数えない。**
        write(
            debug
                .join(".fingerprint")
                .join("kernel-ddd")
                .join("lib-kernel.json"),
            "{}",
        );
        fingerprint("fff", &["default", "heap-poison", "x-test"]);
        let artifacts = Artifacts::new(&debug);
        let copy = debug.join("copy");
        write(copy.clone(), "x build");
        assert_eq!(
            confirm_the_copy(&artifacts, &with_x, &copy),
            Ok(debug.join("deps").join("kernel-aaa"))
        );
        // **既定の構成の ELF を x-test の組として写したら、落とす。**
        write(copy.clone(), "default build");
        let error = confirm_the_copy(&artifacts, &with_x, &copy).unwrap_err();
        assert!(
            error.contains("matches none of the 2 cargo artifact(s)"),
            "{error}"
        );
        // **この回に初めて作った組は、後からできた fingerprint を読み足して見つける。**
        fingerprint("eee", &["default", "heap-poison", "y-test"]);
        write(debug.join("deps").join("kernel-eee"), "y build");
        write(copy.clone(), "y build");
        assert_eq!(
            confirm_the_copy(
                &artifacts,
                &owned(&["default", "heap-poison", "y-test"]),
                &copy
            ),
            Ok(debug.join("deps").join("kernel-eee"))
        );
        // **その features で作った成果物が 1 つも無ければ、落とす。**
        let error = confirm_the_copy(
            &artifacts,
            &owned(&["default", "heap-poison", "z-test"]),
            &copy,
        )
        .unwrap_err();
        assert!(error.contains("no cargo artifact"), "{error}");
        let _ = fs::remove_dir_all(&debug);
    }

    #[test]
    fn one_build_gets_one_name() {
        let canonical = |features: &[&str]| canonical_key(features, &reached_in_a_small_graph);
        // **親を並べても、子だけの名前になる**（子から辿れる）。並びと重ねにもよらない。
        let child = key_of(&["pipe-write-does-not-wake-reader"]);
        assert_eq!(
            canonical(&["pipe-test", "pipe-write-does-not-wake-reader"]),
            child
        );
        assert_eq!(
            canonical(&["pipe-write-does-not-wake-reader", "pipe-test", "pipe-test"]),
            child
        );
        // **fingerprint が持つ全部（既定の構成と、そこから辿れるものを含む）も、同じ名前になる。**
        assert_eq!(
            canonical(&[
                "default",
                "heap-poison",
                "pipe-test",
                "pipe-write-does-not-wake-reader"
            ]),
            child
        );
        // **空の名前と、既定の構成から辿れるものは落ちる**（`feature: ""` は既定の構成）。
        assert_eq!(canonical(&[""]), Key::new());
        assert_eq!(canonical(&[" "]), Key::new());
        assert_eq!(canonical(&["default", "heap-poison"]), Key::new());
        assert_eq!(canonical(&[]), Key::new());
        // **ほかの crate へ伸びる依存があっても、kernel の feature で揃う。**
        assert_eq!(
            canonical(&["serial-stress-test", "serial-no-lock-test"]),
            key_of(&["serial-no-lock-test"])
        );
        // **依存の無い 2 つは両方残る。知らない名前も残す**（cargo が断る）。
        assert_eq!(
            canonical(&["b-test", "a-test"]),
            key_of(&["a-test", "b-test"])
        );
        assert_eq!(
            canonical(&["no-such-feature", ""]),
            key_of(&["no-such-feature"])
        );
    }

    #[test]
    fn the_order_and_the_asks_meet_on_the_same_name() {
        let normalize: Arc<Normalize> =
            Arc::new(|features: &[&str]| canonical_key(features, &reached_in_a_small_graph));
        // **先に作る順には、fingerprint の形（有効だった全部）と、前の記録の形（親も並べた名前・空の名前）が混ざりうる。**
        let ahead = vec![
            owned(&[
                "default",
                "heap-poison",
                "pipe-test",
                "pipe-write-does-not-wake-reader",
            ]),
            owned(&["pipe-test", "pipe-write-does-not-wake-reader"]),
            owned(&[""]),
            Key::new(),
        ];
        assert_eq!(
            canonical_order(&ahead, normalize.as_ref()),
            vec![key_of(&["pipe-write-does-not-wake-reader"]), Key::new()]
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let service = Arc::new(Service::new(ahead, Arc::clone(&normalize)));
        let worker = {
            let service = Arc::clone(&service);
            let calls = Arc::clone(&calls);
            std::thread::spawn(move || {
                service.run(&move |key: &Key| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(outcome(key))
                })
            })
        };
        service.build_ahead();
        let deadline = Instant::now() + Duration::from_secs(10);
        while calls.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(20));
        // **項目は親も並べた名前で求めるが、先に作った組をそのまま受け取る。**
        let pipe = service.ask_features(&["pipe-test", "pipe-write-does-not-wake-reader"]);
        assert!(pipe.ready_before.is_some());
        // **`""` と既定の構成は同じ組である**（2 度目は作らずに渡す）。
        let empty = service.ask_features(&[""]);
        assert!(empty.ready_before.is_some());
        let default = service.ask_features(&[]);
        assert!(!default.first);
        service.stop();
        worker.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let tally = service.tally();
        assert_eq!(tally.built_ahead_and_used, 2);
        assert_eq!(tally.built_ahead_unused, 0);
        assert_eq!(tally.built_when_asked, 0);
        assert_eq!(
            service.requests(),
            vec![key_of(&["pipe-write-does-not-wake-reader"]), Key::new()]
        );
    }

    #[test]
    fn keys_are_sorted_and_named() {
        assert_eq!(
            key_of(&["b-test", "a-test", "b-test"]),
            vec!["a-test", "b-test"]
        );
        assert_eq!(directory_name(&key_of(&[])), "default");
        assert_eq!(directory_name(&key_of(&["b", "a"])), "a+b");
        let order = parse_order("# comment\n-\nb,a\n\na,b\nc\n");
        assert_eq!(
            order,
            vec![Vec::<String>::new(), key_of(&["a", "b"]), key_of(&["c"])]
        );
        let rendered = render_order(&[key_of(&["c"]), Vec::new()], &order);
        assert_eq!(
            parse_order(&rendered),
            vec![key_of(&["c"]), Vec::new(), key_of(&["a", "b"])]
        );
    }

    #[test]
    fn fingerprints_give_the_features_in_the_order_they_were_built() {
        let json = r#"{"rustc":1,"features":"[\"default\", \"heap-poison\", \"x-test\"]","declared_features":"[\"a\"]"}"#;
        assert_eq!(
            fingerprint_features(json),
            Some(vec![
                "default".to_string(),
                "heap-poison".to_string(),
                "x-test".to_string()
            ])
        );
        let at = |seconds: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + seconds);
        let names = |list: &[&str]| list.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        let found = vec![
            (at(30), names(&["b"])),
            (at(10), names(&["a"])),
            (at(0), names(&["old"])),
            (at(20), names(&["a"])),
            (at(40), names(&["c"])),
        ];
        // **同じ組は新しい方の時刻で並び、窓の外（いちばん新しいものから 35 秒より前）は落ちる。**
        assert_eq!(
            order_by_time(found, Duration::from_secs(35)),
            vec![names(&["a"]), names(&["b"]), names(&["c"])]
        );
    }

    #[test]
    fn an_asked_build_goes_before_the_builds_ahead() {
        let service = Arc::new(Service::new(keys(&["a", "b", "c"]), plain()));
        let built = Arc::new(Mutex::new(Vec::new()));
        let worker = {
            let service = Arc::clone(&service);
            let built = Arc::clone(&built);
            std::thread::spawn(move || {
                service.run(&move |key: &Key| {
                    built.lock().unwrap().push(directory_name(key));
                    Ok(outcome(key))
                })
            })
        };
        // **先に作り始める前に求めた組は、すぐ作る。**
        let received = service.ask(key_of(&["z"]));
        assert_eq!(received.result, Ok(outcome(&key_of(&["z"]))));
        assert!(received.first);
        assert_eq!(received.ready_before, None);
        service.build_ahead();
        let received = service.ask(key_of(&["c"]));
        assert_eq!(received.result, Ok(outcome(&key_of(&["c"]))));
        // **2 度目は作らずに渡す。初めてではない。**
        let again = service.ask(key_of(&["c"]));
        assert!(!again.first);
        service.stop();
        worker.join().unwrap();
        let built = built.lock().unwrap().clone();
        assert_eq!(built.first().map(String::as_str), Some("z"));
        assert_eq!(built.iter().filter(|name| *name == "c").count(), 1);
        assert_eq!(service.requests(), vec![key_of(&["z"]), key_of(&["c"])]);
        let tally = service.tally();
        assert_eq!(tally.asks, 3);
        assert_eq!(tally.failed, 0);
        assert!(tally.built_when_asked >= 1);
    }

    #[test]
    fn a_build_made_ahead_is_handed_over_without_building_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = Arc::new(Service::new(keys(&["a", "b"]), plain()));
        let worker = {
            let service = Arc::clone(&service);
            let calls = Arc::clone(&calls);
            std::thread::spawn(move || {
                service.run(&move |key: &Key| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if directory_name(key) == "b" {
                        Err("error: no such feature".to_string())
                    } else {
                        Ok(outcome(key))
                    }
                })
            })
        };
        service.build_ahead();
        // **先の 2 つを作り終えるまで待ってから求める。**
        let deadline = Instant::now() + Duration::from_secs(10);
        while calls.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(20));
        let a = service.ask(key_of(&["a"]));
        assert!(a.ready_before.is_some());
        assert_eq!(a.result, Ok(outcome(&key_of(&["a"]))));
        // **失敗も、その組を求めた項目に渡す**（作り直さない。同じ中身から同じ失敗になる）。
        let b = service.ask(key_of(&["b"]));
        assert_eq!(b.result, Err("error: no such feature".to_string()));
        service.stop();
        worker.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let tally = service.tally();
        assert_eq!(tally.built_ahead_and_used, 2);
        assert_eq!(tally.failed, 1);
        assert_eq!(tally.ready_on_ask, 2);
    }
}
