//! VirtualBox の走行の記録を判定する（`ADR-0068` の 2-2。2026-09-24）。
//!
//! **判定は xtask だけが持つ。** **道具（`tools/vbox-vm.py` の `run`）は起こす・打つ・記録を
//! 採るだけである。** 記録の置き場は `target/vbox/<VM>/<時刻>/`（道具の doc）。
//!
//! # VM の外から数えた証拠を判定に入れる
//!
//! **カーネルの行（「最初の打鍵がベクタ 0x42 で届いた」）はカーネル自身の言い分である。**
//! **VirtualBox のデバッガの計数は、VM の外から数えた値である**（レビューの追加の条件。
//! 2026-09-23）——**送った打鍵のバイト数だけベクタ 0x42 が増え、8259 のベクタ
//! （0x20〜0x2F）が増えないこと**を見る。

use std::collections::BTreeMap;

/// I/O APIC 経由のキーボードのベクタ（`kernel/src/idt` の `IOAPIC_KEYBOARD_VECTOR`）。
pub const KEYBOARD_VECTOR: u8 = 0x42;

/// 8259 から来うるベクタの範囲（PIC を `0x20` へ再マップしている。主と従で 16 本）。
pub const LEGACY_PIC_VECTORS: std::ops::RangeInclusive<u8> = 0x20..=0x2F;

/// `VBoxManage debugvm <VM> statistics` の出力から、APIC のベクタごとの計数を拾う。
///
/// **出力は `<Counter c="N" unit="times" vis="used" name="/Devices/apic/<CPU>/Vectors/<16進>"/>`
/// の並びである**（実測。2026-09-23）。**CPU をまたいで足す**（どの CPU へ配られたかは問わない）。
/// **形の合わない要素は飛ばす**——**数えなかったものは、下の判定で「増えていない」側に倒れる**
/// ので、判定は拾えた値だけで行い、拾えた数を行に出す。
pub fn apic_vector_counts(text: &str) -> BTreeMap<u8, u64> {
    let mut counts = BTreeMap::new();
    for element in text.split("<Counter").skip(1) {
        let element = element.split("/>").next().unwrap_or("");
        let Some(count) = attribute(element, "c").and_then(|c| c.parse::<u64>().ok()) else {
            continue;
        };
        let Some(name) = attribute(element, "name") else {
            continue;
        };
        let Some((cpu, vector)) = name
            .strip_prefix("/Devices/apic/")
            .and_then(|rest| rest.split_once("/Vectors/"))
        else {
            continue;
        };
        if cpu.parse::<u32>().is_err() {
            continue;
        }
        let Ok(vector) = u8::from_str_radix(vector, 16) else {
            continue;
        };
        *counts.entry(vector).or_insert(0) += count;
    }
    counts
}

fn attribute<'a>(element: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!(" {key}=\"");
    let start = element.find(&needle)? + needle.len();
    let end = element[start..].find('"')? + start;
    Some(&element[start..end])
}

/// 送った打鍵のバイト数（`keys.txt` の 16 進の語の数）。
pub fn key_bytes(text: &str) -> usize {
    text.split_whitespace()
        .filter(|word| u8::from_str_radix(word, 16).is_ok())
        .count()
}

/// 打鍵の窓の前後の計数の差。
#[derive(Debug, PartialEq, Eq)]
pub struct CounterVerdict {
    /// キーボードのベクタの増え。
    pub keyboard_delta: u64,
    /// 8259 のベクタのうち、増えたもの（ベクタと増え）。**空であるべきである。**
    pub legacy_pic_increases: Vec<(u8, u64)>,
}

impl CounterVerdict {
    /// 打鍵の数だけキーボードのベクタが増え、8259 のベクタが 1 つも増えていないか。
    pub fn holds(&self, key_bytes: usize) -> bool {
        self.keyboard_delta == key_bytes as u64 && self.legacy_pic_increases.is_empty()
    }
}

/// 前後の計数から差を出す。**前に無かったベクタは 0 から数える**（VirtualBox は 1 度も
/// 配られていないベクタの計数を出さない。実測で 0x21 の行が無かった）。
pub fn counter_verdict(before: &BTreeMap<u8, u64>, after: &BTreeMap<u8, u64>) -> CounterVerdict {
    let delta = |vector: u8| {
        after
            .get(&vector)
            .copied()
            .unwrap_or(0)
            .saturating_sub(before.get(&vector).copied().unwrap_or(0))
    };
    CounterVerdict {
        keyboard_delta: delta(KEYBOARD_VECTOR),
        legacy_pic_increases: LEGACY_PIC_VECTORS
            .map(|vector| (vector, delta(vector)))
            .filter(|&(_, increase)| increase > 0)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-23 に `zaytos-probe-usb` で採った形（`tr -d '\r'` の後）。
    const MEASURED: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="no" ?>
<Statistics>
<Counter c="106" unit="times" vis="used" name="/Devices/apic/0/Vectors/20"/>
<Counter c="8" unit="times" vis="used" name="/Devices/apic/0/Vectors/42"/>
<Counter c="1500" unit="times" vis="used" name="/Devices/apic/0/Vectors/fe"/>
<Counter c="700" unit="times" vis="used" name="/Devices/apic/1/Vectors/fe"/>
</Statistics>
"#;

    #[test]
    fn the_measured_counter_output_is_read_per_vector_across_cpus() {
        let counts = apic_vector_counts(MEASURED);
        assert_eq!(counts.get(&0x20), Some(&106));
        assert_eq!(counts.get(&0x42), Some(&8));
        assert_eq!(counts.get(&0xfe), Some(&2200));
        assert_eq!(counts.len(), 3);
    }

    #[test]
    fn four_key_bytes_that_raise_0x42_by_four_hold() {
        let before = apic_vector_counts(MEASURED);
        let after = apic_vector_counts(&MEASURED.replace(r#"c="8""#, r#"c="12""#));
        let verdict = counter_verdict(&before, &after);
        assert_eq!(verdict.keyboard_delta, 4);
        assert!(verdict.legacy_pic_increases.is_empty());
        assert!(verdict.holds(key_bytes("1e 9e 1c 9c")));
    }

    #[test]
    fn a_keystroke_that_also_came_through_the_8259_fails() {
        let before = apic_vector_counts(MEASURED);
        let after = apic_vector_counts(&format!(
            "{}<Counter c=\"4\" unit=\"times\" vis=\"used\" name=\"/Devices/apic/0/Vectors/21\"/>",
            MEASURED.replace(r#"c="8""#, r#"c="12""#)
        ));
        let verdict = counter_verdict(&before, &after);
        assert_eq!(verdict.legacy_pic_increases, vec![(0x21, 4)]);
        assert!(!verdict.holds(4));
    }

    #[test]
    fn fewer_deliveries_than_key_bytes_fail() {
        let before = apic_vector_counts(MEASURED);
        let after = apic_vector_counts(&MEASURED.replace(r#"c="8""#, r#"c="11""#));
        assert!(!counter_verdict(&before, &after).holds(4));
    }

    #[test]
    fn an_empty_counter_output_reads_as_nothing_and_does_not_hold() {
        let empty = apic_vector_counts("");
        assert!(empty.is_empty());
        assert!(!counter_verdict(&empty, &empty).holds(4));
    }
}
