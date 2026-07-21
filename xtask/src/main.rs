use std::{
    env,
    ffi::OsString,
    fs,
    io::Write,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};

mod font;

const OVMF_CODE_PATH: &str = "/usr/share/OVMF/OVMF_CODE_4M.fd";
const OVMF_VARS_TEMPLATE_PATH: &str = "/usr/share/OVMF/OVMF_VARS_4M.fd";
const BOOTLOADER_PACKAGE: &str = "bootloader";
const UEFI_TARGET: &str = "x86_64-unknown-uefi";
const KERNEL_PACKAGE: &str = "kernel";
const KERNEL_TARGET: &str = "x86_64-unknown-none";
const PANIC_TEST_FEATURE: &str = "panic-test";
/// kernel 側の feature。有効にすると M3-a の描画テストパターンを描き、
/// コンソールを起動しない（ADR-0017）。
const GFX_TEST_PATTERN_FEATURE: &str = "gfx-test-pattern";

/// 例外ハンドラの回帰チェック（`--exception-test <kind>`）。
///
/// 起動完了後に意図した例外をわざと発生させ、ハンドラが呼ばれることを
/// 確認する。**必ず TCG で実行する。** KVM では `-d int` に何も残らず、
/// `v=..` の突き合わせができないため（troubleshooting.md 参照）。
struct ExceptionTest {
    /// `--exception-test` に渡す名前。
    name: &'static str,
    /// kernel 側で有効化する feature。
    feature: &'static str,
    /// 期待するベクタ番号。
    vector: u8,
    /// `qemu-debug.log` に現れる目印（`-d int` の出力形式）。
    qemu_marker: &'static str,
    /// シリアルログに現れることを追加で要求する文字列。
    extra_serial_markers: &'static [&'static str],
    /// 汎用レジスタの並び順を既知の値で突き合わせるか。
    check_registers: bool,
    /// `CPU Reset` の回数が起動時の 2 回から増えていないことを確認するか。
    /// ダブルフォルトがトリプルフォルトへ落ちていないことの確認に使う。
    check_no_extra_cpu_reset: bool,
}

/// `--exception-test invalid-opcode` が例外の直前に各 GPR へ入れる既知の値。
///
/// kernel 側の `known_register_values` と一致していなければならない。
/// スタブの push 順と `ExceptionContext` のフィールド順が食い違うと、
/// ダンプは出るのに名前と値の対応だけが入れ替わる。値がもっともらしいため
/// 気づきにくいので、レジスタごとに異なる値を入れて突き合わせる。
const KNOWN_REGISTERS: &[(&str, &str)] = &[
    ("rax", "0x1111111111111111"),
    ("rbx", "0x2222222222222222"),
    ("rcx", "0x3333333333333333"),
    ("rdx", "0x4444444444444444"),
    ("rsi", "0x5555555555555555"),
    ("rdi", "0x6666666666666666"),
    ("rbp", "0x7777777777777777"),
    ("r8", "0x8888888888888888"),
    ("r9", "0x9999999999999999"),
    ("r10", "0xaaaaaaaaaaaaaaaa"),
    ("r11", "0xbbbbbbbbbbbbbbbb"),
    ("r12", "0xcccccccccccccccc"),
    ("r13", "0xdddddddddddddddd"),
    ("r14", "0xeeeeeeeeeeeeeeee"),
    ("r15", "0xffffffffffffffff"),
];

const EXCEPTION_TESTS: &[ExceptionTest] = &[
    ExceptionTest {
        name: "divide-by-zero",
        feature: "exception-test-divide-by-zero",
        vector: 0,
        qemu_marker: "v=00",
        extra_serial_markers: &["error code = (none for this exception)"],
        check_registers: false,
        check_no_extra_cpu_reset: false,
    },
    ExceptionTest {
        name: "invalid-opcode",
        feature: "exception-test-invalid-opcode",
        vector: 6,
        qemu_marker: "v=06",
        extra_serial_markers: &[],
        check_registers: true,
        check_no_extra_cpu_reset: false,
    },
    ExceptionTest {
        name: "page-fault",
        feature: "exception-test-page-fault",
        vector: 14,
        qemu_marker: "v=0e",
        extra_serial_markers: &[
            // フォルトしたアドレスが CR2 から取れていること。
            "cr2=0x0000400000000000 (faulting address)",
            // エラーコードが人間に読める形へ展開されていること。
            "cause=page not present access=read mode=supervisor",
        ],
        check_registers: false,
        check_no_extra_cpu_reset: false,
    },
    ExceptionTest {
        name: "double-fault",
        feature: "exception-test-double-fault",
        vector: 8,
        qemu_marker: "v=08",
        extra_serial_markers: &[
            // IST1 へ切り替わっていること。切り替わっていなければ
            // 壊れている可能性のあるスタックの上でハンドラが動いている。
            "on IST1=true",
            // #DF のエラーコードは常に 0。
            "always zero for #DF",
        ],
        check_registers: false,
        // IST が効いていなければトリプルフォルトになり、CPU Reset が増える。
        check_no_extra_cpu_reset: true,
    },
];

/// 起動時に必ず記録される `CPU Reset` の回数（電源投入シーケンス、
/// `docs/troubleshooting.md` 参照）。これを超えたらリセットが起きている。
const EXPECTED_CPU_RESET_COUNT: usize = 2;

const EXCEPTION_TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// クリティカルセクションとロックの回帰チェック（`--critical-test <kind>`）。
///
/// 検出の仕組みを入れても、それが機能しなければ意味がない。わざと異常経路を
/// 踏ませて、検出が働くことを確かめる（M4-b-1 のスタブ検証と同じ発想）。
struct CriticalTest {
    name: &'static str,
    feature: &'static str,
    /// シリアルログに現れることを要求する文字列。
    expected_markers: &'static [&'static str],
    /// 現れてはいけない文字列（検出をすり抜けたことを示すもの）。
    forbidden_markers: &'static [&'static str],
}

const CRITICAL_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "double-lock",
    feature: "critical-test-double-lock",
    expected_markers: &[
        "lock: double acquisition detected",
        "halting (cli + hlt loop)",
    ],
    // 検出をすり抜けて 2 回目の lock() が戻ってきた場合に出る行。
    forbidden_markers: &["double-lock detection FAILED"],
}];

/// カーネルが起動したことを示す、シリアルログの既知の行。
///
/// kernel の `kernel_main` が最初に出す行（`common::log` の INFO 形式）。
/// これがログに無ければ、カーネルは走っていない。
const KERNEL_STARTED_MARKER: &str = "[INFO] ZaytOS kernel: entered _start";

/// bootloader が起動したことを示す、シリアルログの既知の行。
///
/// panic-test は bootloader を検証するもので、kernel へ到達する前に panic
/// する。したがって kernel の marker ではなくこちらで起動を判定する。
const BOOTLOADER_STARTED_MARKER: &str = "[INFO] ZaytOS bootloader: serial log established";

/// OVMF がまれにカーネルを起動せず、シェルやアイドルループへフォールバック
/// することがある（`docs/troubleshooting.md` 2026-07-19 の記録）。この場合
/// ファームウェアのタイマ割り込み（ベクタ 0x20）が延々と記録され、RIP は
/// ファームウェア領域（おおむね 0x0f00_0000 以上、実 RAM 256MiB の外）を指す。
const FIRMWARE_REGION_START: u64 = 0x0f00_0000;

/// テストの前提（カーネルが起動したか）を判定した結果。
enum BootOutcome {
    /// カーネルが起動した。テスト結果を信頼してよい。
    Started,
    /// カーネルが起動しなかった。テストの成否ではなく環境の問題。
    DidNotStart {
        /// ファームウェア領域を指す RIP を `-d int` ログから拾えた場合。
        firmware_rip: Option<u64>,
    },
}

/// シリアルログと QEMU デバッグログから、対象が起動したかを判定する。
///
/// `started_marker` は起動を示す既知の行（kernel なら
/// [`KERNEL_STARTED_MARKER`]、bootloader なら [`BOOTLOADER_STARTED_MARKER`]）。
///
/// **テストの FAIL を報告する前に必ず呼ぶこと。** 「実装の問題」と「OVMF の
/// 起動フレーキネス」を取り違えると、存在しないバグを追いかけることになる
/// （実際に M4-c-1 でこの取り違えが起きかけた）。
fn classify_boot(serial: &str, qemu_debug: &str, started_marker: &str) -> BootOutcome {
    if serial.contains(started_marker) {
        return BootOutcome::Started;
    }

    // 起動していない。裏付けとして、ファームウェア領域を指す RIP を探す。
    // `-d int` の RIP 行は "RIP=000000000f6e973c ..." の形。
    let firmware_rip = qemu_debug.lines().rev().find_map(|line| {
        let rest = line.trim_start().strip_prefix("RIP=")?;
        let hex = rest.split_whitespace().next()?;
        let value = u64::from_str_radix(hex, 16).ok()?;
        (value >= FIRMWARE_REGION_START).then_some(value)
    });

    BootOutcome::DidNotStart { firmware_rip }
}

/// 起動失敗を報告する。テスト FAIL とは別物として扱う。
///
/// 戻り値の `Err` は「テストが落ちた」ではなく「環境の問題で判定できない」
/// ことを表す。呼び出し側はこのメッセージで両者を区別する。
fn report_did_not_start(context: &str, firmware_rip: Option<u64>) -> Result<()> {
    println!("{context}: TARGET DID NOT START (not a test failure)");
    println!("{context}:   the serial log has no start-up marker line");
    match firmware_rip {
        Some(rip) => println!(
            "{context}:   qemu -d int shows RIP={rip:#018x} in the firmware region \
             (>= {FIRMWARE_REGION_START:#x}); OVMF fell back to its shell/idle loop"
        ),
        None => println!(
            "{context}:   could not confirm a firmware RIP, but the first log line is absent"
        ),
    }
    println!(
        "{context}:   this is the OVMF boot flakiness noted in docs/troubleshooting.md; \
         re-run the test"
    );
    bail!("{context}: kernel did not start (environment, not the code); re-run")
}

// パニックハンドラの出力（bootloader/src/panic.rs）と対応する、回帰チェック用の
// 目印文字列。フォーマットを変更した場合はここも合わせて更新すること。
const PANIC_MARKER_HEADER: &str = "[ERROR] panic:";
const PANIC_MARKER_HALT: &str = "halting (cli + hlt loop)";
const PANIC_TEST_TIMEOUT: Duration = Duration::from_secs(10);
const PANIC_TEST_POLL_INTERVAL: Duration = Duration::from_millis(100);

const DEFAULT_SCREENSHOT_WAIT: Duration = Duration::from_secs(8);
const MONITOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SCREENDUMP_FILE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    const USAGE: &str = "usage: cargo xtask run [--panic-test] [--gui] [--gfx-test] [--kvm]\n       cargo xtask run --exception-test <kind>\n       cargo xtask run --critical-test <kind>\n       cargo xtask screenshot [output.png] [--wait-secs N] [--gfx-test] [--kvm]\n       cargo xtask gen-font";

    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => {
            let rest = &args[1..];
            let panic_test = rest.iter().any(|a| a == "--panic-test");
            let gui = rest.iter().any(|a| a == "--gui");
            let gfx_test = rest.iter().any(|a| a == "--gfx-test");
            let kvm = rest.iter().any(|a| a == "--kvm");
            if let Some(index) = rest.iter().position(|a| a == "--critical-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = CRITICAL_TESTS.iter().map(|t| t.name).collect();
                    format!("--critical-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_critical_test(kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--exception-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = EXCEPTION_TESTS.iter().map(|t| t.name).collect();
                    format!("--exception-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_exception_test(kind);
            }
            cmd_run(panic_test, gui, gfx_test, kvm)
        }
        Some("screenshot") => cmd_screenshot(&args[1..]),
        Some("gen-font") => font::generate(&workspace_root()?),
        Some(other) => bail!("unknown xtask subcommand: {other}\n\n{USAGE}"),
        None => bail!("missing xtask subcommand\n\n{USAGE}"),
    }
}

/// QEMU の `-serial` に渡す送り先。通常運用は人間がその場で読める `stdio`、
/// panic-test 回帰チェック・screenshot はプログラムから内容を検査できる
/// `file` を使う。
enum SerialSink {
    Stdio,
    File(PathBuf),
}

/// QEMU の `-display` バックエンド。既定は `none`（ADR-0003: シリアルログを
/// 唯一の観測手段とする）。`--gui` 指定時のみ実際のウィンドウを開く。
enum DisplayMode {
    None,
    Gui,
}

/// QEMU のアクセラレータ。既定は TCG（純粋エミュレーション）。
///
/// 既定を TCG のままにしているのは、これまでの全マイルストーンを TCG で
/// 検証してきており、既定を変えると挙動差の切り分け軸が増えるため。
/// KVM は計測時にだけ `--kvm` で明示的に選ぶ（ADR-0015 Addendum）。
enum Accelerator {
    Tcg,
    Kvm,
}

/// `qemu_launch_args` に渡す設定一式。引数が増えてきたため、位置引数の
/// 取り違えを避けるためにまとめている。
struct QemuLaunchOptions<'a> {
    ovmf_code: &'a Path,
    ovmf_vars: &'a Path,
    esp_dir: &'a Path,
    serial: &'a SerialSink,
    debug_log: &'a Path,
    display: DisplayMode,
    /// `Some` の場合、この UNIX ソケットパスで QEMU monitor (HMP) を
    /// server モードで待ち受けさせる（screenshot サブコマンド用）。
    monitor_socket: Option<&'a Path>,
    accelerator: Accelerator,
}

fn cmd_run(panic_test: bool, gui: bool, gfx_test: bool, kvm: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, panic_test)?;
    let kernel_elf = build_kernel(&workspace_root, gfx_test)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    if panic_test {
        run_panic_test(&workspace_root, &ovmf_vars, &esp_dir)
    } else {
        run_interactive(&workspace_root, &ovmf_vars, &esp_dir, gui, kvm)
    }
}

fn run_interactive(
    workspace_root: &Path,
    ovmf_vars: &Path,
    esp_dir: &Path,
    gui: bool,
    kvm: bool,
) -> Result<()> {
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        serial: &SerialSink::Stdio,
        debug_log: &debug_log,
        display: if gui {
            DisplayMode::Gui
        } else {
            DisplayMode::None
        },
        monitor_socket: None,
        accelerator: if kvm {
            Accelerator::Kvm
        } else {
            Accelerator::Tcg
        },
    });
    println!("qemu debug log (-d int,cpu_reset): {}", debug_log.display());
    if kvm {
        println!(
            "accelerator: KVM (measurement mode). exception logging via -d int is \
             largely unavailable; use the default TCG for debugging."
        );
    }

    let status = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .status()
        .context(
            "failed to launch qemu-system-x86_64 (is it installed? `apt install qemu-system-x86`)",
        )?;

    if !status.success() {
        bail!("qemu-system-x86_64 exited with {status}");
    }
    Ok(())
}

/// パニックハンドラの回帰チェック。`panic-test` フィーチャ付きでビルドした
/// bootloader（起動完了直後に意図的に `panic!` する）を起動し、シリアル出力に
/// 期待どおりのパニックダンプが現れるかをポーリングで確認する。
///
/// bootloader はパニック後も `hlt` ループで動き続け自然終了しないため、目印を
/// 検出し次第（またはタイムアウトで）QEMU プロセスを強制終了する。
fn run_panic_test(workspace_root: &Path, ovmf_vars: &Path, esp_dir: &Path) -> Result<()> {
    let serial_log_path = workspace_root.join("target").join("panic-test-serial.log");
    let _ = fs::remove_file(&serial_log_path);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        serial: &SerialSink::File(serial_log_path.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        // パニック経路の回帰チェックは例外まわりの挙動を見るものなので、
        // 常に TCG で行う。
        accelerator: Accelerator::Tcg,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the panic-test regression check")?;

    let deadline = Instant::now() + PANIC_TEST_TIMEOUT;
    let found = loop {
        if panic_markers_present(&serial_log_path) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    };

    let _ = child.kill();
    let _ = child.wait();

    let captured = fs::read_to_string(&serial_log_path).unwrap_or_default();
    let qemu = fs::read_to_string(&debug_log).unwrap_or_default();
    println!("--- panic-test: captured serial output ---\n{captured}--- end ---");

    if found {
        println!("panic-test: PASS (panic handler produced the expected dump and halted)");
        return Ok(());
    }

    // 失敗した。実装の問題か、そもそも bootloader が起動しなかったかを分ける。
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&captured, &qemu, BOOTLOADER_STARTED_MARKER)
    {
        return report_did_not_start("panic-test", firmware_rip);
    }

    bail!(
        "panic-test: FAIL (bootloader started but did not produce the expected \
         panic-handler output within {PANIC_TEST_TIMEOUT:?})"
    )
}

fn panic_markers_present(serial_log_path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(serial_log_path) else {
        return false;
    };
    contents.contains(PANIC_MARKER_HEADER) && contents.contains(PANIC_MARKER_HALT)
}

/// `cargo xtask screenshot [output.png] [--wait-secs N]` の引数を解釈する。
fn cmd_screenshot(args: &[String]) -> Result<()> {
    let mut wait = DEFAULT_SCREENSHOT_WAIT;
    let mut output_path = None;
    let mut gfx_test = false;
    let mut kvm = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gfx-test" => gfx_test = true,
            "--kvm" => kvm = true,
            "--wait-secs" => {
                i += 1;
                let value = args
                    .get(i)
                    .context("--wait-secs requires a value (seconds)")?;
                let secs: u64 = value
                    .parse()
                    .with_context(|| format!("invalid --wait-secs value: {value}"))?;
                wait = Duration::from_secs(secs);
            }
            other => output_path = Some(PathBuf::from(other)),
        }
        i += 1;
    }

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel(&workspace_root, gfx_test)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let output_path =
        output_path.unwrap_or_else(|| workspace_root.join("target").join("screenshot.png"));

    take_screenshot(&workspace_root, &ovmf_vars, &esp_dir, wait, &output_path, kvm)
}

/// QEMU を monitor (HMP) 付きで起動し、`wait` だけ待ってから `screendump` を
/// 発行し、結果の PPM を PNG へ変換して `output_path` に保存する。
///
/// 既存のシリアル出力（`SerialSink`）・デバッグログ（`-D`）の分離は崩さない:
/// このコマンド専用のシリアルログファイルへ出力させ、ターミナルには
/// このコマンド自身の進捗メッセージのみを出す。
fn take_screenshot(
    workspace_root: &Path,
    ovmf_vars: &Path,
    esp_dir: &Path,
    wait: Duration,
    output_path: &Path,
    kvm: bool,
) -> Result<()> {
    // AF_UNIX のパス長制限 (108 バイト程度) を避けるため、ワークスペース内の
    // 長いパスではなく /tmp 配下の短い一意なパスを使う。
    let monitor_socket =
        PathBuf::from(format!("/tmp/zaytos-xtask-mon-{}.sock", std::process::id()));
    let _ = fs::remove_file(&monitor_socket);
    let serial_log = workspace_root.join("target").join("screenshot-serial.log");
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let ppm_path = workspace_root.join("target").join("screenshot.ppm");
    let _ = fs::remove_file(&ppm_path);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        serial: &SerialSink::File(serial_log),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: Some(&monitor_socket),
        accelerator: if kvm {
            Accelerator::Kvm
        } else {
            Accelerator::Tcg
        },
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for screenshot")?;

    println!("waiting {wait:?} for the boot sequence to settle before capturing...");
    thread::sleep(wait);

    let result = capture_screendump(&monitor_socket, &ppm_path);

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&monitor_socket);

    result?;

    let img = image::open(&ppm_path)
        .with_context(|| format!("failed to decode {}", ppm_path.display()))?;
    img.save(output_path)
        .with_context(|| format!("failed to save {}", output_path.display()))?;

    println!(
        "screenshot saved: {} ({}x{})",
        output_path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}

/// QEMU monitor (HMP) へ接続し `screendump <ppm_path>` を発行、ファイルが
/// 現れるまで待つ。
fn capture_screendump(monitor_socket: &Path, ppm_path: &Path) -> Result<()> {
    let mut stream = connect_monitor_with_retry(monitor_socket)?;
    writeln!(stream, "screendump {}", ppm_path.display())
        .context("failed to send screendump command to the QEMU monitor")?;
    stream.flush().ok();
    wait_for_file(ppm_path, SCREENDUMP_FILE_TIMEOUT)
}

fn connect_monitor_with_retry(socket_path: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + MONITOR_CONNECT_TIMEOUT;
    loop {
        match UnixStream::connect(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to connect to QEMU monitor socket at {}",
                        socket_path.display()
                    )
                })
            }
        }
    }
}

fn wait_for_file(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            bail!(
                "timed out after {timeout:?} waiting for {} to be created",
                path.display()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

/// `bootloader` パッケージを UEFI ターゲット向けにビルドし、生成された
/// `.efi` バイナリのパスを返す。`panic_test` が true の場合、起動完了直後に
/// 意図的に `panic!` する `panic-test` フィーチャを有効にする。
fn build_bootloader(workspace_root: &Path, panic_test: bool) -> Result<PathBuf> {
    let mut args = vec![
        "build",
        "--target",
        UEFI_TARGET,
        "-p",
        BOOTLOADER_PACKAGE,
        "--bin",
        BOOTLOADER_PACKAGE,
    ];
    if panic_test {
        args.push("--features");
        args.push(PANIC_TEST_FEATURE);
    }

    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .args(&args)
        .status()
        .context("failed to invoke cargo to build the bootloader")?;

    if !status.success() {
        bail!("bootloader build failed ({status})");
    }

    let efi_path = workspace_root
        .join("target")
        .join(UEFI_TARGET)
        .join("debug")
        .join(format!("{BOOTLOADER_PACKAGE}.efi"));
    if !efi_path.exists() {
        bail!(
            "bootloader build reported success but {} is missing",
            efi_path.display()
        );
    }
    Ok(efi_path)
}

/// `kernel` パッケージを `x86_64-unknown-none` ターゲット向けにビルドし、
/// 生成された ELF バイナリのパスを返す。
/// クリティカルセクション/ロックの回帰チェックを 1 種類実行する。
///
/// 例外テストと同じく **TCG 固定**。
fn cmd_critical_test(kind: &str) -> Result<()> {
    let test = CRITICAL_TESTS
        .iter()
        .find(|t| t.name == kind)
        .with_context(|| {
            let names: Vec<&str> = CRITICAL_TESTS.iter().map(|t| t.name).collect();
            format!("unknown critical test {kind:?} (expected one of: {})", names.join(", "))
        })?;

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, &[test.feature])?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root.join("target").join("critical-test-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
    });

    let sentinel = test.expected_markers[0];
    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the critical test")?;

    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    loop {
        if fs::read_to_string(&serial_log)
            .map(|c| c.contains(sentinel))
            .unwrap_or(false)
        {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let _ = child.kill();
    let _ = child.wait();

    let serial = fs::read_to_string(&serial_log).unwrap_or_default();
    let qemu = fs::read_to_string(&debug_log).unwrap_or_default();

    let context = format!("critical-test {}", test.name);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip);
    }

    println!("--- {context}: relevant output ---");
    for line in serial
        .lines()
        .filter(|l| l.contains("critical") || l.contains("lock:"))
    {
        println!("{line}");
    }
    println!("--- end ---");

    let mut ok = true;
    for marker in test.expected_markers {
        let present = serial.contains(marker);
        ok &= present;
        println!(
            "{context}: serial contains {marker:?} = {}",
            if present { "OK" } else { "NG" }
        );
    }
    for marker in test.forbidden_markers {
        let absent = !serial.contains(marker);
        ok &= absent;
        println!(
            "{context}: serial does NOT contain {marker:?} = {}",
            if absent { "OK" } else { "NG (detection was bypassed)" }
        );
    }

    if ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAIL")
    }
}

/// 例外ハンドラの回帰チェックを 1 種類実行する。
///
/// シリアルログのハンドラ出力と、`qemu-debug.log` の `v=..` の両方を
/// 突き合わせる。片方だけでは、ベクタ番号を取り違えたまま動いているように
/// 見える事故を防げない（ADR-0018）。
fn cmd_exception_test(kind: &str) -> Result<()> {
    let test = EXCEPTION_TESTS
        .iter()
        .find(|t| t.name == kind)
        .with_context(|| {
            let names: Vec<&str> = EXCEPTION_TESTS.iter().map(|t| t.name).collect();
            format!("unknown exception test {kind:?} (expected one of: {})", names.join(", "))
        })?;

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, &[test.feature])?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("exception-test-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    // 例外の記録が要るので、この検証は常に TCG で行う。
    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
    });

    let expected_serial = format!("[ERROR] exception: vector={} ", test.vector);

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the exception test")?;

    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let handler_ran = loop {
        if fs::read_to_string(&serial_log)
            .map(|c| c.contains(&expected_serial))
            .unwrap_or(false)
        {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    };

    let _ = child.kill();
    let _ = child.wait();

    let serial = fs::read_to_string(&serial_log).unwrap_or_default();
    let qemu = fs::read_to_string(&debug_log).unwrap_or_default();

    // テスト結果を読む前に、そもそもカーネルが起動したかを判定する。
    // 起動していなければ、以降の OK/NG は意味を持たない。
    let context = format!("exception-test {}", test.name);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip);
    }

    let qemu_saw_it = qemu.contains(test.qemu_marker);

    println!("--- exception-test {}: handler output ---", test.name);
    for line in serial.lines().filter(|l| l.starts_with("[ERROR]")) {
        println!("{line}");
    }
    println!("--- end ---");

    println!(
        "exception-test {}: handler reported vector {} = {}",
        test.name,
        test.vector,
        if handler_ran { "OK" } else { "NG" }
    );
    println!(
        "exception-test {}: qemu -d int recorded {} = {}",
        test.name,
        test.qemu_marker,
        if qemu_saw_it { "OK" } else { "NG" }
    );

    // 追加の目印（CR2、エラーコードの展開、IST の確認など）。
    let mut markers_ok = true;
    for marker in test.extra_serial_markers {
        let present = serial.contains(marker);
        markers_ok &= present;
        println!(
            "exception-test {}: serial contains {marker:?} = {}",
            test.name,
            if present { "OK" } else { "NG" }
        );
    }

    // 汎用レジスタの並び順。名前と値の対応が入れ替わっていれば落ちる。
    let mut registers_ok = true;
    if test.check_registers {
        let mut wrong = Vec::new();
        for (name, value) in KNOWN_REGISTERS {
            if !serial.contains(&format!("{name}={value}")) {
                wrong.push(*name);
            }
        }
        registers_ok = wrong.is_empty();
        if registers_ok {
            println!(
                "exception-test {}: all {} general purpose registers dumped with the \
                 expected value = OK",
                test.name,
                KNOWN_REGISTERS.len()
            );
        } else {
            println!(
                "exception-test {}: register mismatch for {:?} = NG (the push order in \
                 the stub and the field order in ExceptionContext disagree)",
                test.name, wrong
            );
        }
    }

    // トリプルフォルトになっていないこと。
    let mut reset_ok = true;
    if test.check_no_extra_cpu_reset {
        let resets = qemu.matches("CPU Reset").count();
        reset_ok = resets <= EXPECTED_CPU_RESET_COUNT;
        println!(
            "exception-test {}: CPU Reset count = {resets} (expected <= {}) = {}",
            test.name,
            EXPECTED_CPU_RESET_COUNT,
            if reset_ok { "OK" } else { "NG (triple fault?)" }
        );
    }

    if handler_ran && qemu_saw_it && markers_ok && registers_ok && reset_ok {
        println!("exception-test {}: PASS", test.name);
        Ok(())
    } else {
        bail!(
            "exception-test {}: FAIL (handler={handler_ran}, qemu={qemu_saw_it}, \
             markers={markers_ok}, registers={registers_ok}, cpu_reset={reset_ok})",
            test.name
        );
    }
}

/// 指定した feature 付きで kernel をビルドする。
fn build_kernel_with_features(workspace_root: &Path, features: &[&str]) -> Result<PathBuf> {
    let mut command = Command::new("cargo");
    command.current_dir(workspace_root).args([
        "build",
        "--target",
        KERNEL_TARGET,
        "-p",
        KERNEL_PACKAGE,
        "--bin",
        KERNEL_PACKAGE,
    ]);
    if !features.is_empty() {
        command.args(["--features", &features.join(",")]);
    }
    let status = command
        .status()
        .context("failed to invoke cargo to build the kernel")?;
    if !status.success() {
        bail!("kernel build failed ({status})");
    }

    let elf_path = workspace_root
        .join("target")
        .join(KERNEL_TARGET)
        .join("debug")
        .join(KERNEL_PACKAGE);
    if !elf_path.exists() {
        bail!(
            "kernel build reported success but {} is missing",
            elf_path.display()
        );
    }
    Ok(elf_path)
}

fn build_kernel(workspace_root: &Path, gfx_test: bool) -> Result<PathBuf> {
    let mut command = Command::new("cargo");
    command.current_dir(workspace_root).args([
        "build",
        "--target",
        KERNEL_TARGET,
        "-p",
        KERNEL_PACKAGE,
        "--bin",
        KERNEL_PACKAGE,
    ]);
    if gfx_test {
        command.args(["--features", GFX_TEST_PATTERN_FEATURE]);
    }
    let status = command
        .status()
        .context("failed to invoke cargo to build the kernel")?;

    if !status.success() {
        bail!("kernel build failed ({status})");
    }

    let elf_path = workspace_root
        .join("target")
        .join(KERNEL_TARGET)
        .join("debug")
        .join(KERNEL_PACKAGE);
    if !elf_path.exists() {
        bail!(
            "kernel build reported success but {} is missing",
            elf_path.display()
        );
    }
    Ok(elf_path)
}

/// このプロジェクトで使う OVMF ビルド（Ubuntu の `ovmf` パッケージ）は、
/// ブート可能な `\EFI\BOOT\BOOTX64.EFI` を自動探索するのではなく、既定で
/// 組み込みの UEFI Interactive Shell を起動する（docs/troubleshooting.md
/// 参照。根本原因は未解明で、これは回避策）。そのシェルは起動直後に
/// `startup.nsh` を探して自動実行するため、それを使って明示的に
/// bootloader.efi をチェインロードする。
const STARTUP_NSH: &str = "FS0:\\EFI\\BOOT\\BOOTX64.EFI\r\n";

/// OVMF の既定の起動パス（`\EFI\BOOT\BOOTX64.EFI`）に bootloader.efi を配置
/// した ESP (EFI System Partition) 相当のディレクトリを用意する。QEMU の
/// `fat:` ドライバでこのディレクトリをそのまま仮想 FAT ドライブとして渡せる
/// ため、ディスクイメージファイルを別途作成する必要はない。
fn stage_esp(workspace_root: &Path, bootloader_efi: &Path, kernel_elf: &Path) -> Result<PathBuf> {
    let esp_dir = workspace_root.join("target").join("esp");
    let boot_dir = esp_dir.join("EFI").join("BOOT");
    fs::create_dir_all(&boot_dir)
        .with_context(|| format!("failed to create {}", boot_dir.display()))?;

    let boot_efi = boot_dir.join("BOOTX64.EFI");
    fs::copy(bootloader_efi, &boot_efi).with_context(|| {
        format!(
            "failed to copy {} to {}",
            bootloader_efi.display(),
            boot_efi.display()
        )
    })?;

    let startup_nsh = esp_dir.join("startup.nsh");
    fs::write(&startup_nsh, STARTUP_NSH)
        .with_context(|| format!("failed to write {}", startup_nsh.display()))?;

    // bootloader 側の ELF ローダー（bootloader/src/loader.rs）がここから
    // 読み込む（M2-0c）。
    let kernel_dir = esp_dir.join("zaytos");
    fs::create_dir_all(&kernel_dir)
        .with_context(|| format!("failed to create {}", kernel_dir.display()))?;
    let staged_kernel_elf = kernel_dir.join("kernel.elf");
    fs::copy(kernel_elf, &staged_kernel_elf).with_context(|| {
        format!(
            "failed to copy {} to {}",
            kernel_elf.display(),
            staged_kernel_elf.display()
        )
    })?;

    Ok(esp_dir)
}

fn workspace_root() -> Result<PathBuf> {
    // xtask は常に `<workspace_root>/xtask` に置かれ、cargo run 経由で起動される
    // ため、CARGO_MANIFEST_DIR の親をワークスペースルートとみなせる。
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .parent()
        .map(Path::to_path_buf)
        .context("failed to resolve workspace root from CARGO_MANIFEST_DIR")
}

/// OVMF の変数領域 (NVRAM) は QEMU が起動時に書き込むため、パッケージ配布物を
/// そのまま渡さず target/ovmf/ 配下に書き込み可能なコピーを用意する。
fn prepare_ovmf_vars(workspace_root: &Path) -> Result<PathBuf> {
    let ovmf_dir = workspace_root.join("target").join("ovmf");
    fs::create_dir_all(&ovmf_dir)
        .with_context(|| format!("failed to create {}", ovmf_dir.display()))?;

    let vars_copy = ovmf_dir.join("OVMF_VARS_4M.fd");
    if !vars_copy.exists() {
        fs::copy(OVMF_VARS_TEMPLATE_PATH, &vars_copy).with_context(|| {
            format!(
                "failed to copy OVMF vars template from {} (is the `ovmf` package installed? `apt install ovmf`)",
                OVMF_VARS_TEMPLATE_PATH
            )
        })?;
    }
    Ok(vars_copy)
}

/// QEMU 起動引数を組み立てる。
///
/// `-no-reboot -no-shutdown -d int,cpu_reset` は常時付与する: これらが無いと
/// 致命的例外発生時に QEMU が無言でリブートを
/// 繰り返し、原因を外部から観測できなくなる。
fn qemu_launch_args(opts: &QemuLaunchOptions) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        // デフォルトの i440FX/PIIX チップセット（レガシー IDE を持つ）を使う。
        // q35 では OVMF がドライブを既定の起動先として自動認識しなかった
        // ため（ADR-0007）、単純な IDE 接続のほうが確実である。
        "-m".into(),
        "256M".into(),
        "-drive".into(),
        format!(
            "if=pflash,format=raw,readonly=on,file={}",
            opts.ovmf_code.display()
        )
        .into(),
        "-drive".into(),
        format!("if=pflash,format=raw,file={}", opts.ovmf_vars.display()).into(),
        // ESP 相当のディレクトリを仮想 FAT ドライブとして渡す。OVMF は既定の
        // 起動パス `\EFI\BOOT\BOOTX64.EFI` を自動的に見つけて起動する。
        "-drive".into(),
        format!("format=raw,file=fat:rw:{}", opts.esp_dir.display()).into(),
        "-serial".into(),
        match opts.serial {
            SerialSink::Stdio => "stdio".into(),
            SerialSink::File(path) => format!("file:{}", path.display()).into(),
        },
        // 既定は none（ADR-0003: シリアルログを唯一の観測手段とする）。
        // `--gui` 指定時のみ実際のウィンドウ（WSLg 経由）を開く。
        "-display".into(),
        match opts.display {
            DisplayMode::None => "none".into(),
            DisplayMode::Gui => "gtk".into(),
        },
        "-no-reboot".into(),
        "-no-shutdown".into(),
        // KVM では例外・割り込みの大半が CPU 側で処理され QEMU を経由しない
        // ため、`-d int` の記録はほとんど残らない。デバッグは TCG（既定）、
        // 計測は KVM、という使い分けをすること（troubleshooting.md 参照）。
        "-d".into(),
        "int,cpu_reset".into(),
        // `-d` の出力先を明示的にファイルへ分離する。指定しない場合 QEMU 自身の
        // stderr に出て、`-serial stdio` のシリアル出力と混ざってしまい、
        // ターミナルでの可読性が大きく落ちる。
        "-D".into(),
        opts.debug_log.into(),
    ];

    match opts.accelerator {
        Accelerator::Tcg => {}
        Accelerator::Kvm => {
            args.push("-accel".into());
            args.push("kvm".into());
            // KVM では TSC がホストの実周波数で進む。計測用途では
            // ホスト側の TSC をそのまま見せる方が解釈しやすい。
            args.push("-cpu".into());
            args.push("host".into());
        }
    }

    if let Some(monitor_socket) = opts.monitor_socket {
        args.push("-monitor".into());
        args.push(format!("unix:{},server,nowait", monitor_socket.display()).into());
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options<'a>(serial: &'a SerialSink, debug_log: &'a Path) -> QemuLaunchOptions<'a> {
        QemuLaunchOptions {
            ovmf_code: Path::new("/x/CODE.fd"),
            ovmf_vars: Path::new("/y/VARS.fd"),
            esp_dir: Path::new("/z/esp"),
            serial,
            debug_log,
            display: DisplayMode::None,
            monitor_socket: None,
            accelerator: Accelerator::Tcg,
        }
    }

    fn joined_args(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn qemu_args_always_include_failure_visibility_flags() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a == "-no-reboot"));
        assert!(joined.iter().any(|a| a == "-no-shutdown"));

        let d_pos = joined
            .iter()
            .position(|a| a == "-d")
            .expect("-d flag missing");
        assert_eq!(joined[d_pos + 1], "int,cpu_reset");
    }

    #[test]
    fn qemu_args_reference_given_ovmf_and_esp_paths() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a.contains("/x/CODE.fd")));
        assert!(joined.iter().any(|a| a.contains("/y/VARS.fd")));
        assert!(joined.iter().any(|a| a.contains("fat:rw:/z/esp")));
    }

    #[test]
    fn qemu_args_use_serial_file_sink_when_requested() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let sink = SerialSink::File(PathBuf::from("/tmp/serial.log"));
        let opts = base_options(&sink, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a == "file:/tmp/serial.log"));
    }

    #[test]
    fn qemu_args_separate_debug_log_from_serial() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        let d_capital_pos = joined
            .iter()
            .position(|a| a == "-D")
            .expect("-D flag missing");
        assert_eq!(joined[d_capital_pos + 1], "/z/qemu-debug.log");
    }

    #[test]
    fn qemu_args_default_display_is_none() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        let pos = joined
            .iter()
            .position(|a| a == "-display")
            .expect("-display flag missing");
        assert_eq!(joined[pos + 1], "none");
    }

    #[test]
    fn qemu_args_gui_display_selects_gtk() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let mut opts = base_options(&SerialSink::Stdio, &debug_log);
        opts.display = DisplayMode::Gui;
        let joined = joined_args(&qemu_launch_args(&opts));

        let pos = joined
            .iter()
            .position(|a| a == "-display")
            .expect("-display flag missing");
        assert_eq!(joined[pos + 1], "gtk");
    }

    #[test]
    fn qemu_args_include_monitor_socket_when_requested() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let monitor_socket = PathBuf::from("/tmp/mon.sock");
        let mut opts = base_options(&SerialSink::Stdio, &debug_log);
        opts.monitor_socket = Some(&monitor_socket);
        let joined = joined_args(&qemu_launch_args(&opts));

        let pos = joined
            .iter()
            .position(|a| a == "-monitor")
            .expect("-monitor flag missing");
        assert_eq!(joined[pos + 1], "unix:/tmp/mon.sock,server,nowait");
    }

    #[test]
    fn qemu_args_omit_monitor_by_default() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(!joined.iter().any(|a| a == "-monitor"));
    }

    /// 既定は TCG のまま。これまでの全マイルストーンを TCG で検証してきて
    /// おり、既定を変えると挙動差の切り分け軸が増える（ADR-0015 Addendum）。
    #[test]
    fn qemu_args_do_not_enable_kvm_by_default() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let serial = SerialSink::Stdio;
        let args = joined_args(&qemu_launch_args(&base_options(&serial, &debug_log)));
        assert!(!args.iter().any(|a| a == "-accel"));
        assert!(!args.iter().any(|a| a == "kvm"));
    }

    #[test]
    fn qemu_args_enable_kvm_when_requested() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let serial = SerialSink::Stdio;
        let mut options = base_options(&serial, &debug_log);
        options.accelerator = Accelerator::Kvm;
        let args = joined_args(&qemu_launch_args(&options));

        let accel = args.iter().position(|a| a == "-accel").expect("-accel");
        assert_eq!(args[accel + 1], "kvm");
        // 計測時は TSC の解釈を単純にするためホストの CPU をそのまま見せる。
        let cpu = args.iter().position(|a| a == "-cpu").expect("-cpu");
        assert_eq!(args[cpu + 1], "host");
    }

    /// KVM を選んでも、失敗を可視化するオプションは外さない。取れる情報は
    /// 減るが、外すと「何も出ない」理由が分からなくなる。
    #[test]
    fn kvm_still_keeps_the_failure_visibility_flags() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let serial = SerialSink::Stdio;
        let mut options = base_options(&serial, &debug_log);
        options.accelerator = Accelerator::Kvm;
        let args = joined_args(&qemu_launch_args(&options));
        assert!(args.iter().any(|a| a == "-no-reboot"));
        assert!(args.iter().any(|a| a == "-no-shutdown"));
        assert!(args.iter().any(|a| a == "int,cpu_reset"));
    }

    #[test]
    fn panic_markers_present_requires_both_markers() {
        let dir = env::temp_dir().join(format!(
            "zaytos-xtask-test-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("serial.log");

        fs::write(&path, "boot ok\n").unwrap();
        assert!(!panic_markers_present(&path));

        fs::write(&path, format!("{PANIC_MARKER_HEADER} boom\n")).unwrap();
        assert!(!panic_markers_present(&path));

        fs::write(
            &path,
            format!("{PANIC_MARKER_HEADER} boom\n{PANIC_MARKER_HALT}\n"),
        )
        .unwrap();
        assert!(panic_markers_present(&path));

        let _ = fs::remove_dir_all(&dir);
    }
}
