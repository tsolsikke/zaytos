//! `--shell-test` の台本と、それをバイトへ写す規則（`ADR-0063` の (b3) の (b)）。
//!
//! # なぜここに在るのか
//!
//! **同じ台本を 2 つの駆動で使う**——**`xtask` が QEMU monitor の `sendkey` で打つ形と、
//! カーネルが台本として `read(0)` へ差し込む形である。** **打つ側と読む側の両方が同じ
//! 一覧を見るので、`common` に置く**（`xtask` と `kernel` の両方が依存している）。
//!
//! **目的は `--full` の余裕である**——**打鍵を見ない破壊を、1 本あたり約 50 秒縮む台本の族へ
//! 移す**（`--shell-test` の破壊は 58 秒、台本の族は 8.5 秒。実測。`ADR-0063` の決定 4）。
//!
//! # 写す規則
//!
//! **キー名は QEMU monitor のものである**（`slash` / `spc` / `minus` / `ret` …）。**バイトは
//! カーネルのデコーダが同じキーに出すものと同じにする**（`kernel/src/input.rs` の
//! `bytes_for_event` と、`kernel/src/keyboard/decode.rs` の JIS の表）。**配列は JIS である**
//! （`--shell-test` の既定と同じ）。
//!
//! **キーごとに 1 回の `read` を空にする**（[`PAUSE`]）。**`sendkey` は 32 ミリ秒おきに打つので、
//! シェルは「1 バイト、待ち、1 バイト」と読む**——**台本でも同じ形にしないと、Esc の確定
//! （次の `read` が空なら単体の Esc）が変わる。**
//!
//! # 台本では打てない行
//!
//! **Ctrl+C の畳みは IRQ の経路である**（旗を立てるのは IRQ1 のハンドラで、台本は割り込みを
//! 起こさない）。**`spin` と、それを止める `ctrl-c` の 2 行は台本から外す**（[`Line::keystrokes_only`]）。
//! **外した行に寄りかかる判定は、判定の側が「台本では見ない」と印を付ける**
//! （`xtask` の `judge_shell_session`）。
//!
//! **`tab` は何も出さない**——**実打鍵では入力の輪が捨てる**（`ADR-0050`）**ので、台本でも
//! 届けない。** **`ctrl_r-c`（右 Ctrl の C）は `0x03` のバイトだけである**——**深さ 1 の
//! シェルはバイトで行を捨てるので、台本でも同じに動く。**

/// 台本の 1 行。
pub struct Line {
    /// QEMU monitor のキー名の並び。
    pub keys: &'static [&'static str],
    /// 実打鍵でしか意味を持たない行（IRQ の経路が要る）。**台本では外す。**
    pub keystrokes_only: bool,
}

const fn line(keys: &'static [&'static str]) -> Line {
    Line {
        keys,
        keystrokes_only: false,
    }
}

const fn keystrokes_only(keys: &'static [&'static str]) -> Line {
    Line {
        keys,
        keystrokes_only: true,
    }
}

/// `LINE_MAX` ちょうどの行を打つ本数（SE-c）。
pub const LONG_LINE_KEYS: usize = 128;

/// `z` を 128 個打って Enter（SE-c。`d7de0ce` の修正に判定を置く）。
const LONG_LINE: [&str; LONG_LINE_KEYS + 1] = {
    let mut keys = ["z"; LONG_LINE_KEYS + 1];
    keys[LONG_LINE_KEYS] = "ret";
    keys
};

/// `--shell-test` が打つ行（S11-11。S12 前の手当ての 3 本目で伸ばした）。
///
/// **順序に意味がある。** **`/` を含む側を先に打つ**（あちらは 3 本目より前から通っていた
/// 道なので、固定の既定を入れて壊れていないことを先に見る）。**そのあと `/` を含まない側を
/// 打つ。** **`echo` の行の順は `xtask` の `ECHO_LINES_IN_ORDER` と対になっている**——
/// **足すときは両方へ足すこと**（本数を突き合わせる判定が在る）。
///
/// **各行の意味は `xtask` 側の判定の doc にある。** **ここには行そのものだけを置く。**
pub const LINES: &[Line] = &[
    // /bin/ls
    line(&["slash", "b", "i", "n", "slash", "l", "s", "ret"]),
    // /bin/cat /etc/motd
    line(&[
        "slash", "b", "i", "n", "slash", "c", "a", "t", "spc", "slash", "e", "t", "c", "slash",
        "m", "o", "t", "d", "ret",
    ]),
    // /bin/hello
    line(&[
        "slash", "b", "i", "n", "slash", "h", "e", "l", "l", "o", "ret",
    ]),
    // /bin/chello（C-a。`ADR-0057`）
    line(&[
        "slash", "b", "i", "n", "slash", "c", "h", "e", "l", "l", "o", "ret",
    ]),
    // ls
    line(&["l", "s", "ret"]),
    // cat /etc/motd
    line(&[
        "c", "a", "t", "spc", "slash", "e", "t", "c", "slash", "m", "o", "t", "d", "ret",
    ]),
    // spawn-test beta
    line(&[
        "s", "p", "a", "w", "n", "minus", "t", "e", "s", "t", "spc", "b", "e", "t", "a", "ret",
    ]),
    // abc → Backspace → x
    line(&["a", "b", "c", "backspace", "x", "ret"]),
    // pq → 左 → y
    line(&["p", "q", "left", "y", "ret"]),
    // m → Esc → [ → D → n（zi-a）
    line(&["m", "esc", "bracket_right", "shift-d", "n", "ret"]),
    // ろ → ¥ → Enter（zi-e）
    line(&["ro", "yen", "ret"]),
    // a → b → Home → c → End → d（SE-b）
    line(&["a", "b", "home", "c", "end", "d", "ret"]),
    // e → f → Ctrl+A → g → Ctrl+E → h（SE-b）
    line(&["e", "f", "ctrl-a", "g", "ctrl-e", "h", "ret"]),
    // i → j → 左 → Delete（SE-b）
    line(&["i", "j", "left", "delete", "ret"]),
    // k → Tab → l（SE-b）
    line(&["k", "tab", "l", "ret"]),
    // n → Ctrl+G → o（SE-b）
    line(&["n", "ctrl-g", "o", "ret"]),
    // echo $PATH
    line(&[
        "e", "c", "h", "o", "spc", "shift-4", "shift-p", "shift-a", "shift-t", "shift-h", "ret",
    ]),
    // echo a $UNSET b
    line(&[
        "e", "c", "h", "o", "spc", "a", "spc", "shift-4", "shift-u", "shift-n", "shift-s",
        "shift-e", "shift-t", "spc", "b", "ret",
    ]),
    // echo $UNSET
    line(&[
        "e", "c", "h", "o", "spc", "shift-4", "shift-u", "shift-n", "shift-s", "shift-e",
        "shift-t", "ret",
    ]),
    // echo a$TERM b
    line(&[
        "e", "c", "h", "o", "spc", "a", "shift-4", "shift-t", "shift-e", "shift-r", "shift-m",
        "spc", "b", "ret",
    ]),
    // echo ~
    line(&["e", "c", "h", "o", "spc", "shift-equal", "ret"]),
    // echo ~/x
    line(&[
        "e",
        "c",
        "h",
        "o",
        "spc",
        "shift-equal",
        "slash",
        "x",
        "ret",
    ]),
    // echo a~b
    line(&["e", "c", "h", "o", "spc", "a", "shift-equal", "b", "ret"]),
    // echo @+
    line(&[
        "e",
        "c",
        "h",
        "o",
        "spc",
        "bracket_left",
        "shift-semicolon",
        "ret",
    ]),
    // echo $ZF2（打つ前）
    line(&[
        "e", "c", "h", "o", "spc", "shift-4", "shift-z", "shift-f", "2", "ret",
    ]),
    // export ZF2=exported
    line(&[
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "spc",
        "shift-z",
        "shift-f",
        "2",
        "shift-minus",
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "e",
        "d",
        "ret",
    ]),
    // echo $ZF2（打った後）
    line(&[
        "e", "c", "h", "o", "spc", "shift-4", "shift-z", "shift-f", "2", "ret",
    ]),
    // set
    line(&["s", "e", "t", "ret"]),
    // export 1BAD=x
    line(&[
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "spc",
        "1",
        "shift-b",
        "shift-a",
        "shift-d",
        "shift-minus",
        "x",
        "ret",
    ]),
    // export PATH=/nope
    line(&[
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "spc",
        "shift-p",
        "shift-a",
        "shift-t",
        "shift-h",
        "shift-minus",
        "slash",
        "n",
        "o",
        "p",
        "e",
        "ret",
    ]),
    // hello（見つからない）
    line(&["h", "e", "l", "l", "o", "ret"]),
    // export PATH=/bin
    line(&[
        "e",
        "x",
        "p",
        "o",
        "r",
        "t",
        "spc",
        "shift-p",
        "shift-a",
        "shift-t",
        "shift-h",
        "shift-minus",
        "slash",
        "b",
        "i",
        "n",
        "ret",
    ]),
    // hello（見つかる）
    line(&["h", "e", "l", "l", "o", "ret"]),
    // aa / bb → 上 上（SE-c）
    line(&["a", "a", "ret"]),
    line(&["b", "b", "ret"]),
    line(&["up", "up", "ret"]),
    // cc / dd → Ctrl+P Ctrl+P Ctrl+N（SE-c）
    line(&["c", "c", "ret"]),
    line(&["d", "d", "ret"]),
    line(&["ctrl-p", "ctrl-p", "ctrl-n", "ret"]),
    // r s → Ctrl+B → t → Ctrl+F → u（SE-c）
    line(&["r", "s", "ctrl-b", "t", "ctrl-f", "u", "ret"]),
    // `LINE_MAX` ちょうどの行（SE-c）
    line(&LONG_LINE),
    // echo qq rr → Ctrl+W（SE-f）
    line(&[
        "e", "c", "h", "o", "spc", "q", "q", "spc", "r", "r", "ctrl-w", "ret",
    ]),
    // echo $1（SE-f）
    line(&["e", "c", "h", "o", "spc", "shift-4", "1", "ret"]),
    // kkxx → Ctrl+A → Ctrl+F Ctrl+F → Ctrl+K（SE-f）
    line(&[
        "k", "k", "x", "x", "ctrl-a", "ctrl-f", "ctrl-f", "ctrl-k", "ret",
    ]),
    // uuyy → Ctrl+A → Ctrl+F Ctrl+F → Ctrl+U（SE-f）
    line(&[
        "u", "u", "y", "y", "ctrl-a", "ctrl-f", "ctrl-f", "ctrl-u", "ret",
    ]),
    // mxmy → Ctrl+A → Ctrl+F → Ctrl+D（SE-f）
    line(&["m", "x", "m", "y", "ctrl-a", "ctrl-f", "ctrl-d", "ret"]),
    // Ctrl+D → ee（SE-f。空の行では何も起きない）
    line(&["ctrl-d", "e", "e", "ret"]),
    // ll → Ctrl+L（SE-f）
    line(&["l", "l", "ctrl-l", "ret"]),
    // st → 上 → Ctrl+N（SE-f。打ちかけの行が戻る）
    line(&["s", "t", "up", "ctrl-n", "ret"]),
    // oo / pp / pp → 上 上（SE-c の宿題。同じ行が 2 つ並ばない）
    line(&["o", "o", "ret"]),
    line(&["p", "p", "ret"]),
    line(&["p", "p", "ret"]),
    line(&["up", "up", "ret"]),
    // 打ちかけの行を Ctrl+C で捨てる（深さ 1 の側。バイトで動く）
    line(&["z", "z", "ctrl_r-c", "ret"]),
    // 回り続ける子を Ctrl+C で止める（深さ 2 の側。IRQ の畳みが要る）
    keystrokes_only(&["s", "p", "i", "n", "ret"]),
    keystrokes_only(&["ctrl-c"]),
    // sleep 1（W2-d+）
    line(&["s", "l", "e", "e", "p", "spc", "1", "ret"]),
    // exit
    line(&["e", "x", "i", "t", "ret"]),
];

/// 1 回の `read` を空にする印（`kernel/src/input.rs` の `SCRIPT_PAUSE` と同じ値）。
pub const PAUSE: u8 = 0x04;

/// 台本の終わりの印（`kernel/src/input.rs` の `OBSERVE_DONE` と同じ値）。
pub const DONE: u8 = 0x0c;

/// 「次のバイトは字である」の逃げ（`kernel/src/input.rs` の `SCRIPT_LITERAL` と同じ値）。
///
/// **台本の観測記号は 0x01 や 0x0c など制御のバイトで、Ctrl+英字のバイトと衝突する**
/// （`ctrl-a` は 0x01、`ctrl-l` は 0x0c）。**実測で `ctrl-l` が「台本の終わり」と読まれて途中で
/// 切れた。** **どのキーも 0xFF は作らない**（非 ASCII の字は届けない）**ので、これを逃げにする。**
pub const LITERAL: u8 = 0xff;

/// 写したバイト列の上限。**実測で 1 KiB 台なので 8 倍の余裕である。**
pub const SCRIPT_CAP: usize = 8192;

/// 1 つのキーが出すバイト（最大 4）と、その長さ。
pub struct KeyBytes {
    pub bytes: [u8; 4],
    pub len: usize,
}

const fn one(byte: u8) -> KeyBytes {
    KeyBytes {
        bytes: [byte, 0, 0, 0],
        len: 1,
    }
}

const fn csi(tail: &[u8]) -> KeyBytes {
    let mut bytes = [0x1b, b'[', 0, 0];
    let mut i = 0;
    while i < tail.len() {
        bytes[2 + i] = tail[i];
        i += 1;
    }
    KeyBytes {
        bytes,
        len: 2 + tail.len(),
    }
}

const fn nothing() -> KeyBytes {
    KeyBytes {
        bytes: [0; 4],
        len: 0,
    }
}

const fn eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// `a` が `prefix` で始まるか。**const の中では範囲のスライスが使えないので、添字で見る。**
const fn starts_with(a: &[u8], prefix: &[u8]) -> bool {
    if a.len() < prefix.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if a[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// キー名をバイトへ写す。**知らない名前は const の評価で止まる**（写し忘れを建て時に捕まえる）。
///
/// **JIS の刻印**——`shift-4` は `$`、`shift-minus` は `=`、`shift-equal` は `~`、
/// `shift-semicolon` は `+`、`bracket_left` は `@`、`bracket_right` は `[`、`yen` と `ro` は
/// `\`（`kernel/src/keyboard/decode.rs` の `JIS_SHIFTED` / `JIS_UNSHIFTED` / `JIS_ONLY_KEYS_JIS`）。
pub const fn bytes_for_key(name: &str) -> KeyBytes {
    let n = name.as_bytes();
    if n.len() == 1 && ((n[0] >= b'a' && n[0] <= b'z') || (n[0] >= b'0' && n[0] <= b'9')) {
        return one(n[0]);
    }
    if n.len() == 7 && starts_with(n, b"shift-") {
        let c = n[6];
        if c >= b'a' && c <= b'z' {
            return one(c - b'a' + b'A');
        }
        if c == b'4' {
            return one(b'$');
        }
    }
    if n.len() == 6 && starts_with(n, b"ctrl-") {
        let c = n[5];
        if c >= b'a' && c <= b'z' {
            return one(c - b'a' + 1);
        }
    }
    if eq(n, b"ctrl_r-c") {
        return one(0x03);
    }
    if eq(n, b"spc") {
        return one(b' ');
    }
    if eq(n, b"slash") {
        return one(b'/');
    }
    if eq(n, b"minus") {
        return one(b'-');
    }
    if eq(n, b"ret") {
        return one(b'\n');
    }
    if eq(n, b"backspace") {
        return one(0x08);
    }
    if eq(n, b"esc") {
        return one(0x1b);
    }
    if eq(n, b"tab") {
        return nothing();
    }
    if eq(n, b"delete") {
        return csi(b"3~");
    }
    if eq(n, b"left") {
        return csi(b"D");
    }
    if eq(n, b"right") {
        return csi(b"C");
    }
    if eq(n, b"up") {
        return csi(b"A");
    }
    if eq(n, b"down") {
        return csi(b"B");
    }
    if eq(n, b"home") {
        return csi(b"1~");
    }
    if eq(n, b"end") {
        return csi(b"4~");
    }
    if eq(n, b"shift-minus") {
        return one(b'=');
    }
    if eq(n, b"shift-equal") {
        return one(b'~');
    }
    if eq(n, b"shift-semicolon") {
        return one(b'+');
    }
    if eq(n, b"bracket_left") {
        return one(b'@');
    }
    if eq(n, b"bracket_right") {
        return one(b'[');
    }
    if eq(n, b"yen") || eq(n, b"ro") {
        return one(b'\\');
    }
    panic!("a key name in shell_script::LINES has no byte mapping");
}

/// 台本をバイトへ写す。**`keystrokes_only` の行は外し、キーごとに [`PAUSE`] を置き、
/// 末尾に [`DONE`] を置く。**
pub const fn render() -> ([u8; SCRIPT_CAP], usize) {
    let mut out = [0u8; SCRIPT_CAP];
    let mut n = 0;
    let mut i = 0;
    while i < LINES.len() {
        let line = &LINES[i];
        if !line.keystrokes_only {
            let mut k = 0;
            while k < line.keys.len() {
                let key = bytes_for_key(line.keys[k]);
                let mut b = 0;
                while b < key.len {
                    let byte = key.bytes[b];
                    // **制御のバイトは逃げつきで写す**（改行と ESC は記号と衝突しないので素のまま）。
                    if byte < 0x20 && byte != b'\n' && byte != 0x1b {
                        out[n] = LITERAL;
                        n += 1;
                    }
                    out[n] = byte;
                    n += 1;
                    b += 1;
                }
                out[n] = PAUSE;
                n += 1;
                k += 1;
            }
        }
        i += 1;
    }
    out[n] = DONE;
    n += 1;
    (out, n)
}

const RENDERED: ([u8; SCRIPT_CAP], usize) = render();

/// 写した台本の長さ。
pub const SCRIPT_LEN: usize = RENDERED.1;

/// 長さちょうどの配列へもう一度写す（**const の中では `split_at` が使えない**）。
const SCRIPT_EXACT: [u8; SCRIPT_LEN] = {
    let mut out = [0u8; SCRIPT_LEN];
    let mut i = 0;
    while i < SCRIPT_LEN {
        out[i] = RENDERED.0[i];
        i += 1;
    }
    out
};

/// 写した台本。**カーネルが `shell-script-test` の `SCRIPT` として読む。**
pub const SCRIPT: &[u8] = &SCRIPT_EXACT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_name_in_the_lines_has_a_mapping_and_the_script_fits() {
        // **`render` が const で評価できた時点で写し忘れは無い。** 長さだけ見る。
        assert!(
            SCRIPT.len() < SCRIPT_CAP / 2,
            "the script is {} byte(s)",
            SCRIPT.len()
        );
        assert_eq!(*SCRIPT.last().unwrap(), DONE);
    }

    #[test]
    fn the_jis_legends_are_the_ones_the_kernel_decodes() {
        let one_byte = |name: &str| {
            let key = bytes_for_key(name);
            assert_eq!(key.len, 1, "{name}");
            key.bytes[0]
        };
        assert_eq!(one_byte("shift-4"), b'$');
        assert_eq!(one_byte("shift-minus"), b'=');
        assert_eq!(one_byte("shift-equal"), b'~');
        assert_eq!(one_byte("shift-semicolon"), b'+');
        assert_eq!(one_byte("bracket_left"), b'@');
        assert_eq!(one_byte("bracket_right"), b'[');
        assert_eq!(one_byte("yen"), b'\\');
        assert_eq!(one_byte("ro"), b'\\');
        assert_eq!(one_byte("ctrl-a"), 0x01);
        assert_eq!(one_byte("shift-p"), b'P');
        assert_eq!(bytes_for_key("tab").len, 0);
        let left = bytes_for_key("left");
        assert_eq!(&left.bytes[..left.len], b"\x1b[D");
    }

    #[test]
    fn the_keystroke_only_lines_are_left_out_and_every_key_pauses_once() {
        let text = SCRIPT;
        // `spin` を打つ行が無い。
        let spin = b"s\x04p\x04i\x04n\x04\n\x04";
        assert!(!text.windows(spin.len()).any(|w| w == spin));
        // 先頭の行 `/bin/ls` は、キーごとに 1 つの休止を挟む。
        assert!(text.starts_with(b"/\x04b\x04i\x04n\x04/\x04l\x04s\x04\n\x04"));
        // Ctrl+英字は逃げつきである（`ctrl-a` = 0x01 は観測記号と衝突する）。
        let ctrl_a = [LITERAL, 0x01, PAUSE];
        assert!(text.windows(ctrl_a.len()).any(|w| w == ctrl_a));
        // 逃げの無い制御のバイトは、改行と ESC だけである。
        let mut i = 0;
        while i < text.len() {
            let byte = text[i];
            if byte == LITERAL {
                i += 2;
                continue;
            }
            assert!(
                byte >= 0x20 || byte == b'\n' || byte == 0x1b || byte == PAUSE || byte == DONE,
                "unescaped control byte {byte:#04x} at {i}"
            );
            i += 1;
        }
        // Ctrl+C の畳みが要る行の数は 2 である。
        assert_eq!(LINES.iter().filter(|l| l.keystrokes_only).count(), 2);
    }
}
