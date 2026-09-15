//! Ring 3 への単発遠征と、予期した #GP の畳み（M5-e-3）。
//!
//! カーネル（Ring 0、メイン）が一時的に Ring 3 へ落ち、特権命令（`cli`）で
//! #GP を起こし、制御された形でカーネルへ戻る往復を 1 回だけ行う。ADR-0019 §1 の
//! 「Ring 3 に落ちて、特権命令で #GP になり、戻れる」を検証する。
//!
//! # 遷移の機構
//!
//! iretq 用の偽フレーム（SS/RSP/RFLAGS/CS/RIP）を積んで `iretq` する。CS/SS の
//! RPL=3 と DPL=3 により CPU が Ring 3 へ下りる。**long mode の iretq は特権変化の
//! 有無にかかわらず常に SS:RSP を pop する**ので、フレームのユーザー SS/RSP が
//! 使われて Ring 3 はユーザースタックで動く（M5-c/d のカーネルタスクが動くのも
//! 同じ理由で、そちらは正しいカーネル SS/RSP が pop されている）。
//!
//! # 戻り（畳み）の機構
//!
//! 例外ハンドラ（`exception_entry`）は `-> !` の fail-fast で、復元も iretq も
//! 持たない。そこを壊さずに戻るため、setjmp/longjmp 相当を使う。遠征に入る前に
//! callee-saved レジスタと RSP、復帰 RIP を [`RECOVERY`] へ保存し（setjmp 相当）、
//! #GP ハンドラが遠征中と判定したら [`RECOVERY`] から復元して復帰 RIP へ飛ぶ
//! （longjmp 相当）。例外スタブには一切触れない。
//!
//! # 判定と主張を分ける（S8-a）
//!
//! 畳んでよいかの判定は `exception_entry` が持ち、**どこで畳まれたかの主張は
//! 呼び出し側が持つ。** ハンドラはベクタ・フォルト RIP・CS・RSP を記録するだけで、
//! 予期した位置かどうかは見ない。**遠征ごとに予期する位置は違い、それは呼び出し側の
//! 知識だからである。** 分けておくと、遠征が増えても判定側を触らずに済む。
//!
//! # RSP0 の実利用
//!
//! Ring 3 の #GP は特権を上げる（3→0）ので、CPU は TSS.RSP0 のスタックへ
//! 切り替える。これが `set_rsp0`（M5-c で配線、M5-d でスイッチごとに更新、
//! M5-e-1 で TSS を新 index へ）の初めての実挙動での回収点である。遠征専用の
//! カーネルスタックを RSP0 に据えるのは、メイン（Ring 0）の休眠フレームを
//! ハンドラが踏み潰すのを避けるため（メインの Ring 0 連鎖が RSP0 スタック上に
//! 残るのは、実ユーザータスクと違ってこの遠征に固有の事情）。

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::gdt;

/// ユーザーコードの仮想アドレス（`PML4[USER_PML4_INDEX]` サブツリー。M5-e-2 が
/// 残した中間テーブルを再利用する）。`cli` 1 命令を置く。
pub const USER_CODE_VIRT: u64 = 0x0000_0080_0000_0000;
/// ユーザースタックの仮想アドレス（同サブツリー内、コードの 1 MiB 上）。
pub const USER_STACK_VIRT: u64 = 0x0000_0080_0010_0000;
/// 読み取り専用で張るユーザーページの仮想アドレス（S9-a）。
///
/// `map_4kib` の `writable: false` が実際に W=0 の葉を作ることを、**Ring 3 からの
/// 書き込みが #PF になる**ことで確かめるための的である。**カーネルからは書かない**
/// （BSP は `CR0.WP` が立っているので Ring 0 の書きも落ちる。それを主張しないのは
/// AP で WP が立っていないためで、`ActivePageTable::map_4kib` の doc に書いてある）。
pub const USER_READONLY_VIRT: u64 = 0x0000_0080_0000_3000;
/// ユーザースタックの上端（1 ページ）。iretq 偽フレームの RSP に使う。
pub const USER_STACK_TOP: u64 = USER_STACK_VIRT + 4096;

/// 遠征専用カーネルスタック 1 本の大きさ。#GP が RSP0 経由でここへ切り替わる。
///
/// # 64 KiB は実測で決めた（S11-5）
///
/// **16 KiB では足りず、静かに溢れた。** `spawn` が来るまで、このスタックに
/// 乗るのは `syscall_entry` と `dispatch`、あるいは畳みのハンドラだけだった。
/// **`spawn` は同じスタックの上でローダー一式を走らせる**——`load_user_program`・
/// `load_user_program_into`・`run_loaded_program`、そして
/// `UserProcess`（`FileTable` を含む）と `Quarantine` を抱える。
///
/// **溢れた先は静的領域で、[`ExcursionState::depth`] が壊れた**（実測。
/// `docs/troubleshooting.md`）。**メインのカーネルスタックと同じ 64 KiB にする**
/// ——起動時の `load_user_program` はあちらの上で問題なく走っており、
/// **同じ処理が乗るなら同じ大きさが要る**（`kernel/src/stack.rs`）。
///
/// **実際に使う量は毎起動測って判定行に出す**（[`fill_excursion_stack`] と
/// [`excursion_stack_high_water`]）。**推測ではなく観測で持つ。**
const EXCURSION_STACK_SIZE: usize = 64 * 1024;

/// 遠征スタックを埋める既知のバイト（S11-5）。
///
/// # なぜ 0 でも 0xFF でもないか
///
/// **どちらも普通に書かれる値である。** ゼロ埋めされた領域と区別できず、
/// **「使われた」と「元からそうだった」が混ざる。** ヒープの毒値（`0xDE`）と
/// 同じ考え方で、**偶然そうなる確率が低い値を選ぶ。**
const EXCURSION_STACK_FILL: u8 = 0xE5;

/// 溢れの検出に使う、スタック最下部の見張り区間のバイト数（S11-5）。
///
/// **ここが 1 バイトでも変われば、残りを使い切ったということである。**
/// **ガードページを張れないので**（`.bss` の配列であって、ページ境界に
/// 揃っていない）、**埋めた値で代替する。**
const EXCURSION_STACK_CANARY: usize = 256;

// フィールドは値としては読まず、静的領域のアドレスだけを取る（RSP0 用の
// スタック領域）。dead_code はそのための許容。
#[repr(align(16))]
#[allow(dead_code)]
struct ExcursionStack([u8; EXCURSION_STACK_SIZE]);

/// 遠征の入れ子の深さの上限（S11-2）。
///
/// # なぜ深さが要るのか
///
/// **「呼んだ側が待つ」形の `spawn` は、遠征の入れ子そのものである。**
/// 親の `int 0x80` の処理の中で子を Ring 3 で走らせ、子が終わったら親の続きへ戻る。
///
/// # なぜ 2 か
///
/// **実際の最大が 2 である**（S11-11 で構造が決まった）。**`init` はカーネル側に
/// 居て、深さを消費しない**——`kernel_main` から `spawn` を呼ぶ。
/// **シェルが深さ 1、シェルが起こす `ls`・`cat`・`hello` が深さ 2 である。**
///
/// **`init` を Ring 3 へ置くと足りない。** そちらは `init` が 1、シェルが 2、
/// シェルの子が 3 になる。**`init` をカーネル側に置いたのは、この深さのためである。**
///
/// **書き直した記録を残す（S11 の締め）。** ここにはかつて
/// **「見込みの最大は 2 である——`init` が子を 1 つ起こし、その子が終わるまで待つ。
/// 孫は今のところ要らない（シェルが子を起こすときは `init` が待っている親では
/// なくなる形も考えられるが、S11 の到達条件はそこまで要求しない）」**と書いてあった。
/// **その括弧の中で退けた形が、S11 が実際に作った形である。**
/// **上限 2 という結論は変わらなかったが、理由が別の構造を指していた。**
/// **「検査が前提を持ったまま古くなる」の doc 版である**
/// （`docs/verification-coverage.md`）——**あちらは壊していないのに落ちるので
/// 気づけるが、doc は落ちない。段閉じの `.rs` の grep が拾った。**
///
/// **固定配列の様式に合わせる**（`MAX_CPUS`・`WORKER_COUNT`・`MAX_OPEN_FILES`）。
/// **深さ 1 つにつき遠征スタックを [`EXCURSION_STACK_SIZE`] だけ静的に持つ**ので、
/// **増やすと `.bss` がそのぶん増える**（S10-a でガードページの位置が動いた件と同じ面）。
///
/// # 越えたらどうするか
///
/// **[`enter`] の契約である。** 呼び出し側が [`depth`] で確かめてから呼ぶ。
/// **越えて呼ぶと、上限を越えた添字で静的配列に触ることになるので、
/// 呼び出し側が防ぐ**（`spawn` は `-EAGAIN` を返す形になる）。
pub const MAX_EXCURSION_DEPTH: usize = 2;

/// 遠征専用のカーネルスタック（`.bss`）。IST スタックと同じ静的確保。
///
/// **深さごとに 1 本持つ（S11-2）。** 入れ子のとき、**子がカーネルへ入るときに
/// 親のスタックへ切り替わってはならない**——親はそのスタックの上で
/// `spawn` の処理をしている最中である。
/// **W1-a でスロットごとに分けた。** **W1-a の時点では [`RING3_SLOTS`] が 1 で、
/// 本数も置き場も変わらなかった**——**`.bss` は 1 バイトも増えなかった。**
/// **W1-c-1 で [`RING3_SLOTS`] を 2 にしたので、本数が倍になった。**
/// **2 つ目のスロットを使うのは、足した 1 本のタスクだけである**（W1-c-4。**`concurrent-test` の
/// 構成でだけ走る**。既定の起動では [`current_slot`] が必ず 0 を返す）。
static mut EXCURSION_STACKS: [[ExcursionStack; MAX_EXCURSION_DEPTH]; RING3_SLOTS] =
    [const { [const { ExcursionStack([0; EXCURSION_STACK_SIZE]) }; MAX_EXCURSION_DEPTH] };
        RING3_SLOTS];

/// setjmp/longjmp 相当の回復点。**フィールドのオフセットは `global_asm!` の
/// `[rax + N]` と一対一で対応している。** 並べ替えると asm が別の場所を読む。
#[repr(C)]
struct Recovery {
    rsp: u64,        // +0
    rbx: u64,        // +8
    rbp: u64,        // +16
    r12: u64,        // +24
    r13: u64,        // +32
    r14: u64,        // +40
    r15: u64,        // +48
    resume_rip: u64, // +56
}

/// 回復点。**深さごとに 1 つ持つ（S11-2）。**
///
/// **1 つしか無いと、子の遠征に入った時点で親の回復点が上書きされ、
/// 親が戻れなくなる。**
static mut RECOVERIES: [[Recovery; MAX_EXCURSION_DEPTH]; RING3_SLOTS] = [const {
    [const {
        Recovery {
            rsp: 0,
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            resume_rip: 0,
        }
    }; MAX_EXCURSION_DEPTH]
}; RING3_SLOTS];

/// 今使っている回復点の**アドレス**（S11-2）。
///
/// # なぜアドレスを持つのか。**asm に深さを渡さないため**
///
/// `zaytos_enter_ring3` と `zaytos_resume_from_ring3` は、かつて
/// `lea rax, [rip + RECOVERY]` で回復点を直に指していた。**深さで添字を引く形に
/// すると、asm が深さを読んで掛け算をすることになる。**
///
/// **代わりに、どの回復点を使うかを Rust が決めてここへ置く。**
/// asm は `mov rax, [rip + CURRENT_RECOVERY]` で読むだけになる。
/// **畳み（longjmp）が使うのも同じ値なので、入れ子でも取り違えない。**
static CURRENT_RECOVERY: AtomicU64 = AtomicU64::new(0);

/// Ring 3 へ降りられるタスクの本数（W1-a）。
///
/// # W1-c-1 で 2 にした
///
/// **W1-a の時点ではタスクが 4 本で、Ring 3 へ降りるのは `init` / シェルの系統だけ
/// だった**（デモのワーカー 2 本は降りない。実測。`docs/wayland-inventory.md`）。
/// **W1-c で 2 本を同時に走らせるので、もう 1 本ぶんを持つ**
/// （`task` の `TASK_COUNT` の末尾に足した 1 本と対になる）。
///
/// **W1-c-1 では 2 つ目を誰も使わない**——**スロットを引く口（[`current_slot`]）は、
/// W1-c-3 でタスクから引く形になった後も、W1-c-3c までは必ず 0 を返した。** **W1-c-4 から、
/// `concurrent-test` の構成の足した 1 本だけが 1 を返す。**
/// **大きさだけを変えた段である**（遠征スタックが 128 KiB 増える。あちらの表）。
pub const RING3_SLOTS: usize = 2;

/// 1 本の遠征が持つ状態のうち、**置き場を分けられるもの**（W1-a）。
///
/// # 「入れ替えるもの」とは分けてある
///
/// **`CURRENT_RECOVERY` と RSP0 はここに無い。** **どちらも「単一の既知の場所」で
/// なければならない**——**前者はアセンブラが `[rip + sym]` で読み**（実測。
/// [`zaytos_enter_ring3`] と [`zaytos_resume_from_ring3`]）、**後者は TSS の
/// 決まった欄である。** **あの 2 つはタスクが値を持ち、切り替えで入れ替える形になる
/// （W1-b）。**
///
/// # W1-a と W1-c-3 は振る舞いを変えない
///
/// **W1-a では [`RING3_SLOTS`] が 1 で、引く先は常に同じ 1 つだった。**
/// **W1-c-3 で引く先をタスクのスロットにし、畳みの記録もここへ移したが、
/// 既定の起動では常にスロット 0 である**（[`current_slot`]。**W1-c-4 の `concurrent-test` では
/// 足した 1 本がスロット 1 を引く**）。
/// **変わるのは「どこから引くか」だけで、値も順序も変わらない。**
struct ExcursionState {
    /// 今の遠征の深さ（S11-2）。**0 なら Ring 3 の遠征に入っていない。**
    depth: AtomicUsize,
    /// 今 Ring 3 にいるか（S8-b）。これが false のときの Ring 3 由来の例外は
    /// 「想定外」として畳まず halt する。
    ///
    /// # 「遠征中」から「今 Ring 3 にいる」へ広げた（S8-b）
    ///
    /// **かつては遠征の入口で立て、畳みで降ろすだけだった。** その意味だと
    /// `int 0x80` でカーネルへ入っている間も真のままになる。**カーネルの中にいるのに
    /// 「Ring 3 にいる」と読める状態は、畳む対象を4ベクタへ広げる S8-d で危うい。**
    /// そこで Ring 3 とカーネルの境をまたぐたびに上げ下げする。
    ///
    /// 上げ下げする点は3つある。
    ///
    /// - [`enter`] が iretq の直前で立てる
    /// - `exception_entry` が畳むと決めた時点で降ろす（[`record_and_fold`] が
    ///   [`leave_ring3`] を呼び、降ろすのはそちらである）
    /// - `syscall_entry` が入口で降ろし、Ring 3 へ返る直前で立て直す
    ///
    /// # 今のところ振る舞いは変わらない
    ///
    /// **畳みの判定は「CS.RPL==3」も見る**ので、カーネルの中で起きた例外は
    /// このフラグに関わらず弾かれる。**したがって S8-b は振る舞いを変えない。**
    /// 変えたのは、名前が指すものと実際の状態が一致することである。
    /// **2つの条件が独立に同じことを言う形にしておくと、片方を壊したときに
    /// もう片方が残る**（`Apic::is_spurious` が理由を2つ持つのと同じ形）。
    ///
    /// # 残る窓は2つ、どちらもカーネル側である
    ///
    /// 立ててから iretq するまでと、`syscall_entry` が立て直してから stub が
    /// iretq するまでは、**Ring 0 なのにフラグが真である。** どちらも CS.RPL=0 なので
    /// 畳みの判定には届かない。窓を閉じるには asm 側で上げ下げすることになるが、
    /// **判定が既に閉じているものを閉じるために asm を増やさない。**
    ///
    /// **失効条件——窓が無害なのは判定が CS.RPL を見ているからである。**
    /// **CS.RPL の条件を緩めるなら、この 2 つの窓を閉じることを再検討すること。**
    /// 緩めた瞬間、カーネルの中で起きた例外が畳まれうる。
    in_ring3: AtomicBool,
    /// 畳みが実際に起きたか（会計用。遠征後に true になっているはず）。
    folded: AtomicBool,
    /// 中断（Ctrl+C）で遠征を出たか（S12 前の手当て、C）。
    ///
    /// **`FOLDED` と分けてある。** あちらは「Ring 3 が例外を起こした」で、
    /// **こちらは「外から止めた」である。** 混ぜると、遠征から戻った側が
    /// **「子が落ちた」と「子を止めた」を区別できない。**
    interrupted: AtomicBool,
    /// 畳んだ例外のベクタ。ハンドラが記録する（W1-c-3 で大域の `FAULT_VECTOR` から移した）。
    fault_vector: AtomicU64,
    /// 畳んだ例外のフォルト RIP。ハンドラが記録する。
    ///
    /// **かつてはここに「予期する RIP」を据え、厳密一致を畳みの条件にしていた
    /// （M5-e-3 から S8-a まで）。** 判定から外して記録に変えたのは、S8 で畳む対象を
    /// 4 ベクタへ広げるためである。**遠征のたびに違う 1 点を予期するのは呼び出し側の
    /// 都合であって、畳んでよいかの条件ではない。** 呼び出し側が [`fault_rip`] を
    /// 読んで自分の予期と突き合わせる。
    fault_rip: AtomicU64,
    /// フォルト時の RSP（Ring 3 のユーザースタックのはず）。ハンドラが記録する。
    fault_rsp: AtomicU64,
    /// #GP ハンドラ自身の RSP（RSP0 = 遠征専用スタックのはず）。
    handler_rsp: AtomicU64,
    /// フォルト時の CS（Ring 3 由来なら RPL=3）。Ring 3 到達の実証に使う。
    fault_cs: AtomicU64,
    /// フォルト時の CR2（S8-d）。**#PF のときだけ意味を持つ。**
    ///
    /// 他のベクタでは直前の #PF の残骸か未定義の値なので、呼び出し側は
    /// ベクタが 14 のときだけ読むこと。**記録するだけで、ここでは出力しない。**
    fault_cr2: AtomicU64,
    /// フォルトのエラーコード（S8-e）。#PF では P/W/U のビットが「不在」と
    /// 「権限違反」を区別する——**S7 の到達条件 3 の観測はこの区別に依る**
    /// （カーネル VA への触りは P=1・U=1 の権限違反であって、穴ではない）。
    fault_error_code: AtomicU64,
}

impl ExcursionState {
    const fn new() -> Self {
        Self {
            depth: AtomicUsize::new(0),
            in_ring3: AtomicBool::new(false),
            folded: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            fault_vector: AtomicU64::new(0),
            fault_rip: AtomicU64::new(0),
            fault_rsp: AtomicU64::new(0),
            handler_rsp: AtomicU64::new(0),
            fault_cs: AtomicU64::new(0),
            fault_cr2: AtomicU64::new(0),
            fault_error_code: AtomicU64::new(0),
        }
    }
}

/// 遠征の状態、スロットごと（W1-a）。
static EXCURSION_STATE: [ExcursionState; RING3_SLOTS] =
    [const { ExcursionState::new() }; RING3_SLOTS];

/// 今のタスクの遠征の状態を引く（W1-a。W1-c-3 でタスクのスロットから引く形にした）。
///
/// **引く口をここ 1 つに絞ってある**（[`current_slot`]）。**既定の起動では必ずスロット 0 である**
/// （W1-c-4 の `concurrent-test` では足した 1 本がスロット 1 を引く）。
#[inline(always)]
fn state() -> &'static ExcursionState {
    &EXCURSION_STATE[current_slot()]
}

/// いま載っている回復点のアドレス（W1-b。切り替えが読む）。
///
/// **`CURRENT_RECOVERY` は単一の番地でなければならない**（`ADR-0060`）。
/// **切り替えはこれを控えて、入る側の値を載せる。**
pub fn current_recovery() -> u64 {
    CURRENT_RECOVERY.load(Ordering::SeqCst)
}

/// 回復点のアドレスを載せる（W1-b。切り替えが書く）。
///
/// # Safety の代わりに、呼べる場所を狭めてある
///
/// **`pub` だが、呼ぶのは `crate::task::schedule_switch` だけである**
/// （切り替えの割り込み禁止区間）。**遠征の出入りは [`enter`] の中で
/// 直に触る**——あちらは入れ子の控えと戻しを一続きで行う。
pub fn set_current_recovery(value: u64) {
    CURRENT_RECOVERY.store(value, Ordering::SeqCst);
}

/// 回復点の番地が、スロット `slot` の行の中か（W1-c-4）。**0 は「まだ誰も入っていない」である。**
///
/// # なぜ番地の属する場所で見るのか
///
/// **切り替えの後、asm が longjmp で使うのは [`CURRENT_RECOVERY`] が指す 1 箇所である。**
/// **入るタスクのスロットの行の外を指していたら、そのタスクが畳まれたときに別のタスクの回復点へ跳ぶ。**
///
/// **「入れ替えで書いた値が載ったか」を見る形にしない**——**同じ代入を 2 度読むだけになる**
/// （`0 と 0 を比べて通る` の族である。W1-c-3c）。**これは `stacks are mixed` と同じ形の検算で、
/// 入れ替えの実装とは独立な不変条件を見ている。**
pub fn recovery_belongs_to_slot(recovery: u64, slot: usize) -> bool {
    if recovery == 0 {
        return true;
    }
    if slot >= RING3_SLOTS {
        return false;
    }
    // SAFETY: 静的配列の行のアドレスを取るだけで、中身は読まない。
    let row = unsafe { addr_of!(RECOVERIES[slot]) } as u64;
    let size = (core::mem::size_of::<Recovery>() * MAX_EXCURSION_DEPTH) as u64;
    recovery >= row && recovery < row + size
}

/// 今のタスクのスロットの番号（W1-a。W1-c-3 でタスクから引く形にした）。
///
/// **W1-a から W1-c-2 までは定数 0 だった。** **W1-c-3 で `crate::task` から引く。**
/// **既定の起動では必ず 0 である**——**スロット 1 を使う足した 1 本は、`concurrent-test` の構成で
/// だけ走る**（W1-c-4。`task::current_ring3_slot` の doc）。
///
/// **W1-c-1 から `crate::userland` の `SPAWN_*` もこれで引く。** **W1-c-3 で `CURRENT_FILES`・
/// `CURRENT_HEAP`・`SPAWN_QUARANTINE` も加わった**（`ADR-0060` の Addendum）。
///
/// # `#[inline(never)]` にしてある（W1-c-3 で測って決めた）
///
/// **`#[inline(always)]` にすると、呼ぶ箇所ごとにタスクを引く処理が展開され、`dev` では
/// その一時値が呼んだ側の枠に場所を取った**（実測。`ring3::enter` の枠が 216 から 328 バイト）。
/// **呼ぶ箇所の枠には、呼び出しと戻り値だけが残る形にする。**
#[inline(never)]
pub(crate) fn current_slot() -> usize {
    // 破壊 (W1-c-4, ring3-slot-always-zero): 足した 1 本にもスロット 0 を返す。**2 本が同じ遠征の
    // 状態とスタックを使う。** **切り替えの検算は入る側のタスクから引く（`task::ring3_slot_of`）ので、
    // こちらだけを壊すと食い違いが出る。**
    #[cfg(feature = "ring3-slot-always-zero")]
    return 0;
    #[cfg(not(feature = "ring3-slot-always-zero"))]
    crate::task::current_ring3_slot()
}

// **畳みの記録（`FAULT_*` と `HANDLER_RSP`）は W1-c-3 で [`ExcursionState`] へ移した。**
// **2 本が同時に走ると、片方の畳みがもう片方の記録を上書きするためである。**

extern "C" {
    /// 偽フレームを積んで Ring 3 へ iretq する（setjmp 相当を内包）。畳みで
    /// 戻ってくると、あたかも通常に return したように呼び出し元へ戻る。
    ///
    /// **飛び先と Ring 3 のスタック上端は引数で受け取る**（S9-a）。RDI が
    /// `user_rip`、RSI が `user_stack_top` である（System V の第 1・第 2 引数）。
    fn zaytos_enter_ring3(user_rip: u64, user_stack_top: u64);
    /// [`RECOVERY`] から RSP と callee-saved を復元し、復帰 RIP へ飛ぶ
    /// （longjmp 相当）。戻らない。
    fn zaytos_resume_from_ring3() -> !;
}

// 遠征の遷移ルーチン（setjmp + iretq）。
//
// RECOVERY へ callee-saved と RSP、復帰ラベルを保存してから、iretq 偽フレームを
// 積んで Ring 3 へ落ちる。復帰ラベルへは畳み（zaytos_resume_from_ring3）だけが
// 飛んでくる。そこで ret すると呼び出し元へ戻る。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zaytos_enter_ring3",
    "zaytos_enter_ring3:",
    // setjmp 相当: callee-saved と RSP、復帰 RIP を保存する。
    // **回復点は深さごとに違う**ので、アドレスを Rust が置いた場所から読む（S11-2）。
    "  mov rax, [rip + {recovery_ptr}]",
    "  mov [rax + 0], rsp",
    "  mov [rax + 8], rbx",
    "  mov [rax + 16], rbp",
    "  mov [rax + 24], r12",
    "  mov [rax + 32], r13",
    "  mov [rax + 40], r14",
    "  mov [rax + 48], r15",
    "  lea rcx, [rip + 3f]",
    "  mov [rax + 56], rcx",
    // iretq 偽フレームを積む。pop 順は RIP,CS,RFLAGS,RSP,SS なので、push は
    // 逆順（SS を先＝高位、RIP を最後＝低位）。セレクタと RFLAGS は定数なので
    // mov 経由で積む。**飛び先とユーザー RSP は引数で受け取る**（RDI/RSI）。
    // 上の setjmp 相当が使うのは rax と rcx だけなので、RDI/RSI はここまで生きている。
    "  mov rax, {ss}",
    "  push rax",
    "  push rsi",
    "  mov rax, {rflags}",
    "  push rax",
    "  mov rax, {cs}",
    "  push rax",
    "  push rdi",
    "  iretq",
    // 復帰点（畳みだけがここへ来る。RSP と callee-saved は longjmp が復元済み）。
    "3:",
    "  ret",
    recovery_ptr = sym CURRENT_RECOVERY,
    ss = const gdt::USER_DATA_SELECTOR.bits() as u64,
    cs = const gdt::USER_CODE_SELECTOR.bits() as u64,
    rflags = const 0x202u64,
);

// 畳み（longjmp）。RECOVERY から RSP と callee-saved を復元し、復帰 RIP へ飛ぶ。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zaytos_resume_from_ring3",
    "zaytos_resume_from_ring3:",
    // **入った遠征と同じ回復点へ戻る**（S11-2）。
    "  mov rax, [rip + {recovery_ptr}]",
    "  mov rsp, [rax + 0]",
    "  mov rbx, [rax + 8]",
    "  mov rbp, [rax + 16]",
    "  mov r12, [rax + 24]",
    "  mov r13, [rax + 32]",
    "  mov r14, [rax + 40]",
    "  mov r15, [rax + 48]",
    "  mov rcx, [rax + 56]",
    "  jmp rcx",
    recovery_ptr = sym CURRENT_RECOVERY,
);

/// 遠征専用カーネルスタックの (下端, 上端)。RSP0 とハンドラ RSP の照合に使う。
pub fn excursion_stack_range() -> (u64, u64) {
    // **今いちばん内側の遠征のスタック。** 遠征に入っていなければ深さ 0 のもの
    // （かつての唯一のスタックと同じ）である。
    excursion_stack_range_at(state().depth.load(Ordering::SeqCst).saturating_sub(1))
}

/// 深さ `depth` の遠征スタックの (下端, 上端)（S11-2）。
///
/// 範囲外の `depth` は深さ 0 のものを返す。**呼び出し側が上限を知らなくてよい。**
#[inline(always)]
pub fn excursion_stack_range_at(depth: usize) -> (u64, u64) {
    excursion_stack_range_of(current_slot(), depth)
}

/// スロット `slot` の、深さ `depth` の遠征スタックの (下端, 上端)（W1-c-3）。
///
/// **切り替えが使う**——**見たいのは入る側のタスクのスロットで、今のタスクのスロットではない。**
/// 範囲外の `slot` と `depth` は 0 のものを返す（[`excursion_stack_range_at`] と同じ規則）。
pub fn excursion_stack_range_of(slot: usize, depth: usize) -> (u64, u64) {
    // **範囲外は止める（W1-c-3c）。** **以前は 0 のものを返していた**——**間違った添字が来ると、
    // スロット 0 の深さ 0（シェルのスタック）と重なっても誰も言わなかった**
    // （`docs/wayland-inventory.md` の「W1-c-4 で一斉に発火するもの」の #10）。
    if slot >= RING3_SLOTS || depth >= MAX_EXCURSION_DEPTH {
        report_excursion_index_out_of_range(slot, depth);
    }
    let index = depth;
    // SAFETY: 静的配列の要素のアドレスを取るだけで、中身は読まない。
    let bottom = unsafe { addr_of!(EXCURSION_STACKS[slot][index]) } as u64;
    (bottom, bottom + EXCURSION_STACK_SIZE as u64)
}

/// 深さ `depth` の遠征スタックを既知のバイトで埋める（S11-5）。
///
/// **これから使うスタックを埋めるのであって、今乗っているスタックではない。**
/// 深さ `d` の [`enter`] は深さ `d-1` のスタック（または メインのカーネルスタック）の
/// 上で走るので、**自分の足元を消すことにはならない。**
///
/// # Safety
///
/// `depth` が [`MAX_EXCURSION_DEPTH`] 未満で、そのスタックが今使われていないこと。
unsafe fn fill_excursion_stack(depth: usize) {
    // **範囲外は止める（W1-c-3c）。** **以前は黙って何もしなかった。**
    if depth >= MAX_EXCURSION_DEPTH {
        report_excursion_index_out_of_range(current_slot(), depth);
    }
    // SAFETY: 呼び出し元契約により、この配列要素は今誰も使っていない。
    unsafe {
        let stack = addr_of_mut!(EXCURSION_STACKS[current_slot()][depth]) as *mut u8;
        core::ptr::write_bytes(stack, EXCURSION_STACK_FILL, EXCURSION_STACK_SIZE);
    }
}

/// 深さ `depth` の遠征スタックで、実際に触られた最大バイト数（S11-5）。
///
/// **下から走査して、埋めた値でなくなる最初の位置を探す。**
/// そこから上端までが使われた量である。
///
/// **[`fill_excursion_stack`] を通っていないスタックについては意味を持たない**
/// （埋めていないので、走査は 0 バイト目で止まる）。
pub fn excursion_stack_high_water(depth: usize) -> usize {
    // **範囲外は止める（W1-c-3c）。** **以前は 0 を返していた**——**「0 バイト使った」と
    // 読める値を、使っていない添字について出していた。**
    if depth >= MAX_EXCURSION_DEPTH {
        report_excursion_index_out_of_range(current_slot(), depth);
    }
    // SAFETY: 読み取りのみ。添字は上で範囲内にしてある。
    let stack = unsafe { addr_of!(EXCURSION_STACKS[current_slot()][depth]) } as *const u8;
    for offset in 0..EXCURSION_STACK_SIZE {
        // SAFETY: offset は配列の中である。
        if unsafe { stack.add(offset).read_volatile() } != EXCURSION_STACK_FILL {
            return EXCURSION_STACK_SIZE - offset;
        }
    }
    0
}

/// 深さ `depth` の遠征スタックの見張り区間が無傷か（S11-5）。
///
/// **偽なら、そのスタックを使い切って下の静的領域まで書いた疑いがある。**
/// **溢れは静かに起きる**——このスタックにはガードページが無い
/// （[`EXCURSION_STACK_CANARY`]）。
pub fn excursion_stack_canary_intact(depth: usize) -> bool {
    excursion_stack_high_water(depth) <= EXCURSION_STACK_SIZE - EXCURSION_STACK_CANARY
}

/// 遠征スタック 1 本の容量（判定行に出す。S11-5）。
pub fn excursion_stack_capacity() -> usize {
    EXCURSION_STACK_SIZE
}

/// 深さ `depth` の遠征スタックの使用量が、容量の半分を越えていないか（S11-6）。
///
/// # なぜ半分で見るのか。**見張り区間では遅い**
///
/// [`excursion_stack_canary_intact`] が偽になるのは、**残り
/// [`EXCURSION_STACK_CANARY`] バイトまで使い切ったとき**である。
/// **そこまで来ていたら、判断する余地はもう無い。**
///
/// **半分は、`deferred-decisions.md` の「遠征スタックにガードページが無い」の
/// 解禁条件そのものである。** あの行は「使用量が容量の半分を超えたとき、または
/// 見張り区間が一度でも壊れたとき」と書いてある。**書いただけでは発火しないので、
/// 機械にする**（`install_kernel_stack_guard_page` が 2MiB を見つけたら止めるのと
/// 同じ形である。**あちらは M5-b で条件を書き、S11-5 で発火した**）。
///
/// # 越えたら止める
///
/// **まだ壊れていない。** それでも止めるのは、**越えた状態で先へ進むと、
/// 次に何かを足した人が「前から越えていた」ものとして扱うからである。**
/// **解禁条件は、発火した時点で判断を求めるためにある。**
pub fn excursion_stack_within_budget(depth: usize) -> bool {
    excursion_stack_high_water(depth) * 2 <= EXCURSION_STACK_SIZE
}

/// 今の遠征の深さ（S11-2）。**0 なら遠征に入っていない。**
///
/// **入れ子で呼ぶ側は、これで上限を確かめてから [`enter`] を呼ぶ。**
pub fn depth() -> usize {
    state().depth.load(Ordering::SeqCst)
}

/// Ring 3 へ 1 回遠征する。戻ってきたら（畳みで）会計を返す。
///
/// RSP0 を遠征専用スタックへ据え、遠征フラグを立て、iretq で Ring 3 へ落ちる。
/// Ring 3 が起こした例外を `exception_entry` が畳み、ここへ戻る。戻ったら RSP0 を
/// メインの上端へ戻す。
///
/// # 飛び先は呼び出し側が渡す（S9-a）
///
/// **`user_rip` と `user_stack_top` は引数である。** かつては `global_asm!` が
/// [`USER_CODE_VIRT`] と [`USER_STACK_TOP`] を `const` で埋めており、遠征は 1 か所へ
/// しか落ちられなかった。**ELF から読み込んだプログラムの入口へ落ちるには、
/// 飛び先を実行時に決められる必要がある。**
///
/// **この段では振る舞いを変えていない。** 呼び出し側は 4 か所とも従来と同じ
/// [`USER_CODE_VIRT`] と [`USER_STACK_TOP`] を渡す。変えたのは、値が固定である
/// ことをやめた点だけである。
///
/// **どこで畳まれたかは呼び出し側が主張する。** [`folded`]・[`fault_vector`]・
/// [`fault_rip`] を読み、自分が置いた命令の位置と突き合わせること。この関数は
/// 突き合わせない（遠征ごとに予期する位置が違い、それは呼び出し側の知識である）。
///
/// # ユーザーポインタの窓は引数である（S9-b-3-2b）
///
/// **遠征ごとに、その間だけ有効なユーザー VA の範囲が違う。** 起動時の検証は
/// 本番の空間のユーザーサブツリーを使い、ユーザープログラムは自分の空間の
/// サブツリーを使う。**遠征に入らないと Ring 3 は動かないので、ここで据えれば
/// 「窓を据えずにシステムコールが来る」形は作れない。**
///
/// 戻すのはこの関数である。**畳みで戻っても `exit` で戻っても同じ位置を通る。**
///
/// # AP をここへ通すときに、先に片づけるものがある（S11 の締めで移した）
///
/// **今この関数を呼ぶのは BSP だけである。** AP は
/// [`crate::smp`] の定常ループを回すだけで、Ring 3 へ入らない。
/// **この性質を根拠にしている記述が 2 つあるので、AP をここへ通す経路を
/// 作るときは、両方を先に読むこと。**
///
/// - **`ADR-0023` の S11-11 の Addendum。** 例外経路が BKL を取らない根拠が、
///   **「畳みが戻った先で触るものは per-CPU か、自前の排他を持つものだけである」**
///   に置き換わっている。**その「per-CPU」の側**（この関数の末尾が書く
///   [`ExcursionState::depth`]・[`CURRENT_RECOVERY`]・RSP0・ユーザー窓）は、
///   **書く者が常に 1 つであること**に依っている。**AP が入ると 2 つになる。**
/// - **`ADR-0027` の S8 の Addendum と `docs/roadmap.md` の S7 の到達条件 5**
///   （`AddressSpace` を破棄した後に、古い TLB でそこへ触れないことの直接の観測）。
///   **あれは「破棄した空間を別のコアがまだ見ている」状態を要求する。**
///   単一コアでは作れない——**破棄は CR3 の載せ替えを伴い、載せ替えは非グローバルの
///   TLB を全部落とす**（G ビットは立てておらず、起動時に検査している）。
///   **AP がユーザー空間でタスクを走らせるようになって初めて構成できる。**
///   **その段で、あの到達条件を果たすこと。**
///
/// **条件をここへ置いたのは、段の名前で書くと発火しないからである。**
/// あの到達条件は S7 から S8・S9・S11 へ段名で持ち越され、**どの段でも
/// 構成できないまま 4 回渡った**（`docs/coding-standards.md` の
/// 「段を閉じるときは、その段の名前で全 docs を grep する」が、
/// 段名で書いた条件について同じ形を 7 件記録している）。
/// **Ring 3 への入口は 1 つなので、ここに書けば通る者が必ず読む。**
///
/// # Safety
///
/// 呼び出し前に、`user_rip` と `user_stack_top` が `PML4` のユーザーサブツリーに
/// U=1 で張られており、`user_rip` に置いた命令列が必ずフォルトすること。
/// `user_stack_top` は 1 ページ内の上端で、Ring 3 が push できること。
/// `main_rsp0_top` が呼び出し元（メイン）のカーネルスタック上端で、遠征後に
/// RSP0 をそこへ戻せること。起動時の単一実行文脈から呼ぶこと。
pub unsafe fn enter(
    main_rsp0_top: u64,
    user_rip: u64,
    user_stack_top: u64,
    user_window: (u64, u64),
) {
    // **今のタスクの遠征の状態を 1 回だけ引く（W1-c-3）。** **引く箇所ごとに `state()` を
    // 呼ぶと、`dev` では呼んだ箇所の数だけ一時値が枠を広げた**（実測。この関数の枠が
    // 216 から 840 バイトになり、遠征スタックの高水位が動いた。`docs/coding-standards.md`
    // の「番地以外が動いたら」）。**この関数の枠は、子が走っている間ずっと載っている。**
    let state = state();
    // **この遠征の深さ（S11-2）。** 呼び出し側が [`depth`] で上限を確かめている。
    let depth = state.depth.load(Ordering::SeqCst);
    // **契約が破れていたら止める（W1-c-3c）。** **以前は回復点の添字を `depth.min(上限 - 1)` へ
    // 丸めていた**——**上限を越えて呼ばれると、親の回復点を黙って上書きした。**
    if depth >= MAX_EXCURSION_DEPTH {
        report_excursion_index_out_of_range(current_slot(), depth);
    }
    // **戻す RSP0 が 0 なら止める（W1-c-3c）。** **0 のまま遠征から戻ると、次に Ring 3 から
    // カーネルへ入るとき RSP0=0 の上に積む。** **W1-b でメインのタスクの `rsp0` を 0 のまま
    // 残した形の、Ring 3 へ降りる側の関所である**（`docs/wayland-inventory.md` の #2）。
    if main_rsp0_top == 0 {
        report_zero_rsp0_at_excursion_entry();
    }
    let (_, excursion_top) = excursion_stack_range_at(depth);

    // **この深さの回復点を据える。** 戻すのはこの関数の末尾である。
    // SAFETY: 静的配列の要素のアドレスを取るだけである。深さは上限未満（契約）。
    let slot = unsafe { addr_of_mut!(RECOVERIES[current_slot()][depth]) } as u64;
    let previous_recovery = CURRENT_RECOVERY.swap(slot, Ordering::SeqCst);
    state.depth.store(depth + 1, Ordering::SeqCst);

    // **この遠征の間、ユーザーポインタとして受理する範囲を据える（S9-b-3-2b）。**
    // 戻すのは畳みでも `exit` でも同じ位置（下の longjmp から戻った先）である。
    let previous_window = crate::syscall::set_user_window(user_window.0, user_window.1);

    state.folded.store(false, Ordering::SeqCst);
    // **中断の記録も戻す（S12 前の手当て、C）。** 戻さないと、前の遠征を
    // 止めたことが次の遠征の判定行に出る（`FAULT_CS` を戻していなかった
    // S9-b-3-1 とまったく同じ形である）。
    state.interrupted.store(false, Ordering::SeqCst);
    state.fault_rsp.store(0, Ordering::SeqCst);
    state.handler_rsp.store(0, Ordering::SeqCst);
    state.fault_vector.store(0, Ordering::SeqCst);
    state.fault_rip.store(0, Ordering::SeqCst);
    // **CS も戻す（S9-b-3-1）。** 戻していなかったので、畳まずに戻った遠征の
    // 判定行に**前の遠征の CS が出た。** 畳みで戻る遠征しか無かった間は誰も
    // 読まなかったが、`exit` で戻る経路ができて読まれるようになった。
    state.fault_cs.store(0, Ordering::SeqCst);
    state.fault_cr2.store(0, Ordering::SeqCst);
    state.fault_error_code.store(0, Ordering::SeqCst);

    // **使う前に既知のバイトで埋める（S11-5）。** 戻ってから走査して、
    // **実際に使った量と、見張り区間が無傷かを測る。**
    // **ガードページが無いスタックなので、溢れは静かに起きる**——
    // 実測で `EXCURSION_DEPTH` を壊した（`docs/troubleshooting.md`）。
    // SAFETY: このスタックはこれから使うもので、今は誰も乗っていない。
    unsafe { fill_excursion_stack(depth) };

    // **ここから `iretq` までを割り込み禁止にする（W1-c-3b）。**
    //
    // **下で TSS の RSP0 とタスクの欄（RSP0・深さ）を据えるが、据えてから `iretq` するまでは
    // カーネルスタックの上を走る。** **その間に切り替わると、欄は「遠征中」を言うのに
    // 保存 RSP はカーネルスタックに在り、戻ってくるときに TSS へ古い値を載せる。**
    // **深さ 0 の最初の遠征は IF=1 でここへ来る**（`init` が `sti` の後に最初に起こす 1 本）。
    //
    // **`InterruptGuard` は使えない。** **Ring 3 に居る間ずっと入れ子の深さが 1 のまま残り、
    // `on_timer_tick` が切り替えなくなる**——**目的そのものを壊す**（`ADR-0060` の W1-c-3 の Addendum）。
    // **IF を戻すのは `iretq` が積んだ RFLAGS（`0x202`。IF=1）である。**
    //
    // **埋める（`fill_excursion_stack`。64 KiB）より後に置く**——**長い書き込みを
    // 割り込み禁止の区間へ入れない。**
    // SAFETY: 割り込みを止めるだけで、メモリには触らない。この後で割り込みを許すのは
    // `zaytos_enter_ring3` の `iretq` だけで、そこまでに眠る処理も錠を取る処理も無い。
    unsafe { common::cpu::disable_interrupts() };

    // RSP0 を遠征専用スタックへ据える。#GP はここへ切り替わる。
    // 破壊 (M5-e-4, drop-rsp0): 据えない。#GP がメインのスタックへ切り替わり、
    // handler_in_excursion が false になって捕まる（M5-d の task-switch-drop-rsp0 は
    // schedule_switch 側で別物）。
    #[cfg(not(feature = "ring3-test-drop-rsp0"))]
    // SAFETY: excursion_top は静的な遠征スタックの上端。単一実行文脈。
    unsafe {
        gdt::set_rsp0(excursion_top);
    }
    // **タスクの欄にも残す（W1-b。`ADR-0060`）。** **TSS を書くのは「いま」で、
    // こちらは「次にこのタスクへ戻るとき、何を書くか」である。**
    // **切り替えがこの欄を読む。**
    #[cfg(not(feature = "ring3-test-drop-rsp0"))]
    crate::task::note_current_rsp0(excursion_top);
    // **深さの欄も据える（W1-b）。** **スタック混在の検査が、どのスタックを
    // 期待してよいかをこれで決める。**
    //
    // **欄は「入っている遠征の数」である（W1-c-3b で直した）。** **W1-b は入口で
    // 増やす前の数（`depth`）を控えていた**——**最初の遠征の最中も欄が 0 で、検査が
    // カーネルスタックを期待する食い違いがあった**（`ADR-0060` の W1-c-3 の Addendum の表）。
    // **出口は戻した数を控えるので、こちらだけを直せば揃う。**
    crate::task::note_current_excursion_depth(depth + 1);
    #[cfg(feature = "ring3-test-drop-rsp0")]
    let _ = excursion_top;

    // Ring 3 に入ることを記す（畳みの条件3）。iretq の直前で立てる。
    // 破壊 (M5-e-4, no-fold-flag): 立てない。cli の #GP が畳まれず dump+halt する。
    #[cfg(not(feature = "ring3-test-no-fold-flag"))]
    state.in_ring3.store(true, Ordering::SeqCst);

    // SAFETY: 偽フレームを積んで Ring 3 へ落ちる。ユーザーページは呼び出し側が
    // 張り済み。畳みで戻ってくる（callee-saved と RSP は longjmp が復元する）。
    unsafe {
        zaytos_enter_ring3(user_rip, user_stack_top);
    }

    // データセグメントを復元する。**iretq で Ring 3（低特権）へ落ちるとき、CPU は
    // DPL < CPL になった DS/ES/FS/GS を null 化し、#GP の特権変化で SS も null に
    // なる。** 畳みは iretq を経ない longjmp なので、これらは復元されない。
    // 64bit モードでは null セグメントでも実行は続くが、sti 前検査（ADR-0018 §2）が
    // DS/SS を実状態で照合するため、カーネルデータセレクタへ明示的に戻す。
    // SAFETY: KERNEL_DATA_SELECTOR は有効なカーネルデータセグメント。Ring 0 で
    // データセグメントを再ロードするだけ。
    unsafe {
        let sel = gdt::KERNEL_DATA_SELECTOR.bits() as u32;
        core::arch::asm!(
            "mov ds, {s:e}",
            "mov es, {s:e}",
            "mov ss, {s:e}",
            "mov fs, {s:e}",
            "mov gs, {s:e}",
            s = in(reg) sel,
            options(nostack, preserves_flags),
        );
    }

    // **ここへ戻った時点で IF=0 であることを確かめる（W1-c-3b）。**
    //
    // **戻る道は longjmp だけで**（`exit` は `syscall_entry`、畳みは例外と Ctrl+C の入口から）、
    // **どれも割り込みゲートを通って IF=0 になった文脈から跳ぶ。** **longjmp は RFLAGS を
    // 戻さない。** **したがって下で欄を戻し終えるまで、切り替えは入らない**——**出口の窓が
    // 閉じている根拠はこれだけなので、読みで済ませずに検算する。**
    if common::cpu::read_rflags() & RFLAGS_INTERRUPT_FLAG != 0 {
        report_resumed_with_interrupts_enabled();
    }

    // **深さと回復点を戻す（S11-2）。** 畳みで戻っても `exit` で戻ってもここを通る。
    state.depth.store(depth, Ordering::SeqCst);
    CURRENT_RECOVERY.store(previous_recovery, Ordering::SeqCst);

    // 畳みで戻った。RSP0 を呼び出し側が指定した上端へ戻す。
    // **入れ子のときは、親の遠征スタックの上端がそれである**（S11-2）——
    // 親はそのスタックの上で子を起こす処理をしている最中なので、
    // **メインの上端へ戻すと親のカーネルスタックが変わってしまう。**
    // SAFETY: main_rsp0_top は呼び出し元が使っているカーネルスタックの上端。
    unsafe {
        gdt::set_rsp0(main_rsp0_top);
    }
    // **タスクの欄も戻す（W1-b）。** **入れ子のときは親の遠征スタックの上端が
    // それである**——**上の `main_rsp0_top` と同じ値を入れる。**
    crate::task::note_current_rsp0(main_rsp0_top);
    // **深さの欄も戻す（W1-b）。** **入れ子なら親の遠征の数へ、そうでなければ 0 へ**
    // （**欄は「入っている遠征の数」である**。入口の注記）。
    crate::task::note_current_excursion_depth(state.depth.load(Ordering::SeqCst));

    // **窓を戻す（S9-b-3-2b）。** ここは畳みで戻った場合も `exit` で戻った場合も
    // 通る（どちらの longjmp も `zaytos_enter_ring3` の復帰点へ帰る）。
    crate::syscall::set_user_window(previous_window.0, previous_window.1);
}

/// 遠征スタックと回復点の添字が範囲外だった（W1-c-3c）。**止める。**
///
/// **別の関数にしてある理由**——**呼ぶ側の枠を広げないため**（`panic!` の一時値。
/// `ADR-0060` の W1-c-3 の Addendum の枠の表）。
#[inline(never)]
#[cold]
fn report_excursion_index_out_of_range(slot: usize, depth: usize) -> ! {
    panic!(
        "ring3: excursion slot {slot} / depth {depth} is out of range (RING3_SLOTS = {RING3_SLOTS}, \
         MAX_EXCURSION_DEPTH = {MAX_EXCURSION_DEPTH}); it used to be clamped to 0 silently (W1-c-3c)"
    );
}

/// 遠征の入口で、戻す RSP0 が 0 だった（W1-c-3c）。**止める。**
#[inline(never)]
#[cold]
fn report_zero_rsp0_at_excursion_entry() -> ! {
    panic!(
        "ring3: the RSP0 to restore after the excursion is 0; the next kernel entry from Ring 3 \
         would push onto address 0 (W1-c-3c)"
    );
}

/// RFLAGS の割り込み許可フラグ（IF。bit 9）。
const RFLAGS_INTERRUPT_FLAG: u64 = 1 << 9;

/// 遠征から IF=1 で戻ってきた（W1-c-3b）。**止める。**
///
/// **出口で欄を戻し終えるまでの窓が閉じているのは、戻った時点で IF=0 だからである**
/// （[`enter`] の注記）。**それが崩れたら、切り替えが欄と実物の食い違う瞬間に入りうる**
/// ——**静かに効く側なので止める。**
///
/// **別の関数にしてある理由**——**[`enter`] の枠を広げないため**（`panic!` の一時値。
/// `ADR-0060` の W1-c-3 の Addendum の枠の表）。
#[inline(never)]
#[cold]
fn report_resumed_with_interrupts_enabled() -> ! {
    panic!(
        "ring3: returned from an excursion with interrupts enabled; the window before the task's \
         RSP0 and depth fields are restored is open to a switch (W1-c-3b)"
    );
}

/// `exception_entry` が呼ぶ。今この例外を畳んでよいかを判定する。
///
/// 呼び出し側で「ベクタ==13」「CS.RPL==3」を確認済みで、ここでは今 Ring 3 に
/// いることを見る。**フォルト RIP は見ない**（[`FAULT_RIP`] の doc）。
pub fn should_fold() -> bool {
    state().in_ring3.load(Ordering::SeqCst)
}

/// Ring 3 からカーネルへ入ったことを記す（S8-b）。**入ってすぐに呼ぶこと。**
///
/// 呼ぶ前の値を返す。**Ring 3 から入ったのなら真のはず**なので、呼び出し側は
/// 記録して後から突き合わせられる。
///
/// 現在の呼び出し元は `syscall_entry` だけである。例外の側は
/// [`record_and_fold`] が同じことを行う（あちらは戻らないので分けてある）。
pub fn note_kernel_entry() -> bool {
    state().in_ring3.swap(false, Ordering::SeqCst)
}

/// Ring 3 へ返ることを記す（S8-b）。**iretq の直前で呼ぶこと。**
pub fn note_return_to_ring3() {
    state().in_ring3.store(true, Ordering::SeqCst);
}

/// Ring 3 由来の例外を畳む。ベクタ・フォルト RIP・CS・RSP とハンドラ RSP を記録し、
/// **[`leave_ring3`] で遠征の呼び出し元へ戻る。戻らない。**
///
/// **[`ExcursionState::in_ring3`] を降ろすのは [`leave_ring3`] の側である**（S9-b-3-1 で切り出した）。
/// ここが持つのは「畳みに固有の記録」だけである。
///
/// # Safety
///
/// [`should_fold`] とベクタ/CS.RPL の判別が全て真のときだけ呼ぶこと。
/// [`RECOVERY`] が [`enter`] で保存済みであること（遠征中なら必ずそう）。
pub unsafe fn record_and_fold(
    fault_vector: u64,
    fault_cs: u64,
    fault_rip: u64,
    fault_rsp: u64,
    fault_cr2: u64,
    fault_error_code: u64,
    handler_rsp: u64,
) -> ! {
    // **1 回だけ引く（W1-c-3。[`enter`] の同じ箇所の注記）。**
    let state = state();
    state.fault_vector.store(fault_vector, Ordering::SeqCst);
    state.fault_rip.store(fault_rip, Ordering::SeqCst);
    state.fault_cs.store(fault_cs, Ordering::SeqCst);
    state.fault_cr2.store(fault_cr2, Ordering::SeqCst);
    state
        .fault_error_code
        .store(fault_error_code, Ordering::SeqCst);
    state.fault_rsp.store(fault_rsp, Ordering::SeqCst);
    state.handler_rsp.store(handler_rsp, Ordering::SeqCst);
    state.folded.store(true, Ordering::SeqCst);
    // SAFETY: 呼び出し側契約により遠征中で、RECOVERY は保存済み。
    unsafe { leave_ring3() }
}

/// Ring 3 を出てカーネルへ戻る（S9-b-3-1）。**戻らない。**
///
/// [`ExcursionState::in_ring3`] を降ろし、longjmp で [`enter`] の呼び出し元へ帰る。
///
/// # 理由を問わない
///
/// **この関数は「なぜ Ring 3 を出るのか」を知らない。** 記録は呼び出し側が
/// 済ませてから来る。**畳み**（[`record_and_fold`]。ベクタ・RIP・CS・CR2 を
/// 記録する）と、**プロセスの終了**（S9-b-3-1 で足す。戻り値を記録する）が
/// 利用者である。
///
/// # なぜ切り出したか
///
/// **切り出した時点で利用者が 2 つある。** 1 つのときに切り出せば先回りだが、
/// 2 つ目が来た時点で切るのは「同じ変更を 2 度加えることになった」側である
/// （`map_4kib` と `map_user_4kib` の統合の合図に書いた形）。
///
/// **切らずに `record_and_fold` を使い回すと、名前と doc が意味の外へ伸びる。**
/// あれは「例外を畳む」関数で、**プロセスの終了は例外ではない。**
///
/// # Safety
///
/// [`RECOVERY`] が [`enter`] で保存済みであること（遠征中なら必ずそう）。
/// Ring 3 から入ったカーネル文脈から呼ぶこと。
pub unsafe fn leave_ring3() -> ! {
    state().in_ring3.store(false, Ordering::SeqCst);
    // SAFETY: 呼び出し側契約により RECOVERY は保存済み。longjmp は RSP と
    // callee-saved を復元して復帰 RIP へ飛ぶ。戻らない。
    unsafe { zaytos_resume_from_ring3() }
}

/// 畳みが起きたか（遠征後の会計）。
pub fn folded() -> bool {
    state().folded.load(Ordering::SeqCst)
}

/// 中断で出ることを記す（S12 前の手当て、C）。**`leave_ring3` の直前に呼ぶ。**
pub fn note_interrupted() {
    state().interrupted.store(true, Ordering::SeqCst);
}

/// 中断で出たか（遠征後の会計）。
pub fn interrupted() -> bool {
    state().interrupted.load(Ordering::SeqCst)
}

/// 畳みの記録ひとそろい（S11-5）。**入れ子の遠征をまたいで持ち出すためだけの型である。**
///
/// # なぜ要るのか
///
/// [`enter`] は入場時にこの一式を 0 へ戻し、**戻るときには復元しない。**
/// 遠征が 1 段だけの間はそれで正しかった——**次の遠征が始まるまで、
/// 誰も前の遠征の記録を必要としない。**
///
/// **`spawn` が入れ子を作ると、そうではなくなる。** 子の遠征が親の記録を
/// 0 で潰し、**親が畳んで終わったのか終了したのかを、親の判定行が言えなくなる。**
///
/// **`crate::vfs::swap_current_files` と `crate::syscall::set_user_window` と
/// 同じ形である**——遠征の前に控え、戻ったら戻す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRecord {
    folded: bool,
    /// 中断で出たか（S12 前の手当て、C）。**入れ子で持ち出すものに含める**——
    /// 含めないと、子を止めたことが親の判定行に出る。
    interrupted: bool,
    vector: u64,
    rip: u64,
    rsp: u64,
    handler_rsp: u64,
    cs: u64,
    cr2: u64,
    error_code: u64,
}

/// 今の畳みの記録を控える（S11-5）。
pub fn save_fold_record() -> FoldRecord {
    // **1 回だけ引く（W1-c-3。[`enter`] の同じ箇所の注記）。**
    let state = state();
    FoldRecord {
        folded: state.folded.load(Ordering::SeqCst),
        interrupted: state.interrupted.load(Ordering::SeqCst),
        vector: state.fault_vector.load(Ordering::SeqCst),
        rip: state.fault_rip.load(Ordering::SeqCst),
        rsp: state.fault_rsp.load(Ordering::SeqCst),
        handler_rsp: state.handler_rsp.load(Ordering::SeqCst),
        cs: state.fault_cs.load(Ordering::SeqCst),
        cr2: state.fault_cr2.load(Ordering::SeqCst),
        error_code: state.fault_error_code.load(Ordering::SeqCst),
    }
}

/// 控えた畳みの記録を戻す（S11-5）。
pub fn restore_fold_record(record: FoldRecord) {
    // **1 回だけ引く（W1-c-3。[`enter`] の同じ箇所の注記）。**
    let state = state();
    state.folded.store(record.folded, Ordering::SeqCst);
    state
        .interrupted
        .store(record.interrupted, Ordering::SeqCst);
    state.fault_vector.store(record.vector, Ordering::SeqCst);
    state.fault_rip.store(record.rip, Ordering::SeqCst);
    state.fault_rsp.store(record.rsp, Ordering::SeqCst);
    state
        .handler_rsp
        .store(record.handler_rsp, Ordering::SeqCst);
    state.fault_cs.store(record.cs, Ordering::SeqCst);
    state.fault_cr2.store(record.cr2, Ordering::SeqCst);
    state
        .fault_error_code
        .store(record.error_code, Ordering::SeqCst);
}

/// 記録したフォルト時 RSP（Ring 3 のユーザースタックのはず）。
pub fn fault_rsp() -> u64 {
    state().fault_rsp.load(Ordering::SeqCst)
}

/// 記録した #GP ハンドラの RSP（RSP0 = 遠征専用スタックのはず）。
pub fn handler_rsp() -> u64 {
    state().handler_rsp.load(Ordering::SeqCst)
}

/// 記録したフォルト時 CS（Ring 3 由来なら RPL=3）。
pub fn fault_cs() -> u64 {
    state().fault_cs.load(Ordering::SeqCst)
}

/// 畳んだ例外のベクタ。呼び出し側が予期と突き合わせる。
pub fn fault_vector() -> u64 {
    state().fault_vector.load(Ordering::SeqCst)
}

/// 畳んだ例外のフォルト RIP。呼び出し側が予期と突き合わせる。
pub fn fault_rip() -> u64 {
    state().fault_rip.load(Ordering::SeqCst)
}

/// 畳んだ例外のフォルト CR2（`ExcursionState::fault_cr2`）。**ベクタが 14 のときだけ読むこと。**
pub fn fault_cr2() -> u64 {
    state().fault_cr2.load(Ordering::SeqCst)
}

/// 畳んだ例外のエラーコード（`ExcursionState::fault_error_code`）。
pub fn fault_error_code() -> u64 {
    state().fault_error_code.load(Ordering::SeqCst)
}
