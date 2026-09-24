#!/usr/bin/env python3
"""ZaytOS の起動媒体で VirtualBox の VM を作り、起こし、消す（`ADR-0068` の HW-e。2026-09-23）。

**運用者の機械の設定を触る道具である。** **だから、できることを先に狭めてある。**

# 接頭辞の無い名前は、`VBoxManage` を 1 度も呼ばずに拒む

**名前は `zaytos-` で始まり、小文字と数字と `-` だけであること**（[`NAME`]）。
**合わない名前は、どのコマンドでも、外の道具を呼ぶ前に拒む**（運用者の決定。2026-09-22）。
**`selftest` がそれを確かめる**——**`VBoxManage` を呼んだら失敗する走り手に差し替えて、
拒む名前を全部のコマンドへ渡す。** **`cargo xtask check` が毎回回す。**

**逃げ道は無い。** **`--force` も、名前の検査を飛ばす旗も置かない。**

# `list vms` を使わない

**在るかどうかは `showvminfo <名前>` で見る**——**接頭辞を確かめた名前だけを渡す。**
**`list vms` は機械の全部の VM の名前を読み出すので、使わない**（運用者の決定）。

# 置き場

**VDI と `.vbox` は `D:\\Users\\User\\VirtualBox VMs\\<名前>\\` の下に置く**（運用者の決定）。
**VirtualBox の既定の機械フォルダは `C:\\Users\\User\\VirtualBox VMs` で、こことは違う**
（実測。2026-09-23。`list systemproperties`）——**既定の設定は変えない**（ホストの設定の
変更になる）。**代わりに `createvm --basefolder` で毎回明示する。**

# 消す手順

**`delete` がこの順で行う。** **手で行うときも同じ順である。**

    1. controlvm <名前> poweroff        （走っていなければ黙って飛ばす）
    2. storageattach ... --medium none  （VDI を外す。付けたまま消すと登録が残る）
    3. closemedium disk <VDI> --delete  （登録から外し、ファイルを消す）
    4. unregistervm <名前> --delete     （.vbox と VM フォルダを消す）
    5. 道具が置いたもの（シリアルの写し・画面）を消す
    6. 残ったフォルダが空なら消す。空でなければ、残ったものの名前を出す

**`create` で VDI を作り直すときも 3 を先に行う**——**同じパス・同じ名前の媒体が
登録に残っていると、VirtualBox は「別の UUID の同じファイル」として拒む。**

# 使い方

    python3 tools/vbox-vm.py selftest
    python3 tools/vbox-vm.py create --name zaytos-hw-e --image target/media/zaytos.img
    python3 tools/vbox-vm.py run    --name zaytos-hw-e
    cargo xtask judge-vbox target/vbox/zaytos-hw-e/<時刻>
    python3 tools/vbox-vm.py start  --name zaytos-hw-e --gui
    python3 tools/vbox-vm.py log    --name zaytos-hw-e
    python3 tools/vbox-vm.py screenshot --name zaytos-hw-e
    python3 tools/vbox-vm.py stop   --name zaytos-hw-e
    python3 tools/vbox-vm.py delete --name zaytos-hw-e

# 走行の記録（2-2。2026-09-24）

**`run` は 1 回の走行を `target/vbox/<名前>/<時刻>/` に残す**——**判定はしない。** **判定は
`cargo xtask judge-vbox <その場所>` だけが持つ**（道具は起こす・打つ・採るだけである）。

- `record.txt`——git の HEAD、未コミットの変更が在るか、`git diff HEAD` の sha256、今の像の sha256、
  作ったときの記録（`create` が残す `latest-create.txt`）、VirtualBox の版
- `showvminfo.txt`——`showvminfo --machinereadable`（VM の設定）
- `serial.log`——シリアルの写し。**起こす前に古いものを消す**（前の走行と混ざらない）
- `counters-before.txt` / `counters-after.txt`——打鍵の前後の APIC のベクタごとの計数
  （`debugvm statistics`。**VM の外から数えた証拠**）
- `keys.txt`——送った打鍵（PS/2 のスキャンコード）
- `vbox-nem.txt`——`VBox.log` の `NEM` の行

**`log` は 2 つを出す**——**`VBox.log` の `NEM` の行**（Hyper-V の上で走っているかが分かる。
WSL2 が入っている機械では VirtualBox は自前の VT-x を使えない）**と、シリアルの末尾。**
"""
import argparse
import contextlib
import hashlib
import io
import os
import re
import shutil
import subprocess
import sys
import time

#: 名前の形。**`zaytos-` で始まり、続きは小文字・数字・`-` で 1〜24 文字。**
#:
#: **大文字を許さない**——**Windows のパスは大文字小文字を区別しないので、
#: `Zaytos-x` が既存の `zaytos-x` と同じフォルダを指しうる。**
NAME = re.compile(r"zaytos-[a-z0-9][a-z0-9-]{0,23}")

#: VDI と `.vbox` の置き場（運用者の決定。2026-09-22）。
DEFAULT_BASEFOLDER = r"D:\Users\User\VirtualBox VMs"

#: 記憶の制御器の名前（この道具が作る VM の中だけの名前である）。
CONTROLLER = "SATA"

#: シリアルの口（カーネルは 0x3F8 を見る）。
SERIAL_PORT, SERIAL_IRQ = "0x3f8", "4"

#: 走行の記録の置き場（リポジトリの `target/` の下。**公開物ではない**）。
RECORD_ROOT = os.path.join("target", "vbox")

#: `run` が送る打鍵: `a` と Enter の押下と解放（PS/2 のセット 1）。**4 バイトである。**
#: **`judge-vbox` は、ベクタ 0x42 の増えがこのバイト数と等しいことを見る。**
KEYS = ["1e", "9e", "1c", "9c"]

#: `run` が待つ行。**待つための合図であって、判定ではない**（判定は `judge-vbox`）。
READY_MARKERS = ["zash: ready", "halting"]
ANSWER_MARKER = "zash: a: cannot run"

#: `run` が数える計数（APIC のベクタごと。CPU をまたぐ）。
COUNTER_PATTERN = "/Devices/apic/*/Vectors/*"


class Refused(Exception):
    """この道具が断った。**外の道具は呼んでいない。**"""


def _run(args, *, check=True):
    """`VBoxManage` を呼ぶ。**`selftest` はここを差し替える。**"""
    executable = shutil.which("VBoxManage.exe") or shutil.which("VBoxManage")
    if executable is None:
        raise Refused("VBoxManage が PATH に無い（WSL から Windows 側の PATH が見えているか）")
    print(f"+ VBoxManage {' '.join(args)}", flush=True)
    done = subprocess.run([executable, *args], capture_output=True, text=True)
    if check and done.returncode != 0:
        raise Refused(f"VBoxManage {args[0]} が失敗した:\n{done.stdout}{done.stderr}")
    return done


#: 実際に呼ぶ走り手。**`selftest` が差し替える。**
RUNNER = _run


def vboxmanage(args, *, check=True):
    return RUNNER(args, check=check)


def guard(name):
    """名前を確かめる。**合わなければ、外の道具を呼ぶ前に拒む。**"""
    if not isinstance(name, str) or NAME.fullmatch(name) is None:
        raise Refused(
            f"VM の名前 {name!r} は扱わない。"
            f"この道具は {NAME.pattern!r} に完全に一致する名前だけを扱う"
            "（接頭辞の無い VM には一切触らない。運用者の決定。2026-09-22）"
        )
    return name


def windows_path(path):
    """WSL の道を Windows の道へ直す。**空白入りでも、まだ無い道でも通る**（実測。2026-09-23）。"""
    done = subprocess.run(["wslpath", "-w", str(path)], capture_output=True, text=True)
    if done.returncode != 0:
        raise Refused(f"wslpath -w {path} が失敗した: {done.stderr.strip()}")
    return done.stdout.strip()


def wsl_path(path):
    done = subprocess.run(["wslpath", "-u", str(path)], capture_output=True, text=True)
    if done.returncode != 0:
        raise Refused(f"wslpath -u {path} が失敗した: {done.stderr.strip()}")
    return done.stdout.strip()


class Vm:
    """1 つの VM の在りか。**作る前から道だけは決まる。**"""

    def __init__(self, name, basefolder):
        self.name = guard(name)
        self.basefolder_windows = basefolder
        self.basefolder = wsl_path(basefolder)
        self.folder = os.path.join(self.basefolder, self.name)
        self.vdi = os.path.join(self.folder, "zaytos.vdi")
        self.raw = os.path.join(self.folder, "zaytos-raw.img")
        self.serial = os.path.join(self.folder, "serial.log")
        self.vbox_log = os.path.join(self.folder, "Logs", "VBox.log")

    def exists(self):
        """登録に在るか。**`list vms` を使わない**——**接頭辞を確かめた名前だけを渡す。**"""
        return vboxmanage(["showvminfo", self.name], check=False).returncode == 0


def sha256_of_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_state():
    """HEAD・未コミットの変更が無いか・`git diff HEAD` の sha256。**走行を木へ結びつける。**"""
    head = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    porcelain = subprocess.run(["git", "status", "--porcelain"], capture_output=True, text=True).stdout
    diff = subprocess.run(["git", "diff", "HEAD"], capture_output=True).stdout
    return [
        f"git-head: {head or '(not a git tree)'}",
        f"git-clean: {'true' if porcelain.strip() == '' else 'false'}",
        f"git-diff-head-sha256: {hashlib.sha256(diff).hexdigest()}",
    ]


def virtualbox_version():
    return vboxmanage(["--version"], check=False).stdout.strip() or "(unknown)"


def write_lines(path, lines):
    with open(path, "w", encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")


def check_image(image):
    """像が ZaytOS の起動媒体の形であることを、繋ぐ前に見る。

    **保護 MBR の型が 0xEE で、LBA 1 に `EFI PART` が在ること。** **中身まで検める道具では
    ない**（それは `cargo xtask image` が読み返しで行う）——**別のファイルを渡した事故を
    止めるだけである。**
    """
    if not os.path.isfile(image):
        raise Refused(f"起動媒体の像 {image} が無い（先に `cargo xtask image` を回すこと）")
    with open(image, "rb") as handle:
        head = handle.read(1024)
    if len(head) < 1024 or head[450] != 0xEE or head[512:520] != b"EFI PART":
        raise Refused(f"{image} は GPT の起動媒体に見えない（保護 MBR か GPT のヘッダが無い）")


def create(vm, image, memory, cpus, replace):
    if not os.path.isdir(vm.basefolder):
        raise Refused(f"置き場 {vm.basefolder_windows} が無い（{vm.basefolder}）")
    if vm.exists() or os.path.isdir(vm.folder):
        if not replace:
            raise Refused(
                f"{vm.name} は既に在る。作り直すなら --replace を付けるか、"
                f"先に `delete --name {vm.name}` を回すこと"
            )
        delete(vm)
    check_image(image)
    vboxmanage(
        [
            "createvm",
            "--name", vm.name,
            "--ostype", "Other_64",
            "--register",
            "--basefolder", vm.basefolder_windows,
        ]
    )
    # **像を置き場へ写してから直す。** **WSL の側の道（`\\\\wsl.localhost\\...`）を
    # VirtualBox へ渡さない**——**UNC の道は遅く、失敗の切り分けが増える。**
    shutil.copyfile(image, vm.raw)
    if os.path.isfile(vm.vdi):
        # **同じパスの媒体が登録に残っていると、作り直しが拒まれる**（運用者の指示）。
        vboxmanage(["closemedium", "disk", windows_path(vm.vdi), "--delete"], check=False)
        if os.path.isfile(vm.vdi):
            os.unlink(vm.vdi)
    vboxmanage(
        ["convertfromraw", windows_path(vm.raw), windows_path(vm.vdi), "--format", "VDI"]
    )
    os.unlink(vm.raw)
    vboxmanage(
        [
            "modifyvm", vm.name,
            "--firmware", "efi64",
            "--memory", str(memory),
            "--cpus", str(cpus),
            # **PS/2 のキーボードで確かめる段である**（USB は HW-f）。
            "--keyboard", "ps2",
            "--mouse", "ps2",
            "--audio-enabled", "off",
            "--nic1", "none",
            # **USB は HW-f である**（`ADR-0068`）。**いまは明示的に切る。**
            "--usb-xhci", "off",
            "--usb-ehci", "off",
            "--usb-ohci", "off",
            "--uart1", SERIAL_PORT, SERIAL_IRQ,
            "--uart-mode1", "file", windows_path(vm.serial),
        ]
    )
    vboxmanage(
        [
            "storagectl", vm.name,
            "--name", CONTROLLER,
            "--add", "sata",
            "--controller", "IntelAhci",
            "--portcount", "1",
            "--bootable", "on",
        ]
    )
    vboxmanage(
        [
            "storageattach", vm.name,
            "--storagectl", CONTROLLER,
            "--port", "0",
            "--device", "0",
            "--type", "hdd",
            "--medium", windows_path(vm.vdi),
        ]
    )
    # **作ったときの記録を残す**（2-2）。**`run` がこれを写す**——**VM の中の像がどの木から
    # 建ったかを、走行の記録から辿れるようにする。**
    stamp = time.strftime("%Y%m%d-%H%M%S")
    folder = os.path.join(RECORD_ROOT, vm.name)
    os.makedirs(folder, exist_ok=True)
    created = [
        f"created: {stamp}",
        f"image: {image}",
        f"image-sha256: {sha256_of_file(image)}",
        *git_state(),
        f"cpus: {cpus}",
        f"memory-mib: {memory}",
        f"virtualbox: {virtualbox_version()}",
    ]
    write_lines(os.path.join(folder, f"create-{stamp}.txt"), created)
    write_lines(os.path.join(folder, "latest-create.txt"), created)
    print(f"created: {vm.name}")
    print(f"  VM フォルダ : {windows_path(vm.folder)}")
    print(f"  VDI         : {windows_path(vm.vdi)}")
    print(f"  シリアル    : {vm.serial}")
    print(f"  メモリ {memory}MiB / CPU {cpus} / firmware efi64 / キーボード ps2")


def start(vm, gui):
    if not vm.exists():
        raise Refused(f"{vm.name} が無い（先に `create` を回すこと）")
    vboxmanage(["startvm", vm.name, "--type", "gui" if gui else "headless"])
    print(f"started: {vm.name}（{'画面あり' if gui else '画面なし'}）")
    print(f"  シリアル: tail -f {vm.serial}")


def screenshot(vm, out):
    """画面を PNG で読む（シリアルの無い所で読む唯一の手段である）。"""
    if not vm.exists():
        raise Refused(f"{vm.name} が無い")
    out = out or os.path.join(vm.folder, "screen.png")
    vboxmanage(["controlvm", vm.name, "screenshotpng", windows_path(out)])
    print(f"screenshot: {out}")


def stop(vm):
    if not vm.exists():
        raise Refused(f"{vm.name} が無い")
    done = vboxmanage(["controlvm", vm.name, "poweroff"], check=False)
    if done.returncode != 0:
        print("（走っていなかった）")
    print(f"stopped: {vm.name}")


def delete(vm):
    """上の docstring の「消す手順」のとおりに消す。**途中で無かったものは飛ばす。**"""
    if vm.exists():
        vboxmanage(["controlvm", vm.name, "poweroff"], check=False)
        vboxmanage(
            [
                "storageattach", vm.name,
                "--storagectl", CONTROLLER,
                "--port", "0",
                "--device", "0",
                "--medium", "none",
            ],
            check=False,
        )
    if os.path.isfile(vm.vdi):
        vboxmanage(["closemedium", "disk", windows_path(vm.vdi), "--delete"], check=False)
    if vm.exists():
        vboxmanage(["unregistervm", vm.name, "--delete"], check=False)
    # **道具が置いたものも消す**（2-2）。**`unregistervm --delete` は VirtualBox の知っている
    # ファイルしか消さない**——**シリアルの写しと画面が残り、フォルダが空にならなかった**（実測）。
    for leftover in (vm.raw, vm.vdi, vm.serial, os.path.join(vm.folder, "screen.png")):
        if os.path.isfile(leftover):
            os.unlink(leftover)
    if os.path.isdir(vm.folder):
        remaining = sorted(os.listdir(vm.folder))
        if remaining:
            print(f"（フォルダが空にならなかった。残ったもの: {remaining}）")
        else:
            os.rmdir(vm.folder)
    print(f"deleted: {vm.name}（フォルダが{'残った' if os.path.isdir(vm.folder) else '無くなった'}）")


def wait_for(path, markers, deadline):
    """`path` に `markers` のどれかが出るまで待つ。**上限は `deadline`**（出なければ None）。"""
    while time.monotonic() < deadline:
        if os.path.isfile(path):
            with open(path, encoding="utf-8", errors="replace") as handle:
                text = handle.read()
            for marker in markers:
                if marker in text:
                    return marker
        time.sleep(1)
    return None


def read_counters(vm):
    return vboxmanage(
        ["debugvm", vm.name, "statistics", "--pattern", COUNTER_PATTERN], check=False
    ).stdout.replace("\r", "")


def run(vm, image, timeout):
    """1 回の走行を記録する（2-2）。**判定はしない**——`cargo xtask judge-vbox` が行う。

    起こす → プロンプトを待つ → 計数を読む → 打鍵を送る → シェルの答えを待つ → 計数を読む → 止める
    → 写す。**待ちには上限がある**（`timeout` 秒。出なければ、そこまでの記録を残して止める）。
    """
    if not vm.exists():
        raise Refused(f"{vm.name} が無い（先に `create` を回すこと）")
    info = vboxmanage(["showvminfo", vm.name, "--machinereadable"]).stdout.replace("\r", "")
    if 'VMState="running"' in info:
        raise Refused(f"{vm.name} は走っている。先に `stop` を回すこと（シリアルが前の走行と混ざる）")
    stamp = time.strftime("%Y%m%d-%H%M%S")
    record = os.path.join(RECORD_ROOT, vm.name, stamp)
    os.makedirs(record)
    latest = os.path.join(RECORD_ROOT, vm.name, "latest-create.txt")
    created = open(latest, encoding="utf-8").read().splitlines() if os.path.isfile(latest) else [
        "(no create record: the VM was made before 2-2 or by hand)"
    ]
    write_lines(
        os.path.join(record, "record.txt"),
        [
            f"run: {stamp}",
            f"vm: {vm.name}",
            *git_state(),
            f"current-image: {image}",
            f"current-image-sha256: {sha256_of_file(image) if os.path.isfile(image) else '(missing)'}",
            f"virtualbox: {virtualbox_version()}",
            "--- latest-create.txt ---",
            *created,
        ],
    )
    write_lines(os.path.join(record, "showvminfo.txt"), [info.rstrip()])
    if os.path.isfile(vm.serial):
        os.unlink(vm.serial)
    deadline = time.monotonic() + timeout
    vboxmanage(["startvm", vm.name, "--type", "headless"])
    try:
        ready = wait_for(vm.serial, READY_MARKERS, deadline)
        print(f"  起動: {ready or '上限までに合図が出なかった'}")
        if ready == "zash: ready":
            write_lines(os.path.join(record, "counters-before.txt"), [read_counters(vm).rstrip()])
            vboxmanage(["controlvm", vm.name, "keyboardputscancode", *KEYS])
            write_lines(os.path.join(record, "keys.txt"), [" ".join(KEYS)])
            answered = wait_for(vm.serial, [ANSWER_MARKER], deadline)
            print(f"  打鍵: {'シェルが答えた' if answered else '上限までに答えが出なかった'}")
            # **最後の解放（9c）の配送を待ってから読む。** 答えは Enter の押下で出る。
            time.sleep(1)
            write_lines(os.path.join(record, "counters-after.txt"), [read_counters(vm).rstrip()])
    finally:
        vboxmanage(["controlvm", vm.name, "poweroff"], check=False)
        # **止まりきるまで待つ**（`VBox.log` を写すため）。上限つき。
        stop_deadline = time.monotonic() + 30
        while time.monotonic() < stop_deadline:
            state = vboxmanage(["showvminfo", vm.name, "--machinereadable"], check=False).stdout
            if 'VMState="running"' not in state and 'VMState="stopping"' not in state:
                break
            time.sleep(1)
        if os.path.isfile(vm.serial):
            shutil.copyfile(vm.serial, os.path.join(record, "serial.log"))
        if os.path.isfile(vm.vbox_log):
            with open(vm.vbox_log, encoding="utf-8", errors="replace") as handle:
                nem = [line.rstrip() for line in handle if "NEM" in line]
            write_lines(os.path.join(record, "vbox-nem.txt"), nem or ["(no NEM line)"])
    print(f"recorded: {record}")
    print(f"judge:    cargo xtask judge-vbox {record}")


def log(vm, lines):
    """`VBox.log` の `NEM` の行と、シリアルの末尾を出す。"""
    if os.path.isfile(vm.vbox_log):
        print(f"=== {vm.vbox_log} の NEM の行 ===")
        with open(vm.vbox_log, encoding="utf-8", errors="replace") as handle:
            hits = [line.rstrip() for line in handle if "NEM" in line]
        print("\n".join(hits) if hits else "（NEM の行が無い）")
    else:
        print(f"（{vm.vbox_log} が無い。まだ起こしていない）")
    if os.path.isfile(vm.serial):
        print(f"=== {vm.serial} の末尾 {lines} 行 ===")
        with open(vm.serial, encoding="utf-8", errors="replace") as handle:
            for line in handle.read().splitlines()[-lines:]:
                print(line)
    else:
        print(f"（{vm.serial} が無い）")


#: `selftest` が拒まれることを確かめる名前。**接頭辞の無いものと、形の崩れたもの。**
REFUSED_NAMES = [
    "",
    "zaytos",
    "zaytos-",
    "Zaytos-hw-e",
    "zaytos-HW",
    "zaytos-hw_e",
    "zaytos-hw e",
    "zaytos-hw-e/",
    "zaytos-hw-e/..",
    "../zaytos-hw-e",
    "/zaytos-hw-e",
    "other-vm",
    "dev-machine",
    "zaytos-" + "a" * 40,
    None,
]

#: 受ける名前。
ACCEPTED_NAMES = ["zaytos-hw-e", "zaytos-a", "zaytos-media-only", "zaytos-1"]


def selftest():
    """**接頭辞の無い名前を、`VBoxManage` を呼ばずに拒むことを確かめる。**

    **走り手を「呼ばれたら落ちる」ものに差し替える**——**拒む経路で外の道具へ触れて
    いたら、この検査が落ちる。** **VirtualBox が入っていなくても走る。**
    """
    global RUNNER
    calls = []

    def explode(args, *, check=True):
        calls.append(args)
        raise AssertionError(f"VBoxManage を呼んでしまった: {args}")

    RUNNER = explode
    sink = io.StringIO()
    try:
        for name in ACCEPTED_NAMES:
            assert guard(name) == name, name
        for name in REFUSED_NAMES:
            for attempt in (
                lambda: guard(name),
                lambda: Vm(name, DEFAULT_BASEFOLDER),
            ):
                try:
                    attempt()
                except Refused:
                    continue
                raise AssertionError(f"{name!r} を受けてしまった")
        # **コマンドの入口でも拒むこと**（`main` の道を通す）。
        for name in REFUSED_NAMES:
            if name is None:
                continue
            for command in ("create", "start", "stop", "delete", "log", "screenshot", "run"):
                # **断りの文はここでは読まない**（数だけを見る。出すと検査の出力が埋まる）。
                with contextlib.redirect_stderr(sink):
                    code = main([command, "--name", name, "--image", "/dev/null"])
                assert code == 2, f"{command} {name!r} が {code} で終わった"
        assert not calls, f"VBoxManage を {len(calls)} 回呼んだ"
    finally:
        RUNNER = _run
    refusals = sink.getvalue().count("refused:")
    print(
        f"selftest: OK（受ける名前 {len(ACCEPTED_NAMES)} 個、拒む名前 {len(REFUSED_NAMES)} 個、"
        f"コマンドの入口での断り {refusals} 回、VBoxManage の呼び出し {len(calls)} 回）"
    )


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="ZaytOS の起動媒体で VirtualBox の VM を扱う（zaytos- で始まる名前だけ）",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument(
        "command",
        choices=["create", "start", "stop", "delete", "log", "screenshot", "run", "selftest"],
    )
    parser.add_argument("--name", help="VM の名前（zaytos- で始まること）")
    parser.add_argument("--image", default="target/media/zaytos.img", help="起動媒体の像")
    parser.add_argument("--basefolder", default=DEFAULT_BASEFOLDER, help="VDI と .vbox の置き場")
    parser.add_argument("--memory", type=int, default=2048, help="メモリ（MiB）")
    # **既定は 4 個**（運用者の判断 3。2026-09-24）。**VirtualBox の EFI は 2 個と 3 個で落ちる**
    # （1 個と 4 個は起動する。実測。2026-09-23）。**カーネルの `MAX_CPUS` は 2 なので、4 個では
    # AP を 1 つ起こして SMP で走る。**
    parser.add_argument("--cpus", type=int, default=4, help="CPU の数（2 と 3 は EFI が落ちる）")
    parser.add_argument("--gui", action="store_true", help="画面を開いて起こす")
    parser.add_argument("--replace", action="store_true", help="在る VM を消してから作る")
    parser.add_argument("--lines", type=int, default=20, help="シリアルの末尾の行数")
    parser.add_argument("--out", help="画面の PNG の置き場（既定は VM フォルダの screen.png）")
    parser.add_argument("--timeout", type=int, default=180, help="run の待ちの上限（秒）")
    args = parser.parse_args(argv)
    try:
        if args.command == "selftest":
            selftest()
            return 0
        if args.name is None:
            raise Refused("--name が要る")
        vm = Vm(args.name, args.basefolder)
        if args.command == "create":
            create(vm, args.image, args.memory, args.cpus, args.replace)
        elif args.command == "start":
            start(vm, args.gui)
        elif args.command == "stop":
            stop(vm)
        elif args.command == "delete":
            delete(vm)
        elif args.command == "log":
            log(vm, args.lines)
        elif args.command == "screenshot":
            screenshot(vm, args.out)
        elif args.command == "run":
            run(vm, args.image, args.timeout)
        return 0
    except Refused as refused:
        print(f"refused: {refused}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
