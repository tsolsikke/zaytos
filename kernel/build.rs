//! `link.ld`（ADR-0009: kernel の低位固定アドレスへのリンク）をリンカへ渡す。
//! 相対パスを `.cargo/config.toml` の rustflags に直接書くと呼び出し時の
//! カレントディレクトリに依存して壊れるため、`CARGO_MANIFEST_DIR` から
//! 絶対パスを組み立てて渡す。
//!
//! この build script はパッケージのどのターゲット（実際の kernel バイナリ
//! だけでなく、`cargo test -p kernel --lib` のホスト向けテストバイナリも
//! 含む）をビルドする際にも必ず実行される。`link.ld` は
//! `x86_64-unknown-none` 向け（エントリポイント `_start`、0x100000 に
//! 全セクション配置）を前提にしており、これをホストの通常の実行可能
//! ファイルに適用すると、OS のプロセスローダーが期待する ELF 構造
//! （通常の crt0/エントリポイント）が壊れてプロセス起動直後に
//! セグメンテーション違反を起こす。そのため、実際にビルド対象が
//! `x86_64-unknown-none` のときだけリンカ引数を渡すようにする。
fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set");
    println!("cargo:rerun-if-changed=link.ld");

    // **リンカスクリプトの KERNEL_VIRT_BASE を Rust の定数として生成する。**
    //
    // 同じ値をリンカスクリプトと Rust の両方に手で書くと、片方だけ直した
    // ときに食い違う。食い違った状態は「リンクは通るが、アドレス変換が
    // 一段ずれる」という形で出て、最も診断しにくい。ここで 1 つの出所から
    // 生成しておけば、その状態が起きない。
    //
    // ホスト向けテストでもこの定数は使うので、生成はターゲットに関わらず行う。
    let script =
        std::fs::read_to_string(format!("{manifest_dir}/link.ld")).expect("failed to read link.ld");
    let virt_base = parse_symbol(&script, "KERNEL_VIRT_BASE")
        .expect("link.ld does not define KERNEL_VIRT_BASE");
    let load_addr = parse_symbol(&script, "KERNEL_LOAD_ADDR")
        .expect("link.ld does not define KERNEL_LOAD_ADDR");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is not set");
    std::fs::write(
        format!("{out_dir}/link_symbols.rs"),
        format!(
            "// build.rs が link.ld から生成した。手で編集しないこと。\n\
             pub const KERNEL_VIRT_BASE: u64 = {virt_base};\n\
             pub const KERNEL_LOAD_ADDR: u64 = {load_addr};\n"
        ),
    )
    .expect("failed to write link_symbols.rs");

    // **立っている feature の一覧を生成する（S12 前の手当て）。**
    //
    // **出すのは「実際にコンパイルされたもの」である。** `cargo` が渡した
    // `CARGO_FEATURE_*` をそのまま読むので、**xtask が渡したつもりの構成ではなく、
    // この成果物に効いている構成**が出る。意図と真実が食い違う場合を見分けたいので、
    // 意図の側を出しては意味が無い。
    //
    // **手で並べた一覧を持たない。** 環境変数から導くので、feature を足したときに
    // 書き足しを忘れる余地が無い（`TEST_HOOKS` の網羅検査が守っているのと同じ穴が、
    // ここでは構造的に開かない）。
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(key, _)| key.strip_prefix("CARGO_FEATURE_").map(str::to_string))
        .map(|name| name.to_ascii_lowercase().replace('_', "-"))
        .collect();
    features.sort();
    let listed = features
        .iter()
        .map(|name| format!("    {name:?},\n"))
        .collect::<String>();
    std::fs::write(
        format!("{out_dir}/enabled_features.rs"),
        format!(
            "// build.rs が CARGO_FEATURE_* から生成した。手で編集しないこと。\n\
             pub const ENABLED_FEATURES: &[&str] = &[\n{listed}];\n"
        ),
    )
    .expect("failed to write enabled_features.rs");

    let target = std::env::var("TARGET").expect("TARGET is not set");
    if target != "x86_64-unknown-none" {
        return;
    }

    build_user_programs(&manifest_dir, &out_dir);
    build_fs_image(&manifest_dir, &out_dir);

    println!("cargo:rustc-link-arg=-T{manifest_dir}/link.ld");
}

/// 埋め込むユーザープログラムを `rustc` で直接建て、`OUT_DIR` へ置く（S9-b-1）。
///
/// # なぜ cargo を入れ子にしないか
///
/// ユーザープログラムを別の crate にして build script から `cargo` を呼ぶ形は、
/// **`OUT_DIR` が feature 構成ごとに別なので、`cargo xtask check --full` が
/// kernel を何十回も建てるたびに丸ごと建て直すことになる。**
/// `rustc` を 1 回呼ぶだけなら crate もワークスペースからの除外も要らず、
/// 依存も増えない。ツールチェインに必ずある道具だけで済む。
///
/// # なぜ `include_bytes!` で抱えるか
///
/// S9 は「ファイルシステムに依存せず」を範囲としている（`docs/roadmap.md`）。
/// ESP へ置いて bootloader に読ませる形は、UEFI のファイルシステムに依存する。
///
/// # 生成物
///
/// 非 PIE の ET_EXEC（`userland/user.ld` が `0x400000` へリンクする）。
/// `common::elf` が受理する形であることは S9-b-1 の着手前に実測して確かめた。
fn build_user_programs(manifest_dir: &str, out_dir: &str) {
    const PROGRAMS: &[&str] = &[
        "hello",
        "fault-test",
        "syscall-test",
        "spawn-test",
        "ls",
        "cat",
        "zash",
        "spin",
        "zi",
        "bss-test",
        "rm",
        "tail",
        "mkdir",
        "rmdir",
        "touch",
        "less",
        "more",
        "echo",
    ];

    // **共有する包み（S11-9）。** `ls` と `cat` が `mod userlib;` で取り込む。
    // **`PROGRAMS` には入れない**——単独では建たない（`_start` はあるが
    // `zaytos_main` が無い）。**変わったら建て直す必要はあるので、ここで見る。**
    println!("cargo:rerun-if-changed={manifest_dir}/userland/userlib.rs");

    // **`common` から取り込む純粋な論理（VIEW-a。ADR-0045 の決定 3）。**
    //
    // **載せないと、`common` 側を直してもユーザープログラムが建て直されない。**
    // **`userlib.rs` を載せているのと同じ理由である。**
    println!("cargo:rerun-if-changed={manifest_dir}/../common/src/window.rs");
    println!("cargo:rerun-if-changed={manifest_dir}/../common/src/env.rs");

    let script = format!("{manifest_dir}/userland/user.ld");
    println!("cargo:rerun-if-changed={script}");

    // **受け皿の位置は `user.ld` が唯一の出所である**（S10-b の締め）。
    // 以前はアセンブリの `.org` と Rust の定数の 2 か所にあり、**検算を足して
    // コードが伸びるたびに両方を直していた**（3 度起きた）。ここで読んで
    // 生成すれば、**直す場所は `user.ld` の 1 行だけになる。**
    let userland = std::fs::read_to_string(&script).expect("failed to read user.ld");
    let receiver_offset = parse_symbol(&userland, "USER_RECEIVER_OFFSET")
        .expect("user.ld does not define USER_RECEIVER_OFFSET");
    std::fs::write(
        format!("{out_dir}/userland_layout.rs"),
        format!(
            "// build.rs が user.ld から生成した。手で編集しないこと。\n\
             pub const USER_RECEIVER_OFFSET: u64 = {receiver_offset};\n"
        ),
    )
    .expect("failed to write userland_layout.rs");

    // **ユーザープログラムは `rustc` を直に呼んで建てる**ので、cargo の
    // feature は届かない。**破壊 feature を渡すには `--cfg` を明示する。**
    //
    // **kernel の feature 環境変数から引く**（`CARGO_FEATURE_*`）。ここに
    // 載せた分だけがユーザー側へ届く形で、**列挙が全部である**——足すときは
    // この表へ 1 行足すこと。
    const USER_PROGRAM_CFGS: &[(&str, &str)] = &[
        (
            "CARGO_FEATURE_ZI_CURSOR_IGNORE_UPDOWN_TEST",
            "zi_cursor_ignore_updown",
        ),
        (
            "CARGO_FEATURE_ZI_WRITE_SKIP_BODY_TEST",
            "zi_write_skip_body",
        ),
        (
            "CARGO_FEATURE_ZI_INSERT_DROP_FIRST_TEST",
            "zi_insert_drop_first",
        ),
        // **破壊ではない（zi-e 前の手当て）。** **`zi` の診断行を出すのは検査の
        // 構成だけである**——`write` は画面へも届くので、通常の起動で出すと
        // 全画面のアプリの本文を上書きする（`kernel/userland/zi.rs`）。
        ("CARGO_FEATURE_ZI_TEST", "zi_diagnostics"),
        // **`utf8-test` も `zi` の診断を要る**（`ADR-0054` の判定 3 が
        // `scol=` を読む）。**同じ cfg を 2 つの feature から立てる。**
        ("CARGO_FEATURE_UTF8_TEST", "zi_diagnostics"),
        // **`zi` の桁の計算も同じ破壊を受ける**（`ADR-0054`）。
        // **カーネル側だけ幅を 1 にすると、画面と `scol=` が食い違う。**
        ("CARGO_FEATURE_WIDTH_ALWAYS_ONE_TEST", "width_always_one"),
        ("CARGO_FEATURE_ZI_APPEND_BY_BYTE_TEST", "zi_append_by_byte"),
        ("CARGO_FEATURE_ZI_LINE_END_STAYS_TEST", "zi_line_end_stays"),
        (
            "CARGO_FEATURE_ZI_FIRST_NONBLANK_TO_ZERO_TEST",
            "zi_first_nonblank_to_zero",
        ),
        ("CARGO_FEATURE_ZI_ESCAPE_BY_BYTE_TEST", "zi_escape_by_byte"),
        (
            "CARGO_FEATURE_ZI_STATUS_STALE_COLUMN_TEST",
            "zi_status_stale_column",
        ),
        (
            "CARGO_FEATURE_ZASH_PROMPT_DROP_COLOR_TEST",
            "zash_prompt_drop_color",
        ),
        (
            "CARGO_FEATURE_SHELL_KEEP_CONTROL_BYTES_TEST",
            "zash_keep_control_bytes",
        ),
        (
            "CARGO_FEATURE_SHELL_EXPORT_NOT_PUSHED_TEST",
            "zash_export_not_pushed",
        ),
        (
            "CARGO_FEATURE_SHELL_SKIP_EXPANSION_TEST",
            "zash_skip_expansion",
        ),
        ("CARGO_FEATURE_SHELL_DROP_HISTORY_TEST", "zash_drop_history"),
        (
            "CARGO_FEATURE_SHELL_SHIFT_DELETE_RANGE_TEST",
            "zash_shift_delete_range",
        ),
        (
            "CARGO_FEATURE_ZI_STATUS_FREEZE_MODE_TEST",
            "zi_status_freeze_mode",
        ),
        (
            "CARGO_FEATURE_ZI_ESC_NEEDS_SECOND_KEY_TEST",
            "zi_esc_needs_second_key",
        ),
        (
            "CARGO_FEATURE_ZI_STATUS_BELOW_TEXT_TEST",
            "zi_status_below_text",
        ),
        (
            "CARGO_FEATURE_ZI_COMMAND_LINE_SILENT_TEST",
            "zi_command_line_silent",
        ),
        (
            "CARGO_FEATURE_ZI_APPEND_LIKE_INSERT_TEST",
            "zi_append_like_insert",
        ),
        // **破壊そのものはカーネル側に在る（EV）。** **ここで渡すのは
        // `syscall-test` の期待値を合わせるためである**——**環境が空の構成で
        // ABI の検算が落ちると、破壊が別の理由で捕まったことになる。**
        ("CARGO_FEATURE_ENV_DROP_TERM_TEST", "env_drop_term"),
        ("CARGO_FEATURE_ENV_DROP_PATH_TEST", "env_drop_path"),
        (
            "CARGO_FEATURE_ZI_ENTER_DOES_NOTHING_TEST",
            "zi_enter_does_nothing",
        ),
        ("CARGO_FEATURE_ZI_SKIP_RELEASE_TEST", "zi_skip_release"),
        ("CARGO_FEATURE_ZI_SKIP_GROW_TEST", "zi_skip_grow"),
        (
            "CARGO_FEATURE_ZI_JOIN_DOES_NOTHING_TEST",
            "zi_join_does_nothing",
        ),
        ("CARGO_FEATURE_ZI_WINDOW_FROZEN_TEST", "zi_window_frozen"),
        (
            "CARGO_FEATURE_LESS_WINDOW_FROZEN_TEST",
            "less_window_frozen",
        ),
        (
            "CARGO_FEATURE_MORE_USES_ALTERNATE_SCREEN_TEST",
            "more_uses_alternate_screen",
        ),
        (
            "CARGO_FEATURE_FRAME_WRITE_PER_PIECE_TEST",
            "frame_write_per_piece",
        ),
        (
            "CARGO_FEATURE_ZI_SKIP_CURSOR_FLUSH_TEST",
            "zi_skip_cursor_flush",
        ),
        (
            "CARGO_FEATURE_LESS_REDRAW_WHOLE_SCREEN_TEST",
            "less_redraw_whole_screen",
        ),
        (
            "CARGO_FEATURE_ZI_REDRAW_WHOLE_SCREEN_TEST",
            "zi_redraw_whole_screen",
        ),
        (
            "CARGO_FEATURE_ZI_EDIT_REDRAWS_EVERYTHING_TEST",
            "zi_edit_redraws_everything",
        ),
    ];
    let mut extra_cfgs: Vec<String> = Vec::new();
    for (env, cfg) in USER_PROGRAM_CFGS {
        if std::env::var(env).is_ok() {
            extra_cfgs.push((*cfg).to_string());
        }
    }

    for name in PROGRAMS {
        let source = format!("{manifest_dir}/userland/{name}.rs");
        let output = format!("{out_dir}/{name}.elf");
        println!("cargo:rerun-if-changed={source}");

        let mut command =
            std::process::Command::new(std::env::var("RUSTC").unwrap_or("rustc".into()));
        for cfg in &extra_cfgs {
            command.args(["--cfg", cfg]);
        }
        let status = command
            .args([
                "--edition",
                "2021",
                "--target",
                "x86_64-unknown-none",
                "-C",
                "panic=abort",
                // 既定に依存せず、非 PIE を明示する。
                "-C",
                "relocation-model=static",
                "-C",
                "opt-level=s",
                "-C",
                "strip=symbols",
                "-C",
                &format!("link-arg=-T{script}"),
                "-o",
                &output,
                &source,
            ])
            .status()
            .unwrap_or_else(|e| panic!("failed to run rustc for the user program {name}: {e}"));

        assert!(status.success(), "rustc failed for the user program {name}");
    }
}

/// `NAME = 0x...;` の形の代入から値を読む。
///
/// リンカスクリプトの完全な構文解析はしない。ZaytOS の `link.ld` が使って
/// いる形だけを見る。形が変わったら `None` になり、`expect` で落ちる。
/// 黙って既定値へ倒れるより、そこで止まるほうがよい。
fn parse_symbol(script: &str, name: &str) -> Option<u64> {
    for line in script.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let value = rest.trim().trim_end_matches(';').trim();
        let value = value.strip_prefix("0x").unwrap_or(value);
        return u64::from_str_radix(value, 16).ok();
    }
    None
}

/// ext2 の像を `mke2fs` で建て、決定的にしてから `OUT_DIR` へ置く（S10-a）。
///
/// # なぜ `mke2fs` を呼ぶか
///
/// **`roadmap.md` の S10 の到達条件が「`mke2fs` で作ったイメージを読めること」で
/// ある。** 自作の書き手が作った像を読めても、それは自分の理解どうしの一致しか
/// 言わない。**外の道具が作った像を読むことが主張の中身である。**
///
/// # 出力は決定的にする
///
/// **`mke2fs` の出力はそのままでは再現しない。** 3 通り測った（S10-a の着手前）。
///
/// - 既定: 2 回作ると md5 が違う（UUID と時刻）
/// - `-U` と `-E hash_seed=` を固定: **同じ秒なら一致し、秒をまたぐと不一致**
/// - `SOURCE_DATE_EPOCH`: **この版（1.47.0）では効かなかった。**
///   `Filesystem created` は現在時刻のままだった。**版によっては対応が入って
///   いるので、「効かない」と一般化しないこと**
///
/// **残る差は時刻だけなので、建てた後に 0 で上書きする。** 上書きするのは
/// superblock の 3 つと、全 inode の 4 つである。**ext2 にはチェックサムが
/// 無い**ので、バイトを書き換えても整合は崩れない（`e2fsck -fn` で確かめる）。
///
/// # ビルド環境への要求
///
/// **`mke2fs`（e2fsprogs）が要る。** `rust-toolchain.toml` では固定できない
/// 種類の要求である。**S12 の独立検証で `e2fsck` が必須になるので、どのみち
/// e2fsprogs は要る**（`ADR-0025`）。不在のときは、何が要るかと何のために
/// 要るかを出して止まる。
fn build_fs_image(manifest_dir: &str, out_dir: &str) {
    /// 像の大きさ。
    ///
    /// # 8 MiB は起動しない。実測で決めた
    ///
    /// **`AllocatePages(Address(0x100000))` が `NOT_FOUND` で落ちる。**
    /// bootloader はカーネル像を固定アドレスへ置く（ADR-0009）ので、
    /// **像が大きいほど、その 1 回の確保が大きくなる。**
    ///
    /// 実測（像の大きさ → 起動）——**6 MiB は起動し、7 MiB は落ちた**
    /// （落ちた側の要求は 2030 ページ = 約 7.9 MiB）。**空き領域は
    /// `0x100000` から 7 MiB ほどで尽きる。**
    ///
    /// # それでも 2 MiB を選ぶ
    ///
    /// **入る最大を選ばない。** カーネル自身がこの先も大きくなる（S10-b の
    /// VFS と syscall、S11 のシェル）ので、**上限ぎりぎりを取ると、次に
    /// カーネルが数百 KiB 増えた時点で起動しなくなる。**
    ///
    /// **中身は 59 ブロックしか使っていない**（`e2fsck` の実測。512 ブロック中）。
    /// **像を大きくしても中身は増えない。**
    const IMAGE_BYTES: u64 = 2 * 1024 * 1024;
    /// `/data/writable` の初期の大きさ（S12-c）。**ブロック境界にしない。**
    const WRITABLE_SEED_BYTES: usize = 100;
    /// 直接ブロックだけで収まる最大の大きさ（12 ブロック × 4096）。
    const DIRECT_MAX_BYTES: usize = 12 * 4096;

    let seed = format!("{manifest_dir}/fsimage/seed");
    println!("cargo:rerun-if-changed={seed}");

    // 種を OUT_DIR へ写し、生成するファイルを足す。**リポジトリへバイナリを
    // 置かない**（種はテキストだけで、大きいものはここで作る）。
    let staging = format!("{out_dir}/fsimage-root");
    let _ = std::fs::remove_dir_all(&staging);
    copy_tree(std::path::Path::new(&seed), std::path::Path::new(&staging));

    std::fs::create_dir_all(format!("{staging}/bin"))
        .expect("failed to create /bin in the staging");
    // **`/bin` へ置くもの**（S11-5 で `spawn-test`、S12 前の手当てで `spin` が加わった）。
    //
    // **本数を書かない。** かつて「2 本」と書いてあったが、**列が 5 本に
    // なっても直されなかった。** 数は列そのものが持っている。
    //
    // **`spawn-test` は `USER_PROGRAMS` に載っていない。** 上から走らせると
    // 深さ 1 になり、孫の `spawn` が成功してしまう。**`syscall-test` が
    // 深さ 2 で起こすためだけに、像の中に居る。**
    for name in [
        "hello",
        "spawn-test",
        "ls",
        "cat",
        "zash",
        "spin",
        "zi",
        "bss-test",
        "rm",
        "tail",
        "mkdir",
        "rmdir",
        "touch",
        "less",
        "more",
        "echo",
    ] {
        std::fs::copy(
            format!("{out_dir}/{name}.elf"),
            format!("{staging}/bin/{name}"),
        )
        .unwrap_or_else(|e| panic!("failed to place {name} into the staging: {e}"));
    }

    // **単一間接ブロックの境界を挟む 2 本。** 直接ブロックは 12 個なので、
    // 12 ブロックちょうどは間接を使わず、1 バイト超えると使う。
    // **`/tmp` を作る（DIR-1c。ADR-0042）。** **中身は置かない**——
    // **一時ファイルの置き場であって、像に焼くものではない。**
    // **ADR-0042 が「作る」と決めた唯一のものである。**
    std::fs::create_dir_all(format!("{staging}/tmp"))
        .expect("failed to create /tmp in the staging");

    // **`/root` を作る（f-1。`ADR-0042` と `ADR-0052`）。** **中身は置かない**
    // ——**`root` のホームであって、像に焼くものではない。**
    // **`git` は空のディレクトリを追跡しないので、種ではなくここで作る**
    // （`/tmp` と同じ理由）。
    std::fs::create_dir_all(format!("{staging}/root"))
        .expect("failed to create /root in the staging");

    std::fs::create_dir_all(format!("{staging}/data"))
        .expect("failed to create /data in the staging");
    // **`zi` が開く複数行のファイル（zi-d-1）。**
    //
    // **`/data/writable` を使わない**——あちらは S12-c の検算が中身を
    // 固定しており、**1 行（改行を含まないバイト列）なので、上下の移動が
    // 起きない。** `zi` の台本は行を移るので、**移る先が要る。**
    std::fs::write(
        format!("{staging}/data/lines"),
        b"alpha\nbravo\ncharlie\ndelta\n" as &[u8],
    )
    .expect("failed to write /data/lines into the staging");

    // **多バイトの字の判定に使う（`ADR-0054`）。**
    //
    // **種の木へは置かない**——**`kernel/fsimage/seed/` は ASCII だけと決めてある**
    // （`docs/coding-standards.md` の「像へ入れるテキストはASCIIに限る」）。
    // **ここは `build.rs` が書くので、あの検査の範囲の外である。**
    //
    // **中身は `あいu` である**——**全角 2 つと半角 1 つで、画面では
    // 2 + 2 + 1 = 5 セルぶんになる。**
    std::fs::write(format!("{staging}/data/utf8"), "あいu\n".as_bytes())
        .expect("failed to write /data/utf8 into the staging");

    // **壊れたバイトを含むファイル（`ADR-0054`）。**
    //
    // **`0xFF` は UTF-8 の頭になれない。** **1 バイトにつき 1 つの置換文字に
    // なることを見る**——**以前は `write` が丸ごと落ちて、行ごと消えていた。**
    std::fs::write(format!("{staging}/data/badutf8"), [b'x', 0xFF, b'y', b'\n'])
        .expect("failed to write /data/badutf8 into the staging");

    // **`$` と `^` を多バイトの行で見るファイル（VIM-1）。**
    //
    // **先頭に空白 2 つを置いてある**——**`^` が飛ばす先が在る形である。**
    // **`  あいu` は 2 + 3 + 3 + 1 = 9 バイトで、`$` が指す最後の字の先頭は
    // 8 バイト目である**（`u`）。**桁で数えると 2 + 2 + 2 = 6 である。**
    // **`^` の行き先は 2 バイト目で、桁も 2 である**（`あ` の先頭）。
    //
    // **バイトと桁が食い違う形を 1 本で持てる**——**`$` と `^` が
    // 字の境界に留まることを、同時に主張できる。**
    std::fs::write(format!("{staging}/data/vimops"), "  あいu\n".as_bytes())
        .expect("failed to write /data/vimops into the staging");

    // **`zi` の上限が外れたことを示すファイル（H-b-2）。**
    //
    // **2 つを 1 本に入れてある。** **100 行**（b-1 までの上限は 64 行で、
    // 越えるファイルは開かずに拒んでいた）と、**200 バイトの行 1 本**
    // （b-1 までの 1 行の上限は 128 バイトだった）。
    //
    // **大きさは約 1 ブロックである**（実測で 2181 バイト。ブロックは 4096）。
    // **像を 1 ブロック太らせるだけで、上限の両方に触れる。**
    //
    // **中身は決定的である。** **判定は像の中身を定数として持たない**
    // ——**`cat` を編集の前後で 2 回撮り、差が編集の分だけであることを見る。**
    {
        const BIG_LINES: usize = 100;
        const LONG_LINE_BYTES: usize = 200;
        let mut big = String::new();
        // **先頭の 1 行だけを長くする。** 26 文字の巡回で、切れたら分かる。
        for index in 0..LONG_LINE_BYTES {
            big.push((b'a' + (index % 26) as u8) as char);
        }
        big.push('\n');
        for line in 1..BIG_LINES {
            // **19 文字 + 改行 = 20 バイト。**
            big.push_str(&format!("big-line-{line:03}-xxxxxx\n"));
        }
        std::fs::write(format!("{staging}/data/big"), big.as_bytes())
            .expect("failed to write /data/big into the staging");
    }

    // **穴を持つファイルを常設する（ADR-0038 の到達条件 2）。**
    //
    // **中身のあるブロック・全 0 のブロック・中身のあるブロック**の 3 つで、
    // **`mke2fs -d` は真ん中を穴にする**（zi.elf で実測した挙動と同じである）。
    //
    // **常設する理由は「無検査へ戻さない」ことである。** 穴を読む能力は
    // `/bin/zi` がたまたま穴を持ったことで発覚したが、**zi が伸びて穴が
    // 消えれば、その能力は誰も検査しない機構に戻る。** ここに 1 本置けば、
    // 像の作り方が変わらない限り穴は在り続ける。
    {
        const SPARSE_BLOCK: usize = 4096;
        let mut sparse = vec![0u8; SPARSE_BLOCK * 3];
        // **先頭と末尾だけを埋める。** 真ん中は 0 のままで、穴になる。
        for (index, byte) in sparse[..SPARSE_BLOCK].iter_mut().enumerate() {
            *byte = (index % 251) as u8 | 1;
        }
        for (index, byte) in sparse[SPARSE_BLOCK * 2..].iter_mut().enumerate() {
            *byte = (index % 241) as u8 | 1;
        }
        std::fs::write(format!("{staging}/data/sparse-hole"), &sparse)
            .expect("failed to write /data/sparse-hole into the staging");
    }

    let pattern: Vec<u8> = (0..DIRECT_MAX_BYTES + 1).map(|i| (i % 251) as u8).collect();
    std::fs::write(
        format!("{staging}/data/direct-max"),
        &pattern[..DIRECT_MAX_BYTES],
    )
    .expect("failed to write direct-max");
    std::fs::write(format!("{staging}/data/indirect-first"), &pattern[..])
        .expect("failed to write indirect-first");

    // **書き込みの的（S12-c）。** カーネルが追記する唯一のファイルである。
    //
    // **大きさをブロック境界にしない。** 100 バイトなら末尾のブロックに
    // 3996 バイト空いているので、**1 回目の追記は割り当てを起こさない**
    // （末尾の空きを埋めるだけ）。**2 回目で境界を越えて割り当てが起きる。**
    // **その 2 つの道を 1 本のファイルで通せる大きさを選んだ。**
    //
    // **既存の的へ書かない理由は 2 つ。** `/etc/motd` は `cat` の判定が
    // 中身を見ており、書くと落ちる。そして**短くて 1 ブロックの端にあるので、
    // 境界の場合分けが作りにくい。**
    let writable: Vec<u8> = (0..WRITABLE_SEED_BYTES).map(|i| (i % 251) as u8).collect();
    std::fs::write(format!("{staging}/data/writable"), &writable[..])
        .expect("failed to write the writable target");

    // 像の器を作る（ゼロ埋め）。
    let image = format!("{out_dir}/fs.img");
    let file = std::fs::File::create(&image).expect("failed to create the image file");
    file.set_len(IMAGE_BYTES)
        .expect("failed to size the image file");
    drop(file);

    let version = mke2fs_version();

    // UUID とハッシュシードを固定する。**残る差（時刻）は下で潰す。**
    let status = external_tool("mke2fs")
        .args([
            "-q",
            "-t",
            "ext2",
            "-U",
            "11111111-2222-3333-4444-555555555555",
            "-E",
            "hash_seed=66666666-7777-8888-9999-000000000000",
            "-d",
            &staging,
            &image,
        ])
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to run mke2fs: {e}. ZaytOS builds the ext2 test image with mke2fs \
                 (e2fsprogs); install it (for example `apt install e2fsprogs`). It is needed \
                 because roadmap S10 requires reading an image made by an outside tool, and \
                 S12 will verify ZaytOS's writes with e2fsck from the same package."
            )
        });
    assert!(status.success(), "mke2fs failed for the ext2 test image");

    zero_image_timestamps(&image);

    // **版を判定行へ載せる**（S10-a）。**別の版で既定値が変われば、決めた
    // パラメータ（block 4096・inode size 256・rev 1）が動く。**
    //
    // **2 本の大きさと最後の 1 バイトも一緒に出す**（S10-a の単一間接の刻み）。
    // **模様を決めているのはここなので、期待値をここから出す。** カーネル側へ
    // 書き写すと、模様を変えたときに片方だけが古くなる。
    let direct_max_last = pattern[DIRECT_MAX_BYTES - 1];
    let indirect_first_last = pattern[DIRECT_MAX_BYTES];
    // **像の中の番号を、建てた直後に外の道具から測る（e-4 の手当て）。**
    let motd = stat_of(&image, "/etc/motd");
    let indirect = stat_of(&image, "/data/indirect-first");
    let motd_inode = motd
        .inode
        .expect("debugfs did not report /etc/motd's inode");
    let motd_block = motd
        .first_block
        .expect("debugfs did not report /etc/motd's first block");
    let indirect_table = indirect
        .indirect_block
        .expect("debugfs did not report the single indirect block");
    let first_free = first_free_block(&image);
    std::fs::write(
        format!("{out_dir}/fsimage_info.rs"),
        format!(
            "// build.rs が生成した。手で編集しないこと。\n\
             pub const MKE2FS_VERSION: &str = {version:?};\n\
             pub const IMAGE_BYTES: u64 = {IMAGE_BYTES};\n\
             pub const DIRECT_MAX_BYTES: u64 = {DIRECT_MAX_BYTES};\n\
             pub const DIRECT_MAX_LAST_BYTE: u8 = {direct_max_last};\n\
             pub const INDIRECT_FIRST_BYTES: u64 = {};\n\
             pub const INDIRECT_FIRST_LAST_BYTE: u8 = {indirect_first_last};\n\
             pub const WRITABLE_SEED_BYTES: u64 = {WRITABLE_SEED_BYTES};\n\
             pub const MOTD_INODE: usize = {motd_inode};\n\
             pub const MOTD_DATA_BLOCK: usize = {motd_block};\n\
             pub const INDIRECT_TABLE_BLOCK: usize = {indirect_table};\n\
             pub const FIRST_FREE_BLOCK: usize = {first_free};\n",
            DIRECT_MAX_BYTES + 1
        ),
    )
    .expect("failed to write fsimage_info.rs");
}

/// 出力を解析する外の道具を呼ぶ（e-4 の後の手当て）。**言語を固定する。**
///
/// **`xtask` の `external_tool` と同じ形である**（あちらの doc に理由がある）。
/// **2 つの crate にまたがるので、入口は 2 つある**——**寄せられるのは
/// crate の中までで、`cargo xtask check` の静的検査が両方を見る。**
fn external_tool(name: &str) -> std::process::Command {
    let mut command = std::process::Command::new(name);
    command.env("LC_ALL", "C");
    command
}

/// `debugfs -R "stat <path>"` から拾う番号（e-4 の手当て）。
struct ImageStat {
    inode: Option<usize>,
    first_block: Option<usize>,
    indirect_block: Option<usize>,
}

/// 像の中の番号を `debugfs` に訊く（e-4 の手当て）。
///
/// # なぜ build.rs が測るのか
///
/// **像の中の inode 番号とブロック番号は、像の中身で決まる。**
/// **ユーザープログラムを変える feature（`USER_PROGRAM_CFGS`）を立てると
/// 像が変わる**ので、**構成ごとに番号が違う。**
///
/// **手で測って定数へ書く形は、その構成の分しか合わない。** 実際に踏んだ——
/// **`zi` が e-4 で 1 ブロック太り、`zi-test` の構成でだけ番号がずれて、
/// 壊した像の検算が「壊れていない」と言った**（`docs/troubleshooting.md`）。
///
/// **`user.ld` から受け皿の位置を読んで生成しているのと同じ形である**
/// （直す場所を 1 つにする）。**道具は増えていない**——`debugfs` は
/// `mke2fs` と同じ e2fsprogs にある。
fn stat_of(image: &str, path: &str) -> ImageStat {
    let output = external_tool("debugfs")
        .args(["-R", &format!("stat {path}"), image])
        .output()
        .unwrap_or_else(|e| {
            panic!("failed to run debugfs for {path}: {e}. ZaytOS measures the image numbers with debugfs (e2fsprogs)")
        });
    let text = String::from_utf8_lossy(&output.stdout);
    let mut stat = ImageStat {
        inode: None,
        first_block: None,
        indirect_block: None,
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Inode: ") {
            stat.inode = rest
                .split_whitespace()
                .next()
                .and_then(|number| number.parse().ok());
        }
        // **`BLOCKS:` の次の行に `(0):B` と `(IND):B` が並ぶ。**
        if line.starts_with('(') {
            for piece in line.split(", ") {
                if let Some(rest) = piece.strip_prefix("(0):") {
                    stat.first_block = rest.trim().parse().ok();
                }
                if let Some(rest) = piece.strip_prefix("(IND):") {
                    stat.indirect_block = rest.trim().parse().ok();
                }
            }
        }
    }
    stat
}

/// 最初の空きブロック（`dumpe2fs` の `Free blocks:` の先頭。e-4 の手当て）。
///
/// **使用しているブロックの上端 + 1 である。** 壊した像の検算は、
/// **ここまでを写せば像が読み切れる。**
fn first_free_block(image: &str) -> usize {
    let output = external_tool("dumpe2fs")
        .arg(image)
        .output()
        .unwrap_or_else(|e| panic!("failed to run dumpe2fs: {e} (e2fsprogs)"));
    let text = String::from_utf8_lossy(&output.stdout);
    // **群の節に入ってから読む。** **superblock の節にも `Free blocks:` が
    // あるが、あちらは「空きの数」で、こちらは「空きの範囲」である**
    // （実測。数のほうを読んで 419 を得た）。
    let mut in_group = false;
    for line in text.lines() {
        if line.starts_with("Group ") {
            in_group = true;
            continue;
        }
        if !in_group {
            continue;
        }
        if let Some(rest) = line.trim().strip_prefix("Free blocks: ") {
            // **`93-511` の形と、`93` だけの形がある。**
            let first = rest.split(&['-', ','][..]).next().unwrap_or("").trim();
            if let Ok(block) = first.parse::<usize>() {
                return block;
            }
        }
    }
    panic!("dumpe2fs did not report a free block range for the image")
}

/// `mke2fs -V` の 1 行目。**版を記録に残すためだけに読む。**
fn mke2fs_version() -> String {
    // `mke2fs -V` は版をコード 1 で標準エラーへ出す。**成否は見ない**——
    // 実際に建てるときの失敗が、不在の診断を出す側である。
    let output = external_tool("mke2fs").arg("-V").output();
    match output {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stderr);
            text.lines().next().unwrap_or("unknown").trim().to_string()
        }
        Err(_) => "unknown".to_string(),
    }
}

/// 種のディレクトリを丸ごと写す。**シンボリックリンクは扱わない**
/// （`roadmap.md` の S10 が symlink を範囲外と書いている）。
fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("failed to create a staging directory");
    let entries = std::fs::read_dir(from).expect("failed to read the seed directory");
    for entry in entries {
        let entry = entry.expect("failed to read a seed entry");
        let kind = entry.file_type().expect("failed to stat a seed entry");
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target);
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target).expect("failed to copy a seed file");
        } else {
            panic!(
                "the seed tree has an entry that is neither a file nor a directory: {:?}",
                entry.path()
            );
        }
    }
}

/// 像の時刻フィールドを 0 にして、出力を決定的にする（S10-a）。
///
/// **触るのは superblock の 4 つと、全 inode の 4 つだけである。**
/// superblock: `s_mtime`(44) / `s_wtime`(48) / `s_lastcheck`(64) / `s_mkfs_time`(264)。
/// inode: `i_atime`(8) / `i_ctime`(12) / `i_mtime`(16) / `i_dtime`(20)。
///
/// **inode の位置は group descriptor から引く。** ここが ext2 の読み取りと
/// 重なるが、**読むのは 1 フィールド（`bg_inode_table`）だけで、カーネル側の
/// パーサとは別物である。** 正しさは「2 回建てて md5 が一致すること」と
/// 「`e2fsck -fn` が clean と言うこと」で確かめる。
fn zero_image_timestamps(image: &str) {
    let mut bytes = std::fs::read(image).expect("failed to read the image back");

    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);

    const SUPERBLOCK_OFFSET: usize = 1024;
    let sb = SUPERBLOCK_OFFSET;
    let block_size = 1024usize << u32_at(&bytes, sb + 24);
    let inodes_count = u32_at(&bytes, sb) as usize;
    let inodes_per_group = u32_at(&bytes, sb + 40) as usize;
    let inode_size = u16_at(&bytes, sb + 88) as usize;
    let first_data_block = u32_at(&bytes, sb + 20) as usize;
    let group_count = inodes_count.div_ceil(inodes_per_group);

    // `s_mtime`(44) / `s_wtime`(48) / `s_lastcheck`(64) / `s_mkfs_time`(264)。
    // **`s_mkfs_time` は実測で見つけた**——最初は 3 つだけ潰し、2 回建てて
    // md5 が食い違ったので `cmp` で位置を出した（バイト 1288 = superblock+264）。
    for offset in [44usize, 48, 64, 264] {
        bytes[sb + offset..sb + offset + 4].copy_from_slice(&0u32.to_le_bytes());
    }

    // group descriptor テーブルは superblock の次のブロックから始まる。
    let gd_table = (first_data_block + 1) * block_size;
    for group in 0..group_count {
        let gd = gd_table + group * 32;
        let inode_table = u32_at(&bytes, gd + 8) as usize * block_size;
        for index in 0..inodes_per_group {
            let inode = inode_table + index * inode_size;
            if inode + inode_size > bytes.len() {
                break;
            }
            for offset in [8usize, 12, 16, 20] {
                bytes[inode + offset..inode + offset + 4].copy_from_slice(&0u32.to_le_bytes());
            }
            // **256 バイトの inode は、128 バイトの外に時刻をもう 5 つ持つ。**
            // `i_ctime_extra`(132) / `i_mtime_extra`(136) / `i_atime_extra`(140) /
            // **`i_crtime`(144)** / `i_crtime_extra`(148)。
            // **`i_crtime` も実測で見つけた**——2 回建てて 10 バイトだけ食い違い、
            // `cmp -l` の位置が inode の 144 に揃っていた。
            if inode_size >= 152 {
                for offset in [132usize, 136, 140, 144, 148] {
                    bytes[inode + offset..inode + offset + 4].copy_from_slice(&0u32.to_le_bytes());
                }
            }
        }
    }

    std::fs::write(image, &bytes).expect("failed to write the image back");
}
