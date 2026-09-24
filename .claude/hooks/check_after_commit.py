#!/usr/bin/env python3
"""PostToolUse(Bash) で、コミットの直後に check を回す。

**規律が「コミット直後に基底 check」と書いてある**（`CLAUDE.md` の
「コミットの運用」）。**忘れても気づけないので、機械で回す。**

**カーネルに触ったコミットは `--commit` を回す**（運用者の承認。2026-09-18）。
**基底は像の番地を 1 つも見ない**——**起動ログの突き合わせは、`--commit` が
基底に足す 1 項目である。** **`ADR-0063` の (b2) で、段の途中で破壊を
足したあと `--commit` を回さず、`--full` が 110.6 分かけて赤を出した**
（`docs/troubleshooting.md`）。**hook の増分は約 36 秒なので**（2026-09-25 に
起動ログの採り方を直した後。以前は 4.5 分）、**1 回の抜けのほうが 100 倍以上高い。**

**範囲は `kernel/` と `common/` である。** **`xtask/src/` は像に影響しない**
（検査の側なので、壊れれば基底で見える）。

**実行の形だけを見る。** 部分一致にすると、文書に書いた文字列で発火する
（`deny_dangerous_bash.py` が実測で踏んだ形である）。

**緑のときも言う**（運用者の足す1点。2026-09-21）。**以前は緑の1行を素の
stdout に出していたが、`exit 0` の stdout は読み手に届かない**（通った回の行は
一度も見えていなかった）。**緑も黙り、走らなかったときも黙るので、区別が
付かなかった**——**実際に、コミットの後で自分で `--commit` を打ち直した**
（`ADR-0066` の Y-b の締め）。**いまは緑のとき JSON で言う**——`systemMessage`
（利用者の画面）と `additionalContext`（読み手の文脈）の両方へ（[`announce`]）。
**これで、黙っていれば「終わらなかった」と読める。**

**限界が 3 つある。**

- **温まっていれば約 5 秒だが、冷えていればビルドの時間が乗る**
  （実測。2026-09-25 に温まった状態で 6.40 / 4.87 / 5.14 秒。以前は 1.5 秒で、
  その後に項目が 46 まで増えた）。
- **`--commit` は起動ログを 3 構成ぶん捕る**（実測で約 36 秒。2026-09-25 に採り方を
  直す前は 270 秒）。
  **harness に切られたら、hook は何も言わずに消える**——**そのときは
  自分で `cargo xtask check --commit` を打つこと。** **以前は harness の上限
  （`settings.json` の 300 秒）が、この hook の内部の上限（900 秒）より短かった**
  ——**内部の上限が効く前に harness が切るので、「走らせられなかった」と言う機会が
  無かった。** **いまは harness の上限を内部の上限より長くしてあり、`--self-test` が
  その大小を毎回確かめる**（[`registered_timeout`]）。
- **これは「コミットの後」であって「前」ではない。** 落ちたら `--amend` か
  次のコミットで直す。**前で止める形は、コミットの経路を hook から
  横取りすることになるので採らない。**


# 錠で断られたら「走らせなかった」と言う（2026-09-25。検査の体系の改善の ③）

**全検査の間は、`--commit` が検査の錠で断られる**（`xtask/src/check_lock.rs`。終了の値 75）。
**断られた回は緑に数えない**——**「走らせなかった」と言い、`exit 2` で終える。** **記録には
`xtask` が「断られた」を残し、push の前の関門（`deny_push_when_red.py`）は、そのコミットの合格の記録が
在るまで押させない**（コミットは既に積まれているので、ここでは止められない）。**基底は錠を取らない**
——**全検査の間も走る。**

**環境変数を前に置いたコミットも見る**（`X=1 git commit`。2026-09-25 に見つけた穴）。


**全部拒むようになったときの抜け方は `docs/coding-standards.md` の
「hookが全部拒むようになったときの抜け方」にある。** **ここに写さない。**
"""
import json
import os
import re
import subprocess
import sys
import time

# **`.pyc` を書かせない**（隣を import すると `.claude/hooks/__pycache__/` ができ、`git status` に出る）。
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
# **引用と heredoc を落とす道具と、前に置けるものの並びは隣の hook が持っている。** **写さない。**
from deny_dangerous_bash import PREFIX, executable_part  # noqa: E402

# **実行の形（引用と heredoc を落とした後）に当てる**（2026-09-25）。**前に置けるもの（代入・`env`・
# `timeout` 等）は [`PREFIX`] で読み飛ばす**——**以前は `git` の直前に区切りを求めていたので、これらを
# 前に置くと発火しなかった。**
COMMIT = re.compile(r"(?:^|[;&|]\s*|\n\s*)" + PREFIX + r"git\s+commit\b")
TIMEOUT_SECONDS = 240
"""基底 check の上限（秒）。"""

COMMIT_CHECK_TIMEOUT_SECONDS = 900
"""`--commit` の上限（秒）。**起動ログの捕獲は約 36 秒**（実測。2026-09-25 に採り方を直す前は
270 秒）。**冷えていればビルドの時間が乗るので、上限は下げていない。**"""

IMAGE_PATH_PREFIXES = ("kernel/", "common/")
"""この下に触ったコミットは `--commit` を回す。**像の番地が動く側である。**

**`xtask/src/full_check.rs` の `IMAGE_PATH_PREFIXES` と同じである**（あちらは記録と push の前の関門で
要る検査を決める）。**基底の確かめが一致を見る。**"""

REFUSED_EXIT_CODE = 75
"""検査の錠が取れずに `xtask` が断ったときの終了の値（`xtask/src/check_lock.rs`）。"""


def looks_like_a_commit(command: str) -> bool:
    """発火するかを決める。**ここだけが判定であり、self-test が覆う。**"""
    return bool(COMMIT.search(executable_part(command)))


def wants_commit_check(paths: list[str]) -> bool:
    """`--commit` を回すかを決める。**パスの一覧だけで決める。**

    **`git show --name-only` の出力を渡す。** **判定を引数だけの関数にして
    あるので、self-test が覆える**（`git` を呼ぶ側は覆えない）。
    """
    return any(path.startswith(IMAGE_PATH_PREFIXES) for path in paths)


def paths_in_head(root: str) -> list[str] | None:
    """直前のコミットが触ったパス。**読めなければ `None` を返す。**

    **空の一覧と「読めなかった」を分ける**——**分けないと、読めなかったときに
    黙って基底へ落ちる**（「失敗を空に落とすと、もっともらしい誤りが出る」の族。
    運用者の指摘。2026-09-18）。**どちらにするかは [`decide`] が決める。**
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
        return None
    if done.returncode != 0:
        return None
    return [line.strip() for line in done.stdout.splitlines() if line.strip()]


def refused_notice(label: str, stderr: str) -> str:
    """錠で断られたときに言うこと。**引数だけで決める**（`--self-test` が形を覆う）。"""
    rerun = "cargo xtask check" + (" --commit" if label == "--commit" else "")
    lines = [
        f"post-commit check: 走らせなかった（{label}。検査の錠が取れなかった——全検査が走っている）。"
        "緑に数えない。",
        f"  記録には「断られた」が残る。push の前の関門は、このコミットの {label} の合格の記録が"
        "在るまで押させない。",
        f"  全検査が終わってから、このコミットが HEAD のうちに {rerun} を打ち直すこと"
        "（HEAD が進んだら cargo xtask full <コミット> で確かめる）。",
    ]
    # **`cargo` の進みの行は落とす**（`Finished`・`Running`・`Compiling`。断りの行が埋もれる）。
    noise = ("Finished ", "Running ", "Compiling ", "Blocking ")
    kept = [line for line in stderr.splitlines() if line.strip() and not line.strip().startswith(noise)]
    lines += ["  " + line for line in kept][:12]
    return "\n".join(lines)


UNREADABLE_NOTICE = (
    "post-commit check: 直前のコミットのパスが読めなかったので、基底に落とした。"
    "カーネルに触ったコミットなら、自分で cargo xtask check --commit を回すこと"
)


def decide(paths: list[str] | None) -> tuple[bool, str | None]:
    """どちらの check を回すかと、言うべきことを決める。**引数だけで決める。**

    **読めなければ基底へ落とす**——**`--commit` を回せないより、回さないほうが
    害が小さい。** **ただし黙らない**——**落としたことを言い、`exit 2` で終える**
    （シリアルの錠の fail open と同じ形。理由が在り、観測が残る）。
    """
    if paths is None:
        return False, UNREADABLE_NOTICE
    return wants_commit_check(paths), None


def announce_payload(event: str, text: str) -> dict:
    """緑の知らせの JSON。**引数だけで決める**（`--self-test` が形を覆う）。

    **`systemMessage` は利用者の画面へ、`additionalContext` は読み手の文脈へ届く。**
    **`exit 0` の素の stdout はどちらにも届かない**（この doc の「緑のときも言う」）。
    **`permissionDecision` は入れない**——**入れると、許可の流れを hook が決めてしまう。**
    """
    return {
        "systemMessage": text,
        "hookSpecificOutput": {"hookEventName": event, "additionalContext": text},
    }


def announce(event: str, text: str) -> int:
    """緑の知らせを出して `exit 0` で終える。**`deny_push_when_red.py` も使う。**"""
    print(json.dumps(announce_payload(event, text), ensure_ascii=False))
    return 0


def registered_timeout(script: str) -> float | None:
    """`settings.json` がこの hook に付けた上限（秒）。**無ければ `None`。**

    **harness の上限は、hook の内部の上限より長くなければならない**——**短いと、
    内部の上限が効く前に harness が切り、hook は何も言えずに消える**（この doc の
    「限界」）。**`--self-test` がこれを読んで大小を確かめる。**
    """
    settings = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "settings.json")
    with open(settings, encoding="utf-8") as handle:
        config = json.load(handle)
    for entries in config.get("hooks", {}).values():
        for entry in entries:
            for hook in entry.get("hooks", []):
                if script in hook.get("command", ""):
                    timeout = hook.get("timeout")
                    return None if timeout is None else float(timeout)
    return None


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
    commit_check, notice = decide(paths_in_head(root))
    argv = ["cargo", "xtask", "check"] + (["--commit"] if commit_check else [])
    label = "--commit" if commit_check else "基底"
    started = time.monotonic()
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
    seconds = time.monotonic() - started
    # **錠で断られた回は、緑にも赤にも数えない**——**走らせなかったと言う**（この doc の 2026-09-25）。
    if done.returncode == REFUSED_EXIT_CODE:
        print(refused_notice(label, done.stderr), file=sys.stderr)
        if notice is not None:
            print(notice, file=sys.stderr)
        return 2
    if done.returncode == 0:
        summary = [l for l in done.stdout.splitlines() if "check(s) passed" in l]
        text = (
            f"post-commit check（{label}）: "
            + (summary[-1] if summary else "OK")
            + f"（{seconds:.0f} 秒）"
        )
        if notice is not None:
            # **通ったが、落とした事実は言って終える。** **`exit 2` の stderr が読み手に
            # 届くので、緑の行もそちらへ出す**（**素の stdout は届かない**）。
            print(text, file=sys.stderr)
            print(notice, file=sys.stderr)
            return 2
        # **緑も言う**（この doc の「緑のときも言う」）。
        return announce("PostToolUse", text)
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
    if notice is not None:
        print(notice, file=sys.stderr)
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
        # **代入を前に置いた形も当てる**（2026-09-25）。
        ("X=1 git " + "commit -m x", True),
        ("A='a b' B=2 git " + "commit -q -F -", True),
        ("X=1 cargo xtask check", False),
        ("timeout 60 git " + "commit -m x", True),
        ("env X=1 git " + "commit -m x", True),
        ("nice git " + "commit -m x", True),
        ("timeout 60 cargo xtask check", False),
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
    # **読めなかったときは基底へ落ちつつ、言う**（2026-09-18。運用者の足す1点）。
    decide_cases = [
        (None, (False, True)),
        ([], (False, False)),
        (["kernel/src/task.rs"], (True, False)),
        (["docs/roadmap.md"], (False, False)),
    ]
    for paths, (want_commit, want_notice) in decide_cases:
        got_commit, got_notice = decide(paths)
        if got_commit != want_commit or (got_notice is not None) != want_notice:
            print(
                f"self-test: decide({paths!r}) wanted ({want_commit}, notice={want_notice}) "
                f"but got ({got_commit}, notice={got_notice is not None})"
            )
            failures += 1
    # **緑の知らせの形**（2026-09-21。運用者の足す1点）。**読み手と利用者の両方へ届く鍵が
    # 在り、許可の判断は入っていないこと。**
    announced = announce_payload("PostToolUse", "x")
    if (
        announced.get("systemMessage") != "x"
        or announced.get("hookSpecificOutput", {}).get("additionalContext") != "x"
        or announced.get("hookSpecificOutput", {}).get("hookEventName") != "PostToolUse"
        or "permissionDecision" in announced.get("hookSpecificOutput", {})
    ):
        print(f"self-test: announce_payload has the wrong shape: {announced!r}")
        failures += 1
    # **錠で断られたときの知らせ**（2026-09-25）。**走らせなかったと言い、緑に数えず、打ち直す
    # 検査と持ち主の行を出す。**
    refused = refused_notice(
        "--commit",
        "    Finished `dev` profile\nxtask: the check lock is held\n  holder: pid 42 (exclusive)",
    )
    for needle in ("走らせなかった", "緑に数えない", "cargo xtask check --commit", "holder: pid 42"):
        if needle not in refused:
            print(f"self-test: refused_notice lacks {needle!r}: {refused!r}")
            failures += 1
    if "Finished" in refused:
        print(f"self-test: refused_notice kept the cargo progress line: {refused!r}")
        failures += 1
    if "--commit" in refused_notice("基底", ""):
        print("self-test: refused_notice for the base check names --commit")
        failures += 1
    # **harness の上限が内部の上限より長いこと**（2026-09-21）。**短いと、`--commit` の
    # 途中で harness が切り、何も言わずに消える。**
    harness = registered_timeout("check_after_commit.py")
    if harness is None or harness <= COMMIT_CHECK_TIMEOUT_SECONDS:
        print(
            f"self-test: settings.json gives this hook {harness} s, which must be longer than "
            f"its own --commit limit of {COMMIT_CHECK_TIMEOUT_SECONDS} s"
        )
        failures += 1
    if failures:
        return 1
    total = len(cases) + len(path_cases) + len(decide_cases) + 5
    print(f"self-test: {total} case(s) decided as expected")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--self-test":
        raise SystemExit(self_test())
    raise SystemExit(main())
