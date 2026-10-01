//! COM1 (16550 互換 UART) 経由のシリアルポート出力。
//!
//! ADR-0003: 画面描画より先に確立する、最優先の観測手段。
//! ハードウェアアクセスは [`crate::arch::x86_64::port`] の `unsafe fn`（`outb` / `inb`）
//! だけに閉じ込め、それ以外は呼び出し側から見て安全な API として公開する
//! （unsafe は最小範囲に限定する）。

use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::arch::x86_64::port::{inb, outb};

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
/// # ロックの順序——**シリアルは最内側である**
///
/// **これがこの体制で 3 つ目の排他機構になるので、順序を不変条件として書く。**
///
/// | ロック | 取る順 | 保持中に取ってよいもの |
/// |---|---|---|
/// | BKL | 外 | シリアル |
/// | [`Locked<T>`](crate::critical::Locked) | 中 | シリアル |
/// | **このロック** | **最内側** | **無い** |
///
/// **シリアルを持ったまま BKL も `Locked<T>` も取らない。** **保持区間は
/// [`Serial::write_fmt`] の中だけで、そこから呼ぶのは整形と `outb` しかない。**
/// **逆向き（BKL を持ったままシリアルを取る）は正常である**——**ログは BKL の
/// 内側からも外側からも出る。**
///
/// # 既存のロックが 2 つとも使えない
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
/// **そして、破られたことは数えて出す**（[`serial_reentry_count`]）。
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
/// サイクルの桁だろう」と書いたが、測っていなかった。** **計測がその場で
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

/// ロックを取る。**取れなくても返る**（上の doc の fail open）。
fn acquire_uart() -> SerialGuard {
    // **破壊テスト（シリアルの排他の段）**: 取らない。**2 コアが同時に書くと混ざる。**
    if cfg!(feature = "serial-no-lock") {
        return SerialGuard { owner: false };
    }
    let me = lock_identity();
    let started = crate::arch::x86_64::cpu::read_timestamp_counter();
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
                if crate::arch::x86_64::cpu::read_timestamp_counter().wrapping_sub(started)
                    > WAIT_TIMEOUT_CYCLES
                {
                    UART_LOCK.forced.fetch_add(1, Ordering::Relaxed);
                    return SerialGuard { owner: false };
                }
                core::hint::spin_loop();
            }
        }
    }
}

/// ロックの持ち主を表す値。**`cpuid` の初期 APIC ID を使う。**
///
/// # なぜ [`cpu_id`](crate::percpu::cpu_id) を使わないのか
///
/// **試作で踏んだ（2026-09-12）。** **AP は自分の per-CPU GDT を載せるより前に
/// シリアルへ書く**（`kernel/src/smp.rs` の `zeikos_ap_entry`。**「まだ per-CPU の
/// GDT/TSS/IDT が無いので `cpu_id()` は使わない」と、あちらの行自身が書いている**）。
///
/// **そこで `cpu_id()` を呼ぶと止まる。** **GDTR からスロットを導けないとき、あの関数は
/// シリアルへ理由を書いて停止する**——**その書き込みがまたロックを取り、また
/// `cpu_id()` を呼ぶ。** **無限再帰である。** **実測で、AP の最初の 1 行が
/// 出ないまま起動が止まった。**
///
/// **`cpuid` は per-CPU の状態を要らない。** **葉 1 の EBX の上位 8 ビットが
/// 初期 APIC ID で、コアごとに違う。** **MMIO もマッピングも要らない。**
///
/// **教訓は「最内側のロックは、何にも依存できない」である。**
#[inline]
fn lock_identity() -> usize {
    // **`unsafe` は要らない**——**`__cpuid` は x86_64 では safe fn である**
    // （葉 1 はすべての x86_64 が持つので、機能の照会が要らない）。
    let leaf = core::arch::x86_64::__cpuid(1);
    (leaf.ebx >> 24) as usize
}

/// 上限を越えて、ロックを取らずに書いた回数（計測）。**判定にしない**——**揺れる。**
///
/// # 契約（境界の関数。2026-09-30）
///
/// - 上限を越えて、最内側のロックを取らずに書いた回数を読むだけで、何も変えない（計測で、判定にしない）。
pub fn serial_forced_write_count() -> u64 {
    UART_LOCK.forced.load(Ordering::Relaxed)
}

/// 同一コアの再入を検出した回数（計測）。**判定にしない。**
///
/// **0 でなければ、「割り込みハンドラは何も出力しない」が破られている。**
/// **あの規約は検査されていないので、これが事後の観測になる。**
///
/// # 契約（境界の関数。2026-09-30）
///
/// - 割り込みハンドラの中からシリアルへ書いた回数を読むだけで、何も変えない。
pub fn serial_reentry_count() -> u64 {
    UART_LOCK.reentered.load(Ordering::Relaxed)
}

/// UART の設定（[`Serial::init`] が書く 7 つのレジスタ）を、起動の 1 回だけ書くための印（2026-10-01）。
///
/// # 何を直すのか
///
/// **以前は、ポートを作る所がどこも、作るたびに設定を書き直していた**（30 か所。端末へ書くシステムコールや
/// プログラムの起動のような、ふつうの経路を含む。実測で、起動から `init` を起こす手前までに 69 回書いていた）。
/// **設定は DLAB を立てて分周を書き、送信の FIFO を空にする。**
/// **その間は最内側のロックを取らないので、ほかの CPU が書いている途中に重なると、文字が欠ける。**
/// 実測で、片方の CPU が 1 行ごとに開き直す形にすると、2 つの CPU が同時に書いた 400 行のうち
/// 無傷は 360〜398 行だった（QEMU の `-smp 2`。30 回）。設定を 1 回だけにすると 400 行とも無傷になる（10 回）。
///
/// # 印は 3 つの状態を持つ
///
/// **まだ → 設定中 → 済み** の順に 1 度だけ進む。**最初に呼んだ所が設定を書く**——どの入口から来ても同じである
/// （`kernel_main` の入口の確かめや、早いパニックは、起動の初めの `init` より前に書きうる）。
///
/// **設定中に来た呼び出しは、済むまで少しだけ待つ**（[`SETUP_WAIT_SPINS`]）。**待たずに書くと、DLAB が立っている
/// 間に書くことになり、直そうとしている形を自分で作る。** **上限に着いたら、待つのをやめて出力を書く**
/// （設定は書かない）——設定中の CPU が止まっていても、パニックの報告は止めない（`ADR-0003`）。
/// 待つのをやめた回数は数えて出す（[`serial_setup_gave_up_count`]）。
///
/// **今のカーネルでは、設定中に別の CPU が来る形は起きない**（AP を起こすのは、`kernel_main` の初めの `init` の
/// 後である）。待つ側は、契約として正しい側に倒してある。
///
/// # 印は 1 つである
///
/// **作れるポートは COM1 だけなので、印もモジュールに 1 つ持つ。** ブートローダとカーネルは別のバイナリなので、
/// 印もそれぞれが持つ（カーネルは、ブートローダが書いた設定に頼らず、自分で 1 回書く）。
struct UartSetup {
    /// [`SETUP_NOT_YET`]・[`SETUP_IN_PROGRESS`]・[`SETUP_DONE`] のどれか。
    state: AtomicU8,
    /// 設定を書いた回数。**1 が正常である。**
    writes: AtomicU64,
    /// 設定中のまま上限に着いて、待つのをやめた回数。**0 が正常である。**
    gave_up: AtomicU64,
}

const SETUP_NOT_YET: u8 = 0;
const SETUP_IN_PROGRESS: u8 = 1;
const SETUP_DONE: u8 = 2;

/// 設定中の間、済むのを待つ回数の上限。
///
/// **測って決めた**（2026-10-01。QEMU の TCG、`-smp 2`。TSC のサイクル）。
///
/// | 測ったもの | サイクル |
/// |---|---|
/// | 設定を書く（7 つのレジスタ。18 回測った） | 14,774 〜 96,650 |
/// | 10 万回待つ（9 回測った） | 8.7 × 10^7 〜 1.2 × 10^8 |
/// | 100 万回待つ（9 回測った） | 8.9 × 10^8 〜 1.04 × 10^9 |
///
/// **10 万回は、測った中でいちばん長い設定の 900 倍以上である。** 100 万回でも足りるが、設定中の CPU が
/// 止まったときに、ポートを作る所がどこも毎回この上限だけ待つので、長くしすぎない
/// （[`WAIT_TIMEOUT_CYCLES`] の「大きくしすぎない理由」と同じ）。**実機では測っていない。**
const SETUP_WAIT_SPINS: u32 = 100_000;

static UART_SETUP: UartSetup = UartSetup {
    state: AtomicU8::new(SETUP_NOT_YET),
    writes: AtomicU64::new(0),
    gave_up: AtomicU64::new(0),
};

/// [`take_setup_turn`] の答え。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupTurn {
    /// 最初の呼び出しである。**設定を書き、書き終えたら印を「済み」にする。**
    Write,
    /// 設定は済んでいる（待っている間に済んだ場合を含む）。**何もしない。**
    AlreadyDone,
    /// 設定中のまま上限に着いた。**待つのをやめて返る**（設定は書かない。呼び出し元は、そのまま出力を書く）。
    GaveUp,
}

/// 設定を書く番かを決める（純粋な論理。印を「まだ」から「設定中」へ進めるのは、1 つの呼び出しだけである）。
fn take_setup_turn(state: &AtomicU8, wait_spins: u32) -> SetupTurn {
    match state.compare_exchange(
        SETUP_NOT_YET,
        SETUP_IN_PROGRESS,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => SetupTurn::Write,
        Err(SETUP_DONE) => SetupTurn::AlreadyDone,
        Err(_) => {
            for _ in 0..wait_spins {
                if state.load(Ordering::Acquire) == SETUP_DONE {
                    return SetupTurn::AlreadyDone;
                }
                core::hint::spin_loop();
            }
            SetupTurn::GaveUp
        }
    }
}

/// UART の設定を書いた回数（計測）。**1 が正常である**——起動の 1 回だけ書く。
///
/// # 契約（境界の関数。2026-10-01）
///
/// - [`Serial::init`] がレジスタへ設定を書いた回数を読むだけで、何も変えない。
pub fn serial_setup_write_count() -> u64 {
    UART_SETUP.writes.load(Ordering::Relaxed)
}

/// 設定中のまま上限に着いて、待つのをやめた回数（計測）。**0 が正常である。**
///
/// # 契約（境界の関数。2026-10-01）
///
/// - [`Serial::init`] が、ほかの呼び出しの設定が済むのを待ち切れなかった回数を読むだけで、何も変えない。
pub fn serial_setup_gave_up_count() -> u64 {
    UART_SETUP.gave_up.load(Ordering::Relaxed)
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
/// **作れるのは、決まった番地の 1 本目のシリアルだけである**（[`Serial::primary`] と [`open_direct_serial`]。
/// 2026-09-30 に `new` を外へ出さない形にした）。**任意のポートを受ける入口を外に置くと、安全な関数の
/// `init`・`write_byte` から任意のポートへ書けてしまう。** 実際の
/// ハードウェアアクセスは [`crate::arch::x86_64::port`] の `outb` / `inb` に閉じ込め、
/// `init` / `write_byte` はその契約（固定の既知オフセットのみを、決められた
/// 16550 初期化手順どおりに叩く）を自身で満たすことで安全な API として
/// 公開する。
///
/// # 契約（境界の型。2026-09-30）
///
/// - 共通の側が作るのは [`Serial::primary`]（ログに使う 1 本目のシリアル）と
///   [`open_direct_serial`]（設定の済んだポートを返す）だけである（`new` は外へ出していない）。直に開ける所は、
///   xtask の許可の表で数える。
/// - UART の設定を書くのは、起動の 1 回だけである（[`Self::init`] の契約）。**COM1 のレジスタを書くのは、
///   この module だけである**——`kernel`・`common`・`bootloader` でポートへ書く所を検索して確かめた
///   （2026-10-01。読んだ範囲である）。ポートへ直に書く所は xtask の許可の表（`DIRECT_PORT_IO_ALLOWLIST`）が
///   数えていて、ほかに書くのは、キーボードのコントローラ・PIC・PIT・PCI の設定の決まった番地と、PCI の装置が
///   持つ I/O の範囲（番地は装置が決める）である。ユーザーのプログラムにポートへ書かせる入口は無い。
///   そのため、パニックと例外の経路でも設定を書き直さない。
///   **PCI の装置の I/O の範囲が COM1 の番地（`0x3F8` から 8 つ）と重なる割り当てになっていないことは、
///   確かめていない**（ファームウェアが割り当てた番地を、そのまま使っている）。上の「この module だけ」は、
///   重なっていないことを前提にしている。
/// - 行を書く間の排他は `write_fmt` の中のロックが持つ（最内側。取れなければ、混ざるのを承知で
///   書く）。割り込みは止めない。
/// - 割り込みの処理からは書かない（規約。モジュールの doc の「UART を書いている間の排他」）。
pub struct Serial {
    base: u16,
}

impl Serial {
    /// 番地 `base` の 16550 を指す（**この module の中だけで使う**。外へは [`Self::primary`] と
    /// [`open_direct_serial`] だけを出す）。
    const fn new(base: u16) -> Self {
        Self { base }
    }

    /// ログに使う 1 本目のシリアル（PC では COM1）。**設定は書かない**——書く前に [`Self::init`] を呼ぶこと
    /// （[`open_direct_serial`] は呼んでから返す）。**設定を実際に書くのは、最初の `init` だけである。**
    #[inline(always)]
    pub const fn primary() -> Self {
        Self::new(COM1_BASE)
    }

    /// UART の設定が済んでいるようにする。**設定を書くのは初めの 1 回だけで、2 回目からは何もしない。**
    ///
    /// # 契約（境界の関数。2026-10-01）
    ///
    /// - 初めの呼び出しが、16550 の設定（[`Self::write_setup`]）を書く。2 回目からの呼び出しは、レジスタに
    ///   触らない。**名前は「初期化」のままだが、呼ぶたびに初期化し直すものではない**（呼び出し元は、ポートを
    ///   作るたびに呼んでよい）。
    /// - ほかの呼び出しが設定を書いている途中なら、済むまで少しだけ待つ。上限に着いたら、待つのをやめて返る
    ///   （理由は [`UartSetup`]、上限は [`SETUP_WAIT_SPINS`]）。**上限は QEMU で測って決めた。実機では
    ///   測っていない。**
    /// - 設定を書く間は、最内側のロックを取らない（ほかの CPU がまだ動いていない、起動の初めに済む）。
    pub fn init(&mut self) {
        // **破壊テスト（`serial-reinit-every-open`）**: 以前の形に戻す。**呼ばれるたびに設定を書き直す。**
        // ほかの CPU が書いている途中に重なると、文字が欠ける。
        if cfg!(feature = "serial-reinit-every-open") {
            self.write_setup();
            UART_SETUP.writes.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match take_setup_turn(&UART_SETUP.state, SETUP_WAIT_SPINS) {
            SetupTurn::Write => {
                self.write_setup();
                UART_SETUP.writes.fetch_add(1, Ordering::Relaxed);
                UART_SETUP.state.store(SETUP_DONE, Ordering::Release);
            }
            SetupTurn::AlreadyDone => {}
            SetupTurn::GaveUp => {
                UART_SETUP.gave_up.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// 16550 UART の標準的な初期化手順（割り込み無効化 → ボーレート設定
    /// → 通信フォーマット設定 → FIFO 有効化 → モデム制御設定）を実行する。**[`Self::init`] だけが呼ぶ。**
    fn write_setup(&mut self) {
        // SAFETY: 触れるのは `self.base` を起点とする 16550 の既知のレジスタ
        // だけで、オフセットはいずれもこのモジュール内の定数である。ZeikOS は
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

impl fmt::Write for Serial {
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
    /// **呼び出し側は 1 つも触らない。** **`Serial::new` を書いた箇所は
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

/// **ロガーも BKL も `Locked<T>` も通さずに、シリアルへ直に書くための入口**（2026-09-28。境界の段階の手順 2）。
///
/// 設定の済んだポートを返す。**共通の側は返り値の型の名前を書かず、`write!` / `writeln!` で書くだけにする。**
/// **PC では COM1（16550 互換の UART）を返す。** **ARM の機械では、同じ役目を別の UART（PL011 など）が担い、
/// その機械の置き場が同じ名前の入口を持つ。**
///
/// # どこで使うか
///
/// **ロックを取れない場所で使う**——BKL の中の報告（BKL 自身の失敗）、割り込みを禁止した区間（タスクの切り替え）、
/// パニック。**渡されるロガーが無い場所**（BKL を解いた区間、タスクの入口）でも使う。**どこから呼んでよいかは、
/// xtask の許可リストが決める**（`DIRECT_SERIAL_PORT_ALLOWLIST`）。
///
/// # 書くときに取るロック
///
/// **取るのは UART 自身の最内側のロックだけで、1 回の `write!` の間だけ持つ**（[`SerialLock`]）。
/// **取れなければ、混ざるのを承知で書く**——ここで止めると、報せる手段そのものが消える。
///
/// # 契約（境界の関数。2026-09-30）
///
/// - 設定の済んだポートを返す。**呼ぶたびに設定を書き直すことはしない**——設定を書くのは起動の 1 回だけで
///   （[`Serial::init`] の契約）、ほかの CPU が書いている途中に開いても、その出力を壊さない。
/// - 設定がまだなら、この呼び出しが書く（起動の初めの `init` より前に呼ばれた場合。[`UartSetup`]）。
/// - 呼んでよい所と、書く間に取るロックは、上の 2 つの節のとおりである。
pub fn open_direct_serial() -> Serial {
    let mut serial = Serial::new(COM1_BASE);
    serial.init();
    serial
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **最初の呼び出しだけが設定を書く番を取る。** 印は「設定中」へ進む。
    #[test]
    fn the_first_call_takes_the_turn_to_write_the_setup() {
        let state = AtomicU8::new(SETUP_NOT_YET);
        assert_eq!(take_setup_turn(&state, 8), SetupTurn::Write);
        assert_eq!(state.load(Ordering::Acquire), SETUP_IN_PROGRESS);
    }

    /// **済んだ後の呼び出しは、何もしない。** 印も動かない。
    #[test]
    fn a_call_after_the_setup_is_done_does_nothing() {
        let state = AtomicU8::new(SETUP_DONE);
        assert_eq!(take_setup_turn(&state, 8), SetupTurn::AlreadyDone);
        assert_eq!(take_setup_turn(&state, 0), SetupTurn::AlreadyDone);
        assert_eq!(state.load(Ordering::Acquire), SETUP_DONE);
    }

    /// **設定中のまま上限に着いたら、待つのをやめる。** 番は取らず、印も動かさない
    /// （設定を書いている側が、後で「済み」にする）。
    #[test]
    fn a_call_during_the_setup_gives_up_at_the_limit() {
        let state = AtomicU8::new(SETUP_IN_PROGRESS);
        assert_eq!(take_setup_turn(&state, 8), SetupTurn::GaveUp);
        assert_eq!(take_setup_turn(&state, 0), SetupTurn::GaveUp);
        assert_eq!(state.load(Ordering::Acquire), SETUP_IN_PROGRESS);
    }

    /// **設定中に来た呼び出しは、済むのを待ってから返る。** 別のスレッドが「済み」にするまで待ち、
    /// 設定を書く番は取らない。
    #[test]
    fn a_call_during_the_setup_waits_until_it_is_done() {
        let state = AtomicU8::new(SETUP_NOT_YET);
        assert_eq!(take_setup_turn(&state, 0), SetupTurn::Write);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| take_setup_turn(&state, u32::MAX));
            std::thread::sleep(std::time::Duration::from_millis(20));
            state.store(SETUP_DONE, Ordering::Release);
            assert_eq!(waiter.join().unwrap(), SetupTurn::AlreadyDone);
        });
    }

    /// **番を取れるのは、同時に呼んでも 1 つだけである。**
    #[test]
    fn only_one_of_many_callers_takes_the_turn() {
        let state = AtomicU8::new(SETUP_NOT_YET);
        let writers = std::thread::scope(|scope| {
            let callers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| take_setup_turn(&state, 0)))
                .collect();
            callers
                .into_iter()
                .map(|caller| caller.join().unwrap())
                .filter(|turn| *turn == SetupTurn::Write)
                .count()
        });
        assert_eq!(writers, 1);
    }
}
