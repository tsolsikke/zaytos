//! システムコール（`int 0x80`）の入口（M5-f-1）。
//!
//! ADR-0020 のとおり、レジスタ規約は Linux x86-64 に合わせる。番号は RAX、
//! 戻り値は RAX、第 1〜6 引数は RDI/RSI/RDX/R10/R8/R9、失敗は `-errno`
//! （`-1..-4095`）。**第 4 引数は RCX ではなく R10** である。`int 0x80` の間は
//! RCX/R11 は実際には保存されるが、`syscall`/`sysret` へ移る段でこれらは命令が
//! 破壊するため、**保存に依存しない**（ユーザー側ラッパはクロバー扱いにする）。
//!
//! # 入口の機構
//!
//! ベクタ 0x80 の IDT ゲートを DPL=3 の割り込みゲートにし、[`crate::idt`] の
//! `zaytos_syscall_stub` へ向ける。スタブは IRQ スタイルの復元経路を写した
//! `zaytos_syscall_common` へ jmp し、GPR 15 本を退避して [`syscall_entry`] を
//! 呼ぶ。Ring 3 からの `int 0x80` は特権変化（3→0）なので、CPU が TSS.RSP0 の
//! スタックへ自動で切り替える（M5-c/d で更新している RSP0 がここで効く）。
//!
//! `irq_entry` とは経路を分けてある。本番 IRQ 経路へ「ソフトウェア割り込みか」の
//! 分岐を足さない方針（ADR-0018 Addendum 3）と揃え、戻り値の RAX 書き戻しという
//! syscall 固有の振る舞いを IRQ 側へ持ち込まないためである。
//!
//! # M5-f-1 の範囲
//!
//! ディスパッチャは検証用の probe システムコール 1 つだけを持つ（M5-f-1-2）。
//! probe は 6 引数と番号を静的領域へ記録し、既知の戻り値 [`PROBE_RETURN`] を返す。
//! これにより「6 引数が規約どおり届き、戻り値が RAX で Ring 3 へ返る」ことを実証
//! する。ユーザーポインタを取るシステムコールは後段（M5-f-2）で足す。
//!
//! # 破壊 feature（M5-f-1-2）
//!
//! - `syscall-test-arg4-rcx`: 第 4 引数を `context.r10` でなく `context.rcx` から
//!   読む。R10 規約の実証（記録した第 4 引数が期待値と食い違う）。
//! - `syscall-test-drop-retval`: 戻り値の `context.rax` 書き戻しを落とす。ユーザーが
//!   期待した戻り値を受け取れない（ユーザースタックへ store した値が食い違う）。
//! - `syscall-test-gate-dpl0`: ゲートを DPL=0 にする（[`crate::idt`] 側）。Ring 3 から
//!   の `int 0x80` がゲート DPL<CPL で #GP になり、`syscall_entry` に到達しない。

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use common::addr::{DirectMap, PhysAddr};

use crate::idt::context::IrqContext;

/// `-ENOSYS`（未実装システムコール）の errno。失敗は `-errno` で返す。
pub const ENOSYS: i64 = 38;

/// `-EFAULT`（不正なアドレス）の errno。ユーザーポインタ検証に落ちたとき返す。
pub const EFAULT: i64 = 14;

/// `-EINVAL`（引数が不正）の errno（S9-a）。**アドレスは正しいが、値が受け付け
/// られない**ときに返す。現在の用途は [`CHECKSUM_BUF_LEN`] の超過だけである。
///
/// 値は Linux と同じ 22 である（ADR-0020 の Addendum で「errno の値を Linux に
/// 合わせる」と決めてある）。
pub const EINVAL: i64 = 22;

/// ZaytOS 独自のシステムコール番号の基点（S9-a）。
///
/// # なぜ Linux の番号表から離すのか
///
/// ADR-0020 の Addendum で「番号の割り当ては Linux x86-64 から採る」と決めた。
/// **`read`=0 や `write`=1 のように Linux に対応するものがある呼び出しは、その
/// 番号を使う。** 問題は、対応するものが無い呼び出しである。下記の検証用
/// システムコールは ZaytOS 固有で、Linux に相当するものが未来にも現れない。
///
/// **かつては 0x2A・0x2B・0x2C に置いており、Linux の 42（`connect`）・
/// 43（`accept`）・44（`sendto`）と衝突していた。** 番号表の中の空きに置くと、
/// Linux がそこを埋めた時点で衝突する（歴史的に未実装のまま空いている番号も、
/// 将来 Linux が再利用しうる）。**表の中に安全な空きは無い。**
///
/// そこで表の外へまとめる。Linux x86-64 の番号は現在 500 未満で、増え方は年に
/// 数本である。**0x1000（4096）なら当面ぶつからない。** x32 ABI が使う
/// `0x4000_0000` のビットとも重ならない。
///
/// **独自の呼び出しを足すときは、必ずこの基点より上に置くこと。**
pub const ZAYTOS_PRIVATE_BASE: u64 = 0x1000;

/// ユーザーポインタを取る検証用システムコールの番号（M5-f-2-1）。
/// 第 1 引数(RDI)=buf、第 2 引数(RSI)=len。範囲が Ring 3 からアクセス可能なら 0、
/// 不可なら -EFAULT を返す（**この段はバイトを読まない**。copy は M5-f-2-2）。
pub const SYS_CHECK_PTR: u64 = ZAYTOS_PRIVATE_BASE + 1;

/// ユーザーバッファのバイト総和（チェックサム）を返すシステムコールの番号
/// （M5-f-2-2）。第 1 引数(RDI)=buf、第 2 引数(RSI)=len。範囲を検証してから
/// 範囲内バイトを読み総和を返す。不正な範囲なら -EFAULT、長さが
/// [`CHECKSUM_BUF_LEN`] を超えるなら -EINVAL。
pub const SYS_CHECKSUM: u64 = ZAYTOS_PRIVATE_BASE + 2;

/// SYS_CHECKSUM がユーザーバイトを読み込む固定カーネルバッファの大きさ。
///
/// これを超える len は -EINVAL で弾く。**意味的には「引数の値が受け付けられない」
/// のであって、ポインタ不正（EFAULT = Bad address）ではない。** S9-a より前は
/// errno が 2 つしか無く -EFAULT で代用していた。
pub const CHECKSUM_BUF_LEN: usize = 64;

/// ユーザーポインタとして受理する下限（S9-b-3-2b）。**方針である。**
///
/// # 理由が変わった。値は変わっていない
///
/// **S9-b-1 でこの値を置いた理由は、起動順の偶然だった。** ポインタ検証の battery は
/// 恒等除去より前に走るので、その時点の低位 VA にはカーネルの恒等写像が居る。
/// 下限を 0 にすると `0x100000`（カーネル像）が範囲の検査を通ってしまい、
/// **U=1 の判定だけが拒否の根拠になる**（`validate-skip-us` の破壊で受理された）。
///
/// **S9-b-3-2b で窓を 1 つに畳んだので、その理由は当たらなくなった。** 起動時の
/// battery が使う窓は本番の空間のユーザーサブツリー（`PML4[1]` = 512 GiB 以上）で、
/// カーネル像はそもそも窓の外である。
///
/// **それでも 0 にしない。** null 近傍を**範囲の側でも**拒む層を残す。Linux の
/// `mmap_min_addr` が低位を空けておくのと同じ向きで、**層を 1 枚減らすには
/// 減らす理由が要る。** 減らす理由が無い。
///
/// **同じ値を、違う根拠で持っている。**
pub const USER_MIN_ADDR: u64 = 0x40_0000;

/// PML4 の添字 1 つ分が覆う仮想範囲の大きさ（512 GiB）。
const PML4_ENTRY_SPAN: u64 = 1 << 39;

/// ユーザーサブツリーの添字から、ポインタ検証の窓を導く（S9-b-3-2b）。
///
/// 返すのは `[start, end)` で、`start` は [`USER_MIN_ADDR`] で床を打ってある。
///
/// # 窓は 1 つである
///
/// **S9-b-1 から S9-b-3-2a までは 2 つあった**（起動時の検証用と、ユーザー
/// プログラム用）。どちらか一方に収まっていれば受理する形で、**またぐ範囲を
/// 受理しない条件を明示的に書く必要があった。**
///
/// **1 つに畳むと、その条件は消える。** またぐ範囲が受理されないのは、
/// **窓が 1 つしかないからである**（書かれた条件ではなく、構造の帰結になった）。
///
/// # 窓が有限であることが、走査の停止性を与えている
///
/// 「下位半分すべて」へ広げてはならない。広げると長さの上限が消え、
/// `over-long` のような呼び出しでページ走査が何百万回もまわる。
pub const fn window_for_subtree(index: usize) -> (u64, u64) {
    let start = (index as u64) * PML4_ENTRY_SPAN;
    let end = start + PML4_ENTRY_SPAN;
    if start < USER_MIN_ADDR {
        (USER_MIN_ADDR, end)
    } else {
        (start, end)
    }
}

/// `write(fd, buf, len)`（S9-b-1）。**Linux の番号 1 をそのまま使う**
/// （ADR-0020 の Addendum。対応するものがある呼び出しは Linux の番号を採る）。
///
/// 現在の実装は `fd` を見ず、**バイト列を静的領域へ記録して長さを返すだけである。**
/// シリアルへは出さない。`syscall_entry` は出力しないという既存の方針
/// （例外・IRQ ハンドラと同じ）に従い、**観測は畳んで戻った後に呼び出し側が
/// 記録越しに行う。**
pub const SYS_WRITE: u64 = 1;

/// [`SYS_WRITE`] が記録するバイト数の上限。
pub const WRITE_BUF_LEN: usize = 64;

/// `exit(status)`（S9-b-3-1）。**Linux の番号 60 をそのまま使う。**
///
/// # `exit_group`（231）は採らない
///
/// あちらは「呼んだスレッドが属するスレッドグループ全体を終わらせる」呼び出しで、
/// **ZaytOS にはスレッドの概念が無い。** 番号を用意しても、`exit` と区別できる
/// 振る舞いが書けない。**同じ振る舞いの入口を 2 つ置くと、どちらが正なのかが
/// 呼び出し側にも実装側にも決まらない。** スレッドを作る段で足す。
///
/// # 戻らない
///
/// **[`dispatch`] の戻り値では「戻らない」を表せない**ので、記録だけをあちらで
/// 行い、**Ring 3 へ返らない分岐は [`syscall_entry`] が持つ**（あちらの
/// 「exit は出口を通らない」の節）。
pub const SYS_EXIT: u64 = 60;

/// 検証用 probe システムコールの番号（ZaytOS 独自。[`ZAYTOS_PRIVATE_BASE`]）。
pub const PROBE_NUMBER: u64 = ZAYTOS_PRIVATE_BASE;

/// **永久に実装しない番号**（S9-b-3-2a）。`-ENOSYS` の的である。
///
/// # なぜ「空いている番号」で済ませないか
///
/// **未実装の番号は、いつか実装される。** そのとき、`-ENOSYS` が返ることを
/// 確かめていた検査は静かに別のものを見はじめる（戻り値が変わるので落ちはするが、
/// **落ちた理由が「実装したから」だと分かる材料がどこにも無い**）。
///
/// **予約しておけば、実装しようとした人がこの doc を読む。** [`ZAYTOS_PRIVATE_BASE`]
/// の上に置くので、Linux の番号表とも衝突しない。
pub const SYS_NEVER_IMPLEMENTED: u64 = ZAYTOS_PRIVATE_BASE + 0xFF;

/// probe が返す既知の戻り値。ユーザーはこれを RAX で受け取り、ユーザースタックへ
/// store する。カーネルが畳み後に読み戻して一致を確かめることで、戻り値が RAX 経由で
/// Ring 3 へ渡ったことを実証する。`-errno` の範囲（`-1..-4095`）と紛れない値にする。
pub const PROBE_RETURN: u64 = 0x00C0_FFEE;

/// probe の呼び出しでユーザーが各引数レジスタ（RDI/RSI/RDX/R10/R8/R9）へ入れる
/// 既知値。**レジスタごとに区別できる値**にする（第 4 引数を R10 でなく RCX から
/// 読む破壊が、記録した第 4 引数の食い違いとして必ず現れるように）。
pub const PROBE_ARGS: [u64; 6] = [
    0x1111_1111,
    0x2222_2222,
    0x3333_3333,
    0x4444_4444,
    0x5555_5555,
    0x6666_6666,
];

/// probe の呼び出しでユーザーが RCX へ入れる番兵。RCX は引数ではない（クロバー扱い）。
/// `syscall-test-arg4-rcx` が第 4 引数を RCX から読むと、この値が第 4 引数として
/// 記録され、`PROBE_ARGS[3]` と決定的に食い違う。
pub const SENTINEL_RCX: u64 = 0xCCCC_CCCC;

/// [`SYS_WRITE`] が最後に受け取った fd。
static WRITE_FD: AtomicU64 = AtomicU64::new(0);
/// [`SYS_WRITE`] が最後に記録したバイト数。
static WRITE_LEN: AtomicU64 = AtomicU64::new(0);
/// [`SYS_WRITE`] が最後に記録したバイト列。
static WRITE_BUF: [AtomicU8; WRITE_BUF_LEN] = [const { AtomicU8::new(0) }; WRITE_BUF_LEN];

/// 今 Ring 3 が使っている窓の下端と上端（S9-b-3-2b）。
///
/// # 据えるのは Ring 3 へ落ちる側である
///
/// [`crate::ring3::enter`] が遠征の間だけ据え、戻るときに元へ戻す。**据えないまま
/// ここへ来ることはない**——[`validate_user_range`] を呼ぶのは [`dispatch`] だけで、
/// あちらは `syscall_entry` からしか来ず、`syscall_entry` は Ring 3 からしか来ない。
///
/// # 既定値は空の窓である
///
/// `(0, 0)` は**どんな長さ 1 以上の範囲も受理しない。** 据え忘れたときに黙って
/// 通る形にしない。**安全側は「窓が無ければ何も通さない」である。**
static USER_WINDOW_START: AtomicU64 = AtomicU64::new(0);
static USER_WINDOW_END: AtomicU64 = AtomicU64::new(0);

/// [`PROBE_NUMBER`] を受け取ったか（S9-b-3-2a）。
static PROBE_INVOKED: AtomicBool = AtomicBool::new(false);
/// [`PROBE_NUMBER`] の呼び出しで届いた 6 引数（S9-b-3-2a）。
///
/// # なぜ [`LAST_ARGS`] で足りないか
///
/// あちらは**直近の呼び出し**を持つ。**起動時の battery は 1 回しか発行しないので
/// 足りていた**が、ユーザープログラムは 4 回発行する（probe・`write`・未実装の
/// 番号・`exit`）。**最後の `exit` で上書きされ、probe の引数は残らない。**
///
/// **番号ごとに要るのではなく、「主張したい 1 回」が要る。** 主張は
/// 「6 引数が `ADR-0020` の規約どおりに届くこと」で、それを言えるのは probe の
/// 回だけである。
static PROBE_SEEN_ARGS: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// [`SYS_EXIT`] を受け取ったか（S9-b-3-1）。**呼び出し側は、遠征から戻った理由が
/// 終了なのか畳みなのかをこれで区別する。**
static PROCESS_EXITED: AtomicBool = AtomicBool::new(false);
/// [`SYS_EXIT`] が受け取った終了状態（RDI）。[`PROCESS_EXITED`] が真のときだけ意味を持つ。
static PROCESS_EXIT_STATUS: AtomicU64 = AtomicU64::new(0);

/// `syscall_entry` が呼ばれた回数（会計用）。
static INVOCATION_COUNT: AtomicU64 = AtomicU64::new(0);
/// 直近に受け取った番号（RAX）。往復検証で PROBE_NUMBER と突き合わせる。
static LAST_NUMBER: AtomicU64 = AtomicU64::new(0);
/// 直近に受け取った 6 引数（RDI/RSI/RDX/R10/R8/R9）。PROBE_ARGS と突き合わせる。
static LAST_ARGS: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
/// `syscall_entry` が走ったときの RSP（RSP0 スタックのはず）。読み戻し検証に使う。
static HANDLER_RSP: AtomicU64 = AtomicU64::new(0);
/// 入場時点の [`crate::ring3`] の「今 Ring 3 にいる」の値（S8-b）。**Ring 3 から
/// 来たのなら真のはず**で、往復検証が突き合わせる。
static IN_RING3_AT_ENTRY: AtomicBool = AtomicBool::new(false);

/// 検証済みのユーザー範囲を表す証明トークン（M5-f-2-2、案T）。
///
/// **フィールドは private で、公開コンストラクタを持たない。** 構築できるのは同一
/// モジュール内の [`validate_user_range`] だけである。したがって [`copy_from_user`] が
/// `&UserSlice` を要求することで、**モジュール外の全呼び出し元に対しては「検証を経ないと
/// ユーザーメモリを読めない」ことが型で保証される。**
///
/// # 型で保証される範囲と、規律で守る範囲
///
/// この保証はモジュール境界に依存する。同一 `syscall.rs` モジュール内からは private
/// フィールドに触れるため `UserSlice { .. }` を直接構築できてしまう。したがって:
/// - モジュール外: 検証を経ないと `UserSlice` が作れない（型で保証）。
/// - モジュール内: 直接構築は `copy-skip-validate` 破壊 feature 専用であり、通常コードでは
///   行わない（この規律は型ではなくレビューで守る）。`copy-skip-validate` はまさにこの境界を
///   突く破壊である。
///
/// # 有効期間
///
/// `UserSlice` は**同一 syscall 内・同一アドレス空間でのみ有効**。跨いで保持しない
/// （static 等に置かない）。higher-half B 後のプロセス別アドレス空間では、トークンは
/// 「その CR3 の下でのみ有効」になるため、CR3 を跨いで使わない制約を f-3 で型（世代/CR3 を
/// 持たせる等）または doc で担保する（再確認の申し送り。verification-coverage 参照）。
pub struct UserSlice {
    buf: u64,
    len: u64,
}

impl UserSlice {
    /// 範囲の先頭アドレス（ユーザー VA）。
    pub fn buf(&self) -> u64 {
        self.buf
    }
    /// 範囲の長さ（バイト）。
    pub fn len(&self) -> u64 {
        self.len
    }
    /// 範囲が空（len==0）か。len==0 は常に受理されるので有効なトークンとして存在しうる。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// 指定した [buf, buf+len) が Ring 3 からアクセス可能かを、**カーネルが読み書きに
/// 踏み込む前に**判定し、可なら証明トークン [`UserSlice`] を返す（M5-f-2-1 / M5-f-2-2）。
///
/// **len==0 は常に受理する。** 0 バイトのアクセスは buf を問わず安全であり、この
/// 契約はこの検証器を共有する全 syscall が継承する（呼び出し側で短絡しない）。
/// それ以外は次を満たすとき `Some`:
///   (a) 長さの加算にオーバーフローが無い（`checked_add`）。
///   (b) 範囲が**今 Ring 3 が使っている窓**に収まる（[`user_window`]）。
///   (c) 範囲を跨ぐ全 4KiB ページが present && 全階層 U=1
///       （[`crate::paging::verify::walk_user_accessible`]）。
///
/// (a)(b)(c-present) は多層防御として (c-U=1) に冗長で、単独では隔離した破壊確認が
/// できない（詳細は verification-coverage）。それらは default battery の first-line
/// 拒否者として実運用・実証される。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が walk_user_accessible の契約を満たすこと。
pub unsafe fn validate_user_range(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    buf: u64,
    len: u64,
) -> Option<UserSlice> {
    // 破壊 (M5-f-2-1, skip-all): 検証器を常に受理にする。検証器の全体機能停止を
    // battery が検出して halt する（多層防御の最後の砦の確認）。
    #[cfg(feature = "syscall-test-validate-skip-all")]
    {
        let _ = (pml4_phys, direct_map);
        return Some(UserSlice { buf, len });
    }
    #[cfg(not(feature = "syscall-test-validate-skip-all"))]
    {
        // (a) len==0 は常に受理（契約）。
        if len == 0 {
            return Some(UserSlice { buf, len });
        }
        // (a) 加算オーバーフロー無し。end は排他的上端（buf+len）。
        let end = buf.checked_add(len)?;
        // (b) 範囲が**今の窓**に収まっていること（S9-b-3-2b で 1 つに畳んだ）。
        // **またぐ範囲が受理されないのは、窓が 1 つしかないからである**（S9-b-1 から
        // S9-b-3-2a までは窓が 2 つあり、「またいだものは受理しない」と書いて
        // いた。いまは書く条件ではなく構造の帰結である）。
        let (window_start, window_end) = user_window();
        if buf < window_start || end > window_end {
            return None;
        }
        // (c) 範囲を跨ぐ全 4KiB ページを walk。境界非整列でも先頭・末尾を覆う。
        let first_page = buf & !0xFFF;
        let full_last_page = (end - 1) & !0xFFF;
        // 破壊 (M5-f-2-1, skip-laststep): 走査上端を先頭ページに潰し、先頭ページだけを
        // 検証する。無効4（跨ぎ）の末尾無効を取り逃し、battery が検出して halt する。
        let last_page = if cfg!(feature = "syscall-test-validate-skip-laststep") {
            first_page
        } else {
            full_last_page
        };
        let mut page = first_page;
        while page <= last_page {
            let virt = common::addr::VirtAddr::new(page)?;
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。読み取りのみ。
            if unsafe { crate::paging::verify::walk_user_accessible(pml4_phys, direct_map, virt) }
                .is_err()
            {
                return None;
            }
            page += 0x1000;
        }
        Some(UserSlice { buf, len })
    }
}

/// [`validate_user_range`] の bool 版（M5-f-2-1 の SYS_CHECK_PTR 用）。可なら true。
///
/// # Safety
///
/// [`validate_user_range`] と同じ契約。
pub unsafe fn user_range_accessible(
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
    buf: u64,
    len: u64,
) -> bool {
    // SAFETY: 呼び出し元契約による。
    unsafe { validate_user_range(pml4_phys, direct_map, buf, len) }.is_some()
}

/// 検証済みの [`UserSlice`] から `dst` へ、範囲内バイトだけを読む bounded read
/// （M5-f-2-2）。**`UserSlice` を要求するので、検証を経ないと呼べない。**
///
/// ユーザーバイトは稼働中アドレス空間の VA を直接参照する（present・U=1 でマップ済み、
/// SMAP 未有効なのでカーネルが直接読める。テーブル walk は検証で使うが、データ読みに
/// direct_map は要らない）。読んだバイト数を返す。
///
/// **TOCTOU について。** 検証と読みが実質アトミックなのは、syscall_entry が割り込みゲート
/// （IF=0）で入りプリエンプトが来ないこと、シングルコアであること、ユーザーページを
/// アンマップする経路が syscall 中に走らないこと、の構造条件に依存する。将来 IF を立てる
/// syscall（長時間ブロッキング等）を入れると、この前提が崩れ TOCTOU（検証後・読み前に
/// アンマップ/再マップ）が現実化するため再検証が要る（verification-coverage の申し送り）。
///
/// # Safety
///
/// `slice` が現在のアドレス空間に対して有効に検証されていること（[`validate_user_range`]
/// が返したものであること）。`dst` が読むバイト数を収められること。
pub unsafe fn copy_from_user(dst: &mut [u8], slice: &UserSlice) -> usize {
    // 破壊 (M5-f-2-2, copy-overrun): len を 1 バイト超えて読む。末尾の有効ページ内に置いた
    // 余分な既知バイトが総和へ混ざり、内容往復のチェックサムが決定的に食い違う（#PF は副次）。
    let n = slice.len as usize
        + if cfg!(feature = "syscall-test-copy-overrun") {
            1
        } else {
            0
        };
    // dst に収まる分だけ読む（copy-overrun で n が dst を超えても範囲外にしない）。
    let count = n.min(dst.len());
    for (i, slot) in dst.iter_mut().enumerate().take(count) {
        // SAFETY: slice は検証済みで、buf+i は present・U=1 のユーザーページ。SMAP 未有効。
        *slot = unsafe { core::ptr::read_volatile((slice.buf as *const u8).add(i)) };
    }
    count
}

/// 番号を実装へ振り分ける（M5-f-1-2 / M5-f-2-1）。
///
/// probe は既知の戻り値 [`PROBE_RETURN`] を返す。SYS_CHECK_PTR はユーザーポインタの
/// 範囲を検証し、可なら 0、不可なら -EFAULT を返す（**バイトは読まない**）。それ以外は
/// 未実装で `-ENOSYS`。
///
/// **[`SYS_EXIT`] だけは記録して終わる**（S9-b-3-1）。**戻り値では「戻らない」を
/// 表せない**ので、Ring 3 へ返さない分岐は [`syscall_entry`] が持つ。
///`pml4_phys` / `direct_map` は稼働中テーブルのもの（syscall_entry
/// が用意する）で、ポインタ検証にのみ使う。
///
/// # Safety
///
/// `pml4_phys` / `direct_map` が [`user_range_accessible`] の契約を満たすこと。
unsafe fn dispatch(
    number: u64,
    args: &[u64; 6],
    pml4_phys: PhysAddr,
    direct_map: DirectMap,
) -> u64 {
    match number {
        PROBE_NUMBER => {
            // **この回の引数を残す（S9-b-3-2a）。** [`LAST_ARGS`] は後続の呼び出しで
            // 上書きされるので、**主張したい 1 回**をここで押さえる。
            for (slot, value) in PROBE_SEEN_ARGS.iter().zip(args.iter()) {
                slot.store(*value, Ordering::SeqCst);
            }
            PROBE_INVOKED.store(true, Ordering::SeqCst);
            PROBE_RETURN
        }
        SYS_CHECK_PTR => {
            let buf = args[0];
            let len = args[1];
            // **踏み込む前に**範囲を検証する。可なら 0、不可なら -EFAULT。この段は
            // バイトを読まない（copy は M5-f-2-2）。
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            if unsafe { user_range_accessible(pml4_phys, direct_map, buf, len) } {
                0
            } else {
                (-EFAULT) as u64
            }
        }
        SYS_WRITE => {
            let buf = args[1];
            let len = args[2];
            if len as usize > WRITE_BUF_LEN {
                return (-EINVAL) as u64;
            }
            // **踏み込む前に検証する。** 検証済みトークンを得てから読む。
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            let Some(slice) = (unsafe { validate_user_range(pml4_phys, direct_map, buf, len) })
            else {
                return (-EFAULT) as u64;
            };
            let mut kbuf = [0u8; WRITE_BUF_LEN];
            // SAFETY: slice は検証済み。dst は len を収める。
            let read = unsafe { copy_from_user(&mut kbuf, &slice) };
            WRITE_FD.store(args[0], Ordering::SeqCst);
            for (slot, value) in WRITE_BUF.iter().zip(kbuf.iter()) {
                slot.store(*value, Ordering::SeqCst);
            }
            WRITE_LEN.store(read as u64, Ordering::SeqCst);
            read as u64
        }
        SYS_CHECKSUM => {
            let buf = args[0];
            let len = args[1];
            // 長さがカーネルバッファを超える。**アドレスの問題ではないので
            // -EINVAL であって -EFAULT ではない**（S9-a で分けた）。
            //
            // 破壊 (S9-a, einval-as-efault): 分ける前の -EFAULT へ戻す。長さの誤りと
            // アドレスの誤りが同じ errno へ潰れ、over-long の判定行が捕まえる。
            if len as usize > CHECKSUM_BUF_LEN {
                #[cfg(not(feature = "syscall-test-einval-as-efault"))]
                let errno = EINVAL;
                #[cfg(feature = "syscall-test-einval-as-efault")]
                let errno = EFAULT;
                return (-errno) as u64;
            }
            // **踏み込む前に検証する。** 検証済みトークン UserSlice を得てから読む。
            // SAFETY: 呼び出し元契約により pml4_phys / direct_map は有効。
            #[cfg(not(feature = "syscall-test-copy-skip-validate"))]
            let slice = unsafe { validate_user_range(pml4_phys, direct_map, buf, len) };
            // 破壊 (M5-f-2-2, copy-skip-validate): 検証を経ずに UserSlice をモジュール内で
            // 直接構築する（型保証の境界を突く。モジュール内なので private フィールドに触れる）。
            // カーネルポインタを渡すと、-EFAULT のはずが総和が返り verify が検出して halt する。
            #[cfg(feature = "syscall-test-copy-skip-validate")]
            let slice = Some(UserSlice { buf, len });
            let Some(slice) = slice else {
                return (-EFAULT) as u64;
            };
            let mut kbuf = [0u8; CHECKSUM_BUF_LEN];
            // SAFETY: slice は検証済み（copy-skip-validate を除く）。dst は len+破壊1 を収める。
            let read = unsafe { copy_from_user(&mut kbuf, &slice) };
            kbuf[..read].iter().map(|b| *b as u64).sum()
        }
        SYS_EXIT => {
            // **記録するだけである。** Ring 3 へ返らない分岐は `syscall_entry` が
            // 持つ（[`SYS_EXIT`] の doc）。**戻り値は読まれない。**
            //
            // 破壊 (S9-b-3-1, user-exit-wrong-status): 終了状態を第 1 引数（RDI）
            // ではなく第 2 引数（RSI）から読む。**`arg4-rcx` と同じ、引数レジスタを
            // 1 本取り違える形である。** `hello` は `exit` の直前に RSI を
            // 触らない（`write` へ渡したバイト列の番地が残っている）ので、
            // **0 でない既知の値が終了状態として記録される。**
            #[cfg(not(feature = "user-exit-wrong-status"))]
            let status = args[0];
            #[cfg(feature = "user-exit-wrong-status")]
            let status = args[1];
            PROCESS_EXIT_STATUS.store(status, Ordering::SeqCst);
            PROCESS_EXITED.store(true, Ordering::SeqCst);
            0
        }
        // 失敗は -errno（-1..-4095）。
        _ => (-ENOSYS) as u64,
    }
}

/// `zaytos_syscall_common` から `extern "sysv64"` で呼ばれる。**[`SYS_EXIT`] 以外は戻る。**
///
/// # exit は出口を通らない（S9-b-3-1）
///
/// [`SYS_EXIT`] を受けたときだけ、`ring3::leave_ring3`（longjmp）で
/// `ring3::enter` の呼び出し元へ帰る。**この関数の末尾を通らない。**
///
/// **したがって BKL の解放を `Drop` に任せられない。** longjmp は `Drop` を
/// 走らせないので、**取ったまま出て二度と解かれない。** 分岐の中で明示的に
/// `drop` する。**BKL を取る入口に、出口を通らない経路ができたのはここが初めて
/// である**（`bkl.rs` の [`crate::bkl::NON_ACQUIRING_ENTRIES`] の隣の注記）。
///
/// 番号（RAX）と 6 引数（RDI/RSI/RDX/R10/R8/R9）を読み、記録し、ディスパッチして、
/// 戻り値を `context.rax` へ書き戻し、復元経路が使う RSP を返す。M5-f-1 は切り替え
/// ないので入場時の `IrqContext` 先頭をそのまま返す（`irq_entry` の no-switch と
/// 同じ）。復元経路が `pop rax` で `context.rax` を復元するので、書き戻した戻り値が
/// ユーザーの RAX に入る。
///
/// **出力しない。** 例外・IRQ ハンドラと同じく、ここでは共有状態の更新だけを行う。
/// 観測は畳んで戻った後にカーネルが記録越しに行う。
///
/// # Safety
///
/// `context` はスタブが積んだ有効な [`IrqContext`] を指していること。
/// `rsp_at_call` はスタブが `call` 直前に読んだ RSP であること。
pub(crate) fn syscall_entry(context: *mut IrqContext, rsp_at_call: u64) -> u64 {
    // **BKL を取る（S4-b-2）。** 割り込みゲート経由なので入場時点で IF=0 だが、
    // BKL の保持区間であることを型で表すためにガードを取る。
    let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::Syscall);

    // カーネルへ入ったので「今 Ring 3 にいる」を降ろす（S8-b）。Ring 3 へ返る直前で
    // 立て直す。降ろす前の値を記録しておき、往復の検証で突き合わせる（Ring 3 から
    // 来たのなら真のはず）。
    IN_RING3_AT_ENTRY.store(crate::ring3::note_kernel_entry(), Ordering::SeqCst);

    // SAFETY: スタブが直前に積んだ有効な IrqContext を指す。読み書きともこの
    // フレームに限る。
    let ctx = unsafe { &mut *context };

    // 既存の境界計算が syscall 経路でも正しいことの裏取り（IRQ と同じ検査）。
    crate::idt::check_stack_alignment(rsp_at_call, "syscall", ctx.vector);

    // 番号は RAX。**書き戻しの前に読む。**
    let number = ctx.rax;

    // 第 4 引数は R10（RCX ではない。ADR-0020）。
    // 破壊 (M5-f-1-2, arg4-rcx): 第 4 引数を RCX から読む。記録した第 4 引数が
    // PROBE_ARGS[3] と食い違い、R10 規約であることが実証される。
    #[cfg(not(feature = "syscall-test-arg4-rcx"))]
    let arg3 = ctx.r10;
    #[cfg(feature = "syscall-test-arg4-rcx")]
    let arg3 = ctx.rcx;
    let args = [ctx.rdi, ctx.rsi, ctx.rdx, arg3, ctx.r8, ctx.r9];

    INVOCATION_COUNT.fetch_add(1, Ordering::SeqCst);
    LAST_NUMBER.store(number, Ordering::SeqCst);
    for (slot, value) in LAST_ARGS.iter().zip(args.iter()) {
        slot.store(*value, Ordering::SeqCst);
    }
    HANDLER_RSP.store(rsp_at_call, Ordering::SeqCst);

    // ポインタ検証のため、稼働中テーブルの PML4 物理と登録 direct map を用意する。
    let direct_map = common::addr::direct_map();
    // SAFETY: CR3 を読んで現在のテーブルを構築するだけ（読み取り）。IF=0 の単一文脈。
    let pml4_phys =
        unsafe { crate::paging::active::ActivePageTable::current(direct_map) }.pml4_phys();

    // SAFETY: pml4_phys / direct_map は稼働中テーブルのもので、walk の契約を満たす。
    let ret = unsafe { dispatch(number, &args, pml4_phys, direct_map) };

    // **exit だけは Ring 3 へ返らない。**
    //
    // 破壊 (S9-b-3-1, user-exit-ignored): 終了させずに Ring 3 へ返す。プロセスは
    // `exit` の直後に置いた `ud2` へ落ち、ベクタ 6 の畳みとして現れる。
    #[cfg(not(feature = "user-exit-ignored"))]
    if number == SYS_EXIT {
        // **BKL は自分で解く。** 下の `leave_ring3` は longjmp で、`Drop` を
        // 走らせない。**取ったまま戻ると、二度と解かれない。**
        //
        // 破壊 (S9-b-3-1, user-exit-keep-bkl): 解かずに戻る。次に BKL を取る者
        // （空間を畳む側）が、同じコアの再取得として捕まえる。
        #[cfg(not(feature = "user-exit-keep-bkl"))]
        drop(_bkl);
        // SAFETY: Ring 3 から `int 0x80` で入った文脈で、RECOVERY は
        // `ring3::enter` が保存済みである。BKL は上で解いてある。
        unsafe { crate::ring3::leave_ring3() }
    }

    // 戻り値を RAX へ書き戻す。復元経路の pop rax がこれをユーザー RAX へ載せる。
    // 破壊 (M5-f-1-2, drop-retval): 書き戻しを落とす。ctx.rax は番号のままで、
    // ユーザーは期待した戻り値を受け取れない。
    #[cfg(not(feature = "syscall-test-drop-retval"))]
    {
        ctx.rax = ret;
    }
    #[cfg(feature = "syscall-test-drop-retval")]
    let _ = ret;

    // Ring 3 へ返る（stub の復元経路が iretq する）。立て直す（S8-b）。
    // 立て直してから実際に iretq するまでは Ring 0 なのに真だが、畳みの判定は
    // CS.RPL=0 を弾くので届かない（ring3.rs の IN_RING3 の doc）。
    crate::ring3::note_return_to_ring3();

    // M5-f-1 は切り替えない。入場時の IrqContext 先頭を返す。
    context as u64
}

/// [`SYS_WRITE`] が最後に記録した fd。
pub fn last_write_fd() -> u64 {
    WRITE_FD.load(Ordering::SeqCst)
}

/// [`SYS_WRITE`] が最後に記録したバイト数。
pub fn last_write_len() -> usize {
    WRITE_LEN.load(Ordering::SeqCst) as usize
}

/// [`SYS_WRITE`] が最後に記録したバイト列を `dst` へ写す。写した長さを返す。
pub fn last_write_bytes(dst: &mut [u8]) -> usize {
    let len = last_write_len().min(dst.len()).min(WRITE_BUF_LEN);
    for (slot, value) in dst.iter_mut().zip(WRITE_BUF.iter()).take(len) {
        *slot = value.load(Ordering::SeqCst);
    }
    len
}

/// 会計カウンタを 0 に戻す（往復検証の直前に呼ぶ）。
///
/// **終了の記録も戻す（S9-b-3-1）。** プロセスは順に 1 本ずつ走るので、
/// **前のプロセスの終了が次のプロセスのものとして読まれない**ようにする。
///
/// **`write` の記録も戻す（S9-b-3-2a）。** 戻していなかったので、
/// **`write` を発行しないプロセスについて「送っていない」を主張できなかった**
/// ——前のプロセスが送ったバイト列がそのまま残る。**1 本しか走らない間は
/// 差が出ないので、複数になって初めて要る**（`FAULT_CS` の戻し忘れと同じ形で、
/// `verification-coverage.md` に記録がある）。
pub fn reset_counters() {
    INVOCATION_COUNT.store(0, Ordering::SeqCst);
    LAST_NUMBER.store(0, Ordering::SeqCst);
    for slot in LAST_ARGS.iter() {
        slot.store(0, Ordering::SeqCst);
    }
    HANDLER_RSP.store(0, Ordering::SeqCst);
    IN_RING3_AT_ENTRY.store(false, Ordering::SeqCst);
    PROCESS_EXITED.store(false, Ordering::SeqCst);
    PROCESS_EXIT_STATUS.store(0, Ordering::SeqCst);
    WRITE_FD.store(0, Ordering::SeqCst);
    WRITE_LEN.store(0, Ordering::SeqCst);
    for slot in WRITE_BUF.iter() {
        slot.store(0, Ordering::SeqCst);
    }
    // **probe の記録も戻す（S9-b-3-2a）。** 起動時の battery が発行した probe の
    // 引数が、ユーザープログラムのものとして読まれないようにする
    // （`verification-coverage.md` の「1 つしかない間は、リセット漏れが観測できない」）。
    PROBE_INVOKED.store(false, Ordering::SeqCst);
    for slot in PROBE_SEEN_ARGS.iter() {
        slot.store(0, Ordering::SeqCst);
    }
}

/// 今 Ring 3 が使っている窓を返す（S9-b-3-2b）。
pub fn user_window() -> (u64, u64) {
    (
        USER_WINDOW_START.load(Ordering::SeqCst),
        USER_WINDOW_END.load(Ordering::SeqCst),
    )
}

/// 窓を据え、**据える前の値を返す**（S9-b-3-2b）。
///
/// **戻すのは呼び出し側の責任である。** 現在の呼び出し元は
/// [`crate::ring3::enter`] だけで、あちらが遠征の前後で対にしている。
/// **入れ子にはならない**（Ring 3 の遠征は入れ子にならない）が、
/// **前の値を返す形にしてあるので、入れ子になっても壊れない。**
pub fn set_user_window(start: u64, end: u64) -> (u64, u64) {
    let previous_start = USER_WINDOW_START.swap(start, Ordering::SeqCst);
    let previous_end = USER_WINDOW_END.swap(end, Ordering::SeqCst);
    (previous_start, previous_end)
}

/// [`PROBE_NUMBER`] が呼ばれたか（S9-b-3-2a）。
pub fn probe_invoked() -> bool {
    PROBE_INVOKED.load(Ordering::SeqCst)
}

/// [`PROBE_NUMBER`] の呼び出しで届いた 6 引数（S9-b-3-2a）。
pub fn probe_seen_args() -> [u64; 6] {
    core::array::from_fn(|i| PROBE_SEEN_ARGS[i].load(Ordering::SeqCst))
}

/// [`SYS_EXIT`] を受け取ったか（S9-b-3-1）。
pub fn process_exited() -> bool {
    PROCESS_EXITED.load(Ordering::SeqCst)
}

/// [`SYS_EXIT`] が受け取った終了状態。[`process_exited`] が真のときだけ意味を持つ。
pub fn process_exit_status() -> u64 {
    PROCESS_EXIT_STATUS.load(Ordering::SeqCst)
}

/// `syscall_entry` が呼ばれた回数。
pub fn invocation_count() -> u64 {
    INVOCATION_COUNT.load(Ordering::SeqCst)
}

/// 直近に受け取った番号（RAX）。
pub fn last_number() -> u64 {
    LAST_NUMBER.load(Ordering::SeqCst)
}

/// 直近に受け取った 6 引数（RDI/RSI/RDX/R10/R8/R9 の順）。
pub fn last_args() -> [u64; 6] {
    core::array::from_fn(|i| LAST_ARGS[i].load(Ordering::SeqCst))
}

/// `syscall_entry` が走ったときの RSP。RSP0 スタック範囲との照合に使う。
pub fn handler_rsp() -> u64 {
    HANDLER_RSP.load(Ordering::SeqCst)
}

/// 入場時点で「今 Ring 3 にいる」が立っていたか（S8-b）。
pub fn in_ring3_at_entry() -> bool {
    IN_RING3_AT_ENTRY.load(Ordering::SeqCst)
}
