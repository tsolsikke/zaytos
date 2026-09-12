//! COM1 (16550 互換 UART) 経由のシリアルポート出力。
//!
//! ADR-0003: 画面描画より先に確立する、最優先の観測手段。
//! ハードウェアアクセスは [`crate::port`] の `unsafe fn`（`outb` / `inb`）
//! だけに閉じ込め、それ以外は呼び出し側から見て安全な API として公開する
//! （unsafe は最小範囲に限定する）。

use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::port::{inb, outb};

/// UART を書いている間の排他（2026-09-12）。
///
/// # 何を直すのか
///
/// **BSP と AP が同じ COM1 へ同時に書くと、行がバイト単位で混ざる。**
/// **S4-b-4 で実物を観測している**（`docs/verification-coverage.md`。5 回に 1 回）。
/// **当時は「許可された箇所どうしの混線は防げない」と書いて避けた**——**判定を
/// BKL の内側の行へ移した。** **起動ログの参照はそうできない**（全部の行を見る）
/// ので、**B-e で実際に落ちた。**
///
/// # 錠の順序——**シリアルは最内側である**
///
/// **これがこの体制で 3 つ目の排他機構になるので、順序を不変条件として書く。**
///
/// | 錠 | 取る順 | 保持中に取ってよいもの |
/// |---|---|---|
/// | BKL | 外 | シリアル |
/// | [`Locked<T>`](crate::critical::Locked) | 中 | シリアル |
/// | **この錠** | **最内側** | **無い** |
///
/// **シリアルを持ったまま BKL も `Locked<T>` も取らない。** **保持区間は
/// [`SerialPort::write_fmt`] の中だけで、そこから呼ぶのは整形と `outb` しかない。**
/// **逆向き（BKL を持ったままシリアルを取る）は正常である**——**ログは BKL の
/// 内側からも外側からも出る。**
///
/// # 既存の錠が 2 つとも使えない
///
/// **[`Locked<T>`](crate::critical::Locked) は競合で停止する。** **BSP と AP が
/// 同時にログを出すのは正常な動作なので、止めてはならない。**
///
/// **BKL も使えない。** **再帰取得で停止する**のに、**ログは BKL の内側からも
/// 出る**——内側から取れば必ず再帰になる。
///
/// # 取れなければ、混ざるのを承知で書く（fail open）
///
/// **BKL と `Locked<T>` は取れなければ止める。ここは逆である。**
/// **`ADR-0003` が「シリアルは最優先の観測手段」と決めている**——**そこで
/// 止めると、報せる手段そのものが消える。**
/// **混ざるのは今の状態であって、悪化ではない。**
///
/// # 割り込みを禁止しない
///
/// **理由は 3 つ。** **(1) 割り込みハンドラは何も出力しない規約が在る**
/// （`docs/verification-coverage.md` の「割り込みハンドラは何も出力しない」）。
/// **(2) bootloader は UEFI の下で走るので、`cli` を増やしたくない。**
/// **(3) `cli` / `sti` の許可リストを増やさない。**
///
/// **限界**——**その規約は検査されていない**（あの節自身がそう書いている）。
/// **破られたら同一コアの混線は残る。** **そのときは今と同じであって、悪化しない。**
/// **そして、破られたことは数えて出す**（[`reentry_count`]）。
struct SerialLock {
    /// 保持しているコアの番号。誰も持っていなければ [`NO_HOLDER`]。
    holder: AtomicUsize,
    /// 上限を越えて、取らずに書いた回数。**0 でなければ上限か設計を見直す材料。**
    forced: AtomicU64,
    /// 同一コアの再入を検出した回数。**0 でなければ、出力しないはずの経路が
    /// 出力している。**
    reentered: AtomicU64,
}

/// [`SerialLock::holder`] の「誰も持っていない」。**CPU 番号として現れない値。**
const NO_HOLDER: usize = usize::MAX;

/// 待つ上限（TSC のサイクル）。
///
/// **BKL の `WAIT_TIMEOUT_CYCLES`（2 × 10^10）とは桁が違う。** **あちらは
/// デッドロックを疑う尺度で、こちらは「1 行ぶん待つ」尺度である。**
///
/// # 境目を測って決めた（2026-09-12）
///
/// **`--serial-test` の演習（2 コアが 200 行ずつ同時に書く）で、上限を変えながら
/// 「上限を越えて書いた回数」を読んだ。**
///
/// | 上限 | 越えた回数 | 無傷の行 |
/// |---|---|---|
/// | 10^7 | 21 | 343 / 400 |
/// | 2 × 10^7 | 11 | 368 / 400 |
/// | 5 × 10^7 | **0** | **400 / 400** |
/// | 10^8 / 10^9 / 2 × 10^9 | 0 | 400 / 400 |
///
/// **境目は 2 × 10^7 と 5 × 10^7 の間である。** **いちばん小さい「0 になる値」の
/// 10 倍を採った。**
///
/// **最初に置いた 10^7 は、根拠のない見積もりだった**——**「1 行は 10^5
/// サイクルの桁だろう」と書いたが、測っていなかった。** **計器がその場で
/// 嘘を言った**（「21 回越えた」）。**暫定値には根拠を残すという規律の、
/// そのままの例である。**
///
/// # 大きくしすぎない理由
///
/// **保持したまま死んだコアが在れば、他のコアは毎行この上限だけ待つ。**
/// **5 × 10^8 は 2GHz で約 0.25 秒である**——**ログが這うが、止まりはしない。**
const WAIT_TIMEOUT_CYCLES: u64 = 500_000_000;

static UART_LOCK: SerialLock = SerialLock {
    holder: AtomicUsize::new(NO_HOLDER),
    forced: AtomicU64::new(0),
    reentered: AtomicU64::new(0),
};

/// 保持していることを表す。**Drop で手放す。**
struct SerialGuard {
    /// 実際に取れたか。**取れなかった側は手放さない**（他のコアが持っている）。
    owner: bool,
}

impl Drop for SerialGuard {
    fn drop(&mut self) {
        if self.owner {
            UART_LOCK.holder.store(NO_HOLDER, Ordering::Release);
        }
    }
}

/// 錠を取る。**取れなくても返る**（上の doc の fail open）。
fn acquire_uart() -> SerialGuard {
    // **破壊（シリアルの排他の段）**: 取らない。**2 コアが同時に書くと混ざる。**
    if cfg!(feature = "serial-no-lock") {
        return SerialGuard { owner: false };
    }
    let me = lock_identity();
    let started = crate::cpu::read_timestamp_counter();
    loop {
        match UART_LOCK.holder.compare_exchange_weak(
            NO_HOLDER,
            me,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => return SerialGuard { owner: true },
            Err(current) => {
                if current == me {
                    // **同一コアの再入。** **待っても解けない**（解く者が自分である）。
                    UART_LOCK.reentered.fetch_add(1, Ordering::Relaxed);
                    return SerialGuard { owner: false };
                }
                if crate::cpu::read_timestamp_counter().wrapping_sub(started) > WAIT_TIMEOUT_CYCLES
                {
                    UART_LOCK.forced.fetch_add(1, Ordering::Relaxed);
                    return SerialGuard { owner: false };
                }
                core::hint::spin_loop();
            }
        }
    }
}

/// 錠の持ち主を表す値。**`cpuid` の初期 APIC ID を使う。**
///
/// # なぜ [`cpu_id`](crate::percpu::cpu_id) を使わないのか
///
/// **試作で踏んだ（2026-09-12）。** **AP は自分の per-CPU GDT を載せるより前に
/// シリアルへ書く**（`kernel/src/smp.rs` の `zaytos_ap_entry`。**「まだ per-CPU の
/// GDT/TSS/IDT が無いので `cpu_id()` は使わない」と、あちらの行自身が書いている**）。
///
/// **そこで `cpu_id()` を呼ぶと止まる。** **GDTR から枠を導けないとき、あの関数は
/// シリアルへ理由を書いて停止する**——**その書き込みがまた錠を取り、また
/// `cpu_id()` を呼ぶ。** **無限再帰である。** **実測で、AP の最初の 1 行が
/// 出ないまま起動が止まった。**
///
/// **`cpuid` は per-CPU の状態を要らない。** **葉 1 の EBX の上位 8 ビットが
/// 初期 APIC ID で、コアごとに違う。** **MMIO も写像も要らない。**
///
/// **教訓は「最内側の錠は、何にも依存できない」である。**
#[inline]
fn lock_identity() -> usize {
    // **`unsafe` は要らない**——**`__cpuid` は x86_64 では safe fn である**
    // （葉 1 はすべての x86_64 が持つので、機能の照会が要らない）。
    let leaf = core::arch::x86_64::__cpuid(1);
    (leaf.ebx >> 24) as usize
}

/// 上限を越えて、錠を取らずに書いた回数（計器）。**判定にしない**——**揺れる。**
pub fn forced_write_count() -> u64 {
    UART_LOCK.forced.load(Ordering::Relaxed)
}

/// 同一コアの再入を検出した回数（計器）。**判定にしない。**
///
/// **0 でなければ、「割り込みハンドラは何も出力しない」が破られている。**
/// **あの規約は検査されていないので、これが事後の観測になる。**
pub fn reentry_count() -> u64 {
    UART_LOCK.reentered.load(Ordering::Relaxed)
}

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

    /// **1 回の `write!` / `writeln!` 全体を保持区間にする**（[`SerialLock`]）。
    ///
    /// # なぜここで取るのか
    ///
    /// **`write_str` で取ると、1 行が複数回に分かれる。** **実測で、`Logger` の
    /// 1 行は `write_fmt` を 1 回・`write_str` を 5 回呼ぶ**（2026-09-12。
    /// ホストで数えた）。**`write_str` 側で取ると、その 5 つの間に別のコアが
    /// 割り込める。**
    ///
    /// **呼び出し側は 1 つも触らない。** **`SerialPort::new` を書いた箇所は
    /// 25 あるが、どれも `write!` / `writeln!` を通るので、ここだけで覆える。**
    ///
    /// **覆えないものが 2 つある**（実測）——**`write_str` を直に呼ぶ箇所である。**
    /// `kernel/src/task.rs` の `Display` の中の 1 つ（**外側の `write_fmt` が
    /// 覆っているので問題ない**）と、`kernel/src/heap/allocator.rs` が改行を
    /// 1 バイト出す 1 つ（**1 バイトなので、それ自体は裂けない**）。
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        let _guard = acquire_uart();
        // **既定の実装と同じ経路である。** `fmt::write` は `write_str` だけを
        // 呼ぶので、ここから再入することはない（実測で確かめた）。
        fmt::write(self, args)
    }
}
