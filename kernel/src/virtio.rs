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
        let base = self.ring_virt;
        let spare = self.spare_virt();

        // SAFETY: リングと器は [`setup`] が 0 埋めした自前の領域である。
        // **装置が読者なので `write_volatile` で書く**（module doc の契約。
        // 以降のリングへの書き込みすべて同じ）。
        unsafe {
            // 要求ヘッダ（type / reserved / sector）。
            core::ptr::write_volatile(spare as *mut u32, BLK_T_IN);
            core::ptr::write_volatile((spare + 8) as *mut u64, first_sector);
            // desc[0]: ヘッダ（装置が読む）。
            self.write_desc(0, self.spare_phys, 16, DESC_F_NEXT, 1);
            // desc[1]: データ（装置が書く）。
            self.write_desc(1, data_phys, bytes, DESC_F_NEXT | DESC_F_WRITE, 2);
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

        // === used.idx が進むまでポーリングする。上限つき ===
        let expected = self.completed.wrapping_add(1);
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
    #[cfg(not(feature = "virtio-short-desc-test"))]
    let bytes = 512u32;
    #[cfg(feature = "virtio-short-desc-test")]
    let bytes = 511u32;

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
