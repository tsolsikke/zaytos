# 0033. virtioとはlegacy interfaceで話す

## Status
Accepted

## Date
2026-08-17

## Context

S13-bでvirtio-blkのvirtqueueを立てる。transitionalな装置はlegacy（virtio 0.9.5）とmodern（virtio 1.0以降）の両方の話し方を受けるので、どちらで話すかを決める必要がある。

S13-aの実測（QEMU 8系、i440FX、`virtio-blk-pci`の既定構成）:

- 装置はtransitionalである（`disable-legacy=off` / `disable-modern=false`。`info qtree`で観測）
- legacyの資源はBAR0のI/Oポート（実測`0xc000`）
- modernの資源はBAR4のMMIO（実測`0xc000000000`。1TB付近の64bit prefetchable）

direct map窓が覆うのは物理RAM（256MiB）とAPICのMMIOだけで、BAR4はその外にある。modernで話すには、この領域への写像を新しく張る必要がある。legacyで話すなら、ポートI/Oの土台（S13-aの`0xCF8`/`0xCFC`で実績がある`common::port`）だけで足り、写像は一切要らない。

## Decision

**virtio装置とはlegacy interface（BAR0のI/Oポート）で話す。**

- 初期化の握手・feature交渉・queueの設定・notifyのすべてをBAR0経由で行う
- featureは何も受けずに交渉する（装置側のfeature bitsは読んで判定行に出す。観測はする）
- リングの物理アドレスはQueueAddressレジスタへPFN（物理アドレス右シフト12）で渡す

決めないこと（ADR-0025と同じ形。各段の着手時に決める）:

- **割り込みの方式（INTx / MSI-X）**——S13-dの着手時。**transitionalな装置はlegacyで話してもMSI-Xのcapabilityを持つ**（S13-aの実測で`vectors = 2`）。**legacyを選んだことはINTxを選んだことにならない。** IO-APICのpolarity設定の非対称は`deferred-decisions.md`の「MADTは極性とトリガを読めるのに、I/O APICへ設定する側が無い」の行にある
- **ブロック層の境界（全像ロード / ブロックキャッシュ）**——S13-cの着手時
- **書き込みの経路（`VIRTIO_BLK_T_OUT`、flushの扱い）**——S13-e
- **複数キュー・複数リクエスト**——要る場面が出るまで

**いずれも本ADRのDecisionからは導かれない**——legacyで話すことは、割り込みの届き方・像の供給のされ方・要求の種類・キューの本数を何も制約しない。導かれるなら、それは決めていることである。

## Alternatives Considered

**modern interface（BAR4のMMIO）——却下。**

- BAR4（`0xc000000000`）はdirect mapの外で、写像を新しく張る必要がある。`deferred-decisions.md`の「4KiBを1枚張る経路が2つあることの統合」が示すとおり、同じ操作の実装は既に2経路あり、**3つ目を作らない**
- modernが要るのはvirtio 1.0でしか使えない機能（packed queueなど）を使うときで、S13の範囲（1本のsplit queueで読み書きする）には無い
- 「新しい方が正しい」は採用理由にならない。要る土台が小さい方を採る（`docs/vision.md`の「要らないものを先回りで置かない」）

## Consequences

得るもの:

- MMIOの写像が要らない。ポートI/Oだけで完結し、S13-bの範囲が縮む
- リングとバッファはRAM上にあり、direct map越しに読み書きできる（`DirectMap::phys_to_virt`）

代償と、当たらない理由:

- **QueueAddressがPFN（u32）なので、リングは物理16TB未満に要る。** 現在のRAMは256MiBで当たらない。**ただしこれは現在の構成への依存であって、性質ではない**——`-m`を変えても16TBは超えないが、仮定であることをここに書き残す
- **feature交渉が32bitに限られる。** 受けるfeatureが無いので当たらない。受けたいfeatureが上位32bitに現れたら、それは再訪条件の発火である

再訪条件: **virtio 1.0でしか使えない機能が要るとき**（packed queue、上位のfeature bit、64bitのqueueアドレス指定など）。そのときこのADRをSupersedeし、modernへの移行と写像の経路の判断を一緒に行う。
