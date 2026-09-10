#!/usr/bin/env python3
"""段で偽になった決定の候補を出す（2026-09-10）。

**段を締めるときの道具である**（`.claude/skills/record-a-decision/SKILL.md` の
「段を締めるときは、その段で偽になった決定を当てる」）。

**何も主張しない。** **出るのは候補で、当てるか決めるのは人である**
（`tools/judgement-map.py` と同じ立ち位置）。

# 何を見るか

**その段が「消した語」を指している ADR を挙げる。** 決定が偽になるのは、
**その決定が名指ししているものが消えたときだからである**——旗、定数、関数の名前。

**実測で決めた形である**（2026-09-10）。**触ったファイルで引く形も試したが、
20 ファイルの段で 58 本中 14 本が当たった**（`kernel/src/main.rs` のような
大きいファイルを、多くの ADR が指しているためである）。**消した語で引くと 3 本で、
1 本目が探していた ADR-0057 だった**（`-mno-sse` ほか 3 語が一致）。

# 使い方

    python3 tools/decisions-touched.py <range>

`<range>` は `git diff` が受ける形（`54e3101..e1348d7` など）。
"""
import glob
import os
import re
import subprocess
import sys

# 拾う語の形。**旗・大文字の定数・`zt_` 等の接頭辞つき関数**。
# **短い語は拾わない**——普通の英単語と当たる。
TOKEN_PATTERNS = [
    r'"(-[A-Za-z0-9-]{3,})"',
    r"\b([A-Z][A-Z0-9_]{4,})\b",
    r"\b((?:zt|stbtt)_[a-z0-9_]{3,})\b",
]


def removed_tokens(commit_range: str) -> set:
    """その範囲で消えた行から、特徴のある語を拾う。"""
    diff = subprocess.run(
        ["git", "diff", "-U0", commit_range, "--", "*.rs", "*.c", "*.h", "*.toml", "*.ld"],
        capture_output=True,
        text=True,
        check=False,
    ).stdout
    tokens = set()
    for line in diff.split("\n"):
        if not line.startswith("-") or line.startswith("---"):
            continue
        for pattern in TOKEN_PATTERNS:
            tokens |= set(re.findall(pattern, line))
    return tokens


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    tokens = removed_tokens(sys.argv[1])
    print(f"消えた語: {len(tokens)}")
    hits = {}
    for adr in sorted(glob.glob("docs/adr/*.md")):
        text = open(adr, encoding="utf-8").read()
        common = sorted(token for token in tokens if token in text)
        if common:
            hits[os.path.basename(adr)] = common
    if not hits:
        print("消えた語を指している ADR は無い（当てる先が無いとは限らない）")
        return 0
    print(f"当たった ADR: {len(hits)}（一致した語の多い順）")
    for name, common in sorted(hits.items(), key=lambda kv: -len(kv[1])):
        print(f"  {name}: {', '.join(common[:6])}")
    print()
    print("**候補である。** 読んで、決定が偽になったかを人が決めること。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
