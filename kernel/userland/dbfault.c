/* `#DB`（ベクタ1）を Ring 3 から上げる（B-a の後の掃き。ADR-0058）。
 *
 * **`EFLAGS.TF` は Ring 3 から `popfq` で立てられる**（`IF` と違って IOPL を
 * 見ない）。**立てると次の命令の後に `#DB` が上がる。**
 *
 * **主張は「カーネルが止まらないこと」である**——**畳まれてシェルへ戻り、
 * 台本が最後まで進む。** **掃きの前は、この1本でカーネルが止まっていた**
 * （実測。2026-09-07）。 */

#include "libc.h"

int main(void) {
    puts("dbfault: setting EFLAGS.TF");
    __asm__ volatile("pushfq\n\t"
                     "orq $0x100, (%rsp)\n\t"
                     "popfq\n\t"
                     "nop");
    /* **ここへは来ない。** 来たら、単一ステップが効いていない。 */
    puts("dbfault: still running (the debug exception did not fire)");
    return 0;
}
