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
    const PROGRAMS: &[&str] = &["hello", "fault-test", "syscall-test"];

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

    for name in PROGRAMS {
        let source = format!("{manifest_dir}/userland/{name}.rs");
        let output = format!("{out_dir}/{name}.elf");
        println!("cargo:rerun-if-changed={source}");

        let status = std::process::Command::new(std::env::var("RUSTC").unwrap_or("rustc".into()))
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
    std::fs::copy(
        format!("{out_dir}/hello.elf"),
        format!("{staging}/bin/hello"),
    )
    .expect("failed to place hello into the staging");

    // **単一間接ブロックの境界を挟む 2 本。** 直接ブロックは 12 個なので、
    // 12 ブロックちょうどは間接を使わず、1 バイト超えると使う。
    std::fs::create_dir_all(format!("{staging}/data"))
        .expect("failed to create /data in the staging");
    let pattern: Vec<u8> = (0..DIRECT_MAX_BYTES + 1).map(|i| (i % 251) as u8).collect();
    std::fs::write(
        format!("{staging}/data/direct-max"),
        &pattern[..DIRECT_MAX_BYTES],
    )
    .expect("failed to write direct-max");
    std::fs::write(format!("{staging}/data/indirect-first"), &pattern[..])
        .expect("failed to write indirect-first");

    // 像の器を作る（ゼロ埋め）。
    let image = format!("{out_dir}/fs.img");
    let file = std::fs::File::create(&image).expect("failed to create the image file");
    file.set_len(IMAGE_BYTES)
        .expect("failed to size the image file");
    drop(file);

    let version = mke2fs_version();

    // UUID とハッシュシードを固定する。**残る差（時刻）は下で潰す。**
    let status = std::process::Command::new("mke2fs")
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
    std::fs::write(
        format!("{out_dir}/fsimage_info.rs"),
        format!(
            "// build.rs が生成した。手で編集しないこと。\n\
             pub const MKE2FS_VERSION: &str = {version:?};\n\
             pub const IMAGE_BYTES: u64 = {IMAGE_BYTES};\n\
             pub const DIRECT_MAX_BYTES: u64 = {DIRECT_MAX_BYTES};\n\
             pub const DIRECT_MAX_LAST_BYTE: u8 = {direct_max_last};\n\
             pub const INDIRECT_FIRST_BYTES: u64 = {};\n\
             pub const INDIRECT_FIRST_LAST_BYTE: u8 = {indirect_first_last};\n",
            DIRECT_MAX_BYTES + 1
        ),
    )
    .expect("failed to write fsimage_info.rs");
}

/// `mke2fs -V` の 1 行目。**版を記録に残すためだけに読む。**
fn mke2fs_version() -> String {
    // `mke2fs -V` は版をコード 1 で標準エラーへ出す。**成否は見ない**——
    // 実際に建てるときの失敗が、不在の診断を出す側である。
    let output = std::process::Command::new("mke2fs").arg("-V").output();
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
