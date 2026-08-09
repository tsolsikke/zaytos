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

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::addr::{DirectMap, PhysAddr};

use crate::idt::context::IrqContext;

/// `-ENOSYS`（未実装システムコール）の errno。失敗は `-errno` で返す。
pub const ENOSYS: i64 = 38;

/// `-EFAULT`（不正なアドレス）の errno。ユーザーポインタ検証に落ちたとき返す。
pub const EFAULT: i64 = 14;

/// ユーザーポインタを取る検証用システムコールの番号（M5-f-2-1、暫定割り当て）。
/// 第 1 引数(RDI)=buf、第 2 引数(RSI)=len。範囲が Ring 3 からアクセス可能なら 0、
/// 不可なら -EFAULT を返す（**この段はバイトを読まない**。copy は M5-f-2-2）。
pub const SYS_CHECK_PTR: u64 = 0x2B;

/// ユーザーバッファのバイト総和（チェックサム）を返すシステムコールの番号
/// （M5-f-2-2、暫定割り当て）。第 1 引数(RDI)=buf、第 2 引数(RSI)=len。範囲を検証してから
/// 範囲内バイトを読み総和を返す。不正な範囲なら -EFAULT。バッファ容量超過も -EFAULT で
/// 代用する（下記 [`CHECKSUM_BUF_LEN`] のコメント参照）。
pub const SYS_CHECKSUM: u64 = 0x2C;

/// SYS_CHECKSUM がユーザーバイトを読み込む固定カーネルバッファの大きさ。
///
/// これを超える len は現状 -EFAULT で弾く。**意味的には「バッファ容量超過」であり、
/// ポインタ不正（EFAULT = Bad address）とは異なる。** errno 体系がまだ最小なので -EFAULT で
/// 代用しているが、errno を増やす段（POSIX 互換の構想）で見直す。
pub const CHECKSUM_BUF_LEN: usize = 64;

/// ユーザーサブツリー（PML4[[`crate::USER_PML4_INDEX`]]）の仮想範囲
/// [USER_VIRT_MIN, USER_VIRT_MAX)。現在 PML4[1] = [512 GiB, 1 TiB)。
///
/// **この範囲は現在のアドレス空間レイアウト（カーネル=下位半分に恒等、ユーザー=
/// PML4[1]）に依存する。** higher-half B でカーネルを上位半分へ移しユーザーを低位へ
/// 広げると、この範囲は変わる（verification-coverage に再評価の申し送り）。
pub const USER_VIRT_MIN: u64 = 1 << 39;
pub const USER_VIRT_MAX: u64 = 2 << 39;

/// 検証用 probe システムコールの番号（ZaytOS 独自の暫定割り当て）。
pub const PROBE_NUMBER: u64 = 0x2A;

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
///   (b) buf と末尾(buf+len-1) が [USER_VIRT_MIN, USER_VIRT_MAX)。
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
        // (b) buf と末尾（end-1）がユーザー範囲内。end <= USER_VIRT_MAX で末尾も範囲内。
        if buf < USER_VIRT_MIN || end > USER_VIRT_MAX {
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
/// 未実装で `-ENOSYS`。`pml4_phys` / `direct_map` は稼働中テーブルのもの（syscall_entry
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
        PROBE_NUMBER => PROBE_RETURN,
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
        SYS_CHECKSUM => {
            let buf = args[0];
            let len = args[1];
            // バッファ容量超過は -EFAULT で代用（上記 CHECKSUM_BUF_LEN のコメント）。
            if len as usize > CHECKSUM_BUF_LEN {
                return (-EFAULT) as u64;
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
        // 失敗は -errno（-1..-4095）。
        _ => (-ENOSYS) as u64,
    }
}

/// `zaytos_syscall_common` から `extern "sysv64"` で呼ばれる。**戻る。**
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

/// 会計カウンタを 0 に戻す（往復検証の直前に呼ぶ）。
pub fn reset_counters() {
    INVOCATION_COUNT.store(0, Ordering::SeqCst);
    LAST_NUMBER.store(0, Ordering::SeqCst);
    for slot in LAST_ARGS.iter() {
        slot.store(0, Ordering::SeqCst);
    }
    HANDLER_RSP.store(0, Ordering::SeqCst);
    IN_RING3_AT_ENTRY.store(false, Ordering::SeqCst);
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
