#!/usr/bin/env python3
"""起動ログの2つを、番地を伏せて突き合わせる（2026-09-13）。

**振る舞いを変えない段で使う道具である**（W1-a で要った。`ADR-0060`）。

**何も主張しない。** **出るのは数と行で、読むのは人である**
（`tools/judgement-map.py` と `tools/decisions-touched.py` と同じ立ち位置）。
**`cargo xtask check` はこれを回さない。**

# なぜ要るのか

**`cargo xtask run --boot-log-diff` は「一致するか」しか言わない。**
**振る舞いを変えない段では、番地は必ず動く**——**コードの大きさが変われば
すべての番地がずれる。** **したがって「差分が空」は最初から成り立たない主張で、
成り立たない主張は読まれない。**

**要るのは「番地以外が動いていないこと」である。**

**W1-a で実際に効いた。** **番地を伏せると 6 行が残り、遠征スタックの高水位が
16 バイト増えていた**（28,840 → 28,856。%は同じ）。**`--full` の 296 項目は
どれもこれを見ておらず、全部緑だった。** **原因は `state()` の呼び出しが 1 段
増えたことで、カーネルは `dev` で建てるので `#[inline]` が効かない。**
**`#[inline(always)]` にしたら消えた。**

# 伏せる規則（ここが 1 箇所である）

**毎回手で作ると、伏せる範囲がぶれる。** **ぶれると捕まるものが変わる。**

| 伏せる | 伏せない |
|---|---|
| **16 進の数**（`0x` で始まるもの） | **10 進の数**（大きさ・回数・割合） |
| ——— | **`=true` / `=false` の判定値** |
| ——— | **行の並びと本数** |

**10 進を伏せない理由が、この道具の目的そのものである**——**高水位も、空き
ブロック数も、システムコールの回数も 10 進で出る。** **あれが動いたなら、
振る舞いが動いている。**

**`--boot-log-diff` が行ごと落とす揺れる行**（`BOOT_LOG_VOLATILE_MARKERS`）
**は、こちらでも落とす。** **落とす一覧は `xtask/src/main.rs` から読む**
——**写すと片方だけが古くなる。**

# 使い方

    python3 tools/boot-log-compare.py <古い方> <新しい方>

**参照と突き合わせるなら、古い方に `xtask/reference/boot-log-smp2.txt` を、
新しい方に `target/boot-log-smp2.log` を渡す。**
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

#: 16 進の数を伏せる（上の表）。
HEX = re.compile(r"0x[0-9a-fA-F]+")


def volatile_markers() -> list[str]:
    """`xtask` が行ごと落としている標識を読む。**写さない。**"""
    source = (ROOT / "xtask" / "src" / "main.rs").read_text(encoding="utf-8")
    start = source.find("const BOOT_LOG_VOLATILE_MARKERS: &[&str] = &[")
    if start < 0:
        raise SystemExit(
            "xtask/src/main.rs に BOOT_LOG_VOLATILE_MARKERS が見つからない。"
            "名前が変わったなら、この道具も直すこと"
        )
    end = source.find("];", start)
    body = source[start:end].split("= &[", 1)[1]
    found = re.findall(r'"((?:[^"\\]|\\.)*)"', body)
    return [marker.encode().decode("unicode_escape") for marker in found]


def load(path: Path, markers: list[str]) -> list[str]:
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = [line for line in text.split("\n") if not any(m in line for m in markers)]
    while lines and lines[-1] == "":
        lines.pop()
    return lines


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__.strip().split("# 使い方", 1)[1].strip())
        return 2
    markers = volatile_markers()
    old = load(Path(sys.argv[1]), markers)
    new = load(Path(sys.argv[2]), markers)

    print(f"古い方 {len(old)} 行 / 新しい方 {len(new)} 行")
    if len(old) != len(new):
        print("**行数が違う。** 番地の話ではない——行が増えたか減っている")

    differ = [(a, b) for a, b in zip(old, new) if a != b]
    masked = [(a, b) for a, b in differ if HEX.sub("0xADDR", a) != HEX.sub("0xADDR", b)]
    print(f"食い違う行 {len(differ)}")
    print(f"**番地を伏せても食い違う行 {len(masked)}**")
    if len(old) == len(new) and not masked:
        if differ:
            print("——番地だけが動いている")
        else:
            print("——1 行も動いていない")
    for before, after in masked:
        print(f"  旧: {before}")
        print(f"  新: {after}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
