# 0007. QEMU の machine type に pc（i440FX）を採用する

## Status
Accepted

## Date
2026-07-19

## Context
M1 で `cargo xtask run` から実際に UEFI アプリ（kernel.efi）を起動できる
ようにする過程で、QEMU の machine type として最初に `q35`（PCIe 世代の
チップセットで、AHCI/virtio 経由でのドライブ接続が前提）を選んだ。

しかし、ESP 相当のディレクトリを QEMU の `fat:` ドライバで仮想 FAT ドライブ
として渡し、`if=virtio` でアタッチしても、OVMF はそのドライブを既定の
起動先候補として認識せず、組み込みの UEFI Interactive Shell の待機画面
（`Shell>`）で止まり続けた。QEMU モニタの `info block` では `ide0-hd0` /
`virtio` いずれの経路でもドライブ自体は正しく認識されていたため、原因は
ドライブの接続方式（AHCI/virtio という「固定ディスク」的な接続）と、この
OVMF ビルドの起動時挙動（後述、ADR-0007 ではなく troubleshooting.md 参照）
の組み合わせにあると判断した。

machine type をデフォルトの `pc`（i440FX + PIIX、レガシー IDE）に変更し、
ESP ドライブを素の IDE 接続（`if=` 省略時のデフォルト）にしたところ、
OVMF がドライブを認識し、シェルの `startup.nsh` 経由でチェインロードした
`kernel.efi` が正しく起動することを確認した（詳細な起動シーケンスの調査は
docs/troubleshooting.md に記録）。

## Decision
QEMU の machine type にはデフォルトの `pc`（i440FX + PIIX、レガシー IDE）
を採用する。`cargo xtask` は `-machine` オプションを明示的に指定しない
（QEMU のデフォルトに委ねる）。

## Alternatives Considered
- **q35 のまま起動経路の問題を解決する**: AHCI コントローラを明示的に
  追加する（`-device ahci` + `-device ide-hd,bus=ahci.0,...`）、または
  実際に GPT + ESP パーティションを持つディスクイメージを `mtools`/
  `mkfs.vfat` 等で事前に作成し、それを `if=virtio` で渡す、といった手段は
  試していない（未達）。q35 自体が悪いのではなく、単純な `fat:` ディレクトリ
  + virtio の組み合わせでは今回うまくいかなかっただけの可能性が高い。
  M1 の時点ではブート確認を最優先し、より簡単に動作した `pc` を採用した。

## Consequences
- 現状はレガシー IDE + i440FX という、実機の最新 UEFI システムとは異なる
  構成でテストすることになる。x86_64 アーキテクチャ自体の学習（ADR-0006）
  には影響しないが、APIC・HPET・PCIe パススルーなど q35/ICH9 世代の
  チップセット固有機能を扱う段階（M4 の割り込み・タイマ回り、あるいは
  それ以降）になったら、q35 への移行を再検討すること。
- 移行時は、本 ADR の Context に記した「OVMF がドライブを起動先として
  認識しない」問題を改めて解決する必要がある（AHCI 明示接続 or 実ディスク
  イメージ化などを試す）。
- 現時点では実害はない。M1〜M3 (画面描画まで) は割り込み・APIC 等に
  依存しないため、pc/i440FX のままで支障なく進められる。
