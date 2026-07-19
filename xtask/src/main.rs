use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};

const OVMF_CODE_PATH: &str = "/usr/share/OVMF/OVMF_CODE_4M.fd";
const OVMF_VARS_TEMPLATE_PATH: &str = "/usr/share/OVMF/OVMF_VARS_4M.fd";
const BOOTLOADER_PACKAGE: &str = "bootloader";
const UEFI_TARGET: &str = "x86_64-unknown-uefi";
const KERNEL_PACKAGE: &str = "kernel";
const KERNEL_TARGET: &str = "x86_64-unknown-none";
const PANIC_TEST_FEATURE: &str = "panic-test";

// パニックハンドラの出力（bootloader/src/panic.rs）と対応する、回帰チェック用の
// 目印文字列。フォーマットを変更した場合はここも合わせて更新すること。
const PANIC_MARKER_HEADER: &str = "[ERROR] panic:";
const PANIC_MARKER_HALT: &str = "halting (cli + hlt loop)";
const PANIC_TEST_TIMEOUT: Duration = Duration::from_secs(10);
const PANIC_TEST_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("run") => {
            let panic_test = args.any(|a| a == "--panic-test");
            cmd_run(panic_test)
        }
        Some(other) => {
            bail!("unknown xtask subcommand: {other}\n\nusage: cargo xtask run [--panic-test]")
        }
        None => bail!("missing xtask subcommand\n\nusage: cargo xtask run [--panic-test]"),
    }
}

/// QEMU の `-serial` に渡す送り先。通常運用は人間がその場で読める `stdio`、
/// panic-test 回帰チェックはプログラムから内容を検査できる `file` を使う。
enum SerialSink {
    Stdio,
    File(PathBuf),
}

fn cmd_run(panic_test: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, panic_test)?;
    let kernel_elf = build_kernel(&workspace_root)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    if panic_test {
        run_panic_test(&workspace_root, &ovmf_vars, &esp_dir)
    } else {
        run_interactive(&ovmf_vars, &esp_dir)
    }
}

fn run_interactive(ovmf_vars: &Path, esp_dir: &Path) -> Result<()> {
    let qemu_args = qemu_launch_args(
        Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        &SerialSink::Stdio,
    );

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

    let qemu_args = qemu_launch_args(
        Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        &SerialSink::File(serial_log_path.clone()),
    );

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
    println!("--- panic-test: captured serial output ---\n{captured}--- end ---");

    if found {
        println!("panic-test: PASS (panic handler produced the expected dump and halted)");
        Ok(())
    } else {
        bail!(
            "panic-test: FAIL (did not observe the expected panic-handler output within {PANIC_TEST_TIMEOUT:?})"
        );
    }
}

fn panic_markers_present(serial_log_path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(serial_log_path) else {
        return false;
    };
    contents.contains(PANIC_MARKER_HEADER) && contents.contains(PANIC_MARKER_HALT)
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
///
/// M2-0c で ELF ローダーが実装されるまで、この kernel.elf は bootloader
/// から実際にロード・実行されない。ここではビルドして ESP に配置する
/// ところまでを行う（readelf/objdump による構造検証は開発時に別途行う）。
fn build_kernel(workspace_root: &Path) -> Result<PathBuf> {
    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .args([
            "build",
            "--target",
            KERNEL_TARGET,
            "-p",
            KERNEL_PACKAGE,
            "--bin",
            KERNEL_PACKAGE,
        ])
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

    // M2-0c で bootloader 側の ELF ローダーがここから読み込む想定の配置先。
    // 現時点ではまだ誰もこのファイルを読まない（ロード・実行は行われない）。
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
fn qemu_launch_args(
    ovmf_code: &Path,
    ovmf_vars: &Path,
    esp_dir: &Path,
    serial: &SerialSink,
) -> Vec<OsString> {
    vec![
        // デフォルトの i440FX/PIIX チップセット（レガシー IDE を持つ）を使う。
        // q35 では OVMF がドライブを既定の起動先として自動認識しなかった
        // ため（ADR-0007）、単純な IDE 接続のほうが確実である。
        "-m".into(),
        "256M".into(),
        "-drive".into(),
        format!(
            "if=pflash,format=raw,readonly=on,file={}",
            ovmf_code.display()
        )
        .into(),
        "-drive".into(),
        format!("if=pflash,format=raw,file={}", ovmf_vars.display()).into(),
        // ESP 相当のディレクトリを仮想 FAT ドライブとして渡す。OVMF は既定の
        // 起動パス `\EFI\BOOT\BOOTX64.EFI` を自動的に見つけて起動する。
        "-drive".into(),
        format!("format=raw,file=fat:rw:{}", esp_dir.display()).into(),
        "-serial".into(),
        match serial {
            SerialSink::Stdio => "stdio".into(),
            SerialSink::File(path) => format!("file:{}", path.display()).into(),
        },
        // GOP 経由の画面描画は M3 まで実装しないため、現段階では表示は不要。
        // シリアルログを唯一の観測手段として扱う (ADR-0003)。
        "-display".into(),
        "none".into(),
        "-no-reboot".into(),
        "-no-shutdown".into(),
        "-d".into(),
        "int,cpu_reset".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_args_always_include_failure_visibility_flags() {
        let args = qemu_launch_args(
            Path::new("/dummy/CODE.fd"),
            Path::new("/dummy/VARS.fd"),
            Path::new("/dummy/esp"),
            &SerialSink::Stdio,
        );
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

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
        let args = qemu_launch_args(
            Path::new("/x/CODE.fd"),
            Path::new("/y/VARS.fd"),
            Path::new("/z/esp"),
            &SerialSink::Stdio,
        );
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(joined.iter().any(|a| a.contains("/x/CODE.fd")));
        assert!(joined.iter().any(|a| a.contains("/y/VARS.fd")));
        assert!(joined.iter().any(|a| a.contains("fat:rw:/z/esp")));
    }

    #[test]
    fn qemu_args_use_serial_file_sink_when_requested() {
        let args = qemu_launch_args(
            Path::new("/x/CODE.fd"),
            Path::new("/y/VARS.fd"),
            Path::new("/z/esp"),
            &SerialSink::File(PathBuf::from("/tmp/serial.log")),
        );
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(joined.iter().any(|a| a == "file:/tmp/serial.log"));
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
