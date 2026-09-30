//! 切り替えの試しのワーカー本体（アセンブリ。M5-c の協調と M5-d のプリエンプティブ）。
//!
//! **`kernel/src/task.rs` から移した**（2026-09-27。境界の段階の手順 2）。**15 本の GPR に印を入れて切り替えを
//! 往復する本体は x86 のレジスタを直に扱うので、CPU 固有の置き場に置く。** **本体が呼ぶ照合と会計の関数と、
//! 共有のバッファは `task` に残る。**

use crate::arch::x86_64::idt::YIELD_VECTOR;
use crate::task::{
    current_task_base, preemptive_loop_top, verify_gprs_and_advance, verify_preemptive_gprs,
    worker_done_and_yield, GPR_BUF, IN_GPR_WINDOW, PREEMPT_DELAY, PREEMPT_WINDOW_SLED,
};

// ワーカー本体（アセンブリ）。偽 IrqContext の RIP がここを指す。
//
// 各ラウンド:
//   1. current_task_base() で自分の base を得る（rax）
//   2. 15 本の GPR へ base + tag を入れる（rax=base+0、rbx=base+1、…）
//   3. int YIELD_VECTOR で yield（往復でスタブが GPR を退避・復元する）
//   4. 復帰後の 15 本を GPR_BUF へ rip 相対で書き出す（レジスタを空けずに済む）
//   5. verify_gprs_and_advance() で照合。1 なら次ラウンド、0 なら終了
// 終了時は worker_done_and_yield() を呼び、戻ってこない前提で jmp ループする。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zeikos_worker_body",
    "zeikos_worker_body:",
    "2:", // ラウンドループ
    "  call {current_base}",
    "  lea rbx, [rax + 1]",
    "  lea rcx, [rax + 2]",
    "  lea rdx, [rax + 3]",
    "  lea rsi, [rax + 4]",
    "  lea rdi, [rax + 5]",
    "  lea rbp, [rax + 6]",
    "  lea r8,  [rax + 8]",
    "  lea r9,  [rax + 9]",
    "  lea r10, [rax + 10]",
    "  lea r11, [rax + 11]",
    "  lea r12, [rax + 12]",
    "  lea r13, [rax + 13]",
    "  lea r14, [rax + 14]",
    "  lea r15, [rax + 15]",
    // rax は既に base（タグ 0）。
    "  int {yv}",
    // 復帰。15 本を GPR_BUF へ rip 相対で書き出す（アドレスにレジスタを使わない）。
    "  mov qword ptr [rip + {buf} + 0],   rax",
    "  mov qword ptr [rip + {buf} + 8],   rbx",
    "  mov qword ptr [rip + {buf} + 16],  rcx",
    "  mov qword ptr [rip + {buf} + 24],  rdx",
    "  mov qword ptr [rip + {buf} + 32],  rsi",
    "  mov qword ptr [rip + {buf} + 40],  rdi",
    "  mov qword ptr [rip + {buf} + 48],  rbp",
    "  mov qword ptr [rip + {buf} + 56],  r8",
    "  mov qword ptr [rip + {buf} + 64],  r9",
    "  mov qword ptr [rip + {buf} + 72],  r10",
    "  mov qword ptr [rip + {buf} + 80],  r11",
    "  mov qword ptr [rip + {buf} + 88],  r12",
    "  mov qword ptr [rip + {buf} + 96],  r13",
    "  mov qword ptr [rip + {buf} + 104], r14",
    "  mov qword ptr [rip + {buf} + 112], r15",
    "  call {verify}",
    "  test rax, rax",
    "  jnz 2b",
    // 終了。
    "3:",
    "  call {done}",
    "  jmp 3b",
    current_base = sym current_task_base,
    verify = sym verify_gprs_and_advance,
    done = sym worker_done_and_yield,
    buf = sym GPR_BUF,
    yv = const YIELD_VECTOR,
);

// プリエンプティブなワーカー本体（アセンブリ）。yield を呼ばない。
//
// 各周回:
//   1. current_task_base() で自分の base を得る（rax）
//   2. 15 本の GPR へ base + tag を入れる
//   3. IN_GPR_WINDOW を 1 にする（rip 相対、レジスタを使わない）
//   4. NOP そりを挟んでウィンドウを広げる（条件1: プリエンプトがウィンドウに落ちる確率を上げ、
//      N > 0 を保証する。widen feature でそりを長くして N が増えることを確かめる）
//   5. 15 本を GPR_BUF へ書き出す
//   6. IN_GPR_WINDOW を 0 にする
//   7. verify_preemptive_gprs() で照合し進捗を数える
//   8. 無限に繰り返す（締切で on_timer_tick がこのワーカーを走行不可にして
//      スケジューラが選ばなくなることで止まる。自分では抜けない）
// timer がこのビジーループを任意の瞬間にプリエンプトし、切り替えが 15 本を
// 保存・復元する。set と store の間（IN_GPR_WINDOW=1）でプリエンプトした回が、
// 保存・復元の検査として意味を持つ。
core::arch::global_asm!(
    ".section .text",
    ".p2align 4",
    ".globl zeikos_preemptive_body",
    "zeikos_preemptive_body:",
    "2:",
    // ループ先頭（IF=1、プリエンプト可）。base を得る。preempt-in-critical の
    // 破壊テストでの確認では、ここで DEMO_LOCK を保持したままスピンする（IF=1 なので
    // timer が食い込む。正常ビルドでは何もしない）。
    "  call {loop_top}",
    "  lea rbx, [rax + 1]",
    "  lea rcx, [rax + 2]",
    "  lea rdx, [rax + 3]",
    "  lea rsi, [rax + 4]",
    "  lea rdi, [rax + 5]",
    "  lea rbp, [rax + 6]",
    "  lea r8,  [rax + 8]",
    "  lea r9,  [rax + 9]",
    "  lea r10, [rax + 10]",
    "  lea r11, [rax + 11]",
    "  lea r12, [rax + 12]",
    "  lea r13, [rax + 13]",
    "  lea r14, [rax + 14]",
    "  lea r15, [rax + 15]",
    // ウィンドウに入る。ここから cli までの間にプリエンプトすると、保存・復元の検査に
    // なる（15 本を保持したまま切り替わる）。
    "  mov byte ptr [rip + {window}], 1",
    // メモリカウンタの遅延ループでウィンドウを広げる。dec/jnz はフラグしか使わず
    // （フラグは IrqContext の rflags で保存・復元される）、pattern の 15 本は
    // 触らない。カウンタはメモリなのでレジスタも使わない。コードは数命令で、
    // そりの長さが .text を膨らませない。
    "  mov qword ptr [rip + {delay}], {sled}",
    "4:",
    "  dec qword ptr [rip + {delay}]",
    "  jnz 4b",
    // cli で store と照合を保護する。GPR_BUF は A/B 共有なので、store の後
    // 照合の前にプリエンプトされると別ワーカーが上書きし、他タスクの値を読んで
    // しまう。cli してから store・照合すれば、その区間は別タスクが割り込めない。
    // 検査対象のウィンドウ（set から cli まで）は cli の前なのでプリエンプト可のまま。
    "  cli",
    "  mov byte ptr [rip + {window}], 0",
    "  mov qword ptr [rip + {buf} + 0],   rax",
    "  mov qword ptr [rip + {buf} + 8],   rbx",
    "  mov qword ptr [rip + {buf} + 16],  rcx",
    "  mov qword ptr [rip + {buf} + 24],  rdx",
    "  mov qword ptr [rip + {buf} + 32],  rsi",
    "  mov qword ptr [rip + {buf} + 40],  rdi",
    "  mov qword ptr [rip + {buf} + 48],  rbp",
    "  mov qword ptr [rip + {buf} + 56],  r8",
    "  mov qword ptr [rip + {buf} + 64],  r9",
    "  mov qword ptr [rip + {buf} + 72],  r10",
    "  mov qword ptr [rip + {buf} + 80],  r11",
    "  mov qword ptr [rip + {buf} + 88],  r12",
    "  mov qword ptr [rip + {buf} + 96],  r13",
    "  mov qword ptr [rip + {buf} + 104], r14",
    "  mov qword ptr [rip + {buf} + 112], r15",
    "  call {verify}",
    "  sti",
    "  jmp 2b",
    loop_top = sym preemptive_loop_top,
    verify = sym verify_preemptive_gprs,
    buf = sym GPR_BUF,
    window = sym IN_GPR_WINDOW,
    delay = sym PREEMPT_DELAY,
    sled = const PREEMPT_WINDOW_SLED,
);
