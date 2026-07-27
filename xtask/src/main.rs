use std::{
    env,
    ffi::OsString,
    fs,
    io::Write,
    os::unix::{ffi::OsStrExt, net::UnixStream},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};

mod font;

const OVMF_CODE_PATH: &str = "/usr/share/OVMF/OVMF_CODE_4M.fd";
const OVMF_VARS_TEMPLATE_PATH: &str = "/usr/share/OVMF/OVMF_VARS_4M.fd";
const BOOTLOADER_PACKAGE: &str = "bootloader";
const UEFI_TARGET: &str = "x86_64-unknown-uefi";
const KERNEL_PACKAGE: &str = "kernel";
const KERNEL_TARGET: &str = "x86_64-unknown-none";
const PANIC_TEST_FEATURE: &str = "panic-test";
/// kernel 側の feature。有効にすると M3-a の描画テストパターンを描き、
/// コンソールを起動しない（ADR-0017）。
const GFX_TEST_PATTERN_FEATURE: &str = "gfx-test-pattern";

/// 例外ハンドラの回帰チェック（`--exception-test <kind>`）。
///
/// 起動完了後に意図した例外をわざと発生させ、ハンドラが呼ばれることを
/// 確認する。**必ず TCG で実行する。** KVM では `-d int` に何も残らず、
/// `v=..` の突き合わせができないため（troubleshooting.md 参照）。
struct ExceptionTest {
    /// `--exception-test` に渡す名前。
    name: &'static str,
    /// kernel 側で有効化する feature。
    feature: &'static str,
    /// 期待するベクタ番号。
    vector: u8,
    /// `qemu-debug.log` に現れる目印（`-d int` の出力形式）。
    qemu_marker: &'static str,
    /// シリアルログに現れることを追加で要求する文字列。
    extra_serial_markers: &'static [&'static str],
    /// 汎用レジスタの並び順を既知の値で突き合わせるか。
    check_registers: bool,
    /// `CPU Reset` の回数が起動時の 2 回から増えていないことを確認するか。
    /// ダブルフォルトがトリプルフォルトへ落ちていないことの確認に使う。
    check_no_extra_cpu_reset: bool,
}

/// `--exception-test invalid-opcode` が例外の直前に各 GPR へ入れる既知の値。
///
/// kernel 側の `known_register_values` と一致していなければならない。
/// スタブの push 順と `ExceptionContext` のフィールド順が食い違うと、
/// ダンプは出るのに名前と値の対応だけが入れ替わる。値がもっともらしいため
/// 気づきにくいので、レジスタごとに異なる値を入れて突き合わせる。
const KNOWN_REGISTERS: &[(&str, &str)] = &[
    ("rax", "0x1111111111111111"),
    ("rbx", "0x2222222222222222"),
    ("rcx", "0x3333333333333333"),
    ("rdx", "0x4444444444444444"),
    ("rsi", "0x5555555555555555"),
    ("rdi", "0x6666666666666666"),
    ("rbp", "0x7777777777777777"),
    ("r8", "0x8888888888888888"),
    ("r9", "0x9999999999999999"),
    ("r10", "0xaaaaaaaaaaaaaaaa"),
    ("r11", "0xbbbbbbbbbbbbbbbb"),
    ("r12", "0xcccccccccccccccc"),
    ("r13", "0xdddddddddddddddd"),
    ("r14", "0xeeeeeeeeeeeeeeee"),
    ("r15", "0xffffffffffffffff"),
];

const EXCEPTION_TESTS: &[ExceptionTest] = &[
    ExceptionTest {
        name: "divide-by-zero",
        feature: "exception-test-divide-by-zero",
        vector: 0,
        qemu_marker: "v=00",
        extra_serial_markers: &["error code = (none for this exception)"],
        check_registers: false,
        check_no_extra_cpu_reset: false,
    },
    ExceptionTest {
        name: "invalid-opcode",
        feature: "exception-test-invalid-opcode",
        vector: 6,
        qemu_marker: "v=06",
        extra_serial_markers: &[],
        check_registers: true,
        check_no_extra_cpu_reset: false,
    },
    ExceptionTest {
        name: "page-fault",
        feature: "exception-test-page-fault",
        vector: 14,
        qemu_marker: "v=0e",
        extra_serial_markers: &[
            // フォルトしたアドレスが CR2 から取れていること。
            "cr2=0x0000400000000000 (faulting address)",
            // エラーコードが人間に読める形へ展開されていること。
            "cause=page not present access=read mode=supervisor",
        ],
        check_registers: false,
        check_no_extra_cpu_reset: false,
    },
    ExceptionTest {
        name: "double-fault",
        feature: "exception-test-double-fault",
        vector: 8,
        qemu_marker: "v=08",
        extra_serial_markers: &[
            // IST1 へ切り替わっていること。切り替わっていなければ
            // 壊れている可能性のあるスタックの上でハンドラが動いている。
            "on IST1=true",
            // #DF のエラーコードは常に 0。
            "always zero for #DF",
        ],
        check_registers: false,
        // IST が効いていなければトリプルフォルトになり、CPU Reset が増える。
        check_no_extra_cpu_reset: true,
    },
];

/// 起動時に必ず記録される `CPU Reset` の回数（電源投入シーケンス、
/// `docs/troubleshooting.md` 参照）。これを超えたらリセットが起きている。
const EXPECTED_CPU_RESET_COUNT: usize = 2;

const EXCEPTION_TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// クリティカルセクションとロックの回帰チェック（`--critical-test <kind>`）。
///
/// 検出の仕組みを入れても、それが機能しなければ意味がない。わざと異常経路を
/// 踏ませて、検出が働くことを確かめる（M4-b-1 のスタブ検証と同じ発想）。
struct CriticalTest {
    name: &'static str,
    feature: &'static str,
    /// シリアルログに現れることを要求する文字列。
    expected_markers: &'static [&'static str],
    /// 現れてはいけない文字列（検出をすり抜けたことを示すもの）。
    forbidden_markers: &'static [&'static str],
    /// 期待マーカーが出た時点で打ち切らず、必ずタイムアウトまで待つか。
    ///
    /// **「起きないこと」を確かめるテストで必要になる。** EOI を落とした
    /// ビルドはティック 1 回で `hlt` に入ったまま二度と起きない。カーネルは
    /// 失敗を報告することすらできない（報告するコードが動かない）ので、
    /// 「一定時間待ってもハートビートが 1 本も出ないこと」でしか判定できない。
    /// 早期に打ち切ると「まだ出ていないだけ」と区別がつかない。
    wait_for_full_timeout: bool,
    /// シリアルログに現れるべきハートビート行の最低数。
    ///
    /// `hlt` で眠ったまま起きなくなる「完全停止」は、カーネル内部からは
    /// 検出できない。外側からハートビートの本数を数えるのが唯一の手段。
    min_heartbeats: Option<usize>,
}

const CRITICAL_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "double-lock",
        feature: "critical-test-double-lock",
        expected_markers: &[
            "lock: double acquisition detected",
            "halting (cli + hlt loop)",
        ],
        // 検出をすり抜けて 2 回目の lock() が戻ってきた場合に出る行。
        forbidden_markers: &["double-lock detection FAILED"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // IF=1 の状態から InterruptGuard に入り、抜けたときに復元されることを
    // 確認する。IF=0 から入る経路は通常の起動ログで毎回通っているが、
    // 「保存値が IF=1 のときだけ sti する」という分岐の片側はここでしか
    // 通らない。一時的に sti するため、PIC を全マスクした M4-c-3 の後に
    // 実行する。
    CriticalTest {
        name: "restore-enabled",
        feature: "critical-test-restore-enabled",
        expected_markers: &[
            "critical-test: restore path OK",
            "critical-test: IF after sti = true",
            "critical-test: IF inside the guard = false",
            "critical-test: IF after the guard dropped = true",
        ],
        forbidden_markers: &["critical-test: restore path FAILED"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// 割り込みを有効化する経路の回帰チェック（`--interrupt-test <kind>`）。
///
/// 構造は [`CriticalTest`] と同じ。判定に必要なのは「出るべき行」と
/// 「出てはいけない行」の 2 つだけなので、型を分けずに使い回す。
const INTERRUPT_TESTS: &[CriticalTest] = &[
    // 全 IRQ をマスクしたまま sti し、何も届かないことを確認する。
    // 「回っているが割り込みが来ない」と「そもそも回っていない」を
    // 区別するため、周回回数が 0 でないことも見る。
    CriticalTest {
        name: "enable-only",
        feature: "interrupt-test-enable-only",
        expected_markers: &[
            "interrupt-test: sti-then-idle OK",
            "sti: interrupts are now enabled (IF=true",
            "heartbeat: loop iterations=",
            "sti-check summary: 4. PIC remapped to 0x20-0x2F = UNVERIFIABLE",
        ],
        forbidden_markers: &[
            "sti-then-idle FAILED",
            "the loop never iterated",
            "an interrupt arrived while every IRQ is masked",
            "refusing to sti",
            "stack alignment:",
            "exception: vector=",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // int 0x40 をソフトウェア発行し、IRQ 経路が GPR を復元することを確認。
    CriticalTest {
        name: "irq-path",
        feature: "interrupt-test-irq-path",
        expected_markers: &[
            "irq-path: OK (the IRQ stub returned via iretq",
            "irq-path: int 0x40 handled (handler count for vector 0x40 = 1)",
        ],
        forbidden_markers: &[
            "irq-path: a general purpose register was not restored",
            "stack alignment:",
            "exception: vector=",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // タイマを実際に動かす（M4-d-2）。ティックが増え続けることが EOI の
    // 動作証明になる。
    CriticalTest {
        name: "timer",
        feature: "interrupt-test-timer",
        expected_markers: &[
            "interrupt-test: timer OK",
            // ICW2 の事後証明。M4-c-3 で未検証のまま残した論点が閉じる。
            // これは起動しきったことの目印でもある。
            KERNEL_BOOT_COMPLETE_MARKER,
            "pic: IMR after unmasking IRQ0 master=0xfe slave=0xff",
            "heartbeat: ticks=",
        ],
        // 500 ティックで止めるので 100 ティックごとのハートビートが 5 本。
        // 4 本以上出ていれば、ループは最後まで起き続けている。
        wait_for_full_timeout: false,
        min_heartbeats: Some(4),
        forbidden_markers: &[
            "interrupt-test: timer FAILED",
            "no tick arrived before the deadline",
            "the PIC vector offset (ICW2) is wrong",
            "configuring the PIT changed the interrupt mask",
            "stack alignment:",
            "exception: vector=",
        ],
    },
    // EOI をわざと落とし、ティックが 1 回で止まることを確認する。
    // **検証が実際に機能していることの確認**なので、期待する結果は失敗側。
    CriticalTest {
        name: "no-eoi",
        feature: "no-eoi-test",
        // **カーネルは失敗を報告できない。** EOI を送らないとティック 1 回で
        // `hlt` に入ったまま二度と起きず、判定コードに到達しないためである。
        // これは「無条件 hlt のメインループは完全停止を自己検出できない」と
        // いう既知の限界そのものであり、その限界を逆手に取った検証になる。
        // 期待するのは「最初のティックまでは確かに届いたこと」だけで、
        // その後止まったことは forbidden 側で外から見る。
        expected_markers: &[
            KERNEL_BOOT_COMPLETE_MARKER,
            "pit: channel 0 set to divisor=11932",
        ],
        // 2 回目以降が来ていればハートビートが出る。1 本でも出たら、
        // EOI 無しでもティックが続いたことになり検証が成立しない。
        forbidden_markers: &["interrupt-test: timer OK", "heartbeat: ticks="],
        // 「出ないこと」の確認なので早期に打ち切らず、最後まで待つ。
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // PIC をわざと 0x30-0x3F へ再マップし、ティックがそのベクタで届くことを
    // 確認する。**M4-c-3 で残した申し送りを閉じるためのテストである。**
    // 通常構成の 0x20 は OVMF が既に使っていた可能性が高く、こちらの ICW2 が
    // 誤っていても動いてしまいうる。別の値で届けばその疑いが晴れると同時に、
    // 配送元が LAPIC ではなく 8259A であることも示せる（LAPIC 経由なら
    // ベクタは移動しない）。
    CriticalTest {
        name: "alt-offset",
        feature: "alt-offset-test",
        expected_markers: &[
            "interrupt-test: timer OK",
            // ここが 0x20 のままなら、再マップが効いていないか LAPIC 由来。
            "timer: first tick arrived as vector 0x30",
            "pic: IMR after unmasking IRQ0 master=0xfe slave=0xff",
        ],
        forbidden_markers: &[
            "interrupt-test: timer FAILED",
            "no tick arrived before the deadline",
            "the PIC vector offset (ICW2) is wrong",
            "timer: first tick arrived as vector 0x20",
            "stack alignment:",
            "exception: vector=",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: Some(4),
    },
    // 境界調整をわざと外し、境界検証が働くことを確認する。
    // **検証が壊れていないことを確かめるためのテストなので、期待する結果は
    // 「検出して停止する」である。**
    CriticalTest {
        name: "misaligned",
        feature: "misalign-test,interrupt-test-irq-path",
        expected_markers: &[
            "stack alignment:",
            "violated the SysV ABI",
            "halting (cli + hlt loop)",
        ],
        forbidden_markers: &["irq-path: OK"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// ページテーブルの分割・アンマップの回帰チェック（`--paging-test <kind>`）。
///
/// いずれも「検査が実際に働くこと」を確かめる。正常系は通常起動の
/// `split-test:` 行が毎回見ているので、ここには置かない。
const PAGING_TESTS: &[CriticalTest] = &[
    // 正しい実装で、PCD 付きの 2MiB ページを分割しても属性が残ること。
    // わざと壊す側（drop-pcd）と対にして初めて意味を持つ。
    CriticalTest {
        name: "pcd",
        feature: "paging-test",
        expected_markers: &[
            "paging-test: scratch",
            "paging-test: PCD survived the split on all 512 entries = OK",
            "paging-test: done",
        ],
        forbidden_markers: &["PCD was lost", "exception: vector="],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 分割時に PCD を落とす。読み戻し照合が不一致を検出すること。
    CriticalTest {
        name: "drop-pcd",
        feature: "paging-test-drop-pcd",
        expected_markers: &["paging-test: PCD was lost on 512 of 512 entries after the split = NG"],
        forbidden_markers: &["PCD survived the split"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // PD エントリの差し替えを先に行い、中間状態を意図的に踏む。
    // **#PF が起きること**と、CR2 が踏んだアドレスを指すことを見る。
    CriticalTest {
        name: "wrong-order",
        feature: "paging-test-wrong-order",
        expected_markers: &["exception: vector=14", "halting (cli + hlt loop)"],
        forbidden_markers: &["split-test: split and unmap behave as planned"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // アンマップの添字を間違える。対象が生きたまま別のページが消えること。
    CriticalTest {
        name: "bad-index",
        feature: "paging-test-bad-index",
        expected_markers: &["split-test: the split/unmap round trip did not behave as planned"],
        forbidden_markers: &["split-test: split and unmap behave as planned"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // アンマップしたページを読む。正しい実装では #PF（vector=14）になる。
    CriticalTest {
        name: "unmap-fault",
        feature: "paging-test-unmap-fault",
        expected_markers: &[
            "paging-test: about to read the unmapped page",
            "exception: vector=14",
            "halting (cli + hlt loop)",
        ],
        forbidden_markers: &["the read did NOT fault"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // invlpg を落とす。**アンマップ前に触ってあるので TLB にエントリが
    // ある。** それが残っているとフォルトせずに古い値が読める。
    // unmap-fault と対にして初めて「invlpg が効いている」と言える。
    //
    // 対象ページをアンマップ前に触らないと、そもそも TLB エントリが存在せず
    // invlpg の有無が結果に現れない。最初にそう書いてしまい、両方とも #PF に
    // なって差が出なかった。
    CriticalTest {
        name: "no-invlpg",
        feature: "paging-test-no-invlpg",
        expected_markers: &[
            "paging-test: about to read the unmapped page",
            "paging-test: the read did NOT fault",
            "paging-test: done",
        ],
        forbidden_markers: &["exception: vector=14"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 稼働中のヒープが載る 2MiB ページを、使いながら分割する。
    CriticalTest {
        name: "split-heap",
        feature: "paging-test-split-heap",
        expected_markers: &["paging-test: split the live heap page", "paging-test: done"],
        forbidden_markers: &["exception: vector="],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // A-1 の direct map 窓を高位ではなく低位で張る。独立 walker が高位窓の
    // 不在を検出し、CR3 を切り替えずに止まること。壊れていなければ
    // `direct-map: verified` まで進むので、それを forbidden にして対にする。
    CriticalTest {
        name: "directmap-low-window",
        feature: "paging-test-directmap-low-window",
        expected_markers: &["does not resolve: NotPresent", "refusing to switch CR3"],
        forbidden_markers: &[
            "direct-map: verified",
            "direct-map: CR3 switch instruction executed",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // A-2 の登録窓の base を 1 ページずらす。差し替え直後の phys_to_virt 検証が
    // 食い違いを検出し、フレームバッファ・コンソールの高位アクセスへ進む前に
    // 止まること。壊れていなければ登録窓が有効になるので、それを forbidden に
    // して対にする。
    CriticalTest {
        name: "directmap-wrong-base",
        feature: "paging-test-directmap-wrong-base",
        expected_markers: &["phys_to_virt", "the registered window is wrong"],
        forbidden_markers: &["direct-map A-2: registered window active"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// カーネルスタックのガードページの回帰チェック（`--stack-test <kind>`、M5-b）。
const STACK_TESTS: &[CriticalTest] = &[
    // スタックを溢れさせ、ガードページに触れた #PF が IST2 上で、CR2 =
    // ガードページとして報告されること。#DF へ昇格しないこと。
    CriticalTest {
        name: "guard",
        feature: "stack-guard-test",
        expected_markers: &[
            "exception: vector=14 (#PF",
            "cr2 is in the kernel stack guard page = true",
            "on IST2=true",
        ],
        forbidden_markers: &["exception: vector=8"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // #PF に IST を与えず、溢れ → 壊れたスタック上の #PF → #DF の本来の連鎖で
    // ダブルフォルトが出ること。#PF が IST2 上で完結しないこと。
    CriticalTest {
        name: "df",
        feature: "stack-overflow-df-test",
        expected_markers: &["exception: vector=8 (#DF", "on IST1=true"],
        forbidden_markers: &["on IST2=true"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// 協調的コンテキストスイッチの回帰チェック（`--task-test <kind>`、M5-c）。
const TASK_TESTS: &[CriticalTest] = &[
    // スイッチで次タスクの rbx を壊す。復帰したタスクが GPR 照合で検出する。
    CriticalTest {
        name: "drop-reg",
        feature: "task-switch-drop-reg",
        expected_markers: &["GPR(s) corrupted across the switch", "halting"],
        forbidden_markers: &["cooperative switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // RSP の差し替えを省く。スイッチが起きず、ワーカーが走らないまま会計が
    // 合わないことを検出する。
    CriticalTest {
        name: "no-swap",
        feature: "task-switch-no-swap",
        expected_markers: &["accounting did not balance"],
        forbidden_markers: &["cooperative switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // InterruptGuard 保持中に yield を呼ぶ。on_yield のガードが fail-fast する。
    CriticalTest {
        name: "in-critical",
        feature: "task-switch-yield-in-critical",
        expected_markers: &[
            "yield called while holding a Locked/InterruptGuard",
            "halting",
        ],
        forbidden_markers: &["cooperative switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // プリエンプティブスイッチで RSP0 の更新を落とす。M5-c の RSP0 読み戻し
    // 検査が食い違いを検出して halt する。
    CriticalTest {
        name: "drop-rsp0",
        feature: "task-switch-drop-rsp0",
        expected_markers: &["TSS.RSP0 readback", "halting"],
        forbidden_markers: &["preemptive switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // プリエンプト窓を広げると窓カウントが増えること（統計的検証の判定が働く
    // ことの裏）。窓カウント > 0 で verified まで進めば OK。窓が広い分だけ
    // カウントは normal より増える（値は非決定的なので verified の到達で見る）。
    CriticalTest {
        name: "widen-window",
        feature: "task-widen-preempt-window",
        expected_markers: &["preempts in the GPR window=", "preemptive switch verified"],
        forbidden_markers: &["no preemption landed in the GPR window"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // InterruptGuard の cli を落とし、防御スキップも外す。Locked 保持中に timer が
    // プリエンプトして別ワーカーが同じ Locked を取り、二重取得検出が発火する。
    CriticalTest {
        name: "preempt-in-critical",
        feature: "task-preempt-in-critical",
        expected_markers: &["double acquisition detected", "halting"],
        forbidden_markers: &["preemptive switch verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// Ring 3 遷移の破壊確認（M5-e-4）。いずれも遠征が verified に到達しないことを
/// 確かめる。
const RING3_TESTS: &[CriticalTest] = &[
    // ucode64 の DPL を 0 にする。M5-e-1 の読み戻しアサートは cfg で外してあり、
    // iretq 自身が #GP になる（Ring 3 に落ちない。フォルト元 Ring 0 なので畳まれない）。
    CriticalTest {
        name: "user-desc-dpl0",
        feature: "ring3-test-user-desc-dpl0",
        expected_markers: &["exception: vector=13", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // ユーザーページの USER を落とす。遠征前の両側 U/S 監査が捕まえる。
    CriticalTest {
        name: "user-page-supervisor",
        feature: "ring3-test-user-page-supervisor",
        expected_markers: &["U/S audit before the excursion failed", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 遠征の RSP0 据え付けを落とす。#GP がメインのスタックで走り、
    // handler_in_excursion が false になって捕まる。
    CriticalTest {
        name: "drop-rsp0",
        feature: "ring3-test-drop-rsp0",
        expected_markers: &["RSP0 did not take effect", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 遠征フラグを立てない。cli の #GP が畳まれず dump+halt する（フォルト RIP が
    // ユーザーコード入口なのが user-desc-dpl0 との違い）。
    CriticalTest {
        name: "no-fold-flag",
        feature: "ring3-test-no-fold-flag",
        expected_markers: &["exception: vector=13", "rip=0x0000008000000000", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// int 0x80 システムコールの破壊確認（M5-f-1-2）。いずれも probe の往復が verified に
/// 到達しないことを確かめる。
const SYSCALL_TESTS: &[CriticalTest] = &[
    // 第 4 引数を context.r10 でなく context.rcx から読む。probe が記録した第 4 引数が
    // 期待値と食い違い、検証が argument register mismatch で止まる（R10 規約の実証）。
    CriticalTest {
        name: "arg4-rcx",
        feature: "syscall-test-arg4-rcx",
        expected_markers: &["syscall: argument register mismatch", "halting"],
        forbidden_markers: &["syscall: probe int 0x80 round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // ゲート 0x80 を DPL=0 にする。Ring 3 からの int 0x80 がゲート DPL<CPL で #GP になり、
    // syscall_entry に到達しない。畳みの予期 RIP は cli 位置なので畳まれず dump+halt。
    CriticalTest {
        name: "gate-dpl0",
        feature: "syscall-test-gate-dpl0",
        expected_markers: &["exception: vector=13", "halting"],
        forbidden_markers: &["syscall: probe int 0x80 round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 戻り値の context.rax 書き戻しを落とす。ユーザーが store した値が PROBE_RETURN と
    // 食い違い、検証が return value mismatch で止まる。
    CriticalTest {
        name: "drop-retval",
        feature: "syscall-test-drop-retval",
        expected_markers: &["syscall: return value mismatch", "halting"],
        forbidden_markers: &["syscall: probe int 0x80 round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // ユーザーポインタ検証（M5-f-2-1）。葉/中間の U=1 判定を外す。無効3（supervisor in
    // user range）が受理され、battery が検出して halt する。
    CriticalTest {
        name: "validate-skip-us",
        feature: "syscall-test-validate-skip-us",
        expected_markers: &[
            "syscall: pointer validation battery failed",
            "supervisor in user range",
            "halting",
        ],
        forbidden_markers: &["syscall: pointer validation battery verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // ページ走査を先頭ページだけで打ち切る。無効4（straddle）の末尾無効を取り逃して
    // 受理され、battery が検出して halt する。
    CriticalTest {
        name: "validate-skip-laststep",
        feature: "syscall-test-validate-skip-laststep",
        expected_markers: &[
            "syscall: pointer validation battery failed",
            "straddle last page",
            "halting",
        ],
        forbidden_markers: &["syscall: pointer validation battery verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // 検証器を常に受理にする。最初の拒否ケース（kernel pointer）が受理され、battery が
    // 検出して halt する（多層防御の最後の砦の確認）。
    CriticalTest {
        name: "validate-skip-all",
        feature: "syscall-test-validate-skip-all",
        expected_markers: &[
            "syscall: pointer validation battery failed",
            "kernel pointer",
            "halting",
        ],
        forbidden_markers: &["syscall: pointer validation battery verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // copy_from_user が検証を経ずに読む（M5-f-2-2）。カーネルポインタで -EFAULT のはずが総和が
    // 返り、内容往復の検証が「-EFAULT のはずが値」を検出して halt する。
    CriticalTest {
        name: "copy-skip-validate",
        feature: "syscall-test-copy-skip-validate",
        expected_markers: &[
            "syscall: checksum case 'kernel pointer' expected reject",
            "halting",
        ],
        forbidden_markers: &["syscall: checksum round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // copy_from_user が len を 1 バイト超えて読む（M5-f-2-2）。末尾の余分な既知バイトが総和へ
    // 混ざり、内容往復のチェックサムが決定的に食い違って halt する。
    CriticalTest {
        name: "copy-overrun",
        feature: "syscall-test-copy-overrun",
        expected_markers: &["syscall: checksum mismatch", "halting"],
        forbidden_markers: &["syscall: checksum round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// higher-half（B-2a-5）の破壊確認。「cpu_reset が起きた」だけを合格条件に
/// しない（どんな理由で死んでも合格になり検査にならない）。各テストが「期待した
/// 箇所で死んだ」ことを、シリアルの位置署名（到達した行 present / その先へ進んで
/// いない行 absent）と「定常状態に到達していない（heartbeat が 1 本も出ない）」の
/// AND で判定する。
///
/// **実測メモ（B-2a-5）**: これらはトリプルフォルト（cpu_reset）しない。
/// (a)(b) はトランポリン/最初期の #PF が我々の IDT 導入前に起きるため UEFI の IDT が
/// 拾い、ファームウェアが制御を取り戻す（-d int に firmware 領域の RIP が残る）。
/// (c) は M2-d 切り替え後に高位で #PF し、定常へ進めず停止する。いずれも cpu_reset は
/// 増えない。したがって判定は位置署名 + 定常未到達で行い、cpu_reset 数と firmware RIP は
/// 参考情報として出す（cpu_reset を必須条件にしない、が実測でより強く正当化された）。
struct HighhalfTest {
    name: &'static str,
    feature: &'static str,
    /// シリアルに現れるべき行（そこまで到達した証拠）。
    present_markers: &'static [&'static str],
    /// シリアルに現れてはいけない行（その先へ進んでいないことの証拠）。
    absent_markers: &'static [&'static str],
}

const HIGHHALF_TESTS: &[HighhalfTest] = &[
    // (a) 初期 PML4[0]（恒等）を not-present にする。mov cr3 直後の低位命令フェッチが
    // 解決できず #PF。我々の IDT 導入前なので UEFI の IDT が拾い、ファームウェアへ戻る。
    // bootloader はトランポリンへ到達しているが、カーネルの最初のログは出ない。
    // (a) と (b) は同一署名（トランポリンとカーネルの間で死ぬ）。
    HighhalfTest {
        name: "no-identity-in-boot-pt",
        feature: "highhalf-no-identity-in-boot-pt",
        present_markers: &["kernel entry: VMA"],
        absent_markers: &["ZaytOS kernel: entered _start"],
    },
    // (b) PDPT_high のエントリを 510→509 へずらす。高位 _start への jmp 先が未マップで
    // #PF。署名は (a) と同じ。
    HighhalfTest {
        name: "bad-high-slot",
        feature: "highhalf-bad-high-slot",
        present_markers: &["kernel entry: VMA"],
        absent_markers: &["ZaytOS kernel: entered _start"],
    },
    // (c) 本流テーブルからカーネル高位マッピングを外す。高位到達し、切替前の必須マッピング
    // 検証も通過（それは物理範囲を見るので高位マッピングの欠落を捕まえない。実測で確定）した
    // うえで、M2-d の CR3 切り替え後に高位で #PF して停止する。
    HighhalfTest {
        name: "no-kernel-high-in-live-table",
        feature: "highhalf-no-kernel-high-in-live-table",
        present_markers: &["higher-half: arrived at high VA"],
        absent_markers: &["paging: CR3 switch verified"],
    },
    // B-2b-4: 恒等除去の 5a 復帰。他の highhalf 破壊と違い「どこかで死ぬ」テストではなく、
    // 恒等除去点で remove_identity を呼び、必須領域のヒープ高位 VA を解決不能な高位 VA
    // （空の PML4[257] = 0xffff808000000000）へ差し替えて step4 を失敗させる。5a が PML4[0] を
    // 書き戻し（restored PML4[0]）、恒等が実際に復活し（revived=true）、除去を完了せず
    // （done は出ない）halt する。除去点は start_timer より前なので heartbeat=0 も成立する。
    HighhalfTest {
        name: "remove-verify-fail",
        feature: "highhalf-remove-verify-fail",
        present_markers: &[
            "required [heap high VA] 0xffff808000000000: FAILED",
            "verification FAILED. restored PML4[0]",
            "revived=true",
        ],
        absent_markers: &["identity-removal: done"],
    },
    // B-2b-4(e): ヒープ初期化を低位（heap_start=物理）へ戻す。恒等除去は begin→clear まで進み、
    // 5b のフラッシュ後に step6 が high_mapped（低位ヒープ上）を辿って #PF で死ぬ（removal は
    // 除去は既定どおり done まで完走し（high_mapped はスタックなので除去自身は生き残る）、その後
    // 1181 のヒープスモークテストが低位ヒープ（`heap_start` 物理）をデレフして #PF で死ぬ。**この
    // テストは #PF ダンプ（vector=14 + cr2 が低位＝物理ヒープ域）が出ることが成功署名**であり、
    // フレーク判断の一般手順（`exception: vector=` は回帰）とは区別される意図的破壊（除去より前に
    // ヒープを高位化する順序の必要性を実証。verification-coverage 参照）。cr2=0x00000000… で低位を確認。
    HighhalfTest {
        name: "remove-before-highify",
        feature: "highhalf-remove-before-highify",
        present_markers: &[
            "identity-removal: done",
            "exception: vector=14",
            "cr2=0x00000000",
        ],
        absent_markers: &[],
    },
    // B-2b-4(e): 恒等除去の直後に意図的 panic。除去は done まで完走し、その後パニックダンプが出る。
    // **パニックダンプが出ることが成功署名**（パニック経路がシリアル I/O・レジスタ値のみ・walk
    // なしで恒等非依存であり、恒等を外した後も動くことの実証）。同じく一般手順とは区別される。
    HighhalfTest {
        name: "panic-after-remove",
        feature: "highhalf-panic-after-remove",
        present_markers: &[
            "identity-removal: done",
            "intentional panic right after identity removal",
        ],
        absent_markers: &[],
    },
];

/// トランポリンのコード先頭 24 バイト（B-2a-2/B-2a-3b で 2 度、再リンク・
/// B-1 撤去を跨いで不変を実証した期待リテラル）。rel32 はすべて .text.trampoline
/// 内なので、他の変更で動かない。sabotage (d) はここへ 1 命令挿入して食い違わせる。
const EXPECTED_TRAMPOLINE_BYTES: [u8; 24] = [
    0x48, 0x8b, 0x05, 0x11, 0x00, 0x00, 0x00, // mov rax, [rip + zaytos_tramp_pml4]
    0x0f, 0x22, 0xd8, // mov cr3, rax
    0x48, 0x8b, 0x25, 0x0f, 0x00, 0x00, 0x00, // mov rsp, [rip + zaytos_tramp_stack]
    0xff, 0x25, 0x11, 0x00, 0x00, 0x00, // jmp [rip + zaytos_tramp_entry]
    0x90, // p2align 3 のパディング
];

/// ビルド済み kernel.elf の入口（トランポリン）先頭 24 バイトを読む。
///
/// 新規クレート依存を増やさず、既存の `common::elf` を再利用する。入口は
/// `.text.trampoline` の先頭に置いてあり、入口を含む PT_LOAD セグメントの
/// ファイル内容から `entry - p_vaddr` オフセットで取り出す。
fn trampoline_bytes(kernel_elf: &Path) -> Result<[u8; 24]> {
    let bytes =
        fs::read(kernel_elf).with_context(|| format!("failed to read {}", kernel_elf.display()))?;
    let elf = common::elf::Elf::parse(&bytes)
        .map_err(|e| anyhow::anyhow!("failed to parse {} as ELF: {e:?}", kernel_elf.display()))?;
    let entry = elf.entry_point;
    let seg = elf
        .load_segments()
        .find(|s| entry >= s.p_vaddr && entry < s.p_vaddr + s.p_memsz)
        .context("no PT_LOAD segment contains the entry point")?;
    let data = elf.segment_data(&seg);
    let offset = (entry - seg.p_vaddr) as usize;
    let slice = data
        .get(offset..offset + 24)
        .context("the entry point is too close to the end of its segment")?;
    let mut out = [0u8; 24];
    out.copy_from_slice(slice);
    Ok(out)
}

/// カーネルが起動したことを示す、シリアルログの既知の行。
///
/// kernel の `kernel_main` が最初に出す行（`common::log` の INFO 形式）。
/// これがログに無ければ、カーネルは走っていない。
const KERNEL_STARTED_MARKER: &str = "[INFO] ZaytOS kernel: entered _start";

/// カーネルが**起動しきった**ことを示す行。
///
/// M4-d-2 より前は `kernel: halting` が到達点の目印だった。タイマを入れて
/// カーネルが停止しなくなったため、その行はもう出ない。代わりに
/// 「最初のタイマ割り込みが届いた」を到達点とする。ここまで来ていれば、
/// GDT / IDT / ページング / ヒープ / コンソール / PIC / PIT のすべてが
/// 動いており、割り込みも配送されている。
///
/// [`KERNEL_STARTED_MARKER`] とは役割が違う。あちらは「そもそもカーネルが
/// 走ったか」（OVMF の起動失敗との切り分け）を見るためのもので、こちらは
/// 「最後まで通ったか」を見る。
const KERNEL_BOOT_COMPLETE_MARKER: &str = "timer: first tick arrived as vector 0x20";

/// bootloader が起動したことを示す、シリアルログの既知の行。
///
/// panic-test は bootloader を検証するもので、kernel へ到達する前に panic
/// する。したがって kernel の marker ではなくこちらで起動を判定する。
const BOOTLOADER_STARTED_MARKER: &str = "[INFO] ZaytOS bootloader: serial log established";

/// OVMF がまれにカーネルを起動せず、シェルやアイドルループへフォールバック
/// することがある（`docs/troubleshooting.md` 2026-07-19 の記録）。この場合
/// ファームウェアのタイマ割り込み（ベクタ 0x20）が延々と記録され、RIP は
/// ファームウェア領域（おおむね 0x0f00_0000 以上、実 RAM 256MiB の外）を指す。
const FIRMWARE_REGION_START: u64 = 0x0f00_0000;

/// テストの前提（カーネルが起動したか）を判定した結果。
enum BootOutcome {
    /// カーネルが起動した。テスト結果を信頼してよい。
    Started,
    /// カーネルが起動しなかった。テストの成否ではなく環境の問題。
    DidNotStart {
        /// ファームウェア領域を指す RIP を `-d int` ログから拾えた場合。
        firmware_rip: Option<u64>,
    },
}

/// シリアルログと QEMU デバッグログから、対象が起動したかを判定する。
///
/// `started_marker` は起動を示す既知の行（kernel なら
/// [`KERNEL_STARTED_MARKER`]、bootloader なら [`BOOTLOADER_STARTED_MARKER`]）。
///
/// **テストの FAIL を報告する前に必ず呼ぶこと。** 「実装の問題」と「OVMF の
/// 起動フレーキネス」を取り違えると、存在しないバグを追いかけることになる
/// （実際に M4-c-1 でこの取り違えが起きかけた）。
fn classify_boot(serial: &str, qemu_debug: &str, started_marker: &str) -> BootOutcome {
    if serial.contains(started_marker) {
        return BootOutcome::Started;
    }

    // 起動していない。裏付けとして、ファームウェア領域を指す RIP を探す。
    // `-d int` の RIP 行は "RIP=000000000f6e973c ..." の形。
    let firmware_rip = qemu_debug.lines().rev().find_map(|line| {
        let rest = line.trim_start().strip_prefix("RIP=")?;
        let hex = rest.split_whitespace().next()?;
        let value = u64::from_str_radix(hex, 16).ok()?;
        (value >= FIRMWARE_REGION_START).then_some(value)
    });

    BootOutcome::DidNotStart { firmware_rip }
}

/// 起動失敗を報告する。テスト FAIL とは別物として扱う。
///
/// 戻り値の `Err` は「テストが落ちた」ではなく「起動しなかったので判定
/// できない」ことを表す。呼び出し側はこのメッセージで両者を区別する。
///
/// # 原因を断定しない
///
/// 以前ここは「OVMF の起動フレーキネスである」と断定して書いていた。
/// **その文言が、まったく別の原因を 2 度覆い隠した。**
///
/// - varstore を使い回していたため OVMF の起動項目が蓄積していた
/// - QEMU の monitor ソケットのパスが `sun_path` の 108 バイト上限を
///   超えており、QEMU がソケットを作れずに終了していた
///
/// どちらも症状は「起動マーカーが出ない」で同一だが、原因も直し方も違う。
/// 便利なラベルを貼ると、そこで調査が止まる。断定するのは観測した事実
/// （マーカーが無い、RIP がどこを指していた）だけにして、原因は候補を
/// 並べるにとどめる。
fn report_did_not_start(
    context: &str,
    firmware_rip: Option<u64>,
    qemu_exit: Option<&str>,
) -> Result<()> {
    println!("{context}: TARGET DID NOT START (not a test failure)");
    println!("{context}:   observed: the serial log has no start-up marker line");
    match firmware_rip {
        Some(rip) => println!(
            "{context}:   observed: qemu -d int shows RIP={rip:#018x} in the firmware region \
             (>= {FIRMWARE_REGION_START:#x}), so qemu ran and the firmware kept executing"
        ),
        None => println!(
            "{context}:   observed: no firmware RIP in the qemu log either, so qemu may not \
             have reached the firmware at all"
        ),
    }
    if let Some(status) = qemu_exit {
        println!("{context}:   observed: qemu exited on its own ({status})");
    }
    println!("{context}:   this check does not identify the cause. Candidates:");
    println!("{context}:     - OVMF booted but fell back to its shell / idle loop");
    println!("{context}:     - qemu never started (bad arguments, missing OVMF, port in use)");
    println!(
        "{context}:     - the monitor socket path exceeded the {SOCKET_PATH_LIMIT}-byte \
         sun_path limit"
    );
    println!("{context}:   see docs/troubleshooting.md 2026-07-22 for two past instances");
    bail!("{context}: kernel did not start (environment, not the code); re-run")
}

/// UNIX ドメインソケットのパス長の上限（`sockaddr_un::sun_path`）。
///
/// 終端の NUL を含めて 108 バイト。超えると `bind` に失敗する。QEMU は
/// エラーを出して即座に終了するため、症状は「ゲストが 1 行も出力しない」
/// になり、起動失敗と見分けがつかない。実際にこれで 8 回連続の失敗を
/// 「OVMF の起動フレーキネス」と誤認しかけた。
const SOCKET_PATH_LIMIT: usize = 108;

/// monitor ソケットのパスが `sun_path` の上限に収まることを確かめる。
///
/// **起動する前に弾く。** 超えたまま起動すると、QEMU が黙って終了して
/// 「起動しなかった」としか分からない。ここで長さを指摘すれば、
/// 作業ディレクトリが深すぎることがその場で分かる。
fn ensure_socket_path_fits(socket: &Path) -> Result<()> {
    let length = socket.as_os_str().as_bytes().len();
    if length < SOCKET_PATH_LIMIT {
        return Ok(());
    }
    bail!(
        "the qemu monitor socket path is {length} bytes, which does not fit in the \
         {SOCKET_PATH_LIMIT}-byte sun_path limit: {}\n\
         move the workspace to a shorter path; qemu would exit without producing any \
         guest output, which is indistinguishable from a boot failure",
        socket.display()
    )
}

// パニックハンドラの出力（bootloader/src/panic.rs）と対応する、回帰チェック用の
// 目印文字列。フォーマットを変更した場合はここも合わせて更新すること。
const PANIC_MARKER_HEADER: &str = "[ERROR] panic:";
const PANIC_MARKER_HALT: &str = "halting (cli + hlt loop)";
const PANIC_TEST_TIMEOUT: Duration = Duration::from_secs(10);
const PANIC_TEST_POLL_INTERVAL: Duration = Duration::from_millis(100);

const DEFAULT_SCREENSHOT_WAIT: Duration = Duration::from_secs(8);
const MONITOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SCREENDUMP_FILE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> Result<()> {
    const USAGE: &str = "usage: cargo xtask check [--full]\n       cargo xtask run [--panic-test] [--gui] [--gfx-test] [--kvm] [--no-limit]\n       cargo xtask run --exception-test <kind>\n       cargo xtask run --critical-test <kind>\n       cargo xtask run --interrupt-test <kind>\n       cargo xtask run --paging-test <kind>\n       cargo xtask run --stack-test <kind>\n       cargo xtask run --task-test <kind>\n       cargo xtask run --ring3-test <kind>\n       cargo xtask run --syscall-test <kind>\n       cargo xtask run --highhalf-test <kind>\n       cargo xtask screenshot [output.png] [--wait-secs N] [--gfx-test] [--kvm]\n       cargo xtask gen-font";

    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => {
            let rest = &args[1..];
            let panic_test = rest.iter().any(|a| a == "--panic-test");
            let gui = rest.iter().any(|a| a == "--gui");
            let gfx_test = rest.iter().any(|a| a == "--gfx-test");
            let kvm = rest.iter().any(|a| a == "--kvm");
            let no_limit = rest.iter().any(|a| a == "--no-limit");
            if let Some(index) = rest.iter().position(|a| a == "--interrupt-test") {
                if rest.get(index + 1).map(String::as_str) == Some("keyboard") {
                    return cmd_keyboard_test();
                }
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = INTERRUPT_TESTS.iter().map(|t| t.name).collect();
                    format!("--interrupt-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(INTERRUPT_TESTS, "interrupt-test", kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--paging-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = PAGING_TESTS.iter().map(|t| t.name).collect();
                    format!("--paging-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(PAGING_TESTS, "paging-test", kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--stack-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = STACK_TESTS.iter().map(|t| t.name).collect();
                    format!("--stack-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(STACK_TESTS, "stack-test", kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--task-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = TASK_TESTS.iter().map(|t| t.name).collect();
                    format!("--task-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(TASK_TESTS, "task-test", kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--ring3-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = RING3_TESTS.iter().map(|t| t.name).collect();
                    format!("--ring3-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(RING3_TESTS, "ring3-test", kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--syscall-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = SYSCALL_TESTS.iter().map(|t| t.name).collect();
                    format!("--syscall-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(SYSCALL_TESTS, "syscall-test", kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--highhalf-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = HIGHHALF_TESTS.iter().map(|t| t.name).collect();
                    format!(
                        "--highhalf-test requires a kind ({} | trampoline-absolute-ref)",
                        names.join(" | ")
                    )
                })?;
                // (d) はビルド + 静的バイト検査（QEMU 不要）。(a)(b)(c) は QEMU で判定。
                if kind == "trampoline-absolute-ref" {
                    return cmd_highhalf_trampoline_check(
                        &workspace_root()?,
                        &["highhalf-trampoline-absolute-ref"],
                        false,
                    );
                }
                return cmd_highhalf_test(kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--critical-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = CRITICAL_TESTS.iter().map(|t| t.name).collect();
                    format!("--critical-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(CRITICAL_TESTS, "critical-test", kind);
            }
            if let Some(index) = rest.iter().position(|a| a == "--exception-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = EXCEPTION_TESTS.iter().map(|t| t.name).collect();
                    format!("--exception-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_exception_test(kind);
            }
            cmd_run(panic_test, gui, gfx_test, kvm, no_limit)
        }
        Some("check") => cmd_check(args[1..].iter().any(|a| a == "--full")),
        Some("screenshot") => cmd_screenshot(&args[1..]),
        Some("gen-font") => font::generate(&workspace_root()?),
        Some(other) => bail!("unknown xtask subcommand: {other}\n\n{USAGE}"),
        None => bail!("missing xtask subcommand\n\n{USAGE}"),
    }
}

/// QEMU の `-serial` に渡す送り先。通常運用は人間がその場で読める `stdio`、
/// panic-test 回帰チェック・screenshot はプログラムから内容を検査できる
/// `file` を使う。
enum SerialSink {
    Stdio,
    File(PathBuf),
}

/// QEMU の `-display` バックエンド。既定は `none`（ADR-0003: シリアルログを
/// 唯一の観測手段とする）。`--gui` 指定時のみ実際のウィンドウを開く。
enum DisplayMode {
    None,
    Gui,
}

/// QEMU のアクセラレータ。既定は TCG（純粋エミュレーション）。
///
/// 既定を TCG のままにしているのは、これまでの全マイルストーンを TCG で
/// 検証してきており、既定を変えると挙動差の切り分け軸が増えるため。
/// KVM は計測時にだけ `--kvm` で明示的に選ぶ（ADR-0015 Addendum）。
enum Accelerator {
    Tcg,
    Kvm,
}

/// `qemu_launch_args` に渡す設定一式。引数が増えてきたため、位置引数の
/// 取り違えを避けるためにまとめている。
struct QemuLaunchOptions<'a> {
    ovmf_code: &'a Path,
    ovmf_vars: &'a Path,
    esp_dir: &'a Path,
    serial: &'a SerialSink,
    debug_log: &'a Path,
    display: DisplayMode,
    /// `Some` の場合、この UNIX ソケットパスで QEMU monitor (HMP) を
    /// server モードで待ち受けさせる（screenshot サブコマンド用）。
    monitor_socket: Option<&'a Path>,
    accelerator: Accelerator,
}

fn cmd_run(panic_test: bool, gui: bool, gfx_test: bool, kvm: bool, no_limit: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, panic_test)?;
    let kernel_elf = build_kernel(&workspace_root, gfx_test)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    if panic_test {
        run_panic_test(&workspace_root, &ovmf_vars, &esp_dir)
    } else {
        run_interactive(&workspace_root, &ovmf_vars, &esp_dir, gui, kvm, no_limit)
    }
}

/// `cargo xtask run` の既定の実行時間上限。
///
/// **M4-d-2 でカーネルが停止しなくなった。** タイマ割り込みで回り続けるため、
/// 放置すると `-d int` のデバッグログが増え続ける。実測で約 137KB/秒
/// （100Hz、1 ティックあたり 20 行強）なので、1 時間放置すれば 500MB に
/// 達する。「知らずに放置してディスクが埋まる」経路を塞ぐため、既定で
/// 打ち切る。`--no-limit` で解除できる。
const RUN_TIME_LIMIT: Duration = Duration::from_secs(120);

/// `-d int,cpu_reset` のログが増える概算速度（実測、TCG・100Hz）。
const DEBUG_LOG_GROWTH_KB_PER_SEC: u64 = 137;

fn run_interactive(
    workspace_root: &Path,
    ovmf_vars: &Path,
    esp_dir: &Path,
    gui: bool,
    kvm: bool,
    no_limit: bool,
) -> Result<()> {
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        serial: &SerialSink::Stdio,
        debug_log: &debug_log,
        display: if gui {
            DisplayMode::Gui
        } else {
            DisplayMode::None
        },
        monitor_socket: None,
        accelerator: if kvm {
            Accelerator::Kvm
        } else {
            Accelerator::Tcg
        },
    });
    println!("qemu debug log (-d int,cpu_reset): {}", debug_log.display());
    // M4-d-2 以降、カーネルは halt せずタイマで回り続ける。ログが増え続ける
    // ことを知らせておく。
    println!(
        "note: the kernel no longer halts (M4-d-2). It keeps ticking, so the debug log grows \
         at roughly {DEBUG_LOG_GROWTH_KB_PER_SEC} KB/s."
    );
    if no_limit {
        println!("note: --no-limit given; qemu will run until you stop it (Ctrl-C).");
    } else {
        println!(
            "note: qemu will be stopped automatically after {} s (pass --no-limit to disable).",
            RUN_TIME_LIMIT.as_secs()
        );
    }
    if kvm {
        println!(
            "accelerator: KVM (measurement mode). exception logging via -d int is \
             largely unavailable; use the default TCG for debugging."
        );
    }

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context(
            "failed to launch qemu-system-x86_64 (is it installed? `apt install qemu-system-x86`)",
        )?;

    if no_limit {
        let status = child
            .wait()
            .context("failed to wait for qemu-system-x86_64")?;
        if !status.success() {
            bail!("qemu-system-x86_64 exited with {status}");
        }
        return Ok(());
    }

    // 上限まで待つ。途中で自分から終わった（パニック等）なら、そこで抜ける。
    let deadline = Instant::now() + RUN_TIME_LIMIT;
    loop {
        match child
            .try_wait()
            .context("failed to poll qemu-system-x86_64")?
        {
            Some(status) => {
                if !status.success() {
                    bail!("qemu-system-x86_64 exited with {status}");
                }
                return Ok(());
            }
            None => {
                if Instant::now() >= deadline {
                    println!(
                        "\nreached the {} s limit; stopping qemu. The debug log is about {} MB.",
                        RUN_TIME_LIMIT.as_secs(),
                        (RUN_TIME_LIMIT.as_secs() * DEBUG_LOG_GROWTH_KB_PER_SEC) / 1024
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(());
                }
                thread::sleep(PANIC_TEST_POLL_INTERVAL);
            }
        }
    }
}

/// パニックハンドラの回帰チェック。`panic-test` フィーチャ付きでビルドした
/// bootloader（起動完了直後に意図的に `panic!` する）を起動し、シリアル出力に
/// 期待どおりのパニックダンプが現れるかをポーリングで確認する。
///
/// bootloader はパニック後も `hlt` ループで動き続け自然終了しないため、目印を
/// 検出し次第（またはタイムアウトで）QEMU プロセスを強制終了する。
fn run_panic_test(workspace_root: &Path, ovmf_vars: &Path, esp_dir: &Path) -> Result<()> {
    let serial_log_path = workspace_root.join("target").join("panic-test-serial.log");
    let _ = fs::remove_file(&serial_log_path);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        serial: &SerialSink::File(serial_log_path.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        // パニック経路の回帰チェックは例外まわりの挙動を見るものなので、
        // 常に TCG で行う。
        accelerator: Accelerator::Tcg,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the panic-test regression check")?;

    let deadline = Instant::now() + PANIC_TEST_TIMEOUT;
    let found = loop {
        if panic_markers_present(&serial_log_path) {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    };

    let _ = child.kill();
    let _ = child.wait();

    let captured = fs::read_to_string(&serial_log_path).unwrap_or_default();
    let qemu = fs::read_to_string(&debug_log).unwrap_or_default();
    println!("--- panic-test: captured serial output ---\n{captured}--- end ---");

    if found {
        println!("panic-test: PASS (panic handler produced the expected dump and halted)");
        return Ok(());
    }

    // 失敗した。実装の問題か、そもそも bootloader が起動しなかったかを分ける。
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&captured, &qemu, BOOTLOADER_STARTED_MARKER)
    {
        return report_did_not_start("panic-test", firmware_rip, None);
    }

    bail!(
        "panic-test: FAIL (bootloader started but did not produce the expected \
         panic-handler output within {PANIC_TEST_TIMEOUT:?})"
    )
}

fn panic_markers_present(serial_log_path: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(serial_log_path) else {
        return false;
    };
    contents.contains(PANIC_MARKER_HEADER) && contents.contains(PANIC_MARKER_HALT)
}

/// `cargo xtask screenshot [output.png] [--wait-secs N]` の引数を解釈する。
fn cmd_screenshot(args: &[String]) -> Result<()> {
    let mut wait = DEFAULT_SCREENSHOT_WAIT;
    let mut output_path = None;
    let mut gfx_test = false;
    let mut kvm = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--gfx-test" => gfx_test = true,
            "--kvm" => kvm = true,
            "--wait-secs" => {
                i += 1;
                let value = args
                    .get(i)
                    .context("--wait-secs requires a value (seconds)")?;
                let secs: u64 = value
                    .parse()
                    .with_context(|| format!("invalid --wait-secs value: {value}"))?;
                wait = Duration::from_secs(secs);
            }
            other => output_path = Some(PathBuf::from(other)),
        }
        i += 1;
    }

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel(&workspace_root, gfx_test)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let output_path =
        output_path.unwrap_or_else(|| workspace_root.join("target").join("screenshot.png"));

    take_screenshot(
        &workspace_root,
        &ovmf_vars,
        &esp_dir,
        wait,
        &output_path,
        kvm,
    )
}

/// QEMU を monitor (HMP) 付きで起動し、`wait` だけ待ってから `screendump` を
/// 発行し、結果の PPM を PNG へ変換して `output_path` に保存する。
///
/// 既存のシリアル出力（`SerialSink`）・デバッグログ（`-D`）の分離は崩さない:
/// このコマンド専用のシリアルログファイルへ出力させ、ターミナルには
/// このコマンド自身の進捗メッセージのみを出す。
fn take_screenshot(
    workspace_root: &Path,
    ovmf_vars: &Path,
    esp_dir: &Path,
    wait: Duration,
    output_path: &Path,
    kvm: bool,
) -> Result<()> {
    // AF_UNIX のパス長制限 (108 バイト程度) を避けるため、ワークスペース内の
    // 長いパスではなく /tmp 配下の短い一意なパスを使う。
    let monitor_socket =
        PathBuf::from(format!("/tmp/zaytos-xtask-mon-{}.sock", std::process::id()));
    let _ = fs::remove_file(&monitor_socket);
    ensure_socket_path_fits(&monitor_socket)?;
    let serial_log = workspace_root.join("target").join("screenshot-serial.log");
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let ppm_path = workspace_root.join("target").join("screenshot.ppm");
    let _ = fs::remove_file(&ppm_path);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars,
        esp_dir,
        serial: &SerialSink::File(serial_log),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: Some(&monitor_socket),
        accelerator: if kvm {
            Accelerator::Kvm
        } else {
            Accelerator::Tcg
        },
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for screenshot")?;

    println!("waiting {wait:?} for the boot sequence to settle before capturing...");
    thread::sleep(wait);

    let result = capture_screendump(&monitor_socket, &ppm_path);

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&monitor_socket);

    result?;

    let img = image::open(&ppm_path)
        .with_context(|| format!("failed to decode {}", ppm_path.display()))?;
    img.save(output_path)
        .with_context(|| format!("failed to save {}", output_path.display()))?;

    println!(
        "screenshot saved: {} ({}x{})",
        output_path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}

/// QEMU monitor (HMP) へ接続し `screendump <ppm_path>` を発行、ファイルが
/// 現れるまで待つ。
fn capture_screendump(monitor_socket: &Path, ppm_path: &Path) -> Result<()> {
    let mut stream = connect_monitor_with_retry(monitor_socket)?;
    writeln!(stream, "screendump {}", ppm_path.display())
        .context("failed to send screendump command to the QEMU monitor")?;
    stream.flush().ok();
    wait_for_file(ppm_path, SCREENDUMP_FILE_TIMEOUT)
}

fn connect_monitor_with_retry(socket_path: &Path) -> Result<UnixStream> {
    let deadline = Instant::now() + MONITOR_CONNECT_TIMEOUT;
    loop {
        match UnixStream::connect(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "failed to connect to QEMU monitor socket at {}",
                        socket_path.display()
                    )
                })
            }
        }
    }
}

fn wait_for_file(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            bail!(
                "timed out after {timeout:?} waiting for {} to be created",
                path.display()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

/// `bootloader` パッケージを UEFI ターゲット向けにビルドし、生成された
/// `.efi` バイナリのパスを返す。`panic_test` が true の場合、起動完了直後に
/// 意図的に `panic!` する `panic-test` フィーチャを有効にする。
fn build_bootloader(workspace_root: &Path, panic_test: bool) -> Result<PathBuf> {
    let mut args = vec![
        "build",
        "--target",
        UEFI_TARGET,
        "-p",
        BOOTLOADER_PACKAGE,
        "--bin",
        BOOTLOADER_PACKAGE,
    ];
    if panic_test {
        args.push("--features");
        args.push(PANIC_TEST_FEATURE);
    }

    let status = Command::new("cargo")
        .current_dir(workspace_root)
        .args(&args)
        .status()
        .context("failed to invoke cargo to build the bootloader")?;

    if !status.success() {
        bail!("bootloader build failed ({status})");
    }

    let efi_path = workspace_root
        .join("target")
        .join(UEFI_TARGET)
        .join("debug")
        .join(format!("{BOOTLOADER_PACKAGE}.efi"));
    if !efi_path.exists() {
        bail!(
            "bootloader build reported success but {} is missing",
            efi_path.display()
        );
    }
    Ok(efi_path)
}

/// `kernel` パッケージを `x86_64-unknown-none` ターゲット向けにビルドし、
/// 生成された ELF バイナリのパスを返す。
/// クリティカルセクション/ロックの回帰チェックを 1 種類実行する。
///
/// 例外テストと同じく **TCG 固定**。
/// `--interrupt-test keyboard` が QEMU monitor へ流すキー列。
///
/// `codes` は、そのキーで i8042 が出すスキャンコードの本数。修飾つきは
/// 「修飾の押下・本体の押下・本体の離脱・修飾の離脱」で 4 本、単独は
/// 押下と離脱で 2 本になる。取りこぼしの検証に使うため、期待値を
/// ここで明示的に持つ。
struct KeyInjection {
    monitor: &'static str,
    codes: u64,
}

/// 送るキー列。結果は `Hello!` になる。
///
/// **大文字と記号の両方を含める。** どちらも Shift を伴い、英字は
/// Shift と Caps の XOR、記号は Shift のみという非対称な経路を通る。
const KEYBOARD_TEST_KEYS: &[KeyInjection] = &[
    KeyInjection {
        monitor: "shift-h",
        codes: 4,
    },
    KeyInjection {
        monitor: "e",
        codes: 2,
    },
    KeyInjection {
        monitor: "l",
        codes: 2,
    },
    KeyInjection {
        monitor: "l",
        codes: 2,
    },
    KeyInjection {
        monitor: "o",
        codes: 2,
    },
    KeyInjection {
        monitor: "shift-1",
        codes: 4,
    },
    KeyInjection {
        monitor: "ret",
        codes: 2,
    },
];

/// 期待する 1 行。
const KEYBOARD_TEST_EXPECTED_LINE: &str = "keyboard: line = \"Hello!\"";

/// キーボードの回帰チェック（`--interrupt-test keyboard`）。
///
/// QEMU monitor の `sendkey` で既知のキー列を注入し、シリアルログを検証する。
/// monitor への接続は `screenshot` の実装を再利用する。
///
/// **キーリピート（タイプマティック）は検証できない。** `sendkey` は保持時間を
/// 指定してもリピートを模擬せず、押下と離脱を 1 組送るだけである
/// （`sendkey a 3000` で実測）。リピートは手動確認に回す。
fn cmd_keyboard_test() -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel(&workspace_root, false)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("keyboard-test-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);
    // **screenshot と同じく /tmp の短いパスを使う。** workspace 配下に置くと、
    // 作業ディレクトリが深い場所（git worktree を /tmp の下に作った場合など）で
    // `sun_path` の 108 バイト上限を超える。超えると QEMU はソケットを作れずに
    // 即座に終了し、ゲストの出力が 1 行も出ない。症状が起動失敗と区別できず、
    // 実際にこれを「OVMF の起動フレーキネス」と 8 回連続で誤認しかけた。
    let monitor_socket = PathBuf::from(format!(
        "/tmp/zaytos-xtask-keyboard-{}.sock",
        std::process::id()
    ));
    let _ = fs::remove_file(&monitor_socket);
    ensure_socket_path_fits(&monitor_socket)?;

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: Some(&monitor_socket),
        accelerator: Accelerator::Tcg,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the keyboard test")?;

    // キーを送る前に、IRQ1 が解禁されるまで待つ。**上限つき。**
    let ready_marker = "keyboard: IRQ1 is unmasked";
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let mut ready = false;
    while Instant::now() < deadline {
        if fs::read_to_string(&serial_log)
            .map(|c| c.contains(ready_marker))
            .unwrap_or(false)
        {
            ready = true;
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let mut expected_codes = 0u64;
    if ready {
        match connect_monitor_with_retry(&monitor_socket) {
            Ok(mut stream) => {
                for key in KEYBOARD_TEST_KEYS {
                    if writeln!(stream, "sendkey {}", key.monitor).is_err() {
                        break;
                    }
                    expected_codes += key.codes;
                    thread::sleep(Duration::from_millis(200));
                }
            }
            Err(e) => println!("keyboard-test: could not reach the QEMU monitor: {e}"),
        }
        // 反映とハートビートの更新を待つ。
        thread::sleep(Duration::from_secs(3));
    }

    // **kill する前に、既に終わっていないかを見る。** 自分から終了して
    // いたなら、それは QEMU 側の異常（引数が不正、OVMF が無い、ソケットを
    // 作れない等）であって、ゲストが動かなかったのとは別である。この 1 行が
    // 無いと両者を区別できない。
    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&monitor_socket);

    let serial = fs::read_to_string(&serial_log).unwrap_or_default();
    let qemu = fs::read_to_string(&debug_log).unwrap_or_default();

    let context = "interrupt-test keyboard";
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(context, firmware_rip, qemu_exit.as_deref());
    }

    println!("--- {context}: relevant output ---");
    for line in serial
        .lines()
        .filter(|l| l.contains("keyboard") || l.contains("heartbeat") || l.contains("i8042"))
    {
        println!("{line}");
    }
    println!("--- end ---");

    let mut ok = ready;
    if !ready {
        println!("{context}: the kernel never reached {ready_marker:?} = NG");
    }

    // 1. 期待した文字列になったか（大文字と記号の変換を含む）。
    let line_ok = serial.contains(KEYBOARD_TEST_EXPECTED_LINE);
    println!(
        "{context}: serial contains {KEYBOARD_TEST_EXPECTED_LINE:?} = {}",
        if line_ok { "OK" } else { "NG" }
    );
    ok &= line_ok;

    // 2. IRQ1 の配送経路。
    let vector_ok = serial.contains("keyboard: first key arrived as vector 0x21");
    println!(
        "{context}: the first key arrived as vector 0x21 = {}",
        if vector_ok { "OK" } else { "NG" }
    );
    ok &= vector_ok;

    // 3. 送った本数と受け取った本数の一致（取りこぼしなし）。
    let expected_keys = format!("keys={expected_codes} ");
    let count_ok = serial.contains(&expected_keys);
    println!(
        "{context}: received exactly {expected_codes} scancode(s) = {}",
        if count_ok { "OK" } else { "NG" }
    );
    ok &= count_ok;

    // 4. 会計が閉じていること、溢れていないこと。
    let balanced_ok = serial.contains("balanced=true") && !serial.contains("balanced=false");
    println!(
        "{context}: the scancode accounting balances = {}",
        if balanced_ok { "OK" } else { "NG" }
    );
    ok &= balanced_ok;

    // 「dropped= が出ていて、そのすべてが 0」であることを見る。
    // contains("dropped=0") だけだと、別の行に dropped=3 があっても通る。
    let no_drop_ok = serial.contains("dropped=0")
        && !serial
            .lines()
            .any(|l| l.contains("dropped=") && !l.contains("dropped=0 "));
    println!(
        "{context}: no scancode was dropped = {}",
        if no_drop_ok { "OK" } else { "NG" }
    );
    ok &= no_drop_ok;

    // 5. ティックが進み続けていること（タイマとキーボードの共存）。
    let heartbeats = serial.matches("heartbeat: ticks=").count();
    println!("{context}: heartbeat lines = {heartbeats} (expected at least 2)");
    ok &= heartbeats >= 2;

    // 6. 例外が起きていないこと。
    for marker in ["v=0e", "v=08"] {
        let present = qemu.contains(marker);
        println!(
            "{context}: qemu log free of {marker:?} = {}",
            if present { "NG" } else { "OK" }
        );
        ok &= !present;
    }

    if ok {
        println!("{context}: PASS");
        println!(
            "{context}: note - key repeat (typematic) is NOT covered here; QEMU's sendkey does \
             not emulate it. Check it by hand with `cargo xtask run --gui`."
        );
        Ok(())
    } else {
        bail!("{context}: FAIL")
    }
}

/// higher-half（B-2a-5）の破壊確認を走らせる。
///
/// トリプルフォルト系は「cpu_reset が起きた」だけでなく、位置署名（到達した/
/// していない行）との AND で「期待した箇所で死んだ」ことを判定する。カーネルが
/// 起動しないのは (a)(b) では期待挙動なので、基盤の生死は bootloader の起動
/// マーカーで判定する。
fn cmd_highhalf_test(kind: &str) -> Result<()> {
    let test = HIGHHALF_TESTS
        .iter()
        .find(|t| t.name == kind)
        .with_context(|| {
            let names: Vec<&str> = HIGHHALF_TESTS.iter().map(|t| t.name).collect();
            format!(
                "unknown highhalf-test {kind:?} (expected one of: {})",
                names.join(", ")
            )
        })?;

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let features: Vec<&str> = test.feature.split(',').collect();
    let kernel_elf = build_kernel_with_features(&workspace_root, &features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("highhalf-test-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
    });

    // **必ずタイムアウトまで待つ。** 破壊ビルドは「死んで止まる」ので、present
    // marker が出た時点で kill すると、死亡直前までのシリアルが流れ切る前に切って
    // しまい、absent marker（死亡点の手前）まで届かないことがある。full timeout まで
    // 待てば、死ぬまでに出るログがすべて流れ、かつ定常（heartbeat）へ進まないことも
    // 確かめられる。到達しても heartbeat が延々出るだけなので上限は変わらない。
    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the highhalf test")?;

    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    while Instant::now() < deadline {
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = fs::read_to_string(&serial_log).unwrap_or_default();
    let qemu = fs::read_to_string(&debug_log).unwrap_or_default();

    let context = format!("highhalf-test {}", test.name);

    // テスト基盤自体が動いたか（bootloader が起動したか）を先に確かめる。
    // カーネルが起動しないのは (a)(b) では期待挙動なので、KERNEL ではなく
    // BOOTLOADER の起動マーカーで基盤の生死を判定する。
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, BOOTLOADER_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip, qemu_exit.as_deref());
    }

    println!("--- {context}: relevant output ---");
    for line in serial.lines().filter(|l| {
        l.contains("kernel entry")
            || l.contains("higher-half")
            || l.contains("entered _start")
            || l.contains("CR3 switch")
            || l.contains("required range")
            || l.contains("identity-removal")
            || l.contains("halting")
    }) {
        println!("{line}");
    }
    println!("--- end ---");

    let mut ok = true;
    for marker in test.present_markers {
        let present = serial.contains(marker);
        ok &= present;
        println!(
            "{context}: serial contains {marker:?} = {}",
            if present { "OK" } else { "NG" }
        );
    }
    for marker in test.absent_markers {
        let absent = !serial.contains(marker);
        ok &= absent;
        println!(
            "{context}: serial does NOT contain {marker:?} = {}",
            if absent { "OK" } else { "NG" }
        );
    }

    // **定常状態（heartbeat）へ到達していないこと。** 破壊が効いていれば起動は死んで
    // 止まり、タイマループへ入らない。これが「期待箇所で死んだ」ことの最終的な裏づけ。
    let heartbeats = serial.matches("heartbeat: ticks=").count();
    let no_steady_state = heartbeats == 0;
    ok &= no_steady_state;
    println!(
        "{context}: heartbeat lines = {heartbeats} (expected 0, boot must not reach steady state) = {}",
        if no_steady_state { "OK" } else { "NG" }
    );

    // 参考情報（判定条件ではない。実測でこれらは cpu_reset しないため）。
    let resets = qemu.matches("CPU Reset").count();
    let firmware_rip = matches!(
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER),
        BootOutcome::DidNotStart {
            firmware_rip: Some(_)
        }
    );
    println!(
        "{context}: (info) CPU Reset count = {resets} (baseline {EXPECTED_CPU_RESET_COUNT}; these \
         sabotages fault without a reset), reverted to firmware = {firmware_rip}"
    );

    if ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAILED")
    }
}

/// トランポリンのバイト単位一致検査（B-2a-5）。
///
/// `expect_match` が真なら既定ビルドで一致すること、偽なら sabotage
/// （trampoline-absolute-ref）で不一致になることを確かめる。QEMU 不要の静的検査。
fn cmd_highhalf_trampoline_check(
    workspace_root: &Path,
    features: &[&str],
    expect_match: bool,
) -> Result<()> {
    let kernel_elf = build_kernel_with_features(workspace_root, features)?;
    let bytes = trampoline_bytes(&kernel_elf)?;
    let matches = bytes == EXPECTED_TRAMPOLINE_BYTES;
    let label = if features.is_empty() {
        "default build".to_string()
    } else {
        format!("features [{}]", features.join(","))
    };
    println!(
        "trampoline byte check ({label}): matches expected = {matches} (wanted {expect_match})"
    );
    if matches != expect_match {
        println!("  expected: {:02x?}", EXPECTED_TRAMPOLINE_BYTES);
        println!("  actual:   {:02x?}", bytes);
        bail!("trampoline byte check ({label}): the trampoline code does not match expectations");
    }
    Ok(())
}

/// マーカー突き合わせ方式の回帰チェック（critical-test / interrupt-test 共通）。
///
/// シリアルログに「出るべき行」がすべて出て、「出てはいけない行」が 1 つも
/// 出ていないことを確認する。起動しなかった場合はテスト失敗と区別する。
fn cmd_marker_test(tests: &[CriticalTest], kind_label: &str, kind: &str) -> Result<()> {
    let test = tests.iter().find(|t| t.name == kind).with_context(|| {
        let names: Vec<&str> = tests.iter().map(|t| t.name).collect();
        format!(
            "unknown {kind_label} {kind:?} (expected one of: {})",
            names.join(", ")
        )
    })?;

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let features: Vec<&str> = test.feature.split(',').collect();
    let kernel_elf = build_kernel_with_features(&workspace_root, &features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join(format!("{kind_label}-serial.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
    });

    // **期待マーカーが全部そろうまで待つ。** 先頭 1 本を見た時点で QEMU を
    // 落とす作りだと、残りのマーカーがまだシリアルへ流れている途中で
    // 打ち切られる。38400 baud では 4 行で約 70ms かかり、ポーリング間隔
    // （100ms）と同じ桁なので、たいていは間に合うが時々間に合わない。
    // 実際に `interrupt-test misaligned` が、4 行のうち 2 行だけ出た状態で
    // 「halting の行が無い」と判定して落ちた。テストの成否が実行ごとの
    // タイミングで変わる状態は、失敗を見ても実装の問題か取りこぼしかを
    // 区別できない。
    //
    // 全部そろうまで待てば、遅れて届く行を取りこぼさない。到達しない場合は
    // 従来どおり `deadline` で打ち切るので、上限は変わらない。
    let wait_for_full_timeout = test.wait_for_full_timeout;
    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the critical test")?;

    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    loop {
        if !wait_for_full_timeout
            && fs::read_to_string(&serial_log)
                .map(|c| test.expected_markers.iter().all(|m| c.contains(m)))
                .unwrap_or(false)
        {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    // **kill する前に、既に終わっていないかを見る。** 自分から終了して
    // いたなら、それは QEMU 側の異常（引数が不正、OVMF が無い、ソケットを
    // 作れない等）であって、ゲストが動かなかったのとは別である。この 1 行が
    // 無いと両者を区別できない。
    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = fs::read_to_string(&serial_log).unwrap_or_default();
    let qemu = fs::read_to_string(&debug_log).unwrap_or_default();

    let context = format!("{kind_label} {}", test.name);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip, qemu_exit.as_deref());
    }

    println!("--- {context}: relevant output ---");
    for line in serial.lines().filter(|l| {
        l.contains("critical")
            || l.contains("lock:")
            || l.contains("sti")
            || l.contains("irq-path")
            || l.contains("heartbeat")
            || l.contains("interrupt-test")
            || l.contains("stack alignment")
    }) {
        println!("{line}");
    }
    println!("--- end ---");

    // 期待/禁止マーカーとは独立した、外側からの生存判定の結果。
    let mut ok_override = true;

    // **完全停止の外側からの検出。** メインループが `hlt` で眠ったまま
    // 二度と起きなくなった場合、カーネル自身はそれを検出できない（検出用の
    // コードが動かない）。QEMU も `-no-shutdown` で生き続けるため、プロセスの
    // 生死からも判断できない。**シリアルログのハートビート回数**だけが外から
    // 見える手掛かりになる。「カーネル内部では検出できないが、テスト基盤では
    // 検出できる」という切り分けである。
    if let Some(expected) = test.min_heartbeats {
        let seen = serial.matches("heartbeat: ticks=").count();
        println!("{context}: heartbeat lines = {seen} (expected at least {expected})");
        if seen < expected {
            println!(
                "{context}: too few heartbeats - the main loop probably stopped waking up. \
                 This is the only way to notice a permanent hlt from the outside."
            );
            ok_override = false;
        }
    }

    let mut ok = ok_override;
    for marker in test.expected_markers {
        let present = serial.contains(marker);
        ok &= present;
        println!(
            "{context}: serial contains {marker:?} = {}",
            if present { "OK" } else { "NG" }
        );
    }
    for marker in test.forbidden_markers {
        let absent = !serial.contains(marker);
        ok &= absent;
        println!(
            "{context}: serial does NOT contain {marker:?} = {}",
            if absent {
                "OK"
            } else {
                "NG (detection was bypassed)"
            }
        );
    }

    if ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAIL")
    }
}

/// 例外ハンドラの回帰チェックを 1 種類実行する。
///
/// シリアルログのハンドラ出力と、`qemu-debug.log` の `v=..` の両方を
/// 突き合わせる。片方だけでは、ベクタ番号を取り違えたまま動いているように
/// 見える事故を防げない（ADR-0018）。
fn cmd_exception_test(kind: &str) -> Result<()> {
    let test = EXCEPTION_TESTS
        .iter()
        .find(|t| t.name == kind)
        .with_context(|| {
            let names: Vec<&str> = EXCEPTION_TESTS.iter().map(|t| t.name).collect();
            format!(
                "unknown exception test {kind:?} (expected one of: {})",
                names.join(", ")
            )
        })?;

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let features: Vec<&str> = test.feature.split(',').collect();
    let kernel_elf = build_kernel_with_features(&workspace_root, &features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("exception-test-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    // 例外の記録が要るので、この検証は常に TCG で行う。
    let qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
    });

    let expected_serial = format!("[ERROR] exception: vector={} ", test.vector);
    // **ダンプの終端行が出るまで待つ。** 待ち条件を先頭行（vector=X）だけに
    // すると、それが現れた瞬間に kill してしまい、後続のダンプ（GPR・CR2・
    // on IST・halting）が serial へ書き出される前に切れることがある。実際に
    // double-fault で「on IST1=true」が捕捉から漏れて落ちた。ハンドラは必ず
    // 最後にこの行を出すので、これを見てから kill すれば全行がそろう
    // （マーカーテストの「全マーカーがそろうまで待つ」と同じ考え）。
    const DUMP_TERMINATOR: &str = "[ERROR] halting (cli + hlt loop)";

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the exception test")?;

    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let handler_ran = loop {
        if fs::read_to_string(&serial_log)
            .map(|c| c.contains(&expected_serial) && c.contains(DUMP_TERMINATOR))
            .unwrap_or(false)
        {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    };

    // **kill する前に、既に終わっていないかを見る。** 自分から終了して
    // いたなら、それは QEMU 側の異常（引数が不正、OVMF が無い、ソケットを
    // 作れない等）であって、ゲストが動かなかったのとは別である。この 1 行が
    // 無いと両者を区別できない。
    let qemu_exit = child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| format!("{status}"));
    let _ = child.kill();
    let _ = child.wait();

    let serial = fs::read_to_string(&serial_log).unwrap_or_default();
    let qemu = fs::read_to_string(&debug_log).unwrap_or_default();

    // テスト結果を読む前に、そもそもカーネルが起動したかを判定する。
    // 起動していなければ、以降の OK/NG は意味を持たない。
    let context = format!("exception-test {}", test.name);
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip, qemu_exit.as_deref());
    }

    let qemu_saw_it = qemu.contains(test.qemu_marker);

    println!("--- exception-test {}: handler output ---", test.name);
    for line in serial.lines().filter(|l| l.starts_with("[ERROR]")) {
        println!("{line}");
    }
    println!("--- end ---");

    println!(
        "exception-test {}: handler reported vector {} = {}",
        test.name,
        test.vector,
        if handler_ran { "OK" } else { "NG" }
    );
    println!(
        "exception-test {}: qemu -d int recorded {} = {}",
        test.name,
        test.qemu_marker,
        if qemu_saw_it { "OK" } else { "NG" }
    );

    // 追加の目印（CR2、エラーコードの展開、IST の確認など）。
    let mut markers_ok = true;
    for marker in test.extra_serial_markers {
        let present = serial.contains(marker);
        markers_ok &= present;
        println!(
            "exception-test {}: serial contains {marker:?} = {}",
            test.name,
            if present { "OK" } else { "NG" }
        );
    }

    // 汎用レジスタの並び順。名前と値の対応が入れ替わっていれば落ちる。
    let mut registers_ok = true;
    if test.check_registers {
        let mut wrong = Vec::new();
        for (name, value) in KNOWN_REGISTERS {
            if !serial.contains(&format!("{name}={value}")) {
                wrong.push(*name);
            }
        }
        registers_ok = wrong.is_empty();
        if registers_ok {
            println!(
                "exception-test {}: all {} general purpose registers dumped with the \
                 expected value = OK",
                test.name,
                KNOWN_REGISTERS.len()
            );
        } else {
            println!(
                "exception-test {}: register mismatch for {:?} = NG (the push order in \
                 the stub and the field order in ExceptionContext disagree)",
                test.name, wrong
            );
        }
    }

    // トリプルフォルトになっていないこと。
    let mut reset_ok = true;
    if test.check_no_extra_cpu_reset {
        let resets = qemu.matches("CPU Reset").count();
        reset_ok = resets <= EXPECTED_CPU_RESET_COUNT;
        println!(
            "exception-test {}: CPU Reset count = {resets} (expected <= {}) = {}",
            test.name,
            EXPECTED_CPU_RESET_COUNT,
            if reset_ok { "OK" } else { "NG (triple fault?)" }
        );
    }

    if handler_ran && qemu_saw_it && markers_ok && registers_ok && reset_ok {
        println!("exception-test {}: PASS", test.name);
        Ok(())
    } else {
        bail!(
            "exception-test {}: FAIL (handler={handler_ran}, qemu={qemu_saw_it}, \
             markers={markers_ok}, registers={registers_ok}, cpu_reset={reset_ok})",
            test.name
        );
    }
}

/// 指定した feature 付きで kernel をビルドする。
/// `cargo xtask check` が順に実行する検査。
///
/// # なぜ 1 コマンドに畳むのか
///
/// M3-b 以降、実装ループから `cargo fmt --check` と `cargo clippy` が抜け落ち、
/// 誰も気づかないまま整形差分が 52 箇所、clippy 警告が 10 件まで積み上がった。
/// 原因は個々の見落としではなく、**維持していることを確認する手順が
/// どこにも無かった**ことである。手順が増えるほど飛ばされやすくなるので、
/// 覚えるものを 1 つに減らす。
///
/// # 構成を明示的に並べる理由
///
/// bootloader と kernel は**ターゲットが違う**（`x86_64-unknown-uefi` と
/// `x86_64-unknown-none`）。`--workspace --all-targets` でまとめて回すことは
/// できない。ホスト向けに bootloader をビルドしようとして失敗するためである。
/// 構成ごとに並べるほかない。
const CHECKS: &[(&str, &[&str])] = &[
    (
        "build bootloader (uefi)",
        &["build", "-p", BOOTLOADER_PACKAGE, "--target", UEFI_TARGET],
    ),
    (
        "build kernel (none)",
        &["build", "-p", KERNEL_PACKAGE, "--target", KERNEL_TARGET],
    ),
    ("build common (host)", &["build", "-p", "common"]),
    ("build xtask (host)", &["build", "-p", "xtask"]),
    ("test (host)", &["test", "--workspace"]),
    (
        "clippy bootloader (uefi)",
        &[
            "clippy",
            "-p",
            BOOTLOADER_PACKAGE,
            "--target",
            UEFI_TARGET,
            "--",
            "-D",
            "warnings",
        ],
    ),
    (
        "clippy kernel (none)",
        &[
            "clippy",
            "-p",
            KERNEL_PACKAGE,
            "--target",
            KERNEL_TARGET,
            "--",
            "-D",
            "warnings",
        ],
    ),
    (
        "clippy common (host)",
        &["clippy", "-p", "common", "--", "-D", "warnings"],
    ),
    (
        "clippy xtask (host)",
        &["clippy", "-p", "xtask", "--", "-D", "warnings"],
    ),
    ("fmt --check", &["fmt", "--all", "--", "--check"]),
];

/// 直接の割り込み制御（`InterruptGuard` 非経由の `cli`/`sti`）を許可する箇所。
///
/// 排他は `common::critical` の [`InterruptGuard`]/`Locked<T>` の裏に閉じる決まりで
/// ある（ADR-0023 §3 の seam整備）。それでも直接 `cli`/`sti` が要る箇所は存在し、
/// **いずれも「共有データの排他」以外の目的**である。目的別に許可し、リスト外の
/// 出現は FAIL させる。
///
/// # 行番号ではなく所属アイテム名で識別する
///
/// 行番号は編集のたびに動くので使わない。所属する関数名（`global_asm!` の中は
/// `global_asm!`）で識別する。**関数名を変えると検査が落ちる**が、これは欠点では
/// なく利点である。許可リストは「意図的に承認したもの」の記録なので、コードが
/// 変わったら再承認を求めるのが正しい。
///
/// # この検査が守れないこと
///
/// 検出できるのは「新しい直接 `cli`/`sti` の追加」だけである。**許可済み箇所の
/// 中身が排他目的に変質したことは検出できない**（文字列走査では意図は見えない）。
struct DirectInterruptControlSite {
    /// ワークスペース相対パス。
    file: &'static str,
    /// 所属する関数名。`global_asm!` の中は `"global_asm!"`。
    item: &'static str,
    /// なぜ直接触ってよいのか。
    reason: &'static str,
}

/// 直接 `cli`/`sti` の許可リスト（[`DirectInterruptControlSite`] 参照）。
const DIRECT_INTERRUPT_CONTROL_ALLOWLIST: &[DirectInterruptControlSite] = &[
    // (a) 排他の実装本体。ここが「排他の所在」であり、他は全部これを使う。
    DirectInterruptControlSite {
        file: "common/src/critical.rs",
        item: "enter",
        reason: "InterruptGuard::enter そのもの（排他の実装本体）",
    },
    DirectInterruptControlSite {
        file: "common/src/critical.rs",
        item: "drop",
        reason: "InterruptGuard::drop の復元（排他の実装本体）",
    },
    // (b) 起動の一度きり。スコープを抜けたら復元する意味を持たない恒久的な禁止。
    DirectInterruptControlSite {
        file: "kernel/src/main.rs",
        item: "_start",
        reason: "M4-d まで恒久的に禁止する起動時の一度きり（ADR-0014）",
    },
    // (c)(d)(e) 割り込み許可状態の遷移。sti は検証 7 項目通過後のみ（ADR-0018 §2）。
    DirectInterruptControlSite {
        file: "kernel/src/interrupts.rs",
        item: "spin_with_interrupts_enabled",
        reason: "sti する箇所の 1 つ（M4-d-1 の期限つきスピン）と観測後の復帰 cli",
    },
    DirectInterruptControlSite {
        file: "kernel/src/interrupts.rs",
        item: "run_timer_loop",
        reason: "sti する箇所の 1 つ（M4-d-2 のタイマループ）と sti;hlt 隣接・上限到達時の cli",
    },
    // (f) テスト経路。復元経路そのものを実証するので直接触る必要がある。
    DirectInterruptControlSite {
        file: "kernel/src/main.rs",
        item: "trigger_critical_test",
        reason: "IF=1 で enter した場合の復元経路を実証する検査（critical-test）",
    },
    // (g) **例外: ここは共有データの排他である。** InterruptGuard を使えない asm 文脈
    // なので生の cli/sti で守っている。**BKL では同一コアの割り込みしか防げないため
    // 再検討が要る**（deferred-decisions.md の「per-CPU seam が MAX_CPUS > 1 で…」の
    // 隣に論点として記録した）。
    DirectInterruptControlSite {
        file: "kernel/src/task.rs",
        item: "global_asm!",
        reason: "GPR_BUF（A/B 共有）の store と照合を守る排他。asm 文脈で InterruptGuard を \
                 使えないための例外。BKL で再検討（複数コアでは防げない）",
    },
];

/// 許可リストに無い直接の割り込み制御を探す。
///
/// 対象は `cpu::disable_interrupts` / `cpu::enable_interrupts`
/// （`enable_interrupts_and_halt` を含む）の呼び出しと、`asm!`/`global_asm!` 内の
/// 生の `"cli"` / `"sti"`。
///
/// `common/src/cpu.rs` は除外する（primitive の定義本体で、命令そのものはここに
/// 集約されている）。`halt_forever` の `cli; hlt` も同ファイルなので自動的に外れる
/// （停止用であって排他ではない）。
/// # 数の単位
///
/// 許可リストのエントリ数（`file` + `item` の組）と、実際の**出現数**（`cli`/`sti` を
/// 含む行数）は別の単位である。1 つの関数に複数の出現があれば 1 エントリで複数
/// 出現になる（例: `run_timer_loop` は `sti` / `sti;hlt` / 上限到達時の `cli`）。
/// 数字を並べたときに取り違えないよう、`approved_occurrences` で出現数を数えて
/// 呼び出し側が両方を表示できるようにする。
fn find_unapproved_interrupt_control(
    workspace_root: &Path,
    approved_occurrences: &mut usize,
) -> Result<Vec<String>> {
    // SAFETY 検査と同じ理由で、追跡済みだけでなく未追跡のファイルも見る
    // （新規ファイルの最初の検査が素通りするのを防ぐ）。
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "*.rs",
        ])
        .output()
        .context("failed to list Rust sources")?;
    if !output.status.success() {
        bail!("git ls-files failed while collecting Rust sources");
    }
    let listing = String::from_utf8(output.stdout).context("git ls-files produced non-UTF-8")?;

    let mut findings = Vec::new();
    for relative in listing.lines().filter(|l| !l.is_empty()) {
        // primitive の定義本体は対象外。
        if relative == "common/src/cpu.rs" {
            continue;
        }
        // xtask はホスト上のビルドツールで、ring 0 の命令を実行しえない。この
        // 検査自身の実装（needle の文字列リテラルを含む）もここに入る。
        if relative.starts_with("xtask/") {
            continue;
        }
        let path = workspace_root.join(relative);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let mut current_item = "<file scope>";
        let mut in_global_asm = false;
        // `asm!` / `global_asm!` の**塊の中にいるか**。生の命令は `asm!(` と別の行に
        // 書かれるので、塊全体を追う必要がある（`asm!` を含む行だけを見ていたときは
        // 複数行の `asm!` 内の `cli` を取りこぼした）。開き `asm!(` から、trim 後に
        // `);` で始まる行までを塊とする。
        //
        // **同じ行で閉じる呼び出し（`asm!("cli")` の 1 行形）では塊に入れない。**
        // 入れてしまうと閉じ `);` が独立した行に現れないため `in_asm` が解除されず、
        // **そのファイルの残り全部を asm の中と誤認する**。実測では、単一行の
        // `global_asm!` を 1 つ置くだけで `current_item` が固定され、`interrupts.rs` の
        // 許可済み 5 箇所すべてが所属名の不一致で未許可と判定された（FAIL する方向
        // なので静かには壊れないが、正当な 1 行形を書くと検査が使えなくなる）。
        let mut in_asm = false;
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            // この行が `asm!` 呼び出しを開いているか（1 行で閉じる形を含む）。
            let opens_asm = line.contains("asm!(");
            let closes_on_same_line = opens_asm && asm_call_closes_on_same_line(line);

            if opens_asm {
                if !closes_on_same_line {
                    in_asm = true;
                    if line.contains("global_asm!(") {
                        in_global_asm = true;
                        current_item = "global_asm!";
                    }
                }
            } else if in_asm && trimmed.starts_with(");") {
                in_asm = false;
                if in_global_asm {
                    in_global_asm = false;
                    current_item = "<file scope>";
                }
            } else if !in_global_asm {
                if let Some(name) = function_name_declared_on(line) {
                    current_item = name;
                }
            }

            // ドキュメントコメントと行コメントは対象外（説明文で言及するのは自由）。
            if trimmed.starts_with("///") || trimmed.starts_with("//!") || trimmed.starts_with("//")
            {
                continue;
            }

            // 1 行で閉じる形は `in_asm` に入れないので、その行自体は `opens_asm` で拾う。
            let hit = (in_asm || opens_asm) && mentions_raw_instruction(line)
                || mentions_interrupt_primitive(line);

            if !hit {
                continue;
            }

            let approved = DIRECT_INTERRUPT_CONTROL_ALLOWLIST
                .iter()
                .any(|site| site.file == relative && site.item == current_item);
            if approved {
                *approved_occurrences += 1;
                continue;
            }
            findings.push(format!(
                "{relative}:{} (in {current_item}): {}",
                index + 1,
                line.trim().chars().take(60).collect::<String>()
            ));
        }
    }
    Ok(findings)
}

/// `asm!` 呼び出しがその行の中で閉じているか（`asm!("cli")` の 1 行形）。
///
/// `asm!(` の `(` から括弧を数え、その行の終わりで残高が 0 なら閉じている。
/// 文字列リテラルの中の括弧は数えない（asm のオペランド文字列に `(` が現れても
/// 残高を狂わせないため）。
///
/// # 守れない範囲
///
/// - `line.find("asm!(")` は**最初の 1 つ**しか見ない。1 行に複数の `asm!` 呼び出しが
///   あると 2 つ目以降を見落とす。
/// - 文字列内のエスケープ `\"` で `in_string` が誤って反転する。
///
/// どちらも現在のコードには該当箇所が無く実害は無い。これらは
/// `docs/verification-coverage.md` に挙げた「検出をブロック追跡から切り離す」提案の
/// 根拠 3 件目でもある（その場しのぎのスキャナでソースを解析していることが原因）。
fn asm_call_closes_on_same_line(line: &str) -> bool {
    let Some(position) = line.find("asm!(") else {
        return false;
    };
    let mut depth = 0i32;
    let mut in_string = false;
    for character in line[position + "asm!".len()..].chars() {
        match character {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// その行の文字列リテラルが生の `cli` / `sti` 命令を含むか（`asm!` の中）。
///
/// # 引用符の内側の空白まで見る必要がある
///
/// asm のオペランドは `"  cli",` のように**引用符の内側にインデントを付けて**
/// 書かれる。当初 `"cli"` の完全一致で探していたため、`kernel/src/task.rs` の
/// `global_asm!` にある `"  cli"` / `"  sti"`（GPR_BUF を守る唯一の「排他目的」の
/// 直接操作）を**取りこぼしていた**。許可リストに載っているのに検出されない、
/// つまりその許可エントリが死んでいて、同じ書き方で新しく追加された `cli` も
/// 素通りする状態だった。
///
/// そこで各文字列リテラルを取り出して trim し、先頭トークン（`;` `,` を除く）が
/// `cli` / `sti` かで判定する。`"sti-check 1: ..."` のようなログ文字列は先頭
/// トークンが `sti-check` になるので一致しない。
fn mentions_raw_instruction(line: &str) -> bool {
    line.split('"').skip(1).step_by(2).any(|literal| {
        literal
            .trim()
            .split(|c: char| c.is_whitespace() || c == ';' || c == ',')
            .next()
            .is_some_and(|token| token == "cli" || token == "sti")
    })
}

/// その行が割り込み制御 primitive を呼んでいるか。
///
/// `may_enable_interrupts` のような別の識別子の一部を拾わないよう、直前の文字が
/// 識別子構成文字でないことを確かめる。
fn mentions_interrupt_primitive(line: &str) -> bool {
    ["disable_interrupts", "enable_interrupts"]
        .iter()
        .any(|needle| {
            let mut rest = line;
            let mut base = 0usize;
            while let Some(position) = rest.find(needle) {
                let absolute = base + position;
                let preceded_by_identifier = line[..absolute]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
                if !preceded_by_identifier {
                    return true;
                }
                base = absolute + needle.len();
                rest = &line[base..];
            }
            false
        })
}

/// その行が宣言している関数名（`fn` の直後の識別子）。宣言でなければ `None`。
fn function_name_declared_on(line: &str) -> Option<&str> {
    let position = line.find("fn ")?;
    // `fn` の前が識別子構成文字なら別の語の一部（`asm_fn` など）。
    if line[..position]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let rest = line[position + "fn ".len()..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let name = &rest[..end];
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// `// SAFETY:` コメントを伴わない `unsafe` ブロックを探す。
///
/// `unsafe` ブロックには、なぜ safe に書けないのかと、何を前提に安全性が
/// 保たれるのか（invariant）を `// SAFETY:` コメントとして添える決まりで
/// ある。
///
/// # 走査の規則
///
/// `unsafe {` の行から上へ遡り、**最初に現れる非空行**を見る。それが `//`
/// で始まる行なら、そこから連続するコメント塊の中に `SAFETY` があるかを見る。
/// コメント塊でなければ、間に挟まってよいものだけを読み飛ばす。
///
/// 読み飛ばすのは属性（`#[...]`）と、`let ... =` のように `unsafe` ブロックを
/// 右辺に取る行の左側だけである。これらは SAFETY コメントと `unsafe` の間に
/// 正当に挟まりうる。それ以外の行に当たったら、コメントは無いものとする。
///
/// ドキュメントコメント（`///` と `//!`）の中の `unsafe {` は対象外。
/// 使用例を書いただけの行まで拾うと、直せない指摘が出続ける。
fn find_unsafe_without_safety_comment(workspace_root: &Path) -> Result<Vec<String>> {
    // **追跡済みだけでなく、未追跡のファイルも見る。**
    //
    // 以前は `git ls-files "*.rs"` で追跡済みだけを列挙していた。ところが
    // 新しいファイルを書いてから検査を通し、その後に `git add` して
    // コミットする、という自然な順序だと、**そのファイルの最初の検査は
    // 追跡される前に走る。** 素通りしたまま「通った」と報告され、次に
    // 検査が走るまで誰も気づかない。実際に `kernel/src/paging/verify.rs`
    // がこれで 2 コミットのあいだ見逃されていた。
    //
    // `--cached --others --exclude-standard` にすると、追跡済みと、
    // 無視されていない未追跡の両方が出る。`--exclude-standard` を付けるのは
    // `target/` の中を拾わないためである。
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "*.rs",
        ])
        .output()
        .context("failed to list Rust sources")?;
    if !output.status.success() {
        bail!("git ls-files failed while collecting Rust sources");
    }
    let listing = String::from_utf8(output.stdout).context("git ls-files produced non-UTF-8")?;

    let mut findings = Vec::new();
    for relative in listing.lines().filter(|l| !l.is_empty()) {
        let path = workspace_root.join(relative);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let lines: Vec<&str> = source.lines().collect();

        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("///") || trimmed.starts_with("//!") {
                continue;
            }
            if !contains_unsafe_block_opener(line) {
                continue;
            }
            if has_safety_comment_above(&lines, index) {
                continue;
            }
            findings.push(format!(
                "{relative}:{}: {}",
                index + 1,
                line.trim().chars().take(70).collect::<String>()
            ));
        }
    }
    Ok(findings)
}

/// その行が `unsafe` ブロックを開いているか。
///
/// `unsafe fn` の宣言は対象にしない。関数側の契約は `# Safety` セクションで
/// 書く決まりで、ブロックの `// SAFETY:` とは役割が違う。
fn contains_unsafe_block_opener(line: &str) -> bool {
    let mut rest = line;
    while let Some(position) = rest.find("unsafe") {
        let after = rest[position + "unsafe".len()..].trim_start();
        let before_is_boundary = position == 0
            || !rest[..position]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if before_is_boundary && after.starts_with('{') {
            return true;
        }
        rest = &rest[position + "unsafe".len()..];
    }
    false
}

fn has_safety_comment_above(lines: &[&str], index: usize) -> bool {
    let mut cursor = index;
    while cursor > 0 {
        cursor -= 1;
        let trimmed = lines[cursor].trim();
        if trimmed.is_empty() {
            return false;
        }
        if trimmed.starts_with("//") {
            // コメント塊に入った。塊を上へ辿って SAFETY を探す。
            let mut scan = cursor;
            loop {
                let text = lines[scan].trim();
                if !text.starts_with("//") {
                    return false;
                }
                if text.contains("SAFETY") {
                    return true;
                }
                if scan == 0 {
                    return false;
                }
                scan -= 1;
            }
        }
        // 属性と、`unsafe` ブロックを右辺に取る行の左側だけ読み飛ばす。
        if trimmed.starts_with("#[") || trimmed.starts_with("#!") || trimmed.ends_with('=') {
            continue;
        }
        return false;
    }
    false
}

/// コミットメッセージの文体規約を適用し始めた時点。
///
/// これより前のコミットは規約を定める前に書かれたもので、違反ではない。
///
/// # なぜハッシュではなく日付なのか
///
/// **ハッシュを埋め込むと履歴の書き換えで壊れる。** 書き換えると全コミットの
/// ハッシュが変わり、埋め込んだ値は存在しないコミットを指す。そうなると
/// `git log` が失敗して検査自体が動かなくなり、`git bisect` の途中でも
/// 同じことが起きる（bisect は実際に使っている手法である。
/// `docs/troubleshooting.md` の alt-offset の項を参照）。
///
/// 日付なら、書き換えでコミッタ日時が保たれる限り壊れない。タグを使う案も
/// あるが、タグは書き換え後に貼り直しが要る。
///
/// 値の根拠は、規約に沿って書いた最初のコミットが 2026-07-22 08:48:45、
/// その 1 つ前が 08:07:52 で、その間にコミットが無いことである。
/// 境界のちょうど中間を採ってある。どちらのコミットの秒とも一致させて
/// いないので、書き換えで秒がずれても境界は動かない。
///
/// # タイムゾーンを明示する
///
/// オフセットを省くと、git は実行環境のローカルタイムゾーンとして解釈する。
/// **境界の前後の余裕は前後 20 分程度しかないので、TZ が 1 時間ずれれば
/// 境界を飛び越える。** 別のマシンで動かしたときや TZ 設定を変えたときに、
/// 検査対象が静かに変わることになる。ISO 8601 でオフセットまで書く。
const COMMIT_STYLE_SINCE: &str = "2026-07-22T08:30:00+09:00";

/// コミットメッセージに和文と英数字の間の半角空白が無いことを見る。
///
/// 検出するのは「かな・カタカナ・漢字」と「英数字・括弧」が半角空白 1 個を
/// 挟んで隣り合う形だけである。英単語どうしの空白や、コード片の内部は
/// 対象にしない。
fn check_commit_message_style(workspace_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "log",
            "--format=%h%x1f%s%x1e",
            &format!("--since={COMMIT_STYLE_SINCE}"),
            "HEAD",
        ])
        .output()
        .context("failed to read commit subjects")?;
    if !output.status.success() {
        bail!("git log failed while reading commit subjects");
    }
    let text = String::from_utf8(output.stdout).context("git log produced non-UTF-8")?;

    let mut findings = Vec::new();
    for record in text.split('\u{1e}') {
        let record = record.trim_start_matches('\n');
        let Some((hash, subject)) = record.split_once('\u{1f}') else {
            continue;
        };
        if japanese_ascii_gap(subject) {
            findings.push(format!("{hash}: {subject}"));
        }
    }
    Ok(findings)
}

fn japanese_ascii_gap(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for window in chars.windows(3) {
        if window[1] != ' ' {
            continue;
        }
        let (left, right) = (window[0], window[2]);
        if (is_japanese(left) && is_ascii_word_start(right))
            || (is_ascii_word_end(left) && is_japanese(right))
        {
            return true;
        }
    }
    false
}

fn is_japanese(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}')
}

fn is_ascii_word_start(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '(' || c == '（'
}

fn is_ascii_word_end(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == ')' || c == '）'
}

/// 意図的に壊した経路を有効にする feature の接頭辞・名前。
///
/// **既定ビルドにこれらが入ってはならない。** 入ったまま出荷すると、
/// 壊れた状態で測った結果を正常な結果として扱うことになる。
const SABOTAGE_FEATURES: &[&str] = &[
    "misalign-test",
    "no-eoi-test",
    "alt-offset-test",
    "tiny-key-buffer",
    "paging-test",
    "exception-test",
    "critical-test",
    "interrupt-test",
    "panic-test",
    "gfx-test-pattern",
    "highhalf-no-identity-in-boot-pt",
    "highhalf-bad-high-slot",
    "highhalf-no-kernel-high-in-live-table",
    "highhalf-trampoline-absolute-ref",
    "highhalf-remove-verify-fail",
    "highhalf-remove-before-highify",
    "highhalf-panic-after-remove",
];

/// 割り込み層の境界（`kernel/src/irq/`）の内部が、外へ公開されていないことを
/// 確かめる（seam整備の項目1、S0-a）。
///
/// # コンパイラが保証することと、この検査が守ること
///
/// `irq/mod.rs` が `mod pic;`（非公開）と宣言している限り、境界の外から
/// `crate::irq::pic::…` と書くとビルドが落ちる。**参照が無いこと自体は
/// コンパイラが保証する。** この検査が守るのは、その保証が将来の単純化で
/// 外されないことである。可視性修飾を 1 つ足すだけで保証は消えるが、
/// ビルドは通り続けるので、検査が無ければ気づけない。
///
/// # 見るもの
///
/// `irq/` 配下の**全ファイル**を対象に、`mod` 宣言と `use` に private 以外の
/// 可視性修飾（`pub` / `pub(crate)` / `pub(super)` / `pub(in …)`）が付いて
/// いないことを見る。`mod.rs` だけを見る形だと、配下に新しいファイルを作って
/// そこから再公開する経路を捕まえられない。
///
/// # 守らないもの（実態より強く書かない）
///
/// - **境界の公開関数が生の値を返す形は捕まらない。** `pub fn read_masks()
///   -> (u8, u8)` を `irq` に足せば、可視性は private のままでも呼び出し側は
///   PIC の語彙を持てる。これはレビューで守る。
/// - **コメント内の言及は対象外。** コメントは実行されないので結合を作らない。
///   `pic` は `topic` の部分文字列でもあり、素朴な一致は誤検出源になる。
/// - **生の I/O ポート直叩きは対象外。** 境界の外から `outb(0x21, …)` と書けば
///   IMR は触れる。現在そのような箇所は無いことを実測で確認しているが、
///   この検査はそれを見ていない。
///
/// # 保守的に禁じている形もある（実態より強く書かない）
///
/// 禁じている形のうち、**外から実際に到達できるものと、保守的に禁じている
/// だけのものが混ざっている。** 前者だけを見て「全部が穴だった」と読まない
/// ように、実測（S0-a の 1c）の結果をそのまま残す。
///
/// - **判定は行頭だけを見るが、インデントされた宣言を見逃す穴にはならない。**
///   インデントされるのはインライン `mod` の内側であり、その親が非公開なら
///   下と同じ理屈で外から到達できず、親が公開なら**親の行が行頭で拾われる**。
///   「行頭だけなのは手抜きでは」と考えて作り直すと、この性質が失われる。
/// - `pub(self) mod pic;` は実際には非公開と同義だが FAIL させる。これも下の
///   「保守的に禁じている形」に含まれる。
///
/// 実測（S0-a の 1c）の結果は次のとおり。
///
/// - `pub mod pic;` → 到達できる（`crate::irq::pic::MASK_ALL` がビルドを通る）
/// - `pub(crate) mod pic;` → クレート内から到達できる
/// - `irq/mod.rs` での `pub use pic::…` → 到達できる
/// - `irq/mod.rs` での `pub type Alias = pic::Inner;` → 到達できる（別名を通して
///   内部の型を掴める。`pub use` と同じ性質の実在の穴である）
/// - **配下の非公開ファイルからの `pub(crate) use super::pic::*;` → 到達できない。**
///   Rust の実効可視性は親モジュールの可視性で頭打ちになるので、
///   `crate::irq::compat::MASK_ALL` は `E0603: module compat is private` になる。
///   これは漏れる穴ではなく、**保守的に禁じているだけ**である。禁じたままに
///   するのは、`mod compat;` を `pub mod compat;` にするだけで漏れる形へ
///   変わるためで、その 1 文字の差を検査の外に置きたくない。
fn find_boundary_visibility_leaks(workspace_root: &Path) -> Result<Vec<String>> {
    // 列挙は SAFETY 検査と同じ理由で `--cached --others --exclude-standard`
    // にする。**未追跡の新規ファイルこそ検査が要る**（配下に新しいファイルを
    // 作って再公開する経路が、追跡される前に素通りするのを防ぐ）。
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "*.rs",
        ])
        .output()
        .context("failed to list Rust sources")?;
    if !output.status.success() {
        bail!("git ls-files failed while collecting Rust sources");
    }
    let listing = String::from_utf8(output.stdout).context("git ls-files produced non-UTF-8")?;

    let mut findings = Vec::new();
    for relative in listing.lines().filter(|l| !l.is_empty()) {
        if !relative.starts_with("kernel/src/irq/") {
            continue;
        }
        let path = workspace_root.join(relative);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(item) = visibility_qualified_mod_or_use(trimmed) {
                findings.push(format!(
                    "{relative}:{}: {item} is exported out of the interrupt-layer boundary: {}",
                    index + 1,
                    trimmed.trim_end()
                ));
            }
        }
    }
    Ok(findings)
}

/// 行頭が「private 以外の可視性修飾 + `mod` / `use`」なら、その語を返す。
///
/// 判定は行の先頭だけを見る。`mod` と `use` はアイテム宣言なので、可視性修飾は
/// 必ず行頭に来る（インラインモジュールの中でも、その行の先頭に来る）。
fn visibility_qualified_mod_or_use(trimmed: &str) -> Option<&'static str> {
    let rest = trimmed.strip_prefix("pub")?;
    // `pub(crate)` / `pub(super)` / `pub(in …)` の括弧を読み飛ばす。
    let rest = match rest.strip_prefix('(') {
        Some(after) => after.split_once(')')?.1,
        None => rest,
    };
    let rest = rest.trim_start();
    if rest.starts_with("mod ") {
        Some("mod")
    } else if rest.starts_with("use ") {
        Some("use")
    } else if rest.starts_with("type ") {
        // `pub type Alias = pic::Inner;` は `mod` でも `use` でもないが、
        // **別名を通して内部の型を外から掴める**（実測で到達を確認した）。
        // `pub use` と同じ性質の実在の穴なので拾う。
        Some("type")
    } else {
        None
    }
}

/// kernel の既定 feature に仕込みが混ざっていないことを確かめる。
///
/// `default` から推移的に辿って、[`SABOTAGE_FEATURES`] のいずれかに
/// 行き着かないことを見る。`kernel/Cargo.toml` の `[features]` を
/// そのまま読む（`name = ["a", "b"]` の形しか使っていない）。
fn check_default_features_are_clean(workspace_root: &Path) -> Result<Vec<String>> {
    let manifest = fs::read_to_string(workspace_root.join("kernel").join("Cargo.toml"))
        .context("failed to read kernel/Cargo.toml")?;

    let mut in_features = false;
    let mut graph: Vec<(String, Vec<String>)> = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_features = trimmed == "[features]";
            continue;
        }
        if !in_features || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((name, rest)) = trimmed.split_once('=') else {
            continue;
        };
        let deps: Vec<String> = rest
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|d| d.trim().trim_matches('"').to_string())
            .filter(|d| !d.is_empty())
            .collect();
        graph.push((name.trim().to_string(), deps));
    }
    if !graph.iter().any(|(name, _)| name == "default") {
        bail!("kernel/Cargo.toml has no `default` feature; the check cannot run");
    }

    // `default` から推移的に辿る。
    let mut reached: Vec<String> = vec!["default".to_string()];
    let mut index = 0;
    while index < reached.len() {
        let current = reached[index].clone();
        index += 1;
        if let Some((_, deps)) = graph.iter().find(|(name, _)| *name == current) {
            for dep in deps {
                if !reached.contains(dep) {
                    reached.push(dep.clone());
                }
            }
        }
    }

    Ok(reached
        .into_iter()
        .filter(|name| SABOTAGE_FEATURES.contains(&name.as_str()))
        .map(|name| format!("`default` reaches the sabotage feature `{name}`"))
        .collect())
}

/// 全構成のビルド・テスト・clippy・fmt を順に実行する。
///
/// **1 つ落ちてもそこで止めない。** 止めると「直しては再実行」を
/// 繰り返すことになり、全体像が分からない。最後にまとめて報告する。
fn cmd_check(full: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let mut failed: Vec<String> = Vec::new();
    let mut total = 0usize;

    for (name, args) in CHECKS {
        total += 1;
        println!("=== xtask check: {name}");
        let status = Command::new("cargo")
            .current_dir(&workspace_root)
            .args(*args)
            .status()
            .with_context(|| format!("failed to invoke cargo for the {name} check"))?;
        if status.success() {
            println!("--- {name}: OK");
        } else {
            println!("--- {name}: FAILED ({status})");
            failed.push((*name).to_string());
        }
    }

    total += 1;
    println!("=== xtask check: unsafe blocks carry a SAFETY comment");
    let missing = find_unsafe_without_safety_comment(&workspace_root)?;
    if missing.is_empty() {
        println!("--- unsafe/SAFETY: OK");
    } else {
        for finding in &missing {
            println!("    {finding}");
        }
        println!("--- unsafe/SAFETY: FAILED ({} block(s))", missing.len());
        failed.push("unsafe/SAFETY".to_string());
    }

    total += 1;
    println!("=== xtask check: direct cli/sti stays on the approved list");
    let mut approved_occurrences = 0usize;
    let unapproved = find_unapproved_interrupt_control(&workspace_root, &mut approved_occurrences)?;
    if unapproved.is_empty() {
        println!(
            "--- direct cli/sti: OK ({} approved entr(y/ies) = file+item pairs, covering {} \
             occurrence(s) = cli/sti lines)",
            DIRECT_INTERRUPT_CONTROL_ALLOWLIST.len(),
            approved_occurrences
        );
    } else {
        for finding in &unapproved {
            println!("    {finding}");
        }
        println!("    approved sites (file / item / reason):");
        for site in DIRECT_INTERRUPT_CONTROL_ALLOWLIST {
            println!("      {} / {} / {}", site.file, site.item, site.reason);
        }
        println!(
            "--- direct cli/sti: FAILED ({} unapproved site(s); exclusion belongs behind \
             InterruptGuard/Locked, or add an entry with a reason)",
            unapproved.len()
        );
        failed.push("direct cli/sti".to_string());
    }

    total += 1;
    println!("=== xtask check: the interrupt-layer boundary keeps its internals private");
    let leaks = find_boundary_visibility_leaks(&workspace_root)?;
    if leaks.is_empty() {
        println!("--- irq boundary: OK (no visibility qualifier on mod/use under kernel/src/irq/)");
    } else {
        for finding in &leaks {
            println!("    {finding}");
        }
        println!(
            "--- irq boundary: FAILED ({} export(s); the boundary must be the only way in)",
            leaks.len()
        );
        failed.push("irq boundary".to_string());
    }

    total += 1;
    println!("=== xtask check: the default kernel build has no sabotage features");
    let sabotage = check_default_features_are_clean(&workspace_root)?;
    if sabotage.is_empty() {
        println!("--- default features: OK");
    } else {
        for finding in &sabotage {
            println!("    {finding}");
        }
        println!("--- default features: FAILED");
        failed.push("default features".to_string());
    }

    total += 1;
    println!("=== xtask check: commit message style (since {COMMIT_STYLE_SINCE})");
    let offenders = check_commit_message_style(&workspace_root)?;
    if offenders.is_empty() {
        println!("--- commit style: OK");
    } else {
        for offender in &offenders {
            println!("    {offender}");
        }
        println!("--- commit style: FAILED ({} commit(s))", offenders.len());
        failed.push("commit style".to_string());
    }

    // トランポリンのバイト単位一致検査（B-2a-5、静的）。既定ビルドの入口 24 バイトが
    // 期待リテラルと一致すること。base 検査なので `--full` でなくても毎回走る。
    total += 1;
    println!("=== xtask check: trampoline byte match (default build)");
    match cmd_highhalf_trampoline_check(&workspace_root, &[], true) {
        Ok(()) => println!("--- trampoline byte match: OK"),
        Err(e) => {
            println!("    {e}");
            println!("--- trampoline byte match: FAILED");
            failed.push("trampoline byte match".to_string());
        }
    }

    let mut retries: Vec<String> = Vec::new();
    if full {
        // QEMU を起動する回帰チェック。1 種類ごとにカーネルをビルドし直して
        // 起動するため重い。既定では走らせない。段階の完了時、`unsafe`・
        // 割り込み・ページテーブル・GDT・IDT に触れた変更のコミット前、
        // ツールチェインやビルド設定を変更したときに走らせる。
        for test in EXCEPTION_TESTS {
            total += 1;
            let name = format!("exception-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_exception_test(test.name)
            });
        }
        for test in CRITICAL_TESTS {
            total += 1;
            let name = format!("critical-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(CRITICAL_TESTS, "critical-test", test.name)
            });
        }
        for test in PAGING_TESTS {
            total += 1;
            let name = format!("paging-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(PAGING_TESTS, "paging-test", test.name)
            });
        }
        for test in STACK_TESTS {
            total += 1;
            let name = format!("stack-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(STACK_TESTS, "stack-test", test.name)
            });
        }
        for test in TASK_TESTS {
            total += 1;
            let name = format!("task-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(TASK_TESTS, "task-test", test.name)
            });
        }
        for test in RING3_TESTS {
            total += 1;
            let name = format!("ring3-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(RING3_TESTS, "ring3-test", test.name)
            });
        }
        for test in SYSCALL_TESTS {
            total += 1;
            let name = format!("syscall-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(SYSCALL_TESTS, "syscall-test", test.name)
            });
        }
        for test in INTERRUPT_TESTS {
            total += 1;
            let name = format!("interrupt-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(INTERRUPT_TESTS, "interrupt-test", test.name)
            });
        }
        total += 1;
        run_regression(
            "interrupt-test keyboard",
            &mut failed,
            &mut retries,
            cmd_keyboard_test,
        );
        total += 1;
        run_regression("panic-test", &mut failed, &mut retries, || {
            cmd_run(true, false, false, false, false)
        });
        // higher-half の破壊確認（B-2a-5）。(a)(b)(c) は QEMU で位置署名 + 定常未到達を
        // 判定、(d) はビルド + トランポリンのバイト不一致を静的に判定。
        for test in HIGHHALF_TESTS {
            total += 1;
            let name = format!("highhalf-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_highhalf_test(test.name)
            });
        }
        total += 1;
        run_regression(
            "highhalf-test trampoline-absolute-ref",
            &mut failed,
            &mut retries,
            || {
                cmd_highhalf_trampoline_check(
                    &workspace_root,
                    &["highhalf-trampoline-absolute-ref"],
                    false,
                )
            },
        );
    }

    println!();
    if !retries.is_empty() {
        println!(
            "xtask check: {} check(s) were retried because the target did not start: {}",
            retries.len(),
            retries.join(", ")
        );
    }
    if failed.is_empty() {
        println!("xtask check: all {total} check(s) passed");
        return Ok(());
    }
    bail!(
        "xtask check: {} of {total} check(s) failed: {}",
        failed.len(),
        failed.join(", ")
    );
}

/// 起動失敗（環境要因）を表すメッセージの目印。
///
/// [`report_did_not_start`] が返すエラーだけがこれを含む。テストの失敗
/// （実装の問題）とは区別されている。
const DID_NOT_START_MARKER: &str = "kernel did not start (environment, not the code)";

/// 回帰チェックを 1 件走らせ、失敗しても止めずに記録する。
///
/// `--full` は「何が壊れているか」を一度に知るためのものなので、
/// 最初の失敗で打ち切らない。
///
/// # 環境要因のときだけ 1 回だけ再試行する
///
/// OVMF はまれに起動に失敗し、シェル / アイドルループへ落ちる。これは
/// 実装の問題ではないので、そのたびに手で再実行するのは無駄である。
/// xtask が起動失敗と判定した場合に限り、自動で 1 回だけやり直す。
///
/// **再試行したことは必ず出力する。** 黙って通すと、環境が悪化して
/// 起動失敗が常態化しても気づけない。回数を数えて最後にまとめて出す。
///
/// テストの失敗（実装の問題）では再試行しない。落ちるものは落ちたまま
/// 報告する。
fn run_regression(
    name: &str,
    failed: &mut Vec<String>,
    retries: &mut Vec<String>,
    mut body: impl FnMut() -> Result<()>,
) {
    println!("=== xtask check: {name}");
    let first = body();
    let error = match first {
        Ok(()) => {
            println!("--- {name}: OK");
            return;
        }
        Err(error) => error,
    };

    let did_not_start = format!("{error:#}").contains(DID_NOT_START_MARKER);
    if !did_not_start {
        println!("--- {name}: FAILED ({error:#})");
        failed.push(name.to_string());
        return;
    }

    println!("--- {name}: the target did not start; retrying once (environment, not the code)");
    retries.push(name.to_string());
    match body() {
        Ok(()) => println!("--- {name}: OK (on the retry)"),
        Err(error) => {
            println!("--- {name}: FAILED ({error:#})");
            failed.push(name.to_string());
        }
    }
}

fn build_kernel_with_features(workspace_root: &Path, features: &[&str]) -> Result<PathBuf> {
    let mut command = Command::new("cargo");
    command.current_dir(workspace_root).args([
        "build",
        "--target",
        KERNEL_TARGET,
        "-p",
        KERNEL_PACKAGE,
        "--bin",
        KERNEL_PACKAGE,
    ]);
    if !features.is_empty() {
        command.args(["--features", &features.join(",")]);
    }
    let status = command
        .status()
        .context("failed to invoke cargo to build the kernel")?;
    if !status.success() {
        bail!("kernel build failed ({status})");
    }

    let elf_path = workspace_root
        .join("target")
        .join(KERNEL_TARGET)
        .join("debug")
        .join(KERNEL_PACKAGE);
    if !elf_path.exists() {
        bail!(
            "kernel build reported success but {} is missing",
            elf_path.display()
        );
    }
    Ok(elf_path)
}

fn build_kernel(workspace_root: &Path, gfx_test: bool) -> Result<PathBuf> {
    let mut command = Command::new("cargo");
    command.current_dir(workspace_root).args([
        "build",
        "--target",
        KERNEL_TARGET,
        "-p",
        KERNEL_PACKAGE,
        "--bin",
        KERNEL_PACKAGE,
    ]);
    if gfx_test {
        command.args(["--features", GFX_TEST_PATTERN_FEATURE]);
    }
    let status = command
        .status()
        .context("failed to invoke cargo to build the kernel")?;

    if !status.success() {
        bail!("kernel build failed ({status})");
    }

    let elf_path = workspace_root
        .join("target")
        .join(KERNEL_TARGET)
        .join("debug")
        .join(KERNEL_PACKAGE);
    if !elf_path.exists() {
        bail!(
            "kernel build reported success but {} is missing",
            elf_path.display()
        );
    }
    Ok(elf_path)
}

/// このプロジェクトで使う OVMF ビルド（Ubuntu の `ovmf` パッケージ）は、
/// ブート可能な `\EFI\BOOT\BOOTX64.EFI` を自動探索するのではなく、既定で
/// 組み込みの UEFI Interactive Shell を起動する（docs/troubleshooting.md
/// 参照。根本原因は未解明で、これは回避策）。そのシェルは起動直後に
/// `startup.nsh` を探して自動実行するため、それを使って明示的に
/// bootloader.efi をチェインロードする。
const STARTUP_NSH: &str = "FS0:\\EFI\\BOOT\\BOOTX64.EFI\r\n";

/// OVMF の既定の起動パス（`\EFI\BOOT\BOOTX64.EFI`）に bootloader.efi を配置
/// した ESP (EFI System Partition) 相当のディレクトリを用意する。QEMU の
/// `fat:` ドライバでこのディレクトリをそのまま仮想 FAT ドライブとして渡せる
/// ため、ディスクイメージファイルを別途作成する必要はない。
fn stage_esp(workspace_root: &Path, bootloader_efi: &Path, kernel_elf: &Path) -> Result<PathBuf> {
    let esp_dir = workspace_root.join("target").join("esp");
    let boot_dir = esp_dir.join("EFI").join("BOOT");
    fs::create_dir_all(&boot_dir)
        .with_context(|| format!("failed to create {}", boot_dir.display()))?;

    let boot_efi = boot_dir.join("BOOTX64.EFI");
    fs::copy(bootloader_efi, &boot_efi).with_context(|| {
        format!(
            "failed to copy {} to {}",
            bootloader_efi.display(),
            boot_efi.display()
        )
    })?;

    let startup_nsh = esp_dir.join("startup.nsh");
    fs::write(&startup_nsh, STARTUP_NSH)
        .with_context(|| format!("failed to write {}", startup_nsh.display()))?;

    // bootloader 側の ELF ローダー（bootloader/src/loader.rs）がここから
    // 読み込む（M2-0c）。
    let kernel_dir = esp_dir.join("zaytos");
    fs::create_dir_all(&kernel_dir)
        .with_context(|| format!("failed to create {}", kernel_dir.display()))?;
    let staged_kernel_elf = kernel_dir.join("kernel.elf");
    fs::copy(kernel_elf, &staged_kernel_elf).with_context(|| {
        format!(
            "failed to copy {} to {}",
            kernel_elf.display(),
            staged_kernel_elf.display()
        )
    })?;

    Ok(esp_dir)
}

fn workspace_root() -> Result<PathBuf> {
    // xtask は常に `<workspace_root>/xtask` に置かれ、cargo run 経由で起動される
    // ため、CARGO_MANIFEST_DIR の親をワークスペースルートとみなせる。
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .parent()
        .map(Path::to_path_buf)
        .context("failed to resolve workspace root from CARGO_MANIFEST_DIR")
}

/// OVMF の変数領域 (NVRAM) は QEMU が起動時に書き込むため、パッケージ配布物を
/// そのまま渡さず target/ovmf/ 配下に書き込み可能なコピーを用意する。
fn prepare_ovmf_vars(workspace_root: &Path) -> Result<PathBuf> {
    let ovmf_dir = workspace_root.join("target").join("ovmf");
    fs::create_dir_all(&ovmf_dir)
        .with_context(|| format!("failed to create {}", ovmf_dir.display()))?;

    // **毎回テンプレートから作り直す。** OVMF はこの varstore を書き換える
    // ので、使い回すと起動項目（`EFI Internal Shell`、PXE、HTTP boot）が
    // 蓄積し、実行を重ねるほど条件が変わっていく。実際、初回だけコピーする
    // 作りだったときは `--full` の 3 回中 3 回で「OVMF がシェルへ落ちる」
    // 事象が起き、毎回作り直すと 2 回中 0 件になった
    // （docs/troubleshooting.md 参照）。
    //
    // 原因をバイト単位まで特定したわけではない。ただ、テストのたびに状態が
    // 持ち越される作りは、それ自体が「実行ごとに条件が変わる」という観測上の
    // 不確実性であり、無くしておく価値がある。費用はファイルコピー 1 回で、
    // 失うのは前回の起動で OVMF が覚えた設定だけである。ZaytOS は毎回同じ
    // ESP から同じ構成で起動するので、覚えていてほしいものは無い。
    let vars_copy = ovmf_dir.join("OVMF_VARS_4M.fd");
    {
        fs::copy(OVMF_VARS_TEMPLATE_PATH, &vars_copy).with_context(|| {
            format!(
                "failed to copy OVMF vars template from {} (is the `ovmf` package installed? `apt install ovmf`)",
                OVMF_VARS_TEMPLATE_PATH
            )
        })?;
    }
    Ok(vars_copy)
}

/// QEMU 起動引数を組み立てる。
///
/// `-no-reboot -no-shutdown -d int,cpu_reset` は常時付与する: これらが無いと
/// 致命的例外発生時に QEMU が無言でリブートを
/// 繰り返し、原因を外部から観測できなくなる。
fn qemu_launch_args(opts: &QemuLaunchOptions) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        // デフォルトの i440FX/PIIX チップセット（レガシー IDE を持つ）を使う。
        // q35 では OVMF がドライブを既定の起動先として自動認識しなかった
        // ため（ADR-0007）、単純な IDE 接続のほうが確実である。
        "-m".into(),
        "256M".into(),
        "-drive".into(),
        format!(
            "if=pflash,format=raw,readonly=on,file={}",
            opts.ovmf_code.display()
        )
        .into(),
        "-drive".into(),
        format!("if=pflash,format=raw,file={}", opts.ovmf_vars.display()).into(),
        // ESP 相当のディレクトリを仮想 FAT ドライブとして渡す。OVMF は既定の
        // 起動パス `\EFI\BOOT\BOOTX64.EFI` を自動的に見つけて起動する。
        "-drive".into(),
        format!("format=raw,file=fat:rw:{}", opts.esp_dir.display()).into(),
        "-serial".into(),
        match opts.serial {
            SerialSink::Stdio => "stdio".into(),
            SerialSink::File(path) => format!("file:{}", path.display()).into(),
        },
        // 既定は none（ADR-0003: シリアルログを唯一の観測手段とする）。
        // `--gui` 指定時のみ実際のウィンドウ（WSLg 経由）を開く。
        "-display".into(),
        match opts.display {
            DisplayMode::None => "none".into(),
            DisplayMode::Gui => "gtk".into(),
        },
        "-no-reboot".into(),
        "-no-shutdown".into(),
        // KVM では例外・割り込みの大半が CPU 側で処理され QEMU を経由しない
        // ため、`-d int` の記録はほとんど残らない。デバッグは TCG（既定）、
        // 計測は KVM、という使い分けをすること（troubleshooting.md 参照）。
        "-d".into(),
        "int,cpu_reset".into(),
        // `-d` の出力先を明示的にファイルへ分離する。指定しない場合 QEMU 自身の
        // stderr に出て、`-serial stdio` のシリアル出力と混ざってしまい、
        // ターミナルでの可読性が大きく落ちる。
        "-D".into(),
        opts.debug_log.into(),
    ];

    match opts.accelerator {
        Accelerator::Tcg => {}
        Accelerator::Kvm => {
            args.push("-accel".into());
            args.push("kvm".into());
            // KVM では TSC がホストの実周波数で進む。計測用途では
            // ホスト側の TSC をそのまま見せる方が解釈しやすい。
            args.push("-cpu".into());
            args.push("host".into());
        }
    }

    if let Some(monitor_socket) = opts.monitor_socket {
        args.push("-monitor".into());
        args.push(format!("unix:{},server,nowait", monitor_socket.display()).into());
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_options<'a>(serial: &'a SerialSink, debug_log: &'a Path) -> QemuLaunchOptions<'a> {
        QemuLaunchOptions {
            ovmf_code: Path::new("/x/CODE.fd"),
            ovmf_vars: Path::new("/y/VARS.fd"),
            esp_dir: Path::new("/z/esp"),
            serial,
            debug_log,
            display: DisplayMode::None,
            monitor_socket: None,
            accelerator: Accelerator::Tcg,
        }
    }

    fn joined_args(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn qemu_args_always_include_failure_visibility_flags() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a == "-no-reboot"));
        assert!(joined.iter().any(|a| a == "-no-shutdown"));

        let d_pos = joined
            .iter()
            .position(|a| a == "-d")
            .expect("-d flag missing");
        assert_eq!(joined[d_pos + 1], "int,cpu_reset");
    }

    #[test]
    fn qemu_args_reference_given_ovmf_and_esp_paths() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a.contains("/x/CODE.fd")));
        assert!(joined.iter().any(|a| a.contains("/y/VARS.fd")));
        assert!(joined.iter().any(|a| a.contains("fat:rw:/z/esp")));
    }

    #[test]
    fn qemu_args_use_serial_file_sink_when_requested() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let sink = SerialSink::File(PathBuf::from("/tmp/serial.log"));
        let opts = base_options(&sink, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(joined.iter().any(|a| a == "file:/tmp/serial.log"));
    }

    #[test]
    fn qemu_args_separate_debug_log_from_serial() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        let d_capital_pos = joined
            .iter()
            .position(|a| a == "-D")
            .expect("-D flag missing");
        assert_eq!(joined[d_capital_pos + 1], "/z/qemu-debug.log");
    }

    #[test]
    fn qemu_args_default_display_is_none() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        let pos = joined
            .iter()
            .position(|a| a == "-display")
            .expect("-display flag missing");
        assert_eq!(joined[pos + 1], "none");
    }

    #[test]
    fn qemu_args_gui_display_selects_gtk() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let mut opts = base_options(&SerialSink::Stdio, &debug_log);
        opts.display = DisplayMode::Gui;
        let joined = joined_args(&qemu_launch_args(&opts));

        let pos = joined
            .iter()
            .position(|a| a == "-display")
            .expect("-display flag missing");
        assert_eq!(joined[pos + 1], "gtk");
    }

    #[test]
    fn qemu_args_include_monitor_socket_when_requested() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let monitor_socket = PathBuf::from("/tmp/mon.sock");
        let mut opts = base_options(&SerialSink::Stdio, &debug_log);
        opts.monitor_socket = Some(&monitor_socket);
        let joined = joined_args(&qemu_launch_args(&opts));

        let pos = joined
            .iter()
            .position(|a| a == "-monitor")
            .expect("-monitor flag missing");
        assert_eq!(joined[pos + 1], "unix:/tmp/mon.sock,server,nowait");
    }

    #[test]
    fn qemu_args_omit_monitor_by_default() {
        let debug_log = PathBuf::from("/z/qemu-debug.log");
        let opts = base_options(&SerialSink::Stdio, &debug_log);
        let joined = joined_args(&qemu_launch_args(&opts));

        assert!(!joined.iter().any(|a| a == "-monitor"));
    }

    /// 既定は TCG のまま。これまでの全マイルストーンを TCG で検証してきて
    /// おり、既定を変えると挙動差の切り分け軸が増える（ADR-0015 Addendum）。
    #[test]
    fn qemu_args_do_not_enable_kvm_by_default() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let serial = SerialSink::Stdio;
        let args = joined_args(&qemu_launch_args(&base_options(&serial, &debug_log)));
        assert!(!args.iter().any(|a| a == "-accel"));
        assert!(!args.iter().any(|a| a == "kvm"));
    }

    #[test]
    fn qemu_args_enable_kvm_when_requested() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let serial = SerialSink::Stdio;
        let mut options = base_options(&serial, &debug_log);
        options.accelerator = Accelerator::Kvm;
        let args = joined_args(&qemu_launch_args(&options));

        let accel = args.iter().position(|a| a == "-accel").expect("-accel");
        assert_eq!(args[accel + 1], "kvm");
        // 計測時は TSC の解釈を単純にするためホストの CPU をそのまま見せる。
        let cpu = args.iter().position(|a| a == "-cpu").expect("-cpu");
        assert_eq!(args[cpu + 1], "host");
    }

    /// KVM を選んでも、失敗を可視化するオプションは外さない。取れる情報は
    /// 減るが、外すと「何も出ない」理由が分からなくなる。
    #[test]
    fn kvm_still_keeps_the_failure_visibility_flags() {
        let debug_log = PathBuf::from("/dummy/qemu-debug.log");
        let serial = SerialSink::Stdio;
        let mut options = base_options(&serial, &debug_log);
        options.accelerator = Accelerator::Kvm;
        let args = joined_args(&qemu_launch_args(&options));
        assert!(args.iter().any(|a| a == "-no-reboot"));
        assert!(args.iter().any(|a| a == "-no-shutdown"));
        assert!(args.iter().any(|a| a == "int,cpu_reset"));
    }

    #[test]
    fn panic_markers_present_requires_both_markers() {
        let dir = env::temp_dir().join(format!(
            "zaytos-xtask-test-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("serial.log");

        fs::write(&path, "boot ok\n").unwrap();
        assert!(!panic_markers_present(&path));

        fs::write(&path, format!("{PANIC_MARKER_HEADER} boom\n")).unwrap();
        assert!(!panic_markers_present(&path));

        fs::write(
            &path,
            format!("{PANIC_MARKER_HEADER} boom\n{PANIC_MARKER_HALT}\n"),
        )
        .unwrap();
        assert!(panic_markers_present(&path));

        let _ = fs::remove_dir_all(&dir);
    }
}
