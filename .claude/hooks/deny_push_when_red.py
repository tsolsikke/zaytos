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

# コミットと同じコマンドの `push` は、検査せずに拒む（2026-09-18）

**この hook は `PreToolUse` で、コマンドが始まる前に 1 度だけ検査する。** **同じコマンドの中で
コミットを作ってから押すと、検査が見るのは「これから作るコミット」ではなく 1 つ前の木である**
——**実測で、本文 1 行のコミットをそのまま押した**（2026-09-18。`docs/troubleshooting.md`）。
**そこで、同じコマンドに `git commit` の呼び出しが在れば、検査せずに拒む。** **コミットと
`push` は別のコマンドにすること**——**基底 check の出力を読んでから、次のコマンドで押す。**

# 並走していたら、検査せずに拒む

**`cargo` は `target/` にファイルロックを取る。** **`--full` が走っている最中に
検査を始めると、終わるまで返ってこない**（最大 90 分）。**待つのではなく押さない**
——**走っているものを止めるか、終わってから押すこと。** **これは安全側である**
（`CLAUDE.md` の絶対ルール 1 と同じ理由で、並走そのものを避ける）。

**同じ木の `xtask` だけを数える**（2026-09-25。検査の体系の改善の ③）——**全検査は別の作業木
（`target/full-check/wt`）で回り、`target/` は木ごとに別である。** **作業木の全検査の間も、本の木で
基底を回して押せる。** **同じ木かは `/proc/<pid>/exe` が本の木の `target/debug/xtask` を指すかで見る。**
**`cargo xtask full` の親は数えない**——**本の木の `xtask` として走るが、作業木の子を待つだけで、
本の木では何も建てない。**

# 要る検査を済ませていないコミットを止める（2026-09-25。運用者の足す1点）

**コミットの後の hook は、全検査の間は `--commit` を錠で断られる**——**コミットは既に積まれている。**
**そこで基底が緑のあと `cargo xtask full --gate` を回し、押すコミット（どのリモートにも無いもの）の
それぞれに、要る検査（`kernel/` か `common/` に触れたものは `--commit`、他は基底）の合格の記録が
在るかを見る。** **無ければ、足りない検査とコミットを出して拒む。** **読むのは
`cargo xtask full --status` と同じ記録である**（本の木の `target/full-check/records.tsv`）。

**旗で越えられる**——**`ZAYTOS_PUSH_UNCHECKED='<理由>' git push ...`**（理由は空にできない）。
**越えたコミットは、理由と一緒に記録へ「override」として残る。** **使うのは、要る検査を後から
回せないときだけである**（例: 全検査の間に積んだコミットで、HEAD が先へ進んだ）。

**環境変数を前に置いた `push` も見る**（`X=1 git push`。**以前は `git` の直前に区切りを求めていたので、
代入を前に置くとこの hook が発火しなかった**。2026-09-25 に見つけた）。


**全部拒むようになったときの抜け方は `docs/coding-standards.md` の
「hookが全部拒むようになったときの抜け方」にある。** **ここに写さない。**
"""
import json
import os
import re
import subprocess
import sys
import time

# **`.pyc` を書かせない。** **隣を import すると `.claude/hooks/__pycache__/`
# ができ、`git status` に未追跡として出る**（実測。2026-09-06）。
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
# **引用と heredoc を落とす道具は隣の hook が持っている。** **写さない**
# ——**2 つ持つと、片方だけが「文書の言及まで拒む」形へ戻る。**
from deny_dangerous_bash import PREFIX, executable_part  # noqa: E402
# **緑の知らせと、harness の上限の読み方は隣の hook が持っている。** **写さない。**
from check_after_commit import announce, announce_payload, registered_timeout  # noqa: E402

# **`(` も始まりに数える**（隣の hook が `( ... &)` で穴を踏んだのと同じ形）。
# **前に置けるもの（代入・`env`・`timeout` 等）は [`PREFIX`] で読み飛ばす**（隣の hook が持つ。
# 2026-09-25。**以前は代入や `timeout` を前に置くと、この hook が発火しなかった**）。
START = r"(?:^|[;&|(]\s*|\n\s*)" + PREFIX
# `git -C dir push` のような大域の旗も通す。
PUSH = re.compile(START + r"git\s+(?:-\S+\s+\S+\s+|-\S+\s+)*push\b")
# **`commit-tree` のような下位の命令は当てない**（`\b` だと `-` の手前で切れて当たる）。
COMMIT = re.compile(START + r"git\s+(?:-\S+\s+\S+\s+|-\S+\s+)*commit(?![-\w])")
TIMEOUT_SECONDS = 240
GATE_TIMEOUT_SECONDS = 50
"""関門（`cargo xtask full --gate`）の上限（秒）。**基底の直後なので建てる時間は乗らない。**"""

# **旗**（この doc の「要る検査を済ませていないコミットを止める」）。**実行の形に在ることを見てから、
# 理由を元の文から読む**——**引用は実行の形では空白に落ちる。**
OVERRIDE = re.compile(START + r"ZAYTOS_PUSH_UNCHECKED=\S*\s+git\s+(?:-\S+\s+\S+\s+|-\S+\s+)*push\b")
OVERRIDE_REASON = re.compile(r"ZAYTOS_PUSH_UNCHECKED=(?:'([^']*)'|\"([^\"]*)\"|(\S+))")


def invokes_git_push(command: str) -> bool:
    """発火するかを決める。**ここだけが判定であり、self-test が覆う。**"""
    return bool(PUSH.search(executable_part(command)))


def commits_and_pushes(command: str) -> bool:
    """同じコマンドの中で、コミットを作ってから押す形か。**self-test が覆う。**

    **引用と heredoc の中の言及は数えない**（[`executable_part`] が落とす）。
    """
    body = executable_part(command)
    return bool(PUSH.search(body)) and bool(COMMIT.search(body))


def override_reason(command: str) -> str | None:
    """旗の理由（旗が無ければ `None`、在って空なら `""`）。**self-test が覆う。**"""
    if not OVERRIDE.search(executable_part(command)):
        return None
    match = OVERRIDE_REASON.search(command)
    if match is None:
        return ""
    return next((group for group in match.groups() if group is not None), "").strip()


def same_tree_builds(processes: list, binary: str) -> list:
    """同じ木の `xtask` を挙げる（`(pid, comm, 本体の道, 引数)` の並びから。**self-test が覆う。**）

    **建て直されて消えた本体は ` (deleted)` を外して比べる。** **`cargo xtask full` の親（最初の
    引数が `full`）は数えない**——**作業木の子を待つだけで、本の木では何も建てない。**
    """
    found = []
    for pid, comm, exe, args in processes:
        if comm != "xtask" or exe is None or exe.removesuffix(" (deleted)") != binary:
            continue
        if len(args) > 1 and args[1] == "full":
            continue
        found.append(f"{comm} (pid {pid})")
    return found


def running_builds(root: str) -> list:
    """並走している同じ木の `xtask` を挙げる（`/proc` の `comm` と `exe` で見る。args では見ない）。

    **args で見ると、`git push` を含むこのシェル自身に当たる。**
    """
    processes = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            with open(f"/proc/{entry}/comm", encoding="utf-8") as handle:
                comm = handle.read().strip()
        except OSError:
            continue
        try:
            exe = os.readlink(f"/proc/{entry}/exe")
        except OSError:
            exe = None
        try:
            with open(f"/proc/{entry}/cmdline", "rb") as handle:
                args = [part.decode("utf-8", "replace") for part in handle.read().split(b"\0") if part]
        except OSError:
            args = []
        processes.append((int(entry), comm, exe, args))
    binary = os.path.realpath(os.path.join(root, "target", "debug", "xtask"))
    return same_tree_builds(processes, binary)


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

    # **コミットと同じコマンドなら、検査の前に拒む。** **検査はコミットより前の木を見るので、
    # 緑でも意味が無い**（この doc の「コミットと同じコマンドの `push`」）。
    if commits_and_pushes(command):
        return deny(
            "同じコマンドの中で git commit と git push を呼んでいる。この hook はコマンドの前に "
            "1 度だけ検査するので、これから作るコミットを見られない。コミットと push を別の"
            "コマンドに分け、コミット直後の基底 check の出力を読んでから押すこと"
        )

    # **旗は理由を要る**（空の理由では越えさせない）。
    reason = override_reason(command)
    if reason == "":
        return deny(
            "ZAYTOS_PUSH_UNCHECKED の理由が空である。要る検査を後から回せない理由を書くこと"
            "（記録に残る）"
        )

    root = os.environ.get("CLAUDE_PROJECT_DIR") or payload.get("cwd") or "."
    # **並走を確かめられなければ押さない。** **`/proc` が読めないときに「何も
    # 走っていない」と答えると、確かめていないものを確かめたことにする。**
    try:
        others = running_builds(root)
    except Exception as error:
        return deny(f"並走を確かめられなかった（{error}）。確かめられないので押さない")
    if others:
        return deny(
            "push の前の基底 check が走らせられない（同じ木で並走: "
            + ", ".join(others)
            + "）。target/ のロックで待たされるので検査しない。"
            "止めるか終わるまで待つこと（.claude/skills/stop-a-process/SKILL.md）"
        )
    started = time.monotonic()
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
        # **基底が緑なら、要る検査の記録を見る**（この doc の「要る検査を済ませていないコミットを
        # 止める」）。
        gate = ["cargo", "xtask", "full", "--gate"] + (["--override", reason] if reason else [])
        try:
            checked = subprocess.run(
                gate, cwd=root, capture_output=True, text=True, timeout=GATE_TIMEOUT_SECONDS
            )
        except Exception as error:
            return deny(f"push の前の関門を走らせられなかった（{error}）")
        lines = [line for line in checked.stdout.splitlines() if line.strip()]
        if checked.returncode != 0:
            reasons = [line for line in checked.stderr.splitlines() if line.startswith("Error:")]
            return deny(
                "push の前の関門: 要る検査の合格の記録が無いコミットがある（コミットは既に"
                "積まれている）。そのコミットが HEAD のうちに要る検査を回すか、cargo xtask full "
                "<コミット> で確かめること。後から回せないときだけ、運用者に確かめて "
                "ZAYTOS_PUSH_UNCHECKED='<理由>' git push で越える（記録に残る）:\n  "
                + "\n  ".join((lines + reasons)[:20])
            )
        # **緑も言う**（2026-09-21。運用者の足す1点）。**黙って通すと、hook が読み込まれて
        # いなくても押せてしまい、「検査して通した」と区別が付かない**
        # （`check_after_commit.py` の「緑のときも言う」と同じ族）。
        summary = [l for l in done.stdout.splitlines() if "check(s) passed" in l]
        return announce(
            "PreToolUse",
            "push 前の基底 check: "
            + (summary[-1] if summary else "OK")
            + f"（{time.monotonic() - started:.0f} 秒）。"
            + (lines[-1] if lines else "関門: 出力なし"),
        )

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
        # **代入を前に置いた形も当てる**（2026-09-25。**以前は発火しなかった**）。
        (f"X=1 git {push}", True),
        (f"ZAYTOS_PUSH_UNCHECKED='the reason' git {push} origin main", True),
        (f"cd /x && A=1 B=2 git {push}", True),
        (f"echo 'X=1 git {push}'", False),
        # **前に命令を置いた形も当てる**（2026-09-25）。
        (f"timeout 60 git {push} origin main", True),
        # **値を取る旗（`-k 5`）の後は読まない**（限界。この形は打たない）。
        (f"timeout -k 5 60 git {push}", False),
        (f"env X=1 git {push}", True),
        (f"env -i PATH=/usr/bin git {push}", True),
        (f"nice -n 10 git {push}", True),
        (f"command git {push}", True),
        (f"exec git {push}", True),
        (f"timeout 60 cargo xtask check", False),
    ]
    commit = "commit"
    # **同じコマンドにコミットと push が在るか**（2026-09-18）。**文書の言及は通す**
    # ——**部分一致が文書の言及まで拒む形を、隣の hook で 2 度踏んでいる。**
    combined = [
        # **2 度目に実際に押した形である**——**改行で並べていた。**
        (f"git add a && git {commit} -q -m x -m y\ncargo xtask check\ngit {push}", True),
        (f"git {commit} -m x && git {push}", True),
        (f"git {commit} -q --amend -m x; cargo xtask check; git {push} origin main", True),
        (f"(git {commit} -m x) && git {push}", True),
        (f"git {push}", False),
        (f"git {commit} -m x", False),
        (f"git {commit}-tree abc -m x && git {push}", False),
        (f"echo 'git {commit} の後に押す' && git {push}", False),
        (f"git log --grep 'git {commit}' && git {push}", False),
        (f"cat <<'EOF'\ngit {commit} -m x\nEOF\ngit {push}", False),
    ]
    # **旗の理由**（2026-09-25）。**実行の形に在るときだけ読み、引用の中の言及は旗にしない。**
    flag = "ZAYTOS_PUSH_UNCHECKED"
    overrides = [
        (f"{flag}='refused during a full' git {push} origin main", "refused during a full"),
        (f'{flag}="two words" git {push}', "two words"),
        (f"{flag}=plain git {push}", "plain"),
        (f"{flag}='' git {push}", ""),
        (f"{flag}='  ' git {push}", ""),
        (f"git {push}", None),
        (f"echo '{flag}=x git {push}'", None),
        (f"{flag}=x cargo xtask check", None),
    ]
    # **同じ木の `xtask` だけを数える**（2026-09-25）。**消えた本体も同じ木に数え、別の木と QEMU は
    # 数えない。**
    binary = "/r/target/debug/xtask"
    xtask = "target/debug/xtask"
    processes = [
        (1, "xtask", "/r/target/debug/xtask", [xtask, "check"]),
        (2, "xtask", "/r/target/debug/xtask (deleted)", [xtask, "check", "--full"]),
        (3, "xtask", "/r/target/full-check/wt/target/debug/xtask", [xtask, "check", "--full"]),
        (4, "qemu-system-x86", "/usr/bin/qemu-system-x86_64", ["qemu-system-x86_64"]),
        (5, "xtask", None, []),
        (6, "xtask", "/r/target/debug/xtask", [xtask, "full", "HEAD"]),
    ]
    failures = 0
    for command, want in overrides:
        got = override_reason(command)
        if got != want:
            print(f"self-test (override): {command!r} wanted {want!r} but got {got!r}")
            failures += 1
    same = same_tree_builds(processes, binary)
    if same != ["xtask (pid 1)", "xtask (pid 2)"]:
        print(f"self-test (same tree): got {same!r}")
        failures += 1
    for command, want in cases:
        got = invokes_git_push(command)
        if got != want:
            print(f"self-test: {command!r} wanted {want} but got {got}")
            failures += 1
    for command, want in combined:
        got = commits_and_pushes(command)
        if got != want:
            print(f"self-test (commit and push): {command!r} wanted {want} but got {got}")
            failures += 1
    # **緑の知らせの形と、harness の上限が内部の上限より長いこと**（2026-09-21）。
    announced = announce_payload("PreToolUse", "x")
    if (
        announced.get("hookSpecificOutput", {}).get("hookEventName") != "PreToolUse"
        or "permissionDecision" in announced.get("hookSpecificOutput", {})
    ):
        print(f"self-test: announce_payload has the wrong shape: {announced!r}")
        failures += 1
    # **harness の上限は、基底と関門の上限の和より長いこと**（2026-09-25。関門を足した）。
    harness = registered_timeout("deny_push_when_red.py")
    inner = TIMEOUT_SECONDS + GATE_TIMEOUT_SECONDS
    if harness is None or harness <= inner:
        print(
            f"self-test: settings.json gives this hook {harness} s, which must be longer than "
            f"its own limits of {TIMEOUT_SECONDS} + {GATE_TIMEOUT_SECONDS} s"
        )
        failures += 1
    if failures:
        return 1
    total = len(cases) + len(combined) + len(overrides) + 1 + 2
    print(f"self-test: {total} case(s) decided as expected")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--self-test":
        raise SystemExit(self_test())
    raise SystemExit(main())
