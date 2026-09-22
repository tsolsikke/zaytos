#!/usr/bin/env python3
"""カーネルの関数ごとの枠の大きさを、基底の版と作業木で並べる（2026-09-23）。

**枠の大きさは `.debug_frame` の CFA から読む。前置きの `sub rsp` を手で読まない。**

# なぜ要るのか

**4KiB を超える枠は、`sub rsp,0x1000` で 1 ページずつ刻んでから残りを引く。** **最初の `sub` だけを
読むと、枠の大きさを取り違える。** **規約に書いた規則だったが、W1-b-2 で 1 度、`ADR-0068` の HW-a と
HW-b で 2 度読み違えた**（HW-a の +240 を最深の経路の上に無い関数へ帰した。HW-b の +400 も同じ）。
**規約で守れなかったので、道具にした**（レビューの指示。2026-09-23）。

**CFA は、その番地で「戻り番地がスタックのどこに在るか」を言う表である**（`readelf --debug-dump=frames-interp`）。
**関数の中の CFA の最大から 8（戻り番地）を引いたものを、枠の大きさとする。** **刻んで引く形も、
途中で積む形も、表がそのまま言う。**

# 使い方

    python3 tools/frame-sizes.py                          # HEAD と作業木で、枠が動いた関数を並べる
    python3 tools/frame-sizes.py --base f8f07a2           # 基底の版を選ぶ
    python3 tools/frame-sizes.py kernel::kernel_main      # 名前を挙げた関数は、動いていなくても出す
    python3 tools/frame-sizes.py --largest 20             # 作業木で枠の大きい関数を 20 出す

**名前は `nm -C` の形で、完全一致である。** **末尾に `*` を付けると前方一致になる。**

**基底の版は `target/frame-sizes/wt` に `git worktree` で取り出して建てる**（1 度建てれば次からは差分だけ）。
**作業木の像は `cargo build` で建て直す。** **`target/` を `xtask` と共有するので、`--full` と並べて
走らせない**（`CLAUDE.md` の絶対ルール 1）。

**何も主張しない。** **`cargo xtask check` は回さない。** **人が読むためのものである**
（`tools/boot-log-compare.py` と同じ立ち位置）。**どの関数が最深の経路に載っているかは、この道具では
分からない**——**`tools/stack-deepest.py` が実測する。**
"""
import argparse
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = "x86_64-unknown-none"
WORKTREE = os.path.join(ROOT, "target", "frame-sizes", "wt")


def run(args, cwd):
    result = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    if result.returncode != 0:
        sys.exit(f"{' '.join(args)} failed in {cwd}:\n{result.stderr[-2000:]}")
    return result.stdout


def build(tree, features):
    """その木でカーネルを建て、ELF の道を返す（`xtask` と同じ引数。既定の feature）。"""
    args = ["cargo", "build", "--target", TARGET, "-p", "kernel", "--bin", "kernel"]
    if features:
        args += ["--features", features]
    run(args, tree)
    return os.path.join(tree, "target", TARGET, "debug", "kernel")


def base_tree(rev):
    """基底の版を取り出した木を返す。**本体の木には触らない。**"""
    run(["git", "worktree", "prune"], ROOT)
    commit = run(["git", "rev-parse", "--verify", f"{rev}^{{commit}}"], ROOT).strip()
    if os.path.isdir(WORKTREE):
        run(["git", "checkout", "--quiet", "--detach", commit], WORKTREE)
    else:
        os.makedirs(os.path.dirname(WORKTREE), exist_ok=True)
        run(["git", "worktree", "add", "--quiet", "--detach", WORKTREE, commit], ROOT)
    return WORKTREE, commit


def frames(elf):
    """関数の名前 → 枠の大きさ（CFA の最大 − 8）。"""
    names = {}
    for line in run(["nm", "-C", elf], ROOT).splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3 and parts[1] in "tTwW":
            names.setdefault(int(parts[0], 16), parts[2])
    result = {}
    table = run(["readelf", "--debug-dump=frames-interp", elf], ROOT)
    for block in table.split("\n\n"):
        header = re.search(r"FDE cie=\S+ pc=([0-9a-f]+)\.\.", block)
        offsets = [int(value) for value in re.findall(r"^[0-9a-f]{16} rsp\+(\d+)", block, re.M)]
        if header and offsets:
            name = names.get(int(header.group(1), 16), f"<{header.group(1)}>")
            result[name] = max(result.get(name, 0), max(offsets) - 8)
    return result


def matches(name, pattern):
    return name.startswith(pattern[:-1]) if pattern.endswith("*") else name == pattern


def main():
    parser = argparse.ArgumentParser(description="関数ごとの枠の大きさを、基底の版と作業木で並べる")
    parser.add_argument("names", nargs="*", help="動いていなくても出す関数（nm -C の名前。末尾 * で前方一致）")
    parser.add_argument("--base", default="HEAD", help="基底の版（既定 HEAD）")
    parser.add_argument("--features", default="", help="両方の建てに渡す feature（既定は無し）")
    parser.add_argument("--top", type=int, default=30, help="動いた関数を大きい順にいくつ出すか（既定 30）")
    parser.add_argument("--largest", type=int, default=0, help="作業木で枠の大きい関数をいくつ出すか")
    options = parser.parse_args()

    tree, commit = base_tree(options.base)
    before = frames(build(tree, options.features))
    after = frames(build(ROOT, options.features))
    print(f"base {options.base} ({commit[:7]}) vs the working tree; frame = max CFA offset - 8 (byte)")

    moved = [(name, before.get(name), after.get(name)) for name in set(before) | set(after)
             if before.get(name) != after.get(name)]
    moved.sort(key=lambda row: -abs((row[2] or 0) - (row[1] or 0)))
    print(f"\n{len(moved)} function(s) changed their frame; the largest {min(len(moved), options.top)}:")
    for name, old, new in moved[: options.top]:
        delta = (new or 0) - (old or 0)
        print(f"  {old if old is not None else '-':>7} -> {new if new is not None else '-':>7}  {delta:+7d}  {name[:150]}")

    if options.names:
        print("\nnamed:")
        for pattern in options.names:
            hits = sorted(name for name in set(before) | set(after) if matches(name, pattern))
            if not hits:
                print(f"  (no function named {pattern!r})")
            for name in hits:
                old, new = before.get(name), after.get(name)
                print(f"  {old if old is not None else '-':>7} -> {new if new is not None else '-':>7}  {name[:150]}")

    if options.largest:
        print(f"\nthe {options.largest} largest frame(s) in the working tree:")
        for name, size in sorted(after.items(), key=lambda row: -row[1])[: options.largest]:
            print(f"  {size:>7}  {name[:150]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
