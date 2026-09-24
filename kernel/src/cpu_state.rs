//! 棚卸しの前提を起動のたびに確かめる（2026-09-24。`ADR-0018` の Addendum 9）。
//!
//! # なぜ要るのか
//!
//! **「ユーザーが変えられて、カーネルが前提にしている CPU の状態」の棚卸しは、いまの形に依って
//! いる**——CR0.AM・CR4.SMAP・CR4.FSGSBASE・CR4.PKE・CR4.OSXSAVE・EFER.SCE が 0 であること。
//! **どれかを立てる変更をすると、棚卸しの結論が黙って偽になる**（通る理由が変わる族）。
//! **ここで起動のたびに読み、立っていたら止める。** **値は起動ログの参照にも載る**
//! ——**棚卸しに関わらないビット（SMEP・UMIP など）が変わっても、参照の突き合わせが落ちる。**
//!
//! # AP も見る（2026-09-24。レビューの足す1点）
//!
//! **AP の CR0・CR4・EFER は、BSP とは別の経路（トランポリン）で作られる。** **INIT の直後の値から
//! 始まり、トランポリンは PAE・LME・PG と PE しか立てない**——**実測で、AP は CD と NW が 1（キャッシュが
//! 効かない形）で、WP と NE が 0 のまま走っていた**（`docs/troubleshooting.md`）。**棚卸しの結論は全 CPU に
//! ついてなので、見張りも全 CPU に要る。**
//!
//! **AP は起きた直後に BSP の値を写し**（[`adopt_bsp_state_on_this_ap`]）、**起動の終わりに自分の値を
//! 読んで控える**（[`record_this_ap`]）。**BSP は、起きた AP の値が自分の値と一致することを確かめ、
//! 食い違えば止まる**（[`check_aps_match_bsp`]）。

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use common::log::Logger;
use common::percpu::MAX_CPUS;
use common::serial::SerialPort;

/// 棚卸しが 0 であることに依っているビットの在りか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Register {
    Cr0,
    Cr4,
    Efer,
}

impl Register {
    /// 行に出す名前。
    pub fn name(self) -> &'static str {
        match self {
            Register::Cr0 => "CR0",
            Register::Cr4 => "CR4",
            Register::Efer => "EFER",
        }
    }
}

/// 棚卸しが 0 であることに依っているビット（在りか・ビット・名前・立てたときに崩れる結論）。
pub const INVENTORY_BITS: [(Register, u32, &str, &str); 7] = [
    (
        Register::Cr0,
        18,
        "CR0.AM",
        "Ring 3 could raise #AC (vector 17), which is not folded, by setting AC",
    ),
    (
        Register::Cr4,
        21,
        "CR4.SMAP",
        "interrupts do not clear AC, so the entries would need clac",
    ),
    (
        Register::Cr4,
        16,
        "CR4.FSGSBASE",
        "Ring 3 could write the FS and GS bases with wrfsbase and wrgsbase",
    ),
    (
        Register::Cr4,
        22,
        "CR4.PKE",
        "Ring 3 could change PKRU with wrpkru",
    ),
    (
        Register::Cr4,
        18,
        "CR4.OSXSAVE",
        "Ring 3 could use AVX state that fxsave does not save",
    ),
    (
        Register::Efer,
        0,
        "EFER.SCE",
        "the syscall instruction would become a 4th entry that skips the IDT stubs",
    ),
    // **全ビットの分類で足した**（2026-09-24）。**PVI が立つと、Ring 3 の `cli`・`sti` が `#GP` ではなく
    // VIF を変える**（Intel SDM の CLI の擬似コード）——**棚卸しの「IF は Ring 3 から変えられない」が依る。**
    (
        Register::Cr4,
        1,
        "CR4.PVI",
        "Ring 3 cli and sti would change VIF instead of raising #GP",
    ),
];

/// CPU の製造元（2026-09-24。**運用者の決定——Intel と AMD の両方に対応する**）。
///
/// **CR4 と EFER のビットの意味と予約は、製造元ごとに違う**——Intel SDM Vol.3A（253668-082US）と
/// AMD APM Vol.2（24593 Rev. 3.45）。**起動のたびに CPUID の葉 0 で見分け、その製造元の表で判定する。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vendor {
    Intel,
    Amd,
    /// **Intel でも AMD でもない**——**両者が同じ意味で定義するビットだけで判定する。** **それ以外が
    /// 立っていれば、予約でも止めずに `[WARN]` で名前を出す**（運用者の決定）。
    Other,
}

impl Vendor {
    /// CPUID の葉 0 の 12 文字（EBX・EDX・ECX の順）から見分ける（純粋ロジック）。
    pub fn from_signature(signature: &[u8; 12]) -> Self {
        match signature {
            b"GenuineIntel" => Vendor::Intel,
            b"AuthenticAMD" => Vendor::Amd,
            _ => Vendor::Other,
        }
    }

    /// 判定の出所（行に出す）。
    pub fn manual(self) -> &'static str {
        match self {
            Vendor::Intel => "the Intel table (SDM Vol.3A, 253668-082US)",
            Vendor::Amd => "the AMD table (APM Vol.2, 24593 Rev. 3.45)",
            Vendor::Other => {
                "the bits Intel and AMD define alike only; any other set bit is warned about, and no \
                 reserved bit stops the boot"
            }
        }
    }
}

/// CPUID の葉 0 の 12 文字を読む（EBX・EDX・ECX の順に並べると `GenuineIntel` などになる）。
pub fn read_vendor_signature() -> [u8; 12] {
    // **`unsafe` は要らない**——**`__cpuid` は x86_64 では safe fn である**（葉 0 はすべての x86_64 が持つ）。
    let leaf = core::arch::x86_64::__cpuid(0);
    let mut signature = [0u8; 12];
    signature[0..4].copy_from_slice(&leaf.ebx.to_le_bytes());
    signature[4..8].copy_from_slice(&leaf.edx.to_le_bytes());
    signature[8..12].copy_from_slice(&leaf.ecx.to_le_bytes());
    signature
}

/// どの製造元が、そのビットをその意味で定義しているか（2026-09-24）。**定義していない側では予約である。**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Intel と AMD が同じビットに同じ意味を定義している。
    Both,
    /// Intel だけ（AMD では予約）。
    Intel,
    /// AMD だけ（Intel では予約）。
    Amd,
}

impl Scope {
    /// その製造元の表に入るか。**Intel でも AMD でもない製造元には、両者が同じ意味で定義するものだけが入る。**
    pub fn applies_to(self, vendor: Vendor) -> bool {
        matches!(
            (self, vendor),
            (Scope::Both, _) | (Scope::Intel, Vendor::Intel) | (Scope::Amd, Vendor::Amd)
        )
    }
}

/// **カーネルのコードが依っているビット**（2026-09-24。レビューの足す1点）——在りか・ビット・名前・
/// あるべき値・定義する製造元・崩れたときに困ること。**棚卸しの「0 であるべき」（[`INVENTORY_BITS`]）と
/// 対にする**——あちらはユーザーが変えられる状態についての結論が依るビット、こちらはカーネルのコード
/// そのものが依るビットである。
///
/// # 誰が立てるか
///
/// - **CR0 の WP・NE を立て、CD・NW を落とすのは [`establish_required_bits_on_bsp`] である**（BSP）。
///   **前はファームウェアが良い値で渡していたので動いていただけで、カーネルは自分で決めていなかった**
///   ——**OVMF と VirtualBox の EFI は同じ値で渡すので、2 台の機械では見えない。**
/// - **MP・EM・OSFXSR・OSXMMEXCPT は `fp::enable_on_this_cpu` が持つ**（`ADR-0058`）。
/// - **PE・PG・PAE・LME・LMA は、長モードで走っている時点で立っている**（ブートローダとトランポリン）。
/// - **「0 であるべき」の残りは、カーネルが立てないビットである**——**ファームウェアが立てて渡したら止まる。**
/// - **AP は BSP を丸ごと写す**（[`adopt_bsp_state_on_this_ap`]）。
///
/// # NXE を入れていない理由
///
/// **カーネルのページ表は実行禁止のビット（XD。63 番）を使っていない**（`kernel/src/paging`）。
/// **NXE が 0 なら XD は予約のビットになる**（Intel SDM Vol.3A）が、**立てていないので落ちない。**
/// **実行禁止の保護を入れる段で「1 であるべき」に移す**（`docs/deferred-decisions.md`）。
pub const REQUIRED_BITS: [(Register, u32, &str, bool, Scope, &str); 24] = [
    (
        Register::Cr0,
        0,
        "CR0.PE",
        true,
        Scope::Both,
        "the kernel runs in protected and long mode",
    ),
    (
        Register::Cr0,
        1,
        "CR0.MP",
        true,
        Scope::Both,
        "fxsave is used for the user FP state (ADR-0058)",
    ),
    (
        Register::Cr0,
        2,
        "CR0.EM",
        false,
        Scope::Both,
        "SSE instructions would raise #UD",
    ),
    (
        Register::Cr0,
        5,
        "CR0.NE",
        true,
        Scope::Both,
        "x87 errors must arrive as #MF, which is folded",
    ),
    (
        Register::Cr0,
        16,
        "CR0.WP",
        true,
        Scope::Both,
        "read-only pages must stop kernel writes too",
    ),
    (
        Register::Cr0,
        29,
        "CR0.NW",
        false,
        Scope::Both,
        "caching must be the normal write-back kind",
    ),
    (
        Register::Cr0,
        30,
        "CR0.CD",
        false,
        Scope::Both,
        "the caches must be on",
    ),
    (
        Register::Cr0,
        31,
        "CR0.PG",
        true,
        Scope::Both,
        "the kernel runs with paging",
    ),
    (
        Register::Cr4,
        5,
        "CR4.PAE",
        true,
        Scope::Both,
        "4-level paging needs it",
    ),
    (
        Register::Cr4,
        9,
        "CR4.OSFXSR",
        true,
        Scope::Both,
        "fxsave and SSE need it (ADR-0058)",
    ),
    (
        Register::Cr4,
        10,
        "CR4.OSXMMEXCPT",
        true,
        Scope::Both,
        "SIMD errors must arrive as #XM, which is folded",
    ),
    (
        Register::Efer,
        8,
        "EFER.LME",
        true,
        Scope::Both,
        "the kernel runs in long mode",
    ),
    (
        Register::Efer,
        10,
        "EFER.LMA",
        true,
        Scope::Both,
        "the kernel runs in long mode",
    ),
    // **全ビットの分類で足した**（2026-09-24）。
    (
        Register::Cr0,
        3,
        "CR0.TS",
        false,
        Scope::Both,
        "#NM (vector 7) is not folded; ADR-0058 saves FP state eagerly and never uses TS",
    ),
    // **MCE が 0 だと、機械チェックは #MC にならず、プロセッサが shutdown の状態に入る**
    // （Intel SDM Vol.3A 6-52、Interrupt 18。「If the machine-check mechanism is not enabled (the MCE flag
    // in control register CR4 is clear), a machine-check exception causes the processor to enter the
    // shutdown state.」）。**Halt and Dump の方針は、見える形で止まることである。** **BSP の CR4 には
    // ファームウェアが立てて渡していた**——**いま効いているものを、カーネルの要求として明示する。**
    (
        Register::Cr4,
        6,
        "CR4.MCE",
        true,
        Scope::Both,
        "a machine check must arrive as #MC; with MCE clear the processor shuts down",
    ),
    (
        Register::Cr4,
        12,
        "CR4.LA57",
        false,
        Scope::Both,
        "the kernel builds 4-level page tables",
    ),
    (
        Register::Cr4,
        17,
        "CR4.PCIDE",
        false,
        Scope::Both,
        "the kernel writes CR3 without a PCID",
    ),
    // **CET・PKS・UINTR は本文を読んで分類した**（2026-09-24。レビューの判断 B）。**どれも、カーネルが
    // 設定も退避もしていない状態に効き目が依る。**
    // CET——「If CR4.CET = 1, certain memory accesses are identified as shadow-stack accesses and certain
    // linear addresses translate to shadow-stack pages」（SDM Vol.3A 4.1.3）。AMD も同じ 23 番に置く。
    (
        Register::Cr4,
        23,
        "CR4.CET",
        false,
        Scope::Both,
        "the kernel sets up neither shadow stacks nor the CET MSRs, yet CET makes some pages \
         shadow-stack pages",
    ),
    // PKS——「When set, this flag allows use of the IA32_PKRS MSR to specify ... whether supervisor-mode
    // linear addresses with that protection key can be read or written」（SDM Vol.3A 2.5）。
    (
        Register::Cr4,
        24,
        "CR4.PKS",
        false,
        Scope::Intel,
        "the kernel's own pages would be governed by IA32_PKRS, which the kernel never writes",
    ),
    // UINTR——「Enables user interrupts when set, including user-interrupt delivery, user-interrupt
    // notification identification, and the user-interrupt instructions」（SDM Vol.3A 2.5）。**「The
    // user-interrupt feature is XSAVE-managed」（7.2）。**
    (
        Register::Cr4,
        25,
        "CR4.UINTR",
        false,
        Scope::Intel,
        "the user-interrupt state is XSAVE-managed, and the kernel saves only the fxsave state \
         (ADR-0058)",
    ),
    // **AMD だけが定義するビット**（APM Vol.2 3.1.7。運用者の決定で読んだ）。
    // LMSLE——「When EFER.LMSLE = 1, reads and writes in 64-bit mode at CPL > 0, using the DS, ES, FS, or
    // SS segments, have a segment-limit check applied」「If the DS, ES, FS, or SS segment is null ...,
    // the effect of the limit check is undefined」（4.12.2）。
    (
        Register::Efer,
        13,
        "EFER.LMSLE",
        false,
        Scope::Amd,
        "Ring 3 data accesses would be limit-checked, with an undefined effect on a null segment",
    ),
    // FFXSR——「Setting this bit to 1 enables the FXSAVE and FXRSTOR instructions to execute faster in
    // 64-bit mode at CPL 0. This is accomplished by not saving or restoring the XMM registers」（3.1.7）。
    // **ユーザーの FP の退避（`ADR-0058`）は CPL 0 の 64 ビットで `fxsave` する。**
    (
        Register::Efer,
        14,
        "EFER.FFXSR",
        false,
        Scope::Amd,
        "fxsave at CPL 0 would skip XMM0-15, so the user FP state (ADR-0058) would not be saved",
    ),
    // TCE——「Page table management software must be written in a way that takes this behavior into
    // account」（3.1.7）。**カーネルのページ表の扱いは、それを前提に書いていない。**
    (
        Register::Efer,
        15,
        "EFER.TCE",
        false,
        Scope::Amd,
        "invlpg would keep unrelated upper-level entries, which the page-table code does not expect",
    ),
    // UAIE——「the processor no longer performs a canonical address check on bits 63:57 of the logical
    // address for memory references that use either the DS or ES segment」（5.10.2）。
    (
        Register::Efer,
        20,
        "EFER.UAIE",
        false,
        Scope::Amd,
        "DS and ES references would skip the canonical check on bits 63:57, so a bad pointer would \
         alias instead of faulting",
    ),
];

/// 棚卸しにも要るビットにも入らないビットの扱い（2026-09-24。レビューの足す1点）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Other {
    /// どちらでもよい（理由つき）。
    Either,
    /// **分類していない**——**定義はあるが、分類に要る本文を読めていない。** **推測で分類しない。**
    /// **立っていたら `[WARN]` で名前を出す**（止めない——実機で無害なビットで起動できなくなるのを避ける）。
    Unclassified,
}

/// 棚卸しにも要るビットにも入らない、**定義のある**ビット（在りか・ビット・名前・扱い・定義する製造元・理由）。
///
/// **CR0・CR4・EFER の全ビットは、製造元ごとに、[`INVENTORY_BITS`]・[`REQUIRED_BITS`]・この表・予約
/// （[`RESERVED_INTEL`]・[`RESERVED_AMD`]）のちょうど 1 つに入る**（ホストのテストが守る）。**出所は Intel SDM
/// Vol.3A（253668-082US）の 2.5 節と 2.2.1 節、AMD APM Vol.2（24593 Rev. 3.45）の 3.1 節である。**
pub const OTHER_BITS: [(Register, u32, &str, Other, Scope, &str); 18] = [
    (
        Register::Cr0,
        4,
        "CR0.ET",
        Other::Either,
        Scope::Both,
        "fixed to 1 on current processors",
    ),
    (
        Register::Cr4,
        0,
        "CR4.VME",
        Other::Either,
        Scope::Both,
        "virtual-8086 mode does not exist in long mode",
    ),
    (
        Register::Cr4,
        2,
        "CR4.TSD",
        Other::Either,
        Scope::Both,
        "whether Ring 3 may run rdtsc; either is correct",
    ),
    (
        Register::Cr4,
        3,
        "CR4.DE",
        Other::Either,
        Scope::Both,
        "the kernel does not use the debug registers",
    ),
    (
        Register::Cr4,
        4,
        "CR4.PSE",
        Other::Either,
        Scope::Both,
        "PAE and long mode do not look at it",
    ),
    (
        Register::Cr4,
        7,
        "CR4.PGE",
        Other::Either,
        Scope::Both,
        "no page table entry sets G",
    ),
    (
        Register::Cr4,
        8,
        "CR4.PCE",
        Other::Either,
        Scope::Both,
        "whether Ring 3 may run rdpmc; either is correct",
    ),
    (
        Register::Cr4,
        11,
        "CR4.UMIP",
        Other::Either,
        Scope::Both,
        "a protection candidate, not turned on yet",
    ),
    (
        Register::Cr4,
        13,
        "CR4.VMXE",
        Other::Either,
        Scope::Intel,
        "the kernel does not use VMX",
    ),
    (
        Register::Cr4,
        14,
        "CR4.SMXE",
        Other::Either,
        Scope::Intel,
        "the kernel does not use SMX",
    ),
    (
        Register::Cr4,
        19,
        "CR4.KL",
        Other::Either,
        Scope::Intel,
        "the kernel does not use Key Locker",
    ),
    (
        Register::Cr4,
        20,
        "CR4.SMEP",
        Other::Either,
        Scope::Both,
        "a protection candidate, not turned on yet",
    ),
    (
        Register::Efer,
        11,
        "EFER.NXE",
        Other::Either,
        Scope::Both,
        "no page table entry sets XD yet",
    ),
    // **SVME——VMRUN・VMLOAD・VMSAVE は CPL 0 だけで、VMMCALL は仮想機械の外では #UD になる**（APM Vol.2
    // 15.5・15.18）。**CLGI と INVLPGA の CPL の決まりは APM Vol.3（命令の本文）にあり、取得できなかった。**
    (
        Register::Efer,
        12,
        "EFER.SVME",
        Other::Unclassified,
        Scope::Amd,
        "the CPL rules of CLGI and INVLPGA are in APM Vol.3, which could not be obtained",
    ),
    (
        Register::Efer,
        17,
        "EFER.MCOMMIT",
        Other::Unclassified,
        Scope::Amd,
        "what MCOMMIT does, and at which CPL, is in APM Vol.3, which could not be obtained",
    ),
    (
        Register::Efer,
        18,
        "EFER.INTWB",
        Other::Either,
        Scope::Amd,
        "it only makes wbinvd and wbnoinvd interruptible, and the kernel runs neither",
    ),
    (
        Register::Efer,
        21,
        "EFER.AIBRSE",
        Other::Either,
        Scope::Amd,
        "it only adds IBRS speculation protection at CPL 0",
    ),
    (
        Register::Efer,
        24,
        "EFER.EnhancedTlbi",
        Other::Either,
        Scope::Amd,
        "it only extends invlpgb and tlbsync, which the kernel does not run",
    ),
];

/// **Intel の予約のビット**（SDM Vol.3A 253668-082US の 2.5 節と 2.2.1 節。**0 であるべきとして扱う**）。
/// **添字は CR0・CR4・EFER の順である。**
pub const RESERVED_INTEL: [u64; 3] = [
    // CR0: 6〜15、17、19〜28、32〜63。
    0xFFFF_FFFF_1FFA_FFC0,
    // CR4: 15、26〜63。
    0xFFFF_FFFF_FC00_8000,
    // EFER: 1〜7、9、12〜63。
    0xFFFF_FFFF_FFFF_F2FE,
];

/// **AMD の予約のビット**（APM Vol.2 24593 Rev. 3.45 の 3.1.1・3.1.3・3.1.7 節。**0 であるべきとして扱う**）。
/// **CR0 の 6〜15・17・19〜28 は「do not change」と書かれている**が、**立っている形は見ないので 0 として扱う。**
pub const RESERVED_AMD: [u64; 3] = [
    // CR0: 6〜15、17、19〜28、32〜63（Intel と同じ）。
    0xFFFF_FFFF_1FFA_FFC0,
    // CR4: 13〜15、19、24〜63（VMXE・SMXE・KL・PKS・UINTR は Intel だけ）。
    0xFFFF_FFFF_FF08_E000,
    // EFER: 1〜7、9、16、19、22、23、25〜63。
    0xFFFF_FFFF_FEC9_02FE,
];

/// その製造元の予約（**Intel でも AMD でもなければ無い**——予約では止めない）。
pub fn reserved_mask(vendor: Vendor) -> Option<[u64; 3]> {
    match vendor {
        Vendor::Intel => Some(RESERVED_INTEL),
        Vendor::Amd => Some(RESERVED_AMD),
        Vendor::Other => None,
    }
}

/// ビットの分類（2026-09-24）。**製造元ごとに、どのビットもちょうど 1 つに入る**（ホストのテスト）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// 棚卸しが 0 であることに依っている（[`INVENTORY_BITS`]）。
    Inventory,
    /// カーネルが 1 であることに依っている（[`REQUIRED_BITS`]）。
    MustBeSet,
    /// カーネルが 0 であることに依っている（[`REQUIRED_BITS`]）。
    MustBeClear,
    /// どちらでもよい（[`OTHER_BITS`]）。
    Either,
    /// 分類していない（[`OTHER_BITS`]。**Intel でも AMD でもない製造元では、両者の意味が揃わないビットと、
    /// どちらかの予約のビット**）。
    Unclassified,
    /// 予約（0 であるべき）。
    Reserved,
}

/// **ビットをその製造元の表で分類する**（純粋ロジック）。**Intel でも AMD でもない製造元では、両者の分類が
/// 同じで予約でないものだけをその分類にし、残りは「分類していない」にする。**
pub fn classify(vendor: Vendor, register: Register, bit: u32) -> Class {
    if vendor == Vendor::Other {
        let intel = classify(Vendor::Intel, register, bit);
        return if intel == classify(Vendor::Amd, register, bit) && intel != Class::Reserved {
            intel
        } else {
            Class::Unclassified
        };
    }
    if INVENTORY_BITS
        .iter()
        .any(|&(on, at, _, _)| on == register && at == bit)
    {
        return Class::Inventory;
    }
    if let Some(&(_, _, _, must_be_set, _, _)) = REQUIRED_BITS
        .iter()
        .find(|&&(on, at, _, _, scope, _)| on == register && at == bit && scope.applies_to(vendor))
    {
        return if must_be_set {
            Class::MustBeSet
        } else {
            Class::MustBeClear
        };
    }
    if let Some(&(_, _, _, other, _, _)) = OTHER_BITS
        .iter()
        .find(|&&(on, at, _, _, scope, _)| on == register && at == bit && scope.applies_to(vendor))
    {
        return match other {
            Other::Either => Class::Either,
            Other::Unclassified => Class::Unclassified,
        };
    }
    // **どの表にも無ければ予約である**（ホストのテストが、予約の表と重ならず漏れないことを守る）。
    Class::Reserved
}

/// 在りかの値を引く。
fn value_of(register: Register, [cr0, cr4, efer]: [u64; 3]) -> u64 {
    match register {
        Register::Cr0 => cr0,
        Register::Cr4 => cr4,
        Register::Efer => efer,
    }
}

/// 3 つの在りか。
const REGISTERS: [Register; 3] = [Register::Cr0, Register::Cr4, Register::Efer];

/// 立っている予約のビット（在りかとビット。純粋ロジック）。**Intel でも AMD でもなければ 1 つも返さない。**
pub fn reserved_set(vendor: Vendor, values: [u64; 3]) -> impl Iterator<Item = (Register, u32)> {
    let mask = reserved_mask(vendor).unwrap_or([0; 3]);
    REGISTERS
        .into_iter()
        .zip(mask)
        .flat_map(move |(register, reserved)| {
            let value = value_of(register, values);
            (0..64u32)
                .filter(move |bit| value & reserved & (1u64 << bit) != 0)
                .map(move |bit| (register, bit))
        })
}

/// 立っている「分類していない」ビット（在りか・ビット・理由。純粋ロジック）。
pub fn unclassified_set(
    vendor: Vendor,
    values: [u64; 3],
) -> impl Iterator<Item = (Register, u32, &'static str)> {
    REGISTERS.into_iter().flat_map(move |register| {
        let value = value_of(register, values);
        (0..64u32)
            .filter(move |bit| {
                value & (1u64 << bit) != 0
                    && classify(vendor, register, *bit) == Class::Unclassified
            })
            .map(move |bit| {
                let why = OTHER_BITS
                    .iter()
                    .find(|&&(on, at, _, _, scope, _)| {
                        on == register && at == bit && scope.applies_to(vendor)
                    })
                    .map_or(
                        "Intel and AMD do not define it alike, and this vendor is neither",
                        |&(_, _, _, _, _, why)| why,
                    );
                (register, bit, why)
            })
    })
}

/// その製造元で「分類していない」最初のビット（破壊 `cpu-state-sees-an-unclassified-bit` が立てる）。
pub fn first_unclassified(vendor: Vendor) -> Option<(Register, u32)> {
    REGISTERS.into_iter().find_map(|register| {
        (0..64u32)
            .find(|&bit| classify(vendor, register, bit) == Class::Unclassified)
            .map(|bit| (register, bit))
    })
}

/// あるべき値と違うビット（純粋ロジック）。**その製造元の表に入るものだけを見る。**
pub fn required_violations(
    vendor: Vendor,
    values: [u64; 3],
) -> impl Iterator<Item = &'static (Register, u32, &'static str, bool, Scope, &'static str)> {
    REQUIRED_BITS
        .iter()
        .filter(move |(register, bit, _, must_be_set, scope, _)| {
            scope.applies_to(vendor)
                && (value_of(*register, values) & (1u64 << bit) != 0) != *must_be_set
        })
}

/// その製造元でカーネルが要るビットの数（行に出す）。
pub fn required_count(vendor: Vendor) -> usize {
    REQUIRED_BITS
        .iter()
        .filter(|entry| entry.4.applies_to(vendor))
        .count()
}

/// 在りかとビットを `CR4.UINTR` か `bit 40 of EFER` の形で出す。
pub struct BitName(pub Register, pub u32);

impl fmt::Display for BitName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match bit_name(self.0, self.1) {
            Some(name) => write!(f, "{}.{name}", self.0.name()),
            None => write!(f, "bit {} of {}", self.1, self.0.name()),
        }
    }
}

/// [`establish_required_bits_on_bsp`] の前後の CR0（起動ログへ出すため）。
static ESTABLISHED_CR0: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// **BSP で、カーネルが要る CR0 のビットを自分で立てる・落とす**（2026-09-24）——**WP と NE を立て、
/// CD と NW を落とす。** **FP のビットは触らない**（`fp::enable_on_this_cpu` が持つ）。
///
/// # Safety
///
/// **起動の最初期に、BSP で 1 回だけ呼ぶこと。** **PE と PG には触れない。** **CD と NW は同時に落とす**
/// （CD が 0 で NW が 1 の組は `#GP` になる）。
pub unsafe fn establish_required_bits_on_bsp() {
    use common::cpu::{
        read_cr0, CR0_CACHE_DISABLE, CR0_NOT_WRITE_THROUGH, CR0_NUMERIC_ERROR, CR0_WRITE_PROTECT,
    };
    let before = read_cr0();
    let after = (before | CR0_WRITE_PROTECT | CR0_NUMERIC_ERROR)
        & !(CR0_CACHE_DISABLE | CR0_NOT_WRITE_THROUGH);
    // 破壊 (2026-09-24, bsp-keeps-cd): **ファームウェアが CD を立てて渡し、カーネルが落とさない形**を
    // 作る。**OVMF と VirtualBox の EFI は CD を落として渡すので、立てて作る。** **見張りが CR0.CD を
    // 名指しして止まる。**
    #[cfg(feature = "bsp-keeps-cd-test")]
    let after = after | CR0_CACHE_DISABLE;
    if after != before {
        // SAFETY: 呼び出し側の契約。PE と PG を保ち、WP・NE・CD・NW だけを変える。
        unsafe { common::cpu::write_cr0(after) };
    }
    ESTABLISHED_CR0[0].store(before, Ordering::SeqCst);
    ESTABLISHED_CR0[1].store(common::cpu::read_cr0(), Ordering::SeqCst);
}

/// [`establish_required_bits_on_bsp`] の前後の CR0 を 1 行出す（ロガーが使えるようになってから）。
pub fn report_established_bits(logger: &mut Logger<SerialPort>) {
    let before = ESTABLISHED_CR0[0].load(Ordering::SeqCst);
    let after = ESTABLISHED_CR0[1].load(Ordering::SeqCst);
    logger.info(format_args!(
        "cpu-state: the kernel set the CR0 bits it needs on the BSP (WP and NE set, CD and NW \
         clear): CR0 {before:#x} -> {after:#x} [read back]"
    ));
}

/// 立っていて棚卸しを崩すビット（純粋ロジック）。
/// **棚卸しのビットは、Intel と AMD が同じ意味で定義している**（製造元を問わない）。
pub fn inventory_violations(
    values: [u64; 3],
) -> impl Iterator<Item = &'static (Register, u32, &'static str, &'static str)> {
    INVENTORY_BITS
        .iter()
        .filter(move |(register, bit, _, _)| value_of(*register, values) & (1u64 << bit) != 0)
}

/// BSP の CR0・CR4・EFER（[`check_and_report`] が控える。**AP が写し、BSP が突き合わせる元**）。
static BSP_STATE: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// BSP の値を控えたか。
static BSP_RECORDED: AtomicBool = AtomicBool::new(false);
/// AP ごとに控えた値（添字はスロット）。
static AP_STATE: [[AtomicU64; 3]; MAX_CPUS] =
    [const { [const { AtomicU64::new(0) }; 3] }; MAX_CPUS];
/// AP ごとに控えたか。
static AP_RECORDED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// BSP が AP の控えを待つ上限（ティック。1 ティック = 10ms）。
const AP_RECORD_WAIT_TICKS: u64 = 200;

fn read_this_cpu() -> [u64; 3] {
    [
        common::cpu::read_cr0(),
        common::cpu::read_cr4(),
        common::cpu::read_efer().raw(),
    ]
}

/// CR0・CR4・EFER を読み、1 行出し、棚卸しを崩すビットが立っていれば止める。**BSP の値を控える。**
/// **製造元を CPUID で見分け、その製造元の表で判定する**（2026-09-24。運用者の決定）。
pub fn check_and_report(logger: &mut Logger<SerialPort>) {
    let [cr0, cr4, efer] = read_this_cpu();
    for (slot, value) in BSP_STATE.iter().zip([cr0, cr4, efer]) {
        slot.store(value, Ordering::SeqCst);
    }
    BSP_RECORDED.store(true, Ordering::SeqCst);
    let signature = read_vendor_signature();
    let vendor = Vendor::from_signature(&signature);
    // 破壊 (2026-09-24, cpu-state-sees-sce): EFER.SCE が立っているものとして判定する。
    // **MSR は書かない**——**`syscall` 命令が本当に入口になる形は作らない。**
    #[cfg(feature = "cpu-state-sees-sce-test")]
    let efer = efer | common::cpu::Efer::SYSCALL_ENABLE;
    // 破壊 (2026-09-24, cpu-state-sees-an-unclassified-bit): **その製造元で「分類していない」最初のビット**が
    // 立っているものとして判定する（レジスタは書かない）。**[WARN] が名前つきで出て、起動は止まらない。**
    // **QEMU の既定（AMD）では EFER.SVME である。**
    #[cfg(feature = "cpu-state-sees-an-unclassified-bit-test")]
    let [cr0, cr4, efer] = match first_unclassified(vendor) {
        Some((register, bit)) => {
            let mut values = [cr0, cr4, efer];
            values[register as usize] |= 1u64 << bit;
            values
        }
        None => [cr0, cr4, efer],
    };
    // 破壊 (2026-09-24, cpu-state-sees-ffxsr): EFER.FFXSR（14 番。AMD では「0 であるべき」）が立っている
    // ものとして判定する（MSR は書かない。**QEMU の TCG は FFXSR を持たない**——実測）。
    #[cfg(feature = "cpu-state-sees-ffxsr-test")]
    let efer = efer | 1 << 14;
    // 破壊 (2026-09-24, kernel-uses-gs): `gs:` を読む関数を像に残す（呼ばない）。
    // **基底の項目（逆アセンブルで `fs:`・`gs:` を数える）が捕まえる。**
    #[cfg(feature = "kernel-uses-gs-test")]
    core::hint::black_box(read_through_gs as fn() -> u64);
    let values = [cr0, cr4, efer];
    logger.info(format_args!(
        "cpu-state: the CPU vendor is {} (CPUID leaf 0), so CR0, CR4 and EFER are judged by {}",
        core::str::from_utf8(&signature).unwrap_or("<not ASCII>"),
        vendor.manual()
    ));
    logger.info(format_args!(
        "cpu-state: CR0={cr0:#x} CR4={cr4:#x} EFER={efer:#x}; the inventory of user-changeable CPU \
         state (ADR-0018 Addendum 9) rests on CR0.AM, CR4.SMAP, CR4.FSGSBASE, CR4.PKE, \
         CR4.OSXSAVE, CR4.PVI and EFER.SCE being 0"
    ));
    let mut violated = false;
    for (_, _, name, must_be_set, _, needs) in required_violations(vendor, values) {
        violated = true;
        logger.error(format_args!(
            "cpu-state: {name} is {}, but the kernel needs it to be {} ({needs}); the bits the \
             kernel needs are set on the BSP by cpu_state::establish_required_bits_on_bsp and the \
             APs copy the BSP",
            u8::from(!*must_be_set),
            u8::from(*must_be_set)
        ));
    }
    if !violated {
        logger.info(format_args!(
            "cpu-state: the {} bit(s) the kernel needs hold ({})",
            required_count(vendor),
            RequiredSummary(vendor)
        ));
    }
    for (register, bit) in reserved_set(vendor, values) {
        violated = true;
        logger.error(format_args!(
            "cpu-state: bit {bit} of {} is 1, but {} reserves it; a set reserved bit means a \
             processor newer than the classification - classify it in kernel::cpu_state before \
             booting on it",
            register.name(),
            vendor.manual()
        ));
    }
    for (register, bit, why) in unclassified_set(vendor, values) {
        logger.warn(format_args!(
            "cpu-state: {} is 1 and is not classified yet ({why}); it does not stop the boot, but \
             classify it in kernel::cpu_state",
            BitName(register, bit)
        ));
    }
    for (_, _, name, breaks) in inventory_violations(values) {
        violated = true;
        logger.error(format_args!(
            "cpu-state: {name} is 1, but the inventory rests on it being 0 ({breaks}); redo the \
             inventory in ADR-0018 Addendum 9 before turning it on"
        ));
    }
    if violated {
        logger.error(format_args!("cpu-state: halting"));
        common::cpu::halt_forever();
    }
    // 破壊 (2026-09-24, mce-off-after-the-check): **判定の後で** BSP の CR4.MCE を落とす（AP は控えた値を
    // 写すので 1 のまま。注入は CPU 0 へ行う）。
    // **起動は進み、注入した機械チェックが #MC にならず shutdown になる**——**機械の変種の判定が捕まえる。**
    #[cfg(feature = "mce-off-after-the-check-test")]
    // SAFETY: MCE だけを落とす。起動の途中の BSP で、割り込みは禁止のままである。
    unsafe {
        common::cpu::write_cr4(common::cpu::read_cr4() & !(1 << 6));
    }
}

/// AP が BSP の CR0・CR4・EFER を写す（2026-09-24）。**AP の Rust の入口の最初で 1 回だけ呼ぶ。**
///
/// **写す順は CR4 → EFER → CR0 である**——**CR0 で CD と NW を落とし（キャッシュが効く）、WP を立てる**
/// のを最後にする。**BSP の値が控えられていなければ何もしない**（突き合わせが、控えが無いことで止まる）。
///
/// # Safety
///
/// **AP の起動の途中で、長モードに居て、割り込みが禁止されていること。** **BSP の値は同じカーネルの
/// 同じ長モードの値である**（PG・PE・PAE・LME は BSP でも立っている）。
pub unsafe fn adopt_bsp_state_on_this_ap() {
    // 破壊 (2026-09-24, ap-keeps-its-own-control-registers): 写さない。**直す前の形である**——
    // **AP は INIT の直後の CR0（CD・NW が 1、WP・NE が 0）のまま走り、突き合わせで止まる。**
    if cfg!(feature = "ap-keeps-its-own-control-registers-test") {
        return;
    }
    if !BSP_RECORDED.load(Ordering::SeqCst) {
        return;
    }
    let [cr0, cr4, efer] = [0, 1, 2].map(|index| BSP_STATE[index].load(Ordering::SeqCst));
    // SAFETY: 呼び出し側の契約。3 つとも同じカーネルの BSP が長モードで使っている値である。
    unsafe {
        common::cpu::write_cr4(cr4);
        common::cpu::write_efer(common::cpu::Efer::from_raw(efer));
        common::cpu::write_cr0(cr0);
    }
}

/// AP が自分の CR0・CR4・EFER を読んで控える（2026-09-24。起動の終わりに呼ぶ）。
pub fn record_this_ap(slot: usize) {
    if slot >= MAX_CPUS {
        return;
    }
    for (cell, value) in AP_STATE[slot].iter().zip(read_this_cpu()) {
        cell.store(value, Ordering::SeqCst);
    }
    AP_RECORDED[slot].store(true, Ordering::SeqCst);
}

/// ビットの名前（純粋ロジック）。**知らないビットは `None`**（行には番号で出す）。
pub fn bit_name(register: Register, bit: u32) -> Option<&'static str> {
    Some(match (register, bit) {
        (Register::Cr0, 0) => "PE",
        (Register::Cr0, 1) => "MP",
        (Register::Cr0, 2) => "EM",
        (Register::Cr0, 3) => "TS",
        (Register::Cr0, 4) => "ET",
        (Register::Cr0, 5) => "NE",
        (Register::Cr0, 16) => "WP",
        (Register::Cr0, 18) => "AM",
        (Register::Cr0, 29) => "NW",
        (Register::Cr0, 30) => "CD",
        (Register::Cr0, 31) => "PG",
        (Register::Cr4, 0) => "VME",
        (Register::Cr4, 1) => "PVI",
        (Register::Cr4, 2) => "TSD",
        (Register::Cr4, 3) => "DE",
        (Register::Cr4, 4) => "PSE",
        (Register::Cr4, 5) => "PAE",
        (Register::Cr4, 6) => "MCE",
        (Register::Cr4, 7) => "PGE",
        (Register::Cr4, 8) => "PCE",
        (Register::Cr4, 9) => "OSFXSR",
        (Register::Cr4, 10) => "OSXMMEXCPT",
        (Register::Cr4, 11) => "UMIP",
        (Register::Cr4, 12) => "LA57",
        (Register::Cr4, 13) => "VMXE",
        (Register::Cr4, 14) => "SMXE",
        (Register::Cr4, 16) => "FSGSBASE",
        (Register::Cr4, 17) => "PCIDE",
        (Register::Cr4, 18) => "OSXSAVE",
        (Register::Cr4, 19) => "KL",
        (Register::Cr4, 20) => "SMEP",
        (Register::Cr4, 21) => "SMAP",
        (Register::Cr4, 22) => "PKE",
        (Register::Cr4, 23) => "CET",
        (Register::Cr4, 24) => "PKS",
        (Register::Cr4, 25) => "UINTR",
        (Register::Efer, 0) => "SCE",
        (Register::Efer, 8) => "LME",
        (Register::Efer, 10) => "LMA",
        (Register::Efer, 11) => "NXE",
        (Register::Efer, 12) => "SVME",
        (Register::Efer, 13) => "LMSLE",
        (Register::Efer, 14) => "FFXSR",
        (Register::Efer, 15) => "TCE",
        (Register::Efer, 17) => "MCOMMIT",
        (Register::Efer, 18) => "INTWB",
        (Register::Efer, 20) => "UAIE",
        (Register::Efer, 21) => "AIBRSE",
        (Register::Efer, 24) => "EnhancedTlbi",
        _ => return None,
    })
}

/// 「カーネルが要るビット」の行の括弧の中身（2026-09-24）。**[`REQUIRED_BITS`] から組み立てる**
/// ——**文字列で持っていたので、全ビットの分類で MCE・TS・LA57・PCIDE を足したとき、行だけが古いまま
/// 残った**（起動ログの参照の差で気づいた）。**在りかごとに「立っているべき」「落ちているべき」の順で、
/// ビットの番号の順に並べる。**
pub struct RequiredSummary(pub Vendor);

impl fmt::Display for RequiredSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first_group = true;
        for register in [Register::Cr0, Register::Cr4, Register::Efer] {
            let mut first_in_register = true;
            for must_be_set in [true, false] {
                let mut any = false;
                for bit in 0..64u32 {
                    let Some(&(_, _, name, _, _, _)) =
                        REQUIRED_BITS.iter().find(|&&(on, at, _, set, scope, _)| {
                            on == register
                                && at == bit
                                && set == must_be_set
                                && scope.applies_to(self.0)
                        })
                    else {
                        continue;
                    };
                    if any {
                        f.write_str(", ")?;
                    } else if !first_group {
                        f.write_str("; ")?;
                    }
                    first_group = false;
                    if first_in_register {
                        f.write_str(name)?;
                        first_in_register = false;
                    } else {
                        f.write_str(name.split_once('.').map_or(name, |(_, short)| short))?;
                    }
                    any = true;
                }
                if any {
                    f.write_str(if must_be_set { " set" } else { " clear" })?;
                }
            }
        }
        Ok(())
    }
}

/// BSP と AP で違うビットを、名前と「AP の側で立っているか」で並べる（行に出すため。確保しない）。
pub struct DifferingBits {
    pub register: Register,
    pub bsp: u64,
    pub ap: u64,
}

impl fmt::Display for DifferingBits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for bit in 0..64u32 {
            let mask = 1u64 << bit;
            if (self.bsp ^ self.ap) & mask == 0 {
                continue;
            }
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            let side = if self.ap & mask != 0 {
                "set on the AP"
            } else {
                "clear on the AP"
            };
            match bit_name(self.register, bit) {
                Some(name) => write!(f, "{name} {side}")?,
                None => write!(f, "bit {bit} {side}")?,
            }
        }
        Ok(())
    }
}

/// **起きた AP の CR0・CR4・EFER が、BSP の値と一致することを確かめる**（2026-09-24）。
/// **食い違えば、違うビットの名前を出して止まる。** **AP の控えは上限つきで待つ。**
pub fn check_aps_match_bsp(logger: &mut Logger<SerialPort>, started: usize) {
    let bsp = [0, 1, 2].map(|index| BSP_STATE[index].load(Ordering::SeqCst));
    let names = ["CR0", "CR4", "EFER"];
    let registers = [Register::Cr0, Register::Cr4, Register::Efer];
    let mut failed = false;
    for slot in 1..=started.min(MAX_CPUS - 1) {
        let start = crate::idt::timer_ticks();
        while !AP_RECORDED[slot].load(Ordering::SeqCst)
            && crate::idt::timer_ticks().wrapping_sub(start) < AP_RECORD_WAIT_TICKS
        {
            core::hint::spin_loop();
        }
        if !AP_RECORDED[slot].load(Ordering::SeqCst) {
            logger.error(format_args!(
                "cpu-state: ap {slot} did not record its CR0, CR4 and EFER within \
                 {AP_RECORD_WAIT_TICKS} tick(s)"
            ));
            failed = true;
            continue;
        }
        let ap = [0, 1, 2].map(|index| AP_STATE[slot][index].load(Ordering::SeqCst));
        if ap == bsp {
            logger.info(format_args!(
                "cpu-state: ap {slot} CR0={:#x} CR4={:#x} EFER={:#x} matches the BSP",
                ap[0], ap[1], ap[2]
            ));
            continue;
        }
        failed = true;
        for index in 0..3 {
            if ap[index] != bsp[index] {
                logger.error(format_args!(
                    "cpu-state: ap {slot} differs from the BSP in {} ({:#x} on the BSP, {:#x} on \
                     the AP): {}; the inventory in ADR-0018 Addendum 9 is about every CPU - redo the \
                     inventory in ADR-0018 Addendum 9",
                    names[index],
                    bsp[index],
                    ap[index],
                    DifferingBits {
                        register: registers[index],
                        bsp: bsp[index],
                        ap: ap[index],
                    }
                ));
            }
        }
    }
    if failed {
        logger.error(format_args!("cpu-state: halting"));
        common::cpu::halt_forever();
    }
    logger.info(format_args!(
        "cpu-state: {started} started AP(s) match the BSP's CR0, CR4 and EFER"
    ));
}

/// 破壊 `kernel-uses-gs-test` の本体。**呼ばない**（像に残すだけ）。
#[cfg(feature = "kernel-uses-gs-test")]
fn read_through_gs() -> u64 {
    let value: u64;
    // SAFETY: 呼ばれない（`check_and_report` が番地を取るだけ）。呼ばれた場合も GS の基底 + 0 を
    // 読むだけで、書かない。
    unsafe {
        core::arch::asm!("mov {}, gs:[0]", out(reg) value, options(nostack, readonly, preserves_flags));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-24 の実測（カーネルが走る間の `-d int` の記録。8535 回とも同じ値）。**QEMU（AMD）と
    /// VirtualBox（Intel）の BSP で同じ値だった。**
    const CR0: u64 = 0x8001_0033;
    const CR4: u64 = 0x668;
    const EFER: u64 = 0xd00;
    const MEASURED: [u64; 3] = [CR0, CR4, EFER];
    const VENDORS: [Vendor; 3] = [Vendor::Intel, Vendor::Amd, Vendor::Other];

    #[test]
    fn the_vendor_is_read_from_the_cpuid_signature() {
        assert_eq!(Vendor::from_signature(b"GenuineIntel"), Vendor::Intel);
        assert_eq!(Vendor::from_signature(b"AuthenticAMD"), Vendor::Amd);
        assert_eq!(Vendor::from_signature(b"HygonGenuine"), Vendor::Other);
    }

    #[test]
    fn the_measured_state_breaks_nothing() {
        assert_eq!(inventory_violations(MEASURED).count(), 0);
    }

    #[test]
    fn each_inventory_bit_is_named_when_set() {
        let names = |values| -> Vec<&str> {
            inventory_violations(values)
                .map(|(_, _, name, _)| *name)
                .collect()
        };
        assert_eq!(names([CR0 | 1 << 18, CR4, EFER]), vec!["CR0.AM"]);
        assert_eq!(names([CR0, CR4 | 1 << 21, EFER]), vec!["CR4.SMAP"]);
        assert_eq!(names([CR0, CR4 | 1 << 16, EFER]), vec!["CR4.FSGSBASE"]);
        assert_eq!(names([CR0, CR4 | 1 << 22, EFER]), vec!["CR4.PKE"]);
        assert_eq!(names([CR0, CR4 | 1 << 18, EFER]), vec!["CR4.OSXSAVE"]);
        assert_eq!(names([CR0, CR4, EFER | 1]), vec!["EFER.SCE"]);
        assert_eq!(names([CR0, CR4 | 1 << 1, EFER]), vec!["CR4.PVI"]);
    }

    /// 2026-09-24 の実測（QEMU の `-smp 2`。直す前の AP と BSP）で、違うビットを名前で並べる。
    #[test]
    fn the_measured_ap_differences_are_named() {
        let cr0 = DifferingBits {
            register: Register::Cr0,
            bsp: CR0,
            ap: 0xe000_0013,
        };
        assert_eq!(
            format!("{cr0}"),
            "NE clear on the AP, WP clear on the AP, NW set on the AP, CD set on the AP"
        );
        let cr4 = DifferingBits {
            register: Register::Cr4,
            bsp: CR4,
            ap: 0x620,
        };
        assert_eq!(format!("{cr4}"), "DE clear on the AP, MCE clear on the AP");
        let unknown = DifferingBits {
            register: Register::Efer,
            bsp: 0,
            ap: 1 << 40,
        };
        assert_eq!(format!("{unknown}"), "bit 40 set on the AP");
    }

    /// **行は製造元ごとの表から組み立てる**（2026-09-24）。
    #[test]
    fn the_required_bits_line_is_built_from_the_vendor_table() {
        let common = "CR0.PE, MP, NE, WP, PG set; EM, TS, NW, CD clear; CR4.PAE, MCE, OSFXSR, \
                      OSXMMEXCPT set; LA57, PCIDE, CET";
        assert_eq!(
            format!("{}", RequiredSummary(Vendor::Intel)),
            format!("{common}, PKS, UINTR clear; EFER.LME, LMA set")
        );
        assert_eq!(
            format!("{}", RequiredSummary(Vendor::Amd)),
            format!("{common} clear; EFER.LME, LMA set; LMSLE, FFXSR, TCE, UAIE clear")
        );
        assert_eq!(
            format!("{}", RequiredSummary(Vendor::Other)),
            format!("{common} clear; EFER.LME, LMA set")
        );
        assert_eq!(
            VENDORS.map(required_count),
            [20, 22, 18],
            "Intel, AMD, other"
        );
    }

    #[test]
    fn the_measured_state_has_every_required_bit() {
        for vendor in VENDORS {
            assert_eq!(
                required_violations(vendor, MEASURED).count(),
                0,
                "{vendor:?}"
            );
        }
    }

    /// **直す前の AP の値**（2026-09-24 の実測）で、崩れていたビットを名前で拾う。
    #[test]
    fn the_measured_ap_before_the_fix_breaks_the_required_bits() {
        for vendor in VENDORS {
            let names: Vec<&str> = required_violations(vendor, [0xe000_0013, 0x620, 0x500])
                .map(|(_, _, name, _, _, _)| *name)
                .collect();
            assert_eq!(
                names,
                vec!["CR0.NE", "CR0.WP", "CR0.NW", "CR0.CD", "CR4.MCE"],
                "{vendor:?}"
            );
        }
    }

    /// **製造元ごとに、全ビットが棚卸し・要るビット・その他・予約のちょうど 1 つに入る**（2026-09-24。
    /// 運用者の決定）。
    #[test]
    fn every_bit_of_every_register_is_classified_exactly_once_for_each_vendor() {
        for vendor in [Vendor::Intel, Vendor::Amd] {
            let reserved = reserved_mask(vendor).unwrap();
            for (index, register) in REGISTERS.into_iter().enumerate() {
                for bit in 0..64u32 {
                    let homes = INVENTORY_BITS
                        .iter()
                        .filter(|e| e.0 == register && e.1 == bit)
                        .count()
                        + REQUIRED_BITS
                            .iter()
                            .filter(|e| e.0 == register && e.1 == bit && e.4.applies_to(vendor))
                            .count()
                        + OTHER_BITS
                            .iter()
                            .filter(|e| e.0 == register && e.1 == bit && e.4.applies_to(vendor))
                            .count()
                        + usize::from(reserved[index] & (1u64 << bit) != 0);
                    assert_eq!(
                        homes,
                        1,
                        "{vendor:?}: {} bit {bit} is in {homes} home(s)",
                        register.name()
                    );
                }
            }
        }
    }

    /// **Intel でも AMD でもない製造元は、両者が同じ意味で定義するビットだけで判定し、予約では止めない**
    /// （運用者の決定）。
    #[test]
    fn another_vendor_is_judged_only_by_the_bits_intel_and_amd_define_alike() {
        for register in REGISTERS {
            for bit in 0..64u32 {
                let other = classify(Vendor::Other, register, bit);
                assert_ne!(other, Class::Reserved, "{} bit {bit}", register.name());
                let intel = classify(Vendor::Intel, register, bit);
                if intel == classify(Vendor::Amd, register, bit) && intel != Class::Reserved {
                    assert_eq!(other, intel, "{} bit {bit}", register.name());
                } else {
                    assert_eq!(other, Class::Unclassified, "{} bit {bit}", register.name());
                }
            }
        }
        assert_eq!(classify(Vendor::Other, Register::Cr0, 31), Class::MustBeSet);
        assert_eq!(
            classify(Vendor::Other, Register::Cr4, 24),
            Class::Unclassified
        );
        assert_eq!(reserved_set(Vendor::Other, [u64::MAX; 3]).count(), 0);
    }

    /// **表の名前は、ビットの名前と一致する**（打ち違えると、行の名前と判定の対象がずれる）。
    #[test]
    fn every_table_name_matches_the_bit_name() {
        let entries = INVENTORY_BITS
            .iter()
            .map(|e| (e.0, e.1, e.2))
            .chain(REQUIRED_BITS.iter().map(|e| (e.0, e.1, e.2)))
            .chain(OTHER_BITS.iter().map(|e| (e.0, e.1, e.2)));
        for (register, bit, name) in entries {
            assert_eq!(format!("{}", BitName(register, bit)), name);
        }
    }

    /// 2026-09-24 の実測（QEMU と VirtualBox の BSP）には、どの製造元の表でも、予約のビットも分類して
    /// いないビットも無い。
    #[test]
    fn the_measured_state_sets_no_reserved_and_no_unclassified_bit() {
        for vendor in VENDORS {
            assert_eq!(reserved_set(vendor, MEASURED).count(), 0, "{vendor:?}");
            assert_eq!(unclassified_set(vendor, MEASURED).count(), 0, "{vendor:?}");
        }
    }

    #[test]
    fn a_set_reserved_bit_and_a_set_unclassified_bit_are_named() {
        let intel: Vec<(Register, u32)> = reserved_set(
            Vendor::Intel,
            [CR0 | 1 << 40, CR4 | 1 << 15, EFER | 1 << 14],
        )
        .collect();
        assert_eq!(
            intel,
            vec![
                (Register::Cr0, 40),
                (Register::Cr4, 15),
                (Register::Efer, 14)
            ]
        );
        // **PKS は AMD では予約である。**
        let amd: Vec<(Register, u32)> =
            reserved_set(Vendor::Amd, [CR0, CR4 | 1 << 24, EFER | 1 << 16]).collect();
        assert_eq!(amd, vec![(Register::Cr4, 24), (Register::Efer, 16)]);
        let unclassified: Vec<String> =
            unclassified_set(Vendor::Amd, [CR0, CR4, EFER | 1 << 12 | 1 << 17])
                .map(|(register, bit, _)| format!("{}", BitName(register, bit)))
                .collect();
        assert_eq!(unclassified, vec!["EFER.SVME", "EFER.MCOMMIT"]);
        let other: Vec<(String, &str)> =
            unclassified_set(Vendor::Other, [CR0, CR4 | 1 << 24, EFER])
                .map(|(register, bit, why)| (format!("{}", BitName(register, bit)), why))
                .collect();
        assert_eq!(
            other,
            vec![(
                "CR4.PKS".to_string(),
                "Intel and AMD do not define it alike, and this vendor is neither"
            )]
        );
    }

    /// 破壊 `cpu-state-sees-an-unclassified-bit` が立てるビット。**Intel の表には「分類していない」が無い。**
    #[test]
    fn the_first_unclassified_bit_is_what_the_sabotage_sets() {
        assert_eq!(first_unclassified(Vendor::Amd), Some((Register::Efer, 12)));
        assert_eq!(first_unclassified(Vendor::Intel), None);
        assert_eq!(first_unclassified(Vendor::Other), Some((Register::Cr0, 6)));
    }

    /// **FFXSR は AMD では「0 であるべき」、Intel では予約である**（運用者の決定。APM Vol.2 3.1.7）。
    #[test]
    fn ffxsr_must_be_clear_on_amd_and_is_reserved_on_intel() {
        let names: Vec<&str> = required_violations(Vendor::Amd, [CR0, CR4, EFER | 1 << 14])
            .map(|(_, _, name, _, _, _)| *name)
            .collect();
        assert_eq!(names, vec!["EFER.FFXSR"]);
        assert_eq!(classify(Vendor::Intel, Register::Efer, 14), Class::Reserved);
    }

    /// **SMEP と UMIP は棚卸しの結論を崩さない**（起動ログの参照が見る）。
    #[test]
    fn smep_and_umip_do_not_break_the_inventory() {
        assert_eq!(
            inventory_violations([CR0, CR4 | 1 << 20 | 1 << 11, EFER]).count(),
            0
        );
    }
}
