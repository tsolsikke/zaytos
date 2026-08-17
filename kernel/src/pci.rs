//! PCI 構成空間の読み取りと、bus 0 の列挙（S13-a）。
//!
//! # 何をする module か
//!
//! **bus 0 を列挙し、見つけた装置を判定行に出し、virtio-blk を数える。**
//! それだけである。BAR の写像・割り込みの設定・装置の利用はすべて後段で、
//! **ここでは構成空間を読む以外のことをしない**（virtio-blk の BAR0 だけは
//! S13-b が使うので、[`VirtioBlkLocation`] として返す）。
//!
//! # アクセスはポート（`0xCF8` / `0xCFC`）である
//!
//! i440FX（QEMU の既定 machine。ADR-0007）は PCIe 以前の PCI で、
//! **ECAM（MMCONFIG）を持たないのが仕様である。** ただし「無いはず」を
//! 前提にしない——[`crate::acpi`] の走査が MCFG の数を判定行に出しており、
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

use common::log::Logger;
use common::port;
use common::serial::SerialPort;

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
/// **今の QEMU 構成では現れないが、ID の族としては正当なので受ける。**
const VIRTIO_BLK_MODERN: u16 = 0x1041;

/// 1 つの bus に載る device の数（PCI の規定。device 番号は 5 ビット）。
const DEVICES_PER_BUS: u8 = 32;

/// 1 つの device が持ちうる function の数（PCI の規定。function 番号は 3 ビット）。
const FUNCTIONS_PER_DEVICE: u8 = 8;

/// 「不在」を表すベンダ ID。**構成空間が無い場所を読むと全ビット 1 が返る。**
const VENDOR_ABSENT: u16 = 0xFFFF;

/// 見つけた virtio-blk の所在（S13-b で返す形にした）。
///
/// **S13-a では返さなかった**——利用者が居ない機構には検算が置けないためである。
/// **S13-b（virtqueue）が最初の利用者になったので、要る 1 つだけを返す**
/// （IRQ の line / pin は S13-d の話で、要るときに足す）。
pub struct VirtioBlkLocation {
    /// BAR0 の I/O 窓の先頭（下位 2 ビットの種別フラグは落としてある）。
    pub io_base: u16,
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
/// **この前提が崩れたことは、機械が言う**——ブリッジの先に装置が居る構成なら
/// `info pci` は別の bus として列挙し、**こちらの集合が欠けて突き合わせが
/// 落ちる。** ブリッジ用の注記の枝は置かない——**今の構成では一度も走らず、
/// 走らない枝を持つのは「利用者の居ない機構を持たない」に反する**
/// （S13-a で装置を保持しなかったのと同じ判断である。**保持のほうは S13-b で
/// 利用者が来たので、返す形になった**——[`VirtioBlkLocation`]）。
/// **停止性はループの形そのものにある**（最大 32 device
/// かける 8 function の読みで、外部の値に依存しない。S10 の線 4 の族だが、
/// 上限が構造で決まるので打ち切りの機構は要らない）。
pub unsafe fn scan_bus0(logger: &mut Logger<SerialPort>) -> Option<VirtioBlkLocation> {
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

        // 破壊 (S13-a, pci-ignore-multifunction-test): multifunction を見ない。
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
                // **BAR0 が I/O 窓（ビット 0 = 1）のときだけ返す**——legacy で
                // 話す（ADR-0033）ための唯一の入口である。最初の 1 つを採る
                // （2 つ以上は下の判定行の数で見える）。
                if found.is_none() && bars[0] & 0x1 == 1 {
                    found = Some(VirtioBlkLocation {
                        io_base: (bars[0] & !0x3) as u16,
                    });
                }
            }
        }

        // 破壊 (S13-a, pci-stop-at-first-test): 最初に見つけた device で列挙を
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
    // 破壊 (S13-a, pci-config-offset-test): ID の読みを 1 レジスタ（4 バイト）
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
