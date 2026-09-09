/* 数学の最小面（B-b。ADR-0057 の Addendum）。
 *
 * # 4 つしか無い理由
 *
 * **`stb_truetype` がビットマップの経路で呼ぶのは 4 つだけである**——
 * `sqrt` / `floor` / `ceil` / `fabs`（実測。2026-09-09。ヘッダの呼び出し箇所を
 * 呼び出し元の関数ごとに分類した）。**`fmod` と `pow` と `cos` と `acos` は
 * SDF の経路（`stbtt_GetGlyphSDF`）にしか出ない**ので、ここには無い
 * （`docs/deferred-decisions.md` に行が在る）。
 *
 * # 2 度建てる（`libc_string.c` と同じ形）
 *
 *   1. ZaytOS 向け（freestanding）——`libc.c` が標準の名前で包む
 *   2. ホスト——ホストの libc と一緒にリンクして単体テストを走らせる
 *
 * **だから中身は `zt_` の名前で持つ。** 標準の名前で定義するとホストの libc と
 * 衝突する。
 *
 * # 契約
 *
 * **`errno` を据えない。** **C の標準は数学関数の `errno` を実装依存にしており、
 * `stb_truetype` は見ていない**（実測。呼び出し箇所のどれも戻り値しか見ない）。
 * **NaN と ±Inf は、IEEE 754 がその演算に定める値をそのまま返す。**
 *
 * # SSE を使う
 *
 * **`ADR-0058` で有効にした。** `sqrt` は 1 命令（`sqrtsd`）である。
 * **`roundsd`（SSE4.1）は使わない**——**QEMU の既定の CPU が持つとは限らない**
 * ので、`floor` と `ceil` は整数への変換とビット操作で書く。 */

/* ビットで見るための入れ物。**共用体を通す型変換は C99 が認めている。** */
static unsigned long bits_of(double value) {
    union {
        double d;
        unsigned long u;
    } view;
    view.d = value;
    return view.u;
}

static double from_bits(unsigned long bits) {
    union {
        double d;
        unsigned long u;
    } view;
    view.u = bits;
    return view.d;
}

/* 符号のビットだけを落とす。**NaN も Inf もそのまま通る。** */
double zt_fabs(double value) {
    return from_bits(bits_of(value) & 0x7fffffffffffffffUL);
}

/* 2^52。**これ以上の大きさの double は、すでに整数である**（仮数が足りない）。 */
#define ZT_FIRST_NON_FRACTIONAL 4503599627370496.0

/* `x` を超えない最大の整数。
 *
 * **`(long long)` への変換は 0 方向へ切り捨てる**ので、負で端数が在るときだけ
 * 1 を引く。**2^52 以上と NaN と Inf は、比較が偽になる経路でそのまま返る**
 * （`!(|x| < 2^52)` は NaN でも真になる）。 */
double zt_floor(double value) {
    if (!(zt_fabs(value) < ZT_FIRST_NON_FRACTIONAL)) {
        return value;
    }
    double truncated = (double)(long long)value;
    if (truncated > value) {
        truncated -= 1.0;
    }
    /* **±0 の符号を保つ。** `-0.5` の切り捨ては `-1.0` なのでここへ来ない。
     * 来るのは `-0.0` 自身と `0.0` である。 */
    if (truncated == 0.0) {
        return from_bits(bits_of(value) & 0x8000000000000000UL);
    }
    return truncated;
}

/* `x` を下回らない最小の整数。[`zt_floor`] の対である。 */
double zt_ceil(double value) {
    if (!(zt_fabs(value) < ZT_FIRST_NON_FRACTIONAL)) {
        return value;
    }
    double truncated = (double)(long long)value;
    if (truncated < value) {
        truncated += 1.0;
    }
    if (truncated == 0.0) {
        return from_bits(bits_of(value) & 0x8000000000000000UL);
    }
    return truncated;
}

/* 平方根。**`sqrtsd` 1 命令である。**
 *
 * **丸めは MXCSR の RC に従う**（既定は最近接偶数。`ADR-0058` の
 * `FpArea::fresh` が 0x1F80 を据える）。**負なら QNaN を返す**——
 * **例外はマスクされているので、止まらない。**
 *
 * **`stb_truetype` がビットマップの経路で渡すのは、いつも 2 乗の和である**
 * （実測。`stbtt__GetGlyphShapeTT` と `stbtt__tesselate_cubic` の 4 箇所）。
 * **したがって負は来ない。** **来たときの振る舞いは、命令が決めるものをそのまま
 * 契約にする**——**分岐を足しても、返す値は同じである。** */
double zt_sqrt(double value) {
    double result;
    __asm__("sqrtsd %1, %0" : "=x"(result) : "x"(value));
    return result;
}
