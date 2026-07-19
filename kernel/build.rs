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
    let target = std::env::var("TARGET").expect("TARGET is not set");
    if target != "x86_64-unknown-none" {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set");
    println!("cargo:rustc-link-arg=-T{manifest_dir}/link.ld");
    println!("cargo:rerun-if-changed=link.ld");
}
