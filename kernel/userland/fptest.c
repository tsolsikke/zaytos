/* 浮動小数点の状態が保たれることを見る（B-a。ADR-0058）。
 *
 * **判定は3つで、ADR-0058 の決定に1対1で対応する。**
 *
 *   fp: xmm0 at start   決定4（起こすときは既定値から始める）
 *   fp: sum             決定1（切り替えを跨いで保たれる）
 *   fp: xmm0 after spawn 決定2の遠征の側（`spawn` を跨いで保たれる）
 *
 * **XMM を直に読み書きするのは、レジスタそのものが主張の対象だからである。**
 * **C の変数はコンパイラが記憶へ退避しうるので、それでは主張にならない。** */

#include "libc.h"

/* ZaytOS 固有のシステムコール（`kernel/src/syscall.rs`）。 */
#define SYS_SPAWN 0x1004

/* 親が置く目印。**子の値とも、目印タスクの値とも違うものにする。** */
#define PARENT_MARK 0x1122334455667788UL

/* 足す回数。**タイマが何度も食い込む長さにする**——**切り替えが1度も
 * 起きなければ、切り替えの退避は観測できない。** */
#define ROUNDS 2000000UL

static unsigned long xmm0_low(void) {
    unsigned long value;
    __asm__ volatile("movq %%xmm0, %0" : "=r"(value));
    return value;
}

static void set_xmm0_low(unsigned long value) {
    __asm__ volatile("movq %0, %%xmm0" : : "r"(value) : "xmm0");
}

static long spawn_child(const char *path) {
    static const char *argv[] = {"fpchild", 0};
    static const char *envp[] = {0};
    long result;
    __asm__ volatile("int $0x80"
                     : "=a"(result)
                     : "a"((long)SYS_SPAWN), "D"(path), "S"(argv), "d"(envp)
                     : "rcx", "r11", "memory");
    return result;
}

/* 16 進で書く。**`printf` は無い**（ADR-0057 の「決めないこと」）。 */
static void put_hex(unsigned long value) {
    char out[19];
    const char *digits = "0123456789abcdef";
    int at = 0;
    out[at++] = '0';
    out[at++] = 'x';
    for (int shift = 60; shift >= 0; shift -= 4) {
        out[at++] = digits[(value >> shift) & 0xf];
    }
    write(STDOUT, out, (size_t)at);
}

int main(void) {
    /* 決定4。**起こされた時点の XMM は 0 でなければならない。** */
    unsigned long at_start = xmm0_low();
    write(STDOUT, "fp: xmm0 at start = ", 20);
    put_hex(at_start);
    puts("");

    /* 決定1。**切り替えを跨いで積み上がること。** */
    double accumulator = 0.0;
    for (unsigned long round = 0; round < ROUNDS; round++) {
        accumulator += 1.5;
    }
    /* **小数のまま出さない**——書式が無いので、2 倍して整数で出す。 */
    unsigned long doubled = (unsigned long)(accumulator * 2.0);
    write(STDOUT, "fp: sum = ", 10);
    putu(doubled);
    puts("");

    /* 決定2の遠征の側。**子を起こしても親の XMM が残ること。** */
    set_xmm0_low(PARENT_MARK);
    long spawned = spawn_child("/bin/fpchild");
    unsigned long after = xmm0_low();
    write(STDOUT, "fp: xmm0 after spawn = ", 23);
    put_hex(after);
    write(STDOUT, " (child returned ", 17);
    putu((unsigned long)spawned);
    puts(")");

    /* **次に起きるプログラムのために、目印を残して終わる**——決定4の破壊は
     * これを見る（`fp-no-fresh-state` では、次のプログラムがこれを読む）。 */
    set_xmm0_low(PARENT_MARK);
    return 0;
}
