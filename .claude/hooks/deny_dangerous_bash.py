#!/usr/bin/env python3
"""PreToolUse(Bash) で危ない形を拒む（§6-C の実測用）。

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

if __name__ == "__main__":
    raise SystemExit(main())
