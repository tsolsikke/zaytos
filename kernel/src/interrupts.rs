//! メインループ（M4-d-1）。
//!
//! **`sti` の前の 7 項目の確かめ（ADR-0018 §2）は、[`crate::arch::x86_64::interrupt_readiness`] へ移した**
//! （2026-09-27。境界の段階の手順 2）。**全 IRQ をマスクしたまま `sti` して期限つきで待つ確かめ（M4-d-1）も、
//! 同じ所へ移した。**
//!
//! **外からの割り込みと yield の入口関数（`ADR-0072` の 1 の C）も、ここに置く**（2026-09-28。境界の段階の
//! 手順 2 の 9d-2）。**`arch` の入口は、入口の確かめとベクタごとの数えを済ませて、ここを呼ぶ。**
//!
//! **外からの割り込みの処理の表（`ADR-0072` の 5）も、ここに置く**（2026-09-28。9d-4）。装置のドライバが起動の
//! 間に処理を登録し、起動の終わりに登録を閉じる。

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};

use common::log::Logger;
use common::machine::pc::Serial;

use crate::arch::x86_64::ExitAction;

/// 探りの各段で待つスピン上限（S5-c）。上限の無い待ちを書かない。
#[cfg(feature = "smp-tlb-shootdown-probe")]
const SHOOTDOWN_PROBE_WAIT_SPINS: u32 = 3_000_000;

/// 測定用 IPI を何本送るか（S5-a）。会計を主張できる程度の本数にする。
#[cfg(feature = "smp-ipi-probe")]
const IPI_PROBE_ROUNDS: u32 = 4;

/// 1 本ぶんの受け取りを待つスピン上限（S5-a）。上限の無い待ちを書かない。
#[cfg(feature = "smp-ipi-probe")]
const IPI_PROBE_WAIT_SPINS: u32 = 10_000_000;

/// ハートビートを出す間隔（ティック数）。
///
/// 100Hz なので 100 ティック = 約 1 秒。画面で目視して変化が分かる
/// 間隔にしてある。これより短いと画面のスクロールが速すぎて読めず、
/// 長いと「動いているのか止まっているのか」の判断が遅れる。
pub const HEARTBEAT_TICKS: u64 = 100;

/// 定常ループの観測を完了したか（S11-11）。**両方のコアが見る。**
///
/// # なぜ AP も見るのか
///
/// **BSP がシェルへ渡しても、AP は自分のループを回し続ける。**
/// **AP のハートビートが出続けると、起動ログの長さが実時間に依存する**
/// ——**捕捉を打ち切った時点で何本出ているかが決まらない。**
/// **`-smp 1` と `-smp 2` の突き合わせが、その差で落ちた**（実測）。
///
/// **観測の終わりは系全体の性質である。** 片方のコアだけ完了しても、
/// **ログとしては完了していない。**
static STEADY_OBSERVATION_CLOSED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// 定常ループの観測を完了する（S11-11）。**BSP がシェルへ渡す直前に呼ぶ。**
pub fn close_steady_observation() {
    STEADY_OBSERVATION_CLOSED.store(true, core::sync::atomic::Ordering::SeqCst);
}

/// 定常ループの観測が完了しているか。**AP のハートビートが見る。**
pub fn steady_observation_is_closed() -> bool {
    STEADY_OBSERVATION_CLOSED.load(core::sync::atomic::Ordering::SeqCst)
}

/// 最初のティックを待つ上限（TSC サイクル）。
///
/// これを過ぎても 1 件も来ないなら、タイマが設定できていないか、IMR が
/// 効いていないか、ICW2 が誤っているかのいずれかである。無言で待ち続けると
/// ハングと区別がつかないので fail-fast する。
const FIRST_TICK_TIMEOUT_CYCLES: u64 = 20_000_000_000;

/// メインループが 1 回起きるあいだに進んだティック数の最大値。
///
/// これはハードウェアのティック取りこぼしではなく、メインループが
/// 「観測し損ねた」量である。1 なら毎ティック起きて観測できている。
/// 2 以上なら、起きて処理しているあいだに次のティックが来ている。
///
/// 本当の意味でのティック取りこぼし（PIT が発火したのに CPU へ届かない、
/// あるいは EOI が間に合わず次が抑止される）は、独立した第 2 の時間源が
/// 無いと検出できない。現状 TSC しか無く、その TSC も仮想化環境では
/// 信用できない（`common::arch::x86_64::cpu` 参照）。ここで測れるのは
/// 「メインループの追従の遅れ」までである。
static MAX_TICK_JUMP: AtomicU64 = AtomicU64::new(0);

pub fn max_tick_jump() -> u64 {
    MAX_TICK_JUMP.load(Ordering::Relaxed)
}

/// 外からの割り込みの入口関数（`ADR-0072` の 1。2026-09-28。境界の段階の手順 2 の 9d-2）。
///
/// `arch` の入口（`idt::irq_entry`）から呼ばれる。受け取り（`machine`）から完了（`machine`）までの流れと、
/// 方針（BKL・ティックと単調な時刻・装置の処理・切り替え）をここが持つ。
///
/// 出力しない。ADR-0018 §5 のとおり、ここでやるのは共有状態の更新だけである。100Hz で毎回ログを出すと
/// 出力自体がハンドラの処理時間を支配し、ティックを取りこぼす。観測はメインループがカウンタ越しに行う。
///
/// # 契約（境界の関数。2026-09-28）
///
/// - 呼ぶのは `arch` の割り込みの入口だけで、割り込みを止めたまま、入口の確かめ（方向フラグ・スタックの境界・
///   ベクタごとの数え）を済ませた後に呼ぶ。yield はここへ来ない（[`on_yield_interrupt`]）。
/// - `arrival` は、入口のスタブが積んだ到着（x86 ではベクタ）から `arch` が作ったものである。ここでは読まずに、
///   `machine` の受け取る（[`crate::machine::pc::claim`]）へ渡すだけである（`ADR-0072` の 3。9e。ベクタを共通の側に
///   出さない）。登録した処理へは源の番号を渡す。
/// - 戻り値は出口の動きである（9d-3）。ふつうは入口が戻るときに使うスタックポインタ（切り替えないなら割り込まれた
///   文脈のもの、切り替えるなら次のタスクのもの）を返し、遠征を畳むと決めたら [`ExitAction::FoldExcursion`] を
///   返す。畳むのは入口が、この関数が戻った後に行う。
/// - BKL を取らない種類（IPI の探り）は、受け取った直後に分けて、BKL を取らずに完了させる。それ以外は BKL の中で
///   扱い、BKL は戻るときに解く（遠征を畳むときも同じ）。
pub fn on_external_interrupt(
    arrival: crate::machine::pc::Arrival,
    interrupted: &crate::arch::x86_64::Interrupted<'_>,
) -> ExitAction {
    use crate::machine::pc::Claim;

    let current_sp = interrupted.stack_pointer();
    // 届いたものを受け取る（`ADR-0072` の 2）。**BKL なしで呼べる**ので、BKL を取らない種類（IPI の探り）を
    // BKL の前に分けられる。
    let claimed = crate::machine::pc::claim(arrival);

    // 測定用 IPI（S5-a）は BKL を取る前に処理して戻る。
    //
    // BKL 待ちと IPI の相性は未解決である（`deferred-decisions.md` の
    // 「BKL取得待ちのIF=0とIPIのデッドロック」）。ここで BKL を取ると、
    // 測るためだけのベクタでその罠を踏むことになる。
    // 触るのは自コアのカウンタと自コアの Local APIC だけなので、BKL は要らない。
    // 受け取った本数は、受け取る側（`claim`）が数えてある。
    if claimed == Claim::IpiProbe {
        // SAFETY: 実際に配送された割り込みに対してのみ、自コアの LAPIC へ送る。
        unsafe { crate::machine::pc::complete(claimed) };
        return ExitAction::Resume(current_sp);
    }

    // BKL のガードは、この関数から戻るときに解く。遠征を畳むときも、畳むのは入口がこの関数から戻った後なので、
    // ガードはふつうに解かれる（9d-3。それまでは、畳む関数が longjmp の前に自分で解いていた）。
    let _bkl = acquire_bkl_for_interrupt();

    match claimed {
        // Local APIC のスプリアス割り込み（S2-d-1）。EOI を送らずに戻る。
        //
        // 判定を明示にした。以前このベクタに EOI が送られなかったのは
        // 「PIC の担当範囲の外だから」であって、スプリアスだからではなかった。
        // S2-d で Local APIC が配送を担うと LAPIC 由来のベクタには EOI が要るので、
        // その偶然の一致は壊れる。ここで問いの形にしておく。
        //
        // 回数は PIC のスプリアス（IRQ7 / IRQ15）とは別に、受け取る側（`claim`）が数える。機序が違い、
        // 合流させるとどちらが起きたのかハートビートから分からなくなる。
        Claim::Spurious => return ExitAction::Resume(current_sp),
        // Local APIC タイマ（S2-d-2）。LVT 由来なので IRQ 番号を持たない。
        //
        // 判定の順序（LVT 由来を IRQ の表より先に見る）は、受け取る側（`claim`）が固定している。
        //
        // EOI は Local APIC へ送る。8259 は関与しない。
        Claim::LocalTimer => {
            crate::arch::x86_64::count_timer_tick();
            // **単調なティック（W2-d+）。** **較正の後はこちらが数える。**
            advance_clock();
            // SAFETY: 割り込みハンドラの中であり、割り込みゲート経由なので IF=0。
            // 実際に配送された割り込みに対してのみ呼んでいる。
            //
            // EOI は自コアの Local APIC へ届く。送り先の VA は 1 つだが、
            // その物理アドレスは実行しているコア自身の LAPIC に別名づけられている。
            // 共有 IDT で両コアが同じハンドラに入っても、EOI の宛先は分かれる。
            // EOI を省く破壊テスト（`no-eoi-test`）は、完了させる側（`complete`）の中にある。
            unsafe { crate::machine::pc::complete(claimed) };
            // **中断（Ctrl+C）で遠征を終了させる地点はここである（S12 前の手当て、C）。**
            //
            // **EOI を送った後でなければならない。** 畳むのは longjmp で出ていく形なので、
            // **EOI より前に置くと、割り込みを終えないまま抜ける。**
            // **IRQ1 の側に置けないのはこれが理由である**——あちらの EOI は
            // ハンドラより後ろにあり、そこから抜けるとキーボードが二度と来ない。
            //
            //
            // 畳むと決めたら、出口の動きとして返す。longjmp は入口が、この関数から戻った後に行う（9d-3）。
            if should_fold_excursion(interrupted) {
                // 破壊テスト (S12 前の手当て C, kill-fold-keep-bkl): 解かずに終了させる。次に BKL を
                // 取る者が、同じコアの再取得として検出する。**`user-exit-keep-bkl` と
                // 同じ機序で、入口が `Syscall` ではなく `Irq` である点だけが違う。**
                #[cfg(feature = "kill-fold-keep-bkl-test")]
                core::mem::forget(_bkl);
                return ExitAction::FoldExcursion;
            }
            // AP もスケジューラへ入る（S4-c-3-2b）。
            //
            // S4-a から S4-c-3-2a までは、ここで AP を手前へ返していた。当時の AP は
            // タスクを実行せず、入れば `CURRENT` の sentinel を読んで停止したためで
            // ある。S4-c-3-2b で AP に担当タスク（AP 用アイドルタスク）ができ、
            // 起動時に sentinel を解くようになったので、その分岐は不要になった。
            //
            // 破壊テスト `smp-ap-enter-scheduler` はここで引退した。分岐そのものが
            // 無くなったので「分岐を外す」破壊テストは構成できない。役目
            // （sentinel が止めることの実証）は `smp-ap-no-sentinel-clear` が
            // 引き継いでいる（あちらは分岐ではなく sentinel の解除を落とす）。
            return ExitAction::Resume(crate::task::on_timer_tick(current_sp));
        }
        // この系に 1 つのタイマ（8259 経由の PIT）。較正より前と、Local APIC のタイマへ移れなかった機械で届く。
        // 今のタイマかどうかは、受け取る側（`claim`）が見分けてある（8259 の採番ではなく、現在の配送先を問う）。
        Claim::GlobalTimer => {
            crate::arch::x86_64::count_timer_tick();
            // **単調なティック（W2-d+）。** **較正より前は 8259 経由なので、ここも数える**
            // ——**2 箇所に置かないと、起動直後のティックが落ちる。**
            advance_clock();
            // SAFETY: 実際に発生した割り込みに対してのみ呼んでいる。割り込みゲート経由で入場したので
            // IF=0 で、BKL の中なので、ほかの実行文脈が同時に 8259 を触ることはない。
            unsafe { crate::machine::pc::complete(claimed) };
            // タイマはプリエンプティブに切り替える（M5-d）。EOI はここより前で送っているので、次タスクは
            // IF=1 で次ティックを受けられる。遠征を畳むのは Local APIC のタイマの側だけである（今までどおり）。
            return ExitAction::Resume(crate::task::on_timer_tick(current_sp));
        }
        // このベクタはどの源か。移行済みの経路も含めて、受け取る側（`claim`）が引いてある（S2-d-1c。I/O APIC
        // 経由のベクタは 8259 の採番表に載っていない）。源の番号は、配送先のベクタが 8259 経由と I/O APIC 経由で
        // 違っても変わらない。
        Claim::Source { source, .. } => {
            // 源に登録した処理を呼ぶ（`ADR-0072` の 5。9d-4）。完了させる前に呼ぶ。レベルで鳴る源は、処理が源を
            // 下ろしてから戻る（キーボードはデータポートを読み切り、virtio-blk は ISR を読む。`ADR-0072` の 4）。
            // 登録は起動の間だけで、起動の後は表が変わらないので、ここで読むのにロックは要らない。
            //
            // 処理の無い源は、`machine` がその源を禁止してから完了させ、ここで数える（`ADR-0072` の 4。9d-4b）。
            // 禁止しないと、下ろす者の居ないレベルの源が鳴り続ける。数は 1 度だけ出す行に出す
            // （[`report_arrivals_without_handler_once`]）。
            let Some(handler) = INTERRUPT_HANDLERS.handler(source) else {
                // SAFETY: 実際に発生した割り込みに対してのみ、完了させる代わりに 1 回呼ぶ。割り込みゲート経由で
                // 入場したので IF=0 で、BKL の中なので、ほかの実行文脈が同時に 8259 と I/O APIC を触ることはない。
                if unsafe { crate::machine::pc::disable_and_complete(claimed) } {
                    note_arrival_without_handler(source);
                }
                return ExitAction::Resume(current_sp);
            };
            handler(source);

            // 処理を終えてから完了させる（`complete`）。スプリアス（偽）割り込みの判定もそこで行う。
            // IRQ7 / IRQ15 でしか起きず、本物なら ISR の該当ビットが立っている。EOI の宛先は純粋ロジックが
            // 決める（スプリアスの扱いはマスタ側とスレーブ側で非対称）。送った時点で PIC は次の同じ
            // 割り込みを上げられるようになる。
            //
            // 破壊テスト (S13-d, virtio-skip-eoi-test): virtio の IRQ にだけ EOI を
            // 送らない。LAPIC の ISR ビットが立ったままになり、同じ優先度
            // クラス以下の割り込みが以後届かなくなる形を狙う。
            #[cfg(feature = "virtio-skip-eoi-test")]
            let skip_eoi = crate::virtio::armed_source() == Some(source);
            #[cfg(not(feature = "virtio-skip-eoi-test"))]
            let skip_eoi = false;

            if !skip_eoi {
                // SAFETY: 実際に発生した割り込みに対してのみ呼んでいる。割り込みゲート経由で入場したので
                // IF=0 で、BKL の中なので、ほかの実行文脈が同時に 8259 を触ることはない。
                unsafe { crate::machine::pc::complete(claimed) };
            }
        }
        // テスト専用ベクタ（`0x40`、どちらの表にも無い）などの受け取れない到着は、数えるだけで
        // 完了させない。EOI の論理が一切絡まない。IPI の探りは BKL の前で戻っている。
        Claim::IpiProbe | Claim::Unclaimed => {}
    }

    // キーボードやテストベクタは切り替えない（入場時の RSP を返す）。
    ExitAction::Resume(current_sp)
}

/// yield の入口関数（`ADR-0072` の 1 のソフトの入口。2026-09-28。境界の段階の手順 2 の 9d-2）。
///
/// # 契約（境界の関数。2026-09-28）
///
/// - 呼ぶのは `arch` の割り込みの入口だけで、yield のベクタで入ったときに、割り込みを止めたまま呼ぶ。
/// - BKL を取り、スケジューラに次のタスクを選ばせて、そのスタックポインタを返す（協調的 yield。M5-c）。
///   BKL は戻る直前に解く。コントローラを通らないので、完了させるものは無い。
pub fn on_yield_interrupt(interrupted: &crate::arch::x86_64::Interrupted<'_>) -> u64 {
    let _bkl = acquire_bkl_for_interrupt();
    crate::task::on_yield(interrupted.stack_pointer())
}

/// 割り込みの入口関数が BKL を取る（S4-b-2）。ガードを落とすまで、カーネルへ入れるのは 1 コアだけである。
/// 早期 return が複数あるので RAII にする（解放を各 return の手前へ書くと、1 つ落としたときに保持したまま戻り、
/// 系全体が止まる）。
///
/// 同時進入は BKL の中で数える（S4-b-3）。ここで別に数えると、定義が 2 つになる。
fn acquire_bkl_for_interrupt() -> crate::bkl::BklGuard {
    // 破壊テスト (S4-b-4, bkl-skip-timer-entry): ロックを取らず計数だけ行う。
    // 数えているものが本番と違う（`acquire_counting_only` の doc）。
    #[cfg(feature = "bkl-skip-timer-entry-test")]
    let bkl = crate::bkl::acquire_counting_only(crate::bkl::KernelEntry::Irq);
    #[cfg(not(feature = "bkl-skip-timer-entry-test"))]
    let bkl = crate::bkl::acquire(crate::bkl::KernelEntry::Irq);
    widen_entry_window();
    bkl
}

/// 破壊テスト (S4-b-4, bkl-widen-entry-window): 入口の保持区間を広げる。
/// 重なりの増幅器であって、素の重なりの頻度とは別である（feature の doc）。破壊テストでないときは何もしない。
fn widen_entry_window() {
    #[cfg(feature = "bkl-widen-entry-window-test")]
    for _ in 0..crate::bkl::WIDENED_ENTRY_WINDOW_SPINS {
        core::hint::spin_loop();
    }
}

/// 単調なティックを進め、締切を過ぎたタイマの待ちを起こす（W2-d+）。進めるのは BSP だけである
/// （[`crate::arch::x86_64::advance_monotonic_ticks`]）。
fn advance_clock() {
    if let Some(now) = crate::arch::x86_64::advance_monotonic_ticks() {
        // **締切を過ぎたタイマの待ちを起こす（W2-d+）。** **進めた直後に、同じ文脈で起こす**
        // ——**IF=0 かつ BKL の内側である**（外からの割り込みの入口関数が取っている）。
        crate::task::wake_expired_timers(now);
    }
}

/// 中断（Ctrl+C）が要求されていれば、走っている子の遠征を畳むと決める（S12 前の手当て、C）。
///
/// **条件が揃わなければ偽を返す。揃えば真を返し、畳むのは入口が行う**（出口の動き。2026-09-28。9d-3。それまでは
/// `arch` の `fold_if_interrupted` が、決めることと畳むことを両方持っていた）。
///
/// # ここが「深さで分ける」唯一の場所である
///
/// **フラグを立てる側は深さを見ない**（`crate::input::note_scancode_for_interrupt`）。
/// **消費する側もここだけである。** 深さ 1 では消費されず、フラグは立ったまま残るが、
/// **子を起こす直前に降りる**（`crate::userland` が呼ぶ
/// `crate::input::clear_interrupt_request`）ので持ち越さない。
///
/// **深さ 1 の 0x03 は、この経路をまったく通らない。** `Decoder` が
/// 制御文字として出し、前景を通してシェルへ届く。**行を捨てるのはシェルの仕事である。**
///
/// # 条件の順序に意味がある
///
/// **フラグを消費するのは最後である。** 先に消費すると、深さ 1 や
/// カーネル由来の割り込みで**フラグだけが消えて終了させられない。**
///
/// # 3 つの条件
///
/// - **深さが 2 以上**（子が走っている）。深さ 1 はシェル自身なので終了させない
/// - **その割り込みが Ring 3 から来た**（`CS` の RPL が 3）。例外側の条件 (2) と
///   同じ形で、**CPU が積んだ事実だけを見る**
/// - **フラグが立っている**（そして降ろす）
fn should_fold_excursion(interrupted: &crate::arch::x86_64::Interrupted<'_>) -> bool {
    // 破壊テスト (S12 前の手当て C, kill-fold-at-depth-one): 深さ 1 でも終了させる。
    // **シェル自身が Ctrl+C で死ぬ**ので、`init` が起動し直す回数が増える。
    #[cfg(feature = "kill-fold-at-depth-one-test")]
    const MINIMUM_DEPTH: usize = 1;
    #[cfg(not(feature = "kill-fold-at-depth-one-test"))]
    const MINIMUM_DEPTH: usize = 2;

    let depth = interrupted.excursion_depth();
    // **切り離して起動するスロットは深さ 1 でも終了させる（`ADR-0063` の (b3)）。** **そこに居るのは
    // 常に子で、シェルは居ない**——**`spin | cat` の `spin` はスロット 1 の深さ 1 である。**
    // **1 回の押しで終了させるのは 1 本である**（フラグは `take` で 1 回だけ消費される）。**両方が
    // Ring 3 で回っていれば 2 回押す。** **カーネルの中で待っている子には届かない**
    // （`ADR-0063` の (b3) の限界）。
    //
    // 破壊テスト (2026-09-28, kill-fold-ignore-detached-slot-test): 切り離したスロットも深さ 1 で弾く。**`spin | cat`
    // の `spin` が Ctrl+C で止まらなくなる**（`--shell-test` の「ctrl-c stopped the detached spin of a pipeline」が
    // 落ちる）。既定のビルドの式は変えない。
    #[cfg(not(feature = "kill-fold-ignore-detached-slot-test"))]
    let minimum_depth = if interrupted.excursion_slot() == crate::task::detached_slot() {
        1
    } else {
        MINIMUM_DEPTH
    };
    #[cfg(feature = "kill-fold-ignore-detached-slot-test")]
    let minimum_depth = MINIMUM_DEPTH;
    if depth < minimum_depth {
        // **深さ 1 を弾いたことを数える（W2-c-2 の対策）。**
        // **既定では 1 以上、破壊テストでは 0 である**（[`DEPTH_ONE_NOT_FOLDED`] の doc）。
        if depth == 1 {
            DEPTH_ONE_NOT_FOLDED.fetch_add(1, Ordering::Relaxed);
        }
        return false;
    }
    if !interrupted.from_user() {
        return false;
    }
    crate::input::take_interrupt_request()
}

/// 深さがちょうど 1 だったので畳まなかった回数（W2-c-2 の手当て。`ADR-0061`）。
///
/// **「深さ 1 では畳まない」が働いたことの観測である。** **判定は「1 以上」を見る。**
///
/// # 既定では必ず 1 以上になる
///
/// **シェルは遠征中（深さ 1）にタイマ IRQ を受け続けるので、ここを通る。**
/// **打鍵にも待ちにも依らない**——**タイマは 100Hz で入り、セッションは数十秒ある。**
///
/// # 破壊テストでは 0 になる
///
/// **`kill-fold-at-depth-one-test` は [`MINIMUM_DEPTH`] を 1 にするので、深さ 1 は
/// この分岐へ来ない。**
///
/// # なぜ「深さ 1 で畳んだ回数」を数えないのか
///
/// **それでは破壊が捕まらない。** **終了させるには「深さ 1」と「Ring 3 から来た IRQ」の
/// 両方が要るが、待つ形ではシェルが Ring 3 に居るのは `read(0)` が戻ってから次の
/// `read(0)` へ入るまでの μs 単位しかない**——**打鍵の間隔 32 ミリ秒に対して 1% 未満の
/// 見込みで、破壊テストを立てても 0 のままになる**（`ADR-0061`。**実測で 4 回続けて
/// 検出されなかった**）。**弾いた側を数えると、そのウィンドウに依らない。**
static DEPTH_ONE_NOT_FOLDED: AtomicU64 = AtomicU64::new(0);

/// [`DEPTH_ONE_NOT_FOLDED`] の値（W2-c-2 の対策。`init` がセッションの後に出す）。
pub fn depth_one_not_folded() -> u64 {
    DEPTH_ONE_NOT_FOLDED.load(Ordering::Relaxed)
}

/// 処理の表の大きさ（源の数。`ADR-0072` の 5。2026-09-28。境界の段階の手順 2 の 9d-4）。
///
/// 源の番号は `machine` が採番する（[`InterruptSource`]）。今の採番は ISA の IRQ の番号（2 台の 8259 の 16 本）で、
/// I/O APIC へ移した IRQ も同じ番号で受け取る（[`crate::machine::pc::claim`]）。
const INTERRUPT_SOURCES: usize = 16;

/// 外からの割り込みの源の番号（カーネルの登録番号。`ADR-0072` の 3。2026-09-29。境界の段階の手順 2 の 9e）。
///
/// # 契約（境界の型。2026-09-29）
///
/// - **`machine` が装置の割り込みを解決して返す**（ISA の IRQ は [`crate::machine::pc::source_for_isa_irq`]、PCI の
///   INTx は [`crate::machine::pc::source_for_pci_intx`]）。受け取った割り込みも、源の番号で届く
///   （[`crate::machine::pc::Claim`]）。
/// - **共通の側は、番号を作らず、読まない**——処理の表の添字に使うのと、起動ログの行に番号を出す（`Display`）
///   だけである。ISA の IRQ 番号・GSI・PCI の割り込み線・ベクタとは別の型で、取り違えを型で防ぐ。
/// - 例外・IPI・タイマは源の番号を持たない（別の入口か、受け取りの種類である）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InterruptSource(u8);

impl InterruptSource {
    /// `machine` が源を解決したときに作る（共通の側は作らない。処理の表は、自分の添字から一覧を作るときだけ作る）。
    pub(crate) const fn assigned_by_machine(index: u8) -> Self {
        Self(index)
    }

    /// 処理の表の添字（この表と、源を ISA の IRQ に直す `machine` だけが使う）。
    pub(crate) const fn table_index(self) -> u8 {
        self.0
    }
}

impl core::fmt::Display for InterruptSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 外からの割り込みの処理の表（`ADR-0072` の 5。2026-09-28。9d-4）。添字は源の番号である。
///
/// 登録は起動の間だけで、閉じた（[`Self::close`]）後は断る。起動の後は表が変わらないので、割り込みの中で
/// 読むのにロックが要らない（BKL を取らない種類からも読める）。
///
/// 処理を整数で持つのは、`common::percpu` の `install_cpu_id_reader` と同じ形である（0 は「登録なし」。
/// 関数のポインタは 0 にならない）。
struct HandlerTable {
    handlers: [AtomicUsize; INTERRUPT_SOURCES],
    closed: AtomicBool,
}

/// 登録を断った理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// 登録を閉じた後（起動の後）だった。
    AfterBoot,
    /// 表の外の番号だった。
    OutsideTable,
    /// その源には、もう処理がある。黙って置き換えない。
    AlreadyRegistered,
}

impl HandlerTable {
    const fn new() -> Self {
        Self {
            handlers: [const { AtomicUsize::new(0) }; INTERRUPT_SOURCES],
            closed: AtomicBool::new(false),
        }
    }

    /// 源に処理を登録する。閉じた後・表の外・2 つ目は断る。
    fn register(
        &self,
        source: InterruptSource,
        handler: fn(InterruptSource),
    ) -> Result<(), Refusal> {
        if self.closed.load(Ordering::Acquire) {
            return Err(Refusal::AfterBoot);
        }
        let slot = self
            .handlers
            .get(usize::from(source.table_index()))
            .ok_or(Refusal::OutsideTable)?;
        slot.compare_exchange(0, handler as usize, Ordering::Release, Ordering::Relaxed)
            .map(|_| ())
            .map_err(|_| Refusal::AlreadyRegistered)
    }

    /// 登録を閉じ、処理のある源の一覧を返す。
    fn close(&self) -> RegisteredSources {
        self.closed.store(true, Ordering::Release);
        self.sources()
    }

    /// 処理のある源の一覧（小さい順）。登録は閉じない。
    fn sources(&self) -> RegisteredSources {
        let mut sources = [InterruptSource::assigned_by_machine(0); INTERRUPT_SOURCES];
        let mut count = 0;
        for (index, slot) in (0..).zip(&self.handlers) {
            if slot.load(Ordering::Acquire) != 0 {
                // 表の添字が源の番号である（`machine` の採番をそのまま添字にしている）。
                sources[count] = InterruptSource::assigned_by_machine(index);
                count += 1;
            }
        }
        RegisteredSources { sources, count }
    }

    /// 源に登録した処理（無ければ `None`）。
    fn handler(&self, source: InterruptSource) -> Option<fn(InterruptSource)> {
        let address = self
            .handlers
            .get(usize::from(source.table_index()))?
            .load(Ordering::Acquire);
        if address == 0 {
            return None;
        }
        // SAFETY: 0 でない値を書くのは `register` だけで、書くのは `fn(InterruptSource)` を `usize` へ変えた値である。
        // 関数のポインタは `usize` と同じ大きさで、0 にならない。書く側と読む側の順序: 書くのは起動の間の登録だけで、
        // その源を許可する前に書く（Release）。読むのは、その源を許可した後に届いた割り込みの中である（Acquire。
        // 起動の途中に届いた割り込みでも読む）。起動の後は誰も書かない。読み書きは原子的なので、読めるのは 0 か、
        // 書き終えた値だけである。
        Some(unsafe { core::mem::transmute::<usize, fn(InterruptSource)>(address) })
    }
}

/// 処理のある源の一覧（小さい順）。起動ログの行に出し、割り込みを許してよい源の期待に使う（9d-5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisteredSources {
    sources: [InterruptSource; INTERRUPT_SOURCES],
    count: usize,
}

impl RegisteredSources {
    /// 処理のある源（小さい順）。
    pub fn as_slice(&self) -> &[InterruptSource] {
        &self.sources[..self.count]
    }
}

impl core::fmt::Display for RegisteredSources {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Some((first, rest)) = self.as_slice().split_first() else {
            return f.write_str("none");
        };
        write!(f, "{first}")?;
        for source in rest {
            write!(f, ", {source}")?;
        }
        Ok(())
    }
}

/// 外からの割り込みの処理の表の本体。
static INTERRUPT_HANDLERS: HandlerTable = HandlerTable::new();

/// 外からの割り込みの源に、処理を登録する（`ADR-0072` の 5。2026-09-28。境界の段階の手順 2 の 9d-4）。
///
/// # 契約（2026-09-28）
///
/// - 呼ぶのは装置のドライバで、起動の途中、その源を許可する前に 1 回だけ呼ぶ（処理を登録してから源を許可する）。
///   今の呼び手（キーボードと virtio-blk の用意）は、ほかの CPU が走り出す前で、割り込みを止めた所にいる。
/// - 起動の後（[`close_interrupt_handler_registration`] の後）に呼ぶと、名前つきで止まる。書く側の守り（ページ
///   テーブルの `ensure_child`）と同じ形である。表の外の番号と、同じ源への 2 つ目の登録も、名前つきで止まる。
/// - 登録した処理は、その源の割り込みが届くたびに、BKL の中で、完了させる前に呼ばれる。引数は源の番号である
///   （9e。それまでは到着の番号（x86 ではベクタ）だった）。レベルで鳴る源の処理は、源を下ろしてから戻ること
///   （`ADR-0072` の 4）。
pub fn register_interrupt_handler(source: InterruptSource, handler: fn(InterruptSource)) {
    match INTERRUPT_HANDLERS.register(source, handler) {
        Ok(()) => {}
        Err(Refusal::AfterBoot) => panic!(
            "interrupts: refused to register a handler for source {source} after boot; the handler table \
             is fixed before init (ADR-0072)"
        ),
        Err(Refusal::OutsideTable) => panic!(
            "interrupts: refused to register a handler for source {source}; the table has \
             {INTERRUPT_SOURCES} sources"
        ),
        Err(Refusal::AlreadyRegistered) => panic!(
            "interrupts: refused to register a second handler for source {source}; it already has one"
        ),
    }
}

/// 処理の無い源が届いた回数（`ADR-0072` の 4。2026-09-28。9d-4b）。`machine` がその源を禁止してから完了させた
/// 回数で、スプリアス（8259 の 7 番と 15 番）は数えない。**正常な起動では 0 である**——源は処理を登録してから
/// 許可する（`ADR-0072` の 5）。
static ARRIVALS_WITHOUT_HANDLER: AtomicU64 = AtomicU64::new(0);

/// 処理の無いまま届いた最初の源。まだなら [`NO_SOURCE_YET`]。
static FIRST_SOURCE_WITHOUT_HANDLER: AtomicU16 = AtomicU16::new(NO_SOURCE_YET);

/// [`FIRST_SOURCE_WITHOUT_HANDLER`] の「まだ無い」。源の番号は 8 ビットに収まるので、この値にはならない。
const NO_SOURCE_YET: u16 = u16::MAX;

/// [`report_arrivals_without_handler_once`] が既に出したか。
static ARRIVALS_WITHOUT_HANDLER_REPORTED: AtomicBool = AtomicBool::new(false);

/// 処理の無い源が届いたことを数え、最初の源を控える（割り込みの中から呼ぶ。出力しない。ADR-0018 §5）。
///
/// 源を控えてから数を増やす（Release）ので、数が 0 でないのを見た読み手（Acquire）には源が見える。
fn note_arrival_without_handler(source: InterruptSource) {
    let _ = FIRST_SOURCE_WITHOUT_HANDLER.compare_exchange(
        NO_SOURCE_YET,
        u16::from(source.table_index()),
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    ARRIVALS_WITHOUT_HANDLER.fetch_add(1, Ordering::Release);
}

/// 処理の無い源を禁止したことを、1 度だけ出す（`ADR-0072` の 4。2026-09-28。9d-4b）。数が 0 なら何も出さない。
///
/// **心拍の行には載せない**——心拍の行の文言は多くの判定が見ているので、変える所を増やさない（運用者の決定）。
/// **行の境目から呼ぶ**——定常ループの 1 周ごとと、プログラムを起動する入口（`userland::spawn`）の 2 か所で、
/// キーボードの最初の到着の行（`keyboard::report_first_delivery_once`）と同じ所である。書き先の型は名指ししない
/// （共通の側で機械の言葉を増やさないため。どちらの呼び手もシリアルへ出す）。
pub fn report_arrivals_without_handler_once<W: core::fmt::Write>(logger: &mut Logger<W>) {
    let arrivals = ARRIVALS_WITHOUT_HANDLER.load(Ordering::Acquire);
    if arrivals == 0 || ARRIVALS_WITHOUT_HANDLER_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let source = FIRST_SOURCE_WITHOUT_HANDLER.load(Ordering::Relaxed);
    logger.warn(format_args!(
        "interrupts: disabled source {source}, which arrived with no handler registered ({arrivals} \
         arrival(s) without a handler so far)"
    ));
}

/// 割り込みの処理の登録を閉じる（`ADR-0072` の 5。2026-09-28。9d-4）。
///
/// **起動の終わり、カーネル側の PML4 の指紋を採った直後に 1 回だけ呼ぶ**（`ADR-0072` の 5 が決めた時点）。この後の
/// 登録は名前つきで止まる。戻り値は、処理のある源の一覧である（起動ログの行に出す）。
pub fn close_interrupt_handler_registration() -> RegisteredSources {
    INTERRUPT_HANDLERS.close()
}

/// 処理を登録してある源の一覧（小さい順。`ADR-0072` の 5。2026-09-28。9d-5）。登録は閉じない。
///
/// 割り込みを許してよい源を確かめる所（`sti` の前の確かめと、較正の中の I/O APIC の読み）が、期待に使う。
/// 源は処理を登録してから許可するので、開いていてよいのはこの一覧の源だけである（タイマは別の種類で、
/// ここには入らない）。
pub fn registered_interrupt_sources() -> RegisteredSources {
    INTERRUPT_HANDLERS.sources()
}

/// タイマ割り込みで駆動されるメインループ。
///
/// # `hlt` を無条件に使う理由
///
/// ADR-0018 のチェックリスト 10 は「`cli` → 条件確認 → `sti; hlt`」の並びを
/// 求めている。あれが必要なのは「仕事が無ければ眠る」形のループである。
/// 仕事の有無を確認してから眠るまでの隙間に仕事が発生すると、次の割り込みまで
/// 眠り続けてしまう。
///
/// このループは眠るかどうかを条件で決めない。タイマが 100Hz で必ず起こして
/// くれるので、無条件に `hlt` → 起きたらティックを見る → また `hlt` で足りる。
/// 最悪でも 10ms 後には起きるため、取りこぼしという概念が成立しない。
/// 条件つきの形が要るのは M5 の実行キュー（仕事の有無で眠りを決める）である。
///
/// 限界: この形は、起こしてくれるものが止まった瞬間に永久ハングになる。
/// `hlt` で眠っている以上、カーネル自身はそれを検出できない（検出のための
/// コードが動かない）。外側からは検出できるので、xtask がシリアルログの
/// ハートビート回数で判定する。カーネル内部では検出できないが、テスト基盤
/// では検出できる、という切り分けである。
///
/// # Safety
///
/// 割り込みを有効化する。[`verify_ready_for_sti`](crate::arch::x86_64::interrupt_readiness::verify_ready_for_sti) を通し、タイマの設定と
/// IRQ0 の解禁が済んでいること。
pub unsafe fn run_timer_loop(
    logger: &mut Logger<Serial>,
    console: Option<&mut crate::console::Console>,
    stop_after_ticks: u64,
    shell_after_heartbeats: u64,
    controller: Option<&crate::machine::pc::MappedInterruptController>,
    virtio: Option<&mut crate::virtio::VirtioBlk>,
    pm_timer: Option<crate::machine::pc::pmtimer::PmTimer>,
) {
    // 最初のティックが来るまで何も出ないとハングと区別できないので、
    // 待ちに入ることを先に宣言する。
    // どのベクタで届くはずかは `arch` が表示する（ベクタを共通の側に出さない。`ADR-0072` の 3。9e-2）。
    logger.info(format_args!(
        "timer: waiting for the first tick (expected as {}); \
         if nothing arrives, suspect the PIT setup, the IMR, or ICW2",
        crate::arch::x86_64::timer_delivery()
    ));

    let mut console = console;
    let started = common::arch::x86_64::read_timestamp_counter();

    // SAFETY: 呼び出し側の契約により、7 項目の検証とタイマ設定が済んでいる。
    //
    // `sti` を実行するのはここと [`spin_with_interrupts_enabled`] の 2 箇所
    // だけである（数と経緯は spin 側のコメントと ADR-0018 Addendum 5 を参照）。
    // こちらが「`hlt` で待つ本来の形」で、起こしてくれるタイマが実在する
    // M4-d-2 で初めて成立した（ADR-0018 Addendum 2 §3）。
    unsafe {
        common::arch::x86_64::enable_interrupts();
    }

    // **PIT が 1 本も刻まなかったか**（HW-c。`ADR-0068`）。**較正の基準で決まる。**
    // **ICW2 の事後証明が取れるかが、これで変わる。**
    let mut boot_timer_never_ticked = false;

    // === S2-c: Local APIC タイマの較正 ===
    //
    // この位置でなければならない。基準に使う `TIMER_TICKS` は IRQ0 が
    // 増やすので、`sti` より前では進まない。`start_timer` は `-> !` で戻らず、
    // 通常起動では `run_timer_loop` も戻らないので、「割り込みが有効で、かつ
    // 定常ループへ入る前」という区間はここにしか存在しない。APIC 関連の他の
    // 処理（`kmain` の前半）から離れているのはこのためである。まとめないこと。
    //
    // 較正は測るだけで、LAPIC タイマをタイマとして使わない。LVT Timer は
    // マスクされたままで、LINT0 と SVR にも触らない。
    if let Some(controller) = controller {
        // 破壊テスト (HW-c, pm-timer-treated-as-absent): PM タイマを無いものとして渡す。
        // **PIT が刻まない構成（`pit=off`）で、両方無い道を通す**——**較正の基準が 1 つも
        // 無いことを示して止まる行が出る。** **直す前は黙って止まっていた。**
        let pm_timer = if cfg!(feature = "pm-timer-treated-as-absent") {
            None
        } else {
            pm_timer
        };
        let calibration = crate::machine::pc::calibrate_local_timer(logger, controller, pm_timer);

        // === S2-d-1b: 2 つ目のコントローラ実装を 1 回だけ読ませる ===
        //
        // 切り替えない。読むだけである。配送は PIC / PIT のままで、
        // 書き込みは一切しない。
        //
        // 呼ぶ理由は、2 つ目の実装が実ハードウェアを正しく読めることを、
        // 振る舞いが変わらないうちに確かめておくためである。どこからも
        // 呼ばずに S2-d-1c（配送が変わる段階）へ入ると、そこで落ちたときに
        // 「切り替えが悪いのか、実装が悪いのか」を切り分けられない。
        //
        // 期待は「処理を登録した源だけが開いている」である（9d-5。2026-09-28）。源は処理を登録してから許可し、
        // I/O APIC がある構成では、処理を登録した源はすべて I/O APIC 経由へ移してある（キーボードと
        // virtio-blk）。PIC のマスク状態をここへ持ち込まないこと。別のコントローラの状態である。期待は
        // 呼び出し側が持つ（境界が独自に期待を持たない）。
        //
        // **9d-5 までは、期待にキーボードの IRQ1 だけを渡していた。** virtio-blk の IRQ 11 も開いているので、
        // 下の行は「only the routed IRQs are open=false」だった。
        let registered = registered_interrupt_sources();
        match crate::machine::pc::survey_interrupt_masks(controller, registered.as_slice()) {
            Some(check) => logger.info(format_args!(
                "apic: the I/O APIC controller reads its redirection entries: {check}, \
                 only the routed IRQs are open={} (the PIC still owns every other line)",
                check.matches()
            )),
            None => logger.warn(format_args!(
                "apic: no I/O APIC was mapped, so the second controller implementation \
                 could not be exercised"
            )),
        }

        // === S2-d-2: タイマを Local APIC タイマへ移す ===
        //
        // 較正より後でなければならない。初期カウントを較正の戻り値から
        // 求めるためである。そして切り替えの区間ではティックが 1 本も
        // 来ないので、`TIMER_TICKS` を待つ処理（較正のエッジ待ち）は
        // ここより前に済んでいる必要がある。
        // **PIT が刻まなかった回は、ICW2 の事後証明が取れない**（HW-c。`ADR-0068`）——
        // **PIC の割り込みが 1 本も届かないので、`first_pic_vector` は `None` のままである。**
        // **較正の基準を見て決める**（`None` を一色に扱うと、ICW2 の誤りと PIT の不在が混ざる）。
        boot_timer_never_ticked = matches!(
            calibration.as_ref().map(|value| value.reference()),
            Some(crate::machine::pc::CalibrationReference::PmTimer)
        );
        if let Some(calibration) = calibration {
            // SAFETY: ここは起動の途中に BSP が 1 回だけ通る（`run_timer_loop` を呼ぶのは `main.rs` の `start_timer` だけで、
            // `start_timer` は戻らない）。ベクタ LAPIC_TIMER_VECTOR には専用スタブのゲートが入っており（`idt::init`）、
            // ハンドラは EOI を Local APIC へ送って戻る。
            unsafe { crate::machine::pc::switch_to_local_timer(logger, calibration) };
        } else {
            logger.warn(format_args!(
                "apic: the local APIC timer was not calibrated, so the timer stays on the PIT; \
                 the 8259 keeps delivering IRQ0"
            ));
        }
    }

    // プリエンプティブマルチタスクのデモと検証（M5-d）。timer が動き出した
    // この時点で 1 区間だけ実行する。通常起動（stop_after_ticks == 0）でのみ行う。
    // interrupt-test の有限ループ（stop_after_ticks > 0）では実行しない。デモが
    // 終わるとワーカーは走行不可になり、以降このハートビートループは
    // プリエンプトされない（runnable がメインだけなので on_timer_tick は
    // no-op）。
    if stop_after_ticks == 0 {
        crate::task::run_preemptive_demo();

        // === S3-b-2b-1: AP を起動する ===
        //
        // 位置は 2 つの制約で決まっている。`sti` より後でなければならない
        // （10ms の待ちをタイマのティックで作る）。そしてプリエンプティブ
        // デモの後でなければならない（デモの最中に AP が起きると、周回数や
        // 窓カウントの観測に混ざる）。
        //
        // ここに `cli` / `sti` は追加していない。許可リストの数は変わらない。
        if let Some(controller) = controller {
            let mmio = controller.mmio();
            // SAFETY: `controller` はマップ済み、タイマは動いている（直前まで
            // デモが走った）、起動時の 1 回だけである。
            let report =
                unsafe { crate::smp::wake_application_processors(logger, controller, &mmio) };
            logger.info(format_args!(
                "smp: application processors: {} usable CPU(s) reported, {} AP(s) attempted, \
                 {} started, {} skipped for lack of a per-CPU slot (MAX_CPUS={})",
                report.usable,
                report.attempted,
                report.started,
                report.skipped_no_slot,
                common::percpu::MAX_CPUS
            ));
            if report.started != report.attempted {
                logger.error(format_args!(
                    "smp: only {} of {} application processor(s) reported their start signature",
                    report.started, report.attempted
                ));
            }
            // **起動した AP の CR0・CR4・EFER が BSP と一致すること**（2026-09-24。`ADR-0018` の
            // Addendum 9 の監視。**棚卸しの結論は全 CPU についてである**）。
            crate::arch::x86_64::check_aps_match_bsp(logger, report.started);

            // **シリアルの排他の演習（BSP 側）。** **合図を立ててから、AP と
            // 同時に既知の行を書く。** **`kernel/src/smp.rs` の
            // `run_serial_stress_on_ap` が相手である。**
            #[cfg(feature = "serial-stress-test")]
            run_serial_stress_on_bsp(logger);
        }

        // === S13-d: 割り込みが実際に届くことの実演 ===
        //
        // **`sti` の後・AP 起床の後である。** 主張は「届いて数えられる」だけで、
        // 完了の待ちはポーリングのまま（眠りは d-2。ADR-0036）。届かなければ
        // 上限で止まる。**エッジのまま書く破壊テストはここでは落ちない**——実測で
        // QEMU は極性とトリガを厳密に模らず、届いてしまう。あちらを検出する
        // のは配線の読み戻し（宣言との一致）である。
        // **武装済みのときだけ実演する。** I/O APIC が無い構成（ACPI の破壊テストの
        // 一群）では配線されておらず、待っても届かない——あの構成の主張は
        // 「ACPI が読めなくても起動は続く」なので、ここで止めてはならない。
        // 閉じたままであることは `start_timer` が判定行に出している。
        if crate::virtio::armed_source().is_some() {
            if let Some(virtio) = virtio {
                // SAFETY: 配線と武装は `start_timer` が `sti` より前に済ませ、
                // いま IF=1 である。リングとポートウィンドウはこの struct だけが触り、
                // ISR ポートだけはハンドラと共有する（意図した相互作用）。
                if let Err(reason) =
                    unsafe { crate::virtio::exercise_interrupt_read(logger, virtio) }
                {
                    logger.error(format_args!(
                        "virtio-blk: the interrupt exercise failed ({reason:?}); halting"
                    ));
                    common::arch::x86_64::halt_forever();
                }
                // d-2: BKL を解いて眠り、割り込みで起きる（ADR-0036）。
                // SAFETY: 上と同じ位置（配線・武装済み、IF=1）。
                if let Err(reason) =
                    unsafe { crate::virtio::exercise_blocking_read(logger, virtio) }
                {
                    logger.error(format_args!(
                        "virtio-blk: the blocking read failed ({reason:?}); halting"
                    ));
                    common::arch::x86_64::halt_forever();
                }
            }
        }

        // 増幅器 (S4-c-4-3, sched-keep-workers-runnable): AP が起動した後で
        // デモのワーカーを走行可能へ戻す。単独では何も主張しない——
        // bootstrap processor が巡回を続けるだけで、第 1 層が AP を弾く。
        //
        // 位置はここでなければならない。デモより前だとデモの観測に混ざり、
        // 締切分岐を止める形にすると `run_preemptive_demo` が戻らずAP の起動へ
        // 到達しない（`task::rearm_workers_for_smp_stimulus` の doc）。
        #[cfg(feature = "sched-keep-workers-runnable")]
        crate::task::rearm_workers_for_smp_stimulus();

        // === S5-c: TLB シュートダウンの実証（4 段の手順）===
        //
        // 手順の理由は `smp::shootdown_probe` の doc にある。
        // 「AP が触って #PF」だけでは差が出ない——TLB に翻訳が載っていなければ、
        // 世代を上げない構成でもページテーブルを歩いて #PF になる。
        #[cfg(feature = "smp-tlb-shootdown-probe")]
        {
            use crate::smp::shootdown_probe;

            // (1)(2) AP に触らせ、触れたことを確かめる。
            shootdown_probe::command(shootdown_probe::TOUCH_FIRST);
            let mut spun = 0u32;
            while shootdown_probe::touches() == 0 && spun < SHOOTDOWN_PROBE_WAIT_SPINS {
                core::hint::spin_loop();
                spun += 1;
            }
            let first = shootdown_probe::touches();
            logger.info(format_args!(
                "smp: shootdown probe step 1-2: the ap touched the probe page {first} time(s) \
                 (this is the positive evidence that the translation is in its TLB; without it \
                 the comparison below is meaningless)"
            ));
            if first == 0 {
                logger.error(format_args!(
                    "smp: the ap never touched the probe page; the shootdown comparison is void"
                ));
            } else {
                // (3) BKL を保持したままマッピングを外し、世代を上げる。
                let flushes_before = crate::bkl::generation_flushes_for(1);
                {
                    let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);
                    // SAFETY: 稼働中のテーブルから、探り用にマップした 1 ページを外す。
                    let mut table = unsafe {
                        crate::arch::x86_64::ActivePageTable::current(common::addr::direct_map())
                    };
                    if let Some(virt) = common::addr::VirtAddr::new(shootdown_probe::virt()) {
                        // SAFETY: 探り用にマップしたページで、他の誰も使っていない。
                        let _ = unsafe { table.unmap_4kib(virt) };
                    }
                    // 破壊テスト (S5-c, smp-tlb-no-generation-bump): 世代を上げない。
                    // AP はフラッシュしないので、古い翻訳で成功する。
                    #[cfg(not(feature = "smp-tlb-no-generation-bump"))]
                    crate::bkl::note_mapping_changed();
                }
                // 世代方式が土台である。上げた場合、AP は次の取得でフラッシュする。
                // その完了を待ってから触らせるので、勝負が時間に依らない。
                let mut spun = 0u32;
                while crate::bkl::generation_flushes_for(1) == flushes_before
                    && spun < SHOOTDOWN_PROBE_WAIT_SPINS
                {
                    core::hint::spin_loop();
                    spun += 1;
                }
                // 主張を数字ではなく文で出す（S8-d で作り直した）。判定側は
                // 「the ap flushed」/「the ap did not flush」を見る。数字は観測用。
                let flushes_after = crate::bkl::generation_flushes_for(1);
                logger.info(format_args!(
                    "smp: shootdown probe step 3: unmapped the probe page; {} (flushes {} -> {})",
                    if flushes_after > flushes_before {
                        "the ap flushed"
                    } else {
                        "the ap did not flush"
                    },
                    flushes_before,
                    flushes_after
                ));

                // (4) もう一度触らせる。
                //
                // **主張が非対称である（S8-d で作り直した）。** フラッシュした側は
                // 2 回目の触りが必ず #PF になる——翻訳が無いので歩き、マッピングが無いので
                // 落ちる。**これはフラッシュの帰結として保証される。** 一方
                // **フラッシュしなかった側の結果は主張しない**——古い翻訳が TLB に
                // 残り続けることは、アーキテクチャが**許しているだけで約束していない**
                // （実 CPU でも QEMU でも、容量の都合でいつでも捨てられてよい）。
                // かつては「古い翻訳で成功する」を期待に置いていて、TCG の TLB の
                // 追い出しがレイアウト依存で発火し、決定的に落ちた（S8-d）。
                shootdown_probe::command(shootdown_probe::TOUCH_AGAIN);
                let mut spun = 0u32;
                let attempts_before = shootdown_probe::attempts();
                let touches_before = shootdown_probe::touches();
                while shootdown_probe::attempts() == attempts_before
                    && spun < SHOOTDOWN_PROBE_WAIT_SPINS
                {
                    core::hint::spin_loop();
                    spun += 1;
                }
                // 触りが #PF になる側では、AP がダンプをシリアルへ書いている最中で
                // ある。**触れたか、予算を使い切るまで待ってから書く**——成功する側は
                // touches の増分で早く抜け、落ちる側は予算ぶんの時間が AP のダンプの
                // 完了に充てられる。
                //
                // **理由が 2 つあったうちの 1 つは失効した（`ADR-0059`）。**
                // **「シリアルにはロックが無いので、すぐ書くと AP のダンプとバイト単位
                // で混ざる」を理由に挙げていたが、いまはロックが在る。** **待ちは残す**
                // ——**もう 1 つの理由（触りが届くのを待つ）はそのまま生きている。**
                // **外すなら、この判定の周りを触るときに測って決めること**
                // （`docs/deferred-decisions.md` の「混線を避けて選んだ形」）。
                let mut spun = 0u32;
                while shootdown_probe::touches() == touches_before
                    && spun < SHOOTDOWN_PROBE_WAIT_SPINS
                {
                    core::hint::spin_loop();
                    spun += 1;
                }
                logger.info(format_args!(
                    "smp: shootdown probe step 4: attempts={} touches {first} -> {} (the \
                     flushed side must fault on the second touch: no translation, no mapping. \
                     The unflushed side's outcome is not asserted: keeping a stale translation \
                     is permitted to a TLB, never promised. The attempt count is the evidence \
                     that the ap tried)",
                    shootdown_probe::attempts(),
                    shootdown_probe::touches()
                ));
            }
        }

        // === S5-b: 世代を 1 つ上げて、AP が次の取得でフラッシュすることを見る ===
        //
        // 本番にはマッピングを変える経路が無いので、そのままでは一度も発火しない。
        // 発火させて機序を見るためだけの feature である。
        //
        // BKL を保持したまま上げる——それが `note_mapping_changed` の契約で、
        // 順序（Acquire/Release の対）が成り立つ前提でもある。
        #[cfg(feature = "smp-tlb-generation-probe")]
        {
            // 上げる前の AP のフラッシュ回数を控える（S7-d で足した）。
            // 控えないと、後から「この bump のせいで増えた」が言えない。
            // ハートビートは bump より後にしか出ないので、前の値はここでしか取れない。
            // シュートダウンの探り（S5-c）は最初から同じ形で出している。対称にした。
            let flushes_before = crate::bkl::generation_flushes_for(1);
            {
                let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);
                crate::bkl::note_mapping_changed();
            }
            // AP が実際にフラッシュするまで待つ。上限つきである。
            // 待ちの上限。シュートダウンの探りの定数はその feature の下にしか
            // 無いので、ここに持つ（同じ桁）。上限の無い待ちを書かない。
            const GENERATION_PROBE_WAIT_SPINS: u32 = 3_000_000;
            let mut spun = 0u32;
            while crate::bkl::generation_flushes_for(1) == flushes_before
                && spun < GENERATION_PROBE_WAIT_SPINS
            {
                core::hint::spin_loop();
                spun += 1;
            }
            logger.info(format_args!(
                "smp: bumped the tlb generation to {} while holding the BKL; every core must \
                 flush at its next acquire (the entry takes the BKL on every tick, so this \
                 settles within one tick); ap flushes {} -> {}",
                crate::bkl::tlb_generation(),
                flushes_before,
                crate::bkl::generation_flushes_for(1)
            ));
        }

        // === S5-a: 測定用 IPI を送る（feature `smp-ipi-probe` のときだけ）===
        //
        // 既定ビルドでは送らない。送る側は上限つきとはいえスピンで待つので、
        // 他の検査の時間の形を変える。実際に踏んだ——既定ビルドへ入れたところ、
        // S4-c-4-3 の梯子（第 2 層の実証）が 5 回中 4 回しか通らなくなった。
        // 位置を刺激の後ろへ動かしても揺れは残った。測るためのものが別の検査の
        // 前提を壊すので、測るときだけ入れる形にする。
        //
        // 位置も刺激より後ろにしてある（前に置くと AP の起動から刺激までが延びる）。
        #[cfg(feature = "smp-ipi-probe")]
        {
            // === S5-a: 測定用 IPI を 1 本送る ===
            //
            // 目的は「TCG で IPI が届くか」を測ることだけである。
            // 宛先は起動した AP で、ハンドラは per-CPU カウンタと EOI だけを行う
            // （`idt::IPI_PROBE_VECTOR`）。BKL は要求しない。
            //
            // 送信完了（ICR の delivery status）と、相手が受け取ったこと（受信
            // カウンタ）は別の量である。前者は既存の AP の起動が見ているものと
            // 同じで、後者が測りたいものである。
            if let Some(controller) = controller {
                for slot in 1..common::percpu::MAX_CPUS {
                    let Some(processor) = crate::machine::pc::started_processor(slot) else {
                        continue;
                    };
                    // 1 本ずつ、受け取りを確かめてから次を送る。
                    //
                    // まとめて送ると数が合わない。同じベクタの IPI は Local APIC の
                    // IRR の 1 ビットなので、処理より速く送るとまとめられる。
                    // 「送った数と受け取った数が一致する」を主張したいなら、
                    // まとめられない送り方にする必要がある。
                    for _ in 0..IPI_PROBE_ROUNDS {
                        let before = crate::arch::x86_64::ipi_probe_received_for(slot);
                        // SAFETY: `controller` はマップ済みで、宛先は起動を確認した AP である。送る所とベクタは
                        // `machine` が持つ（`ADR-0072` の 7。9e-2）。
                        let accepted =
                            unsafe { crate::machine::pc::send_ipi_probe(controller, processor) };
                        if !accepted {
                            logger.error(format_args!(
                                "smp: the ICR did not accept a probe IPI for apic id {processor}"
                            ));
                            break;
                        }
                        crate::arch::x86_64::record_ipi_probe_sent();
                        // 上限つきで待つ（上限の無い待ちを書かない）。
                        let mut spun = 0u32;
                        while crate::arch::x86_64::ipi_probe_received_for(slot) == before
                            && spun < IPI_PROBE_WAIT_SPINS
                        {
                            core::hint::spin_loop();
                            spun += 1;
                        }
                        if crate::arch::x86_64::ipi_probe_received_for(slot) == before {
                            logger.error(format_args!(
                                "smp: a probe IPI to apic id {processor} was accepted by the ICR but \
                                 the target did not handle it within the spin limit"
                            ));
                            break;
                        }
                    }
                    logger.info(format_args!(
                        "smp: probe IPI ({}) to apic id {processor}: sent={} received={} \
                         (one at a time; the same vector coalesces in the IRR if sent faster than \
                         it is handled, so they are not batched)",
                        crate::machine::pc::probe_ipi(),
                        crate::arch::x86_64::ipi_probe_sent(),
                        crate::arch::x86_64::ipi_probe_received_for(slot)
                    ));
                }
            }
        }
    }

    let mut last_ticks = 0u64;
    let mut next_heartbeat = HEARTBEAT_TICKS;
    // **1 ティックあたりの TSC サイクル。** 前のハートビートからの差で出す。
    //
    // **判定行に出すのは、揺れる値だからである**（TCG と KVM で桁が違う）。
    // **docs へ書くと測った条件が変わったときに古くなる。**
    // **対比の相手は `console:` の行の所要である**——あちらは BKL を保持している
    // 区間へ入る量で、**「1 行を書くあいだにティックが何本入りうるか」がここで出る。**
    // **`console:` の行には出せない。** あれは `sti` より前に出るので、
    // その時点ではティックが進んでいない。
    // **両方を同じ時点で読む。** 片方を `0` で始めると、ループへ入る前に
    // 進んでいたぶん（較正が回した PIT のティック）が分母に入り、
    // 1 本目の値だけが桁で外れる（実測で 154,813 と 37,563,944）。
    //
    // **1 本目は `0` が出る。** ループへ入る時点で既に閾値を越えているので、
    // 最初のハートビートは基準と同じティックで出る（差が 0 なので
    // `checked_div` が `None` を返す）。**値が乗るのは 2 本目からである。**
    let mut last_heartbeat_cycles = common::arch::x86_64::read_timestamp_counter();
    let mut last_heartbeat_ticks = crate::arch::x86_64::timer_ticks();
    // 出したハートビートの本数（S11-11）。**シェルへ渡すタイミングを決める。**
    let mut heartbeats = 0u64;
    let mut announced_first = false;
    let mut announced_first_key = false;
    let mut decoder = crate::keyboard::decode::Decoder::new();
    let mut line = TypedLine::new();

    loop {
        let ticks = crate::arch::x86_64::timer_ticks();

        if ticks == 0 {
            if common::arch::x86_64::read_timestamp_counter() - started > FIRST_TICK_TIMEOUT_CYCLES
            {
                logger.error(format_args!(
                    "timer: no tick arrived before the deadline; halting. \
                     Check the PIT divisor write, the IMR (IRQ0 must be unmasked), \
                     and the PIC vector offset (ICW2)"
                ));
                common::arch::x86_64::halt_forever();
            }
            // まだ 1 件も来ていない。`hlt` すると、タイマが動いていない場合に
            // 永久に眠ってしまい上の期限判定へ戻れない。最初の 1 件だけは
            // スピンで待つ。
            core::hint::spin_loop();
            continue;
        }

        if !announced_first {
            announced_first = true;
            // ICW2 の事後証明。実際に届いたベクタ番号を実値で確認する。
            //
            // 8259 の採番と突き合わせる。現在の配送先ではない。**比べるのも表示するのも `arch` である**
            // （[`crate::arch::x86_64::first_tick_arrival`]。ベクタを共通の側に出さない。`ADR-0072` の 3。9e-2）。
            // 記録されているのは「PIC の採番範囲で最初に届いたベクタ」で、定義からして 8259 由来の観測である。
            // S2-d-2 でタイマが Local APIC へ移った後も、移行より前に PIT が動いていた（較正が PIT のティックを
            // 使う）ので値は残っており、ICW2 の事後証明としては依然として有効である。今の配送先と比べると、
            // 移行後に `0x20` と `0xfe` を突き合わせて誤って落ちる。実際に落ちた。
            let first = crate::arch::x86_64::first_tick_arrival();
            if first.is_the_expected_tick() {
                log_both(
                    logger,
                    console.as_deref_mut(),
                    format_args!(
                        "timer: first tick arrived as {first} - this is the \
                         proof that ICW2 was written correctly (it cannot be read back)"
                    ),
                );
            } else if first.none_arrived() && boot_timer_never_ticked {
                // **PIT が刻まない機械では、PIC の割り込みが 1 本も届かない**（HW-c）。
                // **ICW2 の事後証明は取れない。** **止めない**——**言えないことを言えないと
                // 書く**（`sti` 前の項目 4 と同じ立ち位置）。
                logger.info(format_args!(
                    "timer: no PIC interrupt ever arrived because the PIT does not tick on \
                     this machine, so ICW2 has no after-the-fact proof; the timer runs on \
                     the local APIC (the calibration used the ACPI PM timer)"
                ));
            } else {
                logger.error(format_args!(
                    "timer: the first PIC interrupt arrived as {}; the PIC vector offset (ICW2) \
                     is wrong; halting",
                    first.mismatch()
                ));
                common::arch::x86_64::halt_forever();
            }
        }

        let jump = ticks - last_ticks;
        if jump > MAX_TICK_JUMP.load(Ordering::Relaxed) {
            MAX_TICK_JUMP.store(jump, Ordering::Relaxed);
        }
        last_ticks = ticks;

        // === S4-b-2: 共有物を触る区間だけ BKL を保持する ===
        //
        // この 1 周は入口ではない。定常ループはタスクである（ADR-0023
        // Addendum §2）。触る共有物は `SCANCODES`・コンソール・i8042 と PIC の
        // ポート・シリアルの 4 種類で、`hlt` はそのどれにも触らない。
        //
        // ガードのスコープに `hlt` を含めない。含めると、保持したまま眠って
        // もう一方のコアが IF=0 で永久に待つ。構造で起きないようにしてある。
        {
            let _bkl = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);

            // 破壊テスト (S4-b-2, bkl-hold-with-if-set): 保持したまま IF=1 にする。
            // 次のティックで同じコアが irq_entry から取ろうとして再帰検出が発火する。
            #[cfg(feature = "bkl-hold-with-if-set-test")]
            // SAFETY: 破壊テストの feature 専用。BKL を保持している区間である。
            unsafe {
                crate::bkl::sabotage_enable_interrupts_while_held()
            };

            // キーボードのリングバッファを吸い出す。メインループが行う。
            // ハンドラは積むだけで表示しない（ADR-0018 §5）。
            drain_keyboard(
                logger,
                console.as_deref_mut(),
                &mut announced_first_key,
                &mut decoder,
                &mut line,
            );

            // 処理の無い源を禁止したことを 1 度だけ出す（9d-4b。心拍の行には載せない）。
            report_arrivals_without_handler_once(logger);

            if ticks >= next_heartbeat {
                next_heartbeat = ticks + HEARTBEAT_TICKS;
                heartbeats += 1;
                let now_cycles = common::arch::x86_64::read_timestamp_counter();
                let elapsed_ticks = ticks.wrapping_sub(last_heartbeat_ticks);
                let cycles_per_tick = now_cycles
                    .wrapping_sub(last_heartbeat_cycles)
                    .checked_div(elapsed_ticks)
                    .unwrap_or(0);
                last_heartbeat_cycles = now_cycles;
                last_heartbeat_ticks = ticks;
                // **画面へは出さない。シリアルへだけ出す。**
                //
                // **同じ行に読み手が 2 つある**——**検査（`xtask`）はシリアルを読み、
                // 人は画面を見る。** ハートビートが主張するのはタイマ経路の健全性で、
                // **それを確かめるのは検査のほうである。** 画面の側にとっては、
                // **打鍵とシェルの応答の合間に割り込んでくる行でしかない。**
                //
                // **以前は「行が空のときだけ画面にも出す」形だった**——入力中に
                // 割り込むと入力行がぶつ切りになるためで、**その対策は
                // 「画面へ出さない」に含まれる。**
                //
                // **検査の主張は 1 つも変わらない。** `xtask` が見ているのは
                // シリアルのログで、`heartbeat: ticks=` を期待マーカー（9 項目）・
                // 禁止マーカー（4 項目）・本数（`min_heartbeats` と `--shell-test`）
                // として使っているが、いずれもシリアル側である。
                log_both(
                    logger,
                    None,
                    // `heartbeat: ticks=` を行頭に保つ。この部分文字列は xtask の
                    // 期待・禁止マーカーとして 30 箇所近くで使われており、`last_heartbeat_seconds`
                    // が `heartbeat: ticks=256 (2 s), ...` の形を解析している。
                    // S4-a で足す `cpu=` は、その後ろに置く。
                    // AP 側は別の行（`smp: ap heartbeat: cpu=`）なので、この数え上げに混ざらない。
                    format_args!(
                        "heartbeat: ticks={ticks} ({} s), tsc_per_tick={cycles_per_tick}, cpu={}, ap_ticks={}, ticks_total={}, \
                     lapic_timer_deliveries={}, timer_accounting_balanced={}, \
                     max kernel entry depth={}, ap_current={} ap_sched_passes={}, \
                     ipi_sent={} ipi_recv_cpu1={}, tlb_gen={} flush_cpu1={}, \
                     heap_free={} heap_blocks={}, \
                     keys={} dropped={} \
                     stray={} {}, \
                     irq1={} balanced={}, max tick jump={}, i8042 OBF={}, PIC ISR={}, \
                     uart forced={} reentry={}",
                        ticks / crate::machine::pc::irq::timer_frequency_hz() as u64,
                        common::percpu::cpu_id(),
                        ap_tick_summary(),
                        crate::arch::x86_64::timer_ticks_total(),
                        // 合計で閉じる相手である。1 本のティックはどこか 1 コアの
                        // スロットと、このベクタ別カウンタの両方を増やす。
                        crate::arch::x86_64::timer_delivery_count(),
                        crate::arch::x86_64::timer_accounting_balances(),
                        crate::arch::x86_64::max_kernel_entry_depth(),
                        // 「割り当てられた」と「参加した」は別である（S4-c-2、
                        // 観測量は S4-c-3-2a で置き換えた）。占有は `CURRENT` が
                        // 示し、参加は `schedule_switch` を通った回数が示す。
                        // 片方では足りない——占有だけなら「割り当てたが一度も
                        // 通っていない」を通し、参加だけなら「誰の担当か分からない
                        // まま数字が増えている」を通す。
                        //
                        // 前の観測量（アイドルループの反復回数）は捨てた。
                        // 早期リターンを残した構成でも同じように増えるので、
                        // 「参加した」と「従来どおりループしている」を区別
                        // できなかった。
                        crate::task::ap_current_display(),
                        crate::task::ap_schedule_passes(),
                        crate::arch::x86_64::ipi_probe_sent(),
                        crate::arch::x86_64::ipi_probe_received_for(1),
                        crate::bkl::tlb_generation(),
                        crate::bkl::generation_flushes_for(1),
                        // 漂流の観測量（S6-c）。定常状態では動かないはずの量で、
                        // 動いたら「解放されない確保がある」ことになる。
                        //
                        // 専用の出力経路を作らない。既にBKLの内側で出ている
                        // この行へ相乗りする。行を増やすと混線の機会が増える。
                        //
                        // 判定は「全標本が同一であること」である（`docs/verification-coverage.md`）。
                        // 整数なので傾きの推定は要らない。多点の価値は「いつ動いたか」
                        // が特定できることにある。
                        crate::heap::ALLOCATOR.free_bytes(),
                        crate::heap::ALLOCATOR.free_block_count(),
                        crate::keyboard::buffer::received_count(),
                        crate::keyboard::buffer::overflow_count(),
                        crate::keyboard::stray_irq_count(),
                        // `spurious=… lapic_spurious=…`（8259 と Local APIC を分けて数えた観測値。表示は
                        // machine/pc が持つ。2026-09-28 に `idt` から移した）。
                        crate::machine::pc::spurious_counts(),
                        // 会計。irq1 は IDT 側のベクタ別カウンタで、今の配送先のベクタの数である（数えるのは
                        // `arch` の入口。どのベクタかは `machine` が引く。9e）。
                        // keys + stray がこれと一致しなければ経路の取り違えがある。
                        crate::machine::pc::delivered_count(crate::keyboard::interrupt_source()),
                        crate::keyboard::accounting_balances(),
                        max_tick_jump(),
                        // 止まった理由の切り分け材料。キーが来なくなったとき、
                        // OBF が 1 なら「データポートを読んでいない」、
                        // PIC ISR にビットが残っていれば「EOI を送っていない」。
                        // どちらも「1 回動いて止まる」症状になるので、この 2 つが
                        // 無いと区別できない。
                        //
                        // **i8042 が無ければ読まない**（HW-b。探らないと決めた機械で
                        // ポートを叩かない）。
                        if !crate::keyboard::controller_present() {
                            "absent"
                        } else if crate::machine::pc::keyboard_data_ready() {
                            "1"
                        } else {
                            "0"
                        },
                        // SAFETY: メインループは通常文脈で、ここは割り込み禁止中
                        // ではない。i8042/PIC を同時に触りうる別の実行文脈は、この
                        // コアの割り込みハンドラだけである（この関数を走らせるのは
                        // BSP だけで、AP は `smp::ap_heartbeat_loop` へ入る）。ハンドラは
                        // ISR を読んでも元に戻す必要がない読み出し専用の操作しか
                        // しないため、競合しても値がずれるだけで壊れない。
                        // **失効条件は「AP がこの経路へ入るようになるとき」である。**
                        unsafe { crate::machine::pc::irq::service_snapshot() },
                        // **UART のロックの計測（シリアルの排他の段）。**
                        //
                        // **どちらも 0 が正常である。** **`forced` が 0 でなければ
                        // 上限か設計を見直す材料になり、`reentry` が 0 でなければ
                        // 「割り込みハンドラは何も出力しない」が破られている。**
                        // **あの規約は検査されていない**ので、ここが事後の観測になる。
                        //
                        // **判定にしない。** **揺れる値なので、揺れる行へ相乗りする**
                        // （この行は `BOOT_LOG_VOLATILE_MARKERS` に在る）。
                        common::machine::pc::serial::forced_write_count(),
                        common::machine::pc::serial::reentry_count()
                    ),
                );
            }
        }
        // ← ここで BKL を離す。`hlt` はこの外にある。

        // **シェルへ渡す（S11-11）。** 定常ループの観測はここで完了する。
        //
        // **ティック数ではなくハートビートの本数で決める。** ティックの閾値だと
        // **越えた時点で何本出ているかが揺れる**——`hlt` から起きた時点で数えるので、
        // **`-smp 1` と `-smp 2` で行数が 1 本ずれた**（実測）。
        // **本数で決めれば、どの構成でも同じ本数だけ出る。**
        //
        // **割り込みは止めない。** シェルはキーボードの割り込みで動く。
        // **戻らない**——`kernel_main` が `init` を走らせ、そちらが `-> !` である。
        //
        // **なぜここで完了するのか。** ハートビートは**タイマ経路の健全性**を見る
        // もので、**シェルとは別の主張である。** シェルが定期的に出す形にすると
        // **シェルの都合で観測の頻度が変わる。** ここまでで十分な回数のティックを
        // 観測してあるので、**最後の 1 本を出して完了する。**
        if stop_after_ticks == 0
            && shell_after_heartbeats != 0
            && heartbeats >= shell_after_heartbeats
        {
            // **揺れる値をこの行へ載せない**（S9-b-3-1 で決めた形）。
            // **観測したティック数は起動ごとに違う**——`hlt` から起きた時点で
            // 数えるので、**どこで閾値を越えるかが揺れる**（実測で 256 と 259）。
            // **数はハートビートの行が出している。** ここが主張するのは
            // **「会計が合ったまま定常ループを抜ける」**ことだけである。
            // **両方のコアの観測を完了する。** AP のハートビートも止まる。
            close_steady_observation();
            logger.info(format_args!(
                "timer: this is the end of the steady-loop observation, \
                 timer_accounting_balanced={}; the shell takes the foreground from here",
                crate::arch::x86_64::timer_accounting_balances()
            ));
            return;
        }

        if stop_after_ticks != 0 && ticks >= stop_after_ticks {
            logger.info(format_args!(
                "timer: reached the tick limit ({stop_after_ticks}); leaving the loop"
            ));
            // SAFETY: 観測が終わったので、割り込みを禁止した既知の状態へ戻す。
            unsafe {
                common::arch::x86_64::disable_interrupts();
            }
            return;
        }

        // 破壊テスト (S4-b-2, bkl-hold-across-hlt): 離さずに `hlt` する。
        // もう一方のコアが IF=0 で待ち続け、タイムアウトして原因を出す。
        // 「静かに止まる」を「うるさく止まる」へ変えた形の実証である。
        #[cfg(feature = "bkl-hold-across-hlt-test")]
        let _bkl_held_across_halt = crate::bkl::acquire(crate::bkl::KernelEntry::SteadyLoop);

        // 次のティックまで眠る。`sti` は既に効いているが、
        // `enable_interrupts_and_wait` を使うことで `sti; hlt` の隣接が
        // 常に保たれる（M5 で条件つきの形へ移す際もここを変えずに済む）。
        //
        // SAFETY: ハンドラは用意済みで、EOI も発行している。
        unsafe {
            common::arch::x86_64::enable_interrupts_and_wait();
        }
    }
}

/// AP のティック数を 1 つの表示へまとめる（S4-a）。
///
/// BSP 自身のぶんは含めない。ハートビートの `ticks=` が既に BSP のぶんで、
/// 同じ数を 2 度出すと、どちらが合計かが読めなくなる。
struct ApTickSummary;

impl core::fmt::Display for ApTickSummary {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        for cpu in 1..common::percpu::MAX_CPUS {
            if !first {
                write!(f, " ")?;
            }
            first = false;
            write!(f, "cpu{cpu}={}", crate::arch::x86_64::timer_ticks_for(cpu))?;
        }
        if first {
            write!(f, "none")?;
        }
        Ok(())
    }
}

/// [`ApTickSummary`] を作る。
const fn ap_tick_summary() -> ApTickSummary {
    ApTickSummary
}

/// シリアルと画面の両方へ 1 行出す。必ずシリアルを先に書く
/// （architecture.md §6.7）。
/// シリアルの排他の演習（BSP 側）。
///
/// **合図を立て、AP と同時に既知の行を書き、AP が書き終えるのを待つ。**
/// **待つのは、演習の途中でこの先の起動シーケンスが混ざらないようにするため
/// である**——**混ざると、判定が「錠が効いていない」と「起動の行が挟まった」を
/// 区別できなくなる。**
#[cfg(feature = "serial-stress-test")]
fn run_serial_stress_on_bsp(logger: &mut Logger<Serial>) {
    use core::sync::atomic::Ordering;

    crate::smp::SERIAL_STRESS_GO.store(true, Ordering::Release);
    for index in 0..crate::smp::SERIAL_STRESS_LINES {
        logger.info(format_args!(
            "serial-stress: cpu0 {index:04} {}",
            crate::smp::SERIAL_STRESS_PADDING
        ));
    }
    // **上限つきで待つ。**
    let started = common::arch::x86_64::read_timestamp_counter();
    while !crate::smp::SERIAL_STRESS_AP_DONE.load(Ordering::Acquire) {
        if common::arch::x86_64::read_timestamp_counter().wrapping_sub(started) > 20_000_000_000 {
            logger.error(format_args!(
                "serial-stress: the application processor never finished; the exercise asserts \
                 nothing"
            ));
            return;
        }
        core::hint::spin_loop();
    }
    // **計測（判定にしない）。** **揺れる値なので `(info)` の側である。**
    logger.info(format_args!(
        "serial-stress: done; the uart lock was forced {} time(s) and re-entered on the same \
         core {} time(s)",
        common::machine::pc::serial::forced_write_count(),
        common::machine::pc::serial::reentry_count()
    ));
}

fn log_both(
    logger: &mut Logger<Serial>,
    console: Option<&mut crate::console::Console>,
    args: core::fmt::Arguments,
) {
    logger.info(args);
    if let Some(console) = console {
        use core::fmt::Write as _;
        let _ = writeln!(console, "[INFO] {args}");
    }
}

/// リングバッファを吸い出し、生のスキャンコードをシリアルへ出す。
///
/// 段階 4（ハードウェア接続の確認）で最も重要な出力である。変換もエコーも
/// せず受け取ったバイトをそのまま 16 進で出すので、ここが出ていれば
/// 「割り込みが届き、データポートが読めている」ことが確定する。以降の不具合は
/// すべてデコード側の問題に絞り込める。
fn drain_keyboard(
    logger: &mut Logger<Serial>,
    console: Option<&mut crate::console::Console>,
    announced_first: &mut bool,
    decoder: &mut crate::keyboard::decode::Decoder,
    line: &mut TypedLine,
) {
    use crate::keyboard::decode::KeyEvent;

    let mut console = console;

    // **前景を Ring 3 が持っているなら、こちらは取り出さない（S11-10）。**
    //
    // **入力の消費者は同時に 1 つである**（`crate::input` の不変条件）。
    // **リングは取り出したら消える**ので、2 人が取ると**どちらも全部は見ない。**
    //
    // **積むのは止めない。** 割り込みハンドラはそのままリングへ積み、
    // **前景が戻ったときに、溜まっていたぶんをこちらが読む。**
    if crate::input::foreground_is_claimed() {
        return;
    }

    loop {
        // ロックは 1 バイトごとに取って離す。保持したままログを出さない。
        // ログ出力は長く、その間ずっと割り込みが禁止されるとティックを
        // 取りこぼす。
        let code = {
            let mut ring = crate::keyboard::buffer::SCANCODES.lock();
            ring.pop()
        };
        let Some(code) = code else {
            return;
        };

        if !*announced_first {
            *announced_first = true;
            // IRQ1 の配送経路の証明。タイマで 0x20 を確認したのと同じ趣旨。**プログラムを起動する入口と
            // 同じ関数で 1 度だけ出す**（HW-e-2。`crate::keyboard::report_first_delivery_once`）。
            crate::keyboard::report_first_delivery_once(logger);
        }

        // 生のスキャンコード。押下と離脱で 2 回出るので、エコーと二重に
        // なって読みにくい。既定では出さず、切り分けが要るときだけ
        // `keyboard-raw-log` feature で有効にする。
        #[cfg(feature = "keyboard-raw-log")]
        logger.info(format_args!("keyboard: scancode {code:#04x}"));

        let Some(event) = decoder.feed(code) else {
            continue;
        };

        match event {
            KeyEvent::Char(character) => {
                line.push(character);
                echo(console.as_deref_mut(), character);
            }
            KeyEvent::Enter => {
                echo(console.as_deref_mut(), '\n');
                // 1 行分をまとめて出す。自動テストはこの行を突き合わせる。
                logger.info(format_args!("keyboard: line = \"{}\"", line.as_str()));
                line.clear();
            }
            // **カーソルの矢印と Esc は、この行では何もしない（S12 前の手当て。
            // zi-a で上下と Esc が増えた）。**
            //
            // **ここの行（`TypedLine`）は挿入点を持たない。** 前景が取られる前の
            // 診断用で、**編集を持つのはシェルの側である**（`kernel/userland/zash.rs`）。
            // **前景が取られている間、この経路は動かない。**
            KeyEvent::ArrowLeft
            | KeyEvent::ArrowRight
            | KeyEvent::ArrowUp
            | KeyEvent::ArrowDown
            | KeyEvent::Escape
            // **Delete も何もしない（zi-f）。** **この行は挿入点を持たない**
            // ——`Backspace` と同じ理由で、編集は Ring 3 の側にある。
            | KeyEvent::Delete
            // **Home と End も同じである（SE-a。`ADR-0050`）。**
            | KeyEvent::Home
            | KeyEvent::End => {}
            KeyEvent::Backspace => {
                // 画面上の消去は行わない。コンソール側でセルごとの
                // 占有種別（全角の先頭 / 後続）を管理する必要があり、
                // 割り込みとは別の仕事になる（`docs/deferred-decisions.md`）。
                // キーとして認識していることだけ示す。
                logger.info(format_args!(
                    "keyboard: backspace (not applied to the screen yet)"
                ));
            }
            KeyEvent::Unsupported(code) => {
                UNSUPPORTED_KEYS.fetch_add(1, Ordering::Relaxed);
                logger.info(format_args!("keyboard: unsupported scancode {code:#04x}"));
            }
        }
    }
}

/// 対応していないキーを受けた回数。
static UNSUPPORTED_KEYS: AtomicU64 = AtomicU64::new(0);

pub fn unsupported_key_count() -> u64 {
    UNSUPPORTED_KEYS.load(Ordering::Relaxed)
}

/// 入力された文字を画面へ出す。
///
/// メインループから呼ぶ（ADR-0018 §5）。ハンドラからは呼ばない。
///
/// # シリアルへは 1 文字ずつ出さない
///
/// シリアルへ 1 文字ずつ流すには `Logger` の内側の `Serial` を直接
/// 触る必要がある。そのためのアクセサを `Logger` に足すと、レベル判定と
/// 接頭辞の書式を迂回する経路を全利用者に開くことになる。ADR-0017
/// Addendum の反省（守るべき制約と、たまたま採った手段を混同しない）に
/// 照らして、ここは足さない。
///
/// 代わりにシリアルへは Enter のときに 1 行としてまとめて出す。1 文字ずつの
/// 追跡が要る場合は `keyboard-raw-log` feature で生スキャンコードを出す。
fn echo(console: Option<&mut crate::console::Console>, character: char) {
    use core::fmt::Write as _;

    if let Some(console) = console {
        let _ = write!(console, "{character}");
    }
}

/// 打ち込んだ 1 行を貯める固定長バッファ。
///
/// ヒープを使わない。長さを超えた分は捨てる（入力行が異常に長いのは
/// テストの想定外で、捨てても診断に影響しない）。
struct TypedLine {
    buffer: [u8; Self::CAPACITY],
    len: usize,
}

impl TypedLine {
    const CAPACITY: usize = 128;

    const fn new() -> Self {
        Self {
            buffer: [0; Self::CAPACITY],
            len: 0,
        }
    }

    fn push(&mut self, character: char) {
        // ASCII だけを貯める。現在のデコーダは ASCII しか返さない。
        if character.is_ascii() && self.len < Self::CAPACITY {
            self.buffer[self.len] = character as u8;
            self.len += 1;
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn as_str(&self) -> &str {
        // SAFETY: push で ASCII だけを入れているため、常に有効な UTF-8。
        core::str::from_utf8(&self.buffer[..self.len]).unwrap_or("<invalid>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 試験の中で源の番号を作る（本物は `machine` が作る）。
    fn source(index: u8) -> InterruptSource {
        InterruptSource::assigned_by_machine(index)
    }

    #[test]
    fn a_registered_handler_is_found_by_its_source() {
        static ARRIVED: AtomicU64 = AtomicU64::new(0);
        fn record(source: InterruptSource) {
            ARRIVED.store(u64::from(source.table_index()) + 100, Ordering::Relaxed);
        }
        let table = HandlerTable::new();
        assert_eq!(table.register(source(1), record), Ok(()));
        assert!(table.handler(source(2)).is_none());
        let handler = table
            .handler(source(1))
            .expect("the handler registered for source 1");
        handler(source(1));
        assert_eq!(ARRIVED.load(Ordering::Relaxed), 101);
    }

    #[test]
    fn registration_is_refused_after_the_table_is_closed() {
        fn ignore(_source: InterruptSource) {}
        let table = HandlerTable::new();
        assert_eq!(table.register(source(1), ignore), Ok(()));
        let _ = table.close();
        assert_eq!(table.register(source(11), ignore), Err(Refusal::AfterBoot));
        assert!(table.handler(source(1)).is_some());
        assert!(table.handler(source(11)).is_none());
    }

    #[test]
    fn a_second_handler_for_the_same_source_is_refused() {
        static FIRST_CALLED: AtomicBool = AtomicBool::new(false);
        fn first(_source: InterruptSource) {
            FIRST_CALLED.store(true, Ordering::Relaxed);
        }
        fn second(_source: InterruptSource) {}
        let table = HandlerTable::new();
        assert_eq!(table.register(source(11), first), Ok(()));
        assert_eq!(
            table.register(source(11), second),
            Err(Refusal::AlreadyRegistered)
        );
        table.handler(source(11)).expect("the first handler stays")(source(11));
        assert!(FIRST_CALLED.load(Ordering::Relaxed));
    }

    #[test]
    fn a_source_outside_the_table_is_refused() {
        fn ignore(_source: InterruptSource) {}
        let table = HandlerTable::new();
        assert_eq!(
            table.register(source(16), ignore),
            Err(Refusal::OutsideTable)
        );
        assert!(table.handler(source(16)).is_none());
    }

    #[test]
    fn closing_lists_the_sources_with_a_handler() {
        fn ignore(_source: InterruptSource) {}
        assert_eq!(format!("{}", HandlerTable::new().close()), "none");
        let table = HandlerTable::new();
        assert_eq!(table.register(source(11), ignore), Ok(()));
        assert_eq!(table.register(source(1), ignore), Ok(()));
        assert_eq!(format!("{}", table.close()), "1, 11");
    }

    #[test]
    fn the_sources_can_be_listed_without_closing() {
        fn ignore(_source: InterruptSource) {}
        let table = HandlerTable::new();
        assert_eq!(table.register(source(11), ignore), Ok(()));
        assert_eq!(table.register(source(1), ignore), Ok(()));
        assert_eq!(table.sources().as_slice(), &[source(1), source(11)]);
        assert_eq!(table.register(source(5), ignore), Ok(()));
        assert_eq!(
            table.close().as_slice(),
            &[source(1), source(5), source(11)]
        );
    }
}
