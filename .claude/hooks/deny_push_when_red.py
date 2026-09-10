#!/usr/bin/env python3
"""PreToolUse(Bash) で、基底 check が赤のままの `git push` を拒む（2026-09-06）。

# 規約と抵触しないことを先に書く

**`CLAUDE.md` の「コミットの運用」と `check_after_commit.py` の doc は
「コミットの経路を hook から横取りしない」と書いている。** **あれはコミットの
経路の話である。** **ここが止めるのは `push` で、`commit` は今までどおり作れる**
——**したがって抵触しない。** **止めるのは「赤のまま外へ出ること」だけである。**

# なぜ push だけ止めるのか

**押した後は直せないからである。** **コミットメッセージの文体の検査は全履歴を
見るので、違反したコミットを押すと、以後ずっと赤くなる。** **同じ形は 3 回出て、
3 回目で実際に押した**（2026-09-06。`--amend` と `--force-with-lease` で直した。
`docs/troubleshooting.md`）。**直前の 2 回は押す前に気づいただけで、気づき方は
同じではない**——**規律では止まらないと判断した。**

# 判定は 2 段である

- **`git push` を実際に呼ぶ形か**（[`invokes_git_push`]。**ここだけが判定表を
  持ち、`--self-test` が覆う**）
- **呼ぶなら基底 `cargo xtask check` を回し、赤なら拒む**（2.3 秒。実測）

# 並走していたら、検査せずに拒む

**`cargo` は `target/` にファイルロックを取る。** **`--full` が走っている最中に
検査を始めると、終わるまで返ってこない**（最大 90 分）。**待つのではなく押さない**
——**走っているものを止めるか、終わってから押すこと。** **これは安全側である**
（`CLAUDE.md` の絶対ルール 1 と同じ理由で、並走そのものを避ける）。
"""
import json
import os
import re
import subprocess
import sys

# **`.pyc` を書かせない。** **隣を import すると `.claude/hooks/__pycache__/`
# ができ、`git status` に未追跡として出る**（実測。2026-09-06）。
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
# **引用と heredoc を落とす道具は隣の hook が持っている。** **写さない**
# ——**2 つ持つと、片方だけが「文書の言及まで拒む」形へ戻る。**
from deny_dangerous_bash import executable_part  # noqa: E402

# **`(` も始まりに数える**（隣の hook が `( ... &)` で穴を踏んだのと同じ形）。
START = r"(?:^|[;&|(]\s*|\n\s*)"
# `git -C dir push` のような大域の旗も通す。
PUSH = re.compile(START + r"git\s+(?:-\S+\s+\S+\s+|-\S+\s+)*push\b")
TIMEOUT_SECONDS = 240


def invokes_git_push(command: str) -> bool:
    """発火するかを決める。**ここだけが判定であり、self-test が覆う。**"""
    return bool(PUSH.search(executable_part(command)))


def running_builds() -> list:
    """並走している QEMU / xtask を挙げる（`comm` で見る。args では見ない）。

    **args で見ると、`git push` を含むこのシェル自身に当たる。**
    """
    out = subprocess.run(
        ["ps", "-eo", "pid,comm"], capture_output=True, text=True, timeout=10
    ).stdout
    found = []
    for line in out.splitlines()[1:]:
        parts = line.split(None, 1)
        if len(parts) == 2 and re.search(r"qemu|xtask", parts[1]):
            found.append(f"{parts[1].strip()} (pid {parts[0]})")
    return found


def deny(reason: str) -> int:
    print(json.dumps({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": reason,
    }}))
    return 0


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except Exception as error:
        # **読めなければ拒む**（隣の hook と同じ理由。2026-09-10）。
        print(
            f"deny_push_when_red: 入力が読めなかった（{error}）。判定できないので拒む",
            file=sys.stderr,
        )
        return 2
    command = payload.get("tool_input", {}).get("command", "")
    if not invokes_git_push(command):
        return 0

    # **並走を確かめられなければ押さない。** **`ps` が落ちたときに「何も
    # 走っていない」と答えると、確かめていないものを確かめたことにする。**
    try:
        others = running_builds()
    except Exception as error:
        return deny(f"並走を確かめられなかった（{error}）。確かめられないので押さない")
    if others:
        return deny(
            "push の前の基底 check が走らせられない（並走: "
            + ", ".join(others)
            + "）。target/ のロックで待たされるので検査しない。"
            "止めるか終わるまで待つこと（.claude/skills/stop-a-process/SKILL.md）"
        )

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
        return deny(f"push の前の基底 check を走らせられなかった（{error}）")
    if done.returncode == 0:
        return 0

    detail = [line for line in done.stdout.splitlines() if "FAILED" in line]
    reasons = [line for line in done.stderr.splitlines() if line.startswith("Error:")]
    return deny(
        "push の前の基底 check が赤である。押すと直せない（文体の検査は全履歴を"
        "見る）。直してから押すこと:\n  " + "\n  ".join((detail + reasons)[:10])
    )


def self_test() -> int:
    """判定表を回す（`cargo xtask check` から呼ぶ）。

    **見るのは「どの形で発火するか」だけである。** **実際に拒めることは、
    赤の木で押そうとして確かめるしかない**——**hook は読み込まれていなくても
    エラーを出さない**（`CLAUDE.md` の規律。実測で 2 回素通りしている）。
    """
    push = "push"
    cases = [
        (f"git {push}", True),
        (f"git {push} -q origin main", True),
        (f"git {push} --force-with-lease origin main", True),
        # **3 回目に実際に押した形である**——**赤でも `;` は次へ進む。**
        (f"cargo xtask check ; git {push} origin main", True),
        (f"cargo xtask check && git {push} origin main", True),
        (f"(git {push} origin main)", True),
        (f"git -C /tmp/clean {push}", True),
        (f"git --no-pager {push}", True),
        ("git status --short", False),
        (f"git {push}notes", False),
        (f"git {push}_all", False),
        # **文中の言及は通る**（隣の hook が 2 度踏んだ形である）。
        (f"echo 'git {push} は基底 check の後'", False),
        (f"grep -n 'git {push}' docs/troubleshooting.md", False),
        (f"cat <<'EOF'\ngit {push} origin main\nEOF", False),
        ("cargo xtask check", False),
    ]
    failures = 0
    for command, want in cases:
        got = invokes_git_push(command)
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
