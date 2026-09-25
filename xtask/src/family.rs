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

/// 変更したパスが選ぶもの（2026-09-26。**骨組みは運用者の決定**——土台は全部へ倒す／`xtask/src/main.rs`
/// は全部／基底だけの置き場／1 つのパスに複数の族）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// 全部（全検査）。**どの族の前提にもなる土台である。**
    All,
    /// 族の集まり（基底は毎回回るので数えない）。
    Families(&'static [Family]),
    /// 基底だけ（族は 0。**意図した 0 である**）。
    BaseOnly,
}

/// 対応表の 1 行。**型はできるだけディレクトリの形で持つ**（運用者の回答 2。**次の段（境界を切る）で
/// 多くのファイルが新しいディレクトリへ移る**）。**`**` は `/` をまたぎ、`*` はまたがない。**
pub struct PathRule {
    pub patterns: &'static [&'static str],
    pub reach: Reach,
}

/// Ring 3 のプログラムを走らせる族と起動。**シェル（`zash`）と Ring 3 の核を通らない回が無い。**
const RING3_FAMILIES: &[Family] = &[
    Family::Boot,
    Family::Process,
    Family::Ipc,
    Family::Shell,
    Family::Apps,
];

/// 画面に描く族と起動（コンソール・字形・描画）。
const SCREEN_FAMILIES: &[Family] = &[Family::Boot, Family::Ipc, Family::Shell, Family::Apps];

/// 変更したパスと族の対応表（2026-09-26。族にまとめる段）。
///
/// **読み方は和である**——**パスに当たる行を全部集め、1 つでも全部なら全部、そうでなければ族の和、
/// どれも基底だけなら基底だけ。** **行を足しても選びが狭まることは無い**（順序で意味が変わらない）。
///
/// **当たる行が無いパスは全部へ倒す**（選ぶ側）。**そのうえで基底が、追跡している全ファイルに
/// 当たる行が在ることを強いる**（[`table_problems`]）——**新しいファイルを足したら、表に行を足すまで
/// 基底が落ちる。** **`kernel/`・`common/`・`bootloader/` の下は基底だけに当たってはならない。**
///
/// **境界の段で作る CPU 固有・機械固有・外部 ABI の置き場は、はじめは全部へ倒す**（運用者の回答 2）。
/// **族へ振り分けるのは、境界が落ち着いてから。**
pub const PATH_RULES: &[PathRule] = &[
    // ── 全部（土台）。**どの族の項目も通る。** ──
    PathRule {
        patterns: &[
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/**",
            // **どの族の判定を変えたかを、パスで言えない**（検査の本体）。**起動の口はどの走行も通る。**
            "xtask/Cargo.toml",
            "xtask/src/main.rs",
            "xtask/src/launch.rs",
            "bootloader/**",
            "common/Cargo.toml",
            "common/src/lib.rs",
            "common/src/log.rs",
            "common/src/serial.rs",
            "common/src/port.rs",
            "common/src/boot_info.rs",
            "common/src/addr.rs",
            "common/src/cpu.rs",
            "common/src/percpu.rs",
            "common/src/critical.rs",
            // **時計は眠りとタイムアウトの全部が読む**（シェルの台本の破壊 `timer-never-wakes` 等）。
            "common/src/time.rs",
            // **どの Ring 3 のプログラムも ELF として読む。**
            "common/src/elf.rs",
            "kernel/Cargo.toml",
            "kernel/build.rs",
            "kernel/link.ld",
            "kernel/src/main.rs",
            "kernel/src/lib.rs",
            "kernel/src/panic.rs",
            "kernel/src/cpu_state.rs",
            "kernel/src/bkl.rs",
            "kernel/src/frame_allocator.rs",
            "kernel/src/memory_map.rs",
            "kernel/src/paging/**",
            "kernel/src/gdt/**",
            "kernel/src/heap/**",
            // **割り込みの配送とタスクの切り替え**——**タイマの割り込みとスケジューラは、どの族の項目も
            // 通る**（AP の `ap-touch-scheduler`・シェルの台本の眠り）。
            "kernel/src/idt/**",
            "kernel/src/irq/**",
            "kernel/src/interrupts.rs",
            "kernel/src/apic.rs",
            "kernel/src/task.rs",
            "kernel/src/task/**",
            // **Ring 3 の核**——**既定の起動が `init` とシェルと起動時の `syscall-test` を走らせ、SMP の
            // 項目も Ring 3 を AP で走らせる。**
            "kernel/src/syscall.rs",
            "kernel/src/ring3.rs",
            "kernel/src/userland.rs",
            "kernel/userland/userlib.rs",
            "kernel/userland/user.ld",
            "kernel/userland/libc*",
        ],
        reach: Reach::All,
    },
    // ── 割り込み・SMP・メモリ ──
    PathRule {
        patterns: &["kernel/src/acpi/**"],
        reach: Reach::Families(&[Family::Boot, Family::Interrupts, Family::Smp]),
    },
    PathRule {
        patterns: &["kernel/src/pmtimer.rs"],
        reach: Reach::Families(&[Family::Interrupts, Family::Smp]),
    },
    PathRule {
        patterns: &["kernel/src/smp.rs"],
        reach: Reach::Families(&[Family::Boot, Family::Interrupts, Family::Smp]),
    },
    PathRule {
        patterns: &["kernel/src/keyboard/**"],
        reach: Reach::Families(&[Family::Interrupts, Family::Smp, Family::Ipc, Family::Shell]),
    },
    PathRule {
        // **プロセスごとの空間・返した枠の置き場・カーネルのスタック**——**どの Ring 3 のプログラムも
        // 通り、AP も持つ。**
        patterns: &[
            "kernel/src/address_space.rs",
            "kernel/src/quarantine.rs",
            "kernel/src/stack.rs",
        ],
        reach: Reach::Families(&[
            Family::Boot,
            Family::Memory,
            Family::Smp,
            Family::Process,
            Family::Ipc,
            Family::Shell,
            Family::Apps,
        ]),
    },
    PathRule {
        // **FP の状態は切り替えのたびに移る**——**Ring 3 のどのプログラムも使いうる。**
        patterns: &["kernel/src/fp.rs"],
        reach: Reach::Families(RING3_FAMILIES),
    },
    // ── Ring 3 のプログラム ──
    PathRule {
        patterns: &[
            "kernel/userland/bss-test.rs",
            "kernel/userland/spawn-test.rs",
            "kernel/userland/syscall-test.rs",
            "kernel/userland/fault-test.rs",
            "kernel/userland/spin.rs",
            "kernel/userland/hello.rs",
            "kernel/userland/chello.c",
            "kernel/userland/dbfault.c",
            "kernel/userland/fp*.c",
            "kernel/userland/ticker*",
        ],
        reach: Reach::Families(&[Family::Boot, Family::Smp, Family::Process]),
    },
    PathRule {
        patterns: &["kernel/userland/zash.rs"],
        reach: Reach::Families(RING3_FAMILIES),
    },
    PathRule {
        patterns: &[
            "kernel/userland/cat.rs",
            "kernel/userland/echo.rs",
            "kernel/userland/ls.rs",
            "kernel/userland/mkdir.rs",
            "kernel/userland/rm.rs",
            "kernel/userland/rmdir.rs",
            "kernel/userland/touch.rs",
            "kernel/userland/tail.rs",
            "kernel/userland/sleep.rs",
        ],
        reach: Reach::Families(&[Family::Ipc, Family::Shell, Family::Apps]),
    },
    // ── 管・ソケット・入力・画面 ──
    PathRule {
        patterns: &["kernel/src/pipe.rs", "kernel/src/ring.rs"],
        reach: Reach::Families(&[Family::Ipc, Family::Shell]),
    },
    PathRule {
        patterns: &[
            "kernel/src/socket.rs",
            "kernel/src/shm.rs",
            "kernel/userland/poll*.rs",
            "kernel/userland/sock*.rs",
            "kernel/userland/comp*.rs",
            "kernel/userland/gfx*.rs",
            "kernel/userland/inputd.rs",
        ],
        reach: Reach::Families(&[Family::Ipc]),
    },
    PathRule {
        // **`input.rs` は台本（`zi`・`utf8`・`profile` の打鍵）も持つ。**
        patterns: &["kernel/src/input.rs"],
        reach: Reach::Families(&[Family::Ipc, Family::Shell, Family::Apps]),
    },
    PathRule {
        patterns: &[
            "kernel/src/console/**",
            "kernel/src/graphics/**",
            "third_party/unifont/**",
        ],
        reach: Reach::Families(SCREEN_FAMILIES),
    },
    // ── シェルとアプリ ──
    PathRule {
        // **コンソールが使う**（`kernel/src/console`・`kernel/src/graphics`）。
        patterns: &[
            "common/src/ansi.rs",
            "common/src/text.rs",
            "common/src/screen.rs",
            "common/src/window.rs",
        ],
        reach: Reach::Families(SCREEN_FAMILIES),
    },
    PathRule {
        // **シェルの一部である**——**シェルから起こす項目の全部が通る**（`zash.rs` と同じ）。
        patterns: &[
            "common/src/complete.rs",
            "common/src/shell_script.rs",
            "common/src/env.rs",
        ],
        reach: Reach::Families(RING3_FAMILIES),
    },
    PathRule {
        // **`utf8-test`（シェルの族）が `zi` を使う。**
        patterns: &["kernel/userland/zi.rs"],
        reach: Reach::Families(&[Family::Shell, Family::Apps]),
    },
    PathRule {
        patterns: &[
            "kernel/userland/less.rs",
            "kernel/userland/more.rs",
            "kernel/userland/ttfglyph.c",
            "third_party/dejavu/**",
            "third_party/stb/**",
        ],
        reach: Reach::Families(&[Family::Apps]),
    },
    // ── ファイルシステムと装置 ──
    PathRule {
        // **どのプログラムもファイルシステムから起こす**（`userland::spawn`）。
        patterns: &["kernel/src/vfs.rs", "common/src/ext2.rs"],
        reach: Reach::Families(&[
            Family::Boot,
            Family::Process,
            Family::Ipc,
            Family::Shell,
            Family::Apps,
            Family::Fs,
        ]),
    },
    PathRule {
        // **`zi` の保存は装置まで届いたかを見る**（`virtio-skip-install-test`）。
        patterns: &["kernel/src/pci.rs", "kernel/src/virtio.rs"],
        reach: Reach::Families(&[Family::Boot, Family::Apps, Family::Fs, Family::Devices]),
    },
    PathRule {
        // **像の中身と、起動時の設定と環境**（`profile-test`・環境の出どころ）。
        patterns: &["kernel/fsimage/seed/**"],
        reach: Reach::Families(&[Family::Boot, Family::Shell, Family::Fs]),
    },
    // ── 起動と手の道具 ──
    PathRule {
        patterns: &[
            "xtask/src/media.rs",
            "xtask/machine-variants.txt",
            "xtask/reference/boot-log-*",
        ],
        reach: Reach::Families(&[Family::Boot]),
    },
    PathRule {
        patterns: &["xtask/src/tool_checks.rs", "tools/**"],
        reach: Reach::Families(&[Family::Harness]),
    },
    // ── 基底だけ（ホストのテストと基底の確かめが覆う） ──
    PathRule {
        patterns: &[
            "docs/**",
            "*.md",
            "LICENSE",
            ".gitignore",
            ".claude/**",
            ".github/**",
            "probes/**",
            "xtask/src/check_lock.rs",
            "xtask/src/family.rs",
            "xtask/src/font.rs",
            "xtask/src/full_check.rs",
            "xtask/src/metrics.rs",
            "xtask/src/vbox.rs",
            "xtask/reference/host-tests.txt",
        ],
        reach: Reach::BaseOnly,
    },
];

/// 基底だけに当たってはならない置き場（族か全部）。
const NEVER_BASE_ONLY: [&str; 3] = ["kernel/", "common/", "bootloader/"];

/// 型がパスに当たるか（`**` は `/` をまたぎ、`*` はまたがない。**`**/` は 0 段にも当たる**）。
pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    fn walk(pattern: &[u8], path: &[u8]) -> bool {
        match pattern {
            [] => path.is_empty(),
            [b'*', b'*', rest @ ..] => {
                let rest = rest.strip_prefix(b"/").unwrap_or(rest);
                (0..=path.len()).any(|start| walk(rest, &path[start..]))
            }
            [b'*', rest @ ..] => (0..=path.len())
                .take_while(|&end| end == 0 || path[end - 1] != b'/')
                .any(|end| walk(rest, &path[end..])),
            [first, rest @ ..] => path.first() == Some(first) && walk(rest, &path[1..]),
        }
    }
    walk(pattern.as_bytes(), path.as_bytes())
}

/// 1 つのパスが選ぶもの（表の和）。**当たる行が無ければ `None`。**
pub fn reach_of(rules: &[PathRule], path: &str) -> Option<PathReach> {
    let mut matched = false;
    let mut families: Vec<Family> = Vec::new();
    for rule in rules {
        if !rule
            .patterns
            .iter()
            .any(|pattern| pattern_matches(pattern, path))
        {
            continue;
        }
        matched = true;
        match rule.reach {
            Reach::All => return Some(PathReach::All),
            Reach::Families(listed) => families.extend_from_slice(listed),
            Reach::BaseOnly => {}
        }
    }
    families.sort();
    families.dedup();
    match (matched, families.is_empty()) {
        (false, _) => None,
        (true, true) => Some(PathReach::BaseOnly),
        (true, false) => Some(PathReach::Families(families)),
    }
}

/// 1 つのパスが選ぶもの（[`reach_of`] の答え）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathReach {
    All,
    Families(Vec<Family>),
    BaseOnly,
}

/// 表の問題（基底の確かめ。純粋な論理）。**当たる行が無いパス、基底だけに当たった `kernel/` 等の
/// パス、どのパスにも当たらない型（死んだ行）を返す。**
pub fn table_problems(rules: &[PathRule], paths: &[&str]) -> Vec<String> {
    let mut problems = Vec::new();
    for path in paths {
        match reach_of(rules, path) {
            None => problems.push(format!("{path}: no row in the path-to-family table")),
            Some(PathReach::BaseOnly)
                if NEVER_BASE_ONLY
                    .iter()
                    .any(|prefix| path.starts_with(prefix)) =>
            {
                problems.push(format!(
                    "{path}: base only, but nothing under {} may be (give it families or all)",
                    NEVER_BASE_ONLY.join(", ")
                ))
            }
            Some(_) => {}
        }
    }
    // **死んだ行も探す**——**移したファイルの古い型が残ると、表を読んだ人がそこを覆っていると読む。**
    for rule in rules {
        for pattern in rule.patterns {
            if !paths.iter().any(|path| pattern_matches(pattern, path)) {
                problems.push(format!("the pattern {pattern} matches no tracked file"));
            }
        }
    }
    problems
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

    /// **`**` は `/` をまたぎ、`*` はまたがない。`**/` は 0 段にも当たる。**
    #[test]
    fn patterns_cross_slashes_only_with_two_stars() {
        assert!(pattern_matches("docs/**", "docs/a.md"));
        assert!(pattern_matches("docs/**", "docs/adr/0069-x.md"));
        assert!(!pattern_matches("docs/**", "docsx/a.md"));
        assert!(pattern_matches("*.md", "README.md"));
        assert!(!pattern_matches("*.md", "docs/a.md"));
        assert!(pattern_matches("**/*.md", "README.md"));
        assert!(pattern_matches("**/*.md", "docs/adr/a.md"));
        assert!(pattern_matches(
            "kernel/userland/libc*",
            "kernel/userland/libc_math.c"
        ));
        assert!(!pattern_matches(
            "kernel/userland/libc*",
            "kernel/userland/sub/libc.c"
        ));
        assert!(pattern_matches(
            "kernel/userland/fp*.c",
            "kernel/userland/fpchild.c"
        ));
        assert!(!pattern_matches(
            "kernel/userland/fp*.c",
            "kernel/userland/fpchild.h"
        ));
        assert!(pattern_matches("xtask/src/main.rs", "xtask/src/main.rs"));
        assert!(!pattern_matches(
            "xtask/src/main.rs",
            "xtask/src/main.rs.orig"
        ));
    }

    /// **表は和で読む**——**1 つでも全部なら全部、基底だけの行は族を減らさない。**
    #[test]
    fn a_path_takes_the_union_of_every_row_it_matches() {
        assert_eq!(
            reach_of(PATH_RULES, "xtask/src/main.rs"),
            Some(PathReach::All)
        );
        assert_eq!(
            reach_of(PATH_RULES, "kernel/src/paging/table.rs"),
            Some(PathReach::All)
        );
        assert_eq!(
            reach_of(PATH_RULES, "docs/roadmap.md"),
            Some(PathReach::BaseOnly)
        );
        assert_eq!(
            reach_of(PATH_RULES, "kernel/src/pci.rs"),
            Some(PathReach::Families(vec![
                Family::Boot,
                Family::Apps,
                Family::Fs,
                Family::Devices
            ]))
        );
        // **`third_party` の README は字形の族に入る**（基底だけの `*.md` は根の直下だけ）。
        assert_eq!(
            reach_of(PATH_RULES, "third_party/dejavu/README.md"),
            Some(PathReach::Families(vec![Family::Apps]))
        );
        assert_eq!(reach_of(PATH_RULES, "arch/x86_64/new.rs"), None);
        let rules = [
            PathRule {
                patterns: &["a/**"],
                reach: Reach::BaseOnly,
            },
            PathRule {
                patterns: &["a/b.rs"],
                reach: Reach::Families(&[Family::Fs]),
            },
            PathRule {
                patterns: &["a/c.rs"],
                reach: Reach::All,
            },
        ];
        assert_eq!(
            reach_of(&rules, "a/b.rs"),
            Some(PathReach::Families(vec![Family::Fs]))
        );
        assert_eq!(reach_of(&rules, "a/c.rs"), Some(PathReach::All));
        assert_eq!(reach_of(&rules, "a/d.rs"), Some(PathReach::BaseOnly));
    }

    /// **基底の確かめ**——**行の無いパス、基底だけの `kernel/` 等、死んだ行を挙げる。**
    #[test]
    fn the_table_check_names_unmatched_paths_base_only_kernel_paths_and_dead_rows() {
        let rules = [
            PathRule {
                patterns: &["docs/**", "kernel/notes.md"],
                reach: Reach::BaseOnly,
            },
            PathRule {
                patterns: &["kernel/src/**", "gone/**"],
                reach: Reach::All,
            },
        ];
        let problems = table_problems(
            &rules,
            &[
                "docs/a.md",
                "kernel/notes.md",
                "kernel/src/main.rs",
                "new.txt",
            ],
        );
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems[0].starts_with("kernel/notes.md: base only"),
            "{problems:?}"
        );
        assert!(problems[1].starts_with("new.txt: no row"), "{problems:?}");
        assert!(problems[2].contains("gone/**"), "{problems:?}");
        assert!(table_problems(&rules[1..], &["kernel/src/main.rs", "gone/x"]).is_empty());
    }
}
