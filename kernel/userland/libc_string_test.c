/* `libc_string.c` の純粋な関数を、ホストで固定する（C-c。ADR-0057 の Decision 5）。
 *
 * # QEMU を起こさない
 *
 * ADR-0045 と同じ理由である。**ハード依存が無いので、ホストで走らせられる。**
 * `cargo xtask check` の項目が、ホストの `cc` で建てて走らせる。
 *
 * # ホストの libc と一緒にリンクする
 *
 * **だから `zt_` の名前を試す。** 標準の名前で定義していたら衝突する
 * （`libc_string.c` の doc）。 */

#include <stdio.h>
#include <string.h>

unsigned long zt_strlen(const char *s);
int zt_strcmp(const char *a, const char *b);
char *zt_strcpy(char *dst, const char *src);
void *zt_memcpy(void *dst, const void *src, unsigned long n);
void *zt_memset(void *dst, int c, unsigned long n);
void *zt_memmove(void *dst, const void *src, unsigned long n);
int zt_utoa(char *out, unsigned long value);

static int failures;

static void check(int ok, const char *what) {
    if (!ok) {
        printf("    FAILED: %s\n", what);
        failures++;
    }
}

int main(void) {
    check(zt_strlen("") == 0, "strlen of the empty string is 0");
    check(zt_strlen("zaytos") == 6, "strlen counts bytes");

    check(zt_strcmp("a", "a") == 0, "strcmp says equal");
    check(zt_strcmp("a", "b") < 0, "strcmp orders a before b");
    check(zt_strcmp("b", "a") > 0, "strcmp orders b after a");
    check(zt_strcmp("ab", "a") > 0, "the longer string is greater");
    /* **符号なしで比べる。** 0x80 以上のバイトを負として扱うと順序が狂う。 */
    check(zt_strcmp("\x80", "\x01") > 0, "strcmp compares as unsigned");

    char buffer[16];
    char *returned = zt_strcpy(buffer, "zaytos");
    check(returned == buffer, "strcpy returns its destination");
    check(strcmp(buffer, "zaytos") == 0, "strcpy copies the terminator too");

    zt_memset(buffer, 'x', 4);
    check(buffer[0] == 'x' && buffer[3] == 'x', "memset fills");
    check(buffer[4] == 'o', "memset stops at the length");

    zt_memcpy(buffer, "ab", 2);
    check(buffer[0] == 'a' && buffer[1] == 'b', "memcpy copies");

    /* **重なりの両向きを見る。** 前から写す実装は、片方でだけ壊れる。 */
    char forward[8] = "abcdefg";
    zt_memmove(forward + 1, forward, 6);
    check(memcmp(forward, "aabcdef", 7) == 0, "memmove handles dst above src");

    char backward[8] = "abcdefg";
    zt_memmove(backward, backward + 1, 6);
    check(memcmp(backward, "bcdefgg", 7) == 0, "memmove handles dst below src");

    char zero[8] = "abcdefg";
    zt_memmove(zero, zero, 7);
    check(memcmp(zero, "abcdefg", 7) == 0, "memmove with the same pointer is a no-op");

    char digits[24];
    int count = zt_utoa(digits, 0);
    check(count == 1 && digits[0] == '0', "utoa writes a single zero");
    count = zt_utoa(digits, 1234567890UL);
    check(count == 10 && memcmp(digits, "1234567890", 10) == 0, "utoa writes the digits in order");
    count = zt_utoa(digits, 18446744073709551615UL);
    check(count == 20, "utoa writes 20 digits for the largest u64");

    if (failures == 0) {
        printf("libc string tests: all passed\n");
        return 0;
    }
    printf("libc string tests: %d failed\n", failures);
    return 1;
}
