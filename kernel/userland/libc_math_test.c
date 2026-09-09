/* `libc_math.c` をホストで固定する（B-b）。
 *
 * # ホストの libm と突き合わせる
 *
 * **`sqrt` は IEEE 754 が「正しく丸めること」を要求している命令である。**
 * **ホストの `sqrt` も同じ要求を満たすので、ビット単位で一致するはずである**
 * ——**一致を主張にする。** `floor` と `ceil` も同じ形で見る。
 *
 * # 端の値を明示的に置く
 *
 * **±0 の符号・NaN・Inf・2^52 の境目は、突き合わせだけでは通ってしまう**
 * （ホストも同じ誤りをすれば一致する）。**そこは値をビットで書いて確かめる。** */

#include <math.h>
#include <stdio.h>
#include <string.h>

double zt_fabs(double value);
double zt_floor(double value);
double zt_ceil(double value);
double zt_sqrt(double value);

static int failures;

static void check(int ok, const char *what) {
    if (!ok) {
        printf("    FAILED: %s\n", what);
        failures++;
    }
}

static unsigned long bits(double value) {
    unsigned long out;
    memcpy(&out, &value, sizeof(out));
    return out;
}

/* 突き合わせに使う値。**端と、端でないものを混ぜる。** */
static const double SAMPLES[] = {
    0.0,      -0.0,     1.0,       -1.0,      0.5,      -0.5,     2.5,
    -2.5,     3.0,      -3.0,      0.9999999, -0.9999999, 1e-300,  -1e-300,
    1e300,    -1e300,   4503599627370496.0,   -4503599627370496.0,
    4503599627370495.5, -4503599627370495.5,  123456.789, -123456.789,
    2.0,      3.0,      10.0,      1e-8,      7.0,      1048576.25,
};

int main(void) {
    const unsigned long count = sizeof(SAMPLES) / sizeof(SAMPLES[0]);

    /* --- ホストと突き合わせる --- */
    for (unsigned long i = 0; i < count; i++) {
        double x = SAMPLES[i];
        check(bits(zt_fabs(x)) == bits(fabs(x)), "fabs matches the host");
        check(bits(zt_floor(x)) == bits(floor(x)), "floor matches the host");
        check(bits(zt_ceil(x)) == bits(ceil(x)), "ceil matches the host");
        if (x >= 0.0) {
            check(bits(zt_sqrt(x)) == bits(sqrt(x)), "sqrt matches the host");
        }
    }

    /* **正しく丸めていること。** **端数のある平方根を並べて、1 つずつ見る。** */
    for (int i = 1; i <= 2000; i++) {
        double x = (double)i / 7.0;
        check(bits(zt_sqrt(x)) == bits(sqrt(x)), "sqrt is correctly rounded");
    }

    /* --- ±0 の符号 --- */
    check(bits(zt_fabs(-0.0)) == 0UL, "fabs(-0) is +0");
    check(bits(zt_floor(-0.0)) == 0x8000000000000000UL, "floor(-0) keeps the sign");
    check(bits(zt_ceil(-0.0)) == 0x8000000000000000UL, "ceil(-0) keeps the sign");
    check(bits(zt_floor(0.0)) == 0UL, "floor(+0) is +0");
    check(bits(zt_sqrt(-0.0)) == 0x8000000000000000UL, "sqrt(-0) is -0 (IEEE 754)");

    /* --- 端数の向き --- */
    check(zt_floor(2.5) == 2.0, "floor(2.5)");
    check(zt_floor(-2.5) == -3.0, "floor(-2.5)");
    check(zt_ceil(2.5) == 3.0, "ceil(2.5)");
    check(zt_ceil(-2.5) == -2.0, "ceil(-2.5)");
    check(zt_floor(-0.5) == -1.0, "floor(-0.5)");
    check(zt_ceil(-0.5) == 0.0, "ceil(-0.5)");
    check(bits(zt_ceil(-0.5)) == 0x8000000000000000UL, "ceil(-0.5) is -0");

    /* --- 2^52 の境目。**これ以上は端数を持てない** --- */
    check(zt_floor(4503599627370496.0) == 4503599627370496.0, "floor at 2^52");
    check(zt_ceil(4503599627370496.0) == 4503599627370496.0, "ceil at 2^52");
    check(zt_floor(4503599627370495.5) == 4503599627370495.0, "floor just below 2^52");
    check(zt_ceil(4503599627370495.5) == 4503599627370496.0, "ceil just below 2^52");

    /* --- NaN と Inf --- */
    double nan_value = zt_sqrt(-1.0);
    check(nan_value != nan_value, "sqrt(-1) is NaN");
    check(zt_sqrt(INFINITY) == INFINITY, "sqrt(inf) is inf");
    check(zt_floor(INFINITY) == INFINITY, "floor(inf) is inf");
    check(zt_ceil(-INFINITY) == -INFINITY, "ceil(-inf) is -inf");
    check(zt_fabs(-INFINITY) == INFINITY, "fabs(-inf) is inf");
    double nan_in = NAN;
    check(zt_floor(nan_in) != zt_floor(nan_in), "floor(NaN) is NaN");
    check(zt_ceil(nan_in) != zt_ceil(nan_in), "ceil(NaN) is NaN");
    check(zt_fabs(nan_in) != zt_fabs(nan_in), "fabs(NaN) is NaN");

    /* --- 平方根の値そのもの --- */
    check(zt_sqrt(0.0) == 0.0, "sqrt(0)");
    check(zt_sqrt(1.0) == 1.0, "sqrt(1)");
    check(zt_sqrt(4.0) == 2.0, "sqrt(4)");
    check(zt_sqrt(1e300) == sqrt(1e300), "sqrt(1e300)");

    if (failures) {
        printf("libc math tests: %d check(s) failed\n", failures);
        return 1;
    }
    printf("libc math tests: all passed\n");
    return 0;
}
