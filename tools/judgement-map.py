#!/usr/bin/env python3
"""`cargo xtask check --full` の出力から、判定ごとに「落とす破壊」を並べる。

破壊の表（`docs/verification-coverage.md`）は「破壊 → 落ちる判定」の向きで、
逆向きは書いていない。**落とす破壊が無い判定は、嘘でなくても「働いているか
分からない」**——この道具はその一覧を出す。

使い方:

    cargo xtask check --full > /tmp/full.txt 2>&1
    python3 tools/judgement-map.py /tmp/full.txt

**追加の起動は要らない。** `--full` はどのみち通すので、その出力を取っておけばよい。

読む行の形は `<文脈>: <判定> = <真偽>` である（`xtask` の判定行）。
`(info)` で始まる行は数えない——あちらは主張を持たない計器である。
"""

import re
import sys
from collections import defaultdict

LINE = re.compile(r"^(?P<context>[^:]+): (?P<name>.+?) = (?P<value>true|false)\b")

# **カーネルの行を数えない。** `xtask` はシリアルの一部をそのまま印字するので、
# **`[INFO] spawn: ... = true` のような行が同じ形に見える**（実測。2026-09-04。
# **最初に書いた版は判定を366本と数え、その大半がカーネルの行だった**）。
#
# **判定行の文脈は `xtask` が組み立てたもので、`[` も `/` も含まない**
# （`zi-test`・`utf8-test zi-line-end-stays-test` のような形である）。
def is_a_judgement(line: str, context: str) -> bool:
    if line.startswith("[") or line.startswith(" ") or "\x1b" in line:
        return False
    if "[" in context or "/" in context or len(context) > 60:
        return False
    return True



def main(path: str) -> int:
    falls: dict[str, set[str]] = defaultdict(set)
    holds: dict[str, set[str]] = defaultdict(set)
    for raw in open(path, encoding="utf-8", errors="replace"):
        match = LINE.match(raw.rstrip("\n"))
        if not match:
            continue
        name = match.group("name")
        # **`(info)` は主張しない計器、`(signal)` は合図である。**
        # **合図は「走ったこと」の確認で、偽なら台本ごと動かない**
        # ——**「これだけが落ちる破壊」は在りえないので、弱い判定として
        # 数えると一覧が読めなくなる**（運用者の指摘。2026-09-04）。
        # **合否には載っている。読み方だけを分けている。**
        if name.startswith("(info)") or name.startswith("(signal)"):
            continue
        context = match.group("context")
        if not is_a_judgement(raw, context):
            continue
        if match.group("value") == "false":
            falls[name].add(context)
        else:
            holds[name].add(context)

    seen = sorted(set(falls) | set(holds))
    never = [name for name in seen if not falls[name]]

    print(f"判定の本数（名前で数えた）: {len(seen)}")
    print(f"どこかで偽になった判定: {len(seen) - len(never)}")
    print(f"一度も偽にならなかった判定: {len(never)}")
    print()
    print("=== 一度も偽にならなかった判定 ===")
    for name in never:
        print(f"  {name}  （真だった構成 {len(holds[name])} 個）")
    print()
    print("=== 偽になった判定と、落とした構成 ===")
    for name in seen:
        if not falls[name]:
            continue
        contexts = ", ".join(sorted(falls[name]))
        print(f"  {name}")
        print(f"      {contexts}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        sys.exit(2)
    try:
        sys.exit(main(sys.argv[1]))
    except BrokenPipeError:
        # **`head` へ繋いだときに出る。** **道具の誤りではないので、
        # 追跡を出さずに終える。**
        sys.exit(0)
