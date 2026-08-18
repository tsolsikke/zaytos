# 0035. virtioの割り込みはINTxで受ける

## Status
Accepted

## Date
2026-08-18

## Context

S13-dでvirtio-blkの完了通知を割り込みで受ける。transitionalな装置はINTx（PCIの共有線）とMSI-Xの両方を提供しており、どちらで受けるかを決める必要がある。

実測（S13-a・S13-bで観測済み）:

- virtio-blkのINTxはIRQ 11・pin A（`info pci`）。PCIのINTxはレベルトリガ・アクティブローである
- 装置はMSI-Xのcapabilityを持つ（`info qtree`で`vectors = 2`）
- **MSI-X tableはBARに住む。** 実測でvirtio-blkのBAR1は`0x810a0000`（32bitメモリBAR）で、**direct map（RAM 256MiBとAPICのMMIO）の外にある**——MSI-Xを採ると、BARの写像を新しく張る必要がある
- MADTの解釈はPCI由来のアクティブロー・レベルトリガを読める（`kernel/src/acpi/madt.rs`）が、**I/O APICのredirection entryへtrigger/polarityを設定する側が無い**（`deferred-decisions.md`の行。grepで0件の実測）

## Decision

**virtioの割り込みはINTx（IRQ 11、レベル・アクティブロー）で受ける。**

- I/O APICのredirection entryへtrigger=level / polarity=active-lowを書く側をd-1で作る。**読む側は既にある**（`gsi_for_irq`・`redirection_flags_for_irq`）ので、読めている値を書く側へ流す
- legacyのISRレジスタを読んでdeassertする（レベルトリガの要件）
- **持ち越しの「MADTは極性とトリガを読めるのに、I/O APICへ設定する側が無い」はこの決定で発火し、d-1で「済」へ動く。** MSI-Xを採っていれば、あの行は保留のまま残った

## 決めないこと（ADR-0025と同じ形）

- **キューごとのベクタ**——キューは1本である（S13の範囲）。要る場面が出るまで
- **複数装置のINTx共有の識別**——装置は1つである。2つ目の装置が同じ線に載るときに決める
- **眠りの設計**——ADR-0036（d-2）へ。**そのとき「IFの規律」の節を置くこと**——取り逃しの窓の閉じ（BKLの下での検査と遷移）が成り立つのは、**BKL保持中に同じコアへIRQが入らないときに限る**（入れば、ハンドラのacquireがスピンで待ち、保持者は戻らない）。**契約として明記するもの**: BKL保持中のコアのIFの状態（実態を実測してから書く）／BKLを解いてから切替が終わるまでの区間のIF／起こしが別コアのIRQ経由になる条件（**BSP 1コアで動かした場合に眠りから起きられるか、を含む**）

いずれも本ADRのDecisionからは導かれない——INTxで受けることは、キューの本数・装置の数・眠り方を何も制約しない。

## Alternatives Considered

**MSI-X——却下。**

- **MSI-X tableがBAR1にあり、写像を新しく張る必要がある。** ADR-0033がmodernを、ADR-0034がブロックキャッシュを却下したのと同じ判断である——要る土台が小さい方を採る
- **MSI-Xを有効にすると、legacyのレジスタ配置そのものが+4ずれる**（`config_msix_vector`と`queue_msix_vector`が`0x14`/`0x16`に入り、装置固有領域が`0x18`へ動く）。S13-b/cが読んでいるcapacityの位置が黙って変わる——**このずれは仕様からの見込みであり、MSI-Xを採る日が来たら実測で確かめること**
- INTx側の代償（レベルトリガの設定とdeassertの手間）は、既存のirq層の中の拡張で払える。MSI-X側の代償（写像・capability走査・tableの書き込み）は新しい層を要する

## Consequences

得るもの:

- 写像を張らずに済む。変更はirq層の中に収まる
- 持ち越しの非対称（読める側と設定する側）が、正面から解消される

代償と、当たらない理由:

- **INTxは共有線である。** IRQ 11に他の装置が載れば、ハンドラは発生源の識別が要る。**実測でIRQ 11にはe1000も載っている**（`info pci`。ただしe1000はドライバが無く、割り込みを発生させる状態にしない）——**当たらない根拠は「今の構成で他に鳴る者が居ない」ことであって、性質ではない。** 2つ目の鳴る装置が載るときが再訪である
- **レベルトリガはdeassertを忘れると再送が続く。** ISR読みをハンドラの経路に置き、破壊（読まない形）で捕まえることをd-1の範囲に含める

再訪条件: **同じINTx線で複数の装置が鳴るようになったとき、またはキューごとのベクタが要るとき。** そのときMSI-Xと写像の判断を一緒に再訪する（ADR-0033の再訪条件と重なる形である）。
