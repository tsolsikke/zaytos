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
            "sti-check summary: 4. interrupt delivery vectors are set as intended = UNVERIFIABLE",
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
    // IRQ スタブ表の索引を 1 本ずらし、配置検証が働くことを確認する。
    //
    // **この検査は S6-a まで「落ちるところを一度も見ていない」側だった。**
    // 既存の破壊 38 件のいずれもこの検査を落としていないことを確かめたうえで
    // 置いた（`verification-coverage.md`）。
    //
    // **ずらすのはベクタ `0x3F` の 1 本だけである。** 既定ビルドでそこへ
    // 割り込みは届かないので、**振る舞いは変わらず検査だけが落ちる。**
    // 落ちた結果として ADR-0018 §2 の門が `sti` を拒む。
    CriticalTest {
        name: "irq-stub-offset",
        feature: "idt-irq-stub-offset-test",
        expected_markers: &[
            "irq stub table ok=false",
            "sti-check summary: 3. IDT loaded, all exception gates present = FAILED",
            "the pre-sti checks did not pass; refusing to sti",
        ],
        forbidden_markers: &["irq stub table ok=true", "sti: interrupts are now enabled"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
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
    // S7-c: 新しいアドレス空間へカーネルの上位を写さずに CR3 を差し替える。
    //
    // **到達条件4（CR3 切り替え後もカーネルが動くこと）の対照である。** 既定ビルドでは
    // 切り替えた後の行が出る。**上位を写さないと、切り替えた瞬間に命令フェッチが
    // 翻訳できなくなり、その行が出ない。**
    //
    // **「動いた」を主張する検査には、動かない側が要る。** 既定ビルドの行だけでは、
    // 切り替えが実際に効いているのか何もしていないのかを区別できない。
    CriticalTest {
        name: "addrspace-no-kernel-share",
        feature: "addrspace-no-kernel-share",
        expected_markers: &["address-space: built a second address space"],
        forbidden_markers: &["address-space: still running after the switch"],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // S9-a: map_4kib の書き込み可否の引数を無視し、葉を常に W=1 で作る。
    //
    // **既定ビルドの主張は「writable=false で張った葉に Ring 3 が書くと #PF になる」**
    // で、ring3-vectors の 6 本目（#PF-write-ro）がそれを見ている。この破壊は
    // W=0 を作らせないので、Ring 3 の書きが通り、命令列の末尾に置いた ud2 が
    // ベクタ 6 で畳まれる。判定行が「ベクタが違う」と言って止まる。
    //
    // **feature 名は paging-test の傘に入れていない。** 傘に入れると Ring 3 の
    // 検証自体が載らず、破壊を観測する側が消える。
    CriticalTest {
        name: "map-force-writable",
        feature: "map-force-writable",
        expected_markers: &["#PF-write-ro folded with vector=6", "halting"],
        forbidden_markers: &["ring3-vectors: all six Ring 3 faults"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
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
    // 遠征の RSP0 据え付けを落とす。#GP がメインのスタックで走る。
    //
    // S8-c で捕まえる場所が変わった。フレームの健全性判定の従（ハンドラ自身が
    // そのベクタの想定スタックにいること）が先に落ちるので、畳まずに dump+halt する。
    // 以前は畳んだ後に遠征の呼び出し側が handler_in_excursion で捕まえていた
    // （"RSP0 did not take effect"）。**この破壊は健全性判定の従を実証する側でもある。**
    CriticalTest {
        name: "drop-rsp0",
        feature: "ring3-test-drop-rsp0",
        expected_markers: &["exception frame is not trustworthy", "halting"],
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
    // 例外フレームの CS を既知でない値へ差し替える。畳みの3条件は通り、健全性判定の
    // 主（CS が既知のセレクタであること）だけが落ちて dump+halt する。
    // 畳めるはずの Ring 3 の #GP に掛けてあるので、止まったのが健全性判定のためだと
    // 特定できる（もともと畳まない例外に掛けると区別が付かない）。
    CriticalTest {
        name: "corrupt-frame-cs",
        feature: "ring3-test-corrupt-frame-cs",
        expected_markers: &["exception frame is not trustworthy", "halting"],
        forbidden_markers: &["ring3: Ring 3 excursion verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S9-b-1: 埋め込んだユーザープログラムの破壊確認。
    CriticalTest {
        name: "user-skip-load",
        feature: "user-run-skip-load",
        expected_markers: &["user-run: expected #UD (6)", "halting"],
        forbidden_markers: &["user-load: hello ran from its own address space"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **この破壊だけが user-load の読み戻しまで到達する。** map-force-writable は
    // 先に ring3-vectors の 6 本目が落ちるので、後ろの読み戻しへ届かない。
    CriticalTest {
        name: "user-writable-text",
        feature: "user-run-writable-text",
        expected_markers: &["has w=true (expected false)", "halting"],
        forbidden_markers: &["user-load: hello ran from its own address space"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "user-wrong-entry",
        feature: "user-run-wrong-entry",
        expected_markers: &["user-run: folded at 0x400000", "halting"],
        forbidden_markers: &["user-load: hello ran from its own address space"],
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
    // syscall_entry に到達しない。
    //
    // S8-a で捕まえる場所が変わった。畳みの判定から RIP の厳密一致を外したので、この
    // #GP は int の位置で畳まれてカーネルへ戻る。予期は cli の位置なので、遠征の
    // 呼び出し側の主張（assert_folded_at）が食い違いを捕まえて停止する。
    // 以前は畳まれずに例外ダンプへ落ちていた（"exception: vector=13"）。
    CriticalTest {
        name: "gate-dpl0",
        feature: "syscall-test-gate-dpl0",
        expected_markers: &["folded at an unexpected place", "halting"],
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
        name: "einval-as-efault",
        feature: "syscall-test-einval-as-efault",
        expected_markers: &[
            "syscall: checksum case 'over-long' expected -EINVAL",
            "halting",
        ],
        forbidden_markers: &["syscall: checksum round-trip verified"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // S9-a: 容量超過の errno を分ける前へ戻す。**この分岐は S9-a で初めて通るように
    // なった経路である。** 通り始めたばかりの経路を手で1度確かめただけにしないため、
    // 永続の破壊として置く。次に dispatch を触ったときに落ちる。
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
    let data = elf
        .segment_data(&seg)
        .map_err(|e| anyhow::anyhow!("the entry segment lies outside the file: {e:?}"))?;
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
    const USAGE: &str = "usage: cargo xtask check [--full]\n       cargo xtask flaky\n       cargo xtask run [--panic-test] [--gui] [--gfx-test] [--kvm] [--no-limit]\n       cargo xtask run --exception-test <kind>\n       cargo xtask run --critical-test <kind>\n       cargo xtask run --interrupt-test <kind>\n       cargo xtask run --paging-test <kind>\n       cargo xtask run --stack-test <kind>\n       cargo xtask run --task-test <kind>\n       cargo xtask run --ring3-test <kind>\n       cargo xtask run --syscall-test <kind>\n       cargo xtask run --acpi-test <kind>\n       cargo xtask run --acpi-smp-test\n       cargo xtask run --apic-test <kind>\n       cargo xtask run --apic-decode-test\n       cargo xtask run --ioapic-test <kind>\n       cargo xtask run --lapic-timer-test <kind>\n       cargo xtask run --drift-test [MINUTES] [--smp N]
       cargo xtask run --boot-log-diff [--update-reference]
       cargo xtask run --calibration-spread [N]\n       cargo xtask run --highhalf-test <kind>\n       cargo xtask screenshot [output.png] [--wait-secs N] [--gfx-test] [--kvm]\n       cargo xtask gen-font";

    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => {
            let rest = &args[1..];
            let panic_test = rest.iter().any(|a| a == "--panic-test");
            let gui = rest.iter().any(|a| a == "--gui");
            let gfx_test = rest.iter().any(|a| a == "--gfx-test");
            let kvm = rest.iter().any(|a| a == "--kvm");
            let no_limit = rest.iter().any(|a| a == "--no-limit");
            if let Some(index) = rest.iter().position(|a| a == "--lapic-timer-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = LAPIC_TIMER_TESTS.iter().map(|t| t.name).collect();
                    format!("--lapic-timer-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_lapic_timer_test(kind);
            }
            // **AP のティックのレートをホストの実時間と突き合わせる（S4-a）。**
            // 表を持たない単独の検査なので、種別を取らない。
            if rest.iter().any(|a| a == "--ap-timer-rate") {
                return cmd_ap_timer_rate();
            }
            if let Some(index) = rest.iter().position(|a| a == "--bkl-test") {
                let kind = rest
                    .get(index + 1)
                    .map(String::as_str)
                    .unwrap_or(BKL_TESTS[0].name);
                if let Some(test) = BKL_TIMEOUT_TESTS.iter().find(|t| t.name == kind) {
                    return cmd_marker_test(BKL_TIMEOUT_TESTS, "bkl-test", test.name, Some(2));
                }
                return cmd_marker_test(BKL_TESTS, "bkl-test", kind, None);
            }
            if rest.iter().any(|a| a == "--bkl-exclusion-proof") {
                return cmd_bkl_exclusion_proof();
            }
            if rest.iter().any(|a| a == "--kernel-entry-concurrency") {
                return cmd_kernel_entry_concurrency();
            }
            if let Some(index) = rest.iter().position(|a| a == "--ioapic-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = IOAPIC_SABOTAGE_TESTS.iter().map(|t| t.name).collect();
                    format!("--ioapic-test requires a kind ({})", names.join(" | "))
                })?;
                let test = IOAPIC_SABOTAGE_TESTS
                    .iter()
                    .find(|t| t.name == kind)
                    .with_context(|| format!("unknown --ioapic-test kind: {kind}"))?;
                return cmd_ioapic_sabotage(test.name, test.feature, test.expected);
            }
            if let Some(index) = rest.iter().position(|a| a == "--interrupt-test") {
                if rest.get(index + 1).map(String::as_str) == Some("keyboard") {
                    return cmd_keyboard_test();
                }
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = INTERRUPT_TESTS.iter().map(|t| t.name).collect();
                    format!("--interrupt-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(INTERRUPT_TESTS, "interrupt-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--paging-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = PAGING_TESTS.iter().map(|t| t.name).collect();
                    format!("--paging-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(PAGING_TESTS, "paging-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--stack-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = STACK_TESTS.iter().map(|t| t.name).collect();
                    format!("--stack-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(STACK_TESTS, "stack-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--task-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = TASK_TESTS.iter().map(|t| t.name).collect();
                    format!("--task-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(TASK_TESTS, "task-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--ring3-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = RING3_TESTS.iter().map(|t| t.name).collect();
                    format!("--ring3-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(RING3_TESTS, "ring3-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--syscall-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = SYSCALL_TESTS.iter().map(|t| t.name).collect();
                    format!("--syscall-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(SYSCALL_TESTS, "syscall-test", kind, None);
            }
            if rest.iter().any(|a| a == "--acpi-smp-test") {
                return cmd_marker_test(
                    ACPI_SMP_TESTS,
                    "acpi-smp-test",
                    ACPI_SMP_TESTS[0].name,
                    Some(2),
                );
            }
            if rest.iter().any(|a| a == "--boot-log-diff") {
                let update = rest.iter().any(|a| a == "--update-reference");
                return cmd_boot_log_diff(update);
            }
            if let Some(index) = rest.iter().position(|a| a == "--drift-test") {
                let minutes = rest
                    .get(index + 1)
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(DRIFT_TEST_DEFAULT_MINUTES);
                let smp = rest
                    .iter()
                    .position(|a| a == "--smp")
                    .and_then(|i| rest.get(i + 1))
                    .and_then(|v| v.parse::<u32>().ok());
                return cmd_drift_test(minutes, smp);
            }
            if rest.iter().any(|a| a == "--acpi-smp4-test") {
                return cmd_marker_test(
                    ACPI_SMP4_TESTS,
                    "acpi-smp-test",
                    ACPI_SMP4_TESTS[0].name,
                    Some(4),
                );
            }
            if let Some(index) = rest.iter().position(|a| a == "--smp-ap-test") {
                let kind = rest
                    .get(index + 1)
                    .map(String::as_str)
                    .unwrap_or(SMP_AP_TESTS[0].name);
                return cmd_marker_test(SMP_AP_TESTS, "smp-ap-test", kind, Some(2));
            }
            if rest.iter().any(|a| a == "--smp-tramp-test") {
                return cmd_marker_test(
                    SMP_TRAMP_TESTS,
                    "smp-tramp-test",
                    SMP_TRAMP_TESTS[0].name,
                    Some(2),
                );
            }
            if rest.iter().any(|a| a == "--percpu-test") {
                return cmd_marker_test(PERCPU_TESTS, "percpu-test", PERCPU_TESTS[0].name, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--acpi-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = ACPI_TESTS.iter().map(|t| t.name).collect();
                    format!("--acpi-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(ACPI_TESTS, "acpi-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--apic-test") {
                let kind = rest.get(index + 1).with_context(|| {
                    let names: Vec<&str> = APIC_TESTS.iter().map(|t| t.name).collect();
                    format!("--apic-test requires a kind ({})", names.join(" | "))
                })?;
                return cmd_marker_test(APIC_TESTS, "apic-test", kind, None);
            }
            if let Some(index) = rest.iter().position(|a| a == "--calibration-spread") {
                let runs = rest
                    .get(index + 1)
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(DEFAULT_CALIBRATION_RUNS);
                return cmd_calibration_spread(runs);
            }
            if rest.iter().any(|a| a == "--apic-decode-test") {
                return cmd_marker_test(
                    APIC_DECODE_TESTS,
                    "apic-decode-test",
                    APIC_DECODE_TESTS[0].name,
                    None,
                );
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
                return cmd_marker_test(CRITICAL_TESTS, "critical-test", kind, None);
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
        // `--full` から外した確率的な項目を手で回す。外した項目を回す手段が
        // なければ、外すことは「守らないと決める」ことになる。
        Some("flaky") => cmd_flaky(),
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
/// キーボードの回帰チェックの個別主張。**破壊確認が「どれが落ちたか」を
/// 見るために、合否をまとめずに返す。**
#[derive(Debug, Clone, Copy)]
struct KeyboardAssertions {
    /// 期待した文字列になったか。
    line: bool,
    /// 新しいベクタ（0x42）で届いたか。**到達の検出経路。**
    arrived_on_new_vector: bool,
    /// 旧ベクタ（0x21）で届いたキーが無いか。**二重配送の検出。**
    no_legacy_delivery: bool,
    /// redirection entry のベクタ欄が書いた値か。**設定の検出経路。**
    readback_vector: bool,
    /// redirection entry のマスクが外れているか。**解禁の検出経路。**
    readback_unmasked: bool,
    /// 送った本数と受け取った本数が一致するか。
    count: bool,
    /// 会計が閉じているか。
    balanced: bool,
    /// `sti` 前の検査が落ちて、割り込みを有効にせず止まったか。
    ///
    /// **正常な起動では `false` である。** 破壊確認で「何も届かない」だけを
    /// 見ると、原因を問わず通ってしまう。**どこで止まったかを名指しする。**
    refused_sti: bool,
}

impl KeyboardAssertions {
    fn all_ok(&self) -> bool {
        self.line
            && self.arrived_on_new_vector
            && self.no_legacy_delivery
            && self.readback_vector
            && self.readback_unmasked
            && self.count
            && self.balanced
            && !self.refused_sti
    }
}

fn cmd_keyboard_test() -> Result<()> {
    let assertions = run_keyboard_test(&[])?;
    if assertions.all_ok() {
        println!("interrupt-test keyboard: OK");
        Ok(())
    } else {
        bail!("interrupt-test keyboard: FAILED")
    }
}

fn run_keyboard_test(features: &[&str]) -> Result<KeyboardAssertions> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, features)?;
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
        report_did_not_start(context, firmware_rip, qemu_exit.as_deref())?;
        // 上は必ず Err を返すので、ここには来ない。
        bail!("{context}: the kernel did not start");
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
    let ready_ok = ready;

    // 1. 期待した文字列になったか（大文字と記号の変換を含む）。
    let line_ok = serial.contains(KEYBOARD_TEST_EXPECTED_LINE);
    println!(
        "{context}: serial contains {KEYBOARD_TEST_EXPECTED_LINE:?} = {}",
        if line_ok { "OK" } else { "NG" }
    );
    ok &= line_ok;

    // 2. IRQ1 の配送経路（**到達**）。S2-d-1c で 0x21 から 0x42 へ変えた。
    //
    // **0x42 は 8259 が出しえないベクタである。** ベクタオフセットは
    // 0x20（alt-offset では 0x30）で、そこに IRQ 番号を足したものしか出ない。
    // したがって**このベクタで届いたこと自体が、I/O APIC 経由である証拠**になる。
    // 0x21 のままだとどちらの経路でも同じ値になり、何も示さなかった。
    let vector_ok = serial.contains("keyboard: first key arrived as vector 0x42");
    println!(
        "{context}: the first key arrived as vector 0x42, which the 8259 cannot produce = {}",
        if vector_ok { "OK" } else { "NG" }
    );
    ok &= vector_ok;

    // 2b. 二重配送が起きていないこと（PPP'）。
    //
    // 移行後は経路ごとにベクタが違うので、旧ベクタで届いたキーがあれば
    // **PIC 側の線が開いたままである。** 移行前は区別できなかった形である。
    let no_legacy_delivery = !serial.contains("keyboard: first key arrived as vector 0x21");
    println!(
        "{context}: no key arrived on the old 8259 vector 0x21 = {}",
        if no_legacy_delivery { "OK" } else { "NG" }
    );
    ok &= no_legacy_delivery;

    // 2c. 設定の読み戻し（**到達とは独立した検出経路**）。
    //
    // 読み戻しだけだと、設定できても配送されない形（マスクの外し忘れ、
    // 宛先の誤り）を通す。到達だけだと、書いた値が entry に保持されて
    // いるかを見ていない。**両方あって初めて経路が閉じる。**
    //
    // **ベクタ欄とマスクビットを別々に見る。** 1 つに畳むと、マスクを外し
    // 忘れた構成で「設定が書けていない」と読めてしまい、2 つの経路が
    // 独立していることを示せない（実際に畳んだ形で測って気づいた）。
    let readback_line = serial
        .lines()
        .find(|l| l.contains("ioapic: IRQ1 redirection entry read back:"))
        .unwrap_or_default();
    let readback_vector_ok = readback_line.contains("vector=0x42");
    println!(
        "{context}: the redirection entry carries the vector we wrote = {}",
        if readback_vector_ok { "OK" } else { "NG" }
    );
    ok &= readback_vector_ok;

    let readback_unmasked_ok = readback_line.contains("masked=false");
    println!(
        "{context}: the redirection entry is unmasked = {}",
        if readback_unmasked_ok { "OK" } else { "NG" }
    );
    ok &= readback_unmasked_ok;

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

    // 6. 意図しない例外が起きていないこと。
    //
    // S8-d/e の遠征が Ring 3 の #PF を意図して起こす（未マップ VA とカーネル VA の
    // 2 本）。除外は CR2 の列挙ではなく **cpl=3 で行う**——S8 以降、Ring 3 由来の
    // #PF は畳まれて処理される事象であり、この検査が守るべき不変条件は
    // 「意図しない**カーネルの** #PF が無いこと」だからである。CR2 を並べる形だと
    // 遠征を足すたびに除外が増え、増えた分だけ検査が守る範囲が黙って狭くなる。
    // cpl=0 の #PF は従来どおり NG、#DF（v=08）も無条件に NG である。
    let unintended_pf = qemu
        .lines()
        .any(|l| l.contains("v=0e") && !l.contains("cpl=3"));
    println!(
        "{context}: qemu log free of ring-0 \"v=0e\" = {}",
        if unintended_pf { "NG" } else { "OK" }
    );
    ok &= !unintended_pf;
    let df_present = qemu.contains("v=08");
    println!(
        "{context}: qemu log free of \"v=08\" = {}",
        if df_present { "NG" } else { "OK" }
    );
    ok &= !df_present;

    // ready / no_drop / heartbeats / 例外の 4 つは、どの構成でも成り立つべき
    // 前提として `line` へ畳んでいる。破壊確認が見分けたいのは、
    // 到達・二重配送・読み戻し・本数の 4 つである。
    let assertions = KeyboardAssertions {
        line: ready_ok && line_ok && no_drop_ok && heartbeats >= 2 && ok,
        arrived_on_new_vector: vector_ok,
        no_legacy_delivery,
        readback_vector: readback_vector_ok,
        readback_unmasked: readback_unmasked_ok,
        count: count_ok,
        balanced: balanced_ok,
        refused_sti: serial.contains("refusing to sti"),
    };
    println!(
        "{context}: {}",
        if assertions.all_ok() { "PASS" } else { "FAIL" }
    );
    println!(
        "{context}: note - key repeat (typematic) is NOT covered here; QEMU's sendkey does \
         not emulate it. Check it by hand with `cargo xtask run --gui`."
    );
    Ok(assertions)
}

/// S2-d-2 の検査。**ホストの実時間を独立の基準として使う。**
///
/// カーネル側の許容幅の判定は「要求周波数」と「較正値から導いた実効周波数」を
/// 比べており、**どちらも内側の値である。** 較正値が 2 倍になれば初期カウントも
/// 2 倍になって比は変わらず、**自己無矛盾のまま通ってしまう。**
///
/// ここではハートビートが報告する経過秒を、**ホスト側で測った実時間**と
/// 突き合わせる。較正値が現実とずれていれば、この比がずれる。
struct LapicTimerTest {
    name: &'static str,
    /// 追加する feature。既定ビルドなら空。
    features: &'static [&'static str],
    /// 実時間との比がこの範囲に入るべきか。
    expect_within_tolerance: bool,
    /// 破壊が適用されたことを示すマーカー（空なら見ない）。
    ///
    /// **破壊のフックが踏まれなかった場合、破壊ビルドは正常に見えて緑になる。**
    /// 適用の痕跡を必須にして、空振りを落とす。
    sabotage_marker: &'static str,
    /// 空でなければ、**カーネルがこの行を出して停止することを期待する。**
    /// 速さの比ではなく、名指しの検出で捕まる破壊に使う。
    expect_halt_marker: &'static str,
}

const LAPIC_TIMER_TESTS: &[LapicTimerTest] = &[
    LapicTimerTest {
        name: "rate",
        features: &[],
        expect_within_tolerance: true,
        sabotage_marker: "",
        expect_halt_marker: "",
    },
    // 較正の戻り値を 2 倍にする。**カーネル内の比は変わらないが、実時間との
    // 比は倍になる。** これが「較正値が初期カウントへ実際に流れている」ことの
    // 証明である。
    LapicTimerTest {
        name: "scaled-calibration",
        features: &["lapic-timer-scale-calibration-test"],
        expect_within_tolerance: false,
        sabotage_marker: "apic: SABOTAGE applied - the calibration result is scaled",
        expect_halt_marker: "",
    },
    // 較正で書く分周と、戻り値に載せる分周を食い違わせる。**較正は分周なしで
    // 測るので周波数が 16 倍に出て、運用は 16 分周で走る。** 実時間との比が
    // 大きく崩れる。
    //
    // **検出経路は `scaled-calibration` と同じ（実時間との比）である。**
    // 主張は違う（あちらは較正値が初期カウントへ流れること、こちらは較正時と
    // 運用時で分周が揃うこと）が、**落ちる場所は同じ**である。
    LapicTimerTest {
        name: "wrong-divide",
        features: &["lapic-timer-wrong-divide-test"],
        expect_within_tolerance: false,
        sabotage_marker: "",
        expect_halt_marker: "",
    },
    // PIC を全マスクせずに LVT を開ける。**速さの比では捕まらない。**
    // 切り替えの直後に IMR を読み戻す検査が、IRQ0 が開いたままであることを
    // 名指しで捕まえて停止する。**`irq::mask_all()` が実際に呼ばれている
    // ことの裏返しの証明でもある**（呼ばれていなければこの検査が落ちる）。
    LapicTimerTest {
        name: "no-mask-all",
        features: &["lapic-timer-no-mask-all-test"],
        expect_within_tolerance: false,
        sabotage_marker: "",
        expect_halt_marker: "lapic-timer: the 8259 is not fully masked",
    },
];

/// ハートビートが報告する経過秒と、ホストで測った実時間の比を見る。
///
/// 許容幅はカーネル側の ±0.5% より**大きく取る**。QEMU の実行そのものが
/// 実時間に対して一定の比で進む保証が無く、起動処理やホストの負荷も混ざる
/// ためである。**ここで見たいのは「桁が合っているか」であって、
/// 較正の精度ではない。**
const LAPIC_TIMER_RATE_TOLERANCE: f64 = 0.25;

fn cmd_lapic_timer_test(kind: &str) -> Result<()> {
    let test = LAPIC_TIMER_TESTS
        .iter()
        .find(|t| t.name == kind)
        .with_context(|| {
            let names: Vec<&str> = LAPIC_TIMER_TESTS.iter().map(|t| t.name).collect();
            format!(
                "unknown --lapic-timer-test kind: {kind} ({})",
                names.join(" | ")
            )
        })?;
    let context = format!("lapic-timer-test {}", test.name);

    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, test.features)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root.join("target").join("lapic-timer-serial.log");
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

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the lapic timer test")?;

    // **最初のハートビートが出てから測り始める。** 起動処理の時間を
    // 分母に入れると、比が起動の重さに引きずられる。
    let first_marker = "heartbeat: ticks=";
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let mut started_at = None;
    let mut first_seconds = 0u64;
    while Instant::now() < deadline {
        if let Some((seconds, _)) = last_heartbeat_seconds(&serial_log) {
            started_at = Some(Instant::now());
            first_seconds = seconds;
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let mut measured = None;
    if let Some(started_at) = started_at {
        thread::sleep(LAPIC_TIMER_MEASURE_WINDOW);
        if let Some((seconds, _)) = last_heartbeat_seconds(&serial_log) {
            measured = Some((
                seconds.saturating_sub(first_seconds),
                started_at.elapsed().as_secs_f64(),
            ));
        }
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
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(&context, firmware_rip, qemu_exit.as_deref());
    }

    let mut ok = true;

    // 破壊が実際に適用されたこと。**空振りを落とす。**
    if !test.sabotage_marker.is_empty() {
        let applied = serial.contains(test.sabotage_marker);
        println!(
            "{context}: the sabotage was actually applied = {}",
            if applied { "OK" } else { "NG" }
        );
        if !applied {
            println!("{context}: the sabotage hook was never reached, so this run proves nothing");
        }
        ok &= applied;
    }

    // 名指しの検出で捕まる破壊は、速さの比を見ない。
    if !test.expect_halt_marker.is_empty() {
        let halted = serial.contains(test.expect_halt_marker);
        println!(
            "{context}: the kernel refused to proceed with the named check = {}",
            if halted { "OK" } else { "NG" }
        );
        ok &= halted;
        if ok {
            println!("{context}: PASS");
            return Ok(());
        }
        bail!("{context}: FAIL")
    }

    let ticking = serial.contains(first_marker);

    match measured {
        Some((kernel_seconds, host_seconds)) if host_seconds > 0.0 => {
            println!(
                "{context}: heartbeats are being produced = {}",
                if ticking { "OK" } else { "NG" }
            );
            ok &= ticking;

            let ratio = kernel_seconds as f64 / host_seconds;
            let within = (ratio - 1.0).abs() <= LAPIC_TIMER_RATE_TOLERANCE;
            println!(
                "{context}: kernel {kernel_seconds} s vs host {host_seconds:.1} s (ratio {ratio:.3}), within {LAPIC_TIMER_RATE_TOLERANCE} = {within} (wanted {})",
                test.expect_within_tolerance
            );
            ok &= within == test.expect_within_tolerance;
        }
        // **ハートビートが 1 本も出ないのも「比が崩れている」の一形態である。**
        // 遅くなる向きに壊すと、測定窓の中に 1 本も入らない。**「測れなかった」
        // で片づけると、壊れていることを検出できたのに落としてしまう。**
        //
        // ただし**正常であるはずの構成では失敗として扱う。** 出ないことが
        // 正しいのは、幅の外へ出ると宣言した構成だけである。
        _ if !test.expect_within_tolerance => {
            println!(
                "{context}: no heartbeat appeared within the window, which is itself out of tolerance (the timer is far too slow) = OK"
            );
            // 起動そのものが失敗した場合と区別する。
            let started = serial.contains("lapic-timer: the timer now arrives as vector");
            println!(
                "{context}: the switch to the local APIC timer did happen = {}",
                if started { "OK" } else { "NG" }
            );
            ok &= started;
        }
        _ => {
            println!("{context}: could not measure the tick rate = NG");
            ok = false;
        }
    }

    if ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAIL")
    }
}

/// 測定窓。**短いとハートビートの粒度（1 秒）が効きすぎる。**
const LAPIC_TIMER_MEASURE_WINDOW: Duration = Duration::from_secs(20);

/// シリアルログの最後のハートビートが報告する経過秒とティック数。
fn last_heartbeat_seconds(serial_log: &Path) -> Option<(u64, u64)> {
    let content = fs::read_to_string(serial_log).ok()?;
    let line = content.lines().rfind(|l| l.contains("heartbeat: ticks="))?;
    // 形は `heartbeat: ticks=256 (2 s), ...`
    let ticks_part = line.split("ticks=").nth(1)?;
    let ticks: u64 = ticks_part.split_whitespace().next()?.parse().ok()?;
    let seconds_part = line.split('(').nth(1)?;
    let seconds: u64 = seconds_part.split_whitespace().next()?.parse().ok()?;
    Some((seconds, ticks))
}

/// AP のハートビートが報告するティック数（S4-a）。
fn last_ap_heartbeat_ticks(serial_log: &Path) -> Option<u64> {
    let content = fs::read_to_string(serial_log).ok()?;
    let line = content
        .lines()
        .rfind(|l| l.contains("smp: ap heartbeat: cpu=1 ticks="))?;
    // 形は `smp: ap heartbeat: cpu=1 ticks=8400 tsc=...`
    let ticks_part = line.split("ticks=").nth(1)?;
    ticks_part.split_whitespace().next()?.parse().ok()
}

/// AP のティックのレートをホストの実時間と突き合わせる（S4-a）。
///
/// # これが検証しているのは仮定である
///
/// AP は較正をやり直さず、**BSP が測った分周と初期カウントをそのまま自分の LVT へ
/// 書く。** それは「Local APIC タイマの周波数がコア間で同じ」という**仮定**である。
///
/// **カーネルの内側では確かめられない。** 要求周波数も実効周波数も同じ較正値から
/// 導くので自己無矛盾になる（`lapic-timer-scale-calibration-test` が示した形と
/// 同じである）。**独立な基準はホストの実時間しかない。**
///
/// 仮定が崩れる環境（コアごとに LAPIC タイマの周波数が違う機械）では、
/// **AP のティックのレートがホスト時間と合わなくなり、ここで捕まる。**
fn cmd_ap_timer_rate() -> Result<()> {
    let context = "smp-ap-test ap-timer-rate";
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, &[])?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root.join("target").join("smp-ap-rate-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
    });
    qemu_args.push("-smp".into());
    qemu_args.push("2".into());

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the AP timer rate test")?;

    // **AP の最初のハートビートが出てから測り始める。** 起動処理の時間を
    // 分母に入れると、比が起動の重さに引きずられる（BSP 側の測り方と同じ）。
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    let mut started_at = None;
    let mut first_ticks = 0u64;
    while Instant::now() < deadline {
        if let Some(ticks) = last_ap_heartbeat_ticks(&serial_log) {
            started_at = Some(Instant::now());
            first_ticks = ticks;
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }

    let mut measured = None;
    if let Some(started_at) = started_at {
        thread::sleep(LAPIC_TIMER_MEASURE_WINDOW);
        if let Some(ticks) = last_ap_heartbeat_ticks(&serial_log) {
            measured = Some((
                ticks.saturating_sub(first_ticks),
                started_at.elapsed().as_secs_f64(),
            ));
        }
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
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(context, firmware_rip, qemu_exit.as_deref());
    }

    let mut ok = true;
    let Some((ap_ticks, host_seconds)) = measured else {
        println!("{context}: could not measure the AP tick rate = NG");
        bail!("{context}: FAIL")
    };
    if host_seconds <= 0.0 {
        println!("{context}: the measurement window was empty = NG");
        bail!("{context}: FAIL")
    }

    // **要求周波数はカーネルの外へ出ていない。** ハートビートの粒度から導く。
    // AP のハートビートは 100 ティックごとなので、ティック数そのものを使う。
    let measured_hz = ap_ticks as f64 / host_seconds;
    let ratio = measured_hz / AP_EXPECTED_TICK_HZ;
    let within = (ratio - 1.0).abs() <= LAPIC_TIMER_RATE_TOLERANCE;
    println!(
        "{context}: AP produced {ap_ticks} tick(s) in {host_seconds:.1} host second(s) = \
         {measured_hz:.2} Hz, expected {AP_EXPECTED_TICK_HZ:.0} Hz (ratio {ratio:.3}), \
         within {LAPIC_TIMER_RATE_TOLERANCE} = {within}"
    );
    println!(
        "{context}: this is the check that the shared calibration is a valid ASSUMPTION; the \
         kernel cannot check it from inside because both sides come from the same calibration"
    );
    ok &= within;

    if ok {
        println!("{context}: PASS");
        Ok(())
    } else {
        bail!("{context}: FAIL")
    }
}

/// カーネル入口への同時進入を実測する（S4-a）。**KVM でしか観測できない。**
///
/// # なぜ TCG では駄目なのか
///
/// **TCG は 2 つの vCPU を並行に走らせない**（実測）。`-smp 2` を 86 秒
/// （17,060 ティック）回しても同時進入数は 1 のままで、入口の窓を 3,000 回の
/// `spin_loop` ぶん意図的に広げても 0 回だった。**「稀」ではなく「起きない」である。**
/// KVM では 2 になる。
///
/// # **検査項目ではない。手動で回す観測である**
///
/// **KVM でも決定的ではない。** 同じ構成で 4 回回して 3 回は 2 が出たが、
/// 1 回は 116 秒（両コアで約 23,000 回の入口通過）のあいだ 1 のままだった。
/// ホスト側のスケジューリング次第で重なるかどうかが変わる。
/// **確率的なものを `--full` に入れると、落ちたときに退行か揺らぎかが
/// 区別できなくなる。** したがって `--full` からは外し、
/// `cargo xtask run --kernel-entry-concurrency` で手で回す形にしてある。
///
/// # なぜこれが要るのか。**S4-b の証明がこれに乗る**
///
/// BKL を入れた後、TCG で「同時進入数が 1」を観測しても**それは BKL の証明に
/// ならない。** BKL が無くても 1 だからである。**「同値である間は分類の誤りが
/// 観測できない」**の、まさにその形に入る。
/// **S4-a で「BKL が無ければ 2 になる」を KVM で押さえておく**ことが、
/// S4-b で「BKL を入れると 1 になる」を意味のある主張にする。
///
/// # 環境要因と主張の失敗を混ぜない
///
/// `/dev/kvm` が使えない環境でこの項目が落ちたとき、**BKL の退行と誤読されては
/// ならない。** 使えない場合は `environment: KVM unavailable` と明示して、
/// 主張が落ちたのではないことを出力で区別する。
fn cmd_kernel_entry_concurrency() -> Result<()> {
    let context = "smp-ap-test kernel-entry-concurrency";
    let workspace_root = workspace_root()?;

    if !Path::new(KVM_DEVICE_PATH).exists() {
        println!(
            "{context}: environment: KVM unavailable ({KVM_DEVICE_PATH} does not exist). \
             This check needs real parallel execution; TCG serialises the vCPUs, so the \
             observation cannot be made here. THIS IS NOT AN ASSERTION FAILURE"
        );
        bail!("{context}: SKIPPED (environment: KVM unavailable)")
    }

    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, &[])?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("smp-ap-concurrency-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Kvm,
    });
    qemu_args.push("-smp".into());
    qemu_args.push("2".into());

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the kernel entry concurrency test")?;

    // **深さ 2 が出るまで待つ。** 出た時点で打ち切る（それ以上待っても
    // 主張は強くならない）。出なければ期限で打ち切る。
    let deadline = Instant::now() + KERNEL_ENTRY_CONCURRENCY_TIMEOUT;
    let mut observed = false;
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(&serial_log) {
            if text.contains(KERNEL_ENTRY_DEPTH_TWO_MARKER) {
                observed = true;
                break;
            }
        }
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
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start(context, firmware_rip, qemu_exit.as_deref());
    }

    // **起動そのものが進んだことを分けて見る。** 深さ 2 が出ないのが
    // 「並行しなかった」なのか「定常状態へ来なかった」なのかを区別する。
    let steady = serial.contains("heartbeat: ticks=");
    println!(
        "{context}: the run reached steady state = {}",
        if steady { "OK" } else { "NG" }
    );
    println!(
        "{context}: two cores were inside a kernel entry at the same time \
         ({KERNEL_ENTRY_DEPTH_TWO_MARKER}) = {}",
        if observed { "OK" } else { "NG" }
    );
    if steady && observed {
        println!("{context}: PASS");
        return Ok(());
    }
    bail!("{context}: FAIL")
}

/// BKL の相互排除の証明（S4-b-4）。**KVM を要し、無ければ落とす。**
///
/// # 2 構成を同じ増幅器の上で比べる
///
/// `bkl-widen-entry-window` を**両方の構成で固定**し、`bkl-skip-timer-entry` の
/// 有無だけを変える。**同じ条件で比べていることが構成から保証される。**
///
/// | 構成 | 期待 |
/// |---|---|
/// | widen だけ | 同時進入数の最大 = 1 |
/// | widen + skip | 同時進入数の最大 = 2 |
///
/// **前者が「BKL が効いている」、後者が「取らなければ 2 になる」である。**
/// 片方だけでは「重なりが少ないから 1」と読める余地が残る。
///
/// # KVM が無ければ落とす。**`SKIPPED` にしない**
///
/// **この項目は S4 の相互排除の証明そのものを担っている。** 走っていないのに
/// 緑になれば、**「検査が緑」と「系が悪くなっていない」を取り違える。**
/// 手動の `kernel-entry-concurrency` が `SKIPPED` でよいのは、あちらが
/// **補助実証であって主張を担っていない**からである。**扱いが違うのは、
/// 担っているものが違うからである。**
///
/// # 判定は heartbeat の値で行う
///
/// 同時進入が起きた瞬間に出る `bkl: kernel entry depth reached 2` は
/// **判定に使わない。** BKL の外から書かれるので**他コアの行と混線しうる**
/// （5 回のうち 1 回、実際にバイト単位で混ざって一致しなかった）。
/// heartbeat の `max kernel entry depth=` は BKL の内側で書かれるので混ざらない。
fn cmd_bkl_exclusion_proof() -> Result<()> {
    let context = "bkl-test exclusion-proof";
    let workspace_root = workspace_root()?;

    if !Path::new(KVM_DEVICE_PATH).exists() {
        println!(
            "{context}: environment: KVM unavailable ({KVM_DEVICE_PATH} does not exist).              This check IS the mutual-exclusion proof, so it is NOT skipped: TCG never runs              two vCPUs in parallel, and a green result without this check would mean the              proof did not run at all"
        );
        bail!("{context}: FAIL (environment: KVM unavailable)")
    }

    let cases = [
        ("widen only", "bkl-widen-entry-window-test", 1u64),
        (
            "widen + skip",
            "bkl-widen-entry-window-test,bkl-skip-timer-entry-test",
            2,
        ),
    ];
    let mut ok = true;
    for (label, features, expected) in cases {
        let observed = run_for_max_entry_depth(&workspace_root, features)?;
        println!("{context}: {label}: max kernel entry depth = {observed:?} (expected {expected})");
        ok &= observed == Some(expected);
    }
    if ok {
        println!("{context}: PASS");
        return Ok(());
    }
    bail!("{context}: FAIL")
}

/// 1 構成を KVM の `-smp 2` で起動し、heartbeat が報告する最大の同時進入数を返す。
fn run_for_max_entry_depth(workspace_root: &Path, features: &str) -> Result<Option<u64>> {
    let ovmf_vars = prepare_ovmf_vars(workspace_root)?;
    let bootloader_efi = build_bootloader(workspace_root, false)?;
    let feature_list: Vec<&str> = features.split(',').collect();
    let kernel_elf = build_kernel_with_features(workspace_root, &feature_list)?;
    let esp_dir = stage_esp(workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join("bkl-exclusion-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Kvm,
    });
    qemu_args.push("-smp".into());
    qemu_args.push("2".into());

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the BKL exclusion proof")?;
    thread::sleep(BKL_EXCLUSION_WINDOW);
    let _ = child.kill();
    let _ = child.wait();

    let serial = fs::read_to_string(&serial_log).unwrap_or_default();
    Ok(serial
        .lines()
        .filter_map(|line| line.split("max kernel entry depth=").nth(1))
        .filter_map(|rest| {
            rest.split(|c: char| !c.is_ascii_digit())
                .next()
                .and_then(|d| d.parse::<u64>().ok())
        })
        .max())
}

/// 各構成の観測窓。**5 回の測定で 40 秒を使い、全一致した。**
const BKL_EXCLUSION_WINDOW: Duration = Duration::from_secs(40);

/// KVM のデバイスノード。**存在しなければ環境要因として扱う。**
const KVM_DEVICE_PATH: &str = "/dev/kvm";

/// 同時進入が観測されるまでの上限。
const KERNEL_ENTRY_CONCURRENCY_TIMEOUT: Duration = Duration::from_secs(120);

/// ハートビートが出す同時進入数の最大値が 2 になった形。
const KERNEL_ENTRY_DEPTH_TWO_MARKER: &str = "max kernel entry depth=2";

/// AP のティックの期待レート。**BSP と同じ要求周波数である。**
///
/// カーネル側の `irq::timer_frequency_hz()` と同じ値を、**外側の基準として
/// ここに置く。** カーネルから読んだ値と突き合わせると、両辺が同じ出所から
/// 導かれてしまい自己無矛盾になる。
const AP_EXPECTED_TICK_HZ: f64 = 100.0;

/// S2-d-1c の破壊 1 件ぶんの定義。
struct IoApicSabotage {
    name: &'static str,
    feature: &'static str,
    /// 壊れたビルドで**期待される**主張の値。`false` が「落ちるはず」。
    expected: KeyboardAssertions,
}

/// S2-d-1c の破壊一覧。
///
/// **`line` と `balanced` は判定に使っていない**（`cmd_ioapic_sabotage` が
/// 見るのは到達・二重配送・読み戻し・本数の 4 つ）。値は埋めるが意味を持たない。
const IOAPIC_SABOTAGE_TESTS: &[IoApicSabotage] = &[
    // ゲートの無いベクタへ向ける。**読み戻しも到達も落ちる。**
    IoApicSabotage {
        name: "wrong-vector",
        feature: "ioapic-wrong-vector-test",
        expected: KeyboardAssertions {
            line: false,
            arrived_on_new_vector: false,
            no_legacy_delivery: true,
            readback_vector: false,
            readback_unmasked: true,
            count: false,
            balanced: false,
            refused_sti: false,
        },
    },
    // I/O APIC 側のマスクを外さない。**設定は正しいので読み戻しは通り、
    // 到達だけが落ちる。** 2 つの主張が独立している証拠になる。
    IoApicSabotage {
        name: "skip-unmask",
        feature: "ioapic-skip-unmask-test",
        expected: KeyboardAssertions {
            line: false,
            arrived_on_new_vector: false,
            no_legacy_delivery: true,
            readback_vector: true,
            readback_unmasked: false,
            count: false,
            balanced: false,
            refused_sti: false,
        },
    },
    // PIC 側の IRQ1 をマスクしない。
    //
    // # 何を検出するか。**二重配送の検出ではない**
    //
    // この破壊が示すのは「**二重配送に至る状態へ進むことを拒否する**」で
    // あって、「二重配送が起きたら検出できる」ではない。**別の主張である。**
    //
    // 設計時の予測は「二重配送が旧ベクタ `0x21` で観測できる」だったが、
    // 実測で外れた。**そこへ至る前に `sti` 前の項目 5 が落ちる。** 8259 の IMR が
    // `0xfc`（IRQ1 が開いたまま）で、移行後の期待値 `0xfe` と食い違うため、
    // カーネルは割り込みを有効にせず停止する。**予測より早く、より強い。**
    //
    // **二重配送そのものは依然として未観測である。** 起こす手段が無いので、
    // 「起きたときに検出できるか」はこの破壊では何も言えない。
    //
    // 主張は「1 本も届かない」側になるが、**それだけだと原因を問わず通る。**
    // `sti` を拒否した痕跡を併せて見る。**どこで止まったかを名指ししない
    // 破壊は、壊れ方を区別できない。**
    IoApicSabotage {
        name: "keep-pic-irq1",
        feature: "ioapic-keep-pic-irq1-test",
        expected: KeyboardAssertions {
            line: false,
            arrived_on_new_vector: false,
            no_legacy_delivery: true,
            readback_vector: true,
            readback_unmasked: true,
            count: false,
            balanced: false,
            refused_sti: true,
        },
    },
];

/// S2-d-1c の破壊確認。**キーボードの回帰チェックを壊れたビルドで走らせ、
/// 落ちるべき主張だけが落ちることを見る。**
///
/// `expected` は「この主張は落ちるはず」を並べたもので、`true` は健全な側で
/// ある。**「どれかが落ちた」ではなく「これが落ちてこれは落ちない」を見る**
/// ので、破壊が意図した経路だけを壊していることまで確かめられる。
fn cmd_ioapic_sabotage(name: &str, feature: &str, expected: KeyboardAssertions) -> Result<()> {
    let context = format!("ioapic-test {name}");
    println!("=== {context}: building with feature {feature:?} ===");
    let actual = run_keyboard_test(&[feature])?;

    let checks: [(&str, bool, bool); 6] = [
        (
            "arrived on the new vector",
            actual.arrived_on_new_vector,
            expected.arrived_on_new_vector,
        ),
        (
            "no delivery on the old vector",
            actual.no_legacy_delivery,
            expected.no_legacy_delivery,
        ),
        (
            "redirection entry carries our vector",
            actual.readback_vector,
            expected.readback_vector,
        ),
        (
            "redirection entry is unmasked",
            actual.readback_unmasked,
            expected.readback_unmasked,
        ),
        ("scancode count matches", actual.count, expected.count),
        (
            "refused to sti before enabling interrupts",
            actual.refused_sti,
            expected.refused_sti,
        ),
    ];

    let mut ok = true;
    for (label, got, want) in checks {
        let matched = got == want;
        println!(
            "{context}: {label} = {got} (wanted {want}) = {}",
            if matched { "OK" } else { "NG" }
        );
        ok &= matched;
    }

    if ok {
        println!("{context}: PASS");
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
    // **空文字は「既定ビルド」を意味する。** 破壊 feature を持たない構成
    // （`-smp 2` での列挙の確認）が既定のまま走れるようにする。空要素をそのまま
    // 渡すと `--features ""` になってしまう。
    let features: Vec<&str> = test
        .feature
        .split(',')
        .filter(|feature| !feature.is_empty())
        .collect();
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
/// `tlb-generation` の探りが主張していることを、絶対値によらず確かめる（S7-d）。
///
/// # 何を見るか
///
/// **探りが上げた世代に AP が追いつき、そのために 1 回以上フラッシュしたこと。**
///
/// - `smp: bumped the tlb generation to N` から `N` を読む
/// - ハートビートの `tlb_gen=G flush_cpu1=F` のうち**最後のもの**を読む
/// - **`G == N`**（AP を含む全コアが、探りの上げた世代を見ている）
/// - **`F >= 1`**（AP は世代の食い違いで実際にフラッシュした）
///
/// # なぜ絶対値をやめたか
///
/// **以前の期待（`... to 1` と `tlb_gen=1 flush_cpu1=1`）は、起動の間に世代を
/// 動かすものが他に無いことに依存していた。** それは**この探りの主張とは無関係な
/// 事情である。** S7-d のアドレス空間のデモが世代を 2 つ消費した時点で落ちた。
///
/// **カーネルには手を入れていない。** 検査のためにカーネルを変えるのは避ける。
fn check_tlb_generation_relation(serial: &str) -> Result<String> {
    let line = serial
        .lines()
        .rfind(|line| line.contains("bumped the tlb generation to "))
        .context("the probe never logged `bumped the tlb generation to N`")?;

    let (before, after) = line
        .split_once("ap flushes ")
        .and_then(|(_, rest)| rest.split_once(" -> "))
        .and_then(|(before, after)| {
            let after: String = after.chars().take_while(char::is_ascii_digit).collect();
            Some((before.parse::<u64>().ok()?, after.parse::<u64>().ok()?))
        })
        .context("the probe's line did not carry `ap flushes X -> Y`")?;

    if after <= before {
        bail!(
            "the probe bumped the generation, but cpu1's flush count did not move \
             ({before} -> {after}): the mismatch did not drive a flush"
        );
    }
    Ok(format!(
        "cpu1's flush count moved {before} -> {after} across the probe's bump"
    ))
}

/// 起動ログのうち、**起動ごとに値が変わる行**（S6-d）。
///
/// # なぜ捨てるのか——問いが違うからである
///
/// **起動間の変動と起動内の変動は別の問いである。**
/// **起動ログの差分が問うのは「この起動は参照と同じ形か」**で、ここは
/// **環境由来の揺れが支配する**（OVMF が返すメモリマップの大きさ、TSC の較正）。
/// **漂流が問うのは「1 回の起動の中でこの量が動くか」**で、**環境は固定されている。**
///
/// **同じ量でも問いが違うので扱いが違う。** 空きフレーム数はここでは捨てるが、
/// 漂流の測定では値として見る（`docs/verification-coverage.md`）。
/// **捨てる理由は「どうでもいいから」ではない。**
const BOOT_LOG_VOLATILE_MARKERS: &[&str] = &[
    // OVMF が返すメモリマップの大きさと、そこから導かれる量。
    "memory map: descriptors_len=",
    "frame allocator:",
    "paging: required range [BootInfo]",
    "paging: required range [memory map buffer]",
    "apic: frame accounting:",
    // S7-d のアドレス空間のデモが出す、フレームの本数とアドレス。
    // **`frame allocator:` と同じ理由である**——OVMF が返すメモリマップで動く。
    "allocator free",
    "address-space: the same VA",
    // TSC の較正。実行ごとに揺れる。
    "apic: LAPIC timer calibration",
    "lapic-timer: programmed from the calibration",
    "armed its own LAPIC timer with the BSP's calibration",
    // デモの反復回数。走った時間で変わる。
    "task: preemptive demo finished",
    "preemptive switch verified",
    // **OVMF が出す行。** カーネルの出力ではない。**参照に含めると、参照が
    // インストール済みの OVMF の版に縛られる**（別の機械で偽の失敗になる）。
    "BdsDxe",
    // ハートビート。**打ち切った時点のティック数が載るので、実行ごとに変わる。**
    //
    // **落とした結果、この行の形は参照の対象外である。** 形を見ているのは
    // マーカーで解析している既存の項目群のほうで、**ここは覆っていない。**
    "heartbeat",
];

/// 起動ログのうち、**コア数で変わる行**（S6-d の定義 3）。
///
/// # 定義 3 が主張していること
///
/// **「コア数に依らない部分が、コア数を変えても同じであること」**である。
/// **ここに挙げた行は主張の対象外である**——`-smp 1` には AP が無く、
/// `-smp 4` は `MAX_CPUS` を超えた分を起こさずに警告を出すので、**行そのものが
/// 変わるのが正しい。**
///
/// **対象外にした部分は、既存の `smp-ap-test` の項目群が見ている。** 定義 3 は
/// **それ以外の起動経路がコア数に汚染されていないこと**を見る。
const BOOT_LOG_CORE_COUNT_MARKERS: &[&str] = &[
    "smp:",
    // **`usable CPU(s)` はここにあった。S6-d の空振り点検で外した。**
    // **`smp:` を含む行にしか現れず、1 度も効いていなかった**（外しても
    // 定義 3 の判定が変わらない）。**効かない項目は「覆っている」と読まれる。**
    "per-CPU slot",
    "entry type=0 (Processor Local APIC)",
    "signature=\"APIC\" length=",
    "acpi: MADT enumeration complete",
];

/// 起動ログを正規化する（S6-d）。
///
/// **値を消すのではなく、行ごと落とす。** 桁や書式に結合しないためである
/// （`docstyle` の正規化差分で、書式へ結合させると検査が文書の書き方を縛ると
/// 分かっている）。
fn normalize_boot_log(serial: &str, drop_core_count_lines: bool) -> Vec<String> {
    serial
        .lines()
        .filter(|line| !BOOT_LOG_VOLATILE_MARKERS.iter().any(|m| line.contains(m)))
        .filter(|line| {
            !drop_core_count_lines || !BOOT_LOG_CORE_COUNT_MARKERS.iter().any(|m| line.contains(m))
        })
        .map(|line| line.to_string())
        .collect()
}

/// 起動ログを 1 本取る（S6-d）。QEMU を起こし、`marker` が出るまで待って落とす。
fn capture_boot_log(workspace_root: &Path, smp: Option<u32>, tag: &str) -> Result<String> {
    let ovmf_vars = prepare_ovmf_vars(workspace_root)?;
    let bootloader_efi = build_bootloader(workspace_root, false)?;
    let kernel_elf = build_kernel(workspace_root, false)?;
    let esp_dir = stage_esp(workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join(format!("boot-log-{tag}.log"));
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
    });
    if let Some(count) = smp {
        qemu_args.push("-smp".into());
        qemu_args.push(count.to_string().into());
    }

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the boot log capture")?;

    // **ハートビートが 3 本出るまで待つ。** 起動が終わって定常状態へ入った
    // ことの目印である。**上限は付ける**（出ない場合に無限に待たない）。
    let deadline = Instant::now() + EXCEPTION_TEST_TIMEOUT;
    loop {
        let seen = fs::read_to_string(&serial_log)
            .map(|c| c.matches("heartbeat: ticks=").count())
            .unwrap_or(0);
        if seen >= 3 || Instant::now() >= deadline {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();

    let serial = fs::read_to_string(&serial_log).unwrap_or_default();
    let qemu = fs::read_to_string(&debug_log).unwrap_or_default();
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        bail!("boot log capture ({tag}): the kernel did not start (firmware rip {firmware_rip:?})");
    }
    Ok(serial)
}

/// 参照となる正規化済み起動ログの置き場所（S6-d）。
const REFERENCE_BOOT_LOG: &str = "xtask/reference/boot-log-smp2.txt";

/// 起動ログの突き合わせ（S6-d）。**2 つの主張を 1 つの機構で見る。**
///
/// - **参照との一致。** `-smp 2` の正規化済み起動ログが、記録した参照と一致すること。
///   **これまで段ごとに手で行っていた行形の全件比較を機械にしたものである。**
/// - **定義 3。** `-smp 1` / `2` / `4` の正規化済み起動ログが、**コア数で変わる行を
///   除いて**一致すること。
///
/// `--update-reference` を付けると参照を書き換える。**意図した変更のときだけ付ける。**
fn cmd_boot_log_diff(update_reference: bool) -> Result<()> {
    let workspace_root = workspace_root()?;
    let reference_path = workspace_root.join(REFERENCE_BOOT_LOG);

    println!("=== boot log diff: capturing -smp 1 / 2 / 4");
    let smp1 = capture_boot_log(&workspace_root, Some(1), "smp1")?;
    let smp2 = capture_boot_log(&workspace_root, Some(2), "smp2")?;
    let smp4 = capture_boot_log(&workspace_root, Some(4), "smp4")?;

    let mut failed = false;

    // --- 1. 参照との一致 ---
    let current = normalize_boot_log(&smp2, false);
    if update_reference {
        if let Some(parent) = reference_path.parent() {
            fs::create_dir_all(parent).context("failed to create the reference directory")?;
        }
        fs::write(&reference_path, format!("{}\n", current.join("\n")))
            .context("failed to write the reference boot log")?;
        println!(
            "--- boot log diff: reference updated ({} line(s)) at {REFERENCE_BOOT_LOG}",
            current.len()
        );
    } else {
        let reference: Vec<String> = fs::read_to_string(&reference_path)
            .with_context(|| {
                format!(
                    "failed to read {REFERENCE_BOOT_LOG}; run `cargo xtask run --boot-log-diff \
                     --update-reference` once to record it"
                )
            })?
            .lines()
            .map(|l| l.to_string())
            .collect();
        match first_difference(&reference, &current) {
            None => println!(
                "--- boot log diff: matches the reference ({} normalized line(s)): OK",
                current.len()
            ),
            Some(report) => {
                println!("{report}");
                println!(
                    "--- boot log diff: differs from the reference: FAILED (if the change is \
                     intended, re-record with --update-reference and say so in the commit)"
                );
                failed = true;
            }
        }
    }

    // --- 2. 定義 3: コア数を変えても、コア数に依らない部分は同じ ---
    let core_free1 = normalize_boot_log(&smp1, true);
    let core_free2 = normalize_boot_log(&smp2, true);
    let core_free4 = normalize_boot_log(&smp4, true);
    for (label, other) in [("-smp 1", &core_free1), ("-smp 4", &core_free4)] {
        match first_difference(&core_free2, other) {
            None => println!(
                "--- boot log diff: {label} matches -smp 2 outside the core-count lines ({} \
                 line(s)): OK",
                core_free2.len()
            ),
            Some(report) => {
                println!("{report}");
                println!("--- boot log diff: {label} differs from -smp 2: FAILED");
                failed = true;
            }
        }
    }

    if failed {
        bail!("boot log diff: FAILED");
    }
    println!("boot log diff: PASS");
    Ok(())
}

/// 2 つの行列の最初の食い違いを、前後の文脈つきで報告する。
///
/// **全部の差分を出さない。** 1 行ずれると以降が全部ずれて出るので、
/// **最初の 1 箇所だけを見せるほうが原因へ近い。**
fn first_difference(expected: &[String], actual: &[String]) -> Option<String> {
    let limit = expected.len().min(actual.len());
    for index in 0..limit {
        if expected[index] != actual[index] {
            return Some(format!(
                "    line {}:\n      expected: {}\n      actual:   {}",
                index + 1,
                expected[index],
                actual[index]
            ));
        }
    }
    if expected.len() != actual.len() {
        return Some(format!(
            "    line count differs: expected {} line(s), got {}",
            expected.len(),
            actual.len()
        ));
    }
    None
}

/// 漂流の測定の既定の長さ（分）。**S6 の設計で先に固定した数である。**
const DRIFT_TEST_DEFAULT_MINUTES: u64 = 10;

/// 標本の間隔（ハートビート何本ごとか）。ハートビートは 1 秒に 1 本なので 10 秒。
const DRIFT_SAMPLE_STRIDE: usize = 10;

/// 漂流の測定（S6-c）。
///
/// # 何を主張するか
///
/// **定常状態で動かないはずの量が、実際に動かないこと。** 判定は
/// **「全標本が同一であること」**である。**傾きの推定はしない**——量は厳密な
/// 整数なので、揺れない。**多点の価値は「いつ動いたか」が特定できることにある。**
///
/// # 専用の出力経路を作っていない
///
/// **既に BKL の内側で出ているハートビートの行へ相乗りする**（`heap_free=` と
/// `heap_blocks=`）。行を増やせば混線の機会が増えるので増やさない。
/// **間引きはこちら側で行う**——カーネルには新しい周期の定数を置かない。
///
/// # 覆う範囲と、覆わない範囲
///
/// **覆うのは漂流であって、デッドロックではない。** BKL の待ちは
/// `WAIT_TIMEOUT_CYCLES`（実測で約 15 秒）で自分から報せて止まるので、
/// 10 分を要しない。
///
/// **10 分は導出値ではなく標本の予算である。** 10 分より長い周期で起きる事象は
/// 見えない。**フレームアロケータの量は測っていない**（下記）。
fn cmd_drift_test(minutes: u64, smp: Option<u32>) -> Result<()> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel(&workspace_root, false)?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root.join("target").join("drift-serial.log");
    let _ = fs::remove_file(&serial_log);
    let debug_log = workspace_root.join("target").join("qemu-debug.log");
    let _ = fs::remove_file(&debug_log);

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
    });
    if let Some(count) = smp {
        qemu_args.push("-smp".into());
        qemu_args.push(count.to_string().into());
    }

    let cores = smp.map_or("default".to_string(), |c| c.to_string());
    println!("=== drift test: {minutes} minute(s), -smp {cores}, sampling every {DRIFT_SAMPLE_STRIDE} heartbeat(s)");

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the drift test")?;

    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_secs(5));
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
    if let BootOutcome::DidNotStart { firmware_rip } =
        classify_boot(&serial, &qemu, KERNEL_STARTED_MARKER)
    {
        return report_did_not_start("drift test", firmware_rip, qemu_exit.as_deref());
    }

    let heartbeats: Vec<&str> = serial
        .lines()
        .filter(|l| l.contains("heartbeat: ticks="))
        .collect();
    let samples: Vec<&&str> = heartbeats.iter().step_by(DRIFT_SAMPLE_STRIDE).collect();
    println!(
        "--- drift test: {} heartbeat(s), {} sample(s)",
        heartbeats.len(),
        samples.len()
    );

    // **動かないはずの量。** 増える量（ティック等）はここに入れない。
    const INVARIANTS: &[&str] = &["heap_free=", "heap_blocks="];
    let mut failed = false;
    for field in INVARIANTS {
        let values: Vec<String> = samples
            .iter()
            .filter_map(|line| field_value(line, field))
            .collect();
        if values.len() != samples.len() {
            println!(
                "--- drift test: {field} missing from {} of {} sample(s): FAILED",
                samples.len() - values.len(),
                samples.len()
            );
            failed = true;
            continue;
        }
        let first = &values[0];
        match values.iter().position(|v| v != first) {
            None => println!(
                "--- drift test: {field}{first} identical across all {} sample(s): OK",
                values.len()
            ),
            Some(index) => {
                println!(
                    "--- drift test: {field} changed at sample {index} ({first} -> {}): FAILED",
                    values[index]
                );
                failed = true;
            }
        }
    }

    // **停止していないこと。** 動かない量が動かないだけでは足りない
    // （止まっていても動かない）。**進んでいる量が進んでいることを併せて見る。**
    let last_ticks = samples.last().and_then(|l| field_value(l, "ticks="));
    let first_ticks = samples.first().and_then(|l| field_value(l, "ticks="));
    match (first_ticks, last_ticks) {
        (Some(a), Some(b)) if a != b => println!("--- drift test: ticks advanced {a} -> {b}: OK"),
        (Some(a), Some(b)) => {
            println!("--- drift test: ticks did not advance ({a} -> {b}): FAILED");
            failed = true;
        }
        _ => {
            println!("--- drift test: could not read ticks from the samples: FAILED");
            failed = true;
        }
    }

    if failed {
        bail!("drift test: FAILED");
    }
    println!("drift test: PASS");
    Ok(())
}

/// ハートビートの行から `field=` に続く値を取り出す（区切りは空白かカンマ）。
fn field_value(line: &str, field: &str) -> Option<String> {
    let start = line.find(field)? + field.len();
    let rest = &line[start..];
    let end = rest.find([',', ' ']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn cmd_marker_test(
    tests: &[CriticalTest],
    kind_label: &str,
    kind: &str,
    smp: Option<u32>,
) -> Result<()> {
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

    let mut qemu_args = qemu_launch_args(&QemuLaunchOptions {
        ovmf_code: Path::new(OVMF_CODE_PATH),
        ovmf_vars: &ovmf_vars,
        esp_dir: &esp_dir,
        serial: &SerialSink::File(serial_log.clone()),
        debug_log: &debug_log,
        display: DisplayMode::None,
        monitor_socket: None,
        accelerator: Accelerator::Tcg,
    });
    // コア数を指定する構成（現在は ACPI の列挙が `-smp` と一致することの確認だけ）。
    // **既定は指定しない。** `QemuLaunchOptions` へ足さずここで付けるのは、
    // 他の起動経路の引数を一切変えないためである。
    if let Some(count) = smp {
        qemu_args.push("-smp".into());
        qemu_args.push(count.to_string().into());
    }

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

    // **`tlb-generation` だけ、マーカーでは表せない関係を見る（S7-d）。**
    // マーカーは部分文字列の有無しか言えないので、**絶対値でしか書けない。**
    // この探りが主張しているのは関係のほうなので、ここで別に確かめる。
    let mut relation_note: Option<String> = None;
    if test.name == "tlb-generation" {
        match check_tlb_generation_relation(&serial) {
            Ok(note) => relation_note = Some(note),
            Err(error) => {
                println!("{context}: the generation relation does not hold: {error}");
                bail!("{context}: FAILED")
            }
        }
    }
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
            || l.contains("acpi")
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
        if let Some(note) = relation_note {
            println!("{context}: generation relation OK ({note})");
        }
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

/// シリアルへの口を直接開けてよい箇所（[`DIRECT_SERIAL_PORT_ALLOWLIST`] 参照）。
struct DirectSerialPortSite {
    /// ワークスペース相対のパス。
    file: &'static str,
    /// その中の所属名（関数名）。
    item: &'static str,
    /// なぜ BKL の外から書いてよいのか。
    reason: &'static str,
}

/// シリアルへの口を直接開けてよい箇所の許可リスト（S6-b）。
///
/// # 名前は、見ているものを指す
///
/// **動機は「BKL の外から書いてよい箇所を固定する」だが、実際に見ているのは
/// `SerialPort::new(` を書いた箇所である。** 書き込みでも BKL の状態でもない。
/// **守りたいことではなく、見ているものを名前にしてある**（同じずれを 3 度
/// 起こしている。`verification-coverage.md` の失敗類型）。
///
/// # 何を固定していて、何を固定していないか
///
/// **固定するのは「どこから書いてよいか」だけである。** **許可された箇所どうしの
/// 混線は防げない**——S4-b-4 で、BKL の外の 2 行がバイト単位で混ざる実物を観測して
/// いる（`docs/verification-coverage.md`）。**この許可リストはその範囲を狭めるもので
/// あって、混線を無くすものではない。**
///
/// # 粒度の限界
///
/// **見ているのは `SerialPort::new(` を書いた箇所である。** `kernel/src/task.rs` の
/// `serial_line` のように**多数の呼び出しをまとめる出口**があると、**1 エントリが
/// その全部を覆う**（実測で 31 箇所）。**「どのファイルのどの関数が口を開けるか」は
/// 固定できるが、「その口を誰が使うか」は固定できない。**
///
/// # bootloader が対象外である理由
///
/// **bootloader には BKL が存在しない。** 「BKL の外」という区別自体が無いので、
/// 許可リストの意味が無い。**対象は `kernel/` と `common/` である。**
const DIRECT_SERIAL_PORT_ALLOWLIST: &[DirectSerialPortSite] = &[
    // (a) BKL 自身。**取れない**——ここで取ると、報せようとしている当の対象を
    // もう一度取ることになる。
    DirectSerialPortSite {
        file: "kernel/src/bkl.rs",
        item: "report_recursive_acquire_and_halt",
        reason: "BKL の再帰取得の報告。BKL を取れない当の経路である",
    },
    DirectSerialPortSite {
        file: "kernel/src/bkl.rs",
        item: "report_timeout_and_halt",
        reason: "BKL の待ちがタイムアウトした報告。取れないから報せている",
    },
    DirectSerialPortSite {
        file: "kernel/src/bkl.rs",
        item: "sabotage_hold_forever",
        reason: "破壊 bkl-hold-forever-test の実装。保持したまま報せる",
    },
    DirectSerialPortSite {
        file: "kernel/src/bkl.rs",
        item: "sabotage_enable_interrupts_while_held",
        reason: "破壊 bkl-hold-with-if-set-test の実装。保持したまま報せる",
    },
    // (b) 戻らない経路。**ADR-0023 で BKL を取らないと決めてある。**
    // 取らないと決めた以上、その経路の行も BKL の外にしかなりえない。
    DirectSerialPortSite {
        file: "kernel/src/panic.rs",
        item: "panic",
        reason: "パニックハンドラ（ADR-0004 の halt and dump、ADR-0023 で BKL を取らない）",
    },
    DirectSerialPortSite {
        file: "kernel/src/idt/mod.rs",
        item: "exception_entry",
        reason: "例外ハンドラ。依存を最小にする（ADR-0018）",
    },
    DirectSerialPortSite {
        file: "kernel/src/idt/mod.rs",
        item: "check_stack_alignment",
        reason: "スタブ入口の境界違反の報告。違反した状態で呼び出しを増やさない",
    },
    DirectSerialPortSite {
        file: "common/src/critical.rs",
        item: "report_contended_lock_and_halt",
        reason: "Locked<T> の競合の報告。停止する経路である",
    },
    DirectSerialPortSite {
        file: "common/src/critical.rs",
        item: "report_double_lock_and_halt",
        reason: "Locked<T> の二重取得の報告。停止する経路である",
    },
    DirectSerialPortSite {
        file: "kernel/src/console/screen.rs",
        item: "report_flush_failure_and_halt",
        reason: "画面への書き出しが失敗したときの報告。停止する経路である",
    },
    // (c) BKL より下位の機構。**BKL がこれらに依存しているので、逆向きに
    // 依存させられない。**
    DirectSerialPortSite {
        file: "kernel/src/main.rs",
        item: "kernel_main",
        reason: "ロガーを組み立てる起点。BKL はまだ無い",
    },
    DirectSerialPortSite {
        file: "kernel/src/gdt/mod.rs",
        item: "cpu_id_from_gdtr",
        reason: "cpu_id() 自身の失敗経路。BKL は cpu_id に依存する",
    },
    DirectSerialPortSite {
        file: "kernel/src/heap/allocator.rs",
        item: "log_directly_to_serial",
        reason: "確保に失敗したときの報告。ヒープ経由のログは再帰的に確保しうる",
    },
    // (d) AP の起動経路。**BKL に参加する前である。**
    DirectSerialPortSite {
        file: "kernel/src/smp.rs",
        item: "zaytos_ap_entry",
        reason: "AP の入口。per-CPU もスタックもまだ整っていない",
    },
    DirectSerialPortSite {
        file: "kernel/src/smp.rs",
        item: "bring_up_application_processor",
        reason: "AP の起こし。BKL へ参加する前の経過を出す",
    },
    DirectSerialPortSite {
        file: "kernel/src/smp.rs",
        item: "ap_after_switch",
        reason: "AP がスタックを切り替えた直後。BKL を取る区間は書き込みだけに絞ってある",
    },
    // (e) 同時進入の報告。**BKL の外にいることを報せる行なので、取れない。**
    DirectSerialPortSite {
        file: "kernel/src/idt/mod.rs",
        item: "report_concurrent_entry_once",
        reason: "カーネル内の同時進入の報告。BKL の外にいることが報告内容である",
    },
    // (f) **ここだけ性質が違う。**
    //
    // `serial_line` は**スケジューラの観測行の出口**で、31 箇所から呼ばれる
    // （S6-b で数えた）。**呼び出し元は BKL の内側と外側にまたがっている**——
    // `schedule_switch` や検出器は内側、デモの進行を出す行は外側である。
    //
    // **したがってこの 1 エントリは「BKL の外から書いてよい」を主張していない。**
    // 主張しているのは「シリアルへの口をここに 1 つだけ開ける」である。
    // **許可リストの粒度がこの出口までしか届かないことが、この検査の限界である**
    // （型の doc の「粒度の限界」）。
    DirectSerialPortSite {
        file: "kernel/src/task.rs",
        item: "serial_line",
        reason: "スケジューラの観測行の唯一の出口（31 箇所の呼び出しを覆う。粒度の限界）",
    },
];

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
    DirectInterruptControlSite {
        file: "kernel/src/smp.rs",
        item: "ap_heartbeat_loop",
        reason: "AP の定常ループ（S4-a）。sti;hlt 隣接で、BSP の run_timer_loop と同じ形で \
                 ある。**排他ではない。** この段の AP はタスクを実行せず、ハンドラが触る \
                 のは per-CPU かアトミックだけである（roadmap.md の S4-a）。**BKL が \
                 入る S4-b で、この一覧に依存した安全は不要になる。**",
    },
    // (h) BKL の排他（S4-b-2）。**自発的なクリティカルセクションではないので
    // 深さを数えない。** 数えると `on_timer_tick` の防御スキップと `on_yield` の
    // 判定が壊れる（`EntryInterruptGuard` の doc）。
    // **`EntryInterruptGuard::enter` と `::drop` のエントリはここにあった。**
    // **S6-d の死んだエントリの検査が見つけて外した。** 走査が作る所属名は
    // **裸の関数名**（`function_name_declared_on`）なので、**`型::メソッド` の
    // 書き方は一致しようがない。1 度も効いていなかった。**
    //
    // **実体は上の `enter` / `drop` の 2 件が覆っている**（同じファイルの同じ
    // 関数名なので、`InterruptGuard` と `EntryInterruptGuard` を**区別できない**）。
    // **これは粒度の限界である**——`serial_line` の 1 エントリが 31 箇所を覆うのと
    // 同じ形で、`docs/verification-coverage.md` に書いてある。
    DirectInterruptControlSite {
        file: "kernel/src/bkl.rs",
        item: "sabotage_enable_interrupts_while_held",
        reason: "破壊 feature 専用（bkl-hold-with-if-set）。**保持区間 = IF=0 の \
                 不変条件そのものを壊す。** 既定ビルドには存在しない",
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
        file: "kernel/src/smp.rs",
        item: "global_asm!",
        reason: "AP トランポリンの入口。**排他ではない。** SIPI 直後の AP は \
                 リアルモードで IDT を持たないので、割り込みが来ても行き先が無い。 \
                 InterruptGuard は 16 ビットの asm からは使えない。**対応する sti が \
                 無いのは設計である**（AP は IDT を載せずに halt するので、割り込みを \
                 有効化する地点が存在しない）。この検査を将来「cli と sti が対になって \
                 いること」へ強化するなら、**このエントリは意図的な例外として扱うこと。**",
    },
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
    // **死んだエントリも探す（S6-d）。** 一致しなかったエントリが残っていると、
    // **検査は緑のまま通り、一覧を読んだ人は「この箇所は許可されている」と読む。**
    // **一覧が静かに狭くなることの鏡像である。**
    let mut used = vec![false; DIRECT_INTERRUPT_CONTROL_ALLOWLIST.len()];
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

            let matched = DIRECT_INTERRUPT_CONTROL_ALLOWLIST
                .iter()
                .position(|site| site.file == relative && site.item == current_item);
            if let Some(index) = matched {
                *approved_occurrences += 1;
                used[index] = true;
                continue;
            }
            findings.push(format!(
                "{relative}:{} (in {current_item}): {}",
                index + 1,
                line.trim().chars().take(60).collect::<String>()
            ));
        }
    }
    for (index, hit) in used.iter().enumerate() {
        if !hit {
            let site = &DIRECT_INTERRUPT_CONTROL_ALLOWLIST[index];
            findings.push(format!(
                "dead allowlist entry (nothing matched): {} / {} / {}",
                site.file, site.item, site.reason
            ));
        }
    }
    Ok(findings)
}

/// 許可リストに無い場所でシリアルの口を開けている箇所を探す（S6-b）。
///
/// **`cli`/`sti` の走査と同じ形にしてある**（同じ `function_name_declared_on` で
/// 所属名を追い、コメント行を飛ばし、追跡済みと未追跡の両方を見る）。
/// **`asm!` の塊を追う必要はない**——シリアルは Rust の式でしか触らない。
fn find_unapproved_direct_serial_ports(
    workspace_root: &Path,
    approved_occurrences: &mut usize,
) -> Result<Vec<String>> {
    // 死んだエントリも探す（`find_unapproved_interrupt_control` と同じ理由）。
    let mut used = vec![false; DIRECT_SERIAL_PORT_ALLOWLIST.len()];
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
        // 対象は kernel と common だけ（許可リストの doc の「bootloader が対象外で
        // ある理由」）。xtask はホスト側で、この検査自身の文字列リテラルも入る。
        if !(relative.starts_with("kernel/") || relative.starts_with("common/")) {
            continue;
        }
        // ポートの実装本体は対象外。
        if relative == "common/src/serial.rs" {
            continue;
        }
        let path = workspace_root.join(relative);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let mut current_item = "<file scope>";
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(name) = function_name_declared_on(line) {
                current_item = name;
            }
            if trimmed.starts_with("///") || trimmed.starts_with("//!") || trimmed.starts_with("//")
            {
                continue;
            }
            if !line.contains("SerialPort::new(") {
                continue;
            }
            let matched = DIRECT_SERIAL_PORT_ALLOWLIST
                .iter()
                .position(|site| site.file == relative && site.item == current_item);
            if let Some(index) = matched {
                *approved_occurrences += 1;
                used[index] = true;
                continue;
            }
            findings.push(format!(
                "{relative}:{} (in {current_item}): {}",
                index + 1,
                line.trim().chars().take(60).collect::<String>()
            ));
        }
    }
    for (index, hit) in used.iter().enumerate() {
        if !hit {
            let site = &DIRECT_SERIAL_PORT_ALLOWLIST[index];
            findings.push(format!(
                "dead allowlist entry (nothing matched): {} / {} / {}",
                site.file, site.item, site.reason
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

/// Markdown ドキュメントの機械的に判定できる文体規則を見る（`docs/coding-standards.md` §1）。
///
/// # なぜ xtask に持つのか。ローカルの補助スクリプトでは規律のままである
///
/// 文体の検査にはローカル専用の補助スクリプトがあるが、**それは追跡対象外なので
/// `cargo xtask check` から呼べない**（呼ぶと、公開されるリポジトリの中から
/// 存在しないファイルを指すことになる）。回し忘れが実際に起き、閉じ括弧の直後に
/// 半角空白が入った差分がコミットされた。**規律で守っている箇所が残っていた**ので、
/// 機械で判定できる 2 つだけをここへ移し、コミット前に構造的に止まるようにする。
///
/// # ここで見るのは 2 つだけである。**補助スクリプトの代わりにはならない**
///
/// - S1: GFM タスクリストのマーカー直後に半角空白があること
/// - S2: 行結合の痕跡（和文の句読点・閉じ括弧の直後の半角空白、および直前に
///   空白の無い `/` の直後の半角空白）
///
/// **レンダリング結果を要する検査は移していない**（閉じない強調、タグ列の比較、
/// 見出しレベルの飛び、リンク先の生存）。Markdown レンダラが必要で、そのために
/// 新しいクレート依存を増やす判断はしていない。**したがって補助スクリプトは
/// 引き続き要る。** ここが覆うのは「回し忘れても落ちる」範囲だけである。
///
/// # 対象は追跡対象の `.md` だけ
///
/// `git ls-files` で引く。未追跡の作業メモを対象にすると、コミットに関係のない
/// ファイルで落ちる。
fn check_markdown_prose_style(workspace_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["ls-files", "*.md"])
        .output()
        .context("failed to run git ls-files for the markdown style check")?;
    if !output.status.success() {
        bail!("git ls-files failed for the markdown style check");
    }
    let listing = String::from_utf8_lossy(&output.stdout);

    let mut findings = Vec::new();
    for rel in listing.lines().filter(|l| !l.trim().is_empty()) {
        let path = workspace_root.join(rel);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let mut in_fence = false;
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;

            // S1 はコードフェンスの内外を問わず全行を見る（補助スクリプトと同じ）。
            if let Some(rest) = task_list_marker_rest(line) {
                if !rest.starts_with(' ') && !rest.is_empty() {
                    findings.push(format!(
                        "{rel}:{number}: S1 タスクリストのマーカー直後に空白が無い: {}",
                        line.trim()
                    ));
                }
            }

            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            // フェンス内・インデントコード・表の行は S2 の対象にしない。
            if in_fence || line.starts_with("    ") || line.trim_start().starts_with('|') {
                continue;
            }
            if let Some(column) = join_trace_column(line) {
                findings.push(format!(
                    "{rel}:{number}: S2 行結合の痕跡（列 {column}）: {}",
                    line.trim()
                ));
            }
        }
    }
    Ok(findings)
}

/// GFM タスクリストのマーカー（`- [ ]` / `- [x]`）に一致したら、その直後を返す。
fn task_list_marker_rest(line: &str) -> Option<&str> {
    let rest = line.trim_start();
    let rest = rest.strip_prefix(['-', '*', '+'])?;
    let rest = rest.strip_prefix(' ')?;
    let rest = rest.strip_prefix('[')?;
    let mut chars = rest.chars();
    let marker = chars.next()?;
    if !matches!(marker, ' ' | 'x' | 'X') {
        return None;
    }
    chars.as_str().strip_prefix(']')
}

/// 行結合の痕跡を探し、見つかった位置（1 起点の文字数）を返す。
///
/// 探すのは 2 形である。**コードスパン（バッククォート）の中は対象にしない。**
///
/// - 和文の句読点・閉じ括弧（`、。」』）`）の直後が半角空白 1 個 + 非空白
/// - `/` の直後が半角空白 1 個 + 非空白で、かつ `/` の直前が空白でも `/` でもない
///
/// 後者の条件は、箇条書きの区切りに使う ` / ` を許し、行を結合したときに現れる
/// `語/ 語` の形だけを捕まえるためである。
fn join_trace_column(line: &str) -> Option<usize> {
    let masked = mask_code_spans(line);
    for (index, window) in masked.windows(3).enumerate() {
        let [current, next, after] = [window[0], window[1], window[2]];
        if next != ' ' || after.is_whitespace() {
            continue;
        }
        if matches!(current, '、' | '。' | '」' | '』' | '）') {
            return Some(index + 1);
        }
        if current == '/' {
            let previous = index.checked_sub(1).map(|i| masked[i]);
            let guarded = matches!(previous, Some(' ') | Some('/'));
            if !guarded {
                return Some(index + 1);
            }
        }
    }
    None
}

/// バッククォートで囲まれた範囲を、判定に引っかからない文字で潰す。
///
/// 潰す先は `\0` にしてある。**空白にはしない。** 空白にすると、コードスパンの
/// 直後にある区切りが「直前が空白」に見えて、S2 の条件が変わってしまう。
fn mask_code_spans(line: &str) -> Vec<char> {
    let mut out: Vec<char> = Vec::new();
    let mut in_span = false;
    for ch in line.chars() {
        if ch == '`' {
            in_span = !in_span;
            out.push('\0');
            continue;
        }
        out.push(if in_span { '\0' } else { ch });
    }
    out
}

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

/// ACPI の検証経路の破壊確認（S1-b-2）。
///
/// **観測は panic ではない。** S1 の ACPI 経路は異常を見つけても停止しないので、
/// 「検出のログが出ること」と「MADT の列挙が完了しないこと」の 2 つで判定する。
/// さらにハートビートを期待マーカーに入れて、**検出した後もカーネルが動き続ける
/// こと**まで見る。停止してしまえば機能的な後退であり、それはそれで異常である。
///
/// 禁止マーカーの `acpi: MADT enumeration complete` は、**列挙が最後まで通った
/// ときにだけ出る行**である。検出をすり抜けた場合にこれが出る。
const ACPI_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "bad-signature",
        feature: "acpi-test-bad-signature",
        expected_markers: &[
            "acpi: the MADT at",
            "has the wrong signature",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["acpi: MADT enumeration complete"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "bad-checksum",
        feature: "acpi-test-bad-checksum",
        expected_markers: &["failed its checksum", "heartbeat: ticks="],
        forbidden_markers: &["acpi: MADT enumeration complete"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "bad-length",
        feature: "acpi-test-bad-length",
        // 署名の破れ（`the wrong signature`）とは別の経路であることを、
        // 期待する文言そのもので示す。
        expected_markers: &["failed validation: LengthTooSmall", "heartbeat: ticks="],
        forbidden_markers: &["acpi: MADT enumeration complete"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "zero-entry-length",
        feature: "acpi-test-zero-entry-length",
        // **チェックサムではなくエントリ長で止まったことを確かめる。** 破壊側で
        // チェックサムを合わせ直してあるので、`failed its checksum` は出ない。
        expected_markers: &[
            "acpi: the MADT entry walk stopped",
            "ZeroLength",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["acpi: MADT enumeration complete", "failed its checksum"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "unmapped-rsdp",
        feature: "acpi-test-unmapped-rsdp",
        expected_markers: &[
            "[acpi-test-unmapped-rsdp] redirecting",
            "which the live page table does not map",
            "heartbeat: ticks=",
        ],
        // 差し替え先が見つからなければ破壊が成立しない。**「壊したつもり」で
        // 緑になる形を塞ぐ。**
        forbidden_markers: &[
            "acpi: MADT enumeration complete",
            "the sabotage did nothing",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "rsdp-outside-window",
        feature: "acpi-test-rsdp-outside-window",
        expected_markers: &[
            "[acpi-test-rsdp-outside-window] redirecting",
            "outside the direct map window",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &[
            "acpi: MADT enumeration complete",
            "the sabotage did nothing",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// `-smp 2` での ACPI 列挙の確認（S1-b-2）。
///
/// **既定ビルドである**（`feature` が空）。壊すのではなく、コア数を変えた構成で
/// 列挙結果が QEMU の指定と一致することを見る。roadmap の S1 到達条件に
/// 「`-smp 2` で起動しても列挙結果が QEMU の指定と一致すること」とあるのに、
/// **手動実行でしか確かめていなかった。手動確認は再現されず、必ず腐る。**
///
/// 全項目を複数のコア数で回すと項目数も所要時間もそのまま倍になるので、
/// **この 1 項目だけを 2 コアで回す。** 費用は QEMU 起動 1 回である。
/// S3-b-2a の tripwire の破壊確認（`--percpu-test <kind>`）。
///
/// `task::require_bootstrap_processor` は GPR 照合デモが bootstrap processor 以外で
/// 走ることを拒む。`cpu_id()` に非 `0` を返させて、**その分岐が働くこと**を見る。
///
/// # **示すのは分岐が働くことだけである**
///
/// **実際の並行アクセスは示さない。** `GPR_BUF` が別コアから同時に触られる状況を
/// 作ってはおらず、AP は起こしていない。**機序の直接観測は「AP がタスクを実行する
/// 段」の到達条件である**（`roadmap.md`）。
///
/// # この破壊は `MAX_CPUS > 1` でなければ作れない
///
/// `MAX_CPUS = 1` のまま非 `0` を返すと `this_cpu_ptr` が配列外を指し、
/// **破壊が別の未定義動作を作ってしまう。** S3-b-2a で `MAX_CPUS` を 2 へ
/// 上げたので初めて構成できるようになった。**tripwire の破壊確認には、
/// tripwire が守ろうとしている能力そのものが必要である。**
/// 設置した AP トランポリンが雛形と一致することの破壊確認（S3-b-2b-1）。
///
/// **壊すのはコピーであって雛形ではない。** 雛形を壊すとコピー元が変わるだけで
/// 両方が同じ値になり、比較は通ってしまう。**検査が見ているのは「コピーとパッチが
/// 正しく行われたか」なので、壊すべきはコピー側である。**
/// AP の per-CPU 資産と CURRENT の sentinel の破壊確認（S3-b-2b-2）。
const SMP_AP_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "ap-touch-scheduler",
        feature: "smp-ap-touch-scheduler-test",
        expected_markers: &[
            "is about to read the scheduler",
            "CURRENT is still the sentinel",
            "halting",
        ],
        // **BSP は走り続ける**（止まるのは AP だけ）ので、ハートビートは出る。
        // 禁止するのは **AP が最後まで進んだこと**である。丸めていたら停止せず、
        // タスク 0 を走らせているように見えたまま、ここまで来たはずである。
        forbidden_markers: &["is up with its own per-CPU state"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "ap-timer",
        feature: "",
        expected_markers: &[
            // AP が自分の SVR を書き、**ソフトウェア有効化が立つこと**。
            "ap 1 wrote its own SVR",
            "software_enabled=true",
            // AP が自分の LVT タイマを開けたこと。
            "ap 1 armed its own LAPIC timer",
            // **コアごとのハートビート。** AP 側は BSP と別の行である。
            "smp: ap heartbeat: cpu=1",
            // **AP のティックが進んでいること。** `cpu1=0` を禁止マーカーで落とす
            // だけだと、行そのものが出ない構成を通してしまう。
            "ap_ticks=cpu1=",
            // **会計が閉じること。**
            "timer_accounting_balanced=true",
            // 定常状態まで到達すること。
            "heartbeat: ticks=",
        ],
        // **AP のティックが 1 本も進んでいない状態を落とす。** ハートビートは
        // 100 ティックごとなので、1 本目のハートビートが出る時点で AP は既に
        // 数え始めている。
        forbidden_markers: &[
            "ap_ticks=cpu1=0,",
            "timer_accounting_balanced=false",
            "halting",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "ap-no-svr",
        feature: "smp-ap-timer-no-svr-test",
        expected_markers: &[
            "is skipping its own SVR write (sabotage)",
            // **落ちるのは「開けられない」ではなく「届かない」である。**
            // LVT への書き込みそのものは成功する（実測）。SVR の bit 8 が
            // 落ちているので Local APIC が無効で、**割り込みが 1 本も配送されない。**
            // したがって AP のティックが 0 のまま止まる。
            //
            // **BSP の書き込みが AP に効いていないことの実証でもある。**
            // 効いていれば、AP が書かなくてもティックが来てしまう。
            "ap_ticks=cpu1=0,",
            // BSP は走り続ける。**起動が死んだのではないことを分けて見る。**
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["smp: ap heartbeat: cpu=1"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "ap-share-ticks",
        feature: "smp-ap-timer-share-ticks-test",
        expected_markers: &[
            // **会計が閉じないことを名指しで見る。** 合計が配送数のおよそ 2 倍になる。
            "timer_accounting_balanced=false",
        ],
        forbidden_markers: &["timer_accounting_balanced=true"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **S4-a の `ap-enter-scheduler` はここにあった。S4-c-3-2b で引退した。**
    // 破壊の対象だった「AP を手前で返す分岐」が本番から消えたので、
    // **「分岐を外す」破壊は構成できない。** 役目（sentinel が止めることの実証）は
    // 下の `ap-no-sentinel-clear` が引き継いでいる。**引退の記録は
    // `docs/verification-coverage.md` の破壊 feature 一覧にある。**
    CriticalTest {
        name: "ap-no-sentinel-clear",
        feature: "smp-ap-no-sentinel-clear",
        expected_markers: &[
            // **sentinel が止める。** `ap-enter-scheduler` から引き継いだ主張で、
            // **同じ行**を見ている（引き継ぎが成立していることの担保）。
            "CURRENT is still the sentinel",
        ],
        // AP は最初のティックで止まるので、AP のハートビートは出ない。
        forbidden_markers: &["smp: ap heartbeat: cpu=1"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **tripwire の機序の直接観測（S4-c-4-1）。**
    //
    // 主マーカーは**停止行そのもの**である。AP のハートビートが出ないことは
    // **補助**であって、主マーカーにはしない（回数より機序の直接観測）。
    CriticalTest {
        name: "ap-runs-preemptive-demo",
        feature: "smp-ap-runs-preemptive-demo",
        expected_markers: &["may only run on the bootstrap processor"],
        forbidden_markers: &[
            // tripwire が鳴らずに戻ってきた形。
            "the tripwire did not fire",
            // 補助: 入口で止まるので AP はタイマを開けず、ハートビートも出ない。
            "smp: ap heartbeat: cpu=1",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **S4-c-4-2 の梯子。ここに置けたのは下 2 段だけである。**
    //
    //   tripwire 外し                 → AP がワーカーを取れない = 第 1 層の実証（対照）
    //   tripwire 外し + ignore-owner  → AP がワーカーを取れる   = 第 1 層の実証（本命）
    //
    // **この 2 つは対になって第 1 層の効きを直接示す。** 差は第 1 層 1 つだけで、
    // 「取れない」と「取れる」が `selected task ... owned by cpu` の 1 行で分かれる。
    //
    // **第 2 層と検出器の実証はここには置けなかった。** デモ経由では窓が閉じる——
    // `setup_preemptive_tasks` が `demo_deadline` を**呼んだコアのティック**で
    // 決めるのに対し、`on_timer_tick` の締切判定は**各コアが自分のティック**で
    // 行う。AP のティックは 0、bootstrap processor は既に数百なので、
    // **BSP は次のティックでワーカーを `Blocked` に戻す。** 競合が起きないまま
    // 窓が閉じるので、「検出行が出ない」は守りの効きを示さない。
    // 詳細と代案は `docs/verification-coverage.md` にある。
    //
    // **「窓があるか」は `selected task ... owned by cpu` の 1 行で見る。**
    // ハートビートの `ap_current` は 1 秒ごとの標本なので取り逃しうる。
    // **こちらは起きた瞬間に 1 度だけ出るので、観測が運に依存しない。**
    //
    // **判定に使わない観測量**（走らせる前に決めてある）:
    // GPR 照合の結果、`switches`、`sum(resumes)`。第 1 層を外すと `GPR_BUF` が
    // コア間で競合し、デモの会計もコア別ではないので合わない。**どちらも構成の
    // 帰結であって退行ではない。**
    // **名前は主張を指す（S4-c-4-3で改名した）。**
    //
    // 旧名は `ap-demo-layer1-holds` で「第1層が保つ」と読めたが、**第1層の実証は
    // S4-c-4-3 の梯子が担っている。** この項目が表明しているのは
    // **「`CURRENT` を外から 0 にされると、AP のアイドルタスクの `saved_rsp` が
    // 0 のまま切り替え先になり、範囲検査が止めること」**である。
    // **名前が主張を指さないと、一覧を読んだ人が誤って引退させる。**
    CriticalTest {
        name: "ap-forced-current-range-check",
        feature: "smp-ap-runs-preemptive-demo,sched-ignore-bootstrap-tripwire",
        // **AP は落ち先（自分のアイドルタスク）へ回され、そこで停止する。**
        //
        // `setup_preemptive_tasks` が `set_current_index(0)` を呼ぶので AP の
        // `CURRENT` が 0 になり、次の選択でアイドルタスクが**切り替え先**に
        // なる。その `saved_rsp` は 0 のままなので範囲検査が捕まえる。
        // **停止まで含めて表明する**——書かないと「穏やかに落ち先へ回った」と
        // 読まれるが、実際には止まっている。
        expected_markers: &[
            "starting preemptive demo",
            "is outside its stack",
            "stacks are mixed",
        ],
        forbidden_markers: &[
            // 第 1 層が効いているので、AP は担当外のワーカーを取れない。
            "the first guard layer did not keep it out",
            // **`double selection detected` はここにあった。S6-a で外した。**
            //
            // **この構成では正当に鳴りうる。** `setup_preemptive_tasks` は先頭で
            // `set_current_index(0)` を呼ぶので、**AP の `CURRENT` が一時的に
            // タスク 0 を指す。** その窓の間に bootstrap processor がティックを
            // 受けると、走行可能な担当ワーカーがまだ無いので落ち先の 0 を選び、
            // **検出器は「タスク 0 が他コアの current でもある」を見て鳴る。**
            //
            // **検出器は誤っていない。** `currents` は本当に `[0, 0]` である。
            // **誤っていたのはこの禁止マーカーのほうである**——鳴らないことを
            // 要求していたが、**鳴るかどうかは窓に重なるかどうかで決まる。**
            //
            // **この項目はもう二重選択を主張しない。** その主張は S4-c-4-3 の
            // 梯子（`smp-stimulus-*`）が担っている。詳細と、窓を広げて観測した
            // 記録は `verification-coverage.md` にある。
        ],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // **`ap-demo-layer1-off` はここにあった。S4-c-4-3 で引退した。**
    //
    // **主張が重なったためである。** あれは「第 1 層を外すと AP が担当外の
    // ワーカーを取れる」を示していたが、**同じ主張を S4-c-4-3 の梯子がより強い
    // 構成で示す**——あちらの窓は「bootstrap processor がワーカーを回している
    // 間ずっと」開くのに対し、こちらはデモ経由で**締切の食い違いに依存する
    // 短い窓**だった。**弱いほうを引退させる。**
    //
    // **上の `ap-demo-layer1-holds` は残す。** あちらは第 1 層の実証ではなく、
    // **`CURRENT` を外から 0 にされたときにアイドルタスクの `saved_rsp` が 0 の
    // まま読まれ、範囲検査が止めること**を表明している。**S4-c-4-3 の梯子は
    // その事象を作らないので、主張は重ならない。**
    // **S4-c-4-3 の梯子。隣り合う 2 つが層 1 つぶんだけ違う。**
    //
    //   刺激のみ                          → 担当外選択なし・検出行なし = 刺激は単独で何も起こさない
    //   刺激 + ignore-owner               → 担当外選択あり・検出行なし = 第 1 層と第 2 層の実証
    //   刺激 + ignore-owner + ignore-current → 検出行あり              = 検出器の実証
    //
    // **窓は 3 構成すべてに在る**（ワーカーが `Ready` で bootstrap processor が
    // 回している）。**1 段目で担当外選択が出ないのは、窓が無いからではなく
    // 第 1 層が働いているからである。** ここが S4-c-3-2b で踏んだ形との違いで、
    // **「緑だが何も検査していない」に戻らない。**
    //
    // **停止は検出行を妨げない。ただし無条件ではない。**
    // `halt_forever` が止めるのは**呼んだコアだけ**なので、片方が GPR 照合で
    // 止まってももう片方は回り続ける。**しかし検出を行うコア自身が先に停止すれば
    // マーカーは出ない。** 今回それが起きていないことは**5/5 の実測が根拠であって、
    // 機序の保証ではない。** 「観測が停止までの範囲に限られる」と同じ性質の注記である。
    //
    // **判定に使わない観測量**（走らせる前に決めてある）: GPR 照合の結果、
    // `switches`、`sum(resumes)`、デモの進捗判定。第 1 層を外すと `GPR_BUF` が
    // コア間で競合し、会計もコア別ではないので合わない。**構成の帰結であって
    // 退行ではない。**
    CriticalTest {
        name: "smp-stimulus-only",
        feature: "sched-keep-workers-runnable",
        expected_markers: &["demo workers are runnable again"],
        forbidden_markers: &[
            "the first guard layer did not keep it out",
            "double selection detected",
        ],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "smp-stimulus-layer1-off",
        feature: "sched-keep-workers-runnable,sched-ignore-owner",
        // **働いた側を直接観測する。** 「検出行が出ないこと」だけだと、
        // `GPR_BUF` の競合で系が止まった場合と区別できない（毎回止まることを
        // 実測した）。**第 2 層が候補を弾いた行を要求する。**
        expected_markers: &[
            "the first guard layer did not keep it out",
            "the second guard layer skipped a candidate",
        ],
        // **観測は停止までの範囲に限られる。** この構成では `GPR_BUF` が
        // コア間で競合し、**GPR 照合が毎回停止する**（5/5 で実測）。
        // **停止後に出るはずの行を期待マーカーへ足さないこと。**
        forbidden_markers: &["double selection detected"],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "smp-stimulus-both-layers-off",
        feature: "sched-keep-workers-runnable,sched-ignore-owner,sched-ignore-current",
        expected_markers: &[
            "the first guard layer did not keep it out",
            "double selection detected",
        ],
        forbidden_markers: &[],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **IPI が届くことの主張（S5-a）。** 破壊ではなく、**機能そのものの検査**である。
    //
    // **判定は 2 つとも要る**——「受け取った本数が送った本数と一致すること」と
    // 「AP が生き続けること」。**前者だけだと、AP が死んでいても 0 と 0 で
    // 一致してしまう。** 実際、受け口を用意する前は AP が死んで両方 0 だった。
    CriticalTest {
        name: "ipi-probe",
        feature: "smp-ipi-probe",
        expected_markers: &[
            // 送受信が一致した要約行。**本数まで含めて固定する。**
            "sent=4 received=4",
            // AP が生き続けている証拠。**ハートビートが出るのは死んでいない側だけ。**
            "smp: ap heartbeat: cpu=1",
        ],
        forbidden_markers: &[
            "did not accept a probe IPI",
            "did not handle it within the spin limit",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **世代方式の実証（S5-b）。** 世代を 1 つ上げると、**AP が次の取得で
    // フラッシュする。** 入口はティックごとに BKL を取るので、**遅くとも
    // 1 ティックで整合する。**
    //
    // **判定は 2 つとも要る**——AP がフラッシュしたことと、AP が生き続けること。
    // **前者だけだと、フラッシュして死んでいても通る。**
    CriticalTest {
        name: "tlb-generation",
        feature: "smp-tlb-generation-probe",
        // **絶対値で期待しない（S7-d で直した）。** 以前は
        // `bumped the tlb generation to 1` と `tlb_gen=1 flush_cpu1=1` を見ていたが、
        // **それは「起動の間に世代を動かすものが他に無い」という、この探りの主張とは
        // 無関係な事情に依存していた。** S7-d のアドレス空間のデモが既定ビルドで
        // 世代を 2 つ消費した時点で落ちた。
        //
        // **関係のほうを見る**（[`check_tlb_generation_relation`]）——**探りが上げた
        // 世代に AP が追いつき、そのために 1 回以上フラッシュしたこと。**
        // **カーネルの出力は変えていない。** 既にある行から関係を導いている。
        expected_markers: &["smp: ap heartbeat: cpu=1"],
        forbidden_markers: &[],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    // **TLB シュートダウンの実証（S5-c。S8-d で主張を作り直した）。** 世代を
    // 上げた側は AP がフラッシュし（the ap flushed）、**2 回目の触りは必ず #PF に
    // なる**——翻訳が無いので歩き、写像が無いので落ちる。フラッシュの帰結として
    // 保証される側なので、#PF のダンプ（vector=14、CR2=探りのページ）まで期待する。
    CriticalTest {
        name: "tlb-shootdown",
        feature: "smp-tlb-shootdown-probe",
        expected_markers: &[
            "the ap touched the probe page 1 time(s)",
            "the ap flushed",
            "touches 1 -> 1",
            "exception: vector=14",
            "cr2=0xffff818000000000",
        ],
        forbidden_markers: &["touches 1 -> 2", "the ap did not flush"],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // **破壊: 世代を上げない。** AP はフラッシュしない（the ap did not flush）。
    //
    // **2 回目の触りの結果は主張しない（S8-d で作り直した）。** かつては
    // 「古い翻訳で成功する（touches 1 -> 2）」を期待に置いていたが、**TLB が翻訳を
    // 保持し続けることはアーキテクチャが許しているだけで約束していない。**
    // TCG のソフトウェア TLB は無効化事象なしにエントリを捨て、どれを捨てるかは
    // バイナリのレイアウトで決まるため、無関係な変更で決定的に落ちた（S8-d）。
    // この破壊の本来の主張は「世代を上げなければ AP は世代フラッシュをしない」で
    // あり、それは flushes の不動が観測している。
    CriticalTest {
        name: "tlb-no-shootdown",
        feature: "smp-tlb-shootdown-probe,smp-tlb-no-generation-bump",
        expected_markers: &[
            "the ap touched the probe page 1 time(s)",
            "the ap did not flush",
        ],
        forbidden_markers: &["the ap flushed ("],
        wait_for_full_timeout: true,
        min_heartbeats: None,
    },
    // **宛先の主張の破壊（S4-a）。** 確実に落ちるのは読み戻しの主張のほうで、
    // 配送が実際にどうなるかは観測していない。
    CriticalTest {
        name: "ioapic-keyboard-broadcast",
        feature: "ioapic-keyboard-broadcast-test",
        expected_markers: &["IRQ1 is not aimed at the bootstrap processor in physical mode"],
        forbidden_markers: &["heartbeat: ticks="],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// BKL 待ちのタイムアウト（S4-b-4）。**TCG で回る。**
///
/// # KVM を要さない
///
/// タイムアウトの機序は「保持者が離さない + 待ち側の TSC が進む」であって、
/// **同時実行を要さない。** 実測でも TCG のラウンドロビンで発火した
/// （起動を含めて約 15 秒）。**KVM 必須の項目を最小に保つ。**
const BKL_TIMEOUT_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "hold-forever",
    feature: "bkl-hold-forever-test",
    expected_markers: &[
        "took the lock and will never release it (sabotage)",
        // **待ちの上限に達したことと、原因の中身。**
        "has waited",
        "without acquiring the lock",
        "the lock reads as held by cpu",
        "halting",
    ],
    // **完走しない構成である。** AP が止まるので定常状態へ来ない。
    forbidden_markers: &[],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

/// BKL の破壊（S4-b-2）。**いずれも名指しの検出で停止する。**
const BKL_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "hold-with-if-set",
        feature: "bkl-hold-with-if-set-test",
        expected_markers: &[
            "enabling interrupts while holding the lock (sabotage)",
            // **再帰検出が発火する。** 保持区間 = IF=0 の不変条件が崩れると、
            // 同じコアが irq_entry から取ろうとする。
            "bkl: recursive acquisition on cpu",
            "halting",
        ],
        forbidden_markers: &[],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "hold-across-hlt",
        feature: "bkl-hold-across-hlt-test",
        expected_markers: &[
            // **保持したまま `hlt` すると、次に自分が入口へ入るときに再帰になる。**
            // 単一コアでも観測できるのはこのためである（もう一方のコアが要らない）。
            "bkl: recursive acquisition on cpu",
            "halting",
        ],
        forbidden_markers: &[],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

const SMP_TRAMP_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "corrupt-copy",
    feature: "smp-tramp-corrupt-copy-test",
    expected_markers: &["diverges from the template at offset", "halting"],
    forbidden_markers: &["application processor 1 started", "heartbeat: ticks="],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

const PERCPU_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "fake-nonzero-cpu-id",
    feature: "percpu-fake-nonzero-cpu-id",
    expected_markers: &["may only run on the bootstrap processor", "halting"],
    // 停止せずにデモへ進んでいたら、tripwire が働いていない。
    forbidden_markers: &["task: cooperative switch verified", "heartbeat: ticks="],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

/// per-CPU スロットが足りない構成の確認（`-smp 4`、S3-b-2a）。
///
/// # なぜ `-smp 4` を別に置くのか
///
/// **覆いの報告の「覆えていない」側を評価する構成を残すためである。**
/// `MAX_CPUS = 2` なので `-smp 2` では覆えてしまい、`false` の枝が通らない。
///
/// **これを置かないと、同じ失敗が再発しても次の段まで気づけない。** S3-b-1 で
/// 覆いの検査を `halt_forever` で入れたときに `-smp 2` の起動が止まり、
/// **既存の検査は緑のままだった**（見ているマーカーが停止点より前にあった）。
/// **`false` 側を評価する構成が無い状態を作らない。**
///
/// # 主張は 2 つある。**片方だけでは足りない**
///
/// - 警告が出ること（`false` 側が実際に評価されている）
/// - **完走すること**（`heartbeat: ticks=`）。**警告だけを見ると、また停止に
///   戻ったときに捕まらない。**
///
/// コア数は `cmd_marker_test` の引数なので、`-smp 2` の表とは別に持つ。
const ACPI_SMP4_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "smp4-more-cpus-than-slots",
    feature: "",
    expected_markers: &[
        "4 local APIC(s) of which 4 usable",
        // 覆いの報告の `false` 側。**MAX_CPUS を 4 以上へ上げると出なくなるので、
        // そのとき この項目が落ちる**（意図した破壊確認である）。
        "there are more usable CPUs than per-CPU slots",
        // **`MAX_CPUS` を超えるコアは起こさない**（S3-b-2b-1 の方針）。
        // 起こした分は署名を出し、**起こさなかった分があること**も主張する。
        // **後者が無いと「全部起こしてしまった」を捕まえられない。**
        "smp: application processor 1 started",
        "1 AP(s) attempted, 1 started, 2 skipped",
        // 警告を出しても停止しないこと。
        "heartbeat: ticks=",
    ],
    forbidden_markers: &["halting"],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

const ACPI_SMP_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "smp2-enumeration",
    feature: "",
    expected_markers: &[
        "acpi: MADT enumeration complete",
        "2 local APIC(s) of which 2 usable",
        // **AP が実際に起きて署名を出すこと**（S3-b-2b-1）。
        "smp: application processor 1 started",
        "1 AP(s) attempted, 1 started, 0 skipped",
        // **AP が自分の per-CPU 資産を持って本番 CR3 へ移ったこと**（S3-b-2b-2）。
        // GDTR 由来の cpu_id とデータブロックの索引が一致することも見る。
        "cpu_id() now reads 1 from GDTR",
        "match=true",
        "PML4[0] read back from this core = empty:true",
        "ap 1 is up with its own per-CPU state",
        // **`-smp 2` で定常状態まで到達すること**（S3-b-1）。
        //
        // この 1 行を足す前は、上の 2 つが出た時点で打ち切っていたので、
        // **打ち切りより後で起動が止まる退行を構造的に捕まえられなかった。**
        // 実際に踏んだ: per-CPU スロットの覆いの検査を停止付きで入れたところ、
        // 「2 コア列挙 / スロット 1」で halt して `-smp 2` の起動が止まったが、
        // **この項目は緑のままだった**（見ているマーカーが停止点より前にある）。
        //
        // 期待マーカーに入れると、打ち切りの条件が「これも出るまで待つ」に
        // 変わるので、**定常状態まで進むことが要求される。**
        "heartbeat: ticks=",
    ],
    // 1 個しか出ないのは、`-smp` が効いていないか列挙が取りこぼしているかの
    // どちらかである。**どちらも見逃したくない。**
    //
    // `halting` は**二次的な網である。** `cpu::halt_forever()` 自身はこの語を
    // 出さない（呼び出し側がログへ書いたときにだけ現れる）ので、**ログを書かずに
    // 停止する経路は捕まえられない。** 実測で確認した（停止を一時的に戻すと、
    // 落としたのは `heartbeat: ticks=` のほうで、この禁止マーカーは素通りした）。
    // **停止を捕まえている主たる根拠は、上の `heartbeat: ticks=` である。**
    forbidden_markers: &["1 local APIC(s) of which 1 usable", "halting"],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

/// `--calibration-spread` の既定の起動回数。
///
/// S1-b でメモリ型の確認に 5 回使った前例に揃えてある。
const DEFAULT_CALIBRATION_RUNS: usize = 5;

/// Local APIC タイマの較正結果を、複数回の起動にわたって集める（S2-c）。
///
/// **これは検査項目ではない。** 合否を判定せず、値を並べて出すだけである。
/// 許容幅を決めるための入力を人が読む形で集めるのが目的で、`--full` には
/// 入れていない（項目数は増えない）。
///
/// **手で 5 回回して記録する形を採らないのは、手動確認が再現されず必ず腐るから
/// である**（`-smp 2` の確認を検査項目にしたのと同じ判断）。ここでは合否を
/// 決められないので検査項目にはできないが、**手順だけはコマンドとして固定する。**
fn cmd_calibration_spread(runs: usize) -> Result<()> {
    println!("=== calibration spread over {runs} boot(s) ===");
    let mut medians: Vec<u64> = Vec::new();

    for run in 1..=runs {
        let serial = capture_serial_for_calibration(run)?;
        let mut found = false;
        for line in serial.lines() {
            if line.contains("apic: LAPIC timer calibration:") {
                println!("run {run}: {}", line.trim());
                if let Some(median) = parse_labelled_number(line, "median=") {
                    medians.push(median);
                    found = true;
                }
            } else if line.contains("calibration samples:") || line.contains("widest tick advance")
            {
                println!("run {run}: {}", line.trim());
            }
        }
        if !found {
            println!("run {run}: no calibration line was produced");
        }
    }

    if medians.is_empty() {
        return Err(anyhow::anyhow!(
            "no calibration result was captured in {runs} run(s)"
        ));
    }

    medians.sort_unstable();
    let low = medians[0];
    let high = medians[medians.len() - 1];
    let spread = high - low;
    // 相対ばらつきを ppm で出す。**許容幅はここから導く。**
    let relative_ppm = spread.saturating_mul(1_000_000) / high.max(1);
    println!(
        "--- across {} run(s) that produced a value ---",
        medians.len()
    );
    println!("min={low} Hz max={high} Hz spread={spread} Hz ({relative_ppm} ppm of max)");
    println!(
        "note: this command reports values only. it does not decide pass or fail, so it is \
         not one of the `--full` check items."
    );
    Ok(())
}

/// `label` に続く 10 進数を取り出す。
fn parse_labelled_number(line: &str, label: &str) -> Option<u64> {
    let rest = line.split(label).nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// 既定ビルドを 1 回起動して serial を返す。
///
/// **feature を 1 つも足さない既定ビルドである。** 較正値は既定ビルドのものを
/// 集める（`verification-coverage.md` の「値は既定ビルドの起動ログから取ること」）。
fn capture_serial_for_calibration(run: usize) -> Result<String> {
    let workspace_root = workspace_root()?;
    let ovmf_vars = prepare_ovmf_vars(&workspace_root)?;
    let bootloader_efi = build_bootloader(&workspace_root, false)?;
    let kernel_elf = build_kernel_with_features(&workspace_root, &[])?;
    let esp_dir = stage_esp(&workspace_root, &bootloader_efi, &kernel_elf)?;

    let serial_log = workspace_root
        .join("target")
        .join(format!("serial-calibration-{run}.log"));
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
        // **既定は TCG である。** KVM での較正結果は測っていない。
        accelerator: Accelerator::Tcg,
    });

    let mut child = Command::new("qemu-system-x86_64")
        .args(&qemu_args)
        .spawn()
        .context("failed to launch qemu-system-x86_64 for the calibration run")?;

    // 較正が出るまで待つ。**上限を必ず付ける**（CLAUDE.md §14）。
    let deadline = Instant::now() + CALIBRATION_RUN_TIMEOUT;
    loop {
        if fs::read_to_string(&serial_log)
            .map(|c| c.contains("apic: LAPIC timer calibration:"))
            .unwrap_or(false)
        {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(PANIC_TEST_POLL_INTERVAL);
    }
    let _ = child.kill();
    let _ = child.wait();

    Ok(fs::read_to_string(&serial_log).unwrap_or_default())
}

/// 較正 1 回あたりの上限。較正窓は 5 標本 × 100ms なので、起動と合わせて余裕を取る。
const CALIBRATION_RUN_TIMEOUT: Duration = Duration::from_secs(40);

/// I/O APIC のレジスタが実際にデコードされることの確認（S2-a）。
///
/// **既定ビルドである**（`feature` が空）。壊すのではなく、読み経路が生きている
/// ことを見る。S1-c の時点では翻訳が張られたことしか確かめられておらず、
/// 「MMIO が本当にデコードされるか」は未確認のまま残っていた。S2-a で IOREGSEL
/// への書き込みを解禁して読めるようになったので、ここで閉じる。
///
/// **生の値をマーカーにしない。** `0x00170020` のような実測値そのものを書くと、
/// 構成が変わったときに「何を主張していた検査なのか」が読めないまま落ちる。
/// 主張が読める文言（デコードしていること、エントリが 24 本あること）に一致を取る。
///
/// 禁止マーカーは、判定が否側へ落ちたときにだけ出る行である。
const APIC_DECODE_TESTS: &[CriticalTest] = &[CriticalTest {
    name: "ioapic-decodes",
    feature: "",
    expected_markers: &[
        "apic: I/O APIC MMIO decodes",
        "24 redirection entr(y/ies)",
        // **定常状態まで到達すること。** この行が無いと、上の 2 つが出た時点で
        // 打ち切るので**その後で起動が止まる退行を捕まえられない**（`-smp 2` の
        // 項目で実際に踏んだ形と同じ死角である）。既定ビルドを走らせる項目は
        // これを入れておく。`APIC_TESTS` が同じ理由で入れてあるのに揃える。
        "heartbeat: ticks=",
    ],
    forbidden_markers: &["does not look decoded"],
    wait_for_full_timeout: false,
    min_heartbeats: None,
}];

/// APIC MMIO の写像の破壊確認（S1-c）。
///
/// **観測は panic ではない。** `acpi` と同じく S1 の経路は異常を見つけても
/// 停止しないので、「検出のログが出ること」と「Local APIC の読みが行われない
/// こと」の 2 つで判定する。ハートビートを期待マーカーに入れて、**検出した後も
/// カーネルが動き続けること**まで見る。
///
/// 禁止マーカーの `apic: LAPIC probe:` は、**写像が確認できたときにだけ出る行**
/// である。検出をすり抜けた場合にこれが出る。
///
/// **3 種は検出経路が別である。** 写像の有無 / 写像先の正しさ / MSR との
/// 突き合わせで、1 つの破壊で複数の経路が同時に落ちない形にしてある。
const APIC_TESTS: &[CriticalTest] = &[
    CriticalTest {
        name: "skip-map",
        feature: "apic-test-skip-map",
        expected_markers: &[
            "[apic-test-skip-map] skipping the mapping",
            "still has no translation",
            "registers are NOT read",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["apic: LAPIC probe:"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "wrong-target",
        feature: "apic-test-wrong-target",
        // **「翻訳がある」だけでは通らないことを、この文言で示す。** 翻訳は
        // 張られているので、`still has no translation` は出ない。
        expected_markers: &[
            "[apic-test-wrong-target] pointing the mapping",
            "not the expected physical address",
            "registers are NOT read",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &["apic: LAPIC probe:", "still has no translation"],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
    CriticalTest {
        name: "base-mismatch",
        feature: "apic-test-base-mismatch",
        // 突き合わせで止まるので、**写像そのものへ到達しない。**
        expected_markers: &[
            "[apic-test-base-mismatch] moving the MADT local APIC address",
            "the two disagree, so nothing is mapped and nothing is read",
            "heartbeat: ticks=",
        ],
        forbidden_markers: &[
            "apic: LAPIC probe:",
            "apic: mapped the local APIC",
            "the sabotage did nothing",
        ],
        wait_for_full_timeout: false,
        min_heartbeats: None,
    },
];

/// **構造的なガードが既定ビルドのバイナリに在ること**を見る（S4-c-3-2b、S5-bで拡張）。
///
/// **名前は主張を指す。** 当初は検出器だけを見ていたが、**世代フラッシュ（S5-b）も
/// 同じ問いなので同じ検査へ入れた。** **「検出器のシンボル」では主張とずれる。**
///
/// # なぜこの検査が要るのか
///
/// 検出器は**本番ビルドにも置いてある。** 守りが 2 層とも効いている限り鳴らない
/// ので、**「鳴らないこと」が主張になる。** ところが**コードが無くても鳴らない。**
/// 「本番で出ない」と「コードが無い」を区別するために、シンボルの存在を見る。
///
/// # **主たる論拠は構造の側にある。これは裏取りである**
///
/// 破壊 `sched-ignore-current` が触るのは `pick_next` のフィルタでの参照だけで、
/// **検出器の呼び出しに `cfg` は付かない。** よって検出器は構成によらず全ビルドに
/// 在る。**この検査はそれを機械的に確かめるだけである。**
///
/// # 世代フラッシュも同じ問いである（S5-b）
///
/// `bkl` の世代フラッシュは、**探りが feature 越しなので、既定ビルドに在ることを
/// 探りでは示せない。** 検出器と同じく、**主たる論拠は構造の側**（`cfg` が付かない）
/// で、**ここはその裏取りである。**
///
/// # **限界: 「呼ばれうる位置に在る」までしか見えない**
///
/// シンボルが在ることは、**正しい位置で呼ばれることを示さない。** それを示すのは
/// 2 層とも壊した構成（`smp-ap-test sched-ignore-both-layers`）で実際に鳴るほうで
/// ある。**この検査だけが緑でも、検出器が働いていることの証明にはならない。**
/// **世代フラッシュも同じで、「正しい位置で呼ばれる」ことは
/// `smp-ap-test tlb-generation` と `tlb-shootdown` が示す。**
fn check_structural_guard_symbols_present(workspace_root: &Path) -> Result<String> {
    let kernel_elf = build_kernel(workspace_root, false)?;
    let output = Command::new("nm")
        .arg(&kernel_elf)
        .output()
        .context("failed to invoke nm (is binutils installed?)")?;
    if !output.status.success() {
        bail!("nm failed on {}", kernel_elf.display());
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut found = Vec::new();
    for fragment in STRUCTURAL_GUARD_SYMBOL_FRAGMENTS {
        match listing.lines().find(|line| line.contains(fragment)) {
            Some(line) => found.push(line.split_whitespace().last().unwrap_or(line).to_string()),
            None => bail!(
                "the default kernel build has no symbol containing {fragment:?}. The production \
                 guards must exist in the default build: \"it does not fire\" is only a claim \
                 if the code is there. If it was renamed, update \
                 STRUCTURAL_GUARD_SYMBOL_FRAGMENTS; if it was removed or inlined away, restore it (both are marked #[inline(never)] for \
                 exactly this reason)."
            ),
        }
    }
    Ok(found.join(", "))
}

/// **構造的なガード**のシンボル名に必ず現れる断片（S4-c-3-2b、S5-bで拡張）。
///
/// Rust のシンボルはマングルされるので、**関数名の断片で照合する。**
/// 改名したらここも直すこと（検査の失敗メッセージがそう言う）。
const STRUCTURAL_GUARD_SYMBOL_FRAGMENTS: &[&str] = &[
    "report_double_selection",
    "report_foreign_task_adoption",
    "report_layer_two_skip",
    // 世代フラッシュ（S5-b）。**探りは feature 越しなので、既定ビルドに在ることは
    // 探りでは示せない。** ここで裏取りする。
    "flush_if_generation_is_stale",
];

/// 意図的に壊した経路を有効にする feature の名前。
///
/// **接頭辞ではない。** 照合は完全一致である（`contains`）。**以前この doc は
/// 「接頭辞・名前」と書いていたが、コードは一度も接頭辞として扱っていない**（S6-e）。
///
/// **既定ビルドにこれらが入ってはならない。** 入ったまま出荷すると、
/// 壊れた状態で測った結果を正常な結果として扱うことになる。
const SABOTAGE_FEATURES: &[&str] = &[
    "percpu-fake-nonzero-cpu-id",
    "smp-tramp-corrupt-copy-test",
    "smp-ap-touch-scheduler-test",
    "lapic-timer-scale-calibration-test",
    "lapic-timer-wrong-divide-test",
    "lapic-timer-no-mask-all-test",
    "ioapic-wrong-vector-test",
    "ioapic-skip-unmask-test",
    "ioapic-keep-pic-irq1-test",
    "misalign-test",
    "idt-irq-stub-offset-test",
    "addrspace-no-kernel-share",
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
    "acpi-test",
    "apic-test",
    "smp-ap-no-sentinel-clear",
    "sched-ignore-owner",
    "sched-ignore-current",
    "smp-ap-runs-preemptive-demo",
    "sched-ignore-bootstrap-tripwire",
    "sched-keep-workers-runnable",
    "smp-ipi-probe",
    "smp-tlb-generation-probe",
    "smp-tlb-shootdown-probe",
    "smp-tlb-no-generation-bump",
];

/// 内部を隠す約束のディレクトリ。
///
/// - `kernel/src/irq/`: 割り込みコントローラとタイマ源の境界（S0-a）。外から
///   `pic` / `pit` を参照できないことをコンパイラが保証する。
/// - `kernel/src/task/`: スケジューラの実体（`static mut SCHEDULER`）。外から
///   構造体全体への参照を作れないことをコンパイラが保証する（S0-b）。
/// - `kernel/src/acpi/`: ファームウェアが提示する構成表の境界（S1-b）。
///   テーブルの生バイトとパーサの型は境界の中に留まる。**物理アドレスは
///   S1-c で出るようになった**（`ApicMmio`。APIC の MMIO を写像するには
///   所在そのものが要る）ので、「物理アドレスも留まる」はもう成り立たない。
///   宣言（`mod rsdp;`）は `acpi/mod.rs` にあるので、
///   `irq` と同じくディレクトリ指定で中に入る。
/// - `kernel/src/task.rs`: 上の `mod scheduler;` 宣言がここにある。**ディレクトリ
///   だけを見る形では、この 1 行が対象から外れる**（`irq` は宣言が
///   `irq/mod.rs` にあるので中に入るが、`task` は `task.rs` が外にある）。
///   実測で気づいた穴なので、ファイルを明示して対象に入れる。`task.rs` を
///   `task/mod.rs` へ改名すれば対称になるが、**このパスは docs から 6 箇所で
///   参照されている**（`verification-coverage` 4 / `roadmap` 1 /
///   `deferred-decisions` 1）ため改名しない。M5-f-3 を改名しなかったのと同じ
///   理由である。
///
/// `task` 側について 1 つ正確に書いておく。`mod scheduler;` を `pub mod` にしても、
/// アクセサが `pub(super) fn` なので外からは呼べない（実測では
/// `E0603: function switches is private` になった）。**この対象追加が捕まえるのは
/// 「漏れる形」ではなく「漏れる条件の片方」である。** `irq` の
/// `pub(crate) use super::pic::*;` と同じく、保守的に禁じている側に当たる。
/// 検査は `fn` の可視性を見ない（見ると境界の公開 API まで禁じることになる）。
///
/// **どちらも「コンパイラが保証し、検査はその保証が外されるのを防ぐ」形である。**
/// 保証の作り方が同じなので、検査も 1 つで足りる。
static PRIVATE_BOUNDARY_DIRS: &[&str] = &[
    "kernel/src/irq/",
    "kernel/src/task/",
    "kernel/src/task.rs",
    "kernel/src/acpi/",
];

/// 内部を隠す約束のモジュール（[`PRIVATE_BOUNDARY_DIRS`]）が、その内部を外へ
/// 公開していないことを確かめる（seam整備の項目1=S0-a、S0-b）。
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
        if !PRIVATE_BOUNDARY_DIRS
            .iter()
            .any(|dir| relative.starts_with(dir))
        {
            continue;
        }
        let path = workspace_root.join(relative);
        let source = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim_start();
            if let Some(item) = visibility_qualified_mod_or_use(trimmed) {
                findings.push(format!(
                    // 文言は境界の名前を書かない。対象は`PRIVATE_BOUNDARY_DIRS`で
                    // 増えるので、特定の境界（かつては割り込み層だけだった）を
                    // 名指しすると、対象が増えたときに事実と食い違う。
                    "{relative}:{}: {item} is exported out of a private-by-design boundary: {}",
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
/// 行き着かないことを見る。`[features]` を そのまま読む
/// （`name = ["a", "b"]` の形しか使っていない）。
///
/// **kernel と bootloader の両方を見る（S6-e で広げた）。** 以前は kernel だけを
/// 読んでおり、**`panic-test`（bootloader の feature）は一覧にあっても照合の
/// 対象に一度も入っていなかった。** **bootloader の `default` が壊す feature へ
/// 行き着いても、誰も落とさない状態だった。**
///
/// **あわせて死んだエントリも見る**——[`SABOTAGE_FEATURES`] に、どちらの
/// マニフェストにも無い名前が載っていないこと。**許可リストの死んだエントリと
/// 同じ穴である**（S6-d）。
fn check_default_features_are_clean(workspace_root: &Path) -> Result<Vec<String>> {
    let mut findings = Vec::new();
    let mut declared: Vec<String> = Vec::new();
    for crate_name in ["kernel", "bootloader"] {
        findings.extend(check_one_manifest_default_features(
            workspace_root,
            crate_name,
            &mut declared,
        )?);
    }
    for feature in SABOTAGE_FEATURES {
        if !declared.iter().any(|d| d == feature) {
            findings.push(format!(
                "dead SABOTAGE_FEATURES entry (no such feature in kernel/ or bootloader/): \
                 `{feature}`"
            ));
        }
    }
    Ok(findings)
}

/// 1 つのマニフェストについて上を行う。宣言されている feature 名を `declared` へ足す。
fn check_one_manifest_default_features(
    workspace_root: &Path,
    crate_name: &str,
    declared: &mut Vec<String>,
) -> Result<Vec<String>> {
    let manifest = fs::read_to_string(workspace_root.join(crate_name).join("Cargo.toml"))
        .with_context(|| format!("failed to read {crate_name}/Cargo.toml"))?;

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
    declared.extend(graph.iter().map(|(name, _)| name.clone()));
    // **bootloader には `default` が無い。** 無い場合は「`default` から辿れる
    // ものは何も無い」ので、辿る側の検査は空で正しい。
    if !graph.iter().any(|(name, _)| name == "default") {
        return Ok(Vec::new());
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
        .map(|name| format!("{crate_name}: `default` reaches the sabotage feature `{name}`"))
        .collect())
}

/// 全構成のビルド・テスト・clippy・fmt を順に実行する。
///
/// **1 つ落ちてもそこで止めない。** 止めると「直しては再実行」を
/// 繰り返すことになり、全体像が分からない。最後にまとめて報告する。
fn cmd_check(full: bool) -> Result<()> {
    // 外した確率的な項目の一覧が実態を指しているかを先に見る（列挙の腐りを防ぐ）。
    check_flaky_list_matches_tables()?;

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

    if full {
        total += 1;
        println!("=== xtask check: the boot log matches the reference and does not depend on the core count");
        match cmd_boot_log_diff(false) {
            Ok(()) => println!("--- boot log diff: OK"),
            Err(error) => {
                println!("--- boot log diff: FAILED ({error})");
                failed.push("boot log diff".to_string());
            }
        }
    }

    total += 1;
    println!("=== xtask check: direct serial ports stay on the approved list");
    let mut approved_direct_serial_occurrences = 0usize;
    let unapproved_direct_serial = find_unapproved_direct_serial_ports(
        &workspace_root,
        &mut approved_direct_serial_occurrences,
    )?;
    if unapproved_direct_serial.is_empty() {
        println!(
            "--- direct serial ports: OK ({} approved entr(y/ies) = file+item pairs, covering {} \
             occurrence(s) = SerialPort::new lines)",
            DIRECT_SERIAL_PORT_ALLOWLIST.len(),
            approved_direct_serial_occurrences
        );
    } else {
        for finding in &unapproved_direct_serial {
            println!("    {finding}");
        }
        println!("    approved sites (file / item / reason):");
        for site in DIRECT_SERIAL_PORT_ALLOWLIST {
            println!("      {} / {} / {}", site.file, site.item, site.reason);
        }
        println!(
            "--- direct serial ports: FAILED ({} unapproved site(s); route the line through the \
             logger held inside the BKL, or add an entry with a reason)",
            unapproved_direct_serial.len()
        );
        failed.push("direct serial ports".to_string());
    }

    total += 1;
    println!("=== xtask check: private-by-design modules keep their internals private");
    let leaks = find_boundary_visibility_leaks(&workspace_root)?;
    if leaks.is_empty() {
        println!(
            "--- private boundaries: OK (no visibility qualifier on mod/use/type under {})",
            PRIVATE_BOUNDARY_DIRS.join(", ")
        );
    } else {
        for finding in &leaks {
            println!("    {finding}");
        }
        println!(
            "--- private boundaries: FAILED ({} export(s); the boundary must be the only way in)",
            leaks.len()
        );
        failed.push("private boundaries".to_string());
    }

    total += 1;
    println!("=== xtask check: every kernel feature appears in the runtime TEST_HOOKS table");
    let uncovered = find_features_missing_from_test_hooks(&workspace_root)?;
    if uncovered.is_empty() {
        println!(
            "--- test hooks coverage: OK (every feature in kernel/Cargo.toml is either in \
             TEST_HOOKS or on the {}-entry exclusion list)",
            TEST_HOOKS_EXCLUSIONS.len()
        );
    } else {
        for finding in &uncovered {
            println!("    {finding}");
        }
        println!("    exclusions (feature / reason):");
        for (feature, reason) in TEST_HOOKS_EXCLUSIONS {
            println!("      {feature} / {reason}");
        }
        println!(
            "--- test hooks coverage: FAILED ({} feature(s) not reported at boot)",
            uncovered.len()
        );
        failed.push("test hooks coverage".to_string());
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
    println!("=== xtask check: markdown prose style (tracked .md)");
    let prose = check_markdown_prose_style(&workspace_root)?;
    if prose.is_empty() {
        println!("--- markdown prose style: OK");
    } else {
        for finding in &prose {
            println!("    {finding}");
        }
        println!("--- markdown prose style: FAILED ({} line(s))", prose.len());
        failed.push("markdown prose style".to_string());
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

    // 二重選択の検出器が既定ビルドに在ること（S4-c-3-2b、静的）。
    total += 1;
    println!("=== xtask check: the structural guards are present in the default build");
    match check_structural_guard_symbols_present(&workspace_root) {
        Ok(symbol) => println!("--- guard symbols: OK ({symbol})"),
        Err(e) => {
            println!("    {e}");
            println!("--- guard symbols: FAILED");
            failed.push("guard symbols".to_string());
        }
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
                cmd_marker_test(CRITICAL_TESTS, "critical-test", test.name, None)
            });
        }
        for test in PAGING_TESTS {
            total += 1;
            let name = format!("paging-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(PAGING_TESTS, "paging-test", test.name, None)
            });
        }
        for test in STACK_TESTS {
            total += 1;
            let name = format!("stack-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(STACK_TESTS, "stack-test", test.name, None)
            });
        }
        for test in TASK_TESTS {
            total += 1;
            let name = format!("task-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(TASK_TESTS, "task-test", test.name, None)
            });
        }
        for test in RING3_TESTS {
            total += 1;
            let name = format!("ring3-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(RING3_TESTS, "ring3-test", test.name, None)
            });
        }
        for test in SYSCALL_TESTS {
            total += 1;
            let name = format!("syscall-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(SYSCALL_TESTS, "syscall-test", test.name, None)
            });
        }
        for test in ACPI_TESTS {
            total += 1;
            let name = format!("acpi-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(ACPI_TESTS, "acpi-test", test.name, None)
            });
        }
        for test in ACPI_SMP_TESTS {
            total += 1;
            let name = format!("acpi-smp-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(ACPI_SMP_TESTS, "acpi-smp-test", test.name, Some(2))
            });
        }
        // **BKL の相互排除の証明（S4-b-4）。KVM を要する。**
        total += 1;
        run_regression(
            "bkl-test exclusion-proof",
            &mut failed,
            &mut retries,
            cmd_bkl_exclusion_proof,
        );
        // BKL 待ちのタイムアウト（S4-b-4）。**-smp 2 が要る**（別コアが保持する）。
        for test in BKL_TIMEOUT_TESTS {
            total += 1;
            let name = format!("bkl-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(BKL_TIMEOUT_TESTS, "bkl-test", test.name, Some(2))
            });
        }
        // BKL の破壊（S4-b-2）。
        for test in BKL_TESTS {
            total += 1;
            let name = format!("bkl-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(BKL_TESTS, "bkl-test", test.name, None)
            });
        }
        // **AP のティックのレートをホストの実時間と突き合わせる（S4-a）。**
        // 較正値を BSP と共有するのは仮定なので、**カーネルの外の基準で確かめる。**
        total += 1;
        run_regression(
            "smp-ap-test ap-timer-rate",
            &mut failed,
            &mut retries,
            cmd_ap_timer_rate,
        );
        // per-CPU スロットが足りない構成（S3-b-2a）。覆いの報告の `false` 側を
        // 評価する構成をここで残す。
        for test in ACPI_SMP4_TESTS {
            total += 1;
            let name = format!("acpi-smp-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(ACPI_SMP4_TESTS, "acpi-smp-test", test.name, Some(4))
            });
        }
        for test in APIC_TESTS {
            total += 1;
            let name = format!("apic-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(APIC_TESTS, "apic-test", test.name, None)
            });
        }
        for test in APIC_DECODE_TESTS {
            total += 1;
            let name = format!("apic-decode-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(APIC_DECODE_TESTS, "apic-decode-test", test.name, None)
            });
        }
        for test in INTERRUPT_TESTS {
            if is_excluded_flaky("interrupt-test", test.name) {
                println!(
                    "=== xtask check: interrupt-test {} は確率的なので --full から外してある（cargo xtask flaky）",
                    test.name
                );
                continue;
            }
            total += 1;
            let name = format!("interrupt-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(INTERRUPT_TESTS, "interrupt-test", test.name, None)
            });
        }
        if is_excluded_flaky("interrupt-test", "keyboard") {
            println!(
                "=== xtask check: interrupt-test keyboard は確率的なので --full から外してある（cargo xtask flaky）"
            );
        } else {
            total += 1;
            run_regression(
                "interrupt-test keyboard",
                &mut failed,
                &mut retries,
                cmd_keyboard_test,
            );
        }
        // S2-d-2 の検査と破壊確認。**健全な `rate` を先頭に置いてある**ので、
        // 破壊が意図した経路だけを壊していることまで確かめられる。
        for test in LAPIC_TIMER_TESTS {
            total += 1;
            let name = format!("lapic-timer-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_lapic_timer_test(test.name)
            });
        }
        // S3-b-2b-2 の sentinel の破壊確認。
        for test in SMP_AP_TESTS {
            if is_excluded_flaky("smp-ap-test", test.name) {
                println!(
                    "=== xtask check: smp-ap-test {} は確率的なので --full から外してある（cargo xtask flaky）",
                    test.name
                );
                continue;
            }
            total += 1;
            let name = format!("smp-ap-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(SMP_AP_TESTS, "smp-ap-test", test.name, Some(2))
            });
        }
        // S3-b-2b-1 の雛形一致検査の破壊確認。
        for test in SMP_TRAMP_TESTS {
            total += 1;
            let name = format!("smp-tramp-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(SMP_TRAMP_TESTS, "smp-tramp-test", test.name, Some(2))
            });
        }
        // S3-b-2a の tripwire の破壊確認。
        for test in PERCPU_TESTS {
            total += 1;
            let name = format!("percpu-test {}", test.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_marker_test(PERCPU_TESTS, "percpu-test", test.name, None)
            });
        }
        // S2-d-1c の破壊確認。**落ちるべき主張だけが落ちること**を見る。
        // 健全な側も並べて指定しているので、破壊が意図した経路だけを
        // 壊していることまで確かめられる。
        for sabotage in IOAPIC_SABOTAGE_TESTS {
            total += 1;
            let name = format!("ioapic-test {}", sabotage.name);
            run_regression(&name, &mut failed, &mut retries, || {
                cmd_ioapic_sabotage(sabotage.name, sabotage.feature, sabotage.expected)
            });
        }
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
    // **項目数が会計行と一致すること。** 検査を足して会計行を更新し忘れる形を
    // 構造で止める（`EXPECTED_CHECK_COUNT` の doc）。**`total` はここで確定して
    // いるので、`cmd_check` の組み替えは要らない。**
    check_count_matches_accounting(&workspace_root, total, full)?;

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

/// 会計行に記録されている項目数（`docs/verification-coverage.md` の「項目会計」）。
///
/// # なぜ二重に持つのか。**片方が機械で強制されるなら、両方が腐るのとは違う**
///
/// 会計行は「検査を足したとき数が閉じていることを、この行だけで追う」場所である。
/// **ところがその行自体が 2 段ぶん古くなっていた**（S2-d-1c の 3 種と S2-d-2 の
/// 4 種を足したときに更新しておらず、`--full` が 77 のまま残っていた）。
/// **単一の出所と決めた場所が古くなると、他のすべての参照が正しくても会計は止まる。**
///
/// 以前この案を「二重持ちの場所が変わるだけ」として見送った記録があるが、
/// **weigh きれていなかった点がある。** この定数は**合わないとビルド（検査）が
/// 落ちる**。docs の 2 箇所が両方とも静かに腐るのとは性質が違う。
///
/// # **強制されるのは定数の更新だけである。会計行の更新は強制されない**
///
/// 検査を足すとここが落ち、直すには定数を上げる必要がある。**そのとき この doc が
/// 会計行を指しているので、「検査を足したら会計行を見る」が手順ではなく構造から
/// 促される。** ただし**促されるだけで、強制ではない。** 定数だけ上げて会計行を
/// 放置することはできる。**半分だけ構造へ移った状態である。** 残りの半分は
/// 依然として規律なので、そう書いておく（守れない箇所を守れると書かない）。
///
/// # 会計行を機械で読む案は、2 度目が起きたので採った
///
/// **以前はこの案を採らなかった。** 理由は「xtask が会計行の書式へ結合し、
/// 文書の書き方が検査の都合で固定される。会計行は人が読んで経緯を辿るための
/// 散文なので釣り合わない」だった。**その判断のあとで、同じ規律が 2 度目に破れた**
/// （S3-b-2b-2 が定数を 88 から 89 へ上げ、会計行は 88 のまま残った）。
///
/// **天秤の重みが変わった。** 促されるだけでは足りないことが、同じ行で 2 度
/// 示された。そこで [`check_accounting_line_lists_current_counts`] を足した。
/// **結合を最小にしてある**——見るのは「`推移: ` で始まる行に、現在の base と
/// full が太字の数字として現れること」だけで、推移の書き方・順序・理由の文言には
/// 触れない。歴史の数字は残るので、**行を消さない**運用とも噛み合う。
///
/// # **強制できるのは数字の鮮度だけである。経緯の正しさは強制されない**
///
/// この検査は `**89**` という文字列が行に在ることしか見ない。**理由を書かずに
/// 数字だけ足せば通る。** 会計行の価値は「どの段が何を足したか」の側にあり、
/// そこは依然として規律である。**守れない箇所を守れると書かないために明示する。**
/// 検査が保証するのは「数が変わったときに、この行が触られること」までである。
///
/// # 走らせる前に総数を出す必要は無い
///
/// 以前の記録は「走らせずに総数を出せるよう `cmd_check` を組み替えるのが前提」と
/// 書いていたが、**照合するだけなら要らない。** `total` は `cmd_check` の末尾で
/// 既に確定している（`all {total} check(s) passed` がそれを印字している）。
/// 組み替えが要るのは「走らせる前に印字する」形の場合だけである。
struct ExpectedCheckCount {
    /// `cargo xtask check`（QEMU を起動しない検査だけ）。
    base: usize,
    /// `cargo xtask check --full`。
    full: usize,
}

/// 会計行の現在値。**検査を足したらここを上げ、あわせて会計行も更新すること。**
const EXPECTED_CHECK_COUNT: ExpectedCheckCount = ExpectedCheckCount {
    base: 20,
    full: 116,
};

/// 実際に走った項目数が会計行と一致するかを見る。
///
/// 一致しないときは**検査の失敗として扱う。** 項目を足したのに会計行を更新して
/// いない状態でコミットへ進めないようにするためである。
fn check_count_matches_accounting(workspace_root: &Path, total: usize, full: bool) -> Result<()> {
    let expected = if full {
        EXPECTED_CHECK_COUNT.full
    } else {
        EXPECTED_CHECK_COUNT.base
    };
    if total != expected {
        let field = if full { "full" } else { "base" };
        bail!(
            "xtask check: ran {total} check(s) but EXPECTED_CHECK_COUNT.{field} is {expected}. \
             If you added or removed a check, update that constant in xtask AND the \
             item-accounting line in docs/verification-coverage.md"
        );
    }
    check_accounting_line_lists_current_counts(workspace_root)
}

/// `TEST_HOOKS` に載せなくてよい feature と、その理由。
///
/// **除外は列挙だが、向きが逆である。** 「列挙に無い形を静かに通す」形ではなく、
/// **列挙に無い feature は TEST_HOOKS に在るはず**という向きに書いてある。
/// 新しい feature を足して両方に入れ忘れれば、静かに通らず落ちる。
const TEST_HOOKS_EXCLUSIONS: &[(&str, &str)] = &[
    ("default", "feature の既定値であって、壊す経路ではない"),
    (
        "heap-poison",
        "解放したメモリを毒値で埋める開発時の補助。壊す経路ではない",
    ),
    (
        "keyboard-raw-log",
        "スキャンコードを生のままログへ出すだけ。壊す経路ではない",
    ),
];

/// `kernel/Cargo.toml` の feature のうち、`TEST_HOOKS` にも除外リストにも
/// 無いものを挙げる。
///
/// # なぜ数ではなく名前の集合で見るのか
///
/// **数を数えるだけの検査は、足した数と消した数が釣り合うと素通りする。**
/// 名前の集合の一致なら、入れ替わりも捕まる。**会計行の強制（数字が在るだけで
/// 満たせる）より強い保証である**（`ExpectedCheckCount` の doc に、あちらで
/// 強制できるのが数字の鮮度だけであることを書いてある）。
///
/// # 3 度目である
///
/// 「一覧が足したときに更新されず静かに狭くなる」は、会計行・
/// `verification-coverage.md` の破壊 feature 一覧に続いて 3 件目である。
/// **今回は機械で守れる形なので、規律に戻さない。**
///
/// # 逆向きは別の機構が守っている。**両向きが揃っている**
///
/// この関数が見るのは片側だけである（Cargo.toml の feature → 表に在ること）。
/// **逆向き、すなわち表に在るのに Cargo.toml に無い feature は、ここでは
/// 捕まらない。**
///
/// **その逆向きは `unexpected_cfgs` lint が構造的に覆っている**（実測）。
/// 存在しない feature 名で `cfg!(feature = "…")` を書くと、`-D warnings` の
/// clippy が落ちる。
///
///     error: unexpected `cfg` condition value: `zzz-not-a-real-feature`
///          = note: `-D unexpected-cfgs` implied by `-D warnings`
///
/// `cargo xtask check` は 4 構成すべてに `-D warnings` を掛けているので、
/// **この経路は既に検査に入っている。**
///
/// **したがって両向きが、別々の機構で守られている。** 片側はこの関数、
/// 逆側は lint である。**逆向きの検査を足さない理由はこれである**（同じことを
/// 言う検査を 2 つ置かない）。**lint を緩める変更（`allow(unexpected_cfgs)` を
/// 足す、`-D warnings` を外す）は、この保証を落とす。**
fn find_features_missing_from_test_hooks(workspace_root: &Path) -> Result<Vec<String>> {
    let manifest = workspace_root.join("kernel").join("Cargo.toml");
    let manifest_text = fs::read_to_string(&manifest)
        .with_context(|| format!("could not read {}", manifest.display()))?;
    let source = workspace_root.join("kernel").join("src").join("main.rs");
    let source_text = fs::read_to_string(&source)
        .with_context(|| format!("could not read {}", source.display()))?;

    let Some(start) = source_text.find(TEST_HOOKS_TABLE_MARKER) else {
        bail!(
            "xtask check: could not find {TEST_HOOKS_TABLE_MARKER:?} in kernel/src/main.rs. \
             If the table was renamed, update TEST_HOOKS_TABLE_MARKER in xtask along with it"
        );
    };
    let table = &source_text[start..];
    let table = match table.find("\n];") {
        Some(end) => &table[..end],
        None => table,
    };

    let mut findings = Vec::new();
    for line in manifest_text.lines() {
        // `feature-name = [...]` の形だけを feature とみなす。
        let Some((name, rest)) = line.split_once(" = ") else {
            continue;
        };
        if !rest.starts_with('[') || name.is_empty() {
            continue;
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            continue;
        }
        if TEST_HOOKS_EXCLUSIONS.iter().any(|(f, _)| *f == name) {
            continue;
        }
        // **`cfg!` の形で探す。** 表示名だけを見ると、名前と `cfg!` が食い違って
        // いても通ってしまう（別の feature の状態を、この名前で報告する形）。
        let needle = format!("cfg!(feature = \"{name}\")");
        if !table.contains(&needle) {
            findings.push(format!(
                "kernel/Cargo.toml: feature {name:?} is not reported by TEST_HOOKS \
                 (add an entry, or put it on the exclusion list with a reason)"
            ));
        }
    }
    Ok(findings)
}

/// `TEST_HOOKS` の表を探す目印。**この形に結合しているのはここだけである。**
const TEST_HOOKS_TABLE_MARKER: &str = "const TEST_HOOKS: &[(&str, bool, &str)] = &[";

/// 会計行がある文書。
const ACCOUNTING_DOC_PATH: &str = "docs/verification-coverage.md";

/// 会計行の目印。**この行の書式に結合しているのはここだけである。**
const ACCOUNTING_LINE_PREFIX: &str = "推移: ";

/// 会計行が現在の項目数を載せているかを見る。
///
/// **見るのは数字が在ることだけである。** 推移の書き方・順序・理由の文言には
/// 触れない（[`ExpectedCheckCount`] の doc に、何が強制され何が強制されないかを
/// 書いてある）。**歴史の数字は残ってよい**ので、含むことだけを条件にしてある。
fn check_accounting_line_lists_current_counts(workspace_root: &Path) -> Result<()> {
    let path = workspace_root.join(ACCOUNTING_DOC_PATH);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("could not read the accounting doc {}", path.display()))?;

    let Some(line) = text
        .lines()
        .find(|line| line.starts_with(ACCOUNTING_LINE_PREFIX))
    else {
        bail!(
            "xtask check: {ACCOUNTING_DOC_PATH} has no line starting with \
             {ACCOUNTING_LINE_PREFIX:?}. That line is the item-accounting record. If it was \
             renamed, update ACCOUNTING_LINE_PREFIX in xtask along with it"
        );
    };

    for (field, value) in [
        ("base", EXPECTED_CHECK_COUNT.base),
        ("full", EXPECTED_CHECK_COUNT.full),
    ] {
        let needle = format!("**{value}**");
        if !line.contains(&needle) {
            bail!(
                "xtask check: EXPECTED_CHECK_COUNT.{field} is {value}, but the item-accounting \
                 line in {ACCOUNTING_DOC_PATH} does not carry {needle} . Append the new count to \
                 that line, with the stage that added the check(s). Only the digits are enforced; \
                 the reason next to them is not, and it is the part worth having"
            );
        }
    }
    Ok(())
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
/// `--full` から外した確率的な項目（S9-b の途中で、独立した作業として外した）。
///
/// # なぜ外すか
///
/// **確率的なものを `--full` へ入れると、落ちたときに退行か揺らぎかが区別できない。**
/// 方針は `coding-standards.md` にあり、`kernel-entry-concurrency` を外した前例も
/// ある。**同じ性質の 4 項目が入ったままだった**（`deferred-decisions.md` の
/// 持ち越し 12「決めた方針が、後から該当した項目へ適用されていない」）。
///
/// **実測**——T2-c で `--full` 15 回中 4 回（約 27%）、S9-b-2 で 2 回中 2 回。
/// **毎回、落ちたのが退行か揺らぎかを手で切り分けることになる。**
///
/// # 外して何が失われるか
///
/// **前例とは事情が違う。** `kernel-entry-concurrency` は**補助実証**で、
/// `verification-coverage.md` が「何かの主張を担っていない」と書いている。
/// **こちらの 4 つは主張を担っている**（TLB シュートダウン、キーボードの配送、
/// 二層の守り）。**外すと、`cargo xtask flaky` を回さない限り誰も見ない。**
///
/// **そして前例が実際に手で回された記録は無い。** 外したのは S4-a で、
/// それ以降 `kernel-entry-concurrency` を回した記録が docs に見当たらない。
/// **したがって「手で回す」は、手順を置いても回される保証が無い。**
/// **失われるものを正確に書いたうえで外している。**
const FLAKY_EXCLUDED: &[(&str, &str)] = &[
    ("interrupt-test", "keyboard"),
    ("smp-ap-test", "tlb-shootdown"),
    ("smp-ap-test", "smp-stimulus-layer1-off"),
    ("smp-ap-test", "smp-stimulus-both-layers-off"),
];

/// その項目が [`FLAKY_EXCLUDED`] に載っているか。
fn is_excluded_flaky(group: &str, name: &str) -> bool {
    FLAKY_EXCLUDED
        .iter()
        .any(|(g, n)| *g == group && *n == name)
}

/// [`FLAKY_EXCLUDED`] の各行が実在の項目を指しているかを確かめる。
///
/// **列挙で守るものは、列挙が実態からずれると静かに効かなくなる。**
/// 名前を打ち間違えると「外したつもりで外れていない」か「存在しない項目を
/// 外している」になる。どちらも出力からは分からないので、ここで落とす。
fn check_flaky_list_matches_tables() -> Result<()> {
    for (group, name) in FLAKY_EXCLUDED {
        let found = match (*group, *name) {
            // `keyboard` は表の項目ではなく単独の関数である（`cmd_keyboard_test`）。
            // **表だけを見ていると「無い」と判定するので、ここで明示的に扱う。**
            // 実際、最初に表だけを見る形で書いて、この検査に落とされた。
            ("interrupt-test", "keyboard") => true,
            ("interrupt-test", n) => INTERRUPT_TESTS.iter().any(|t| t.name == n),
            ("smp-ap-test", n) => SMP_AP_TESTS.iter().any(|t| t.name == n),
            (other, _) => anyhow::bail!("FLAKY_EXCLUDED names an unknown group {other:?}"),
        };
        if !found {
            anyhow::bail!("FLAKY_EXCLUDED names {group} {name:?}, which no table has");
        }
    }
    Ok(())
}

/// `--full` から外した確率的な項目を手で回す（`cargo xtask flaky`）。
///
/// **外した項目を回す手段がなければ、外すことは「守らないと決める」ことになる。**
/// 1 項目につき最大 [`FLAKY_ATTEMPTS`] 回まで試し、**何回目で通ったかを出す。**
///
/// **回数そのものが情報である。** 1 回目で通り続けているうちは揺らぎが小さく、
/// 3 回目まで要るようになったなら確率が上がっている。**通らなければ退行である。**
fn cmd_flaky() -> Result<()> {
    let mut never_passed: Vec<String> = Vec::new();

    for (group, name) in FLAKY_EXCLUDED {
        let label = format!("{group} {name}");
        let mut passed_on = None;
        for attempt in 1..=FLAKY_ATTEMPTS {
            let result = match (*group, *name) {
                ("interrupt-test", "keyboard") => cmd_keyboard_test(),
                ("interrupt-test", n) => cmd_marker_test(INTERRUPT_TESTS, group, n, None),
                ("smp-ap-test", n) => cmd_marker_test(SMP_AP_TESTS, group, n, Some(2)),
                (other, _) => anyhow::bail!("FLAKY_EXCLUDED names an unknown group {other:?}"),
            };
            if result.is_ok() {
                passed_on = Some(attempt);
                break;
            }
            println!("--- flaky: {label} did not pass on attempt {attempt}");
        }
        match passed_on {
            Some(attempt) => println!("--- flaky: {label}: OK (passed on attempt {attempt})"),
            None => {
                println!("--- flaky: {label}: FAILED ({FLAKY_ATTEMPTS} attempt(s), none passed)");
                never_passed.push(label);
            }
        }
    }

    if never_passed.is_empty() {
        println!(
            "flaky: all {} excluded item(s) passed within {FLAKY_ATTEMPTS} attempt(s)",
            FLAKY_EXCLUDED.len()
        );
        Ok(())
    } else {
        anyhow::bail!(
            "flaky: {} item(s) never passed: {}",
            never_passed.len(),
            never_passed.join(", ")
        )
    }
}

/// [`cmd_flaky`] が 1 項目に許す試行回数。
///
/// **27% の揺らぎなら 3 回で約 2%、5 回で約 0.14% まで落ちる。**
/// 5 回とも落ちたなら揺らぎでは説明しにくく、退行を疑う根拠になる。
/// **ただし「5 回の緑では足りない」は逆向きにも効く**——通ったことは
/// 「揺らぎが消えた」の証拠にはならない（`coding-standards.md`）。
const FLAKY_ATTEMPTS: usize = 5;

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
