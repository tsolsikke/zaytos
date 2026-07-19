//! COM1 (16550 互換 UART) 経由のシリアルポート出力。
//!
//! ADR-0003: 画面描画より先に確立する、最優先の観測手段。
//! ここに閉じ込めた 2 つの `unsafe fn`（[`outb`] / [`inb`]）以外は、
//! 呼び出し側から見て安全な API として公開する（unsafe は最小範囲に
//! 限定する）。

use core::fmt;

/// COM1 の I/O ポートベースアドレス。
const COM1_BASE: u16 = 0x3F8;

// レジスタオフセット（ベースポートからの相対位置）。
// 参考: 16550 UART のレジスタマップ（OSDev Wiki "Serial Ports")。
const DATA_OFFSET: u16 = 0; // DLAB=0: 送受信データレジスタ / DLAB=1: 分周比 下位バイト
const INTERRUPT_ENABLE_OFFSET: u16 = 1; // DLAB=0: 割り込み許可 / DLAB=1: 分周比 上位バイト
const FIFO_CONTROL_OFFSET: u16 = 2; // 書き込み: FIFO 制御
const LINE_CONTROL_OFFSET: u16 = 3; // 通信フォーマット・DLAB ビット
const MODEM_CONTROL_OFFSET: u16 = 4;
const LINE_STATUS_OFFSET: u16 = 5;

// レジスタのビットパターン。
const LINE_CONTROL_DLAB: u8 = 0x80; // 分周比設定モードへ切り替え
const LINE_CONTROL_8N1: u8 = 0x03; // データ8bit・パリティなし・ストップビット1
const FIFO_CONTROL_ENABLE_CLEAR_14: u8 = 0xC7; // FIFO有効化・送受信バッファクリア・14byteしきい値
const MODEM_CONTROL_DTR_RTS_OUT2: u8 = 0x0B; // DTR/RTS/OUT2 をアサート（QEMU上の割り込み転送に必要）
const LINE_STATUS_TRANSMIT_EMPTY: u8 = 0x20; // 送信保持レジスタが空＝送信可能

// ボーレート設定。QEMU の仮想UARTは実際のタイミングを強制しないが、
// 実機同様に正しいプロトコルで初期化する。
const UART_CLOCK_HZ: u32 = 115_200;
const BAUD_RATE: u32 = 38_400;
const BAUD_DIVISOR: u16 = (UART_CLOCK_HZ / BAUD_RATE) as u16;

/// 1 バイトをポート `port` へ書き込む。
///
/// # Safety
/// 呼び出し側は `port` が意図した意味を持つ有効な I/O ポートであること、
/// および同一ポートへの並行アクセスが競合しないことを保証しなければならない。
#[inline]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: `out dx, al` は指定ポートへ1バイト出力するだけで、メモリには
    // 触れない。呼び出し元がポート番号の妥当性・排他性を保証する契約。
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// ポート `port` から1バイト読み込む。
///
/// # Safety
/// [`outb`] と同様、`port` の妥当性・排他性は呼び出し側の責任。
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: `in al, dx` は指定ポートから1バイト読み込むだけで、メモリには
    // 触れない。呼び出し元がポート番号の妥当性・排他性を保証する契約。
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// COM1 シリアルポートのドライバ。
///
/// `new` はポート番号を記憶するだけで実機には触れないため安全。実際の
/// ハードウェアアクセスは [`outb`] / [`inb`] に閉じ込め、`init` /
/// `write_byte` はその契約（固定の既知オフセットのみを、決められた
/// 16550 初期化手順どおりに叩く）を自身で満たすことで安全な API として
/// 公開する。
pub struct SerialPort {
    base: u16,
}

impl SerialPort {
    pub const COM1_BASE: u16 = COM1_BASE;

    pub const fn new(base: u16) -> Self {
        Self { base }
    }

    /// 16550 UART の標準的な初期化手順（割り込み無効化 → ボーレート設定
    /// → 通信フォーマット設定 → FIFO 有効化 → モデム制御設定）を実行する。
    pub fn init(&mut self) {
        unsafe {
            outb(self.base + INTERRUPT_ENABLE_OFFSET, 0x00);
            outb(self.base + LINE_CONTROL_OFFSET, LINE_CONTROL_DLAB);
            outb(self.base + DATA_OFFSET, (BAUD_DIVISOR & 0xFF) as u8);
            outb(
                self.base + INTERRUPT_ENABLE_OFFSET,
                (BAUD_DIVISOR >> 8) as u8,
            );
            outb(self.base + LINE_CONTROL_OFFSET, LINE_CONTROL_8N1);
            outb(
                self.base + FIFO_CONTROL_OFFSET,
                FIFO_CONTROL_ENABLE_CLEAR_14,
            );
            outb(self.base + MODEM_CONTROL_OFFSET, MODEM_CONTROL_DTR_RTS_OUT2);
        }
    }

    fn transmit_ready(&self) -> bool {
        let status = unsafe { inb(self.base + LINE_STATUS_OFFSET) };
        status & LINE_STATUS_TRANSMIT_EMPTY != 0
    }

    pub fn write_byte(&mut self, byte: u8) {
        while !self.transmit_ready() {}
        unsafe {
            outb(self.base + DATA_OFFSET, byte);
        }
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // 端末での表示崩れを防ぐため、LF の前に CR を送出する。
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}
