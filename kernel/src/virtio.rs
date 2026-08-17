//! virtio-blk と legacy interface で話し、sector 0 を 1 回読む（S13-b）。
//!
//! # 話し方は legacy である（ADR-0033）
//!
//! BAR0 の I/O ポート越しにレジスタを読み書きする。MMIO の写像は使わない。
//! feature は何も受けずに交渉する（装置側の bit は読んで判定行に出す）。
//!
//! # 範囲（S13-b）
//!
//! **queue を 1 本立て、ポーリングで sector 0 の 512 バイトを読む。それだけである。**
//! 割り込み（ISR も読まない）・MSI-X・書き込み・複数リクエスト・複数キュー・
//! ext2 への接続は後段で、ここには入れない。
//!
//! # 装置はこちらのメモリを読む側の観測者である
//!
//! リングへの書き込みは、**CPU ではなく装置が読者である。** x86 の TSO は
//! CPU 同士の順序しか約束せず、**本当の危険はコンパイラの再順序化である**——
//! 記述子と avail の公開が notify の後ろへ動いても、**装置が遅ければ間に合って
//! しまい、大抵は動く。** 再現しない形で稀に壊れる（`flaky` へ隔離してある
//! 性質と同じ形）。**したがってリングへは `write_volatile` で書き、公開と
//! notify の間に [`core::sync::atomic::fence`] を置く**（x86 では命令を出さず、
//! コンパイラの再順序化だけを断つ）。
//!
//! **この契約の破壊は立てられない。** 順序を崩しても決定的に落ちる形が
//! 作れない——「崩しても大抵は動く」のが、まさにこの危険の性質である。
//! 立てると「緑だが何も検査していない」項目になる。

use common::addr::direct_map;
use common::log::Logger;
use common::port;
use common::serial::SerialPort;

use crate::frame_allocator::FrameAllocator;
use crate::pci::VirtioBlkLocation;

/// legacy レジスタ: 装置側の feature bits（読み）。
const REG_HOST_FEATURES: u16 = 0x00;
/// legacy レジスタ: こちらが受ける feature bits（書き）。
const REG_GUEST_FEATURES: u16 = 0x04;
/// legacy レジスタ: 選択中 queue のリングの PFN（物理アドレス右シフト 12）。
const REG_QUEUE_ADDRESS: u16 = 0x08;
/// legacy レジスタ: 選択中 queue の大きさ（読み）。
const REG_QUEUE_SIZE: u16 = 0x0C;
/// legacy レジスタ: queue の選択（書き）。
const REG_QUEUE_SELECT: u16 = 0x0E;
/// legacy レジスタ: notify（書き。値は queue 番号）。
const REG_QUEUE_NOTIFY: u16 = 0x10;
/// legacy レジスタ: 装置の状態（読み書き）。
const REG_DEVICE_STATUS: u16 = 0x12;
/// legacy レジスタ: 装置固有領域の先頭。virtio-blk では capacity（512 バイト
/// 単位の数、u64）がここにある。**MSI-X を有効にすると +4 ずれるが、
/// S13-b は MSI-X に触れないのでずれない。**
const REG_DEVICE_CONFIG: u16 = 0x14;

/// 状態ビット: 装置に気づいた。
const STATUS_ACKNOWLEDGE: u8 = 1;
/// 状態ビット: ドライバが居る。
const STATUS_DRIVER: u8 = 2;
/// 状態ビット: 準備が済み、動かしてよい。
const STATUS_DRIVER_OK: u8 = 4;

/// 記述子の flags: 次の記述子へ続く。
const DESC_F_NEXT: u16 = 1;
/// 記述子の flags: 装置が書く側（こちらは読む側）。
const DESC_F_WRITE: u16 = 2;

/// virtio-blk の要求種別: 読み取り。
const BLK_T_IN: u32 = 0;

/// リングの整列（legacy の vring_align）。
const RING_ALIGN: u64 = 4096;

/// ポーリングの上限（スピン回数）。
///
/// **上限のない待機ループを書かない**（`CLAUDE.md` のシェルの規則と同じ理由が
/// カーネル内にも当たる——notify を落とす破壊はここで止まる）。時計は
/// まだ無いので、回数で切る。既定の QEMU では数千回で完了する（実測は
/// 判定行の `spins` に出る）。
const POLL_SPIN_LIMIT: u64 = 20_000_000;

/// sector 0 の読みが止まる理由（S13-b）。文言は呼び出し側（`main.rs`）が作る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtioBlkError {
    /// queue 0 の大きさが 0（装置が queue を提供していない）。
    QueueSizeZero,
    /// リングの物理連続領域が確保できない。
    RingAllocationFailed { pages: u64 },
    /// ポーリングが上限に達した（装置が要求を処理しない）。
    RequestTimedOut { spins: u64 },
    /// 装置が書いた status バイトが OK（0）でない。
    BadRequestStatus { status: u8 },
    /// used エントリの id が出した記述子の先頭と違う。
    WrongUsedId { id: u32 },
}

/// sector 0 を 1 回読み、観測を判定行に出す（S13-b）。
///
/// # Safety
///
/// [`crate::pci::scan_bus0`] と同じ契約である——**BSP だけが走っており
/// （AP 起床前）、割り込みが無効である位置から呼ぶこと。** 加えて:
///
/// - `virtio.io_base` が virtio-blk の BAR0 の I/O 窓であること
///   （呼び出し側は `scan_bus0` の返り値をそのまま渡す）
/// - このポート窓とリングの物理領域を触るのはこの関数だけであること
///   （確保した領域は返さないので、以後も誰も触らない）
pub unsafe fn read_first_sector(
    logger: &mut Logger<SerialPort>,
    virtio: &VirtioBlkLocation,
    allocator: &mut FrameAllocator,
) -> Result<(), VirtioBlkError> {
    let io = virtio.io_base;

    // === 握手（legacy）。reset -> ACKNOWLEDGE -> DRIVER -> queue -> DRIVER_OK ===
    // SAFETY: この関数の契約（doc）どおり、ポート窓は virtio-blk の BAR0 で、
    // 触るのはこの関数だけである（以下のポート I/O すべて同じ）。
    unsafe {
        port::outb(io + REG_DEVICE_STATUS, 0);
        port::outb(io + REG_DEVICE_STATUS, STATUS_ACKNOWLEDGE);
        port::outb(io + REG_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    }
    // SAFETY: 同上。
    let host_features = unsafe { port::inl(io + REG_HOST_FEATURES) };
    // **何も受けない**（ADR-0033）。
    // SAFETY: 同上。
    unsafe { port::outl(io + REG_GUEST_FEATURES, 0) };

    // capacity は装置固有領域の先頭（u64。512 バイト単位の数）。
    // SAFETY: 同上。
    let capacity = unsafe {
        u64::from(port::inl(io + REG_DEVICE_CONFIG))
            | (u64::from(port::inl(io + REG_DEVICE_CONFIG + 4)) << 32)
    };
    // **feature bits は行を分ける。** 実測で QEMU の virtio-blk の feature は
    // コア数で変わる（`-smp 1` と `-smp 2` で 0x1000 違う——キューの数が
    // vCPU 数に従うため）。**この行はコア数依存の標識に入っている**
    // （xtask の `BOOT_LOG_CORE_COUNT_MARKERS`）。capacity は依らないので
    // 下の行に残す。
    logger.info(format_args!(
        "virtio-blk: host features={host_features:#010x} (accepted 0)"
    ));
    logger.info(format_args!(
        "virtio-blk: handshake: ACKNOWLEDGE -> DRIVER; capacity={capacity} sector(s)"
    ));

    // === queue 0 のリングを建てる ===
    // SAFETY: 同上。
    unsafe { port::outw(io + REG_QUEUE_SELECT, 0) };
    // SAFETY: 同上。
    let queue_size = u64::from(unsafe { port::inw(io + REG_QUEUE_SIZE) });
    if queue_size == 0 {
        return Err(VirtioBlkError::QueueSizeZero);
    }

    // legacy の vring のレイアウト（仕様の式そのまま）:
    //   desc 16N / avail 6+2N / （4096 整列の境界）/ used 6+8N
    let desc_bytes = 16 * queue_size;
    let avail_bytes = 6 + 2 * queue_size;
    let used_offset = (desc_bytes + avail_bytes).next_multiple_of(RING_ALIGN);
    let used_bytes = 6 + 8 * queue_size;
    // 末尾の 1 ページを要求の器（ヘッダ 16B・データ 512B・status 1B）に使う。
    let ring_bytes = used_offset + used_bytes;
    let pages = ring_bytes.div_ceil(4096) + 1;

    let Some(ring_phys) = allocator.allocate_contiguous_aligned(pages, 1) else {
        return Err(VirtioBlkError::RingAllocationFailed { pages });
    };
    let ring_virt = direct_map().phys_to_virt(ring_phys);

    // SAFETY: いま確保した `pages` ページは direct map が覆う RAM で、
    // この関数のほかに参照する者は居ない（確保したまま返さない）。
    let ring: &mut [u8] = unsafe {
        core::slice::from_raw_parts_mut(ring_virt.as_u64() as *mut u8, (pages * 4096) as usize)
    };

    // **埋める前の中身を測ってから 0 で埋める。** `allocate_contiguous_aligned`
    // は帳簿だけを動かし、中身には触れない（実測。本体に書き込みが無い）。
    // **装置は used の索引など、こちらが書かない欄も読む**ので、全体を 0 に
    // してから渡す。埋める前の非 0 の数は「ゼロ埋めを飛ばす破壊が効くか」の
    // 判定材料として出す（0 なら、その破壊は族の 1 つ目で立てられない）。
    let nonzero_before = ring.iter().filter(|&&b| b != 0).count();
    ring.fill(0);

    let aligned = ring_phys.as_u64() % RING_ALIGN == 0;
    logger.info(format_args!(
        "virtio-blk: queue 0: size {queue_size}; ring at phys {:#x}..{:#x} ({pages} page(s), \
         4096-aligned={aligned}); zeroed {} byte(s) ({nonzero_before} were nonzero before)",
        ring_phys.as_u64(),
        ring_phys.as_u64() + pages * 4096,
        pages * 4096,
    ));

    // === リングの住所を装置へ告げ、DRIVER_OK にする ===
    //
    // **要求の公開より先である。** 逆にすると、装置は住所を知った時点で
    // 公開済みの要求を見つけて処理してしまい、**notify を落とす破壊が
    // 効かなくなる**（実測で踏んだ。破壊が緑を出す族の「機会が無い」——
    // notify とは別の機序が同じ仕事を済ませていた）。
    //
    // SAFETY: ポート I/O は冒頭と同じ契約。PFN は 4096 整列を上で確かめた値。
    unsafe {
        port::outl(io + REG_QUEUE_ADDRESS, (ring_phys.as_u64() >> 12) as u32);
        port::outb(
            io + REG_DEVICE_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK,
        );
    }

    // === 記述子 3 本の鎖と要求の器を書く ===
    //
    // 器の配置（リングの末尾のページ）:
    //   +0    要求ヘッダ 16B（type / reserved / sector）
    //   +512  データ 512B（装置が書く）
    //   +1024 status 1B（装置が書く）
    let buffer_phys = ring_phys.as_u64() + (pages - 1) * 4096;
    let header_phys = buffer_phys;
    let data_phys = buffer_phys + 512;
    let status_phys = buffer_phys + 1024;

    // 破壊 (S13-b, virtio-wrong-sector-test): sector 1 を要求する。
    // **像の模様は絶対オフセットから決まる**ので、中身の突き合わせが落ちる。
    #[cfg(not(feature = "virtio-wrong-sector-test"))]
    let sector = 0u64;
    #[cfg(feature = "virtio-wrong-sector-test")]
    let sector = 1u64;

    // 破壊 (S13-b, virtio-short-desc-test): データ記述子の長さを 511 にする。
    // **見込みは「装置は黙って 511 バイトだけ書く」だったが、実測では QEMU が
    // 要求ごと拒む**——status に 1（IOERR）が書かれ、status の検査が捕まえる。
    // 中身の突き合わせまで届かない。**捕まえ方の見込みは外れたが、捕まる。**
    #[cfg(not(feature = "virtio-short-desc-test"))]
    let data_len = 512u32;
    #[cfg(feature = "virtio-short-desc-test")]
    let data_len = 511u32;

    let base = ring_virt.as_u64();
    // SAFETY: `base` から `used_offset + used_bytes` までは上で 0 埋めした
    // 自前の領域である。**装置が読者なので `write_volatile` で書く**（module
    // doc の契約。以降のリングへの書き込みすべて同じ）。
    unsafe {
        // 要求ヘッダ。
        core::ptr::write_volatile((base + (pages - 1) * 4096) as *mut u32, BLK_T_IN);
        core::ptr::write_volatile((base + (pages - 1) * 4096 + 8) as *mut u64, sector);
        // desc[0]: ヘッダ（装置が読む）。
        write_desc(base, 0, header_phys, 16, DESC_F_NEXT, 1);
        // desc[1]: データ（装置が書く）。
        write_desc(base, 1, data_phys, data_len, DESC_F_NEXT | DESC_F_WRITE, 2);
        // desc[2]: status（装置が書く）。
        write_desc(base, 2, status_phys, 1, DESC_F_WRITE, 0);
        // avail.ring[0] = 先頭の記述子、avail.idx = 1（公開）。
        core::ptr::write_volatile((base + desc_bytes + 4) as *mut u16, 0);
        core::ptr::write_volatile((base + desc_bytes + 2) as *mut u16, 1);
    }

    // **公開が notify より先に装置から見えること**（module doc の契約）。
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    // 破壊 (S13-b, virtio-skip-notify-test): notify を書かない。
    // **要求は公開されたままで、装置は読まない。** ポーリングが上限に
    // 達して止まる——**上限のある待機だけが、この形を観測へ変える。**
    //
    // SAFETY: ポート I/O は冒頭と同じ契約。
    #[cfg(not(feature = "virtio-skip-notify-test"))]
    unsafe {
        port::outw(io + REG_QUEUE_NOTIFY, 0);
    }

    // === used.idx が進むまでポーリングする。上限つき ===
    let used_idx_at = base + used_offset + 2;
    let mut spins = 0u64;
    loop {
        // SAFETY: used は装置が書く領域で、こちらは読むだけである。
        // **装置が書き手なので `read_volatile` で読む。**
        let used_idx = unsafe { core::ptr::read_volatile(used_idx_at as *const u16) };
        if used_idx != 0 {
            break;
        }
        spins += 1;
        if spins >= POLL_SPIN_LIMIT {
            return Err(VirtioBlkError::RequestTimedOut { spins });
        }
        core::hint::spin_loop();
    }

    // used エントリの中身（id と装置が書いた長さ）。
    // SAFETY: 同上（装置が書いた領域の volatile 読み）。
    let used_id = unsafe { core::ptr::read_volatile((base + used_offset + 4) as *const u32) };
    // SAFETY: 同上。
    let used_len = unsafe { core::ptr::read_volatile((base + used_offset + 8) as *const u32) };
    if used_id != 0 {
        return Err(VirtioBlkError::WrongUsedId { id: used_id });
    }
    // SAFETY: 同上。
    let status =
        unsafe { core::ptr::read_volatile((base + (pages - 1) * 4096 + 1024) as *const u8) };
    if status != 0 {
        return Err(VirtioBlkError::BadRequestStatus { status });
    }

    // === 中身の観測。判定はホスト側（xtask）が像のファイルから同じ計算をする ===
    let data_at = (base + (pages - 1) * 4096 + 512) as *const u8;
    let mut checksum = 0u32;
    let mut first = [0u8; 8];
    for index in 0..512usize {
        // SAFETY: データも装置が書いた領域である。volatile で読む。
        let byte = unsafe { core::ptr::read_volatile(data_at.add(index)) };
        // **位置で重み付けする。** 単純な和だと並べ替えに気づけない。
        checksum = checksum.wrapping_add(u32::from(byte).wrapping_mul(index as u32 + 1));
        if index < first.len() {
            first[index] = byte;
        }
    }
    // **揺れる値（spins）は行を分ける。** 判定行は起動ログの参照が行単位で
    // 突き合わせるので、起動ごとに動く値を載せると参照が壊れる。この行は
    // xtask の正規化の標識に入っている（揺れることが正常な観測である）。
    logger.info(format_args!(
        "virtio-blk: polling took {spins} spin(s) (limit {POLL_SPIN_LIMIT})"
    ));
    logger.info(format_args!(
        "virtio-blk: read sector {sector}: 512 byte(s) requested, used.len={used_len}, \
         status=0 (OK); checksum={checksum:#010x} first bytes={first:02x?}"
    ));
    Ok(())
}

/// 記述子 1 本を書く。
///
/// # Safety
///
/// `base` が 0 埋め済みの自前のリングを指し、`index * 16 + 16` がその中に
/// 収まること。呼び出し側（[`read_first_sector`]）だけが使う。
unsafe fn write_desc(base: u64, index: u64, addr: u64, len: u32, flags: u16, next: u16) {
    let at = base + index * 16;
    // SAFETY: 呼び出し元の契約のとおり自前の領域で、装置が読者なので
    // `write_volatile` で書く。
    unsafe {
        core::ptr::write_volatile(at as *mut u64, addr);
        core::ptr::write_volatile((at + 8) as *mut u32, len);
        core::ptr::write_volatile((at + 12) as *mut u16, flags);
        core::ptr::write_volatile((at + 14) as *mut u16, next);
    }
}
