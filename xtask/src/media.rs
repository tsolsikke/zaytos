//! 起動媒体の像（`ADR-0068` の HW-e）。**GPT と FAT32 を自分で書き、書いた像を自分で読み返す。**
//!
//! # 何のために在るのか
//!
//! **QEMU の `fat:rw:<dir>` は QEMU だけの道である。** **VirtualBox と実機は「ディレクトリを
//! FAT に見せる」機能を持たない**——**1 つのファイルに、分割表（GPT）と ESP（FAT32）と
//! 3 つの成果物を入れて渡す。**
//!
//! # 何をしないか
//!
//! - **読み書きできる FAT の実装ではない。** **1 度だけ書き、そのまま読み返すだけである**
//!   （[`read_boot_media`]）。**空きの再利用も、断片化も、名前の変更も無い。**
//! - **長い名前（LFN）を書かない。** **置く 7 つの名前は全部 8.3 に収まる**——**小文字は
//!   予約バイトの旗で見せる**（[`short_name`]）。**収まらない名前を渡されたら拒む**
//!   ——**黙って切り詰めると、ファームウェアが別の名前で探すことになる。**
//! - **ファイルを触らない。** **入口も出口もバイト列である**（`std::fs` を使わない）
//!   ——**呼ぶ側（`xtask` の `cmd_image`）が読み書きする。** **こうしておくと、ホストの
//!   テストが像を建てて読み返すところまでを、ファイルを 1 つも作らずに確かめられる。**
//! - **時刻を持たない。** **FAT の紀元（1980-01-01 00:00）に固定する**——**像を決定的に
//!   する**（`kernel/build.rs` が ext2 の時刻を潰しているのと同じ理由。**同じ木から建てた
//!   像は、いつ建てても同じバイト列である**）。
//!
//! # なぜ外の道具を使わないのか
//!
//! **`mkfs.vfat`（dosfstools）も `mtools` も、この環境に入っていない**（実測。2026-09-23）。
//! **入れれば済むが、`cargo xtask check` が新しいパッケージを要求するようになる**
//! ——**ext2 の側は `mke2fs` を要求しているので前例は在る**が、**FAT32 は「1 度書いて読むだけ」
//! なので、自分で書くほうが小さい**（この節の「何をしないか」のぶんだけ小さい）。
//! **分割表の側は外から確かめる**——**`sfdisk` は util-linux に在り、どの Linux にも入っている**
//! （`cmd_image` が `--json` で読み、こちらの書いた値と突き合わせる）。

use anyhow::{bail, Result};

/// 1 セクタのバイト数。**512 だけを扱う。**
///
/// **UEFI は 512 と 4096 を許すが、QEMU も VirtualBox も既定で 512 で見せる**
/// （実測。`sfdisk --json` の `sectorsize`）。**4096 の媒体を扱うときは、この定数ではなく
/// 「どこから来た値か」を設計し直すこと**——**BPB の `BytsPerSec` と GPT の LBA の両方が動く。**
pub const SECTOR_BYTES: usize = 512;

/// ESP が始まる LBA。**1MiB 境界**（2048 × 512）。
///
/// **`sgdisk` の既定と同じ値である**（実測。2026-09-23）——**合わせておくと、外の道具で
/// 読んだときに「同じ形」であることが一目で分かる。**
pub const ESP_FIRST_LBA: u64 = 2048;

/// ESP の大きさ（セクタ数）。**64MiB。**
///
/// # なぜ 64MiB なのか。**FAT32 の下限がすぐ下に在る**
///
/// **置くものは 9.4MB である**（実測。`kernel.elf` 7.2MB・`fs.img` 2MiB・`BOOTX64.EFI` 156KiB）。
/// **64MiB はその 7 倍で、像の大きさは 66MiB に収まる。**
///
/// **これより小さくすると FAT32 で建てられない。** **FAT32 は塊の数が 65,525 以上であることを
/// 要求する**（[`MIN_FAT32_CLUSTERS`]）。**64MiB を 1 セクタ 1 塊で切ると 129,022 塊で、
/// 下限の約 2 倍である**（[`Fat32Layout::for_sectors`] の doc に数が在る）。**32MiB では、
/// 1 セクタ 1 塊にしても 65,000 塊ほどで下限を割る。**
///
/// **塊を 2 セクタ（1KiB）にすると、64MiB でも下限を割る**（65,000 ほど）——**だから
/// [`SECTORS_PER_CLUSTER`] は 1 である。** **これは Microsoft の表（fatgen103 の
/// `DskSzToSecPerClus`）と同じ選択で、260MB 以下の FAT32 は 1 セクタ 1 塊である。**
pub const ESP_SECTORS: u64 = 131_072;

/// 像全体の大きさ（セクタ数）。**66MiB。**
///
/// **先頭の 2048 セクタ（保護 MBR と GPT と隙間）と、末尾の 33 セクタ（控えの GPT）が要る。**
/// **`sgdisk` で同じ大きさの像を建てると `lastlba` が 135,134 になる**（実測。2026-09-23）
/// ——**こちらの計算と一致する**（135,168 − 34）。
pub const IMAGE_SECTORS: u64 = 135_168;

/// 1 塊のセクタ数。**1**（[`ESP_SECTORS`] の doc に理由が在る）。
pub const SECTORS_PER_CLUSTER: u32 = 1;

/// FAT32 が要求する塊の数の下限。**これを下回る像は FAT16 として読まれる**（fatgen103）。
pub const MIN_FAT32_CLUSTERS: u32 = 65_525;

/// FAT32 が扱える塊の数の上限（fatgen103）。**0x0FFFFFF7 以上は特別な値である。**
pub const MAX_FAT32_CLUSTERS: u32 = 0x0FFF_FFF5;

/// ESP の型の GUID（UEFI 仕様。C12A7328-F81F-11D2-BA4B-00A0C93EC93B）。
///
/// **前の 3 つの欄は小端で、後の 2 つはそのまま並ぶ**（GUID の記法の決まり）。
const ESP_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];

/// ESP の型の GUID の文字の形。**外の道具の出力と突き合わせるために持つ**
/// （`xtask` の `verify_partition_table`）。**[`ESP_TYPE_GUID`] と同じものであることを、
/// ホストのテストが見る。**
pub const ESP_TYPE_GUID_TEXT: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";

/// 像の GUID。**固定である。**
///
/// # なぜ固定なのか。**像を決定的にするため**
///
/// **建てるたびに違う GUID を振ると、像のバイト列が毎回変わる**——**同じ木から建てた像が
/// 同じであることを、チェックサムで言えなくなる**（`kernel/build.rs` が ext2 の時刻を
/// 潰しているのと同じ理由）。
///
/// **失うもの**——**同じ機械に ZaytOS の媒体を 2 つ繋ぐと、GUID がぶつかる。** **1 つずつ
/// 使う前提である。** **VirtualBox は媒体を自分の UUID で見分けるので、そちらとは関係が無い**
/// （VDI の UUID は `convertfromraw` が振る）。
const DISK_GUID: [u8; 16] = [
    0x2c, 0x1a, 0x1d, 0x9e, 0x3b, 0x5f, 0x7a, 0x4e, 0x9c, 0x61, 0x0a, 0x7b, 0x5d, 0x2e, 0x4f, 0x80,
];

/// ESP の GUID。**固定である**（[`DISK_GUID`] と同じ理由）。
const PARTITION_GUID: [u8; 16] = [
    0xd4, 0x21, 0x8f, 0x3c, 0x0e, 0x6b, 0x55, 0x4a, 0x8d, 0x93, 0x1f, 0x62, 0xa7, 0xc4, 0xb0, 0x19,
];

/// GPT の分割の項目の数。**仕様の下限がこの数である**（128 × 128 バイト = 32 セクタ）。
const GPT_ENTRIES: u32 = 128;

/// GPT の分割の項目 1 つのバイト数。
const GPT_ENTRY_BYTES: u32 = 128;

/// GPT のヘッダが使うバイト数（残りはゼロ）。
const GPT_HEADER_BYTES: u32 = 92;

/// 分割の名前（UTF-16LE で 36 文字ぶんの欄に入れる）。
const PARTITION_NAME: &str = "EFI System Partition";

/// FAT のボリュームラベル（11 バイト。8.3 と同じ詰め方）。
const VOLUME_LABEL: &[u8; 11] = b"ZAYTOS     ";

/// FAT のボリュームの番号。**固定である**（[`DISK_GUID`] と同じ理由）。
const VOLUME_ID: u32 = 0x5a41_5954;

/// 予約セクタの数（FAT32 の慣行。0 が起動セクタ、1 が FSInfo、6 と 7 に控えが入る）。
const RESERVED_SECTORS: u32 = 32;

/// FAT の数。**2**（慣行。控えを持つ）。
const FAT_COUNT: u32 = 2;

/// 控えの起動セクタの位置（予約領域の中の相対セクタ）。
const BACKUP_BOOT_SECTOR: u32 = 6;

/// FAT の紀元の日付（1980-01-01）。**年の欄（上位 7 ビット）は 0 で、月が 1、日が 1。**
const FAT_EPOCH_DATE: u16 = (1 << 5) | 1;

// ---------------------------------------------------------------------------
// CRC32（GPT が要求する。IEEE 802.3 の多項式）
// ---------------------------------------------------------------------------

/// GPT のヘッダと項目の配列に要る CRC32。**表を持たない**（要るのは 1 回の像の組み立てだけで、
/// 速さは関係が無い）。
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = if crc & 1 != 0 { 0xEDB8_8320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}

// ---------------------------------------------------------------------------
// FAT32 の幾何
// ---------------------------------------------------------------------------

/// FAT32 の幾何（純粋ロジック）。**BPB に書く数はすべてここから出る。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fat32Layout {
    /// 分割全体のセクタ数。
    pub total_sectors: u32,
    /// 予約セクタの数。
    pub reserved_sectors: u32,
    /// FAT の数。
    pub fats: u32,
    /// FAT 1 つのセクタ数。
    pub sectors_per_fat: u32,
    /// 1 塊のセクタ数。
    pub sectors_per_cluster: u32,
    /// 塊の数（**2 番から数えた、実際に使える塊の数**）。
    pub clusters: u32,
}

impl Fat32Layout {
    /// 分割のセクタ数から幾何を決める。
    ///
    /// # FAT の大きさは、自分自身に依る
    ///
    /// **FAT の大きさは塊の数で決まり、塊の数は FAT の大きさで決まる。** **大きい側から
    /// 縮めて、最小の不動点を採る**——**先に「足りる値」まで増やし、そこから 1 ずつ減らして
    /// 足りなくなる 1 つ前で止める。**
    ///
    /// **64MiB（131,072 セクタ）を 1 セクタ 1 塊で切ると、FAT は 1,009 セクタ、塊は
    /// 129,022 である**（実測ではなく、この関数の計算。ホストのテストが数を固定している）。
    /// **1,008 セクタでは足りない**——**塊が 129,024 になり、項目が 129,026 個で
    /// 1,009 セクタ要る。** **1 つの値の周りで振動するので、「足りるほう」を採る。**
    ///
    /// # 塊の数を数え直す
    ///
    /// **決めた FAT の大きさから塊の数を引き算で出し直し、FAT32 の範囲に入っていることを
    /// 見る**（[`MIN_FAT32_CLUSTERS`]・[`MAX_FAT32_CLUSTERS`]）。**下限を割った像は FAT16
    /// として読まれる**——**ファームウェアはこちらの意図を知らないので、BPB の
    /// `FilSysType`（文字列）を見て FAT32 と信じることはしない**（fatgen103 は
    /// 「あの文字列を判定に使ってはならない」と明記している）。
    pub fn for_sectors(total_sectors: u32, sectors_per_cluster: u32) -> Result<Self> {
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            bail!("sectors per cluster must be a power of two, got {sectors_per_cluster}");
        }
        let overhead = RESERVED_SECTORS;
        if total_sectors <= overhead + FAT_COUNT {
            bail!("a partition of {total_sectors} sector(s) is too small for FAT32");
        }
        let entries_per_sector = (SECTOR_BYTES / 4) as u32;
        let clusters_for = |sectors_per_fat: u32| -> u32 {
            let data = total_sectors - overhead - FAT_COUNT * sectors_per_fat;
            data / sectors_per_cluster
        };
        let needed_for = |sectors_per_fat: u32| -> u32 {
            // **FAT の 0 番と 1 番は塊ではない**（媒体の種別と終わりの印）——**+2 する。**
            (clusters_for(sectors_per_fat) + 2).div_ceil(entries_per_sector)
        };
        // **足りる値まで増やす。**
        let mut sectors_per_fat = 1u32;
        for _ in 0..64 {
            let needed = needed_for(sectors_per_fat);
            if needed <= sectors_per_fat {
                break;
            }
            sectors_per_fat = needed;
        }
        if needed_for(sectors_per_fat) > sectors_per_fat {
            bail!("the FAT size did not converge for {total_sectors} sector(s)");
        }
        // **最小の不動点まで縮める。**
        while sectors_per_fat > 1 && needed_for(sectors_per_fat - 1) < sectors_per_fat {
            sectors_per_fat -= 1;
        }
        let clusters = clusters_for(sectors_per_fat);
        if clusters < MIN_FAT32_CLUSTERS {
            bail!(
                "{total_sectors} sector(s) at {sectors_per_cluster} sector(s) per cluster gives \
                 {clusters} cluster(s), below the {MIN_FAT32_CLUSTERS} that FAT32 requires \
                 (such a volume is read as FAT16)"
            );
        }
        if clusters >= MAX_FAT32_CLUSTERS {
            bail!(
                "{total_sectors} sector(s) at {sectors_per_cluster} sector(s) per cluster gives \
                 {clusters} cluster(s), at or above the FAT32 limit of {MAX_FAT32_CLUSTERS}"
            );
        }
        Ok(Self {
            total_sectors,
            reserved_sectors: RESERVED_SECTORS,
            fats: FAT_COUNT,
            sectors_per_fat,
            sectors_per_cluster,
            clusters,
        })
    }

    /// データ領域が始まるセクタ（分割の先頭から数える）。
    pub fn first_data_sector(&self) -> u32 {
        self.reserved_sectors + self.fats * self.sectors_per_fat
    }

    /// 塊の番号から、分割の先頭から数えたセクタを出す。**2 番が最初の塊である。**
    pub fn cluster_sector(&self, cluster: u32) -> u32 {
        self.first_data_sector() + (cluster - 2) * self.sectors_per_cluster
    }

    /// 1 塊のバイト数。
    pub fn bytes_per_cluster(&self) -> u32 {
        self.sectors_per_cluster * SECTOR_BYTES as u32
    }

    /// 使える塊の番号の上限（この番号を含む）。
    pub fn last_cluster(&self) -> u32 {
        self.clusters + 1
    }
}

// ---------------------------------------------------------------------------
// 8.3 の名前
// ---------------------------------------------------------------------------

/// 8.3 の名前に使える文字（fatgen103 の一覧から、こちらが要るものだけ）。
fn is_short_name_char(byte: u8) -> bool {
    byte.is_ascii_uppercase()
        || byte.is_ascii_lowercase()
        || byte.is_ascii_digit()
        || matches!(
            byte,
            b'$' | b'%' | b'-' | b'_' | b'@' | b'~' | b'!' | b'(' | b')'
        )
}

/// 名前を 8.3 の 11 バイトと、小文字の旗へ写す（純粋ロジック）。
///
/// # 小文字は旗で見せる。**LFN は書かない**
///
/// **FAT の名前の欄は大文字である。** **予約バイト（12 番）の 0x08 が「本体は小文字」、
/// 0x10 が「拡張子は小文字」を表す**（Windows NT が足した慣行で、Linux の vfat も見る）。
/// **`zaytos`・`kernel.elf`・`fs.img`・`startup.nsh` は、これで小文字のまま見える。**
///
/// **ファームウェアがこの旗を見なくても、探す側は困らない**——**FAT の名前の照合は
/// 大文字小文字を区別しない**（UEFI 仕様の `EFI_FILE_PROTOCOL.Open`）。**旗が無視されると、
/// Linux で見たときに大文字で出るだけである**（見た目だけの違いである）。
///
/// **本体と拡張子は、それぞれ全部大文字か全部小文字でなければ拒む**——**旗が部分ごとに
/// 1 ビットしか無いので、混ざった名前は表せない。** **表せないものを黙って大文字にすると、
/// 「置いた名前」と「見える名前」が違う像になる。**
pub fn short_name(name: &str) -> Result<([u8; 11], u8)> {
    let (base, extension) = match name.rsplit_once('.') {
        Some((base, extension)) => (base, extension),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 {
        bail!(
            "{name:?} does not fit 8.3: the base is {} byte(s)",
            base.len()
        );
    }
    if extension.len() > 3 {
        bail!(
            "{name:?} does not fit 8.3: the extension is {} byte(s)",
            extension.len()
        );
    }
    let mut flags = 0u8;
    let mut bytes = *b"           ";
    for (part, offset, flag) in [(base, 0usize, 0x08u8), (extension, 8usize, 0x10u8)] {
        if part.is_empty() {
            continue;
        }
        let lower = part.bytes().all(|b| !b.is_ascii_uppercase());
        let upper = part.bytes().all(|b| !b.is_ascii_lowercase());
        if !lower && !upper {
            bail!(
                "{name:?} mixes upper and lower case in {part:?}; the FAT case flag is one bit \
                 per part, so such a name cannot be written without LFN entries"
            );
        }
        if lower && part.bytes().any(|b| b.is_ascii_lowercase()) {
            flags |= flag;
        }
        for (index, byte) in part.bytes().enumerate() {
            if !is_short_name_char(byte) {
                bail!("{name:?} has a byte {byte:#04x} that 8.3 names cannot hold");
            }
            bytes[offset + index] = byte.to_ascii_uppercase();
        }
    }
    Ok((bytes, flags))
}

/// 8.3 の 11 バイトと旗から、元の名前へ戻す（[`read_boot_media`] が使う）。
fn long_name(bytes: &[u8; 11], flags: u8) -> String {
    let mut name = String::new();
    for byte in bytes[..8].iter().copied() {
        if byte == b' ' {
            break;
        }
        name.push(case_of(byte, flags & 0x08 != 0));
    }
    let extension: String = bytes[8..]
        .iter()
        .copied()
        .take_while(|&byte| byte != b' ')
        .map(|byte| case_of(byte, flags & 0x10 != 0))
        .collect();
    if !extension.is_empty() {
        name.push('.');
        name.push_str(&extension);
    }
    name
}

fn case_of(byte: u8, lower: bool) -> char {
    let byte = if lower {
        byte.to_ascii_lowercase()
    } else {
        byte
    };
    char::from(byte)
}

// ---------------------------------------------------------------------------
// 置くものの木
// ---------------------------------------------------------------------------

/// 像に置くもの（呼ぶ側が渡す形）。**`path` は `/` で区切った、ESP の根からの道である。**
pub struct MediaFile<'a> {
    pub path: &'a str,
    pub bytes: &'a [u8],
}

/// 組み立ての途中の節（内部）。**添字で繋ぐ**——**親から子へも、子から親へも辿るため。**
struct Node {
    name: String,
    kind: NodeKind,
    /// 最初の塊の番号（[`assign_clusters`] が入れる）。
    first_cluster: u32,
    /// 中身のバイト数（ディレクトリは項目の数 × 32）。
    byte_len: u32,
}

enum NodeKind {
    Dir(Vec<usize>),
    File(usize),
}

/// 建てた像。
pub struct BootMedia {
    /// 像そのもの（[`IMAGE_SECTORS`] × [`SECTOR_BYTES`] バイト）。
    pub bytes: Vec<u8>,
    /// 使った幾何（判定行に出す）。
    pub layout: Fat32Layout,
    /// 使った塊の数。
    pub used_clusters: u32,
}

/// 起動媒体の像を建てる（`ADR-0068` の HW-e）。
///
/// **渡された順序のまま並べる**——**ディレクトリの項目の順も、塊の割り当ての順も、
/// 渡された順である。** **像が決定的であることは、この順序に依る。**
pub fn build_boot_media(files: &[MediaFile]) -> Result<BootMedia> {
    let esp = build_fat32(ESP_SECTORS as u32, SECTORS_PER_CLUSTER, files)?;
    let mut bytes = vec![0u8; IMAGE_SECTORS as usize * SECTOR_BYTES];
    let start = ESP_FIRST_LBA as usize * SECTOR_BYTES;
    bytes[start..start + esp.bytes.len()].copy_from_slice(&esp.bytes);
    write_protective_mbr(&mut bytes);
    write_gpt(&mut bytes);
    Ok(BootMedia {
        bytes,
        layout: esp.layout,
        used_clusters: esp.used_clusters,
    })
}

/// FAT32 の分割 1 つを建てる（純粋ロジック。**GPT を知らない**）。
fn build_fat32(total_sectors: u32, sectors_per_cluster: u32, files: &[MediaFile]) -> Result<Fat32> {
    let layout = Fat32Layout::for_sectors(total_sectors, sectors_per_cluster)?;
    let mut nodes: Vec<Node> = vec![Node {
        name: String::new(),
        kind: NodeKind::Dir(Vec::new()),
        first_cluster: 0,
        byte_len: 0,
    }];
    for (index, file) in files.iter().enumerate() {
        let byte_len = match u32::try_from(file.bytes.len()) {
            Ok(len) => len,
            Err(_) => bail!(
                "{} is {} byte(s); FAT32 holds at most 4GiB per file",
                file.path,
                file.bytes.len()
            ),
        };
        insert(&mut nodes, file.path, index, byte_len)?;
    }
    // **ディレクトリの大きさを先に決める**（項目の数で決まる。**根はボリュームラベルが
    // 1 つ、他は `.` と `..` が 2 つ余分に入る**）。
    let sizes: Vec<(usize, u32)> = nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| match &node.kind {
            NodeKind::Dir(children) => {
                let extra = if index == 0 { 1 } else { 2 };
                Some((index, (children.len() as u32 + extra) * 32))
            }
            NodeKind::File(_) => None,
        })
        .collect();
    for (index, size) in sizes {
        nodes[index].byte_len = size;
    }
    let used_clusters = assign_clusters(&mut nodes, &layout)?;

    let mut bytes = vec![0u8; total_sectors as usize * SECTOR_BYTES];
    let mut fat = vec![0u32; (layout.clusters + 2) as usize];
    fat[0] = 0x0FFF_FFF8;
    fat[1] = 0x0FFF_FFFF;
    for node in 0..nodes.len() {
        let content = serialise_node(&nodes, node, files);
        let clusters = chain_of(&nodes[node], &layout);
        for (step, cluster) in clusters.iter().copied().enumerate() {
            fat[cluster as usize] = match clusters.get(step + 1) {
                Some(&next) => next,
                None => 0x0FFF_FFFF,
            };
            let at = layout.cluster_sector(cluster) as usize * SECTOR_BYTES;
            let from = step * layout.bytes_per_cluster() as usize;
            let take = layout.bytes_per_cluster() as usize;
            let slice = &content[from..content.len().min(from + take)];
            bytes[at..at + slice.len()].copy_from_slice(slice);
        }
    }
    write_boot_sector(&mut bytes, &layout);
    write_fs_info(&mut bytes, &layout, used_clusters);
    write_fat(&mut bytes, &layout, &fat);
    Ok(Fat32 {
        bytes,
        layout,
        used_clusters,
    })
}

struct Fat32 {
    bytes: Vec<u8>,
    layout: Fat32Layout,
    used_clusters: u32,
}

/// 木へ 1 つ挿す。**途中のディレクトリは、無ければ作る。**
fn insert(nodes: &mut Vec<Node>, path: &str, file: usize, byte_len: u32) -> Result<()> {
    let mut at = 0usize;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty() {
            bail!("{path:?} has an empty component");
        }
        // **名前が 8.3 に収まることを、ここで見る**（挿す時点で拒む）。
        short_name(part)?;
        let last = parts.peek().is_none();
        let existing = match &nodes[at].kind {
            NodeKind::Dir(children) => children
                .iter()
                .copied()
                .find(|&child| nodes[child].name == part),
            NodeKind::File(_) => bail!("{path:?} goes through a file"),
        };
        match existing {
            Some(_) if last => bail!("{path:?} is listed twice"),
            Some(child) => at = child,
            None => {
                let node = nodes.len();
                nodes.push(Node {
                    name: part.to_string(),
                    kind: if last {
                        NodeKind::File(file)
                    } else {
                        NodeKind::Dir(Vec::new())
                    },
                    first_cluster: 0,
                    byte_len: if last { byte_len } else { 0 },
                });
                let NodeKind::Dir(children) = &mut nodes[at].kind else {
                    unreachable!("checked above");
                };
                children.push(node);
                at = node;
            }
        }
    }
    Ok(())
}

/// 塊を前から順に割り当てる（**深さ優先。親が先**）。**返すのは使った塊の数である。**
fn assign_clusters(nodes: &mut [Node], layout: &Fat32Layout) -> Result<u32> {
    let mut next = 2u32;
    let order = preorder(nodes, 0);
    for node in order {
        let count = nodes[node]
            .byte_len
            .div_ceil(layout.bytes_per_cluster())
            .max(1);
        nodes[node].first_cluster = next;
        next += count;
        if next > layout.last_cluster() + 1 {
            bail!(
                "the files need more than the {} cluster(s) the volume has",
                layout.clusters
            );
        }
    }
    Ok(next - 2)
}

fn preorder(nodes: &[Node], at: usize) -> Vec<usize> {
    let mut order = vec![at];
    if let NodeKind::Dir(children) = &nodes[at].kind {
        for &child in children {
            order.extend(preorder(nodes, child));
        }
    }
    order
}

/// 節が占める塊の列。
fn chain_of(node: &Node, layout: &Fat32Layout) -> Vec<u32> {
    let count = node.byte_len.div_ceil(layout.bytes_per_cluster()).max(1);
    (node.first_cluster..node.first_cluster + count).collect()
}

/// 節の中身をバイト列にする。**ディレクトリは 32 バイトの項目の列である。**
fn serialise_node(nodes: &[Node], at: usize, files: &[MediaFile]) -> Vec<u8> {
    match &nodes[at].kind {
        NodeKind::File(index) => files[*index].bytes.to_vec(),
        NodeKind::Dir(children) => {
            let mut bytes = Vec::new();
            if at == 0 {
                bytes.extend_from_slice(&dir_entry(VOLUME_LABEL, 0x08, 0, 0, 0));
            } else {
                let parent = nodes
                    .iter()
                    .position(|node| match &node.kind {
                        NodeKind::Dir(children) => children.contains(&at),
                        NodeKind::File(_) => false,
                    })
                    .expect("every directory but the root has a parent");
                bytes.extend_from_slice(&dir_entry(
                    b".          ",
                    0x10,
                    0,
                    nodes[at].first_cluster,
                    0,
                ));
                // **根を指す `..` の塊は 0 である**（FAT の決まり）。
                let up = if parent == 0 {
                    0
                } else {
                    nodes[parent].first_cluster
                };
                bytes.extend_from_slice(&dir_entry(b"..         ", 0x10, 0, up, 0));
            }
            for &child in children {
                let (name, flags) =
                    short_name(&nodes[child].name).expect("the name was checked when inserted");
                let (attr, size) = match nodes[child].kind {
                    NodeKind::Dir(_) => (0x10u8, 0),
                    NodeKind::File(_) => (0x20u8, nodes[child].byte_len),
                };
                bytes.extend_from_slice(&dir_entry(
                    &name,
                    attr,
                    flags,
                    nodes[child].first_cluster,
                    size,
                ));
            }
            bytes
        }
    }
}

/// 32 バイトの項目を作る。**時刻は FAT の紀元に固定する。**
fn dir_entry(name: &[u8; 11], attr: u8, case_flags: u8, cluster: u32, size: u32) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[..11].copy_from_slice(name);
    entry[11] = attr;
    entry[12] = case_flags;
    entry[16..18].copy_from_slice(&FAT_EPOCH_DATE.to_le_bytes());
    entry[18..20].copy_from_slice(&FAT_EPOCH_DATE.to_le_bytes());
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    entry[24..26].copy_from_slice(&FAT_EPOCH_DATE.to_le_bytes());
    entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

/// 起動セクタ（BPB）を書く。**控え（6 番）にも同じものを書く。**
fn write_boot_sector(bytes: &mut [u8], layout: &Fat32Layout) {
    let mut sector = [0u8; SECTOR_BYTES];
    sector[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    sector[3..11].copy_from_slice(b"MSWIN4.1");
    sector[11..13].copy_from_slice(&(SECTOR_BYTES as u16).to_le_bytes());
    sector[13] = layout.sectors_per_cluster as u8;
    sector[14..16].copy_from_slice(&(layout.reserved_sectors as u16).to_le_bytes());
    sector[16] = layout.fats as u8;
    sector[21] = 0xF8;
    sector[24..26].copy_from_slice(&63u16.to_le_bytes());
    sector[26..28].copy_from_slice(&255u16.to_le_bytes());
    sector[28..32].copy_from_slice(&(ESP_FIRST_LBA as u32).to_le_bytes());
    sector[32..36].copy_from_slice(&layout.total_sectors.to_le_bytes());
    sector[36..40].copy_from_slice(&layout.sectors_per_fat.to_le_bytes());
    sector[44..48].copy_from_slice(&2u32.to_le_bytes());
    sector[48..50].copy_from_slice(&1u16.to_le_bytes());
    sector[50..52].copy_from_slice(&(BACKUP_BOOT_SECTOR as u16).to_le_bytes());
    sector[64] = 0x80;
    sector[66] = 0x29;
    sector[67..71].copy_from_slice(&VOLUME_ID.to_le_bytes());
    sector[71..82].copy_from_slice(VOLUME_LABEL);
    sector[82..90].copy_from_slice(b"FAT32   ");
    sector[510..512].copy_from_slice(&[0x55, 0xAA]);
    bytes[..SECTOR_BYTES].copy_from_slice(&sector);
    let backup = BACKUP_BOOT_SECTOR as usize * SECTOR_BYTES;
    bytes[backup..backup + SECTOR_BYTES].copy_from_slice(&sector);
}

/// FSInfo を書く（1 番と、控えの 7 番）。**空きの数は「報せ」であって権威ではない**が、
/// **嘘を書くと道具が文句を言うので、正しく入れる。**
fn write_fs_info(bytes: &mut [u8], layout: &Fat32Layout, used_clusters: u32) {
    let mut sector = [0u8; SECTOR_BYTES];
    sector[..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    sector[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    sector[488..492].copy_from_slice(&(layout.clusters - used_clusters).to_le_bytes());
    sector[492..496].copy_from_slice(&(used_clusters + 2).to_le_bytes());
    sector[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    bytes[SECTOR_BYTES..2 * SECTOR_BYTES].copy_from_slice(&sector);
    let backup = (BACKUP_BOOT_SECTOR as usize + 1) * SECTOR_BYTES;
    bytes[backup..backup + SECTOR_BYTES].copy_from_slice(&sector);
}

/// FAT を書く。**2 つとも同じ中身である。**
fn write_fat(bytes: &mut [u8], layout: &Fat32Layout, fat: &[u32]) {
    for index in 0..layout.fats {
        let at = (layout.reserved_sectors + index * layout.sectors_per_fat) as usize * SECTOR_BYTES;
        for (entry, value) in fat.iter().copied().enumerate() {
            let offset = at + entry * 4;
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
    }
}

// ---------------------------------------------------------------------------
// GPT
// ---------------------------------------------------------------------------

/// 保護 MBR（GPT の前に置く。**GPT を知らない道具が「空でない」と見るためのものである**）。
fn write_protective_mbr(bytes: &mut [u8]) {
    let entry = 446usize;
    bytes[entry] = 0x00;
    bytes[entry + 1..entry + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    bytes[entry + 4] = 0xEE;
    bytes[entry + 5..entry + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    bytes[entry + 8..entry + 12].copy_from_slice(&1u32.to_le_bytes());
    let sectors = u32::try_from(IMAGE_SECTORS - 1).unwrap_or(u32::MAX);
    bytes[entry + 12..entry + 16].copy_from_slice(&sectors.to_le_bytes());
    bytes[510..512].copy_from_slice(&[0x55, 0xAA]);
}

/// GPT のヘッダと項目の配列を、正と控えの両方に書く。
fn write_gpt(bytes: &mut [u8]) {
    let entries_sectors = (GPT_ENTRIES * GPT_ENTRY_BYTES) / SECTOR_BYTES as u32;
    let last_lba = IMAGE_SECTORS - 1;
    let backup_entries_lba = last_lba - u64::from(entries_sectors);
    let mut entries = vec![0u8; (GPT_ENTRIES * GPT_ENTRY_BYTES) as usize];
    entries[..16].copy_from_slice(&ESP_TYPE_GUID);
    entries[16..32].copy_from_slice(&PARTITION_GUID);
    entries[32..40].copy_from_slice(&ESP_FIRST_LBA.to_le_bytes());
    entries[40..48].copy_from_slice(&(ESP_FIRST_LBA + ESP_SECTORS - 1).to_le_bytes());
    for (index, unit) in PARTITION_NAME.encode_utf16().enumerate() {
        let at = 56 + index * 2;
        entries[at..at + 2].copy_from_slice(&unit.to_le_bytes());
    }
    let entries_crc = crc32(&entries);
    for (my_lba, alternate_lba, entries_lba) in
        [(1u64, last_lba, 2u64), (last_lba, 1u64, backup_entries_lba)]
    {
        let mut header = [0u8; SECTOR_BYTES];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        header[12..16].copy_from_slice(&GPT_HEADER_BYTES.to_le_bytes());
        header[24..32].copy_from_slice(&my_lba.to_le_bytes());
        header[32..40].copy_from_slice(&alternate_lba.to_le_bytes());
        header[40..48].copy_from_slice(&(2 + u64::from(entries_sectors)).to_le_bytes());
        header[48..56].copy_from_slice(&(backup_entries_lba - 1).to_le_bytes());
        header[56..72].copy_from_slice(&DISK_GUID);
        header[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        header[80..84].copy_from_slice(&GPT_ENTRIES.to_le_bytes());
        header[84..88].copy_from_slice(&GPT_ENTRY_BYTES.to_le_bytes());
        header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&header[..GPT_HEADER_BYTES as usize]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        let at = my_lba as usize * SECTOR_BYTES;
        bytes[at..at + SECTOR_BYTES].copy_from_slice(&header);
        let at = entries_lba as usize * SECTOR_BYTES;
        bytes[at..at + entries.len()].copy_from_slice(&entries);
    }
}

// ---------------------------------------------------------------------------
// 読み返し（**書いた側とは別の道である**）
// ---------------------------------------------------------------------------

/// FAT32 の分割から読んだもの（内部。**3 つ組を返さないために在る**）。
struct Fat32Contents {
    layout: Fat32Layout,
    label: String,
    files: Vec<(String, Vec<u8>)>,
}

/// 読み返した像。
pub struct ReadBack {
    /// 道（`/` 区切り）と中身。**書いた順ではなく、ディレクトリの項目の順である。**
    pub files: Vec<(String, Vec<u8>)>,
    /// BPB から読み直した幾何。
    pub layout: Fat32Layout,
    /// ボリュームラベル（根の項目から読む）。
    pub label: String,
    /// ESP の位置（GPT から読む）。
    pub esp: (u64, u64),
}

/// 像を読み返す（`ADR-0068` の HW-e）。**書く側の計算を使わずに、像のバイトだけから辿る。**
///
/// # なぜ読み返すのか
///
/// **「書けた」ことは「読める」ことの証明にならない。** **ファームウェアが読むのは像であって、
/// こちらの意図ではない。** **CRC も塊の鎖も、像から読み直して確かめる**——**書く側と読む側で
/// 同じ定数を共有しているが、道は別である**（[`Fat32Layout::for_sectors`] の答えと、BPB に
/// 書かれた数が一致することも見る）。
pub fn read_boot_media(image: &[u8]) -> Result<ReadBack> {
    if !image.len().is_multiple_of(SECTOR_BYTES) {
        bail!(
            "the image is {} byte(s), not a whole number of sectors",
            image.len()
        );
    }
    let sectors = (image.len() / SECTOR_BYTES) as u64;
    if image[510..512] != [0x55, 0xAA] {
        bail!("the protective MBR has no 0xAA55 signature");
    }
    if image[446 + 4] != 0xEE {
        bail!(
            "the protective MBR's partition type is {:#04x}, not 0xEE",
            image[446 + 4]
        );
    }
    let esp = read_gpt(image, sectors, 1)?;
    let backup = read_gpt(image, sectors, sectors - 1)?;
    if esp != backup {
        bail!("the primary and the backup GPT name different partitions");
    }
    let (first_lba, esp_sectors) = esp;
    let start = first_lba as usize * SECTOR_BYTES;
    let end = start + esp_sectors as usize * SECTOR_BYTES;
    if end > image.len() {
        bail!("the ESP runs past the end of the image");
    }
    let volume = &image[start..end];
    let contents = read_fat32(volume)?;
    Ok(ReadBack {
        files: contents.files,
        layout: contents.layout,
        label: contents.label,
        esp,
    })
}

/// GPT のヘッダ 1 つを読み、ESP の位置を返す。**CRC を 2 つとも確かめる。**
fn read_gpt(image: &[u8], sectors: u64, header_lba: u64) -> Result<(u64, u64)> {
    let at = header_lba as usize * SECTOR_BYTES;
    let header = &image[at..at + SECTOR_BYTES];
    if &header[..8] != b"EFI PART" {
        bail!("the GPT header at LBA {header_lba} has no EFI PART signature");
    }
    let header_bytes = u32::from_le_bytes(header[12..16].try_into().unwrap());
    if header_bytes < GPT_HEADER_BYTES || header_bytes as usize > SECTOR_BYTES {
        bail!("the GPT header at LBA {header_lba} claims {header_bytes} byte(s)");
    }
    let mut copy = header[..header_bytes as usize].to_vec();
    let stored = u32::from_le_bytes(copy[16..20].try_into().unwrap());
    copy[16..20].copy_from_slice(&0u32.to_le_bytes());
    let computed = crc32(&copy);
    if stored != computed {
        bail!(
            "the GPT header at LBA {header_lba} has CRC {stored:#010x}, computed {computed:#010x}"
        );
    }
    let my_lba = u64::from_le_bytes(header[24..32].try_into().unwrap());
    if my_lba != header_lba {
        bail!("the GPT header at LBA {header_lba} says it lives at {my_lba}");
    }
    let entries_lba = u64::from_le_bytes(header[72..80].try_into().unwrap());
    let count = u32::from_le_bytes(header[80..84].try_into().unwrap());
    let size = u32::from_le_bytes(header[84..88].try_into().unwrap());
    let entries_crc = u32::from_le_bytes(header[88..92].try_into().unwrap());
    let total = (count * size) as usize;
    let from = entries_lba as usize * SECTOR_BYTES;
    if from + total > image.len() {
        bail!("the partition entries at LBA {entries_lba} run past the end of the image");
    }
    let entries = &image[from..from + total];
    let computed = crc32(entries);
    if entries_crc != computed {
        bail!(
            "the partition entries of the header at LBA {header_lba} have CRC {entries_crc:#010x}, \
             computed {computed:#010x}"
        );
    }
    let mut found = None;
    for index in 0..count as usize {
        let entry = &entries[index * size as usize..(index + 1) * size as usize];
        if entry[..16] == [0u8; 16] {
            continue;
        }
        if entry[..16] != ESP_TYPE_GUID {
            bail!("partition {} is not an ESP", index + 1);
        }
        let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
        let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
        if first == 0 || last < first || last >= sectors {
            bail!(
                "partition {} spans {first}..={last} in a {sectors}-sector image",
                index + 1
            );
        }
        if found.is_some() {
            bail!("the image has more than one partition");
        }
        found = Some((first, last - first + 1));
    }
    match found {
        Some(esp) => Ok(esp),
        None => bail!("the GPT names no partition"),
    }
}

/// FAT32 の分割を読む。
fn read_fat32(volume: &[u8]) -> Result<Fat32Contents> {
    if volume[510..512] != [0x55, 0xAA] {
        bail!("the FAT boot sector has no 0xAA55 signature");
    }
    let bytes_per_sector = u16::from_le_bytes(volume[11..13].try_into().unwrap());
    if bytes_per_sector as usize != SECTOR_BYTES {
        bail!("the BPB says {bytes_per_sector} byte(s) per sector");
    }
    let layout = Fat32Layout {
        total_sectors: u32::from_le_bytes(volume[32..36].try_into().unwrap()),
        reserved_sectors: u32::from(u16::from_le_bytes(volume[14..16].try_into().unwrap())),
        fats: u32::from(volume[16]),
        sectors_per_fat: u32::from_le_bytes(volume[36..40].try_into().unwrap()),
        sectors_per_cluster: u32::from(volume[13]),
        clusters: 0,
    };
    let data =
        layout.total_sectors - layout.reserved_sectors - layout.fats * layout.sectors_per_fat;
    let layout = Fat32Layout {
        clusters: data / layout.sectors_per_cluster,
        ..layout
    };
    // **書く側の計算と一致することを見る**（別の道で同じ答えに着く）。
    let expected = Fat32Layout::for_sectors(layout.total_sectors, layout.sectors_per_cluster)?;
    if expected != layout {
        bail!("the BPB says {layout:?}, but the geometry for that size is {expected:?}");
    }
    if layout.clusters < MIN_FAT32_CLUSTERS || layout.clusters >= MAX_FAT32_CLUSTERS {
        bail!("the volume has {} cluster(s)", layout.clusters);
    }
    if volume[..SECTOR_BYTES]
        != volume[BACKUP_BOOT_SECTOR as usize * SECTOR_BYTES
            ..(BACKUP_BOOT_SECTOR as usize + 1) * SECTOR_BYTES]
    {
        bail!("the backup boot sector differs from the boot sector");
    }
    let root_cluster = u32::from_le_bytes(volume[44..48].try_into().unwrap());
    let fat = read_fat(volume, &layout);
    let mut label = String::new();
    let mut files = Vec::new();
    walk(
        volume,
        &layout,
        &fat,
        root_cluster,
        "",
        &mut label,
        &mut files,
    )?;
    Ok(Fat32Contents {
        layout,
        label,
        files,
    })
}

/// FAT を読む（**1 本目だけ。2 本目が同じであることは別に見る**）。
fn read_fat(volume: &[u8], layout: &Fat32Layout) -> Vec<u32> {
    let at = layout.reserved_sectors as usize * SECTOR_BYTES;
    (0..(layout.clusters + 2) as usize)
        .map(|entry| {
            let offset = at + entry * 4;
            u32::from_le_bytes(volume[offset..offset + 4].try_into().unwrap()) & 0x0FFF_FFFF
        })
        .collect()
}

/// 塊の鎖を辿って中身を集める。
fn read_chain(volume: &[u8], layout: &Fat32Layout, fat: &[u32], first: u32) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut cluster = first;
    let mut steps = 0usize;
    while cluster >= 2 && cluster <= layout.last_cluster() {
        let at = layout.cluster_sector(cluster) as usize * SECTOR_BYTES;
        bytes.extend_from_slice(&volume[at..at + layout.bytes_per_cluster() as usize]);
        cluster = fat[cluster as usize];
        steps += 1;
        if steps > layout.clusters as usize {
            bail!("the cluster chain from {first} does not end");
        }
    }
    if cluster < 0x0FFF_FFF8 {
        bail!("the cluster chain from {first} ends at {cluster:#x}, not at an end-of-chain mark");
    }
    Ok(bytes)
}

/// ディレクトリを辿る。
fn walk(
    volume: &[u8],
    layout: &Fat32Layout,
    fat: &[u32],
    cluster: u32,
    prefix: &str,
    label: &mut String,
    files: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    let bytes = read_chain(volume, layout, fat, cluster)?;
    for entry in bytes.chunks_exact(32) {
        if entry[0] == 0x00 {
            break;
        }
        if entry[0] == 0xE5 {
            continue;
        }
        let attr = entry[11];
        if attr & 0x0F == 0x0F {
            bail!("the image has a long-name entry, which this writer never writes");
        }
        let name: [u8; 11] = entry[..11].try_into().unwrap();
        if attr & 0x08 != 0 {
            *label = long_name(&name, 0).trim_end().to_string();
            continue;
        }
        if &name[..1] == b"." {
            continue;
        }
        let first = u32::from(u16::from_le_bytes(entry[26..28].try_into().unwrap()))
            | (u32::from(u16::from_le_bytes(entry[20..22].try_into().unwrap())) << 16);
        let path = if prefix.is_empty() {
            long_name(&name, entry[12])
        } else {
            format!("{prefix}/{}", long_name(&name, entry[12]))
        };
        if attr & 0x10 != 0 {
            walk(volume, layout, fat, first, &path, label, files)?;
        } else {
            let size = u32::from_le_bytes(entry[28..32].try_into().unwrap()) as usize;
            let mut bytes = read_chain(volume, layout, fat, first)?;
            if bytes.len() < size {
                bail!(
                    "{path} claims {size} byte(s) but the chain holds {}",
                    bytes.len()
                );
            }
            bytes.truncate(size);
            files.push((path, bytes));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拒まれたことと、その文を取り出す。**`unwrap_err` は `Debug` を要求するが、像の型に
    /// `Debug` を持たせると 66MiB を印字できる形になる**——**持たせない。**
    fn refusal<T>(result: Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected a refusal"),
            Err(error) => error.to_string(),
        }
    }

    /// **公表されている値と合うこと**（IEEE 802.3 の CRC32）。**GPT はこの値でヘッダを守る。**
    #[test]
    fn the_crc_matches_the_published_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// **GUID の文字の形とバイトの並びが同じものであること**（前の 3 つの欄は小端である）。
    ///
    /// **外の道具（`sfdisk`）の出力は文字で来るので、突き合わせる側の綴りを間違えると、
    /// 判定が「いつも通る」形になる。**
    #[test]
    fn the_guid_text_matches_the_bytes() {
        let bytes = ESP_TYPE_GUID;
        let mut text = String::new();
        // **前の 3 つの欄は小端で、後の 2 つはそのまま並ぶ。**
        for group in [&bytes[0..4], &bytes[4..6], &bytes[6..8]] {
            for byte in group.iter().rev() {
                text.push_str(&format!("{byte:02X}"));
            }
            text.push('-');
        }
        for byte in &bytes[8..10] {
            text.push_str(&format!("{byte:02X}"));
        }
        text.push('-');
        for byte in &bytes[10..16] {
            text.push_str(&format!("{byte:02X}"));
        }
        assert_eq!(text, ESP_TYPE_GUID_TEXT);
    }

    /// **64MiB の幾何を数で固定する**（`Fat32Layout::for_sectors` の doc に同じ数が在る）。
    #[test]
    fn the_sixty_four_mib_partition_has_the_geometry_we_recorded() {
        let layout = Fat32Layout::for_sectors(ESP_SECTORS as u32, SECTORS_PER_CLUSTER).unwrap();
        assert_eq!(layout.sectors_per_fat, 1_009);
        assert_eq!(layout.clusters, 129_022);
        assert_eq!(layout.first_data_sector(), 32 + 2 * 1_009);
        assert_eq!(layout.cluster_sector(2), layout.first_data_sector());
        assert_eq!(layout.bytes_per_cluster(), 512);
    }

    /// **FAT は塊の全部を指せる大きさであること**（数え直し）。
    #[test]
    fn the_fat_can_hold_an_entry_for_every_cluster() {
        let layout = Fat32Layout::for_sectors(ESP_SECTORS as u32, SECTORS_PER_CLUSTER).unwrap();
        let entries = u64::from(layout.clusters) + 2;
        let holds = u64::from(layout.sectors_per_fat) * (SECTOR_BYTES as u64) / 4;
        assert!(
            holds >= entries,
            "the FAT holds {holds} entries for {entries} cluster(s)"
        );
        // **1 セクタ削ると足りなくなる**（最小の不動点であることの裏返し）。
        let smaller = layout.sectors_per_fat - 1;
        let clusters_then = layout.total_sectors - layout.reserved_sectors - layout.fats * smaller;
        let holds_then = u64::from(smaller) * (SECTOR_BYTES as u64) / 4;
        assert!(
            holds_then < u64::from(clusters_then) + 2,
            "{holds_then} entries would have to cover {clusters_then} cluster(s)"
        );
    }

    /// **1KiB の塊にすると、64MiB では FAT32 の下限を割る**——**拒むこと。**
    #[test]
    fn a_sixty_four_mib_partition_with_one_kib_clusters_is_refused() {
        let error = refusal(Fat32Layout::for_sectors(ESP_SECTORS as u32, 2));
        assert!(
            error.contains("below the 65525"),
            "the refusal should name the FAT32 minimum, got {error}"
        );
    }

    /// **8.3 に収まらない名前を拒む。** **小文字は旗で見せ、読み返しで元へ戻る。**
    #[test]
    fn short_names_carry_the_case_in_a_flag() {
        let (name, flags) = short_name("kernel.elf").unwrap();
        assert_eq!(&name, b"KERNEL  ELF");
        assert_eq!(flags, 0x08 | 0x10);
        assert_eq!(long_name(&name, flags), "kernel.elf");
        let (name, flags) = short_name("EFI").unwrap();
        assert_eq!(&name, b"EFI        ");
        assert_eq!(flags, 0);
        assert_eq!(long_name(&name, flags), "EFI");
        let (name, flags) = short_name("startup.nsh").unwrap();
        assert_eq!(long_name(&name, flags), "startup.nsh");
        assert!(refusal(short_name("Kernel.elf")).contains("mixes upper and lower"));
        assert!(refusal(short_name("verylongname.txt")).contains("does not fit 8.3"));
        assert!(refusal(short_name("name.text")).contains("does not fit 8.3"));
    }

    fn sample() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            (
                "EFI/BOOT/BOOTX64.EFI",
                (0..2_000u32).map(|i| i as u8).collect(),
            ),
            ("zaytos/kernel.elf", vec![0x7f; 5_000]),
            (
                "zaytos/fs.img",
                (0..1_024u32).map(|i| (i * 7) as u8).collect(),
            ),
            ("startup.nsh", b"FS0:\\EFI\\BOOT\\BOOTX64.EFI\r\n".to_vec()),
        ]
    }

    fn build(files: &[(&'static str, Vec<u8>)]) -> BootMedia {
        let files: Vec<MediaFile> = files
            .iter()
            .map(|(path, bytes)| MediaFile { path, bytes })
            .collect();
        build_boot_media(&files).unwrap()
    }

    /// **書いた像を、書く側の計算を使わずに読み返せること。** **道と中身が一致する。**
    #[test]
    fn the_media_reads_back_what_was_written() {
        let files = sample();
        let media = build(&files);
        assert_eq!(media.bytes.len(), IMAGE_SECTORS as usize * SECTOR_BYTES);
        let read = read_boot_media(&media.bytes).unwrap();
        assert_eq!(read.esp, (ESP_FIRST_LBA, ESP_SECTORS));
        assert_eq!(read.label, "ZAYTOS");
        assert_eq!(read.layout, media.layout);
        let mut expected: Vec<(String, Vec<u8>)> = files
            .iter()
            .map(|(path, bytes)| ((*path).to_string(), bytes.clone()))
            .collect();
        expected.sort();
        let mut got = read.files;
        got.sort();
        assert_eq!(got, expected);
    }

    /// **同じ入力から同じバイト列が出ること**（時刻も GUID も固定である）。
    #[test]
    fn the_image_is_deterministic() {
        let files = sample();
        assert_eq!(build(&files).bytes, build(&files).bytes);
    }

    /// **壊れた CRC を読み返しが捕まえること**（GPT のヘッダの 1 バイトを変える）。
    #[test]
    fn a_broken_gpt_crc_is_caught() {
        let mut media = build(&sample()).bytes;
        media[SECTOR_BYTES + 56] ^= 0xff;
        let error = refusal(read_boot_media(&media));
        assert!(error.contains("CRC"), "got {error}");
    }

    /// **壊れた FAT の鎖を読み返しが捕まえること**（終わりの印を消す）。
    #[test]
    fn a_chain_without_an_end_is_caught() {
        let mut media = build(&sample()).bytes;
        let layout = Fat32Layout::for_sectors(ESP_SECTORS as u32, SECTORS_PER_CLUSTER).unwrap();
        // 根（2 番）の項目を、終わりではなく 0 を指す形にする。
        let at = (ESP_FIRST_LBA as usize + layout.reserved_sectors as usize) * SECTOR_BYTES + 2 * 4;
        media[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        let error = refusal(read_boot_media(&media));
        assert!(error.contains("end-of-chain"), "got {error}");
    }

    /// **入らない大きさを拒むこと**（塊が足りない）。
    #[test]
    fn files_that_do_not_fit_are_refused() {
        let big = vec![0u8; 36 * 1024 * 1024];
        let files = [MediaFile {
            path: "big.bin",
            bytes: &big,
        }];
        let error = refusal(build_fat32(70_000, 1, &files));
        assert!(error.contains("more than the"), "got {error}");
    }

    /// **同じ道を 2 度渡されたら拒むこと**（黙って上書きしない）。
    #[test]
    fn the_same_path_twice_is_refused() {
        let bytes = [0u8; 4];
        let files = [
            MediaFile {
                path: "zaytos/fs.img",
                bytes: &bytes,
            },
            MediaFile {
                path: "zaytos/fs.img",
                bytes: &bytes,
            },
        ];
        let error = refusal(build_boot_media(&files));
        assert!(error.contains("listed twice"), "got {error}");
    }
}
