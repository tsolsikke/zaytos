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

**全検査（`cargo xtask full`）を止めたとき、検査の錠は自分で放れる**
（flock はプロセスが終われば、SIGKILL でもカーネルが放す。2026-09-25）——
**錠のファイルを消す手順は要らない。** **錠の持ち主は `cargo xtask full --status`
で見る。** **QEMU の子だけが残ることはある**（錠では見えない）——**上の 1 で
`qemu` が出たら、同じ手順で止める。** **次の `cargo xtask full` と
`cargo xtask check --commit` は、残った QEMU が在れば断る。**
