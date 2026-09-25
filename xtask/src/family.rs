//! 全検査の項目の族（2026-09-26。検査の体系の改善の、族にまとめる段。運用者の決定）。
//!
//! **どの項目も族を 1 つ名乗る**——**項目の見出しを出す口（`begin_item`）が族を取るので、名乗らない
//! 項目は建たない。** **族の分け方は、項目の見出しの形で 409 項目を振り分けて決めた**（2026-09-26。
//! 他の走行が無い全検査 `0249d12` のログ）。**当たらなかった 3 本の入れ先も運用者の決定である**——
//! `gen-font` は基底、書く側の上限は手の道具、FS/GS の破壊は割り込み（コードの在り処が例外・
//! クリティカルの塊の中）。

/// 全検査の項目の族（12）。**順序は全検査の項目の並びにおおよそ合わせた。**
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    /// 基底の静的な検査（どの選び方でも毎回回る）。
    Base,
    /// 起動ログの突き合わせ・機械の変種・起動媒体の像・higher-half・panic・トランポリン。
    Boot,
    /// 手で使う道具の確かめ（`stack-deepest`・calibration・screenshot）と書く側の上限。
    Harness,
    /// 例外・クリティカル・割り込み・APIC・IO-APIC・LAPIC タイマ・ACPI・FS/GS の破壊。
    Interrupts,
    /// ページング・スタック。
    Memory,
    /// BKL・AP・percpu・シリアルの並走。
    Smp,
    /// タスク・Ring 3・システムコール・FP・並行・`.bss`。
    Process,
    /// pipe・socket・poll・入力・画面・合成。
    Ipc,
    /// シェル・台本・UTF-8・profile・history・補完・ANSI・キー配列・環境。
    Shell,
    /// `zi`・`less`/`more`・TTF。
    Apps,
    /// ext2 の取り出し・作成・書き込み・切り詰め・ビットマップ・永続・疎な読み。
    Fs,
    /// PCI・virtio-blk・virtio の割り込み。
    Devices,
}

impl Family {
    pub const ALL: [Family; 12] = [
        Family::Base,
        Family::Boot,
        Family::Harness,
        Family::Interrupts,
        Family::Memory,
        Family::Smp,
        Family::Process,
        Family::Ipc,
        Family::Shell,
        Family::Apps,
        Family::Fs,
        Family::Devices,
    ];

    /// 記録と行に出す名前。
    pub fn name(self) -> &'static str {
        match self {
            Family::Base => "base",
            Family::Boot => "boot",
            Family::Harness => "harness",
            Family::Interrupts => "interrupts",
            Family::Memory => "memory",
            Family::Smp => "smp",
            Family::Process => "process",
            Family::Ipc => "ipc",
            Family::Shell => "shell",
            Family::Apps => "apps",
            Family::Fs => "fs",
            Family::Devices => "devices",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_family_is_listed_once_in_order_with_its_own_name() {
        let mut sorted = Family::ALL.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, Family::ALL.to_vec());
        let mut names: Vec<&str> = Family::ALL.iter().map(|family| family.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Family::ALL.len());
    }
}
