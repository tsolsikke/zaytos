//! virtio-blk と legacy interface で話す（S13-b、S13-c）。
//!
//! # 話し方は legacy である（ADR-0033）
//!
//! BAR0 の I/O ポート越しにレジスタを読み書きする。MMIO の写像は使わない。
//! feature は何も受けずに交渉する（装置側の bit は読んで判定行に出す）。
//!
//! # 範囲（S13-c まで）
//!
//! **queue を 1 本立て、ポーリングで読む。それだけである。**
//! 要求は逐次 1 つずつ（同時に複数を出さない）。割り込み（ISR も読まない）・
//! MSI-X・書き込み・ext2 のキャッシュ化は後段で、ここには入れない。
//! S13-c は像の全ロード（ADR-0034）のためにこの読みを 4KiB ずつ繰り返す。
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
/// この module は MSI-X に触れないのでずれない。**
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
/// virtio-blk の要求種別: 書き込み（S13-e）。
const BLK_T_OUT: u32 = 1;

/// リングの整列（legacy の vring_align）。
const RING_ALIGN: u64 = 4096;

/// sector の大きさ（virtio-blk の単位）。
pub const SECTOR_BYTES: u64 = 512;

/// ポーリングの上限（スピン回数）。
///
/// **上限のない待機ループを書かない**（`CLAUDE.md` のシェルの規則と同じ理由が
/// カーネル内にも当たる——notify を落とす破壊はここで止まる）。時計は
/// まだ無いので、回数で切る。既定の QEMU では数百スピンまでに完了する（実測は
/// 判定行の `spins` に出る）。**TCG で数秒に収まる大きさにしてある。**
const POLL_SPIN_LIMIT: u64 = 20_000_000;

/// 読みが止まる理由（S13-b）。文言は呼び出し側（`main.rs`）が作る。
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

/// 設定の済んだ virtio-blk（S13-c で保持する形にした）。
///
/// **S13-b では設定と読みが 1 つの関数で、リングは使い捨てだった。**
/// **像の全ロード（ADR-0034）が 2 人目の利用者になったので、設定を分けて
/// 保持する**——`VirtioBlkLocation` を返す形にしたときと同じ進み方である。
pub struct VirtioBlk {
    io_base: u16,
    queue_size: u64,
    /// リングの物理先頭（4096 整列）。
    ring_phys: u64,
    /// リングの仮想先頭（direct map 越し）。
    ring_virt: u64,
    /// desc 表のバイト数（avail はこの直後）。
    desc_bytes: u64,
    /// used リングのリング内オフセット。
    used_offset: u64,
    /// 要求の器（ヘッダ・予備データ・status）を置くページの物理先頭。
    spare_phys: u64,
    /// 完了済みの要求数。**used.idx の期待値である**（u16 で自然に巻く）。
    completed: u16,
    /// これまでの要求で最も長かったポーリング（実測の観測用）。
    max_spins: u64,
    /// 構成空間の Interrupt Line（S13-d。配線と武装が使う）。
    irq_line: u8,
}

/// 握手から queue の設定までを行い、設定の済んだ装置を返す（S13-b）。
///
/// # Safety
///
/// [`crate::pci::scan_bus0`] と同じ契約である——**BSP だけが走っており
/// （AP 起床前）、割り込みが無効である位置から呼ぶこと。** 加えて:
///
/// - `virtio.io_base` が virtio-blk の BAR0 の I/O 窓であること
///   （呼び出し側は `scan_bus0` の返り値をそのまま渡す）
/// - このポート窓とリングの物理領域を触るのは、返した [`VirtioBlk`] だけで
///   あること（複製を作らない）
pub unsafe fn setup(
    logger: &mut Logger<SerialPort>,
    virtio: &VirtioBlkLocation,
    allocator: &mut FrameAllocator,
) -> Result<VirtioBlk, VirtioBlkError> {
    let io = virtio.io_base;

    // === 握手（legacy）。reset -> ACKNOWLEDGE -> DRIVER ===
    // SAFETY: この関数の契約（doc）どおり、ポート窓は virtio-blk の BAR0 で、
    // 触るのはこの module だけである（以下のポート I/O すべて同じ）。
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
    // 末尾の 1 ページを要求の器（ヘッダ 16B・予備データ 512B・status 1B）に使う。
    let ring_bytes = used_offset + used_bytes;
    let pages = ring_bytes.div_ceil(4096) + 1;

    let Some(ring_phys) = allocator.allocate_contiguous_aligned(pages, 1) else {
        return Err(VirtioBlkError::RingAllocationFailed { pages });
    };
    let ring_virt = direct_map().phys_to_virt(ring_phys);

    // SAFETY: いま確保した `pages` ページは direct map が覆う RAM で、
    // 返す [`VirtioBlk`] のほかに参照する者は居ない。
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

    Ok(VirtioBlk {
        io_base: io,
        queue_size,
        ring_phys: ring_phys.as_u64(),
        ring_virt: ring_virt.as_u64(),
        desc_bytes,
        used_offset,
        spare_phys: ring_phys.as_u64() + (pages - 1) * 4096,
        completed: 0,
        max_spins: 0,
        irq_line: virtio.irq_line,
    })
}

impl VirtioBlk {
    /// 構成空間の Interrupt Line（S13-d）。
    pub fn irq_line(&self) -> u8 {
        self.irq_line
    }

    /// 要求の器（末尾ページ）の仮想アドレス。
    fn spare_virt(&self) -> u64 {
        self.ring_virt + (self.spare_phys - self.ring_phys)
    }

    /// `first_sector` から `bytes` バイトを物理 `data_phys` へ読む。
    ///
    /// **要求は 1 つずつで、返ってから次を出す**（同時に複数を出さない。
    /// S13-c の範囲）。`bytes` は 512 の倍数であること。
    ///
    /// # Safety
    ///
    /// [`setup`] と同じ位置の契約に加えて、`data_phys..data_phys+bytes` が
    /// direct map の覆う RAM で、装置が書いてよい（他の誰も同時に読み書き
    /// しない）領域であること。
    pub unsafe fn read_at(
        &mut self,
        first_sector: u64,
        bytes: u32,
        data_phys: u64,
    ) -> Result<(), VirtioBlkError> {
        // SAFETY: 呼び出し元の契約をそのまま `request_at` へ渡す。
        unsafe { self.request_at(first_sector, bytes, data_phys, false) }
    }

    /// `first_sector` から `bytes` バイトを物理 `data_phys` **へ書き戻す**
    /// （S13-e。ADR-0034 の Addendum の全像フラッシュが使う）。
    ///
    /// **[`read_at`](Self::read_at) と対称である**——向きだけが逆で、装置が
    /// `data_phys` から読み、ディスクへ書く。`bytes` は 512 の倍数であること。
    ///
    /// # Safety
    ///
    /// [`read_at`](Self::read_at) と同じ位置の契約に加えて、
    /// `data_phys..data_phys+bytes` が direct map の覆う RAM で、いま他の誰も
    /// 書き換えていない（装置が一貫した内容を読む）こと。
    pub unsafe fn write_at(
        &mut self,
        first_sector: u64,
        bytes: u32,
        data_phys: u64,
    ) -> Result<(), VirtioBlkError> {
        // SAFETY: 呼び出し元の契約をそのまま `request_at` へ渡す。
        unsafe { self.request_at(first_sector, bytes, data_phys, true) }
    }

    /// 読み書き 1 要求を出して完了までポーリングする（S13-b、S13-e）。
    ///
    /// **`to_device` が向きを決める**——`false` は読み（装置が `data_phys` へ
    /// 書く）、`true` は書き（装置が `data_phys` から読む）。記述子の鎖の形は
    /// 同じで、ヘッダの type とデータ記述子の `DESC_F_WRITE` だけが違う。
    ///
    /// # Safety
    ///
    /// [`read_at`](Self::read_at) / [`write_at`](Self::write_at) の契約。
    unsafe fn request_at(
        &mut self,
        first_sector: u64,
        bytes: u32,
        data_phys: u64,
        to_device: bool,
    ) -> Result<(), VirtioBlkError> {
        // SAFETY: 呼び出し元契約をそのまま渡す。
        let expected = unsafe { self.issue_at(first_sector, bytes, data_phys, to_device) };
        // SAFETY: 直前に発行した要求である。
        unsafe { self.wait_for_used(expected) }
    }

    /// 要求を発行し、notify まで行う（P-c）。**完了は待たない。**
    ///
    /// **返すのは、待つべき `used.idx` の値である。**
    ///
    /// # なぜ割るのか
    ///
    /// **シェルの文脈は BKL を保持して入る。** **`ADR-0036` が「BKL を保持した
    /// まま待たない」と決めているので、発行だけを BKL 下で行い、待ちは解いた後に
    /// 行う必要がある。** **割る前は [`request_at`] が中で完了まで回しており、
    /// 発行だけを取り出す口が無かった。**
    ///
    /// **起動シーケンスは [`request_at`] のまま**（発行と待ちを続けて行う）
    /// ——**あちらは BKL を持っていない。**
    ///
    /// # Safety
    ///
    /// [`request_at`] と同じ契約。
    unsafe fn issue_at(
        &mut self,
        first_sector: u64,
        bytes: u32,
        data_phys: u64,
        to_device: bool,
    ) -> u16 {
        let base = self.ring_virt;
        let spare = self.spare_virt();
        let (blk_type, data_flags) = if to_device {
            // 書き: 装置がデータを読む（`DESC_F_WRITE` を立てない）。
            (BLK_T_OUT, DESC_F_NEXT)
        } else {
            // 読み: 装置がデータを書く。
            (BLK_T_IN, DESC_F_NEXT | DESC_F_WRITE)
        };

        // SAFETY: リングと器は [`setup`] が 0 埋めした自前の領域である。
        // **装置が読者なので `write_volatile` で書く**（module doc の契約。
        // 以降のリングへの書き込みすべて同じ）。
        unsafe {
            // 要求ヘッダ（type / reserved / sector）。
            core::ptr::write_volatile(spare as *mut u32, blk_type);
            core::ptr::write_volatile((spare + 8) as *mut u64, first_sector);
            // desc[0]: ヘッダ（装置が読む）。
            self.write_desc(0, self.spare_phys, 16, DESC_F_NEXT, 1);
            // desc[1]: データ（向きは `data_flags` が決める）。
            self.write_desc(1, data_phys, bytes, data_flags, 2);
            // desc[2]: status（装置が書く）。器の +1024 に置く。
            core::ptr::write_volatile((spare + 1024) as *mut u8, 0xFF);
            self.write_desc(2, self.spare_phys + 1024, 1, DESC_F_WRITE, 0);
            // avail.ring[idx % N] = 先頭の記述子、avail.idx += 1（公開）。
            let slot = u64::from(self.completed) % self.queue_size;
            core::ptr::write_volatile((base + self.desc_bytes + 4 + 2 * slot) as *mut u16, 0);
            core::ptr::write_volatile(
                (base + self.desc_bytes + 2) as *mut u16,
                self.completed.wrapping_add(1),
            );
        }

        // **公開が notify より先に装置から見えること**（module doc の契約）。
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        // 破壊 (S13-b, virtio-skip-notify-test): notify を書かない。
        // **要求は公開されたままで、装置は読まない。** ポーリングが上限に
        // 達して止まる——**上限のある待機だけが、この形を観測へ変える。**
        //
        // SAFETY: ポート I/O は [`setup`] と同じ契約。
        #[cfg(not(feature = "virtio-skip-notify-test"))]
        unsafe {
            port::outw(self.io_base + REG_QUEUE_NOTIFY, 0);
        }

        self.completed.wrapping_add(1)
    }

    /// 発行した要求の完了を待つ（P-c）。**上限つきのポーリングである。**
    ///
    /// **既に完了していれば、1 周目で返る。** **眠って待った後に呼ぶと、
    /// たいていそうなる。**
    ///
    /// # Safety
    ///
    /// [`issue_at`] が返した `expected` であること。
    unsafe fn wait_for_used(&mut self, expected: u16) -> Result<(), VirtioBlkError> {
        let base = self.ring_virt;
        let spare = self.spare_virt();
        // === used.idx が進むまでポーリングする。上限つき ===
        let used_idx_at = base + self.used_offset + 2;
        let mut spins = 0u64;
        loop {
            // SAFETY: used は装置が書く領域で、こちらは読むだけである。
            // **装置が書き手なので `read_volatile` で読む。**
            let used_idx = unsafe { core::ptr::read_volatile(used_idx_at as *const u16) };
            if used_idx == expected {
                break;
            }
            spins += 1;
            if spins >= POLL_SPIN_LIMIT {
                return Err(VirtioBlkError::RequestTimedOut { spins });
            }
            core::hint::spin_loop();
        }
        self.max_spins = self.max_spins.max(spins);

        // used エントリの中身（id）。
        let slot = u64::from(self.completed) % self.queue_size;
        // SAFETY: 同上（装置が書いた領域の volatile 読み）。
        let used_id = unsafe {
            core::ptr::read_volatile((base + self.used_offset + 4 + 8 * slot) as *const u32)
        };
        if used_id != 0 {
            return Err(VirtioBlkError::WrongUsedId { id: used_id });
        }
        // SAFETY: 同上。
        let status = unsafe { core::ptr::read_volatile((spare + 1024) as *const u8) };
        if status != 0 {
            return Err(VirtioBlkError::BadRequestStatus { status });
        }
        self.completed = expected;
        Ok(())
    }

    /// 記述子 1 本を書く。
    ///
    /// # Safety
    ///
    /// リングは [`setup`] が建てた自前の領域で、`index * 16 + 16` がその中に
    /// 収まること。
    unsafe fn write_desc(&self, index: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let at = self.ring_virt + index * 16;
        // SAFETY: 呼び出し元の契約のとおり自前の領域で、装置が読者なので
        // `write_volatile` で書く。
        unsafe {
            core::ptr::write_volatile(at as *mut u64, addr);
            core::ptr::write_volatile((at + 8) as *mut u32, len);
            core::ptr::write_volatile((at + 12) as *mut u16, flags);
            core::ptr::write_volatile((at + 14) as *mut u16, next);
        }
    }
}

/// 割り込みで観測する準備が済んだ virtio の所在（S13-d）。
///
/// **IRQ ハンドラ（`idt::irq_entry`）から届く必要があるので static である。**
/// 0 は「まだ武装していない」を表す（I/O ポート 0 は PCI の BAR に現れない）。
static ARMED_ISR_PORT: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);
/// 武装した IRQ 番号（+1 で保持。0 = 未武装）。
static ARMED_IRQ_PLUS_ONE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
/// 自分宛（ISR の bit0 が立っていた）の届いた数。
static IRQ_DELIVERED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// 据えてある装置（P-c-1）。**シェルの文脈から届く唯一の口である。**
///
/// # なぜ静的な家が要るのか
///
/// **`VirtioBlk` は `kernel_main` のローカルだった**（実測。2026-08-28）。
/// **シェルの文脈**（Ring 3 → `int 0x80` → BKL の下）**から届く経路が
/// 1 つも無かった。**
///
/// **形は `console::FOREGROUND` と同じである**——**据えている間だけ生きる
/// ガードが `&mut` を預かり、落ちるときに静的を戻す。** **借用が静的に効く。**
static DEVICE: core::sync::atomic::AtomicPtr<VirtioBlk> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// 装置を占有しているか（P-c-1）。
///
/// # なぜ旗が要るのか。**BKL では足りない**
///
/// **`ADR-0036` は「BKL を解いてから眠る」と決めている。** **解いている間、
/// 他のコアが同じ装置へ入りうる。** **BKL は解いた時点で守りにならない。**
///
/// **旗は待つ間も持ったままにする。** **`Locked<T>` の族と考え方は同じだが、
/// あちらは競合したら待たずに停止する**（fail-fast。`ADR-0004`）。
/// **こちらは断って返す**（`-EBUSY`）——**シェルの文脈なので、止めるより
/// 断るほうが観測できる。**
static DEVICE_IN_USE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// 像の物理の置き場（P-c-1）。**据えるときに控える。**
static IMAGE_PHYS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// 像の長さ。
static IMAGE_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// 書き戻した回数（P-c-1 の計器）。
static FLUSHES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// 書き戻したバイト数。
static FLUSHED_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// 書き戻しに使ったサイクル数。**揺れるので判定には載せない。**
static FLUSH_CYCLES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// 眠った回数（`hlt` を踏んだ数）。
static FLUSH_HALTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// [`DEVICE`] へ据えている間だけ生きるガード（P-c-1）。
pub struct InstalledDevice<'a> {
    /// 据えている装置。**手放すときに静的を戻すために持つ。**
    device: &'a mut VirtioBlk,
}

impl Drop for InstalledDevice<'_> {
    fn drop(&mut self) {
        let _ = &self.device;
        DEVICE.store(core::ptr::null_mut(), core::sync::atomic::Ordering::Release);
    }
}

/// 装置を据える（P-c-1）。**起動シーケンスが 1 度だけ呼ぶ。**
///
/// **像の置き場も一緒に控える**——**書き戻す者が、どこを書けばよいかを
/// 知る必要がある。**
pub fn install(device: &mut VirtioBlk, image_phys: u64, image_bytes: u64) -> InstalledDevice<'_> {
    IMAGE_PHYS.store(image_phys, core::sync::atomic::Ordering::Release);
    IMAGE_BYTES.store(image_bytes, core::sync::atomic::Ordering::Release);
    DEVICE.store(
        device as *mut VirtioBlk,
        core::sync::atomic::Ordering::Release,
    );
    InstalledDevice { device }
}

/// 装置の占有（P-c-1）。**落ちるときに旗を降ろす。**
pub struct DeviceClaim {
    /// 待った回数（`hlt` を踏んだ数）。**計器へ足すために持つ。**
    halts: u64,
}

impl Drop for DeviceClaim {
    fn drop(&mut self) {
        FLUSH_HALTS.fetch_add(self.halts, core::sync::atomic::Ordering::Relaxed);
        DEVICE_IN_USE.store(false, core::sync::atomic::Ordering::Release);
    }
}

/// 装置が据えられているか（P-c-1）。
///
/// # なぜ「占有が取れない」と分ける必要があるのか
///
/// **起動シーケンスの中でもユーザープログラムが走る**（`syscall-test` など）。
/// **あれらは据える前に走り、書きで開いた口を閉じる。** **据えられていない時点で
/// 断ると、起動が止まる**（実測。2026-08-28。`close(3) after writing did not
/// return 0` で `syscall-test` が落ちた）。
///
/// **据えられていないときは書き戻さない。** **起動シーケンスが最後に自分で
/// 書き戻すので、失われるものが無い。**
pub fn installed() -> bool {
    !DEVICE.load(core::sync::atomic::Ordering::Acquire).is_null()
}

/// 装置を占有する（P-c-1）。**取れなければ `None`。**
///
/// **据えられていなければ取れない**——**起動シーケンスが据える前に呼ぶ者は
/// 居ないはずだが、居たら断る。**
pub fn claim() -> Option<DeviceClaim> {
    if DEVICE.load(core::sync::atomic::Ordering::Acquire).is_null() {
        return None;
    }
    if DEVICE_IN_USE
        .compare_exchange(
            false,
            true,
            core::sync::atomic::Ordering::AcqRel,
            core::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        return None;
    }
    Some(DeviceClaim { halts: 0 })
}

impl DeviceClaim {
    /// 像の全体を書き戻す要求を発行する（P-c-1）。**完了は待たない。**
    ///
    /// **1 回の要求で全部を書く。** **装置は 1 回の要求の上限を申告していない**
    /// （`ADR-0033` の Addendum。`VIRTIO_BLK_F_SIZE_MAX` が提示されていない）。
    ///
    /// # Safety
    ///
    /// **BKL を保持して呼ぶこと。** 発行はリングを触るので、他の入口と重ならない
    /// ことが要る（旗は他コアを止めるが、同じコアの再入は BKL が止める）。
    pub unsafe fn issue_image_write(&mut self) -> Option<(u16, u64, u32)> {
        let phys = IMAGE_PHYS.load(core::sync::atomic::Ordering::Acquire);
        let bytes = IMAGE_BYTES.load(core::sync::atomic::Ordering::Acquire);
        if phys == 0 || bytes == 0 || bytes > u64::from(u32::MAX) {
            return None;
        }
        let device = DEVICE.load(core::sync::atomic::Ordering::Acquire);
        if device.is_null() {
            return None;
        }
        // SAFETY: 旗を持っているので、他のコアはここへ入れない。
        // 据えたガードが生きているので、指す先も生きている（[`DEVICE`] の doc）。
        let device = unsafe { &mut *device };
        let before = IRQ_DELIVERED.load(core::sync::atomic::Ordering::Acquire);
        // SAFETY: 像は連続する物理範囲で、装置が読む向きである。
        let expected = unsafe { device.issue_at(0, bytes as u32, phys, true) };
        Some((expected, u64::from(before), bytes as u32))
    }

    /// 完了を眠って待ち、確かめる（P-c-1）。**BKL を解いた後に呼ぶこと。**
    ///
    /// **形は [`exercise_blocking_read`] と同じである**——`cli` 下で検査し、
    /// `sti; hlt` を隣接させて眠る（`ADR-0036` の IF の規律）。
    /// **起きた理由がタイマかもしれないので、上限つきで回す。**
    ///
    /// # Safety
    ///
    /// [`issue_image_write`] が返した値であること。**BKL を保持していないこと。**
    pub unsafe fn wait_for_image_write(
        &mut self,
        expected: u16,
        before: u64,
    ) -> Result<(), VirtioBlkError> {
        let deadline = crate::idt::timer_ticks() + BLOCKING_WAIT_TICKS;
        loop {
            let guard = common::critical::EntryInterruptGuard::enter();
            if u64::from(IRQ_DELIVERED.load(core::sync::atomic::Ordering::Acquire)) > before {
                drop(guard);
                break;
            }
            if crate::idt::timer_ticks() >= deadline {
                drop(guard);
                break;
            }
            core::mem::forget(guard);
            // SAFETY: [`exercise_blocking_read`] と同じ位置の契約である。
            unsafe { common::cpu::enable_interrupts_and_halt() };
            self.halts += 1;
        }
        let device = DEVICE.load(core::sync::atomic::Ordering::Acquire);
        if device.is_null() {
            return Err(VirtioBlkError::QueueSizeZero);
        }
        // SAFETY: 旗を持っている間、指す先はこの占有だけのものである。
        let device = unsafe { &mut *device };
        // SAFETY: 直前に発行した要求である。**眠っている間に完了しているはずだが、
        // 上限で起きた場合もあるので、ここで確かめる。**
        unsafe { device.wait_for_used(expected) }
    }
}

/// 計器を足す（P-c-1）。**書き戻しが 1 回済んだときに呼ぶ。**
pub fn note_flush(bytes: u32, cycles: u64) {
    FLUSHES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    FLUSHED_BYTES.fetch_add(u64::from(bytes), core::sync::atomic::Ordering::Relaxed);
    FLUSH_CYCLES.fetch_add(cycles, core::sync::atomic::Ordering::Relaxed);
}

/// 計器を読んで、0 へ戻す（P-c-1）。**プログラムが終わるたびに読む。**
pub fn take_flush_stats() -> (u64, u64, u64, u64) {
    (
        FLUSHES.swap(0, core::sync::atomic::Ordering::Relaxed),
        FLUSHED_BYTES.swap(0, core::sync::atomic::Ordering::Relaxed),
        FLUSH_CYCLES.swap(0, core::sync::atomic::Ordering::Relaxed),
        FLUSH_HALTS.swap(0, core::sync::atomic::Ordering::Relaxed),
    )
}
/// 自分宛でなかった数。**共有線の仮定（他に鳴る者が居ない）が破れたときに
/// 最初に動く値である**——黙って捨てると、deassert されない線の嵐が
/// 原因の見えない形で出る（ADR-0035 の共有線の代償）。
static IRQ_NOT_MINE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// legacy レジスタ: ISR（読み）。**読むと割り込みが deassert される。**
const REG_ISR: u16 = 0x13;

/// IRQ ハンドラが ISR を読めるように武装する（S13-d）。
///
/// **配線（route）の前に呼ぶこと。** 逆だと、武装前に届いた割り込みを
/// ハンドラが読めず、レベルの線が上がったまま残る。
///
/// # Safety
///
/// [`setup`] と同じ位置の契約（BSP のみ・IF=0）。**ここで ISR を 1 度読んで
/// 捨てる**——S13-b/c の要求は完了のたびに ISR を立てており、読まれずに
/// 溜まっている。読まずに線を開くと、開いた瞬間に過去のぶんが 1 回届き、
/// 「届いた数」の判定が実演と混ざる。
pub unsafe fn arm_interrupt(blk: &VirtioBlk, irq_line: u8) {
    // SAFETY: この関数の契約。ISR の読みは deassert の副作用を意図している。
    let _stale = unsafe { port::inb(blk.io_base + REG_ISR) };
    ARMED_ISR_PORT.store(blk.io_base + REG_ISR, core::sync::atomic::Ordering::Relaxed);
    ARMED_IRQ_PLUS_ONE.store(irq_line + 1, core::sync::atomic::Ordering::Relaxed);
}

/// 武装済みの IRQ 番号（未武装なら `None`）。`idt::irq_entry` の分岐が使う。
pub fn armed_irq() -> Option<u8> {
    match ARMED_IRQ_PLUS_ONE.load(core::sync::atomic::Ordering::Relaxed) {
        0 => None,
        plus_one => Some(plus_one - 1),
    }
}

/// IRQ ハンドラ本体（S13-d）。**ISR を読んで deassert し、数える。**
///
/// ログは出さない（ADR-0018 §5。ハンドラ内の出力はティックを取りこぼす）。
/// 観測はメインループ側が [`exercise_interrupt_read`] でカウンタ越しに行う。
pub fn handle_irq() {
    let isr_port = ARMED_ISR_PORT.load(core::sync::atomic::Ordering::Relaxed);
    if isr_port == 0 {
        return;
    }
    // 破壊 (S13-d, virtio-skip-isr-read-test): ISR を読まない。レベルの線が
    // deassert されず、EOI の後に同じ割り込みが再送され続ける形を狙う。
    #[cfg(feature = "virtio-skip-isr-read-test")]
    {
        IRQ_DELIVERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    #[cfg(not(feature = "virtio-skip-isr-read-test"))]
    {
        // SAFETY: `arm_interrupt` が武装した ISR ポートで、読みは deassert の
        // 副作用を意図している。割り込みゲート経由（IF=0）なので再入しない。
        let isr = unsafe { port::inb(isr_port) };
        if isr & 0x1 != 0 {
            IRQ_DELIVERED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        } else {
            // **自分宛でなかったことを黙って捨てない**（ADR-0035。共有線の
            // 仮定が破れた最初の兆候である）。数は実演の判定行に出る。
            IRQ_NOT_MINE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// 割り込みが実際に届くことを、読み 1 回で実証する（S13-d）。
///
/// **主張は「届いて数えられる」ことだけである。** 完了の待ちはポーリングの
/// まま（眠りは S13-e の利用者と一緒に d-2 で設計する。ADR-0036）。
///
/// # Safety
///
/// - 配線（`route_to_apic`）と武装（[`arm_interrupt`]）が済み、IF=1 であること
/// - [`read_at`](VirtioBlk::read_at) と同じ排他（この struct だけが
///   リングとポート窓を触る。**ISR ポートだけはハンドラと共有し、
///   それは意図した相互作用である**——装置が上げ、ハンドラが読んで下ろす）
pub unsafe fn exercise_interrupt_read(
    logger: &mut Logger<SerialPort>,
    blk: &mut VirtioBlk,
) -> Result<(), VirtioBlkError> {
    // **読みは 2 回である。** 1 回では EOI を落とす破壊が見えない——実測で、
    // 1 発目は届いて数えられ、timer（優先度クラス 15）は生きたままなので
    // 起動も続いてしまう。**2 発目が LAPIC の ISR ビットに塞がれて届かない**
    // ことが、EOI の欠落を観測へ変える（下の待ちが上限で落とす）。
    let data_phys = blk.spare_phys + 512;
    for _round in 0..2u32 {
        let before = IRQ_DELIVERED.load(core::sync::atomic::Ordering::Relaxed);
        // SAFETY: この関数の契約そのまま。読み先は器の中で、装置だけが書く。
        unsafe { blk.read_at(2, 512, data_phys)? };

        // 完了は見えた。**割り込みも届くまで待つ。上限つき**——ポーリングが
        // 先に完了を見る形は正常で、配送はその直後に来る。
        let mut spins = 0u64;
        loop {
            if IRQ_DELIVERED.load(core::sync::atomic::Ordering::Relaxed) > before {
                break;
            }
            spins += 1;
            if spins >= POLL_SPIN_LIMIT {
                return Err(VirtioBlkError::RequestTimedOut { spins });
            }
            core::hint::spin_loop();
        }
    }

    let delivered = IRQ_DELIVERED.load(core::sync::atomic::Ordering::Relaxed);
    let not_mine = IRQ_NOT_MINE.load(core::sync::atomic::Ordering::Relaxed);
    logger.info(format_args!(
        "virtio-blk: interrupt exercise: 2 read(s) completed with interrupts enabled; \
         delivered={delivered} not-mine={not_mine} (wanted 2 and 0)"
    ));
    Ok(())
}

/// 眠って待つ実演が待ちを打ち切るティック数（S13-d-2）。
///
/// **時計が無いのでティックで数える。** 100Hz なので 200 は 2 秒である。
/// `hlt` はタイマ割り込みでも起きるので、完了 IRQ が来なくてもここへ戻って
/// 上限を見られる（起こし忘れの破壊はこの上限で捕まる）。
const BLOCKING_WAIT_TICKS: u64 = 200;

/// **BKL を解いて眠り、割り込みで起きる**ことを実演する（S13-d-2。ADR-0036）。
///
/// d-1 の [`exercise_interrupt_read`] はポーリングで「届いて数えられる」ことを
/// 見た。**こちらは「BKL を保持したまま待たない」を実際に守る最初の利用者**
/// である（`CLAUDE.md` §6）。post-boot の読み 1 回に限る（S13-e の flush へ
/// 広げない）。
///
/// # 取り逃しの窓の閉じ（ADR-0036 の IF の規律）
///
/// 完了フラグの検査を `cli` 下（[`EntryInterruptGuard`]）で行い、未完了なら
/// **`sti; hlt` を隣接させて眠る**（[`common::cpu::enable_interrupts_and_halt`]）。
/// 検査から `hlt` まで IF=0 なので、「検査したら未完了と見てから眠るまでの間に
/// 完了 IRQ が来て取り逃す」窓が開かない。
///
/// # Safety
///
/// [`exercise_interrupt_read`] と同じ位置の契約（配線・武装済み、IF=1、
/// この struct だけがリングとポート窓を触る）。
pub unsafe fn exercise_blocking_read(
    logger: &mut Logger<SerialPort>,
    blk: &mut VirtioBlk,
) -> Result<(), VirtioBlkError> {
    let before = IRQ_DELIVERED.load(core::sync::atomic::Ordering::Acquire);

    // === 要求を発行する。**BKL を保持したまま待たない**（§6。ADR-0036）===
    //
    // read_at は notify まで行う。**発行だけを BKL 下で行い、完了待ちは
    // BKL を解いた後にする。**
    {
        let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);
        // 破壊 (S13-d, virtio-wait-holding-bkl-test): BKL を解かずに待ちへ入る。
        // **§6 違反そのものである。** 保持したまま下の hlt へ進むと、BKL 下は
        // IF=0 なので完了 IRQ が来ず、他コアも永久に待つ。**次に BKL を取る者が
        // 同じコアの再取得として捕まえる**見込み（`kill-fold-keep-bkl` の族）。
        #[cfg(feature = "virtio-wait-holding-bkl-test")]
        {
            // SAFETY: read_at の契約は exercise 側の doc が満たす。
            unsafe { blk.read_at(2, 512, blk.spare_phys + 512)? };
            core::mem::forget(_bkl);
        }
        #[cfg(not(feature = "virtio-wait-holding-bkl-test"))]
        {
            let data_phys = blk.spare_phys + 512;
            // SAFETY: 同上。
            unsafe { blk.read_at(2, 512, data_phys)? };
        }
    } // ここで BKL が解け、IF が発行前の値（=1）へ戻る。

    // === 完了を眠って待つ。上限つき（ティック）===
    let deadline = crate::idt::timer_ticks() + BLOCKING_WAIT_TICKS;
    let mut halts = 0u64;
    loop {
        // cli 下で完了を検査する（取り逃しの窓を閉じる。ADR-0036）。
        let guard = common::critical::EntryInterruptGuard::enter();
        if IRQ_DELIVERED.load(core::sync::atomic::Ordering::Acquire) > before {
            drop(guard);
            break;
        }
        if crate::idt::timer_ticks() >= deadline {
            drop(guard);
            return Err(VirtioBlkError::RequestTimedOut { spins: halts });
        }
        // 破壊 (S13-d, virtio-open-wakeup-window-test): 検査の後・hlt の前で
        // IF を開ける（窓を開く）。**cli 下の検査で見た「未完了」と hlt の間に
        // 完了 IRQ が入ると、その IRQ を処理してから眠り、次の IRQ まで起きない**
        // ——lost wakeup。ただし QEMU で決定的に踏めるかは実測（下の報告）。
        #[cfg(feature = "virtio-open-wakeup-window-test")]
        drop(guard);
        #[cfg(not(feature = "virtio-open-wakeup-window-test"))]
        core::mem::forget(guard);
        // **`sti; hlt` を隣接させる。** guard の cli を sti が上書きし、hlt が
        // 眠る。タイマでも起きるので、完了 IRQ が来なくても上限を見られる。
        //
        // SAFETY: 配線済みで、IF=1 で受けてよいベクタにハンドラが揃っている
        // （sti 前 7 項目は `start_timer` が検証済み）。
        unsafe { common::cpu::enable_interrupts_and_halt() };
        halts += 1;
    }

    // **主張は「BKL を解いてから待った」ことである**（§6。ADR-0036）。
    // **「眠った（hlt）」とは言い切らない**——実測で QEMU の TCG は完了 IRQ を
    // 眠る前に配送し、既定では halt を 1 度も踏まない（d-1 のポーリングが数
    // spin で返るのと同じ速さの限界）。取り逃しの窓を閉じる cli 下の検査は
    // 毎回通る。
    logger.info(format_args!(
        "virtio-blk: blocking read: released the BKL before waiting; the completion was seen"
    ));
    // **halt 数は揺れる観測なので行を分ける**（装置の速さと負荷で変わる。
    // スピン数と同じ扱い）。xtask の正規化の標識に入っている。
    logger.info(format_args!(
        "virtio-blk: blocking wait: {halts} halt(s) before the completion woke it"
    ));
    Ok(())
}

/// superblock の sector を 1 つ読み、観測を判定行に出す（S13-b）。
///
/// **sector 2 を読む**（オフセット 1024。ext2 の superblock で、`s_magic` を
/// 含むので必ず非 0 である）。**S13-b では sector 0 を読んでいたが、S13-c で
/// ディスクの中身が ext2 の像になり、sector 0 は boot 領域の全 0 になった**
/// ——「0 を読んでも、読めていなくても 0」（破壊が緑を出す族の 1 つ目）を
/// 避けるため、必ず非 0 の場所へ移した。判定はホスト側（xtask）が像の
/// ファイルの同じ 512 バイトから同じ計算をする。
///
/// # Safety
///
/// [`setup`] と同じ位置の契約。
pub unsafe fn exercise_read(
    logger: &mut Logger<SerialPort>,
    blk: &mut VirtioBlk,
) -> Result<(), VirtioBlkError> {
    // 破壊 (S13-b, virtio-wrong-sector-test): 隣の sector を要求する。
    // **superblock の前半（非 0）と後半（ほぼ 0）で中身が違う**ので、
    // ホスト側の突き合わせが落ちる。
    #[cfg(not(feature = "virtio-wrong-sector-test"))]
    let sector = 2u64;
    #[cfg(feature = "virtio-wrong-sector-test")]
    let sector = 3u64;

    // 破壊 (S13-b, virtio-short-desc-test): データ記述子の長さを 511 にする。
    // **見込みは「装置は黙って 511 バイトだけ書く」だったが、実測では QEMU が
    // 要求ごと拒む**——status に 1（IOERR）が書かれ、status の検査が捕まえる。
    // 中身の突き合わせまで届かない。**捕まえ方の見込みは外れたが、捕まる。**
    //
    // **正しい長さから 1 を引く形で書く。** **以前は `512` と `511` を別々に
    // 書いていた**——**正しい側を変えると、破壊が「1 バイト短い」でなくなる**
    // （効き目が別の定数に依存する形。2026-08-28 の洗い出しで見つけた）。
    const EXERCISE_READ_BYTES: u32 = 512;
    #[cfg(not(feature = "virtio-short-desc-test"))]
    let bytes = EXERCISE_READ_BYTES;
    #[cfg(feature = "virtio-short-desc-test")]
    let bytes = EXERCISE_READ_BYTES - 1;

    // 器の +512 を読み先に使う（S13-b の使い捨てと同じ場所）。
    let data_phys = blk.spare_phys + 512;
    let before = blk.max_spins;
    // SAFETY: この関数の契約そのまま。読み先は器の中で、装置だけが書く。
    unsafe { blk.read_at(sector, bytes, data_phys)? };

    let data_at = (blk.spare_virt() + 512) as *const u8;
    let mut checksum = 0u32;
    let mut first = [0u8; 8];
    for index in 0..512usize {
        // SAFETY: データは装置が書いた領域である。volatile で読む。
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
        "virtio-blk: polling took {} spin(s) (limit {POLL_SPIN_LIMIT})",
        blk.max_spins - before
    ));
    logger.info(format_args!(
        "virtio-blk: read sector {sector}: 512 byte(s) requested, status=0 (OK); \
         checksum={checksum:#010x} first bytes={first:02x?}"
    ));
    Ok(())
}
