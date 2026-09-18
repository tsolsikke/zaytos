#!/usr/bin/env python3
"""PostToolUse(Bash) で、コミットの直後に check を回す。

**規律が「コミット直後に基底 check」と書いてある**（`CLAUDE.md` の
「コミットの運用」）。**忘れても気づけないので、機械で回す。**

**カーネルに触ったコミットは `--commit` を回す**（運用者の承認。2026-09-18）。
**基底の 34 項目は像の番地を 1 つも見ない**——**起動ログの突き合わせは
`--commit` の 35 項目目である。** **`ADR-0063` の (b2) で、段の途中で破壊を
足したあと `--commit` を回さず、`--full` が 110.6 分かけて赤を出した**
（`docs/troubleshooting.md`）。**hook の増分は 4.5 分なので、1 回の抜けの
ほうが 20 倍以上高い。**

**範囲は `kernel/` と `common/` である。** **`xtask/src/` は像に影響しない**
（検査の側なので、壊れれば基底で見える）。

**実行の形だけを見る。** 部分一致にすると、文書に書いた文字列で発火する
（`deny_dangerous_bash.py` が実測で踏んだ形である）。

**限界が 3 つある。**

- **温まっていれば 1.5 秒だが、冷えていればビルドの時間が乗る**
  （実測。温まった状態で 1.46 / 1.47 / 1.48 秒）。
  **harness 側の上限で先に切られることがある。**
- **`--commit` は起動ログを 3 構成ぶん捕る**（実測で 270 秒）。
  **harness に切られたら、hook は何も言わずに消える**——**そのときは
  自分で `cargo xtask check --commit` を打つこと。**
- **これは「コミットの後」であって「前」ではない。** 落ちたら `--amend` か
  次のコミットで直す。**前で止める形は、コミットの経路を hook から
  横取りすることになるので採らない。**


**全部拒むようになったときの抜け方は `docs/coding-standards.md` の
「hookが全部拒むようになったときの抜け方」にある。** **ここに写さない。**
"""
import json
import os
import re
import subprocess
import sys

COMMIT = re.compile(r"(?:^|[;&|]\s*|\n\s*)git\s+commit\b")
TIMEOUT_SECONDS = 240
"""基底 check の上限（秒）。"""

COMMIT_CHECK_TIMEOUT_SECONDS = 900
"""`--commit` の上限（秒）。**起動ログの捕獲だけで 270 秒かかる**（実測）。"""

IMAGE_PATH_PREFIXES = ("kernel/", "common/")
"""この下に触ったコミットは `--commit` を回す。**像の番地が動く側である。**"""


def looks_like_a_commit(command: str) -> bool:
    """発火するかを決める。**ここだけが判定であり、self-test が覆う。**"""
    return bool(COMMIT.search(command))


def wants_commit_check(paths: list[str]) -> bool:
    """`--commit` を回すかを決める。**パスの一覧だけで決める。**

    **`git show --name-only` の出力を渡す。** **判定を引数だけの関数にして
    あるので、self-test が覆える**（`git` を呼ぶ側は覆えない）。
    """
    return any(path.startswith(IMAGE_PATH_PREFIXES) for path in paths)


def paths_in_head(root: str) -> list[str]:
    """直前のコミットが触ったパス。**読めなければ空を返す。**

    **空なら基底を回す**——**`--commit` を回せないより、回さないほうが
    害が小さい**（回らなければ次の `--commit` か `--full` が言う）。
    """
    try:
        done = subprocess.run(
            ["git", "show", "--name-only", "--pretty=format:", "HEAD"],
            cwd=root,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except Exception:
        return []
    if done.returncode != 0:
        return []
    return [line.strip() for line in done.stdout.splitlines() if line.strip()]


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
    touched = paths_in_head(root)
    commit_check = wants_commit_check(touched)
    argv = ["cargo", "xtask", "check"] + (["--commit"] if commit_check else [])
    label = "--commit" if commit_check else "基底"
    try:
        done = subprocess.run(
            argv,
            cwd=root,
            capture_output=True,
            text=True,
            timeout=(
                COMMIT_CHECK_TIMEOUT_SECONDS if commit_check else TIMEOUT_SECONDS
            ),
        )
    except Exception as error:
        print(
            f"post-commit check: 走らせられなかった（{label}。{error}）",
            file=sys.stderr,
        )
        return 2
    if done.returncode == 0:
        summary = [l for l in done.stdout.splitlines() if "check(s) passed" in l]
        print(f"post-commit check（{label}）: " + (summary[-1] if summary else "OK"))
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
    print(
        f"post-commit check: 落ちた（コミットの直後の {label} check）。",
        file=sys.stderr,
    )
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
    # **どちらの check を回すかの判定**（2026-09-18）。**像の番地が動く側だけ
    # `--commit` へ上げる。**
    path_cases = [
        (["kernel/src/task.rs"], True),
        (["common/src/time.rs"], True),
        (["docs/roadmap.md", "kernel/Cargo.toml"], True),
        (["docs/roadmap.md"], False),
        (["xtask/src/main.rs", "xtask/reference/boot-log-smp2.txt"], False),
        ([".claude/hooks/check_after_commit.py"], False),
        ([], False),
        # **接頭辞で見るので、似た名前の別のディレクトリは当てない。**
        (["kernelspace/notes.md"], False),
        (["docs/kernel/overview.md"], False),
    ]
    for paths, want in path_cases:
        got = wants_commit_check(paths)
        if got != want:
            print(f"self-test: {paths!r} wanted {want} but got {got}")
            failures += 1
    if failures:
        return 1
    total = len(cases) + len(path_cases)
    print(f"self-test: {total} case(s) decided as expected")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--self-test":
        raise SystemExit(self_test())
    raise SystemExit(main())
