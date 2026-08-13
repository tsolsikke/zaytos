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
/// **溢れた先は静的領域で、[`EXCURSION_DEPTH`] が壊れた**（実測。
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
/// **見込みの最大は 2 である**——`init` が子を 1 つ起こし、その子が終わるまで待つ。
/// **孫は今のところ要らない**（シェルが子を起こすときは `init` が待っている親では
/// なくなる形も考えられるが、S11 の到達条件はそこまで要求しない）。
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
static mut EXCURSION_STACKS: [ExcursionStack; MAX_EXCURSION_DEPTH] =
    [const { ExcursionStack([0; EXCURSION_STACK_SIZE]) }; MAX_EXCURSION_DEPTH];

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
static mut RECOVERIES: [Recovery; MAX_EXCURSION_DEPTH] = [const {
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
}; MAX_EXCURSION_DEPTH];

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

/// 今の遠征の深さ（S11-2）。**0 なら Ring 3 の遠征に入っていない。**
static EXCURSION_DEPTH: AtomicUsize = AtomicUsize::new(0);

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
static IN_RING3: AtomicBool = AtomicBool::new(false);
/// 畳みが実際に起きたか（会計用。遠征後に true になっているはず）。
static FOLDED: AtomicBool = AtomicBool::new(false);
/// 畳んだ例外のベクタ。ハンドラが記録する。
static FAULT_VECTOR: AtomicU64 = AtomicU64::new(0);
/// 畳んだ例外のフォルト RIP。ハンドラが記録する。
///
/// **かつてはここに「予期する RIP」を据え、厳密一致を畳みの条件にしていた
/// （M5-e-3 から S8-a まで）。** 判定から外して記録に変えたのは、S8 で畳む対象を
/// 4 ベクタへ広げるためである。**遠征のたびに違う 1 点を予期するのは呼び出し側の
/// 都合であって、畳んでよいかの条件ではない。** 呼び出し側が [`fault_rip`] を
/// 読んで自分の予期と突き合わせる。
static FAULT_RIP: AtomicU64 = AtomicU64::new(0);
/// フォルト時の RSP（Ring 3 のユーザースタックのはず）。ハンドラが記録する。
static FAULT_RSP: AtomicU64 = AtomicU64::new(0);
/// #GP ハンドラ自身の RSP（RSP0 = 遠征専用スタックのはず）。
static HANDLER_RSP: AtomicU64 = AtomicU64::new(0);
/// フォルト時の CS（Ring 3 由来なら RPL=3）。Ring 3 到達の実証に使う。
static FAULT_CS: AtomicU64 = AtomicU64::new(0);
/// フォルト時の CR2（S8-d）。**#PF のときだけ意味を持つ。**
///
/// 他のベクタでは直前の #PF の残骸か未定義の値なので、呼び出し側は
/// ベクタが 14 のときだけ読むこと。**記録するだけで、ここでは出力しない。**
static FAULT_CR2: AtomicU64 = AtomicU64::new(0);
/// フォルトのエラーコード（S8-e）。#PF では P/W/U のビットが「不在」と
/// 「権限違反」を区別する——**S7 の到達条件 3 の観測はこの区別に依る**
/// （カーネル VA への触りは P=1・U=1 の権限違反であって、穴ではない）。
static FAULT_ERROR_CODE: AtomicU64 = AtomicU64::new(0);

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
    excursion_stack_range_at(EXCURSION_DEPTH.load(Ordering::SeqCst).saturating_sub(1))
}

/// 深さ `depth` の遠征スタックの (下端, 上端)（S11-2）。
///
/// 範囲外の `depth` は深さ 0 のものを返す。**呼び出し側が上限を知らなくてよい。**
pub fn excursion_stack_range_at(depth: usize) -> (u64, u64) {
    let index = if depth < MAX_EXCURSION_DEPTH {
        depth
    } else {
        0
    };
    // SAFETY: 静的配列の要素のアドレスを取るだけで、中身は読まない。
    let bottom = unsafe { addr_of!(EXCURSION_STACKS[index]) } as u64;
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
    if depth >= MAX_EXCURSION_DEPTH {
        return;
    }
    // SAFETY: 呼び出し元契約により、この配列要素は今誰も使っていない。
    unsafe {
        let stack = addr_of_mut!(EXCURSION_STACKS[depth]) as *mut u8;
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
    if depth >= MAX_EXCURSION_DEPTH {
        return 0;
    }
    // SAFETY: 読み取りのみ。添字は上で範囲内にしてある。
    let stack = unsafe { addr_of!(EXCURSION_STACKS[depth]) } as *const u8;
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
    EXCURSION_DEPTH.load(Ordering::SeqCst)
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
    // **この遠征の深さ（S11-2）。** 呼び出し側が [`depth`] で上限を確かめている。
    let depth = EXCURSION_DEPTH.load(Ordering::SeqCst);
    let (_, excursion_top) = excursion_stack_range_at(depth);

    // **この深さの回復点を据える。** 戻すのはこの関数の末尾である。
    // SAFETY: 静的配列の要素のアドレスを取るだけである。深さは上限未満（契約）。
    let slot = unsafe { addr_of_mut!(RECOVERIES[depth.min(MAX_EXCURSION_DEPTH - 1)]) } as u64;
    let previous_recovery = CURRENT_RECOVERY.swap(slot, Ordering::SeqCst);
    EXCURSION_DEPTH.store(depth + 1, Ordering::SeqCst);

    // **この遠征の間、ユーザーポインタとして受理する範囲を据える（S9-b-3-2b）。**
    // 戻すのは畳みでも `exit` でも同じ位置（下の longjmp から戻った先）である。
    let previous_window = crate::syscall::set_user_window(user_window.0, user_window.1);

    FOLDED.store(false, Ordering::SeqCst);
    FAULT_RSP.store(0, Ordering::SeqCst);
    HANDLER_RSP.store(0, Ordering::SeqCst);
    FAULT_VECTOR.store(0, Ordering::SeqCst);
    FAULT_RIP.store(0, Ordering::SeqCst);
    // **CS も戻す（S9-b-3-1）。** 戻していなかったので、畳まずに戻った遠征の
    // 判定行に**前の遠征の CS が出た。** 畳みで戻る遠征しか無かった間は誰も
    // 読まなかったが、`exit` で戻る経路ができて読まれるようになった。
    FAULT_CS.store(0, Ordering::SeqCst);
    FAULT_CR2.store(0, Ordering::SeqCst);
    FAULT_ERROR_CODE.store(0, Ordering::SeqCst);

    // **使う前に既知のバイトで埋める（S11-5）。** 戻ってから走査して、
    // **実際に使った量と、見張り区間が無傷かを測る。**
    // **ガードページが無いスタックなので、溢れは静かに起きる**——
    // 実測で `EXCURSION_DEPTH` を壊した（`docs/troubleshooting.md`）。
    // SAFETY: このスタックはこれから使うもので、今は誰も乗っていない。
    unsafe { fill_excursion_stack(depth) };

    // RSP0 を遠征専用スタックへ据える。#GP はここへ切り替わる。
    // 破壊 (M5-e-4, drop-rsp0): 据えない。#GP がメインのスタックへ切り替わり、
    // handler_in_excursion が false になって捕まる（M5-d の task-switch-drop-rsp0 は
    // schedule_switch 側で別物）。
    #[cfg(not(feature = "ring3-test-drop-rsp0"))]
    // SAFETY: excursion_top は静的な遠征スタックの上端。単一実行文脈。
    unsafe {
        gdt::set_rsp0(excursion_top);
    }
    #[cfg(feature = "ring3-test-drop-rsp0")]
    let _ = excursion_top;

    // Ring 3 に入ることを記す（畳みの条件3）。iretq の直前で立てる。
    // 破壊 (M5-e-4, no-fold-flag): 立てない。cli の #GP が畳まれず dump+halt する。
    #[cfg(not(feature = "ring3-test-no-fold-flag"))]
    IN_RING3.store(true, Ordering::SeqCst);

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

    // **深さと回復点を戻す（S11-2）。** 畳みで戻っても `exit` で戻ってもここを通る。
    EXCURSION_DEPTH.store(depth, Ordering::SeqCst);
    CURRENT_RECOVERY.store(previous_recovery, Ordering::SeqCst);

    // 畳みで戻った。RSP0 を呼び出し側が指定した上端へ戻す。
    // **入れ子のときは、親の遠征スタックの上端がそれである**（S11-2）——
    // 親はそのスタックの上で子を起こす処理をしている最中なので、
    // **メインの上端へ戻すと親のカーネルスタックが変わってしまう。**
    // SAFETY: main_rsp0_top は呼び出し元が使っているカーネルスタックの上端。
    unsafe {
        gdt::set_rsp0(main_rsp0_top);
    }

    // **窓を戻す（S9-b-3-2b）。** ここは畳みで戻った場合も `exit` で戻った場合も
    // 通る（どちらの longjmp も `zaytos_enter_ring3` の復帰点へ帰る）。
    crate::syscall::set_user_window(previous_window.0, previous_window.1);
}

/// `exception_entry` が呼ぶ。今この例外を畳んでよいかを判定する。
///
/// 呼び出し側で「ベクタ==13」「CS.RPL==3」を確認済みで、ここでは今 Ring 3 に
/// いることを見る。**フォルト RIP は見ない**（[`FAULT_RIP`] の doc）。
pub fn should_fold() -> bool {
    IN_RING3.load(Ordering::SeqCst)
}

/// Ring 3 からカーネルへ入ったことを記す（S8-b）。**入ってすぐに呼ぶこと。**
///
/// 呼ぶ前の値を返す。**Ring 3 から入ったのなら真のはず**なので、呼び出し側は
/// 記録して後から突き合わせられる。
///
/// 現在の呼び出し元は `syscall_entry` だけである。例外の側は
/// [`record_and_fold`] が同じことを行う（あちらは戻らないので分けてある）。
pub fn note_kernel_entry() -> bool {
    IN_RING3.swap(false, Ordering::SeqCst)
}

/// Ring 3 へ返ることを記す（S8-b）。**iretq の直前で呼ぶこと。**
pub fn note_return_to_ring3() {
    IN_RING3.store(true, Ordering::SeqCst);
}

/// Ring 3 由来の例外を畳む。ベクタ・フォルト RIP・CS・RSP とハンドラ RSP を記録し、
/// **[`leave_ring3`] で遠征の呼び出し元へ戻る。戻らない。**
///
/// **[`IN_RING3`] を降ろすのは [`leave_ring3`] の側である**（S9-b-3-1 で切り出した）。
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
    FAULT_VECTOR.store(fault_vector, Ordering::SeqCst);
    FAULT_RIP.store(fault_rip, Ordering::SeqCst);
    FAULT_CS.store(fault_cs, Ordering::SeqCst);
    FAULT_CR2.store(fault_cr2, Ordering::SeqCst);
    FAULT_ERROR_CODE.store(fault_error_code, Ordering::SeqCst);
    FAULT_RSP.store(fault_rsp, Ordering::SeqCst);
    HANDLER_RSP.store(handler_rsp, Ordering::SeqCst);
    FOLDED.store(true, Ordering::SeqCst);
    // SAFETY: 呼び出し側契約により遠征中で、RECOVERY は保存済み。
    unsafe { leave_ring3() }
}

/// Ring 3 を出てカーネルへ戻る（S9-b-3-1）。**戻らない。**
///
/// [`IN_RING3`] を降ろし、longjmp で [`enter`] の呼び出し元へ帰る。
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
    IN_RING3.store(false, Ordering::SeqCst);
    // SAFETY: 呼び出し側契約により RECOVERY は保存済み。longjmp は RSP と
    // callee-saved を復元して復帰 RIP へ飛ぶ。戻らない。
    unsafe { zaytos_resume_from_ring3() }
}

/// 畳みが起きたか（遠征後の会計）。
pub fn folded() -> bool {
    FOLDED.load(Ordering::SeqCst)
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
    FoldRecord {
        folded: FOLDED.load(Ordering::SeqCst),
        vector: FAULT_VECTOR.load(Ordering::SeqCst),
        rip: FAULT_RIP.load(Ordering::SeqCst),
        rsp: FAULT_RSP.load(Ordering::SeqCst),
        handler_rsp: HANDLER_RSP.load(Ordering::SeqCst),
        cs: FAULT_CS.load(Ordering::SeqCst),
        cr2: FAULT_CR2.load(Ordering::SeqCst),
        error_code: FAULT_ERROR_CODE.load(Ordering::SeqCst),
    }
}

/// 控えた畳みの記録を戻す（S11-5）。
pub fn restore_fold_record(record: FoldRecord) {
    FOLDED.store(record.folded, Ordering::SeqCst);
    FAULT_VECTOR.store(record.vector, Ordering::SeqCst);
    FAULT_RIP.store(record.rip, Ordering::SeqCst);
    FAULT_RSP.store(record.rsp, Ordering::SeqCst);
    HANDLER_RSP.store(record.handler_rsp, Ordering::SeqCst);
    FAULT_CS.store(record.cs, Ordering::SeqCst);
    FAULT_CR2.store(record.cr2, Ordering::SeqCst);
    FAULT_ERROR_CODE.store(record.error_code, Ordering::SeqCst);
}

/// 記録したフォルト時 RSP（Ring 3 のユーザースタックのはず）。
pub fn fault_rsp() -> u64 {
    FAULT_RSP.load(Ordering::SeqCst)
}

/// 記録した #GP ハンドラの RSP（RSP0 = 遠征専用スタックのはず）。
pub fn handler_rsp() -> u64 {
    HANDLER_RSP.load(Ordering::SeqCst)
}

/// 記録したフォルト時 CS（Ring 3 由来なら RPL=3）。
pub fn fault_cs() -> u64 {
    FAULT_CS.load(Ordering::SeqCst)
}

/// 畳んだ例外のベクタ。呼び出し側が予期と突き合わせる。
pub fn fault_vector() -> u64 {
    FAULT_VECTOR.load(Ordering::SeqCst)
}

/// 畳んだ例外のフォルト RIP。呼び出し側が予期と突き合わせる。
pub fn fault_rip() -> u64 {
    FAULT_RIP.load(Ordering::SeqCst)
}

/// 畳んだ例外のフォルト CR2（[`FAULT_CR2`]）。**ベクタが 14 のときだけ読むこと。**
pub fn fault_cr2() -> u64 {
    FAULT_CR2.load(Ordering::SeqCst)
}

/// 畳んだ例外のエラーコード（[`FAULT_ERROR_CODE`]）。
pub fn fault_error_code() -> u64 {
    FAULT_ERROR_CODE.load(Ordering::SeqCst)
}
