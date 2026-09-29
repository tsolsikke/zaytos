//! 項目のログの塊（2026-09-29。全検査の試験を同時に走らせる）。
//!
//! **同時に走る項目の出力を混ぜない。** 項目を走らせる糸が塊を持つ間、その糸の出力は塊へ積む。**項目が終わったら、
//! 塊をまとめて書く**（終わった順に塊が並ぶ）。**塊を持たない糸は、今までどおりその場で書く**——順に回す検査の
//! 出力は変わらない。
//!
//! **出力の入口は、クレートの根で置き換えた `print!`・`println!`・`eprint!`・`eprintln!` である**（`main.rs` の
//! モジュールの宣言より前に置いて、全部のモジュールに効かせた）。**標準出力と標準エラーは、塊の中では書いた順に
//! 1 つに並ぶ**（全検査のログは、どちらも同じファイルへ書かれている）。
//!
//! **子のプロセス（QEMU・cargo）の出力も、塊を持つ糸から起こしたときは受け取って塊へ積む**——受け継ぐと、ほかの項目の
//! 塊の間に混ざる。子の出力を読む糸へは [`sink`] を渡す（[`append_to`]）。

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// 塊の中身（標準出力と標準エラーを、書いた順に積む）。
pub type Sink = Arc<Mutex<String>>;

std::thread_local! {
    /// いまの糸の塊（**持たなければ `None`**——その場で書く）。
    static BLOCK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// 標準出力へ書く（`print!`・`println!` の置き換えが呼ぶ）。**塊を持つ糸なら、塊へ積む。**
pub fn out(text: &str) {
    if !append(text) {
        std::print!("{text}");
    }
}

/// 標準エラーへ書く（`eprint!`・`eprintln!` の置き換えが呼ぶ）。**塊を持つ糸なら、塊へ積む。**
pub fn err(text: &str) {
    if !append(text) {
        std::eprint!("{text}");
    }
}

/// 塊を持つ糸なら積んで真を返す。
fn append(text: &str) -> bool {
    BLOCK.with(|block| match &*block.borrow() {
        Some(sink) => {
            append_to(sink, text);
            true
        }
        None => false,
    })
}

/// いまの糸の塊（**子の出力を読む糸へ渡す**。持たなければ `None`——子は出力を受け継ぐ）。
pub fn sink() -> Option<Sink> {
    BLOCK.with(|block| block.borrow().clone())
}

/// 塊へ積む（子の出力を読む糸が呼ぶ）。
pub fn append_to(sink: &Sink, text: &str) {
    if let Ok(mut sink) = sink.lock() {
        sink.push_str(text);
    }
}

/// 子の出力を 1 行ずつ読み、塊へ積む糸を起こす（**子が出力を閉じたら終わる**）。
pub fn forward(from: impl Read + Send + 'static, sink: Sink) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(from);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => append_to(&sink, &String::from_utf8_lossy(&line)),
            }
        }
    })
}

/// 塊を始める（並べた行を走らせる糸が、行の始めに呼ぶ。`crate::batch`）。
pub fn begin() {
    BLOCK.with(|block| *block.borrow_mut() = Some(Arc::new(Mutex::new(String::new()))));
}

/// 塊を終え、中身を返す（**持っていなければ `None`**）。
pub fn take() -> Option<String> {
    let sink = BLOCK.with(|block| block.borrow_mut().take())?;
    let text = sink
        .lock()
        .map(|mut text| std::mem::take(&mut *text))
        .unwrap_or_default();
    Some(text)
}

/// 塊をまとめて標準出力へ書く（**ほかの糸の出力と、塊の途中で混ざらない**——標準出力の錠を持って 1 度に書く）。
pub fn write(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// 塊を持つ間に `body` を走らせ、塊の中身を返す（テストの中だけで使う）。
#[cfg(test)]
pub fn with_block(body: impl FnOnce()) -> String {
    begin();
    body();
    take().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **塊を持つ間は、標準出力と標準エラーの行を書いた順に積む。** **終えたら、その糸は塊を持たない。**
    #[test]
    fn a_block_keeps_both_streams_in_the_order_they_were_written() {
        assert!(sink().is_none());
        let block = with_block(|| {
            out("=== one\n");
            err("warning: two\n");
            out("--- three: OK\n");
        });
        assert_eq!(block, "=== one\nwarning: two\n--- three: OK\n");
        assert!(sink().is_none());
    }

    /// **子の出力を読む糸は、渡された塊へ積む**（子が出力を閉じたら終わる）。
    #[test]
    fn a_child_output_is_forwarded_into_the_block() {
        let block = with_block(|| {
            let sink = sink().unwrap();
            let reader = forward(&b"qemu: warning: one\nqemu: warning: two\n"[..], sink);
            reader.join().unwrap();
        });
        assert_eq!(block, "qemu: warning: one\nqemu: warning: two\n");
    }
}
