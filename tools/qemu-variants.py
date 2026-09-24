#!/usr/bin/env python3
"""QEMU の機械の属性だけを変えて、既定の像を起こす（2026-09-22）。

**実機の棚卸しの道具である**（`docs/hardware-inventory.md`）。

**何も主張しない。** **出るのは変種ごとの数と行と画面で、読むのは人である**
（`tools/boot-log-compare.py` と同じ立ち位置）。**`cargo xtask check` はこれを回さない。**

# なぜ要るのか

**`xtask` の QEMU は `-m 256M` で、`-machine` を指定しない（既定の `pc`＝i440FX）。**
**実機との差が、そのまま検査の死角になる。** **実際に効いた**——**メモリを 1GiB より
大きくすると、`q35` でも `pc` でも起動の最初期に #PF で止まった**（2026-09-22。
静的な初期ページ表が恒等で [0, 1GiB) しか張っていなかった）。**`--full` の 357 項目は
どれも 256MiB で走るので、全部緑だった。**

**直した後にもう一度使う**（6GiB で起動する、`i8042=off` と `pit=off` で止まらない）。
**毎回手で組むと、起こし方がぶれる。**

# 変種の表

**起こし方は `xtask/machine-variants.txt` に在り、`xtask` と共有する**（`ADR-0068` の HW-b。
**HW-a の時点では両方に書いていた**）。**判定は `xtask` だけが持つ。**

# 使い方

    cargo xtask run --boot-log-diff                 # 既定の像を target/esp と target/disk0.img へ置く
    python3 tools/qemu-variants.py                  # 全部の変種（1 つ 60 秒）
    python3 tools/qemu-variants.py q35-6g pc-6g     # 選んだ変種だけ
    python3 tools/qemu-variants.py --wait 45 q35-2g # 待つ秒数を変える
    python3 tools/qemu-variants.py --list           # 変種の一覧

**像は直前に置かれたものを使う。** **`--full` や破壊の回の後は、破壊の構成の像が残って
いることがある**——**先に `cargo xtask run --boot-log-diff` で既定の像を置き直すこと。**
**どの像だったかは、各変種の `build:` の行に出る。**

**`target/` と `disk0.img` を `xtask` と共有するので、`--full` と並べて走らせない**
（`CLAUDE.md` の絶対ルール 1）。**ESP・ディスク・OVMF の変数は変種ごとに
`target/hw/<変種>/` へ写してから使う**（元を汚さない）。

# 待ち方

**QEMU は子として起こし、決めた秒数の後に monitor で画面を読み戻して止める**
（`xtask` と同じ形）。**待ちは固定の秒数で、上限の無い待ちは無い。**
**画面は PNG に直して `target/hw/<変種>/screen.png` へ置く**（シリアルの無い変種で使う）。
"""
import argparse
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import time
import zlib
import signal


# **QEMU は xtask の起動の口（`xtask/src/launch.rs`）と同じ形で起こす**（2026-09-24。ホストの保護）。
# 書く側の上限を prlimit の fsize でカーネルに持たせ、SIGXFSZ を無視して起こす（越えた書き込みは
# EFBIG で失敗するだけで、コアを吐かない。WSL の core_pattern はパイプで、RLIMIT_CORE が届かない）。
# 自分の組で起こし、止めるときは組ごと SIGKILL を送る。
QEMU_FILE_LIMIT = 4 << 30
CAPPED = ["sh", "-c", "trap '' XFSZ; exec \"$@\"", "zaytos-qemu", "prlimit",
          f"--fsize={QEMU_FILE_LIMIT}", "--core=0", "--"]


def stop_group(child):
    """QEMU の組ごと SIGKILL で止める（コアを吐かない）。"""
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target", "hw")
OVMF_CODE = "/usr/share/OVMF/OVMF_CODE_4M.fd"
OVMF_VARS = "/usr/share/OVMF/OVMF_VARS_4M.fd"
ANSI = re.compile(r"\x1b\[[0-9;]*m")

TABLE = os.path.join(ROOT, "xtask", "machine-variants.txt")


def load_variants():
    """変種の名前 → (-machine, -m, シリアル, ディスク, ESP, CPU)。**`xtask` の既定は `pc-default` である。**

    **形の崩れた行は、行番号を添えて止める**（`xtask` の `parse_machine_variants` と同じ規則）。
    """
    variants = {}
    with open(TABLE, encoding="utf-8") as handle:
        for number, line in enumerate(handle, start=1):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            fields = line.split()
            if (len(fields) != 7 or fields[3] not in ("file", "none")
                    or fields[4] not in ("virtio", "none")
                    or fields[5] not in ("dir", "media") or fields[0] in variants):
                sys.exit(f"{TABLE} line {number}: not a well-formed row: {line!r}")
            variants[fields[0]] = tuple(fields[1:])
    return variants


VARIANTS = load_variants()


def ppm_to_png(ppm_path, png_path):
    """QEMU の `screendump`（P6 の PPM）を PNG へ直す。**外の道具を使わない。**"""
    data = open(ppm_path, "rb").read()
    parts = data.split(b"\n", 3)
    width, height = map(int, parts[1].split())
    pixels = parts[3]
    rows = b"".join(b"\x00" + pixels[y * width * 3:(y + 1) * width * 3] for y in range(height))

    def chunk(kind, body):
        crc = zlib.crc32(kind + body) & 0xFFFFFFFF
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", crc)

    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(rows, 6)) + chunk(b"IEND", b"")
    open(png_path, "wb").write(png)


def run_variant(name, wait):
    machine, mem, serial, disk, esp, cpu = VARIANTS[name]
    out = os.path.join(OUT, name)
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    shutil.copytree(os.path.join(ROOT, "target", "esp"), os.path.join(out, "esp"))
    shutil.copy(os.path.join(ROOT, "target", "disk0.img"), os.path.join(out, "disk0.img"))
    shutil.copy(OVMF_VARS, os.path.join(out, "vars.fd"))
    # **socket のパスは短く保つ**（`sun_path` は 108 バイト。`xtask` の `ensure_socket_path_fits`）。
    sock = f"/tmp/zaytos-variant-{name}.sock"
    if os.path.exists(sock):
        os.remove(sock)
    serial_arg = "none" if serial == "none" else f"file:{out}/serial.log"
    args = [
        "qemu-system-x86_64", "-machine", machine, "-m", mem,
        "-drive", f"if=pflash,format=raw,readonly=on,file={OVMF_CODE}",
        "-drive", f"if=pflash,format=raw,file={out}/vars.fd",
    ]
    # **CPU の欄（2026-09-24。運用者の決定）。** **`-` なら既定の qemu64（製造元は AuthenticAMD）。**
    if cpu != "-":
        args += ["-cpu", cpu]
    # **ESP の渡し方（`ADR-0068` の HW-e）。** **`media` の変種は、GPT と FAT32 を自分で書いた
    # 1 つの像を渡す**（VirtualBox と実機と同じ形）——**`fat:rw:` は QEMU だけの道である。**
    # **像は `cargo xtask image` が置く。**
    if esp == "media":
        image = os.path.join(ROOT, "target", "media", "zaytos.img")
        shutil.copy(image, os.path.join(out, "zaytos.img"))
        args += ["-drive", f"format=raw,file={out}/zaytos.img"]
    else:
        args += ["-drive", f"format=raw,file=fat:rw:{out}/esp"]
    # **ディスクが none の変種では virtio-blk を付けない**（`ADR-0068` の HW-d）。
    # **カーネルは ESP の `\zaytos\fs.img` を RAM ディスクとして使う。**
    if disk == "virtio":
        args += ["-drive", f"if=none,id=disk0,format=raw,file={out}/disk0.img",
                 "-device", "virtio-blk-pci,drive=disk0"]
    args += [
        "-serial", serial_arg, "-display", "none", "-no-reboot", "-no-shutdown",
        "-d", "int,cpu_reset", "-D", f"{out}/qemu-debug.log",
        "-monitor", f"unix:{sock},server,nowait",
    ]
    with open(os.path.join(out, "qemu.out"), "w") as log:
        child = subprocess.Popen(CAPPED + args, stdout=log, stderr=subprocess.STDOUT,
                                 start_new_session=True)
        try:
            time.sleep(wait)
            if child.poll() is None:
                monitor = socket.socket(socket.AF_UNIX)
                monitor.connect(sock)
                monitor.sendall(f"screendump {out}/screen.ppm\n".encode())
                time.sleep(3)
                monitor.close()
        finally:
            stop_group(child)
            child.wait(timeout=30)
    if os.path.exists(sock):
        os.remove(sock)
    if os.path.exists(os.path.join(out, "screen.ppm")):
        ppm_to_png(os.path.join(out, "screen.ppm"), os.path.join(out, "screen.png"))
    report(name, machine, mem, serial, out)


def report(name, machine, mem, serial, out):
    print(f"=== {name} (-machine {machine} -m {mem} -serial {serial})")
    serial_log = os.path.join(out, "serial.log")
    if os.path.exists(serial_log):
        text = ANSI.sub("", open(serial_log, "rb").read().decode("utf-8", "replace"))
        lines = text.splitlines()
        build = next((line for line in lines if "build:" in line), "(no build line)")
        print(f"  {build.strip()[:160]}")
        print(f"  lines={len(lines)} zash_ready={text.count('zash: ready')} halting={text.count('halting')}")
        for line in [line for line in lines if "ERROR" in line or "halting" in line][:3]:
            print("  err:", line[:220])
        for line in lines[-2:]:
            print("  last:", line[:220])
    else:
        print(f"  no serial log; the screen is at {out}/screen.png")
    debug_log = os.path.join(out, "qemu-debug.log")
    debug = open(debug_log, "rb").read().decode("utf-8", "replace") if os.path.exists(debug_log) else ""
    print(f"  CPU Reset count in the debug log: {debug.count('CPU Reset')}")
    sys.stdout.flush()


def main():
    parser = argparse.ArgumentParser(description="QEMU の機械の属性だけを変えて、既定の像を起こす")
    parser.add_argument("variants", nargs="*", help="変種の名前（省くと全部）")
    parser.add_argument("--wait", type=int, default=60, help="画面を読み戻すまでの秒数（既定 60）")
    parser.add_argument("--list", action="store_true", help="変種の一覧を出して終わる")
    options = parser.parse_args()
    if options.list:
        # **表の欄は 6 つである**（2026-09-24 に CPU の欄を足した）。**5 つで開いていたので、
        # 足した後ずっと `--list` が落ちていた**（2026-09-25 に基底の確かめを置いて見つけた）。
        for name, (machine, mem, serial, disk, esp, cpu) in VARIANTS.items():
            print(
                f"{name}: -machine {machine} -m {mem} -serial {serial} "
                f"disk={disk} esp={esp} cpu={cpu}"
            )
        return 0
    for name in options.variants:
        if name not in VARIANTS:
            print(f"unknown variant {name!r}; see --list", file=sys.stderr)
            return 2
    for need in (os.path.join(ROOT, "target", "esp"), os.path.join(ROOT, "target", "disk0.img")):
        if not os.path.exists(need):
            print(f"{need} is missing; run `cargo xtask run --boot-log-diff` first", file=sys.stderr)
            return 2
    for name in options.variants or list(VARIANTS):
        run_variant(name, options.wait)
    return 0


if __name__ == "__main__":
    sys.exit(main())
