//! `link.ld`（ADR-0009: kernel の低位固定アドレスへのリンク）をリンカへ渡す。
//! 相対パスを `.cargo/config.toml` の rustflags に直接書くと呼び出し時の
//! カレントディレクトリに依存して壊れるため、`CARGO_MANIFEST_DIR` から
//! 絶対パスを組み立てて渡す。
//!
//! この build script はパッケージのどのターゲット（実際の kernel バイナリ
//! だけでなく、`cargo test -p kernel --lib` のホスト向けテストバイナリも
//! 含む）をビルドする際にも必ず実行される。`link.ld` は
//! `x86_64-unknown-none` 向け（エントリポイント `_start`、0x100000 に
//! 全セクション配置）を前提にしており、これをホストの通常の実行可能
//! ファイルに適用すると、OS のプロセスローダーが期待する ELF 構造
//! （通常の crt0/エントリポイント）が壊れてプロセス起動直後に
//! セグメンテーション違反を起こす。そのため、実際にビルド対象が
//! `x86_64-unknown-none` のときだけリンカ引数を渡すようにする。
fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set");
    println!("cargo:rerun-if-changed=link.ld");

    // **リンカスクリプトの KERNEL_VIRT_BASE を Rust の定数として生成する。**
    //
    // 同じ値をリンカスクリプトと Rust の両方に手で書くと、片方だけ直した
    // ときに食い違う。食い違った状態は「リンクは通るが、アドレス変換が
    // 一段ずれる」という形で出て、最も診断しにくい。ここで 1 つの出所から
    // 生成しておけば、その状態が起きない。
    //
    // ホスト向けテストでもこの定数は使うので、生成はターゲットに関わらず行う。
    let script =
        std::fs::read_to_string(format!("{manifest_dir}/link.ld")).expect("failed to read link.ld");
    let virt_base = parse_symbol(&script, "KERNEL_VIRT_BASE")
        .expect("link.ld does not define KERNEL_VIRT_BASE");
    let load_addr = parse_symbol(&script, "KERNEL_LOAD_ADDR")
        .expect("link.ld does not define KERNEL_LOAD_ADDR");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is not set");
    std::fs::write(
        format!("{out_dir}/link_symbols.rs"),
        format!(
            "// build.rs が link.ld から生成した。手で編集しないこと。\n\
             pub const KERNEL_VIRT_BASE: u64 = {virt_base};\n\
             pub const KERNEL_LOAD_ADDR: u64 = {load_addr};\n"
        ),
    )
    .expect("failed to write link_symbols.rs");

    let target = std::env::var("TARGET").expect("TARGET is not set");
    if target != "x86_64-unknown-none" {
        return;
    }
    println!("cargo:rustc-link-arg=-T{manifest_dir}/link.ld");
}

/// `NAME = 0x...;` の形の代入から値を読む。
///
/// リンカスクリプトの完全な構文解析はしない。ZaytOS の `link.ld` が使って
/// いる形だけを見る。形が変わったら `None` になり、`expect` で落ちる。
/// 黙って既定値へ倒れるより、そこで止まるほうがよい。
fn parse_symbol(script: &str, name: &str) -> Option<u64> {
    for line in script.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_end_matches(';').trim();
        let value = value.strip_prefix("0x").unwrap_or(value);
        return u64::from_str_radix(value, 16).ok();
    }
    None
}
