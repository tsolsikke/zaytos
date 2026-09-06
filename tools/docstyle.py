#!/usr/bin/env python3
"""Markdownの文体変更が内容とレンダリング結果を変えていないことを検査する。

使い方:
    python3 tools/docstyle.py                 # 現在のツリーだけを検査する
    python3 tools/docstyle.py --base <ref>    # <ref> と比較する検査も行う

検査項目:
    S1 GFMタスクリストのマーカー直後に半角空白があること
    S2 行結合の痕跡（和文の句読点・閉じ括弧の直後の半角空白、および
       直前に空白の無い `/` の直後の半角空白）
    S3 閉じない強調（**既定では走らせない。** `--with-s3` で明示的に実行する。
       理由と失効条件は下の「S3を既定から外した理由」を見ること）
    S4 フェンスの対と見出しレベルの飛び。h3より深い見出しは参考として出す
    S5 リポジトリ内リンクの参照先と、見出しアンカーの生存
    C1 レンダリング結果の差分（タグ列とテキスト。空白差は無視する）
    C2 正規化テキストの一致（空白をすべて除去した本文）
    C3 ASCII語彙の一致（多重集合）
    C4 S2の件数がベースラインより増えていないこと

C1からC4は --base を与えたときだけ実行する。

S3を既定から外した理由（T1-4で決定）:
    S3は20件を指摘するが、**全件が誤検出である。** いずれも `**` とバッククォートや
    かぎ括弧が隣り合う形で、ソースでは対になっている。判定を正しくするには
    インラインコードと強調の入れ子を解く必要があり、**Markdownの部分実装を
    持つことになる。** 費用が釣り合わない。

    **同じ目的の検査が `cargo xtask check` の「markdown prose style」に既にある。**
    「同じことを言う検査を2つ置かない」に従い、こちらは強制しない。

    **落ち続ける出力を残さないのが要点である。** 緑にならない道具は、
    出力そのものを人が読み飛ばすことを学習させる。だから消すのではなく
    既定から外し、必要なときだけ `--with-s3` で見る。

    失効条件: **`cargo xtask check` のmarkdown文体検査が、強調の対をなす範囲を
    失ったとき。** そのときS3を再訪する。
C1とC2の差分は意図した変更でもありうるため、内訳を出すだけで合否は判定しない。
`<strong>` の増減だけなら太字の整理、それ以外のタグが動いていれば要確認である。
機械的に合否を出すのはS1からS5とC3・C4である。
"""

import argparse
import re
import subprocess
import sys
from collections import Counter

try:
    from markdown_it import MarkdownIt
except ImportError:
    sys.exit("markdown-it-py が必要です: python3 -m pip install markdown-it-py")

MD = MarkdownIt("commonmark").enable(["table", "strikethrough"])

TASK_MARKER = re.compile(r"^\s*[-*+] \[[ xX]\]")
TASK_NO_SPACE = re.compile(r"^\s*[-*+] \[[ xX]\](?=\S)")
# 和文の句読点・閉じ括弧の直後の半角空白と、区切りの ` / ` ではない `/ `。
# ` / `（前後に空白のある区切り）は本プロジェクトの表記なので対象外とする。
JOIN_TRACE = re.compile(r"(?:[、。」』）] |(?<![ /])/ )(?=\S)")
CODE_SPAN = re.compile(r"`[^`]*`")


def list_files(ref=None):
    """検査対象のMarkdownを列挙する。

    **追跡済みだけでなく、無視されていない未追跡のファイルも対象にする。**
    `git ls-files` だけだと追跡済みしか出ないため、新しい文書を書いてから
    検査を通し、その後に `git add` するという自然な順序では、そのファイルの
    最初の検査が素通りする。新規追加こそ最も検査を必要とする場面である。
    同じ穴が `cargo xtask check` のSAFETY検査にもあった
    （`docs/troubleshooting.md` 2026-07-22 の記録）。
    """
    args = [
        "git",
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "README.md",
        "CLAUDE.md",
        "docs/",
        "probes/",
    ]
    out = subprocess.check_output(args, text=True).split()
    # --cached と --others は排他なので重複しないが、念のため順序を保って
    # 重複を落とす。件数の会計が合わなくなるのを避けるため。
    seen = []
    for f in out:
        if f.endswith(".md") and f not in seen:
            seen.append(f)
    return seen


def read(path, ref=None):
    if ref is None:
        with open(path, encoding="utf-8") as fh:
            return fh.read()
    return subprocess.check_output(["git", "show", f"{ref}:{path}"], text=True)


def split_by_presence(files, ref):
    """`ref` に在るものと無いものへ分ける。

    ベースラインとの比較は「同じファイルの前後」を見るものなので、
    **ベースに無いファイルは比較そのものが成立しない。**
    以前はここで `git show` が失敗して落ちていた。
    無いものは比較から外すが、**外したことは呼び出し側が必ず表示する。**
    黙って減らすと、比較した範囲が実際より広く見える。
    """
    listed = subprocess.check_output(
        ["git", "ls-tree", "-r", "--name-only", ref], text=True
    ).split("\n")
    at_ref = set(listed)
    return [f for f in files if f in at_ref], [f for f in files if f not in at_ref]


def prose_lines(text):
    """コードフェンス・インデントコード・表の行を除いた行を (行番号, 行) で返す。"""
    infence = False
    for i, line in enumerate(text.split("\n"), 1):
        if line.lstrip().startswith("```"):
            infence = not infence
            continue
        if infence or line.startswith("    ") or line.strip().startswith("|"):
            continue
        yield i, line


def mask_code_spans(line):
    return CODE_SPAN.sub(lambda m: "\x00" * len(m.group()), line)


def tag_sequence(html):
    return re.findall(r"</?[a-z0-9]+", html)


def stripped_text(html):
    return re.sub(r"\s+", "", re.sub(r"<[^>]+>", "", html))


def ascii_words(text):
    return Counter(re.findall(r"[A-Za-z0-9_]+", text))


# --- 単独検査 -------------------------------------------------------------


def check_task_list(files, report):
    hits = []
    for f in files:
        for n, line in enumerate(read(f).split("\n"), 1):
            if TASK_NO_SPACE.match(line):
                hits.append(f"{f}:{n}: {line.strip()[:70]}")
    report("S1 GFMタスクリストのマーカー直後の空白", hits)


def count_join_traces(files, ref=None):
    hits = []
    for f in files:
        for n, line in prose_lines(read(f, ref)):
            for m in JOIN_TRACE.finditer(mask_code_spans(line)):
                hits.append(f"{f}:{n}: …{line[max(0, m.start() - 20):m.end() + 25]}")
    return hits


def check_literal_emphasis(files, ref=None):
    hits = []
    for f in files:
        text = stripped_text(MD.render(read(f, ref)))
        for m in re.finditer(r"\*\*", text):
            hits.append(f"{f}: …{text[max(0, m.start() - 35):m.start() + 35]}")
    return hits


def check_markdown_health(files, report):
    hits, deep = [], []
    for f in files:
        src = read(f)
        if len([l for l in src.split("\n") if l.lstrip().startswith("```")]) % 2:
            hits.append(f"{f}: コードフェンスが奇数個")
        prev = 0
        infence = False
        for n, line in enumerate(src.split("\n"), 1):
            if line.lstrip().startswith("```"):
                infence = not infence
                continue
            if infence:
                continue
            m = re.match(r"^(#{1,6}) ", line)
            if not m:
                continue
            level = len(m.group(1))
            if prev and level > prev + 1:
                hits.append(f"{f}:{n}: 見出しレベルが h{prev} から h{level} へ飛んでいる")
            if level > 3:
                deep.append(f"{f}:{n}: h{level}")
            prev = level
    report("S4 Markdownの健全性（フェンスの対・見出しレベルの飛び）", hits)
    if deep:
        print(f"    （参考: h3より深い見出し {len(deep)} 件）")
        for d in deep:
            print("      " + d)


def check_links(files, report):
    hits = []
    tracked = set(subprocess.check_output(["git", "ls-files"], text=True).split())
    anchors = {}
    for f in files:
        got = set()
        for line in read(f).split("\n"):
            m = re.match(r"^#{1,6} (.+)$", line)
            if m:
                slug = re.sub(r"[^\w\- ]", "", m.group(1).lower()).strip().replace(" ", "-")
                got.add(slug)
        anchors[f] = got
    for f in files:
        base = f.rsplit("/", 1)[0] if "/" in f else ""
        for m in re.finditer(r"\[[^\]]*\]\(([^)]+)\)", read(f)):
            target = m.group(1)
            if target.startswith(("http://", "https://", "mailto:")):
                continue
            path, _, frag = target.partition("#")
            if path:
                full = path if path.startswith("/") else (f"{base}/{path}" if base else path)
                full = re.sub(r"/\./", "/", full).lstrip("./") if full.startswith("./") else full
                norm = full.rstrip("/")
                if norm not in tracked and not any(t.startswith(norm + "/") for t in tracked):
                    hits.append(f"{f}: リンク先が存在しない -> {target}")
            if frag:
                target_file = f if not path else full
                if target_file in anchors and frag not in anchors[target_file]:
                    hits.append(f"{f}: アンカーが存在しない -> {target}")
    report("S5 リンクとアンカーの生存", hits)


# --- ベースラインとの比較 -------------------------------------------------


def compare_with_base(files, base, report):
    import difflib

    files, absent = split_by_presence(files, base)
    if absent:
        print(f"--  ベースに無いファイル {len(absent)} 件（C1からC4の比較から外す）")
        for f in absent:
            print("      " + f)
        print("      S1・S2・S4・S5 は --base 無しの実行がこれらも見ている")

    bold_only, other, text, vocab = [], [], [], []
    for f in files:
        old, new = read(f, base), read(f)
        ho, hn = MD.render(old), MD.render(new)
        if tag_sequence(ho) != tag_sequence(hn):
            c = Counter(tag_sequence(hn))
            c.subtract(Counter(tag_sequence(ho)))
            delta = {k: v for k, v in c.items() if v}
            line = f"{f}: {delta}"
            (bold_only if set(delta) <= {"<strong", "</strong"} else other).append(line)
        xo, xn = stripped_text(ho), stripped_text(hn)
        if xo != xn:
            text.append(f"{f}:")
            for op, i1, i2, j1, j2 in difflib.SequenceMatcher(None, xo, xn).get_opcodes():
                if op != "equal":
                    text.append(f"    - …{xo[max(0, i1 - 20):i2 + 20]}")
                    text.append(f"    + …{xn[max(0, j1 - 20):j2 + 20]}")
        if ascii_words(old) != ascii_words(new):
            c = ascii_words(new)
            c.subtract(ascii_words(old))
            vocab.append(f"{f}: ASCII語彙の増減 {dict((k, v) for k, v in c.items() if v)}")
    print(f"--  C1 タグ列の差分: 太字のみ {len(bold_only)} ファイル / それ以外 {len(other)} ファイル")
    for line in bold_only:
        print("      [太字整理] " + line)
    for line in other:
        print("      [要確認]   " + line)
    print(f"--  C1/C2 レンダリング後テキストの差分: {sum(1 for t in text if not t.startswith(' '))} ファイル")
    for line in text:
        print("      " + line)
    report("C3 ASCII語彙の多重集合", vocab)

    now, before = len(count_join_traces(files)), len(count_join_traces(files, base))
    report(
        f"C4 行結合の痕跡が増えていないこと（変更前 {before} 件 / 現在 {now} 件）",
        [] if now <= before else count_join_traces(files),
    )
    en, eo = check_literal_emphasis(files), check_literal_emphasis(files, base)
    report(
        f"S3 閉じない強調が増えていないこと（変更前 {len(eo)} 件 / 現在 {len(en)} 件）",
        en if len(en) > len(eo) else [],
    )
    if en:
        print(f"    （既知の未修正 {len(en)} 件）")
        for h in en:
            print("      " + h)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", help="比較対象のGitリビジョン")
    ap.add_argument(
        "--with-s3",
        action="store_true",
        help="S3（閉じない強調）も走らせる。既定では走らせない（理由は冒頭のdocstring）",
    )
    args = ap.parse_args()

    files = list_files()
    failed = []

    def report(name, hits):
        if hits:
            failed.append(name)
            print(f"NG  {name}  ({len(hits)} 件)")
            for h in hits:
                print("      " + h)
        else:
            print(f"OK  {name}")

    print(f"対象 {len(files)} ファイル")
    check_task_list(files, report)
    if not args.base:
        report("S2 行結合の痕跡", count_join_traces(files))
        if args.with_s3:
            report("S3 閉じない強調", check_literal_emphasis(files))
    check_markdown_health(files, report)
    check_links(files, report)
    if args.base:
        compare_with_base(files, args.base, report)

    print()
    if failed:
        print(f"失敗 {len(failed)} 項目: " + " / ".join(failed))
        return 1
    print("すべて通過")
    return 0


if __name__ == "__main__":
    sys.exit(main())
