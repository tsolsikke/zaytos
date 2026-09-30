//! PCI 構成空間の読み取りと、bus 0 の列挙（S13-a）。
//!
//! # 何をする module か
//!
//! **bus 0 を列挙し、見つけた装置を判定行に出し、virtio-blk を数える。**
//! それだけである。BAR のマッピング・割り込みの設定・装置の利用はすべて後の段階で、
//! **ここでは構成空間を読む以外のことをしない**（virtio-blk の BAR0 だけは
//! S13-b が使うので、[`VirtioBlkLocation`] として返す）。
//!
//! **BAR0 は、レジスタの窓（[`RegisterWindow`]）にして返す**（2026-09-30）。**どの番地を叩いてよいかの前提は、
//! 窓を作るここに 1 つにまとめてある**——窓の読み書きは、位置が窓の中であることを確かめる。読み出しは安全な関数で、
//! 書き込みは、書く値の前提を呼ぶ側が持つ unsafe fn である。
//!
//! # アクセスはポート（`0xCF8` / `0xCFC`）である
//!
//! i440FX（QEMU の既定 machine。ADR-0007）は PCIe 以前の PCI で、
//! **ECAM（MMCONFIG）を持たないのが仕様である。** ただし「無いはず」を
//! 前提にしない——[`crate::machine::pc::acpi`] の走査が MCFG の数を判定行に出しており、
//! **0 でなかったらこの前提が崩れたことが見える**（そのときは設計を見直す。
//! ポートでも読めるが、判断が生まれるので ADR の対象になる）。
//!
//! # 判定は外の道具が持つ
//!
//! **QEMU のモニタ（`info pci`）は、QEMU 自身が持つ装置の帳簿である。**
//! こちらの読みと独立しているので、xtask が両方を突き合わせる
//! （S12 の `dumpe2fs` と同じ形）。**期待値を定数で持たない**——
//! bus 0 / device 4 のような位置は QEMU の並べ方に依存するので、
//! **カーネルが主張するのは「見つけられた」ことだけで、位置は出すだけである。**

use core::sync::atomic::{AtomicU32, Ordering};

use common::arch::x86_64::port;
use common::log::Logger;
use common::machine::pc::serial::Serial;

/// `CONFIG_ADDRESS`。どの (bus, device, function, offset) を読むかを書く側。
const CONFIG_ADDRESS: u16 = 0xCF8;

/// `CONFIG_DATA`。[`CONFIG_ADDRESS`] が指した場所の中身が読める側。
const CONFIG_DATA: u16 = 0xCFC;

/// virtio のベンダ ID。
const VIRTIO_VENDOR: u16 = 0x1AF4;

/// virtio-blk のデバイス ID（transitional）。**QEMU の既定はこちらである**
/// （実測。`disable-legacy=off` / `disable-modern=false` の構成）。
const VIRTIO_BLK_TRANSITIONAL: u16 = 0x1001;

/// virtio-blk のデバイス ID（modern only。`0x1040 + 1`）。
/// **今の QEMU 構成では現れないが、ID の種類としては正当なので受ける。**
const VIRTIO_BLK_MODERN: u16 = 0x1041;

/// virtio-blk の BAR0 に作る窓の長さ（バイト。2026-09-30）。
///
/// **virtio の仕様が BAR0 に必ず置くと決めている範囲だけを覆う**——legacy の共通のレジスタ（20 バイト。MSI-X を
/// 有効にしない形。この module も `crate::virtio` も有効にしない）と、その後ろの装置固有領域の先頭の capacity
/// （8 バイト。virtio-blk では常に在る）である。BAR の実際の大きさはこれ以上である（QEMU の既定の構成の実測で、
/// virtio-blk の BAR0 が `0xc000` で、次の装置の I/O の BAR が `0xc080` から始まる）。
const VIRTIO_BLK_WINDOW_BYTES: u16 = 20 + 8;

/// 1 つの bus に載る device の数（PCI の規定。device 番号は 5 ビット）。
const DEVICES_PER_BUS: u8 = 32;

/// 1 つの device が持ちうる function の数（PCI の規定。function 番号は 3 ビット）。
const FUNCTIONS_PER_DEVICE: u8 = 8;

/// 「不在」を表すベンダ ID。**構成空間が無い場所を読むと全ビット 1 が返る。**
const VENDOR_ABSENT: u16 = 0xFFFF;

/// 見つけた virtio-blk の所在（S13-b で返す形にした）。
///
/// **S13-a では返さなかった**——利用者が居ない機構には検算が用意できないためである。
/// **S13-b（virtqueue）が最初の利用者になったので、要るものだけを返す。**
/// S13-d で割り込みの配線に `irq_line` が要るようになり、2 つになった。
///
/// # 契約（境界の型。2026-09-30）
///
/// - 作るのは [`scan_bus0`] だけである（BAR0 が I/O の空間を指す、最初の virtio-blk について 1 つ）。
/// - 窓（`registers`）を取り出せるのは、このクレートの中だけである（受け取るのはドライバの `crate::virtio::setup`）。
///   起動の順序（`main.rs`。別のクレート）は、所在をそのまま渡す。
/// - `irq_line` は構成空間の Interrupt Line の値そのものである（割り込みの源への解決は `machine` が行う。
///   [`crate::machine::pc::PciIntx`]）。
pub struct VirtioBlkLocation {
    /// BAR0 のレジスタの窓（2026-09-30 に、I/O ウィンドウの先頭の番地から窓にした）。**取り出せるのは、この
    /// クレートの中だけである**——受け取るのはドライバ（`crate::virtio::setup`）で、起動の順序（`main.rs`。別の
    /// クレート）は所在を渡すだけで、窓には触れない。
    pub(crate) registers: RegisterWindow,
    /// 構成空間の Interrupt Line（S13-d で割り込みの配線に使う。実測で 11）。
    pub irq_line: u8,
}

/// 装置のレジスタの窓（2026-09-30。境界の段階の手順 2）。PC では、PCI の BAR が指す I/O の空間の範囲である。
///
/// # 契約（境界の型。2026-09-30）
///
/// - **窓の中の位置（先頭からのバイト数）で読み書きする。** 読み書きの関数は、位置と幅が窓に収まることを確かめ、
///   収まらなければ止まる（カーネルの誤りである）。**窓の外を指せないので、読み出しの関数は安全な関数である。**
/// - **どの番地を叩いてよいかの前提は、窓を作る所（`from_io_bar`。[`scan_bus0`] が BAR から作る）に 1 つに
///   まとめてある。** 窓を作れるのは、この module だけである。
/// - **複製できない**（`Clone` を持たない）。窓を持つ者が、その装置のレジスタを触る唯一の者である。割り込みの
///   ハンドラが読むレジスタだけは、窓と位置を [`RegisterCell`] に控えて分け合う。
/// - **書き込みの関数は unsafe fn である**（運用者の決定。2026-09-30）。書き込みには、装置にメモリを読み書きさせる
///   ものがある（virtio では、リングの番地を告げる書き込みと、要求を知らせる書き込み）。安全な関数から呼べると、
///   unsafe を書かずに装置へ任意の番地を読み書きさせられるので、その番地と中身が正しいことは呼ぶ側が保証する
///   （根拠は、書くドライバの関数の契約である。`crate::virtio` の `setup` と `read_at`・`write_at`）。
/// - 窓の読み書きは、その前後のメモリの読み書きとの順序を約束しない。装置に見せる順序が要る所は、ドライバが
///   fence を置く（`crate::virtio` の `issue_at`）。
pub struct RegisterWindow {
    /// 窓の先頭（I/O の空間の番地。0 でない）。
    base: u16,
    /// 窓の長さ（バイト。先頭から数えて I/O の空間の中に収まる）。
    len: u16,
}

impl RegisterWindow {
    /// PCI の BAR が指す I/O の空間の番地 `address`（種別のビットは落とした値）から、長さ `len` の窓を作る。
    /// 番地が 0（割り当てられていない）か、窓が I/O の空間（64 KiB）に収まらなければ `None`。
    ///
    /// # Safety
    ///
    /// **どの番地を叩いてよいかの前提は、ここに 1 つにまとめる。**
    ///
    /// - `address` から `len` バイトが、その装置のレジスタであること（BAR が指す範囲の中で、装置の仕様がそこに
    ///   レジスタを置くと決めている範囲）
    /// - その範囲を触るのは、返した窓（と、その窓から控えた [`RegisterCell`]）だけであること。**1 つの範囲に
    ///   窓は 1 つだけ作る**
    unsafe fn from_io_bar(address: u32, len: u16) -> Option<Self> {
        let base = u16::try_from(address).ok().filter(|&base| base != 0)?;
        (u32::from(base) + u32::from(len) <= 0x1_0000).then_some(Self { base, len })
    }

    /// 位置 `offset` から `width` バイトが窓に収まることを確かめる。収まらなければ止まる。
    fn check(&self, offset: u16, width: u16) {
        assert!(
            fits(self.len, offset, width),
            "register window: {width} byte(s) at offset {offset:#x} do not fit in the {}-byte window",
            self.len
        );
    }

    /// 位置 `offset` から `width` バイトの、I/O の空間の番地。窓に収まらなければ止まる。
    fn at(&self, offset: u16, width: u16) -> u16 {
        self.check(offset, width);
        self.base + offset
    }

    /// 位置 `offset` の 1 バイトを読む。**読むと装置の状態が変わるレジスタもある**（virtio の ISR）。
    pub fn read8(&self, offset: u16) -> u8 {
        let at = self.at(offset, 1);
        // SAFETY: `at` は窓の中（`at` が確かめた）で、窓の中は装置のレジスタである（窓を作る所の契約）。
        unsafe { port::inb(at) }
    }

    /// 位置 `offset` の 2 バイトを読む。
    pub fn read16(&self, offset: u16) -> u16 {
        let at = self.at(offset, 2);
        // SAFETY: [`Self::read8`] と同じ。
        unsafe { port::inw(at) }
    }

    /// 位置 `offset` の 4 バイトを読む。
    pub fn read32(&self, offset: u16) -> u32 {
        let at = self.at(offset, 4);
        // SAFETY: [`Self::read8`] と同じ。
        unsafe { port::inl(at) }
    }

    /// 位置 `offset` へ 1 バイトを書く。位置が窓の中であることは、この関数が確かめる（窓の外なら止まる）。
    ///
    /// # Safety
    ///
    /// 書く値が装置にメモリへの読み書きをさせる場合（番地を告げる、要求を知らせる）、その番地と中身が正しいことを
    /// 呼ぶ側が保証する。
    pub unsafe fn write8(&self, offset: u16, value: u8) {
        let at = self.at(offset, 1);
        // SAFETY: `at` は窓の中（`at` が確かめた）で、窓の中は装置のレジスタである（窓を作る所の契約）。書く値が
        // 装置にさせることは、呼ぶ側が保証する（この関数の # Safety）。
        unsafe { port::outb(at, value) }
    }

    /// 位置 `offset` へ 2 バイトを書く。
    ///
    /// # Safety
    ///
    /// [`Self::write8`] と同じ。
    pub unsafe fn write16(&self, offset: u16, value: u16) {
        let at = self.at(offset, 2);
        // SAFETY: [`Self::write8`] と同じ。
        unsafe { port::outw(at, value) }
    }

    /// 位置 `offset` へ 4 バイトを書く。
    ///
    /// # Safety
    ///
    /// [`Self::write8`] と同じ。
    pub unsafe fn write32(&self, offset: u16, value: u32) {
        let at = self.at(offset, 4);
        // SAFETY: [`Self::write8`] と同じ。
        unsafe { port::outl(at, value) }
    }
}

/// 位置 `offset` から `width` バイトが、長さ `len` の窓に収まるか（純粋な論理。ホストで確かめる）。
const fn fits(len: u16, offset: u16, width: u16) -> bool {
    offset < len && width <= len - offset
}

/// レジスタの窓の中の 1 バイトのレジスタを控える所（2026-09-30）。割り込みのハンドラが読むので、`static` に置く。
///
/// **控えるのは窓と位置である**（窓の先頭と、窓の中の位置）。控えるとき（[`RegisterCell::record`]）に位置が
/// 窓の中であることを確かめるので、読む側（[`RegisterCell::read8`]）は確かめずに読める。
///
/// # 契約（境界の型。2026-09-30）
///
/// - **BKL なしで読める。** 割り込みのハンドラは、BKL を持たずに読む。窓と位置は 1 つの原子的な語に入っており、
///   読む側が見るのは「空」（控える前）か「控えた窓と位置」のどちらかで、途中の値は見えない。**どちらでも安全で
///   ある**——空なら読まずに `None` を返し、控えた値なら窓の中を読む。したがって読む側はロックを取らず、順序も
///   Relaxed で足りる。
/// - 控えるのは、その割り込みの源を許可する前の 1 回である（装置のドライバの武装。`crate::virtio::arm_interrupt`）。
///   控え直しても、読む側が見るのは古い値か新しい値のどちらかで、どちらも窓の中である。
/// - **読むと装置の状態が変わりうる**（virtio の ISR は、読むと割り込みの線を下ろす）。ハンドラと窓の持ち主の
///   両方が同じレジスタを読むのは、意図した相互作用である（装置が上げ、ハンドラが読んで下ろす）。
pub struct RegisterCell(AtomicU32);

impl RegisterCell {
    /// 空の控え（まだ何も控えていない）。
    pub const fn empty() -> Self {
        Self(AtomicU32::new(0))
    }

    /// 窓 `window` の位置 `offset` の 1 バイトのレジスタを控える。位置が窓に収まらなければ止まる。
    pub fn record(&self, window: &RegisterWindow, offset: u16) {
        window.check(offset, 1);
        // 窓の先頭は 0 でない（窓を作る所が確かめる）ので、控えた値は空（0）と区別できる。
        self.0.store(
            (u32::from(window.base) << 16) | u32::from(offset),
            Ordering::Relaxed,
        );
    }

    /// 控えてあるか（読まずに確かめる）。
    pub fn is_recorded(&self) -> bool {
        self.0.load(Ordering::Relaxed) != 0
    }

    /// 控えたレジスタを 1 バイト読む。控えていなければ読まずに `None` を返す。
    pub fn read8(&self) -> Option<u8> {
        let recorded = self.0.load(Ordering::Relaxed);
        if recorded == 0 {
            return None;
        }
        let at = (recorded >> 16) as u16 + (recorded & 0xFFFF) as u16;
        // SAFETY: 控えた値を書くのは `record` だけで、`record` は窓（窓を作る所の契約で、中は装置のレジスタ）と、
        // 窓の中と確かめた位置から作る。したがって `at` は窓の中である。
        Some(unsafe { port::inb(at) })
    }
}

/// 構成空間の 1 dword を読む。
///
/// # Safety
///
/// - **BSP だけが走っており（AP 起床前）、割り込みが無効であること。**
///   `CONFIG_ADDRESS` への書き込みと `CONFIG_DATA` の読みは対で 1 つの操作で、
///   **間に別の書き込みが挟まると読む場所がすり替わる。** この契約が
///   同一コアの再入（割り込み・例外）と他コアの並行の両方を断つ。
/// - **このポート対を触るのはこの module だけであること**（作成時に grep で
///   0 件を確認済み。新しい利用者を作るなら、排他をここへ寄せること）。
unsafe fn config_read(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let address: u32 = 0x8000_0000
        | (u32::from(bus) << 16)
        | (u32::from(device) << 11)
        | (u32::from(function) << 8)
        | u32::from(offset & 0xFC);
    // SAFETY: 呼び出し元の契約（この関数の doc）で、対の間に他の書き込みが
    // 挟まらないこと、ポートの意味が PCI ホストブリッジの規定どおりで
    // あることが保証される。
    unsafe {
        port::outl(CONFIG_ADDRESS, address);
        port::inl(CONFIG_DATA)
    }
}

/// bus 0 を列挙し、見つけた function を判定行に出す（S13-a）。
///
/// # Safety
///
/// [`config_read`] の契約そのままである——**BSP だけが走っており（AP 起床前）、
/// 割り込みが無効である位置から呼ぶこと。** 呼び出し位置が契約である
/// （`kernel_main` の ACPI 走査の直後。AP の起床と `sti` はどちらも後段にある）。
///
/// # bus 0 だけを見る
///
/// 実測で全装置が bus 0 に居る（QEMU の i440FX は単一ホストブリッジ）。
/// **この前提が崩れたことは、機械が示す**——ブリッジの先に装置が居る構成なら
/// `info pci` は別の bus として列挙し、**こちらの集合が欠けて突き合わせが
/// 落ちる。** ブリッジ用の注記の枝は置かない——**今の構成では一度も走らず、
/// 走らない枝を持つのは「利用者の居ない機構を持たない」に反する**
/// （S13-a で装置を保持しなかったのと同じ判断である。**保持のほうは S13-b で
/// 利用者が来たので、返す形になった**——[`VirtioBlkLocation`]）。
/// **停止性はループの形そのものにある**（最大 32 device
/// かける 8 function の読みで、外部の値に依存しない。S10 の線 4 と同じ種類だが、
/// 上限が構造で決まるので打ち切りの機構は要らない）。
pub unsafe fn scan_bus0(logger: &mut Logger<Serial>) -> Option<VirtioBlkLocation> {
    let mut functions = 0u32;
    let mut virtio_blk = 0u32;
    let mut found: Option<VirtioBlkLocation> = None;

    for device in 0..DEVICES_PER_BUS {
        // SAFETY: この関数の契約をそのまま引き継ぐ。
        let id = unsafe { read_id(0, device, 0) };
        if (id & 0xFFFF) as u16 == VENDOR_ABSENT {
            continue;
        }

        // header type のビット 7 が multifunction。**function 0 で読む。**
        // SAFETY: 同上。
        let header = unsafe { config_read(0, device, 0, 0x0C) };
        let multifunction = header & 0x0080_0000 != 0;

        // 破壊テスト (S13-a, pci-ignore-multifunction-test): multifunction を見ない。
        // **i440FX では device 1 の function 1（IDE）と 3（bridge）が消える**
        // ので、`info pci` との集合の突き合わせが落ちる（実測が保証する）。
        #[cfg(feature = "pci-ignore-multifunction-test")]
        let multifunction = false;

        let last_function = if multifunction {
            FUNCTIONS_PER_DEVICE
        } else {
            1
        };
        for function in 0..last_function {
            // SAFETY: 同上。
            let id = unsafe { read_id(0, device, function) };
            let vendor = (id & 0xFFFF) as u16;
            if vendor == VENDOR_ABSENT {
                continue;
            }
            let device_id = (id >> 16) as u16;
            functions += 1;

            // SAFETY: 同上。
            let (class, header_type, irq, bars) = unsafe { read_details(0, device, function) };
            // **BAR は 1 行に並べる。** `{:#x?}` の配列は複数行に割れて、
            // 起動ログの参照との突き合わせが読みにくくなる（`verify_path_lookup`
            // が改行を判定行に出さないのと同じ理由）。
            logger.info(format_args!(
                "pci: bus 0 device {device} function {function}: {vendor:04x}:{device_id:04x} \
                 class={:#04x} subclass={:#04x} header={:#04x} irq line={} pin={} \
                 bars=[{:#x} {:#x} {:#x} {:#x} {:#x} {:#x}]",
                (class >> 24) as u8,
                (class >> 16) as u8,
                header_type,
                (irq & 0xFF) as u8,
                ((irq >> 8) & 0xFF) as u8,
                bars[0],
                bars[1],
                bars[2],
                bars[3],
                bars[4],
                bars[5],
            ));

            if vendor == VIRTIO_VENDOR
                && (device_id == VIRTIO_BLK_TRANSITIONAL || device_id == VIRTIO_BLK_MODERN)
            {
                virtio_blk += 1;
                // **BAR0 が I/O ウィンドウ（ビット 0 = 1）のときだけ返す**——legacy で
                // 話す（ADR-0033）ための唯一の入口である。最初の 1 つを採る
                // （2 つ以上は下の判定行の数で見える）。
                if found.is_none() && bars[0] & 0x1 == 1 {
                    // SAFETY: BAR0 のビット 0 が 1 なので、BAR0 は I/O の空間の範囲を指す。transitional の
                    // virtio-blk は legacy の口を BAR0 の I/O の空間に出すのが仕様で、その先頭の
                    // `VIRTIO_BLK_WINDOW_BYTES` バイトは legacy の共通のレジスタと capacity である。窓はここで
                    // 1 つだけ作り（最初の 1 つだけを採る）、返す所在が持つ。
                    let registers = unsafe {
                        RegisterWindow::from_io_bar(bars[0] & !0x3, VIRTIO_BLK_WINDOW_BYTES)
                    };
                    if let Some(registers) = registers {
                        found = Some(VirtioBlkLocation {
                            registers,
                            irq_line: (irq & 0xFF) as u8,
                        });
                    }
                }
            }
        }

        // 破壊テスト (S13-a, pci-stop-at-first-test): 最初に見つけた device で列挙を
        // やめる。**集合が host bridge の 1 つに痩せる**ので、突き合わせが落ちる。
        #[cfg(feature = "pci-stop-at-first-test")]
        if functions > 0 {
            break;
        }
    }

    logger.info(format_args!(
        "pci: enumeration complete: {functions} function(s) on bus 0, virtio-blk \
         (vendor {VIRTIO_VENDOR:#06x} device {VIRTIO_BLK_TRANSITIONAL:#06x} or \
         {VIRTIO_BLK_MODERN:#06x}) found {virtio_blk} time(s)"
    ));
    found
}

/// ベンダとデバイス ID の dword を読む。
///
/// # Safety
///
/// [`config_read`] の契約そのまま。
unsafe fn read_id(bus: u8, device: u8, function: u8) -> u32 {
    // 破壊テスト (S13-a, pci-config-offset-test): ID の読みを 1 レジスタ（4 バイト）
    // ずらす。**command/status が ID として読まれ、全装置の ID が壊れる**ので、
    // `info pci` との突き合わせが落ちる。
    #[cfg(not(feature = "pci-config-offset-test"))]
    let offset = 0x00;
    #[cfg(feature = "pci-config-offset-test")]
    let offset = 0x04;
    // SAFETY: 呼び出し元の契約をそのまま引き継ぐ。
    unsafe { config_read(bus, device, function, offset) }
}

/// class / header type / IRQ / BAR をまとめて読む。
///
/// # Safety
///
/// [`config_read`] の契約そのまま。
unsafe fn read_details(bus: u8, device: u8, function: u8) -> (u32, u8, u32, [u32; 6]) {
    // SAFETY: 呼び出し元の契約をそのまま引き継ぐ（以下同じ）。
    let class = unsafe { config_read(bus, device, function, 0x08) };
    // SAFETY: 同上。
    let header_type = (unsafe { config_read(bus, device, function, 0x0C) } >> 16) as u8;
    // SAFETY: 同上。
    let irq = unsafe { config_read(bus, device, function, 0x3C) };
    let mut bars = [0u32; 6];
    for (index, bar) in bars.iter_mut().enumerate() {
        // SAFETY: 同上。
        *bar = unsafe { config_read(bus, device, function, 0x10 + 4 * index as u8) };
    }
    (class, header_type, irq, bars)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 窓の読み書きは、位置と幅が窓に収まるときだけ通す（2026-09-30）。virtio-blk の窓では、使うレジスタの端
    /// （capacity の上位の 4 バイト）と ISR は収まり、その 1 バイト先と窓の外は収まらない。
    #[test]
    fn a_register_window_admits_only_positions_inside_it() {
        assert!(fits(VIRTIO_BLK_WINDOW_BYTES, 0x18, 4));
        assert!(fits(VIRTIO_BLK_WINDOW_BYTES, 0x13, 1));
        assert!(!fits(VIRTIO_BLK_WINDOW_BYTES, 0x19, 4));
        assert!(!fits(VIRTIO_BLK_WINDOW_BYTES, VIRTIO_BLK_WINDOW_BYTES, 1));
        assert!(!fits(VIRTIO_BLK_WINDOW_BYTES, u16::MAX, 2));
    }

    /// 窓は、割り当てられていない番地 0 と、I/O の空間（64 KiB）からはみ出す範囲からは作らない（2026-09-30）。
    #[test]
    fn a_register_window_is_made_only_inside_the_io_space() {
        // SAFETY: 作るだけで、読み書きしない（ホストの検査）。
        unsafe {
            assert!(RegisterWindow::from_io_bar(0xc000, 28).is_some());
            assert!(RegisterWindow::from_io_bar(0x1_0000 - 28, 28).is_some());
            assert!(RegisterWindow::from_io_bar(0, 28).is_none());
            assert!(RegisterWindow::from_io_bar(0xFFF0, 28).is_none());
            assert!(RegisterWindow::from_io_bar(0x1_0000, 28).is_none());
        }
    }
}
