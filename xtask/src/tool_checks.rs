//! 手で使う道具の軽い確かめ（2026-09-25。検査の体系の改善。運用者の決定）。
//!
//! **検査の項目でない道具は、黙って腐る。** `--calibration-spread` は HW-c の後ずっと何も採れて
//! いなかった（`docs/troubleshooting.md`）。`tools/qemu-variants.py --list` は、表に CPU の欄を
//! 足した後ずっと落ちていた（2026-09-25 に、この確かめを置いて見つけた）。
//!
//! **道具ごとに「動いて、空でない値を出す」ことだけを見る。** **道具の中身の正しさは見ない**
//! ——それは道具を使う人が読む。**置き場は 3 つである。** QEMU を起こさないものは基底、起こすものは
//! `--full`、分の単位のものは段の締め（`cargo xtask flaky`）。

use std::fs;
use std::path::Path;
use std::process::Output;

use anyhow::{bail, Context, Result};

use super::{external_tool, parse_machine_variants, MACHINE_VARIANT_TABLE, REFERENCE_BOOT_LOG};

/// 確かめが一時のファイルを置く所（ビルドの置き場の下。追跡しない）。
fn scratch_dir(root: &Path) -> Result<std::path::PathBuf> {
    let dir = root.join("target").join("tool-checks");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

/// `tools/` の Python の道具を走らせる。**出力は UTF-8 で読む**（道具は日本語を出す）。
fn python(root: &Path, script: &str, args: &[&str]) -> Result<Output> {
    let mut tool = external_tool("python3");
    tool.arg(root.join("tools").join(script))
        .env("PYTHONIOENCODING", "utf-8")
        .current_dir(root);
    for arg in args {
        tool.arg(arg);
    }
    tool.output()
        .with_context(|| format!("failed to run tools/{script}"))
}

/// 終了の値が 0 であることを見て、標準出力を返す。
fn succeeded(script: &str, output: &Output) -> Result<String> {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        bail!(
            "tools/{script} exited with {}: {}{}",
            output.status,
            stdout.chars().take(400).collect::<String>(),
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(400)
                .collect::<String>()
        );
    }
    Ok(stdout)
}

/// `tools/boot-log-compare.py`——**参照をそれ自身と比べて 0 行、1 行だけ変えた写しと比べて 1 行。**
pub(super) fn boot_log_compare(root: &Path) -> Result<String> {
    let reference = root.join(REFERENCE_BOOT_LOG);
    let reference_arg = reference.to_string_lossy().into_owned();
    let same = succeeded(
        "boot-log-compare.py",
        &python(
            root,
            "boot-log-compare.py",
            &[reference_arg.as_str(), reference_arg.as_str()],
        )?,
    )?;
    if !same.contains("番地を伏せても食い違う行 0") {
        bail!("the reference compared with itself did not come out as 0 line(s): {same}");
    }

    // **1 行の末尾に語を足す**——**行の数は変えない**（数が違うと道具は比べずに降りる）。
    let text = fs::read_to_string(&reference)
        .with_context(|| format!("failed to read {}", reference.display()))?;
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let Some(first) = lines.first_mut() else {
        bail!("the reference is empty");
    };
    first.push_str(" (changed by the tool check)");
    let changed = scratch_dir(root)?.join("boot-log-compare-changed.txt");
    fs::write(&changed, format!("{}\n", lines.join("\n")))
        .with_context(|| format!("failed to write {}", changed.display()))?;
    let changed_arg = changed.to_string_lossy().into_owned();
    let differ = succeeded(
        "boot-log-compare.py",
        &python(
            root,
            "boot-log-compare.py",
            &[reference_arg.as_str(), changed_arg.as_str()],
        )?,
    )?;
    if !differ.contains("番地を伏せても食い違う行 1") {
        bail!("a copy with one line changed did not come out as 1 line: {differ}");
    }
    Ok(
        "the reference against itself: 0 line(s); against a copy with one line changed: 1"
            .to_string(),
    )
}

/// `tools/decisions-touched.py`——**最後の 1 コミットで走り、見出しの行を出す。**
pub(super) fn decisions_touched(root: &Path) -> Result<String> {
    let stdout = succeeded(
        "decisions-touched.py",
        &python(root, "decisions-touched.py", &["HEAD~1..HEAD"])?,
    )?;
    let Some(first) = stdout
        .lines()
        .next()
        .filter(|line| line.starts_with("消えた語: "))
    else {
        bail!("tools/decisions-touched.py did not print its header line: {stdout}");
    };
    Ok(format!("HEAD~1..HEAD: {first}"))
}

/// `tools/docstyle.py`——**S1・S2・S4・S5 を走らせ、全部通ること。** **`markdown-it-py` が要る**
/// （CI には版を固定して入れる。運用者の決定。2026-09-25）。
pub(super) fn docstyle(root: &Path) -> Result<String> {
    let output = python(root, "docstyle.py", &[])?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("markdown-it-py") {
        bail!(
            "tools/docstyle.py needs markdown-it-py; install the pinned version \
             (see .github/workflows/check.yml): {}",
            stderr.trim()
        );
    }
    let stdout = succeeded("docstyle.py", &output)?;
    if !stdout.contains("すべて通過") {
        bail!("tools/docstyle.py did not report that everything passed: {stdout}");
    }
    let checks = stdout
        .lines()
        .filter(|line| line.starts_with("OK "))
        .count();
    Ok(format!(
        "{checks} check(s) reported OK and everything passed"
    ))
}

/// `tools/frame-sizes.py`——**作業木の `.debug_frame` を読み、関数の枠が 1 つ以上出る。**
///
/// **基底の版は建てない**（`--working-tree-only`）——**取り出しと建てに分の単位が掛かる。**
pub(super) fn frame_sizes(root: &Path) -> Result<String> {
    let stdout = succeeded(
        "frame-sizes.py",
        &python(
            root,
            "frame-sizes.py",
            &["--working-tree-only", "--largest", "3"],
        )?,
    )?;
    let largest: Vec<&str> = stdout
        .lines()
        .skip_while(|line| !line.contains("largest frame(s) in the working tree"))
        .skip(1)
        .filter(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|size| size.parse::<u64>().is_ok())
        })
        .collect();
    let Some(top) = largest.first() else {
        bail!("tools/frame-sizes.py listed no frame: {stdout}");
    };
    Ok(format!(
        "{} frame(s) listed; the largest is{}",
        largest.len(),
        top.trim_start()
            .split_once(' ')
            .map(|(size, name)| format!(" {size} byte(s) in {}", name.trim()))
            .unwrap_or_default()
    ))
}

/// `tools/judgement-map.py`——**小さな抜き書きから、判定の本数と偽になった判定を数える。**
///
/// **抜き書きはここに持つ**（木にファイルを置かない）。**カーネルの行と `(info)` を数えない**ことも見る。
pub(super) fn judgement_map(root: &Path) -> Result<String> {
    const SAMPLE: &str = "\
fs-extract: the copy lies outside the kernel image = true
fs-extract fs-create-keep-test+ext2-create-skip-links-test: e2fsck found nothing to complain about = false (complaints: [])
fs-extract: e2fsck found nothing to complain about = true
[INFO] spawn: a kernel line that looks like a judgement = true
zi-test: (info) an instrument, not a judgement = false
";
    let sample = scratch_dir(root)?.join("judgement-map-sample.txt");
    fs::write(&sample, SAMPLE).with_context(|| format!("failed to write {}", sample.display()))?;
    let sample_arg = sample.to_string_lossy().into_owned();
    let stdout = succeeded(
        "judgement-map.py",
        &python(root, "judgement-map.py", &[sample_arg.as_str()])?,
    )?;
    for expected in [
        "判定の本数（名前で数えた）: 2",
        "どこかで偽になった判定: 1",
        "一度も偽にならなかった判定: 1",
    ] {
        if !stdout.contains(expected) {
            bail!("tools/judgement-map.py did not report {expected:?} for the sample: {stdout}");
        }
    }
    Ok("the sample of 5 line(s) came out as 2 judgement(s), 1 of them false".to_string())
}

/// `tools/qemu-variants.py --list`——**表の名前の並びが、`xtask` の読んだものと同じ。**
pub(super) fn qemu_variants(root: &Path) -> Result<String> {
    let stdout = succeeded(
        "qemu-variants.py",
        &python(root, "qemu-variants.py", &["--list"])?,
    )?;
    let listed: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim()))
        .collect();
    let parsed: Vec<&str> = parse_machine_variants(MACHINE_VARIANT_TABLE)?
        .iter()
        .map(|variant| variant.name)
        .collect();
    if listed != parsed {
        bail!("tools/qemu-variants.py --list gave {listed:?}, but xtask read {parsed:?}");
    }
    Ok(format!(
        "{} variant(s), the same names as xtask reads",
        listed.len()
    ))
}

/// `tools/stack-deepest.py`——**1 つの深さで走らせ、プロンプトの深さと最初の書き込みの経路が出る**
/// （`--full`。QEMU を 2 回起こす。実測で 22 秒ほど）。
///
/// **道具は `target/esp` に置かれた像をそのまま起こす**ので、**先に既定の像を置く**——**前の項目が
/// 破壊の構成の像を置いたままだと、別の像を測る。**
pub(super) fn stack_deepest(root: &Path) -> Result<String> {
    super::stage_default_image(root)?;
    let stdout = succeeded(
        "stack-deepest.py",
        &python(root, "stack-deepest.py", &["--depth", "4096"])?,
    )?;
    let prompt_depth = stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("at the prompt the kernel stack had been used down to ")?
                .split(' ')
                .next()?
                .parse::<u64>()
                .ok()
        })
        .context("tools/stack-deepest.py did not report the depth at the prompt")?;
    if prompt_depth == 0 {
        bail!("tools/stack-deepest.py read a depth of 0 at the prompt");
    }
    let frames = stdout
        .lines()
        .skip_while(|line| !line.starts_with("=== the first write at depth 4096"))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .count();
    if frames == 0 {
        bail!("tools/stack-deepest.py found no path for the first write at depth 4096: {stdout}");
    }
    Ok(format!(
        "the stack was used down to {prompt_depth} byte(s) at the prompt; the first write at depth \
         4096 came through {frames} frame(s)"
    ))
}

/// `cargo xtask run --calibration-spread 1`——**1 回で較正の値が 1 つ採れる**（`--full`。値は判定しない）。
pub(super) fn calibration_spread(_root: &Path) -> Result<String> {
    super::cmd_calibration_spread(1)?;
    Ok("one run produced a value; the value itself is not judged".to_string())
}

/// `cargo xtask screenshot`——**PNG が書かれ、PNG として読めて大きさを持つ**（`--full`）。
pub(super) fn screenshot(root: &Path) -> Result<String> {
    let shot = scratch_dir(root)?.join("screenshot.png");
    let _ = fs::remove_file(&shot);
    super::cmd_screenshot(&[shot.to_string_lossy().into_owned()])?;
    let (width, height) = png_dimensions(&shot)?;
    Ok(format!("a {width}x{height} PNG was written"))
}

/// PNG の署名と IHDR から、大きさを読む。
pub(super) fn png_dimensions(path: &Path) -> Result<(u32, u32)> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || !bytes.starts_with(SIGNATURE) || &bytes[12..16] != b"IHDR" {
        bail!("{} is not a PNG ({} byte(s))", path.display(), bytes.len());
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width == 0 || height == 0 {
        bail!("{} has a zero dimension ({width}x{height})", path.display());
    }
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **PNG の署名と IHDR から大きさを読む。** **署名の無いものと 0 の大きさは断る。**
    #[test]
    fn png_dimensions_are_read_from_the_header() {
        let dir = std::env::temp_dir().join(format!("zaytos-png-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut good = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        good.extend_from_slice(&1280u32.to_be_bytes());
        good.extend_from_slice(&800u32.to_be_bytes());
        let path = dir.join("good.png");
        fs::write(&path, &good).unwrap();
        assert_eq!(png_dimensions(&path).unwrap(), (1280, 800));

        let mut zero = good.clone();
        zero[16..20].copy_from_slice(&0u32.to_be_bytes());
        fs::write(&path, &zero).unwrap();
        assert!(png_dimensions(&path).is_err());

        fs::write(&path, b"P6\n1280 800\n255\n").unwrap();
        assert!(png_dimensions(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
