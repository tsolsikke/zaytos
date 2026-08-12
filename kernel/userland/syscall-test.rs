//! `syscall-test`: ABI の契約をユーザー側から確かめるプログラム（S9-b-3-2a）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。`kernel/build.rs` が
//! `rustc` を 1 回呼んで単独でリンクし、できた ELF を kernel が `include_bytes!`
//! で抱える。**`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # 確かめているのは dispatch ではない
//!
//! カーネルには既に往復の検証がある（`verify_syscall_roundtrip`）。**あちらは
//! ユーザールーチンの機械語をカーネルが Rust で組み立てている**——オペコードを
//! 直接並べ、即値を `to_le_bytes` で埋める。**つまり ABI の符号化はカーネルの
//! 著者の手にある。**
//!
//! こちらは asm で `mov rdi, ...` と書き、**アセンブラが符号化し、リンカが配置し、
//! ローダーが写像し、`iretq` が飛ばす。** 確かめているのは dispatch ではなく、
//! **`ADR-0020` が決めた ABI の契約そのものが、その経路を通っても成り立つこと**
//! である。
//!
//! # 二重に見る
//!
//! - **カーネル側**——`dispatch` が記録した番号と 6 引数を、既知値と突き合わせる。
//!   **第 4 引数が R10 から来ていることが `ADR-0020` の要である。**
//! - **ユーザー側**——戻り値を自分で検算し、結果を `exit` の終了状態で返す。
//!   **終了状態の一致は S9-b-3-1 で判定行になっており、反証も置いてある**ので、
//!   新しい観測路を作らずに済む。
//!
//! # 終了状態の意味
//!
//! **値に意味がある。** どの検算が落ちたかは、この値でしか分からない。
//! **カーネル側の `SYSCALL_TEST_STATUS` と対になっている。**
//!
//! - `0` すべて通った
//! - `1` probe の戻り値が `PROBE_RETURN` でなかった
//! - `2` `write` が渡したバイト数を返さなかった
//! - `3` 実装していない番号が `-ENOSYS` を返さなかった
//! - `4` `open("/etc/motd", O_RDONLY)` が 0 番を返さなかった
//! - `5` `close(0)` が 0 を返さなかった
//! - `6` 閉じた直後の `open` が 0 番を返さなかった（枠が空いていない）
//! - `7` `open("/nope")` が `-ENOENT` を返さなかった
//! - `8` `open("/etc/motd", O_WRONLY)` が `-EROFS` を返さなかった
//! - `9` `close` の 2 度目が `-EBADF` を返さなかった
//! - `10` `open(NULL)` が `-EFAULT` を返さなかった
//! - `11` `/etc/motd` の全長 `read` が 18 を返さなかった
//! - `12` 読めたバイト列が既知の中身と食い違った
//! - `13` 末尾での `read` が 0 を返さなかった
//! - `14` 5 バイトの短い `read` が 5 を返さなかった、または中身が食い違った
//! - `15` 続きの `read` が残りの 13 を返さなかった、または中身が食い違った
//! - `16` ディレクトリの `read` が `-EISDIR` を返さなかった
//! - `17` 閉じた fd の `read` が `-EBADF` を返さなかった
//! - `18` `stat("/etc/motd")` が 0 を返さなかった
//! - `19` `st_size` が 18 でなかった
//! - `20` `st_mode` が通常ファイルを表していなかった
//! - `21` `st_blocks` が 8 でなかった（**512 バイト単位**）
//! - `22` `/etc` の `st_mode` がディレクトリを表していなかった
//! - `23` `stat("/nope")` が `-ENOENT` を返さなかった
//! - `24` `getdents64` がバッファを埋めなかった
//! - `25` ルートの一覧が 6 エントリでなかった
//! - `26` `d_reclen` が 8 の倍数でなかった
//! - `27` `d_type` が通常ファイルとディレクトリを分けなかった
//! - `28` 末尾での `getdents64` が 0 を返さなかった
//! - `29` 1 レコードも収まらないバッファで `-EINVAL` を返さなかった
//!
//! # 中身の突き合わせは `hello` の `write` と同じ形である
//!
//! **既知のバイト列と一致することを言う。** カーネル側は種のファイルを
//! `include_bytes!` で持っており、**こちらはその写しを持つ。**
//! **食い違えば 12 番か 15 番の検算が落ちる**ので、静かには残らない。
//!
//! # `open` はこのプログラムの `.rodata` のパスを渡す
//!
//! **カーネルが受け取るのはユーザー空間のポインタである。** `UserSlice` と
//! 窓（`S9-b-3-2b` で 1 つに畳んだもの）がそのまま効くことを、**この経路が
//! 実際に通ることで確かめている。**
//!
//! # 定数はカーネルの写しである
//!
//! このファイルは crate に属さないので、`kernel/src/syscall.rs` の定数を
//! `use` できない。**同じ値を 2 か所で持つが、食い違えば判定行が落ちる**ので
//! 静かには残らない（`hello.rs` の `.org` と `HELLO_UD2_OFFSET` の関係と同じ）。

#![no_std]
#![no_main]

/// 検証用 probe の番号（`ZAYTOS_PRIVATE_BASE`）。
const PROBE_NUMBER: u32 = 0x1000;
/// probe が返す既知の値。
const PROBE_RETURN: u32 = 0x00C0_FFEE;
/// probe へ渡す 6 引数。**レジスタごとに区別できる値である。**
const PROBE_ARG0: u32 = 0x1111_1111;
const PROBE_ARG1: u32 = 0x2222_2222;
const PROBE_ARG2: u32 = 0x3333_3333;
const PROBE_ARG3: u32 = 0x4444_4444;
const PROBE_ARG4: u32 = 0x5555_5555;
const PROBE_ARG5: u32 = 0x6666_6666;
/// RCX へ入れる番兵。**RCX は引数ではない**（`ADR-0020`。第 4 引数は R10）。
const SENTINEL_RCX: u32 = 0xCCCC_CCCC;
/// `write` の番号（Linux と同じ 1）。
const SYS_WRITE: u32 = 1;
/// `exit` の番号（Linux と同じ 60）。
const SYS_EXIT: u32 = 60;
/// **永久に実装しない番号。** `-ENOSYS` が返ることを確かめるための的である。
const NEVER_IMPLEMENTED: u32 = 0x10FF;
/// `-ENOSYS`。失敗は `-errno` で返る（`ADR-0020`）。
const MINUS_ENOSYS: i32 = -38;
/// 送るバイト列の長さ。
const MESSAGE_LEN: u32 = 24;
/// `open` の番号（Linux と同じ 2）。
const SYS_OPEN: u32 = 2;
/// `close` の番号（Linux と同じ 3）。
const SYS_CLOSE: u32 = 3;
/// 読み取りで開く（`O_RDONLY`）。
const O_RDONLY: u32 = 0;
/// 書き込みで開く（`O_WRONLY`）。**読み取り専用なので拒まれるはずである。**
const O_WRONLY: u32 = 1;
/// `-ENOENT`（そのパスは無い）。
const MINUS_ENOENT: i32 = -2;
/// `-EBADF`（そのファイルディスクリプタは開いていない）。
const MINUS_EBADF: i32 = -9;
/// `-EROFS`（読み取り専用のファイルシステム）。
const MINUS_EROFS: i32 = -30;
/// `-EFAULT`（不正なアドレス）。
const MINUS_EFAULT: i32 = -14;
/// `read` の番号（Linux と同じ 0）。
const SYS_READ: u32 = 0;
/// `-EISDIR`（ディレクトリに対して許されない操作）。
const MINUS_EISDIR: i32 = -21;
/// `/etc/motd` の長さ。**種のファイルと同じでなければ検算が落ちる。**
const MOTD_LEN: u32 = 18;
/// 短い `read` で読む長さ。
const MOTD_HEAD: u32 = 5;
/// その続きに残る長さ。
const MOTD_TAIL: u32 = 13;
/// 末尾を越えて要求する長さ。**`i_size` で切られるはずである。**
const OVER_READ: u32 = 100;
/// `stat` の番号（Linux と同じ 4）。
const SYS_STAT: u32 = 4;
/// `struct stat` の `st_mode` の位置（実測）。
const STAT_MODE_OFFSET: u32 = 24;
/// `struct stat` の `st_size` の位置（実測）。
const STAT_SIZE_OFFSET: u32 = 48;
/// `struct stat` の `st_blocks` の位置（実測）。
const STAT_BLOCKS_OFFSET: u32 = 64;
/// `st_mode` のうちファイル種別を表すビット。
const MODE_FORMAT_MASK: u32 = 0xF000;
/// 種別: 通常ファイル。
const MODE_REGULAR: u32 = 0x8000;
/// 種別: ディレクトリ。
const MODE_DIRECTORY: u32 = 0x4000;
/// `/etc/motd` が占める 512 バイト単位のブロック数。**4096 の 1 ブロック分である。**
const MOTD_BLOCKS: u32 = 8;
/// `getdents64` の番号（Linux と同じ 217）。
const SYS_GETDENTS64: u32 = 217;
/// ルートディレクトリのエントリ数（`. .. lost+found bin data etc`）。
const ROOT_ENTRIES: u32 = 6;
/// `linux_dirent64` の `d_reclen` の位置。
const DIRENT_RECLEN_OFFSET: u32 = 16;
/// `linux_dirent64` の `d_type` の位置。
const DIRENT_TYPE_OFFSET: u32 = 18;
/// `d_type`: ディレクトリ。
const DT_DIR: u32 = 4;
/// `d_type`: 通常ファイル。
const DT_REG: u32 = 8;
/// **1 レコードも収まらない大きさ。** 固定部だけで 19 バイト要る。
const TINY_BUFFER: u32 = 16;
/// `-EINVAL`。
const MINUS_EINVAL: i32 = -22;

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",

    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // --- 1. probe。6 引数を規約どおりのレジスタへ置く ---
    "  mov edi, {arg0}",
    "  mov esi, {arg1}",
    "  mov edx, {arg2}",
    "  mov r10d, {arg3}",
    "  mov r8d, {arg4}",
    "  mov r9d, {arg5}",
    // **RCX は引数ではない。** 番兵を置くので、カーネルが第 4 引数を RCX から
    // 読んでいれば、記録される値が食い違う。
    "  mov ecx, {sentinel}",
    "  mov eax, {probe}",
    "  int 0x80",
    "  cmp rax, {probe_ret}",
    "  mov edi, 1",
    "  jne 9f",

    // --- 2. write。渡したバイト数が返るはず ---
    "  mov eax, {sys_write}",
    "  mov edi, 1",
    "  lea rsi, [rip + MESSAGE]",
    "  mov edx, {msg_len}",
    "  int 0x80",
    "  cmp rax, {msg_len}",
    "  mov edi, 2",
    "  jne 9f",

    // --- 3. 実装していない番号。-ENOSYS が返るはず ---
    "  mov eax, {never}",
    "  int 0x80",
    "  cmp rax, {minus_enosys}",
    "  mov edi, 3",
    "  jne 9f",

    // --- 4. open("/etc/motd", O_RDONLY)。最初の fd は 0 のはず ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 4",
    "  jne 9f",

    // --- 5. close(0)。0 が返るはず ---
    "  mov eax, {sys_close}",
    "  xor edi, edi",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 5",
    "  jne 9f",

    // --- 6. もう一度 open。**閉じた枠が空いているので、また 0 のはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 6",
    "  jne 9f",

    // --- 7. 無いパス。-ENOENT が返るはず ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MISSING_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_enoent}",
    "  mov edi, 7",
    "  jne 9f",

    // --- 8. 書き込みで開く。**読み取り専用なので -EROFS のはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_wronly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_erofs}",
    "  mov edi, 8",
    "  jne 9f",

    // --- 9. 同じ fd を 2 度閉じる。**2 度目は -EBADF のはず** ---
    "  mov eax, {sys_close}",
    "  xor edi, edi",
    "  int 0x80",
    "  mov eax, {sys_close}",
    "  xor edi, edi",
    "  int 0x80",
    "  cmp rax, {minus_ebadf}",
    "  mov edi, 9",
    "  jne 9f",

    // --- 10. パスに NULL を渡す。**窓の下端より下なので -EFAULT のはず** ---
    // **`open` がユーザーポインタを検証していることの、否定側の観測である。**
    "  mov eax, {sys_open}",
    "  xor edi, edi",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  cmp rax, {minus_efault}",
    "  mov edi, 10",
    "  jne 9f",

    // --- 11/12. /etc/motd を全部読み、既知のバイト列と突き合わせる ---
    // **読み込み先はユーザースタックである**（このプログラムに書ける区画は無い）。
    "  sub rsp, 64",
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_len}",
    "  int 0x80",
    "  cmp rax, {motd_len}",
    "  mov edi, 11",
    "  jne 9f",
    "  cld",
    "  mov rsi, rsp",
    "  lea rdi, [rip + MOTD_BYTES]",
    "  mov ecx, {motd_len}",
    "  repe cmpsb",
    "  mov edi, 12",
    "  jne 9f",

    // --- 13. 末尾での read。**0 が返るはず（EOF）** ---
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_len}",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 13",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",

    // --- 14/15. 短く読んでから続きを読む。**位置が進んでいること** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_head}",
    "  int 0x80",
    "  cmp rax, {motd_head}",
    "  mov edi, 14",
    "  jne 9f",
    "  cld",
    "  mov rsi, rsp",
    "  lea rdi, [rip + MOTD_BYTES]",
    "  mov ecx, {motd_head}",
    "  repe cmpsb",
    "  mov edi, 14",
    "  jne 9f",
    // **末尾を越えて要求する。** 残りの 13 だけが返るはず。
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {over_read}",
    "  int 0x80",
    "  cmp rax, {motd_tail}",
    "  mov edi, 15",
    "  jne 9f",
    "  cld",
    "  mov rsi, rsp",
    "  lea rdi, [rip + MOTD_REST]",
    "  mov ecx, {motd_tail}",
    "  repe cmpsb",
    "  mov edi, 15",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",

    // --- 16. ディレクトリを read。**-EISDIR が返るはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + ETC_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_head}",
    "  int 0x80",
    "  cmp rax, {minus_eisdir}",
    "  mov edi, 16",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",

    // --- 17. 閉じた fd を read。**-EBADF が返るはず** ---
    "  mov eax, {sys_read}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {motd_head}",
    "  int 0x80",
    "  cmp rax, {minus_ebadf}",
    "  mov edi, 17",
    "  jne 9f",
    "  add rsp, 64",

    // --- 18..21. stat("/etc/motd")。**埋まる欄を突き合わせる** ---
    // `struct stat` は 144 バイトなので、スタックへ余裕を取る。
    "  sub rsp, 192",
    "  mov eax, {sys_stat}",
    "  lea rdi, [rip + MOTD_PATH]",
    "  mov rsi, rsp",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 18",
    "  jne 9f",
    "  mov rax, [rsp + {stat_size_off}]",
    "  cmp rax, {motd_len}",
    "  mov edi, 19",
    "  jne 9f",
    "  mov eax, [rsp + {stat_mode_off}]",
    "  and eax, {mode_mask}",
    "  cmp eax, {mode_regular}",
    "  mov edi, 20",
    "  jne 9f",
    "  mov rax, [rsp + {stat_blocks_off}]",
    "  cmp rax, {motd_blocks}",
    "  mov edi, 21",
    "  jne 9f",

    // --- 22. /etc は ディレクトリ ---
    "  mov eax, {sys_stat}",
    "  lea rdi, [rip + ETC_PATH]",
    "  mov rsi, rsp",
    "  int 0x80",
    "  mov eax, [rsp + {stat_mode_off}]",
    "  and eax, {mode_mask}",
    "  cmp eax, {mode_directory}",
    "  mov edi, 22",
    "  jne 9f",

    // --- 23. 無いパス。-ENOENT が返るはず ---
    "  mov eax, {sys_stat}",
    "  lea rdi, [rip + MISSING_PATH]",
    "  mov rsi, rsp",
    "  int 0x80",
    "  cmp rax, {minus_enoent}",
    "  mov edi, 23",
    "  jne 9f",
    "  add rsp, 192",

    // --- 24..27. ルートを getdents64 で読み、レコードを歩く ---
    // r12=fd、r13=バッファ先頭、r14=書かれたバイト数、r15=歩いた位置。
    "  sub rsp, 1024",
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + ROOT_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_getdents}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, 1024",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 24",
    "  jle 9f",
    "  mov r14, rax",
    "  mov r13, rsp",
    "  xor r15, r15",          // 歩いた位置
    "  xor ebx, ebx",          // 数えたエントリ
    "  xor ebp, ebp",          // 見た d_type の論理和
    "20:",
    "  cmp r15, r14",
    "  jae 21f",
    // d_reclen が 8 の倍数か。
    "  movzx eax, word ptr [r13 + r15 + {reclen_off}]",
    "  test eax, 7",
    "  mov edi, 26",
    "  jne 9f",
    // 進まないレコードは無いはず（**歩きが止まる**）。
    "  test eax, eax",
    "  mov edi, 26",
    "  je 9f",
    // d_type を集める。
    "  movzx ecx, byte ptr [r13 + r15 + {type_off}]",
    "  or ebp, ecx",
    "  inc ebx",
    "  add r15, rax",
    "  jmp 20b",
    "21:",
    "  cmp ebx, {root_entries}",
    "  mov edi, 25",
    "  jne 9f",
    // **ルートは全部ディレクトリである**（`. .. lost+found bin data etc`）。
    "  cmp ebp, {dt_dir}",
    "  mov edi, 27",
    "  jne 9f",

    // --- 27 の本命。**/data は `. ..` と通常ファイル 2 本なので、
    // `d_type` が DT_DIR と DT_REG の両方になる。** ルートだけでは分かれない。
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + DATA_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_getdents}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, 1024",
    "  int 0x80",
    "  mov r14, rax",
    "  mov r13, rsp",
    "  xor r15, r15",
    "  xor ebp, ebp",
    "22:",
    "  cmp r15, r14",
    "  jae 23f",
    "  movzx eax, word ptr [r13 + r15 + {reclen_off}]",
    "  test eax, eax",
    "  mov edi, 26",
    "  je 9f",
    "  movzx ecx, byte ptr [r13 + r15 + {type_off}]",
    "  or ebp, ecx",
    "  add r15, rax",
    "  jmp 22b",
    "23:",
    "  cmp ebp, {dt_both}",
    "  mov edi, 27",
    "  jne 9f",

    // --- 28. 末尾での getdents64。**0 が返るはず** ---
    "  mov eax, {sys_getdents}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, 1024",
    "  int 0x80",
    "  test rax, rax",
    "  mov edi, 28",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",

    // --- 29. 1 レコードも収まらないバッファ。**-EINVAL が返るはず** ---
    "  mov eax, {sys_open}",
    "  lea rdi, [rip + ROOT_PATH]",
    "  mov esi, {o_rdonly}",
    "  xor edx, edx",
    "  int 0x80",
    "  mov r12, rax",
    "  mov eax, {sys_getdents}",
    "  mov rdi, r12",
    "  mov rsi, rsp",
    "  mov edx, {tiny}",
    "  int 0x80",
    "  cmp rax, {minus_einval}",
    "  mov edi, 29",
    "  jne 9f",
    "  mov eax, {sys_close}",
    "  mov rdi, r12",
    "  int 0x80",
    "  add rsp, 1024",

    // すべて通った。
    "  xor edi, edi",

    // --- 4. exit(status)。ここから戻らない ---
    "9:",
    "  mov eax, {sys_exit}",
    "  int 0x80",
    // **`exit` が戻ってきたときの受け皿**（`hello.rs` と同じ規律）。位置を
    // `.org` で固定してあるので、ここへ落ちたことが RIP で分かる。
    ".org 0x800, 0x90",
    "  ud2",

    ".section .rodata",
    "MESSAGE:",
    "  .ascii \"syscall-test wrote this\\n\"",
    "MOTD_PATH:",
    "  .asciz \"/etc/motd\"",
    "MISSING_PATH:",
    "  .asciz \"/nope\"",
    "ETC_PATH:",
    "  .asciz \"/etc\"",
    "ROOT_PATH:",
    "  .asciz \"/\"",
    "DATA_PATH:",
    "  .asciz \"/data\"",
    // **`/etc/motd` の中身の写し。** 種のファイルと食い違えば 12 番が落ちる。
    "MOTD_BYTES:",
    "  .ascii \"welco\"",
    "MOTD_REST:",
    "  .ascii \"me to ZaytOS\\n\"",

    arg0 = const PROBE_ARG0,
    arg1 = const PROBE_ARG1,
    arg2 = const PROBE_ARG2,
    arg3 = const PROBE_ARG3,
    arg4 = const PROBE_ARG4,
    arg5 = const PROBE_ARG5,
    sentinel = const SENTINEL_RCX,
    probe = const PROBE_NUMBER,
    probe_ret = const PROBE_RETURN,
    sys_write = const SYS_WRITE,
    msg_len = const MESSAGE_LEN,
    never = const NEVER_IMPLEMENTED,
    minus_enosys = const MINUS_ENOSYS,
    sys_exit = const SYS_EXIT,
    sys_open = const SYS_OPEN,
    sys_close = const SYS_CLOSE,
    o_rdonly = const O_RDONLY,
    o_wronly = const O_WRONLY,
    minus_enoent = const MINUS_ENOENT,
    minus_ebadf = const MINUS_EBADF,
    minus_erofs = const MINUS_EROFS,
    minus_efault = const MINUS_EFAULT,
    sys_read = const SYS_READ,
    minus_eisdir = const MINUS_EISDIR,
    motd_len = const MOTD_LEN,
    motd_head = const MOTD_HEAD,
    motd_tail = const MOTD_TAIL,
    over_read = const OVER_READ,
    sys_stat = const SYS_STAT,
    stat_mode_off = const STAT_MODE_OFFSET,
    stat_size_off = const STAT_SIZE_OFFSET,
    stat_blocks_off = const STAT_BLOCKS_OFFSET,
    mode_mask = const MODE_FORMAT_MASK,
    mode_regular = const MODE_REGULAR,
    mode_directory = const MODE_DIRECTORY,
    motd_blocks = const MOTD_BLOCKS,
    sys_getdents = const SYS_GETDENTS64,
    root_entries = const ROOT_ENTRIES,
    reclen_off = const DIRENT_RECLEN_OFFSET,
    type_off = const DIRENT_TYPE_OFFSET,
    dt_dir = const DT_DIR,
    dt_both = const DT_DIR | DT_REG,
    tiny = const TINY_BUFFER,
    minus_einval = const MINUS_EINVAL,
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // 到達しない。no_std のバイナリに必須なので置くだけである。
    loop {}
}
