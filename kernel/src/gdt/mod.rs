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

use core::ptr::addr_of;

use layout::{
    tss_descriptor, user_segment_descriptor, SegmentSelector, TaskStateSegment, KERNEL_CODE_ACCESS,
    KERNEL_CODE_FLAGS, KERNEL_DATA_ACCESS, KERNEL_DATA_FLAGS,
};

/// GDT のエントリ数。null / コード / データ / TSS（16 バイト = 2 スロット）。
const GDT_ENTRY_COUNT: usize = 5;

const NULL_INDEX: u16 = 0;
const KERNEL_CODE_INDEX: u16 = 1;
const KERNEL_DATA_INDEX: u16 = 2;
/// TSS は 16 バイトなので、ここから 2 スロットを占める。
const TSS_INDEX: u16 = 3;

/// カーネルコードセグメントのセレクタ。M4-b の IDT エントリが参照する。
pub const KERNEL_CODE_SELECTOR: SegmentSelector = SegmentSelector::new(KERNEL_CODE_INDEX, 0);
/// カーネルデータセグメントのセレクタ。
pub const KERNEL_DATA_SELECTOR: SegmentSelector = SegmentSelector::new(KERNEL_DATA_INDEX, 0);
/// TSS のセレクタ。`ltr` に渡す。
pub const TSS_SELECTOR: SegmentSelector = SegmentSelector::new(TSS_INDEX, 0);

/// ダブルフォルトに割り当てる IST の番号（1 始まり）。
/// M4-b の IDT エントリでこの番号を指定する。
pub const DOUBLE_FAULT_IST_INDEX: usize = 1;

/// ページフォルトに割り当てる IST の番号（1 始まり、M5-b）。
/// ガードページに触れた #PF が、溢れた通常スタックの上ではなく専用スタックで
/// 動くようにするため、IDT の #PF ゲートでこの番号を指定する（ADR-0019 §3.1）。
pub const PAGE_FAULT_IST_INDEX: usize = 2;

static mut GDT: [u64; GDT_ENTRY_COUNT] = [0; GDT_ENTRY_COUNT];
static mut TSS: TaskStateSegment = TaskStateSegment::new();

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
    unsafe {
        let tss = addr_of!(TSS) as *mut TaskStateSegment;
        (*tss).interrupt_stack_table[DOUBLE_FAULT_IST_INDEX - 1] = double_fault_stack_top;
        (*tss).interrupt_stack_table[PAGE_FAULT_IST_INDEX - 1] = page_fault_stack_top;
        // RSP0 は特権レベルが下がる遷移（ユーザー → カーネル）で使われる。
        // ユーザーモードを導入する M5 以降まで実際には効かないが、
        // 0 のままにしておくと、その時点で気づきにくい形で壊れる。
        // 現時点では通常のカーネルスタックと同じ場所を指しておく。
        (*tss).privilege_stack_table[0] = crate::stack::kernel_stack_range().top.as_u64();
    }

    let tss_base = addr_of!(TSS) as u64;
    let tss_limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u32;
    let (tss_low, tss_high) = tss_descriptor(tss_base, tss_limit);

    // SAFETY: 同上。GDT はこの関数でのみ書き込む。
    unsafe {
        let gdt = addr_of!(GDT) as *mut [u64; GDT_ENTRY_COUNT];
        (*gdt)[NULL_INDEX as usize] = 0;
        (*gdt)[KERNEL_CODE_INDEX as usize] =
            user_segment_descriptor(KERNEL_CODE_ACCESS, KERNEL_CODE_FLAGS);
        (*gdt)[KERNEL_DATA_INDEX as usize] =
            user_segment_descriptor(KERNEL_DATA_ACCESS, KERNEL_DATA_FLAGS);
        (*gdt)[TSS_INDEX as usize] = tss_low;
        (*gdt)[TSS_INDEX as usize + 1] = tss_high;
    }

    let pointer = DescriptorTablePointer {
        limit: (GDT_ENTRY_COUNT * core::mem::size_of::<u64>() - 1) as u16,
        base: addr_of!(GDT) as u64,
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

/// 自前の GDT の先頭アドレス。読み戻しの照合に使う。
pub fn gdt_base() -> u64 {
    addr_of!(GDT) as u64
}

/// 自前の TSS の先頭アドレス。
pub fn tss_base() -> u64 {
    addr_of!(TSS) as u64
}

/// TSS に設定済みのダブルフォルト用スタック上端。読み戻しの照合に使う。
pub fn double_fault_stack_top() -> u64 {
    // SAFETY: 読み取りのみ。init 以降は書き換えない。
    unsafe {
        let tss = addr_of!(TSS);
        (*tss).interrupt_stack_table[DOUBLE_FAULT_IST_INDEX - 1]
    }
}

/// TSS に設定済みの RSP0。
pub fn privilege_stack_top() -> u64 {
    // SAFETY: 読み取りのみ。
    unsafe {
        let tss = addr_of!(TSS);
        (*tss).privilege_stack_table[0]
    }
}
