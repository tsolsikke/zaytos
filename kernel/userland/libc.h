/* 自前の libc の口（C-c。ADR-0057）。
 *
 * **面は 7 つである**（ADR-0057 の Decision 1）——起動と終了／`int 0x80` の
 * 呼び出し規約／標準入出力／`brk` の上の `malloc`／`str*` と `mem*`／
 * 終了状態／`errno`。**広げるときは ADR を見直すこと。**
 *
 * **ヘッダは 1 本である。** 標準の `<string.h>` や `<stdio.h>` の分け方を
 * 真似ない——**面が 7 つしか無いので、分けると読む先が増えるだけである。** */

#ifndef ZAYTOS_LIBC_H
#define ZAYTOS_LIBC_H

typedef unsigned long size_t;

/* 標準入出力の fd（`kernel/src/syscall.rs`）。 */
#define STDIN 0
#define STDOUT 1
#define STDERR 2

/* 直前のシステムコールが返した `-errno` の絶対値（ADR-0057 の Decision 1）。
 *
 * **単一スレッドなので素のグローバルである。** TLS を使わない。
 * **成功したときは触らない**——**C の慣行どおりで、呼ぶ側は失敗を見てから読む。** */
extern int errno;

/* 起動と終了。`_start` が `main` を呼び、返り値を `exit` へ渡す。 */
int main(void);
void exit(int status) __attribute__((noreturn));

/* 標準入出力。**失敗は -1 を返し、`errno` を据える。** */
long write(int fd, const void *buf, size_t count);
long read(int fd, void *buf, size_t count);

/* 文字列を書いて改行を足す。**書けたら 0、失敗は -1。** */
int puts(const char *s);
/* 符号なし 10 進を書く。**改行は足さない。** */
int putu(unsigned long value);

/* `brk` の上の割り当て。**`free` は最後の 1 つだけ戻せる**（下の doc）。 */
void *malloc(size_t size);
void free(void *pointer);

/* 純粋な中身（`libc_string.c`）。**ホストでも建てて単体テストを走らせるため、
 * 標準の名前とは別に持つ**（あちらの doc）。 */
size_t zt_strlen(const char *s);
int zt_strcmp(const char *a, const char *b);
char *zt_strcpy(char *dst, const char *src);
void *zt_memcpy(void *dst, const void *src, size_t n);
void *zt_memset(void *dst, int c, size_t n);
void *zt_memmove(void *dst, const void *src, size_t n);
int zt_utoa(char *out, unsigned long value);

/* 数学の最小面（B-b。`libc_math.c`）。**4 つしか無い**——`stb_truetype` が
 * ビットマップの経路で呼ぶのがこれだけだからである（あちらの doc）。
 *
 * **標準の名前（`sqrt` など）で包まない。** **包む相手が居ない**——
 * `stb_truetype` はマクロ（`STBTT_sqrt` ほか）で名前を選べるので、
 * `zt_` の名前をそのまま指せる。**要る人が現れたら包む。** */
double zt_fabs(double value);
double zt_floor(double value);
double zt_ceil(double value);
double zt_sqrt(double value);

/* 文字列とメモリ。**中身は `libc_string.c` に在る。** */
size_t strlen(const char *s);
int strcmp(const char *a, const char *b);
char *strcpy(char *dst, const char *src);
void *memcpy(void *dst, const void *src, size_t n);
void *memset(void *dst, int c, size_t n);
void *memmove(void *dst, const void *src, size_t n);

#endif
