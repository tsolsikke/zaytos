/* 自前の libc（C-c。ADR-0057）。
 *
 * # 何をしないか
 *
 * **`printf` は無い。** 可変長引数は面が広く、いま要る利用者がいない。
 * **`puts` と `putu` で足りる**（要る者が来たら、そのとき決める）。
 *
 * **`stdio` の緩衝も無い。** `write` はそのままシステムコールへ落ちる。
 *
 * # SSE を使う
 *
 * **`ADR-0058` で有効にした**（ADR-0057 の Decision 3 は役目を終えた）。
 * **ABI の選択なので、libc も利用側も同じフラグで建てること** —— **建てる
 * 場所は `kernel/build.rs` の 1 箇所である**（Decision 4）。 */

#include "libc.h"

/* システムコールの番号（`kernel/src/syscall.rs`。Linux x86-64 から採る）。 */
#define SYS_READ 0
#define SYS_WRITE 1
#define SYS_OPEN 2
#define SYS_CLOSE 3
#define SYS_BRK 12
#define SYS_EXIT 60

int errno;

/* `int 0x80` の呼び出し規約は Linux x86-64 と同じ並びである
 * （rax=番号、rdi/rsi/rdx=引数、rax=返り）。
 *
 * **失敗は `-errno` で返る**（ADR-0020）。 */
static long syscall3(long number, long a, long b, long c) {
    long result;
    __asm__ volatile("int $0x80"
                     : "=a"(result)
                     : "a"(number), "D"(a), "S"(b), "d"(c)
                     : "memory");
    return result;
}

/* 返り値を C の慣行へ直す。**負なら `errno` を据えて -1 を返す。** */
static long settle(long result) {
    if (result < 0) {
        errno = (int)-result;
        return -1;
    }
    return result;
}

void exit(int status) {
    syscall3(SYS_EXIT, status, 0, 0);
    /* ここへは来ない。**来た場合の行き先を用意しておく**——
     * ゼロ埋めを実行し始めるより、確定的な #UD のほうが読める。 */
    __asm__ volatile("ud2");
    __builtin_unreachable();
}

long write(int fd, const void *buf, size_t count) {
    return settle(syscall3(SYS_WRITE, fd, (long)buf, (long)count));
}

long read(int fd, void *buf, size_t count) {
    return settle(syscall3(SYS_READ, fd, (long)buf, (long)count));
}

/* `write` は「届いた分だけ」を返しうるので繰り返す
 * （`userlib.rs` の `write_all` と同じ理由）。 */
int open(const char *path, int flags) {
    return (int)settle(syscall3(SYS_OPEN, (long)path, flags, 0));
}

int close(int fd) {
    return (int)settle(syscall3(SYS_CLOSE, fd, 0, 0));
}

static int write_all(int fd, const char *bytes, size_t length) {
    size_t done = 0;
    while (done < length) {
        long written = write(fd, bytes + done, length - done);
        if (written <= 0) {
            return -1;
        }
        done += (size_t)written;
    }
    return 0;
}

int puts(const char *s) {
    if (write_all(STDOUT, s, strlen(s)) != 0) {
        return -1;
    }
    return write_all(STDOUT, "\n", 1);
}

int putu(unsigned long value) {
    char digits[20];
    int count = zt_utoa(digits, value);
    return write_all(STDOUT, digits, (size_t)count);
}

/* # 割り当ては前へ積むだけである
 *
 * **`brk` は上端しか動かさない**（`kernel/src/syscall.rs` の `sys_brk`）。
 * **上端を伸ばして、伸ばした先を返す。**
 *
 * **`free` は最後の 1 つだけ戻せる。** それ以外は何もしない——
 * **穴を管理する構造を持たない。** **面を 7 つに絞ると決めたためである**
 * （ADR-0057 の Decision 1）。**足りなくなったら、そのとき測って決める。**
 *
 * **返る領域は 0 で埋まっている**（カーネルが写す前にフレームを 0 で埋める。
 * `sys_brk` の doc）。 */

/* いま返した最後の領域の先頭。**0 は「無い」。** */
static unsigned long last_block;
/* その長さ。 */
static size_t last_length;

/* 整列の単位。**`long` の境界に合わせる。** */
#define ALIGN (sizeof(long))

void *malloc(size_t size) {
    if (size == 0) {
        return 0;
    }
    size_t rounded = (size + ALIGN - 1) & ~(ALIGN - 1);
    long current = syscall3(SYS_BRK, 0, 0, 0);
    if (current <= 0) {
        errno = 12; /* ENOMEM */
        return 0;
    }
    unsigned long base = (unsigned long)current;
    long reached = syscall3(SYS_BRK, (long)(base + rounded), 0, 0);
    if (reached < 0 || (unsigned long)reached != base + rounded) {
        /* **半端に伸びた分を戻す**（`userlib.rs` の `heap::reserve` と同じ形）。 */
        syscall3(SYS_BRK, (long)base, 0, 0);
        errno = 12; /* ENOMEM */
        return 0;
    }
    last_block = base;
    last_length = rounded;
    return (void *)base;
}

void free(void *pointer) {
    if (pointer == 0) {
        return;
    }
    if ((unsigned long)pointer != last_block) {
        /* **最後の 1 つでなければ何もしない。** 穴は作らない。 */
        return;
    }
    syscall3(SYS_BRK, (long)last_block, 0, 0);
    last_block = 0;
    last_length = 0;
}

/* `libc_string.c` の純粋な関数を、標準の名前で包む。
 *
 * **中身をここに持たないのは、ホストでも建てて単体テストを走らせるためである**
 * （`libc_string.c` の doc）。 */
size_t strlen(const char *s) { return zt_strlen(s); }
int strcmp(const char *a, const char *b) { return zt_strcmp(a, b); }
char *strcpy(char *dst, const char *src) { return zt_strcpy(dst, src); }
void *memcpy(void *dst, const void *src, size_t n) { return zt_memcpy(dst, src, n); }
void *memset(void *dst, int c, size_t n) { return zt_memset(dst, c, n); }
void *memmove(void *dst, const void *src, size_t n) { return zt_memmove(dst, src, n); }

/* 入口。**`main` を呼び、返り値をそのまま終了状態にする。** */
void _start(void) {
    exit(main());
}
