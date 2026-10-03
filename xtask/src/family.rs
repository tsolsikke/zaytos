//! 全検査の項目のグループ（2026-09-26。検査の体系の改善の、族にまとめる段。運用者の決定）。
//!
//! **どの項目もグループを 1 つ名乗る**——**項目の見出しを出す関数（`begin_item`）がグループを取るので、名乗らない
//! 項目は作られない。** **グループの分け方は、項目の見出しの形で 409 項目を振り分けて決めた**（2026-09-26。
//! 他の実行が無い全検査 `0249d12` のログ）。**当たらなかった 3 本の入れ先も運用者の決定である**——
//! `gen-font` は基本の検査、書く側の上限は手の道具、FS/GS の破壊テストは割り込み（コードの在り処が例外・
//! クリティカルの塊の中）。

/// 全検査の項目のグループ（12）。**順序は全検査の項目の並びにおおよそ合わせた。**
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    /// 基本の検査の静的な検査（どの選び方でも毎回実行される）。
    Base,
    /// 起動ログの突き合わせ・機械の変種・起動媒体のイメージ・higher-half・panic・トランポリン。
    Boot,
    /// 手で使う道具の確かめ（`stack-deepest`・calibration・screenshot）と書く側の上限。
    Harness,
    /// 例外・クリティカル・割り込み・APIC・IO-APIC・LAPIC タイマ・ACPI・FS/GS の破壊テスト。
    Interrupts,
    /// ページング・スタック。
    Memory,
    /// BKL・AP・percpu・シリアルの並行実行。
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
/// は全部／基底だけの置き場／1 つのパスに複数のグループ）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// 全部（全検査）。**どのグループの前提にもなる土台である。**
    All,
    /// グループの集まり（基本の検査は毎回実行されるので数えない）。
    Families(&'static [Family]),
    /// 基底だけ（グループは 0。**意図した 0 である**）。
    BaseOnly,
}

/// 対応表の 1 行。**型はできるだけディレクトリの形で持つ**（運用者の回答 2。**次の段階（境界を切る）で
/// 多くのファイルが新しいディレクトリへ移る**）。**`**` は `/` をまたぎ、`*` はまたがない。**
pub struct PathRule {
    pub patterns: &'static [&'static str],
    pub reach: Reach,
}

/// Ring 3 のプログラムを走らせるグループと起動。**シェル（`zash`）と Ring 3 の核を通らない回が無い。**
const RING3_FAMILIES: &[Family] = &[
    Family::Boot,
    Family::Process,
    Family::Ipc,
    Family::Shell,
    Family::Apps,
];

/// 画面に描くグループと起動（コンソール・字形・描画）。
const SCREEN_FAMILIES: &[Family] = &[Family::Boot, Family::Ipc, Family::Shell, Family::Apps];

/// 変更したパスとグループの対応表（2026-09-26。族にまとめる段）。
///
/// **読み方は和である**——**パスに当たる行を全部集め、1 つでも全部なら全部、そうでなければグループの和、
/// どれも基底だけなら基底だけ。** **行を足しても選択が狭まることは無い**（順序で意味が変わらない）。
///
/// **当たる行が無いパスは全部へ倒す**（[`select`]）。**そのうえで基本の検査が、追跡している全ファイルに
/// 当たる行が在ることを強いる**（[`table_problems`]）——**新しいファイルを足したら、表に行を足すまで
/// 基本の検査が落ちる。** **`kernel/`・`common/`・`bootloader/` の下は基底だけに当たってはならない。**
///
/// **境界の段階で作る CPU 固有・機械固有・外部 ABI の置き場は、はじめは全部へ倒す**（運用者の回答 2）。
/// **グループへ振り分けるのは、境界が落ち着いてから。**
pub const PATH_RULES: &[PathRule] = &[
    // ── 全部（土台）。**どのグループの項目も通る。** ──
    PathRule {
        patterns: &[
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/**",
            // **どのグループの判定を変えたかを、パスで言えない**（検査の本体）。**起動の入口はどの実行も通る。**
            "xtask/Cargo.toml",
            "xtask/src/main.rs",
            "xtask/src/launch.rs",
            // **回の置き場**（案 B の ①）——**QEMU を起動するどの項目も通る。**
            "xtask/src/run_dir.rs",
            // **項目のログの塊**——**どの項目の出力も通る。**
            "xtask/src/item_log.rs",
            // **全検査の表の行を同時に走らせる**——**全検査の表のループの項目は全部通る。**
            "xtask/src/batch.rs",
            "xtask/src/kernel_builds.rs",
            "bootloader/**",
            "common/Cargo.toml",
            "common/src/lib.rs",
            "common/src/log.rs",
            "common/src/boot_info.rs",
            "common/src/addr.rs",
            "common/src/percpu.rs",
            "common/src/critical.rs",
            // **時計は眠りとタイムアウトの全部が読む**（シェルの台本の破壊テスト `timer-never-wakes` 等）。
            "common/src/time.rs",
            // **どの Ring 3 のプログラムも ELF として読む。**
            "common/src/elf.rs",
            // **境界の段階で作る CPU 固有・機械固有・外部 ABI の置き場は、はじめは全部へ倒す**（運用者の回答 2。
            // グループへ振り分けるのは、境界が落ち着いてから）。
            "common/src/arch/**",
            "common/src/machine/**",
            "kernel/src/arch/**",
            "kernel/src/machine/**",
            "kernel/src/abi/**",
            "kernel/Cargo.toml",
            "kernel/build.rs",
            "kernel/link.ld",
            "kernel/src/main.rs",
            "kernel/src/lib.rs",
            "kernel/src/panic.rs",
            "kernel/src/bkl.rs",
            // **起動の終わりの目印**——**どの起動も立て、「起動の後はしない」決まりが読む。**
            "kernel/src/boot.rs",
            "kernel/src/frame_allocator.rs",
            "kernel/src/memory_map.rs",
            // **ページの権限の一覧**——**既定の起動が時点ごとに要約を出し、どのユーザーのプログラムの終わりでも出す。**
            "kernel/src/page_survey.rs",
            "kernel/src/paging/**",
            "kernel/src/heap/**",
            // **割り込みの配送とタスクの切り替え**——**タイマの割り込みとスケジューラは、どのグループの項目も
            // 通る**（AP の `ap-touch-scheduler`・シェルの台本の眠り）。
            "kernel/src/interrupts.rs",
            "kernel/src/task.rs",
            "kernel/src/task/**",
            // **Ring 3 の核**——**既定の起動が `init` とシェルと起動時の `syscall-test` を走らせ、SMP の
            // 項目も Ring 3 を AP で走らせる。**
            "kernel/src/syscall.rs",
            "kernel/src/userland.rs",
            "kernel/userland/userlib.rs",
            "kernel/userland/user.ld",
            "kernel/userland/libc*",
        ],
        reach: Reach::All,
    },
    // ── 割り込み・SMP・メモリ ──
    PathRule {
        patterns: &["kernel/src/smp.rs"],
        reach: Reach::Families(&[Family::Boot, Family::Interrupts, Family::Smp]),
    },
    PathRule {
        patterns: &["kernel/src/keyboard/**"],
        reach: Reach::Families(&[Family::Interrupts, Family::Smp, Family::Ipc, Family::Shell]),
    },
    PathRule {
        // **返したフレームの置き場・カーネルのスタック**——**どの Ring 3 のプログラムも通り、AP も持つ。**
        // **プロセスごとの空間（`address_space`）は `arch/x86_64/paging` へ移し、全部へ倒す**（2026-09-27）。
        patterns: &["kernel/src/quarantine.rs"],
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
    // ── Ring 3 のプログラム ──
    PathRule {
        patterns: &[
            "kernel/userland/bss-test.rs",
            "kernel/userland/spawn-test.rs",
            "kernel/userland/syscall-test.rs",
            "kernel/userland/fault-test.rs",
            "kernel/userland/nx-*.rs",
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
    // ── パイプ・ソケット・入力・画面 ──
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
        // **シェルの一部である**——**シェルから起動する項目の全部が通る**（`zash.rs` と同じ）。
        patterns: &[
            "common/src/complete.rs",
            "common/src/shell_script.rs",
            "common/src/env.rs",
        ],
        reach: Reach::Families(RING3_FAMILIES),
    },
    PathRule {
        // **`utf8-test`（シェルのグループ）が `zi` を使う。**
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
        // **どのプログラムもファイルシステムから起動する**（`userland::spawn`）。
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
        patterns: &["kernel/src/virtio.rs"],
        reach: Reach::Families(&[Family::Boot, Family::Apps, Family::Fs, Family::Devices]),
    },
    PathRule {
        // **イメージの中身と、起動時の設定と環境**（`profile-test`・環境の出どころ）。
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
    // **ページの権限の一覧を読んで比べる側と、その参照**——**一覧の道具の項目（メモリの組）だけが通る。**
    PathRule {
        patterns: &[
            "xtask/src/page_permissions.rs",
            "xtask/reference/page-permissions.txt",
            "xtask/reference/page-permissions-heap.txt",
            "xtask/reference/page-permissions-screen.txt",
        ],
        reach: Reach::Families(&[Family::Memory]),
    },
    // ── 基底だけ（ホストのテストと基本の検査の確かめが覆う） ──
    PathRule {
        patterns: &[
            "docs/**",
            ".githooks/**",
            "*.md",
            "LICENSE",
            ".gitignore",
            ".claude/**",
            ".github/**",
            "probes/**",
            "xtask/src/check_lock.rs",
            // **手で使う、試験の一覧を並べて回す道具**（案 B の ②）——**全検査の項目は使わない。**
            "xtask/src/run_set.rs",
            "xtask/src/family.rs",
            "xtask/src/font.rs",
            "xtask/src/full_check.rs",
            "xtask/src/metrics.rs",
            "xtask/src/sampling.rs",
            "xtask/src/vbox.rs",
            "xtask/reference/host-tests.txt",
            "xtask/reference/kernel-layout.txt",
            "xtask/reference/x86-words.txt",
        ],
        reach: Reach::BaseOnly,
    },
];

/// 基底だけに当たってはならない置き場（グループか全部）。
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

/// 表の問題（基本の検査の確かめ。純粋な論理）。**当たる行が無いパス、基底だけに当たった `kernel/` 等の
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

/// 変更したパスの集まりから選んだもの。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// 全部へ倒した理由のパスと、その訳（土台か、表に行が無いか）。
    pub all: Vec<(String, &'static str)>,
    /// グループごとの、選んだ理由のパス。
    pub families: std::collections::BTreeMap<Family, Vec<String>>,
    /// 基底だけのパス。
    pub base_only: Vec<String>,
    /// 比べられないので全部へ倒した訳（2026-09-26。第三者レビューの取り込み）——**比較元が無い・消えた・
    /// 履歴が書き換えられた・環境（`rustc`・QEMU 等）が変わった。** **パスより先に効く。**
    pub not_comparable: Vec<String>,
}

/// 変更したパスから選ぶ（2026-09-26。**当たる行が無いパスは全部へ倒す**——**「対象なし」で通さない**）。
pub fn select(rules: &[PathRule], paths: &[String]) -> Selection {
    let mut selection = Selection::default();
    for path in paths {
        match reach_of(rules, path) {
            None => selection.all.push((path.clone(), "no row in the table")),
            Some(PathReach::All) => selection.all.push((path.clone(), "a foundation path")),
            Some(PathReach::Families(families)) => {
                for family in families {
                    selection
                        .families
                        .entry(family)
                        .or_default()
                        .push(path.clone());
                }
            }
            Some(PathReach::BaseOnly) => selection.base_only.push(path.clone()),
        }
    }
    selection
}

impl Selection {
    /// 選んだグループ（`None` は全部。空は基底だけ。2026-09-26）。**当たりの計測が比べる。**
    pub fn chosen(&self) -> Option<Vec<Family>> {
        (self.not_comparable.is_empty() && self.all.is_empty())
            .then(|| self.families.keys().copied().collect())
    }

    /// 記録に書く短い形（`all`・`none`・グループの名前の並び）。
    pub fn summary(&self) -> String {
        match self.chosen() {
            None => "all".to_string(),
            Some(families) if families.is_empty() => "none".to_string(),
            Some(families) => families
                .iter()
                .map(|family| family.name())
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    /// 記録に書く理由の短い形（先頭の `count` 本。2026-09-26）。**全部なら倒した訳、グループならグループごとの最初の
    /// パス、基底だけなら最初のパスである。**
    pub fn reasons(&self, count: usize) -> String {
        let listed: Vec<String> = if !self.not_comparable.is_empty() {
            self.not_comparable.iter().take(count).cloned().collect()
        } else if !self.all.is_empty() {
            self.all
                .iter()
                .take(count)
                .map(|(path, why)| format!("{path} ({why})"))
                .collect()
        } else if !self.families.is_empty() {
            self.families
                .iter()
                .take(count)
                .map(|(family, paths)| format!("{}: {}", family.name(), paths[0]))
                .collect()
        } else {
            self.base_only.iter().take(count).cloned().collect()
        };
        if listed.is_empty() {
            "-".to_string()
        } else {
            listed.join(", ")
        }
    }

    /// 人が読む行（`--status` が出す）。**理由のパスはグループごとに数本まで出し、残りは数で示す。**
    pub fn lines(&self, changed: usize) -> Vec<String> {
        const SHOWN: usize = 4;
        let listed = |paths: &[String]| {
            let mut text = paths
                .iter()
                .take(SHOWN)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            if paths.len() > SHOWN {
                text.push_str(&format!(" and {} more", paths.len() - SHOWN));
            }
            text
        };
        // **比べられない訳は、パスより先に示す**（変わったパスが 0 でも全部である）。
        if !self.not_comparable.is_empty() {
            let mut lines = vec![
                "families selected: all (the full check), because the comparison is not possible:"
                    .to_string(),
            ];
            lines.extend(self.not_comparable.iter().map(|why| format!("    {why}")));
            return lines;
        }
        if changed == 0 {
            return vec!["families selected: none (nothing changed)".to_string()];
        }
        if !self.all.is_empty() {
            let mut lines = vec![format!(
                "families selected: all (the full check), because of {} path(s):",
                self.all.len()
            )];
            lines.extend(
                self.all
                    .iter()
                    .take(SHOWN * 2)
                    .map(|(path, why)| format!("    {path} ({why})")),
            );
            if self.all.len() > SHOWN * 2 {
                lines.push(format!("    and {} more", self.all.len() - SHOWN * 2));
            }
            return lines;
        }
        if self.families.is_empty() {
            return vec![format!(
                "families selected: none beyond the base ({} path(s), all base only: {})",
                self.base_only.len(),
                listed(&self.base_only)
            )];
        }
        let mut lines = vec![format!(
            "families selected: {} of {} ({})",
            self.families.len(),
            Family::ALL.len() - 1,
            self.families
                .keys()
                .map(|family| family.name())
                .collect::<Vec<_>>()
                .join(", ")
        )];
        for (family, paths) in &self.families {
            lines.push(format!("    {}: {}", family.name(), listed(paths)));
        }
        if !self.base_only.is_empty() {
            lines.push(format!(
                "    base only: {} path(s): {}",
                self.base_only.len(),
                listed(&self.base_only)
            ));
        }
        lines
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

    /// **表は和で読む**——**1 つでも全部なら全部、基底だけの行はグループを減らさない。**
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
            reach_of(PATH_RULES, "kernel/src/virtio.rs"),
            Some(PathReach::Families(vec![
                Family::Boot,
                Family::Apps,
                Family::Fs,
                Family::Devices
            ]))
        );
        // **`third_party` の README は字形のグループに入る**（基底だけの `*.md` は根の直下だけ）。
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

    /// **基本の検査の確かめ**——**行の無いパス、基底だけの `kernel/` 等、死んだ行を挙げる。**
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

    /// **選び方**——**行の無いパスは全部へ倒す（「対象なし」で通さない）。文書だけなら意図した 0。**
    #[test]
    fn selection_falls_to_all_for_an_unknown_path_and_to_none_for_documents_only() {
        let paths = |list: &[&str]| list.iter().map(|path| path.to_string()).collect::<Vec<_>>();
        let documents = select(PATH_RULES, &paths(&["docs/roadmap.md", "README.md"]));
        assert!(documents.all.is_empty() && documents.families.is_empty());
        assert_eq!(documents.base_only.len(), 2);
        assert!(documents.lines(2)[0].starts_with("families selected: none beyond the base"));

        let unknown = select(PATH_RULES, &paths(&["docs/roadmap.md", "arch/new.rs"]));
        assert_eq!(
            unknown.all,
            vec![("arch/new.rs".to_string(), "no row in the table")]
        );
        assert!(unknown.lines(2)[0].starts_with("families selected: all"));

        let leaf = select(
            PATH_RULES,
            &paths(&[
                "kernel/userland/less.rs",
                "kernel/src/socket.rs",
                "docs/a.md",
            ]),
        );
        assert!(leaf.all.is_empty());
        assert_eq!(
            leaf.families.keys().copied().collect::<Vec<_>>(),
            vec![Family::Ipc, Family::Apps]
        );
        let lines = leaf.lines(3);
        assert_eq!(lines[0], "families selected: 2 of 11 (ipc, apps)");
        assert!(lines
            .iter()
            .any(|line| line == "    apps: kernel/userland/less.rs"));
        assert!(lines
            .iter()
            .any(|line| line.starts_with("    base only: 1 path(s)")));

        assert_eq!(
            select(PATH_RULES, &[]).lines(0),
            vec!["families selected: none (nothing changed)".to_string()]
        );
        assert_eq!(leaf.summary(), "ipc,apps");
        assert_eq!(
            leaf.reasons(3),
            "ipc: kernel/src/socket.rs, apps: kernel/userland/less.rs"
        );
        assert_eq!(unknown.reasons(3), "arch/new.rs (no row in the table)");
        assert_eq!(documents.summary(), "none");
        assert_eq!(unknown.summary(), "all");
        // **比べられない訳は、変わったパスが 0 でも全部へ倒す**（2026-09-26）。
        let mut gone = select(PATH_RULES, &[]);
        gone.not_comparable
            .push("the last green full check is gone".to_string());
        assert_eq!(gone.chosen(), None);
        assert_eq!(gone.summary(), "all");
        assert_eq!(
            gone.lines(0),
            vec![
                "families selected: all (the full check), because the comparison is not possible:"
                    .to_string(),
                "    the last green full check is gone".to_string()
            ]
        );
    }
}
