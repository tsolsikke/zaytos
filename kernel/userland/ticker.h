/* 2 本の Ring 3 が同時に進むことを見る（W1-c-4。ADR-0060）。本体。
 *
 * **`tickera.c` と `tickerb.c` がこれを取り込む。** **名前と周回数だけが違う。**
 * **引数で渡さないのは、`_start` が `main` へ引数を渡さないからである**
 * （`libc.c` の `exit(main())`。**`_start` を変えると、すべての C の像が動く**）。
 *
 * **2 本は別の空間で、同じ VA に `.data` の `name` を持つ。** **起こした後に自分の名前を
 * 書き、毎周読み直す。**
 *
 * **判定は行の順序ではなく内容で見る。** 終わりに 1 行出す。
 *
 *   ticker X done rounds=N name_ok=true sum_ok=true
 *
 *   name_ok  自分の `.data` の名前が、書いた値のままだった（CR3 の入れ替え）
 *   sum_ok   途中の和が毎周期待値どおりだった（FP の入れ替え）
 *
 * **足す量を 2 本で変える**——**同じ量だと、入れ替えを省いても値が揃って見えうる。**
 *
 * **`A` は最後に `ud2` で畳まれる**（回復点の入れ替えを見る）。**`B` は `exit(0)` で終わる。** */

#include "libc.h"

/* 1 周で足す回数。**タイマが何度も食い込む長さにする**（`fptest.c` と同じ）。 */
#define TICKER_ADDS_PER_ROUND 2000000UL

/* **`volatile` にする**——**`-Os` は「書いてから読むまでに呼び出しが無い」を見て、
 * 比較を畳みうる。** **畳まれると、空間を取り違えても `name_ok` は真のままである。** */
static volatile char ticker_name[8] = "?";

int main(void) {
    const char me = TICKER_NAME;
    ticker_name[0] = me;

    /* **2 進で割り切れる量にして、和を厳密に比べる。** */
    const double step = TICKER_STEP;
    double accumulator = 0.0;
    int name_ok = 1;
    int sum_ok = 1;
    unsigned long done = 0;
    for (; done < TICKER_ROUNDS; done++) {
        for (unsigned long add = 0; add < TICKER_ADDS_PER_ROUND; add++) {
            accumulator += step;
        }
        if (ticker_name[0] != me) {
            name_ok = 0;
        }
        if (accumulator != step * (double)TICKER_ADDS_PER_ROUND * (double)(done + 1)) {
            sum_ok = 0;
        }
    }

    write(STDOUT, "ticker ", 7);
    write(STDOUT, &me, 1);
    write(STDOUT, " done rounds=", 13);
    putu(done);
    if (name_ok) {
        write(STDOUT, " name_ok=true", 13);
    } else {
        write(STDOUT, " name_ok=false", 14);
    }
    puts(sum_ok ? " sum_ok=true" : " sum_ok=false");

#ifdef TICKER_FOLDS
    __builtin_trap();
#endif
    return 0;
}
