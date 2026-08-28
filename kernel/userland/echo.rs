//! `echo`: 引数を空白で区切って書き、改行で終える（SE-e）。
//!
//! # crate ではない
//!
//! `cat.rs` と同じで、cargo のパッケージに属さない（`userlib.rs` の doc）。
//!
//! # 外部である（`ADR-0043` の基準）
//!
//! **基準は「シェル自身の状態を変えるものだけが組み込み」である。**
//! **`echo` はシェルの状態を1つも変えない**——受け取った `argv` を書いて終わる。
//! **`bash` は組み込みに持つが、あちらの理由は速さである**（`ADR-0043` の理由(2)）。
//! **この体制は速さを理由に採らない。** 判断と理由は `ADR-0043` の Addendum にある。
//!
//! # `$VAR` はここでは展開しない
//!
//! **展開はシェルが行う**（`ADR-0049`）。**`echo` に値へ変える手段が無い**
//! ——**`argv` を受け取った時点で `$PATH` はただの5バイトである。**
//!
//! # `-n` は作らない
//!
//! **利用者が言っていない。** **先回りして作らない**（`CLAUDE.md` の
//! 「スコープ外」の姿勢）。**要る場面が出たら足す。**
//! **いま足すと、`echo -n` と打つ人が居ないまま、旗の解釈という層が1つ増える**
//! ——**`--` の扱いや「`-n` という文字列を書きたい場合」まで決めることになる。**
//!
//! # 出力は1回にまとめる
//!
//! **語ごとに `write` を出すと、`echo a b c` で6回になる。**
//! **溜めてから1回で出す**（PERF 段が減らした側と同じ考えである）。
//! **溜めきれない長さが来たら、その時点で出して続ける。**
//!
//! # 終了状態の意味
//!
//! - `0` 常に。**書けなかったことは報せない**——**報せる先も標準出力である。**

#![no_std]
#![no_main]

#[path = "userlib.rs"]
mod userlib;

use userlib::{exit, length_of, write_all, STDOUT};

/// 溜める大きさ。
///
/// **`zash` の1行は128バイトなので、いまの経路ではここへ届かない**
/// （`kernel/userland/zash.rs` の `LINE_MAX`）。**それでも溢れを扱う**
/// ——**この形が「届かないから書かない」で壊れるのは、別の呼び手が来た日である。**
const OUT_MAX: usize = 256;

/// 1つの引数として読む上限。**`OUT_MAX` と同じにする理由は無い**ので別に持つ。
const ARG_MAX: usize = 4096;

/// 溜めて、溢れたら出す。
fn push(out: &mut [u8; OUT_MAX], used: &mut usize, bytes: &[u8]) {
    for chunk in bytes.chunks(OUT_MAX) {
        if *used + chunk.len() > OUT_MAX {
            write_all(STDOUT, &out[..*used]);
            *used = 0;
        }
        out[*used..*used + chunk.len()].copy_from_slice(chunk);
        *used += chunk.len();
    }
}

/// `_start` から呼ばれる（`userlib.rs` の `global_asm!`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
#[no_mangle]
pub unsafe extern "sysv64" fn zaytos_main(stack: *const u64) -> ! {
    let mut out = [0u8; OUT_MAX];
    let mut used = 0usize;
    let mut index = 1usize;

    // **`argv` は NULL で終わる。** `argument` がそこで `None` を返す。
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    while let Some(pointer) = (unsafe { userlib::argument(stack, index) }) {
        if index > 1 {
            // **区切りは空白1つである。** **語の数だけ空白が入る**ので、
            // **空の語が来れば空白が2つ並ぶ**——`ADR-0049` は、展開で
            // 丸ごと空になった語をシェルが落とすと決めている。
            push(&mut out, &mut used, b" ");
        }
        // SAFETY: `argv` の要素はカーネルが NUL 終端で積んだ文字列である。
        let length = unsafe { length_of(pointer, ARG_MAX) };
        // SAFETY: 上で数えた長さは NUL の手前までで、同じ割り当ての中である。
        let bytes = unsafe { core::slice::from_raw_parts(pointer, length) };
        push(&mut out, &mut used, bytes);
        index += 1;
    }

    // **引数が無くても改行は出す。** `echo` だけを打つと空行が出る。
    push(&mut out, &mut used, b"\n");
    write_all(STDOUT, &out[..used]);
    exit(0);
}
