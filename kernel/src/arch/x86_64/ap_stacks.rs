//! AP の CPU ごとのスタック（`PML4[258]` の置き場と、IST を含む並び）。
//!
//! **`kernel/src/smp.rs` から移した**（2026-09-28。境界の段階の手順 2）。**置き場はページテーブルの段の番号で決まり、
//! 並びは CPU 固有のスタックの大きさ（[`super::stack`]）で決まるので、CPU 固有の置き場に置く。** **引き継ぎ表と、
//! AP 用のアイドルタスクへ範囲を渡す所（`smp` の `ap_kernel_stack_range`）は `smp` に残る。**

use common::addr::VirtAddr;
use common::log::Logger;
use common::machine::pc::serial::Serial;

use crate::arch::x86_64::ap_bring_up::ApStacks;
use crate::arch::x86_64::paging::active::{ActivePageTable, PageAttributes};
use crate::frame_allocator::FrameAllocator;

#[cfg(test)]
mod tests {
    use super::*;

    /// `map_ap_stacks` が実際にマップする並びから、通常スタックの頂点を導く。
    ///
    /// 本番のコードではなくテスト側に置いてある。production 側に同じ式を
    /// 2 本持つと片方だけが古くなるので、照合する側にだけ独立に書く。
    /// 並び = ガード + kernel + ガード + IST1 + ガード + IST2（`AP_STACK_STRIDE`）。
    fn kernel_top_from_the_layout(slot: usize) -> u64 {
        AP_STACK_REGION_BASE
            + (slot as u64) * AP_STACK_STRIDE
            + crate::arch::x86_64::stack::GUARD_SIZE as u64
            + crate::arch::x86_64::stack::KERNEL_STACK_SIZE as u64
    }

    /// AP 用アイドルタスクへ記述する範囲が、実際にマップした通常スタックと一致する
    /// （S4-c-3-2a）。
    ///
    /// # なぜホストテストで守るのか
    ///
    /// `schedule_switch` の範囲検査は切り替えが起きたときにしか走らない。
    /// AP 用アイドルタスクでは切り替えが起きないので、この記述が嘘でも実行時
    /// には誰も気づかない（気づくのは将来ここで切り替えが起きたときで、
    /// そのとき初めて落ちる）。実行時に照合されない記述を守れるのは、ここだけ
    /// である。
    #[test]
    fn the_recorded_ap_kernel_stack_range_matches_the_mapped_layout() {
        let slot = 1usize;
        let top = kernel_top_from_the_layout(slot);
        let (bottom, recorded_top) = kernel_stack_bounds_from_top(top);

        assert_eq!(recorded_top, top);
        // 幅はちょうど通常スタック 1 本ぶんで、IST を含んでいない。
        assert_eq!(
            recorded_top - bottom,
            crate::arch::x86_64::stack::KERNEL_STACK_SIZE as u64
        );

        // 下端はガードの穴より上にある。ガードはマップしない穴なので、
        // 範囲がそこへ食い込むと「ガードの上で走ってよい」と記述したことになる。
        let slot_base = AP_STACK_REGION_BASE + (slot as u64) * AP_STACK_STRIDE;
        assert_eq!(
            bottom,
            slot_base + crate::arch::x86_64::stack::GUARD_SIZE as u64
        );

        // IST1 の下端より下にある（範囲が IST へ食い込んでいない）。
        let ist1_bottom = recorded_top + crate::arch::x86_64::stack::GUARD_SIZE as u64;
        assert!(recorded_top <= ist1_bottom);

        // 次のスロットの領域へはみ出していない。
        assert!(recorded_top <= AP_STACK_REGION_BASE + ((slot + 1) as u64) * AP_STACK_STRIDE);

        // 起動ログで実測した値に釘付けする（S4-c-3-2a、`-smp 2`、スロット 1）。
        // 算術が合っていても定数がずれれば動くので、実測値を 1 点持っておく。
        //
        // **P-c-1 でカーネルスタックを 64KiB から 128KiB へ広げたので、
        // 測り直した**（2026-08-28。起動ログの `smp: mapped per-CPU stacks` の行）。
        // **釘は測って打つものなので、算術で導かない。**
        assert_eq!(bottom, 0xffff_8100_0002_c000);
        assert_eq!(recorded_top, 0xffff_8100_0004_c000);
    }
}

// ===========================================================================
// S3-b-2b-2: AP の per-CPU スタックを PML4[258] へマップする
// ===========================================================================

/// AP の per-CPU スタックを置く仮想アドレス空間の先頭（`PML4[258]`）。
///
/// # なぜ `PML4[257]` ではないのか
///
/// `[257..510]` は SMP の per-CPU 用に温存してきた範囲で、ここがその目的どおりの
/// 初使用である。しかし `PML4[257]`（`0xffff808000000000`）は使えない。
/// あれは破壊テストの feature `highhalf-remove-verify-fail` のサボタージュ VA そのもので、
/// あの破壊テストは「そこが空であること」に依存している。使うと破壊テストが静かに意味を失う
/// （`docs/verification-coverage.md` と `docs/deferred-decisions.md` の 2 箇所に
/// 警告がある）。サボタージュ VA を移さずに済むほうを選んだ。
///
/// # なぜ静的配列にしないのか
///
/// `StackBlock` は 108.0 KiB で、静的に二重化すると `MAX_CPUS = 4` で 2MiB 境界を
/// 越える（`common/src/percpu.rs` の `MAX_CPUS` の doc）。フレームアロケータから
/// 取ってマップすればイメージが増えない。
const AP_STACK_REGION_BASE: u64 = 0xffff_8100_0000_0000;

/// 1 コアぶんのスタック領域の大きさ。BSP の `StackBlock` と同じ構成にする。
///
/// ガード（4KiB）+ kernel（64KiB）+ ガード + IST1（16KiB）+ ガード + IST2（16KiB）。
/// ガードは各スタックの下に置く（スタックは下へ伸びるので、溢れると下のガードに
/// 当たる）。BSP の `StackBlock` と同じ並びである。
const AP_STACK_STRIDE: u64 = (crate::arch::x86_64::stack::GUARD_SIZE
    + crate::arch::x86_64::stack::KERNEL_STACK_SIZE
    + crate::arch::x86_64::stack::GUARD_SIZE
    + crate::arch::x86_64::stack::IST_STACK_SIZE
    + crate::arch::x86_64::stack::GUARD_SIZE
    + crate::arch::x86_64::stack::IST_STACK_SIZE) as u64;

/// AP 用スタックをマップする（S3-b-2b-2）。
///
/// # ガードページはマップせずに「開けておく」
///
/// 3 本のスタックの下に 1 ページずつ、マップしない穴を残す。BSP 側は静的配置の
/// 上で `unmap_4kib` して穴を開けているが、こちらは最初からマップしないので
/// 分割も解除も要らない。direct map（2MiB ページ）に手を入れずに済むのが、
/// この置き方を選んだ理由の 1 つである。
///
/// # 契約（境界の関数。2026-09-28）
///
/// - `slot` は AP のスロットの番号（1 から `MAX_CPUS - 1`）である。フレームは `allocator` から取り、結果は `logger` へ出す。
/// - 返す 3 つの頂点は、本番のページテーブルにだけある**仮想アドレス**（`PML4[258]` の中）である。各スタックの下の
///   1 ページは、マップしない穴（ガード）のまま残す。フレームかマップが足りなければ `None` を返す（それまでにマップした
///   ページは外さない）。
/// - 呼んでよいのは、起動の単一の文脈（AP はまだ走っていない）で、本番のページテーブルが CR3 に載った後である。
///   テーブルは direct map を通して書く。BKL は要らない（ほかに走っている CPU が無い）。
/// - 書くのは全 CPU が共有する本番のページテーブルだが、書く先はまだ誰も使っていない仮想アドレスである。ほかの CPU
///   との同期（TLB を消すこと）は含まない。
///
/// # Safety
///
/// 起動時の単一文脈から、AP を起動する前に呼ぶこと。
pub unsafe fn map_ap_stacks<const CAP: usize>(
    logger: &mut Logger<Serial>,
    slot: usize,
    allocator: &mut FrameAllocator<CAP>,
) -> Option<ApStacks> {
    let base = AP_STACK_REGION_BASE + (slot as u64) * AP_STACK_STRIDE;
    // SAFETY: CR3 は本番テーブルを指しており、その配下は direct map ウィンドウから
    // 読み書きできる。起動時の単一文脈で、AP はまだ走っていない。
    let mut table = unsafe { ActivePageTable::current(common::addr::direct_map()) };

    // (ガードのページ数, 本体のバイト数) を下から順に。
    let layout = [
        crate::arch::x86_64::stack::KERNEL_STACK_SIZE as u64,
        crate::arch::x86_64::stack::IST_STACK_SIZE as u64,
        crate::arch::x86_64::stack::IST_STACK_SIZE as u64,
    ];

    let mut cursor = base;
    let mut tops = [0u64; 3];
    let free_before = allocator.free_frame_count();
    // **この CPU の分の範囲を、ページの権限の一覧（`crate::page_survey`）に登録する。** ページ数は CPU の数で
    // 変わるので、要約の値には入れない。
    crate::page_survey::register(
        "application processor stacks",
        base,
        base + AP_STACK_STRIDE,
        false,
    );
    for (index, size) in layout.iter().enumerate() {
        // ガードぶんを空けたまま進める（マップしないので穴になる）。**穴は、何も写っていてはならない領域として
        // 登録する。**
        crate::page_survey::register_absent(
            crate::arch::x86_64::stack::GUARD_PAGES_REGION,
            cursor,
            cursor + crate::arch::x86_64::stack::GUARD_SIZE as u64,
        );
        cursor += crate::arch::x86_64::stack::GUARD_SIZE as u64;
        let bottom = cursor;
        let mut offset = 0;
        while offset < *size {
            let Some(frame) = allocator.allocate_frame() else {
                logger.error(format_args!(
                    "smp: ran out of frames while mapping the per-CPU stacks for slot {slot}"
                ));
                return None;
            };
            let virt = VirtAddr::new(bottom + offset)?;
            // user=false, writable=true, cacheable=true（通常のカーネルメモリ）。
            //
            // 破壊テスト (2026-10-01, ap-stacks-uncached-test): キャッシュ無効で写す。**起動は通る**（遅くなるだけ）。
            // ページの権限の一覧の道具が、この領域のキャッシュの属性の違いを名前つきで示す。
            let attributes = PageAttributes {
                user: false,
                writable: true,
                cacheable: !cfg!(feature = "ap-stacks-uncached-test"),
                shared: false,
            };
            // SAFETY: 稼働中のテーブルへ、まだ誰も使っていない VA をマップする。
            if let Err(error) = unsafe { table.map_4kib(virt, frame, attributes, allocator) } {
                logger.error(format_args!(
                    "smp: could not map the per-CPU stack page at {:#x} for slot {slot}: \
                     {error:?}",
                    virt.as_u64()
                ));
                return None;
            }
            offset += crate::frame_allocator::FRAME_SIZE;
        }
        cursor = bottom + *size;
        tops[index] = cursor;
    }
    let free_after = allocator.free_frame_count();

    logger.info(format_args!(
        "smp: mapped per-CPU stacks for slot {slot} at {base:#x} (PML4[258], not [257] which a \
         sabotage VA depends on): kernel top {:#x}, IST1 top {:#x}, IST2 top {:#x}; \
         {} frame(s) consumed (pages plus page tables), guards left unmapped",
        tops[0],
        tops[1],
        tops[2],
        free_before - free_after
    ));

    Some(ApStacks {
        kernel_top: tops[0],
        double_fault_top: tops[1],
        page_fault_top: tops[2],
    })
}

/// 通常カーネルスタックの頂点から `[下端, 頂点)` を導く（S4-c-3-2a）。
///
/// 純粋な算術として切り出してある。この範囲は
/// `schedule_switch` の範囲検査が使うが、AP 用アイドルタスクでは
/// 切り替えが起きないので実行時には照合されない（`task::init_ap_idle_task`）。
/// 実行時に照合されない記述なので、誤りを捕まえられるのはホストテストだけである。
///
/// # 契約（境界の関数。2026-09-28）
///
/// - `kernel_top` は通常スタックの頂点の**仮想アドレス**で、[`super::stack::KERNEL_STACK_SIZE`] 以上であること。
///   返すのは `[下端, 頂点)` の仮想アドレスの組で、IST とガードは含まない。
/// - 算術だけで、何も読み書きしない。いつ、どの CPU から呼んでもよい。
pub const fn kernel_stack_bounds_from_top(kernel_top: u64) -> (u64, u64) {
    (
        kernel_top - crate::arch::x86_64::stack::KERNEL_STACK_SIZE as u64,
        kernel_top,
    )
}
