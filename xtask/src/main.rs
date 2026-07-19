use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};

const OVMF_CODE_PATH: &str = "/usr/share/OVMF/OVMF_CODE_4M.fd";
const OVMF_VARS_TEMPLATE_PATH: &str = "/usr/share/OVMF/OVMF_VARS_4M.fd";

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("run") => cmd_run(),
        Some(other) => bail!("unknown xtask subcommand: {other}\n\nusage: cargo xtask run"),
        None => bail!("missing xtask subcommand\n\nusage: cargo xtask run"),
    }
}

fn cmd_run() -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;

    let qemu_args = qemu_launch_args(Path::new(OVMF_CODE_PATH), &ovmf_vars);

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
fn qemu_launch_args(ovmf_code: &Path, ovmf_vars: &Path) -> Vec<OsString> {
    vec![
        "-machine".into(),
        "q35".into(),
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
        "-serial".into(),
        "stdio".into(),
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
        let args = qemu_launch_args(Path::new("/dummy/CODE.fd"), Path::new("/dummy/VARS.fd"));
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
    fn qemu_args_reference_given_ovmf_paths() {
        let args = qemu_launch_args(Path::new("/x/CODE.fd"), Path::new("/y/VARS.fd"));
        let joined: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        assert!(joined.iter().any(|a| a.contains("/x/CODE.fd")));
        assert!(joined.iter().any(|a| a.contains("/y/VARS.fd")));
    }
}
