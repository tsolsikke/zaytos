//! GDT と TSS（M4-a）。
//!
//! - [`layout`][mod@layout]: ディスクリプタの符号化（純粋ロジック、ホスト
//!   `cargo test` で検証）。
//! - このモジュール: 実体の静的確保と `lgdt` / `ltr`（unsafe）。
//!
//! kernel はこれまで UEFI が用意した GDT をそのまま使っていた。UEFI 由来の
//! テーブルは `EfiBootServicesData` の回収（ADR-0010）を解禁すれば上書き
//! されうるため、自前のものへ移る。M4-b の IDT はここで定義したコード
//! セレクタを参照し、ダブルフォルトハンドラは TSS の IST を使う。
//!
//! GDT・TSS ともに `.bss` の静的領域に置く。フレームアロケータより前に
//! ロードできること、kernel イメージの一部として既にマップ済み・予約済みで
//! あることが理由（[`crate::stack`] と同じ）。

pub mod layout;

use core::ptr::addr_of_mut;

use common::percpu::{PerCpu, MAX_CPUS};

use layout::{
    tss_descriptor, user_segment_descriptor, SegmentSelector, TaskStateSegment, KERNEL_CODE_ACCESS,
    KERNEL_CODE_FLAGS, KERNEL_DATA_ACCESS, KERNEL_DATA_FLAGS, USER_CODE32_FLAGS, USER_CODE64_FLAGS,
    USER_CODE_ACCESS, USER_DATA_ACCESS, USER_DATA_FLAGS,
};

/// GDT のエントリ数。null / カーネルコード / カーネルデータ / ユーザーコード32 /
/// ユーザーデータ / ユーザーコード64 / TSS（16 バイト = 2 スロット）。並びは
/// SYSCALL/SYSRET の STAR 互換順に固定する（ADR-0020）。
const GDT_ENTRY_COUNT: usize = 8;

pub const NULL_INDEX: u16 = 0;
pub const KERNEL_CODE_INDEX: u16 = 1;
pub const KERNEL_DATA_INDEX: u16 = 2;
/// ユーザー 32bit コード。STAR 互換順を満たす枠で、M5-e/f では使わない。
pub const USER_CODE32_INDEX: u16 = 3;
/// ユーザーデータ（SYSRET では STAR 基準 +8）。
pub const USER_DATA_INDEX: u16 = 4;
/// ユーザー 64bit コード（SYSRET では STAR 基準 +16）。
pub const USER_CODE64_INDEX: u16 = 5;
/// TSS は 16 バイトなので、ここから 2 スロット（6, 7）を占める。ユーザー用
/// ディスクリプタを STAR 互換順に前へ置いたため、M4-a の index 3 から後ろへ
/// ずれた（M5-e-1）。
const TSS_INDEX: u16 = 6;

/// カーネルコードセグメントのセレクタ。M4-b の IDT エントリが参照する。
pub const KERNEL_CODE_SELECTOR: SegmentSelector = SegmentSelector::new(KERNEL_CODE_INDEX, 0);
/// カーネルデータセグメントのセレクタ。
pub const KERNEL_DATA_SELECTOR: SegmentSelector = SegmentSelector::new(KERNEL_DATA_INDEX, 0);
/// ユーザー 64bit コードのセレクタ（RPL=3）。M5-e-3 の iretq 偽フレームで CS に
/// 積む。
pub const USER_CODE_SELECTOR: SegmentSelector = SegmentSelector::new(USER_CODE64_INDEX, 3);
/// ユーザーデータのセレクタ（RPL=3）。M5-e-3 の iretq 偽フレームで SS に積む。
pub const USER_DATA_SELECTOR: SegmentSelector = SegmentSelector::new(USER_DATA_INDEX, 3);
/// TSS のセレクタ。`ltr` に渡す。
pub const TSS_SELECTOR: SegmentSelector = SegmentSelector::new(TSS_INDEX, 0);

/// ダブルフォルトに割り当てる IST の番号（1 始まり）。
/// M4-b の IDT エントリでこの番号を指定する。
pub const DOUBLE_FAULT_IST_INDEX: usize = 1;

/// ページフォルトに割り当てる IST の番号（1 始まり、M5-b）。
/// ガードページに触れた #PF が、溢れた通常スタックの上ではなく専用スタックで
/// 動くようにするため、IDT の #PF ゲートでこの番号を指定する（ADR-0019 §3.1）。
pub const PAGE_FAULT_IST_INDEX: usize = 2;

/// GDT と TSS はコアごとに持つ（seam整備3c、ADR-0023）。各コアが自分の GDT を
/// 構築して `lgdt`/`ltr` し、自分の TSS（RSP0・IST）を持つ。GDT も per-CPU に
/// するのは、共有 GDT にすると TSS ディスクリプタ（long mode で 16 バイト = 2
/// エントリ）をコアごとに別スロットへ置く必要が生じ、GDT レイアウトが MAX_CPUS
/// に比例して STAR 互換順（ADR-0020）と絡むため。per-CPU なら各コアの GDT
/// レイアウトが従来のまま保たれる。1 コアあたり 64 バイトで増分は無視できる。
///
/// シングルコア（`MAX_CPUS = 1`）では [`PerCpu::this_cpu_ptr`] が常に唯一の
/// スロットを指すので、構築・ロードされるテーブルは従来と同一である。
static mut GDT: PerCpu<[u64; GDT_ENTRY_COUNT]> = PerCpu::new([[0; GDT_ENTRY_COUNT]; MAX_CPUS]);
static mut TSS: PerCpu<TaskStateSegment> =
    PerCpu::new([const { TaskStateSegment::new() }; MAX_CPUS]);

/// `lgdt` / `sgdt` が扱うディスクリプタテーブルレジスタの形。
///
/// limit（2 バイト）に base（8 バイト）が続く。`packed` にしないと
/// base が 8 バイト境界へ寄せられ、CPU が別の場所を読む。
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// GDT と TSS を構築してロードし、セグメントレジスタを自前のものへ切り替える。
///
/// この関数から戻った時点で、CS/DS/ES/SS/FS/GS はすべて自前の GDT の
/// ディスクリプタを指し、TR は自前の TSS を指している。
///
/// # Safety
///
/// - 起動時に 1 回だけ呼ぶこと。
/// - 呼び出し時点で割り込みが禁止されていること。GDT の入れ替え中に割り込みが
///   入ると、古いセレクタと新しいテーブルが混ざった状態でハンドラへ入る。
/// - `double_fault_stack_top` と `page_fault_stack_top` が、通常のスタックとも
///   互いとも別の、有効でマップ済みのスタック上端であること。
pub unsafe fn init(double_fault_stack_top: u64, page_fault_stack_top: u64) {
    // TSS を先に埋める。GDT の TSS ディスクリプタがそのアドレスを指すため。
    // SAFETY: 起動時の単一実行文脈であり、他に誰もこの static に触れていない。
    // 自コアのスロットへ書く（`this_cpu_ptr` の契約: `this` は有効な static、
    // 書き込みは単一文脈内）。
    unsafe {
        let tss = PerCpu::this_cpu_ptr(addr_of_mut!(TSS));
        (*tss).interrupt_stack_table[DOUBLE_FAULT_IST_INDEX - 1] = double_fault_stack_top;
        (*tss).interrupt_stack_table[PAGE_FAULT_IST_INDEX - 1] = page_fault_stack_top;
        // RSP0 は特権レベルが下がる遷移（ユーザー → カーネル）で使われる。
        // ユーザーモードを導入する M5 以降まで実際には効かないが、
        // 0 のままにしておくと、その時点で気づきにくい形で壊れる。
        // 現時点では通常のカーネルスタックと同じ場所を指しておく。
        (*tss).privilege_stack_table[0] = crate::stack::kernel_stack_range().top.as_u64();
    }

    // SAFETY: 同上（起動時の単一文脈、自コアのスロット）。読み取り目的で
    // アドレスを取る。
    let tss_base = unsafe { PerCpu::this_cpu_ptr(addr_of_mut!(TSS)) } as u64;
    let tss_limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u32;
    let (tss_low, tss_high) = tss_descriptor(tss_base, tss_limit);

    // SAFETY: 同上。GDT はこの関数でのみ書き込む。自コアのスロットへ書く。
    unsafe {
        let gdt = PerCpu::this_cpu_ptr(addr_of_mut!(GDT));
        (*gdt)[NULL_INDEX as usize] = 0;
        (*gdt)[KERNEL_CODE_INDEX as usize] =
            user_segment_descriptor(KERNEL_CODE_ACCESS, KERNEL_CODE_FLAGS);
        (*gdt)[KERNEL_DATA_INDEX as usize] =
            user_segment_descriptor(KERNEL_DATA_ACCESS, KERNEL_DATA_FLAGS);
        // ユーザー用（Ring 3、DPL=3）。並びは STAR 互換順（ADR-0020）。M5-e-3 が
        // 使うのは ucode64 と udata で、ucode32 は枠を埋めるためだけに置く。
        (*gdt)[USER_CODE32_INDEX as usize] =
            user_segment_descriptor(USER_CODE_ACCESS, USER_CODE32_FLAGS);
        (*gdt)[USER_DATA_INDEX as usize] =
            user_segment_descriptor(USER_DATA_ACCESS, USER_DATA_FLAGS);
        #[cfg(not(feature = "ring3-test-user-desc-dpl0"))]
        {
            (*gdt)[USER_CODE64_INDEX as usize] =
                user_segment_descriptor(USER_CODE_ACCESS, USER_CODE64_FLAGS);
        }
        // 破壊 (M5-e-4): ucode64 の DPL を 0 にする（KERNEL_CODE_ACCESS）。RPL=3 の
        // セレクタで iretq すると iretq 自身が #GP になり、Ring 3 に落ちない。
        #[cfg(feature = "ring3-test-user-desc-dpl0")]
        {
            (*gdt)[USER_CODE64_INDEX as usize] =
                user_segment_descriptor(KERNEL_CODE_ACCESS, USER_CODE64_FLAGS);
        }
        (*gdt)[TSS_INDEX as usize] = tss_low;
        (*gdt)[TSS_INDEX as usize + 1] = tss_high;
    }

    // SAFETY: 自コアの GDT スロットのアドレスを lgdt へ渡す（起動時の単一文脈）。
    let gdt_slot_base = unsafe { PerCpu::this_cpu_ptr(addr_of_mut!(GDT)) } as u64;
    let pointer = DescriptorTablePointer {
        limit: (GDT_ENTRY_COUNT * core::mem::size_of::<u64>() - 1) as u16,
        base: gdt_slot_base,
    };

    // SAFETY: pointer は今組み立てた有効な GDT を指す。呼び出し側の契約により
    // 割り込みは禁止されている。
    unsafe {
        core::arch::asm!(
            "lgdt [{ptr}]",
            ptr = in(reg) &pointer,
            options(readonly, nostack, preserves_flags),
        );
        reload_segment_registers();
        load_task_register();
    }
}

/// CS とデータセグメントレジスタを自前のディスクリプタへ切り替える。
///
/// `lgdt` はテーブルの場所を教えるだけで、既にロード済みのセグメント
/// レジスタは古いディスクリプタのキャッシュを保持したままになる。CS は
/// `mov` で書き換えられないため、far return（`retfq`）で「新しい CS と
/// 戻り番地」を積んで飛ぶ。
///
/// # Safety
///
/// 有効な GDT がロード済みで、[`KERNEL_CODE_SELECTOR`] と
/// [`KERNEL_DATA_SELECTOR`] がそれぞれ正しいディスクリプタを指していること。
unsafe fn reload_segment_registers() {
    // SAFETY: 呼び出し側の契約どおり GDT はロード済み。retfq は直後のラベルへ
    // 戻るだけで、制御フローはこの関数内に閉じている。
    unsafe {
        core::arch::asm!(
            // retfq は RIP → CS の順に取り出すので、CS を先に積む。
            "push {code}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {data:e}",
            "mov es, {data:e}",
            "mov ss, {data:e}",
            "mov fs, {data:e}",
            "mov gs, {data:e}",
            code = in(reg) KERNEL_CODE_SELECTOR.bits() as u64,
            data = in(reg) KERNEL_DATA_SELECTOR.bits() as u32,
            tmp = lateout(reg) _,
            options(preserves_flags),
        );
    }
}

/// TR に TSS セレクタをロードする。
///
/// # Safety
///
/// 有効な GDT がロード済みで、[`TSS_SELECTOR`] が使用可能な 64bit TSS
/// ディスクリプタを指していること。
unsafe fn load_task_register() {
    // SAFETY: 呼び出し側の契約どおり。
    unsafe {
        core::arch::asm!(
            "ltr {sel:x}",
            sel = in(reg) TSS_SELECTOR.bits(),
            options(nostack, preserves_flags),
        );
    }
}

/// 現在ロードされている GDT の位置と大きさ（`sgdt` の読み戻し）。
pub fn current_gdt() -> (u64, u16) {
    let mut pointer = DescriptorTablePointer { limit: 0, base: 0 };
    // SAFETY: sgdt は GDTR を読むだけで副作用が無い。書き込み先は
    // このスタックフレーム上の有効な領域。
    unsafe {
        core::arch::asm!(
            "sgdt [{ptr}]",
            ptr = in(reg) &mut pointer,
            options(nostack, preserves_flags),
        );
    }
    (pointer.base, pointer.limit)
}

/// GDTR が指す稼働中の GDT から、`index` 番目の 8 バイトディスクリプタを
/// 読み戻す。
///
/// `sgdt` で得た base から読むので、`GDT` 静的変数ではなく CPU が今参照して
/// いる実体を見る（A-1 / M2-d と同じく「設定したつもり」ではなく実状態を
/// 確認する）。TSS のような 16 バイトディスクリプタは、下位・上位を別々の
/// index で読む。
pub fn loaded_descriptor(index: usize) -> u64 {
    let (base, _limit) = current_gdt();
    // SAFETY: base は sgdt が返した稼働中 GDT の先頭。index はテーブル内
    // （呼び出し側が GDT_ENTRY_COUNT 未満で渡す）。読み取りのみ。
    unsafe { core::ptr::read_volatile((base as *const u64).add(index)) }
}

/// 稼働中の GDT に期待される limit（バイト数 - 1）。読み戻しの照合に使う。
pub fn expected_gdt_limit() -> u16 {
    (GDT_ENTRY_COUNT * core::mem::size_of::<u64>() - 1) as u16
}

/// 現在の CS セレクタ。
pub fn current_code_selector() -> u16 {
    let selector: u16;
    // SAFETY: CS の読み取りは副作用が無い。
    unsafe {
        core::arch::asm!("mov {sel:x}, cs", sel = out(reg) selector, options(nomem, nostack, preserves_flags));
    }
    selector
}

/// 現在の DS と SS セレクタ（`(ds, ss)`）。
///
/// ADR-0018 §2 の項目 1 は「CS/DS/SS が自前ディスクリプタ」を要求している。
/// M4-a では CS と TR しか読み戻していなかったため、`sti` 前の検証を完全に
/// するために追加した（M4-d-1）。
///
/// **SS が特に重要である。** 割り込み配送時、CPU は SS:RSP をスタックへ積み、
/// `iretq` はそれを読み戻して復元する。SS が想定と違うディスクリプタを
/// 指していると、復帰の瞬間に #GP になる。`lgdt` の後にデータセグメントの
/// 再ロードを忘れていても、割り込みを有効化するまでは何も起きないため、
/// 症状が出るのは `sti` した後になる。
pub fn current_data_selectors() -> (u16, u16) {
    let data: u16;
    let stack: u16;
    // SAFETY: DS / SS の読み取りは副作用が無い。
    unsafe {
        core::arch::asm!(
            "mov {ds:x}, ds",
            "mov {ss:x}, ss",
            ds = out(reg) data,
            ss = out(reg) stack,
            options(nomem, nostack, preserves_flags)
        );
    }
    (data, stack)
}

/// 現在の TR セレクタ（`str` の読み戻し）。
pub fn current_task_register() -> u16 {
    let selector: u16;
    // SAFETY: TR の読み取りは副作用が無い。
    unsafe {
        core::arch::asm!("str {sel:x}", sel = out(reg) selector, options(nomem, nostack, preserves_flags));
    }
    selector
}

/// 自前の GDT（現在のコアのスロット）の先頭アドレス。読み戻しの照合に使う。
pub fn gdt_base() -> u64 {
    // SAFETY: addr_of_mut! は参照を作らない。自コアのスロットのアドレスを
    // 読み取り目的で取る（`this_cpu_ptr` の契約: `this` は有効な static）。
    unsafe { PerCpu::this_cpu_ptr(addr_of_mut!(GDT)) as u64 }
}

/// 自前の TSS（現在のコアのスロット）の先頭アドレス。
pub fn tss_base() -> u64 {
    // SAFETY: 同上。読み取り目的でアドレスを取る。
    unsafe { PerCpu::this_cpu_ptr(addr_of_mut!(TSS)) as u64 }
}

/// TSS に設定済みのダブルフォルト用スタック上端。読み戻しの照合に使う。
pub fn double_fault_stack_top() -> u64 {
    // SAFETY: 読み取りのみ。init 以降は書き換えない。自コアのスロットを読む。
    unsafe {
        let tss = PerCpu::this_cpu_ptr(addr_of_mut!(TSS));
        (*tss).interrupt_stack_table[DOUBLE_FAULT_IST_INDEX - 1]
    }
}

/// TSS の RSP0 を更新する（M5-c、コンテキストスイッチのたびに呼ぶ）。
///
/// RSP0 は Ring 3 → Ring 0 遷移で CPU が切り替える先のスタックである。タスク
/// ごとにカーネルスタックが分かれる以上、現在のタスクのスタック頂点へ更新
/// しないと、あるタスクのシステムコールが別のタスクのカーネルスタックを使って
/// 静かに壊す（ADR-0019 §2.2）。実際に効くのは Ring 3 を導入する M5-e だが、
/// 切り替え経路には M5-c から配線しておく。
///
/// # Safety
///
/// `top` が現在のタスクの、有効でマップ済みのカーネルスタック上端であること。
/// 起動時の単一実行文脈、またはコンテキストスイッチの割り込み禁止区間から
/// 呼ぶこと。
pub unsafe fn set_rsp0(top: u64) {
    // SAFETY: 呼び出し元契約による。TSS は起動時に構築済みの静的領域で、
    // 書き込むのは自コアのスロットの RSP0（privilege_stack_table[0]）のみ。
    unsafe {
        let tss = PerCpu::this_cpu_ptr(addr_of_mut!(TSS));
        (*tss).privilege_stack_table[0] = top;
    }
}

/// TSS に設定済みの RSP0。
pub fn privilege_stack_top() -> u64 {
    // SAFETY: 読み取りのみ。自コアのスロットを読む。
    unsafe {
        let tss = PerCpu::this_cpu_ptr(addr_of_mut!(TSS));
        (*tss).privilege_stack_table[0]
    }
}
