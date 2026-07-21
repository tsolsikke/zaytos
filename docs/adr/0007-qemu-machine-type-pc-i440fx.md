# 0007. QEMUのmachine typeにpc（i440FX）を採用する

## Status
Accepted

## Date
2026-07-19

## Context
M1で`cargo xtask run`から実際にUEFIアプリ（kernel.efi）を起動できるようにする過程で、QEMUのmachine typeとして最初に`q35`（PCIe世代のチップセットで、AHCI/virtio経由でのドライブ接続が前提）を選んだ。

しかし、ESP相当のディレクトリをQEMUの`fat:`ドライバで仮想FATドライブとして渡し、`if=virtio`でアタッチしても、OVMFはそのドライブを既定の起動先候補として認識せず、組み込みのUEFI Interactive Shellの待機画面（`Shell>`）で止まり続けた。
QEMUモニタの`info block`では`ide0-hd0` / `virtio`いずれの経路でもドライブ自体は正しく認識されていたため、原因はドライブの接続方式（AHCI/virtioという「固定ディスク」的な接続）と、このOVMFビルドの起動時挙動（後述、ADR-0007ではなくtroubleshooting.md参照）の組み合わせにあると判断した。

machine typeをデフォルトの`pc`（i440FX + PIIX、レガシーIDE）に変更し、ESPドライブを素のIDE接続（`if=`省略時のデフォルト）にしたところ、OVMFがドライブを認識し、シェルの`startup.nsh`経由でチェインロードした`kernel.efi`が正しく起動することを確認した（詳細な起動シーケンスの調査はdocs/troubleshooting.mdに記録）。

## Decision
QEMUのmachine typeにはデフォルトの`pc`（i440FX + PIIX、レガシーIDE）を採用する。
`cargo xtask`は`-machine`オプションを明示的に指定しない（QEMUのデフォルトに委ねる）。

## Alternatives Considered
- **q35のまま起動経路の問題を解決する**: AHCIコントローラを明示的に追加する（`-device ahci` + `-device ide-hd,bus=ahci.0,...`）、または実際にGPT + ESPパーティションを持つディスクイメージを`mtools`/`mkfs.vfat`等で事前に作成し、それを`if=virtio`で渡す、といった手段は試していない（未達）。q35自体が悪いのではなく、単純な`fat:`ディレクトリ + virtioの組み合わせでは今回うまくいかなかっただけの可能性が高い。M1の時点ではブート確認を最優先し、より簡単に動作した`pc`を採用した。

## Consequences
- 現状はレガシーIDE + i440FXという、実機の最新UEFIシステムとは異なる構成でテストすることになる。x86_64アーキテクチャ自体の学習（ADR-0006）には影響しないが、APIC・HPET・PCIeパススルーなどq35/ICH9世代のチップセット固有機能を扱う段階（M4の割り込み・タイマ回り、あるいはそれ以降）になったら、q35への移行を再検討すること。
- 移行時は、本ADRのContextに記した「OVMFがドライブを起動先として認識しない」問題を改めて解決する必要がある（AHCI明示接続or実ディスクイメージ化などを試す）。
- 現時点では実害はない。M1〜M3 (画面描画まで)は割り込み・APIC等に依存しないため、pc/i440FXのままで支障なく進められる。
