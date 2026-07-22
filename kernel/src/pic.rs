//! 8259A PIC の再マップとマスク（M4-c-3）。
//!
//! **unsafe を含む。** I/O ポートを直接叩いて 2 個の 8259A を初期化する。
//!
//! ## なぜ再マップが必要か
//!
//! 8259A の既定のベクタは マスタが 0x08-0x0F、スレーブが 0x70-0x77 である。
//! マスタ側が CPU の例外ベクタ（0x00-0x1F）と真正面から衝突する。再マップ
//! せずに `sti` すると、タイマ（IRQ0）が**ベクタ 8 = ダブルフォルトとして
//! 配送される**。ダブルフォルトのハンドラは IST スタックへ切り替えて停止
//! するため、「タイマを有効にした瞬間に、スタックオーバーフローでもないのに
//! ダブルフォルトが出る」という、原因の見当がつかない症状になる
//! （ADR-0018 のチェックリスト 1）。
//!
//! そこで例外ベクタの外側、0x20-0x2F へ移す。ここは IDT の全 256 エントリを
//! 埋めてある範囲（`crate::idt`）に収まるので、万一この段階で IRQ が届いても
//! 「予期しないベクタ」として報告される。無言では落ちない。
//!
//! ## この段階では全 IRQ をマスクする
//!
//! M4-c にはまだ IRQ ハンドラが無い。[`remap`] は初期化の最後に必ず
//! OCW1 で全 IRQ をマスクし、明示的に [`set_masks`] を呼ぶまで何も上がら
//! ない状態にする。「再マップしたが解禁の指示を忘れた」場合に暴走ではなく
//! 沈黙する側へ倒す。
//!
//! ## ベクタオフセットは読み戻せない（検証範囲の限界）
//!
//! **ICW2（ベクタオフセット）は書き込み専用である。** データポート
//! （0x21 / 0xA1）から読めるのは IMR だけで、8259A に「今どのベクタへ
//! 割り当てているか」を問い合わせる手段は無い。したがって [`remap`] の
//! 直後にできる検証は**マスクの読み戻しだけ**であり、オフセットが実際に
//! 意図どおり書けたかは、この時点では一切確かめられていない。
//!
//! マスクの読み戻しが通ったことをもって「再マップが正しい」と読まないこと。
//! それは検査していない対象について検査済みだと錯覚する、このプロジェクトで
//! 過去 2 回起きた形の誤りである。
//!
//! **オフセットが証明されるのは M4-d で最初のタイマ割り込みがベクタ 0x20
//! として届いたときである。** それまでは未検証のまま持ち越す。
//!
//! ICW2 を書き間違えた場合の症状:
//!
//! - **どこにも届かないオフセットを書いた場合**: タイマのマスクを外して
//!   `sti` しても、割り込みが一向に来ない。「PIT の初期化を間違えた」
//!   「EOI を忘れた」と誤診しやすいが、実際はベクタが別の場所を向いている。
//! - **意図と違うベクタを書いた場合**: そのベクタのハンドラ（M4-b-1 で
//!   全 256 に入れた「予期しないベクタ」ハンドラ）が受けて停止する。
//!   報告されるベクタ番号が 0x20 でなければ、ICW2 の値を疑う。
//! - **CPU の例外ベクタと重なる値を書いた場合**: [`validate_offsets`] が
//!   弾くので、この経路には入らない。
//!
//! M4-d でタイマが動かないときは、まずこの 3 つを切り分けること。
//!
//! ## 純粋ロジックの分離
//!
//! 初期化語（ICW）の組み立てとベクタ範囲の妥当性検査は、ポート I/O を
//! 含まない関数として切り出してホスト `cargo test` で検証する。ポートを
//! 叩く部分だけが `unsafe` になる。

use common::port::{inb, io_wait, outb};

/// マスタ PIC のコマンドポート（ICW1 / OCW2 / OCW3）。
const MASTER_COMMAND_PORT: u16 = 0x20;
/// マスタ PIC のデータポート（ICW2-4 / OCW1 = IMR）。
const MASTER_DATA_PORT: u16 = 0x21;
/// スレーブ PIC のコマンドポート。
const SLAVE_COMMAND_PORT: u16 = 0xA0;
/// スレーブ PIC のデータポート。
const SLAVE_DATA_PORT: u16 = 0xA1;

/// ICW1: 初期化開始（bit4）+ ICW4 を送る（bit0）。
///
/// bit1 = 0 はカスケード構成（2 個使う）、bit3 = 0 はエッジトリガ。
const ICW1_INIT_WITH_ICW4: u8 = 0x11;

/// ICW4: 8086/88 モード。
///
/// bit0 だけを立てる。AEOI（自動 EOI, bit1）は使わない。EOI を明示的に
/// 送る形にしておかないと、ハンドラが EOI を出しているかどうかを実装から
/// 読み取れなくなる（ADR-0018 のチェックリスト 2）。
const ICW4_8086_MODE: u8 = 0x01;

/// スレーブ PIC がぶら下がっているマスタ側の IRQ 番号。
///
/// IBM PC 以来の配線で固定されている。
pub const CASCADE_IRQ: u8 = 2;

/// 全 IRQ をマスクする IMR の値。
pub const MASK_ALL: u8 = 0xFF;

/// 1 個の PIC が扱う IRQ の本数。
pub const IRQS_PER_PIC: u8 = 8;

/// マスタ PIC を割り当てるベクタの先頭（IRQ0 = 0x20）。
#[cfg(not(feature = "alt-offset-test"))]
pub const MASTER_VECTOR_OFFSET: u8 = 0x20;
/// スレーブ PIC を割り当てるベクタの先頭（IRQ8 = 0x28）。
#[cfg(not(feature = "alt-offset-test"))]
pub const SLAVE_VECTOR_OFFSET: u8 = 0x28;

// `alt-offset-test`: わざと別のオフセットへ再マップする。
//
// **目的は「ZaytOS の ICW2 書き込みが実際にオフセットを動かしている」ことの
// 確認である。** 通常構成の 0x20 は、OVMF が既に同じ値を使っていた可能性が
// 高く（`docs/troubleshooting.md` 2026-07-21）、こちらの ICW2 が間違って
// いてもハードウェアが既に正しい状態にあるせいで動いてしまいうる。
// 別の値でティックが届けば、その疑いが晴れる。
//
// 同時に**配送元の切り分けにもなる**。8259A 経由ならベクタが移動し、
// LAPIC 経由なら 0x20 のまま届く。
#[cfg(feature = "alt-offset-test")]
pub const MASTER_VECTOR_OFFSET: u8 = 0x30;
#[cfg(feature = "alt-offset-test")]
pub const SLAVE_VECTOR_OFFSET: u8 = 0x38;

/// ベクタオフセットとして受け付けられない値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffsetError {
    /// 8 の倍数でない。ICW2 の下位 3 ビットは無視され、実際のベクタが
    /// 指定した値と食い違う。
    NotEightAligned,
    /// CPU の例外ベクタ（0x00-0x1F）と重なる。再マップの目的そのものを
    /// 満たさない。
    OverlapsCpuExceptions,
    /// 先頭 + 8 がベクタ番号の範囲（0-255）を超える。
    RangeOverflows,
    /// マスタとスレーブの 8 ベクタずつが重なっている。
    RangesOverlap,
}

/// CPU の例外が占めるベクタ数（0x00-0x1F）。
const CPU_EXCEPTION_VECTOR_COUNT: u8 = 32;

/// ベクタオフセットの組が使えるものかを検査する（純粋ロジック）。
pub const fn validate_offsets(master_offset: u8, slave_offset: u8) -> Result<(), OffsetError> {
    if !master_offset.is_multiple_of(IRQS_PER_PIC) || !slave_offset.is_multiple_of(IRQS_PER_PIC) {
        return Err(OffsetError::NotEightAligned);
    }
    if master_offset < CPU_EXCEPTION_VECTOR_COUNT || slave_offset < CPU_EXCEPTION_VECTOR_COUNT {
        return Err(OffsetError::OverlapsCpuExceptions);
    }
    // 8 の倍数なので、先頭が 0xF8 を超えることはこの時点であり得ないが、
    // 定数を変えたときに気づけるよう明示的に検査する。
    if master_offset > u8::MAX - IRQS_PER_PIC || slave_offset > u8::MAX - IRQS_PER_PIC {
        return Err(OffsetError::RangeOverflows);
    }
    if master_offset == slave_offset {
        return Err(OffsetError::RangesOverlap);
    }
    Ok(())
}

/// 初期化コマンド語（ICW1-4）の組。
///
/// **ICW3 だけがマスタとスレーブで意味が違う。** マスタ側は「どの IRQ 線に
/// スレーブがぶら下がっているか」を示す**ビットマスク**、スレーブ側は
/// 「自分がマスタの何番につながっているか」を示す**番号そのもの**である。
/// 両方に同じ値を送るのが典型的な間違いで、そうするとカスケードが成立せず
/// スレーブ側の IRQ8-15 が一切届かなくなる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitCommandWords {
    pub icw1: u8,
    /// マスタの ICW2: ベクタオフセット。
    pub master_icw2: u8,
    /// スレーブの ICW2: ベクタオフセット。
    pub slave_icw2: u8,
    /// マスタの ICW3: スレーブが接続された IRQ 線のビットマスク。
    pub master_icw3: u8,
    /// スレーブの ICW3: 自分が接続されたマスタ側の IRQ 番号。
    pub slave_icw3: u8,
    pub icw4: u8,
}

/// 指定したベクタオフセットに対する ICW を組み立てる（純粋ロジック）。
pub const fn init_command_words(master_offset: u8, slave_offset: u8) -> InitCommandWords {
    InitCommandWords {
        icw1: ICW1_INIT_WITH_ICW4,
        master_icw2: master_offset,
        slave_icw2: slave_offset,
        master_icw3: 1 << CASCADE_IRQ,
        slave_icw3: CASCADE_IRQ,
        icw4: ICW4_8086_MODE,
    }
}

/// IRQ 番号（0-15）に対応する割り込みベクタ（純粋ロジック）。
///
/// 範囲外は `None`。
pub const fn irq_vector(irq: u8) -> Option<u8> {
    if irq < IRQS_PER_PIC {
        Some(MASTER_VECTOR_OFFSET + irq)
    } else if irq < 2 * IRQS_PER_PIC {
        Some(SLAVE_VECTOR_OFFSET + (irq - IRQS_PER_PIC))
    } else {
        None
    }
}

/// 現在の IMR（割り込みマスクレジスタ）を読む。`(マスタ, スレーブ)`。
///
/// ビットが 1 の IRQ がマスクされている。データポートは ICW シーケンスの
/// 外では OCW1 = IMR として振る舞うため、読み出しに副作用は無い。
pub fn read_masks() -> (u8, u8) {
    // SAFETY: 0x21 / 0xA1 は 8259A の IMR であり、読み出しは状態を変えない。
    // シングルコアかつ、この関数を呼ぶのは起動シーケンスと診断だけ。
    unsafe { (inb(MASTER_DATA_PORT), inb(SLAVE_DATA_PORT)) }
}

/// IMR を書き換える。ビットが 1 の IRQ がマスクされる。
///
/// **スレーブを使うならマスタの IRQ2 をマスクしてはいけない。** スレーブは
/// マスタの IRQ2 にカスケード接続されており（[`CASCADE_IRQ`]）、IRQ8-15 は
/// すべてマスタの IRQ2 を経由して CPU へ届く。マスタ側で IRQ2 を落とすと、
/// スレーブ側でいくら該当ビットを開けても何も来ない。キーボード（IRQ1）は
/// マスタ側なので M4-e では影響しないが、RTC（IRQ8）や PS/2 マウス
/// （IRQ12）を使う段になると効いてくる。
///
/// # Safety
///
/// マスクを外した IRQ には、EOI を発行するハンドラが IDT に入っていなければ
/// ならない。入っていない状態で割り込みが有効化されると、「予期しないベクタ」
/// として停止するか、EOI が出ずに以降の割り込みが止まる。
pub unsafe fn set_masks(master: u8, slave: u8) {
    // SAFETY: データポートへの書き込みは OCW1（IMR の設定）であり、
    // 呼び出し側がハンドラの用意を保証する契約。
    unsafe {
        outb(MASTER_DATA_PORT, master);
        io_wait();
        outb(SLAVE_DATA_PORT, slave);
        io_wait();
    }
}

/// 2 個の PIC を初期化し、指定のベクタオフセットへ再マップする。
///
/// 完了時点で**全 IRQ がマスクされている**。解禁は呼び出し側が
/// [`set_masks`] で明示的に行う。
///
/// **戻り値の `Ok` は「引数が妥当で、規定の順序で書き込んだ」ことしか
/// 意味しない。** オフセットが実際に反映されたかは ICW2 が読めない以上
/// 確認できず、M4-d で最初の割り込みが届くまで未検証のまま残る
/// （モジュールの doc コメント参照）。
///
/// # Safety
///
/// - 起動時に 1 回だけ呼ぶこと。ICW シーケンスの途中で別の実行文脈が同じ
///   ポートを触ると、PIC が中途半端な状態のまま残る。
/// - 呼び出し時点で割り込みが禁止されていること。
/// - `master_offset` から始まる 8 個と `slave_offset` から始まる 8 個の
///   ベクタが、present なハンドラを持つ IDT でカバーされていること。
pub unsafe fn remap(master_offset: u8, slave_offset: u8) -> Result<(), OffsetError> {
    validate_offsets(master_offset, slave_offset)?;
    let words = init_command_words(master_offset, slave_offset);

    // ICW1 を書いた時点で、両 PIC はデータポートへの以降の書き込みを
    // ICW2 → ICW3 → ICW4 の順に解釈する初期化モードへ入る。順序を
    // 崩したり途中で抜けたりすると、以後の IMR 書き込みまで ICW として
    // 解釈され続ける。
    //
    // 各段の後に io_wait() を挟む。QEMU では不要だが、実機の ISA バスは
    // 連続した out に追いつけないことがある。
    //
    // SAFETY: すべて 8259A の既知のポートに対する、規定の初期化手順どおりの
    // 書き込み。呼び出し側の契約により、この区間に割り込みは入らず、他の
    // 実行文脈が同じポートを触ることもない。
    unsafe {
        outb(MASTER_COMMAND_PORT, words.icw1);
        io_wait();
        outb(SLAVE_COMMAND_PORT, words.icw1);
        io_wait();

        outb(MASTER_DATA_PORT, words.master_icw2);
        io_wait();
        outb(SLAVE_DATA_PORT, words.slave_icw2);
        io_wait();

        outb(MASTER_DATA_PORT, words.master_icw3);
        io_wait();
        outb(SLAVE_DATA_PORT, words.slave_icw3);
        io_wait();

        outb(MASTER_DATA_PORT, words.icw4);
        io_wait();
        outb(SLAVE_DATA_PORT, words.icw4);
        io_wait();
    }

    // 初期化直後の IMR は不定。ハンドラを 1 つも書いていない今の段階では
    // 全マスクが唯一の安全な状態なので、ICW シーケンスの直後に必ず閉じる。
    //
    // SAFETY: すべてマスクする方向の変更であり、ハンドラの有無に依存しない。
    unsafe {
        set_masks(MASK_ALL, MASK_ALL);
    }

    Ok(())
}

/// OCW3: 次にコマンドポートを読んだとき ISR を返させる。
///
/// bit3(=1) が「OCW3 である」ことを示し、bit1-0 = 11 で ISR を選ぶ
/// （10 なら IRR）。ISR は「今 CPU へ配送中の割り込み」のビット。
const OCW3_READ_IN_SERVICE: u8 = 0x0B;

/// ISR（In-Service Register）を読む。`(マスタ, スレーブ)`。
///
/// スプリアス割り込みの判定に使う。**読み出しの前に OCW3 を書く必要が
/// あり、副作用がある**（コマンドポートの読み出し対象を切り替える）ため、
/// [`read_masks`] のように安全な関数にはしていない。
///
/// # Safety
///
/// コマンドポートの読み出し対象を変更する。他の実行文脈が同時に PIC を
/// 触っていないこと。
pub unsafe fn read_isr() -> (u8, u8) {
    // SAFETY: OCW3 を書いてから同じポートを読む、8259A の規定の手順。
    // 呼び出し側が排他を保証する契約。
    unsafe {
        outb(MASTER_COMMAND_PORT, OCW3_READ_IN_SERVICE);
        outb(SLAVE_COMMAND_PORT, OCW3_READ_IN_SERVICE);
        io_wait();
        (inb(MASTER_COMMAND_PORT), inb(SLAVE_COMMAND_PORT))
    }
}

/// スプリアス割り込みが現れる IRQ 番号（各 PIC の最下位優先度の線）。
pub const MASTER_SPURIOUS_IRQ: u8 = 7;
pub const SLAVE_SPURIOUS_IRQ: u8 = 15;

/// 受けた割り込みがスプリアス（偽）かどうかを判定する（純粋ロジック）。
///
/// 8259A はノイズなどで、実際には要求が無いのに割り込みを上げることがある。
/// その場合ベクタは各 PIC の最下位優先度（IRQ7 / IRQ15）として届く。
/// **本物なら ISR の該当ビットが立っている。立っていなければ偽である。**
///
/// IRQ7 / IRQ15 以外はスプリアスになりえないので常に `false`。
pub const fn is_spurious(irq: u8, isr: (u8, u8)) -> bool {
    let (master_isr, slave_isr) = isr;
    if irq == MASTER_SPURIOUS_IRQ {
        master_isr & (1 << MASTER_SPURIOUS_IRQ) == 0
    } else if irq == SLAVE_SPURIOUS_IRQ {
        slave_isr & (1 << (SLAVE_SPURIOUS_IRQ - IRQS_PER_PIC)) == 0
    } else {
        false
    }
}

/// EOI をどこへ送るか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EoiAction {
    /// 送らない。
    None,
    /// マスタにだけ送る。
    MasterOnly,
    /// スレーブ → マスタの順に送る。
    SlaveThenMaster,
}

/// 受けた割り込みに対して EOI をどう送るかを決める（純粋ロジック）。
///
/// **スプリアスの扱いがマスタ側とスレーブ側で非対称である。**
///
/// - **IRQ7 のスプリアス**: マスタ自身が偽の割り込みを上げただけなので、
///   ISR にビットは立っておらず、EOI を送る相手がいない。送ってはいけない。
/// - **IRQ15 のスプリアス**: スレーブが偽を上げた場合でも、**マスタ側は
///   IRQ2（カスケード）を本物として受け付けており ISR のビットが立っている**。
///   スレーブへ送ってはいけないが、**マスタへは送らなければならない**。
///   送らないとマスタが処理中のまま残り、以降マスタの全 IRQ が上がらなくなる。
pub const fn eoi_action_for(irq: u8, spurious: bool) -> EoiAction {
    if spurious {
        if irq == SLAVE_SPURIOUS_IRQ {
            // スレーブは偽だがマスタは本物として受けている。
            EoiAction::MasterOnly
        } else {
            EoiAction::None
        }
    } else if irq < IRQS_PER_PIC {
        EoiAction::MasterOnly
    } else if irq < 2 * IRQS_PER_PIC {
        EoiAction::SlaveThenMaster
    } else {
        EoiAction::None
    }
}

/// [`eoi_action_for`] が決めた宛先へ EOI を送る。
///
/// # Safety
///
/// `action` は [`eoi_action_for`] が返した値であること。独自の判断で
/// `MasterOnly` などを渡すと、実在しない割り込みへ応答することになる。
pub unsafe fn send_eoi_for(action: EoiAction) {
    // SAFETY: コマンドポートへの OCW2 書き込み。宛先の妥当性は呼び出し側の契約。
    unsafe {
        match action {
            EoiAction::None => {}
            EoiAction::MasterOnly => {
                outb(MASTER_COMMAND_PORT, OCW2_END_OF_INTERRUPT);
                io_wait();
            }
            EoiAction::SlaveThenMaster => {
                outb(SLAVE_COMMAND_PORT, OCW2_END_OF_INTERRUPT);
                io_wait();
                outb(MASTER_COMMAND_PORT, OCW2_END_OF_INTERRUPT);
                io_wait();
            }
        }
    }
}

/// OCW2: 非特定 EOI（End Of Interrupt）。
///
/// bit5 だけを立てる。「今処理中の最も優先度の高い割り込み」を終了と
/// みなす形式で、ベクタ番号を指定する特定 EOI（0x60 | irq）より単純である。
/// シングルコアで多重割り込みを許していない現状では両者に差が無い。
const OCW2_END_OF_INTERRUPT: u8 = 0x20;

/// 指定した IRQ の処理完了を PIC へ通知する。
///
/// **ハンドラの処理を終えてから送ること。** EOI を送った時点で PIC は次の
/// 同じ割り込みを上げられるようになる。
///
/// **スレーブ側の IRQ（8-15）では両方へ送る必要がある。** スレーブは
/// マスタの IRQ2 を経由して CPU へ届くため、マスタから見ても 1 件の割り込みが
/// 進行中になっている。スレーブにだけ送るとマスタ側が処理中のまま残り、
/// **以降マスタの全 IRQ が上がらなくなる**（EOI 忘れと同じ症状が、別の
/// IRQ に対して起きる）。順序はスレーブ → マスタ。
///
/// # Safety
///
/// **実際に発生した割り込みに対してのみ呼ぶこと。** 発生していない割り込みに
/// EOI を送ると、PIC の優先度スタックの状態が実態とずれる。とくに
/// スプリアス割り込み（IRQ7 / IRQ15）へ送ってはならない。
pub unsafe fn send_end_of_interrupt(irq: u8) {
    // SAFETY: コマンドポートへの OCW2 書き込み。呼び出し側が「実際に発生した
    // 割り込みである」ことを保証する契約。
    unsafe {
        if irq >= IRQS_PER_PIC {
            outb(SLAVE_COMMAND_PORT, OCW2_END_OF_INTERRUPT);
            io_wait();
        }
        outb(MASTER_COMMAND_PORT, OCW2_END_OF_INTERRUPT);
        io_wait();
    }
}

/// 現在のマスクから、指定した IRQ 1 本だけを解除した値を返す（純粋ロジック）。
///
/// マスタ側の IRQ を解除する場合は何も連動しない。**スレーブ側（8-15）を
/// 解除する場合は、マスタの IRQ2（カスケード）も同時に解除しなければ
/// ならない。** スレーブの割り込みはすべてマスタの IRQ2 を通って CPU へ
/// 届くため、そこが閉じていればスレーブ側で開けても何も来ない。
pub const fn masks_with_irq_unmasked(masks: (u8, u8), irq: u8) -> (u8, u8) {
    let (master, slave) = masks;
    if irq < IRQS_PER_PIC {
        (master & !(1 << irq), slave)
    } else if irq < 2 * IRQS_PER_PIC {
        (
            // カスケードも開ける。忘れるとスレーブ側の IRQ が一切届かない。
            master & !(1 << CASCADE_IRQ),
            slave & !(1 << (irq - IRQS_PER_PIC)),
        )
    } else {
        masks
    }
}

/// 指定した IRQ 1 本のマスクを解除する。
///
/// # Safety
///
/// 解除する IRQ には、EOI を発行するハンドラが IDT に入っていなければ
/// ならない（ADR-0018 §2 の項目 5 / 7）。
pub unsafe fn unmask_irq(irq: u8) {
    let updated = masks_with_irq_unmasked(read_masks(), irq);
    // SAFETY: ハンドラの用意は呼び出し側の契約。
    unsafe {
        set_masks(updated.0, updated.1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_offsets_are_valid() {
        assert_eq!(
            validate_offsets(MASTER_VECTOR_OFFSET, SLAVE_VECTOR_OFFSET),
            Ok(())
        );
    }

    #[test]
    fn offsets_that_collide_with_cpu_exceptions_are_rejected() {
        // 8259A の既定値（マスタ 0x08）。これを弾けないと、再マップの
        // 目的そのものを取り違えたまま通ってしまう。
        assert_eq!(
            validate_offsets(0x08, 0x70),
            Err(OffsetError::OverlapsCpuExceptions)
        );
        assert_eq!(
            validate_offsets(0x20, 0x18),
            Err(OffsetError::OverlapsCpuExceptions)
        );
    }

    #[test]
    fn offsets_must_be_multiples_of_eight() {
        // ICW2 の下位 3 ビットは無視されるため、8 の倍数でない指定は
        // 「書いた値と実際のベクタが違う」という形で静かに裏切る。
        assert_eq!(
            validate_offsets(0x21, 0x28),
            Err(OffsetError::NotEightAligned)
        );
        assert_eq!(
            validate_offsets(0x20, 0x2A),
            Err(OffsetError::NotEightAligned)
        );
    }

    #[test]
    fn the_two_ranges_must_not_overlap() {
        assert_eq!(
            validate_offsets(0x20, 0x20),
            Err(OffsetError::RangesOverlap)
        );
    }

    #[test]
    fn ranges_must_fit_in_the_vector_space() {
        assert_eq!(
            validate_offsets(0xF8, 0x20),
            Err(OffsetError::RangeOverflows)
        );
    }

    #[test]
    fn the_command_words_match_the_datasheet() {
        let words = init_command_words(0x20, 0x28);
        assert_eq!(words.icw1, 0x11, "初期化開始 + ICW4 あり");
        assert_eq!(words.master_icw2, 0x20);
        assert_eq!(words.slave_icw2, 0x28);
        assert_eq!(words.icw4, 0x01, "8086/88 モード、AEOI は使わない");
    }

    /// マスタの ICW3 はビットマスク、スレーブの ICW3 は番号。
    ///
    /// ここを取り違えると IRQ8-15 が一切届かなくなる。値が偶然一致しない
    /// ことを明示的に固定する。
    #[test]
    fn the_cascade_words_use_different_encodings() {
        let words = init_command_words(0x20, 0x28);
        assert_eq!(words.master_icw3, 0b0000_0100, "IRQ2 のビットが立つ");
        assert_eq!(words.slave_icw3, 2, "接続先の IRQ 番号そのもの");
        assert_ne!(words.master_icw3, words.slave_icw3);
    }

    #[test]
    fn irq_vectors_cover_0x20_to_0x2f_without_gaps() {
        let vectors: [Option<u8>; 16] = core::array::from_fn(|irq| irq_vector(irq as u8));
        let expected: [Option<u8>; 16] = core::array::from_fn(|i| Some(0x20 + i as u8));
        assert_eq!(vectors, expected);
        assert_eq!(irq_vector(16), None);
        assert_eq!(irq_vector(255), None);
    }

    #[test]
    fn unmasking_a_master_irq_touches_only_the_master() {
        assert_eq!(
            masks_with_irq_unmasked((MASK_ALL, MASK_ALL), 0),
            (0b1111_1110, MASK_ALL)
        );
        assert_eq!(
            masks_with_irq_unmasked((MASK_ALL, MASK_ALL), 1),
            (0b1111_1101, MASK_ALL)
        );
    }

    /// スレーブ側を開けるとき、マスタの IRQ2 も一緒に開くこと。
    ///
    /// ここを忘れると、スレーブ側で該当ビットを開けても IRQ8-15 が一切
    /// 届かない。しかも「マスクは外したのに来ない」という形で症状が出るため、
    /// PIC ではなくデバイス側を疑って時間を溶かしやすい。
    #[test]
    fn unmasking_a_slave_irq_also_opens_the_cascade_on_the_master() {
        // IRQ8（RTC）。スレーブの bit0 と、マスタの bit2 が開く。
        assert_eq!(
            masks_with_irq_unmasked((MASK_ALL, MASK_ALL), 8),
            (0b1111_1011, 0b1111_1110)
        );
        // IRQ12（PS/2 マウス）。スレーブの bit4 と、マスタの bit2。
        assert_eq!(
            masks_with_irq_unmasked((MASK_ALL, MASK_ALL), 12),
            (0b1111_1011, 0b1110_1111)
        );
    }

    #[test]
    fn unmasking_an_out_of_range_irq_changes_nothing() {
        assert_eq!(
            masks_with_irq_unmasked((MASK_ALL, MASK_ALL), 16),
            (MASK_ALL, MASK_ALL)
        );
    }

    #[test]
    fn only_irq7_and_irq15_can_be_spurious() {
        // ISR が全部 0（何も処理中でない）でも、7 / 15 以外は偽にならない。
        assert!(!is_spurious(0, (0, 0)));
        assert!(!is_spurious(1, (0, 0)));
        assert!(!is_spurious(8, (0, 0)));
    }

    #[test]
    fn a_real_irq7_has_its_bit_set_in_the_master_isr() {
        // bit7 が立っていれば本物。
        assert!(!is_spurious(7, (0b1000_0000, 0)));
        // 立っていなければ偽。
        assert!(is_spurious(7, (0b0000_0000, 0)));
        // 他のビットが立っていても、7 番目でなければ判定は変わらない。
        assert!(is_spurious(7, (0b0111_1111, 0)));
    }

    #[test]
    fn a_real_irq15_has_its_bit_set_in_the_slave_isr() {
        // スレーブ側の bit7（= IRQ15）を見る。マスタ側ではない。
        assert!(!is_spurious(15, (0, 0b1000_0000)));
        assert!(is_spurious(15, (0, 0b0000_0000)));
        // マスタ側に立っていてもスレーブ側が 0 なら偽。
        assert!(is_spurious(15, (0b1000_0000, 0b0000_0000)));
    }

    #[test]
    fn a_real_interrupt_gets_eoi_on_the_right_pics() {
        assert_eq!(eoi_action_for(0, false), EoiAction::MasterOnly);
        assert_eq!(eoi_action_for(7, false), EoiAction::MasterOnly);
        assert_eq!(eoi_action_for(8, false), EoiAction::SlaveThenMaster);
        assert_eq!(eoi_action_for(15, false), EoiAction::SlaveThenMaster);
        assert_eq!(eoi_action_for(16, false), EoiAction::None);
    }

    /// **スプリアスの扱いは非対称である。**
    ///
    /// IRQ7 の偽には何も送らない。IRQ15 の偽には**マスタにだけ**送る。
    /// マスタは IRQ2（カスケード）を本物として受けており、送らないと
    /// 処理中のまま残って以降マスタの全 IRQ が止まる。
    #[test]
    fn a_spurious_interrupt_is_acknowledged_asymmetrically() {
        assert_eq!(eoi_action_for(7, true), EoiAction::None);
        assert_eq!(eoi_action_for(15, true), EoiAction::MasterOnly);
        // 非対称であること自体を固定する。
        assert_ne!(eoi_action_for(7, true), eoi_action_for(15, true));
    }

    #[test]
    fn the_cascade_irq_belongs_to_the_master() {
        assert!(CASCADE_IRQ < IRQS_PER_PIC);
        assert_eq!(irq_vector(CASCADE_IRQ), Some(0x22));
    }
}
