/* chello: C で書いた最初のユーザープログラム（C-a。ADR-0057）。
 *
 * # 何をするか
 *
 * write(1, "...", n) を int 0x80 で 1 回発行し、exit(0) で終わる。
 * Rust の hello と同じ形で、言語だけが違う。
 *
 * # crate ではない
 *
 * kernel/build.rs が gcc を 1 回呼んで単独でリンクする。cargo は関わらない。
 * リンクスクリプトは Rust のユーザープログラムと同じ user.ld である
 * （ADR-0057 の Decision 2。実測で、これを通さないと PT_LOAD が 3 つになり、
 * 1 つが像より下の 0x3ff000 へ出る）。
 *
 * # SSE を使わずに建てる
 *
 * ADR-0057 の Decision 3。-mno-sse -mno-mmx -mno-80387 で建てる。
 * ABI の選択なので、この先 libc を足すときも同じフラグで建てること。
 *
 * # libc はまだ無い
 *
 * C-a は「C で書いたプログラムが走る」までである。システムコールの殻を
 * このファイルの中に持っている。C-c で libc へ切り出す。 */

/* システムコールの番号（kernel/src/syscall.rs。Linux x86-64 から採っている）。 */
#define SYS_WRITE 1
#define SYS_EXIT  60

/* 標準出力。 */
#define STDOUT 1

/* int 0x80 の呼び出し規約は Linux x86-64 と同じ並びである
 * （rax=番号、rdi/rsi/rdx=引数、rax=返り）。
 *
 * memory の clobber は、緩衝の中身をカーネルが読むためである。 */
static long syscall3(long number, long a, long b, long c) {
    long result;
    __asm__ volatile("int $0x80"
                     : "=a"(result)
                     : "a"(number), "D"(a), "S"(b), "d"(c)
                     : "memory");
    return result;
}

static void write_all(long fd, const char *bytes, long length) {
    long done = 0;
    while (done < length) {
        long written = syscall3(SYS_WRITE, fd, (long)(bytes + done), length - done);
        if (written <= 0) {
            return;
        }
        done += written;
    }
}

/* exit は戻らない。戻ってきた場合の行き先を用意しておく（Rust の hello と
 * 同じ理由）——ud2 なら、戻ってきたことが確定的な #UD として現れる。 */
void _start(void) {
    static const char line[] = "hello from C\n";
    write_all(STDOUT, line, (long)(sizeof line - 1));
    syscall3(SYS_EXIT, 0, 0, 0);
    __asm__ volatile("ud2");
    __builtin_unreachable();
}
