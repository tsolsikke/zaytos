#!/usr/bin/env python3
"""PreToolUse(Bash) で危ない形を拒む（`CLAUDE.md` の「バックグラウンド実行の手順」）。

**実行の形だけを見る。** 部分一致にすると、文書に書いた文字列まで拒む
（実測で踏んだ——この規則そのものを書く heredoc が拒まれた）。


**全部拒むようになったときの抜け方は `docs/coding-standards.md` の
「hookが全部拒むようになったときの抜け方」にある。** **ここに写さない。**
"""
import json
import re
import sys

# **前に置けるものを読み飛ばす**（2026-09-25）——**環境変数の代入（`X=1 git push`）と、
# `env`・`command`・`exec`・`nice`・`timeout`（`timeout 60 git push`）。** **以前は判定が `git` の直前に
# 区切りを求めていたので、これらを前に置くと 3 本の hook のどれも発火しなかった**
# （`docs/troubleshooting.md`）。**引用は [`executable_part`] が空白に落とすので、代入の値は `\S*` で
# 足りる。** **値を取る旗（`timeout -k 5 60`）の後は読まない**（限界。この形は打たない）。
# **隣の 2 本もこれを使う**——**写さない**（片方だけが古くなる）。
PREFIX = (
    r"(?:\w+=\S*\s+|(?:env|command|exec)\s+(?:-\S+\s+)*"
    r"|nice\s+(?:-n\s*\S+\s+|-\S+\s+)?|timeout\s+(?:-\S+\s+)*\S+\s+)*"
)
START = r"(?:^|[;&|]\s*|\n\s*)" + PREFIX
RULES = [
    (re.compile(START + r"git\s+add\s+(?:-A|--all)\b"), "deny",
     "HOOK-PROBE-ADD: 全部足す形は使わない。パスを明示すること"),
    (re.compile(START + r"git\s+commit\s+(?:-[a-zA-Z]*a[a-zA-Z]*|--all)\b"), "exit2",
     "HOOK-PROBE-EXIT2: commit の -a は使わない"),
    # **背景へ落とす `&` は「行末」だけではない**（2026-09-03。実測で踏んだ）。
    #
    # **`( ... &)` で起こしたら、この規則は拒まなかった**——**`&` の後ろに
    # `)` が在ったからである。** **害は出なかったが、穴が在ることが分かった。**
    #
    # **形を 4 つ並べる**——**行末 / `)` の直前 / `;` の直前 / 改行の直前である。**
    # **`&&` は除く**（`(?<!&)` と `(?!&)`）。**`2>&1` のような向き先も除く**
    # （`&` の前が `>` や `<` なら、背景へ落とす `&` ではない）。
    (re.compile(r"(?<![&><])&(?!&)\s*(?:$|[)\n;])"), "deny",
     "HOOK-PROBE-AMP: 背景へ落とす & は使わない。run_in_background を使うこと"),
    # **走行の前の `;` と改行は、前が落ちても走行を始める**（2026-09-19。実測で踏んだ
    # ——**一覧を書き換える Python が落ちたのに、`;` の先の走行が進み、移していない木で
    # `--full` が緑を出した**。`docs/troubleshooting.md`）。
    #
    # **編集の側では塞げない**——**編集の形は列挙できない**（Python の heredoc・`sed -i`・
    # リダイレクト・…）。**走行の側は 3 つで尽きる**——`cargo`・`git commit`・`git push`
    # （`timeout` 付きも）。**`&&` の後の改行と `do`/`then`/`else` の後の改行は区切りでは
    # ないので、[`executable_part`] が先に畳む。** **走行の後の `;` は見ない**（前が落ちる話
    # ではない）。
    (re.compile(r"(?:;|\n)[ \t]*" + PREFIX + r"(?:cargo\b|git\s+(?:commit|push)\b)"), "deny",
     "HOOK-PROBE-SEMI: 走行（cargo / git commit / git push）の前の ; と改行は使わない。"
     "前の編集が落ちても走行が始まる。&& で繋ぐこと"),
]

CONTINUATION = re.compile(r"(&&|\|\||\||\\|\b(?:do|then|else))[ \t]*\n[ \t]*")


def joined(text: str) -> str:
    """行の続きを 1 行に畳む（2026-09-19）。

    **`&&` の後の改行は「前が通ったら」であって、区切りではない。** **`do`/`then`/`else` の
    後の改行も同じである**（ループと分岐の本文の 1 行目）。**畳まなければ、走行の前の改行を
    拒む規則が、`&&` の後に改行を置いて `cargo` を続ける形まで拒む。**
    """
    return CONTINUATION.sub(r"\1 ", text)

def executable_part(command: str) -> str:
    """引用と heredoc の中身を落とす（2026-09-03）。

    **実行の形だけを見る**（このファイルの冒頭）。**`&` の形を増やしたら、
    文中の言及まで拒むようになった**——**この規則を書くコミットメッセージ自身が
    拒まれた**（実測）。**1 度目に踏んだ形と同じである。**

    **落とすのは 3 つ**——`'...'`・`"..."`・heredoc の本文である。
    **落とした跡は空白にする**（語が繋がって別の形に見えないように）。
    """
    out = []
    i = 0
    length = len(command)
    while i < length:
        c = command[i]
        # heredoc（`<<EOF` / `<<'EOF'` / `<<-EOF`）。本文を終端まで落とす。
        if c == "<" and command[i : i + 2] == "<<":
            match = re.match(r"<<-?\s*(['\"]?)(\w+)\1", command[i:])
            if match:
                tag = match.group(2)
                out.append(" ")
                rest = command[i + match.end() :]
                end = re.search(r"^\s*" + re.escape(tag) + r"\s*$", rest, re.M)
                i += match.end() + (end.end() if end else len(rest))
                continue
        if c in ("'", '"'):
            closing = command.find(c, i + 1)
            out.append(" ")
            i = len(command) if closing < 0 else closing + 1
            continue
        out.append(c)
        i += 1
    return joined("".join(out))


def main() -> int:
    try:
        command = json.load(sys.stdin).get("tool_input", {}).get("command", "")
    except Exception as error:
        # **読めなければ拒む。** **守る側が黙って通ると、守っていないことが
        # 誰にも見えない**（2026-09-10 に Python 側を洗って直した）。
        print(
            f"deny_dangerous_bash: 入力が読めなかった（{error}）。判定できないので拒む",
            file=sys.stderr,
        )
        return 2
    command = executable_part(command)
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
        # **`&` の形を 4 つ並べる**（2026-09-03。**`( ... &)` で踏んだ**）。
        ("(sleep 1 &)", "deny"),
        ("(cargo xtask check --commit > out.txt 2>&1 &) ; sleep 2", "deny"),
        ("sleep 1 &\npwd", "deny"),
        ("sleep 1 & pwd", "allow"),
        # **向き先の `&` は背景ではない。**
        ("cargo build 2>&1 | tail -2", "allow"),
        ("cmd >out 2>&1", "allow"),
        # **文中の言及は通る**（**1 度目に踏んだ「部分一致が文書の言及まで拒む」を
        # 再び踏まないためである**）。
        ("echo '行末の & は使わない'", "allow"),
        ("grep -n 'sleep 1 &' docs/troubleshooting.md", "allow"),
        # **引用と heredoc の中身は実行の形ではない**（2026-09-03。
        # **この規則を書くコミットメッセージ自身が拒まれた**）。
        ("git " + "commit -m '( ... &)で踏んだ'", "allow"),
        ("echo \"( sleep 1 &)\"", "allow"),
        ("cat <<'EOF'\n( sleep 1 &)\nEOF", "allow"),
        ("cat <<'EOF'\ngit " + "add -A\nEOF", "allow"),
        # **引用の外は拒む。** 落としても形は残る。
        ("echo 'x' ; (sleep 1 &)", "deny"),
        # **走行の前の `;` と改行は拒む**（2026-09-19。**編集が黙って落ちたのに走行が進んだ**）。
        ("python3 move.py; cargo xtask check", "deny"),
        ("sed -i 's/a/b/' x.rs\ncargo fmt --all", "deny"),
        ("cat <<'EOF' > x.rs\nfn main() {}\nEOF\ncargo xtask check", "deny"),
        ("git " + "add x; git " + "commit -m x", "deny"),
        ("git log -1; git push", "deny"),
        ("echo x; timeout 600 cargo xtask check --full", "deny"),
        # **`&&` と行の続きは通る。** **走行の後の `;` も通る**（前が落ちる話ではない）。
        ("python3 move.py && cargo xtask check", "allow"),
        ("cargo fmt --all &&\n  cargo xtask check", "allow"),
        ("for i in 1 2 3; do timeout 600 cargo xtask check; done", "allow"),
        ("while true; do\n  timeout 60 cargo xtask check\ndone", "allow"),
        ("timeout 600 cargo xtask check --full; echo done", "allow"),
        # **文中の言及は通る。**
        ("echo 'x; cargo xtask check'", "allow"),
        ("grep -n 'x; git " + "commit' docs/a.md", "allow"),
        # **前に置いたものを読み飛ばす**（2026-09-25。**以前は素通りした**）。
        ("X=1 git " + "add -A", "deny"),
        ("timeout 60 git " + "add --all", "deny"),
        ("env git " + "commit -am x", "exit2"),
        ("echo x; X=1 cargo xtask check", "deny"),
        ("echo x\nenv -i PATH=/usr/bin cargo build", "deny"),
        ("git log -1; nice git push", "deny"),
        ("X=1 cargo xtask check; echo done", "allow"),
    ]
    failures = 0
    for command, want in cases:
        got = "allow"
        for pattern, how, _ in RULES:
            if pattern.search(executable_part(command)):
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
