/* chello: C で書いたユーザープログラム（C-a で置き、C-c で libc を使う形にした）。
 *
 * # 何をするか
 *
 * 自前の libc の面を一通り使い、結果を画面へ出す。**libc が動いていることの
 * 実演であり、判定の材料でもある**（`--shell-test` が出力の行を見る）。
 *
 * # crate ではない
 *
 * `kernel/build.rs` が `gcc` を呼んで、`libc.c` と `libc_string.c` と一緒に
 * リンクする。リンクスクリプトは Rust のユーザープログラムと同じ `user.ld` で、
 * フラグは `ADR-0057` の Decision 2 と 3 のものである。 */

#include "libc.h"

int main(void) {
    /* 1. 標準入出力。 */
    puts("hello from C");

    /* 2. 文字列。**長さと比較と複製を 1 行で見せる。** */
    char copy[32];
    strcpy(copy, "zaytos");
    puts("strlen/strcmp/strcpy:");
    putu(strlen(copy));
    write(STDOUT, " ", 1);
    putu((unsigned long)(strcmp(copy, "zaytos") == 0));
    write(STDOUT, " ", 1);
    puts(copy);

    /* 3. メモリ。**重なる向きの `memmove` を見せる**——
     * 前から写すと壊れる形である。 */
    char buffer[8];
    memset(buffer, 'a', sizeof buffer);
    memcpy(buffer, "xy", 2);
    memmove(buffer + 1, buffer, 6);
    buffer[7] = '\0';
    puts("memset/memcpy/memmove:");
    puts(buffer);

    /* 4. ヒープ。**取って、書いて、返す。** */
    char *heap = (char *)malloc(16);
    if (heap == 0) {
        puts("malloc failed");
        return 1;
    }
    strcpy(heap, "heap ok");
    puts(heap);
    free(heap);

    /* 5. 終了状態。**0 以外を返すとシェルが数を出す**ので、0 で終わる。 */
    return 0;
}
