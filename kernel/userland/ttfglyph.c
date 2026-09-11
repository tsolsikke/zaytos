/* フォントを像から読み、Ring 3 で 1 文字ラスタライズする（B-d）。
 *
 * # 1 本を 2 度建てる（`libc_string.c` / `libc_math.c` と同じ形）
 *
 *   1. ZaytOS 向け（freestanding）——`/lib/font.ttf` を読み、fd 1 へ出す
 *   2. ホスト——同じ源を同じ書式で出し、`xtask` が 2 つをバイト単位で
 *      突き合わせる
 *
 * **描画の中身は共有し、分けるのは入出力だけである。** **違うのは
 * `#include` の塊と、フォントの在処の 2 つだけで、どちらも `ZT_HOST` と
 * `ZT_FONT_PATH` で切り替える。**
 *
 * # なぜバイト単位の一致を主張にできるか
 *
 * **同じ版・同じフォント・同じ倍率なら、出るビットマップは同じである。**
 * **設計の前に測った**（2026-09-10）——**`-O2` / `-Os` × ホストの `libm` /
 * 自前の数学の 4 通りで建て、`A`@16px・`g`@32px・`M`@12px・`W`@48px の
 * 4 つを出し、16 通りとも一致した**（md5 で比べた）。
 *
 * **`stb_truetype` のビットマップの経路は整数と `double` の四則と `sqrt` /
 * `floor` / `ceil` / `fabs` しか使わない**（`libc_math.c` の doc）。
 * **どれも IEEE 754 が値を一意に定める演算である**——**だから機械が
 * 違っても、最適化が違っても、同じ値が出る。**
 *
 * # `stb_truetype` の SDF の 4 つは、宣言だけ置く
 *
 * **`STBTT_sqrt` を定義すると `STBTT_pow` も定義しなければならない**
 * ——**stb の既定のマクロは対で置かれている**（実測。`sqrt` と `pow`、
 * `cos` と `acos`、`ifloor` と `iceil`）。**しかしビットマップの経路は
 * SDF の 4 つを呼ばない。**
 *
 * **そこで定義せず、宣言だけ置く。** **`--gc-sections` が「どこからも
 * 届かない」ことを証明できたときだけリンクが通る**——**呼ぶ経路が
 * 生えたら、リンクがその場で落ちる。** **これは判定である**（費用は
 * 旗が 3 つ増えることだけである）。 */

#ifdef ZT_HOST
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
double zt_fabs(double value);
double zt_floor(double value);
double zt_ceil(double value);
double zt_sqrt(double value);
#else
#include "libc.h"
#endif

#include <stddef.h>

/* フォントの在処。**ホスト側は `xtask` が `-D` で渡す。** */
#ifndef ZT_FONT_PATH
#define ZT_FONT_PATH "/lib/font.ttf"
#endif

/* 見る字と倍率。**判定の対象なので、ここが唯一の出どころである。** */
#define GLYPH 'A'
#define PIXEL_HEIGHT 16.0f

/* フォントを読み込む器の大きさ。
 *
 * **大きさを先に知る口を持たない**（`libc.h` の「ファイルを開いて閉じる」）。
 * **0 が返るまで読み、器を使い切ったら失敗として言う**——**黙って途中で
 * 切らない。** **いまのフォントは 343,140 バイトで、器はその 1.5 倍である。** */
#define FONT_CAPACITY (512 * 1024)

/* SDF の経路だけが呼ぶ 4 つ。**定義を置かない**（上の doc）。 */
double zt_sdf_only_pow(double base, double exponent);
double zt_sdf_only_fmod(double numerator, double denominator);
double zt_sdf_only_cos(double radians);
double zt_sdf_only_acos(double value);

#define STBTT_ifloor(x) ((int)zt_floor(x))
#define STBTT_iceil(x) ((int)zt_ceil(x))
#define STBTT_sqrt(x) zt_sqrt(x)
#define STBTT_pow(x, y) zt_sdf_only_pow((x), (y))
#define STBTT_fmod(x, y) zt_sdf_only_fmod((x), (y))
#define STBTT_cos(x) zt_sdf_only_cos(x)
#define STBTT_acos(x) zt_sdf_only_acos(x)
#define STBTT_fabs(x) zt_fabs(x)
#define STBTT_malloc(size, context) ((void)(context), malloc(size))
#define STBTT_free(pointer, context) ((void)(context), free(pointer))
/* **止めない。** **像の読みが崩れていれば、出るビットマップが変わって
 * 判定が落ちる**——**`assert` で止めると、落ちた理由が「止まった」に
 * 化けるだけである。** */
#define STBTT_assert(condition) ((void)0)
#define STBTT_strlen(s) strlen(s)
#define STBTT_memcpy memcpy
#define STBTT_memset memset

#define STB_TRUETYPE_IMPLEMENTATION
#include "stb_truetype.h"

/* 16 進 1 バイト。**大文字を使わない**（ホストと綴りを揃えるため、
 * 出す側をこの 1 箇所に絞る）。 */
static void put_hex_byte(char *out, unsigned char value) {
    static const char digits[] = "0123456789abcdef";
    out[0] = digits[(value >> 4) & 0xF];
    out[1] = digits[value & 0xF];
}

/* 書き切る。**短く書けた分は繰り返す。** */
static void write_all_bytes(const char *bytes, size_t length) {
    size_t written = 0;
    while (written < length) {
        long step = write(1, bytes + written, length - written);
        if (step <= 0) {
            return;
        }
        written += (size_t)step;
    }
}

/* 1 行書く。**改行を足す。** */
static void put_line(const char *bytes, size_t length) {
    write_all_bytes(bytes, length);
    write_all_bytes("\n", 1);
}

/* 符号つき 10 進を `out` へ書き、書いた長さを返す。 */
static size_t put_int(char *out, int value) {
    char digits[12];
    size_t count = 0;
    unsigned int magnitude;
    size_t length = 0;
    if (value < 0) {
        out[length++] = '-';
        magnitude = (unsigned int)(-(long)value);
    } else {
        magnitude = (unsigned int)value;
    }
    do {
        digits[count++] = (char)('0' + (magnitude % 10));
        magnitude /= 10;
    } while (magnitude != 0);
    while (count > 0) {
        out[length++] = digits[--count];
    }
    return length;
}

/* 失敗を 1 行で言って降りる。**黙って 0 で終わらない**——**出力が
 * 空のまま緑になる形をここで塞ぐ。** */
static int give_up(const char *what) {
    char line[80];
    size_t length = 0;
    const char *prefix = "ttf: failed ";
    while (prefix[length] != '\0') {
        line[length] = prefix[length];
        length++;
    }
    size_t at = 0;
    while (what[at] != '\0' && length < sizeof(line) - 1) {
        line[length++] = what[at++];
    }
    put_line(line, length);
    return 1;
}

int main(void) {
    int fd = open(ZT_FONT_PATH, O_RDONLY);
    if (fd < 0) {
        return give_up("open");
    }
    unsigned char *font = (unsigned char *)malloc(FONT_CAPACITY);
    if (font == NULL) {
        return give_up("malloc");
    }
    /* **読みの回数はここで数えない。** **数えて出すと、ホストと ZaytOS で
     * 食い違う**（ホストは 1 回で返し、ZaytOS はブロックごとに返しうる）
     * ——**突き合わせるのはビットマップだけである。** **回数はカーネル側の
     * `vfs:` の計器が起動ログへ出す。** */
    long filled = 0;
    for (;;) {
        long step = read(fd, font + filled, (size_t)(FONT_CAPACITY - filled));
        if (step < 0) {
            return give_up("read");
        }
        if (step == 0) {
            break;
        }
        filled += step;
        if (filled == FONT_CAPACITY) {
            return give_up("too big for FONT_CAPACITY");
        }
    }
    if (filled == 0) {
        return give_up("empty");
    }
    if (close(fd) != 0) {
        return give_up("close");
    }

    stbtt_fontinfo font_info;
    if (stbtt_InitFont(&font_info, font, stbtt_GetFontOffsetForIndex(font, 0)) == 0) {
        return give_up("init");
    }

    int width = 0;
    int height = 0;
    int x_offset = 0;
    int y_offset = 0;
    float scale = stbtt_ScaleForPixelHeight(&font_info, PIXEL_HEIGHT);
    unsigned char *bitmap = stbtt_GetCodepointBitmap(&font_info, 0.0f, scale, GLYPH, &width,
                                                     &height, &x_offset, &y_offset);
    if (bitmap == NULL || width <= 0 || height <= 0) {
        return give_up("bitmap");
    }

    /* 表題の行。**字・倍率・大きさ・原点からのずれを出す。** */
    {
        char line[96];
        size_t length = 0;
        const char *head = "ttf: glyph ";
        while (head[length] != '\0') {
            line[length] = head[length];
            length++;
        }
        line[length++] = '\'';
        line[length++] = (char)GLYPH;
        line[length++] = '\'';
        line[length++] = ' ';
        length += put_int(line + length, (int)PIXEL_HEIGHT);
        line[length++] = 'p';
        line[length++] = 'x';
        line[length++] = ' ';
        length += put_int(line + length, width);
        line[length++] = 'x';
        length += put_int(line + length, height);
        line[length++] = ' ';
        length += put_int(line + length, x_offset);
        line[length++] = ' ';
        length += put_int(line + length, y_offset);
        put_line(line, length);
    }

    /* 1 行ずつ 16 進で出す。**行ごとに分けるのは、落ちたときに
     * どの段で食い違ったかが読めるようにするためである。** */
    for (int row = 0; row < height; row++) {
        char line[16 + 2 * 64];
        size_t length = 0;
        const char *head = "ttf: row ";
        while (head[length] != '\0') {
            line[length] = head[length];
            length++;
        }
        length += put_int(line + length, row);
        line[length++] = ' ';
        for (int column = 0; column < width && column < 64; column++) {
            put_hex_byte(line + length, bitmap[row * width + column]);
            length += 2;
        }
        put_line(line, length);
    }

    put_line("ttf: done", 9);
    return 0;
}
