#!/usr/bin/env python3
"""検査の錠の Python の側（2026-09-25。検査の体系の改善の ③。運用者の決定）。

**本体は `xtask/src/check_lock.rs` である。** **ここは手で使う道具（`tools/`）が QEMU と VirtualBox を
起こす前に、同じ錠を共有で取るためのものである。** **全検査の間は断る**（終了の値 75。待たない）。

# 道は `xtask` と同じ決め方にする

**`<git rev-parse --git-common-dir>/zaytos/check.lock`。** **根はこのファイルの在り処から決め、git は
`GIT_*` を外して呼ぶ**——**環境変数で道が変わると、錠が 2 つになって排他が黙って外れる**（運用者の
回答 3）。**同じ道になることは、基底の確かめが本の木・作業木・環境を減らした子で見る。**

# 全検査の中から呼ばれたときは取らない

**`ZAYTOS_CHECK_LOCK_OWNER` の pid が自分の祖先で、`/proc/locks` でこの錠を持っているときだけ**
取らずに進む（`xtask` と同じ規則）。

# VirtualBox の VM を起こしたまま残すとき

**`tools/vbox-vm.py start` は VM を起こしたまま終わる**——**錠を持ち続けられない。** **そこで錠の
置き場に「起こしたまま」の印を残し、`stop` と `delete` で消す。** **全検査の入口は、印が在れば断る。**
**VM は Windows 側で走るので `/proc` では見えず、一覧を読む操作は使わない決まりである。**

# 使い方（確かめ用）

    python3 tools/check_lock.py path [--root DIR]
    python3 tools/check_lock.py try FILE    # 一時の錠を共有で取ってみる（取れれば 0、断られれば 75）
"""
import fcntl
import os
import subprocess
import sys
import time

OWNER_ENV = "ZAYTOS_CHECK_LOCK_OWNER"
REFUSED_EXIT_CODE = 75
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

#: 取った錠（プロセスが終わるまで開いたまま持つ。閉じると放れる）。
_HELD = []

#: **確かめのときだけ**置き場を差し替える（`tools/vbox-vm.py selftest`）。**同じプロセスの中からしか
#: 変えられない**——**環境変数では変えない**（環境変数で道が変わる形そのものを避けている）。
DIR_FOR_TESTS = None


def git_common_dir(root=ROOT):
    """git の共通の置き場。**`GIT_*` を外して呼ぶ**（`GIT_DIR` だけで答えが変わる。実測）。"""
    env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    env["LC_ALL"] = "C"
    done = subprocess.run(
        ["git", "-C", root, "rev-parse", "--path-format=absolute", "--git-common-dir"],
        capture_output=True, text=True, env=env, timeout=30,
    )
    if done.returncode != 0:
        raise RuntimeError(f"git rev-parse --git-common-dir が {root} で失敗した: {done.stderr.strip()}")
    return os.path.realpath(done.stdout.strip())


def lock_dir(root=ROOT):
    if DIR_FOR_TESTS is not None:
        return DIR_FOR_TESTS
    return os.path.join(git_common_dir(root), "zaytos")


def lock_path(root=ROOT):
    return os.path.join(lock_dir(root), "check.lock")


def device_numbers(dev):
    """装置の番号を分ける（`os.major`・`os.minor` と同じ。`/proc/locks` の `08:30` と比べる）。"""
    return os.major(dev), os.minor(dev)


def holders_in(locks, identity):
    """`/proc/locks` の中身から、あるファイルに flock を持つ (pid, 取り方) を拾う。**待ちの行は読まない。**"""
    found = []
    for line in locks.splitlines():
        fields = line.split()
        if len(fields) < 6 or fields[1] != "FLOCK" or fields[3] not in ("WRITE", "READ"):
            continue
        parts = fields[5].split(":")
        if len(parts) != 3:
            continue
        try:
            key = (int(parts[0], 16), int(parts[1], 16), int(parts[2]))
            pid = int(fields[4])
        except ValueError:
            continue
        if key == identity:
            found.append((pid, "exclusive" if fields[3] == "WRITE" else "shared"))
    return found


def holders_of(handle):
    stat = os.fstat(handle.fileno())
    with open("/proc/locks", encoding="utf-8", errors="replace") as locks:
        text = locks.read()
    return holders_in(text, (*device_numbers(stat.st_dev), stat.st_ino))


def ancestors():
    """自分の祖先（親から順に。上限 64 段）。"""
    found = []
    pid = os.getppid()
    for _ in range(64):
        if pid <= 0:
            break
        found.append(pid)
        if pid == 1:
            break
        try:
            with open(f"/proc/{pid}/stat", encoding="utf-8", errors="replace") as stat:
                text = stat.read()
        except OSError:
            break
        rest = text.rsplit(")", 1)[-1].split()
        if len(rest) < 2:
            break
        pid = int(rest[1])
    return found


def covering_owner(named, ancestors_list, holders):
    """持ち主の下で走っているか（共有を求める側）。**祖先で、しかも錠を持つ pid だけ。**"""
    try:
        owner = int((named or "").strip())
    except ValueError:
        return None
    if owner in ancestors_list and any(pid == owner for pid, _ in holders):
        return owner
    return None


def probe(directory):
    """置き場のファイルシステムが flock を扱えることを確かめる（`xtask` の `probe` と同じ）。"""
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, f"probe-{os.getpid()}")
    try:
        with open(path, "a+") as first, open(path, "a+") as second:
            fcntl.flock(first, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                fcntl.flock(second, fcntl.LOCK_SH | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise RuntimeError("同じファイルを 2 度開くと、両方が錠を取れた（flock が効いていない）")
            if not any(pid == os.getpid() and mode == "exclusive" for pid, mode in holders_of(first)):
                raise RuntimeError("/proc/locks に自分の錠が見えない")
    finally:
        try:
            os.unlink(path)
        except OSError:
            pass


def _command_line(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as handle:
            return " ".join(part.decode("utf-8", "replace") for part in handle.read().split(b"\0") if part)
    except OSError:
        return "(gone)"


def refusal_message(what, path, holders, content):
    lines = [
        f"tools: the check lock is held, so `{what}` was not run (exit {REFUSED_EXIT_CODE}).",
        f"  lock: {path}",
    ]
    if not holders:
        lines.append("  holder: none listed in /proc/locks (it may have just ended; try again)")
    for pid, mode in holders:
        lines.append(f"  holder: pid {pid} ({mode}) {_command_line(pid)}")
    exclusive = [pid for pid, mode in holders if mode == "exclusive"]
    if exclusive and content.startswith(f"pid: {exclusive[0]}\n"):
        lines.append("  the full check that holds it wrote:")
        lines.extend(f"    {line}" for line in content.splitlines())
    lines.append(
        "  A full check holds the lock for its whole run, and QEMU and VirtualBox checks are refused "
        "meanwhile. Run this again after the holder ends."
    )
    return "\n".join(lines)


def log_run(what, outcome, root=ROOT):
    """走行を 1 行残す（全検査のまとめが数える）。**残せなくても止めない。**"""
    try:
        directory = lock_dir(root)
        os.makedirs(directory, exist_ok=True)
        local = time.strftime("%Y-%m-%d %H:%M:%S")
        with open(os.path.join(directory, "runs.tsv"), "a", encoding="utf-8") as runs:
            runs.write(f"{int(time.time())}\t{local}\t{os.getpid()}\t{what}\t{outcome}\t{root}\n")
    except Exception:
        pass


def attempt_shared(path):
    """共有で取ってみる。**(取れたか, 持ち主の下か, 断りの中身)** を返す。**取れたら持ち続ける。**"""
    probe(os.path.dirname(path))
    handle = open(path, "a+")
    owner = covering_owner(os.environ.get(OWNER_ENV), ancestors(), holders_of(handle))
    if owner is not None:
        handle.close()
        return True, owner, None
    try:
        fcntl.flock(handle, fcntl.LOCK_SH | fcntl.LOCK_NB)
    except BlockingIOError:
        holders = holders_of(handle)
        handle.seek(0)
        content = handle.read()
        handle.close()
        return False, None, (holders, content)
    _HELD.append(handle)
    return True, None, None


def hold_shared_or_exit(what, root=ROOT):
    """道具が QEMU か VirtualBox を起こす前に呼ぶ。**取れなければ断りを出して 75 で終える。**

    **置き場が flock を扱えなければ、検査装置の故障として止める**（`xtask` と同じ。運用者の回答 3）。
    """
    try:
        path = lock_path(root)
        taken, _, refused = attempt_shared(path)
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"harness fault: the check lock could not be used ({error})", file=sys.stderr)
        sys.exit(1)
    if taken:
        return
    log_run(what, "refused", root)
    print(refusal_message(what, path, *refused), file=sys.stderr)
    sys.exit(REFUSED_EXIT_CODE)


def vbox_marker_dir(root=ROOT):
    """「VM を起こしたまま」の印の置き場。"""
    return os.path.join(lock_dir(root), "vbox-running")


def mark_vbox_running(name, root=ROOT):
    directory = vbox_marker_dir(root)
    os.makedirs(directory, exist_ok=True)
    with open(os.path.join(directory, name), "w", encoding="utf-8") as marker:
        marker.write(f"started: {time.strftime('%Y-%m-%d %H:%M:%S')} by pid {os.getpid()}\n")


def clear_vbox_running(name, root=ROOT):
    try:
        os.unlink(os.path.join(vbox_marker_dir(root), name))
    except FileNotFoundError:
        pass


def main(argv):
    if len(argv) >= 1 and argv[0] == "path":
        root = argv[argv.index("--root") + 1] if "--root" in argv else ROOT
        print(lock_path(root))
        return 0
    if len(argv) == 2 and argv[0] == "try":
        taken, owner, refused = attempt_shared(argv[1])
        if owner is not None:
            print(f"covered by {owner}")
            return 0
        if taken:
            print("taken")
            return 0
        print(refusal_message("check_lock.py try", argv[1], *refused), file=sys.stderr)
        return REFUSED_EXIT_CODE
    print("usage: check_lock.py path [--root DIR] | try FILE", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
