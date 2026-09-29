//! 全検査の間のホストの様子を残す（2026-09-29。運用者の決定。試験の時間を縮める案の 0）。
//!
//! # なぜ在るのか
//!
//! **2026-09-28 の夜の全検査は、朝の回よりビルドが 1.24 倍、QEMU が 1.07 倍遅く、遅れは 2 時間を通してほぼ
//! 一様だった。** 記録とログからは原因が分からなかった（ホストの CPU の周波数、Windows の背景の処理、仮想ディスクの
//! 領域の出し入れ）。**次に遅くなったときに原因を確かめられるように、全検査の間の様子を残す。**
//!
//! # 何を残すか
//!
//! - **30 秒ごとの 1 行**（`…-samples.tsv`）: 時刻、そのとき走っていた項目、`/proc/pressure` の cpu・io・memory の
//!   累計（待たされた時間。マイクロ秒）、`/proc/loadavg` の 1 分の値、`/proc/meminfo` の `MemAvailable`・`Cached`・
//!   `SwapFree`、Windows の C: と D: の空き。
//! - **項目の始まり**（同じファイルの `item` の行）。**所要だけでは、Windows の側の出来事と突き合わせられない。**
//! - **Windows の側の計数**（`…-windows.csv`）: `typeperf` を 1 本走らせ、30 秒ごとに 1 行を受け取る。CPU の周波数の
//!   割合（`% Processor Performance`）と使用率、C: と D: の 1 回の転送の待ちと待ち行列。**読むだけのコマンドとして
//!   運用者が許した**（2026-09-29）。**Windows の設定は変えない。**
//!
//! **`typeperf` は、言語を固定する表（`PARSED_EXTERNAL_TOOLS`）に載せない**——Windows のプログラムで、WSL の
//! 言語の設定は届かない。**英語の計数の名前を指定し、数の CSV の行だけを読む**（見出しの行と、日本語で出る
//! 終わりの知らせは読まない）。
//!
//! # 止めない
//!
//! **読めないものは `-` を書いて続ける。** **計測が検査の結果を変えることは無い。** `println!` は使わない（項目の
//! 出力のコピーに混ざる）——書くのは自分のファイルだけで、まとめの行は [`stop`] が返す。

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::launch;

/// 1 行を残す間隔。
const INTERVAL: Duration = Duration::from_secs(30);

/// Windows の側の計数（`typeperf` の英語の名前）。**表示の言語が日本語の Windows でも、この名前で読めた**
/// （2026-09-29 の実測）。
const WINDOWS_COUNTERS: [&str; 6] = [
    r"\Processor Information(_Total)\% Processor Performance",
    r"\Processor Information(_Total)\% Processor Utility",
    r"\LogicalDisk(C:)\Avg. Disk sec/Transfer",
    r"\LogicalDisk(C:)\Current Disk Queue Length",
    r"\LogicalDisk(D:)\Avg. Disk sec/Transfer",
    r"\LogicalDisk(D:)\Current Disk Queue Length",
];

/// `typeperf` が自分で終わるまでの回数。**止め損ねても残らないように**、全検査の上限（195 分）より長い 4 時間で切る。
const WINDOWS_SAMPLE_LIMIT: u32 = 480;

/// Windows の側の計数を読む子と、読む糸と、書く置き場。
type WindowsCounters = (Child, JoinHandle<Vec<Vec<f64>>>, PathBuf);

/// 30 秒ごとの 1 回分。
#[derive(Clone, Default, Debug, PartialEq)]
struct Sample {
    load1: Option<f64>,
    cpu_some_us: Option<u64>,
    io_some_us: Option<u64>,
    io_full_us: Option<u64>,
    memory_some_us: Option<u64>,
    memory_full_us: Option<u64>,
    mem_available_kb: Option<u64>,
    cached_kb: Option<u64>,
    swap_free_kb: Option<u64>,
    system_free: Option<u64>,
    vhd_drive_free: Option<u64>,
}

/// 走っている間の持ち物。
struct Running {
    samples_path: PathBuf,
    file: Arc<Mutex<File>>,
    sampler: JoinHandle<Vec<Sample>>,
    windows: Option<WindowsCounters>,
    windows_note: Option<String>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
static STOP: AtomicBool = AtomicBool::new(false);
static CURRENT_ITEM: Mutex<String> = Mutex::new(String::new());

/// 記録を始める（全検査の入口。**2 度目は何もしない**）。`log` は全検査のログの置き場で、隣に 2 つのファイルを置く。
pub fn start(root: &Path, log: Option<&Path>) {
    let Ok(mut running) = RUNNING.lock() else {
        return;
    };
    if running.is_some() {
        return;
    }
    let stem = match log {
        Some(log) => log.with_extension(""),
        None => root.join("target").join("full-check").join("host"),
    };
    let samples_path = PathBuf::from(format!("{}-samples.tsv", stem.display()));
    let windows_path = PathBuf::from(format!("{}-windows.csv", stem.display()));
    if let Some(parent) = samples_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(mut file) = File::create(&samples_path) else {
        return;
    };
    let _ = writeln!(
        file,
        "# unix_ms\tkind\titem\tload1\tcpu_some_us\tio_some_us\tio_full_us\tmemory_some_us\tmemory_full_us\t\
         mem_available_kb\tcached_kb\tswap_free_kb\t{}_free\t{}_free",
        launch::HOST_SYSTEM_DRIVE,
        launch::HOST_VHD_DRIVE
    );
    let file = Arc::new(Mutex::new(file));
    STOP.store(false, Ordering::SeqCst);
    let sampler = {
        let file = Arc::clone(&file);
        std::thread::spawn(move || sample_until_stopped(&file))
    };
    let (windows, windows_note) = if launch::in_wsl() {
        match start_windows_counters(&windows_path) {
            Ok(started) => (Some(started), None),
            Err(why) => (None, Some(why)),
        }
    } else {
        (None, Some("not in WSL".to_string()))
    };
    *running = Some(Running {
        samples_path,
        file,
        sampler,
        windows,
        windows_note,
    });
}

/// 項目の始まりを残す（`begin_item` が呼ぶ。**記録していないときは何もしない**）。
pub fn note_item(label: &str) {
    if let Ok(mut current) = CURRENT_ITEM.lock() {
        *current = one_field(label);
    }
    let file = RUNNING
        .lock()
        .ok()
        .and_then(|running| running.as_ref().map(|running| Arc::clone(&running.file)));
    if let Some(file) = file {
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{}\titem\t{}", unix_ms(), one_field(label));
        }
    }
}

/// 記録を止め、まとめの行を返す（全検査のまとめ。**記録していないときは空**）。
pub fn stop() -> Vec<String> {
    let Some(running) = RUNNING.lock().ok().and_then(|mut running| running.take()) else {
        return Vec::new();
    };
    STOP.store(true, Ordering::SeqCst);
    let samples = running.sampler.join().unwrap_or_default();
    let mut lines = vec![samples_line(&samples, &running.samples_path)];
    match running.windows {
        Some((mut child, reader, path)) => {
            let _ = child.kill();
            let _ = child.wait();
            let rows = reader.join().unwrap_or_default();
            lines.push(windows_line(&rows, &path));
        }
        None => lines.push(format!(
            "(info) Windows counters: not taken ({})",
            running.windows_note.unwrap_or_default()
        )),
    }
    lines
}

/// 30 秒ごとに 1 行を書く。**止めの合図は 1 秒ごとに見る**（止めてから最後の 1 行を書く）。
fn sample_until_stopped(file: &Mutex<File>) -> Vec<Sample> {
    let mut samples = Vec::new();
    loop {
        let sample = take_sample();
        let item = CURRENT_ITEM
            .lock()
            .map(|item| item.clone())
            .unwrap_or_default();
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(
                file,
                "{}\tsample\t{}\t{}",
                unix_ms(),
                item,
                sample_fields(&sample)
            );
        }
        samples.push(sample);
        let started = Instant::now();
        while started.elapsed() < INTERVAL {
            if STOP.load(Ordering::SeqCst) {
                let sample = take_sample();
                if let Ok(mut file) = file.lock() {
                    let _ = writeln!(
                        file,
                        "{}\tsample\t{}\t{}",
                        unix_ms(),
                        item,
                        sample_fields(&sample)
                    );
                }
                samples.push(sample);
                return samples;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn take_sample() -> Sample {
    let pressure = |what: &str| {
        fs::read_to_string(format!("/proc/pressure/{what}"))
            .map(|text| pressure_totals(&text))
            .unwrap_or((None, None))
    };
    let (cpu_some_us, _) = pressure("cpu");
    let (io_some_us, io_full_us) = pressure("io");
    let (memory_some_us, memory_full_us) = pressure("memory");
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let in_wsl = launch::in_wsl();
    let drive = |path: &str| {
        in_wsl
            .then(|| launch::available_bytes(Path::new(path)))
            .flatten()
    };
    Sample {
        load1: fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|text| text.split_whitespace().next()?.parse().ok()),
        cpu_some_us,
        io_some_us,
        io_full_us,
        memory_some_us,
        memory_full_us,
        mem_available_kb: meminfo_kb(&meminfo, "MemAvailable"),
        cached_kb: meminfo_kb(&meminfo, "Cached"),
        swap_free_kb: meminfo_kb(&meminfo, "SwapFree"),
        system_free: drive(launch::HOST_SYSTEM_DRIVE),
        vhd_drive_free: drive(launch::HOST_VHD_DRIVE),
    }
}

/// `/proc/pressure/*` の `some` と `full` の `total=`（マイクロ秒。純粋な論理）。
fn pressure_totals(text: &str) -> (Option<u64>, Option<u64>) {
    let total = |kind: &str| {
        text.lines()
            .find(|line| line.starts_with(kind))?
            .split_whitespace()
            .find_map(|field| field.strip_prefix("total="))?
            .parse()
            .ok()
    };
    (total("some "), total("full "))
}

/// `/proc/meminfo` の 1 項目（kB。純粋な論理）。
fn meminfo_kb(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.split(':').next() == Some(name))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn sample_fields(sample: &Sample) -> String {
    let number = |value: Option<u64>| value.map_or("-".to_string(), |value| value.to_string());
    [
        sample
            .load1
            .map_or("-".to_string(), |value| format!("{value:.2}")),
        number(sample.cpu_some_us),
        number(sample.io_some_us),
        number(sample.io_full_us),
        number(sample.memory_some_us),
        number(sample.memory_full_us),
        number(sample.mem_available_kb),
        number(sample.cached_kb),
        number(sample.swap_free_kb),
        number(sample.system_free),
        number(sample.vhd_drive_free),
    ]
    .join("\t")
}

/// Windows の側の計数を始める。`typeperf` の出力のうち CSV の行だけを書き、数を持っておく。
fn start_windows_counters(path: &Path) -> std::result::Result<WindowsCounters, String> {
    let mut command = Command::new("typeperf.exe");
    command
        .args(WINDOWS_COUNTERS)
        .args(["-si", &INTERVAL.as_secs().to_string()])
        .args(["-sc", &WINDOWS_SAMPLE_LIMIT.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not start typeperf.exe: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "typeperf.exe has no stdout".to_string())?;
    let mut file = File::create(path)
        .map_err(|error| format!("could not create {}: {error}", path.display()))?;
    let reader = std::thread::spawn(move || {
        let mut rows = Vec::new();
        for line in BufReader::new(stdout)
            .split(b'\n')
            .map_while(|line| line.ok())
        {
            let line = String::from_utf8_lossy(&line).trim_end().to_string();
            if !line.starts_with('"') {
                continue;
            }
            let _ = writeln!(file, "{line}");
            if let Some(values) = csv_values(&line) {
                rows.push(values);
            }
        }
        rows
    });
    Ok((child, reader, path.to_path_buf()))
}

/// `typeperf` の CSV の 1 行から、時刻の後ろの数を読む（見出しの行と、読めない行は `None`。純粋な論理）。
fn csv_values(line: &str) -> Option<Vec<f64>> {
    let fields: Vec<&str> = line
        .split("\",\"")
        .map(|field| field.trim_matches('"'))
        .collect();
    if fields.len() != WINDOWS_COUNTERS.len() + 1 || fields[0].starts_with("(PDH-CSV") {
        return None;
    }
    Some(
        fields[1..]
            .iter()
            .map(|field| field.trim().parse().unwrap_or(f64::NAN))
            .collect(),
    )
}

/// まとめの 1 行目（ホストの側）。
fn samples_line(samples: &[Sample], path: &Path) -> String {
    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return format!("(info) host samples: none in {}", path.display());
    };
    let seconds = |pick: fn(&Sample) -> Option<u64>| match (pick(first), pick(last)) {
        (Some(before), Some(after)) => format!("{:.0}s", after.saturating_sub(before) as f64 / 1e6),
        _ => "-".to_string(),
    };
    let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    let drive = |pick: fn(&Sample) -> Option<u64>| {
        let values: Vec<u64> = samples.iter().filter_map(pick).collect();
        match (values.first(), values.last(), values.iter().min()) {
            (Some(start), Some(end), Some(low)) => {
                format!(
                    "{:.1} -> {:.1} GiB (lowest {:.1})",
                    gib(*start),
                    gib(*end),
                    gib(*low)
                )
            }
            _ => "-".to_string(),
        }
    };
    let mut loads: Vec<f64> = samples.iter().filter_map(|sample| sample.load1).collect();
    loads.sort_by(f64::total_cmp);
    let load = match (loads.get(loads.len() / 2), loads.last()) {
        (Some(median), Some(max)) => format!("median {median:.2}, highest {max:.2}"),
        _ => "-".to_string(),
    };
    let lowest_kb = |pick: fn(&Sample) -> Option<u64>| {
        samples
            .iter()
            .filter_map(pick)
            .min()
            .map_or("-".to_string(), |kb| {
                format!("{:.1} GiB", kb as f64 / (1u64 << 20) as f64)
            })
    };
    format!(
        "(info) host samples: {} every {}s in {}; waited over the run (/proc/pressure): cpu {}, io {} (all tasks {}), \
         memory {} (all tasks {}); load average (1 min) {load}; MemAvailable lowest {}, SwapFree lowest {}; \
         free on {} {}, on {} {}",
        samples.len(),
        INTERVAL.as_secs(),
        path.display(),
        seconds(|sample| sample.cpu_some_us),
        seconds(|sample| sample.io_some_us),
        seconds(|sample| sample.io_full_us),
        seconds(|sample| sample.memory_some_us),
        seconds(|sample| sample.memory_full_us),
        lowest_kb(|sample| sample.mem_available_kb),
        lowest_kb(|sample| sample.swap_free_kb),
        launch::HOST_VHD_DRIVE,
        drive(|sample| sample.vhd_drive_free),
        launch::HOST_SYSTEM_DRIVE,
        drive(|sample| sample.system_free),
    )
}

/// まとめの 2 行目（Windows の側）。各計数の中央値と最小・最大。
///
/// **`typeperf` が読めなかった値は数えない**——空の欄と `-1` である（どの計数も負にはならない）。**2026-09-29 の
/// 全検査で、周波数の割合の 3 行が `-1` になり、まとめが「最小 -1.000」と出した。** 読めなかった数は、あれば欄の
/// 後ろに出す。**ディスクの時間はミリ秒で出す**（秒の小数 3 桁では、全部 0.000 と出て読めなかった）。
fn windows_line(rows: &[Vec<f64>], path: &Path) -> String {
    if rows.is_empty() {
        return format!("(info) Windows counters: no rows in {}", path.display());
    }
    // 名前、出すときに掛ける数、小数の桁。
    let columns: [(&str, f64, usize); 6] = [
        ("% Processor Performance", 1.0, 1),
        ("% Processor Utility", 1.0, 1),
        ("C: ms/transfer", 1000.0, 3),
        ("C: queue", 1.0, 1),
        ("D: ms/transfer", 1000.0, 3),
        ("D: queue", 1.0, 1),
    ];
    let columns: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(index, &(name, scale, digits))| {
            let read: Vec<f64> = rows.iter().filter_map(|row| row.get(index).copied()).collect();
            let mut values: Vec<f64> = read
                .iter()
                .copied()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value * scale)
                .collect();
            let unread = read.len() - values.len();
            values.sort_by(f64::total_cmp);
            let unread = if unread > 0 {
                format!("; {unread} not read by typeperf")
            } else {
                String::new()
            };
            match (values.first(), values.get(values.len() / 2), values.last()) {
                (Some(low), Some(median), Some(high)) => format!(
                    "{name} median {median:.digits$} (lowest {low:.digits$}, highest {high:.digits$}{unread})"
                ),
                _ => format!("{name} -{unread}"),
            }
        })
        .collect();
    format!(
        "(info) Windows counters: {} row(s) every {}s in {}; {}",
        rows.len(),
        INTERVAL.as_secs(),
        path.display(),
        columns.join("; ")
    )
}

/// 1 つの欄に入る形（タブと改行を空白へ）。
fn one_field(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pressure_totals_and_meminfo_fields_are_read() {
        let pressure = "some avg10=0.00 avg60=0.00 avg300=0.00 total=482931241\n\
                        full avg10=0.00 avg60=0.00 avg300=0.00 total=17\n";
        assert_eq!(pressure_totals(pressure), (Some(482931241), Some(17)));
        assert_eq!(
            pressure_totals("some avg10=0.00 total=5\n"),
            (Some(5), None)
        );
        let meminfo = "MemTotal:       16318500 kB\nMemAvailable:   13930000 kB\nCached:         12000000 kB\n\
                       SwapCached:          4 kB\n";
        assert_eq!(meminfo_kb(meminfo, "MemAvailable"), Some(13930000));
        assert_eq!(meminfo_kb(meminfo, "Cached"), Some(12000000));
        assert_eq!(meminfo_kb(meminfo, "SwapFree"), None);
    }

    #[test]
    fn typeperf_rows_are_read_and_the_header_is_skipped() {
        let header = "\"(PDH-CSV 4.0)\",\"\\\\HOST\\Processor Information(_Total)\\% Processor Performance\",\"a\",\"b\",\"c\",\"d\",\"e\"";
        assert_eq!(csv_values(header), None);
        let row = "\"09/29/2026 06:48:02.042\",\"131.596446\",\"19.472778\",\"0.000081\",\"0.000000\",\" \",\"0.000000\"";
        let values = csv_values(row).unwrap();
        assert_eq!(values.len(), 6);
        assert!((values[0] - 131.596446).abs() < 1e-9);
        assert!(values[4].is_nan());
        assert_eq!(csv_values("\"too\",\"short\""), None);
    }

    /// **読めなかった値（`-1` と空）は数えず、数を出す。ディスクの時間はミリ秒で出す。**
    #[test]
    fn the_windows_line_skips_values_typeperf_could_not_read() {
        let rows = vec![
            vec![129.0, 20.0, 0.000066, 0.0, 0.000040, 0.0],
            vec![-1.0, 22.0, 0.000319, 0.0, 0.001621, 0.0],
            vec![f64::NAN, 18.0, 0.000029, 0.0, 0.000020, 1.0],
            vec![131.5, 34.7, 0.000100, 0.0, 0.000050, 0.0],
        ];
        let line = windows_line(&rows, Path::new("w.csv"));
        assert!(
            line.contains(
                "% Processor Performance median 131.5 (lowest 129.0, highest 131.5; 2 not read by typeperf)"
            ),
            "{line}"
        );
        assert!(
            line.contains("% Processor Utility median 22.0 (lowest 18.0, highest 34.7)"),
            "{line}"
        );
        assert!(
            line.contains("C: ms/transfer median 0.100 (lowest 0.029, highest 0.319)"),
            "{line}"
        );
        assert!(
            line.contains("D: ms/transfer median 0.050 (lowest 0.020, highest 1.621)"),
            "{line}"
        );
        assert!(
            line.contains("D: queue median 0.0 (lowest 0.0, highest 1.0)"),
            "{line}"
        );
        assert!(!line.contains("-1"), "{line}");
        let unread = vec![vec![-1.0, f64::NAN, 0.0, 0.0, 0.0, 0.0]];
        assert!(windows_line(&unread, Path::new("w.csv"))
            .contains("% Processor Performance -; 1 not read by typeperf"));
    }
}
