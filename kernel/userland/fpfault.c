/* 浮動小数点の例外を、わざと上げる（B-a。ADR-0058）。
 *
 * **`OSXMMEXCPT` を立てたので、`#XM`（ベクタ19）が Ring 3 から届くように
 * なった。** **`MXCSR` の既定は全例外マスクだが、利用者は `ldmxcsr` で
 * マスクを外せる**——**この1本が、その経路を実際に通る。**
 *
 * **主張は「カーネルが止まらないこと」である。** **畳まれてシェルへ戻り、
 * 台本が最後まで進む。** */

#include "libc.h"

int main(void) {
    puts("fpfault: unmasking the divide-by-zero exception");

    /* MXCSR のビット9（ZM。0除算のマスク）を落とす。 */
    unsigned int mxcsr;
    __asm__ volatile("stmxcsr %0" : "=m"(mxcsr));
    mxcsr &= ~(1u << 9);
    __asm__ volatile("ldmxcsr %0" : : "m"(mxcsr));

    /* **書けたことを読み戻して言う。** **上がらなかったときに「マスクが
     * 外れていない」と「例外が上がらない」を分けるためである。** */
    unsigned int back;
    __asm__ volatile("stmxcsr %0" : "=m"(back));
    write(STDOUT, "fpfault: mxcsr = ", 17);
    putu(back);
    puts("");

    /* 0 で割る。**マスクを外したので `#XM` が上がる**——**はずだが、
     * QEMU の TCG は上げない**（実測。同じコードはホストで SIGFPE になる）。 */
    volatile double one = 1.0;
    volatile double zero = 0.0;
    volatile double result = one / zero;
    (void)result;
    puts("fpfault: the SIMD exception did not fire (QEMU TCG does not deliver it)");

    /* **x87 の側も試す。** **こちらは `#MF`（ベクタ16）である。**
     * **x87 の例外は次の x87 命令まで保留されるので、`fwait` で受ける。** */
    puts("fpfault: unmasking the x87 divide-by-zero exception");
    unsigned short cw;
    __asm__ volatile("fnstcw %0" : "=m"(cw));
    cw &= (unsigned short)~(1u << 2);
    __asm__ volatile("fldcw %0" : : "m"(cw));
    volatile long double a = 1.0L;
    volatile long double b = 0.0L;
    volatile long double c = a / b;
    (void)c;
    __asm__ volatile("fwait");

    /* **ここへ来たら、どちらの例外も上がっていない。** */
    puts("fpfault: still running (neither exception fired)");
    return 0;
}
