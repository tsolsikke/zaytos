//! ユーザープログラムが共有する最小の包み（S11-9）。
//!
//! # crate ではない
//!
//! `hello.rs` と同じで、cargo のパッケージに属さない。**`kernel/build.rs` が
//! `rustc` を呼ぶとき、根のファイルから `mod userlib;` で取り込まれる。**
//! **`cargo fmt` と `clippy` はこのファイルを見ない。**
//!
//! # なぜ置いたか。**同じものを 2 箇所で書いたから**
//!
//! **抽出の条件は「2 つ目のファイルシステムを足すとき、または同じ操作を 2 箇所で
//! 書いたとき」である**（`kernel/src/vfs.rs` が trait を引かない理由として書いた
//! 条件を、そのまま使う）。**`ls` と `cat` が 2 箇所目になる。**
//!
//! # 既存の 4 本は `global_asm!` のまま残す
//!
//! `hello` / `fault-test` / `syscall-test` / `spawn-test` は**この包みを使わない。**
//! **あれらは ABI そのものの検算である**——`syscall-test` の doc が書いているとおり、
//! **「asm で `mov rdi, ...` と書き、アセンブラが符号化し、リンカが配置し、
//! ローダーが写像し、`iretq` が飛ばす」**ことを確かめている。
//! **包みを通すと、確かめている当のものが包みの中へ隠れる。**
//!
//! **したがって「4 箇所で書いている」は重複ではない。** 重複になるのは、
//! **検算ではないプログラムが 2 本目を数えたときである。**

#![allow(dead_code)]

/// `read` の番号（Linux と同じ）。
pub const SYS_READ: u64 = 0;
/// `write` の番号（Linux と同じ）。
pub const SYS_WRITE: u64 = 1;
/// `open` の番号（Linux と同じ）。
pub const SYS_OPEN: u64 = 2;
/// `close` の番号（Linux と同じ）。
pub const SYS_CLOSE: u64 = 3;
/// `exit` の番号（Linux と同じ）。
pub const SYS_EXIT: u64 = 60;
/// `getdents64` の番号（Linux と同じ）。
pub const SYS_GETDENTS64: u64 = 217;

/// 読み取りで開く（`O_RDONLY`）。
pub const O_RDONLY: u64 = 0;

/// 書き込みで開く（`O_WRONLY`。ADR-0037）。
pub const O_WRONLY: u64 = 1;

/// 開くと同時に長さ 0 へ切る（`O_TRUNC`。ADR-0037）。
pub const O_TRUNC: u64 = 0o1000;

/// 標準出力の fd。
pub const STDOUT: u64 = 1;
/// 標準エラー出力の fd。
pub const STDERR: u64 = 2;

/// `linux_dirent64` の欄の位置（実測。`kernel/src/syscall.rs` の写しである）。
pub const DIRENT_RECLEN_OFFSET: usize = 16;
/// `linux_dirent64` の名前の開始位置。
pub const DIRENT_NAME_OFFSET: usize = 19;

/// システムコールを 1 回発行する（引数 3 つまで）。
///
/// # 戻り値は符号つきである
///
/// **失敗は `-errno` で返る**（`ADR-0020`）。`-1..-4095` の範囲を負の整数として
/// そのまま受ける。
///
/// # Safety
///
/// 番号と引数がそのシステムコールの契約を満たすこと。**ポインタを渡す場合は、
/// カーネルが読み書きしてよい範囲を指していること。**
pub unsafe fn syscall3(number: u64, a: u64, b: u64, c: u64) -> i64 {
    let ret: i64;
    // SAFETY: 呼び出し元契約による。`int 0x80` は RAX 以外を保存して戻る
    // （スタブが `IrqContext` へ積んで復元する）。
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") number => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
        );
    }
    ret
}

/// `exit(status)`。**戻らない。**
pub fn exit(status: u64) -> ! {
    // SAFETY: `exit` は引数を 1 つ取り、戻らない。
    unsafe { syscall3(SYS_EXIT, status, 0, 0) };
    // **戻ってきた場合の行き先。** 受け皿の `ud2` へ落ちる。
    // SAFETY: `exit` が効かなかったということなので、確定的に落とす。
    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}

/// バイト列を fd へ**すべて**書く。書けた総数、または最初の `-errno` を返す。
///
/// # 繰り返す形にした
///
/// **`write` は「届いた分だけ」を返しうる**（`kernel/src/syscall.rs` の `sys_write`
/// が、途中で検証に失敗したらそこまでの数を返す）。**Linux も同じである。**
///
/// **「1 回で全部書ける」は `write` の実装に依存する仮定である。**
/// いまの実装では 1 回で全部書けるが、**その仮定を呼び出し側へ持ち込まない。**
/// **繰り返す形は、仮定が変わっても壊れない。**
pub fn write_all(fd: u64, bytes: &[u8]) -> i64 {
    let mut done = 0usize;
    while done < bytes.len() {
        // SAFETY: `bytes` は自分の像かスタックの中で、残りの長さを正しく渡す。
        let written = unsafe {
            syscall3(
                SYS_WRITE,
                fd,
                bytes.as_ptr() as u64 + done as u64,
                (bytes.len() - done) as u64,
            )
        };
        if written <= 0 {
            // **0 も失敗として扱う。** 進まないので、繰り返しても終わらない
            // （`common::ext2` の走査と同じ形で、進む量が正でなければ止める）。
            return if done == 0 { written } else { done as i64 };
        }
        done += written as usize;
    }
    done as i64
}

/// `open(path, O_RDONLY)`。**パスは NUL 終端であること。**
pub fn open_read_only(path: &[u8]) -> i64 {
    // SAFETY: `path` は NUL 終端のバイト列を指す。
    unsafe { syscall3(SYS_OPEN, path.as_ptr() as u64, O_RDONLY, 0) }
}

/// 書き込みで開き、同時に長さ 0 へ切る（`O_WRONLY|O_TRUNC`。zi-d-2）。
///
/// **カーネルが受理する 2 形のうちの一方である**（ADR-0037）。
/// **読みながら書き先を開いておくことはできない**——open の時点で切るので、
/// **読み切って閉じてから開き直す。**
pub fn open_write_truncate(path: &[u8]) -> i64 {
    // SAFETY: `path` は NUL 終端のバイト列を指す。
    unsafe { syscall3(SYS_OPEN, path.as_ptr() as u64, O_WRONLY | O_TRUNC, 0) }
}

/// `close(fd)`。
pub fn close(fd: u64) -> i64 {
    // SAFETY: 引数は fd だけである。
    unsafe { syscall3(SYS_CLOSE, fd, 0, 0) }
}

/// `read(fd, buf, len)`。
pub fn read(fd: u64, buf: &mut [u8]) -> i64 {
    // SAFETY: `buf` は自分のスタックの中で、長さを正しく渡す。
    unsafe { syscall3(SYS_READ, fd, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

/// `getdents64(fd, buf, len)`。
pub fn getdents64(fd: u64, buf: &mut [u8]) -> i64 {
    // SAFETY: `buf` は自分のスタックの中で、長さを正しく渡す。
    unsafe {
        syscall3(
            SYS_GETDENTS64,
            fd,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
        )
    }
}

/// `spawn` の番号（`ZAYTOS_PRIVATE_BASE + 4`。ZaytOS 独自）。
pub const SYS_SPAWN: u64 = 0x1004;

/// `spawn(path, argv)`。**子が終わるまで戻らない。**
///
/// 戻り値は子の終了状態（`0..=255`）か `-errno` である
/// （`kernel/src/syscall.rs` の `SYS_SPAWN`）。
///
/// # Safety
///
/// `path` が NUL 終端であること。`argv` が NULL 終端のポインタ配列で、
/// **各要素が NUL 終端の文字列を指していること。**
pub unsafe fn spawn(path: &[u8], argv: &[*const u8]) -> i64 {
    // SAFETY: 呼び出し元契約による。
    unsafe { syscall3(SYS_SPAWN, path.as_ptr() as u64, argv.as_ptr() as u64, 0) }
}

/// `argc` と `argv` を、`_start` の時点の `rsp` から読む。
///
/// # 形は Linux と同じである
///
/// `rsp` の指す先から順に、`argc`・`argv` のポインタ・NULL・`envp` のポインタ・
/// NULL・`auxv` である（`kernel/src/userland.rs` の `build_initial_stack`）。
///
/// # Safety
///
/// `stack` が `_start` の時点の `rsp` であること。
pub unsafe fn argument(stack: *const u64, index: usize) -> Option<*const u8> {
    // SAFETY: 呼び出し元契約により `stack` は初期スタックの先頭を指す。
    let argc = unsafe { *stack } as usize;
    if index >= argc {
        return None;
    }
    // SAFETY: `argv` は `argc` 本ぶん並んでおり、添字は範囲内である。
    let pointer = unsafe { *stack.add(1 + index) };
    if pointer == 0 {
        None
    } else {
        Some(pointer as *const u8)
    }
}

/// NUL 終端のバイト列の長さ（NUL を含まない）を数える。**上限つきである。**
///
/// # 上限が要る
///
/// **NUL が無い入力でも必ず止まる**（`common::ext2` の走査と同じ形で、
/// 進む量が正で上限が有限である）。
///
/// # Safety
///
/// `ptr` が読める範囲を指していること。
pub unsafe fn length_of(ptr: *const u8, limit: usize) -> usize {
    let mut length = 0usize;
    while length < limit {
        // SAFETY: 呼び出し元契約により、上限までは読める。
        if unsafe { *ptr.add(length) } == 0 {
            break;
        }
        length += 1;
    }
    length
}

/// `getdents64` が返した緩衝を歩き、名前を 1 つずつ渡す。
///
/// **`d_reclen` を頼りに進む。** 0 なら止める——**進まない量で歩き続けない。**
///
/// # Safety
///
/// `buf` が `getdents64` の返したバイト数ぶんの、正しいレコード列であること。
pub fn for_each_dirent(buf: &[u8], mut body: impl FnMut(&[u8])) {
    let mut at = 0usize;
    while at + DIRENT_NAME_OFFSET <= buf.len() {
        let reclen =
            u16::from_le_bytes([buf[at + DIRENT_RECLEN_OFFSET], buf[at + DIRENT_RECLEN_OFFSET + 1]])
                as usize;
        if reclen < DIRENT_NAME_OFFSET || at + reclen > buf.len() {
            // **壊れたレコードでは止める。** 名前の終わりが読めない。
            return;
        }
        let name_start = at + DIRENT_NAME_OFFSET;
        let mut name_end = name_start;
        while name_end < at + reclen && buf[name_end] != 0 {
            name_end += 1;
        }
        body(&buf[name_start..name_end]);
        at += reclen;
    }
}

core::arch::global_asm!(
    // **entry の手前に詰め物を置く**（`hello.rs` と同じ理由）。
    ".section .text.prepad,\"ax\"",
    ".rept 8",
    "  ud2",
    ".endr",
    ".section .text._start,\"ax\"",
    ".globl _start",
    "_start:",
    // **入口の rsp をそのまま第 1 引数へ渡す。** ここより前で何も push していない。
    // `call` が戻り番地を 1 つ積むので、呼ばれた側の rsp は 16 の倍数 + 8 になる
    // （SysV の規約どおり）。
    "  mov rdi, rsp",
    "  call zaytos_main",
    // **ここへは戻らない。** `zaytos_main` は `-> !` で、型として戻れない。
    // **それでも `call` の直後を空けない**——`exit` が効かなかったときに
    // 詰め物を走り抜けて次に置かれたものを実行する形にしない
    // （`hello.rs` の受け皿と同じ規律）。
    "  ud2",
);

// **`.userland.receiver` を持たない。**
//
// **`hello` と `syscall-test` はあの節を持つ**——`exit` が戻ってきたときの
// 行き先を、カーネル側が `entry + USER_RECEIVER_OFFSET` として主張するためである
// （`USER_PROGRAMS` の `receiver_offset`）。
//
// **こちらは `USER_PROGRAMS` に載らない**（`spawn` で起こす）ので、
// **位置を主張する相手がいない。** そして**受け皿そのものは `exit` が持っている**
// ——`userlib::exit` はシステムコールの直後に `ud2` を置いてある。
//
// **持たないほうがよい理由もある。** あの節は `USER_LOAD_ADDR + 0x800` に
// 固定で置かれるので、**`.text` がそこを越えるプログラムでは置けない。**
// `ls` の `.text` は実測で 0xEAE である。

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // **到達しない。** `no_std` のバイナリに必須なので置く。
    // 到達したら受け皿と同じ形で落とす。
    // SAFETY: 確定的に #UD にする。
    unsafe { core::arch::asm!("ud2", options(noreturn)) }
}
