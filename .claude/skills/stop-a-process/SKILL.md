---
name: stop-a-process
description: 走っている QEMU や xtask を止める手順（pkill を使わず PID で止める）。プロセスを止めるときに使う
---

## プロセスを止める手順

`pkill -f` / `killall` は使わない。パターンが自分のシェルのコマンドラインに
マッチして、自分自身を殺す。`pkill -f "qemu-system-x86_64"` と
`pkill -f 'xtask check --full'` で 2 回発生している。**2 回目は「使うな」と
書いてあるこの節を読んだ上で起きた。** 禁止の形では作業中に思い出せないので、
手順として書く。次の 3 段階をそのまま実行すること。

    # 1. 存在を確認する（ここで何も出なければ、そもそも止める必要が無い）
    ps -eo pid,comm | grep -E 'qemu|xtask'

    # 2. PID を特定する
    ps -eo pid,args | grep '[q]emu-system-x86_64'

    # 3. PID を指定して止める
    kill <PID>

`grep '[q]emu...'` と書くと grep 自身がマッチしない。パターンを
ブラケットで囲むのはそのためである。

**全検査（`cargo xtask full`）を止めたとき、検査のロックは自分で放れる**
（flock はプロセスが終われば、SIGKILL でもカーネルが放す。2026-09-25）——
**ロックのファイルを消す手順は要らない。** **ロックの持ち主は `cargo xtask full --status`
で見る。** **QEMU の子だけが残ることはある**（ロックでは見えない）——**上の 1 で
`qemu` が出たら、同じ手順で止める。** **次の `cargo xtask full` と
`cargo xtask check --commit` は、残った QEMU が在れば断る。**

## 全検査と run-set を止めたら、子の組と、その下の QEMU も止まったことを確かめる

**全検査（`cargo xtask full`）の子 `xtask check --full` と run-set の子は、自分のプロセスの組で走る。**
**QEMU もそれぞれ自分の組で走る**（`xtask/src/launch.rs`）。**だから親を止めても子は止まらず、
子を止めても QEMU は止まらない。** **子が先に終わると、その QEMU は別の親（WSL では `/init` の
`Relay(…)`）へ付け替わって残る**——**2026-10-01 に全検査を止めたとき、QEMU が 3 本残った。**
**気づかないと走り続け、次の検査の資源を食う。** **全検査では、親を先に止めるのもいけない**——**錠を持つのは
親なので、子が錠の無いまま QEMU を起こし続ける。**

**run-set が上限を越えた子を止める形（`xtask/src/run_set.rs` の `stop_tree`）に倣い、次の順で止める。**

    # 1. 親・子・孫を並べる（pgid で組を、ppid で親を見る）
    ps -eo pid,ppid,pgid,etimes,args | grep -E '[x]task|[q]emu-system'

    # 2. 子の組を止める（SIGSTOP）。止まっている間は、子が新しい QEMU を起こさない
    kill -STOP -- -<子の PID>

    # 3. 孫の QEMU を止める（1 で ppid が子の PID の行）
    kill -KILL <QEMU の PID>

    # 4. 子の組を落とし、最後に親を止める
    kill -KILL -- -<子の PID>
    kill <親の PID>

    # 5. 残っていないことを確かめる
    ps -eo pid,ppid,pgid,comm | grep -E 'qemu|xtask'
    cargo xtask full --status

**5 で `qemu` か `xtask` が出たら、その PID を同じ手順で止め、もう 1 度 5 を打つ。**
**ppid が止めたプロセスのどれでもない QEMU は、付け替わった孫である**（`ps -o comm -p <ppid>` が
`Relay(…)` を出す）。**`--status` は `check lock: free` を出すこと。** **SIGTERM を送った直後は、
QEMU がまだ見えることがある**——**待たずに、次のコマンドでもう 1 度見る**（2026-10-01 は、次の
コマンドのときには消えていた）。
