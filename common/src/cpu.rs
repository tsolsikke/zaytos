//! x86_64 の最小限のプリミティブ。
//!
//! パニックハンドラ（ADR-0004: fail-fast）と正常終了時の停止処理の
//! 両方から使う、共有の低レイヤ操作をここに集約する。

/// 現在のスタックポインタ（RSP）の値を読み取る。
///
/// 呼び出し元自身のスタックポインタを読むだけの操作であり、
/// 呼び出しコンテキストに関わらず常に安全に実行できる。
pub fn read_rsp() -> u64 {
    let rsp: u64;
    // SAFETY: `mov` によるレジスタ読み取りのみで、メモリアクセスや
    // 制御フローの変更を伴わない。
    unsafe {
        core::arch::asm!(
            "mov {}, rsp",
            out(reg) rsp,
            options(nomem, nostack, preserves_flags),
        );
    }
    rsp
}

/// 現在の実行位置に近い命令ポインタ（RIP）の値を読み取る。
///
/// x86_64 には「RIP を汎用レジスタへ読み出す」命令が存在しないため、
/// `lea reg, [rip]` で「次の命令のアドレス」を取得する（呼び出し直後の
/// 命令アドレスに近い値になる）。ページテーブルの検証等、「現在
/// 実行中のコード周辺がマップされているか」を確認する目的には十分な
/// 精度である。
pub fn read_rip() -> u64 {
    let rip: u64;
    // SAFETY: `lea` によるアドレス計算のみで、メモリアクセスや制御フローの
    // 変更を伴わない。
    unsafe {
        core::arch::asm!(
            "lea {}, [rip]",
            out(reg) rip,
            options(nomem, nostack, preserves_flags),
        );
    }
    rip
}

/// RFLAGS レジスタのうち、割り込みフラグ（IF, bit 9）を表すビットマスク。
pub const RFLAGS_INTERRUPT_FLAG: u64 = 1 << 9;

/// 現在の RFLAGS レジスタの値を読み取る。
pub fn read_rflags() -> u64 {
    let rflags: u64;
    // SAFETY: `pushfq` で RFLAGS をスタックへ積み、`pop` で読み出すだけ。
    // スタックを一時的に使うため `nomem`/`nostack` は指定しない。
    unsafe {
        core::arch::asm!("pushfq", "pop {}", out(reg) rflags);
    }
    rflags
}

/// 割り込みを禁止する（`cli`）。
///
/// # Safety
///
/// 割り込みの有効/無効は共有データの排他性の前提そのものである。素朴に
/// 呼ぶと、呼び出し元が既に張っていたクリティカルセクションの前提を崩す。
/// 通常は [`crate::critical::InterruptGuard`] を使うこと。直接呼んでよいのは、
/// 停止処理やパニックハンドラのように「以降割り込みを一切戻さない」場面に
/// 限る。
///
/// `preserves_flags` は付けない。`cli` は RFLAGS.IF を変更するため。
pub unsafe fn disable_interrupts() {
    // SAFETY: 呼び出し側の契約により、割り込みを禁止してよい文脈で呼ばれる。
    // `cli` はマスク可能割り込みの受付を止めるだけで、メモリレイアウトや
    // 制御フローを変えない。
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
}

/// 割り込みを許可する（`sti`）。
///
/// # Safety
///
/// 割り込みを有効化してよい文脈でのみ呼ぶこと。「もともと禁止されていた
/// 文脈」で呼ぶと、呼び出し元が守っていた排他性が失われる。無条件に呼んで
/// はならない。通常は [`crate::critical::InterruptGuard`] の Drop が、保存
/// した状態に応じて呼ぶ。
///
/// `preserves_flags` は付けない。`sti` は RFLAGS.IF を変更するため。
pub unsafe fn enable_interrupts() {
    // SAFETY: 呼び出し側の契約により、割り込みを有効化してよい文脈で呼ばれる。
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

/// 保存した RFLAGS を見て、割り込みを復元すべきか判断する（純粋ロジック）。
///
/// クリティカルセクションを抜けるとき、**保存時に IF=1 だった場合のみ**
/// 割り込みを再度有効化する。無条件に `sti` すると、もともと割り込みが
/// 禁止されていた文脈で勝手に有効になり、入れ子で破綻する。
///
/// ハードウェアに触れないためホスト上で `cargo test` により検証する。
pub const fn should_restore_interrupts(saved_rflags: u64) -> bool {
    saved_rflags & RFLAGS_INTERRUPT_FLAG != 0
}

/// 割り込みを禁止し、`hlt` ループで停止し続ける。
///
/// ADR-0004 の fail-fast 方針（パニック時は即停止する）と、M1 の
/// 正常終了時（それ以上進む処理がない状態）の両方で使う停止処理。
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `cli` はマスク可能割り込みを禁止し、`hlt` は次の割り込み
        // までCPUを停止させる。ループで囲むことで、`hlt` が NMI 等で
        // 一時的に起床しても実行を再開させない。
        unsafe {
            core::arch::asm!("cli", "hlt", options(nomem, nostack));
        }
    }
}

/// 割り込みを有効化して、次の割り込みが来るまで停止する（1 回だけ）。
///
/// [`halt_forever`]（`cli` + `hlt` の無限ループ、停止用）とは目的が正反対
/// なので、名前で取り違えないこと。こちらは**割り込みを待つ**ためのもので、
/// 割り込みから戻ればこの関数も戻る。
///
/// # `sti` と `hlt` が隣接していなければならない理由
///
/// 「条件を確認してから `hlt` する」形のループには、確認と `hlt` の間に
/// 割り込みが入ると、その割り込みを処理した後で `hlt` に入ってしまい、
/// **次の割り込みが来るまで眠り続ける**というレースがある。条件が既に
/// 成立しているのに気づかないまま止まるため、症状は「ハングした」に見える。
///
/// x86 はこれを避けるため、`sti` に特別な規則を持たせている。**`sti` は
/// 直後の 1 命令が終わるまで割り込みの受付を保留する。** したがって
/// `sti; hlt` と並べれば、`sti` から `hlt` に入るまでの隙間で割り込みを
/// 取りこぼすことがない。この保証は 2 命令が隣接している場合にしか働かず、
/// 間に何か挟むと失われる。**そのため 1 つの `asm!` に閉じ込めてある。**
/// 呼び出し側で `enable_interrupts()` と `hlt` を別々に呼ぶ形にしては
/// ならない（ADR-0018 のチェックリスト 10）。
///
/// 想定する使い方:
///
/// ```text
/// loop {
///     let guard = InterruptGuard::enter();   // cli して共有状態を読む
///     let work = shared_state_snapshot();
///     drop(guard);                           // ここではまだ処理しない
///     if work.is_empty() {
///         unsafe { enable_interrupts_and_halt() };
///     }
/// }
/// ```
///
/// # Safety
///
/// 割り込みを有効化する。呼び出し時点で、有効化されうるすべてのベクタに
/// 対して正しく動作するハンドラが用意されていなければならない
/// （ADR-0018 §2 の 7 項目）。
pub unsafe fn enable_interrupts_and_halt() {
    // SAFETY: `sti` は IF を立て、`hlt` は次の割り込みまで CPU を止める。
    // 2 命令を 1 つの asm! に置いているため、コンパイラが間に何かを挟む
    // ことはなく、`sti` の 1 命令保留がそのまま `hlt` に掛かる。
    // ハンドラの用意は呼び出し側の契約。
    unsafe {
        core::arch::asm!("sti", "hlt", options(nomem, nostack));
    }
}

/// CR4 を読む。
///
/// M5-a で必要になったのは **PGE（bit 7、Page Global Enable）** の状態を
/// 知るためである。PGE が有効な状態でページテーブルエントリの G ビットが
/// 立っていると、**CR3 のリロードでも TLB から追い出されない**。
/// 「CR3 を書き直せば全部消える」という前提が成立するかどうかが、ここで決まる。
pub fn read_cr4() -> u64 {
    let value: u64;
    // SAFETY: `mov reg, cr4` は読み取り専用で、メモリにもスタックにも副作用が
    // 無い。CR4 の値は実行環境に依存するため、保守的に options は付けない。
    unsafe {
        core::arch::asm!("mov {}, cr4", out(reg) value);
    }
    value
}

/// CR4 の PGE（Page Global Enable）ビット。
pub const CR4_PAGE_GLOBAL_ENABLE: u64 = 1 << 7;

/// 1 ページ分の TLB エントリを無効化する（`invlpg`）。
///
/// # CR3 のリロードとの使い分け
///
/// CR3 を書き直すと（G ビットの付いたものを除いて）TLB が丸ごと落ちる。
/// 1 ページだけ変えたときにそれを使うと、無関係な翻訳まで捨てて
/// 以降のアクセスがすべて再ウォークになる。逆に、512 本を一度に
/// 置き換えるページ分割で `invlpg` を 512 回発行するのは無駄が多い。
///
/// ZaytOS では**アンマップに `invlpg`、分割に CR3 リロード**を使う。
///
/// # Safety
///
/// `virt` はカーネルが意味を把握しているアドレスであること。この命令自体は
/// メモリを書き換えないが、**TLB を落とすとその後のアクセスが新しい
/// ページテーブルの内容に従う**。呼び出し側は、テーブルの書き換えが
/// 完了した後に呼ぶこと。順序を逆にすると、古い翻訳が残ったまま
/// テーブルだけ変わった状態になる。
pub unsafe fn invalidate_tlb_entry(virt: u64) {
    // SAFETY: 呼び出し元契約を参照。`invlpg` は指定アドレスの TLB エントリを
    // 落とすだけで、メモリの内容もレジスタも変えない。ただし以降のメモリ
    // アクセスの解決先が変わるため、`nomem` は付けない（CR3 の書き換えと
    // 同じ理由で、コンパイラに並べ替えさせない）。
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) virt);
    }
}

/// タイムスタンプカウンタ（TSC）を読む。
///
/// **計測専用。時刻源として使わないこと。** TSC は CPU の起動からの
/// サイクル数を数えるだけのもので、周波数はハイパーバイザや電源管理の
/// 影響を受けうる。時刻が必要になったら、M4 で導入するタイマを使うこと。
///
/// **`rdtsc` は直列化命令ではない。** 前後の命令と実行順序が入れ替わりうる
/// ため、数命令程度の短い区間を測る用途には向かない。数十万サイクル以上の
/// 塊を測る前提で使うこと。厳密な区間計測が必要になった場合は、`lfence` を
/// 併用するか `rdtscp` を検討する。
///
/// **仮想化環境での注意**: 本プロジェクトの開発環境（WSL2 上の QEMU）で
/// KVM を使う場合、WSL2 自体が Hyper-V 上の VM であるため入れ子の仮想化に
/// なる。TCG（純粋エミュレーション）よりは遥かに実機へ近いが、実機その
/// ものではない。計測結果は絶対値ではなく、比較対象との比で判断すること
/// （ADR-0015 Addendum）。
pub fn read_timestamp_counter() -> u64 {
    // SAFETY: `rdtsc` は特権を必要とせず（CR4.TSD が立っていない限り）、
    // メモリにも制御フローにも副作用が無い。EDX:EAX に値を返すだけ。
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// クリティカルセクションを抜けるときの復元判断。保存時に IF=1 なら復元、
    /// IF=0 なら復元しない。この 2 ケースが入れ子の正しさの核心である。
    #[test]
    fn interrupts_are_restored_only_when_they_were_enabled_on_entry() {
        // IF=1 で入ったなら、抜けるとき復元する。
        assert!(should_restore_interrupts(RFLAGS_INTERRUPT_FLAG));
        // IF=0 で入ったなら（入れ子の内側など）、抜けても復元しない。
        assert!(!should_restore_interrupts(0));
    }

    /// IF 以外のビットが立っていても、判断は IF ビットだけで行う。
    #[test]
    fn only_the_interrupt_flag_bit_matters() {
        // 予約ビット bit1 は常に 1。それ以外を色々立てても IF だけを見る。
        let if_set = RFLAGS_INTERRUPT_FLAG | 0b10 | (1 << 0) | (1 << 6);
        let if_clear = 0b10 | (1 << 0) | (1 << 6);
        assert!(should_restore_interrupts(if_set));
        assert!(!should_restore_interrupts(if_clear));
    }
}
