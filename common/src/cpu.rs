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

/// CPU が実装している物理アドレスのビット数（MAXPHYADDR）。
///
/// # 観測値であって、判定には使わない
///
/// `PhysAddr` が弾くのは 52 ビットを超える値である（`common::addr`）。
/// MAXPHYADDR は 52 以下の任意の値を取りうるが、**それを型の判定に使うと、
/// 実行時の値でコンパイル時の不変条件を決めることになり、同じバイナリが
/// 環境によって別の挙動をする。** ここはログに出すだけにする。
///
/// 52 未満の MAXPHYADDR を持つ環境で、MAXPHYADDR 以上 52 未満のアドレスを
/// 作った場合は、ページテーブルへ書いた時点で CPU が予約ビット違反として
/// 弾く。型の側で先回りはしない。
///
/// # 拡張リーフの対応を先に確かめる
///
/// `CPUID.80000008h` を読む前に、`CPUID.80000000h` の EAX が
/// `0x8000_0008` 以上であることを確認する。**拡張リーフが未対応の CPU では、
/// 未定義の値か別のリーフの内容が返る。** 取れなかった場合は `None` を返す。
/// 観測が目的である以上、取れなかったことも観測結果である。
pub fn max_physical_address_bits() -> Option<u8> {
    // SAFETY: `cpuid` は特権を必要とせず、メモリにも制御フローにも副作用が
    // 無い。EAX/EBX/ECX/EDX を書き換えるだけで、Rust の値や借用の不変条件を
    // 壊さない。RBX は LLVM が予約しているため、退避・復元を明示している。
    let highest_extended: u32 = unsafe {
        let eax: u32;
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inlateout("eax") 0x8000_0000u32 => eax,
            lateout("ecx") _,
            lateout("edx") _,
        );
        eax
    };
    if highest_extended < 0x8000_0008 {
        return None;
    }

    // SAFETY: 上と同じ。対応していることを直前に確認した葉だけを読む。
    let eax: u32 = unsafe {
        let eax: u32;
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inlateout("eax") 0x8000_0008u32 => eax,
            lateout("ecx") _,
            lateout("edx") _,
        );
        eax
    };
    Some((eax & 0xFF) as u8)
}

/// `IA32_APIC_BASE` の MSR 番号。
///
/// 出典: Intel SDM Vol.3A 「Local APIC Status and Location」および
/// Vol.4 の MSR 一覧（`IA32_APIC_BASE`、アドレス `1BH`）。
const IA32_APIC_BASE: u32 = 0x1B;

/// `IA32_APIC_BASE` のビット位置（出典は [`IA32_APIC_BASE`] と同じ）。
///
/// **ビットの意味をここ 1 か所に集める。** 呼び出し側は
/// [`ApicBase`] のフィールドを見るだけで済み、ビット位置を知る必要がない。
const APIC_BASE_BSP: u64 = 1 << 8;
const APIC_BASE_EXTD: u64 = 1 << 10;
const APIC_BASE_ENABLE: u64 = 1 << 11;

/// ベースアドレスが載る最下位ビット。これ未満はフラグ領域である。
const APIC_BASE_ADDRESS_SHIFT: u32 = 12;

/// MSR を 1 つ読む。
///
/// **公開しない。** 生の MSR 番号と生の 64 ビット値を境界の外へ出すと、
/// ビット位置の知識が呼び出し側へ散る。外へ出すのは解釈済みの型
/// （[`ApicBase`]）だけにする。
///
/// # Safety
///
/// `msr` がこの CPU に実在すること。**実在しない MSR を読むと `#GP` になる。**
/// 呼び出し側は CPUID 等で存在を確かめてから呼ぶこと。
unsafe fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: `rdmsr` は ECX が指す MSR を EDX:EAX へ読むだけで、メモリにも
    // 制御フローにも副作用が無い。実在する MSR であることは呼び出し元契約。
    // CPL 0 で実行していることは、カーネルからのみ呼ばれることによる。
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((high as u64) << 32) | low as u64
}

/// CPU が Local APIC を持つか（`CPUID.01H:EDX[9]`）。
///
/// **`IA32_APIC_BASE` を読む前に確かめる。** Local APIC を持たない CPU では
/// この MSR が実在せず、読むと `#GP` になる。
fn has_local_apic() -> bool {
    const LEAF_FEATURE_FLAGS: u32 = 1;
    const EDX_APIC_BIT: u32 = 1 << 9;

    // SAFETY: `cpuid` は特権を必要とせず、メモリにも制御フローにも副作用が
    // 無い。RBX は LLVM が予約しているため退避・復元を明示する
    // （`max_physical_address_bits` と同じ形）。リーフ 1 は x86_64 を名乗る
    // CPU に必ず存在する。
    let edx: u32 = unsafe {
        let edx: u32;
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inlateout("eax") LEAF_FEATURE_FLAGS => _,
            lateout("ecx") _,
            lateout("edx") edx,
        );
        edx
    };
    edx & EDX_APIC_BIT != 0
}

/// `IA32_APIC_BASE` を解釈した結果。
///
/// **生のビットではなく問いの形で公開する。** 呼び出し側がビット位置を
/// 知る必要がないようにするためで、`irq`（S0-a）や `acpi`（S1-b）と同じ方針である。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApicBase {
    /// APIC グローバル有効（bit 11）。**落ちていると MMIO も MSR も使えない。**
    pub enabled: bool,
    /// x2APIC モード（bit 10）。
    ///
    /// **立っていると MMIO によるアクセスは無効化される。** x2APIC では
    /// Local APIC を MSR 経由で触るので、MMIO を読むと `#GP` になる。
    /// 「`enabled` が立っているから MMIO で読める」は成立しない。
    pub x2apic: bool,
    /// この CPU が BSP（bit 8）。
    ///
    /// **S3（AP 起こし）の入力になると書いていたが、S3 は使わなかった。**
    /// AP 起こしは MADT の最初の使用可能なエントリで BSP を判定しており、
    /// このビットは**ログへ出すだけである**（`kernel::apic`）。MADT の順序に
    /// 依存する形なので潜在的な欠陥であり、直さない判断と解禁条件は
    /// `docs/deferred-decisions.md` にある。**このビットは、直すときの
    /// 権威のある出所の 1 つである。**
    pub bootstrap_processor: bool,
    /// ベースアドレス（bit 12 以上、MAXPHYADDR まで）をマスクして取り出した値。
    pub base: u64,
    /// 生の値。ログへ出して、上の解釈と突き合わせられるようにする。
    pub raw: u64,
}

impl ApicBase {
    /// Local APIC を MMIO 経由で読んでよいか。
    ///
    /// **`enabled` だけでは足りない。** x2APIC が有効だと MMIO は無効化されて
    /// いるので、両方を見る。
    pub const fn mmio_accessible(&self) -> bool {
        self.enabled && !self.x2apic
    }
}

/// `IA32_APIC_BASE` の生の値を解釈する。**純粋関数。**
///
/// `rdmsr` は CPL 0 でしか実行できないため、[`apic_base`] そのものはホスト上の
/// `cargo test` で走らせられない。**解釈だけを切り出して、ここをホストで検証する**
/// （ハードウェア依存部と純粋ロジックの分離）。
///
/// # ベースアドレスのマスク
///
/// ベースは bit 12 から MAXPHYADDR-1 までに載る。**桁を取り違えると、
/// 突き合わせが常に食い違って見える。** `address_bits` は
/// [`max_physical_address_bits`] の値で、取れなかった場合の 52 は `PhysAddr` の
/// 上限と同じである（それ以上のビットはどのみち物理アドレスとして表せない）。
const fn interpret_apic_base(raw: u64, address_bits: u8) -> ApicBase {
    // `clamp` は const fn ではないので手で畳む。下限が 12 なのは、それ未満だと
    // アドレス部が空になりマスクが 0 になるためである。
    let bits = if address_bits < 12 {
        12
    } else if address_bits > 52 {
        52
    } else {
        address_bits
    };
    let address_mask = ((1u64 << bits) - 1) & !((1u64 << APIC_BASE_ADDRESS_SHIFT) - 1);

    ApicBase {
        enabled: raw & APIC_BASE_ENABLE != 0,
        x2apic: raw & APIC_BASE_EXTD != 0,
        bootstrap_processor: raw & APIC_BASE_BSP != 0,
        base: raw & address_mask,
        raw,
    }
}

/// `IA32_APIC_BASE` を読んで解釈する。Local APIC を持たない CPU では `None`。
pub fn apic_base() -> Option<ApicBase> {
    if !has_local_apic() {
        return None;
    }

    // SAFETY: 直前に `CPUID.01H:EDX[9]` を確認したので、この CPU に
    // `IA32_APIC_BASE` は実在する。
    let raw = unsafe { read_msr(IA32_APIC_BASE) };

    Some(interpret_apic_base(
        raw,
        max_physical_address_bits().unwrap_or(52),
    ))
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

    /// QEMU + OVMF の実測にあたる形。EN が立ち、x2APIC は落ち、BSP である。
    #[test]
    fn a_typical_bsp_value_decodes_to_an_mmio_accessible_local_apic() {
        let raw = 0xFEE0_0000 | APIC_BASE_ENABLE | APIC_BASE_BSP;
        let decoded = interpret_apic_base(raw, 40);
        assert!(decoded.enabled);
        assert!(!decoded.x2apic);
        assert!(decoded.bootstrap_processor);
        assert_eq!(decoded.base, 0xFEE0_0000);
        assert!(decoded.mmio_accessible());
    }

    /// **x2APIC が有効なら MMIO では読めない。** EN が立っていても、である。
    /// ここを `enabled` だけで判断すると `#GP` を踏む。
    #[test]
    fn x2apic_mode_is_not_mmio_accessible_even_though_it_is_enabled() {
        let raw = 0xFEE0_0000 | APIC_BASE_ENABLE | APIC_BASE_EXTD;
        let decoded = interpret_apic_base(raw, 40);
        assert!(decoded.enabled);
        assert!(decoded.x2apic);
        assert!(!decoded.mmio_accessible());
    }

    #[test]
    fn a_disabled_local_apic_is_not_mmio_accessible() {
        let decoded = interpret_apic_base(0xFEE0_0000, 40);
        assert!(!decoded.enabled);
        assert!(!decoded.mmio_accessible());
    }

    /// **フラグのビットがベースアドレスに混ざらない。** ここを取り違えると、
    /// MADT との突き合わせが常に食い違う。
    #[test]
    fn the_flag_bits_are_not_part_of_the_base_address() {
        let raw = 0xFEE0_0000 | APIC_BASE_ENABLE | APIC_BASE_EXTD | APIC_BASE_BSP;
        assert_eq!(interpret_apic_base(raw, 52).base, 0xFEE0_0000);
    }

    /// MAXPHYADDR より上のビットはベースに含めない。
    #[test]
    fn bits_above_maxphyaddr_are_masked_out_of_the_base() {
        // bit 40 が立った値を、MAXPHYADDR = 36 の CPU として解釈する。
        let raw = (1u64 << 40) | 0xFEE0_0000 | APIC_BASE_ENABLE;
        assert_eq!(interpret_apic_base(raw, 36).base, 0xFEE0_0000);
        // 同じ値でも MAXPHYADDR が 52 なら bit 40 はベースの一部である。
        assert_eq!(
            interpret_apic_base(raw, 52).base,
            (1u64 << 40) | 0xFEE0_0000
        );
    }

    /// 極端な `address_bits` でもマスクが壊れない（`1 << bits` の桁あふれや、
    /// アドレス部が空になる形を作らない）。
    #[test]
    fn the_address_mask_is_clamped_for_implausible_maxphyaddr_values() {
        assert_eq!(interpret_apic_base(u64::MAX, 0).base, 0);
        assert_eq!(
            interpret_apic_base(u64::MAX, 255).base,
            ((1u64 << 52) - 1) & !0xFFF
        );
    }

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
