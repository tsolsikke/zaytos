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
//! **cargo は、組によらず同じ置き場（`target/x86_64-unknown-none/debug/kernel`）へ ELF を書く。** **全検査の間は、
//! kernel のビルドをこの流れだけが行う**（項目は結果を受け取るだけ）。**作った直後、次のビルドを始める前に、ELF を
//! 組ごとの置き場へ写し、同じ cargo の出力から取った `OUT_DIR` と対にして返す**——`KernelBuild` の doc の
//! 「ビルドした側と載せる側を対にして持つ」を保つ。
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
//! # 全検査でだけ使う
//!
//! **基本の検査・`--commit`・手の実行は、今までどおりその場でビルドする**（[`is_running`] が偽）。

use std::collections::{HashMap, VecDeque};
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
}

impl Service {
    pub fn new(ahead: Vec<Key>) -> Self {
        Service {
            queue: Mutex::new(Queue {
                ahead: ahead.into(),
                ..Queue::default()
            }),
            changed: Condvar::new(),
        }
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

/// 流れを始める（全検査の入口。**先に作り始めるのは [`build_ahead`] の後**）。
pub fn start(ahead: Vec<Key>, source: String, build: Box<Builder>) {
    let Ok(mut running) = RUNNING.lock() else {
        return;
    };
    if running.is_some() {
        return;
    }
    let service = Arc::new(Service::new(ahead));
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

/// kernel の組を求める（**流れが動いていなければ `None`**——呼ぶ側がその場でビルドする）。
pub fn kernel(features: &[&str]) -> Option<Received> {
    service().map(|service| service.ask(key_of(features)))
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

/// 流れを止め、項目が求めた順と、まとめの数を返す（全検査のまとめ。**動いていなければ `None`**）。
pub fn finish() -> Option<(Vec<Key>, Tally, String)> {
    let handle = RUNNING.lock().ok().and_then(|mut running| running.take())?;
    handle.service.stop();
    let _ = handle.worker.join();
    if let Ok(mut made) = BOOTLOADERS.lock() {
        made.clear();
    }
    Some((
        handle.service.requests(),
        handle.service.tally(),
        handle.source,
    ))
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
        let service = Arc::new(Service::new(keys(&["a", "b", "c"])));
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
        let service = Arc::new(Service::new(keys(&["a", "b"])));
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
