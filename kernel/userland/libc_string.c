/* 文字列とメモリの純粋な操作（C-c。ADR-0057 の Decision 5）。
 *
 * # なぜ名前が `zt_` で始まるのか
 *
 * このファイルは 2 度建てる。
 *
 *   1. ZaytOS 向け（freestanding）——`libc.c` が標準の名前で包む
 *   2. ホスト（`cargo xtask check` の項目）——ホストの libc と一緒にリンクし、
 *      単体テストを走らせる
 *
 * 2 のとき、標準の名前（`strlen` など）で定義すると、ホストの libc と衝突する。
 * **そこで、中身は `zt_` を付けた名前で持ち、標準の名前は `libc.c` が薄く包む。**
 *
 * # なぜホストで試すのか
 *
 * ADR-0045 と同じ理由である。**ハード依存の無いロジックを、QEMU を起こさずに
 * 固定する。** ここに在るのはすべて純粋な関数で、システムコールを 1 つも出さない。
 *
 * # SSE を使わない
 *
 * ADR-0057 の Decision 3。**素朴なループで書く**——ベクタ化されると
 * `-mno-sse` で建たない。速さは要求していない。 */

typedef unsigned long zt_size_t;

zt_size_t zt_strlen(const char *s) {
    const char *p = s;
    while (*p) {
        p++;
    }
    return (zt_size_t)(p - s);
}

int zt_strcmp(const char *a, const char *b) {
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return (int)(unsigned char)*a - (int)(unsigned char)*b;
}

char *zt_strcpy(char *dst, const char *src) {
    char *out = dst;
    while ((*dst++ = *src++) != '\0') {
    }
    return out;
}

void *zt_memcpy(void *dst, const void *src, zt_size_t n) {
    char *d = (char *)dst;
    const char *s = (const char *)src;
    while (n--) {
        *d++ = *s++;
    }
    return dst;
}

void *zt_memset(void *dst, int c, zt_size_t n) {
    char *d = (char *)dst;
    while (n--) {
        *d++ = (char)c;
    }
    return dst;
}

/* 重なりを許す複製。**重なる向きで前から写すと、写した先を読むことになる。** */
void *zt_memmove(void *dst, const void *src, zt_size_t n) {
    char *d = (char *)dst;
    const char *s = (const char *)src;
    if (d == s || n == 0) {
        return dst;
    }
    if (d < s) {
        while (n--) {
            *d++ = *s++;
        }
    } else {
        d += n;
        s += n;
        while (n--) {
            *--d = *--s;
        }
    }
    return dst;
}

/* 符号なし 10 進を書く。**書いた桁数を返す。**
 *
 * **終端の NUL は置かない**——**呼ぶ側が長さで扱う。**
 * **`out` は 20 バイト以上あること**（`u64` の最大は 20 桁）。 */
int zt_utoa(char *out, unsigned long value) {
    char digits[20];
    int count = 0;
    do {
        digits[count++] = (char)('0' + (value % 10));
        value /= 10;
    } while (value != 0);
    for (int i = 0; i < count; i++) {
        out[i] = digits[count - 1 - i];
    }
    return count;
}
