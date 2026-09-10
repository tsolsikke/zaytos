#!/usr/bin/env python3
"""PostToolUse(Bash) で、コミットの直後に基底 check を回す。

**規律が「コミット直後に基底 check」と書いてある**（`CLAUDE.md` の
「コミットの運用」）。**忘れても気づけないので、機械で回す。**

**実行の形だけを見る。** 部分一致にすると、文書に書いた文字列で発火する
（`deny_dangerous_bash.py` が実測で踏んだ形である）。

**限界が 2 つある。**

- **温まっていれば 1.5 秒だが、冷えていればビルドの時間が乗る**
  （実測。温まった状態で 1.46 / 1.47 / 1.48 秒）。
  **harness 側の上限で先に切られることがある。**
- **これは「コミットの後」であって「前」ではない。** 落ちたら `--amend` か
  次のコミットで直す。**前で止める形は、コミットの経路を hook から
  横取りすることになるので採らない。**
"""
import json
import os
import re
import subprocess
import sys

COMMIT = re.compile(r"(?:^|[;&|]\s*|\n\s*)git\s+commit\b")
TIMEOUT_SECONDS = 240


def looks_like_a_commit(command: str) -> bool:
    """発火するかを決める。**ここだけが判定であり、self-test が覆う。**"""
    return bool(COMMIT.search(command))


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except Exception as error:
        # **黙って通さない。** **読めなければ基底 check は走っていない**ので、
        # **その事実を言って止める**（`docs/verification-coverage.md` の
        # 「失敗を空に落とす形」。2026-09-10 に Python 側を洗って直した）。
        print(
            f"post-commit check: 入力が読めなかった（{error}）。基底 check は走っていない",
            file=sys.stderr,
        )
        return 2
    command = payload.get("tool_input", {}).get("command", "")
    if not looks_like_a_commit(command):
        return 0
    root = os.environ.get("CLAUDE_PROJECT_DIR") or payload.get("cwd") or "."
    try:
        done = subprocess.run(
            ["cargo", "xtask", "check"],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=TIMEOUT_SECONDS,
        )
    except Exception as error:
        print(f"post-commit check: 走らせられなかった（{error}）", file=sys.stderr)
        return 2
    if done.returncode == 0:
        summary = [l for l in done.stdout.splitlines() if "check(s) passed" in l]
        print("post-commit check: " + (summary[-1] if summary else "OK"))
        return 0
    # **落ちた行と、その所見だけを出す。** 通った項目まで出すと、
    # **落ちた1行が20行の緑に埋もれる**（実測で埋もれた）。
    detail = [
        line
        for line in done.stdout.splitlines()
        if "FAILED" in line or (line.startswith("    ") and line.strip())
    ]
    reasons = [
        line
        for line in done.stderr.splitlines()
        if line.startswith("Error:") or line.startswith("error")
    ]
    print("post-commit check: 落ちた（コミットの直後の基底 check）。", file=sys.stderr)
    for line in (detail + reasons)[:20]:
        print("  " + line.rstrip(), file=sys.stderr)
    return 2


def self_test() -> int:
    """判定表を回す（`cargo xtask check` から呼ぶ）。

    **見るのは「どの形で発火するか」だけである。**
    **実際に check が回ることは、実際にコミットして確かめるしかない**
    ——**hook は動かなくてもエラーを出さない。**
    """
    cases = [
        ("git " + "commit -m x", True),
        ("git " + "commit --amend --no-edit", True),
        ("cd /x && git " + "commit -F -", True),
        ("git status --short", False),
        ("git " + "log --oneline -3", False),
        ("echo '`git " + "commit` の話'", False),
        ("cargo xtask check", False),
    ]
    failures = 0
    for command, want in cases:
        got = looks_like_a_commit(command)
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
