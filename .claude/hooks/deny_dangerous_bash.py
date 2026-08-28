#!/usr/bin/env python3
"""PreToolUse(Bash) で危ない形を拒む（`CLAUDE.md` の「バックグラウンド実行の手順」）。

**実行の形だけを見る。** 部分一致にすると、文書に書いた文字列まで拒む
（実測で踏んだ——この規則そのものを書く heredoc が拒まれた）。
"""
import json
import re
import sys

START = r"(?:^|[;&|]\s*|\n\s*)"
RULES = [
    (re.compile(START + r"git\s+add\s+(?:-A|--all)\b"), "deny",
     "HOOK-PROBE-ADD: 全部足す形は使わない。パスを明示すること"),
    (re.compile(START + r"git\s+commit\s+(?:-[a-zA-Z]*a[a-zA-Z]*|--all)\b"), "exit2",
     "HOOK-PROBE-EXIT2: commit の -a は使わない"),
    (re.compile(r"(?<!&)&\s*$"), "deny",
     "HOOK-PROBE-AMP: 行末の & は使わない。run_in_background を使うこと"),
]

def main() -> int:
    try:
        command = json.load(sys.stdin).get("tool_input", {}).get("command", "")
    except Exception:
        return 0
    for pattern, how, reason in RULES:
        if not pattern.search(command):
            continue
        if how == "deny":
            print(json.dumps({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }}))
            return 0
        print(reason, file=sys.stderr)
        return 2
    return 0

def self_test() -> int:
    """判定表を回す（`cargo xtask check` から呼ぶ）。

    **hook が読み込まれているかは、ここでは分からない**——**ツールの
    呼び出しを止めるのは harness の側で、こちらからは観測できない。**
    **ここで守れるのは「判定そのものが壊れていないこと」だけである。**

    **読み込まれていることは、実際に打って確かめるしかない**
    （`CLAUDE.md` の規律。**実測で2回、素通りする形を踏んでいる**）。
    """
    cases = [
        ("sleep 1 &", "deny"),
        ("ls && pwd", "allow"),
        ("git " + "add -A", "deny"),
        ("git " + "add --all .", "deny"),
        ("echo '`git " + "add -A` は使わない'", "allow"),
        ("git " + "commit -am x", "exit2"),
        ("git " + "commit -m x", "allow"),
        ("git status --short", "allow"),
        ("cat a.txt | grep x", "allow"),
    ]
    failures = 0
    for command, want in cases:
        got = "allow"
        for pattern, how, _ in RULES:
            if pattern.search(command):
                got = "deny" if how == "deny" else "exit2"
                break
        if got != want:
            print(f"self-test: {command!r} wanted {want} but got {got}")
            failures += 1
    if failures:
        return 1
    print(f"self-test: {len(cases)} case(s) decided as expected")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--self-test":
        raise SystemExit(self_test())
    raise SystemExit(main())
