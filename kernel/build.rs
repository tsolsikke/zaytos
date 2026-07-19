//! `link.ld`（ADR-0009 検討中: リンクアドレスは暫定値）をリンカへ渡す。
//! 相対パスを `.cargo/config.toml` の rustflags に直接書くと呼び出し時の
//! カレントディレクトリに依存して壊れるため、`CARGO_MANIFEST_DIR` から
//! 絶対パスを組み立てて渡す。

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set");
    println!("cargo:rustc-link-arg=-T{manifest_dir}/link.ld");
    println!("cargo:rerun-if-changed=link.ld");
}
