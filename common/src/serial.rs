//! COM1 (16550 互換 UART) 経由のシリアルポート出力。
//!
//! ADR-0003: 画面描画より先に確立する、最優先の観測手段。
//! ハードウェアアクセスは [`crate::port`] の `unsafe fn`（`outb` / `inb`）
//! だけに閉じ込め、それ以外は呼び出し側から見て安全な API として公開する
//! （unsafe は最小範囲に限定する）。

use core::fmt;

use crate::port::{inb, outb};

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

/// COM1 シリアルポートのドライバ。
///
/// `new` はポート番号を記憶するだけで実機には触れないため安全。実際の
/// ハードウェアアクセスは [`crate::port`] の `outb` / `inb` に閉じ込め、
/// `init` / `write_byte` はその契約（固定の既知オフセットのみを、決められた
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
        // SAFETY: 触れるのは `self.base` を起点とする 16550 の既知のレジスタ
        // だけで、オフセットはいずれもこのモジュール内の定数である。ZaytOS は
        // COM1（0x3F8）を自分のログ出力にのみ使い、他の誰もこのポートを
        // 触らない。ポート I/O はメモリを参照しないため、Rust の値や
        // 借用の不変条件を壊さない。書く値も 16550 の初期化手順どおりで、
        // 未定義の副作用を持つビットは立てていない。
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
        // SAFETY: `init` と同じ理由。読むのは COM1 のライン状態レジスタだけで、
        // 読み取りに副作用は無い。
        let status = unsafe { inb(self.base + LINE_STATUS_OFFSET) };
        status & LINE_STATUS_TRANSMIT_EMPTY != 0
    }

    pub fn write_byte(&mut self, byte: u8) {
        while !self.transmit_ready() {}
        // SAFETY: `init` と同じ理由。書くのは COM1 のデータレジスタだけで、
        // 直前に送信保持レジスタが空であることを確認している。
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
