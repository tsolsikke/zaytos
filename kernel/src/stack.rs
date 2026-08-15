//! カーネル自身のスタック（M4-a）。
//!
//! **unsafe を含む。** 実行中のスタックポインタを差し替える。
//!
//! kernel はこれまで bootloader が使っていた UEFI 由来のスタックを
//! そのまま使い続けていた（`docs/architecture.md` §6.3）。UEFI 由来の領域は
//! `EfiBootServicesData` の回収（ADR-0010）を解禁すれば空きメモリとして
//! 配られうるため、自前のスタックへ移る。
//!
//! スタックはフレームアロケータからではなく **`.bss` の静的配列**として
//! 確保する。理由:
//!
//! - アロケータより前（`_start` の最初期）に切り替えられる。切り替えを
//!   遅らせるほど、旧スタックの上に積んだ状態を持ち越すことになる。
//! - kernel イメージの一部なので、フレームアロケータが最初から予約済みと
//!   して除外しており（`__kernel_start`/`__kernel_end`）、マップ済みである
//!   ことも M2-d の必須領域検証で確認済み。確保の失敗経路が存在しない。
//!
//! ## スタックオーバーフローの検出と被害の局所化
//!
//! 本来はスタックの下端にガードページ（未マップの 4KiB）を置き、溢れた
//! 瞬間にページフォルトさせるのが確実である。しかし現在の
//! `paging::table::PageTableBuilder` は 2MiB ページの分割・アンマップに
//! 対応しておらず、恒等マッピングの途中に穴を開けられない。
//!
//! そのため**犠牲領域（ガード）+ 毒値カナリア**で代替する。溢れた瞬間には
//! 気づけないが、(1) 壊れるのが犠牲領域だけで済み、(2) 「いつの間にか
//! 壊れていた」ことを検出できる。ガードページ化は
//! `docs/deferred-decisions.md` の保留項目とする。
//!
//! ## 配置順は意図的に固定する
//!
//! スタックは下へ伸びるため、**その直下に何を置くかで溢れたときの被害が
//! 決まる**。静的変数の配置をリンカ任せにすると、たまたま重要なものが
//! 直下に来る。実際、最初の実装ではリンカが次の順に並べていた。
//!
//! ```text
//! TSS / BSS_CANARY / BOOT_HANDOFF / ALLOCATOR / KERNEL_STACK / ...
//! ```
//!
//! カーネルスタックが溢れると、まずヒープの `ALLOCATOR`（フリーリストの
//! 先頭）を壊し、次に `TSS` を壊す。TSS が壊れると IST の指す先が失われ、
//! **ダブルフォルトがトリプルフォルト（無言のリセット）になる**。つまり
//! 「オーバーフローを検出するための仕組み」を、オーバーフロー自身が
//! 真っ先に破壊する並びだった。
//!
//! そこで全スタックを 1 つの `#[repr(C)]` 構造体にまとめ、順序を言語仕様で
//! 固定する。各スタックの**直下に犠牲領域を置く**。
//!
//! ```text
//! [kernel_guard][kernel_stack][double_fault_guard][double_fault_stack]
//!       ^ 下へ伸びる先          ^ 下へ伸びる先
//! ```
//!
//! - カーネルスタックが溢れる → `kernel_guard` を壊す（無害、検出可能）
//! - ダブルフォルトスタックが溢れる → `double_fault_guard` を壊す（同上）
//!
//! どちらの犠牲領域もカナリアで埋めてあるため、壊れれば検査で分かる。
//!
//! 犠牲領域（各 [`GUARD_SIZE`]）を食い尽くすほど溢れた場合に何が壊れるかも
//! 把握しておく。
//!
//! - カーネルスタックが犠牲領域を越えると、この構造体の手前に置かれた
//!   静的変数（ヒープの `ALLOCATOR`、`BOOT_HANDOFF`、`TSS` 等）に届く。
//!   ここまで来ると検出も復旧もできない。犠牲領域はそこへ達する前に
//!   異常を記録するための猶予である。
//! - ダブルフォルトスタックが犠牲領域を越えると、カーネルスタックの
//!   **上端**、つまり `kernel_main` の最も古いフレームを壊す。ダブル
//!   フォルトハンドラは戻らずに停止する設計（ADR-0018）なので、実害は
//!   「停止するまでの間だけ」に限られる。ハンドラを小さく保つ限り
//!   16KiB + 4KiB を使い切ることはない。

use common::addr::VirtAddr;

use core::ptr::addr_of;

/// 通常実行用のカーネルスタックの大きさ。
pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

/// IST 用スタックの大きさ。ダブルフォルトハンドラが動く分だけあればよい。
pub const IST_STACK_SIZE: usize = 16 * 1024;

/// 各スタックの直下に置く犠牲領域の大きさ。
///
/// 溢れたときにここが壊れることで、隣接する重要なデータ（ヒープの
/// アロケータ状態、TSS 等）への被害を防ぐ。全域をカナリアで埋める。
pub const GUARD_SIZE: usize = 4096;

/// カナリアのパターン。ヒープの毒値（`0xDE`）とは別の値にして、ログに
/// 出たときにどちらの領域の話か区別できるようにする。
pub const CANARY_BYTE: u8 = 0xC5;

/// スタックと犠牲領域をまとめた塊。
///
/// **フィールドの順序に意味がある。** 各スタックの直下（アドレスが小さい側）に
/// 犠牲領域が来るよう並べてある。`#[repr(C)]` により、この順序は言語仕様で
/// 保証される（リンカやコンパイラの都合で入れ替わらない）。
///
/// **`align(4096)` にしてあるのは、`kernel_guard` を 1 ページとして unmap し、
/// ガードページにするためである（M5-b）。** 先頭がページ境界に載り、各
/// フィールドの大きさがいずれも 4KiB の倍数なので、すべてのフィールドが
/// ページ境界に揃う。`kernel_guard` はちょうど 1 ページになり、
/// `unmap_4kib` で 1 枚だけ落とせる。
#[repr(C, align(4096))]
struct StackBlock {
    /// カーネルスタックのガードページ。**M5-b でこの 1 ページを unmap し、
    /// 溢れた瞬間に #PF（CR2 = このページ）として捕まえる。** それまでは
    /// マップされたまま（unmap は起動シーケンスの中で行う）。カナリアは
    /// 敷かない（ガードページ化がカナリアの役割を引き継ぐ）。
    kernel_guard: [u8; GUARD_SIZE],
    /// 通常実行用のカーネルスタック。
    kernel: [u8; KERNEL_STACK_SIZE],
    /// ダブルフォルトスタックが溢れたときに最初に壊れる犠牲領域。
    double_fault_guard: [u8; GUARD_SIZE],
    /// ダブルフォルト用の IST スタック（M4-b で IDT から参照する）。
    /// 通常のスタックが壊れている状況でも例外ハンドラを動かすためのものなので、
    /// 通常スタックとは必ず別領域にする。
    double_fault: [u8; IST_STACK_SIZE],
    /// ページフォルトスタックが溢れたときに最初に壊れる犠牲領域。
    page_fault_guard: [u8; GUARD_SIZE],
    /// ページフォルト用の IST スタック（IST2、M5-b）。ガードページに触れた
    /// #PF が、溢れた通常スタックの上ではなくこの別スタックで動くようにする。
    /// これがないと #PF がダブルフォルトへ昇格し、CR2 が失われる（ADR-0019
    /// §3.1）。IST スタック自体にはガードページを付けず、カナリアで見る。
    page_fault: [u8; IST_STACK_SIZE],
}

static mut STACKS: StackBlock = StackBlock {
    kernel_guard: [0; GUARD_SIZE],
    kernel: [0; KERNEL_STACK_SIZE],
    double_fault_guard: [0; GUARD_SIZE],
    double_fault: [0; IST_STACK_SIZE],
    page_fault_guard: [0; GUARD_SIZE],
    page_fault: [0; IST_STACK_SIZE],
};

/// スタックの範囲（下端と上端）。上端は排他で、スタックポインタの初期値になる。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StackRange {
    pub bottom: VirtAddr,
    pub top: VirtAddr,
}

impl StackRange {
    /// スタックは下へ伸びるため、`top` が初期スタックポインタになる。
    pub fn contains(&self, address: VirtAddr) -> bool {
        (self.bottom..=self.top).contains(&address)
    }

    pub fn size(&self) -> u64 {
        self.top.as_u64() - self.bottom.as_u64()
    }
}

/// スタックブロックの先頭。
///
/// **`.bss` の静的配列なので、これは仮想アドレスである。** 恒等マッピングの
/// 間は物理アドレスとしても同じ値になるが、スタックは仮想アドレスとしてしか
/// 使わない（RSP に入る値、TSS の RSP0 / IST に入る値）。型でそれを表す。
fn block_base() -> VirtAddr {
    VirtAddr::new(addr_of!(STACKS) as u64).expect("a .bss address is canonical")
}

/// 範囲を組み立てる補助。桁溢れも非正規化も起きない前提を 1 箇所へ集約する。
fn range_from(bottom: VirtAddr, size: u64) -> StackRange {
    StackRange {
        bottom,
        top: bottom
            .checked_add(size)
            .expect("the stack block stays within the canonical range"),
    }
}

/// 通常実行用スタックの範囲。
pub fn kernel_stack_range() -> StackRange {
    let bottom = block_base()
        .checked_add(GUARD_SIZE as u64)
        .expect("the stack block stays within the canonical range");
    range_from(bottom, KERNEL_STACK_SIZE as u64)
}

/// カーネルスタックの直下に置くガードページ（M5-b で unmap する 1 ページ）。
///
/// `StackBlock` が `align(4096)` なので、これはちょうど 1 ページ
/// （`GUARD_SIZE == 4096`）で、ページ境界に載っている。カナリアは敷かず、
/// このページを unmap してガードページにする。
pub fn kernel_guard_page() -> StackRange {
    range_from(block_base(), GUARD_SIZE as u64)
}

/// ダブルフォルト用 IST スタック（IST1）の範囲。
pub fn double_fault_stack_range() -> StackRange {
    let bottom = kernel_stack_range()
        .top
        .checked_add(GUARD_SIZE as u64)
        .expect("the stack block stays within the canonical range");
    range_from(bottom, IST_STACK_SIZE as u64)
}

/// ダブルフォルトスタックの直下にある犠牲領域。
pub fn double_fault_guard_range() -> StackRange {
    range_from(kernel_stack_range().top, GUARD_SIZE as u64)
}

/// ページフォルト用 IST スタック（IST2）の範囲（M5-b）。
pub fn page_fault_stack_range() -> StackRange {
    let bottom = double_fault_stack_range()
        .top
        .checked_add(GUARD_SIZE as u64)
        .expect("the stack block stays within the canonical range");
    range_from(bottom, IST_STACK_SIZE as u64)
}

/// ページフォルトスタックの直下にある犠牲領域。
pub fn page_fault_guard_range() -> StackRange {
    range_from(double_fault_stack_range().top, GUARD_SIZE as u64)
}

/// IST スタックの犠牲領域をカナリアで埋める。
///
/// スタックを使い始める前に呼ぶこと。**カーネルスタックのガードページには
/// カナリアを敷かない**（そのページは M5-b で unmap してガードページにする。
/// カナリアを敷いても unmap で消えるうえ、unmap 後の読み戻しは #PF になる）。
/// カナリアを敷くのは IST1（ダブルフォルト）と IST2（ページフォルト）の
/// 犠牲領域だけである。
///
/// # Safety
///
/// 犠牲領域がまだ誰にも使われていないこと。`_start` の最初期に 1 回だけ
/// 呼ぶ前提。
pub unsafe fn init_guards() {
    for range in [double_fault_guard_range(), page_fault_guard_range()] {
        // SAFETY: range は静的構造体の範囲であり、呼び出し側の契約により
        // まだ誰も使っていない。書き込むのは犠牲領域だけで、スタック本体には
        // 触れない。
        unsafe {
            core::ptr::write_bytes(range.bottom.as_mut_ptr::<u8>(), CANARY_BYTE, GUARD_SIZE);
        }
    }
}

/// IST の犠牲領域がどれも無傷かどうか。破壊されていれば IST スタックが
/// 溢れている。カーネルスタックはガードページで見るため、ここには含めない。
pub fn guards_intact() -> bool {
    double_fault_guard_intact() && page_fault_guard_intact()
}

pub fn double_fault_guard_intact() -> bool {
    guard_intact(double_fault_guard_range())
}

pub fn page_fault_guard_intact() -> bool {
    guard_intact(page_fault_guard_range())
}

fn guard_intact(range: StackRange) -> bool {
    // スタックに近い側（上端）から見る。溢れたときに最初に壊れるのはそちら。
    for offset in (0..GUARD_SIZE).rev() {
        // SAFETY: range.bottom + offset は静的構造体の犠牲領域の範囲内。
        // 読み取りのみ。
        let byte = unsafe { core::ptr::read_volatile(range.bottom.as_ptr::<u8>().add(offset)) };
        if byte != CANARY_BYTE {
            return false;
        }
    }
    true
}

/// スタックポインタを自前のカーネルスタックへ切り替え、`continuation` を
/// 呼ぶ。戻らない。
///
/// 呼び出し前のスタック上に置いた値は、切り替え後は参照できなくなる
/// （メモリとしては残るが、意図的に参照しない）。引き継ぎたい情報は
/// 静的領域に置いてから呼ぶこと。
///
/// # Safety
///
/// - 呼び出し後、旧スタック上のデータを参照しないこと。
/// - `continuation` は戻らないこと。
/// - この関数は起動時に 1 回だけ呼ぶこと。
pub unsafe fn switch_to_kernel_stack_and_run(continuation: extern "sysv64" fn() -> !) -> ! {
    let top = kernel_stack_range().top;
    // SAFETY: top は 16 バイト境界に載った静的配列の終端であり、
    // continuation は戻らない関数。
    unsafe { switch_stack_and_call(top.as_u64(), continuation) }
}

/// `rsp` を差し替えて `continuation` を呼ぶ。
///
/// `jmp` ではなく `call` にしているのは、SysV ABI のスタック境界を守るため。
/// ABI は「呼び出し側が `call` を実行する直前に RSP が 16 バイト境界」で
/// あることを要求する。`call` が戻り番地 8 バイトを積むので、呼ばれた側の
/// 入口では RSP % 16 == 8 になる。`jmp` にすると入口で RSP % 16 == 0 と
/// なり規約から外れる。
///
/// `rbp` を 0 にするのは、フレームポインタの連鎖を旧スタックから断つため。
///
/// # Safety
///
/// `new_rsp` が 16 バイト境界に載った有効なスタック上端であり、
/// `continuation` が戻らないこと。
#[unsafe(naked)]
unsafe extern "sysv64" fn switch_stack_and_call(
    new_rsp: u64,
    continuation: extern "sysv64" fn() -> !,
) -> ! {
    core::arch::naked_asm!(
        "mov rsp, rdi",
        "xor rbp, rbp",
        "call rsi",
        // continuation は戻らない契約。万一戻ってきたら未定義命令で止める。
        "ud2",
    );
}

/// ガードページを 1 枚張る（S12 前の手当て、C の途中で寄せた）。
///
/// **粒度を確かめ、2MiB なら分割し、分割後にもう一度読み直してから unmap する。**
///
/// # なぜ 1 つに寄せたのか。**同じことをする関数が 2 つあり、対処が片方にしか入らなかった**
///
/// **かつてガードページを張る場所は 2 つあった**——カーネルスタック
/// （`kernel/src/main.rs` の `install_kernel_stack_guard_page`）と、
/// ワーカースタック（`kernel/src/task.rs` の `install_worker_guard_page`）である。
///
/// **`docs/deferred-decisions.md` の「ガードページの split 化」は S11-5 で発火し、
/// そのとき分割の分岐が配線された。ところが入ったのはカーネルスタックの側だけだった。**
/// **ワーカーの側は「4KiB でなければ止める」のまま残り、S12 前の手当ての C で
/// 像が育ったときに、そちらが止めた。**
///
/// **根は「分割が無かったこと」ではない。「同じ不変を守る場所が 2 つあり、
/// 対処が片側にだけ入ったこと」である。** 配線して終わりにすると、
/// **3 つ目の場所が生まれた日に同じことが起きる。**
///
/// **したがって寄せた。関数が 1 つなら、直し忘れようがない。**
/// **呼び分けはログの文言だけで、ページテーブルの扱いは 1 本である。**
///
/// # 振る舞いは、寄せる前のカーネルスタック側に揃えた
///
/// **2 つは分割の有無だけでなく、unmap の後の確かめ方も違っていた**——
/// **カーネルスタック側は unmap 後に `translate` で解決不能になったことを見ており、
/// ワーカー側は見ていなかった。** **強い側に揃えてある。**
///
/// # Safety
///
/// 自前のページテーブルへ切り替え済みで、`guard_virt` がスタックの直下の
/// 1 ページであること。以後このページへ正規のアクセスが無いこと。
pub unsafe fn install_guard_page(
    guard_virt: VirtAddr,
    allocator: &mut crate::frame_allocator::FrameAllocator,
    tag: &str,
    what: &str,
    log: &mut dyn FnMut(core::fmt::Arguments),
) {
    use crate::paging::active::{ActivePageTable, PageSize};

    // SAFETY: CR3 は自前のテーブルを指し、その配下は登録窓で読み書きできる。
    let mut table = unsafe { ActivePageTable::current(common::addr::direct_map()) };

    match table.translate(guard_virt) {
        Ok(Some(t)) if t.page_size == PageSize::Size4KiB => {}
        Ok(Some(_)) => {
            // **2MiB ページに載っている。** unmap の前に split する（S11-5）。
            // SAFETY: 稼働中のテーブルで、対象はカーネルの高位写像の中である。
            // split は写像内容を変えず、粒度だけを 4KiB へ落とす。
            match unsafe { table.split_huge_page(guard_virt, allocator) } {
                Ok(outcome) => log(format_args!(
                    "{tag}: {what} {:#x} was on a 2MiB page; split {:#x}..+2MiB into 4KiB via a \
                     new page table at {:#x} (old pde={:#x})",
                    guard_virt.as_u64(),
                    outcome.base_virt.as_u64(),
                    outcome.table_phys.as_u64(),
                    outcome.huge_entry
                )),
                Err(e) => {
                    log(format_args!(
                        "{tag}: {what} {:#x} is on a 2MiB page and the split failed ({e:?}); \
                         halting",
                        guard_virt.as_u64()
                    ));
                    common::cpu::halt_forever();
                }
            }
            // **split の後に、粒度をもう一度読み直す。**
            // **split したことを主張の根拠にしない**——実状態で 4KiB になっている
            // ことを、張った側とは独立に確かめる。
            match table.translate(guard_virt) {
                Ok(Some(t)) if t.page_size == PageSize::Size4KiB => {}
                other => {
                    log(format_args!(
                        "{tag}: {what} {:#x} is still not a 4KiB mapping after the split \
                         ({other:?}); halting",
                        guard_virt.as_u64()
                    ));
                    common::cpu::halt_forever();
                }
            }
        }
        other => {
            log(format_args!(
                "{tag}: {what} {:#x} does not resolve ({other:?}); halting",
                guard_virt.as_u64()
            ));
            common::cpu::halt_forever();
        }
    }

    // ガードページを 1 枚 unmap する。unmap_4kib は内部で invlpg も行うので、以後この
    // ページへのアクセスは即座に #PF になる。フレームは解放しない（.bss の一部で
    // アロケータの管理外。M5-a-2 の仕様どおり unmap はフレームを返さない）。
    // SAFETY: guard_virt はスタックの直下のガードページで、スタック本体とは別の
    // 1 ページ。今後このページへ正規のアクセスは無く、触れたら溢れとして #PF で
    // 捕まえるのが目的である。
    match unsafe { table.unmap_4kib(guard_virt) } {
        Ok(old_pte) => {
            // 会計: unmap 後にこのページが解決不能になっていること（ガードが効いて
            // いること）を、構築とは別に translate で確かめる。
            let unmapped = matches!(table.translate(guard_virt), Ok(None));
            log(format_args!(
                "{tag}: unmapped {what} {:#x} (old pte={old_pte:#x}); translate returns \
                 none={unmapped}. #PF now uses IST2.",
                guard_virt.as_u64()
            ));
            if !unmapped {
                log(format_args!(
                    "{tag}: {what} is still resolvable after unmap; halting"
                ));
                common::cpu::halt_forever();
            }
        }
        Err(e) => {
            log(format_args!(
                "{tag}: failed to unmap {what} {:#x}: {e:?}; halting",
                guard_virt.as_u64()
            ));
            common::cpu::halt_forever();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト内で期待値の仮想アドレスを作る補助。
    fn v(raw: u64) -> VirtAddr {
        VirtAddr::new(raw).unwrap()
    }

    #[test]
    fn a_range_contains_its_own_bounds() {
        let range = StackRange {
            bottom: v(0x1000),
            top: v(0x2000),
        };
        assert!(range.contains(v(0x1000)));
        assert!(range.contains(v(0x1800)));
        // top はスタックポインタの初期値そのものなので、含まれる扱いにする。
        assert!(range.contains(v(0x2000)));
        assert!(!range.contains(v(0x0FFF)));
        assert!(!range.contains(v(0x2001)));
        assert_eq!(range.size(), 0x1000);
    }

    #[test]
    fn the_guards_are_a_multiple_of_the_alignment() {
        assert_eq!(GUARD_SIZE % 16, 0);
    }

    /// カナリアの値がヒープの毒値と重ならないこと。ログに出たときに
    /// どちらの領域の話か区別できるようにするため。
    #[test]
    fn the_canary_differs_from_the_heap_poison() {
        assert_ne!(CANARY_BYTE, 0xDE);
    }

    #[test]
    fn the_stacks_are_a_multiple_of_the_required_alignment() {
        assert_eq!(KERNEL_STACK_SIZE % 16, 0);
        assert_eq!(IST_STACK_SIZE % 16, 0);
    }

    /// 犠牲領域が各スタックの**直下**に来ていること。順序が入れ替わると、
    /// 溢れたときに守りたいものを直接壊す。`#[repr(C)]` が保証している
    /// はずだが、フィールドを並べ替えたときに気づけるよう固定する。
    #[test]
    fn each_stack_sits_directly_above_its_guard() {
        use core::mem::offset_of;

        let kernel_guard = offset_of!(StackBlock, kernel_guard);
        let kernel = offset_of!(StackBlock, kernel);
        let df_guard = offset_of!(StackBlock, double_fault_guard);
        let df = offset_of!(StackBlock, double_fault);
        let pf_guard = offset_of!(StackBlock, page_fault_guard);
        let pf = offset_of!(StackBlock, page_fault);

        assert_eq!(
            kernel_guard + GUARD_SIZE,
            kernel,
            "カーネルスタックの直下は犠牲領域（ガードページ）でなければならない"
        );
        assert_eq!(
            df_guard + GUARD_SIZE,
            df,
            "ダブルフォルトスタックの直下は犠牲領域でなければならない"
        );
        assert_eq!(
            pf_guard + GUARD_SIZE,
            pf,
            "ページフォルトスタックの直下は犠牲領域でなければならない"
        );
        // カーネルスタックの上端は次の犠牲領域。重要なデータを挟まない。
        assert_eq!(kernel + KERNEL_STACK_SIZE, df_guard);
        // ダブルフォルトスタックの上端はページフォルト側の犠牲領域。
        assert_eq!(df + IST_STACK_SIZE, pf_guard);
    }

    /// ガードページがちょうど 1 ページで、ページ境界に載っていること。
    /// unmap で 1 枚だけ落とすための前提。
    #[test]
    fn the_kernel_guard_is_exactly_one_aligned_page() {
        use core::mem::{align_of, offset_of};

        assert_eq!(GUARD_SIZE, 4096, "ガードページはちょうど 1 ページ");
        assert!(
            align_of::<StackBlock>() >= 4096,
            "ブロックの先頭がページ境界に載っていること"
        );
        assert_eq!(
            offset_of!(StackBlock, kernel_guard) % 4096,
            0,
            "ガードページがページ境界に載っていること"
        );
    }

    #[test]
    fn the_block_is_exactly_the_sum_of_its_parts() {
        // パディングが入っていないこと。入っていると、下のオフセット計算に
        // 現れない隙間ができる。すべてのフィールドが 4KiB の倍数なので、
        // align(4096) でもパディングは入らない。
        assert_eq!(
            core::mem::size_of::<StackBlock>(),
            GUARD_SIZE * 3 + KERNEL_STACK_SIZE + IST_STACK_SIZE * 2
        );
    }
}
