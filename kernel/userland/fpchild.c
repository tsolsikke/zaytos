/* 親の XMM を壊す子（B-a。ADR-0058）。
 *
 * **`fptest` が起こす。** **やることは1つだけ**——**XMM へ自分の値を載せて
 * 終わる。** **親の値が戻らなければ、親の判定が落ちる。** */

#include "libc.h"

/* 子が置く値。**親の目印とも、目印タスクの値とも違うものにする。** */
#define CHILD_MARK 0x99aabbccddeeff00UL

/* 16 進で書く。**`fptest` と同じ形の行を出す**——**判定は「起こされた時点の
 * XMM が 0」を、親と子の区別なく数える**（ADR-0058 の決定 4）。 */
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
    /* 決定4。**親が XMM に値を持ったまま起こしても、子は 0 から始まる。**
     * **ここが決定 4 の観測点である**——**親の側では見えない。**
     * **親が終わるときに残した目印は、親を起こした側の復元が消してしまう。** */
    unsigned long value;
    __asm__ volatile("movq %%xmm0, %0" : "=r"(value));
    write(STDOUT, "fp: xmm0 at start = ", 20);
    put_hex(value);
    puts("");

    value = CHILD_MARK;
    __asm__ volatile("movq %0, %%xmm0" : : "r"(value) : "xmm0");
    puts("fpchild: clobbered xmm0");
    return 0;
}
