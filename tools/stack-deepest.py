#!/usr/bin/env python3
"""起動時のカーネルスタックの最深経路を実測する（2026-09-23）。**カーネルは変えない。**

# なぜ要るのか

**`stack-water:` の行は「どれだけ使ったか」を言うが、「どの経路で」を言わない。** **高水位が動いたとき、
どの関数の枠が効いたかを静的に当てると外れる**——**`ADR-0068` の HW-a と HW-b で、2 度とも最深の経路の
上に無い関数へ帰した**（`ADR-0068` の「起動時のスタックの最深経路」）。**最深の経路を決め直すときは、
これで測る。** **枠の大きさの比べは `tools/frame-sizes.py` が持つ。**

# 測り方

1. **プロンプトまで起こして止め、カーネルスタックの物理メモリを monitor の `pmemsave` で読む。**
   **カーネルは切り替えの直後に未使用側を 0xA5 で塗る**（`kernel::stack`）**ので、塗りの残っていない
   最も低い番地が、そこまでの最深である。**
2. **深さごとに起こし直し、その深さの 8 バイトへ gdbstub の書き込みの見張り（`Z2`）を置いて走らせる。**
   **塗り（`memset`）で止まった回は続け、それ以外で最初に止まった瞬間の生きたスタックを読む。**
3. **`.debug_frame` の CFA で巻き戻す**（前置きの `sub rsp` は読まない）。

**見る深さは、serial の `stack-water:` の行が言う値と、1 のプロンプトの時点の値である**（`--depth` で選べる）。

**gdb は入っていないので、gdb のリモートの手順（RSP）を最小限だけ話す。**

# 使い方

    cargo xtask run --boot-log-diff             # 既定の像を target/esp と target/disk0.img へ置く
    python3 tools/stack-deepest.py              # プロンプトの時点と、stack-water の行の深さ
    python3 tools/stack-deepest.py --depth 66816

**像は直前に置かれたものを使う**（`tools/qemu-variants.py` と同じ注意。**どの像だったかは `build:` の行に出る**）。
**起こし方は `xtask` の既定と同じ**（`-machine pc -m 256M -smp 2`）。**作業物は `target/stack-deepest/` へ置く。**
**`target/` を `xtask` と共有するので、`--full` と並べて走らせない。**

**何も主張しない。** **`cargo xtask check` は回さない。** **人が読むためのものである。**
"""
import argparse
import bisect
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import time
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
OUT = os.path.join(ROOT, "target", "stack-deepest")
OVMF_CODE = "/usr/share/OVMF/OVMF_CODE_4M.fd"
OVMF_VARS = "/usr/share/OVMF/OVMF_VARS_4M.fd"
ANSI = re.compile(r"\x1b\[[0-9;]*m")
FILL = 0xA5
GUARD, SIZE = 4096, 128 * 1024
WAIT_LIMIT = 180
ELF = os.path.join(ROOT, "target", "esp", "zaytos", "kernel.elf")


def tool(args):
    return subprocess.run(args, capture_output=True, text=True, check=True).stdout


# --- ELF から: 関数の範囲・呼び出しの戻り先・CFA ---
def load_elf():
    starts, names, return_sites = [], [], set()
    previous_call = False
    for line in tool(["objdump", "-d", "--no-show-raw-insn", "-C", ELF]).splitlines():
        header = re.match(r"^([0-9a-f]+) <(.*)>:$", line)
        if header:
            starts.append(int(header.group(1), 16))
            names.append(header.group(2))
            previous_call = False
            continue
        insn = re.match(r"^\s*([0-9a-f]+):\s+(\S+)", line)
        if insn:
            if previous_call:
                return_sites.add(int(insn.group(1), 16))
            previous_call = insn.group(2).startswith("call")
    fdes = []
    for block in tool(["readelf", "--debug-dump=frames-interp", ELF]).split("\n\n"):
        header = re.search(r"FDE cie=\S+ pc=([0-9a-f]+)\.\.([0-9a-f]+)", block)
        if header:
            rows = [(int(a, 16), int(b)) for a, b in re.findall(r"^([0-9a-f]{16}) rsp\+(\d+)", block, re.M)]
            fdes.append((int(header.group(1), 16), int(header.group(2), 16), rows))
    fdes.sort()
    stacks = int(next(line for line in tool(["nm", ELF]).splitlines() if line.endswith("5stack6STACKS")).split()[0], 16)
    return starts, names, return_sites, fdes, stacks


STARTS, NAMES, RETURN_SITES, FDES, STACKS = load_elf()
FDE_STARTS = [fde[0] for fde in FDES]
BOTTOM, TOP = STACKS + GUARD, STACKS + GUARD + SIZE


def function_of(address):
    index = bisect.bisect_right(STARTS, address) - 1
    return NAMES[index] if index >= 0 else "?"


def cfa_offset(pc):
    index = bisect.bisect_right(FDE_STARTS, pc) - 1
    if index < 0 or pc >= FDES[index][1]:
        return None
    offset = None
    for loc, value in FDES[index][2]:
        if loc <= pc:
            offset = value
    return offset


def launch(work, extra):
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    shutil.copytree(os.path.join(ROOT, "target", "esp"), os.path.join(work, "esp"))
    shutil.copy(os.path.join(ROOT, "target", "disk0.img"), os.path.join(work, "disk0.img"))
    shutil.copy(OVMF_VARS, os.path.join(work, "vars.fd"))
    args = [
        "qemu-system-x86_64", "-machine", "pc", "-m", "256M", "-smp", "2",
        "-drive", f"if=pflash,format=raw,readonly=on,file={OVMF_CODE}",
        "-drive", f"if=pflash,format=raw,file={work}/vars.fd",
        "-drive", f"format=raw,file=fat:rw:{work}/esp",
        "-drive", f"if=none,id=disk0,format=raw,file={work}/disk0.img",
        "-device", "virtio-blk-pci,drive=disk0",
        "-serial", f"file:{work}/serial.log", "-display", "none", "-no-reboot", "-no-shutdown",
    ] + extra
    log = open(os.path.join(work, "qemu.out"), "w")
    return subprocess.Popen(CAPPED + args, stdout=log, stderr=subprocess.STDOUT,
                            start_new_session=True)


def serial_text(work):
    path = os.path.join(work, "serial.log")
    return ANSI.sub("", open(path, "rb").read().decode("utf-8", "replace")) if os.path.exists(path) else ""


def dump_at_prompt():
    """プロンプトまで起こし、スタックを読んで、塗りの残っていない最も低い深さを返す。"""
    work = os.path.join(OUT, "prompt")
    sock = f"/tmp/zaytos-stack-deepest-{os.getpid()}.sock"
    child = launch(work, ["-monitor", f"unix:{sock},server,nowait"])
    dump = os.path.join(work, "stack.bin")
    try:
        started = time.time()
        while time.time() - started < WAIT_LIMIT:
            text = serial_text(work)
            if "zash: ready" in text or "halting" in text:
                break
            time.sleep(0.5)
        else:
            sys.exit(f"neither the prompt nor a halt within {WAIT_LIMIT} s")
        high = re.search(r"high-half: kernel image (0x[0-9a-f]+)\.\.0x[0-9a-f]+ is also mapped at (0x[0-9a-f]+)", text)
        offset = int(high.group(2), 16) - int(high.group(1), 16)
        monitor = socket.socket(socket.AF_UNIX)
        monitor.connect(sock)
        monitor.sendall(b"stop\n")
        time.sleep(1)
        # **ファイル名は引用する**（引用しないと `/` を割り算として読む。HW-a で踏んだ）。
        monitor.sendall(f'pmemsave {BOTTOM - offset:#x} {SIZE} "{dump}"\n'.encode())
        time.sleep(3)
        monitor.close()
    finally:
        stop_group(child)
        child.wait(timeout=30)
        if os.path.exists(sock):
            os.remove(sock)
    data = open(dump, "rb").read()
    lowest = next(index for index, byte in enumerate(data) if byte != FILL)
    return SIZE - lowest, text


class Remote:
    """gdb のリモートの手順（RSP）の最小限。"""

    def __init__(self, port):
        for _ in range(100):
            try:
                self.sock = socket.create_connection(("127.0.0.1", port), timeout=900)
                break
            except OSError:
                time.sleep(0.2)
        else:
            sys.exit("could not connect to the gdbstub")
        self.buffer = b""

    def send(self, payload):
        self.sock.sendall(f"${payload}#{sum(payload.encode()) % 256:02x}".encode())
        while b"#" not in self.buffer or len(self.buffer) < self.buffer.index(b"#") + 3:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("the gdbstub closed")
            self.buffer += chunk
        self.buffer = self.buffer[self.buffer.index(b"$"):]
        end = self.buffer.index(b"#")
        packet, self.buffer = self.buffer[1:end].decode(), self.buffer[end + 3:]
        self.sock.sendall(b"+")
        return packet

    def read(self, address, length):
        data = b""
        while length > 0:
            size = min(length, 1024)
            data += bytes.fromhex(self.send(f"m{address:x},{size:x}"))
            address, length = address + size, length - size
        return data

    def rsp_rip(self):
        values = struct.unpack_from("<17Q", bytes.fromhex(self.send("g")))
        return values[7], values[16]


def unwind(remote, rsp, rip):
    """生きたスタックを CFA で巻き戻す。**表の無い葉（`memcpy` など）で止まっていたら、最初の戻り先を探してから辿る。**"""
    stack = remote.read(rsp, TOP - rsp)
    word = lambda at: struct.unpack_from("<Q", stack, at - rsp)[0]
    chain = [(TOP - rsp, rip, function_of(rip), None)]
    pc, sp = rip, rsp
    if cfa_offset(pc) is None:
        at = sp
        while at + 8 <= TOP and word(at) not in RETURN_SITES:
            at += 8
        if at + 8 > TOP:
            return chain
        pc, sp = word(at), at + 8
        chain.append((TOP - at, pc, function_of(pc), None))
    while True:
        offset = cfa_offset(pc)
        if offset is None:
            return chain
        cfa = sp + offset
        if cfa > TOP:
            return chain
        # 1 つ前の関数（いま `pc` が居る関数）の枠の大きさを、その行に添える。
        chain[-1] = chain[-1][:3] + (offset - 8,)
        ra = word(cfa - 8)
        if ra not in RETURN_SITES:
            return chain
        chain.append((TOP - (cfa - 8), ra, function_of(ra), None))
        pc, sp = ra, cfa


def watch(depth, port):
    """この深さへ最初に書いた瞬間の経路を返す。"""
    work = os.path.join(OUT, f"watch-{depth}")
    child = launch(work, ["-S", "-gdb", f"tcp:127.0.0.1:{port}"])
    watched = (TOP - depth) - (TOP - depth) % 8
    try:
        remote = Remote(port)
        remote.send("?")
        if remote.send(f"Z2,{watched:x},8") != "OK":
            sys.exit("the gdbstub refused the write watchpoint")
        for _ in range(20):
            remote.send("c")
            rsp, rip = remote.rsp_rip()
            name = function_of(rip)
            if "memset" in name:
                continue  # 塗り
            chain = unwind(remote, rsp, rip)
            remote.sock.sendall(b"$k#6b")
            return chain
        sys.exit("the watchpoint kept stopping in the paint")
    finally:
        time.sleep(0.5)
        stop_group(child)
        child.wait(timeout=30)


def main():
    parser = argparse.ArgumentParser(description="起動時のカーネルスタックの最深経路を実測する")
    parser.add_argument("--depth", type=int, action="append", help="見る深さ（バイト。繰り返せる）")
    options = parser.parse_args()
    os.makedirs(OUT, exist_ok=True)

    prompt_depth, text = dump_at_prompt()
    build = re.search(r"build: [^\n]*", text)
    print(f"image: {build.group(0) if build else '?'}")
    lines = re.findall(r"stack-water: [^\n]*", text)
    for line in lines:
        print(f"serial: {line}")
    print(f"at the prompt the kernel stack had been used down to {prompt_depth} byte(s) (read from the paint)")

    depths = options.depth or sorted({prompt_depth} | {int(v) for v in re.findall(r"used (\d+) of", "\n".join(lines))})
    for depth in depths:
        print(f"\n=== the first write at depth {depth} (frame sizes in parentheses; from .debug_frame)")
        for at, address, name, frame in reversed(watch(depth, 12000 + os.getpid() % 1000)):
            size = f"({frame})" if frame is not None else ""
            print(f"  {at:7d}  {address:#x}  {name[:130]} {size}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
