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

use core::ptr::addr_of;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::gdt;

/// ユーザーコードの仮想アドレス（`PML4[USER_PML4_INDEX]` サブツリー。M5-e-2 が
/// 残した中間テーブルを再利用する）。`cli` 1 命令を置く。
pub const USER_CODE_VIRT: u64 = 0x0000_0080_0000_0000;
/// ユーザースタックの仮想アドレス（同サブツリー内、コードの 1 MiB 上）。
pub const USER_STACK_VIRT: u64 = 0x0000_0080_0010_0000;
/// ユーザースタックの上端（1 ページ）。iretq 偽フレームの RSP に使う。
pub const USER_STACK_TOP: u64 = USER_STACK_VIRT + 4096;

/// 遠征専用カーネルスタックの大きさ。#GP が RSP0 経由でここへ切り替わる。
const EXCURSION_STACK_SIZE: usize = 16 * 1024;

// フィールドは値としては読まず、静的領域のアドレスだけを取る（RSP0 用の
// スタック領域）。dead_code はそのための許容。
#[repr(align(16))]
#[allow(dead_code)]
struct ExcursionStack([u8; EXCURSION_STACK_SIZE]);

/// 遠征専用のカーネルスタック（`.bss`）。IST スタックと同じ静的確保。
static mut EXCURSION_STACK: ExcursionStack = ExcursionStack([0; EXCURSION_STACK_SIZE]);

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

static mut RECOVERY: Recovery = Recovery {
    rsp: 0,
    rbx: 0,
    rbp: 0,
    r12: 0,
    r13: 0,
    r14: 0,
    r15: 0,
    resume_rip: 0,
};

/// 遠征中か。遠征に入る前に立て、畳みで降ろす。
/// これが false のときの Ring 3 由来 #GP は「想定外」として畳まず halt する。
static EXCURSION_ACTIVE: AtomicBool = AtomicBool::new(false);
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

extern "C" {
    /// 偽フレームを積んで Ring 3 へ iretq する（setjmp 相当を内包）。畳みで
    /// 戻ってくると、あたかも通常に return したように呼び出し元へ戻る。
    fn zaytos_enter_ring3();
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
    "  lea rax, [rip + {recovery}]",
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
    // 逆順（SS を先＝高位、RIP を最後＝低位）。RSP/RIP は 512 GiB 付近で
    // imm32 に収まらないため mov 経由で積む。
    "  mov rax, {ss}",
    "  push rax",
    "  mov rax, {user_rsp}",
    "  push rax",
    "  mov rax, {rflags}",
    "  push rax",
    "  mov rax, {cs}",
    "  push rax",
    "  mov rax, {user_rip}",
    "  push rax",
    "  iretq",
    // 復帰点（畳みだけがここへ来る。RSP と callee-saved は longjmp が復元済み）。
    "3:",
    "  ret",
    recovery = sym RECOVERY,
    ss = const gdt::USER_DATA_SELECTOR.bits() as u64,
    cs = const gdt::USER_CODE_SELECTOR.bits() as u64,
    rflags = const 0x202u64,
    user_rsp = const USER_STACK_TOP,
    user_rip = const USER_CODE_VIRT,
);

// 畳み（longjmp）。RECOVERY から RSP と callee-saved を復元し、復帰 RIP へ飛ぶ。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zaytos_resume_from_ring3",
    "zaytos_resume_from_ring3:",
    "  lea rax, [rip + {recovery}]",
    "  mov rsp, [rax + 0]",
    "  mov rbx, [rax + 8]",
    "  mov rbp, [rax + 16]",
    "  mov r12, [rax + 24]",
    "  mov r13, [rax + 32]",
    "  mov r14, [rax + 40]",
    "  mov r15, [rax + 48]",
    "  mov rcx, [rax + 56]",
    "  jmp rcx",
    recovery = sym RECOVERY,
);

/// 遠征専用カーネルスタックの (下端, 上端)。RSP0 とハンドラ RSP の照合に使う。
pub fn excursion_stack_range() -> (u64, u64) {
    let bottom = addr_of!(EXCURSION_STACK) as u64;
    (bottom, bottom + EXCURSION_STACK_SIZE as u64)
}

/// Ring 3 へ 1 回遠征する。戻ってきたら（畳みで）会計を返す。
///
/// RSP0 を遠征専用スタックへ据え、遠征フラグを立て、iretq で Ring 3 へ落ちる。
/// Ring 3 が起こした #GP を `exception_entry` が畳み、ここへ戻る。戻ったら RSP0 を
/// メインの上端へ戻す。
///
/// **どこで畳まれたかは呼び出し側が主張する。** [`folded`]・[`fault_vector`]・
/// [`fault_rip`] を読み、自分が置いた命令の位置と突き合わせること。この関数は
/// 突き合わせない（遠征ごとに予期する位置が違い、それは呼び出し側の知識である）。
///
/// # Safety
///
/// 呼び出し前に、ユーザーコードとユーザースタックが `PML4` のユーザーサブツリーに
/// U=1 で張られており、Ring 3 が #GP を起こすこと。`main_rsp0_top` が呼び出し元
/// （メイン）のカーネルスタック上端で、遠征後に RSP0 をそこへ戻せること。
/// 起動時の単一実行文脈から呼ぶこと。
pub unsafe fn enter(main_rsp0_top: u64) {
    let (_, excursion_top) = excursion_stack_range();

    FOLDED.store(false, Ordering::SeqCst);
    FAULT_RSP.store(0, Ordering::SeqCst);
    HANDLER_RSP.store(0, Ordering::SeqCst);
    FAULT_VECTOR.store(0, Ordering::SeqCst);
    FAULT_RIP.store(0, Ordering::SeqCst);

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

    // 遠征フラグを立てる（畳みの二重判別の条件3）。
    // 破壊 (M5-e-4, no-fold-flag): 立てない。cli の #GP が畳まれず dump+halt する。
    #[cfg(not(feature = "ring3-test-no-fold-flag"))]
    EXCURSION_ACTIVE.store(true, Ordering::SeqCst);

    // SAFETY: 偽フレームを積んで Ring 3 へ落ちる。ユーザーページは呼び出し側が
    // 張り済み。畳みで戻ってくる（callee-saved と RSP は longjmp が復元する）。
    unsafe {
        zaytos_enter_ring3();
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

    // 畳みで戻った。RSP0 をメインの上端へ戻す（スケジューラの読み戻し前提を保つ）。
    // SAFETY: main_rsp0_top は呼び出し元のカーネルスタック上端。
    unsafe {
        gdt::set_rsp0(main_rsp0_top);
    }
}

/// `exception_entry` が呼ぶ。今この例外を畳んでよいかを判定する。
///
/// 呼び出し側で「ベクタ==13」「CS.RPL==3」を確認済みで、ここでは遠征中であることを
/// 見る。**フォルト RIP は見ない**（[`FAULT_RIP`] の doc）。
pub fn should_fold() -> bool {
    EXCURSION_ACTIVE.load(Ordering::SeqCst)
}

/// Ring 3 由来の例外を畳む。ベクタ・フォルト RIP・CS・RSP とハンドラ RSP を記録し、
/// 遠征フラグを降ろして longjmp で遠征の呼び出し元へ戻る。**戻らない。**
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
    handler_rsp: u64,
) -> ! {
    FAULT_VECTOR.store(fault_vector, Ordering::SeqCst);
    FAULT_RIP.store(fault_rip, Ordering::SeqCst);
    FAULT_CS.store(fault_cs, Ordering::SeqCst);
    FAULT_RSP.store(fault_rsp, Ordering::SeqCst);
    HANDLER_RSP.store(handler_rsp, Ordering::SeqCst);
    EXCURSION_ACTIVE.store(false, Ordering::SeqCst);
    FOLDED.store(true, Ordering::SeqCst);
    // SAFETY: 呼び出し側契約により遠征中で、RECOVERY は保存済み。longjmp は
    // RSP と callee-saved を復元して復帰 RIP へ飛ぶ。戻らない。
    unsafe { zaytos_resume_from_ring3() }
}

/// 畳みが起きたか（遠征後の会計）。
pub fn folded() -> bool {
    FOLDED.load(Ordering::SeqCst)
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
