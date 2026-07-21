//! i8042 PS/2 コントローラ（M4-e）。
//!
//! **unsafe を含む。** I/O ポートを直接叩く。
//!
//! ## どこまで初期化するか
//!
//! **フル初期化はしない。** QEMU では OVMF が既に初期化しており（ブート
//! メニューでキーを使う）、何もしなくても動く可能性が高い。しかしそれは
//! 「こちらが確かめていない前提に乗る」ことであり、M4-c-3 の ICW2 と同じ
//! 構図になる。そこで**確かめられるものは確かめ、確かめられない診断は
//! 入れない**という線で切った。
//!
//! | 操作 | 採否 | 理由 |
//! |---|---|---|
//! | ステータスレジスタの確認 | 採用 | 読む前に OBF が立っているかを見る。立っていないのに 0x60 を読むと古い値かゴミを読む |
//! | 出力バッファのフラッシュ | 採用 | ファームウェアが残したバイトが最初のキー入力として現れる事故を防ぐ |
//! | コンフィグバイトの読み出しと検証 | 採用 | **スキャンコードセットの判定に直結する**（下記） |
//! | コントローラ自己テスト 0xAA | **却下** | 実機では成功時にコンフィグバイトがリセットされる個体があり復元処理が要る。その失敗経路を試す手段が無く、QEMU では必ず成功する。「テストできない対応コードを抱えない」（ADR-0013）に反する |
//! | インターフェーステスト 0xAB | **却下** | 同上。失敗時に何をすべきかを決められない |
//! | キーボードのリセット 0xFF / セット選択 0xF0 | **却下** | 翻訳が有効なら不要 |
//!
//! ## スキャンコードセットを仮定しない
//!
//! キーボードが出すのはセット 2 だが、コンフィグバイトの **bit6
//! （translation）が 1 ならコントローラがセット 1 へ翻訳する**。BIOS/UEFI は
//! 有効のまま渡すのが普通だが、**確かめる**。落ちていれば立てて書き戻し、
//! 読み直して一致を確認する。この検証によって、`decode` がセット 1 だけを
//! 扱えばよいことが保証される。

use common::port::{inb, io_wait, outb};

/// データポート。スキャンコードはここから読む。
const DATA_PORT: u16 = 0x60;
/// 読み出すとステータスレジスタ、書き込むとコマンドレジスタ。
const STATUS_COMMAND_PORT: u16 = 0x64;

/// ステータス bit0: 出力バッファに読むべきデータがある（OBF）。
pub const STATUS_OUTPUT_FULL: u8 = 1 << 0;
/// ステータス bit1: 入力バッファがまだ空いていない（IBF）。
const STATUS_INPUT_FULL: u8 = 1 << 1;

/// コマンド: コンフィグバイトを読む（結果はデータポートへ）。
const COMMAND_READ_CONFIG: u8 = 0x20;
/// コマンド: コンフィグバイトを書く（続けてデータポートへ値を出す）。
const COMMAND_WRITE_CONFIG: u8 = 0x60;

/// コンフィグ bit0: 第 1 ポート（キーボード）の割り込みを有効にする。
pub const CONFIG_KEYBOARD_INTERRUPT: u8 = 1 << 0;
/// コンフィグ bit6: スキャンコードをセット 1 へ翻訳する。
pub const CONFIG_TRANSLATION: u8 = 1 << 6;

/// ポーリングの上限回数。
///
/// **上限のない待機ループを書かない。** コントローラが応答しない場合に
/// 無限に回ると、ハングと区別がつかなくなる。回数で打ち切り、呼び出し側が
/// fail-fast できるよう [`ControllerError`] を返す。
const POLL_LIMIT: usize = 10_000;

/// 出力バッファのフラッシュで読み捨てる上限。
///
/// 正常なら数バイトで空になる。上限に達したらコントローラが壊れているか、
/// 誰かがキーを押しっぱなしにしている。
const DRAIN_LIMIT: usize = 32;

/// フラッシュ時に「まだ出てこないか」を見張る回数。
///
/// **瞬間の OBF だけを見ても足りない。** 実際、その場では空でも、IRQ1 の
/// マスクを外した直後に 1 バイト届く事象が観測された（ファームウェアが
/// 残した入力が、コントローラの内部段からわずかに遅れて出力バッファへ
/// 上がってくる）。読み捨てられずに残ると、それが最初のキー入力として
/// 現れてしまう。少しのあいだ待って、出てきた分も捨てる。
const DRAIN_SETTLE_ITERATIONS: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerError {
    /// 入力バッファが空かず、コマンドを送れなかった。
    InputBufferStuck,
    /// 応答が返ってこなかった。
    NoResponse,
    /// コンフィグバイトを書き戻したが、読み直した値が一致しなかった。
    ConfigMismatch { wrote: u8, read_back: u8 },
}

/// ステータスレジスタを読む。副作用は無い。
pub fn status() -> u8 {
    // SAFETY: 0x64 の読み出しは i8042 のステータスを返すだけで状態を変えない。
    unsafe { inb(STATUS_COMMAND_PORT) }
}

/// 出力バッファに読むべきデータがあるか。
pub fn output_buffer_full() -> bool {
    status() & STATUS_OUTPUT_FULL != 0
}

/// データポートから 1 バイト読む。
///
/// **ステータスを確認せずに読む。** 割り込みハンドラから呼ぶ場合、IRQ1 が
/// 上がっている時点で OBF は立っているためである。**ハンドラは必ずこれを
/// 呼んで出力バッファを空にしなければならない。** 空にしないとコントローラは
/// 次の IRQ1 を上げず、「1 回だけ動いて止まる」症状になる。
///
/// # Safety
///
/// 読み出しは出力バッファを消費する副作用を持つ。同じバイトを 2 回読むことは
/// できないので、呼び出し側は読んだ値を必ず処理すること。
pub unsafe fn read_data() -> u8 {
    // SAFETY: 0x60 は i8042 のデータポート。読み出しで 1 バイト消費する。
    unsafe { inb(DATA_PORT) }
}

/// 入力バッファが空くまで待つ（上限つき）。
fn wait_input_clear() -> Result<(), ControllerError> {
    for _ in 0..POLL_LIMIT {
        if status() & STATUS_INPUT_FULL == 0 {
            return Ok(());
        }
        io_wait();
    }
    Err(ControllerError::InputBufferStuck)
}

/// 出力バッファにデータが来るまで待つ（上限つき）。
fn wait_output_ready() -> Result<(), ControllerError> {
    for _ in 0..POLL_LIMIT {
        if output_buffer_full() {
            return Ok(());
        }
        io_wait();
    }
    Err(ControllerError::NoResponse)
}

/// コンフィグバイトを読む。
///
/// # Safety
///
/// コントローラへコマンドを送る。他の実行文脈が同時に i8042 を触っていない
/// こと。起動シーケンス中（IRQ1 マスク中）に呼ぶこと。
pub unsafe fn read_config() -> Result<u8, ControllerError> {
    wait_input_clear()?;
    // SAFETY: 0x64 へのコマンド書き込み。呼び出し側が排他を保証する契約。
    unsafe {
        outb(STATUS_COMMAND_PORT, COMMAND_READ_CONFIG);
    }
    wait_output_ready()?;
    // SAFETY: 直前に OBF が立つのを確認した。
    Ok(unsafe { read_data() })
}

/// コンフィグバイトを書き、**読み直して一致を確認する**。
///
/// 「設定したつもり」で済ませない（本プロジェクトの作法）。
///
/// # Safety
///
/// [`read_config`] と同じ。
pub unsafe fn write_config(value: u8) -> Result<(), ControllerError> {
    wait_input_clear()?;
    // SAFETY: コマンドを送ってから値を送る、i8042 の規定の手順。
    unsafe {
        outb(STATUS_COMMAND_PORT, COMMAND_WRITE_CONFIG);
    }
    wait_input_clear()?;
    // SAFETY: 入力バッファが空いたことを確認済み。
    unsafe {
        outb(DATA_PORT, value);
    }

    // SAFETY: 書いた直後に読み直す。
    let read_back = unsafe { read_config()? };
    if read_back != value {
        return Err(ControllerError::ConfigMismatch {
            wrote: value,
            read_back,
        });
    }
    Ok(())
}

/// 出力バッファに残っているバイトを読み捨てる。捨てた個数を返す。
///
/// **IRQ1 のマスクを解除する前に必ず呼ぶ。** ファームウェアが残したバイトが
/// 最初のキー入力として現れる事故を防ぐ。実際 OVMF はブートメニューで
/// キーを扱っており、何か残っていてもおかしくない。
///
/// # Safety
///
/// 出力バッファを消費する。IRQ1 がマスクされている間に呼ぶこと。
pub unsafe fn drain_output_buffer() -> usize {
    let mut discarded = 0;
    // 瞬間の OBF だけでなく、少しのあいだ見張る（DRAIN_SETTLE_ITERATIONS の
    // コメント参照）。上限つきなので、応答しないコントローラでも止まらない。
    for _ in 0..DRAIN_SETTLE_ITERATIONS {
        if output_buffer_full() {
            // SAFETY: OBF が立っていることを直前に確認した。
            unsafe {
                read_data();
            }
            discarded += 1;
            if discarded >= DRAIN_LIMIT {
                break;
            }
        }
        io_wait();
    }
    discarded
}

/// コンフィグバイトに必要なビットを立てた値を返す（純粋ロジック）。
///
/// 立てるのは 2 つだけで、**他のビットは触らない**。ファームウェアが設定した
/// 内容（第 2 ポートの有効/無効など）を意図せず変えないため。
pub const fn config_with_keyboard_enabled(current: u8) -> u8 {
    current | CONFIG_KEYBOARD_INTERRUPT | CONFIG_TRANSLATION
}

/// コンフィグバイトが必要な状態になっているか（純粋ロジック）。
pub const fn config_is_ready(config: u8) -> bool {
    config & CONFIG_KEYBOARD_INTERRUPT != 0 && config & CONFIG_TRANSLATION != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_required_bits_are_added_without_touching_the_others() {
        // 第 2 ポート有効(bit1)とクロック関係(bit4,5)が立っている状態を想定。
        let current = 0b0011_0010;
        let updated = config_with_keyboard_enabled(current);
        assert_eq!(updated & CONFIG_KEYBOARD_INTERRUPT, CONFIG_KEYBOARD_INTERRUPT);
        assert_eq!(updated & CONFIG_TRANSLATION, CONFIG_TRANSLATION);
        // 元から立っていたビットが残っていること。
        assert_eq!(updated & 0b0011_0010, 0b0011_0010);
    }

    #[test]
    fn already_correct_config_is_left_alone() {
        let current = CONFIG_KEYBOARD_INTERRUPT | CONFIG_TRANSLATION;
        assert_eq!(config_with_keyboard_enabled(current), current);
        assert!(config_is_ready(current));
    }

    /// 翻訳が落ちているとセット 2 が届き、`decode` の前提が崩れる。
    #[test]
    fn a_config_without_translation_is_not_ready() {
        assert!(!config_is_ready(CONFIG_KEYBOARD_INTERRUPT));
    }

    /// 割り込みが無効なら、マスクを外しても IRQ1 は上がらない。
    #[test]
    fn a_config_without_the_keyboard_interrupt_is_not_ready() {
        assert!(!config_is_ready(CONFIG_TRANSLATION));
    }
}
